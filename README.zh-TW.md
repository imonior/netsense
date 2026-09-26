# NetSense

[English](README.md) · [简体中文](README.zh.md) · **繁體中文** · [日本語](README.ja.md) · [한국어](README.ko.md)

面向 macOS、Windows、Linux 的跨平台網路 **Profile** 管理器。

每個 Profile 回答三個問題：*我在哪個網路上？*（`Rules` / `Conditions`）、*這個網路應該是什麼樣子？*
（命中時用 `THEN`，未命中時用 `ELSE`）、*之後應該發生什麼？*（自動化動作）。NetSense 拿這些 Profile
去比對即時網路，始終只讓其中一個保持 Active，並透過系統自帶的工具下發結果配置 —— macOS 用
`networksetup`、Windows 用 PowerShell CIM + `netsh`、Linux 用 `nmcli`。

它常駐於托盤。啟動時會開啟一次配置編輯器視窗 —— 這是「應用程式確實起來了」唯一無歧義的訊號 ——
而關掉它只是把它收回托盤。沒有需要設定的守護行程、不上傳遙測資料。

## 決策是怎麼做出的

```
            ┌───────────── 啟用的 Profile ─────────────┐
網路 ──▶    │  Rule = 啟用的 Conditions 取 AND         │ ──▶ 命中 1 個  ──▶ 該 Profile 成為 Active
   變化     │  Rules 之間取 OR · 每次快照重算          │ ──▶ 命中 2+ 個 ──▶ Conflict（彈窗，什麼都不下發）
            └──────────────────────────────────────────┘ ──▶ 命中 0 個  ──▶ 全域 fallback（它不是 Profile）
```

- **比對用的是網路身分**：Wi‑Fi SSID、閘道 MAC、AP BSSID —— 因此不同地點的同名 SSID、以及偽造熱點
  都能區分開。另有 `network_interface` 條件可用，用於有線連結，以及「指的就是這張網卡」的場合。
- **沒有 Profile 優先級**，這是刻意的。命中多個時，唯一誠實的答案就是 *Conflict*：NetSense 會把它
  顯示出來並且什麼都不下發，而不是悄悄挑一個贏家。
- **下發是一道屏障（3A）**：IPv4 / 子網路遮罩 / 閘道 / DNS / IPv6 與靜態路由一起下發；分支設定了
  `verify` 時，再回讀系統狀態做驗證，可選再疊加一次 ICMP/HTTP 健康度探測 —— 網路持續不通就回落 DHCP。
- **自動化（3B）只在 3A 通過之後才執行。** 一次性動作（`launch_app`、`run_script`、`set_default_printer`）
  按 `priority` 分批執行 —— 數值小者先跑、同批併行、一批結束才進下一批，且某一批內失敗不會阻擋後續批次。
  設定預設印表機改的只是*目前使用者*的預設值，所以切換網路不會跳出授權對話框。匹配失敗
  （Conflict）與執行失敗（Error）是兩種狀態，分開呈現。
- **偵測方式是 Profile 級的**：對網路事件作出反應、按間隔輪詢、或兩者並存，各自帶自己的變化延遲
  —— 一次不穩的重連因此不會反覆改寫網卡。
- **常駐動作維持的是一個狀態，而不是把命令重複執行。** 每個啟用的常駐動作有自己的 worker，隨 Active
  方案的 THEN 分支啟動，並在下發任何新設定之前停掉；通道核對發現它本來就是通的，就一條命令都不發出，
  而且 worker 絕不會彈出授權框。（`periodic_script` 是唯一沒有「可核對狀態」的一類：它的 tick 本身就
  是執行腳本，因此按設計重複執行。）

## 特性

- 每個 Profile 的靜態 IP / DHCP / 自訂 DNS / IPv6（automatic、manual、off）/ 靜態路由，並分出
  THEN 與 ELSE 兩條分支。
- 回讀驗證與健康度監測在下發步驟裡、按分支設定：只有系統確認落地了，Profile 才報「已套用」。
- 托盤彈出面板：目前使用中的網路（網路介面、SSID 與訊號強度、MAC、IPv4 / 遮罩 / 閘道 / IPv6 / DNS）、
  其餘使用中的網卡、VPN 通道、帶命中徽章的一鍵切換 Profile，以及所有入口 —— 設定、紀錄、DHCP、
  探測、升級、結束。點圖示（左右鍵皆同）開啟，失焦時收起；沒有原生托盤選單。
