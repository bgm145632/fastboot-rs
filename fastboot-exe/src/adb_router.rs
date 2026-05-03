use crate::adb_protocol::A_OKAY;
use crate::adb_winusb_transport::AdbWinUsbDevice;
use std::env;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process;
use std::thread;

const SYNC_DATA_MAX: usize = 64 * 1024;
const SYNC_SEND: &[u8; 4] = b"SEND";
const SYNC_DATA: &[u8; 4] = b"DATA";
const SYNC_DONE: &[u8; 4] = b"DONE";
const SYNC_OKAY: &[u8; 4] = b"OKAY";

pub fn try_handle_adb_args() -> Option<i32> {
    let mut args: Vec<String> = env::args().skip(1).collect();

    while let Some(pos) = args.iter().position(|x| x == "-s") {
        if pos + 1 < args.len() {
            args.remove(pos + 1);
        }
        args.remove(pos);
    }

    args.retain(|x| x != "-d" && x != "-e");

    if args.is_empty() {
        if let Ok(exe_path) = env::current_exe() {
            if exe_path.to_string_lossy().to_lowercase().contains("adb") {
                process::exit(0);
            }
        }
        return None;
    }

    let cmd = args[0].as_str();

    match cmd {
        "wait-for-device" | "start-server" => {
            process::exit(0);
        }
        "version" => {
            println!("Android Debug Bridge version 1.0.41");
            println!("Version 34.0.5-10900879");
            process::exit(0);
        }
        "devices" => handle_devices_native(),
        "devices-l" => handle_devices_native(),
        "reverse" => handle_reverse(&args[1..]),
        "forward" => handle_forward(&args[1..]),
        "push" => {
            if args.len() >= 3 {
                let local = &args[1];
                let remote = &args[2];
                handle_push_native(local, remote)
            } else {
                None
            }
        }
        "shell" => {
            let shell_args = &args[1..];
            handle_shell(shell_args)
        }
        _ => {
            if let Ok(exe_path) = env::current_exe() {
                if exe_path.to_string_lossy().to_lowercase().contains("adb") {
                    process::exit(0);
                }
            }
            None
        }
    }
}

