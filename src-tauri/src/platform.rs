//! 平台抽象层（PAL）— 统一契约，三平台各自独立实现。
//!
//! ```text
//! platform.rs            本文件：trait 契约 + 共享类型 + 共享工具 + 编译期平台选择
//! platform/macos.rs      networksetup / arp / airport / route / scutil / osascript
//! platform/windows.rs    netsh / PowerShell(CIM) / route / rasdial / wireguard.exe / UAC 提权
//! platform/linux.rs      nmcli / ip / pkexec
//! ```
//!
//! 上层（`main` / `ipc` / `core`）只依赖 [`Platform`]（编译期选定的实现）
//! 与 [`NetworkPlatform`] trait，不直接接触任何系统命令 —— 这是跨平台的关键边界。

use crate::config::NetworkConfig;
use serde::Serialize;
use std::process::Command;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
compile_error!("NetSense 仅支持 macOS / Windows / Linux 三个平台"); // i18n-exempt: 编译期消息，只有构建者看得到，永远不会渲染进界面

/// 网卡/网络状态快照。会被 IPC 直接 JSON 化返回给前端，故需 `Serialize`。
#[derive(Debug, Clone, Default, Serialize)]
pub struct InterfaceStatus {
    pub connected: bool,
    pub ssid: Option<String>,
    /// 信号强度（dBm）。Windows 由百分比换算；Linux 取 nmcli SIGNAL。
    pub rssi: Option<i32>,
    pub ipv4: Option<String>,
    pub netmask: Option<String>,
    pub gateway: Option<String>,
    /// 网关 MAC（默认网关 IP → ARP/邻居表解析），独立匹配条件之一
    pub gateway_mac: Option<String>,
    /// 当前 AP 的 BSSID，独立匹配条件之一
    pub bssid: Option<String>,
    pub dns: Option<String>,
    pub v6mode: Option<String>,
    /// 当前无线接口名（macOS: en0 / Windows: Wi-Fi / Linux: wlan0）
    pub iface: Option<String>,
}

/// 网卡种类。用于面板与设置窗口「网络硬件信息」列的分组展示。
///
/// 序列化成小写（`wired` / `wireless` / `vpn` / `other`），前端按字符串直接分组。
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NicKind {
    Wired,
    Wireless,
    Vpn,
    #[default]
    Other,
}

/// 单张网卡的快照。与 [`InterfaceStatus`]（"主无线网卡"视角）互补：
/// 这里回答的是"这台机器上**同时**连着哪些网"，面板的两段网卡列表与设置窗口的硬件列都基于它。
///
/// 字段取不到一律为 `None` —— 前端按缺省显示「—」，不允许用占位文本造假值。
#[derive(Debug, Clone, Default, Serialize)]
pub struct NicInfo {
    /// 设备名（macOS: en0 / utun4；Windows: Wi-Fi / Ethernet0；Linux: wlan0 / tailscale0）
    pub name: String,
    /// 人类可读名：macOS 的网络服务名 / Windows 的适配器描述 / Linux 的连接名
    pub label: Option<String>,
    pub kind: NicKind,
    /// 是否已启用（有 IPv4 或已关联无线即视为在用）
    pub up: bool,
    /// 无线网卡当前关联的 SSID
    pub ssid: Option<String>,
    /// 网卡自身 MAC
    pub mac: Option<String>,
    pub ipv4: Option<String>,
    pub netmask: Option<String>,
    /// 该网卡上的**全局** IPv6 地址（不含 `fe80::` 链路本地地址：它每台机器都长一样，
    /// 既没有识别价值，也不代表这台机器真的能走 IPv6）。取不到即为 `None`。
    pub ipv6: Option<String>,
    /// 该网卡上的默认网关（多网卡时只有走默认路由的那张有值）
    pub gateway: Option<String>,
    /// 该网关 IP 对应的 MAC
    pub gateway_mac: Option<String>,
    pub dns: Option<String>,
    /// VPN 归属软件名（WireGuard / Tailscale / AnyConnect …）；非 VPN 为 None
    pub app: Option<String>,
}

/// NIC 列表缓存 TTL。
///
/// `list_interfaces()` 一次要拉起若干子进程（macOS 上每个设备两次 `ipconfig`），
/// 而面板每次开合都要一份快照、SSID 监视线程每 2~5s 也会间接触发一次 ——
/// 不缓存会把后台线程长时间钉在等子进程上（与 `system_profiler` 缓存同理）。
const NIC_LIST_TTL: Duration = Duration::from_secs(2);

/// [`NIC_LIST_TTL`] 缓存里存的那一条：采样时刻 + 当时的网卡列表。
type NicCacheEntry = (Instant, Vec<NicInfo>);

/// `list_interfaces()` 结果的进程级 TTL 缓存（三平台共用，见 [`NIC_LIST_TTL`]）。
///
/// 传进来的 `sample` 只在缓存缺失/过期时才被调用，因此各平台实现可以放心地在里面
/// 做 I/O。
pub(crate) fn cached_nics<F: FnOnce() -> Vec<NicInfo>>(sample: F) -> Vec<NicInfo> {
    static C: OnceLock<Mutex<Option<NicCacheEntry>>> = OnceLock::new();
    let cell = C.get_or_init(|| Mutex::new(None));
    {
        let guard = cell.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, list)) = guard.as_ref() {
            if at.elapsed() < NIC_LIST_TTL {
                return list.clone();
            }
        }
    }
    let fresh = sample();
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some((Instant::now(), fresh.clone()));
    fresh
}

/// [`cached_status`] 的 TTL，取值与 [`NIC_LIST_TTL`] 一致（同一类快照、同一类刷新频率）。
const STATUS_TTL: Duration = Duration::from_secs(2);

/// [`cached_status`] 与 [`invalidate_status`] 共用的那一个缓存。
///
/// 必须是**模块级** static：函数内各自 `static C` 是两个互不相干的存储，
/// 那样 `invalidate_status()` 会清掉一份没人读的空缓存，而下发后读回校验照旧读到
/// 旧快照 —— 3A 屏障静默失效，这比不加缓存更糟。
static STATUS_CACHE: OnceLock<Mutex<Option<(Instant, InterfaceStatus)>>> = OnceLock::new();

/// [`NetworkPlatform::get_status`] 结果的进程级 TTL 缓存，理由与 [`cached_nics`] 相同：
/// 一次快照要拉起若干系统子进程，而面板每次刷新、SSID 监视每一轮都要一份。
///
/// ⚠️ 任何**改动本机网络**的路径都必须调 [`invalidate_status`]，否则 3A 的
/// 「下发 → 读回校验」会读到下发之前那份快照 —— 校验屏障会当场失效（读回的值永远是
/// 旧的那份，既可能假通过，也可能假失败）。
#[allow(dead_code)] // 目前只有 macOS 走它；Windows 有自己的 status_cached
pub(crate) fn cached_status<F: FnOnce() -> InterfaceStatus>(sample: F) -> InterfaceStatus {
    let cell = STATUS_CACHE.get_or_init(|| Mutex::new(None));
    {
        let guard = cell.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, st)) = guard.as_ref() {
            if at.elapsed() < STATUS_TTL {
                return st.clone();
            }
        }
    }
    let fresh = sample();
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some((Instant::now(), fresh.clone()));
    fresh
}

