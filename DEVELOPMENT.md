# NetSense 开发指南 (DEVELOPMENT.md)

> 跨平台独立桌面应用：根据当前网络身份（SSID / 网关 MAC / BSSID）自动匹配并应用网络 profile（静态 IP / DHCP / DNS / IPv6），带健康度监测（失联回落 DHCP 保底）与按网络触发的自动化任务（设路由 / 开软件 / 跑脚本）。
> 由 `hammerspoon_wifi_switcher` 演进为**不依赖 Hammerspoon** 的独立软件。

---

## 1. 技术栈决策

| 项 | 选择 | 理由 |
|----|------|------|
| 壳 | **Tauri v2 (Rust)** + 系统 WebView | 二进制最小（5–15MB）、Rust 内存安全、依赖少；纯技术首选 |
| 后端 | Rust（Core Engine + PAL） | 类型安全、跨平台编译 |
| 前端 | 复用 `hammerspoon_wifi_switcher` 的 `editor.html` / `popups.html` | 纯 HTML/CSS，经 Tauri `invoke` 与后端通信（替代原 `hs.urlevent`） |
| i18n | 5 语（zh / en / zh-TW / ja / ko） | 借鉴 wireguide-plus 的 `i18n-check.js` key parity 校验，但**代码不复用**（wireguide 是 Wails+Go，本项目走 Rust） |

**Tauri vs Wails 复盘**：wireguide-plus 用 Wails+Go，可复用其延迟探测/i18n/发布流程；但本项目纯技术首选 Tauri（体积极小、内存安全）。两者皆合格，本项目选定 Tauri。

---

## 2. 架构（四层 + 平台分支）

```
┌─────────────────────────────────────────────┐
│ Frontend  前端编辑器（复用 editor.html / popups.html）
├─────────────────────────────────────────────┤
│ App Shell  Tauri：窗口 + 系统托盘 + 事件循环 + IPC
├─────────────────────────────────────────────┤
│ Core Engine  SSID 监视 · 匹配引擎 · 应用 · 健康度 · 自动化 · 校验 · 日志 · i18n
├─────────────────────────────────────────────┤
│ PAL  平台抽象层  getSSID / getStatus / applyProfile / setDHCP
│      / resolveGatewayMac / resolveBssid / probe / addRoute / launchApp / runScript
├──────────┬──────────────┬────────────────────┤
│ macOS    │ Windows      │ Linux              │
│ networksetup + CoreWLAN│ netsh + WMI/Wlan│ nmcli + D-Bus
└──────────┴──────────────┴────────────────────┘
```

运行时流程（SSID 变化 → 解析 SSID+网关MAC+BSSID → 匹配引擎 → 应用 Profile + onApply 自动化 → 健康度监测 loop；连续失败且 `fallback.enabled` 则回落 DHCP + onRevert）。

---

## 3. 仓库布局

```
netsense/
├── src-tauri/
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── build.rs
│   ├── icons/                 # 托盘/窗口图标（gen_icons.py 生成）
│   └── src/
│       ├── main.rs            # 入口：建窗/托盘/弹窗/注册命令/SSID watcher + 配置热重载 + 状态广播
│       ├── popup.rs           # 状态栏弹窗面板：定位 / 显隐 / 失焦收起 + 防抖
│       ├── config.rs          # 配置模型(serde) + 校验 + 读写 + language 字段
│       ├── platform.rs        # ★ PAL trait 契约 + 共享类型 + 共享工具 + 编译期平台选择
│       ├── platform/
│       │   ├── macos.rs       # macOS：networksetup / arp / airport / route / osascript
│       │   ├── windows.rs     # Windows：PowerShell(CIM) 读 + netsh 写 + UAC 提权
│       │   └── linux.rs       # Linux：nmcli 读写 + ip route + sudo/pkexec
│       ├── log.rs             # 按天轮转日志（保留 7 天）
│       ├── i18n.rs            # 5 语字典 + translate + key parity 校验
│       ├── i18n/              # en/zh/zh-TW/ja/ko.json（编译期 include_str! 嵌入）
│       ├── core/
│       │   ├── matcher.rs     # SSID/MAC/BSSID 独立条件匹配（含单测）
│       │   ├── health.rs      # 健康度监测 probe loop + 回落
│       │   └── automation.rs  # onApply/onRevert 任务执行 + allow-list
│       └── ipc.rs             # Tauri 命令（替代 hs.urlevent）
├── frontend/
│   ├── popup.html             # 状态栏弹窗面板（window label = popup）
│   ├── editor.html            # 配置编辑器（window label = main）
│   ├── index.html             # P1 诊断控制台（未挂载，备用）
│   ├── README.md              # 前端说明 + IPC 命令表
│   └── serve.sh               # 开发期静态服务器 :1420
├── scripts/
│   ├── netsense-priv.sh       # macOS 特权白名单包装脚本（root 属主，被 sudoers 授权）
│   ├── install-priv-helper.sh # macOS 安装/卸载特权通道（写 /etc/sudoers.d/netsense）
│   ├── validate.py            # ★ 与平台无关的静态校验（JSON / i18n parity / PAL 边界），CI 也跑
│   ├── build-windows.ps1      # ★ Windows 一键构建（含 MSVC/WebView2/磁盘体检）
│   ├── gen_icons.py           # 生成图标（纯 stdlib）
│   └── sandbox-bootstrap.sh   # 无 MSVC 环境下的 gnu 目标交叉编译验证（开发期自用）
├── .github/workflows/build.yml# ★ 三平台矩阵构建：push tag 即产出 win/mac/linux 安装包
├── config.example.json        # 配置模板（含新字段）
├── DEVELOPMENT.md             # 本文件
├── README.md
├── LICENSE
└── .gitignore
```

---

## 4. PAL 契约（src-tauri/src/platform.rs）

