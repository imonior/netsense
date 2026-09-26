# NetSense Development Guide (DEVELOPMENT.md)

> A cross-platform desktop app that matches the current network identity (SSID / BSSID / gateway MAC /
> interface) against **Profiles** and applies exactly one of them: 3A network configuration (static IP /
> DHCP / DNS / IPv6 / routes, Apply→Verify) and then 3B actions. Two or more Profiles matching at once is
> a **Conflict** — nothing is applied and the user is told. Includes health monitoring with a DHCP safety
> net, a per-Profile detection cadence, and a script allow-list.

---

## 1. Tech Stack Decisions

| Item | Choice | Rationale |
|------|--------|-----------|
| Shell | **Tauri v2 (Rust)** + system WebView | Smallest binary (5–15 MB), Rust memory safety, few dependencies; pure-technical first choice |
| Backend | Rust (Core Engine + PAL) | Type safety, cross-platform compilation |
| Frontend | Plain static HTML/CSS, no framework and no bundler | Talks to the backend via Tauri `invoke`; the build chain needs no Node at all |
| i18n | 5 languages (zh / en / zh-TW / ja / ko) | One embedded dictionary per language, key-parity checked in CI |

**Why Tauri and not Wails+Go**: both are valid; the deciding factors here are the binary size and Rust's memory safety in the layer that writes network configuration. The cost is that no Go-side tooling can be shared, so the probe, i18n and release flows are all this repo's own.

---

## 2. Architecture (four layers + platform branch)

```
┌─────────────────────────────────────────────┐
│ Frontend  popup.html（状态面板）· settings.html（软件设置）· editor.html（自动化配置编辑器）· logs.html（日志窗口）
├─────────────────────────────────────────────┤
│ App Shell  Tauri: window + system tray + event loop + IPC (ipc.rs / state.rs / tray.rs)
├─────────────────────────────────────────────┤
│ Engine  engine.rs —— 唯一的状态机
│   Network Event / Polling / Manual Apply
│        ↓
│   conditions: 对每个 enabled Profile 求值（Rules OR · Rule 内 enabled Conditions AND）
│        ↓
│   decide():  0 命中 → No Active Profile（可走顶层 fallback）
│              1 命中 → Active，执行该 Profile 的 THEN / ELSE
│              2+ 命中 → Conflict，**不自动选边**，广播 netsense://conflict
│        ↓
│   3A network: Apply → Verify(readback [+ health]) —— 硬屏障，失败即 3B 一条都不跑
│        ↓
│   3B1 automation/one_shot: 过滤 disabled → 按 priority 分批（同批并发，批完才下一批）
│   3B2 automation/persistent: 期望状态 worker，随 Active 的 THEN 起停（automation/provider 出语义）
├─────────────────────────────────────────────┤
│ PAL  platform.rs: trait NetworkPlatform（18 方法）+ 编译期选平台
├───────────────────┬────────────────┬────────────┤
│ macOS             │ Windows        │ Linux      │
│ networksetup      │ PowerShell CIM │ nmcli      │
│ + ipconfig        │ + netsh        │ + ip route │
│ + system_profiler │ + New-NetRoute │ + ip neigh │
│ + route           │                │            │
└───────────────────┴────────────────┴────────────┘
```

Runtime flow：`detection` 决定「什么时候重新采样并求值」（每个 Profile 自带 mode / change_delay / poll_interval），
`engine::pass` 做一次全量求值，`reconcile` 负责 0/1/2+ 三种结果的落地，`manual_apply` 是「立即应用」的唯一入口
（它**同样**先评估该 Profile 自己的条件，冲突时直接拒绝）。所有平台 I/O 都在引擎线程或
`#[tauri::command(async)]` 的线程上跑，主线程只负责建窗口与托盘图标。

---

## 3. Repository Layout

```
netsense/
├── src-tauri/
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── Info.plist             # macOS bundle plist merged at packaging: NSLocationWhenInUseUsageDescription
│   ├── build.rs
│   ├── icons/                 # tray/window icons (derived from app-icon.png by gen_icons.py)
│   └── src/
│       ├── main.rs            # entry: build window/tray/popup/register commands, start the engine thread
│       ├── popup.rs           # status-bar popup panel: position / show-hide / blur-collapse + debounce
│       ├── tray.rs            # tray icon only: any button click toggles the popup panel (no native menu)
│       ├── state.rs           # AppState（config/engine 锁、config_path、广播）+ status_payload
│       ├── paths.rs           # 用户级数据目录（配置/日志）解析；exe 同级只作兜底来源
│       ├── appconfig.rs       # 软件配置 settings.json（语言 / 日志保留）+ 三平台开机启动（问系统，不猜文件）
│       ├── ipc.rs             # 32 条 Tauri 命令（前端契约见 frontend/README.md）
│       ├── engine.rs          # ★ 状态机：decide() · reconcile() · execute_branch() · manual_apply()
│       ├── config/
│       │   ├── model.rs       # schema 1 数据结构（Profile / Rule / Condition / Branch / 3A / 3B）
│       │   └── mod.rs         # 读写 + validate() + warnings()；schema≠1 直接拒绝（不做自动迁移）
│       ├── conditions/
│       │   ├── mod.rs         # 层导出：只读快照、只算匹配，不碰平台（副作用都在 engine）
│       │   ├── identity.rs    # NetworkSnapshot：一次采样内的网卡/SSID/BSSID/网关 MAC 快照
│       │   └── evaluator.rs   # Condition→Rule→Profile 三态求值（禁用条件永不算命中）
│       ├── detection/
│       │   └── mod.rs         # 每个 Profile 的 mode / change_delay / poll_interval 节律
│       ├── network/
│       │   ├── mod.rs         # 3A：Apply → Verify，失败即阻断 3B
│       │   ├── readback.rs    # 下发后回读比对（"字段缺失=别碰"，"空串=明确清空"）
│       │   └── health.rs      # Active 期间的探测循环 + 连续失败回落 DHCP
│       ├── automation/
│       │   ├── mod.rs         # 3B 层的脚本 allow-list（安全边界：run_script 能提权）
│       │   ├── one_shot.rs    # 3B1：enabled + priority 分批执行 + 脚本 allow-list
│       │   ├── persistent.rs  # 3B2：每条常驻动作一个 worker 线程（起停 / 错峰 / 自重叠门 / 状态上报）
│       │   └── provider.rs    # 3B2 的动作语义层：把 PersistentActionType 翻成一次 `Tick`
│       ├── platform.rs        # ★ PAL trait contract + shared types + shared utils + compile-time platform selection
│       ├── platform/
│       │   ├── macos.rs       # macOS: networksetup / arp / airport / route / osascript
│       │   ├── windows.rs     # Windows: PowerShell(CIM) read + netsh write + UAC elevation
│       │   ├── win_helper.rs  # Windows 常驻提权 helper：同一 exe 的 `--netsense-helper` + 命名管道 RPC（一次授权/会话）
│       │   └── linux.rs       # Linux: nmcli read/write + ip route + sudo/pkexec
│       ├── update.rs          # 在线升级：check_update / run_update（下载 + 校验 + 安装）
│       ├── win_dialog.rs      # Windows 原生对话框桥
│       ├── log.rs             # daily-rotating log (keep 7 days)
│       ├── i18n.rs            # 5-language dictionary + translate + key-parity check
│       └── i18n/              # en/zh/zh-TW/ja/ko.json (embedded at compile time via include_str!)
├── frontend/
│   ├── popup.html             # status-bar popup panel (window label = popup)
│   ├── editor.html            # config editor (window label = main)
│   ├── settings.html          # 软件设置窗口（窗口 label = settings）：语言 / 开机启动 / 配置与日志位置
│   ├── logs.html              # 日志窗口（窗口 label = logs）：按天文件下拉 / 尾部行 / 级别上色 / 过滤 / 跟随
│   ├── README.md              # frontend notes + IPC command table
│   └── serve.sh               # dev static server on :1420
├── scripts/
│   ├── netsense-priv.sh       # macOS privilege allow-list wrapper script (root-owned, authorized by sudoers)
│   ├── install-priv-helper.sh # macOS install/uninstall privilege channel (writes /etc/sudoers.d/netsense)
│   ├── validate.py            # ★ platform-independent static checks (JSON / i18n parity / PAL boundary / docs alignment), also run in CI
│   ├── editor-smoke.mjs       # ★ headless editor harness (stubbed DOM + fake IPC); asserts the binding rules and can dump the payloads the editor really sends
│   ├── build-windows.ps1      # ★ one-click Windows build (with MSVC/WebView2/disk preflight)
│   ├── gen_icons.py           # resample + round + pack the icon set from app-icon.png (pure stdlib)
│   ├── bump_version.sh        # VERSION → tauri.conf.json + Cargo.toml (VERSION is the single source)
│   ├── check-no-ai.sh         # ★ publish-hygiene guard: no assistant-tool names in commit messages / shipped files
│   ├── setup-hooks.sh         # point core.hooksPath at scripts/git-hooks (the guard above needs it)
│   ├── git-hooks/             # commit-msg + pre-commit, both calling the same guard
│   └── sandbox-bootstrap.sh   # gnu-target cross-compile verification in MSVC-less env (dev-only)
├── .github/workflows/build.yml# ★ three-platform matrix build: push a tag → win/mac/linux installers
├── app-icon.png               # ★ icon artwork master (1024², square, opaque) — edit this + rerun gen_icons.py
├── config.example.json        # config template (schema 1)
├── DEVELOPMENT.md             # this file
├── README.md
├── LICENSE
└── .gitignore
```

---

## 4. PAL Contract (src-tauri/src/platform.rs)

```rust
/// PAL 契约：Core Engine 只依赖此 trait，不直接碰系统命令。
pub trait NetworkPlatform: Send + Sync {
    fn watch_ssid(&self, cb: Box<dyn Fn(Option<String>) + Send + Sync>) -> WatcherHandle;
    fn get_current_ssid(&self) -> Option<String>;
    fn get_status(&self) -> InterfaceStatus;

    /// 3A 读回校验用的快照：默认转 `get_status`，做了进程级缓存的实现**必须**覆盖它 ——
    /// 校验每 800ms 采一次样，比快照的 TTL 短，不覆盖就等于四次采样只有两次真的去问系统。
    fn fresh_status(&self) -> InterfaceStatus { self.get_status() }

    /// 下发 3A 网络配置（IP/掩码/网关/DNS/IPv6）。参数是 `NetworkConfig` 而**不是**整个
    /// Profile —— 否则「下发」这层就看得见匹配条件与动作，跨层依赖又长回来了。
    /// `dns` 是三态的：缺失 = 不产生任何 DNS 操作，空串 = 清空。三份实现都得守住。
    /// 静态路由由 `network::apply_3a` 在其后单独下发，好让两种失败分别归因。
    fn apply_network(&self, p: &NetworkConfig) -> Result<(), String>;
    /// 回落保底：切回 DHCP + 清空自定义 DNS —— 这里的清空是故意的，不受上面那条三态约定约束
    fn set_dhcp(&self) -> Result<(), String>;

    /// 枚举当前在用的全部网卡（有线 / 无线 / VPN）。实现必须经 `cached_nics` 包一层
    /// TTL 缓存：面板刷新调它很频繁，而 macOS 15.6+ 枚举一次要 1–4 秒。
    fn list_interfaces(&self) -> Vec<NicInfo>;
    /// 按**设备名**切回 DHCP；默认回落到 `set_dhcp()`（主无线网卡语义），
    /// 能定位适配器/连接的平台应覆盖它，否则改动会落到另一张网卡上。
    fn set_dhcp_for(&self, dev: &str) -> Result<(), String> { self.set_dhcp() }

    /// ICMP / HTTP 探测；`both` 模式由调用方判定结论
    fn probe(&self, target: &ProbeTarget, timeout_ms: u64) -> Health;

    /// 3B2 的「检查」半边：这条隧道现在是否已连上。`false` 同时涵盖「没连上」与
    /// 「本机没有这条隧道」—— 区别只在报错文本里有意义，而报错由 `tunnel_connect` 给。
    fn tunnel_is_up(&self, target: &TunnelTarget) -> bool;
    /// 3B2 的「恢复」半边。⚠️ **只能走无需交互授权的通道**：worker 每 `interval_secs`
    /// 调一次，提权（授权框 / UAC / pkexec）等于每 N 秒打断用户一次。权限不够就照实
    /// 返回 `Err`，让错误停在界面上，不要为了「让它成功」而偷偷提权。
    fn tunnel_connect(&self, target: &TunnelTarget) -> Result<(), String>;

    fn add_route(&self, dest: &str, gw: &str, metric: u32) -> Result<(), String>;
    fn delete_route(&self, dest: &str) -> Result<(), String>;
    fn launch_app(&self, app: &str, args: &[String]) -> Result<(), String>;
    /// 受 allow-list 约束（automation 层先校验再调用）；elevated 走提权通道
    fn run_script(&self, path: &str, args: &[String], elevated: bool) -> Result<(), String>;

    /// 本机打印机清单（编辑器「设为默认打印机」的候选，含当前默认的那台）。
    /// 枚举不到就返回空表 —— 没装打印系统的机器是正常状态，不是错误。
    fn list_printers(&self) -> Vec<PrinterInfo>;
    /// ⚠️ 三个平台都只改**当前用户**的默认打印机（macOS/Linux 写 `~/.cups/lpoptions`，
    /// Windows 走 `Win32_Printer` 的 `SetDefaultPrinter` 方法），不提权：切网络是高频事件，
    /// 每切一次弹一次授权框不可接受。理由同 `tunnel_connect`。
    fn set_default_printer(&self, printer: &str) -> Result<(), String>;

    /// 系统已保存的无线网络列表（编辑器「尚未配置的网络」用）。不支持则 `None`。
    fn list_known_ssids(&self) -> Option<Vec<String>>;
}
```