/// 丢掉 [`cached_status`] 里的快照（网络被本进程改动之后）。
#[allow(dead_code)] // 调用方同上
pub(crate) fn invalidate_status() {
    if let Some(cell) = STATUS_CACHE.get() {
        *cell.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// 网卡名是否属于典型的隧道/VPN 设备。
///
/// 三平台命名不同（macOS `utun*` / `ppp*` / `ipsec*`；Linux `tun*` / `tap*` /
/// `tailscale*` / `wg*`；Windows 靠适配器描述），但 POSIX 侧的设备名前缀是稳定的，
/// 因此这个判据放在共享层，避免三份实现各写一遍还写不一致。
#[allow(dead_code)] // Windows 那一侧走描述而非设备名，故只有 macOS/Linux 调它
pub(crate) fn is_tunnel_device(dev: &str) -> bool {
    let d = dev.to_ascii_lowercase();
    ["utun", "ppp", "ipsec", "tun", "tap", "wg", "zt", "nordlynx", "tailscale", "sstp"]
        .iter()
        .any(|p| d.starts_with(p))
}

/// [`guess_vpn_app`] 的关键字表。**顺序就是优先级**：兜底的 `"vpn"` 必须留在最后，
/// 否则 `Tailscale Tunnel` 会被认成一台面目模糊的「VPN」。
pub(crate) const VPN_APP_TABLE: &[(&str, &str)] = &[
    ("tailscale", "Tailscale"),
    ("wireguard", "WireGuard"),
    ("anyconnect", "Cisco AnyConnect"),
    ("openvpn", "OpenVPN"),
    ("tunnelblick", "Tunnelblick"),
    ("forti", "FortiClient"),
    ("globalprotect", "GlobalProtect"),
    ("palo alto", "GlobalProtect"),
    ("nordlynx", "NordVPN"),
    ("nordvpn", "NordVPN"),
    ("expressvpn", "ExpressVPN"),
    ("surfshark", "Surfshark"),
    ("proton", "Proton VPN"),
    ("zerotier", "ZeroTier"),
    ("softether", "SoftEther"),
    ("check point", "Check Point"),
    ("sonicwall", "SonicWall"),
    ("pptp", "PPTP"),
    ("l2tp", "L2TP"),
    ("ikev2", "IKEv2"),
    ("ipsec", "IPsec"),
    ("clash", "Clash"),
    ("sing-box", "sing-box"),
    ("singbox", "sing-box"),
    ("mihomo", "Mihomo"),
    ("vpn", "VPN"),
];

/// 从一段自由文本里猜 VPN 软件名（Windows 适配器描述 / macOS 服务名 / nmcli 连接名通用）。
///
/// 命中即返回**产品名**（统一大小写，便于展示），未命中返回 `None`，由调用方退回
/// 更泛的兜底名（如设备名）。
pub(crate) fn guess_vpn_app(text: &str) -> Option<&'static str> {
    let s = text.to_ascii_lowercase();
    for (needle, name) in VPN_APP_TABLE {
        if s.contains(needle) {
            return Some(name);
        }
    }
    None
}

/// 一条「要维持连接」的隧道（3B2 常驻动作的目标）。
///
/// 为什么是一个枚举而不是两个 trait 方法：各平台对 WireGuard 隧道的处理方式和 VPN
/// 拨号几乎没有差别（都是「找到那条隧道，没连上就把它连上」），差别只在**去哪儿找它**。
/// 把差别收进数据里，平台实现就只需要一套清单匹配逻辑。
///
/// ⚠️ `name` 是**用户在自己那套 VPN 软件里看到的隧道名**，不是本应用 `list_interfaces()`
/// 里的设备名（macOS 上是 `utun4`，用户配的是「办公室 WireGuard」）。拿设备名去匹配
/// 用户写的名字，结果永远是「找不到」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelTarget {
    /// WireGuard 隧道（配置名 / 接口名）
    WireGuard { tunnel: String },
    /// 第三方 VPN 客户端里的一条连接。`provider` = 厂商标识（tailscale / proton / …），
    /// `profile` = 该客户端里的连接名。
    Vpn { provider: String, profile: String },
}

impl TunnelTarget {
    /// 平台侧用来定位这条隧道的那个名字。
    pub fn name(&self) -> &str {
        match self {
            TunnelTarget::WireGuard { tunnel } => tunnel,
            TunnelTarget::Vpn { profile, .. } => profile,
        }
    }

    /// 厂商限定；WireGuard 没有厂商概念，返回 `None`。
    ///
    /// 空串按 `None` 处理：allow-list 之外，用户从「还没填完」的编辑器里保存出来的
    /// provider 可能恰好是空的，那不该让所有隧道都匹配不上。
    pub fn provider(&self) -> Option<&str> {
        match self {
            TunnelTarget::WireGuard { .. } => None,
            TunnelTarget::Vpn { provider, .. } => {
                let p = provider.trim();
                (!p.is_empty()).then_some(p)
            }
        }
    }
}

/// 隧道名是否就是目标名：大小写与首尾空白不敏感，允许清单里带包裹引号
/// （macOS `scutil --nc list` 把用户可见标签写成 `"Tailscale"`）。
pub(crate) fn tunnel_name_eq(candidate: &str, want: &str) -> bool {
    let unquote = |s: &str| {
        let t = s.trim();
        for q in ['"', '\''] {
            if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
                return t[1..t.len() - 1].to_ascii_lowercase();
            }
        }
        t.to_ascii_lowercase()
    };
    let (a, b) = (unquote(candidate), unquote(want));
    !b.is_empty() && a == b
}

/// `provider` 只做**包含**匹配，且是对整行文本（不是隧道名）匹配。
///
/// 为什么不做精确比较：厂商信息在各平台落在完全不同的字段里 —— macOS 是 bundle id
/// （`io.tailscale.ipn.macsys`）、Windows 是适配器描述（"Tailscale Tunnel"）、
/// Linux 是连接类型与前缀。要求用户逐字命中任一平台的写法，等于让同一份配置
/// 只能在一台机器上跑对。
pub(crate) fn provider_in(provider: Option<&str>, row: &str) -> bool {
    let Some(p) = provider else {
        return true;
    };
    row.to_ascii_lowercase().contains(&p.trim().to_ascii_lowercase())
}

