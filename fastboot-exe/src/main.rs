mod adb;
mod adb_handler;
mod adb_protocol;
mod adb_router;
mod adb_winusb_transport;
mod cli;
mod crypto;
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

fn parse_flash_extra_args(
    extra: &[String],
) -> Result<(Option<PathBuf>, Option<String>), FastbootError> {
    if extra.is_empty() {
        return Ok((None, None));
    }
    if extra.first().map(|s| s.as_str()) != Some("patch") {
        return Err(FastbootError::InvalidArg(
            "语法应为：fastboot flash <分区> <镜像> patch <模块.ko> [kmi <KMI字符串>]".into(),
        ));
    }
    let ko = extra.get(1).ok_or_else(|| {
        FastbootError::InvalidArg("找不到指定的内核模块文件（patch 后须跟 .ko 路径）".into())
    })?;
    let ko_path = PathBuf::from(ko);
    let mut kmi: Option<String> = None;
    if extra.len() > 2 {
        if extra.len() < 4 || extra[2].as_str() != "kmi" {
            return Err(FastbootError::InvalidArg(
                "尾随参数无效；指定 KMI 须使用：kmi <字符串>".into(),
            ));
        }
        kmi = Some(
            extra
                .get(3)
                .ok_or_else(|| FastbootError::InvalidArg("kmi 后缺少 KMI 字符串".into()))?
                .clone(),
        );
        if extra.len() > 4 {
            return Err(FastbootError::InvalidArg("尾随参数过多".into()));
        }
    }
    Ok((Some(ko_path), kmi))
}

#[cfg(target_os = "windows")]
fn ensure_usb_handle_released() {
    let _ = std::process::Command::new("adb")
        .args(["kill-server"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/IM", "adb.exe", "/T"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    std::thread::sleep(std::time::Duration::from_millis(500));
}

#[cfg(not(target_os = "windows"))]
fn ensure_usb_handle_released() {}

async fn resolve_boot_ab_partition_name(
    driver: &mut driver::FastbootDriver<UsbTransport>,
    partition: &str,
) -> Result<String, FastbootError> {
    if partition != "boot" && partition != "init_boot" {
        return Ok(partition.to_string());
    }
    match driver.get_var("current-slot").await {
        Ok(slot) => {
            let s = slot.trim().to_lowercase();
            if s == "a" || s == "b" {
                Ok(format!("{partition}_{s}"))
            } else {
                Ok(partition.to_string())
            }
        }
        Err(_) => Ok(partition.to_string()),
    }
}

fn cmd_get_kmi(args: ksud::boot_patch::GetKmiArgs) -> Result<(), FastbootError> {
    ksud::boot_patch::get_kmi(args).map_err(|e| {
        FastbootError::InvalidArg(format!("无法提取 KMI 版本，请检查镜像是否合法：{e}"))
    })
}

fn cmd_boot_restore(
    args: ksud::boot_patch::BootRestoreArgs,
    verbose: bool,
) -> Result<(), FastbootError> {
    let _silence = KsudSilence::new(!verbose);
    ksud::boot_patch::restore(args)
        .map_err(|e| FastbootError::InvalidArg(format!("KernelSU 还原失败：{e}")))?;
    println!("还原完成。");
    Ok(())
}

#[tokio::main]
async fn main() {
    env_logger::init();

    if let Some(exit_code) = adb_router::try_handle_adb_args() {
        std::process::exit(exit_code);
    }

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
        Commands::Devices => {
            println!("List of devices attached");
            match crate::adb_winusb_transport::AdbWinUsbDevice::enumerate() {
                Ok(devices) => {
                    for info in devices {
                        let device_path = info.device_path.clone();
                        match crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                            &device_path,
                        ) {
                            Ok(mut dev) => match dev.connect() {
                                Ok(_) => {
                                    let serial = extract_serial_from_path(&device_path);
                                    println!("{}\tdevice", serial);
                                }
                                Err(_) => {
                                    let serial = extract_serial_from_path(&device_path);
                                    println!("{}\tunauthorized", serial);
                                }
                            },
                            Err(_) => {
                                let serial = extract_serial_from_path(&device_path);
                                println!("{}\tunauthorized", serial);
                            }
                        }
                    }
                }
                Err(_) => {}
            }
            std::process::exit(0);
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
            let adb_target = match target.as_str() {
                "fb" | "bootloader" => "bootloader",
                "fbd" | "fastboot" => "fastboot",
                "rec" | "recovery" => "recovery",
                "sid" | "sideload" => "sideload",
                "edl" => "edl",
                _ => "",
            };

            let target_clone = target.clone();
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(1500),
                cmd_reboot(&cli.serial, &target_clone),
            )
            .await;

            let adb_devices =
                crate::adb_winusb_transport::AdbWinUsbDevice::enumerate().unwrap_or_default();
            if !adb_devices.is_empty() {
                if let Ok(mut dev) = crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                    &adb_devices[0].device_path,
                ) {
                    let _ = dev.reboot(adb_target);
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(500));
            std::process::exit(0);
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
            let adb_devices =
                crate::adb_winusb_transport::AdbWinUsbDevice::enumerate().unwrap_or_default();
            if adb_devices.is_empty() {
                eprintln!("未检测到处于 ADB 模式的设备");
                std::process::exit(1);
            }
            if let Ok(mut dev) = crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                &adb_devices[0].device_path,
            ) {
                let is_interactive = command.is_empty();

                if is_interactive {
                    if let Err(e) = dev.true_pty_shell() {
                        eprintln!("终端异常 {}", e);
                    }
                } else {
                    let cmd_string = command.join(" ");
                    match dev.shell_command(&cmd_string) {
                        Ok(output) => print!("{}", output),
                        Err(e) => eprintln!("执行失败 {}", e),
                    }
                }
            } else {
                eprintln!("无法打开物理句柄");
            }
            std::process::exit(0);
        }
        Commands::Push { local, remote } => {
            let local_str = local.to_str().unwrap_or("");
            let remote_str = remote.as_str();
            if let Ok(adb_devices) = crate::adb_winusb_transport::AdbWinUsbDevice::enumerate() {
                if let Some(dev_info) = adb_devices.into_iter().next() {
                    if let Ok(mut dev) = crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                        &dev_info.device_path,
                    ) {
                        match dev.push(local_str, remote_str) {
                            Ok(_) => println!("Push 成功"),
                            Err(e) => eprintln!("Push 失败 {}", e),
                        }
                        std::process::exit(0);
                    }
                }
            }
            eprintln!("未检测到处于 ADB 模式的设备");
        }
        Commands::Pull { remote, local } => {
            let adb_devices =
                crate::adb_winusb_transport::AdbWinUsbDevice::enumerate().unwrap_or_default();
            if adb_devices.is_empty() {
                eprintln!("未检测到处于 ADB 模式的设备");
                std::process::exit(1);
            }

            if let Ok(mut dev) = crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                &adb_devices[0].device_path,
            ) {
                let local_path = local.to_string_lossy().to_string();

                let final_path = if local_path.is_empty() {
                    let path = std::path::Path::new(&remote);
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                } else {
                    local_path
                };

                let final_path = if final_path.is_empty() {
                    "pulled_file.bin".to_string()
                } else {
                    final_path
                };

                match dev.pull(&remote, &final_path) {
                    Ok(_) => println!("文件已保存至 {}", final_path),
                    Err(e) => eprintln!("拉取失败 {}", e),
                }
            } else {
                eprintln!("无法打开物理设备句柄");
            }
            std::process::exit(0);
        }
        Commands::Install { apk, replace } => {
            let apk_str = apk.to_str().unwrap_or("");
            if let Ok(adb_devices) = crate::adb_winusb_transport::AdbWinUsbDevice::enumerate() {
                if let Some(dev_info) = adb_devices.into_iter().next() {
                    if let Ok(mut dev) = crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                        &dev_info.device_path,
                    ) {
                        match dev.install(apk_str) {
                            Ok(_) => println!("安装成功"),
                            Err(e) => eprintln!("安装失败 {}", e),
                        }
                        std::process::exit(0);
                    }
                }
            }
            eprintln!("未检测到处于 ADB 模式的设备");
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
            let adb_devices =
                crate::adb_winusb_transport::AdbWinUsbDevice::enumerate().unwrap_or_default();
            if adb_devices.is_empty() {
                eprintln!("未检测到处于 ADB 模式的设备");
                std::process::exit(1);
            }

            if let Ok(mut dev) = crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                &adb_devices[0].device_path,
            ) {
                let mut grep_keyword = None;
                if !filter.is_empty() {
                    let clean_args: Vec<String> = filter
                        .into_iter()
                        .filter(|s| s != "-s" && s != "--grep")
                        .collect();

                    if !clean_args.is_empty() {
                        grep_keyword = Some(clean_args[0].clone());
                    }
                }

                if let Err(e) = dev.logcat(grep_keyword.as_deref()) {
                    eprintln!("{}", e);
                }
            } else {
                eprintln!("无法打开物理设备句柄");
            }
            std::process::exit(0);
        }
        Commands::Screencap { output } => {
            let output_str = output.to_str().unwrap_or("screencap.png");
            if let Ok(adb_devices) = crate::adb_winusb_transport::AdbWinUsbDevice::enumerate() {
                if let Some(dev_info) = adb_devices.into_iter().next() {
                    match crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                        &dev_info.device_path,
                    ) {
                        Ok(mut opened_dev) => {
                            if let Err(e) = opened_dev.screencap(output_str) {
                                eprintln!("截图流拉取失败 {}", e);
                            }
                            std::process::exit(0);
                        }
                        Err(e) => eprintln!("无法打开 ADB 物理句柄 {:?}", e),
                    }
                }
            } else {
                eprintln!("未检测到处于 ADB 模式的设备");
            }
        }
        Commands::Jietu => {
            let filename = "screenshot.png".to_string();
            if let Ok(adb_devices) = crate::adb_winusb_transport::AdbWinUsbDevice::enumerate() {
                if let Some(dev_info) = adb_devices.into_iter().next() {
                    if let Ok(mut dev) = crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                        &dev_info.device_path,
                    ) {
                        let _ = dev.screencap(&filename);
                    }
                }
            }
            std::process::exit(0);
        }
        Commands::VolUp | Commands::VolDown | Commands::LockScreen => {
            let keycode = match cli.command {
                Commands::VolUp => "24",
                Commands::VolDown => "25",
                Commands::LockScreen => "26",
                _ => unreachable!(),
            };
            if let Ok(adb_devices) = crate::adb_winusb_transport::AdbWinUsbDevice::enumerate() {
                if let Some(dev_info) = adb_devices.into_iter().next() {
                    if let Ok(mut dev) = crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                        &dev_info.device_path,
                    ) {
                        let shell_cmd = format!("input keyevent {}", keycode);
                        match dev.shell_command(&shell_cmd) {
                            Ok(_) => println!("底层按键宏执行成功"),
                            Err(e) => eprintln!("执行失败 {}", e),
                        }
                    }
                }
            }
            std::process::exit(0);
        }
        Commands::Custom { cmd } => match cmd.as_str() {
            "截图" => {
                let filename = "screenshot.png".to_string();
                if let Ok(adb_devices) = crate::adb_winusb_transport::AdbWinUsbDevice::enumerate() {
                    if let Some(dev_info) = adb_devices.into_iter().next() {
                        if let Ok(mut dev) =
                            crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                                &dev_info.device_path,
                            )
                        {
                            let _ = dev.screencap(&filename);
                        }
                    }
                }
                std::process::exit(0);
            }
            "音量加" | "音量减" | "锁屏" | "亮屏" => {
                let keycode = match cmd.as_str() {
                    "音量加" => "24",
                    "音量减" => "25",
                    "锁屏" | "亮屏" => "26",
                    _ => "",
                };
                if let Ok(adb_devices) = crate::adb_winusb_transport::AdbWinUsbDevice::enumerate() {
                    if let Some(dev_info) = adb_devices.into_iter().next() {
                        if let Ok(mut dev) =
                            crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                                &dev_info.device_path,
                            )
                        {
                            let shell_cmd = format!("input keyevent {}", keycode);
                            match dev.shell_command(&shell_cmd) {
                                Ok(_) => println!("{} 执行成功", cmd),
                                Err(e) => eprintln!("执行失败 {}", e),
                            }
                        }
                    }
                }
                std::process::exit(0);
            }
            _ => {
                eprintln!("未知的中文指令 {}", cmd);
                std::process::exit(1);
            }
        },
        Commands::Screenrecord { output, time } => {
            cmd_adb_screenrecord(&cli.serial, &output, time).await?;
        }
        Commands::SetActive { slot } => {
            cmd_set_active(&cli.serial, &slot).await?;
        }
        Commands::Root => {
            let adb_devices =
                crate::adb_winusb_transport::AdbWinUsbDevice::enumerate().unwrap_or_default();
            if adb_devices.is_empty() {
                eprintln!("未检测到处于 ADB 模式的设备");
                std::process::exit(1);
            }
            if let Ok(mut dev) = crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                &adb_devices[0].device_path,
            ) {
                let _ = dev.root();
                println!("请等待 3-5 秒 让设备重新连接到电脑");
            }
            std::process::exit(0);
        }
        Commands::Unroot => {
            let adb_devices =
                crate::adb_winusb_transport::AdbWinUsbDevice::enumerate().unwrap_or_default();
            if adb_devices.is_empty() {
                eprintln!("未检测到处于 ADB 模式的设备");
                std::process::exit(1);
            }
            if let Ok(mut dev) = crate::adb_winusb_transport::AdbWinUsbDevice::open_device(
                &adb_devices[0].device_path,
            ) {
                let _ = dev.unroot();
                println!("请等待 3-5 秒 让设备重新连接到电脑");
            }
            std::process::exit(0);
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
    }

    Ok(())
}

