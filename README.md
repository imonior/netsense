# NetSense

> 跨平台独立桌面应用：根据当前网络身份（SSID / 网关 MAC / BSSID）自动匹配并应用网络 profile（静态 IP / DHCP / DNS / IPv6），带健康度监测（失联回落 DHCP 保底）与按网络触发的自动化任务（设路由 / 开软件 / 跑脚本）。

由 [hammerspoon-wifi-switcher](https://github.com/imonior/hammerspoon-wifi-switcher) 演进为**不依赖 Hammerspoon** 的独立软件，支持 macOS / Windows / Linux。

## 特性

- **网络身份匹配**：SSID、网关 MAC、AP BSSID 三者均可作为独立判断条件（可单独或组合使用，AND 关系）。同名 SSID 多场景、伪造热点（evil twin）也能正确区分。
- **每 SSID profile**：静态 IP / DHCP / 自定义 DNS / IPv6（automatic / manual / off）。
- **全局回退**：`__DEFAULT__` 应用到任何未配置网络。
- **健康度监测**：ICMP / HTTP / both 探测；连续失败且开启回落时自动切回 DHCP 保底，不中断上网。
- **自动化任务**：按网络触发 `route` / `launch` / `run`（netsetman 式附加动作），`on_apply` / `on_revert` 触发，脚本受 allow-list 约束。
- **托盘弹窗面板**：左键点状态栏/托盘图标即弹出面板（状态 + 一键切换 profile + 语言），失焦自动收起；右键出原生菜单。
- **三平台同一套代码**：平台差异全部收敛在 PAL（`platform/{macos,windows,linux}.rs`），上层只依赖 trait。
- **提权体验**：macOS 装一次 `sudoers` 白名单后免密；Windows 以管理员运行一次即免 UAC；Linux 配 `sudo -n` 即免弹窗。不可用时自动回落系统授权框，功能不中断。
- **多语言（zh / en / zh-TW / ja / ko）**，79 个 key × 5 语，带 parity 校验。

## 技术栈

- **Tauri v2 (Rust)** + 系统 WebView（前端复用现有 HTML/CSS 编辑器，无 Node 构建链）
- 后端 Rust：Core Engine（匹配 / 应用 / 健康度 / 自动化）+ PAL（平台抽象层）
- 平台实现（同一套上层代码，编译期选择）：

| 平台 | 读 | 写 | 提权 |
|------|----|----|------|
| macOS | `networksetup` / `arp` / `airport` | `networksetup` / `route` | 免密 sudoers 白名单脚本，回落 `osascript` 授权框 |
| Windows | PowerShell CIM（`Get-NetAdapter` / `Get-NetConnectionProfile` …）+ `netsh` | `netsh` / `New-NetRoute` | 已是管理员则免弹窗，否则 UAC |
| Linux | `nmcli` / `ip neigh` | `nmcli con mod` / `ip route` | `sudo -n` 可用则免弹窗，否则 `pkexec` |

## 快速开始

### 方式一：云端出包（推荐，本地零依赖）

推一个 tag 即可由 CI 同时产出三平台安装包与可执行文件（`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`）：

```bash
git tag v0.3.0 && git push origin v0.3.0
```

也可在 GitHub 网页端 Actions → build → Run workflow 手动触发。

### 方式二：本机构建

```bash
# 前置：Rust + 各平台构建依赖（Windows 另需 MSVC C++ 生成工具 + WebView2）

# Windows（脚本会先体检 Rust/MSVC/SDK/WebView2/磁盘，缺什么告诉你装什么）
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1            # 出裸 exe
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer # 出 msi/nsis

# macOS / Linux
cd src-tauri && cargo build --release   # 裸可执行文件
cd src-tauri && cargo tauri build       # 出安装包（需 tauri-cli）

# macOS 可选：装免密特权通道，消除每次改网络的授权弹窗
sh scripts/install-priv-helper.sh
```

### 校验（不需编译，任何平台可跑）

```bash
python scripts/validate.py     # JSON / 5 语 i18n parity / PAL 边界 / 三平台 trait 覆盖
cd src-tauri && cargo test --lib
```

启动后以托盘形态常驻（不弹主窗口）：**左键点托盘图标**打开弹窗面板，
右键出菜单（显示编辑器 / 打开日志目录 / 查看权限通道 / 退出）。

> **Windows 首次改网络会弹一次 UAC**。以管理员身份运行一次可免除（此后提权通道显示"无需授权"）。

## 配置

编辑 `config.json`（从 `config.example.json` 复制）。关键结构：

```json
{
  "__DEFAULT__": { "mode": "dhcp", "dns": "", "v6mode": "automatic" },
  "Office_5G": {
    "match": { "ssid": "Office_5G", "gateway_mac": "aa:bb:cc:dd:ee:ff", "bssid": "00:11:22:33:44:55" },
    "mode": "manual", "ip": "192.168.1.100", "gateway": "192.168.1.1", "dns": "192.168.1.1,8.8.8.8",
    "health":   { "enabled": true, "fallback": { "enabled": true }, "mode": "both" },
    "automation": { "enabled": true, "on_apply": [ { "type": "route", "dest": "10.0.0.0/8", "gateway": "192.168.1.1" } ] }
  }
}
```

详见 [DEVELOPMENT.md](DEVELOPMENT.md)。

## 许可证

MIT