/// 清单里命中目标隧道的那一行，以及**厂商提示是否也对得上**。
///
/// 为什么要带这个标记而不是直接把厂商做成硬过滤：nmcli 的连接清单里根本没有厂商标识
/// （只有 `wireguard` / `vpn` 这种类型），拿厂商去过滤会让 Linux 上什么都匹配不到；
/// 而 macOS 与 Windows 看得见厂商，此时「名字对但厂商错」就是用户配错了，必须能报出来。
/// 于是共享层只负责「找到那一行 + 告我厂商合不合」，严格与否由各家平台决定。
// 今天只有 macOS 走这条（Windows / Linux 的清单各有各的行结构，还没有共同形状），
// 因此在另两个平台上是 dead_code；留给它们接上时复用，而不是各写一遍匹配顺序。
#[allow(dead_code)]
pub(crate) struct TunnelRow<'a> {
    pub text: &'a str,
    pub provider_matched: bool,
}

/// 从候选清单里挑出目标隧道那一行。`rows` 每项是 `(该行代表的隧道名, 原始整行文本)`。
///
/// 先要名字整段相等（见 [`tunnel_name_eq`]）：清单里同时有「VPN」和「VPN 办公室」时，
/// 模糊匹配会连错那条。名字命中多条时优先取厂商也对得上的那条。
#[allow(dead_code)] // 调用方同上：目前只有 macOS
pub(crate) fn find_tunnel_row<'a>(
    rows: &'a [(String, String)],
    target: &TunnelTarget,
) -> Option<TunnelRow<'a>> {
    let by_name = |n: &str| tunnel_name_eq(n, target.name());
    let provider = target.provider();
    rows.iter()
        .find(|(n, text)| by_name(n) && provider_in(provider, text))
        .map(|(_, text)| TunnelRow { text, provider_matched: true })
        .or_else(|| {
            rows.iter()
                .find(|(n, _)| by_name(n))
                .map(|(_, text)| TunnelRow { text, provider_matched: false })
        })
}

#[derive(Debug, Clone)]
pub struct ProbeTarget {
    pub mode: crate::config::ProbeMode,
    pub http_target: Option<String>,
    pub icmp_target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    Fail,
}

/// SSID 监视句柄：`stop()` 后后台线程会在下一轮退出。
pub struct WatcherHandle {
    stop: Arc<AtomicBool>,
}

impl WatcherHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// 特权执行通道（跨平台统一语义，各平台自行映射到本地机制）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivChannel {
    /// 已具备提权能力，操作不弹授权框。
    /// macOS: `/etc/sudoers.d/netsense` 授权的白名单脚本（NOPASSWD）
    /// Windows: 进程本身以管理员身份运行
    /// Linux: `sudo -n` 可用
    Direct,
    /// 每次操作需系统授权。
    /// macOS: `osascript ... with administrator privileges`
    /// Windows: UAC (`Start-Process -Verb RunAs`)
    /// Linux: `pkexec`
    Prompt,
}

impl PrivChannel {
    pub fn code(self) -> &'static str {
        match self {
            PrivChannel::Direct => "direct",
            PrivChannel::Prompt => "prompt",
        }
    }
}

/// 一台打印机的快照：名字，以及它现在是不是默认打印机。
///
/// `name` 是**系统里那台打印机的队列名**（macOS/Linux 的 CUPS 目的地名、Windows 的
/// `Win32_Printer.Name`），也正是 `set_default_printer` 要写回配置的那个值 —— 中间不留
/// 第二套标识（显示名 / 驱动名 / 设备 URI），否则「界面上选的」和「下发时找的」就不是
/// 同一个东西了。
///
/// `info` 只给人看：系统给这台队列起的说明与位置（CUPS 的 Description/Location、Windows
/// 的 Comment/Location）。用 IP 命名的队列（`_10_20_20_30`）在这里才变成人话；取不到就是
/// `None`，界面回落到队列名 —— 名字永远是对的，说明可能没有。
#[derive(Debug, Clone, Serialize)]
pub struct PrinterInfo {
    pub name: String,
    pub info: Option<String>,
    pub is_default: bool,
}

/// PAL 契约：Core Engine 只依赖此 trait，不直接碰系统命令。
pub trait NetworkPlatform: Send + Sync {
    fn watch_ssid(&self, cb: Box<dyn Fn(Option<String>) + Send + Sync>) -> WatcherHandle;
    fn get_current_ssid(&self) -> Option<String>;
    fn get_status(&self) -> InterfaceStatus;

    /// 供 3A 读回校验使用的快照：必须是**这一秒**的实际情况，不能是缓存。
    ///
    /// 默认实现直接走 [`get_status`]：本来就不缓存的平台没有可绕的东西。做了缓存的平台
    /// 必须覆盖它 —— 校验循环每 800ms 采样一次，而快照的 TTL 比这个间隔长，不覆盖的话
    /// 四次采样里会有两次拿到同一份快照，屏障还在（值确实是下发之后的），只是网卡慢慢
    /// 稳定下来的情形会更早被判成失败。
    fn fresh_status(&self) -> InterfaceStatus {
        self.get_status()
    }

    /// 下发网络配置（IP/掩码/网关/DNS/IPv6）。
    ///
    /// 参数是 **3A 的 `NetworkConfig`**，不是整个 Profile：「下发」这一层一旦看得见
    /// 匹配条件与自动化动作，跨层依赖就是这么长出来的。
    /// `dns` 是**三态**的：缺失 = 不产生任何 DNS 操作，空串 = 清空、交回系统自动获取。
    /// 三份实现都要在编译操作列表时守住这条，否则编辑器的「保持不变」会变成清空。
    /// 静态路由不在这里 —— 它由 `network::apply_3a` 在配置下发之后单独下发，
    /// 好让「配置失败」和「路由失败」在 3A 里分别归因。
    fn apply_network(&self, p: &NetworkConfig) -> Result<(), String>;
    /// 回落保底：切回 DHCP + 清空自定义 DNS。
    ///
    /// 这里的清空是**故意的**：保底路径要的是一个「什么都不再固定」的状态，
    /// 不受上面那条三态约定约束。
    fn set_dhcp(&self) -> Result<(), String>;

    /// 枚举**当前在用**的全部网卡（有线 / 无线 / VPN），每张一张 [`NicInfo`]。
    ///
    /// 与 `get_status()` 的分工：`get_status` 只回答"主无线网卡现在什么参数"（供
    /// profile 匹配），本方法回答"这台机器同时连着哪些网"（供面板与设置窗口展示）。
    /// 实现必须经 [`cached_nics`] 包一层 —— 调用方（面板开合、状态广播）频率很高。
    fn list_interfaces(&self) -> Vec<NicInfo>;

    /// 把**指定设备名**的网卡切回 DHCP。
    ///
    /// 默认实现委派给 [`NetworkPlatform::set_dhcp`]（主无线网卡语义）；能按设备名定位
    /// 网络服务/连接的平台（macOS 网络服务、Windows 适配器、nmcli 连接）应覆盖它 ——
    /// 否则用户在以太网或 VPN 上点「设为 DHCP」时，改动会落到另一张网卡上。
    fn set_dhcp_for(&self, dev: &str) -> Result<(), String> {
        let _ = dev;
        self.set_dhcp()
    }