```rust
pub trait NetworkPlatform: Send + Sync {
    /// 注册 SSID 变化回调（事件驱动，替代 hs.wifi.watcher）
    fn watch_ssid(&self, cb: Box<dyn Fn(Option<String>) + Send + Sync>);

    fn get_current_ssid(&self) -> Option<String>;
    fn get_status(&self) -> InterfaceStatus;

    fn apply_profile(&self, p: &Profile) -> Result<(), String>;
    fn set_dhcp(&self) -> Result<(), String>;

    // —— 扩展1：独立匹配条件 ——
    /// 默认网关 IP → ARP/neighbor 缓存取 MAC
    fn resolve_gateway_mac(&self) -> Option<String>;
    /// 当前 AP 的 BSSID
    fn resolve_bssid(&self) -> Option<String>;

    // —— 扩展2：健康度监测 ——
    /// ICMP / HTTP 探测；both 模式下由调用方决定判定
    fn probe(&self, target: &ProbeTarget, timeout_ms: u64) -> Health;

    // —— 扩展3：自动化任务 ——
    fn add_route(&self, dest: &str, gw: &str, metric: u32) -> Result<(), String>;
    fn delete_route(&self, dest: &str) -> Result<(), String>;
    fn launch_app(&self, app: &str, args: &[String]) -> Result<(), String>;
    /// 受 allow-list 约束（仅 scripts/ 或显式登记路径），elevated 走特权通道
    fn run_script(&self, path: &str, args: &[String], elevated: bool) -> Result<(), String>;
}
```

跨平台实现要点：

| 能力 | macOS | Windows | Linux |
|------|-------|---------|-------|
| 当前 SSID | `networksetup -getairportnetwork` / CoreWLAN | `netsh wlan show interfaces` | `nmcli -t -f active,ssid dev wifi` |
| 网关 MAC | `arp -n <gw>` | `arp -a <gw>` | `ip neigh show <gw>` |
| BSSID | `airport -I en0` | `netsh wlan show interfaces`(BSSID) | `iw dev <if> link` |
| 应用静态 | `networksetup -setmanual` | `netsh interface ip set address` | `nmcli connection modify` |
| ICMP 探测 | `go-ping`/系统 ping | 同 | 同 |
| 加路由 | `route add` | `route add` | `ip route add` |
| 开软件 | `open <app>` | `start <exe>` | `xdg-open` |

---

## 5. 匹配语义（src-tauri/src/core/matcher.rs）

- 每个 Profile 的 `match` 对象含可选 `ssid` / `gateway_mac` / `bssid`，**三者独立、AND 关系**，未声明 = 通配。
- 非 `__DEFAULT__` 的 Profile **必须声明 ≥1 个** `match` 字段，否则不参与匹配。
- 多个 Profile 同时命中时：先比"命中的条件数最多者胜"（最具体），再比 `priority` 字段（大者胜）。
- 完全不命中 → 走 `__DEFAULT__`。
- 解析依赖 PAL：`resolve_gateway_mac()`、`resolve_bssid()`。

示例（仅靠网关 MAC 也能识别，SSID 隐藏/改名也无所谓）：

```json
"OfficeRouter": {
  "match": { "gateway_mac": "aa:bb:cc:dd:ee:ff" },
  "mode": "manual", "ip": "192.168.1.100", "gateway": "192.168.1.1", ...
}
```

---

## 6. 配置 schema（config.example.json + config.rs）

完整字段（新增部分加注释）：

```json
{
  "__DEFAULT__": { "mode": "dhcp", "dns": "", "v6mode": "automatic" },
  "Office_5G": {
    "match": {
      "ssid": "Office_5G",
      "gateway_mac": "aa:bb:cc:dd:ee:ff",
      "bssid": "00:11:22:33:44:55"
    },
    "mode": "manual",
    "ip": "192.168.1.100",
    "netmask": "255.255.255.0",
    "gateway": "192.168.1.1",
    "dns": "192.168.1.1,8.8.8.8",
    "v6mode": "off",
    "priority": 10,

    "health": {
      "enabled": true,                 // ① 是否开监测
      "fallback": { "enabled": true }, // ② 失联时是否回落 DHCP（关=只通知不改动）
      "mode": "both",
      "http_target": "http://cp.cloudflare.com",
      "icmp_target": "223.5.5.5",
      "interval": 30, "retries": 3, "timeout": 5
    },

    "automation": {
      "enabled": true,                // ③ 是否执行自动化任务
      "on_apply": [
        { "type": "route",  "dest": "10.0.0.0/8", "gateway": "192.168.1.1", "metric": 0 },
        { "type": "launch", "app": "/Applications/Slack.app" },
        { "type": "run",    "path": "scripts/office-vpn.sh", "elevated": false }
      ],
      "on_revert": [
        { "type": "route", "dest": "10.0.0.0/8", "delete": true }
      ]
    }
  }
}
```

三个开关**正交**：可"监测但不回落（只报警）"、可"回落开着但自动化关着"。

---

## 7. 健康度监测（src-tauri/src/core/health.rs）

- 对标 wireguide-plus **延迟探测（latency probe）**，但动作从"重连隧道"改为"回落 DHCP"（目标是保活上网）。
- 探测模式：`icmp`（ping IP）、`http`（GET 特殊 URL，推荐 `http://cp.cloudflare.com` 或自定义健康页）、`both`（默认）。`both` 下**仅当 ICMP 与 HTTP 同时失败**才判死，避免误杀（很多网络禁 ICMP 但放 HTTP；也能识别 captive portal）。
- 后台 `probe loop`（专用 `std::thread` + `Arc<AtomicBool>` 停止标志），**不阻塞 UI**（契合"托盘读缓存"范式）。
- 连续失败 ≥ `retries`：若 `health.fallback.enabled` → `set_dhcp()` + 跑该 Profile 的 `on_revert` + 通知；否则仅通知/日志。

---

## 8. 自动化任务（src-tauri/src/core/automation.rs）

- netsetman 式"附加"动作：`route`（加/删路由）、`launch`（开应用）、`run`（执行脚本）。
- 触发点：`on_apply`（Profile 生效后，含首次应用）、`on_revert`（离开该 SSID / 健康度回落时）—— 语义对齐 wireguide-plus 的 on-up/on-down 钩子。
- **安全边界**：`route` / `launch` 为白名单安全动作；`run` 受 **allow-list** 约束（仅执行 `scripts/` 受信目录或配置显式登记的绝对路径脚本，禁止任意命令注入）；需提权时复用特权守护进程通道。
- 确认弹窗逐条列出将执行动作（复用原 force-apply 确认框样式），用户可见才放行。