- 覆蓋整個模型的配置編輯器 —— Rules、Conditions、3A、路由、動作、ELSE 與全域 fallback —— 並把
  引擎的即時判定就地渲染出來。
- 一個軟體設定視窗，裝的是與任何網路都無關的設定：介面語言、登入時啟動（每次開啟視窗都向作業系統讀一次，而不是取自檔案裡的副本）、`config.json` 與紀錄放在哪裡，以及紀錄保留多少天。
- 能免密就免密：macOS 一次性安裝 `sudoers` 白名單、Windows 以管理員身分執行一次、Linux 用
  `sudo -n`。條件不具備時，NetSense 回落到系統授權對話框，而不是直接失敗。
- 線上升級：檢查 GitHub Releases 並挑出本平台的產物。Homebrew 安裝走 `brew upgrade --cask`，全程不下載任何東西；其他方式則下載產物，只有它的 SHA256 與該 Release 的 `SHA256SUMS` 對得上才安裝 —— 這一步做不了（沒有 `SHA256SUMS`、裡面沒列這個產物、或取不到）就中止，改為把你導到發布頁。
- 五種介面語言（English、简体中文、繁體中文、日本語、한국어），對齊情況經過校驗；English 是預設值。
  三種平台上的每一座視窗都跟著這個選擇走 —— 托盤面板、編輯器、軟體設定、日誌視窗，連托盤提示與原生錯誤
  對話框也是；`scripts/validate.py` 裡有一項檢查專門盯著靜態文案有沒有寫死。

## 技術堆疊

Tauri v2 + Rust、系統 WebView、前端為純靜態 HTML/CSS/JS（無 Node 建置鏈）。Core Engine
（detection / conditions / 比對 / network / automation）從不直接呼叫系統命令：所有平台相關的部分都
收在 PAL 的同一個 trait 之後（`src-tauri/src/platform/{macos,windows,linux}.rs`），於編譯期選定。

| 平台 | 讀取 | 寫入 | 提權 |
|------|------|------|------|
| macOS | `CoreWLAN` / `networksetup` / `ipconfig` / `arp` / `system_profiler`（以及仍在時的 `airport`） | `networksetup` / `route` | sudoers 白名單，否則 `osascript` 授權框 |
| Windows | PowerShell CIM（`Get-NetAdapter` …）+ `netsh` | `netsh` / `New-NetRoute` | 已是管理員則免授權，否則 UAC |
| Linux | `nmcli` / `ip neigh` | `nmcli con mod` / `ip route` | `sudo -n`，否則 `pkexec` |

## 快速開始

### 雲端建置（本機零依賴）

推一個版本 tag，就會讓 CI 同時跑四個目標
（`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`），並開出一個附上安裝包的 **Draft** Release；
等四個平台的產物都齊、`SHA256SUMS` 也產生好之後，它自己轉為正式發行：

```bash
git tag v1.0.0 && git push origin v1.0.0
```

同一個 workflow 也可以在 GitHub → Actions → build → Run workflow 手動啟動，那種做法只產出裸
可執行檔、不產 Release。

### 本機建置

```bash
# 需要 Rust 以及各平台的建置依賴
#（Windows 另需 MSVC C++ 建置工具與 WebView2）

# Windows —— 指令稿會先檢查 Rust/MSVC/SDK/WebView2/磁碟，並告訴你缺了什麼
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1            # 可執行檔
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer # msi / nsis

# macOS / Linux
cd src-tauri && cargo build --release   # 可執行檔
cd src-tauri && cargo tauri build       # 安裝包（需 tauri-cli）

# macOS 選用：安裝免密特權通道，讓每次改網路不再要求授權
sh scripts/install-priv-helper.sh
```

### 校驗（不需編譯，任何平台都能跑）

```bash
python3 scripts/validate.py    # 5 種語言的 JSON、i18n 對齊與佔位符、雙向檢查 key 的使用情況、
                               # PAL 邊界、三平台的 trait 覆蓋、tauri.conf.json 欄位、版本一致性、
                               # 文件與程式碼對賬、以及介面文案是否全部取自字典
node scripts/editor-smoke.mjs  # 無頭跑編輯器的資料繫結（需要 Node，但沒有建置步驟）
cd src-tauri && cargo test     # 純 bin crate —— 用 `cargo test`，不是 `--lib`
```