async fn cmd_devices() -> Result<(), FastbootError> {
    use crate::adb_winusb_transport::AdbWinUsbDevice;

    println!("List of devices attached");

    match AdbWinUsbDevice::enumerate() {
        Ok(devices) => {
            for info in devices {
                let device_path = info.device_path.clone();
                match AdbWinUsbDevice::open_device(&device_path) {
                    Ok(mut dev) => match dev.connect() {
                        Ok(_) => {
                            let serial = extract_serial_from_path(&device_path);
                            println!("{}\tdevice", serial);
                        }
                        Err(e) => {
                            let serial = extract_serial_from_path(&device_path);
                            println!("{}\tunauthorized", serial);
                            eprintln!("底层 connect 退出原因 {}", e);
                        }
                    },
                    Err(_) => {
                        let serial = extract_serial_from_path(&device_path);
                        println!("{}\tunauthorized", serial);
                    }
                }
            }
        }
        Err(_) => {}
    }

    let fastboot_devices = UsbTransport::enumerate_devices().map_err(FastbootError::Transport)?;
    for dev in &fastboot_devices {
        let mode = get_fastboot_mode(&dev.serial_number).await;
        println!("{}\t{}", dev.serial_number, mode);
    }

    Ok(())
}

fn extract_serial_from_path(path: &str) -> String {
    path.split('\\').last().unwrap_or("unknown").to_string()
}

async fn get_fastboot_mode(serial: &str) -> &'static str {
    let transport = match UsbTransport::open(Some(serial)) {
        Ok(t) => t,
        Err(_) => return "fastboot",
    };

    let mut driver = driver::FastbootDriver::new(transport);

    match driver.get_var("is-userspace").await {
        Ok(val) if val.trim().eq_ignore_ascii_case("yes") => "fastboot (fastbootd)",
        _ => "fastboot",
    }
}