    // 扩展2：健康度监测（Fail = 判定网络不可达）
    fn probe(&self, target: &ProbeTarget, timeout_ms: u64) -> Health;

    /// 这条隧道现在是否已连上（3B2 常驻动作的「检查」半边）。
    ///
    /// 返回 `false` 同时涵盖「没连上」与「本机根本没有这条隧道」—— 两者的区别只在
    /// 报错文本里有意义，而那个报错由 [`NetworkPlatform::tunnel_connect`] 给出，
    /// 所以这里不返回三态：多一个取值就多一条前端要区分却区分不了的分支。
    fn tunnel_is_up(&self, target: &TunnelTarget) -> bool;

    /// 把这条隧道连上（3B2 的「恢复」半边）。
    ///
    /// ⚠️ **只能使用无需交互授权的通道**：worker 会按 `interval_secs` 反复调用它，
    /// 一旦这里走了提权（macOS osascript 授权框 / Windows UAC / Linux pkexec），
    /// 用户就变成每 N 秒被授权框打断一次 —— 那正是 3A 那边专门记档避免的事。
    /// 因此权限不够时**照实返回 Err**，让错误停在界面上，由用户决定怎么办；
    /// 不要为了「让它成功」而偷偷提权。
    fn tunnel_connect(&self, target: &TunnelTarget) -> Result<(), String>;

    // 扩展3：自动化任务
    fn add_route(&self, dest: &str, gw: &str, metric: u32) -> Result<(), String>;
    fn delete_route(&self, dest: &str) -> Result<(), String>;
    /// 启动一个应用。**契约是「丢出去」，不是「跑完了」**：
    ///
    /// - `Ok(())` = 启动器接受了这个请求（macOS `open` / Windows `Start-Process` /
    ///   Linux 直接 `spawn` 或 `xdg-open`），**不**保证应用还活着，也不保证它存在过 ——
    ///   应用随后自己崩掉，本方法无从得知，动作照样记成功。这是这个动作能给的的全部信息。
    /// - 因此实现**绝不能等应用退出**：浏览器可以开一下午，等到超时会被记成
    ///   「启动失败」，而用户看到的是应用好好地开着。
    /// - `Err` 留给启动器自己就拒绝的情况：找不到该应用、没有执行位、参数根本没法传
    ///   （Linux：非可执行文件走 `xdg-open`，而它无法向应用传参）。
    /// - `args` 是**传给应用**的参数。macOS 必须把它们放在 `--args` 之后 —— `open -a App X`
    ///   里的 X 是「要用该应用打开的文件」。
    fn launch_app(&self, app: &str, args: &[String]) -> Result<(), String>;
    /// 执行一个脚本，**等它结束**并以其退出码定成败（与上面的"丢出去"相反 —— 脚本的
    /// 结果就是我们要的东西）。受 allow-list 约束：调用方（automation 层）先校验
    /// [`crate::automation::AllowedScripts`] 再进本方法，平台层不重复判、也不该放开。
    /// 传进来的 `path` 已经是 `resolve_script` 定过基准的路径，且与通过校验的那条是同一条 ——
    /// 平台层不要再自己解释相对路径（各平台的 CWD 语义不同，会被校验形同虚设）。
    ///
    /// `elevated: true` 必须经由**系统自己的**授权框（macOS osascript / Windows UAC /
    /// Linux sudo-n→pkexec），一次动作只弹一次：这是有意与网络配置那条免密白名单通道
    /// 区别对待的安全决策，不要为了少弹一个框而把它并进免密通道。
    fn run_script(&self, path: &str, args: &[String], elevated: bool) -> Result<(), String>;

    /// 本机打印机清单，含「现在哪台是默认」（3B1 `set_default_printer` 的候选来源）。
    ///
    /// 空列表**不是**错误：这台机器可能没有打印机，也可能那套打印子系统没在跑。界面据此
    /// 显示「没有候选，请手输」；真正下发时的成败由 [`NetworkPlatform::set_default_printer`]
    /// 说话。取不到时不要把「取不到」伪装成「没有」以外的任何东西 —— 但也别为它弹框。
    fn list_printers(&self) -> Vec<PrinterInfo>;

    /// 把 named 打印机设为**当前用户**的默认打印机。
    ///
    /// ⚠️ 三个平台都只动用户级默认（macOS/Linux 写 CUPS 的 `~/.cups/lpoptions`，Windows 调
    /// `Win32_Printer.SetDefaultPrinter`），都**不需要**管理员。系统级默认要提权，而这个动作
    /// 每次进入 Active 都会跑一遍 —— 提权等于每换一次网络弹一次授权框，与
    /// [`NetworkPlatform::tunnel_connect`] 那条是同一个理由。名字在本机不存在时照实 `Err`。
    fn set_default_printer(&self, printer: &str) -> Result<(), String>;

    /// 已保存的无线网络列表（编辑器下拉填充）。平台不支持时返回 `None`。
    fn list_known_ssids(&self) -> Option<Vec<String>>;
}

// —————————————————————————— 共享工具 ——————————————————————————

/// 普通执行（无需提权），返回 stdout 文本，失败返回 stderr 文本。
pub(crate) fn run(program: &str, args: &[&str]) -> Result<String, String> {
    run_env(program, args, &[])
}

/// 调 CUPS 客户端时要钉住的环境变量（见 [`run_env`] 与 [`printers_from_lpstat`]）。
///
/// 只给 CUPS 那几条命令，不做成 `run` 的默认行为：`networksetup`、`ipconfig`、`ifconfig`
/// 的输出形状本来就和 locale 无关，而 `arp -a` 这类我们要解析**本地化表头**的命令，
/// 钉成 C 反而会改变它的输出。
#[allow(dead_code)] // Windows 那一腿走 CIM，不叫 lpstat
pub(crate) const C_LOCALE: [(&str, &str); 2] = [("LANG", "C"), ("LC_ALL", "C")];

/// [`run`] 的带环境变量版本。
///
/// 只有一个用途：解析**结构文本**的子命令必须先把 locale 钉住。CUPS 客户端（`lpstat`）
/// 会把整句消息翻译成系统语言，Linux 上装了语言包就连 `printer`/`Description` 这些关键字
/// 都不是英文了，解析规则会随用户界面语言而变化 —— 加两个环境变量换一条规则，值得。
pub(crate) fn run_env(program: &str, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
    let mut cmd = Command::new(program);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.args(args);
    // Windows：GUI 程序（windows_subsystem="windows"）拉起 powershell/netsh/arp/ping/curl 等
    // 控制台子系统子进程时，若不隐藏窗口，Windows 会为子进程分配一个**可见控制台窗口**，
    // 表现为“启动后弹出 PowerShell 黑窗 + 标题栏按钮”。CREATE_NO_WINDOW 让子进程无窗口运行，
    // 输出仍可被 .output() 正常捕获。UAC 提权路径（run_elevated_ps）的 outer/inner 进程同样受益。
    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let out = cmd
        .output()
        .map_err(|e| {
            crate::i18n::tf("pal.spawn_failed", &[("program", program), ("error", &e.to_string())])
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            crate::i18n::tf("pal.cmd_failed", &[
                ("program", program),
                ("code", &out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into())),
            ])
        } else {
            err
        })
    }
}