`scripts/validate.py` 第 [6] 项会解析这个 trait 的方法清单，并逐个检查
`platform/{macos,windows,linux}.rs` 是否全都实现了 —— 加一个方法而不实现它，
会在 CI 里以「哪个平台缺哪个方法」的形式报出来，而不是等构建到那一台机器才炸。
第 [5] 项反向检查：平台层**之外**的任何模块都不许出现 `MacPlatform` / `WindowsPlatform`
/ `LinuxPlatform` 这类具体类型名。

Cross-platform implementation notes:

| Capability | macOS | Windows | Linux |
|------------|-------|---------|-------|
| Current SSID | `CoreWLAN` (`CWWiFiClient`, needs Location permission) → `networksetup -getairportnetwork` → `ipconfig getsummary` → `system_profiler SPAirPortDataType` (a graded fallback, §9.6) | `Get-NetConnectionProfile` (CIM) | `nmcli -g GENERAL.CONNECTION device show <dev>`, falling back to `con show --active` |
| Gateway MAC | `route -n get default` → `arp -n <gw>` | `arp -a` (whole table, MAC-shaped match) | `ip neigh show <gw>` |
| BSSID | `airport -I <dev>` (≤ 14.3) → `system_profiler` | `netsh wlan show interfaces` (ASCII fields only) | `nmcli -f IN-USE,BSSID,SIGNAL dev wifi list`, the `*` row |
| Apply static | `networksetup -setmanual` | `netsh interface ipv4 set address` | `nmcli connection modify` + `con up` |
| ICMP probe | system `ping -c 1 -t <s>` | system `ping -n 1 -w <ms>` | system `ping -c 1 -W <s>` |
| Add route | `route -n add -net` | `New-NetRoute` / `Remove-NetRoute` (CIM) | `ip route add` / `ip route del` |
| Launch app | `open -a` (`--args` after the app) | `Start-Process -FilePath` | direct exec, else `xdg-open` |
| List printers (3B1) | `lpstat -e` + `lpstat -d` (CUPS client) | `Get-CimInstance Win32_Printer` (`Name`, `Default`) | `lpstat -e` + `lpstat -d` (same parser as macOS) |
| Set default printer (3B1) | `lpoptions -d <name>` → `~/.cups/lpoptions` | `Invoke-CimMethod -MethodName SetDefaultPrinter` | `lpoptions -d <name>` → `~/.cups/lpoptions` |
| Tunnel state (3B2) | `scutil --nc list` (WireGuard for macOS registers each tunnel as a NEVPN config, so it shows up there too) | `Get-NetAdapter` (CIM; the adapter **description** carries the provider hint) | `nmcli -t connection show` → fall back to `ip link show <dev>` |
| Connect tunnel (3B2) | `scutil --nc start <label>` | `wireguard.exe /installtunnelservice <conf>` for WireGuard, `rasdial <name>` for system VPN | `nmcli connection up <name>` → `ip link set <name> up` |

⚠️ The two tunnel rows never go through the elevation channel. A worker calls them every
`interval_secs`; an authorization prompt on that path means the user is interrupted every N seconds,
which is exactly what §8 rule "persistent workers never elevate" exists to prevent. When the system
refuses for permission reasons the platform layer returns the error verbatim and the editor surfaces it —
the user decides whether to grant a passwordless channel or connect manually.

`set_default_printer` holds the same line for the same reason, and CUPS makes it easy: `lpoptions -d`
writes the **per-user** default into `~/.cups/lpoptions`, and Windows' `SetDefaultPrinter` CIM method is
per-user too, so no leg needs admin. The "which one is default" marker in the editor's candidate list
comes from `lpstat -d`, i.e. the destination CUPS resolves for this user — it is display only, the
action itself always targets the printer **by name**, so a disagreement there can never send the write
to the wrong machine.

Two parser details carry weight. The default's name is taken from the text **after the last colon** in
`lpstat -d`, never by matching its English label: CUPS translates that sentence, and a locale-dependent
parse would quietly report "no default" on a localized system. On Windows the name is compared with
`Where-Object { $_.Name -eq $want }` rather than a WQL `-Filter`, which keeps the user-supplied name
inside a single PowerShell string literal — one less layer of quoting for a value that comes from a
config file.

One Linux trap worth keeping in the docs: `ip link show` prints `state UNKNOWN` for a **working**
WireGuard interface (the kernel classifies it as non-broadcast / no-carrier). Readiness is decided from
the flag list (`UP,LOWER_UP`), not from `state` — otherwise every wg tunnel reads as disconnected and the
worker reconnects forever. `platform/linux.rs::ip_link_ready` documents the same.

---

## 5. Match Semantics (src-tauri/src/conditions/ + `engine::decide`)

There is **no Profile priority** and no "most specific wins" tie-break. Matching is two-layer:

```
Profile = Rule1 OR Rule2 OR Rule3
Rule    = (all *enabled* Conditions ANDed)
```

- Every Rule and every Condition carries its own `enabled`. A **disabled Condition never counts as a
  match** (it is not a wildcard) — that is what makes "temporarily un-check one condition" safe.
- A Rule whose conditions are all disabled (or empty) is `INACTIVE`, not `MATCH`.
- A Profile matches when at least one enabled Rule matches. A Profile with no effective condition is
  simply `NOT MATCHED` forever; `Config::validate()` rejects such a Profile at load time instead.
- Exactly one match → that Profile is Active. Zero matches → No Active Profile (+ top-level `fallback`).
  Two or more → **Conflict**: no Profile is applied, the engine broadcasts `netsense://conflict`, and the
  popup and editor both show CONFLICT badges. Nothing in the codebase may auto-pick a winner; the
  regression test `engine::tests::conflict_never_picks_a_winner` is there to keep it that way.
- Manual "Apply now" (`engine::manual_apply`) re-evaluates *that Profile's own* conditions — it is not a
  bypass. It refuses a disabled Profile, and refuses during a conflict with `engine.conflict_blocked`.
- CONFLICT (condition layer) and ERROR (execution layer) are distinct display states and must not be
  collapsed into one another. **Only 3A can produce ERROR** (`engine::Engine::record_failure`, reached from
  a `Stage3A::Failed` or from the health monitor falling the config back to DHCP): it clears `active_id`,
  clears the applied fingerprint, and memoises the "this config + this network already failed" pair in
  `blocked` so the engine stops re-prompting for authorization every pass. A 3B1 batch that ends
  `Partial`/`Failed` produces **neither** — see §8. Four tests in `engine::tests` are that contract:
  `a_failed_batch_is_a_record_not_a_state_change`, `only_a_3a_failure_turns_the_profile_into_error`,
  `a_health_fallback_drops_active_without_an_error_message`, `conflict_and_error_stay_two_different_layers`.
  If one of them has to change, the layering is changing, and that deserves a spec edit rather than a
  test edit.
- Values are compared against a single [`NetworkSnapshot`] sampled once per pass, so SSID / BSSID /
  gateway-MAC conditions cannot disagree because they were read at different instants. MACs are
  normalized on both sides (`aa:bb:cc:dd:ee:ff` == `AA-BB-CC-DD-EE-FF`).

---

## 6. Config Schema 1 (`config.example.json` + `src-tauri/src/config/`)

`schema` is a **required** top-level field and must equal `1`. A file whose schema is missing or
different is rejected with an actionable error, **not** migrated: the loader cannot know what such a
file means when two Profiles match at once, and guessing would push a static IP nobody confirmed onto
the user's NIC. Rewriting by hand against `config.example.json` is the documented path.

这份文件里只有**自动化配置**。界面语言与日志保留天数属于软件配置，存在同一目录下的 `settings.json`
（`appconfig.rs`），判据是一句话：「改了它会改变自动化行为吗？」—— 不会的都不进 `config.json`。
因此改语言既不会触发热重载，也不会让引擎重新评估一次网络；反过来，编辑器保存 `config.json`
也不会顺手改掉用户的界面语言。

```jsonc
{
  "schema": 1,
  "allowed_scripts": ["/opt/ops/office-init.sh"],   // security boundary: global, not per-Profile
  "profiles": [
    {
      "id": "office", "name": "Office_5G", "enabled": true, "quick": true,  // quick: shows up in the panel's Quick Switch
      "detection": {                                  // per Profile, not global
        "mode": "network_events_and_polling",         // | network_events | polling_only
        "change_delay_secs": 5, "poll_interval_secs": 30
      },
      "rules": [                                      // Rules are ORed
        { "id": "r1", "enabled": true, "conditions": [ // enabled Conditions are ANDed
          { "id": "c1", "enabled": true, "type": "wifi_ssid", "value": "Office_5G" },
          { "id": "c2", "enabled": true, "type": "bssid", "value": "00:11:22:33:44:55" },
          { "id": "c3", "enabled": false, "type": "network_interface", "value": "en0" }
        ]},
        { "id": "r2", "enabled": true, "conditions": [
          { "id": "c4", "enabled": true, "type": "gateway_mac", "value": "aa:bb:cc:dd:ee:ff" } ]}
      ],
      "then": {
        "network": {                                  // 3A — hard barrier before 3B
          "mode": "manual", "ip": "192.168.1.100", "netmask": "255.255.255.0",
          "gateway": "192.168.1.1", "dns": "192.168.1.1,8.8.8.8", "v6mode": "off",
          "routes": [ { "dest": "10.0.0.0/8", "gateway": "192.168.1.1", "metric": 0 } ],
          "verify": { "readback": true,
            "health": { "enabled": true, "fallback": { "enabled": true }, "mode": "both",
              "http_target": "http://cp.cloudflare.com", "icmp_target": "223.5.5.5",
              "interval": 30, "retries": 3, "timeout": 5 } }
        },
        "one_shot": [                                 // 3B1
          { "id": "a1", "enabled": true, "priority": 1,
            "action": { "type": "launch_app", "app": "/Applications/Slack.app" } },
          { "id": "a2", "enabled": true, "priority": 1,
            // relative → resolved against the config dir, then allow-listed (see §8)
            "action": { "type": "run_script", "path": "scripts/office-vpn.sh", "elevated": false } },
          { "id": "a3", "enabled": true, "priority": 2,
            // the printer's *name* as the system knows it — the editor lists them (see §8)
            "action": { "type": "set_default_printer", "printer": "Office LaserJet" } }
        ],
        "persistent": [                               // 3B2 — one worker each, started with Active's THEN
          { "id": "p1", "enabled": true, "priority": 1,
            "action": { "type": "keep_wireguard_connected", "tunnel": "wg0", "interval_secs": 15 } }
        ]
      },
      "else": { "network": { "mode": "dhcp",
        "dns": "",                                    // "" clears the servers; omitting `dns` would keep them
        "routes": [ { "dest": "10.0.0.0/8", "delete": true } ] } }
    }
  ],
  "fallback": { "enabled": true,                      // zero-match handling; NOT a Profile
    "network": { "mode": "dhcp", "dns": "", "v6mode": "automatic" } }
}
```

Key modelling points, each of which is easy to get wrong:

- **`id` is the stable handle**, names are decoration. IPC, status reports and "Apply now" all address a
  Profile by `id`, so renaming one cannot point the state at a different Profile.
- **`fallback` is not a Profile.** It has no conditions, so it never participates in matching and can
  never be picked as "current"; it is a top-level field that only handles zero-match.
- **For the fields that honour it, a missing optional key means "don't touch it" and an empty string
  means "clear it".** `v6mode` and `dns` are both modelled this way, and each keeps the distinction all
  the way down to the wire (§7): `None` pushes no command at all on all three platforms, `Some("")`
  pushes a real clear (macOS `-setdnsservers … Empty`, Windows `source=dhcp`, Linux `ipv4.dns ""`).
  That is what makes the editor's three-way DNS select (keep / system-auto / set) honest — "keep" is
  expressed by deleting the key, and the apply path leaves the servers alone. `set_dhcp()` is the one
  deliberate exception: the fallback path clears DNS explicitly, because there "no longer pin anything"
  *is* the desired state.