---

## 9. 权限模型

### 9.1 现状（P2 已落地）：sudoers NOPASSWD + 白名单包装脚本

目标：**改网络不再每次弹授权框**，同时不引入"任意 root 命令"的口子。

```
GUI 进程(非特权) ──sudo -n──▶ /usr/local/libexec/netsense-priv.sh (root 属主, 0755)
                                 └─ 白名单子命令 + 参数形状校验 ──▶ networksetup / route
```

- **结构化操作**：Rust 侧只构造 `PrivOp`（`SetManual/SetDhcp/SetDns/SetV6*/RouteAdd/RouteDelete`），
  经 `PrivOp::encode()` 编成 `op|arg1|arg2` 行，一次性以 stdin 喂给脚本的 `--batch` 模式。
  **任何情况下都不把用户输入拼进 shell 命令**，杜绝命令注入。
- **脚本侧二次校验**：`scripts/netsense-priv.sh` 对每个参数做形状校验（IPv4 逐段 0–255 且拒绝前导零、
  前缀 0–128、路由目标 `a.b.c.d[/len]`、DNS 为逗号分隔 IPv4 集合、服务名非空且不以 `-` 开头），
  未知子命令直接拒绝。
- **授权面**：`/etc/sudoers.d/netsense` 只授权**该脚本的绝对路径**（`NOPASSWD:`），
  不授予用户任意命令；脚本与所在目录均 root 属主、普通用户不可写。
- **回落**：包装脚本不存在、或 `sudo -n` 报"需要密码/无 tty/不允许执行"时，
  自动回落到 `osascript ... with administrator privileges`（即原来的每次授权框），功能不中断。
- **自检**：安装脚本最后会以当前用户身份跑一次 `sudo -n <script> --batch` 验证通道可用。

安装 / 卸载：

```bash
sh scripts/install-priv-helper.sh            # 安装（需要 sudo）
sh scripts/install-priv-helper.sh uninstall  # 卸载
```

### 9.2 为什么不直接上常驻特权守护进程

`launchd` 常驻 + Unix socket 的方案需要自己解决 socket 鉴权（macOS 无 `SO_PEERCRED`，
需 `getpeereid` FFI）与协议版本兼容，复杂度与攻击面都明显更大。
sudoers + 白名单脚本用系统自带的授权机制达成同一目标，且**提权范围更窄**（只放行网络配置操作）。
后续若要做免密之外的更强隔离（如 SMJobBless 签名校验），再按 §14 P5 评估。

> `run` 自动化动作若声明 `elevated: true`，**不走**白名单通道，仍走 `osascript` 授权框——
> 用户脚本与网络配置操作必须区分对待，保证每次执行提权脚本用户都知情。

---

## 9.5 状态栏弹窗面板（主交互入口）

菜单栏应用的标准交互：**左键点图标弹面板，失焦自动收起，右键出原生菜单**。

### 窗口布局（`tauri.conf.json`）

| label | 文件 | 关键配置 |
|-------|------|---------|
| `popup` | `popup.html` | `340×470`、`decorations: false`、`alwaysOnTop`、`skipTaskbar`、`visible: false` |
| `main` | `editor.html` | `760×580`、`center`、`visible: false`（按需弹出，不做启动即开窗） |

两个窗口都在启动时创建、只做显隐，**不每次点击重建 webview**（保证秒开）。

### 显隐与定位（`src-tauri/src/popup.rs`）

1. **触发**：`TrayIconBuilder::show_menu_on_left_click(false)` 让左键不再弹原生菜单，
   改为派发 `TrayIconEvent::Click{Left, Up}` → `popup::toggle()`；右键仍出菜单。
2. **定位**：取事件的 `rect`（图标物理矩形）→ 换算锚点 = 图标水平中心 + 垂直底边，
   `y` 再下移 6px；最后用 `current_monitor()` 的 position/size **做 clamp**，
   防止面板在屏幕边缘或外接显示器上跑到看不见的地方。
3. **失焦收起**：`Builder::on_window_event` 里监听 `WindowEvent::Focused(false)` 且 label 为 `popup` → `popup::hide()`。
4. **防抖（关键细节）**：点图标收起面板时，blur 会先触发一次隐藏，紧接着托盘点击又会把它打开，
   表现为"点一下反而闪一下"。`popup.rs` 用 `LAST_HIDE: Mutex<Option<Instant>>` 记录收起时刻，
   300ms 内的托盘点击直接忽略。
5. **macOS 菜单栏形态**：setup 中 `set_activation_policy(ActivationPolicy::Accessory)`，
   Dock 不显示图标；编辑器窗口「关闭」被 `CloseRequested` 拦截为 `hide()`（`api.prevent_close()`），
   即关窗=收回菜单栏常驻，而非退出。

### 面板内容（`frontend/popup.html`）

连接状态点 + SSID、当前 profile、IPv4 / 网关 / DNS / 信号、提权通道徽标（免密 / 每次授权）、
profile 快速切换列表（点击即 `force_apply`）、底部「打开编辑器 / 刷新 / 退出」。
每次获得焦点（被点开）自动 `get_status` 刷新，同时订阅 `netsense://status` 事件被动刷新。

### 需要实测确认的点（本机无法验证）

- `ActivationPolicy::Accessory` 生效后，`window.set_focus()` 是否能让面板取得键盘焦点，
  从而可靠触发 blur 收起。若发现在 macOS 上点外部不收起，需要在 show 后追加一次「激活应用」调用。
- `show_menu_on_left_click` 在部分 Tauri 2.x 版本名为 `menu_on_left_click`；编译报错时按提示改名即可。
- `TrayIconEvent::Click` 的 `rect` 字段是较新版本才有的；若所用版本没有该字段，
  改用同事件里的 `position`（点击坐标）作为锚点即可，`popup::toggle` 的入参不变。
- 托盘 `rect` 在 Retina 下为物理像素，锚点换算已按物理像素处理；若发现面板横向偏移半个图标宽度，
  说明该平台返回的是逻辑坐标，改按 scale factor 折算即可。

## 9.6 三平台实现对照（`platform/{macos,windows,linux}.rs`）