/// 以 `String` 参数调用（参数来自配置，需要 owned 形态）。
// 目前只有 Linux 的 launch_app / run_script 走这条（macOS 与 Windows 各自用 spawn /
// PowerShell 传参），因此在其它平台上会报 dead_code。
#[allow(dead_code)]
pub(crate) fn run_owned(program: &str, args: &[String]) -> Result<String, String> {
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run(program, &refs)
}

/// 通用 SSID 轮询监视（三平台共用）：未连接 2s / 已连接 5s 自适应，
/// 分片睡眠保证 `stop()` 后最多 500ms 退出；首轮只建基线不回调
/// （启动时的首次应用由 `main.rs` 负责，避免重复触发 `on_apply`）。
pub(crate) fn poll_ssid_watch<F>(
    get: F,
    cb: Box<dyn Fn(Option<String>) + Send + Sync>,
) -> WatcherHandle
where
    F: Fn() -> Option<String> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    std::thread::spawn(move || {
        let mut last: Option<String> = None;
        let mut initialized = false;
        while !stop_thread.load(Ordering::SeqCst) {
            let cur = get();
            if !initialized {
                initialized = true;
                last = cur;
            } else if cur != last {
                last = cur.clone();
                cb(cur);
            }
            let secs = if last.is_none() { 2 } else { 5 };
            let mut ticks = 0;
            while ticks < secs * 2 && !stop_thread.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(500));
                ticks += 1;
            }
        }
    });
    WatcherHandle { stop }
}

/// 解析 `"Key: value"` 形式的行，返回 value。
// 只有 macOS 的命令输出是这个形状（`ipconfig getpacket` / `networksetup`），
// 因此在 Windows / Linux 上是 dead_code。
#[allow(dead_code)]
pub(crate) fn parse_kv(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(key) {
            let rest = rest.trim_start_matches(':').trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// 由 CUPS 的三条命令输出拼出打印机清单（macOS 与 Linux 共用同一套客户端命令）。
///
/// - `long_out` —— `lpstat -l -p`：一台打印机一段，段首 `printer <队列名> is <状态> …`，
///   段内缩进的 `Description:` / `Location:` 就是那台机器的说明与位置。这里是**唯一**能
///   拿到人话标签的来源。
/// - `names` —— `lpstat -e`：一行一个目的地名，没有任何标签文字。只当作降级备份用（见下）。
/// - `default_out` —— `lpstat -d`：形如 `system default destination: HP_Office`。这里取
///   **最后一个冒号之后**的那段，而不是去匹配那句英文：`lpstat` 的消息会被 CUPS 翻译成
///   系统本地语言，而目的地名本身不含冒号。没有默认打印机时 CUPS 打印的是
///   「no system default destination」这类句子（不含冒号），于是取到 `None` —— 正好是对的。
///
/// 为什么主清单是 `-l -p` 而不是 `-e`：`-e` 列的是调度器知道的**所有目的地**，里面混着
/// DNS-SD 浏览出来的临时队列（在 `lpstat -l -e` 里类型是 `network`，`-p` 根本不列它），
/// 于是界面上会出现一台系统设置里没有、选中也下不去的打印机。`-p` 那一批才是用户自己
/// 配置的队列，与系统设置显示的是同一批。
///
/// `names` 的备份不能省：万一 `-l -p` 的段首没解析出任何东西（输出形状被改版、被本地化
/// 成我们不认的样子），退回旧清单只是标签少了人话，不至于把面板变成一个空下拉。
#[allow(dead_code)] // Windows 那一腿走 CIM，不解析 lpstat
pub(crate) fn printers_from_lpstat(long_out: &str, names: &str, default_out: &str) -> Vec<PrinterInfo> {
    let def = lpstat_default(default_out);
    let mut list = lpstat_long_printers(long_out);
    if list.is_empty() {
        list = names
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|name| PrinterInfo {
                name: name.to_string(),
                info: None,
                is_default: false,
            })
            .collect();
    }
    for p in &mut list {
        p.is_default = def.as_deref() == Some(p.name.as_str());
    }
    list
}

/// 解析 `lpstat -l -p` 的长格式：队列名 + 人话标签（说明 · 位置）。
///
/// 段首靠「第一个词是 printer/class、第二个词是队列名、后面跟 `is`」来认，缩进行才装
/// 说明与位置 —— 段首不缩进，所以缩进本身就是「这行属于上一台」的信号。
/// 关键字按英文匹配（调用方已用 `run_env` 钉住 C locale），值本身可以是任何语言。
#[allow(dead_code)] // 同上
fn lpstat_long_printers(long_out: &str) -> Vec<PrinterInfo> {
    #[derive(Default)]
    struct Row {
        name: String,
        description: String,
        location: String,
    }

    /// 说明与位置拼成人话标签；两句都和队列名重复时干脆不给（`None` → 界面用队列名）。
    fn push_row(row: Row, out: &mut Vec<PrinterInfo>) {
        out.push(PrinterInfo {
            info: printer_label(&row.name, &row.description, &row.location),
            name: row.name,
            is_default: false,
        });
    }

    let mut out: Vec<PrinterInfo> = Vec::new();
    let mut cur: Option<Row> = None;
    for line in long_out.lines() {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            if let Some(r) = cur.take() {
                push_row(r, &mut out);
            }
            // 段首：`printer HP_Office is idle.  enabled since …` / `class Office is idle.`
            let mut w = line.split_whitespace();
            let kind = w.next().unwrap_or("");
            let name = w.next().unwrap_or("");
            let verb = w.next().unwrap_or("");
            if (kind.eq_ignore_ascii_case("printer") || kind.eq_ignore_ascii_case("class"))
                && !name.is_empty()
                && verb.eq_ignore_ascii_case("is")
            {
                cur = Some(Row {
                    name: name.to_string(),
                    ..Default::default()
                });
            }
            continue;
        }
        // 缩进行属于上一台；`Description:` / `Location:` 之后可能什么都没有（CUPS 允许空值）
        let Some(row) = cur.as_mut() else { continue };
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "description" => row.description = v.to_string(),
            "location" => row.location = v.to_string(),
            _ => {}
        }
    }
    if let Some(r) = cur.take() {
        push_row(r, &mut out);
    }
    out
}