- **Routes live in 3A**, not in the automation list: a route that failed to land must not let the rest of
  the automation run. Inside 3A it is covered by Apply+Verify and blocks 3B when it fails.
- `Config::validate()` rejects structurally-broken configs before they reach disk (empty/duplicate ids,
  a Profile with no Rule, an empty Rule, a condition with an empty value, `manual` without ip/netmask/gateway,
  non-IPv4 DNS, routes without a gateway unless `delete`). It also checks the 3B/health **payloads**, because a
  half-filled action fails as "nothing happened" rather than as an error: action `id`s must be unique across a
  branch's `one_shot` **and** `persistent` together (`EngineView.last_run` and `EngineView.workers` find their
  card by id, so a collision would report two actions as one), `launch_app.app` / `run_script.path` / `set_default_printer.printer` / `periodic_script.path` /
  `keep_wireguard_connected.tunnel` / `keep_vpn_connected.provider` + `profile` must be non-empty,
  `interval_secs > 0`, and an **enabled** `verify.health` must have a target for its `mode` plus non-zero
  `interval`/`retries`/`timeout` (a disabled health block is left alone — writing the parameters before
  switching it on is a legitimate order of work). `Config::warnings()` reports what loads fine but will
  never run: today that is exactly one thing — enabled `persistent` actions configured on a Profile's
  **else** branch, which the engine never starts workers for (§8).

---

## 7. 3A: Apply → Verify (src-tauri/src/network/)

3A is the hard barrier in front of 3B (spec rules 16/18/19/20). `apply_3a` = push config → push routes →
**read back the real parameters** and compare with what was expected; anything else is
`Stage3A::Failed { reason }`, and the engine then runs **zero** 3B actions and marks the Profile ERROR.

- Verification is a readback, not an exit code, because `networksetup` / `netsh` / `nmcli` routinely
  return 0 when the privilege prompt was denied, the service name didn't resolve, or the NIC had just
  detached. Treating 0 as success gives the user an environment that claims a static IP and has none.
- Readback samples with settle time (`READBACK_ATTEMPTS` × `READBACK_SETTLE`): re-association and DHCP
  take longer than the command that requested them, and an immediate read false-fails.
- "Field absent" vs "field empty" are different expectations (`readback.rs`): expecting no DNS means we
  must **not** compare against the DHCP-supplied servers, and expecting DNS must fail on an empty read.
- `health` is the time-extension of the same check: a network can be correctly configured and still not
  reach anything. `network::HealthMonitor` runs **bound to the Active Profile's lifecycle** (start on
  activate, stop on deactivate/zero-match/manual DHCP): a detached global loop would keep probing
  after a switch and flip the NIC back to DHCP at a moment the user didn't expect. Consecutive failures ≥
  `retries` trigger a verdict; only `verify.health.fallback.enabled` turns it into `set_dhcp()`, otherwise
  it notifies and leaves the config alone.
- Probe modes: `icmp` / `http` / `both`. Under `both` a target is dead only when **both** probes fail
  (many networks block ICMP but pass HTTP; this also keeps captive portals from reading as healthy).
- Rollback (restore the previous working config instead of the blanket DHCP fallback) is not implemented;
  see §14.

---

## 8. 3B: Actions (src-tauri/src/automation/)
**3B1 one-shot** (`automation/one_shot.rs`) runs once per *entry into Active*; a re-evaluation that keeps
the same Profile active does **not** re-run it (otherwise every poll interval re-launches apps).

- Filter out `enabled: false` → sort by `priority` ascending (smaller = earlier) → group adjacent equal
  priorities into one batch.
- Within a batch: concurrent. Across batches: the next batch starts only after **every** action of the
  current one has finished, regardless of success.
- A failing action does **not** block later batches — the opposite of 3A, on purpose: "VPN didn't connect"
  should not prevent "open Slack", while "the network config didn't land" must prevent everything.
- Result recorded as `Empty / Success / Partial / Failed` (`BatchReport` carries per-action outcomes),
  surfaced in the log and as `engine.one_shot_partial` when incomplete.
- **A `Partial`/`Failed` batch never touches the Profile's state.** Attributing the report
  (`Engine::note_run_done`, then `apply_runs`) fills `last_run` and releases the run slot — nothing else.
  The Profile stays Active and no ERROR badge appears: the 3A config really did land, and "the VPN script
  exited non-zero" is information for the user, not a reason to tear the environment down. §5 names the
  tests that pin this.

**3B2 persistent** (`automation/persistent.rs` + `automation/provider.rs`) is a desired-state worker: check
every `interval_secs`, act only when the state is wrong — *not* "run the same command every N seconds".
`provider::tick_for` turns a `PersistentActionType` into one `Tick` (`Satisfied` / `Repaired` /
`Faulted(reason)`), and `persistent.rs` owns everything around it. There is deliberately *one* `Tick`
trait rather than a per-kind provider trio: the kinds differ only in which two PAL calls they make, so
three traits would each have exactly one implementation and would add a file per action type without
adding a seam anyone can substitute.

- **Lifecycle: one worker thread per enabled action, started only with Active's THEN branch.** `else`
  branches describe a state to return to, not one to maintain, so a `persistent` action configured there
  cannot run — `Config::warnings()` says so instead of leaving the user to discover it.
- **Priority only orders worker *start*** (stable sort, `START_SPACING` stagger); once running, workers are
  independent and never block each other.
- **A tick that is still running when the next interval arrives is skipped, not queued** (`busy`
  compare-exchange, released by a `Drop` guard so a panicking tick cannot wedge the worker forever). Sleep
  is sliced (`SLEEP_SLICE`), so "stop" lands in ≤500 ms instead of at the end of an hour-long interval.
- **Already satisfied ⇒ zero commands.** `keep_wireguard_connected` reads `tunnel_is_up` first and only
  calls `tunnel_connect` when it is down; `periodic_script` is the exception the user opts into, and its
  path goes through the same allow-list as 3B1.
- **Workers never elevate** (see §4): an unattended repeating loop plus an interactive auth UI is a
  permission dialog every N seconds.
- **Status reports only on change**, carrying a monotonic `generation`; a late report from a superseded
  session is dropped rather than painted onto the new one. The engine folds them into
  `EngineView.workers` (`pending` / `satisfied` / `repaired` / `faulted` / `overdue`), which the editor
  shows as a chip per card and the popup collapses into one "Maintained" line. A worker's failure
  **never** changes Profile state — same layering as 3B1: 3A owns ERROR, 3B only leaves traces.
- **Every new dispatch stops the old workers first.** `execute_branch` stops them *before* 3A, and
  `deactivate()` / `set_dhcp()` / the health fallback stop them too: otherwise the previous environment's
  "keep this VPN connected" fights the routing table that was just written, and flips the NIC back at a
  moment the user did not ask for.

Tests: `automation::persistent::tests` (self-overlap gate incl. the panic path, prompt stop, one report
per change, superseded-generation drop, start order) and `automation::provider::tests` (an up tunnel
triggers zero commands, the allow-list refusal, labels / budgets).

**Per-platform action semantics** — the trait doc in `platform.rs` is the contract
(`launch_app` = *hand it off*, never wait for the app to exit; `run_script` = *wait*, its exit code is the
result; `elevated` always goes through the system's own authorization UI, exactly once per action;
`set_default_printer` = a **per-user** preference write, so it never touches the elevation channel — a
network switch happens without the user asking, and a dialog on that path is unacceptable). How
each backend meets it, since each divergence here has already cost one bug:

| | `launch_app` | `run_script` | `run_script` + `elevated` | `set_default_printer` |
|---|---|---|---|---|
| macOS | `open -a App` (`--args` when there are arguments); returns once LaunchServices accepted, so an app that is **already running** ignores the arguments | `exec` the path, wait | one `osascript … with administrator privileges` dialog | `lpoptions -d <name>`: CUPS writes the **per-user** default to `~/.cups/lpoptions`; an unknown name exits 1 and its stderr becomes the failure text |
| Windows | `Start-Process -FilePath` without `-Wait`: "process created" is all it can claim | by extension: `.ps1` → `powershell -File`, `.bat`/`.cmd` → `cmd /c`, anything else executed directly | one UAC prompt for the target itself (`run_elevated_target`) | `Invoke-CimMethod -MethodName SetDefaultPrinter` on the `Win32_Printer` row whose `Name` matches exactly; no such row or `ReturnValue ≠ 0` → a non-zero exit plus a message |
| Linux | executable → detached `spawn`; otherwise `xdg-open`, which takes **one** argument and cannot pass any | `exec` the path, wait | `sudo -n`, falling back to `pkexec` | same CUPS client command as macOS (`lpoptions -d <name>`), so the same per-user semantics |

Three details that are not guessable from the outside:
- `open -a App X` means "open file X *with* App", and an X starting with `-` is eaten by `open` itself →
  application arguments have to follow `--args` (`open_args`).
- `Start-Process -ArgumentList @(...)` joins its elements with spaces **without quoting them** — unlike
  `& exe @(...)`, where PowerShell quotes arguments containing spaces for you. So an argument with a space
  needs its own double quotes, which is why `ps_exec_arr` exists next to `ps_arr` instead of being merged
  into it.
- Waiting for a launched GUI app is the wrong shape: `run()` uses `.output()`, which blocks until the child
  exits, so "open the browser" would burn the entire 30-second launch timeout and then be reported as a
  failure *while the browser is open*. Linux therefore goes through `spawn_detached`, and that helper reaps
  the child on a side thread so the long-lived app process doesn't accumulate zombies.

Still owed on real hardware (CI cannot reach any of it): macOS's already-running + arguments case,
Windows' single-prompt + exit-code propagation for `.bat` / `.ps1`, and Linux `xdg-open` both with and
without a matching `.desktop`.

---

**Security boundary**: `run_script` only executes paths inside `<config dir>/scripts` or those explicitly
listed in top-level `allowed_scripts` (`automation::AllowedScripts`). Without that check, a config.json
from anywhere would be arbitrary code execution with an optional `elevated: true` on top.

The path is resolved **before** it is checked, by `automation::resolve_script`, and the resolved value is
what gets executed: absolute paths are kept as written, relative ones are anchored to the config
directory — so `"scripts/office-vpn.sh"` means "next to config.json", independently of which directory
the process happened to start in. Letting the two sides disagree (allow-list reads `<config dir>/scripts`,
`exec` reads `<CWD>/scripts`) is not a cosmetic issue: it makes the file that was approved and the file
that runs two different things. Both sides are then canonicalized with the same rule
(`canonicalize_best`), which is what lets `/var/…` and `/private/var/…` on macOS count as one path, while
`starts_with` comparing path components keeps a sibling `scripts2/` directory out of `scripts`.
When the filesystem cannot answer at all — a config may register a script that is only generated later —
`canonicalize_best` collapses the dot segments lexically instead of comparing the path as written. Same
reason: component-wise `starts_with` would let an unresolved path that goes up out of the trusted
directory and further still count as "inside it", because its leading components do sit inside that
directory — while the OS resolves those upward steps at exec time and runs a file two levels out.

---

## 9. Privilege Model

### 9.1 Implemented: sudoers NOPASSWD + allow-list wrapper script

Goal: **changing the network no longer pops an auth dialog every time**, without opening a hole for "arbitrary root commands".

```
GUI process (unprivileged) ──sudo -n──▶ /usr/local/libexec/netsense-priv.sh (root-owned, 0755)
                                 └─ allow-list subcommand + argument-shape validation ──▶ networksetup / route
```

- **Structured operations**: the Rust side only constructs `PrivOp` (`SetManual/SetDhcp/SetDns/SetV6*/RouteAdd/RouteDelete`), encodes it via `PrivOp::encode()` into an `op|arg1|arg2` line, and feeds the script's `--batch` mode over stdin in one shot.
  **User input is never interpolated into a shell command** under any circumstance, eliminating command injection.
- **Script-side second check**: `scripts/netsense-priv.sh` validates the shape of each argument (IPv4 octet 0–255 and rejects leading zeros, prefix 0–128, route target `a.b.c.d[/len]`, DNS as comma-separated IPv4 set, service name non-empty and not starting with `-`); unknown subcommands are rejected outright.
- **Authorization surface**: `/etc/sudoers.d/netsense` authorizes only the script's absolute path (`NOPASSWD:`), not arbitrary user commands; the script and its directory are root-owned and not writable by normal users.
- **Fallback**: if the wrapper script is missing, or `sudo -n` reports "password required / no tty / not allowed", it automatically falls back to `osascript ... with administrator privileges` (the original per-change auth dialog), with no feature breakage.
- **Self-check**: the install script runs `sudo -n <script> --batch` once as the current user at the end to verify the channel works.