上层（`main.rs` / `ipc.rs` / `core/*`）**只依赖 `platform::Platform` 与 `NetworkPlatform` trait**，
具体平台由 `platform.rs` 末尾按 `target_os` 编译期选定并统一导出：

```rust
#[cfg(target_os = "macos")]   pub use macos::{priv_channel, MacPlatform as Platform};
#[cfg(target_os = "windows")] pub use windows::{priv_channel, WindowsPlatform as Platform};
#[cfg(target_os = "linux")]   pub use linux::{priv_channel, LinuxPlatform as Platform};
```

| 能力 | macOS | Windows | Linux |
|------|-------|---------|-------|
| 无线接口发现 | 固定 `en0` | `Get-NetAdapter` 按 `MediaType='Native 802.11'` | `nmcli -t -f DEVICE,TYPE dev status` 找 `wifi` |
| 当前 SSID | `networksetup -getairportnetwork en0` | `Get-NetConnectionProfile.Name` | 活动连接名（`nmcli`） |
| IP/掩码/网关/DNS | `ipconfig` + `networksetup -getinfo` | 一次 PowerShell 取 JSON（`Get-NetIPAddress` / `Get-NetRoute` / `Get-DnsClientServerAddress`） | `nmcli -g IP4.*` |
| BSSID / 信号 | `airport -I` | `netsh wlan show interfaces`（**只取 ASCII 字段**） | `nmcli -f IN-USE,BSSID,SIGNAL dev wifi list` |
| 网关 MAC | `arp -n <gw>` | `arp -a` 匹配网关行 | `ip neigh show <gw>` |
| 写 IP/DNS | `networksetup -setmanual/-setdnsservers` | `netsh interface ipv4 set address/dnsservers` | `nmcli con mod ipv4.*` + `con up` |
| IPv6 开关 | `networksetup -setv6off/-setv6automatic` | `Enable/Disable-NetAdapterBinding ms_tcpip6` | `nmcli con mod ipv6.method` |
| 路由 | `route -n add/delete` | `New-NetRoute` / `Remove-NetRoute` | `ip route add/del` |
| 探测 ICMP / HTTP | `ping -c1 -t` / `curl` | `ping -n1 -w` / `curl.exe` | `ping -c1 -W` / `curl` |
| 提权 = Direct | sudoers 白名单脚本（`sudo -n`，无弹窗） | 进程本身已是管理员 | `sudo -n` 可用 |
| 提权 = Prompt | `osascript ... with administrator privileges` | UAC（`Start-Process -Verb RunAs`） | `pkexec` |
| 已保存 SSID 列表 | `networksetup -listpreferredwirelessnetworks` | 读 WLAN 配置 XML（`[xml]` 解析） | `nmcli con show` 过滤 `802-11-wireless` |
| 打开日志目录 | `open` | `explorer` | `xdg-open` |

### Windows 后端的三个关键设计（踩坑点，改动前务必先读）

1. **读走 PowerShell CIM，不走 `netsh` 文本**。
   中文 Windows 上 `netsh` 的输出是**本地化 + OEM 代码页（CP936）**的：
   字段名变中文、"已连接"/"信号"之类的值也是中文，且按 UTF-8 解码会乱。
   CIM cmdlet 返回的是对象，语言无关；我们在 `ps()` 里统一注入
   `[Console]::OutputEncoding = UTF8`，保证含中文的返回值（SSID、适配器名）能正确解码。
2. **例外：BSSID / 信号百分比仍从 `netsh` 文本取**，但**只提取 ASCII 字段**
   （MAC 十六进制、`NN%` 数字）。ASCII 字节在 CP936 与 UTF-8 下一致，因此不受编码影响，
   而这两行又恰好没有 CIM 等价物。**不要**用同样的方式去取 SSID（SSID 可能是中文）。
3. **提权命令一律用 `@('a','b')` 数组传参**，不做字符串拼接：
   `& netsh.exe @('interface','ipv4','set','address','name=Wi-Fi','static',...)`。
   接口名含空格/特殊字符时不会被重新切分。整批命令渲染成 PowerShell 脚本后，
   经 `-EncodedCommand`（UTF-16LE + base64，自带实现、零依赖）交给
   `Start-Process -Verb RunAs` **一次 UAC 覆盖全部操作**。

> **状态缓存**：`resolve_current_name()` 一次要问 SSID + 网关 MAC + BSSID，
> 而 Windows 上每次读取都要拉起一个 PowerShell 进程（百毫秒级）。
> `platform/windows.rs` 里用 1.2s TTL 的快照缓存 `status_cached()` 收敛开销，
> 写操作后 `invalidate_cache()` 立即失效。

### 匹配层的跨平台陷阱（已修）

各平台返回的 MAC 格式并不一致：Windows `arp -a` 是 `aa-bb-cc-dd-ee-ff`，
macOS/Linux 是 `aa:bb:cc:dd:ee:ff`，大小写也不统一。
因此 `core/matcher.rs` 的 MAC 比较**双侧都过 `platform::normalize_mac()`**
（统一小写 + 冒号），配置里写哪种形态都能命中。
SSID 比较**保持大小写敏感**（802.11 的 SSID 本身大小写敏感）。

---

## 10. i18n / 日志 / 配置热重载

### 10.1 i18n（`src-tauri/src/i18n.rs` + `i18n/*.json`）

- 5 语（zh / en / zh-TW / ja / ko），**`en.json` 为基准**；字典在编译期用 `include_str!` 嵌入二进制，运行时不读文件、免打包遗漏。
- 查找顺序：当前语言 → `en` → key 本身（**绝不 panic**）。`tf(key, args)` 支持 `{name}` 占位符替换。
- **Key parity 校验**（`check_parity()` 返回 missing/extra/empty，要求三者全为 0），App 启动时跑一次，失败仅告警不阻断启动。
  `cargo test` 中含 `parity_ok_in_bundle` / `fallback_to_en_then_key` / `placeholder_replace` 三个用例守 parity。