/// 把系统给打印机写的两句备注（说明、位置）拼成界面上那一行。三平台共用。
///
/// 规则只有一条：**标签要认得出是哪台机器**。
/// - 说明与队列名重复时（CUPS 新建队列常把队列名原样填进说明）丢掉说明；但同一台的
///   位置（`XSMS`）仍然有用，于是留下 `XSMS`。
/// - 说明**根本没有**、只有位置时不能只亮位置 —— 那会让用户把一台机器认成另一台
///   （Windows 上被误认成「别的系统的打印机」就是这么来的），于是队列名顶上：
///   `\\filesrv\Lobby · 前台`。
/// - 一点新信息都没有（说明重复且无位置，或两句都空）时返回 `None`，界面回落队列名。
pub(crate) fn printer_label(name: &str, description: &str, location: &str) -> Option<String> {
    let d = description.trim();
    let l = location.trim();
    let mut parts: Vec<&str> = Vec::new();
    if !d.is_empty() {
        if d != name {
            parts.push(d);
        }
    } else if !l.is_empty() && l != name {
        // 没有说明、只有位置：补上队列名当主语，位置只是它的注脚
        parts.push(name);
    }
    if !l.is_empty() && !parts.contains(&l) {
        parts.push(l);
    }
    let only_name = parts.len() == 1 && parts[0] == name;
    (!parts.is_empty() && !only_name).then(|| parts.join(" · "))
}

/// 见 [`printers_from_lpstat`]：`lpstat -d` 那一行里的目的地名。
#[allow(dead_code)] // 调用方同上
fn lpstat_default(out: &str) -> Option<String> {
    let line = out.lines().find(|l| l.contains(':'))?;
    let tail = line.rsplit(':').next()?.trim();
    (!tail.is_empty()).then(|| tail.to_string())
}

/// 是否是 `aa:bb:cc:dd:ee:ff` / `aa-bb-cc-dd-ee-ff` 形态的 MAC（6 组双位十六进制）。
///
/// 两种分隔符都要认：Windows `arp -a` 打印 `aa-bb-cc-dd-ee-ff`，macOS/Linux 是
/// `aa:bb:cc:dd:ee:ff`，大小写也不统一。比较统一由 [`normalize_mac`] 归一
/// （见 DEVELOPMENT.md「Cross-platform matcher traps」）—— 因此这里只认冒号是错的：
/// 那会让 Windows 的网关 MAC 永远解析不出来（分隔符对不上，直接判非 MAC）。
pub(crate) fn is_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// 在任意文本行里找出第一个 MAC（`:` / `-` 分隔），原样返回其切片。
///
/// **必须按字节扫描，并且只在确认这 17 字节全是 ASCII 之后才构造 `&str`。**
///
/// 原实现直接 `&line[i..i + 17]`：`i` 一旦落在多字节字符内部就 panic
/// （`byte index N is not a char boundary`）。而这里收到的文本**不可信** ——
/// Windows 命令输出是 OEM 代码页（中文系统 CP936），经 [`run`] 的
/// `from_utf8_lossy` 解码后每个汉字都变成 3 字节的 `U+FFFD`（日志里那个 `�`），
/// 于是"逐字节前进"几乎立刻踩到非法边界：
/// 中文 Windows `arp -a` 的表头「接口: 192.168.1.10 --- 0x10」在 i=1 就崩，
/// 数据行（尾部「动态 / 静态」）在 i=30 崩。这就是 Windows 上
/// 装完即退的直接原因（见 DEVELOPMENT.md §9.6 Windows 第 4 条）。
pub(crate) fn extract_mac(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    if bytes.len() < 17 {
        return None;
    }
    for i in 0..=bytes.len() - 17 {
        let w = &bytes[i..i + 17];
        // 纯字节快筛：MAC 只由 ASCII 十六进制字符与分隔符组成，窗口里含任何
        // 非 ASCII 字节就不可能命中。这一步同时保证下面的 from_utf8 必然成功，
        // 从而**根本不会**出现落在字符中间的切片。
        if !w.is_ascii() {
            continue;
        }
        let s = match std::str::from_utf8(w) {
            Ok(s) => s,
            // 已确认全 ASCII，这里不可达；保守跳过而不是 unwrap
            Err(_) => continue,
        };
        if is_mac(s) {
            return Some(s.to_string());
        }
    }
    None
}

/// MAC 归一化：统一小写 + 冒号分隔，供匹配比较使用。
/// 各平台/各命令返回的 MAC 大小写与分隔符可能不一致（`AA-BB-...` / `aa:bb:...`）。
pub fn normalize_mac(s: &str) -> String {
    let hex: String = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if hex.len() != 12 {
        return s.trim().to_ascii_lowercase();
    }
    hex.as_bytes()
        .chunks(2)
        .map(|c| std::str::from_utf8(c).unwrap_or("").to_string())
        .collect::<Vec<_>>()
        .join(":")
}

/// POSIX 单引号包裹：把一段文本变成 shell 里的**字面量参数**。
///
/// 单引号内部除 `'` 本身外一切字符都不再被 shell 解释，因此 `;` `|` `&` `$()`
/// 反引号、换行等元字符会被中和。内部的单引号以 `'\''` 收尾再续 quoting。
///
/// 凡是**拼接进 shell 字符串**再交给 `sh -c` / `osascript do shell script` 执行的值，
/// 都必须过这个函数一个都不能漏。
// Windows / Linux 走 argv 直传不需要它，此处避免 dead_code 告警。
#[allow(dead_code)]
pub(crate) fn sh_q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 秒级超时（供 ping 等以秒为单位的命令使用，最小 1 秒）。
pub(crate) fn timeout_secs(ms: u64) -> String {
    ((ms.max(1000) / 1000).max(1)).to_string()
}

/// 用系统默认程序打开文件/目录/URL（编辑器"打开日志"等场景）。
#[cfg(target_os = "macos")]
pub fn open_path(path: &str) -> Result<(), String> {
    run("open", &[path]).map(|_| ())
}

#[cfg(target_os = "windows")]
pub fn open_path(path: &str) -> Result<(), String> {
    // explorer 是 Windows 上唯一无需额外依赖即可打开目录/文件的入口。
    // 注意 explorer 对成功常返回非 0 退出码，故只看 spawn 是否成功。
    Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| {
            crate::i18n::tf(
                "pal.open_path_failed",
                &[("path", path), ("error", &e.to_string())],
            )
        })
}

#[cfg(target_os = "linux")]
pub fn open_path(path: &str) -> Result<(), String> {
    run("xdg-open", &[path]).map(|_| ())
}

// —————————————————————————— WebView 渲染运行时 ——————————————————————————

/// WebView2 运行时下载页（Windows 专用；其它平台不会用到，故允许 dead_code）。
#[allow(dead_code)]
pub const WEBVIEW2_DOWNLOAD_URL: &str = "https://developer.microsoft.com/microsoft-edge/webview2/";

/// WebView2 **Evergreen Bootstrapper** 直链（点开即下载官方安装器，体积小、自动装对应架构）。
#[allow(dead_code)]
pub const WEBVIEW2_BOOTSTRAPPER_URL: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";

