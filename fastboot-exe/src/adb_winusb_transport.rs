use crate::adb_protocol::{AdbMessageHeader, A_AUTH, A_CLSE, A_CNXN, A_OKAY, A_OPEN, A_WRTE};
use crate::crypto;
use crate::error::TransportError;
use std::ffi::OsStr;
use std::fs;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Devices::Usb::{
    WinUsb_Initialize, WinUsb_QueryInterfaceSettings, WinUsb_QueryPipe, WinUsb_ReadPipe,
    WinUsb_WritePipe, USB_INTERFACE_DESCRIPTOR, WINUSB_INTERFACE_HANDLE, WINUSB_PIPE_INFORMATION,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

const ADB_CLASS_GUID: GUID = GUID {
    data1: 0xF72FE0D4,
    data2: 0xCBCB,
    data3: 0x407D,
    data4: [0x88, 0x14, 0x9E, 0xD6, 0x73, 0xD0, 0xDD, 0x6B],
};

pub struct AdbWinUsbDevice {
    device_handle: HANDLE,
    winusb_handle: WINUSB_INTERFACE_HANDLE,
    bulk_in_addr: u8,
    bulk_out_addr: u8,
    next_local_id: u32,
    is_connected: bool,
}

pub struct AdbDeviceInfo {
    pub device_path: String,
}

impl AdbWinUsbDevice {
    pub fn enumerate() -> Result<Vec<AdbDeviceInfo>, TransportError> {
        let mut devices = Vec::new();

        unsafe {
            let hdev_info = match SetupDiGetClassDevsW(
                Some(&ADB_CLASS_GUID),
                PCWSTR::null(),
                None,
                windows::Win32::Devices::DeviceAndDriverInstallation::DIGCF_DEVICEINTERFACE
                    | windows::Win32::Devices::DeviceAndDriverInstallation::DIGCF_PRESENT,
            ) {
                Ok(h) => h,
                Err(_) => return Err(TransportError::Usb("SetupDiGetClassDevsW 失败".into())),
            };

            let mut index = 0u32;
            loop {
                let mut interface_data: SP_DEVICE_INTERFACE_DATA = mem::zeroed();
                interface_data.cbSize = mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;

                let result = SetupDiEnumDeviceInterfaces(
                    hdev_info,
                    None,
                    &ADB_CLASS_GUID,
                    index,
                    &mut interface_data,
                );

                if result.is_err() {
                    break;
                }

                let mut required_size: u32 = 0;
                let _ = SetupDiGetDeviceInterfaceDetailW(
                    hdev_info,
                    &interface_data,
                    None,
                    0,
                    Some(&mut required_size),
                    None,
                );

                if required_size > 0 {
                    let mut detail_data: Vec<u8> = vec![0; required_size as usize];
                    let detail_ptr =
                        detail_data.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
                    (*detail_ptr).cbSize =
                        mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

                    let result = SetupDiGetDeviceInterfaceDetailW(
                        hdev_info,
                        &interface_data,
                        Some(detail_ptr),
                        required_size,
                        None,
                        None,
                    );

                    if result.is_ok() {
                        let device_path_ptr = (*detail_ptr).DevicePath.as_ptr();
                        let device_path = widestring_to_string(device_path_ptr);

                        if Self::check_adb_protocol(&device_path) {
                            devices.push(AdbDeviceInfo { device_path });
                        }
                    }
                }

                index += 1;
            }

            let _ = SetupDiDestroyDeviceInfoList(hdev_info);
        }

        Ok(devices)
    }

    fn check_adb_protocol(device_path: &str) -> bool {
        unsafe {
            let path_wide: Vec<u16> = OsStr::new(device_path)
                .encode_wide()
                .chain(Some(0))
                .collect();
            let raw_handle = CreateFileW(
                PCWSTR(path_wide.as_ptr()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                None,
            );

            if raw_handle.is_err() {
                return false;
            }
            let handle = raw_handle.unwrap();
            if handle.is_invalid() {
                return false;
            }
            let device_handle = HANDLE(handle.0);

            let mut winusb_handle: WINUSB_INTERFACE_HANDLE = mem::zeroed();
            let result = WinUsb_Initialize(device_handle, &mut winusb_handle);

            if result.is_err() {
                let _ = CloseHandle(device_handle);
                return false;
            }

            let mut iface_desc: USB_INTERFACE_DESCRIPTOR = mem::zeroed();
            let result = WinUsb_QueryInterfaceSettings(winusb_handle, 0, &mut iface_desc);

            let _ = CloseHandle(device_handle);

            if result.is_err() {
                return false;
            }

            iface_desc.bInterfaceProtocol == 0x01
        }
    }

    pub fn open_any() -> Result<Self, TransportError> {
        let devices = Self::enumerate()?;
        if devices.is_empty() {
            return Err(TransportError::Usb("未找到 ADB 设备".into()));
        }
        Self::open_device(&devices[0].device_path)
    }

    pub fn open_device(device_path: &str) -> Result<Self, TransportError> {
        unsafe {
            let path_wide: Vec<u16> = OsStr::new(device_path)
                .encode_wide()
                .chain(Some(0))
                .collect();
            let raw_handle = CreateFileW(
                PCWSTR(path_wide.as_ptr()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                None,
            );

            if raw_handle.is_err() {
                let err = raw_handle.unwrap_err();
                return Err(TransportError::Usb(format!(
                    "CreateFileW 失败，OS 错误码: {:?}",
                    err
                )));
            }
            let handle = raw_handle.unwrap();
            if handle.is_invalid() {
                return Err(TransportError::Usb("CreateFileW 返回无效句柄".into()));
            }
            let device_handle = HANDLE(handle.0);

            let mut winusb_handle: WINUSB_INTERFACE_HANDLE = mem::zeroed();
            let result = WinUsb_Initialize(device_handle, &mut winusb_handle);

            if result.is_err() {
                let _ = CloseHandle(device_handle);
                return Err(TransportError::Usb(format!(
                    "WinUsb_Initialize 失败，OS 错误码: {:?}",
                    result.err()
                )));
            }

            let (bulk_in, bulk_out) = Self::find_endpoints(winusb_handle)?;

            Ok(AdbWinUsbDevice {
                device_handle,
                winusb_handle,
                bulk_in_addr: bulk_in,
                bulk_out_addr: bulk_out,
                next_local_id: 1,
                is_connected: false,
            })
        }
    }

    unsafe fn find_endpoints(
        winusb_handle: WINUSB_INTERFACE_HANDLE,
    ) -> Result<(u8, u8), TransportError> {
        let mut iface_desc: USB_INTERFACE_DESCRIPTOR = mem::zeroed();
        let result = WinUsb_QueryInterfaceSettings(winusb_handle, 0, &mut iface_desc);

        if result.is_err() {
            return Err(TransportError::Usb(
                "WinUsb_QueryInterfaceSettings 失败".into(),
            ));
        }

        let mut bulk_in: Option<u8> = None;
        let mut bulk_out: Option<u8> = None;

        for pipe_index in 0..iface_desc.bNumEndpoints {
            let mut pipe_info: WINUSB_PIPE_INFORMATION = mem::zeroed();
            let result = WinUsb_QueryPipe(winusb_handle, 0, pipe_index, &mut pipe_info);

            if result.is_ok() {
                let pipe_id = pipe_info.PipeId;
                if pipe_info.PipeType == windows::Win32::Devices::Usb::UsbdPipeTypeBulk {
                    if pipe_id & 0x80 != 0 {
                        bulk_in = Some(pipe_id);
                    } else {
                        bulk_out = Some(pipe_id);
                    }
                }
            }
        }

        match (bulk_in, bulk_out) {
            (Some(in_addr), Some(out_addr)) => Ok((in_addr, out_addr)),
            _ => Err(TransportError::Usb("未找到 Bulk 端点".into())),
        }
    }

    pub fn write_pipe(&self, data: &[u8]) -> Result<(), TransportError> {
        unsafe {
            let mut written: u32 = 0;
            let result = WinUsb_WritePipe(
                self.winusb_handle,
                self.bulk_out_addr,
                data,
                Some(&mut written),
                None,
            );

            if result.is_err() {
                return Err(TransportError::Usb("WinUsb_WritePipe 失败".into()));
            }

            if written as usize != data.len() {
                return Err(TransportError::Usb("写入数据不完整".into()));
            }

            Ok(())
        }
    }

    pub fn read_pipe(&self, buf: &mut [u8]) -> Result<usize, TransportError> {
        unsafe {
            let mut read: u32 = 0;
            let result = WinUsb_ReadPipe(
                self.winusb_handle,
                self.bulk_in_addr,
                Some(buf),
                Some(&mut read),
                None,
            );

            if result.is_err() {
                return Err(TransportError::Usb("WinUsb_ReadPipe 失败".into()));
            }

            Ok(read as usize)
        }
    }

    pub fn read_exact(&self, buf: &mut [u8]) -> Result<(), TransportError> {
        let mut total = 0;
        while total < buf.len() {
            let read = self.read_pipe(&mut buf[total..])?;
            if read == 0 {
                return Err(TransportError::Usb("USB 读取返回 0 字节".into()));
            }
            total += read;
        }
        Ok(())
    }

    pub fn write_header(&mut self, header: &AdbMessageHeader) -> Result<(), TransportError> {
        let data = header.encode();
        self.write_pipe(&data)
    }

    pub fn read_header(&self) -> Result<AdbMessageHeader, TransportError> {
        let mut buf = [0u8; 24];
        self.read_exact(&mut buf)?;
        AdbMessageHeader::decode(&buf)
            .map_err(|e: std::io::Error| TransportError::Usb(e.to_string()))
    }

    pub fn read_header_with_timeout(
        &mut self,
        timeout_ms: u64,
    ) -> Result<Option<AdbMessageHeader>, String> {
        use std::sync::mpsc;
        let (tx, rx) = mpsc::channel();
        let winusb = self.winusb_handle;
        let bulk_in = self.bulk_in_addr;
        std::thread::spawn(move || {
            let mut buf = [0u8; 24];
            let mut total = 0;
            while total < 24 {
                let mut read: u32 = 0;
                let result = unsafe {
                    WinUsb_ReadPipe(
                        winusb,
                        bulk_in,
                        Some(&mut buf[total..]),
                        Some(&mut read),
                        None,
                    )
                };
                if result.is_err() || read == 0 {
                    return;
                }
                total += read as usize;
            }
            let _ = tx.send(buf);
        });
        match rx.recv_timeout(std::time::Duration::from_millis(timeout_ms)) {
            Ok(buf) => {
                let hdr = AdbMessageHeader::decode(&buf).map_err(|e| format!("{:?}", e))?;
                Ok(Some(hdr))
            }
            Err(_) => Ok(None),
        }
    }

    fn read_payload_timeout(&mut self, len: u32, timeout_ms: u64) -> Result<Vec<u8>, String> {
        if len == 0 {
            return Ok(Vec::new());
        }
        use std::sync::mpsc;
        let (tx, rx) = mpsc::channel();
        let winusb = self.winusb_handle;
        let bulk_in = self.bulk_in_addr;
        std::thread::spawn(move || {
            let mut buf = vec![0u8; len as usize];
            let mut total = 0;
            while total < len as usize {
                let mut read: u32 = 0;
                let result = unsafe {
                    WinUsb_ReadPipe(
                        winusb,
                        bulk_in,
                        Some(&mut buf[total..]),
                        Some(&mut read),
                        None,
                    )
                };
                if result.is_err() || read == 0 {
                    return;
                }
                total += read as usize;
            }
            let _ = tx.send(buf);
        });
        match rx.recv_timeout(std::time::Duration::from_millis(timeout_ms)) {
            Ok(buf) => Ok(buf),
            Err(_) => Err("超时".into()),
        }
    }

    fn write_message(
        &mut self,
        command: u32,
        local_id: u32,
        remote_id: u32,
        data: &[u8],
    ) -> Result<(), TransportError> {
        let header = AdbMessageHeader::new(command, local_id, remote_id, data.len() as u32);
        self.write_header(&header)?;
        if !data.is_empty() {
            self.write_pipe(data)?;
        }
        Ok(())
    }

    fn read_payload(&mut self, len: u32) -> Result<Vec<u8>, TransportError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let mut data = vec![0u8; len as usize];
        self.read_exact(&mut data)?;
        Ok(data)
    }

    pub fn connect(&mut self) -> Result<(), String> {
        if self.is_connected {
            return Ok(());
        }

        // 1. 发送初始的 A_CNXN
        let (header, banner) = crate::adb_protocol::make_connection_message();
        self.write_header(&header).map_err(|_| "发送 A_CNXN 失败")?;
        self.write_pipe(&banner)
            .map_err(|_| "发送 A_CNXN 载荷失败")?;

        let mut auth_attempts = 0;

        loop {
            let resp_header = self
                .read_header()
                .map_err(|e| format!("读取报头失败: {:?}", e))?;

            if resp_header.command != 0x4e584e43 && resp_header.command != 0x48545541 {
                if resp_header.data_length > 0 {
                    let mut data = vec![0u8; resp_header.data_length as usize];
                    let _ = self.read_exact(&mut data);
                }
                continue;
            }

            match resp_header.command {
                0x4e584e43 => {
                    let payload = if resp_header.data_length > 0 {
                        let mut data = vec![0u8; resp_header.data_length as usize];
                        let _ = self.read_exact(&mut data);
                        data
                    } else {
                        Vec::new()
                    };
                    self.is_connected = true;
                    return Ok(());
                }
                0x48545541 => {
                    // 收到 A_AUTH
                    let payload = if resp_header.data_length > 0 {
                        let mut data = vec![0u8; resp_header.data_length as usize];
                        let _ = self.read_exact(&mut data);
                        data
                    } else {
                        Vec::new()
                    };
                    let (priv_pem, mut pub_key) =
                        crate::crypto::get_or_create_keys().map_err(|_| "密钥引擎崩溃")?;

                    if resp_header.arg0 == 1 {
                        // AUTH_TOKEN
                        if auth_attempts == 0 {
                            // 第一次收到 Token：尝试签名并发送
                            auth_attempts += 1;
                            if let Ok(signature) = crate::crypto::sign_token(&priv_pem, &payload) {
                                let auth_header =
                                    AdbMessageHeader::new(A_AUTH, 2, 0, signature.len() as u32);
                                self.write_header(&auth_header)
                                    .map_err(|e| format!("发送签名头失败: {:?}", e))?;
                                self.write_pipe(&signature)
                                    .map_err(|e| format!("发送签名失败: {:?}", e))?;
                                continue;
                            }
                        }

                        // 第二次及以后收到 Token：说明签名被拒！必须发公钥触发弹窗！
                        if pub_key.last() != Some(&0) {
                            pub_key.push(0);
                        }

                        if auth_attempts == 1 {
                            println!(
                                "请点亮手机屏幕并点击「允许 USB 调试」"
                            );
                        }
                        auth_attempts += 1;

                        let auth_header = AdbMessageHeader::new(A_AUTH, 3, 0, pub_key.len() as u32);
                        self.write_header(&auth_header)
                            .map_err(|e| format!("发送公钥头失败: {:?}", e))?;
                        self.write_pipe(&pub_key)
                            .map_err(|e| format!("发送公钥失败: {:?}", e))?;
                        continue;
                    }
                }
                _ => {
                    if resp_header.data_length > 0 {
                        let mut data = vec![0u8; resp_header.data_length as usize];
                        let _ = self.read_exact(&mut data);
                    }
                    continue;
                }
            }
        }
    }

    pub fn reboot(&mut self, target: &str) -> Result<(), String> {
        self.connect()?;
        let local_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            | 1;

        let cmd = if target.is_empty() {
            "reboot:\0".to_string()
        } else {
            format!("reboot:{}\0", target)
        };

        self.write_message(0x4e45504f, local_id, 0, cmd.as_bytes())
            .unwrap();

        Ok(())
    }

    pub fn open_stream(&mut self, destination: &str) -> Result<(u32, u32), &'static str> {
        let local_id = self.next_local_id;
        self.next_local_id += 1;

        let mut dest_bytes = destination.as_bytes().to_vec();
        dest_bytes.push(0);

        let header = AdbMessageHeader::new(A_OPEN, local_id, 0, dest_bytes.len() as u32);
        self.write_header(&header).map_err(|_| "发送 A_OPEN 失败")?;
        self.write_pipe(&dest_bytes)
            .map_err(|_| "发送目标路径失败")?;

        let resp_header = self.read_header().map_err(|_| "读取 A_OPEN 回复失败")?;

        if resp_header.command != A_OKAY {
            return Err("预期 A_OKAY 但收到其他命令");
        }

        let remote_id = resp_header.arg0;
        Ok((local_id, remote_id))
    }

    pub fn write_stream(
        &mut self,
        local_id: u32,
        remote_id: u32,
        data: &[u8],
    ) -> Result<(), &'static str> {
        let header = AdbMessageHeader::new(A_WRTE, local_id, remote_id, data.len() as u32);
        self.write_header(&header).map_err(|_| "发送 A_WRTE 失败")?;
        self.write_pipe(data).map_err(|_| "发送数据失败")?;

        let resp_header = self.read_header().map_err(|_| "读取 A_WRTE 回复失败")?;

        if resp_header.command != A_OKAY {
            return Err("预期 A_OKAY 但收到其他命令");
        }

        Ok(())
    }

    pub fn close_stream(&mut self, local_id: u32, remote_id: u32) -> Result<(), &'static str> {
        let header = AdbMessageHeader::new(A_CLSE, local_id, remote_id, 0);
        self.write_header(&header).map_err(|_| "发送 A_CLSE 失败")?;

        let resp_header = self.read_header().map_err(|_| "读取 A_CLSE 回复失败")?;

        if resp_header.command != A_OKAY {
            return Err("预期 A_OKAY 但收到其他命令");
        }

        Ok(())
    }

    pub fn execute_service(&mut self, service: &str) -> Result<Vec<u8>, String> {
        self.connect().map_err(|e| format!("ADB 握手失败: {}", e))?;

        let local_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            | 1;
        let mut remote_id = 0;

        let mut dest_str = service.to_string();
        if !dest_str.ends_with('\0') {
            dest_str.push('\0');
        }

        let header = AdbMessageHeader::new(A_OPEN, local_id, 0, dest_str.len() as u32);
        self.write_header(&header)
            .map_err(|e| format!("发送 A_OPEN 失败: {:?}", e))?;
        self.write_pipe(dest_str.as_bytes())
            .map_err(|e| format!("发送目标路径失败: {:?}", e))?;

        let mut output_buffer = Vec::new();

        loop {
            let resp_header = self.read_header().map_err(|_| "Stream 读取报头失败")?;

            if resp_header.arg1 != local_id {
                continue;
            }

            let payload = if resp_header.data_length > 0 {
                let mut data = vec![0u8; resp_header.data_length as usize];
                let _ = self.read_exact(&mut data);
                data
            } else {
                Vec::new()
            };

            match resp_header.command {
                0x59414b4f => {
                    if remote_id == 0 {
                        remote_id = resp_header.arg0;
                    }
                }
                0x45545257 => {
                    output_buffer.extend_from_slice(&payload);
                    let ok_header = AdbMessageHeader::new(A_OKAY, local_id, resp_header.arg0, 0);
                    let _ = self.write_header(&ok_header);
                    let _ = self.write_pipe(&[]);
                }
                0x45534c43 => {
                    let _ = self.write_header(&AdbMessageHeader::new(
                        A_CLSE,
                        local_id,
                        resp_header.arg0,
                        0,
                    ));
                    let _ = self.write_pipe(&[]);
                    break;
                }
                _ => {}
            }
        }

        Ok(output_buffer)
    }

    pub fn shell_command(&mut self, command: &str) -> Result<String, String> {
        let service = format!("shell:{}", command);
        let raw_data = self.execute_service(&service)?;
        Ok(String::from_utf8_lossy(&raw_data).to_string())
    }

    pub fn screencap(&mut self, save_path: &str) -> Result<(), String> {
        let raw_data = self.execute_service("exec:screencap -p")?;

        fs::write(save_path, &raw_data).map_err(|e| format!("保存图片失败: {}", e))?;
        Ok(())
    }

    pub fn push(&mut self, local_path: &str, remote_path: &str) -> Result<(), String> {
        use std::fs::File;
        use std::io::Read;

        self.connect().map_err(|e| format!("ADB 握手失败: {}", e))?;
        let local_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            | 1;
        let mut remote_id = 0;

        let dest_str = "sync:\0".to_string();
        let header = AdbMessageHeader::new(A_OPEN, local_id, 0, dest_str.len() as u32);
        self.write_header(&header)
            .map_err(|e| format!("发送 A_OPEN 失败: {:?}", e))?;
        self.write_pipe(dest_str.as_bytes())
            .map_err(|e| format!("发送 sync 路径失败: {:?}", e))?;

        loop {
            let resp_header = self.read_header().map_err(|_| "读取 sync OKAY 失败")?;
            if resp_header.arg1 != local_id {
                continue;
            }
            if resp_header.data_length > 0 {
                let mut data = vec![0u8; resp_header.data_length as usize];
                let _ = self.read_exact(&mut data);
            }
            if resp_header.command == A_OKAY {
                remote_id = resp_header.arg0;
                break;
            }
        }

        let mode = 33206;
        let send_req = format!("{},{}", remote_path, mode);
        let mut send_buf = Vec::new();
        send_buf.extend_from_slice(b"SEND");
        send_buf.extend_from_slice(&(send_req.len() as u32).to_le_bytes());
        send_buf.extend_from_slice(send_req.as_bytes());

        let send_header = AdbMessageHeader::new(A_WRTE, local_id, remote_id, send_buf.len() as u32);
        self.write_header(&send_header)
            .map_err(|e| format!("发送 SEND 头失败: {:?}", e))?;
        self.write_pipe(&send_buf)
            .map_err(|e| format!("发送 SEND 数据失败: {:?}", e))?;
        let resp_header = self.read_header().map_err(|_| "读取 SEND OKAY 失败")?;
        if resp_header.arg1 != local_id {
            return Err("幽灵包干扰，SEND 通道同步失败".into());
        }

        let mut file = File::open(local_path).map_err(|e| format!("打开本地文件失败: {}", e))?;
        let mut chunk = vec![0u8; 64 * 1024];

        loop {
            let bytes_read = file
                .read(&mut chunk)
                .map_err(|e| format!("读取文件失败: {}", e))?;
            if bytes_read == 0 {
                break;
            }

            let mut data_buf = Vec::new();
            data_buf.extend_from_slice(b"DATA");
            data_buf.extend_from_slice(&(bytes_read as u32).to_le_bytes());
            data_buf.extend_from_slice(&chunk[..bytes_read]);

            let data_header =
                AdbMessageHeader::new(A_WRTE, local_id, remote_id, data_buf.len() as u32);
            self.write_header(&data_header)
                .map_err(|e| format!("发送 DATA 头失败: {:?}", e))?;
            self.write_pipe(&data_buf)
                .map_err(|e| format!("发送 DATA 数据失败: {:?}", e))?;
            let resp_header = self.read_header().map_err(|_| "读取 DATA OKAY 失败")?;
            if resp_header.arg1 != local_id {
                continue;
            }
            if resp_header.data_length > 0 {
                let mut data = vec![0u8; resp_header.data_length as usize];
                let _ = self.read_exact(&mut data);
            }
        }

        let mtime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "获取时间失败")?
            .as_secs() as u32;
        let mut done_buf = Vec::new();
        done_buf.extend_from_slice(b"DONE");
        done_buf.extend_from_slice(&mtime.to_le_bytes());
        let done_header = AdbMessageHeader::new(A_WRTE, local_id, remote_id, done_buf.len() as u32);
        self.write_header(&done_header)
            .map_err(|e| format!("发送 DONE 头失败: {:?}", e))?;
        self.write_pipe(&done_buf)
            .map_err(|e| format!("发送 DONE 数据失败: {:?}", e))?;
        let resp_header = self.read_header().map_err(|_| "读取 DONE OKAY 失败")?;
        if resp_header.arg1 != local_id {
            return Err("幽灵包干扰，DONE 通道同步失败".into());
        }

        let mut sync_ack_payload = Vec::new();
        loop {
            let hdr = self.read_header().map_err(|_| "读取 SYNC 最终响应失败")?;
            if hdr.arg1 != local_id {
                continue;
            }
            let payload = if hdr.data_length > 0 {
                let mut data = vec![0u8; hdr.data_length as usize];
                let _ = self.read_exact(&mut data);
                data
            } else {
                Vec::new()
            };

            if hdr.command == A_WRTE {
                let _ = self.write_header(&AdbMessageHeader::new(A_OKAY, local_id, hdr.arg0, 0));
                let _ = self.write_pipe(&[]);
                sync_ack_payload = payload;
                break;
            } else if hdr.command == A_CLSE {
                return Err("手机提前关闭了连接，可能空间不足或路径非法".into());
            }
        }

        if sync_ack_payload.len() >= 4 {
            if &sync_ack_payload[0..4] != b"OKAY" {
                let reason = String::from_utf8_lossy(&sync_ack_payload[4..]);
                return Err(format!("手机底层的 SYNC 引擎拒绝了传输: {}", reason));
            }
        } else {
            return Err("收到的 SYNC 响应报文异常损坏".into());
        }

        let _ = self.write_header(&AdbMessageHeader::new(A_CLSE, local_id, remote_id, 0));
        let _ = self.write_pipe(&[]);
        Ok(())
    }

    pub fn pull(&mut self, remote_path: &str, local_path: &str) -> Result<(), String> {
        self.connect()?;
        let local_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            | 1;
        let mut remote_id = 0;

        self.write_message(0x4e45504f, local_id, 0, b"sync:\0")
            .unwrap();
        loop {
            let hdr = self.read_header().map_err(|_| "读取 SYNC 握手失败")?;
            let _ = self.read_payload(hdr.data_length);
            if hdr.arg1 != local_id {
                continue;
            }
            if hdr.command == 0x59414b4f {
                remote_id = hdr.arg0;
                break;
            }
        }

        let mut recv_buf = Vec::new();
        recv_buf.extend_from_slice(b"RECV");
        recv_buf.extend_from_slice(&(remote_path.len() as u32).to_le_bytes());
        recv_buf.extend_from_slice(remote_path.as_bytes());

        self.write_message(0x45545257, local_id, remote_id, &recv_buf)
            .unwrap();

        loop {
            let hdr = self.read_header().unwrap();
            let _ = self.read_payload(hdr.data_length);
            if hdr.arg1 == local_id && hdr.command == 0x59414b4f {
                break;
            }
        }

        use std::fs::File;
        use std::io::Write;
        let mut file = File::create(local_path).map_err(|e| format!("无法创建本地文件: {}", e))?;
        let mut sync_buffer = Vec::new();
        let mut total_bytes = 0;

        'receive: loop {
            let hdr = self.read_header().map_err(|_| "读取流报头失败")?;
            let payload = self.read_payload(hdr.data_length).unwrap_or_default();

            if hdr.arg1 != local_id {
                continue;
            }

            if hdr.command == 0x45545257 {
                let _ = self.write_message(0x59414b4f, local_id, hdr.arg0, &[]);

                sync_buffer.extend_from_slice(&payload);

                while sync_buffer.len() >= 8 {
                    let id = &sync_buffer[0..4];
                    let chunk_len =
                        u32::from_le_bytes(sync_buffer[4..8].try_into().unwrap()) as usize;

                    if id == b"DATA" {
                        if sync_buffer.len() >= 8 + chunk_len {
                            file.write_all(&sync_buffer[8..8 + chunk_len]).unwrap();
                            total_bytes += chunk_len;
                            sync_buffer.drain(0..8 + chunk_len);
                        } else {
                            break;
                        }
                    } else if id == b"DONE" {
                        break 'receive;
                    } else if id == b"FAIL" {
                        let reason = if sync_buffer.len() >= 8 + chunk_len {
                            String::from_utf8_lossy(&sync_buffer[8..8 + chunk_len]).to_string()
                        } else {
                            "未知错误".to_string()
                        };
                        return Err(format!("手机拒绝了拉取: {}", reason));
                    } else {
                        return Err(format!("协议错乱：未知的 SYNC 块标记 {:?}", id));
                    }
                }
            } else if hdr.command == 0x45534c43 {
                let _ = self.write_message(0x45534c43, local_id, hdr.arg0, &[]);
                break;
            }
        }

        let _ = self.write_message(0x45534c43, local_id, remote_id, &[]);
        Ok(())
    }

    pub fn logcat(&mut self, grep_keyword: Option<&str>) -> Result<(), String> {
        self.connect()?;
        let local_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            | 1;
        let mut remote_id = 0;

        self.write_message(0x4e45504f, local_id, 0, b"shell:logcat\0")
            .unwrap();

        loop {
            let hdr = self.read_header().map_err(|_| "读取通道确认失败")?;
            let _ = self.read_payload(hdr.data_length);
            if hdr.arg1 == local_id && hdr.command == 0x59414b4f {
                remote_id = hdr.arg0;
                break;
            }
        }

        if let Some(kw) = grep_keyword {
            println!("拦截包含 {} 的日志 Ctrl+C 退出", kw);
        }

        let mut line_buffer = String::new();

        loop {
            let hdr = match self.read_header() {
                Ok(h) => h,
                Err(_) => break,
            };

            let payload = self.read_payload(hdr.data_length).unwrap_or_default();

            if hdr.command == 0x45545257 {
                let text = String::from_utf8_lossy(&payload);
                line_buffer.push_str(&text);

                while let Some(pos) = line_buffer.find('\n') {
                    let line = line_buffer[..=pos].to_string();
                    line_buffer.drain(..=pos);

                    match grep_keyword {
                        Some(kw) => {
                            if line.contains(kw) {
                                print!("{}", line);
                            }
                        }
                        None => print!("{}", line),
                    }
                }

                use std::io::Write;
                let _ = std::io::stdout().flush();

                let _ = self.write_message(0x59414b4f, local_id, hdr.arg0, &[]);
            } else if hdr.command == 0x45534c43 {
                let _ = self.write_message(0x45534c43, local_id, hdr.arg0, &[]);
                break;
            }
        }
        Ok(())
    }

    pub fn root(&mut self) -> Result<(), String> {
        self.connect()?;
        let local_id = 999;

        self.write_message(0x4e45504f, local_id, 0, b"root:\0")
            .unwrap();

        loop {
            let hdr = match self.read_header() {
                Ok(h) => h,
                Err(_) => break,
            };
            let payload = self.read_payload(hdr.data_length).unwrap_or_default();
            if hdr.command == 0x45545257 {
                let msg = String::from_utf8_lossy(&payload);
                print!("{}", msg.trim());
                let _ = self.write_message(0x59414b4f, local_id, hdr.arg0, &[]);
            } else if hdr.command == 0x45534c43 {
                break;
            }
        }
        Ok(())
    }

    pub fn unroot(&mut self) -> Result<(), String> {
        self.connect()?;
        let local_id = 888;

        self.write_message(0x4e45504f, local_id, 0, b"unroot:\0")
            .unwrap();

        loop {
            let hdr = match self.read_header() {
                Ok(h) => h,
                Err(_) => break,
            };
            let payload = self.read_payload(hdr.data_length).unwrap_or_default();
            if hdr.command == 0x45545257 {
                let msg = String::from_utf8_lossy(&payload);
                println!("{}", msg.trim());
                let _ = self.write_message(0x59414b4f, local_id, hdr.arg0, &[]);
            } else if hdr.command == 0x45534c43 {
                break;
            }
        }
        Ok(())
    }

    pub fn true_pty_shell(&mut self) -> Result<(), String> {
        self.connect()?;
        let local_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            | 1;
        let mut remote_id = 0;

        self.write_message(0x4e45504f, local_id, 0, b"shell:\0")
            .unwrap();

        loop {
            let hdr = self.read_header().map_err(|_| "读取通道确认失败")?;
            let _ = self.read_payload(hdr.data_length);
            if hdr.arg1 == local_id && hdr.command == 0x59414b4f {
                remote_id = hdr.arg0;
                break;
            }
        }

        println!("输入 exit 退出");

        use std::sync::{Arc, Mutex};
        let write_lock = Arc::new(Mutex::new(()));
        let dev_ptr_addr = self as *mut Self as usize;

        let expected_cmd = Arc::new(Mutex::new(String::new()));
        let is_skipping = Arc::new(Mutex::new(false));

        let write_lock_clone = write_lock.clone();
        let expected_cmd_clone = expected_cmd.clone();
        let is_skipping_clone = is_skipping.clone();

        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            loop {
                let mut buf = String::new();
                if stdin.read_line(&mut buf).is_ok() {
                    let cmd = buf.trim_end();
                    if cmd == "exit_rusty" {
                        std::process::exit(0);
                    }

                    if !cmd.is_empty() {
                        *expected_cmd_clone.lock().unwrap() = cmd.to_string();
                        *is_skipping_clone.lock().unwrap() = true;
                    }

                    let script = format!("{}\n", cmd);

                    let _guard = write_lock_clone.lock().unwrap();
                    let writer = unsafe { &mut *(dev_ptr_addr as *mut Self) };
                    let _ =
                        writer.write_message(0x45545257, local_id, remote_id, script.as_bytes());
                }
            }
        });

        let dev_ptr = self as *mut Self;
        let mut skip_buffer = String::new();

        loop {
            let hdr = match self.read_header() {
                Ok(h) => h,
                Err(_) => break,
            };
            let payload = self.read_payload(hdr.data_length).unwrap_or_default();

            if hdr.arg1 != local_id {
                continue;
            }

            if hdr.command == 0x45545257 {
                let payload_str = String::from_utf8_lossy(&payload);
                let mut text_to_print = String::new();

                {
                    let mut skipping = is_skipping.lock().unwrap();
                    if *skipping {
                        skip_buffer.push_str(&payload_str);

                        while let Some(idx) = skip_buffer.find('\n') {
                            let line = skip_buffer[..=idx].to_string();
                            let expected = expected_cmd.lock().unwrap().clone();

                            skip_buffer = skip_buffer[idx + 1..].to_string();

                            if line.contains(&expected) {
                                *skipping = false;
                                text_to_print.push_str(&skip_buffer);
                                skip_buffer.clear();
                                break;
                            }
                        }
                    } else {
                        text_to_print.push_str(&payload_str);
                    }
                }

                if !text_to_print.is_empty() {
                    print!("{}", text_to_print);
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }

                let _guard = write_lock.lock().unwrap();
                let writer = unsafe { &mut *dev_ptr };
                let _ = writer.write_message(0x59414b4f, local_id, hdr.arg0, &[]);
            } else if hdr.command == 0x45534c43 {
                let _guard = write_lock.lock().unwrap();
                let writer = unsafe { &mut *dev_ptr };
                let _ = writer.write_message(0x45534c43, local_id, hdr.arg0, &[]);
                break;
            }
        }
        Ok(())
    }

    pub fn install(&mut self, apk_path: &str) -> Result<(), String> {
        let filename = std::path::Path::new(apk_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        let remote_tmp = format!("/data/local/tmp/{}", filename);

        self.push(apk_path, &remote_tmp)?;

        let shell_cmd = format!("pm install -r \"{}\"", remote_tmp);
        let result = self.shell_command(&shell_cmd)?;

        let _ = self.shell_command(&format!("rm \"{}\"", remote_tmp));
        Ok(())
    }
}

impl Drop for AdbWinUsbDevice {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Devices::Usb::WinUsb_Free(self.winusb_handle);
            let _ = CloseHandle(self.device_handle);
        }
    }
}

unsafe fn widestring_to_string(ptr: *const u16) -> String {
    let mut len = 0;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    String::from_utf16_lossy(slice)
}
