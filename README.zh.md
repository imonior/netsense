# NetSense

面向 macOS、Windows、Linux 的跨平台网络 **Profile** 管理器。

每个 Profile 回答三个问题：*我在哪个网络上？*（`Rules` / `Conditions`）、*这个网络应该是什么样？*
（命中时用 `THEN`，未命中时用 `ELSE`）、*之后还要发生什么？*（自动化动作）。NetSense 拿这些 Profile
去比对当前网络，始终只让其中一个处于 Active，并通过系统自带工具下发配置 —— macOS 用 `networksetup`，
Windows 用 PowerShell CIM + `netsh`，Linux 用 `nmcli`。

它常驻托盘。启动时会打开一次编辑器窗口 —— 这是「应用确实起来了」的唯一无歧义信号 —— 关掉它只是收回
托盘。没有需要配置的守护进程，不上传任何遥测数据。

## 决策是怎么做出的

```
            ┌──────────── 启用的 Profile ────────────┐
网络  ──▶  │  Rule = 启用的 Conditions 取 AND        │ ──▶ 命中 1 个  ──▶ 它成为 Active
   变化     │  Rule 之间取 OR · 每次按快照重算        │ ──▶ 命中 2+ 个 ──▶ Conflict（弹窗，什么都不下发）
            └────────────────────────────────────────┘ ──▶ 命中 0 个  ──▶ 全局 fallback（它不是 Profile）
```

- **匹配用的是网络身份**：Wi‑Fi SSID、网关 MAC、AP BSSID —— 因此不同地点的同名 SSID、以及伪造热点
  （evil twin）都能区分开。另有 `network_interface` 条件可用，面向有线链路，以及「指的就是这块网卡」
  的场合。
- **没有 Profile 优先级**，这是刻意的。命中多个时唯一诚实的答案就是 *Conflict*：NetSense 把它显示
  出来并且什么都不下发，而不是悄悄挑一个赢家。
- **下发是一道屏障（3A）**：IPv4 / 掩码 / 网关 / DNS / IPv6 与静态路由一起下发；分支配置了 `verify`
  时，再回读系统状态做校验，可选叠加 ICMP/HTTP 健康度探测 —— 持续不通就回落 DHCP。
- **自动化（3B）只在 3A 通过后才执行。** 一次性动作（`launch_app`、`run_script`）按 `priority` 分批：
  数值小者先跑、同批并发、一批结束后才进下一批、某一批内失败不影响后续批次。匹配失败（Conflict）与
  执行失败（Error）是两种状态，分开呈现。
- **检测策略是 Profile 级的**：响应网络事件、按间隔轮询、或两者并存，各自带变化延迟 —— 一次瞬断
  重连不会反复改写网卡。
- **常驻动作维持的是一个状态，而不是把命令重复执行。** 每条启用的常驻动作有自己的 worker，随 Active
  方案的 THEN 分支启动，并在下发任何新配置之前停掉；隧道核对发现本来就是通的，就一条命令都不发，
  而且 worker 绝不会弹出授权框。（`periodic_script` 是唯一没有「可核对状态」的一类：它的 tick 本身就
  是跑脚本，因此按设计重复执行。）

## 特性

- 每个 Profile 的静态 IP / DHCP / 自定义 DNS / IPv6（automatic、manual、off）/ 静态路由，并分
  THEN 与 ELSE 两条分支。
- 回读校验与健康度监测在下发步骤里、按分支配置：只有系统确认落地了，Profile 才报「已应用」。
- 托盘弹窗面板：实时状态、全部在用网卡、VPN 隧道、带匹配徽章的一键切换 Profile、语言切换。左键弹出，
  失焦收起，右键出原生菜单。
- 覆盖整个模型的配置编辑器 —— Rules、Conditions、3A、路由、动作、ELSE 与全局 fallback —— 并把引擎
  的实时判定就地渲染出来。
- 能免密就免密：macOS 一次性安装 `sudoers` 白名单、Windows 以管理员运行一次、Linux 配 `sudo -n`。
  条件不具备时回落到系统授权对话框，而不是直接失败。
- 在线升级：检查 GitHub Releases 并挑出本平台的产物。Homebrew 安装走 `brew upgrade --cask`，全程不
  下载任何东西；其他方式则下载产物，只有它的 SHA256 与该 Release 的 `SHA256SUMS` 对得上才安装 ——
  这一步做不了（没有 `SHA256SUMS`、里面没列这个产物、或取不到）就中止升级，改为把你指到 Release 页。
