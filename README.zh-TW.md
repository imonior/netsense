# NetSense

> 跨平台獨立桌面應用：根據目前網路身分（SSID / 閘道 MAC / BSSID）自動比對並套用網路 profile（靜態 IP / DHCP / DNS / IPv6），具備健康度監測（斷線時回退 DHCP 保底）與依網路觸發的自動化任務（設路由 / 開啟軟體 / 執行指令稿）。

由 [hammerspoon-wifi-switcher](https://github.com/imonior/hammerspoon-wifi-switcher) 演進為**不依賴 Hammerspoon** 的獨立軟體，支援 macOS / Windows / Linux。

## 特性

- **網路身分比對**：SSID、閘道 MAC、AP BSSID 三者皆可作為獨立判斷條件（可單獨或組合使用，AND 關係）。同名 SSID 多場景、偽造熱點（evil twin）也能正確區分。
- **每 SSID profile**：靜態 IP / DHCP / 自訂 DNS / IPv6（automatic / manual / off）。
- **全域回退**：`__DEFAULT__` 套用到任何未設定的網路。
- **健康度監測**：ICMP / HTTP / both 探測；連續失敗且開啟回退時自動切回 DHCP 保底，不中斷上網。
- **自動化任務**：依網路觸發 `route` / `launch` / `run`（netsetman 式附加動作），`on_apply` / `on_revert` 觸發，指令稿受 allow-list 約束。
- **托盤彈出面板**：左鍵點狀態列 / 托盤圖示即彈出面板（狀態 + 一鍵切換 profile + 語言），失焦自動收起；右鍵出原生選單。
- **三平台同一套程式碼**：平台差異全部收斂在 PAL（`platform/{macos,windows,linux}.rs`），上層只依賴 trait。
- **提權體驗**：macOS 裝一次 `sudoers` 白名單後免密；Windows 以管理員執行一次即免 UAC；Linux 設 `sudo -n` 即免彈窗。不可用時自動回落系統授權框，功能不中斷。
- **多語言（zh / en / zh-TW / ja / ko）**，79 個 key × 5 語，帶 parity 校驗。

## 技術堆疊

- **Tauri v2 (Rust)** + 系統 WebView（前端複用現有 HTML/CSS 編輯器，無 Node 建構鏈）
- 後端 Rust：Core Engine（比對 / 套用 / 健康度 / 自動化）+ PAL（平台抽象層）
- 平台實作（同一套上層程式碼，編譯期選擇）：

| 平台 | 讀 | 寫 | 提權 |
|------|----|----|------|
| macOS | `networksetup` / `arp` / `airport` | `networksetup` / `route` | 免密 sudoers 白名單指令稿，回落 `osascript` 授權框 |
| Windows | PowerShell CIM（`Get-NetAdapter` / `Get-NetConnectionProfile` …）+ `netsh` | `netsh` / `New-NetRoute` | 已是管理員則免彈窗，否則 UAC |
| Linux | `nmcli` / `ip neigh` | `nmcli con mod` / `ip route` | `sudo -n` 可用則免彈窗，否則 `pkexec` |

## 快速開始

### 方式一：雲端建置（推薦，本機零依賴）

推一個 tag 即可由 CI 同時產出三平台安裝包與執行檔（`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`）：

```bash
git tag v0.3.0 && git push origin v0.3.0
```

也可在 GitHub 網頁端 Actions → build → Run workflow 手動觸發。

### 方式二：本機建置

```bash
# 前置：Rust + 各平台建置依賴（Windows 另需 MSVC C++ 建置工具 + WebView2）

# Windows（指令稿會先體檢 Rust/MSVC/SDK/WebView2/磁碟，缺什麼告訴你裝什麼）
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1            # 出裸 exe
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer # 出 msi/nsis

# macOS / Linux
cd src-tauri && cargo build --release   # 裸執行檔
cd src-tauri && cargo tauri build       # 出安裝包（需 tauri-cli）

# macOS 選用：裝免密特權通道，消除每次改網路的授權彈窗
sh scripts/install-priv-helper.sh
```

### 校驗（不需編譯，任何平台可跑）

```bash
python scripts/validate.py     # JSON / 5 語 i18n parity / PAL 邊界 / 三平台 trait 覆蓋
cd src-tauri && cargo test       # 本專案為純 bin crate（無 lib target），用 cargo test 而非 --lib
```

啟動後以托盤形態常駐（不彈主視窗）：**左鍵點托盤圖示**開啟彈出面板，
右鍵出選單（顯示編輯器 / 開啟日誌目錄 / 檢視權限通道 / 退出）。

> **Windows 首次改網路會彈一次 UAC**。以管理員身分執行一次可免除（此後提權通道顯示「無需授權」）。

## 組態

編輯 `config.json`（從 `config.example.json` 複製）。關鍵結構：

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

詳見 [DEVELOPMENT.md](DEVELOPMENT.md)。

## 授權

MIT
