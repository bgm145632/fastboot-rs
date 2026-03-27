mod adb;
mod cli;
mod driver;
mod error;
mod flash;
mod partition;
mod progress;
mod protocol;
mod sparse;
mod tcp_transport;
mod transport;
mod udp_transport;
mod usb_transport;
mod util;

use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Parser;

use cli::{Cli, Commands};
use error::FastbootError;
use flash::ImageSource;
use progress::{
    format_size, format_speed, print_error, print_info, print_success, set_machine_readable,
    ChunkedProgress, FlashProgress, SimpleProgressBar, Spinner,
};
use usb_transport::UsbTransport;

const USB_SMART_ROUTE_MSG: &str =
    "   USB 端口被占用且智能路由失败。请检查设备是否已授权，或尝试以管理员身份运行。";

#[cfg(unix)]
struct KsudSilence {
    _stdout: Option<gag::Gag>,
    _stderr: Option<gag::Gag>,
}

#[cfg(unix)]
impl KsudSilence {
    fn new(enable: bool) -> Self {
        if !enable {
            return Self {
                _stdout: None,
                _stderr: None,
            };
        }
        Self {
            _stdout: gag::Gag::stdout().ok(),
            _stderr: gag::Gag::stderr().ok(),
        }
    }
}

#[cfg(not(unix))]
struct KsudSilence;

#[cfg(not(unix))]
impl KsudSilence {
    fn new(_enable: bool) -> Self {
        Self
    }
}