- 五种界面语言（English、简体中文、繁體中文、日本語、한국어），key 对齐经过校验，默认 English。

## 技术栈

Tauri v2 + Rust、系统 WebView、前端为纯静态 HTML/CSS/JS（无 Node 构建链）。
Core Engine（detection / conditions / 匹配 / network / automation）从不直接调用系统命令：所有平台相关
部分都收在 PAL 的同一个 trait 之后（`src-tauri/src/platform/{macos,windows,linux}.rs`），编译期选择实现。

| 平台 | 读取 | 写入 | 提权 |
|------|------|------|------|
| macOS | `networksetup` / `ipconfig` / `arp` / `system_profiler`（还有仍在时的 `airport`） | `networksetup` / `route` | sudoers 白名单，否则 `osascript` 授权框 |
| Windows | PowerShell CIM（`Get-NetAdapter` …）+ `netsh` | `netsh` / `New-NetRoute` | 已是管理员则免弹窗，否则 UAC |
| Linux | `nmcli` / `ip neigh` | `nmcli con mod` / `ip route` | `sudo -n`，否则 `pkexec` |

## 快速开始

### 云端构建（本地零依赖）

推一个版本 tag 就会让 CI 同时构建四个目标
（`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`），并创建一个附带安装包的 **Draft**
Release；等四个平台的产物都齐、`SHA256SUMS` 也生成好之后，它自己转为正式发布：

```bash
git tag v1.0.0 && git push origin v1.0.0
```

同一个 workflow 也可在 GitHub → Actions → build → Run workflow 手动触发；那种方式只产出裸可执行文件，
不创建 Release。

### 本地构建

```bash
# 需要 Rust 以及各平台的构建依赖
#（Windows 另需 MSVC C++ 生成工具与 WebView2）

# Windows —— 脚本先体检 Rust/MSVC/SDK/WebView2/磁盘，缺什么告诉你装什么
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1            # 出裸 exe
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer # 出 msi / nsis

# macOS / Linux
cd src-tauri && cargo build --release   # 裸可执行文件
cd src-tauri && cargo tauri build       # 出安装包（需 tauri-cli）

# macOS 可选：安装免密特权通道，让每次改网络不再弹授权
sh scripts/install-priv-helper.sh
```

### 校验（不需编译，任何平台可跑）

```bash
python3 scripts/validate.py    # JSON 合法性；5 语 i18n key 与占位符对齐；key 引用双向检查；
                               # PAL 边界；三平台 trait 覆盖；tauri.conf.json 字段；版本号一致性；
                               # 文档与代码对账
node scripts/editor-smoke.mjs  # 无头跑编辑器的数据绑定（需要 Node，但没有构建步骤）
cd src-tauri && cargo test     # 纯 bin crate —— 用 cargo test，不是 --lib
```

NetSense 常驻托盘：左键打开面板，右键出菜单（设置 · 打开日志目录 · 把当前网络设为 DHCP · 立即探测 ·
退出，这几项之上还有只读的行，展示在用的网卡与隧道）。它的配置是一个单独的 `config.json`（与 NetSense
可执行文件同目录）；`config.example.json` 是一份可直接参考的完整示例。

> Windows 上首次改网络会弹一次 UAC。以管理员身份运行一次即可免除，此后提权通道显示「无需授权」。

## 配置

```jsonc
{
  "schema": 1,
  "language": "en",
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

有两条约定值得先知道。**空字符串表示「显式清空」，而字段缺省表示「不要动它」**。`dns` 也按这个
口径建模，编辑器的 DNS 三态下拉因此不是摆设：选「不修改」是删掉这个键，三平台都不会下发任何 DNS
命令；选「系统自动获取」写入空串，已设的 DNS 会被清空；选「DNS Servers」则按输入框的值下发。
另外，`run_script` 只会执行 `<配置目录>/scripts` 之下、或在 `allowed_scripts` 里登记过的路径；
相对路径按 `config.json` 所在目录解释，而不是按进程启动时的目录。

模型细节、引擎流水线与各平台坑点详见 `DEVELOPMENT.md`（英文）。

## 许可证

MIT