- 语言切换：IPC `set_language` → 写回 `config.language`；前端 `get_strings` 一次性拉取全部文案，避免逐 key 往返 invoke。
- 新增文案 5 语同加，删除 5 语同删。
- **当前规模：79 个 key × 5 语**。前端 `popup.html` / `editor.html` 的文案**全部**取自后端字典
  （模板里不再内嵌 i18n 对象），因此 parity 校验覆盖全部 UI 文案。

### 10.2 日志（`src-tauri/src/log.rs`）

- 路径：`<config 同目录>/logs/netsense-YYYY-MM-DD.log`，**按天轮转、保留 7 天**，启动时与跨天时清理。
- 全局 `OnceLock<Mutex<Logger>>` 串行写，跨线程安全；`error/warn/info/debug` 四个等级，`set_level` 可过滤。
- 未 `init()` 前的日志降级打到 stderr，保证启动早期故障不丢。
- `init(dir, true)` 第二参为 `to_stderr`：开发期 true（终端可见），发布时可置 false。

### 10.3 配置热重载（`main.rs::reload_watch_loop`）

- 后台线程每 3 秒比对 `config.json` 的 mtime，变化则重新加载 + `validate()`。
- 校验失败 → 仅记 error 并**保留旧配置**，不让坏配置把网络搞挂。
- 重载后重新匹配当前身份并 `apply_named(..., run_on_apply=false)`，即**不重跑 `on_apply` 自动化**（避免重复开软件/跑脚本）。
- **幂等保护**：`AppState.applied_fp` 记录「profile 名 + 内容序列化」指纹；若指纹未变则直接跳过网络下发。
  这样切换 UI 语言等与网络无关的改动不会触发二次 `osascript` 提权弹窗。
  健康度回落 DHCP 后会把指纹置空（`None`），保证下次重载重新下发，不会卡在 DHCP。

---

## 11. 构建与运行

三种方式，按你的场景挑一个。**如果只是想尽快拿到能跑的包，直接用方式三（CI），本地零依赖。**

| 方式 | 命令 | 前置 | 适用 |
|------|------|------|------|
| 一、本机直出 | `scripts/build-windows.ps1`（Win）/ `cargo build --release`（mac/linux） | 见下表 | 日常开发 |
| 二、本机出安装包 | `cargo tauri build` | 额外需 tauri-cli | 要 msi/dmg/AppImage |
| 三、云端出包 | push `v*` tag 或手动触发 workflow | 只需一个 GitHub 仓库 | **只想要成品测试** |

### 各平台前置

| 平台 | 必需 | 说明 |
|------|------|------|
| Windows | Rust(msvc) + **MSVC C++ 生成工具** + WebView2 | MSVC 无替代方案，勾「使用 C++ 的桌面开发」，约 3–6 GB；首次构建再留 5–10 GB |
| macOS | Rust + Xcode CLT | WebView 是系统自带的 WKWebView |
| Linux | Rust + `libwebkit2gtk-4.1-dev` `libgtk-3-dev` `librsvg2-dev` `libayatana-appindicator3-dev` `patchelf` | |

### Windows（推荐用脚本，它会先体检再构建）

```powershell
# 体检 + 出裸 exe（最快，产物在 src-tauri\target\release\netsense.exe）
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1

# 出 msi / nsis 安装包
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer

# 带控制台窗口的 debug 版（方便看日志）
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Debug
```

脚本会依次检查 Rust / MSVC / Windows SDK / WebView2 / 磁盘余量，缺什么直接告诉你装哪个，
不会让你在链接阶段才吃到 `LNK1104` 这种不好读的错。

### macOS / Linux

```bash
cargo build --release                 # 裸可执行文件
cargo tauri build                     # 出安装包（需 tauri-cli）

# macOS 首次使用建议先装免密特权通道（消除每次改网络弹授权框），需要 sudo
sh scripts/install-priv-helper.sh
```

### 云端出包（CI）

`.github/workflows/build.yml` 是四目标矩阵：`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`。

```bash
git tag v0.3.0 && git push origin v0.3.0     # 触发构建，产出 draft Release + 各平台 artifact
# 或在 GitHub 网页端 Actions → build → Run workflow（勾 release 可顺带建 Release）
```

流程内先跑 `python scripts/validate.py`（JSON / i18n parity / PAL 边界 / trait 覆盖），
再跑 `cargo test --lib`（matcher 匹配语义 + i18n parity），都过了才开始编译打包。
即使不发布 Release，各平台的裸可执行文件也会作为 artifact 上传，可直接下载测试。

### 首次运行

启动后不弹窗口：应用以托盘形态常驻，**左键点图标**打开弹窗面板，
右键出菜单（显示编辑器 / 打开日志目录 / 权限通道 / 退出）。
配置文件位置：**exe 同目录的 `config.json`**（没有则回退到工作目录）；
可复制 `config.example.json` 改名使用。

> **Windows 首次改网络会弹一次 UAC**。想免掉：以管理员身份运行一次即可（此后该进程内
> 提权通道显示为"无需授权"）。若不想常驻管理员，就接受每次改网络弹一次 UAC —— 这与
> macOS 上的 `osascript` 回落通道是对等设计。

---

## 12. 测试

- **与平台无关、随时可跑**：`python scripts/validate.py`
  检查 JSON 合法性、5 语 i18n parity 与占位符一致性、代码引用的 i18n key 是否存在、
  上层是否越权绕过 PAL、三个平台是否都实现了 trait 的全部方法（按方法名核对）。
- 单元：`cargo test --lib`
  `matcher`（多条件 AND / 最具体者胜 / priority 平手 / 无 match 跳过 / 仅网关 MAC 识别 / MAC 归一化跨平台）、
  `i18n`（parity / 回退链 / 占位符替换）。
- 集成（待补）：mock PAL，验证「匹配 → 应用 → 健康度回落 → on_revert」编排，不依赖真实系统命令。

---

## 13. 发布

- `VERSION` 文件为唯一真值，同步各平台打包元数据（`Cargo.toml` / `tauri.conf.json`）。
- 5 语 `CHANGELOG`（zh/en/zh-TW/ja/ko）+ README 按需更新。
- **提交信息红线**：`release: vX.Y.Z <简述>`，**严禁**含任何 AI 工具名/署名（WorkBuddy / CodeBuddy / Cursor 等）。
- 推 `vX.Y.Z` tag 触发 CI 跨平台构建/签名/发布（见 §11）。