fn get_adb_device_mode(serial: &str) -> &'static str {
    use adb::client::AdbClient;

    let _ = crate::crypto::get_or_create_keys();

    let client = match AdbClient::connect_fast(Some(serial), false) {
        Ok(c) => c,
        Err(e) => {
            let err_msg = e.to_string();
            if err_msg.contains("unauthorized") {
                return "unauthorized";
            }
            return "device";
        }
    };

    let mut client = client;

    if let Ok(twrp_boot) = client.shell("getprop ro.twrp.boot") {
        if twrp_boot.trim() == "1" {
            return "recovery";
        }
    }

    if let Ok(bootmode) = client.shell("getprop ro.bootmode") {
        let bootmode = bootmode.trim().to_lowercase();
        if bootmode.contains("recovery") {
            return "recovery";
        }
        if bootmode.contains("charger") {
            return "charger";
        }
    }

    if let Ok(usb_state) = client.shell("getprop sys.usb.state") {
        if usb_state.trim().contains("sideload") {
            return "sideload";
        }
    }

    "device"
}

async fn cmd_getvar(
    serial: &Option<String>,
    variable: &str,
    verbose: bool,
) -> Result<(), FastbootError> {
    let transport = open_transport(serial).await?;
    let mut driver = driver::FastbootDriver::new(transport);

    if verbose {
        print_info(&format!(
            "连接到设备: {}",
            serial.as_deref().unwrap_or("(auto)")
        ));
    }

    if variable == "all" {
        let vars = driver.get_var_all().await?;
        for var in vars {
            println!("{}", var);
        }
    } else {
        let value = driver.get_var(variable).await?;
        println!("{}: {}", variable, value);
    }

    Ok(())
}

struct PatchArtifactGuard {
    inner: Option<(tempfile::TempDir, PathBuf)>,
}

impl Drop for PatchArtifactGuard {
    fn drop(&mut self) {
        if let Some((dir, path)) = self.inner.take() {
            let _ = fs::remove_file(&path);
            drop(dir);
        }
    }
}

async fn cmd_flash(
    serial: &Option<String>,
    partition: &str,
    filename: &Path,
    verbose: bool,
    patch_module: Option<&Path>,
    kmi_override: Option<&str>,
) -> Result<(), FastbootError> {
    const PATCHED_IMG_NAME: &str = "temp_patched_for_flash.img";

    let used_kernel_su_patch = patch_module.is_some();

    if !filename.exists() {
        return Err(FastbootError::ImageNotFound(filename.display().to_string()));
    }

    let mut patch_guard = PatchArtifactGuard { inner: None };

    if let Some(ko_path) = patch_module {
        if !ko_path.exists() {
            return Err(FastbootError::InvalidArg(format!(
                " ：找不到指定的内核模块文件：{}",
                ko_path.display()
            )));
        }

        let dir = tempfile::TempDir::new().map_err(FastbootError::Io)?;
        let out_dir = dir.path().to_path_buf();

        let args = ksud::boot_patch::BootPatchArgs::for_embedded_flash(
            filename.to_path_buf(),
            ko_path.to_path_buf(),
            out_dir,
            PATCHED_IMG_NAME.to_string(),
            kmi_override.map(str::to_owned),
            verbose,
        );

        {
            let _silence = KsudSilence::new(!verbose);
            ksud::boot_patch::patch(args).map_err(|e| {
                FastbootError::InvalidArg(format!(
                    " ：KernelSU 修补失败（已中止刷机，未向设备写入数据）：{e}"
                ))
            })?;
        }

        let out_path = dir.path().join(PATCHED_IMG_NAME);
        if !out_path.is_file() {
            return Err(FastbootError::InvalidArg(
                "修补流程未生成预期输出镜像，已中止刷机".into(),
            ));
        }

        patch_guard.inner = Some((dir, out_path));
    }

    let image_to_flash: &Path = match patch_guard.inner.as_ref() {
        Some((_, p)) => p.as_path(),
        None => filename,
    };

    let file_size = fs::metadata(image_to_flash)
        .map_err(FastbootError::Io)?
        .len();

    if file_size == 0 {
        return Err(FastbootError::InvalidArg("镜像文件为空".to_string()));
    }

    let transport = open_transport(serial).await?;
    let mut driver = driver::FastbootDriver::new(transport);

    let partition_resolved = resolve_boot_ab_partition_name(&mut driver, partition).await?;

    let max_download_size = driver
        .get_max_download_size()
        .await
        .unwrap_or(512 * 1024 * 1024);

    if verbose {
        print_info(&format!("文件: {}", image_to_flash.display()));
        print_info(&format!("大小: {}", format_size(file_size)));
        print_info(&format!("设备限制: {}", format_size(max_download_size)));
    }

    let is_sparse = sparse::is_sparse_file(image_to_flash).unwrap_or(false);

    use std::time::Instant;
    let start_time = Instant::now();

    let pb = ProgressBar::new(file_size);
    pb.enable_steady_tick(std::time::Duration::from_millis(50));
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:50.cyan/blue}] {bytes:>10}/{total_bytes:10} ({eta})")
        .unwrap()
        .progress_chars("█▉▊▋▌▍▎▏  "));

    use std::sync::Arc;
    let pb_arc = Arc::new(pb);
    let pb_clone = pb_arc.clone();

    driver.set_progress_callback(Box::new(move |sent, total| {
        pb_clone.inc(sent);
    }));

    let flash_result = if file_size > max_download_size {
        if is_sparse {
            flash_sparse_resparse(
                &mut driver,
                partition_resolved.as_str(),
                image_to_flash,
                max_download_size,
            )
            .await
        } else {
            flash_raw_chunked(
                &mut driver,
                partition_resolved.as_str(),
                image_to_flash,
                max_download_size,
            )
            .await
        }
    } else {
        flash_single(
            &mut driver,
            partition_resolved.as_str(),
            image_to_flash,
            file_size,
            max_download_size,
        )
        .await
    };

    let out = flash_result;
    drop(patch_guard);
    if out.is_ok() {
        pb_arc.finish_and_clear();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let elapsed = start_time.elapsed().as_secs_f64();
        println!("OKAY [{:>7.3}s]", elapsed);
        if used_kernel_su_patch {
            println!("修补并刷写完成。");
        }
    } else {
        pb_arc.finish_and_clear();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    out
}