Install / uninstall:

```bash
sh scripts/install-priv-helper.sh            # install (needs sudo)
sh scripts/install-priv-helper.sh uninstall  # uninstall
```

### 9.2 Why not a resident privilege daemon directly

A `launchd` resident + Unix socket approach needs its own socket authentication (macOS has no `SO_PEERCRED`, requires `getpeereid` FFI) and protocol version compatibility — clearly larger complexity and attack surface.
sudoers + allow-list script achieves the same goal using the system's built-in authorization mechanism, and with **a narrower privilege scope** (only network-configuration operations are allowed). If stronger isolation beyond passwordless is needed later (e.g. SMJobBless signature checks), revisit the open items in §14.

Windows ended up taking the helper route anyway (§9.6 "Four key Windows backend designs" rule 3) because what makes this section's launchd+socket plan expensive is nearly free there: the kernel's own named-pipe DACL plus a client-process image-path check authenticate the caller, and the protocol is just "run this already-rendered batch" — no new privilege is ever minted. On macOS the complexities named above still argue for the sudoers allow-list; a Windows-style helper is not on the roadmap.

> If a `run` automation action declares `elevated: true`, it does **not** go through the allow-list channel but still uses the `osascript` auth dialog —
> user scripts must be treated separately from network-configuration operations, guaranteeing the user is informed every time a privileged script runs.

### 9.3 The updater's elevated step (`src-tauri/src/update.rs`)

The updater is the one place that hands root a file an unprivileged process wrote seconds
earlier, so it keeps its own rules:

- **Every temp path is created exclusively.** `open_private_new` / `new_private_dir` combine
  `create_new` with `0600` / `0700` and put pid + nanosecond + counter in the name, so hitting an
  occupied name means *try another name*, never *use that one*. `File::create` looks equivalent and
  is not: the file it opens keeps whoever else created it as **owner**, the sticky bit stops us
  removing it, and that owner can rewrite the content between our write and the moment
  `osascript … with administrator privileges` runs `/bin/sh <path>` (CWE-377). The dmg mount point
  and the zip staging dir follow the same rule — a planted `NetSense.app` in a directory we unpacked
  into is exactly what root's `ditto` would install into `/Applications`.
- `verify_private` (owner is us, no group/other bits) additionally guards the two paths that cross
  the boundary: the script handed to root, and the downloaded asset between checksum and install. It
  covers the case where `$TMPDIR` itself points somewhere another user can swap out from under us.
- **Verification is fail-closed.** No `SHA256SUMS` on the release, an asset it does not list, an
  unfetchable sums file and a mismatch all abort the update; the UI then falls back to "open the
  release page", which is what makes being strict affordable. `build.yml` publishes exactly one
  root-level `SHA256SUMS` after all four legs land (§13.2), so a normal release always verifies.
- **Every hop is https, not just the URL we were handed.** The scheme is screened before a request
  leaves the process, and both curl call sites pass `--proto =https` because `-L` alone would follow
  an `https → http` redirect and finish the transfer in cleartext (`--proto` filters redirect targets
  too; verified against a live redirector). `--proto-redir` is deliberately absent: it needs curl
  7.65.2, while the oldest Windows this app supports ships 7.60.1, and one unrecognised option would
  turn "an update is available" into "download failed" for no added protection.
- **The verdict is made at check time, not at click time.** `check_update` reports
  `installable` / `install_note`, decided from the same `SHA256SUMS` `run_update` is about to read
  (one extra GET to that URL, and only when an update is actually pending). A release the app cannot
  verify therefore never shows an **Update Now** button: the panel says why and offers the release
  page instead. Fail-closed at install time stays exactly as strict — this only moves the bad news
  earlier, so nobody spends a download on an update that was going to be refused. Homebrew installs
  skip both the question and the request.
- Windows and Linux stage the *installer* under a per-user directory (`%LOCALAPPDATA%`,
  `$XDG_DATA_HOME` / `~/.local/share`), which is private already — only the Linux relaunch script
  lives in temp, and it goes through the same private-path helper.

`codesign` stays a **conditional** check: CI without an Apple certificate ships unsigned builds, and
a hard check would disable in-app updating for every user of those. What it still refuses is an
unsigned package replacing an installed *signed* one.

---

## 9.5 Status-Bar Popup Panel (primary interaction entry)

The standard menu-bar app interaction: **click the icon (either button) to toggle the panel, auto-collapse on blur**. There is no native tray menu - see §3 `tray.rs`.

### Window layout (`tauri.conf.json`)

| label | file | key config |
|-------|------|------------|
| `popup` | `popup.html` | `360×620`, `decorations: false`, `alwaysOnTop`, `skipTaskbar`, `visible: false` |
| `main` (editor) | `editor.html` | `1180×700`（`minWidth 980 / minHeight 560`）, `center`, `visible: false`（以隐藏态创建，`setup` 里紧接着 `popup::show_main` 把它显示出来 —— 启动就能看到编辑器） |
| `settings` | `settings.html` | `620×600`（`minWidth 480 / minHeight 420`）, `center`, `visible: false`（面板的「设置」打开它；关闭 = 隐藏，同编辑器） |
| `logs` | `logs.html` | `900×620`（`minWidth 620 / minHeight 420`）, `center`, `visible: false`（面板的「日志」打开它；关闭 = 隐藏，同上） |

All four windows are created at startup and only shown/hidden — the webview is **not rebuilt on every click** (guarantees instant open). 四个 label 都必须在 `capabilities/default.json` 的 `windows` 里登记，否则那扇窗口的每一次 `invoke` 都被 ACL 拒掉、界面永远空白。

### Show/hide & positioning (`src-tauri/src/popup.rs`)

1. **Trigger**: the tray has **no native menu** — `TrayIconEvent::Click{Left|Right, Up}` always dispatches to `popup::toggle()`. One icon, one behavior, identical on all three platforms (macOS would otherwise wait for a menu on right-click, and there is none).
2. **Positioning**: take the event's `rect` (physical icon rectangle) → anchor = icon horizontal center + vertical bottom edge, then shift `y` down 6px; finally `clamp` using `current_monitor()`'s position/size to prevent the panel from going off-screen on edges or external displays.
3. **Blur-collapse**: in `Builder::on_window_event`, watch `WindowEvent::Focused(false)` with label `popup` → `popup::hide()`.
4. **Debounce (key detail)**: when clicking the icon to collapse the panel, blur fires a hide first, then the tray click opens it again — appearing as "one click flickers". `popup.rs` records the collapse moment with `LAST_HIDE: Mutex<Option<Instant>>` and ignores tray clicks within 300ms.
5. **macOS menu-bar form**: `set_activation_policy(ActivationPolicy::Accessory)` in setup hides the Dock icon; the editor window's "close" is intercepted by `CloseRequested` into `hide()` (`api.prevent_close()`), i.e. closing = retracting to menu-bar resident, not quitting.

### Panel content (`frontend/popup.html`)

Three sections, all fed by `get_status` + `get_interfaces` (rows whose value is missing collapse, so
nothing shows a wall of `—`):
1. **Current network** — status dot in the header, then applied profile (three-way: Active name /
   **Conflict: names** in amber / "No profile applied"), last 3B1 run, maintained 3B2 workers, and the
   details of *the NIC actually in use*: interface, SSID + signal (only when it is wireless), MAC, IPv4,
   netmask, gateway, IPv6, DNS. `get_interfaces` guarantees the primary NIC is `nics[0]`
   (the single judge is `automation::primary_nic`), so the frontend never re-implements "which NIC is mine".
   Below it, "Other active interfaces" lists the remaining non-VPN NICs as cards.
2. **VPN / virtual adapters** — one card per VPN NIC (owning app shown as the tag).
3. **Quick Switch** — only Profiles whose `quick` flag is set (a per-profile editor checkbox), each row **with a live status badge**
   (`ACTIVE` / `NOT MATCH` / `CONFLICT` / `DISABLED` / `ERROR`) — clicking a row calls `apply_profile({id})`,
   which re-evaluates that Profile's own conditions rather than forcing anything.
Bottom: "settings / logs / DHCP / probe / check for updates"; the header keeps quit only. The
privilege channel is **not** shown here: it is troubleshooting info, and next to an unreadable SSID it read
as if *reading* the SSID needed authorization —— 它现在显示在软件设置窗口的「运行信息」里。
On each focus (when opened) it auto-refreshes via `get_status`, and also passively refreshes by subscribing to the `netsense://status` event.

### Software settings window (`frontend/settings.html`, label `settings`)

面板的「设置」打开的是**软件设置**，不是编辑器 —— 两份配置各有一个入口，编辑器从这座窗口里再进去
（`open_settings` → 窗口内 `open_editor`）。窗口内容一次问一次：`get_app_settings` 返回语言、开机启动的
**系统实况**、`config.json` / `settings.json` / 日志目录三个路径、日志保留天数与其上下限、提权通道、平台、版本。

- **语言切换必须一次点击就看得见**：`set_language` 之后立刻重拉 `get_strings` 并重绘本窗口，其余窗口靠
  `netsense://status` 广播跟随（面板比较 `status_payload.language`，只在真的变了时才重取词表）。托盘 tooltip
  是建托盘时当场求值的字符串，广播到不了它，所以同一条命令里还要显式重设（`tray::refresh_tooltip`）；已经写进
  日志和动作历史的那些句子保持写下时的语言，见 §10.1。
- **开机启动**勾选框每次窗口获得焦点都重问一次系统：用户可能刚从系统设置或注册表里动过它，读缓存会让这个
  勾选框说谎。写失败时把勾选框回滚到原值，而不是让它显示一个系统没答应的状态。
- **保留天数**由 `set_log_retention` 落盘并立刻按新窗口清一次；命令回的是**夹紧后**的值，输入框按回显走，
  所以「填 9999」不会在界面上留下一个系统其实没有的值。

### Points needing real-device verification (cannot verify locally)