---

## 14. 路线图

| 阶段 | 内容 |
|------|------|
| P0 ✅ | Tauri 壳 + 托盘 + macOS `networksetup` 后端 + 占位前端 |
| P1 ✅ | Core：匹配引擎 + 应用 + 校验 + 日志 7 天轮转 + i18n 5 语 + 配置热重载 |
| P2 ✅ | 特权通道（sudoers + 白名单脚本）+ 状态栏弹窗面板 + editor/popup 前端移植 |
| P3 ✅ | **三平台通用**：PAL 拆为 `platform/{macos,windows,linux}.rs`，Windows(`netsh`+CIM+UAC) 与 Linux(`nmcli`+`pkexec`) 后端落地；CI 四目标矩阵；`validate.py` 静态校验 |
| P4 | 编辑器补齐健康度/自动化面板（目前这两块只能手写 `config.json`） |
| P5 | 打磨：自动更新、代码签名/公证、Linux 无 NetworkManager 的 systemd-networkd 后端 |

> 扩展 1（网关 MAC/BSSID 匹配）与扩展 2（健康度+回落）无新特权面、风险低，已随 P2/P3 落地；
> 扩展 3 的 `run` 脚本 allow-list 是安全边界，收口时需单独评审。

---

## 15. 从 hammerspoon_wifi_switcher 迁移

| 现有 | 迁移到 |
|------|--------|
| `init.lua` 编排 | `main.rs` + IPC 命令 |
| `core.lua` 策略逻辑 | Core Engine（Rust） |
| `core.lua` 系统命令 | 三个 PAL 实现 |
| `config.lua` | `config.rs`（schema 不变，新增 `match` / `health` / `automation`） |
| `network_apply.lua` | `core/*`（apply + health + automation） |
| `ui/web_view.lua` 编辑器 | `frontend/editor.html`（原样复用） |
| `menu_builder.lua` | 系统托盘菜单渲染 |
| `hs.urlevent` handler | Tauri `invoke` 命令 |
| `i18n.lua` | 5 语 JSON（沿用字典内容） |

---

## 16. 设计决策记录（确认基线）

- 技术栈：**Tauri v2 (Rust)**，非 Wails。
- 匹配条件：**SSID / 网关 MAC / BSSID 三者独立、AND、可单独使用**。
- 开关：**健康度监测 / 是否回落 / 自动化任务 三者独立正交**。
- 健康度：**icmp/http/both**，both 下双失败才判死；回落 = DHCP 保底。
- 自动化：**netsetman 式 route/launch/run**，on_apply/on_revert 触发，`run` 受 allow-list 约束。

---

## 17. 实现进度（截至 2026-09-19）

> 本机为 Windows 沙箱，无 Rust/macOS 环境，**Rust 代码未经编译验证**。
> 已在本机实际验证的部分：i18n 5 语 × 79 key 的 parity 与占位符一致性（脚本校验）、
> 全部 JSON 文件合法性、`netsense-priv.sh` 的 `sh -n` 语法与参数校验函数行为（含 IFS 不泄漏）。

### 已完成 P0（平台层 / 应用外壳，可直接编译）
- `src-tauri/src/platform.rs` — **MacPlatform 全量实现**（真实系统命令）：
  - `watch_ssid`：自适应轮询（未接入 2s / 已接入 5s）+ `WatcherHandle::stop()` 优雅停止
  - `get_current_ssid` / `get_status`（ipv4/netmask/dns/网关/网关MAC/BSSID/RSSI/IPv6 模式）
  - `apply_profile` / `set_dhcp`：结构化 `PrivOp` → `exec_ops`（免密通道优先，回落 osascript）
  - `resolve_gateway_mac`（arp）/ `resolve_bssid`（airport 全路径）/ `probe`（icmp=ping / http=curl / both）
  - `add_route`/`delete_route`/`launch_app`/`run_script`（受 allow-list 约束）
  - 固有方法 `list_known_ssids`（networksetup -listpreferredwirelessnetworks）
- `src-tauri/src/config.rs` — `Profile` 加 `Default`；`AutomationConfig` 加 `allowed_scripts`（allow-list）；`Config::validate()`（非 DEFAULT 须声明 match；manual 须 ip/netmask/gateway；DNS 须合法 IPv4）；`Config::save()`
- `src-tauri/src/core/health.rs` — `HealthMonitor::start`：后台线程按 `retries` 连续失败触发 `on_fallback`（仅当 `fallback.enabled`）
- `src-tauri/src/core/automation.rs` — `run_actions`（route/launch/run）+ `is_script_allowed`（scripts/ 目录 + 显式登记路径）
- `src-tauri/src/main.rs` — Tauri v2 Builder：管理 `Arc<AppState>`、建托盘菜单、启动即按当前身份匹配一次、起 SSID watcher → `apply_named`（停旧健康度 → 起新健康度 → 应用 → on_apply）
- `src-tauri/src/ipc.rs` — `get_status` / `save_scene` / `force_apply` / `get_networks` 真实实现
- 图标（`scripts/gen_icons.py` 生成 png/ico/icns）、`frontend/index.html` 占位 + `serve.sh` 静态服务器、`tauri.conf.json` window label + devUrl、`Cargo.toml` 加 `tray-icon` 特性

### 已完成 P1（运行时组件）
- `src-tauri/src/log.rs` — 按天轮转日志，保留 7 天，线程安全，未初始化前降级 stderr
- `src-tauri/src/i18n.rs` + `i18n/{en,zh,zh-TW,ja,ko}.json` — 5 语（启动时各 16 key，现已扩到 79 key）；
  `t` / `tf` 翻译与缺失三级回退、`all_strings()`、`check_parity()`，含 3 个单测
- `src-tauri/src/config.rs` — 新增 `language` 顶层字段（可选，缺省回退 en）
- `src-tauri/src/main.rs` —
  - 启动顺序：日志 → i18n init → 加载配置 → 应用语言 → **跑 i18n parity 校验（仅告警）** → 托盘 → 首次匹配应用 → SSID watcher → 配置热重载线程
  - `apply_named(state, name, run_on_apply)`：新增「是否跑 `on_apply`」参数与**内容指纹幂等保护**
  - 托盘默认缺图标时降级为无图标托盘并告警，不再 `expect()` 崩溃