async fn flash_sparse_resparse(
    driver: &mut driver::FastbootDriver<UsbTransport>,
    partition: &str,
    filename: &Path,
    max_download_size: u64,
) -> Result<(), FastbootError> {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    use sparse::{CHUNK_HEADER_SIZE, SPARSE_HEADER_MAGIC, SPARSE_HEADER_SIZE};

    let mut file = File::open(filename).map_err(FastbootError::Io)?;
    let file_len = file.metadata().map_err(FastbootError::Io)?.len();

    // 1. 循环解析：支持“首尾拼接”的巨型 Sparse 镜像
    let mut chunk_metas = Vec::new();
    let mut offset = 0u64;
    let mut global_total_blocks = 0u32;
    let mut global_block_size = 4096u32;

    while offset < file_len {
        file.seek(SeekFrom::Start(offset)).map_err(FastbootError::Io)?;
        let mut header_buf = [0u8; SPARSE_HEADER_SIZE];

        // 如果读不到 28 字节，说明已经到文件尾部或全是 padding 00，安全退出解析
        if file.read_exact(&mut header_buf).is_err() {
            break;
        }

        let magic = u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]);
        if magic != SPARSE_HEADER_MAGIC {
            // 遇到非魔数数据，说明拼接的有效 Sparse 块已经全部提取完毕
            break;
        }

        let block_size = u32::from_le_bytes([header_buf[12], header_buf[13], header_buf[14], header_buf[15]]);
        global_block_size = block_size;
        let total_blocks = u32::from_le_bytes([header_buf[16], header_buf[17], header_buf[18], header_buf[19]]);
        let total_chunks = u32::from_le_bytes([header_buf[20], header_buf[21], header_buf[22], header_buf[23]]);

        global_total_blocks += total_blocks;
        offset += SPARSE_HEADER_SIZE as u64;

        // 提取当前 Sparse 片段内的所有 Chunk
        for _ in 0..total_chunks {
            file.seek(SeekFrom::Start(offset)).map_err(FastbootError::Io)?;
            let mut ch_buf = [0u8; CHUNK_HEADER_SIZE];
            file.read_exact(&mut ch_buf).map_err(FastbootError::Io)?;

            let ctype = u16::from_le_bytes([ch_buf[0], ch_buf[1]]);
            let cblocks = u32::from_le_bytes([ch_buf[4], ch_buf[5], ch_buf[6], ch_buf[7]]);
            let ctotal_sz = u32::from_le_bytes([ch_buf[8], ch_buf[9], ch_buf[10], ch_buf[11]]);
            let data_sz = ctotal_sz.saturating_sub(CHUNK_HEADER_SIZE as u32);

            chunk_metas.push((ctype, cblocks, data_sz, offset + CHUNK_HEADER_SIZE as u64));
            offset += ctotal_sz as u64;
        }
    }

    if chunk_metas.is_empty() {
        return Err(FastbootError::Protocol("非 Sparse 文件或文件已损坏".to_string()));
    }

    // 2. 将所有解析出来的 Chunk 合并处理并发包
    let mut absolute_block_offset = 0u32;
    let mut current_chunk_idx = 0;
    let mut chunk_internal_block_offset = 0u32;

    while current_chunk_idx < chunk_metas.len() {
        let mut session_buffer = Vec::new();
        let mut session_blocks = 0u32;
        let mut session_chunks = 0u32;

        session_buffer.resize(SPARSE_HEADER_SIZE, 0);

        // 如果不是第一片，插入 DontCare 块跳过已刷入的物理偏移
        if absolute_block_offset > 0 {
            session_buffer.extend_from_slice(&0xCAC3u16.to_le_bytes());
            session_buffer.extend_from_slice(&0u16.to_le_bytes());
            session_buffer.extend_from_slice(&absolute_block_offset.to_le_bytes());
            session_buffer.extend_from_slice(&(CHUNK_HEADER_SIZE as u32).to_le_bytes());

            session_blocks += absolute_block_offset;
            session_chunks += 1;
        }

        while current_chunk_idx < chunk_metas.len() {
            let (ctype, cblocks, data_sz, data_offset) = chunk_metas[current_chunk_idx];
            let blocks_remaining = cblocks - chunk_internal_block_offset;

            let chunk_header_overhead = CHUNK_HEADER_SIZE as u64;
            let available_payload = max_download_size.saturating_sub(session_buffer.len() as u64);

            if available_payload <= chunk_header_overhead {
                break;
            }

            let available_data_payload = available_payload - chunk_header_overhead;

            if ctype == 0xCAC1 {
                let max_blocks_can_fit = (available_data_payload / global_block_size as u64) as u32;
                if max_blocks_can_fit == 0 {
                    break;
                }

                let blocks_to_take = blocks_remaining.min(max_blocks_can_fit);
                let bytes_to_take = (blocks_to_take as usize) * (global_block_size as usize);

                session_buffer.extend_from_slice(&0xCAC1u16.to_le_bytes());
                session_buffer.extend_from_slice(&0u16.to_le_bytes());
                session_buffer.extend_from_slice(&blocks_to_take.to_le_bytes());
                let total_sz = CHUNK_HEADER_SIZE as u32 + bytes_to_take as u32;
                session_buffer.extend_from_slice(&total_sz.to_le_bytes());

                let physical_offset = data_offset + (chunk_internal_block_offset as u64 * global_block_size as u64);
                file.seek(SeekFrom::Start(physical_offset)).map_err(FastbootError::Io)?;
                let old_len = session_buffer.len();
                session_buffer.resize(old_len + bytes_to_take, 0);
                file.read_exact(&mut session_buffer[old_len..]).map_err(FastbootError::Io)?;

                session_blocks += blocks_to_take;
                session_chunks += 1;
                absolute_block_offset += blocks_to_take;
                chunk_internal_block_offset += blocks_to_take;

                if chunk_internal_block_offset < cblocks {
                    break;
                } else {
                    current_chunk_idx += 1;
                    chunk_internal_block_offset = 0;
                }
            } else {
                let bytes_to_take = data_sz as usize;
                if available_data_payload < bytes_to_take as u64 {
                    break;
                }

                session_buffer.extend_from_slice(&ctype.to_le_bytes());
                session_buffer.extend_from_slice(&0u16.to_le_bytes());
                session_buffer.extend_from_slice(&blocks_remaining.to_le_bytes());
                let total_sz = CHUNK_HEADER_SIZE as u32 + bytes_to_take as u32;
                session_buffer.extend_from_slice(&total_sz.to_le_bytes());

                if bytes_to_take > 0 {
                    file.seek(SeekFrom::Start(data_offset)).map_err(FastbootError::Io)?;
                    let old_len = session_buffer.len();
                    session_buffer.resize(old_len + bytes_to_take, 0);
                    file.read_exact(&mut session_buffer[old_len..]).map_err(FastbootError::Io)?;
                }

                session_blocks += blocks_remaining;
                session_chunks += 1;
                absolute_block_offset += blocks_remaining;

                current_chunk_idx += 1;
                chunk_internal_block_offset = 0;
            }
        }

        if session_chunks == 0 {
            return Err(FastbootError::Protocol("max_download_size 配置过小，无法装入数据包".to_string()));
        }

        // 补齐尾部
        if current_chunk_idx >= chunk_metas.len() {
            let trailing_blocks = global_total_blocks.saturating_sub(absolute_block_offset);
            if trailing_blocks > 0 {
                session_buffer.extend_from_slice(&0xCAC3u16.to_le_bytes());
                session_buffer.extend_from_slice(&0u16.to_le_bytes());
                session_buffer.extend_from_slice(&trailing_blocks.to_le_bytes());
                session_buffer.extend_from_slice(&(CHUNK_HEADER_SIZE as u32).to_le_bytes());
                session_blocks += trailing_blocks;
                session_chunks += 1;
                absolute_block_offset += trailing_blocks;
            }
        }

        // 组装 Sparse Header
        session_buffer[0..4].copy_from_slice(&SPARSE_HEADER_MAGIC.to_le_bytes());
        session_buffer[4..6].copy_from_slice(&1u16.to_le_bytes());
        session_buffer[6..8].copy_from_slice(&0u16.to_le_bytes());
        session_buffer[8..10].copy_from_slice(&(SPARSE_HEADER_SIZE as u16).to_le_bytes());
        session_buffer[10..12].copy_from_slice(&(CHUNK_HEADER_SIZE as u16).to_le_bytes());
        session_buffer[12..16].copy_from_slice(&global_block_size.to_le_bytes());
        session_buffer[16..20].copy_from_slice(&session_blocks.to_le_bytes());
        session_buffer[20..24].copy_from_slice(&session_chunks.to_le_bytes());
        session_buffer[24..28].copy_from_slice(&0u32.to_le_bytes());

        driver.download(&session_buffer).await?;
        driver.flash(partition).await?;
    }

    Ok(())
}