/// 本机是否具备 WebView 渲染运行时。
///
/// 只有 Windows 需要显式判断：WebView2 是**独立组件**，极少数精简/长期未更新的系统上
/// 可能缺失，此时 Tauri 创建 WebView 会失败 → 应用在启动阶段直接结束。
/// macOS 的 WKWebView 与 Linux 的 WebKitGTK 是系统库，缺了安装包本身就装不上，
/// 因此恒为 `true`。
#[cfg(target_os = "windows")]
pub use windows::webview2_available;

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)] // 只有 Windows 的启动检查会调用它
pub fn webview2_available() -> bool {
    true
}

/// 当前平台标识（日志 / UI / 诊断用）。
pub const fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

// —————————————————————————— 编译期平台选择 ——————————————————————————

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(crate) mod win_helper;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
pub use macos::{priv_channel, request_location_authorization, MacPlatform as Platform};
#[cfg(target_os = "windows")]
pub use windows::{priv_channel, WindowsPlatform as Platform};
#[cfg(target_os = "linux")]
pub use linux::{priv_channel, LinuxPlatform as Platform};

#[cfg(test)]
mod tests {
    use super::{printer_label, printers_from_lpstat, provider_in, sh_q, tunnel_name_eq, TunnelTarget};

    #[test]
    fn sh_q_neutralises_shell_metacharacters() {
        assert_eq!(sh_q("Wi-Fi"), "'Wi-Fi'");
        // 命令分隔符、管道、后台执行、命令替换、反引号都必须变成普通字符
        assert_eq!(sh_q("a;rm -rf /"), "'a;rm -rf /'");
        assert_eq!(sh_q("a | b"), "'a | b'");
        assert_eq!(sh_q("$(id)"), "'$(id)'");
        assert_eq!(sh_q("`id`"), "'`id`'");
        assert_eq!(sh_q("a && b"), "'a && b'");
        assert_eq!(sh_q("a\nb"), "'a\nb'");
    }

    #[test]
    fn sh_q_escapes_embedded_single_quote() {
        // cat's -> 'cat'\''s' ：关闭引号 → 转义一个引号 → 重新开引号
        assert_eq!(sh_q("cat's"), "'cat'\\''s'");
        // 空串与纯引号边界情况
        assert_eq!(sh_q(""), "''");
        assert_eq!(sh_q("'"), "''\\'''");
    }

    #[test]
    fn sh_q_roundtrip_keeps_value_intact() {
        // sh_q 只做包裹与转义，不得改变其它内容（含 Unicode）
        let probes = ["Wi-Fi", "192.168.1.1/24", "办公室 Wi-Fi", "a'b'c"];
        for p in probes {
            let wrapped = sh_q(p);
            // 去掉首尾包裹的单引号后，内部只允许出现 '\'' 这种转义序列
            let inner = &wrapped[1..wrapped.len() - 1];
            assert!(wrapped.starts_with('\'') && wrapped.ends_with('\''));
            assert!(!inner.ends_with('\\'));
            let restored = inner.replace("'\\''", "'");
            assert_eq!(restored, p, "roundtrip failed for {:?}", p);
        }
    }

    /// 回归：中文 Windows 上「装完即退」的真凶。
    ///
    /// `arp -a` 的接口表头「接口: 192.168.1.10 --- 0x10」经 `from_utf8_lossy`
    /// 解码后每个汉字成为 3 字节 `U+FFFD`（日志里那个 `�`）。旧实现
    /// `&line[i..i + 17]` 在 i=1 处即 panic：
    /// `byte index 1 is not a char boundary; it is inside '�' (bytes 0..3)`。
    /// 下面两个字符串就是当日实机抓到的形态（`\u{fffd}` 即 `�`）。
    #[test]
    fn extract_mac_survives_lossy_decoded_cjk() {
        // 中文接口表头：旧实现在 i=1 崩
        let header = "\u{fffd}\u{fffd}: 192.168.1.10 --- 0x10";
        assert_eq!(super::extract_mac(header), None);

        // 邻居表数据行：尾部「动态」+ 尾随空格，旧实现在 i=30 崩；
        // 修好后应取到短横线形式的 MAC —— Windows arp 就是这么打印的
        let row = "  192.168.1.1           e8-84-c6-93-ad-eb     \u{fffd}\u{fffd}        ";
        assert_eq!(
            super::extract_mac(row).as_deref(),
            Some("e8-84-c6-93-ad-eb")
        );

        // 整段多行文本一起扫也不得 panic（linux.rs 会把整份 `ip neigh` 输出传进来）
        let whole = format!("{}\n{}\n", header, row);
        assert_eq!(
            super::extract_mac(&whole).as_deref(),
            Some("e8-84-c6-93-ad-eb")
        );
    }

    /// 两种分隔符与大小写都必须认：只认冒号会让 Windows 的网关 MAC 永远为空。
    #[test]
    fn extract_mac_accepts_both_separators() {
        assert_eq!(
            super::extract_mac("    BSSID                  : e8:84:c6:93:ad:f3").as_deref(),
            Some("e8:84:c6:93:ad:f3")
        );
        assert_eq!(
            super::extract_mac("E8-84-C6-93-AD-EB").as_deref(),
            Some("E8-84-C6-93-AD-EB")
        );
        // 中文前缀（同样经 lossy 解码）+ 冒号形式
        assert_eq!(
            super::extract_mac("    \u{fffd}\u{fffd}\u{fffd} : 1c:bf:c0:e4:d2:39").as_deref(),
            Some("1c:bf:c0:e4:d2:39")
        );
    }

    #[test]
    fn extract_mac_rejects_non_mac_text() {
        for bad in [
            "",
            "192.168.1.1",
            "e8-84-c6-93-ad",                                    // 只有 5 组
            "e8:84:c6:93:ad:gj",                                 // 非十六进制
            "\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}",  // 纯 U+FFFD：旧实现必崩
        ] {
            assert_eq!(super::extract_mac(bad), None, "should not match: {:?}", bad);
        }
    }

    #[test]
    fn is_mac_accepts_colon_and_dash_only() {
        assert!(super::is_mac("e8:84:c6:93:ad:eb"));
        assert!(super::is_mac("E8-84-C6-93-AD-EB"));
        assert!(!super::is_mac("e8.84.c6.93.ad.eb"));
        assert!(!super::is_mac("e8:84:c6:93:ad"));
        assert!(!super::is_mac("e8:84:c6:93:ad:eb:ff"));
    }

    /// 隧道清单匹配：`scutil --nc list` 把用户可见标签写成带引号的形式，
    /// 而用户在配置里是不会打引号的 —— 两侧都得归一，否则合法配置永远匹配不上。
    #[test]
    fn tunnel_names_are_matched_case_insensitively_and_without_quotes() {
        assert!(tunnel_name_eq("\"Tailscale\"", "tailscale"));
        assert!(tunnel_name_eq("  Office-WG  ", "office-wg"));
        assert!(tunnel_name_eq("'quoted'", "quoted"));
        assert!(!tunnel_name_eq("Tailscale VPN", "tailscale"), "整段相等，不做包含匹配");
        // 空目标名不能变成「匹配一切」
        assert!(!tunnel_name_eq("anything", ""));
        assert!(!tunnel_name_eq("\"\"", "x"));
    }