- `src-tauri/src/ipc.rs` — 新增 `set_language`（写回 `config.language`）/ `get_strings`（前端一次性拉取文案）；`get_status` 返回 `language`
- `src-tauri/tauri.conf.json` — `withGlobalTauri: true`（否则前端拿不到 `window.__TAURI__`）；
  CSP 放开 `script-src 'unsafe-inline'`；**移除配置项里的 `trayIcon`**（与 Rust 中 `TrayIconBuilder::with_id("main")` id 冲突）

### 已完成 P2（特权通道 + 弹窗面板 + 前端移植）
- `scripts/netsense-priv.sh`（新增）— root 属主白名单包装脚本：仅 8 类子命令，逐参数形状校验，
  拒绝任意 shell 透传；支持 `--batch` 从 stdin 批量执行。**已在本机用 `sh -n` 与函数级用例验证**
  （非法 IP/前导零/越界前缀/坏 DNS 均被拒，IFS 无泄漏）。
- `scripts/install-priv-helper.sh`（新增）— 安装/卸载：拷贝脚本到 `/usr/local/libexec`、
  写 `/etc/sudoers.d/netsense`（只授权该脚本路径）、`visudo -cf` 校验、`sudo -n` 自检。
- `src-tauri/src/platform.rs` — 提权重构为 `PrivOp` 结构化操作 + `exec_ops`（免密优先、osascript 回落）；
  `watch_ssid` 返回 `WatcherHandle` 并改自适应轮询（首轮只建基线、不回调，避免启动时重复触发 `on_apply`）；
  `get_status` 补 `netmask` / `bssid` / `rssi`。
- `src-tauri/src/popup.rs`（新增）— 状态栏弹窗面板：几何定位（含显示器工作区 clamp）、
  失焦收起、**300ms 防抖**（见 §9.5）。
- `src-tauri/src/main.rs` — 托盘左键弹面板 / 右键菜单（当前 config、权限通道、显示编辑器、打开日志、退出）；
  `publish_status()` 广播 `netsense://status` 并刷新菜单文案；`WindowEvent::CloseRequested` 把编辑器关窗改为隐藏；
  macOS `ActivationPolicy::Accessory`。
- `src-tauri/src/ipc.rs` — 新增 `get_config` / `delete_profile` / `open_editor` / `close_editor` / `quit_app`；
  `save_scene` / `delete_profile` / `set_language` 改为**副本校验落盘再替换内存**的事务式更新。
- `frontend/editor.html`（由 hammerspoon 版移植）— `hammerspoon://` 全改 `invoke`、占位符改异步拉取、
  推送改事件订阅，并新增原版没有的**匹配条件编辑区**（SSID / 网关 MAC / BSSID / 优先级）。
- `frontend/popup.html`（新增）— 弹窗面板：状态、提权徽标、profile 快速切换、语言、打开编辑器/退出。
- `src-tauri/tauri.conf.json` — 新增 `popup` 窗口（无边框/置顶/不进任务栏/默认隐藏）；
  `main` 窗口指向 `editor.html` 且默认隐藏；版本号 0.2.0。

### 已完成 P3（三平台通用）

> 目标从「macOS 先跑通」改为「三平台同一套代码」。

- **PAL 重构为模块化**（`platform.rs` → `platform/{macos,windows,linux}.rs`）：
  - `platform.rs` 只保留 trait 契约 + 共享类型 + 共享工具（`run` / `poll_ssid_watch` /
    `parse_kv` / `extract_mac` / `normalize_mac` / `open_path`）+ **编译期平台选择**
    （`#[cfg(target_os)] pub use ... as Platform`）。
  - `PrivChannel` 语义跨平台统一为 `Direct`（免密/已提权，无弹窗）与 `Prompt`（每次授权），
    各平台自行映射到本地机制；`InterfaceStatus` 新增 `iface`（当前无线接口名）。
  - **上层不再出现任何具体平台类型名**：`main.rs` / `ipc.rs` / `core/*` 只用 `Platform` 与 trait，
    这条边界由 `scripts/validate.py` 第 5 项检查强制守住。
- `platform/windows.rs`（新增）— 完整 Windows 后端，详见 §9.6：
  - 读：一次 PowerShell 取回 JSON 快照（接口名/SSID/IP/前缀/网关/DNS/IPv6）；
    BSSID 与信号百分比走 `netsh` 但**只取 ASCII 字段**。
  - 写：`netsh interface ipv4 set address/dnsservers` + `New-NetRoute`，全部渲染成
    `& netsh.exe @('a','b')` 数组调用；整批经 `-EncodedCommand` + `Start-Process -Verb RunAs`
    **一次 UAC 覆盖**。
  - 提权通道：进程已是管理员 → `Direct`，否则 → `Prompt`。
  - `list_known_ssids`：读 WLAN 配置 XML（`[xml]`），语言无关且编码正确。
  - 自带 base64 实现（`-EncodedCommand` 需要 UTF-16LE base64），**零新增依赖**。
  - 1.2s TTL 状态缓存，避免一次身份解析拉起多次 PowerShell。
- `platform/linux.rs`（新增）— `nmcli` 读写 + `ip route` + `ip neigh`；
  提权优先 `sudo -n`，回落 `pkexec`；正确处理 `nmcli -t` 的反斜杠转义。
- `platform/macos.rs` — 原 `platform.rs` 的 macOS 部分原样迁入，逻辑未改，
  改为复用共享的 `poll_ssid_watch` 与 `run`。
- `core/matcher.rs` — **修跨平台 MAC 匹配缺陷**：两侧都过 `normalize_mac()`
  （Windows `arp` 用连字符、大小写也不同），并补 **7 个单测**覆盖
  多条件 AND / 最具体者胜 / priority 平手 / 无 match 跳过 / 仅网关 MAC 识别 / 归一化。
- `popup.rs` — 改用 `get_webview_window()`（Tauri v2 正确 API），
  失焦收起逻辑拆出 `mark_hidden()` 以便 `on_window_event` 复用；三平台行为一致。