async fn flash_raw_chunked(
    driver: &mut driver::FastbootDriver<UsbTransport>,
    partition: &str,
    filename: &Path,
    max_download_size: u64,
) -> Result<(), FastbootError> {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};

    let mut file = File::open(filename).map_err(FastbootError::Io)?;
    let file_len = file.metadata().map_err(FastbootError::Io)?.len();
    let max_payload = max_download_size.saturating_sub(4096);
    let max_data_blocks = max_payload / 4096;
    let mut offset_bytes = 0u64;

    while offset_bytes < file_len {
        let remain = file_len - offset_bytes;
        let chunk_bytes = remain.min(max_data_blocks * 4096);
        let padded_chunk_bytes = (chunk_bytes + 4095) / 4096 * 4096;
        let data_blocks = (padded_chunk_bytes / 4096) as u32;
        let offset_blocks = (offset_bytes / 4096) as u32;
        let total_blocks = offset_blocks + data_blocks;
        let total_chunks: u32 = if offset_blocks > 0 { 2 } else { 1 };

        let mut header = Vec::with_capacity(28 + 12 * 2);
        header.extend_from_slice(&0xED26FF3Au32.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&28u16.to_le_bytes());
        header.extend_from_slice(&12u16.to_le_bytes());
        header.extend_from_slice(&4096u32.to_le_bytes());
        header.extend_from_slice(&total_blocks.to_le_bytes());
        header.extend_from_slice(&total_chunks.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());

        if offset_blocks > 0 {
            header.extend_from_slice(&0xCAC3u16.to_le_bytes());
            header.extend_from_slice(&0u16.to_le_bytes());
            header.extend_from_slice(&offset_blocks.to_le_bytes());
            header.extend_from_slice(&12u32.to_le_bytes());
        }

        header.extend_from_slice(&0xCAC1u16.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&data_blocks.to_le_bytes());
        let chunk_total_size = 12 + padded_chunk_bytes as u32;
        header.extend_from_slice(&chunk_total_size.to_le_bytes());

        file.seek(SeekFrom::Start(offset_bytes))
            .map_err(FastbootError::Io)?;

        let mut chunk_data = Vec::with_capacity(header.len() + padded_chunk_bytes as usize);
        chunk_data.extend_from_slice(&header);

        let mut buffer = vec![0u8; 1024 * 1024];
        let mut bytes_to_read = chunk_bytes;
        while bytes_to_read > 0 {
            let read_len = bytes_to_read.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..read_len])
                .map_err(FastbootError::Io)?;
            chunk_data.extend_from_slice(&buffer[..read_len]);
            bytes_to_read -= read_len as u64;
        }

        if chunk_bytes < padded_chunk_bytes {
            let pad_size = (padded_chunk_bytes - chunk_bytes) as usize;
            chunk_data.resize(chunk_data.len() + pad_size, 0);
        }

        driver.download(&chunk_data).await?;
        driver.flash(partition).await?;

        offset_bytes += chunk_bytes;
    }

    Ok(())
}

