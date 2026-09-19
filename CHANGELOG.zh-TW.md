# 更新日誌

NetSense 的所有重要變更記錄於此。格式遵循 [Keep a Changelog](https://keepachangelog.com/)，版本號遵循 [語意化版本](https://semver.org/)。

## [0.2.1] - 2026-09-19

### ✨ 新增
- 多語言文件，**預設英文**：`README.md`（英文）外加 `README.zh.md` / `README.zh-TW.md` / `README.ja.md` / `README.ko.md`；`CHANGELOG.md`（英文）外加 `CHANGELOG.zh.md` / `CHANGELOG.zh-TW.md` / `CHANGELOG.ja.md` / `CHANGELOG.ko.md`。
- 新增 `VERSION` 檔案作為版本號唯一真值；`scripts/bump_version.sh` 將其同步進 `tauri.conf.json` 與 `Cargo.toml`。
- 應用圖示：以 `app-icon.png`（1024² 母圖）作為視窗、系統匣與安裝程式圖示的來源，`scripts/gen_icons.py` 由其衍生整套圖示——各尺寸 PNG、7 幀 ICO（16 → 256，BMP 幀 + PNG 幀）與 8 段 ICNS（ic07–ic14）。

### 🔧 變更
- **Windows 安裝器改為依電腦安裝**（`bundle.windows.nsis.installMode` = `perMachine`）。應用安裝到 `C:\Program Files\NetSense`，安裝時需管理員權限（此前為每使用者安裝，位於 `%LOCALAPPDATA%` 下）。MSI 沒有對應選項——WiX 本就裝到 `%PROGRAMFILES%`——因此不再設定該欄位。
- 發布說明改由英文 `CHANGELOG.md` 中對應版本段落產生，發布內容預設英文。
- `scripts/gen_icons.py` 不再繪製佔位圖形：改為對母圖重新取樣、在每個目標尺寸套用圓角遮罩，並封裝 PNG/ICO/ICNS 容器（純 stdlib，無第三方相依）。

### 🐛 修正
- **Windows：不再出現殘留主控台視窗。** PAL 先前以 `Command::output()` 呼叫 `powershell.exe` / `netsh` 且未設定建立旗標，導致每次讀取狀態時 Windows 都會配置一個可見主控台（連同標題列按鈕一併顯示）——現已改用 `CREATE_NO_WINDOW` 啟動行程。
- **Windows：應用看起來「沒開啟」。** NetSense 是系統匣常駐應用，啟動時兩個視窗都不顯示，主控台視窗一消失就什麼都看不到。現改為啟動即開啟狀態面板，失焦或關閉時收回系統匣。
- CI：Windows 的 `choco install wixtoolset nsis` 步驟加上硬性逾時，下載停滯時不再一路掛到 runner 上限。

### 🔒 安全性
- **所有提權路徑統一做 shell 引用。** 來自設定檔或系統（設定檔名、SSID、閘道位址、路由）的值都會進入 `osascript` / `sudo` / shell。現已在插值處統一做 POSIX 單引號處理，含 `'`、`$(…)`、反引號或 `;` 的值無法再向 root shell 注入指令。
- **macOS 提權分支同樣加引用。** 免密的 `osascript … with administrator privileges` 路徑先前以字串串接組出命令列；現改為路徑與引數逐 token 引用。
- **Windows `v6prefix` 改為解析而非直插。** 系統回傳的前綴先前被直接拼進 PowerShell 命令列；現僅接受整數，否則丟棄該選項。
- **自動化指令碼允許清單不再可被路徑前綴繞過。** 形如 `scripts2/` 的目錄先前用純字串前綴比較就能匹配 `scripts/`；現改為兩側先正規化再按路徑分量比較。

### 🛠 內部
- `scripts/validate.py` 新增檢查 **[7] `tauri.conf.json` 欄位合法性**：對 `bundle`、`bundle.windows`、`nsis`、`wix` 子樹依官方 Tauri v2 schema 驗證，使非法欄位在 10 分鐘的驗證任務就失敗，而不是跑到四平台建置才爆。
- 發布步驟限定為僅 tag 推送時執行，手動 `workflow_dispatch` 驗證不再會改寫既有 Draft Release 的 tag。

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