- `main.rs` — `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`
  （Windows 发布版不弹控制台）；启动日志打印平台后端与提权通道。
- **构建通道**：`.github/workflows/build.yml`（四目标矩阵 + validate + cargo test）、
  `scripts/build-windows.ps1`（含 MSVC/WebView2/磁盘体检）、`scripts/validate.py`（静态校验）。
- **i18n**：`tray.priv_sudoers` / `tray.priv_osascript` 改名为 `tray.priv_direct` / `tray.priv_prompt`
  （语义跨平台化），5 语同步；`popup.html` 徽标样式随之改为 `.badge.direct` / `.badge.prompt`。

### 待办 / 下一阶段
- **编译验证**：本沙箱是 Windows 且 C: 仅剩 164MB、无 MSVC，**Rust 全部代码尚未过编译器**。
  可行路径见 §18「构建环境说明」。
- **三平台实测**：Windows 上跑 `scripts\build-windows.ps1`；macOS/Linux 上 `cargo build --release`。
- **弹窗面板实测项**：见 §9.5「需要实测确认的点」（键盘焦点/blur 收起、`show_menu_on_left_click` 命名、Retina 坐标折算）。
- **编辑器补齐**：健康度（探测参数）与自动化任务（route/launch/run + 三开关）尚未进 UI，目前只能手写 `config.json`。
- **`popups.html` 移植**：日志查看弹窗（当前靠托盘菜单「打开日志目录」代替）。
- **集成测试**：mock PAL 验证「匹配 → 应用 → 健康度回落 → on_revert」编排。
- **Linux 覆盖面**：目前假定 NetworkManager；systemd-networkd / 纯 `ip` 场景未覆盖。

### 偏离基线的实现说明
- `probe.http` 用 `curl`（Windows 用系统自带 `curl.exe`）而非 reqwest（避免新增依赖）；
  ICMP 用系统 `ping`（macOS BSD `-t` 秒 / Windows `-w` 毫秒 / Linux `-W` 秒）。
- **模块名 `core` 与标准库 crate `core` 同名**：Rust 2018 uniform paths 下 `use core::x` 会报 E0659 歧义，
  因此所有导入必须显式写 `use crate::core::{matcher, health, automation}`（表达式路径 `log::info(...)` 不受影响）。
  后续新增 Core Engine 模块请沿用此写法。
- `InterfaceStatus` 增加 `Serialize`（IPC 返回需 JSON 化）与 `gateway_mac` / `netmask` / `bssid` / `iface` 字段。
- 健康度回落后会清空 `applied_fp` 指纹：否则后续热重载会因「内容未变」跳过下发，导致网络卡在 DHCP 状态。
- **权限模型用系统自带授权机制**（macOS sudoers 白名单 / Windows UAC / Linux sudo+pkexec），
  不是原计划的常驻特权守护进程——理由见 §9.2（授权范围更窄、无 socket 鉴权面）。
- `watch_ssid` 三平台均为自适应轮询（2s/5s），**未做事件驱动**：
  macOS 的 CoreWLAN 通知需 objc2 FFI 且要处理 `CWInterface` 通知的对象生命周期，
  Windows 的 `WlanRegisterNotification` 同样要 FFI；收益（省掉每 2–5s 一次子进程）不抵复杂度，暂缓。
  轮询逻辑已抽成共享的 `poll_ssid_watch`，将来换事件驱动只需改各平台 `watch_ssid` 一处。
- 前端未引入构建工具（无 Vite/Svelte），仍是纯静态 HTML；`invoke` 依赖 `withGlobalTauri`。
  **副作用（好的）**：CI 完全不需要 Node，构建链路更短。

---

## 18. 构建环境说明（为什么某些环境下无法就地出包）

Tauri 在 Windows 上**官方要求 MSVC 工具链 + Windows SDK**（官方文档措辞是"没有替代方案"），
macOS 包必须在 macOS 上编译，Linux 需要 webkit2gtk 开发库。因此"出一份三平台安装包"
这件事天然需要三套环境。

在一台 Windows 机器上实测的体检结果（供你判断该走哪条路）：

| 检查项 | 实测结果 | 影响 |
|------|---------|------|
| Rust 工具链 | 未安装（无 `~/.cargo` / `~/.rustup`） | 需先装，约 0.5–1 GB |
| MSVC 链接器 | **缺失**（`VS2019` 目录是空壳，`vswhere` 找不到 VC 工具） | Tauri Windows 构建的硬前置，需装 Visual Studio Build Tools（约 3–6 GB） |
| 替代方案 | 系统有 TDM-GCC（`x86_64-w64-mingw32`），可走 `x86_64-pc-windows-gnu` | 非官方支持路径，`webview2-com-sys` 等依赖在 gnu 下风险高 |
| 磁盘 | C: 剩 **164 MB**、D: 剩 2.8 GB、E: 剩 4.0 GB | **决定性约束**：装完 MSVC + Rust 再编译（首次 target 约 2–5 GB）必然爆盘 |
| 网络 | crates.io / rust-lang.org / rsproxy 均可达 | 下载不是瓶颈，磁盘才是 |

**结论**：磁盘余量装不下 MSVC 工具链，无法就地出 Windows 安装包。可行路径按推荐度排序：

1. **CI 出包（首选）**：把仓库推上 GitHub，`git tag v0.3.0 && git push origin v0.3.0`，
   `.github/workflows/build.yml` 会在云端矩阵上同时产出 windows-x64 / macos-arm64 /
   macos-x64 / linux-x64 的可执行文件与安装包。**本地零依赖，不占磁盘，一次拿全三平台。**
2. **本机出包**：先腾出约 10 GB，装 Visual Studio Build Tools（勾「使用 C++ 的桌面开发」）
   与 Rust，然后跑 `scripts\build-windows.ps1`（脚本会替你把体检做在前面）。
3. **仅验证代码可编译**：装 Rust 的 `x86_64-pc-windows-gnu` 目标 —— `cargo check`
   **不需要链接器**，只需 `windres`（TDM-GCC 自带），
   跑 `cargo check --target x86_64-pc-windows-gnu` 就能验证类型与依赖解析，
   但不产出可执行文件。参考 `scripts/sandbox-bootstrap.sh`。