async fn flash_single(
    driver: &mut driver::FastbootDriver<UsbTransport>,
    partition: &str,
    filename: &Path,
    file_size: u64,
    max_download_size: u64,
) -> Result<(), FastbootError> {
    use progress::FlashProgress;

    let is_json = progress::is_machine_readable();

    if is_json {
        println!(
            r#"{{"type":"start","partition":"{}","size":{}}}"#,
            partition, file_size
        );
    }

    let mut data = fs::read(filename).map_err(FastbootError::Io)?;

    let is_sparse = data.len() >= 4 && data[0..4] == [0x3A, 0xFF, 0x26, 0xED];
    if !is_sparse && data.len() > max_download_size as usize {
        let block_size: u32 = 4096;
        let padded_len = (data.len() + 4095) / 4096 * 4096;
        let total_blocks = (padded_len / 4096) as u32;
        let mut sparse = Vec::with_capacity(28 + 12 + padded_len);
        sparse.extend_from_slice(&0xED26FF3Au32.to_le_bytes());
        sparse.extend_from_slice(&1u16.to_le_bytes());
        sparse.extend_from_slice(&0u16.to_le_bytes());
        sparse.extend_from_slice(&28u16.to_le_bytes());
        sparse.extend_from_slice(&12u16.to_le_bytes());
        sparse.extend_from_slice(&block_size.to_le_bytes());
        sparse.extend_from_slice(&total_blocks.to_le_bytes());
        sparse.extend_from_slice(&1u32.to_le_bytes());
        sparse.extend_from_slice(&0u32.to_le_bytes());
        sparse.extend_from_slice(&0xCAC1u16.to_le_bytes());
        sparse.extend_from_slice(&0u16.to_le_bytes());
        sparse.extend_from_slice(&total_blocks.to_le_bytes());
        let total_chunk_size = 12 + padded_len as u32;
        sparse.extend_from_slice(&total_chunk_size.to_le_bytes());
        sparse.extend_from_slice(&data);
        if data.len() < padded_len {
            sparse.resize(sparse.len() + (padded_len - data.len()), 0);
        }
        data = sparse;
    }

    if is_json {
        println!(
            r#"{{"type":"sending","partition":"{}","size":{}}}"#,
            partition, file_size
        );
    }

    driver.download(&data).await?;
    if is_json {
        println!(r#"{{"type":"writing","partition":"{}"}}"#, partition);
    }
    driver.flash(partition).await?;

    Ok(())
}

async fn flash_sparse_chunked(
    driver: &mut driver::FastbootDriver<UsbTransport>,
    partition: &str,
    filename: &Path,
    max_download_size: u64,
    _verbose: bool,
) -> Result<(), FastbootError> {
    use std::io::{Read, Seek, SeekFrom};

    const SPARSE_HEADER_SIZE: usize = 28;
    const CHUNK_HEADER_SIZE: usize = 12;
    const SPARSE_HEADER_MAGIC: u32 = 0xED26FF3A;
    const CHUNK_TYPE_RAW: u16 = 0xCAC1;
    const CHUNK_TYPE_FILL: u16 = 0xCAC2;
    const CHUNK_TYPE_DONT_CARE: u16 = 0xCAC3;
    const CHUNK_TYPE_CRC32: u16 = 0xCAC4;

    let mut file = fs::File::open(filename).map_err(FastbootError::Io)?;
    let file_size = file.metadata().map_err(FastbootError::Io)?.len();

    let mut header_buf = [0u8; SPARSE_HEADER_SIZE];
    file.read_exact(&mut header_buf)
        .map_err(FastbootError::Io)?;

    let magic = u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]);
    if magic != SPARSE_HEADER_MAGIC {
        return Err(FastbootError::InvalidArg("无效的 Sparse 镜像格式".into()));
    }

    let block_size = u32::from_le_bytes([
        header_buf[12],
        header_buf[13],
        header_buf[14],
        header_buf[15],
    ]);
    let total_blks = u32::from_le_bytes([
        header_buf[16],
        header_buf[17],
        header_buf[18],
        header_buf[19],
    ]);
    let total_chunks = u32::from_le_bytes([
        header_buf[20],
        header_buf[21],
        header_buf[22],
        header_buf[23],
    ]);

    let max_session_blocks =
        ((max_download_size - SPARSE_HEADER_SIZE as u64) / block_size as u64) as u32;

    let mut current_block: u32 = 0;
    let mut session_blocks: u32 = 0;
    let mut session_chunks: u32 = 0;
    let mut session_data: Vec<u8> = Vec::new();
    let mut pending_chunks: Vec<(u16, u32, Vec<u8>)> = Vec::new();

    let mut chunk_offset = SPARSE_HEADER_SIZE as u64;

    for _chunk_idx in 0..total_chunks {
        let prev_offset = chunk_offset;
        file.seek(SeekFrom::Start(chunk_offset))
            .map_err(FastbootError::Io)?;
        let mut chunk_header_buf = [0u8; CHUNK_HEADER_SIZE];
        file.read_exact(&mut chunk_header_buf)
            .map_err(FastbootError::Io)?;

        let chunk_type = u16::from_le_bytes([chunk_header_buf[0], chunk_header_buf[1]]);
        let chunk_blocks = u32::from_le_bytes([
            chunk_header_buf[4],
            chunk_header_buf[5],
            chunk_header_buf[6],
            chunk_header_buf[7],
        ]);
        let total_sz = u32::from_le_bytes([
            chunk_header_buf[8],
            chunk_header_buf[9],
            chunk_header_buf[10],
            chunk_header_buf[11],
        ]);
        let data_size = total_sz.saturating_sub(CHUNK_HEADER_SIZE as u32);

        let mut chunk_data = vec![0u8; data_size as usize];
        if data_size > 0 {
            file.read_exact(&mut chunk_data)
                .map_err(FastbootError::Io)?;
        }

        if chunk_offset == prev_offset {
            panic!(
                "致命错误：解析引擎陷入死循环，卡死在 offset: {}",
                chunk_offset
            );
        }

        let mut remaining_blocks = chunk_blocks;
        let mut data_offset = 0usize;

        while remaining_blocks > 0 {
            let fitting_blocks = max_session_blocks
                .saturating_sub(session_blocks)
                .min(remaining_blocks);

            if fitting_blocks == 0 {
                let session_size = SPARSE_HEADER_SIZE
                    + session_chunks as usize * CHUNK_HEADER_SIZE
                    + session_data.len();
                let mut session_buffer = Vec::with_capacity(session_size);

                session_buffer.extend_from_slice(&SPARSE_HEADER_MAGIC.to_le_bytes());
                session_buffer.extend_from_slice(&1u16.to_le_bytes());
                session_buffer.extend_from_slice(&0u16.to_le_bytes());
                session_buffer.extend_from_slice(&(SPARSE_HEADER_SIZE as u16).to_le_bytes());
                session_buffer.extend_from_slice(&(CHUNK_HEADER_SIZE as u16).to_le_bytes());
                session_buffer.extend_from_slice(&block_size.to_le_bytes());
                session_buffer.extend_from_slice(&session_blocks.to_le_bytes());
                session_buffer.extend_from_slice(&session_chunks.to_le_bytes());
                session_buffer.extend_from_slice(&0u32.to_le_bytes());

                for (ctype, blocks, data) in &pending_chunks {
                    session_buffer.extend_from_slice(&ctype.to_le_bytes());
                    session_buffer.extend_from_slice(&0u16.to_le_bytes());
                    session_buffer.extend_from_slice(&blocks.to_le_bytes());
                    let total_chunk_sz = (CHUNK_HEADER_SIZE as u32) + data.len() as u32;
                    session_buffer.extend_from_slice(&total_chunk_sz.to_le_bytes());
                    session_buffer.extend_from_slice(data);
                }

                driver.download(&session_buffer).await?;
                driver.flash(partition).await?;

                pending_chunks.clear();
                session_blocks = 0;
                session_chunks = 0;
                session_data.clear();
                continue;
            }

            let actual_chunk_type = if chunk_type == CHUNK_TYPE_DONT_CARE {
                CHUNK_TYPE_DONT_CARE
            } else if chunk_type == CHUNK_TYPE_FILL {
                CHUNK_TYPE_FILL
            } else if chunk_type == CHUNK_TYPE_CRC32 {
                CHUNK_TYPE_CRC32
            } else {
                CHUNK_TYPE_RAW
            };

            let chunk_data_slice =
                if actual_chunk_type == CHUNK_TYPE_RAW && remaining_blocks > fitting_blocks {
                    let data_len = (fitting_blocks as usize) * (block_size as usize);
                    let start = data_offset;
                    let end = start + data_len;
                    data_offset += data_len;
                    chunk_data[start..end].to_vec()
                } else if actual_chunk_type == CHUNK_TYPE_FILL {
                    chunk_data.clone()
                } else if actual_chunk_type == CHUNK_TYPE_RAW {
                    let data_len = (fitting_blocks as usize) * (block_size as usize);
                    chunk_data[data_offset..data_offset + data_len].to_vec()
                } else {
                    Vec::new()
                };

            pending_chunks.push((actual_chunk_type, fitting_blocks, chunk_data_slice));
            session_blocks += fitting_blocks;
            session_chunks += 1;
            remaining_blocks -= fitting_blocks;
        }

        chunk_offset += total_sz as u64;
    }

    if !pending_chunks.is_empty() {
        let session_size =
            SPARSE_HEADER_SIZE + session_chunks as usize * CHUNK_HEADER_SIZE + session_data.len();
        let mut session_buffer = Vec::with_capacity(session_size);

        session_buffer.extend_from_slice(&SPARSE_HEADER_MAGIC.to_le_bytes());
        session_buffer.extend_from_slice(&1u16.to_le_bytes());
        session_buffer.extend_from_slice(&0u16.to_le_bytes());
        session_buffer.extend_from_slice(&(SPARSE_HEADER_SIZE as u16).to_le_bytes());
        session_buffer.extend_from_slice(&(CHUNK_HEADER_SIZE as u16).to_le_bytes());
        session_buffer.extend_from_slice(&block_size.to_le_bytes());
        session_buffer.extend_from_slice(&session_blocks.to_le_bytes());
        session_buffer.extend_from_slice(&session_chunks.to_le_bytes());
        session_buffer.extend_from_slice(&0u32.to_le_bytes());

        for (ctype, blocks, data) in &pending_chunks {
            session_buffer.extend_from_slice(&ctype.to_le_bytes());
            session_buffer.extend_from_slice(&0u16.to_le_bytes());
            session_buffer.extend_from_slice(&blocks.to_le_bytes());
            let total_chunk_sz = (CHUNK_HEADER_SIZE as u32) + data.len() as u32;
            session_buffer.extend_from_slice(&total_chunk_sz.to_le_bytes());
            session_buffer.extend_from_slice(data);
        }

        driver.download(&session_buffer).await?;
        driver.flash(partition).await?;
    }

    Ok(())
}