- After `ActivationPolicy::Accessory` takes effect, whether `window.set_focus()` can give the panel keyboard focus, thus reliably triggering blur-collapse. If clicking outside on macOS does not collapse, append an "activate app" call after show.
- Whether right-click on the macOS menu-bar icon delivers `TrayIconEvent::Click` at all (without a menu attached it can fall through to the system's own behavior); if it does not, left-click alone opens the panel — acceptable, not a fallback menu.
- The `rect` field of `TrayIconEvent::Click` exists only in newer versions; if absent, use the same event's `position` (click coordinates) as the anchor — `popup::toggle`'s parameters are unchanged.
- The tray `rect` is physical pixels under Retina; anchor conversion already handles physical pixels. If the panel is offset by half an icon width, the platform returned logical coordinates — convert by scale factor instead.

## 9.6 Three-Platform Implementation Reference (`platform/{macos,windows,linux}.rs`)

The upper layer (`main.rs` / `ipc.rs` / `engine.rs` / `conditions` / `detection` / `network` / `automation`)
**depends only on `platform::Platform` and the `NetworkPlatform` trait**; the concrete platform is selected at compile time by `platform.rs` via `target_os` and re-exported uniformly:

```rust
#[cfg(target_os = "macos")]   pub use macos::{priv_channel, MacPlatform as Platform};
#[cfg(target_os = "windows")] pub use windows::{priv_channel, WindowsPlatform as Platform};
#[cfg(target_os = "linux")]   pub use linux::{priv_channel, LinuxPlatform as Platform};
```

| Capability | macOS | Windows | Linux |
|------------|-------|---------|-------|
| Wi-Fi interface discovery | `networksetup -listallhardwareports` (Hardware Port → Device), cached; **never hardcode `en0`** | `Get-NetAdapter` by `MediaType='Native 802.11'` | `nmcli -t -f DEVICE,TYPE dev status` finds `wifi` |
| Current SSID | gradient: `CoreWLAN` (needs Location permission) → `networksetup` → `ipconfig getsummary` → `system_profiler` (see 9.6.1) | `Get-NetConnectionProfile.Name` | active connection name (`nmcli`) |
| IP/mask/gateway/DNS | `ipconfig` + `networksetup -getinfo` | one PowerShell JSON pull (`Get-NetIPAddress` / `Get-NetRoute` / `Get-DnsClientServerAddress`) | `nmcli -g IP4.*` |
| BSSID / signal | `airport -I` (≤ 14.3) → `system_profiler SPAirPortDataType` | `netsh wlan show interfaces` (**ASCII fields only**) | `nmcli -f IN-USE,BSSID,SIGNAL dev wifi list` |
| Gateway MAC | `arp -n <gw>` | `arp -a` matching gateway line | `ip neigh show <gw>` |
| Write IP/DNS | `networksetup -setmanual/-setdnsservers` | `netsh interface ipv4 set address/dnsservers` | `nmcli con mod ipv4.*` + `con up` |
| IPv6 toggle | `networksetup -setv6off/-setv6automatic` | `Enable/Disable-NetAdapterBinding ms_tcpip6` | `nmcli con mod ipv6.method` |
| Route | `route -n add/delete` | `New-NetRoute` / `Remove-NetRoute` | `ip route add/del` |
| Probe ICMP / HTTP | `ping -c1 -t` / `curl` | `ping -n1 -w` / `curl.exe` | `ping -c1 -W` / `curl` |
| Tunnel up? (3B2 read) | `scutil --nc list` (`WireGuard for macOS` registers each tunnel as a NEVPN config, so it appears here too) | `Get-NetAdapter` — the adapter **description** is the provider hint | `nmcli -t connection show` → `ip link show <dev>` (flags, not `state`) |
| Connect tunnel (3B2 write) | `scutil --nc start <label>` | `wireguard.exe /installtunnelservice <conf>` / `rasdial <name>` | `nmcli connection up <name>` → `ip link set <name> up` |
| Elevate = Direct | sudoers allow-list script (`sudo -n`, no dialog) | process already admin | `sudo -n` available |
| Elevate = Prompt | `osascript ... with administrator privileges` | config batches: resident helper (`win_helper.rs`, one UAC per GUI session, same `Start-Process -Verb RunAs` to spawn it); fallback / user scripts: UAC per call (`Start-Process -Verb RunAs`) | `pkexec` |
| Saved SSID list | `networksetup -listpreferredwirelessnetworks` | read WLAN config XML (`[xml]` parse) | `nmcli con show` filtered to `802-11-wireless` |
| Open log folder | `open` | `explorer` | `xdg-open` |

> The "Connect tunnel" row deliberately uses **neither** elevate channel below: a worker that elevated
> would ask the user for permission every `interval_secs` (§8). When the system refuses, the platform
> layer returns the error and the interface shows it.

#### 9.6.1 macOS backend: Wi-Fi metadata is version-gated (read before changing)

Apple has been progressively locking down Wi-Fi metadata, so **a single source always breaks
on some macOS generation**, and the symptom is a blank SSID area plus an incomplete status-bar
menu. Verified source availability:

| macOS | `CoreWLAN` (SSID) | `airport -I` | `networksetup -getairportnetwork` | `ipconfig getsummary` | `system_profiler` |
|---|---|---|---|---|---|
| ≤ 14.3 | SSID (no prompt) | SSID + RSSI + BSSID | SSID | SSID | all |
| 14.4–14.5 | SSID, needs Location permission | **removed** | SSID | SSID | SSID + RSSI (BSSID empty) |
| 15.0–15.5 | SSID, needs Location permission | removed | **always "You are not associated…"** | SSID (~46 ms) | SSID + RSSI |
| 15.6+ | **the only source still telling the truth**, needs Location permission | removed | same | **SSID `<redacted>`** | **SSID redacted too** (RSSI usable) |

Rules that follow from it:

1. **Never hardcode the interface.** By default `en0` is Wi-Fi on portables, but on iMac /
   Mac mini / Mac Studio `en0` is Ethernet and Wi-Fi is `en1`. Hardcoding it reads the wrong
   port for everything (`wifi_iface()`, cached, resolves it via `listallhardwareports`).
2. **SSID is a gradient, not a lookup**: `CoreWLAN` → `networksetup` → `ipconfig getsummary` →
   `system_profiler`, each guarded by `sanitize_ssid()`. That guard is load-bearing — without
   it the "not associated" sentence or `<redacted>` would be shown as a network name, and the
   chain would never fall through to the next source.
3. **CoreWLAN treats the SSID as location information** (macOS 14+). Reading it requires the
   one-shot `requestWhenInUseAuthorization` fired from the `setup` hook plus
   `NSLocationWhenInUseUsageDescription` in the bundle `Info.plist`
   (`bundle.macos.infoPlist`); only then does NetSense appear in System Settings →
   Privacy & Security → Location Services. While permission is not granted the read returns
   `None` and the chain degrades to the CLI sources — which on 15.6+ all come back blank,
   so an ungranted app shows exactly the old "unknown" behaviour, never a failure.
4. **`system_profiler` survives 14.4+ only for RSSI/BSSID** (its SSID is redacted from 15.6
   on), and it is slow (1–4 s). It is last in the chain and its result is cached for 2 s
   (`SYS_PROFILER_TTL`) so that a panel refresh plus a watcher poll share one call.
   Do not reorder it earlier.
5. **`connected` must not be derived from the SSID alone.** When SSID is unreadable (15.6+,
   or an unauthorized process) the UI would claim "not connected" while holding an IP, and
   SSID-keyed profiles would never match. It is `ssid.is_some() || ipv4.is_some()`.
6. **Nothing that samples status may run on the main thread**: the tray icon and the windows are
   built there, so `publish_status()` samples once per broadcast, and the commands that
   sample (`get_status`, `get_interfaces`, `check_update`, `run_update`, and every command that
   posts to the engine) carry `#[tauri::command(async)]` — in Tauri, a *non-async* command runs on
   the main thread.

> Parsing note: `system_profiler` prints the SSID as a **key**, not as a value
> (`Current Network Information:` → `  Office_5G:`), so it is parsed by indentation: the first
> deeper line ending in `:`. Anything at the same or shallower indent ends the block — without
> that stop condition the next section's SSID gets read instead, which is worse than blank
> because it silently matches the wrong profile.

### Four key Windows backend designs (read before changing — known pitfalls)

1. **Read via PowerShell CIM, not `netsh` text**.
   On Chinese Windows, `netsh` output is **localized + OEM code page (CP936)**: field names become Chinese, values like "已连接"/"信号" are Chinese too, and decoding as UTF-8 corrupts them.
   CIM cmdlets return objects, language-independent; we uniformly inject `[Console]::OutputEncoding = UTF8` in `ps()` so Chinese-containing return values (SSID, adapter name) decode correctly.
2. **Exception: BSSID / signal percentage still come from `netsh` text, but only ASCII fields are extracted** (MAC hex, `NN%` digits). ASCII bytes are identical under CP936 and UTF-8, so unaffected by encoding; and these two lines have no CIM equivalent. **Do not** extract SSID the same way (SSID may be Chinese).
3. **Elevation commands always pass args as `@('a','b')` arrays, never string-concatenated**:
   `& netsh.exe @('interface','ipv4','set','address','name=Wi-Fi','static',...)`.
   Interface names with spaces/special chars are not re-split. The whole batch is rendered into a PowerShell script and handed to `Start-Process -Verb RunAs` via `-EncodedCommand` (UTF-16LE + base64, self-implemented, zero-dependency) for **one UAC covering all operations**.
   Config batches first try the **resident helper** (`platform/win_helper.rs`): the same exe re-launched elevated once per GUI session (`--netsense-helper`), executing batches over a named pipe whose DACL is owner-only and whose server verifies the connecting process is the *same executable*. Trust model: a batch's content comes from a config this very user edited, so the helper — reachable only by that same user — asks nothing new; user scripts (`run_script elevated`) **deliberately keep their per-run UAC**. Any helper failure (prompt declined, pipe dead, hostile same-user process hogging the slot) falls back to the plain per-batch UAC path above, and since every batch op (`netsh set …`) is an idempotent setting, the one retry after a half-delivered request costs nothing.
4. **Never slice a `&str` by byte offset — command output is *lossily* decoded.**
   CP936 Chinese becomes 3-byte `U+FFFD` (the `�` in logs), so `&line[i..i + 17]` panics the instant `i` lands inside one (`byte index N is not a char boundary`).
   `platform.rs::extract_mac` therefore scans `line.as_bytes()`, skips any window containing a non-ASCII byte, and only then builds the `&str`; the regression test uses the bytes captured from a real Chinese Windows box.
   This is the failure mode that kills the app **before the window ever opened**: anything that samples `get_status()` inside `setup()` pays for it on the main thread, and `arp -a`'s Chinese interface header 「接口: 192.168.1.10 --- 0x10」 reached the scanner only because the gateway lookup used `line.contains(gw)` — and `"192.168.1.10"` contains `"192.168.1.1"`.
   Corollary: match IPs as **whole tokens**, never substrings (`has_ip_token`). Otherwise the row for `10.0.0.10` answers for gateway `10.0.0.1`, and you get a *different neighbour's* MAC.

> **Status cache**: `resolve_current_name()` asks SSID + gateway MAC + BSSID at once, but each read on Windows spawns a PowerShell process (hundreds of ms).
> `platform/windows.rs` uses a 1.2s-TTL snapshot cache `status_cached()` to absorb the cost; `invalidate_cache()` clears it immediately after a write.

### Cross-platform MAC comparison traps (fixed)

MAC formats returned by each platform are inconsistent: Windows `arp -a` is `aa-bb-cc-dd-ee-ff`, macOS/Linux is `aa:bb:cc:dd:ee:ff`, and case is not unified either.
So `conditions::evaluator` compares **both sides through `platform::normalize_mac()`** (lowercase + colon), so any form written in config matches.
**Consequence for the parsers: `extract_mac` / `is_mac` must accept both `:` and `-`.** A colon-only shape check made the whole normalization path dead on Windows — `gateway_mac` stayed empty forever (and, before the fix above, the scan panicked on the way).
SSID comparison **stays case-sensitive** (802.11 SSID is itself case-sensitive).

---

## 10. i18n / Logging / Config Hot-Reload

### 10.1 i18n (`src-tauri/src/i18n.rs` + `i18n/*.json`)

- 5 languages (zh / en / zh-TW / ja / ko), **`en.json` is the baseline**; dictionaries are embedded into the binary at compile time via `include_str!`, so no file is read at runtime and no packaging omission can occur.
- **English is the default on every platform, and any other language is a user choice** — nothing depends on a locale guess. The four windows, the tray tooltip and the native startup dialogs all read the same dictionary.
- Lookup order: current language → `en` → the key itself (**never panics**). `tf(key, args)` substitutes `{name}` placeholders; a placeholder a translation drops is a bug, not a style choice, so `{placeholder}` parity is checked per key.
- Namespaces are only key prefixes, and the set of them is derived from `en.json` itself (`app`/`editor`/`engine`/`notify`/`popup`/`status`/`tray`/`sett`/`logs`/`cfg`/`pal`/`act`/`net`/`upd`/`dlg` today, **458 keys × 5 languages**) — adding one needs no change here. `dlg.*` is the odd one out: those strings go to a Win32 `MessageBox`, which never renders the WebView, so no frontend mechanism can reach them.
- **Key-parity check** (`check_parity()` returns missing/extra/empty, requiring all three to be 0) runs once at app startup; failure only warns, does not block startup.
  `cargo test` guards the bundle with four cases: `parity_ok_in_bundle` / `fallback_to_en_then_key` / `placeholder_replace` / `every_language_keeps_ens_placeholders`.
- Language switch: IPC `set_language` → 写 `settings.json`（软件配置，见 §10.4）+ 改进程内的当前语言；它**不**碰 `config.json`，
  所以换语言不会触发热重载、更不会让引擎重新下发一次网络。前端 `get_strings` 一次拉走当前语言的全部文案，避免逐 key 往返 invoke。
- Add new copy to all 5 languages together; delete from all 5 together.
- **界面文案只有一份来源。** Static markup carries `data-i18n` / `data-i18n-placeholder` / `data-i18n-title`, and one loop in each window maps them onto textContent / `placeholder` / `title`; `get_strings` pulls the whole dictionary once, while `get_language` only feeds `<html lang>` —— 那个属性是给读屏软件和 CJK 字形选择看的，不是查表用的。The English left in the markup is the *no-bridge fallback*, and it must equal `en.json` byte for byte: two copies of one sentence is how a UI starts showing text that then jumps when the dictionary lands.
- **A string keeps the language it was written in.** Log lines, action-result chips, the tray tooltip and startup dialogs are point-in-time text: each is rendered once from whatever the current language was at that moment and is never re-translated afterwards. Switching language changes what comes next, not what is already on screen —— that is the invariant, not a bug. Which is also why `set_language` re-sets the tray tooltip through `tray::refresh_tooltip` instead of pretending the existing one can follow along.
- `scripts/validate.py` checks key parity, that each key's `{placeholder}` set matches across
  languages, **and** the reference relation in both directions: a cited key must exist, and an
  existing key must be cited somewhere. The citation scan includes the frontend on purpose — before
  it did, a typo in `t("wrong.key", "fallback")` rendered the fallback and nothing complained, and
  the same blind spot is what lets dead keys pile up after their UI is gone.
  The scan counts a dotted literal as a key only when its first segment is one of the namespaces above
  (which is what keeps the editor's data-bind paths, `"then.network.mode"`, out of the results);
  backtick-quoted text is ignored entirely, because the frontend uses backticks for bind paths and
  Rust doc comments use them for identifiers.
- Group [11] asks the opposite question: *is there copy that never entered the dictionary at all?* Four mechanical rules — prose in static markup must hang on a `data-i18n` element; the markup fallback must equal `en.json`; a multi-word string literal inside `<script>` must sit in a `t()`/`tf()` argument position; backend `log::*` / `win_dialog::*` calls must take `i18n::t`/`tf`, and no CJK literal may sit in Rust code. `i18n-exempt` (a `//` comment in Rust, an HTML comment that covers the following 8 lines) is how a file says *this is deliberately not copy*: the language options in settings show their own names (日本語 stays 日本語), the macOS code matches the Chinese hardware-port label a zh-CN system prints, and an `.expect(...)` message is for whoever reads the panic rather than for the UI.
  Honest limitation: a **single** English word written bare in JS (`"Loading"`) is indistinguishable from an identifier, so that one case is not caught mechanically; multi-word prose is.

### 10.2 Logging (`src-tauri/src/log.rs`)

- Path: the **user log dir** from `paths::user_log_dir()` — `~/Library/Logs/NetSense` (macOS) · `%LOCALAPPDATA%\NetSense\logs` (Windows) · `$XDG_STATE_HOME/netsense/logs`, defaulting to `~/.local/state/netsense/logs` (Linux). File name `netsense-YYYY-MM-DD.log`, **daily rotation**，保留天数由软件配置决定（默认 7 天，可设 1–365，见 §10.4），清理发生在启动时、跨天时、以及用户改动保留天数的那一刻。
- A dir is only chosen if a write probe succeeds (`log::is_writable`), so a read-only location can't swallow the logs silently; the fallback is `<temp>/NetSense/logs`, and if even that fails `init()` returns `false` and everything degrades to stderr.
- Global `OnceLock<Mutex<Logger>>` serializes writes, thread-safe; four levels `error/warn/info/debug`, and all four are written (the level is a line label, not a filter).
- Logs before `init()` degrade to stderr, so early-startup failures are not lost.
- `init(dir, true, days)`: 第二个参数是 `to_stderr`（dev 为 `true`，终端里看得见；release 可为 `false`），第三个是保留天数。保留天数的后续变更走 `set_retention_days`，它立刻按新窗口清一次 —— 用户把 30 天改成 3 天时，期待的是「现在就少一些」，不是「下次启动再说」。

### 10.3 Config hot-reload (`engine.rs::reload_if_changed`)

- The **engine thread** compares `config.json`'s mtime at the head of every loop iteration (on wake, on
  event, on the shortest pending poll timer); on change it reloads + `validate()` + collects `warnings()`.
- Validation failure (including "schema ≠ 1") → log `notify.config_invalid` and **keep the old config**,
  so a hand-edited typo cannot break the network.
- After a successful reload the scheduler's per-Profile timers are reset and the warnings are re-published
  into the engine view; the next pass re-evaluates everything. Whether 3A is re-pushed is decided by the
  **content fingerprint** (`Engine::is_blocked` / `applied_fp`): if the Profile's config didn't change, a
  re-evaluation keeps Active without touching the NIC and **without re-running 3B1**. If it did change, the engine logs `engine.reconfigure` and re-applies network
  only. 界面语言不是这个文件的字段（它在 `settings.json`），所以「改菜单语言结果把 IP 重新下发了一遍」这条路径
  在结构上就不存在，而不是靠 reload 里记得跳过某个字段。

### 10.4 Software settings (`src-tauri/src/appconfig.rs`)

`config.json` 决定网络怎么配，`settings.json` 决定这个应用怎么表现。两者的分界不是「哪些字段碰巧放在
哪里」，而是一句判据：**改了它会改变自动化行为吗？** 不会的（语言、日志保留天数）才进软件配置。
这条分界换来两个可验证的性质：编辑器保存不会改掉用户的界面语言，换语言也不会让引擎 Wake 重评估一次。

- **位置**：`paths::settings_path()`，即用户配置目录下的 `settings.json`。与 `config_path()` 的差别是刻意的 ——
  这里**没有**「可执行文件同级」那一档。那份兜底是为「仓库里带一份自动化配置跑一跑」准备的；语言与保留天数是
  这台机器上这个用户的偏好，从源码树里带一份出来，等于让仓库替每个用户决定他的界面语言。
- **读取时机**：主流程最开始（在 `log::init` 之前），因为日志要用它里面的保留天数；读不出来时不终止进程，
  用默认值起界面并记一条 `app.settings_failed`。文件不存在 = 全部默认值；格式错误 = 一条会写进日志的 `Err`。
- **只有两个字段**是故意的。第三个字段该不该进来，先问上面那句判据。
- **开机启动不在文件里**：它的真相在操作系统里 —— macOS 是 `~/Library/LaunchAgents/com.netsense.app.plist`
  （`RunAtLoad`），Linux 是 `$XDG_CONFIG_HOME/autostart/netsense.desktop`（变量未设时即
  `~/.config/autostart/netsense.desktop`，桌面环境只按这个变量找条目），Windows 是
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的一个值。存一份布尔到 `settings.json` 只会造出
  两个会说不同话的真相来源（用户可以在系统设置里把它关掉）。所以勾选框每次显示时都**问一次系统**：
  读文件存在性，或跑一次 `reg.exe query`。问不出来时给 `autostart: false` + `autostart_error`，界面因此
  不会显示一个假的关闭态还不给解释。生效后移是这一功能的定义而非缺陷：下次登录起生效，界面上写清楚。
- **平台分支用运行时的 `std::env::consts::OS`**，不是 `#[cfg]`：三条分支因此在同一台 macOS 上就能
  编译、lint、测试（含生成的 plist / `.desktop` / 注册表参数这些纯函数），而 `NetworkPlatform` trait 不必
  为这一个功能长出一个方法。注册表值里的引号一律剥掉，路径不能把自己的值闭合出去再拼参数。

---

## 11. Build & Run

Three ways, pick one for your scenario. **If you just want a runnable package fast, use method three (CI) — zero local dependencies.**

| Method | Command | Prereq | Use for |
|--------|---------|--------|---------|
| 1. Local binary | `scripts/build-windows.ps1` (Win) / `cargo build --release` (mac/linux) | see table below | daily dev |
| 2. Local installer | `cargo tauri build` | additionally needs tauri-cli | need msi/dmg/AppImage |
| 3. Cloud build | push `v*` tag or manually trigger workflow | just a GitHub repo | **just want the artifact to test** |

### Per-platform prerequisites

| Platform | Required | Notes |
|----------|----------|-------|
| Windows | Rust(msvc) + **MSVC C++ Build Tools** + WebView2 | no MSVC substitute; check "Desktop development with C++", ~3–6 GB; another 5–10 GB on first build |
| macOS | Rust + Xcode CLT | WebView is the system's built-in WKWebView |
| Linux | Rust + `libwebkit2gtk-4.1-dev` `libgtk-3-dev` `librsvg2-dev` `libayatana-appindicator3-dev` `patchelf` | |

### Windows (recommended to use the script — it preflights before building)

```powershell
# preflight + bare exe (fastest; artifact at src-tauri\target\release\netsense.exe)
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1

# msi / nsis installer
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer

# debug build with a console window (easier to see logs)
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Debug
```

The script checks Rust / MSVC / Windows SDK / WebView2 / free disk in order and tells you exactly what to install, instead of letting you hit an unreadable `LNK1104` at link time.

### macOS / Linux

```bash
cargo build --release                 # bare executable
cargo tauri build                     # installer (needs tauri-cli)

# macOS first-time use: install the passwordless privilege channel (removes per-change auth dialog), needs sudo
sh scripts/install-priv-helper.sh
```

### Cloud build (CI)

`.github/workflows/build.yml` is a four-target matrix: `windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`.

```bash
git tag v1.0.0 && git push origin v1.0.0     # validate -> release(draft) -> build x4 -> checksums -> publish
gh workflow run build.yml --ref main         # compile + test only; never touches a Release
```

**A `main` push compiles nothing** — the workflow listens on `workflow_dispatch` and `push: tags: v*` only. So after changing Rust (or adding a unit test), verify with the dispatched run above: it runs clippy and `cargo test` on all four targets and uploads each platform's bare executable as an artifact, while every release-creating step is gated on `refs/tags/` and is skipped.

Inside the flow it first runs `python3 scripts/validate.py` (JSON / i18n parity / PAL boundary / trait coverage / docs alignment) and `node scripts/editor-smoke.mjs` (the editor's data binding, headlessly). Then each build leg lints itself (`cargo clippy --all-targets -- -D warnings`, §12 — per-leg because a macOS runner never compiles the Windows or Linux PAL code) and runs `cargo test` (config schema, condition evaluation, detection cadence, engine decision, 3A readback, one-shot scheduling, PAL utils, i18n parity, plus the editor's recorded payloads replayed through `Config::validate()`); only after those pass does it compile and package.

> A dispatched run's artifacts are the **bare executables**, not installers: bundling is done by
> `tauri-action`, whose step is gated on `refs/tags/` (§13.2) and is therefore *skipped* on a dispatched
> run, while the separate "upload bare executable" step (`if: always()`) still publishes what `cargo
> build` produced. Use a dispatched run to prove compilation and unit tests, not to obtain an installer.

### First run

The **editor window opens at launch** — the unambiguous "the app actually started" signal. Closing it (`CloseRequested` → `prevent_close` + `hide`) returns the app to the tray *without* quitting; only the panel's **Quit** button really exits (`request_quit` → `QUITTING`).
Afterwards the app lives in the tray: **click the icon** (either button) toggles the popup panel, which carries every entry point - software settings, logs, DHCP, probe, update, quit.
On Windows a fresh tray icon usually starts in the **"hidden icons" overflow** (`^`) — pin it there if you want it always visible.
On Windows the installers do not bundle the WebView2 runtime (`bundle.windows.webviewInstallMode = skip`), which keeps `setup.exe` down to a few MB. Windows 10 1803+ and Windows 11 already ship it; if it is genuinely missing, the app shows a dialog with the official download link rather than exiting silently.
两份文件都在**用户配置目录**里。自动化配置 `config.json`：由 `paths::config_path()` 解析 —— 先看用户配置目录那份
（macOS `~/Library/Application Support/NetSense/config.json`、Windows `%APPDATA%\NetSense\config.json`、
Linux `$XDG_CONFIG_HOME/netsense/config.json`，默认 `~/.config/netsense/`），那里还没有文件时退到**可执行文件同级**那份
（开发期，或手工放进去的种子），两处都没有时仍返回用户目录那条路径，好让编辑器的第一次保存把目录建出来。
软件配置 `settings.json`：只有用户目录这一档，**没有** exe 同级兜底（见 §10.4）。
按用户存放意味着重装不掉配置，而签过名的 macOS `.app` 与只读的 `Program Files` 都不该被写入。把 `config.example.json` 复制一份改名即用。
Logs: the **user log dir** (`paths::user_log_dir()`, see §10.2) when it is writable, otherwise `<temp>/NetSense/logs`. `scripts/` is resolved against the directory that actually holds the chosen `config.json`, so the README's "relative path = next to config.json" stays true wherever the config lives. The panel's **Logs** button opens the log window, and that window prints the directory it is really reading —— 目录可能被后端换过（用户目录不可写时退到临时目录），界面自己拼的那个路径会说谎。 A panic hook writes panics into that same file, so a silent exit is never silent any more.

> **Windows asks UAC once per app session** (the first apply spawns the resident elevated helper, §9.6 "Four key Windows backend designs" rule 3; later batches ride the pipe). To remove that prompt too: launch once as administrator (after that the in-process privilege channel shows "no authorization needed"). Elevation for user scripts still asks every run by design.

---

## 12. Testing

- **Platform-independent, run anytime**: `python3 scripts/validate.py`
  11 checks: JSON validity of the 5 dictionaries, key parity, `{placeholder}` parity, i18n key references
  in **both** directions (a cited key must exist; an existing key must be cited — see §10.1), the PAL
  boundary (no concrete platform type outside `platform*`), trait
  coverage on all three platforms (by method name), `tauri.conf.json` field sanity, version consistency
  across VERSION / `tauri.conf.json` / `Cargo.toml` / the CHANGELOG's first numbered section (§13),
  **docs ↔ code** (§3's repository tree against the disk in both directions, every repo path a doc
  quotes, the command table in `frontend/README.md` against `main.rs`, event names both ways),
  **docs ↔ docs** (every number a doc states — key count, command count, language count, check count —
  equals its source, and the five README / CHANGELOG language copies stay structurally identical with
  their technical tokens intact) and **UI text** (every string the interface can render comes from the
  dictionary: a `data-i18n` reference must resolve, an unbound prose node is a failure, the English
  markup must equal `en.json` byte for byte, backend `log::*` / `win_dialog::*` calls must not carry a
  literal, and no CJK literal may sit in backend source — both rules accept an `i18n-exempt` marker).
  Each check group is worth 10 points and the run prints a total out of
  100; CI treats anything below full marks as a failure.
- **Unit**: `cargo test` — all pure std (no tokio, no real system commands); the exact count differs
  slightly per OS because a handful of PAL tests are `#[cfg]`-gated, so it is deliberately not pinned here:
  `config` (example config loads & validates, an unrecognised schema is rejected with an actionable
  message, serde tags the editor
  emits, validation rules — including action payloads and health timing — and the else-branch persistent
  warning),
  `conditions::evaluator` (Rules OR / enabled-Conditions AND / disabled Profile still reports rule states /
  MAC normalization / interface condition checks **all** live NICs / `evaluate_all` collects every match
  without choosing), `conditions::identity` (fingerprint notices gateway & interface changes),
  `detection` (per-Profile delay isolation, events-only vs events+polling vs polling-only cadence,
  first pass always due, timer reset),
  `engine::decide` (0 / 1 / 2+ → `no_active_profile` / `active` / `conflict`, **conflict never picks a
  winner**, stable tagged serialization),
  `engine` run attribution + **layering** (a `Partial`/`Failed` batch leaves Active untouched, only a 3A
  failure clears Active and yields ERROR, and an ERROR never bleeds into a Conflict on another Profile),
  `network` + `network::readback` (Failed carries the reason that blocks 3B; each readback expectation
  including "cleared DNS must not be compared against DHCP servers"),
  `automation` (allow-list trust rules incl. which directory a relative script path is anchored to,
  batch grouping, and what each action reports on the UI — label text, and the printer action being
  budgeted like a local handoff rather than like a script),
  `automation::provider` (an up tunnel issues no connect, labels/budgets come from the action, a script
  outside the allow-list is reported rather than run),
  `automation::persistent` (the self-overlap gate opens on both the panic and the finish path, a stop ends
  the sliced sleep, one report per state change, a superseded generation is not attributed, start order),
  `platform` (MAC parsing, `printers_from_lpstat` — one default marked, and that default taken from the
  text after the last colon rather than from an English label, so a localized CUPS cannot silently read
  as "no default" — `sh_q` shell-escaping
  round-trip, the op list a `NetworkConfig` compiles to — an absent `dns` key emits no DNS operation
  while `dns: ""` still emits the clear), `update` (https-only urls; an occupied temp name failing
  instead of being reused; a same-named symlink not being followed; the private file / dir / script
  being ours with no group or other bits; and `SHA256SUMS` parsing across GNU, BSD and binary lines —
  fail-closed hangs on that last one), `ipc` (the check-time install verdict: a Homebrew install
  needs no checksums at all, while a missing asset — or a checksum link that is absent, blank or not
  https — is refused before a request leaves the process), `i18n` (parity / fallback chain / placeholder substitution).
- **Frontend**: `scripts/editor-smoke.mjs` loads `editor.html`'s script into a `vm` context with a stubbed
  DOM and a fake `window.__TAURI__`, then drives render → input → click and asserts the binding rules that
  are invisible to every other layer (absent ≠ empty for the tri-state `dns`, disabled actions must drop out
  of the pre-apply plan while equal-priority ones collapse into one batch, a status broadcast must not rebuild
  the form under the user's cursor). `--write-fixtures <path>` additionally dumps the exact
  `save_profile` / `save_global` payloads it sent; CI runs the assertions in the `validate` job and
  re-runs the script in each build leg to produce that dump, which the config test
  `config::tests::payloads_the_editor_actually_sends_are_the_ones_serde_accepts` replays through the
  same upsert + `validate()` path `ipc.rs` uses. That receiving end is the point: serde accepts
  unknown fields and its action enums are internally tagged, so a mistyped `type`
  on the frontend breaks the whole config at load time instead of surfacing a validation warning.
- **Lints**: `cargo clippy --all-targets -- -D warnings`, run in **each build leg** rather than once in
  the `validate` job. Two reasons, both mechanical: the Windows / Linux PAL files and the `cfg(windows)`
  blocks inside shared modules are not part of a macOS build at all (same `#[cfg]` split as §4), so no
  single host can lint them; and `validate` has none of the system libraries needed to build the crate.
  `--all-targets` is what makes the check reach the `#[cfg(test)]` modules — historically about half of
  the warnings lived there.
- Integration (todo): mock PAL to verify the same chain **through the engine thread** rather than at the
  `Engine` method level — "match → 3A apply → verify fail → 3B never submitted → ERROR" and
  "health fallback → deactivate" without real system commands. The state-machine half of that is already
  pinned by the `engine` tests above; what is missing is the wiring (that `execute_branch` really skips
  `submit_one_shot` when 3A failed).

---

## 13. Release

- The `VERSION` file is the single source of truth, syncing each platform's packaging metadata (`Cargo.toml` / `tauri.conf.json`) via `scripts/bump_version.sh`.
- **Documentation exists in 5 languages, with English as the source**: `README.md` + `README.{zh,zh-TW,ja,ko}.md`,
  `CHANGELOG.md` + `CHANGELOG.{zh,zh-TW,ja,ko}.md`. Change the English file first and carry the same change into
  the four translations in the same edit round — a translated doc that lags behind is worse than no translation,
  because it reads as authoritative. (`validate.py` checks [9] and [10] now enforce the mechanically
  checkable half of this for the docs as well: the section structure of the five language copies, the
  technical tokens they must carry over verbatim, every number a doc states, and every repo path a doc
  quotes. What no script can judge is whether a translated paragraph still says what the English one
  means — that part stays a review task.)
  CI extracts the release notes from the tagged version's section of the **English** `CHANGELOG.md`, so the entry
  has to be numbered before the tag goes out. `scripts/validate.py` check [8] compares VERSION / tauri.conf.json /
  Cargo.toml against the CHANGELOG's first **numbered** section, and tolerates a `## [Unreleased]` top: it skips
  the comparison only when no numbered section exists at all, which never happens in a released state.
- **Commit-message red line**: `release: vX.Y.Z <brief>`, **must not** contain any AI tool name or credit.
  The token list the guard matches against lives in `scripts/check-no-ai.sh` and nowhere else — this doc
  deliberately does not reproduce it, since copying it here would put the very names the rule forbids into a
  published file.
- Pushing a `vX.Y.Z` tag triggers CI cross-platform build/sign/release (see §11).

### 13.1 Publish-hygiene guard (`scripts/check-no-ai.sh`)

Enforces the commit-message red line above mechanically instead of by discipline: it fails
if a commit message or a changed published file names an assistant tool (AI-IDE) or
contains the standalone token `AI`.

Two halves, both required:

| Half | File | Notes |
|---|---|---|
| Local (fast) | `scripts/git-hooks/pre-commit` + `scripts/git-hooks/commit-msg` | Enabled by `sh scripts/setup-hooks.sh`. **Two hooks, not one**: `pre-commit` is invoked with no arguments and scans the staged diff, while the message is only visible to `commit-msg`, which git calls with the message file as `$1`. A lone `pre-commit` that calls `check-no-ai.sh "$1"` passes an empty string and silently skips the message scan. `core.hooksPath` lives in `.git/config`, which is **not cloned** — a fresh clone has the hook *files* but zero enforcement until this is run. |
| CI (unskippable) | `.github/workflows/publish-hygiene.yml` | Runs the same script in `--ci <range>` mode on push to `main` and on PRs. The range is resolved to **exactly the pushed commits** (`github.event.before..github.sha`, or `<base>..HEAD` for a PR) — deliberately *not* a rolling lookback window, which would re-scan history predating the guard and stay red forever. |

Deliberate exemptions — each is load-bearing, and removing one either breaks the repo
or makes the guard useless:

- `.gitignore` / `.git/info/exclude` — listing assistant-tool directories there *is* the
  hygiene rule, not a leak (the block at the end of `.gitignore`).
- `DEVELOPMENT.md` — §13 states this rule, so it necessarily contains the standalone token the guard
  searches for. Without the exemption the guard blocks the commit that documents the guard.
- `scripts/check-no-ai.sh`, `scripts/git-hooks/*`, `scripts/setup-hooks.sh`,
  `.github/workflows/publish-hygiene.yml` — self-reference (`skip_file()`).
- `cursor` is excluded from the **content** scan only (it is a CSS property: all four
  `frontend/*.html` files contain `cursor: pointer`). It stays in the commit-message scan.
- The standalone-token scan strips the guard's **own filename** before matching
  (`SELF_REF`): `check-no-ai.sh` contains the literal `-ai.`, so without this every
  commit message that names the guard — including the one editing it — is rejected.
  Assistant-tool names are a separate rule and remain fully enforced.
- The commit that **introduced** the guard is exempted by SHA (`skip_commit`). Its
  message has to spell the rule out in prose (it says "the standalone token AI" and
  names `cursor` while explaining the CSS false positive), so it fails its own scan.
  It is immutable history, so skipping it hides nothing new — but without the
  exemption any `--ci` range reaching back past the guard is red permanently.
  Rewriting history so that commit is unreachable is safe in the other direction: the
  SHA simply never matches and the exemption goes inert.

The scan is **path- and SHA-addressed, not time-addressed**: `--ci` takes an explicit
range, and no default path re-scans history. Keep it that way — a fixed lookback window
(`HEAD~20..HEAD`) is the one change that silently makes the CI half unfixable.

After editing that list, re-verify against the whole tree — a false positive here blocks
every future commit:

```bash
sh scripts/check-no-ai.sh --ci                # range defaults to origin/main..HEAD, else HEAD~1..HEAD
sh scripts/check-no-ai.sh --ci HEAD~5..HEAD   # wide range, reaches past the guard -> still passes
git log --format=%B | grep -Eic "workbuddy|codebuddy|tencent|trae|cursor|claude|copilot"
# and confirm the message half still fires, naming the guard on purpose:
git commit --allow-empty -m "test: touches scripts/check-no-ai.sh"   # must pass
git commit --allow-empty -m "test: let AI do it"                     # must be blocked
```

---

### 13.2 Release job: exactly one draft per tag, addressed by id

`build.yml` is `validate → release → build ×4 → checksums → publish`. The `release` job
creates the Draft Release **once**, before the matrix, and exports its numeric id; every
build leg passes it to `tauri-action` as `releaseId`. The final `publish` job PATCHes that
same release to `draft=false`, and only runs when `checksums` verified the assets it is
about to make downloadable.

Two traps this structure exists to avoid — both were hit for real while cutting a release:

1. **`tauri-action` upserts by tag, which is not safe under a parallel matrix.** It only
   looks up or creates a release when `releaseId` is absent (`if (tagName && !releaseId)`
   in `src/index.ts`). The four legs belong to a *single run*, and the workflow-level
   `concurrency` group only serialises whole runs — so it never protected the legs from
   each other. In the first run, two legs each found no release and each created
   one: **two drafts on one tag**, with the installers split across them (both macOS dmgs
   on one, `msi` + `setup.exe` + `deb` on the other). `gh release view <tag>` shows only
   one of the two, so the rest of the artifacts simply look missing. Passing `releaseId`
   skips the lookup branch entirely.
2. **A draft release is bound to a placeholder tag.** Its `html_url` reads
   `releases/tag/untagged-<hash>` until it is published, so
   `GET /repos/{owner}/{repo}/releases/tags/{tag}` returns **404 while it is still a
   draft** (and `target_commitish` silently falls back to the default branch). The second
   run died exactly there: the release was created fine, then the follow-up lookup
   404'd, the `release` job went red, and the whole build matrix was skipped. The job
   therefore takes the id from the **POST /releases response** and never re-looks it up by
   tag. Resolving a *published* release by tag does work — that is what `cask.yml` relies
   on, and it is triggered by `release: published`.

The `release` job also clears stale drafts for the same tag first, so re-running a tag
self-heals any duplicates, and it **refuses to proceed when the tag already has a
published release** — re-pushing a released tag should fail loudly rather than silently
rewrite published assets and hashes.

> To debug a release-leg failure, read the job log (`gh run view <run-id> --log`) rather
> than the release page. The release page shows a draft under a placeholder tag, which
> makes a correctly-created draft look broken.

**The Homebrew cask is a separate workflow** (`.github/workflows/cask.yml`), triggered by
`release: published` rather than by the tag. The reason is the 404 above: a Draft Release's
assets are not reachable at `releases/download/<tag>/…`, so a cask bumped at tag-push time
would leave `brew install --cask netsense` broken for the whole window between the tag and
the publish click. Since `publish` now auto-publishes once checksums land, the cask follows
without a manual step. Prerelease tags are excluded, and the job no-ops when
`secrets.TAP_PUSH_TOKEN` is absent.

---

## 14. Roadmap

**Shipped** — everything §1–§13 describes is in the binary: the tray shell, popup panel and editor;
the PAL split into `platform/{macos,windows,linux}.rs` on top of the system's own authorization; the
config model and condition engine; per-Profile detection cadence; 3A Apply → routes → readback
Verify; the 0/1/2+ decision with Conflict; 3B1 priority batches running off the engine thread with a
per-action timeout and a structured `last_run`; 3B2 desired-state workers; the four-target CI matrix
and in-app upgrade.

**Not implemented**, roughly in the order that makes each one useful:

| Item | Content |
|------|---------|
| 3A **rollback** | restore the previous working configuration instead of the blanket DHCP fallback (§7) |
| More condition kinds | IPv4 / gateway / connectivity / VPN state beyond what `conditions/` matches today |
| Stronger privilege isolation | if the passwordless sudoers / UAC channel is not enough: a **signed** helper (SMJobBless-style), see §9.2 — Windows already has the plain unsigned elevated helper (§9.6 rule 3), macOS / Linux are the open half |
| Code signing / notarization | unsigned builds cost every user an accept-the-warning step on each OS |
| Signed release checksums | `SHA256SUMS` travels the same HTTPS path as the asset it describes, so it proves integrity, not provenance; an Ed25519/minisign signature checked against a key baked into the app would (§9.3) |
| Linux beyond NetworkManager | a systemd-networkd / pure-`ip` backend; the current Linux PAL assumes NetworkManager is in charge |

---

## 15. Glossary

The project's own vocabulary, mostly short labels that carry load-bearing semantics:

| Term | Means |
|------|-------|
| **Profile** | A named bundle of detection + rules + a 3A network configuration + 3B actions, with `enabled` and `quick` (the tray panel's one-click switch). |
| **Rule / Condition** | Rules are ORed; the **enabled** Conditions inside one Rule are ANDed. Each has its own `enabled`, and disabled never counts as a match. |
| **No Active Profile / Active / Conflict** | The 0 / 1 / 2+ outcome of `decide()`. Conflict applies nothing and names the candidates — there is no tie-break. |
| **ERROR** | The *execution* verdict: 3A failed. Distinct from Conflict, which is a *condition*-layer verdict. |
| **3A** | The network configuration itself: IP / mask / gateway / DNS / IPv6 / static routes. |
| **3B1 / 3B2** | One-shot actions (priority batches, run once per entry into Active) vs. persistent workers (desired state, maintained while Active). |
| **Apply → Verify** | The 3A barrier: write the configuration, then read it back from the OS. Exit codes are not evidence. |
| **Tick / `TickOutcome`** | One check of a persistent action's desired state: `Satisfied` (zero commands issued), `Repaired`, `Faulted(reason)`. |
| **generation** | The monotonic id of a worker session; reports from a superseded session are dropped rather than attributed. |
| **PAL** | Platform Abstraction Layer — `platform.rs` (trait) + `platform/{macos,windows,linux}.rs`. The only place that names system commands. |
| **`PrivChannel` / `Direct` / `Prompt`** | The two privilege channels, in `platform.rs`: `Direct` is already passwordless (sudoers allow-list, an elevated process, `sudo -n`), `Prompt` asks the system per authorization. |
| **`PrivOp` / `WinOp`** | The structured privileged-operation enums each platform batches before elevating (macOS / Windows; Linux pushes `nmcli` argument vectors instead). Elevating once per batch is what keeps one write from meaning several dialogs. |
| **`EngineView`** | The broadcast snapshot of the engine: state, active, conflict, snapshot, per-Profile verdicts, `last_run`, `workers`, warnings. |
| **`applied_fp`** | Content + network fingerprint of the last applied configuration; blocks a re-apply loop, and is deliberately cleared after a health fallback. |

---

## 16. Design Decision Record (confirmed baseline)

- Tech stack: **Tauri v2 (Rust)**, not Wails.
- **One Active Profile at a time.** Profiles have **no priority** and there is no tie-break: 0 matches →
  No Active Profile (+ top-level `fallback`), 1 → Active, 2+ → **Conflict**, and Conflict applies nothing
  and tells the user. "Pick the most specific" is deliberately not the rule — it would silently push one
  network's static config while another network's rules also matched.
- Conditions: **Rules OR, enabled Conditions AND**, four kinds (`network_interface` /
  `gateway_mac` / `wifi_ssid` / `bssid`), each Rule/Condition/Action independently `enabled`.
  **Disabled ≠ wildcard**, and "no effective condition" means never-matching, not always-matching.
- `ELSE` belongs to the Profile being processed; a Profile that simply didn't match never runs its ELSE.
- 3A network configuration is a **hard barrier** in front of 3B, verified by **readback** rather than
  exit code. Health probe (`icmp`/`http`/`both`, `both` needs double failure) is the time-extension of
  that same verification and is bound to the Active Profile's lifecycle.
- **Only 3A can raise ERROR.** A 3B1 batch that ends `Partial`/`Failed` leaves the Profile Active and is
  written to `last_run`; a 3A failure clears Active, marks the Profile ERROR, and memoises the
  (config, network) pair so the engine stops re-prompting. The two must stay separable because the
  remedies differ: "this environment's conditions/config are wrong" versus "one thing inside an
  environment that is correctly in place didn't happen".
- 3B is split: **3B1 one-shot** (priority batches, same priority concurrent, failure doesn't block later
  batches, runs once per entry into Active) and **3B2 persistent** (desired-state workers, started only
  with Active's THEN branch, priority only orders worker *start*). They are never mixed in one list:
  one array cannot express "run this once when we arrive" and "keep this true while we are here"
  without a flag that changes the meaning of every other field in it.
- Static routes are part of 3A, not an automation action.
- "Apply now" re-evaluates that Profile's own conditions — it is a *trigger*, not a bypass.
- `run_script` is allow-listed (`<config dir>/scripts` + `allowed_scripts`), a global security boundary
  rather than a per-Profile field.
- **No automatic config migration.** `schema` must be 1; a missing or different value fails loudly with
  the reason. A file this version cannot read might mean the opposite for multi-match, and guessing that
  semantics is how a static IP ends up on a NIC nobody confirmed.

---

## 17. Implementation Notes

### Verification status

| Layer | Covered by | Still open |
|---|---|---|
| Config model, conditions, detection, engine decisions, 3A readback expectations, 3B1 scheduling, 3B2 workers, PAL utils, i18n | `cargo test` (pure std — no tokio, no real system commands), §12 | the PAL commands themselves, against real adapters |
| Docs ↔ code ↔ docs, i18n parity, PAL boundary, version consistency, UI text sourcing | `scripts/validate.py` (11 scored checks) | nothing mechanical: whether a translated paragraph still *means* the English one stays a review task (§13) |
| Editor data binding, and the payloads it sends | `scripts/editor-smoke.mjs` + the fixture replay test (§12) | rendering in a real WebView |
| Rust lints that `cargo check` cannot see | `cargo clippy --all-targets -- -D warnings` in every build leg (§12) | a Windows- or Linux-only lint — a macOS host never compiles that code, which is why the gate is per-leg instead of once |
| Compile + package on four targets | CI — a dispatched run proves compilation, a tag produces installers (§11) | — |

What no automated gate can reach, and therefore what needs a real machine per OS:

- **Three-platform real-device passes**: the engine loop, 3A readback verification (is the settle window
  long enough on a slow switch?), the conflict flow, the 3B2 tunnel control — including whether
  `scutil --nc start` brings a macOS tunnel up without the elevation channel we deliberately refuse to
  use — and the 3B1 printer action: `list_printers` / `set_default_printer` are built on commands whose
  output and exit codes were captured and checked by hand (`lpstat -e`, `lpstat -d`, `lpoptions -d` on a
  throwaway `HOME`), but the write was never run against real printers, because that would change the
  user's own default as a side effect of a test.
- **Popup panel**: keyboard focus / the blur-collapse rule and Retina coordinate conversion (§9.5).
- **Privilege channel**: a first install of `scripts/install-priv-helper.sh`, and the UAC prompt count for
  a non-admin Windows user — with the helper it should be **one per GUI session** (first apply asks; later
  applies ride the pipe), one per batch if the helper was declined or died, still one per run for `elevated` user scripts.
- **Integration tests**: mock the PAL and run the 3A-fail chain through the engine thread itself, so the
  wiring — that `execute_branch` really skips the 3B submission when 3A failed — is pinned and not just
  the state machine (§12).

### Where the obvious first approach was rejected

- `probe.http` uses `curl` (system `curl.exe` on Windows) instead of reqwest (avoid new dependency); ICMP uses system `ping` (macOS BSD `-t` seconds / Windows `-w` ms / Linux `-W` seconds).
- **A module named `core` collides with the std crate `core`** (Rust 2018 uniform paths, E0659). The modules are named by the question each answers instead: `conditions` / `detection` / `network` / `automation`, all under `crate::`.
- **One provider trait per action kind** would give three traits with one implementation each. One `Tick` trait plus the `provider::tick_for` factory covers the same seam (§8).
- `InterfaceStatus` gains `Serialize` (IPC return needs JSON) and `gateway_mac` / `netmask` / `bssid` / `iface` fields.
- After health fallback, the `applied_fp` fingerprint is cleared: otherwise later hot-reloads would skip push due to "content unchanged", leaving the network stuck on DHCP.
- **Privilege model uses the system's built-in authorization** (macOS sudoers allow-list / Windows UAC / Linux sudo+pkexec); only Windows adds a resident elevated helper (§9.6 rule 3) — macOS / Linux stay daemon-free per §9.2 (narrower scope, no socket-auth surface).
- `watch_ssid` is adaptive polling (2s/5s) on all three platforms, **not event-driven**: macOS's CoreWLAN notifications need objc2 FFI and handling `CWInterface` notification object lifetime, Windows's `WlanRegisterNotification` likewise needs FFI; the payoff (skipping a subprocess every 2–5s) doesn't justify the complexity, deferred.
  The polling logic is extracted into shared `poll_ssid_watch`; switching to event-driven later only requires changing each platform's `watch_ssid` in one place.
- The frontend has no build tooling (no Vite/Svelte), still pure static HTML; `invoke` depends on `withGlobalTauri`.
  **Side effect (good)**: CI needs no Node at all, shorter build chain.

---

## 18. Build Environment Notes (why some environments can't build locally)

Tauri on Windows **officially requires the MSVC toolchain + Windows SDK** (the docs say "no substitute"), macOS packages must be compiled on macOS, and Linux needs the webkit2gtk dev library. So producing "one set of three-platform installers" inherently needs three environments.

Preflight results measured on a Windows machine (for you to judge which path to take):

| Check | Measured | Impact |
|-------|----------|--------|
| Rust toolchain | not installed (no `~/.cargo` / `~/.rustup`) | install first, ~0.5–1 GB |
| MSVC linker | **missing** (`VS2019` dir is an empty shell, `vswhere` finds no VC tools) | hard prereq for Tauri Windows build; install Visual Studio Build Tools (~3–6 GB) |
| Alternative | system has TDM-GCC (`x86_64-w64-mingw32`), can use `x86_64-pc-windows-gnu` | unofficial path, `webview2-com-sys` etc. risky under gnu |
| Disk | C: **164 MB** left, D: 2.8 GB, E: 4.0 GB | **decisive constraint**: after installing MSVC + Rust then compiling (first target ~2–5 GB) it will certainly fill |
| Network | crates.io / rust-lang.org / rsproxy all reachable | download isn't the bottleneck, disk is |

**Conclusion**: disk space can't fit the MSVC toolchain, so a local Windows installer is impossible. Feasible paths in recommended order:

1. **CI build (preferred)**: push the repo to GitHub, `git tag v1.0.0 && git push origin v1.0.0`, and `.github/workflows/build.yml` produces windows-x64 / macos-arm64 / macos-x64 / linux-x64 executables and installers on the cloud matrix. **Zero local dependency, no disk used, get all three platforms at once.**
2. **Local build**: first free ~10 GB, install Visual Studio Build Tools (check "Desktop development with C++") and Rust, then run `scripts\build-windows.ps1` (the script does the preflight for you).
3. **Verify code compiles only**: install Rust's `x86_64-pc-windows-gnu` target — `cargo check` **needs no linker**, only `windres` (TDM-GCC ships it), so `cargo check --target x86_64-pc-windows-gnu` validates types and dependency resolution but produces no executable. See `scripts/sandbox-bootstrap.sh`.
