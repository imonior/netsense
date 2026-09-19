# 更新日志

NetSense 的所有重要变更记录于此。格式遵循 [Keep a Changelog](https://keepachangelog.com/)，版本号遵循 [语义化版本](https://semver.org/)。

## [0.2.1] - 2026-09-19

### ✨ 新增
- 多语言文档，**默认英文**：`README.md`（英文）外加 `README.zh.md` / `README.zh-TW.md` / `README.ja.md` / `README.ko.md`；`CHANGELOG.md`（英文）外加 `CHANGELOG.zh.md` / `CHANGELOG.zh-TW.md` / `CHANGELOG.ja.md` / `CHANGELOG.ko.md`。
- 新增 `VERSION` 文件作为版本号唯一真值；`scripts/bump_version.sh` 将其同步进 `tauri.conf.json` 与 `Cargo.toml`。

### 🔧 变更
- **Windows 安装器改为按计算机安装**（`bundle.windows.nsis.installMode` / `wix.installMode` = `perMachine`）。应用安装到 `C:\Program Files\NetSense`，安装时需管理员权限（此前为每用户安装，位于 `%LOCALAPPDATA%` 下）。
- 发布说明改由英文 `CHANGELOG.md` 中对应版本段落生成，发布内容默认英文。

### 📝 文档
- `DEVELOPMENT.md` 改写为英文（默认）。

## [0.2.0] - 2026-09-18

### ✨ 新增
- **真正的跨平台支持**（macOS / Windows / Linux），通过平台抽象层（PAL）实现：所有系统差异收敛在 `platform/{macos,windows,linux}.rs`，统一在 `NetworkPlatform` trait 之后，编译期选择。上层引擎只依赖该 trait。
- **四目标 CI 构建矩阵**（`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`）。推送 `v*` tag 触发 Draft Release，附各平台安装包（Windows 为 NSIS + MSI，macOS 为 DMG，Linux 为 DEB）及裸可执行文件。
- **应用内多语言界面**（en / zh / zh-TW / ja / ko）：79 个 key × 5 语，由 `scripts/validate.py` 做 parity 校验。
- **提权通道**并优雅回落：macOS 安装 `sudoers` 白名单后免密；Windows 已是管理员则免弹窗（否则 UAC）；Linux 配 `sudo -n` 免弹窗（否则 `pkexec`）。
- **健康度监测**：ICMP / HTTP / both 探测；连续失败且开启回落时自动切回 DHCP 保底。
- **按网络触发的自动化**：`route` / `launch` / `run` 动作在 `on_apply` / `on_revert` 触发，脚本受 allow-list 约束。

### 🐛 修复
- Windows CI 打包：在 `windows-latest` 上显式安装 WiX + NSIS（默认不带），并按平台收敛 bundle 目标。

### 🛠 内部
- `scripts/validate.py` 增加 JSON 校验、5 语 i18n parity（79×5）、PAL 边界、三平台 trait 覆盖检查。
