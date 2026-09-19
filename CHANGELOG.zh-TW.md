# 更新日誌

NetSense 的所有重要變更記錄於此。格式遵循 [Keep a Changelog](https://keepachangelog.com/)，版本號遵循 [語意化版本](https://semver.org/)。

## [0.2.1] - 2026-09-19

### ✨ 新增
- 多語言文件，**預設英文**：`README.md`（英文）外加 `README.zh.md` / `README.zh-TW.md` / `README.ja.md` / `README.ko.md`；`CHANGELOG.md`（英文）外加 `CHANGELOG.zh.md` / `CHANGELOG.zh-TW.md` / `CHANGELOG.ja.md` / `CHANGELOG.ko.md`。
- 新增 `VERSION` 檔案作為版本號唯一真值；`scripts/bump_version.sh` 將其同步進 `tauri.conf.json` 與 `Cargo.toml`。

### 🔧 變更
- **Windows 安裝器改為依電腦安裝**（`bundle.windows.nsis.installMode` / `wix.installMode` = `perMachine`）。應用安裝到 `C:\Program Files\NetSense`，安裝時需管理員權限（此前為每使用者安裝，位於 `%LOCALAPPDATA%` 下）。
- 發布說明改由英文 `CHANGELOG.md` 中對應版本段落產生，發布內容預設英文。

### 📝 文件
- `DEVELOPMENT.md` 改寫為英文（預設）。

## [0.2.0] - 2026-09-18

### ✨ 新增
- **真正的跨平台支援**（macOS / Windows / Linux），透過平台抽象層（PAL）實現：所有系統差異收斂在 `platform/{macos,windows,linux}.rs`，統一在 `NetworkPlatform` trait 之後，編譯期選擇。上層引擎只依賴該 trait。
- **四目標 CI 建置矩陣**（`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`）。推送 `v*` tag 觸發 Draft Release，附各平台安裝包（Windows 為 NSIS + MSI，macOS 為 DMG，Linux 為 DEB）及裸執行檔。
- **應用內多語言介面**（en / zh / zh-TW / ja / ko）：79 個 key × 5 語，由 `scripts/validate.py` 做 parity 校驗。
- **提權通道**並優雅回落：macOS 安裝 `sudoers` 白名單後免密；Windows 已是管理員則免彈窗（否則 UAC）；Linux 設 `sudo -n` 免彈窗（否則 `pkexec`）。
- **健康度監測**：ICMP / HTTP / both 探測；連續失敗且開啟回落時自動切回 DHCP 保底。
- **依網路觸發的自動化**：`route` / `launch` / `run` 動作在 `on_apply` / `on_revert` 觸發，指令稿受 allow-list 約束。

### 🐛 修復
- Windows CI 打包：在 `windows-latest` 上明確安裝 WiX + NSIS（預設不帶），並依平台收斂 bundle 目標。

### 🛠 內部
- `scripts/validate.py` 增加 JSON 校驗、5 語 i18n parity（79×5）、PAL 邊界、三平台 trait 覆蓋檢查。