    #[test]
    fn provider_is_a_loose_hint_on_the_whole_row() {
        let row = "* (Connected) 07CD VPN (io.tailscale.ipn.macsys) \"Tailscale\" [VPN:io.tailscale.ipn.macsys]";
        assert!(provider_in(None, row));
        assert!(provider_in(Some("Tailscale"), row));
        assert!(provider_in(Some("  tailscale "), row));
        assert!(!provider_in(Some("proton"), row));
    }

    /// `provider` 为空的 `keep_vpn_connected` 不该让所有隧道都匹配不上：
    /// 空厂商按「不加限定」处理。
    #[test]
    fn an_empty_provider_counts_as_no_provider() {
        let t = TunnelTarget::Vpn {
            provider: "   ".into(),
            profile: "Office".into(),
        };
        assert_eq!(t.provider(), None);
        assert!(provider_in(t.provider(), "anything at all"));
        assert_eq!(t.name(), "Office");
        let wg = TunnelTarget::WireGuard {
            tunnel: "office-wg".into(),
        };
        assert_eq!(wg.provider(), None);
        assert_eq!(wg.name(), "office-wg");
    }

    /// `lpstat -l -p` / `-e` / `-d` 的真实输出（macOS 上抓的，Linux 同形状）。
    /// 名字里带下划线与前缀点号是这台机器上真实存在的队列名，不是编出来的。
    ///
    /// 这条同时钉住两件事：`-e` 里那台 DNS-SD 浏览出来的 `HP_LaserJet_M104w_01502A_`
    /// 不该出现在清单上（系统设置里也没有它），以及 IP 命名的队列要靠 Description 说人话。
    #[test]
    fn the_cups_printer_list_drops_browsed_destinations_and_labels_the_rest() {
        let long = "\
printer _10_20_20_30 is idle.  enabled since Thu Sep 24 07:46:35 2026
\tForm mounted:
\tContent types: any
\tPrinter types: unknown
\tDescription: 10.20.20.30
\tAlerts: toner-low-warning
\tLocation: JYH
\tConnection: direct
printer _10_30_30_30 is idle.  enabled since Wed Aug 26 11:56:27 2026
\tDescription: 10.30.30.30
\tLocation: WJ
printer CanonG3860 is disabled.
\tDescription: CanonG3860
\tLocation: XSMS
";
        let names = "_10_20_20_30\n_10_30_30_30\nCanonG3860\nHP_LaserJet_M104w_01502A_\n";
        let list = printers_from_lpstat(
            long,
            names,
            "system default destination: CanonG3860\n",
        );
        let got: Vec<(&str, Option<&str>, bool)> = list
            .iter()
            .map(|p| (p.name.as_str(), p.info.as_deref(), p.is_default))
            .collect();
        assert_eq!(
            got,
            vec![
                ("_10_20_20_30", Some("10.20.20.30 · JYH"), false),
                ("_10_30_30_30", Some("10.30.30.30 · WJ"), false),
                // 说明就是把队列名重抄了一遍，于是只留位置； disabled 也照样在清单上
                // （用户要的正是「能选到它」，状态由下发时的成败说话）
                ("CanonG3860", Some("XSMS"), true),
            ]
        );
    }

    /// 长格式一行都没解析出来时（输出形状被改版），退回 `-e` 的裸名单：宁可少标签，
    /// 也不能把整个下拉变成空的。
    #[test]
    fn an_unparsable_long_listing_falls_back_to_the_plain_destination_names() {
        let list = printers_from_lpstat(
            "",
            "HP_Office\n",
            "system default destination: HP_Office\n",
        );
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "HP_Office");
        assert_eq!(list[0].info, None);
        assert!(list[0].is_default);
    }

    /// 没有默认打印机时 CUPS 打印的是「no system default destination」这一类句子。
    /// 它不含冒号，所以按「取最后一个冒号之后」的规则会得到 `None` —— 清单上一台都不该亮。
    #[test]
    fn an_absent_default_printer_lights_up_nothing() {
        for out in [
            "no system default destination\n",
            "system default destination: \n",
            "",
        ] {
            let list = printers_from_lpstat("printer HP is idle.\n", "HP\n", out);
            assert_eq!(list.len(), 1);
            assert!(!list[0].is_default, "默认行 {:?} 不该点亮任何打印机", out);
        }
    }

    /// 冒号前那句标签会被 CUPS 翻译，所以解析只能看冒号之后。
    /// 这条钉住的是**规则**（不匹配英文句子），标签文字本身只是示意。
    #[test]
    fn the_default_is_taken_from_after_the_colon_not_from_an_english_label() {
        let list = printers_from_lpstat(
            "printer Büro-HP is idle.\n",
            "Büro-HP\n",
            "Systemstandardziel: Büro-HP\n",
        );
        assert!(list[0].is_default);
        // 默认指向一台已经不在清单里的打印机时，没有人该亮（宁可少亮，不猜）
        let list = printers_from_lpstat(
            "printer Büro-HP is idle.\n",
            "Büro-HP\n",
            "system default destination: Gone\n",
        );
        assert!(!list[0].is_default);
    }

    /// 队列名 + 说明 + 位置 → 界面上那一行。三平台共用，所以规则只在这一处钉死：
    /// 标签要认得出是哪台机器 —— 重复的不算，缺说明时队列名补位，光秃秃的位置不当主语。
    #[test]
    fn a_printer_label_carries_only_what_the_queue_name_does_not_say() {
        assert_eq!(
            printer_label("HP_Office", "HP LaserJet in the office", "3F").as_deref(),
            Some("HP LaserJet in the office · 3F")
        );
        // 说明只是把队列名重抄一遍（CUPS 新建队列的默认行为）时，它不带来任何信息
        assert_eq!(printer_label("CanonG3860", "CanonG3860", "").as_deref(), None);
        // 但同一台的位置仍然要说
        assert_eq!(printer_label("CanonG3860", "CanonG3860", "XSMS").as_deref(), Some("XSMS"));
        // 两句都一样时不重复一遍
        assert_eq!(printer_label("HP", "前台", "前台").as_deref(), Some("前台"));
        // 没有说明、只有位置：不能让位置单独冒充打印机名，队列名补位当主语
        assert_eq!(
            printer_label(r"\\filesrv\Lobby", "", "前台").as_deref(),
            Some(r"\\filesrv\Lobby · 前台")
        );
        // 什么都没有 → 界面回落到队列名
        assert_eq!(printer_label("HP", "  ", "").as_deref(), None);
    }
}