NetSense 常駐托盤：點圖示（左右鍵皆同）開啟面板，所有入口都在面板裡（設定 · 開啟日誌目錄 ·
把目前網路設為 DHCP · 立即探測 · 結束；這些按鈕之上就是使用中的網卡與通道）。它由兩份設定檔驅動，因為這兩類設定毫無關係：哪個網路
得到哪套處理，是自動化設定 `config.json`，在編輯器視窗裡改；應用本身怎麼表現 —— 介面語言、日誌保留天數、登入時啟動 ——
是軟體設定 `settings.json`，在面板「設定」開啟的視窗裡改，改變它永遠不會重新下發網路設定。
兩份檔案都在屬於目前使用者的 NetSense 目錄裡 —— macOS 為 `~/Library/Application Support/NetSense`、Windows 為
`%APPDATA%\NetSense`、Linux 為 `~/.config/netsense`；它們不在可執行檔的同目錄：簽章過的 macOS
應用包和唯讀的 `Program Files` 都不該被寫入。日誌放在目前使用者的 NetSense 日誌目錄。`config.example.json` 是一份完整的示例。

> Windows 上網路設定的提權**每次執行僅提示一次 UAC**（首次下發會拉起常駐的提權助手，後續批次直接經由它；拒絕授權或助手不可用時退回逐批提示）。以管理員身分執行可連這一次都免除，之後權限通道會回報「無需授權」。

## 配置

```jsonc
{
  "schema": 1,
  "allowed_scripts": ["/opt/ops/office-init.sh"],
  "profiles": [{
    "id": "office", "name": "Office_5G", "enabled": true,
    "detection": { "mode": "network_events_and_polling", "poll_interval_secs": 30 },
    "rules": [{
      "id": "r1", "enabled": true,
      "conditions": [
        { "id": "c1", "enabled": true, "type": "wifi_ssid",   "value": "Office_5G" },
        { "id": "c2", "enabled": true, "type": "gateway_mac", "value": "aa:bb:cc:dd:ee:ff" }
      ]
    }],
    "then": {
      "network": {
        "mode": "manual", "ip": "192.168.1.100", "netmask": "255.255.255.0",
        "gateway": "192.168.1.1", "dns": "192.168.1.1,8.8.8.8", "v6mode": "off",
        "routes": [{ "dest": "10.0.0.0/8", "gateway": "192.168.1.1", "metric": 0 }],
        "verify": { "readback": true,
                    "health": { "enabled": true, "mode": "both",
                                "icmp_target": "192.168.1.1",
                                "http_target": "http://192.168.1.1/",
                                "interval": 30, "retries": 3, "timeout": 5,
                                "fallback": { "enabled": true } } }
      },
      "one_shot": [
        { "id": "a1", "enabled": true, "priority": 1,
          "action": { "type": "run_script", "path": "/opt/ops/office-init.sh", "elevated": false } }
      ]
    },
    "else": { "network": { "mode": "dhcp", "dns": "", "routes": [{ "dest": "10.0.0.0/8", "delete": true }] } }
  }],
  "fallback": { "enabled": true, "network": { "mode": "dhcp", "dns": "", "v6mode": "automatic" } }
}
```

有兩條約定值得先知道。**空字串表示清空該欄位**；而**缺省的欄位**表示不要動它。`dns` 也照這個
口徑建模，編輯器的 DNS 三態下拉因此不是擺設：選「不修改」是刪掉這個鍵，三平台都不會下發任何 DNS
命令；選「系統自動取得」會寫入空字串，已設定的 DNS 會被清空；選「DNS Servers」則按輸入框的值下發。
另外，`run_script` 只會執行位於 `<配置目錄>/scripts` 之下、或在
`allowed_scripts` 裡登記過的路徑；相對路徑以 `config.json` 所在的目錄為準，而不是行程啟動時的目錄。

`DEVELOPMENT.md` 詳細說明了這個模型、引擎流水線以及各平台的坑點。

## 授權

MIT