async fn cmd_erase(serial: &Option<String>, partition: &str) -> Result<(), FastbootError> {
    let transport = open_transport(serial).await?;
    let mut driver = driver::FastbootDriver::new(transport);

    let mut spinner = Spinner::new(&format!("擦除 '{}'...", partition));

    for _ in 0..10 {
        spinner.tick();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    driver.erase(partition).await?;
    spinner.finish(&format!("擦除 {} 完成", partition));

    Ok(())
}

async fn cmd_reboot(serial: &Option<String>, target: &str) -> Result<(), FastbootError> {
    use adb::client::{adb_cli_reboot_proxy, AdbClient};

    let target_lower = target.to_lowercase();
    let target = match target_lower.as_str() {
        "" | "system" => "",
        "fb" | "bootloader" => "bootloader",
        "rec" | "recovery" => "recovery",
        "sideload" | "sideload-auto-reboot" => "sideload",
        "fbd" | "fastbootd" | "fastboot" => "fastboot",
        _ => target,
    };

    let target_serial = serial.as_deref();

    let adb_devices = AdbClient::enumerate_adb_devices().unwrap_or_default();
    let has_adb = adb_devices
        .iter()
        .any(|d| target_serial.map_or(true, |s| d.serial == s));

    if has_adb && !adb_devices.is_empty() {
        let mode = if target.is_empty() {
            None
        } else {
            Some(target)
        };
        let _ = crate::crypto::get_or_create_keys().expect("致命错误：无法生成或加载 RSA 密钥");
        match AdbClient::connect_with_auth(target_serial) {
            Ok(mut client) => {
                client
                    .reboot(mode)
                    .map_err(|e| FastbootError::Adb(e.to_string()))?;
            }
            Err(_) => {
                if adb_cli_reboot_proxy(&target_serial.map(|s| s.to_string()), Some(target), false)
                    .is_err()
                {
                    return Err(FastbootError::InvalidArg(USB_SMART_ROUTE_MSG.into()));
                }
            }
        }

        if target.is_empty() {
            println!("Rebooting...");
        } else {
            let display_name = if target == "fastboot" {
                "fastbootd"
            } else {
                target
            };
            println!("Rebooting to {}...", display_name);
        }
        return Ok(());
    }

    let fastboot_devices = UsbTransport::enumerate_devices().map_err(FastbootError::Transport)?;

    let has_fastboot = fastboot_devices
        .iter()
        .any(|d| target_serial.map_or(true, |s| d.serial_number == s));

    if has_fastboot && !fastboot_devices.is_empty() {
        let transport = open_transport(serial).await?;
        let mut driver = driver::FastbootDriver::new(transport);

        if target.is_empty() {
            println!("Rebooting...");
            driver.reboot().await?;
        } else {
            let display_name = if target == "fastboot" {
                "fastbootd"
            } else {
                target
            };
            println!("Rebooting to {}...", display_name);
            driver.reboot_to(target).await?;
        }
        return Ok(());
    }

    Err(FastbootError::NoDevice)
}

async fn cmd_flashall(serial: &Option<String>, wipe: bool) -> Result<(), FastbootError> {
    let transport = open_transport(serial).await?;
    let mut driver = driver::FastbootDriver::new(transport);

    print_info("准备刷写所有分区...");

    if wipe {
        print_info("将同时擦除 userdata 分区");
    }

    let vars = driver.get_var_all().await?;
    let mut var_map = std::collections::HashMap::new();
    for var in vars {
        if let Some((key, value)) = var.split_once(':') {
            var_map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    let part_mgr = partition::PartitionManager::from_device_vars(&var_map);

    print_info(&format!(
        "设备槽位: {}",
        if part_mgr.has_slot {
            format!(
                "A/B (当前: {})",
                part_mgr.current_slot.as_deref().unwrap_or("?")
            )
        } else {
            "单槽位".to_string()
        }
    ));

    let images = partition::get_standard_images();

    let mut found_images = Vec::new();
    for img in &images {
        let path = Path::new(&img.img_name);
        if path.exists() {
            found_images.push(img.clone());
            if !img.optional {
                print_info(&format!("找到: {}", img.img_name));
            }
        } else if !img.optional {
            return Err(FastbootError::ImageNotFound(img.img_name.clone()));
        }
    }

    if found_images.is_empty() {
        return Err(FastbootError::InvalidArg(
            "当前目录没有找到任何镜像文件".to_string(),
        ));
    }

    print_info(&format!("将刷写 {} 个分区", found_images.len()));

    let mut boot_critical: Vec<_> = found_images
        .iter()
        .filter(|i| i.image_type == partition::ImageType::BootCritical)
        .collect();
    let mut normal: Vec<_> = found_images
        .iter()
        .filter(|i| i.image_type == partition::ImageType::Normal)
        .collect();
    let mut extra: Vec<_> = found_images
        .iter()
        .filter(|i| i.image_type == partition::ImageType::Extra)
        .collect();

    let all_images: Vec<_> = boot_critical
        .drain(..)
        .chain(normal.drain(..))
        .chain(extra.drain(..))
        .collect();

    for (i, img) in all_images.iter().enumerate() {
        let part_name = part_mgr.get_partition_name(&img.part_name);
        let path = Path::new(&img.img_name);

        println!(
            "\n[{}/{}] 刷写 {} -> {}",
            i + 1,
            all_images.len(),
            img.img_name,
            part_name
        );

        let data = fs::read(path).map_err(FastbootError::Io)?;
        let mut pb = SimpleProgressBar::new(data.len() as u64, "Sending");

        driver.download(&data).await?;
        pb.update(data.len() as u64 / 2);

        driver.flash(&part_name).await?;
        pb.finish();
    }

    if wipe {
        println!("\n擦除 userdata...");
        driver.erase("userdata").await?;
        print_success("userdata 已擦除");
    }

    print_success("\n所有分区刷写完成!");
    Ok(())
}

async fn cmd_update(serial: &Option<String>, filename: &Path) -> Result<(), FastbootError> {
    if !filename.exists() {
        return Err(FastbootError::ImageNotFound(filename.display().to_string()));
    }

    print_info(&format!("从 {} 更新...", filename.display()));

    let source = flash::ZipImageSource::from_path(filename).map_err(FastbootError::Io)?;

    let plan = flash::FlashingPlan::default();
    let tool = flash::FlashAllTool::new(source, plan);

    tool.validate()?;

    let tasks = tool.generate_tasks().map_err(FastbootError::Io)?;

    if tasks.is_empty() {
        return Err(FastbootError::InvalidArg(
            "ZIP 包中没有找到可刷写的镜像".to_string(),
        ));
    }

    print_info(&format!("将刷写 {} 个分区", tasks.len()));

    let transport = open_transport(serial).await?;
    let mut driver = driver::FastbootDriver::new(transport);

    for (i, task) in tasks.iter().enumerate() {
        println!(
            "\n[{}/{}] 刷写 {} -> {}",
            i + 1,
            tasks.len(),
            task.filename,
            task.partition
        );

        let data = tool
            .source()
            .read_file(&task.filename)
            .map_err(FastbootError::Io)?;

        let mut pb = SimpleProgressBar::new(data.len() as u64, "Sending");

        driver.download(&data).await?;
        pb.update(data.len() as u64 / 2);

        driver.flash(&task.partition).await?;
        pb.finish();
    }

    print_success("\n更新完成!");
    Ok(())
}

async fn cmd_upload(
    serial: &Option<String>,
    partition: &str,
    filename: &Path,
) -> Result<(), FastbootError> {
    let transport = open_transport(serial).await?;
    let mut driver = driver::FastbootDriver::new(transport);

    print_info(&format!(
        "读取分区 '{}' 到 '{}'...",
        partition,
        filename.display()
    ));

    let size = driver.read_partition(partition, filename).await?;

    print_success(&format!("读取完成: {}", format_size(size)));
    Ok(())
}

async fn cmd_diagnose() -> Result<(), FastbootError> {
    println!("USB jiancha \n");

    println!("检查 USB 设备...");
    let devices = UsbTransport::enumerate_devices().map_err(FastbootError::Transport)?;

    if devices.is_empty() {
        println!("  未发现 fastboot 设备\n");
        println!("排查建议:");
        println!("  1. 确认设备已进入 fastboot 模式");
        println!("  2. 检查 USB 线连接");
        println!("  3. 尝试其他 USB 端口（推荐 USB 3.0）");
        #[cfg(target_os = "linux")]
        println!("  4. 检查 udev 规则");
        #[cfg(target_os = "windows")]
        println!("  4. 检查 fastboot 驱动是否已安装");
    } else {
        println!("  发现 {} 个设备:\n", devices.len());
        for dev in &devices {
            println!("  设备: {}", dev.serial_number);
            println!("    VID:PID = {:04x}:{:04x}", dev.vendor_id, dev.product_id);
            println!(
                "    USB 版本: {}",
                if dev.is_usb3 {
                    "3.0 (SuperSpeed)"
                } else {
                    "2.0 (High Speed)"
                }
            );
            if let Some(ref name) = dev.product_name {
                println!("    产品名: {}", name);
            }
            if let Some(ref mfr) = dev.manufacturer {
                println!("    制造商: {}", mfr);
            }
            println!();
        }

        if let Some(dev) = devices.first() {
            println!("尝试连接 {}...", dev.serial_number);
            match UsbTransport::open(Some(&dev.serial_number)) {
                Ok(transport) => {
                    let mut driver = driver::FastbootDriver::new(transport);
                    match driver.get_var("version").await {
                        Ok(version) => {
                            print_success(&format!("连接成功! Fastboot 版本: {}", version));
                        }
                        Err(e) => {
                            print_error(&format!("连接失败: {}", e));
                        }
                    }
                }
                Err(e) => {
                    print_error(&format!("无法打开设备: {}", e));
                }
            }
        }
    }

    println!("\n完成");
    Ok(())
}

async fn open_transport(serial: &Option<String>) -> Result<UsbTransport, FastbootError> {
    ensure_usb_handle_released();

    let devices = UsbTransport::enumerate_devices().map_err(FastbootError::Transport)?;

    if devices.is_empty() {
        return Err(FastbootError::NoDevice);
    }

    if devices.len() > 1 && serial.is_none() {
        return Err(FastbootError::MultipleDevices);
    }

    UsbTransport::open(serial.as_deref()).map_err(|e| {
        if e.is_usb_access_denied() {
            FastbootError::InvalidArg(USB_SMART_ROUTE_MSG.into())
        } else {
            FastbootError::Transport(e)
        }
    })
}

fn connect_adb(
    serial: Option<&str>,
    verbose: bool,
) -> Result<adb::client::AdbClient, FastbootError> {
    use adb::client::AdbClient;

    let _ = crate::crypto::get_or_create_keys().expect("致命错误：无法生成或加载 RSA 密钥");
    AdbClient::connect_fast(serial, verbose).map_err(|e| FastbootError::Adb(e.to_string()))
}

async fn cmd_adb_shell(
    serial: &Option<String>,
    command: &[String],
    _verbose: bool,
) -> Result<(), FastbootError> {
    use adb::client::adb_cli_shell_proxy;

    let joined_cmd = if command.is_empty() {
        Vec::new()
    } else {
        vec![command.join(" ")]
    };
    adb_cli_shell_proxy(serial, &joined_cmd, false)
        .map_err(|e| FastbootError::Adb(e.to_string()))?;
    Ok(())
}

async fn cmd_adb_push(
    serial: &Option<String>,
    local: &Path,
    remote: &str,
    _verbose: bool,
) -> Result<(), FastbootError> {
    use adb::client::adb_cli_push_proxy;

    if !local.exists() {
        return Err(FastbootError::ImageNotFound(local.display().to_string()));
    }

    let file_size = fs::metadata(local).map_err(FastbootError::Io)?.len();

    adb_cli_push_proxy(serial, local, Path::new(remote), false)
        .map_err(|e| FastbootError::Adb(e.to_string()))?;
    println!("{}: {} pushed", local.display(), format_size(file_size));
    Ok(())
}

async fn cmd_adb_pull(
    serial: &Option<String>,
    remote: &str,
    local: &Path,
    _verbose: bool,
) -> Result<(), FastbootError> {
    use adb::client::adb_cli_pull_proxy;

    adb_cli_pull_proxy(serial, Path::new(remote), local, false)
        .map_err(|e| FastbootError::Adb(e.to_string()))?;
    Ok(())
}

async fn cmd_adb_install(
    serial: &Option<String>,
    apk: &Path,
    replace: bool,
) -> Result<(), FastbootError> {
    use adb::client::adb_cli_install_proxy;

    if !apk.exists() {
        return Err(FastbootError::ImageNotFound(apk.display().to_string()));
    }

    adb_cli_install_proxy(serial, apk, false).map_err(|e| FastbootError::Adb(e.to_string()))?;
    Ok(())
}

async fn cmd_adb_uninstall(serial: &Option<String>, package: &str) -> Result<(), FastbootError> {
    use adb::client::adb_cli_uninstall_proxy;

    adb_cli_uninstall_proxy(serial, package, false)
        .map_err(|e| FastbootError::Adb(e.to_string()))?;
    Ok(())
}

async fn cmd_adb_packages(
    serial: &Option<String>,
    third_party: bool,
    system: bool,
) -> Result<(), FastbootError> {
    use adb::client::adb_cli_packages_proxy;

    adb_cli_packages_proxy(serial, false).map_err(|e| FastbootError::Adb(e.to_string()))?;
    Ok(())
}

async fn cmd_adb_logcat(serial: &Option<String>, filter: &[String]) -> Result<(), FastbootError> {
    use adb::client::adb_cli_logcat_proxy;

    adb_cli_logcat_proxy(serial, false).map_err(|e| FastbootError::Adb(e.to_string()))?;
    Ok(())
}

async fn cmd_adb_screencap(serial: &Option<String>, output: &Path) -> Result<(), FastbootError> {
    use adb::client::adb_cli_screencap_proxy;

    adb_cli_screencap_proxy(serial, output, false)
        .map_err(|e| FastbootError::Adb(e.to_string()))?;
    println!("Screenshot saved to {}", output.display());
    Ok(())
}

async fn cmd_adb_screenrecord(
    serial: &Option<String>,
    output: &Path,
    time: u32,
) -> Result<(), FastbootError> {
    use adb::client::adb_cli_screenrecord_proxy;

    println!("Recording for {} seconds... (Ctrl+C to stop early)", time);

    adb_cli_screenrecord_proxy(serial, output, time, false)
        .map_err(|e| FastbootError::Adb(e.to_string()))?;

    println!("Recording saved to {}", output.display());
    Ok(())
}

async fn cmd_set_active(serial: &Option<String>, slot: &str) -> Result<(), FastbootError> {
    let transport = open_transport(serial).await?;
    let mut driver = driver::FastbootDriver::new(transport);

    let slot = slot.to_lowercase();
    if slot != "a" && slot != "b" {
        return Err(FastbootError::InvalidArg("槽位必须是 a 或 b".to_string()));
    }

    driver.set_active(&slot).await?;
    print_success(&format!("活动槽位已设置为: {}", slot));
    Ok(())
}

async fn cmd_oem(serial: &Option<String>, command: &[String]) -> Result<(), FastbootError> {
    let transport = open_transport(serial).await?;
    let mut driver = driver::FastbootDriver::new(transport);

    let cmd = command.join(" ");
    let result = driver.oem_command(&cmd).await?;

    if !result.is_empty() {
        println!("{}", result);
    } else {
        print_success(&format!("OEM 命令已执行: {}", cmd));
    }
    Ok(())
}

async fn cmd_flashing(serial: &Option<String>, operation: &str) -> Result<(), FastbootError> {
    let transport = open_transport(serial).await?;
    let mut driver = driver::FastbootDriver::new(transport);

    let cmd = format!("flashing {}", operation);
    let result = driver.oem_command(&cmd).await?;

    if !result.is_empty() {
        println!("{}", result);
    } else {
        print_success(&format!("flashing {} 完成", operation));
    }
    Ok(())
}

async fn cmd_create_logical_partition(
    serial: &Option<String>,
    name: &str,
    size: u64,
) -> Result<(), FastbootError> {
    let transport = open_transport(serial).await?;
    let mut driver = driver::FastbootDriver::new(transport);

    let cmd = format!("create-logical-partition:{}:{}", name, size);
    driver.raw_command(&cmd).await?;
    print_success(&format!("逻辑分区 {} 已创建 ({})", name, format_size(size)));
    Ok(())
}

async fn cmd_delete_logical_partition(
    serial: &Option<String>,
    name: &str,
) -> Result<(), FastbootError> {
    let transport = open_transport(serial).await?;
    let mut driver = driver::FastbootDriver::new(transport);

    let cmd = format!("delete-logical-partition:{}", name);
    driver.raw_command(&cmd).await?;
    print_success(&format!("逻辑分区 {} 已删除", name));
    Ok(())
}

async fn cmd_resize_logical_partition(
    serial: &Option<String>,
    name: &str,
    size: u64,
) -> Result<(), FastbootError> {
    let transport = open_transport(serial).await?;
    let mut driver = driver::FastbootDriver::new(transport);

    let cmd = format!("resize-logical-partition:{}:{}", name, size);
    driver.raw_command(&cmd).await?;
    print_success(&format!("逻辑分区 {} 已调整为 {}", name, format_size(size)));
    Ok(())
}