fn handle_devices_native() -> Option<i32> {
    match AdbWinUsbDevice::enumerate() {
        Ok(devices) => {
            if devices.is_empty() {
                println!("List of devices attached");
                return Some(0);
            }

            println!("List of devices attached");

            for info in devices {
                let device_path = info.device_path.clone();
                match AdbWinUsbDevice::open_device(&device_path) {
                    Ok(mut dev) => match dev.connect() {
                        Ok(()) => {
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
            Some(0)
        }
        Err(_) => {
            println!("List of devices attached");
            Some(0)
        }
    }
}

fn extract_serial_from_path(path: &str) -> String {
    path.rsplit('\\')
        .next()
        .and_then(|s| s.split('#').nth(1))
        .and_then(|s| s.split('&').next())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn handle_reverse(args: &[String]) -> Option<i32> {
    if args.len() < 2 {
        return Some(1);
    }
    let remote = &args[0];
    let local = &args[1];
    let cmd = format!("host:reverse:forward:{};{}", remote, local);

    match crate::adb_handler::send_host_command(&cmd) {
        Ok(_) => {
            println!("{}", local);
            Some(0)
        }
        Err(_) => Some(1),
    }
}

fn handle_forward(args: &[String]) -> Option<i32> {
    if args.len() < 2 {
        return Some(1);
    }
    let local = &args[0];
    let remote = &args[1];
    let cmd = format!("host:forward:tcp:{};{}", local, remote);

    match crate::adb_handler::send_host_command(&cmd) {
        Ok(_) => {
            println!("{}", local);
            Some(0)
        }
        Err(_) => Some(1),
    }
}

fn handle_push_native(local: &str, remote: &str) -> Option<i32> {
    let local_path = Path::new(local);
    if !local_path.exists() {
        return Some(1);
    }

    let file = match std::fs::File::open(local_path) {
        Ok(f) => f,
        Err(_) => {
            return Some(1);
        }
    };

    let metadata = match file.metadata() {
        Ok(m) => m,
        Err(_) => {
            return Some(1);
        }
    };

    let mut dev = match AdbWinUsbDevice::open_any() {
        Ok(d) => d,
        Err(_) => {
            return Some(1);
        }
    };

    if let Err(_) = dev.connect() {
        return Some(1);
    }

    let (loc_id, rem_id) = match dev.open_stream("sync:") {
        Ok(ids) => ids,
        Err(_) => {
            return Some(1);
        }
    };

    let mode = 33206;
    let send_path = format!("{},{}", remote, mode);
    let path_bytes = send_path.as_bytes();

    let mut send_msg = Vec::with_capacity(8 + path_bytes.len());
    send_msg.extend_from_slice(SYNC_SEND);
    send_msg.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
    send_msg.extend_from_slice(path_bytes);

    if let Err(_) = dev.write_stream(loc_id, rem_id, &send_msg) {
        return Some(1);
    }

    let mut file = file;
    let mut buf = vec![0u8; SYNC_DATA_MAX];

    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut data_msg = Vec::with_capacity(8 + n);
                data_msg.extend_from_slice(SYNC_DATA);
                data_msg.extend_from_slice(&(n as u32).to_le_bytes());
                data_msg.extend_from_slice(&buf[..n]);

                if let Err(_) = dev.write_stream(loc_id, rem_id, &data_msg) {
                    return Some(1);
                }
            }
            Err(_) => {
                return Some(1);
            }
        }
    }

    let mtime = metadata
        .modified()
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32
        })
        .unwrap_or(0);

    let mut done_msg = Vec::with_capacity(8);
    done_msg.extend_from_slice(SYNC_DONE);
    done_msg.extend_from_slice(&mtime.to_le_bytes());

    if let Err(_) = dev.write_stream(loc_id, rem_id, &done_msg) {
        return Some(1);
    }

    let final_header = match dev.read_header() {
        Ok(h) => h,
        Err(_) => {
            return Some(1);
        }
    };

    let final_data = if final_header.data_length > 0 {
        let mut data = vec![0u8; final_header.data_length as usize];
        match dev.read_exact(&mut data) {
            Ok(_) => data,
            Err(_) => {
                return Some(1);
            }
        }
    } else {
        Vec::new()
    };

    if &final_data != SYNC_OKAY {
        return Some(1);
    }

    let ok_header = crate::adb_protocol::AdbMessageHeader::new(A_OKAY, rem_id, loc_id, 0);
    if let Err(_) = dev.write_header(&ok_header) {
        return Some(1);
    }

    if let Err(_) = dev.close_stream(loc_id, rem_id) {
        return Some(1);
    }

    process::exit(0);
}

fn handle_shell(args: &[String]) -> Option<i32> {
    let has_args = !args.is_empty();
    let cmd_str = if has_args {
        args.join(" ")
    } else {
        String::new()
    };

    let mut stream = match crate::adb_handler::connect_device_transport(None) {
        Ok(s) => s,
        Err(_) => {
            return Some(1);
        }
    };

    let shell_cmd = if has_args {
        format!("shell:{}", cmd_str)
    } else {
        "shell:".to_string()
    };

    if let Err(_) = crate::adb_handler::send_request(&mut stream, &shell_cmd) {
        return Some(1);
    }

    let stdout = io::stdout();
    let mut stdout_lock = stdout.lock();

    if has_args {
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdout_lock.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    if stdout_lock.flush().is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        process::exit(0);
    } else {
        let mut stream_clone = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => {
                return Some(1);
            }
        };

        thread::spawn(move || {
            let stdin = io::stdin();
            let mut stdin_lock = stdin.lock();
            let mut buf = [0u8; 1024];
            loop {
                match stdin_lock.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if stream_clone.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        if stream_clone.flush().is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdout_lock.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    if stdout_lock.flush().is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        process::exit(0);
    }
}
