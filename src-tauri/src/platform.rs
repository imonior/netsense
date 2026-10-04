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
use std::net::Ipv4Addr;
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
/// 这里回答的是"这台机器上**同时**连着哪些网"，面板的两段网卡列表（其他在用网卡、
/// VPN/虚拟网卡）与编辑器的「当前网络」那一格都基于它。
///
/// 字段取不到一律为 `None` —— 前端按缺省显示「—」，不允许用占位文本造假值。
#[derive(Debug, Clone, Default, Serialize)]
pub struct NicInfo {
    /// 设备名（macOS: en0 / utun4；Windows: Wi-Fi / Ethernet0；Linux: wlan0 / tailscale0）
    pub name: String,
    /// 人类可读名：macOS 的网络服务名 / Windows 的适配器描述 / Linux 的连接名
    pub label: Option<String>,
    pub kind: NicKind,
    /// 这条链路当下**连着没有**：有 IPv4 或已关联无线即视为在用；面板的 VPN 段就靠这一格
    /// 说「已连接 / 未连接」。
    ///
    /// 未连接的条目**也在清单里**（Windows 上没连上的虚拟适配器、macOS 上装了但没连的 VPN
    /// 会话）：「这个软件装了，现在没连」本身就是信息，托盘面板与 3B2 的「维持连接」都要
    /// 能找到它。反过来说，`up: false` 不是「采集失败」，而 conditions 层的身份快照不收它
    /// （没连的链路进不了比对集合）。
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
    /// 该网卡上的 **IPv6 默认网关**（`::/0` 的下一跳），取不到即为 `None`。
    ///
    /// 与 `gateway` 同一口径：只收真地址。点对点隧道的 on-link 下一跳在各平台上分别
    /// 写成 `::`（Windows）与 `link#N`（macOS）—— 那些不是网关，采集时就滤掉，
    /// 面板才摆不出一行不是地址的「网关」。
    pub gateway6: Option<String>,
    /// 该网卡自己的路由前缀（`0.0.0.0/0`、`10.30.35.0/24` 这类），最多 12 条。
    ///
    /// VPN 隧道通常是点到点、没有下一跳，`gateway` 于是空着 —— 能回答「这条隧道管哪些网」
    /// 的只有这张表，面板的「网关或路由」那一格读它（默认那一条由网关行回答，展示时滤掉）。
    /// 采集走的是各平台本来就要跑一遍的那份路由表，不额外起子进程；`[]` = 这条链路没有
    /// 可显示的路由。
    pub routes: Vec<String>,
    /// 该网关 IP 对应的 MAC
    pub gateway_mac: Option<String>,
    pub dns: Option<String>,
    /// VPN 归属软件名（WireGuard / Tailscale / AnyConnect …）；非 VPN 为 None。
    ///
    /// 各平台只在拿得准的时候才填：macOS 没有「utun → 进程」的公开映射，猜出来的归属
    /// 会指到**别家软件**头上（那条隧道不是它建的）。认不出来留 `None`，界面退回通用的
    /// 「VPN」标签；认错才是事故。
    ///
    /// 「装了但没连」的那几张卡（macOS 的 VPN 会话、Windows 上没连上的虚拟适配器）填的
    /// 是**这条会话自己的名字**归一出来的产品名 —— 它不认领任何隧道，所以这一格给得出来；
    /// 而它同时带着 `up: false`，界面据此说「未连接」。
    pub app: Option<String>,
}

/// 一张网卡在 [`NicInfo::routes`] 里最多留几条前缀（三平台同一个上限）。
///
/// 隧道可以把整张网段拆成一堆 /32 逐条下发（WireGuard 的 `allowed-ips` 常见形态），
/// 面板那一格只有几十字宽：全采回来既撑爆每次广播的载荷、也一行都显示不完。
pub(crate) const MAX_ROUTES_PER_IFACE: usize = 12;

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
static NIC_CACHE: OnceLock<Mutex<Option<NicCacheEntry>>> = OnceLock::new();

pub(crate) fn cached_nics<F: FnOnce() -> Vec<NicInfo>>(sample: F) -> Vec<NicInfo> {
    let cell = NIC_CACHE.get_or_init(|| Mutex::new(None));
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

/// 丢掉 [`cached_nics`] 的快照。
///
/// 本机网卡集合被本进程改动（接/断隧道、开关 Wi-Fi 接口）之后调用，使下一次 `list_interfaces`
/// 立刻重新采样，而不是在 [`NIC_LIST_TTL`] 内一直用旧名单（旧名单会让「主网卡」「隧道归属」
/// 这类判据对着过时的接口做决定）。
pub(crate) fn invalidate_nics() {
    if let Some(cell) = NIC_CACHE.get() {
        *cell.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// 静态枚举类读取的缓存 TTL。
///
/// 与 [`NIC_LIST_TTL`]（几秒量级）刻意不是一个量级：已装程序、打印机、已知网络这三项
/// 在几分钟内不会因为用户的任何操作而变化，而它们各自的代价是**一整个子进程的冷启动**
/// —— Windows 上尤其明显，PowerShell 5.1 的启动叠上 CIM / 模块加载，单项就到一两秒。
/// 编辑器开屏把这几项和状态快照一起并发拉出去，那几秒全部落在用户「点开窗口到能用」
/// 的等待里，而它们回答的是几分钟内不会变的问题。
///
/// 取两分钟而不是更长：装一个新程序、接一台新打印机之后，用户有理由期望下一次刷新就
/// 看到它；超过两分钟还看不到，第一反应会是「软件坏了」而不是「再等等」。
const ENUM_CACHE_TTL: Duration = Duration::from_secs(120);

/// 三个枚举缓存各自的存储单元。抽出来只是为了不让 `static` 声明处挂一串嵌套泛型 ——
/// 与 `NicCacheEntry` 同一个理由。
type AppCacheCell = Mutex<Option<(Instant, Vec<AppEntry>)>>;
type PrinterCacheCell = Mutex<Option<(Instant, Vec<PrinterInfo>)>>;
type SsidCacheCell = Mutex<Option<(Instant, Vec<String>)>>;

/// 带 TTL 的已装程序清单。
///
/// **空结果不进缓存**：它既可能是「这台机器真的一条都没有」，也可能是脚本没起来 ——
/// `list_installed_apps` 把后者同样收敛成空数组（那条路径只记日志）。缓存一次失败，
/// 之后两分钟里每次下拉都是空的，而界面因此说的是「这台机器没装东西」，错得更彻底。
pub(crate) fn cached_apps(plat: &Platform) -> Vec<AppEntry> {
    static C: OnceLock<AppCacheCell> = OnceLock::new();
    let cell = C.get_or_init(|| Mutex::new(None));
    {
        let guard = cell.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, v)) = guard.as_ref() {
            if at.elapsed() < ENUM_CACHE_TTL {
                return v.clone();
            }
        }
    }
    let fresh = plat.list_installed_apps();
    if !fresh.is_empty() {
        *cell.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), fresh.clone()));
    }
    fresh
}