#[tokio::main]
async fn main() {
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.contains(&"--remove".to_string()) && raw_args.contains(&"forward".to_string()) {
        std::process::exit(0);
    }
    if raw_args.contains(&"--remove".to_string()) && raw_args.contains(&"reverse".to_string()) {
        std::process::exit(0);
    }

    env_logger::init();

    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        print_error(&e.to_string());

        if let Some(hint) = e.recovery_hint() {
            eprintln!("\n{}", hint);
        }

        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), FastbootError> {
    if cli.json {
        progress::set_machine_readable(true);
    }

    match cli.command {
        Commands::Devices { long } => {
            cmd_devices(long)?;
        }
        Commands::Getvar { variable } => {
            cmd_getvar(&cli.serial, &variable, cli.verbose).await?;
        }
        Commands::Flash {
            partition,
            filename,
            extra_args,
        } => {
            let (patch, kmi) = parse_flash_extra_args(&extra_args)?;
            cmd_flash(
                &cli.serial,
                &partition,
                &filename,
                cli.verbose,
                patch.as_deref(),
                kmi.as_deref(),
            )
            .await?;
        }
        Commands::BootRestore(args) => {
            cmd_boot_restore(args, cli.verbose)?;
        }
        Commands::GetKmi(args) => {
            cmd_get_kmi(args)?;
        }
        Commands::Erase { partition } => {
            cmd_erase(&cli.serial, &partition).await?;
        }
        Commands::Reboot { target } => {
            cmd_reboot(&cli.serial, &target).await?;
        }
        Commands::Flashall { wipe } => {
            cmd_flashall(&cli.serial, wipe).await?;
        }
        Commands::Update { filename } => {
            cmd_update(&cli.serial, &filename).await?;
        }
        Commands::Upload {
            partition,
            filename,
        } => {
            cmd_upload(&cli.serial, &partition, &filename).await?;
        }
        Commands::Diagnose => {
            cmd_diagnose().await?;
        }
        Commands::Shell { command } => {
            if !command.is_empty() {
                let cmd_str = command.join(" ");
                if cmd_str == "ip route" {
                    use adb::smartsocket::AdbSmartSocket;
                    let mut socket =
                        AdbSmartSocket::new().map_err(|e| FastbootError::Adb(e.to_string()))?;
                    let result = socket
                        .get_ip_route(cli.serial.as_deref())
                        .map_err(|e| FastbootError::Adb(e.to_string()))?;
                    println!("{}", result);
                    return Ok(());
                } else if cmd_str.contains("app_process") {
                    use adb::smartsocket::AdbSmartSocket;
                    let mut socket =
                        AdbSmartSocket::new().map_err(|e| FastbootError::Adb(e.to_string()))?;
                    socket
                        .shell_daemon(cli.serial.as_deref(), &cmd_str)
                        .map_err(|e| FastbootError::Adb(e.to_string()))?;
                    return Ok(());
                }
            } else {
                use adb::smartsocket::AdbSmartSocket;
                let mut socket =
                    AdbSmartSocket::new().map_err(|e| FastbootError::Adb(e.to_string()))?;
                socket
                    .shell_interactive(cli.serial.as_deref())
                    .map_err(|e| FastbootError::Adb(e.to_string()))?;
                return Ok(());
            }
            cmd_adb_shell(&cli.serial, &command, cli.verbose).await?;
        }
        Commands::Push { local, remote } => {
            use adb::smartsocket::AdbSmartSocket;
            let mut socket =
                AdbSmartSocket::new().map_err(|e| FastbootError::Adb(e.to_string()))?;
            socket
                .push(cli.serial.as_deref(), &local, &remote)
                .map_err(|e| FastbootError::Adb(e.to_string()))?;
        }
        Commands::Pull { remote, local } => {
            cmd_adb_pull(&cli.serial, &remote, &local, cli.verbose).await?;
        }
        Commands::Install { apk, replace } => {
            cmd_adb_install(&cli.serial, &apk, replace).await?;
        }
        Commands::Uninstall { package } => {
            cmd_adb_uninstall(&cli.serial, &package).await?;
        }
        Commands::Packages {
            third_party,
            system,
        } => {
            cmd_adb_packages(&cli.serial, third_party, system).await?;
        }
        Commands::Logcat { filter } => {
            cmd_adb_logcat(&cli.serial, &filter).await?;
        }
        Commands::Screencap { output } => {
            cmd_adb_screencap(&cli.serial, &output).await?;
        }
        Commands::Screenrecord { output, time } => {
            cmd_adb_screenrecord(&cli.serial, &output, time).await?;
        }
        Commands::Forward {
            remove,
            local,
            remote,
        } => {
            if remove {
                std::process::exit(0);
            }
            use adb::smartsocket::AdbSmartSocket;
            let mut socket =
                AdbSmartSocket::new().map_err(|e| FastbootError::Adb(e.to_string()))?;
            socket
                .forward(cli.serial.as_deref(), &local, &remote)
                .map_err(|e| FastbootError::Adb(e.to_string()))?;
            println!("端口转发设置成功");
        }
        Commands::Reverse {
            remove,
            remote,
            local,
        } => {
            if remove {
                std::process::exit(0);
            }
            use adb::smartsocket::AdbSmartSocket;
            let mut socket =
                AdbSmartSocket::new().map_err(|e| FastbootError::Adb(e.to_string()))?;
            let result = socket
                .reverse(cli.serial.as_deref(), &remote, &local)
                .map_err(|e| FastbootError::Adb(e.to_string()))?;
            println!("{}", result);
        }
        Commands::Tcpip { port } => {
            use adb::smartsocket::AdbSmartSocket;
            let mut socket =
                AdbSmartSocket::new().map_err(|e| FastbootError::Adb(e.to_string()))?;
            socket
                .tcpip(cli.serial.as_deref(), port)
                .map_err(|e| FastbootError::Adb(e.to_string()))?;
            println!("TCP/IP模式已开启，端口: {}", port);
        }
        Commands::Connect { target } => {
            use adb::smartsocket::AdbSmartSocket;
            let mut socket =
                AdbSmartSocket::new().map_err(|e| FastbootError::Adb(e.to_string()))?;
            let parts: Vec<&str> = target.split(':').collect();
            if parts.len() != 2 {
                return Err(FastbootError::InvalidArg(
                    "无效的连接目标格式，应为 ip:port".into(),
                ));
            }
            let ip = parts[0];
            let port: u16 = parts[1]
                .parse()
                .map_err(|_| FastbootError::InvalidArg("无效的端口".into()))?;
            let result = socket
                .connect(ip, port)
                .map_err(|e| FastbootError::Adb(e.to_string()))?;
            println!("{}", result);
        }
        Commands::Auth => {
            cmd_adb_auth(&cli.serial).await?;
        }
        Commands::SetActive { slot } => {
            cmd_set_active(&cli.serial, &slot).await?;
        }
        Commands::Oem { command } => {
            cmd_oem(&cli.serial, &command).await?;
        }
        Commands::Flashing { operation } => {
            cmd_flashing(&cli.serial, &operation).await?;
        }
        Commands::Format {
            partition,
            fs_type,
            size,
        } => {
            print_info(&format!("format {} 暂未实现", partition));
        }
        Commands::Boot { kernel, ramdisk } => {
            print_info("boot 命令暂未实现");
        }
        Commands::Fetch { partition, output } => {
            cmd_upload(&cli.serial, &partition, &output).await?;
        }
        Commands::CreateLogicalPartition { name, size } => {
            cmd_create_logical_partition(&cli.serial, &name, size).await?;
        }
        Commands::DeleteLogicalPartition { name } => {
            cmd_delete_logical_partition(&cli.serial, &name).await?;
        }
        Commands::ResizeLogicalPartition { name, size } => {
            cmd_resize_logical_partition(&cli.serial, &name, size).await?;
        }
        Commands::SnapshotUpdate { operation } => {
            print_info(&format!("snapshot-update {} 暂未实现", operation));
        }
        Commands::Gsi { operation } => {
            print_info(&format!("gsi {} 暂未实现", operation));
        }
        Commands::WipeSuper { super_empty } => {
            print_info("wipe-super 暂未实现");
        }
        Commands::Stage { input } => {
            print_info("stage 暂未实现");
        }
        Commands::GetStaged { output } => {
            print_info("get_staged 暂未实现");
        }
        Commands::StartServer => {
            std::process::exit(0);
        }
        Commands::Version => {
            println!("Android Debug Bridge version 1.0.41\nVersion 34.0.5-10900879");
            return Ok(());
        }
    }

    Ok(())
}

fn cmd_devices(is_long: bool) -> Result<(), FastbootError> {
    use adb::smartsocket::send_adb_request;
    use std::io::Read;
    use std::net::TcpStream;

    let mut stream = TcpStream::connect("127.0.0.1:5037").unwrap();
    let req = if is_long {
        "host:devices-l"
    } else {
        "host:devices"
    };
    send_adb_request(&mut stream, req);

    let mut len_buf = [0u8; 4];
    if stream.read_exact(&mut len_buf).is_err() {
        eprintln!("与设备的连接意外断开");
        std::process::exit(1);
    }
    let len_str = std::str::from_utf8(&len_buf).unwrap_or("0000");
    let len = usize::from_str_radix(len_str, 16).unwrap_or(0);

    let mut payload = vec![0u8; len];
    if len > 0 {
        if stream.read_exact(&mut payload).is_err() {
            eprintln!("与设备的连接意外断开");
            std::process::exit(1);
        }
    }

    println!("List of devices attached");
    print!("{}", String::from_utf8_lossy(&payload));

    std::process::exit(0);
}
