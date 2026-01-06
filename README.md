<div align="center">

<!-- 头部动画 -->
<img src="https://capsule-render.vercel.app/api?type=waving&color=gradient&customColorList=24&height=200&section=header&text=Fastboot-RS&fontSize=50&fontColor=fff&animation=fadeIn&fontAlignY=35&desc=🦀%20Rust%20实现的高性能%20Fastboot%20工具&descAlignY=55&descSize=18"/>

<!-- 徽章 -->
[![Rust](https://img.shields.io/badge/Rust-1.70+-orange?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-MIT-blue?style=for-the-badge)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Windows%20%7C%20Linux-green?style=for-the-badge&logo=windows&logoColor=white)](https://github.com/wumai2580/fastboot-rs)

</div>

---

##  项目简介

>  **Fastboot-RS** 是一个用 Rust 从零实现的 Android Fastboot 刷机工具，完全独立于 Google 官方实现。

<div align="center">

```
┌─────────────────────────────────────────────────────────────┐
│                     Fastboot-RS 架构                         │
├─────────────────────────────────────────────────────────────┤
│  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐        │
│  │   CLI   │  │  Flash  │  │Partition│  │ Progress│        │
│  └────┬────┘  └────┬────┘  └────┬────┘  └────┬────┘        │
│       │            │            │            │              │
│       └────────────┴─────┬──────┴────────────┘              │
│                          │                                  │
│                   ┌──────┴──────┐                           │
│                   │   Protocol  │                           │
│                   └──────┬──────┘                           │
│                          │                                  │
│       ┌──────────────────┼──────────────────┐               │
│       │                  │                  │               │
│  ┌────┴────┐       ┌─────┴─────┐      ┌─────┴─────┐        │
│  │   USB   │       │    TCP    │      │    UDP    │        │
│  └─────────┘       └───────────┘      └───────────┘        │
└─────────────────────────────────────────────────────────────┘
```

</div>

---

##  特性

<table>
<tr>
<td width="50%">

###  核心功能
-  高性能刷写** - 40+ MB/s 传输速度
-  Sparse 镜像** - 完整支持稀疏镜像解析
-  批量刷写** - flashall 一键刷机
-  进度显示** - 实时速度和进度条

</td>
<td width="50%">

###  支持命令
- `flash` / `erase` - 刷写/擦除分区
- `reboot` - 重启设备 (支持 bl/rec/fbd)
- `getvar` - 获取设备变量
- `oem` / `flashing` - OEM/解锁命令
- `set_active` - 设置活动槽位
- `flashall` - 批量刷写
- `auth` - ADB 授权
- `shell` / `push` / `pull` - ADB 命令
- `install` / `uninstall` - 安装/卸载 APK

</td>
</tr>
</table>

---

## 快速开始

### Windows 环境配置

以管理员身份打开 PowerShell，执行以下命令：

#### 1. 安装 Rust

```powershell
# 下载并运行 Rust 安装器
Invoke-WebRequest -Uri https://win.rustup.rs/x86_64 -OutFile rustup-init.exe
.\rustup-init.exe -y

# 刷新环境变量（或重启终端）
$env:Path = [System.Environment]::GetEnvironmentVariable("Path","Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path","User")

# 验证安装
rustc --version
cargo --version
```

#### 2. 安装 Visual Studio Build Tools

```powershell
# 下载 VS Build Tools 安装器
Invoke-WebRequest -Uri "https://aka.ms/vs/17/release/vs_BuildTools.exe" -OutFile vs_BuildTools.exe

# 安装 C++ 构建工具（静默安装）
.\vs_BuildTools.exe --quiet --wait --norestart --nocache --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended

# 或者手动运行安装器，勾选 "使用 C++ 的桌面开发"
.\vs_BuildTools.exe
```

#### 3. 克隆并编译

```powershell
# 克隆仓库
git clone https://github.com/wumai2580/fastboot-rs.git
cd fastboot-rs

# 编译 Release 版本
cargo build --release

# 编译产物位于 target/release/fastboot.exe
```

#### 4. 添加到系统 PATH（可选）

```powershell
# 复制到系统目录
Copy-Item .\target\release\fastboot.exe C:\Windows\System32\

# 或添加到用户 PATH
$currentPath = [Environment]::GetEnvironmentVariable("Path", "User")
$newPath = "$currentPath;$PWD\target\release"
[Environment]::SetEnvironmentVariable("Path", $newPath, "User")
```

### 使用

```powershell
# 查看设备
fastboot devices

# 刷写分区
fastboot flash boot boot.img

# 擦除分区
fastboot erase userdata

# 重启设备
fastboot reboot

# 重启到 fastbootd
fastboot reboot fbd

# 重启到 recovery
fastboot reboot rec

# 一键刷机（当前目录有 flash_all.bat 或镜像文件）
fastboot flashall

# ADB 授权
fastboot auth

# ADB Shell
fastboot shell ls -la
```

---

## 📁 项目结构

```
fastboot-rs/
├── 源码/
│   ├── main.rs          # 主入口
│   ├── cli.rs           # 命令行解析
│   ├── error.rs         # 错误处理
│   ├── 传输层/          # Transport Layer
│   │   ├── transport.rs # 传输抽象
│   │   ├── usb.rs       # USB 传输
│   │   ├── tcp.rs       # TCP 传输
│   │   └── udp.rs       # UDP 传输
│   ├── 协议层/          # Protocol Layer
│   │   ├── protocol.rs  # Fastboot 协议
│   │   ├── driver.rs    # 驱动封装
│   │   └── sparse.rs    # Sparse 解析
│   └── 功能层/          # Feature Layer
│       ├── flash.rs     # 刷写功能
│       ├── partition.rs # 分区管理
│       └── progress.rs  # 进度显示
```

---

##  性能对比

<div align="center">

| 工具 | 刷写速度 | Sparse 支持 | 跨平台 |
|:---:|:---:|:---:|:---:|
| **Fastboot-RS** | 🟢 40+ MB/s | ✅ | ✅ |
| Google Fastboot | 🟡 30 MB/s | ✅ | ✅ |

</div>

---

## 🤝 贡献

欢迎提交 Issue 和 Pull Request！

---

<div align="center">

<!-- 底部波浪 -->
<img src="https://capsule-render.vercel.app/api?type=waving&color=gradient&customColorList=24&height=100&section=footer"/>

**Made with ❤️ and 🦀 by [GriefRedd](https://github.com/wumai2580)**

</div>