/// 带 TTL 的打印机清单。空结果不进缓存，理由同 [`cached_apps`]。
pub(crate) fn cached_printers(plat: &Platform) -> Vec<PrinterInfo> {
    static C: OnceLock<PrinterCacheCell> = OnceLock::new();
    let cell = C.get_or_init(|| Mutex::new(None));
    {
        let guard = cell.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, v)) = guard.as_ref() {
            if at.elapsed() < ENUM_CACHE_TTL {
                return v.clone();
            }
        }
    }
    let fresh = plat.list_printers();
    if !fresh.is_empty() {
        *cell.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), fresh.clone()));
    }
    fresh
}

/// 带 TTL 的已知网络（SSID）清单。空结果不进缓存，理由同 [`cached_apps`]。
pub(crate) fn cached_known_ssids(plat: &Platform) -> Vec<String> {
    static C: OnceLock<SsidCacheCell> = OnceLock::new();
    let cell = C.get_or_init(|| Mutex::new(None));
    {
        let guard = cell.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, v)) = guard.as_ref() {
            if at.elapsed() < ENUM_CACHE_TTL {
                return v.clone();
            }
        }
    }
    let fresh = plat.list_known_ssids().unwrap_or_default();
    if !fresh.is_empty() {
        *cell.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), fresh.clone()));
    }
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
///
/// 同时清掉 [`cached_nics`]：隧道起停、接口开关这类改动如果只清状态缓存，网卡名单还会在
/// [`NIC_LIST_TTL`] 内用旧的，于是 3A 读回、主网卡判据都对着过时的接口做决定。
#[allow(dead_code)] // 调用方同上
pub(crate) fn invalidate_status() {
    if let Some(cell) = STATUS_CACHE.get() {
        *cell.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    invalidate_nics();
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

/// 从一段自由文本里猜 VPN 软件名（Windows 适配器描述 / macOS 的 VPN 会话标签 / nmcli 连接名）。
///
/// 命中即返回**产品名**（统一大小写，便于展示），未命中返回 `None`，由调用方退回
/// 更泛的兜底名（如设备名）。
///
/// 用的前提是「这段文本描述的就是这块网卡／这条会话」：三条腿传进来的都是设备自带的描述、
/// 会话标签或连接名。macOS 那一腿**只**拿它做这一步（给一条 VPN 会话归一个产品名），
/// 从不拿它去猜「这条 utun 是谁建的」—— 那边的服务清单里，装过但没连的客户端**一直**在列，
/// 于是无人认领的隧道会被说成是别人建的，认不出比认错糟得多（见 macos 腿的归属判定）。
pub(crate) fn guess_vpn_app(text: &str) -> Option<&'static str> {
    let s = text.to_ascii_lowercase();
    for (needle, name) in VPN_APP_TABLE {
        if s.contains(needle) {
            return Some(name);
        }
    }
    None
}

/// 给一条 VPN 会话／连接归一个**值得单独显示**的产品名；不值得就返回 `None`。
///
/// 「装了但没连」的那几张卡（macOS 的 VPN 会话、Linux 上未激活的 NM 连接）在界面上的标题
/// 就是会话标签／连接名本身，产品名只是旁边那一枚标签。所以只在两者**说的不是同一件事**时
/// 才挂：`Nord` 旁边那枚「NordVPN」是信息，`Office WireGuard` 旁边再来一枚「WireGuard」是
/// 把同一句话写两遍。认不出产品名同样返回 `None`，界面退回通用的「VPN」。
///
/// `#[allow(dead_code)]`：调用方是 macOS 与 Linux 两条腿（它们都有「装了但没连」的会话要
/// 显示）；Windows 那一侧的虚拟适配器在 `list_interfaces` 里直接给出了归属，不经过这里。
#[allow(dead_code)]
pub(crate) fn vpn_app_for(label: &str) -> Option<String> {
    let product = guess_vpn_app(label)?;
    let hay = app_letters(label);
    (!hay.contains(app_letters(product).as_str())).then(|| product.to_string())
}

/// 产品名比对用的写法：只留字母数字并转小写。
///
/// 会话标签往往是用户从客户端窗口里抄来的写法（`ProtonVPN`、`Tailscale Tunnel`），产品名是
/// 表里那一个（`Proton VPN`）。两者指的是同一个软件，差别却只在大小写、空格和连字符上，
/// 所以比对前先把这些抹掉 —— 否则「同一个名字写两遍」这一关会漏掉大半。
fn app_letters(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
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
    /// 提权机制**装着，但不是这个二进制所带的那一份**：执行时按 [`Prompt`](Self::Prompt)
    /// 走（那一次授权顺手把它换成本版），但界面上不能就说「每次都要授权」—— 一次之后
    /// 就免密了，而且这里确实有一份可以撤销的东西。
    ///
    /// 只有 macOS 会报这一档：Windows 的免密来自「进程是不是管理员」，Linux 来自
    /// `sudo -n` 探一下，两边都没有一份带版本的本机脚本可比。另外两条腿上它因此
    /// 「永不被构造」，那条 dead_code 是本设计的形状，不是漏了接线。
    #[allow(dead_code)]
    Outdated,
}

impl PrivChannel {
    pub fn code(self) -> &'static str {
        match self {
            PrivChannel::Direct => "direct",
            PrivChannel::Prompt => "prompt",
            PrivChannel::Outdated => "outdated",
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

/// 一个**可启动的**本机程序：给人看的名字，和 [`NetworkPlatform::launch_app`]
/// 真正要的那个目标。
///
/// `path` 是平台自己受理的形状，不是「某个统一的文件路径」：macOS 是 `.app` 包路径
/// （`open -a` 之外最稳的 `open <路径>`），Windows 是 `.lnk` / `.exe` 的完整路径
/// （`Start-Process` 两种都收），Linux 是 `.desktop` 里 `Exec=` 的可执行文件路径
/// （桌面环境之外 `xdg-open` 也能把 `.desktop` 起起来，但直接 spawn 可执行文件更干净）。
/// 界面**只负责原样带回**：它不该、也无法把这里的路径翻译成另一种形状。
///
/// `name` 只给人看 —— macOS 是去掉 `.app` 后缀的包名、Windows 是快捷方式名、
/// Linux 是 `.desktop` 的 `Name`（已按当前语言挑过的那个）。
#[derive(Debug, Clone, Serialize)]
pub struct AppEntry {
    pub name: String,
    pub path: String,
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
    ///   Linux 直接 `spawn` 或 `xdg-open`），**不**保证应用还活着 ——
    ///   应用随后自己崩掉，本方法无从得知，动作照样记成功。这是这个动作能给的的全部信息。
    /// - 而「目标在不在机器上」必须在**交给启动器之前**判掉，不能指望启动器回头报错：
    ///   Linux 走的是不等退出码的 `spawn_detached`，一个注定失败的 `xdg-open` 只会把
    ///   徽标刷成绿的。URL 与被打开的文档没有本机路径可查，它们的 `Ok` 仍只代表「已丢出去」。
    /// - 因此实现**绝不能等应用退出**：浏览器可以开一下午，等到超时会被记成
    ///   「启动失败」，而用户看到的是应用好好地开着。
    /// - `Err` 留给启动器自己就拒绝的情况：找不到该应用、参数根本没法传
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

    /// 本机**装着的可启动程序**清单（编辑器里启动程序动作的候选）。
    ///
    /// 只服务一件事：3B 里那个应用路径输入框的下拉候选。取不到、这台机器没装任何
    /// 能枚举的东西，都返回空列表 —— 空不是错误，输入框本来就允许手输，界面据此
    /// 显示「没有候选，请手输」；**不要**为它弹框，也不要把它伪装成加载失败。
    ///
    /// 枚举的只是**有启动意图登记**的程序：macOS 的 `.app` 包、Windows 开始菜单里的
    /// 快捷方式（`.lnk`）、Linux 的 `.desktop`。不扫 `Program Files` 的裸 `.exe` ——
    /// 那里面有大量不是给人启动的东西（卸载器、辅助进程、运行时），拉一份进去等于
    /// 把下拉变成垃圾场。找不到的程序照样可以手输路径，这条路不受枚举范围约束。
    ///
    /// 结果按名字排序、`path` 去重；`async` 调用方（见 `ipc::get_installed_apps`）
    /// 负责别把它放进主线程 —— 三套实现都要拉子进程或遍历目录。
    fn list_installed_apps(&self) -> Vec<AppEntry>;

    /// 弹**系统自己的**文件选择器，让用户挑一个程序，返回其路径；用户取消 → `Ok(None)`。
    ///
    /// 与 [`list_installed_apps`](NetworkPlatform::list_installed_apps) 的分工：那个
    /// 回答「菜单里有哪些」，这个回答「磁盘上任意一个我都指得出来」—— 绿色免安装的
    /// 单文件程序不会出现在任何开始菜单里，只能靠这里找。
    ///
    /// 返回的路径同样只要求是 `launch_app` 受理的形状（见 [`AppEntry`] 的 `path`）；
    /// 取消返回 `Ok(None)` 而不是 `Err` —— 「用户想了想又关掉」不是错误，界面不该弹
    /// 报错。选择器起不来（headless、没装对话框工具）才是 `Err`。
    fn pick_app(&self) -> Result<Option<String>, String>;

    /// 已保存的无线网络列表（编辑器下拉填充）。平台不支持时返回 `None`。
    fn list_known_ssids(&self) -> Option<Vec<String>>;

    /// 本机**装着**的网卡（含现在没插线、没连上的），每张一条 [`NicInfo`]。
    ///
    /// 与 [`list_interfaces`](NetworkPlatform::list_interfaces) 的分工：那张回答「现在连着
    /// 哪些网」，面板与身份快照用它；这张回答「这台机器有哪几口」，编辑器的接口条件下拉用
    /// 它。两者不能合成一张：拔着网线时下拉只剩 `en0`，用户就没法提前给另一个口配好网络。
    ///
    /// 实现只需填 `name` / `label` / `kind` / `up`（`up` = 链路是否起来了）；地址类字段留
    /// `None`，界面也不会去读 —— 这里要的是「有哪些口」，不是「每个口现在拿到了什么」。
    /// 刻意**不带 TTL 缓存**：只有编辑器打开/刷新时才要一份，而缓存会让刚插上扩展坞的
    /// 那一次刷新继续显示旧清单。
    fn list_adapters(&self) -> Vec<NicInfo>;

    /// 系统自己的界面语言标签（`zh-CN` / `ja-JP` / `ko-KR` / `en-US` 这一类）。
    ///
    /// 只服务一件事：软件配置里选了「跟随系统」时，启动那一刻要知道该说哪种话。
    /// 取不到就返回 `None`（系统没有这个概念、或读的方式被拒），调用方按英文渲染 ——
    /// 英文是字典的基准语言，也是这个应用在没有别的线索时的默认值。
    ///
    /// 只在启动与切换语言那一刻调用，别放进轮询路径。
    fn ui_language(&self) -> Option<String>;

    /// 系统界面此刻是深色还是浅色。
    ///
    /// 只服务一件事：软件配置里选了「跟随系统」时，本应用该把 `<html>` 推到哪套配色上。
    /// 取不到返回 `None`（这个桌面没有「深/浅」这个概念、读它的方式被拒、或压根没装
    /// 相应的读取工具），调用方按深色渲染 —— 深色是这套界面的设计基准，也是没有别的线索时
    /// 的样子。
    ///
    /// **不要**改用 CSS 的 `prefers-color-scheme`：那一句话问的是「承载这个页面的 WebView
    /// 自己呈现成什么」，而本应用的四座窗口分属三套不同的 WebView，界面颜色必须是它们共同
    /// 的**一个**决定 —— 决定在这里做一次，再随状态广播发给四座窗口，它们才会同进同退。
    ///
    /// 只在窗口加载与状态广播时调用，别放进轮询路径。
    fn ui_prefers_dark(&self) -> Option<bool>;
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

/// 已装程序清单的收口（三平台共用）：按 `path` 去重（同一份程序可能在两个目录各有一份，
/// 例如系统目录与用户目录），名字按不区分大小写排、同名再按路径 —— 下拉要的是稳定顺序，
/// 别让文件系统的枚举顺序决定列表长什么样。
pub(crate) fn dedupe_sort_apps(mut apps: Vec<AppEntry>) -> Vec<AppEntry> {
    let mut seen = std::collections::HashSet::new();
    apps.retain(|a| seen.insert(a.path.clone()));
    apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    apps
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

/// `defaults read -g` 的转储 → 系统此刻是不是深色。
///
/// 判据是「这一对键值在不在」，而不是「`AppleInterfaceStyle` 读得到读不到」：浅色模式下
/// macOS 就是**没有**这个键，`defaults read -g AppleInterfaceStyle` 会以非 0 退出，那条
/// 错误和「`defaults` 这个二进制起不来」在 `run` 的分界里长得一模一样。整域的转储在两种
/// 模式下都成功，于是「浅色」和「问不到」分得开 —— 后者要退深色，前者不该退。
///
/// 只认 `AppleInterfaceStyle = Dark;` 这一行，因此旁边那行
/// `AppleInterfaceStyleSwitchesAutomatically = 1;`（用户设的是「自动」）不会被骗成深色；
/// 自动模式下系统仍然会把 `AppleInterfaceStyle` 写成当下那档，所以跟着走是对的。
#[allow(dead_code)] // 只有 macOS 的命令输出是这个形状
pub(crate) fn prefers_dark_from_defaults(text: &str) -> bool {
    text.lines().any(|line| {
        let mut it = line.trim().splitn(2, '=');
        match (it.next(), it.next()) {
            (Some(k), Some(v)) => {
                let v = v.trim().trim_end_matches(';').trim();
                // `defaults` 打字符串时不带引号，但引号是它别处见过的写法，认下来不亏
                k.trim() == "AppleInterfaceStyle" && v.trim_matches('"') == "Dark"
            }
            _ => false,
        }
    })
}

/// `reg query …\Personalize /v AppsUseLightTheme` 的输出 → 系统此刻是不是深色。
///
/// 值行形如 `    AppsUseLightTheme    REG_DWORD    0x1`。**1 = 浅色，0 = 深色** —— 这个
/// 键名是反着说的（它问的是「用不用浅色」），所以这里不许含糊：认不出形状就是 `None`，
/// 而不是猜一个。取最后一个空白分隔的 token，因为键名本身可以含空格的路径部分不在值行里。
#[allow(dead_code)] // 只有 Windows 的命令输出是这个形状
pub(crate) fn prefers_dark_from_reg(text: &str) -> Option<bool> {
    let line = text.lines().find(|l| l.contains("AppsUseLightTheme"))?;
    let token = line.split_whitespace().last()?;
    let hex = token.strip_prefix("0x").or_else(|| token.strip_prefix("0X"))?;
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(v == 0)
}

/// GNOME 的两条 `gsettings` → 系统此刻是不是深色。
///
/// `color-scheme` 是 GNOME 42 起正问「深/浅」的那一条（`prefer-dark` / `prefer-light` /
/// `default` / `gtk`）；`gtk-theme` 是它的前身，只能从主题名里猜（`Adwaita-dark`）。
/// 两个都问不到（不是 GNOME 的桌面、`gsettings` 没装、dconf 里没这两个键）时返回 `None`，
/// 调用方退深色 —— 猜一套并不存在的深色偏好，比承认问不到更糟。
#[allow(dead_code)] // 只有 Linux 的命令输出是这个形状
pub(crate) fn prefers_dark_from_gsettings(
    color_scheme: Option<&str>,
    gtk_theme: Option<&str>,
) -> Option<bool> {
    // gsettings 把字符串值带引号打出来：`'prefer-dark'`。
    let clean = |s: &str| s.trim().trim_matches('\'').to_string();
    match color_scheme.map(clean).as_deref() {
        Some("prefer-dark") => return Some(true),
        Some("prefer-light") | Some("default") | Some("high-contrast") => return Some(false),
        // `gtk` 这一档把答案交给主题名，读不到这一条也一样：都往下去看 `gtk-theme`。
        _ => {}
    }
    let theme = gtk_theme.map(clean)?;
    Some(theme.contains("dark"))
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

    /// 说明与位置拼成人话标签；除了「说明只是重抄队列名、又没有位置」之外都要给标签
    /// （`None` → 界面用队列名）。规则见 [`printer_label`]。
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
/// 规则只有一条：**标签要认得出是哪台机器**，所以主语永远在，位置只是它的注脚。
/// - 说明与队列名重复时（CUPS 新建队列常把队列名原样填进说明）**也照样说两遍**：
///   重复一个词是无害的，把主语弄没才是问题 —— 丢掉说明后界面只剩 `XSMS`，用户面对的
///   是一排房间号而不是一排机器。正确形状是 `CanonG3860 · XSMS`。
/// - 说明**根本没有**、只有位置时用队列名当主语，不能只亮位置 —— 那会让用户把一台机器
///   认成另一台（Windows 上被误认成「别的系统的打印机」就是这么来的）：
///   `\\filesrv\Lobby · 前台`。
/// - 位置与说明/队列名重复时不重复念。
/// - 一点新信息都没有（只有和队列名相同的说明，或两句都空）时返回 `None`，界面回落队列名。
pub(crate) fn printer_label(name: &str, description: &str, location: &str) -> Option<String> {
    let d = description.trim();
    let l = location.trim();
    let subject = if d.is_empty() { name } else { d };
    let mut parts: Vec<&str> = vec![subject];
    if !l.is_empty() && l != subject {
        parts.push(l);
    }
    let only_name = parts.len() == 1 && parts[0] == name;
    (!only_name).then(|| parts.join(" · "))
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

/// 宽松 MAC 提取：在任意文本行里找出第一个「6 组、每组 1~2 个十六进制数字、以 `:` 或 `-`
/// 分隔」的串，并归一化成标准 `XX:XX:XX:XX:XX:XX`（小写、每组 2 位）。
///
/// 比 [`extract_mac`] 多处理一种情形：macOS `arp -n` 会把每组的前导零吞掉
/// （`00:50:56:c0:00:08` 打印成 `0:50:56:c0:0:8`），而那种 MAC 的首字节 `0` 往往跟在前
/// 面的 `? (192.168.1.1) at ` 文本里、和 IP 数字同处一个 `:` 分组，那种「按整行 `:` 分段
/// 补零」的做法补不到它，于是 `gateway_mac` 条件对这一类 OUI 永远解析不出来。这里用字节
/// 窗口扫描，MAC 的 6 组形态独立成串即可命中，不受周围文本干扰；每组各自补到 2 位，
/// 因此 `0:50:56:c0:0:8` 稳定还原成 `00:50:56:c0:00:08`。
pub(crate) fn extract_mac_loose(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let n = bytes.len();
    for i in 0..n {
        if !bytes[i].is_ascii_hexdigit() {
            continue;
        }
        let mut j = i;
        let mut groups: Vec<&[u8]> = Vec::with_capacity(6);
        let mut ok = true;
        for k in 0..6 {
            let start = j;
            while j < n && bytes[j].is_ascii_hexdigit() {
                j += 1;
            }
            let g = &bytes[start..j];
            if g.is_empty() {
                ok = false;
                break;
            }
            groups.push(g);
            if k < 5 {
                if j >= n || (bytes[j] != b':' && bytes[j] != b'-') {
                    ok = false;
                    break;
                }
                j += 1;
            }
        }
        if ok && groups.len() == 6 && groups.iter().all(|g| g.len() <= 2) {
            let mut out = String::with_capacity(17);
            for (idx, g) in groups.iter().enumerate() {
                if idx > 0 {
                    out.push(':');
                }
                // 每组 1~2 位十六进制，一定能解析成 u8。
                let val = u8::from_str_radix(std::str::from_utf8(g).unwrap_or("0"), 16).unwrap_or(0);
                out.push_str(&format!("{val:02x}"));
            }
            return Some(out);
        }
    }
    None
}

/// L2 DNS 解码：把 NetworkManager 老式 `Nameservers`（`au`，数组元素为每个地址的 `in_addr`）还原成点分十进制串。
///
/// 总线上的 `guint32` 就是 `in_addr.s_addr`：NM 写端 `g_variant_builder_add(&builder, "u", a)`
/// 把 `NMIPAddr` 联合体首 4 字节当作本机 `guint32` 原样发出，全程没有任何 `htonl`/`ntohl`
/// （读端 `nm_inet4_ntop_dup(array[i])` 同样不加端序宏，直接按网络序 `in_addr_t` 解读）。
/// 而 `in_addr_t` 在内存里就是地址的 4 个八位组按网络(大端)序排列（`1.2.3.4` → 内存 `[01 02 03 04]`），
/// 于是小端主机上把这 4 字节当成 `u32` 读出的数值反而是 `0x04030201`，并非 `0x01020304`。
///
/// 因此必须先用 `to_ne_bytes()` 把总线 `u32` 还原成那 4 个网络序字节，再交给 `Ipv4Addr::from`。
/// 这与 socket2（`src/sys/unix.rs` 的 `from_in_addr` 同样写 `Ipv4Addr::from(s_addr.to_ne_bytes())`）
/// 以及 glibc `inet_pton` 落到 `s_addr` 上的值逐字一致——`1.2.3.4` 的总线值正是 `0x04030201`。
///
/// 切勿直接 `Ipv4Addr::from(n)`：`Ipv4Addr::from(u32)` 按大端解读，会把 `0x04030201` 解成 `4.3.2.1`。
/// `8.8.8.8` 是回文（`0x08080808` 正反相同），旧写法在此地址上“碰巧正确”正是它长期没被发现的原因。
///
/// 抽成 cfg 无关纯函数，是为了能在 macOS 宿主机上对这条 linux 专属解码路径直接单测
/// （`cfg(linux)` 下的函数在 mac target 上既编不进也跑不到）。
// 非 linux 宿主构建里只有单测会用到它（`cargo check` 不编测试），故在此放行 dead_code，
// 以免 macOS 上 `cargo check`/`clippy` 报未使用——Linux 构建里它确有调用方。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn decode_nm_nameservers(ns: &[u32]) -> Vec<String> {
    ns.iter()
        .map(|n| Ipv4Addr::from(n.to_ne_bytes()).to_string())
        .collect()
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

/// 撤销 macOS 的免密通道（删掉白名单包装脚本与 `/etc/sudoers.d/netsense`）。
///
/// 走的是自由函数而不是 [`NetworkPlatform`] 的方法：这条通道是 macOS 独有的机制
/// （Windows 靠常驻提权助手、Linux 靠 sudo/pkexec，两边都没有「撤销一个 sudoers 行」
/// 这件事），放进 trait 就要在另外两条腿上各写一个空壳。
/// 界面上的按钮在平台为 macOS 且 `priv` 为 `direct` 或 `outdated` 时出现 —— 撤销的对象是
/// 那条 sudoers 规则，脚本新旧都有得删；`prompt`（什么都没装）才没有入口。下面那条错误是
/// 「有人绕过界面直接调用命令」时的兜底，不是正常路径。
#[cfg(target_os = "macos")]
pub fn uninstall_priv_channel() -> Result<(), String> {
    macos::uninstall_priv_channel()
}

#[cfg(not(target_os = "macos"))]
pub fn uninstall_priv_channel() -> Result<(), String> {
    Err(crate::i18n::t("pal.priv_unsupported"))
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

/// [`NetworkPlatform::ui_language`] 的免装配版本：启动早期还没有 `AppState`，
/// 而「跟随系统」必须在那一刻就把语言定下来 —— 第一条日志就该用对的语言写。
pub fn system_ui_language() -> Option<String> {
    Platform.ui_language()
}

/// [`NetworkPlatform::ui_prefers_dark`] 的免装配版本：配色是窗口加载那一刻就要定的事，
/// 那一刻 `AppState` 可能还没准备好（而「跟随系统」必须和别的界面偏好走同一个来源）。
///
/// 答案缓存 60s。这个值现在**每一次状态广播都会被问一遍**（`status_payload` 里带着它，
/// 好让四座窗口跟着换档），而三条腿的实现都要拉起一个子进程（`defaults` / `reg.exe` /
/// `gsettings`）—— 引擎一轮评估可不止一次广播。60s 是两头都能接受的那个数：用户在系统设置里
/// 翻一下深色，面板最迟一分钟后跟着翻，而这一分钟里剩下的那些次问话都是免费的。
pub fn system_prefers_dark() -> Option<bool> {
    static CACHE: OnceLock<Mutex<(Option<Instant>, Option<bool>)>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new((None, None)));
    let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let (Some(at), value) = &*g {
        if at.elapsed() < Duration::from_secs(60) {
            return *value;
        }
    }
    let value = Platform.ui_prefers_dark();
    *g = (Some(Instant::now()), value);
    value
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
#[cfg(target_os = "linux")]
mod linux_netlink;
#[cfg(target_os = "linux")]
mod linux_nm;

#[cfg(target_os = "macos")]
pub use macos::{priv_channel, request_location_authorization, MacPlatform as Platform};
#[cfg(target_os = "windows")]
pub use windows::{priv_channel, WindowsPlatform as Platform};
#[cfg(target_os = "linux")]
pub use linux::{priv_channel, LinuxPlatform as Platform};

#[cfg(test)]
mod tests {
    use super::{
        app_letters, decode_nm_nameservers, extract_mac_loose, prefers_dark_from_defaults,
        prefers_dark_from_gsettings, prefers_dark_from_reg, printer_label, printers_from_lpstat,
        provider_in, sh_q, tunnel_name_eq, vpn_app_for, TunnelTarget,
    };

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

    /// 回归 #3：macOS `arp -n` 吞掉 MAC 每段的前导零，旧实现整行补零也救不回
    /// （首字节 `0` 与前面 IP 文本同处一个 `:` 分组）。VMware / 华为 / 部分 OUI 的
    /// 网关 MAC 因此永远为 `None`，导致 `gateway_mac` 匹配条件形同虚设。
    /// `extract_mac_loose` 逐组把 1~2 位十六进制归一成规范 MAC，恰好匹配该形态。
    #[test]
    fn extract_mac_loose_restores_leading_zero_ouis() {
        // VMware OUI 00:50:56 —— arp 打印成 0:50:56:c0:0:8
        assert_eq!(
            extract_mac_loose("? (192.168.1.1) at 0:50:56:c0:0:8 on en0 ifscope [ethernet]")
                .as_deref(),
            Some("00:50:56:c0:00:08")
        );
        // 华为云 / 部分虚拟网卡 OUI 00:16:3e
        assert_eq!(
            extract_mac_loose("? (10.0.0.1) at 0:16:3e:ab:cd:ef").as_deref(),
            Some("00:16:3e:ab:cd:ef")
        );
        // 另一常见 OUI 00:1b:77（Apple 历史分配）
        assert_eq!(
            extract_mac_loose("? (172.16.0.1) at 0:1b:77:01:02:03").as_deref(),
            Some("00:1b:77:01:02:03")
        );
        // 既有「每段恰 2 位」的规范形态仍照常工作（不能因为宽松而退化）
        assert_eq!(
            extract_mac_loose("   ? (192.168.1.1) at e8:84:c6:93:ad:eb").as_deref(),
            Some("e8:84:c6:93:ad:eb")
        );
        // 短横线分隔同样支持
        assert_eq!(
            extract_mac_loose("00-50-56-c0-00-08").as_deref(),
            Some("00:50:56:c0:00:08")
        );
        // 非 MAC 文本依旧返回 None
        assert_eq!(extract_mac_loose("no mac here"), None);
    }

    /// 回归 L2 DNS 解码：NM 老式 `Nameservers`（`au`）的总线 `guint32` 是 `in_addr.s_addr`，
    /// 其内存字节即地址的网络序八位组；小端机上数值反序（如 `1.2.3.4` → `0x04030201`），
    /// 须经 `to_ne_bytes()` 还原再交给 `Ipv4Addr::from`。这与 socket2 / glibc `inet_pton`
    /// 落在 `s_addr` 上的值一致，CI（含 ubuntu-latest 真 glibc）即为裁判。
    #[test]
    fn decode_nm_nameservers_recovers_address() {
        // 总线值由 socket2 的 `to_in_addr` 契约求出：`s_addr = u32::from_ne_bytes(octets)`，
        // 与 glibc/macOS `inet_pton` 实际落到 `s_addr` 的数值逐字相同，避免硬编码猜值。
        for (ip, octets) in [
            ("1.2.3.4", [1u8, 2, 3, 4]),
            ("192.168.1.1", [192, 168, 1, 1]),
            ("223.5.5.5", [223, 5, 5, 5]),
            ("208.67.222.222", [208, 67, 222, 222]),
        ] {
            let bus = u32::from_ne_bytes(octets);
            assert_eq!(
                decode_nm_nameservers(&[bus]),
                vec![ip.to_string()],
                "decode {ip}: bus u32 = {bus:#x}",
            );
        }
        // 8.8.8.8 是回文：to_ne_bytes 前后字节序不变，旧 bug 在此地址上"碰巧正确"，
        // 这正是它长期没被发现的原因——保留此用例防止有人误以为只有回文需要覆盖。
        assert_eq!(
            decode_nm_nameservers(&[u32::from_ne_bytes([8, 8, 8, 8])]),
            vec!["8.8.8.8".to_string()]
        );
        // 空数组返回空
        assert!(decode_nm_nameservers(&[]).is_empty());
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
                // 现场报的那一行：说明就是把队列名重抄了一遍（CUPS 新建队列的默认），
                // 但主语不能因此消失 —— 只亮位置会让一排下拉项都成了房间号。
                // disabled 也照样在清单上（用户要的正是「能选到它」，状态由下发时的成败说话）
                ("CanonG3860", Some("CanonG3860 · XSMS"), true),
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
    fn a_printer_label_always_keeps_a_subject() {
        assert_eq!(
            printer_label("HP_Office", "HP LaserJet in the office", "3F").as_deref(),
            Some("HP LaserJet in the office · 3F")
        );
        // 说明只是把队列名重抄一遍（CUPS 新建队列的默认行为）：没有新信息，所以不给标签，
        // 界面用队列名 —— 这与下一行的区别只在于有没有位置。
        assert_eq!(printer_label("CanonG3860", "CanonG3860", "").as_deref(), None);
        // 现场报的就是这一条：说明与队列名相同时把说明丢掉，界面上只剩「XSMS」，一排下拉
        // 项全成了房间号。主语必须在，位置跟在后面。
        assert_eq!(
            printer_label("CanonG3860", "CanonG3860", "XSMS").as_deref(),
            Some("CanonG3860 · XSMS")
        );
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

    /// macOS 那一条只能看「这一行在不在」：浅色模式下 `AppleInterfaceStyle` 根本不存在。
    /// 因此「没有这一行」必须是浅色，而不是「不知道」。
    #[test]
    fn a_macos_dump_without_the_style_key_is_light_not_unknown() {
        assert!(!prefers_dark_from_defaults("AppleAquaFontDesign = normal;\n"));
        assert!(prefers_dark_from_defaults("AppleInterfaceStyle = Dark;\n"));
        // 旁边那行说的是「自动切换」，它本身不等于深色；当下那档仍由上一行给出
        assert!(!prefers_dark_from_defaults(
            "AppleInterfaceStyleSwitchesAutomatically = 1;\n"
        ));
        assert!(prefers_dark_from_defaults(
            "AppleInterfaceStyleSwitchesAutomatically = 1;\nAppleInterfaceStyle = Dark;\n"
        ));
        // 值带引号、含空格、结尾分号旁有空格 —— `defaults` 的转储确实会这样
        assert!(prefers_dark_from_defaults("    AppleInterfaceStyle  =  \"Dark\"  ;\n"));
        // 判据是值等于 Dark，因此任何别的值（包括被写成 Light）都算浅色：浅色模式下
        // macOS 本来就没有这个键，写出一个非 Dark 的值只能是「用户/工具想让它浅色」
        assert!(!prefers_dark_from_defaults("AppleInterfaceStyle = Light;\n"));
        // 只有键名没有赋值号时不算命中（别把注释行读成设置）
        assert!(!prefers_dark_from_defaults("// AppleInterfaceStyle = Dark;\n"));
    }

    /// Windows 的键名是反着问的（`AppsUseLightTheme`），所以 `0x0` 才是深色。
    /// 问不到形状必须给 `None`：退深色是调用方的决定，不是解析器猜出来的。
    #[test]
    fn a_windows_reg_value_is_read_backwards_and_never_guessed() {
        let dark = "HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\n    AppsUseLightTheme    REG_DWORD    0x0\n";
        let light = dark.replace("0x0", "0x1");
        assert_eq!(prefers_dark_from_reg(dark), Some(true));
        assert_eq!(prefers_dark_from_reg(&light), Some(false));
        // 大写十六进制前缀也要认（不同区域/版本的 reg.exe 会给 0X）
        assert_eq!(prefers_dark_from_reg(&dark.replace("0x0", "0X0")), Some(true));
        // 键在、值行被截断 → 不知道，而不是「非 0 就是浅色」
        assert_eq!(prefers_dark_from_reg("    AppsUseLightTheme    REG_DWORD\n"), None);
        assert_eq!(prefers_dark_from_reg("    AppsUseLightTheme    REG_SZ    yes\n"), None);
        // 查询失败时输出的是一句错误消息，里面连键名的那一行的形状都不成立
        assert_eq!(
            prefers_dark_from_reg("ERROR: The system was unable to find the specified registry key or value.\n"),
            None
        );
        assert_eq!(prefers_dark_from_reg(""), None);
    }

    /// GNOME 先问 `color-scheme`，只有它答不了（`gtk` 或缺失）才退到主题名。
    #[test]
    fn gnome_reads_the_explicit_setting_first_and_the_theme_name_only_as_fallback() {
        assert_eq!(prefers_dark_from_gsettings(Some("'prefer-dark'"), None), Some(true));
        assert_eq!(prefers_dark_from_gsettings(Some("'prefer-light'"), None), Some(false));
        assert_eq!(prefers_dark_from_gsettings(Some("'default'"), None), Some(false));
        assert_eq!(
            prefers_dark_from_gsettings(Some("'high-contrast'"), None),
            Some(false)
        );
        // `gtk` 把答案交给主题名：这一档必须往下看，不能就地判成浅色
        assert_eq!(
            prefers_dark_from_gsettings(Some("'gtk'"), Some("'Adwaita-dark'")),
            Some(true)
        );
        assert_eq!(
            prefers_dark_from_gsettings(Some("'gtk'"), Some("'Yaru'")),
            Some(false)
        );
        // 第一条缺失时同样看第二条；引号不带也要认（`--json-output` 就是这么给的）
        assert_eq!(prefers_dark_from_gsettings(None, Some("Adwaita-dark")), Some(true));
        // 两条都问不到 → 承认不知道
        assert_eq!(prefers_dark_from_gsettings(None, None), None);
        assert_eq!(prefers_dark_from_gsettings(Some("'gtk'"), None), None);
    }

    /// 「装了但没连」的那几张卡：标题已经是会话标签／连接名，产品名只在**标题没说清**时挂。
    #[test]
    fn a_product_name_is_only_shown_when_the_label_does_not_already_say_it() {
        // 标签只写了厂商名前缀 → 补全，这一格是真信息
        assert_eq!(vpn_app_for("Forti").as_deref(), Some("FortiClient"));
        assert_eq!(vpn_app_for("Palo Alto").as_deref(), Some("GlobalProtect"));
        // 标签本身就是这个软件的名字（差别只在空格、连字符、大小写）→ 不写第二遍
        assert_eq!(vpn_app_for("ProtonVPN"), None);
        assert_eq!(vpn_app_for("Tailscale Tunnel"), None);
        assert_eq!(vpn_app_for("tailscale"), None);
        // 认不出产品名 → None，界面退回通用的「VPN」；这里不存在「猜一家」的余地
        assert_eq!(vpn_app_for("我的那条隧道"), None);
        assert_eq!(vpn_app_for(""), None);
    }

    /// 抹分隔符这一步是上一条判据的全部权重：漏掉空格或大小写就会把「同一个软件写两遍」
    /// 当成两个不同的名字，界面上多出一枚重复的标签。
    #[test]
    fn separators_and_case_are_not_differences_when_comparing_names() {
        assert_eq!(app_letters("Proton VPN"), app_letters("protonvpn"));
        assert_eq!(app_letters("NordLynx-Home"), "nordlynxhome");
        // 非拉丁文字按「字母数字」保留，不能整段抹成空串
        assert_eq!(app_letters("办公室 Wi-Fi"), "办公室wifi");
        assert_eq!(app_letters("!!"), "");
    }
}
