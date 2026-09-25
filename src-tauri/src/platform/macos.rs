//! macOS 平台实现。
//!
//! 依赖系统命令：`networksetup` / `ipconfig` / `route` / `arp` / `ping` / `curl`，
//! 以及 `airport`（RSSI/BSSID）、`scutil --nc`（VPN/WireGuard 隧道清单与连接）、
//! `lpstat` / `lpoptions`（打印机清单与用户默认）与 `osascript`（授权框提权）。
//!
//! ⚠️ **SSID / 信号的来源随 macOS 版本被逐步收紧，单一来源必然在某代系统上失效**
//! 面板 SSID 空白、菜单里信号缺失这类故障的根因就是只取一个来源：
//!
//! | 版本 | `airport -I` | `networksetup -getairportnetwork` | `ipconfig getsummary` | `system_profiler` |
//! |---|---|---|---|---|
//! | ≤ 14.3 | SSID+RSSI+BSSID | SSID | SSID | 全量 |
//! | 14.4–14.5 | **已删除** | SSID | SSID | SSID+RSSI（BSSID 空） |
//! | 15.0–15.5 | 已删除 | **恒返回 "You are not associated…"** | SSID（约 46ms） | SSID+RSSI |
//! | 15.6+ | 已删除 | 同上 | **SSID 被涂成 `<redacted>`** | SSID+RSSI |
//!
//! 因此这里实现的是**逐级降级**而非单点取值：`networksetup` → `ipconfig getsummary`
//! → `system_profiler`（最后这个是 14.4 起唯一持续可用的来源，但慢，故带 2s 缓存）。
//!
//! 提权策略：优先 `/etc/sudoers.d/netsense` 授权的白名单包装脚本（免密、无弹窗），
//! 不可用时自动回落 `osascript ... with administrator privileges`，功能不中断。

use super::{
    extract_mac, parse_kv, poll_ssid_watch, printers_from_lpstat, run, sh_q, timeout_secs, Health,
    InterfaceStatus, NetworkPlatform, PrinterInfo, PrivChannel, ProbeTarget, TunnelTarget,
    WatcherHandle,
};
use crate::config::{Mode, NetworkConfig, V6Mode};
use crate::i18n;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// `airport` 的固定路径。macOS 14.4 起 Apple 已删除该工具（再调用只会拿到一条弃用
/// 警告文本），所以它只是「读得到就赚到」的第一来源，不再是必需路径。
const AIRPORT: &str =
    "/System/Library/PrivateFrameworks/Apple80211.framework/Versions/Current/Resources/airport";

/// `system_profiler` 结果缓存 TTL。单次调用 1~4s，而 SSID 监视线程每 2~5s 就会轮询一次、
/// 面板刷新也会打 `get_status` —— 不缓存会把后台线程长时间钉在等子进程上。
const SYS_PROFILER_TTL: Duration = Duration::from_secs(2);

/// 特权包装脚本路径（由 scripts/install-priv-helper.sh 安装）。
const PRIV_SCRIPT: &str = "/usr/local/libexec/netsense-priv.sh";

/// 结构化特权操作：只允许这些形状，**禁止任意 shell 透传**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivOp {
    SetDhcp {
        svc: String,
    },
    SetManual {
        svc: String,
        ip: String,
        netmask: String,
        gateway: String,
    },
    SetDns {
        svc: String,
        servers: Vec<String>,
    },
    SetV6Off {
        svc: String,
    },
    SetV6Auto {
        svc: String,
    },
    SetV6Manual {
        svc: String,
        addr: String,
        prefix: String,
        gateway: String,
    },
    RouteAdd {
        dest: String,
        gateway: String,
        metric: u32,
    },
    RouteDelete {
        dest: String,
    },
}

impl PrivOp {
    /// 免密通道编码：`op|f1|f2...`（与 netsense-priv.sh 的行格式一致）
    pub fn encode(&self) -> String {
        match self {
            PrivOp::SetDhcp { svc } => format!("setdhcp|{}", svc),
            PrivOp::SetManual {
                svc,
                ip,
                netmask,
                gateway,
            } => format!("setmanual|{}|{}|{}|{}", svc, ip, netmask, gateway),
            PrivOp::SetDns { svc, servers } => format!("setdns|{}|{}", svc, servers.join(",")),
            PrivOp::SetV6Off { svc } => format!("setv6off|{}", svc),
            PrivOp::SetV6Auto { svc } => format!("setv6auto|{}", svc),
            PrivOp::SetV6Manual {
                svc,
                addr,
                prefix,
                gateway,
            } => format!("setv6manual|{}|{}|{}|{}", svc, addr, prefix, gateway),
            PrivOp::RouteAdd {
                dest,
                gateway,
                metric,
            } => format!("routeadd|{}|{}|{}", dest, gateway, metric),
            PrivOp::RouteDelete { dest } => format!("routedel|{}", dest),
        }
    }

    /// 回落通道：osascript 授权框模式下拼成的等价 networksetup / route 命令。
    ///
    /// ⚠️ 这里拼出来的字符串最终会以 root 身份交给 `/bin/sh`（见 `run_via_osascript`），
    /// 所以**每一个插值参数都必须过 `sh_q`**。早期版本直接裸插值，配置文件里的
    /// `svc` / `dns` 只要带一个 `;` 就能以 root 执行任意命令。
    fn legacy_shell(&self) -> String {
        match self {
            PrivOp::SetDhcp { svc } => format!("networksetup -setdhcp {}", sh_q(svc)),
            PrivOp::SetManual {
                svc,
                ip,
                netmask,
                gateway,
            } => format!(
                "networksetup -setmanual {} {} {} {}",
                sh_q(svc),
                sh_q(ip),
                sh_q(netmask),
                sh_q(gateway)
            ),
            PrivOp::SetDns { svc, servers } => {
                if servers.is_empty() {
                    format!("networksetup -setdnsservers {} Empty", sh_q(svc))
                } else {
                    let list: Vec<String> = servers.iter().map(|s| sh_q(s)).collect();
                    format!("networksetup -setdnsservers {} {}", sh_q(svc), list.join(" "))
                }
            }
            PrivOp::SetV6Off { svc } => format!("networksetup -setv6off {}", sh_q(svc)),
            PrivOp::SetV6Auto { svc } => format!("networksetup -setv6automatic {}", sh_q(svc)),
            PrivOp::SetV6Manual {
                svc,
                addr,
                prefix,
                gateway,
            } => format!(
                "networksetup -setv6manual {} {} {} {}",
                sh_q(svc),
                sh_q(addr),
                sh_q(prefix),
                sh_q(gateway)
            ),
            PrivOp::RouteAdd {
                dest,
                gateway,
                metric,
            } => {
                if *metric > 0 {
                    format!(
                        "route -n add -net {} {} -metrics {}",
                        sh_q(dest),
                        sh_q(gateway),
                        metric
                    )
                } else {
                    format!("route -n add -net {} {}", sh_q(dest), sh_q(gateway))
                }
            }
            PrivOp::RouteDelete { dest } => format!("route -n delete -net {}", sh_q(dest)),
        }
    }
}

/// 当前特权通道：包装脚本存在即视为「已安装」；真正可用性由首次 `sudo -n` 结果决定。
pub fn priv_channel() -> PrivChannel {
    if std::path::Path::new(PRIV_SCRIPT).exists() {
        PrivChannel::Direct
    } else {
        PrivChannel::Prompt
    }
}

/// 提权执行一组结构化操作。
/// 优先 `sudo -n <priv script> --batch`（无弹窗，参数经脚本白名单二次校验）；
/// 免密不可用（未安装 / 需密码）时自动回落 osascript 授权框，功能不中断。
pub fn exec_ops(ops: &[PrivOp]) -> Result<(), String> {
    if ops.is_empty() {
        return Ok(());
    }
    let r = match priv_channel() {
        PrivChannel::Direct => match exec_ops_sudoers(ops) {
            Ok(()) => Ok(()),
            Err(e) if is_nopasswd_unavailable(&e) => exec_ops_osascript(ops),
            Err(e) => Err(e),
        },
        PrivChannel::Prompt => exec_ops_osascript(ops),
    };
    // 网络刚被本进程改动：丢掉状态快照。3A 的「下发 → 读回校验」紧跟着就要读一次
    // `get_status`，那份读数必须是下发**之后**的实况，否则校验屏障形同虚设。
    if r.is_ok() {
        super::invalidate_status();
    }
    r
}

fn is_nopasswd_unavailable(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("password is required")
        || s.contains("a terminal is required")
        || s.contains("no tty present")
        || s.contains("not allowed to execute")
}

fn exec_ops_sudoers(ops: &[PrivOp]) -> Result<(), String> {
    let mut payload = String::new();
    for op in ops {
        payload.push_str(&op.encode());
        payload.push('\n');
    }
    let mut child = Command::new("sudo")
        .arg("-n")
        .arg(PRIV_SCRIPT)
        .arg("--batch")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| i18n::tf("pal.spawn_priv_failed", &[("error", &e.to_string())]))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(payload.as_bytes())
            .map_err(|e| i18n::tf("pal.priv_stdin_failed", &[("error", &e.to_string())]))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| {
            i18n::tf("pal.wait_privileged_failed", &[("error", &e.to_string())])
        })?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            i18n::t("pal.privileged_no_stderr")
        } else {
            err
        })
    }
}

fn exec_ops_osascript(ops: &[PrivOp]) -> Result<(), String> {
    let chain = ops
        .iter()
        .map(|o| o.legacy_shell())
        .collect::<Vec<_>>()
        .join(" ; ");
    run_via_osascript(&chain).map(|_| ())
}

/// 把一份配置编译成按序执行的特权操作，不做任何 I/O。
///
/// DNS 是这里唯一**三态**的字段：`dns` 缺失就不产生 DNS 操作（服务上现在挂着什么
/// 就继续用什么），`dns: ""` 才是一条清空指令。两者在 `networksetup` 那边长得完全不同
/// （前者什么都不做，后者是 `-setdnsservers <svc> Empty`），所以别在编译期把它们抹平。
fn apply_ops(svc: &str, p: &NetworkConfig) -> Result<Vec<PrivOp>, String> {
    let mut ops: Vec<PrivOp> = Vec::new();
    match p.mode {
        Mode::Manual => {
            let ip = p
                .ip
                .clone()
                .ok_or_else(|| i18n::tf("pal.manual_missing", &[("field", "ip")]))?;
            let netmask = p
                .netmask
                .clone()
                .ok_or_else(|| i18n::tf("pal.manual_missing", &[("field", "netmask")]))?;
            let gateway = p
                .gateway
                .clone()
                .ok_or_else(|| i18n::tf("pal.manual_missing", &[("field", "gateway")]))?;
            ops.push(PrivOp::SetManual {
                svc: svc.to_string(),
                ip,
                netmask,
                gateway,
            });
        }
        Mode::Dhcp => ops.push(PrivOp::SetDhcp { svc: svc.to_string() }),
    }
    if let Some(dns) = p.dns.as_deref() {
        let servers: Vec<String> = dns
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        ops.push(PrivOp::SetDns {
            svc: svc.to_string(),
            servers,
        });
    }
    match p.v6mode {
        Some(V6Mode::Off) => ops.push(PrivOp::SetV6Off { svc: svc.to_string() }),
        Some(V6Mode::Automatic) => ops.push(PrivOp::SetV6Auto { svc: svc.to_string() }),
        Some(V6Mode::Manual) => {
            let addr = p
                .ipv6
                .clone()
                .ok_or_else(|| i18n::tf("pal.v6_manual_missing", &[("field", "ipv6")]))?;
            let prefix = p
                .v6prefix
                .clone()
                .ok_or_else(|| i18n::tf("pal.v6_manual_missing", &[("field", "v6prefix")]))?;
            let gateway = p
                .v6gateway
                .clone()
                .ok_or_else(|| i18n::tf("pal.v6_manual_missing", &[("field", "v6gateway")]))?;
            ops.push(PrivOp::SetV6Manual {
                svc: svc.to_string(),
                addr,
                prefix,
                gateway,
            });
        }
        None => {}
    }
    Ok(ops)
}

/// 提权执行用户脚本时拼给 root shell 的命令行。
///
/// 与 `legacy_shell()` 一样，这里的结果最终会进 `/bin/sh`（且是 root），所以每个分量
/// 都必须单独 `sh_q` 引用——不能先 join 再整体转义，那样各 token 边界丢失，`;`、`$()`
/// 依然会被 shell 按语法解释。
/// `open -a` 的参数拼装，单列出来是因为这里有个不显然的坑：
/// `open -a App X Y` 里的 X / Y 是「请用该应用**打开的文件**」，不是应用的启动参数，
/// 而以 `-` 开头的那些还会被 `open` 自己当成它的选项吃掉。要给应用传参必须走 `--args`。
fn open_args(app: &str, args: &[String]) -> Vec<String> {
    let mut v: Vec<String> = vec!["-a".to_string(), app.to_string()];
    if !args.is_empty() {
        v.push("--args".to_string());
        v.extend(args.iter().cloned());
    }
    v
}

fn elevated_script_line(path: &str, args: &[String]) -> String {
    std::iter::once(path)
        .chain(args.iter().map(|s| s.as_str()))
        .map(sh_q)
        .collect::<Vec<_>>()
        .join(" ")
}

/// 系统授权框执行（macOS GUI）：经 osascript 弹出授权，一次覆盖多条命令。
fn run_via_osascript(shell_cmd: &str) -> Result<String, String> {
    let escaped = shell_cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let osa = format!(
        "do shell script \"{}\" with administrator privileges",
        escaped
    );
    run("osascript", &["-e", &osa])
}

/// 解析默认网关 IP（route -n get default → gateway:）。
fn default_gateway() -> Option<String> {
    let out = run("route", &["-n", "get", "default"]).ok()?;
    for line in out.lines() {
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix("gateway:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// 由网关 IP 解析其 MAC（arp -n）。
fn gateway_mac_for(gw: &str) -> Option<String> {
    let out = run("arp", &["-n", gw]).ok()?;
    for line in out.lines() {
        let line = line.trim();
        if line.contains("(incomplete)") || line.contains("no entry") {
            continue;
        }
        if let Some(m) = extract_mac(line) {
            return Some(m);
        }
    }
    None
}

/// 解析 Wi-Fi 网络服务名（networksetup -listallnetworkservices）。
/// 输出可能带 `*`（表示已关闭的服务）前缀，需剥离。
fn wifi_service() -> Option<String> {
    let out = run("networksetup", &["-listallnetworkservices"]).ok()?;
    for line in out.lines() {
        let l = line.trim_start_matches('*').trim();
        if l.is_empty() {
            continue;
        }
        if l.contains("Wi-Fi") || l.contains("AirPort") {
            return Some(l.to_string());
        }
    }
    None
}

/// 查询某个网络服务的 DNS（`networksetup -getdnsservers`）。
///
/// 未设置时命令会打印 `There aren't any DNS Servers set on <svc>.` 或 `Empty`，
/// 这两种都不是 DNS 地址，归一成 `None`。
fn dns_of_service(svc: &str) -> Option<String> {
    let o = run("networksetup", &["-getdnsservers", svc]).ok()?;
    let dns = o.trim();
    if dns.starts_with("There") || dns == "Empty" || dns.is_empty() {
        return None;
    }
    Some(dns.split_whitespace().collect::<Vec<_>>().join(","))
}

/// 某个网络服务当前的**全局** IPv6 地址（`networksetup -getinfo` 的 `IPv6 IP address:`）。
///
/// 判据直接问系统，而不去翻 `ifconfig` 的输出：
/// - `IPv6 IP address: none` 是「这个口上确实没有 v6 地址」的权威回答，而 `ifconfig`
///   里只有 `fe80::` 一条链路本地地址 —— 前端无从判断那算不算「有 IPv6」；
/// - `fe80::` 与隐私临时地址（每分钟轮换）都不适合作为展示值。
///
/// 注意键名是 `IPv6 IP address:`，不是 `IPv6:`（后者是 off / automatic / manual 模式）。
fn v6_of_service(svc: &str) -> Option<String> {
    let o = run("networksetup", &["-getinfo", svc]).ok()?;
    for line in o.lines() {
        let Some(rest) = line.trim().strip_prefix("IPv6 IP address:") else {
            continue;
        };
        let v = rest.trim();
        if v.eq_ignore_ascii_case("none")
            || v.is_empty()
            || v.to_ascii_lowercase().starts_with("fe80:")
        {
            continue;
        }
        return Some(v.split('%').next().unwrap_or(v).to_string());
    }
    None
}

fn probe_icmp(target: Option<&str>, timeout_ms: u64) -> bool {
    let t = target.unwrap_or("223.5.5.5");
    let secs = timeout_secs(timeout_ms);
    // BSD ping：-c 次数，-t 超时(秒)
    run("ping", &["-c", "1", "-t", &secs, t]).is_ok()
}

fn probe_http(target: Option<&str>, timeout_ms: u64) -> bool {
    let url = match target {
        Some(u) if !u.is_empty() => u,
        _ => return false,
    };
    let secs = timeout_secs(timeout_ms);
    match run(
        "curl",
        &["-sS", "-m", &secs, "-o", "/dev/null", "-w", "%{http_code}", url],
    ) {
        Ok(code) => match code.trim().parse::<u16>() {
            Ok(c) => (200..400).contains(&c),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// 无线接口名。**绝不能硬编码 en0**：带以太网的机型（iMac / Mac mini / Mac Studio）上
/// en0 是以太网、Wi-Fi 落在 en1，硬编码会让 SSID / IP / 网关全部取错接口。
/// 设备名只解析一次并缓存。
///
/// 取法与 Apple 社区通行写法一致：
/// `networksetup -listallhardwareports | awk '/Wi-Fi|AirPort/{getline; print $NF}'`
fn wifi_iface() -> String {
    static IFACE: OnceLock<String> = OnceLock::new();
    IFACE
        .get_or_init(|| discover_wifi_device().unwrap_or_else(|| "en0".to_string()))
        .clone()
}

/// 从 `networksetup -listallhardwareports` 中取 Wi-Fi 的设备名：
/// ```text
/// Hardware Port: Wi-Fi
/// Device: en0
/// ```
/// 端口名在本地化系统上可能是 `Wi-Fi` / `AirPort` / `无线网络`，三种都认。
fn discover_wifi_device() -> Option<String> {
    let out = run("networksetup", &["-listallhardwareports"]).ok()?;
    let mut is_wifi_port = false;
    for line in out.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("Hardware Port:") {
            let port = rest.trim();
            is_wifi_port = port.contains("Wi-Fi")
                || port.contains("WiFi")
                || port.contains("AirPort")
                || port.contains("无线"); // i18n-exempt: 匹配中文版 macOS 自己报的硬件口名，不是界面文案
        } else if is_wifi_port {
            if let Some(dev) = l.strip_prefix("Device:") {
                let dev = dev.trim();
                if !dev.is_empty() {
                    return Some(dev.to_string());
                }
            }
        }
    }
    None
}

/// 按首个冒号切一行成 `(key, value)`；半角与全角冒号都认（本地化系统会把整句提示
/// 翻成中文）。这里刻意不用 `parse_kv`：它要求 key 精确前缀匹配，扛不住
/// ` SSID : x` 这种多余空格，也会被 `BSSID` 这类「key 是另一个 key 后缀」的行带偏。
fn split_kv(line: &str) -> Option<(&str, &str)> {
    // 记录冒号**起始**字节位与其字节长度：全角「：」占 3 字节，切片必须按它自身的
    // 长度前进，否则会把 `k：v` 切成 `k` 和 `v` 时算错边界（甚至切在 UTF-8 中间 panic）。
    let (at, colon) = line.char_indices().find(|(_, c)| *c == ':' || *c == '：')?;
    Some((line[..at].trim(), line[at + colon.len_utf8()..].trim()))
}

/// 取某行里 key 精确匹配（忽略大小写）的冒号后取值。
fn value_of(out: &str, key: &str) -> Option<String> {
    let want = key.trim();
    for line in out.lines() {
        if let Some((k, v)) = split_kv(line) {
            if k.eq_ignore_ascii_case(want) {
                if let Some(s) = sanitize_ssid(v) {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// 把「读不到」的各种表现统一归一成 `None`，否则会把提示语当成网络名显示出来
/// （SSID 区看起来「有内容却不对 / 空白」，多半就是这里没归一）。
///
/// 实测会遇到的形态：
/// - macOS 15.0+ `networksetup` 恒返回 `You are not associated with an AirPort network.`
/// - macOS 15.6+ `ipconfig getsummary` 把 SSID 涂成 `<redacted>`
/// - 未设置时 `networksetup -getdnsservers` 式的 `There aren't any …` 提示
///
/// 只有归一成 `None`，上层才会继续走下一级降级链。
fn sanitize_ssid(v: &str) -> Option<String> {
    let t = v.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if lower == "<redacted>" || lower == "redacted" {
        return None;
    }
    for bad in [
        "not associated",
        "not connected",
        "you are not",
        "there aren't",
        "does not exist",
        "no such",
    ] {
        if lower.contains(bad) {
            return None;
        }
    }
    // SSID 允许空格，所以不能靠「有没有空格」判断；但提示语都是长句且以句号收尾。
    // i18n-exempt: '。' 是在匹配中文版 macOS 自己写出的提示句尾，不是界面文案
    if (t.ends_with('.') || t.ends_with('。')) && t.chars().count() > 24 {
        return None;
    }
    Some(t.to_string())
}

/// `system_profiler SPAirPortDataType` 提取出的无线信息。
#[derive(Debug, Clone, Default)]
struct RadioInfo {
    ssid: Option<String>,
    rssi: Option<i32>,
    bssid: Option<String>,
}

/// `airport -I`：≤ macOS 14.3 的完整来源（14.4 起该命令已被 Apple 删除，
/// 调用只会拿到弃用警告，故返回 `(None, None)` 让调用方继续降级）。
fn airport_info(dev: &str) -> (Option<i32>, Option<String>) {
    let out = match run(AIRPORT, &["-I", dev]) {
        Ok(o) => o,
        Err(_) => return (None, None),
    };
    let rssi = parse_kv(&out, "agrCtlRSSI").and_then(|v| v.trim().parse::<i32>().ok());
    let bssid = parse_kv(&out, "BSSID").and_then(|v| extract_mac(&v));
    (rssi, bssid)
}

/// `system_profiler` 结果，带 TTL 缓存（见 `SYS_PROFILER_TTL`）。
///
/// 这是 macOS 14.4 之后唯一持续可用的 SSID/RSSI 来源，但单次调用 1~4s；缓存让
/// 「面板刷新 + 监视轮询」在同一时间窗内只付一次代价。2s 的窗口短于监视轮的 2~5s
/// 间隔，因此对「SSID 变化」的判定延迟没有可见影响。
fn system_profiler_info() -> RadioInfo {
    static CACHE: Mutex<Option<(Instant, RadioInfo)>> = Mutex::new(None);

    {
        let guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, info)) = guard.as_ref() {
            if at.elapsed() < SYS_PROFILER_TTL {
                return info.clone();
            }
        }
    }

    let fresh = run("system_profiler", &["SPAirPortDataType"])
        .map(|out| parse_system_profiler(&out))
        .unwrap_or_default();

    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some((Instant::now(), fresh.clone()));
    fresh
}

/// 解析 `system_profiler SPAirPortDataType` 的文本输出。
///
/// 关键点：**SSID 是输出里的「键」，不是某行的值**：
/// ```text
///       Current Network Information:
///         Office_5G:                    <- 这一行的名字才是 SSID
///           Signal / Noise: -55 dBm / -92 dBm
///           BSSID: aa:bb:cc:dd:ee:ff
///       Other Local Wi-Fi Networks:     <- 同级，说明当前网络那段到此为止
/// ```
/// 所以按缩进判块：进入 `Current Network Information:` 后，第一条缩进更深、以冒号
/// 结尾的行即 SSID；遇到同级或更浅的行就结束该段 —— 否则会把邻近网络的 SSID 读进来
/// （那比读不到更糟：会匹配到错误的 profile）。
/// 该段内的 `Signal / Noise` 与 `BSSID` 一并取出。
fn parse_system_profiler(out: &str) -> RadioInfo {
    let mut info = RadioInfo::default();
    let mut block_indent: Option<usize> = None;

    for raw in out.lines() {
        let indent = raw.len() - raw.trim_start().len();
        let l = raw.trim();
        if l.is_empty() {
            continue;
        }

        let Some(hi) = block_indent else {
            if l.starts_with("Current Network Information") {
                block_indent = Some(indent);
                // 少数版本把名字写在同一行冒号后；多数版本写在下一行。
                if let Some((_, v)) = split_kv(l) {
                    info.ssid = sanitize_ssid(v);
                }
            }
            continue;
        };

        if indent <= hi {
            // 当前网络这一段结束
            block_indent = None;
            if l.starts_with("Current Network Information") {
                block_indent = Some(indent);
            }
            continue;
        }

        if info.ssid.is_none() && l.ends_with(':') {
            info.ssid = sanitize_ssid(l.trim_end_matches(':'));
            continue;
        }
        if let Some((k, v)) = split_kv(l) {
            if k.eq_ignore_ascii_case("Signal / Noise") && info.rssi.is_none() {
                // 形如 "-55 dBm / -92 dBm"，只取信号那一半
                info.rssi = v
                    .split('/')
                    .next()
                    .map(|s| s.trim().trim_end_matches("dBm").trim())
                    .and_then(|s| s.parse::<i32>().ok());
            } else if k.eq_ignore_ascii_case("BSSID") && info.bssid.is_none() {
                info.bssid = extract_mac(v);
            }
        }
    }
    info
}

/// 来源 1：`networksetup -getairportnetwork <dev>`，输出 `Current Wi-Fi Network: X`。
/// ≤ macOS 14 可靠；15.0 起恒返回 "You are not associated with an AirPort network."
/// （被 `sanitize_ssid` 归一成 `None`，从而自动继续降级）。
fn ssid_via_networksetup(dev: &str) -> Option<String> {
    let out = run("networksetup", &["-getairportnetwork", dev]).ok()?;
    value_of(&out, "Current Wi-Fi Network").or_else(|| {
        // 服务名被用户改过、或系统本地化时整行前缀会变，此时退化为「整行看起来像
        // 网络名提示语时取冒号后的值」——「未关联」那句没有冒号且会被 sanitize 拦掉。
        out.lines().find_map(|l| {
            let l = l.trim();
            if l.contains("Network") || l.contains("网络") { // i18n-exempt: 匹配中文版系统输出的行标签，不是界面文案
                split_kv(l).and_then(|(_, v)| sanitize_ssid(v))
            } else {
                None
            }
        })
    })
}

/// 来源 2：`ipconfig getsummary <dev>`，输出 `  SSID : X`。
/// macOS 15.0–15.5 可用且约 46ms（比 system_profiler 快两个数量级）；
/// 15.6 起该字段被涂黑，同样由 `sanitize_ssid` 拦下后继续降级。
fn ssid_via_ipconfig(dev: &str) -> Option<String> {
    let out = run("ipconfig", &["getsummary", dev]).ok()?;
    value_of(&out, "SSID")
}

/// RSSI/BSSID：`airport`（≤14.3）→ `system_profiler`（14.4 起唯一来源）。
/// 两者各取所需，任一侧缺失就补另一侧。
fn radio_info(dev: &str) -> (Option<i32>, Option<String>) {
    let (rssi, bssid) = airport_info(dev);
    if rssi.is_some() && bssid.is_some() {
        return (rssi, bssid);
    }
    let sp = system_profiler_info();
    (rssi.or(sp.rssi), bssid.or(sp.bssid))
}

/// `networksetup -listallhardwareports` 的一条记录：
/// ```text
/// Hardware Port: Wi-Fi
/// Device: en0
/// Ethernet Address: a1:b2:c3:d4:e5:f6
/// ```
struct HwPort {
    port: String,
    dev: String,
    mac: Option<String>,
}

/// 全部硬件端口（含设备名与网卡 MAC）。
fn hardware_ports() -> Vec<HwPort> {
    let Ok(out) = run("networksetup", &["-listallhardwareports"]) else {
        return Vec::new();
    };
    let mut list = Vec::new();
    let mut port: Option<String> = None;
    let mut dev: Option<String> = None;
    let mut mac: Option<String> = None;
    // 每条记录以 `Hardware Port:` 开头；遇到下一条或 `VLAN Configurations` 之类的
    // 分隔段落（缩进的 `*` 列表）时收口。用「下一条 Hardware Port」作为边界即可。
    let flush = |list: &mut Vec<HwPort>, port: &mut Option<String>, dev: &mut Option<String>, mac: &mut Option<String>| {
        if let (Some(p), Some(d)) = (port.take(), dev.take()) {
            list.push(HwPort { port: p, dev: d, mac: mac.take() });
        }
        *mac = None;
    };
    for raw in out.lines() {
        let l = raw.trim();
        if let Some(rest) = l.strip_prefix("Hardware Port:") {
            flush(&mut list, &mut port, &mut dev, &mut mac);
            port = Some(rest.trim().to_string());
        } else if let Some(rest) = l.strip_prefix("Device:") {
            dev = Some(rest.trim().to_string());
        } else if let Some(rest) = l.strip_prefix("Ethernet Address:") {
            mac = extract_mac(rest).or_else(|| {
                let t = rest.trim();
                if t.is_empty() { None } else { Some(t.to_string()) }
            });
        }
    }
    flush(&mut list, &mut port, &mut dev, &mut mac);
    list
}

/// 全部网络服务名（含 VPN 这类软件创建的服务）。
fn network_services() -> Vec<String> {
    let Ok(out) = run("networksetup", &["-listallnetworkservices"]) else {
        return Vec::new();
    };
    out.lines()
        .skip(1) // 首行是说明文字（"An asterisk (*) denotes..."）
        .map(|l| l.trim_start_matches('*').trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// 系统上所有 BSD 设备名（`ifconfig -l`）。
fn all_devices() -> Vec<String> {
    let Ok(out) = run("ifconfig", &["-l"]) else {
        return Vec::new();
    };
    out.split_whitespace().map(|s| s.to_string()).collect()
}

/// 这些设备是系统内部用途（回环 / AirDrop / 桥接 / 虚拟机网络…），
/// 展示出来只会把"当前连着哪些网"这个问题搅浑。
fn is_noise_device(dev: &str) -> bool {
    let d = dev.to_ascii_lowercase();
    // `en0`/`en1` 是真实网卡，故以太网要用前缀+数字边界判断，不能简单 starts_with("en")
    for p in ["lo", "awdl", "llw", "anpi", "stf", "gif", "bridge", "vmenet", "p2p", "wds", "ap"] {
        if d.starts_with(p) {
            return true;
        }
    }
    false
}

/// 默认路由表：`设备名 -> 网关 IP`。
///
/// `netstat -rn -f inet` 的 default 行形如：
/// ```text
/// default            192.168.1.1        UGScg                  en0
/// default            10.8.0.1           UGScIg                utun4
/// ```
/// 只有**带默认网关**的那张网卡才需要展示网关信息，其余留空（避免把别的接口
/// 的路由张冠李戴）。
fn default_routes() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(out) = run("netstat", &["-rn", "-f", "inet"]) else {
        return map;
    };
    for line in out.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() >= 4 && t[0] == "default" {
            let gw = t[1];
            let dev = t[t.len() - 1];
            // 直连路由（link#N）与重复项跳过
            if gw.contains('.') && !map.contains_key(dev) {
                map.insert(dev.to_string(), gw.to_string());
            }
        }
    }
    map
}

/// 设备名 → 种类。无线优先按硬件端口名判断（本地化系统上是「Wi-Fi」/「无线局域网」），
/// 隧道设备按 BSD 设备名前缀判断。
fn kind_of(dev: &str, port: Option<&str>) -> super::NicKind {
    if super::is_tunnel_device(dev) {
        return super::NicKind::Vpn;
    }
    if let Some(p) = port {
        let p = p.to_ascii_lowercase();
        if p.contains("wi-fi") || p.contains("wifi") || p.contains("airport") || p.contains("无线") { // i18n-exempt: 匹配中文版 macOS 的硬件口名，不是界面文案
            return super::NicKind::Wireless;
        }
        if p.contains("ethernet") || p.contains("thunderbolt") || p.contains("usb") || p.contains("以太") { // i18n-exempt: 匹配中文版 macOS 的硬件口名，不是界面文案
            return super::NicKind::Wired;
        }
        // 硬件端口名未知但设备名是 en*：macOS 上这就是以太网
        if dev.starts_with("en") {
            return super::NicKind::Wired;
        }
    } else if dev.starts_with("en") {
        return super::NicKind::Wired;
    }
    super::NicKind::Other
}

/// 猜 VPN 网卡归属的软件名。
///
/// macOS 没有公开的「utun → 进程」映射，可行的两条线索：
/// 1. **按 IP 认领**：VPN 客户端创建网络服务时用的是自己的名字（Tailscale /
///    WireGuard / Cisco AnyConnect …），这些服务出现在 `networksetup
///    -listallnetworkservices` 里、但**不在** `-listallhardwareports` 里（不是硬件）。
///    逐个问 `-getinfo`，IP 与隧道设备一致的那个服务即归属软件。
/// 2. 无 IP 或无人认领时，按服务名关键词猜（[`super::guess_vpn_app`]）。
fn vpn_app_for(dev: &str, ip: Option<&str>, hw_ports: &[HwPort]) -> Option<String> {
    let hw: std::collections::HashSet<&str> = hw_ports.iter().map(|p| p.port.as_str()).collect();
    let candidates: Vec<String> = network_services()
        .into_iter()
        .filter(|s| !hw.contains(s.as_str()))
        .collect();
    if candidates.is_empty() {
        let _ = dev;
        return None;
    }

    if let Some(addr) = ip {
        if !addr.is_empty() {
            for svc in &candidates {
                if let Ok(info) = run("networksetup", &["-getinfo", svc]) {
                    if let Some(v) = value_of(&info, "IP address") {
                        if v == addr {
                            return Some(svc.clone());
                        }
                    }
                }
            }
        }
    }
    for svc in &candidates {
        if let Some(app) = super::guess_vpn_app(svc) {
            return Some(app.to_string());
        }
    }
    if candidates.len() == 1 {
        return Some(candidates[0].clone());
    }
    None
}

/// 设备名 → 网络服务名（供 `networksetup -setdhcp` 等写入操作定位）。
fn service_for_dev(dev: &str, hw_ports: &[HwPort]) -> Option<String> {
    hw_ports
        .iter()
        .find(|p| p.dev == dev)
        .map(|p| p.port.clone())
}

// —————————————————————————— macOS 实现 ——————————————————————————

/// 无状态：`Copy` 让它可以按值传给后台线程（健康度监测）而不必套 `Arc`。
#[derive(Debug, Clone, Copy)]
pub struct MacPlatform;

/// 清单里属于目标隧道的那一行。
///
/// 真实形态（本机抓取，两种状态各一条）：
///
/// ```text
/// * (Connected)      07CD635E-… VPN (io.tailscale.ipn.macsys) "Tailscale"   [VPN:io.tailscale.ipn.macsys]
/// * (Disconnected)   2C5C591F-… VPN (ch.protonvpn.mac) "ProtonVPN"          [VPN:ch.protonvpn.mac]
/// ```
///
/// 所以匹配键是**双引号里的用户可见标签**，厂商在整行文本里做包含匹配。用
/// `list_interfaces()` 的设备名（`utun4`）当键是错的：用户写的是自己在 VPN 软件里
/// 看到的那个名字。
fn scutil_nc_rows(list_text: &str) -> Vec<(String, String)> {
    list_text
        .lines()
        .filter_map(|line| {
            let label = quoted_token(line)?;
            Some((label.to_string(), line.to_string()))
        })
        .collect()
}

/// 目标隧道在清单里的那一行，以及厂商提示是否也对得上。
fn scutil_nc_row<'a>(
    rows: &'a [(String, String)],
    target: &TunnelTarget,
) -> Option<super::TunnelRow<'a>> {
    super::find_tunnel_row(rows, target)
}

/// 取一行里第一段双引号包裹的文本（没有成对引号或内容为空时返回 `None`）。
fn quoted_token(line: &str) -> Option<&str> {
    // 至少要有 3 段才算「有一对引号」：`a "b` 这种残缺行不能当作标签为 b
    if line.split('"').count() < 3 {
        return None;
    }
    let inner = line.split('"').nth(1)?;
    (!inner.trim().is_empty()).then_some(inner.trim())
}

/// 这一行 `scutil --nc list` 条目是否处于已连接状态。
fn scutil_row_connected(line: &str) -> bool {
    line.contains("(Connected)")
}

/// 清单里全部隧道的用户可见标签（报错时列给用户看，省得他一条条试）。
fn scutil_nc_labels() -> Vec<String> {
    let Ok(out) = run("/usr/sbin/scutil", &["--nc", "list"]) else {
        return Vec::new();
    };
    scutil_nc_rows(&out)
        .into_iter()
        .map(|(label, _)| label)
        .collect()
}

/// 一次 `scutil --nc list` 的判定结果。不把行文本带出去，省掉一串生命周期问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NcMatch {
    found: bool,
    provider_matched: bool,
    connected: bool,
}

/// 查清单并把匹配做完：`tunnel_is_up` 与 `tunnel_connect` 都要问同一件事，
/// 分开写会让两处的判据慢慢长歪（一个认厂商、一个不认，就会出现「永远连不上」）。
fn scutil_match(target: &TunnelTarget) -> NcMatch {
    let Ok(out) = run("/usr/sbin/scutil", &["--nc", "list"]) else {
        return NcMatch { found: false, provider_matched: false, connected: false };
    };
    let rows = scutil_nc_rows(&out);
    match scutil_nc_row(&rows, target) {
        Some(row) => NcMatch {
            found: true,
            provider_matched: row.provider_matched,
            connected: scutil_row_connected(row.text),
        },
        None => NcMatch { found: false, provider_matched: false, connected: false },
    }
}

/// 采一份主无线网卡的状态快照 —— [`NetworkPlatform::get_status`] 的真实工作。
///
/// 单独成函数只为套上 TTL 缓存：一次快照要起 6~7 个子进程（`networksetup` / `ipconfig`
/// / `route` / `arp`），而面板每刷新一次就要一份。改动网络的路径（[`exec_ops`]）会显式
/// 丢掉缓存，所以 3A 的「下发 → 读回校验」拿到的始终是下发之后的实况。
fn read_status() -> InterfaceStatus {
    let dev = wifi_iface();
    let ssid = MacPlatform.get_current_ssid();
    let (rssi, bssid) = radio_info(&dev);

    let mut st = InterfaceStatus {
        ssid,
        rssi,
        bssid,
        iface: Some(dev.clone()),
        ..Default::default()
    };

    // 连通性判据必须独立于 SSID：macOS 15.6+ 与非授权进程都可能读不到 SSID，
    // 若沿用「ssid.is_some()」判连接，整面板会显示离线、按 SSID 的 profile 也永远
    // 匹配不上。接口拿到 IPv4 地址即为已连接。
    //
    // 注意这两条 ipconfig 不依赖网络服务名，故不再挂在 `wifi_service()` 之下
    // （服务名只在 DNS / IPv6 查询时才需要）。
    if let Ok(o) = run("ipconfig", &["getifaddr", &dev]) {
        let v = o.trim();
        if !v.is_empty() {
            st.ipv4 = Some(v.to_string());
        }
    }
    if let Ok(o) = run("ipconfig", &["getoption", &dev, "subnet_mask"]) {
        let v = o.trim();
        if !v.is_empty() {
            st.netmask = Some(v.to_string());
        }
    }
    st.connected = st.ssid.is_some() || st.ipv4.is_some();

    if let Some(svc) = wifi_service() {
        if let Ok(o) = run("networksetup", &["-getdnsservers", &svc]) {
            let dns = o.trim();
            // 未设置时输出英文提示语（"There aren't any DNS Servers set on ..."）或 "Empty"
            if !dns.starts_with("There") && dns != "Empty" {
                st.dns = Some(dns.split_whitespace().collect::<Vec<_>>().join(","));
            }
        }
        if let Ok(o) = run("networksetup", &["-getinfo", &svc]) {
            st.v6mode = parse_kv(&o, "IPv6:").or(Some("automatic".into()));
        }
    }
    if let Some(gw) = default_gateway() {
        st.gateway = Some(gw.clone());
        st.gateway_mac = gateway_mac_for(&gw);
    }
    st
}

impl NetworkPlatform for MacPlatform {
    fn watch_ssid(&self, cb: Box<dyn Fn(Option<String>) + Send + Sync>) -> WatcherHandle {
        // MacPlatform 为 ZST，直接 move 进线程（不捕获 &self，生命周期非 'static）
        poll_ssid_watch(|| MacPlatform.get_current_ssid(), cb)
    }

    /// 逐级降级取 SSID，顺序 = 由快到慢、由准到糙（版本矩阵见文件头注释）：
    /// `networksetup` → `ipconfig getsummary` → `system_profiler`。
    ///
    /// 单点取值的代价：只用 `networksetup` 时，macOS 15.0 起
    /// 会稳定拿到 "You are not associated with an AirPort network."，于是 SSID 区永远空白。
    fn get_current_ssid(&self) -> Option<String> {
        let dev = wifi_iface();
        ssid_via_networksetup(&dev)
            .or_else(|| ssid_via_ipconfig(&dev))
            .or_else(|| system_profiler_info().ssid)
    }

    fn get_status(&self) -> InterfaceStatus {
        super::cached_status(read_status)
    }

    fn fresh_status(&self) -> InterfaceStatus {
        read_status()
    }

    fn apply_network(&self, p: &NetworkConfig) -> Result<(), String> {
        let svc = wifi_service().ok_or_else(|| i18n::t("pal.no_wifi_service"))?;
        exec_ops(&apply_ops(&svc, p)?)
    }

    fn set_dhcp(&self) -> Result<(), String> {
        let svc = wifi_service().ok_or_else(|| i18n::t("pal.no_wifi_service"))?;
        exec_ops(&[
            PrivOp::SetDhcp { svc: svc.clone() },
            PrivOp::SetDns {
                svc,
                servers: Vec::new(),
            },
        ])
    }

    /// 按**设备名**切回 DHCP。
    ///
    /// 设备 → 网络服务的映射来自 `-listallhardwareports`（服务名即硬件端口名）。
    /// 隧道设备（utun*）没有网络服务，改 DHCP 对它没有意义，直接告知不支持。
    fn set_dhcp_for(&self, dev: &str) -> Result<(), String> {
        if super::is_tunnel_device(dev) {
            return Err(i18n::tf("pal.vpn_iface_not_switchable", &[("dev", dev)]));
        }
        let hw = hardware_ports();
        let svc = service_for_dev(dev, &hw).or_else(wifi_service).ok_or_else(|| {
            i18n::tf("pal.no_service_for_iface", &[("dev", dev)])
        })?;
        exec_ops(&[
            PrivOp::SetDhcp { svc: svc.clone() },
            PrivOp::SetDns {
                svc,
                servers: Vec::new(),
            },
        ])
    }

    /// 枚举当前在用的全部网卡。
    ///
    /// 「在用」= 拿到 IPv4，或者是已关联的无线网卡（刚连上还没拿到地址的瞬间也要能看到）。
    /// 每次调用会拉起若干子进程，故整体经 [`super::cached_nics`] 做 TTL 缓存
    /// —— 面板每次状态广播都要一份快照。
    fn list_interfaces(&self) -> Vec<super::NicInfo> {
        super::cached_nics(|| {
            let hw = hardware_ports();
            let routes = default_routes();
            let wifi_dev = wifi_iface();
            let mut out: Vec<super::NicInfo> = Vec::new();

            for dev in all_devices() {
                if is_noise_device(&dev) {
                    continue;
                }
                let port = hw.iter().find(|p| p.dev == dev);
                let kind = kind_of(&dev, port.map(|p| p.port.as_str()));
                let ipv4 = run("ipconfig", &["getifaddr", &dev])
                    .ok()
                    .map(|o| o.trim().to_string())
                    .filter(|s| !s.is_empty());
                let netmask = run("ipconfig", &["getoption", &dev, "subnet_mask"])
                    .ok()
                    .map(|o| o.trim().to_string())
                    .filter(|s| !s.is_empty());
                let ssid = if kind == super::NicKind::Wireless && dev == wifi_dev {
                    self.get_current_ssid()
                } else {
                    None
                };
                if ipv4.is_none() && ssid.is_none() {
                    continue;
                }

                let gateway = routes.get(&dev).cloned();
                let gateway_mac = gateway.as_deref().and_then(gateway_mac_for);
                let dns = port.and_then(|p| dns_of_service(&p.port));
                let ipv6 = port.and_then(|p| v6_of_service(&p.port));
                let app = if kind == super::NicKind::Vpn {
                    vpn_app_for(&dev, ipv4.as_deref(), &hw)
                } else {
                    None
                };

                out.push(super::NicInfo {
                    name: dev,
                    label: port.map(|p| p.port.clone()),
                    kind,
                    up: true,
                    ssid,
                    mac: port.and_then(|p| p.mac.clone()),
                    ipv4,
                    netmask,
                    ipv6,
                    gateway,
                    gateway_mac,
                    dns,
                    app,
                });
            }

            // 展示顺序：无线 → 有线 → VPN → 其它；同类按设备名（en0 先于 en5）
            let rank = |k: super::NicKind| match k {
                super::NicKind::Wireless => 0,
                super::NicKind::Wired => 1,
                super::NicKind::Vpn => 2,
                super::NicKind::Other => 3,
            };
            out.sort_by(|a, b| {
                rank(a.kind)
                    .cmp(&rank(b.kind))
                    .then_with(|| a.name.cmp(&b.name))
            });
            out
        })
    }

    fn probe(&self, target: &ProbeTarget, timeout_ms: u64) -> Health {
        use crate::config::ProbeMode;
        let mut icmp_ok = false;
        let mut http_ok = false;
        match target.mode {
            ProbeMode::Icmp => icmp_ok = probe_icmp(target.icmp_target.as_deref(), timeout_ms),
            ProbeMode::Http => http_ok = probe_http(target.http_target.as_deref(), timeout_ms),
            ProbeMode::Both => {
                icmp_ok = probe_icmp(target.icmp_target.as_deref(), timeout_ms);
                http_ok = probe_http(target.http_target.as_deref(), timeout_ms);
            }
        }
        let dead = match target.mode {
            ProbeMode::Icmp => !icmp_ok,
            ProbeMode::Http => !http_ok,
            // both：两端同时失败才判死（避免 ICMP 被墙时误杀）
            ProbeMode::Both => !(icmp_ok || http_ok),
        };
        if dead {
            Health::Fail
        } else {
            Health::Ok
        }
    }

    /// macOS 上 WireGuard 与第三方 VPN 走同一条路：两者的隧道都是**系统 VPN 配置**
    /// （WireGuard for macOS 把每个隧道注册成一条 NEVPN 配置），`scutil --nc list` 里
    /// 都能看到，所以不需要为两种 `TunnelTarget` 分叉。
    ///
    /// 厂商提示在这里**参与判定**：清单行里有 bundle id，「名字对但厂商错」就该继续
    /// 判为未连接，让 `tunnel_connect` 去解释 —— 否则用户配错了 provider 却看到
    /// 「已满足」，那条隧道其实根本没人管。
    fn tunnel_is_up(&self, target: &TunnelTarget) -> bool {
        let m = scutil_match(target);
        m.connected && m.provider_matched
    }

    /// ⚠️ 不提权：`scutil --nc start` 对某些隧道需要 root，权限不够时这里**照实报错**，
    /// 绝不改走 `exec_ops`（那条通道每次都要系统授权框，而 worker 每 N 秒就来一次）。
    fn tunnel_connect(&self, target: &TunnelTarget) -> Result<(), String> {
        let name = target.name();
        let m = scutil_match(target);
        if !m.found {
            let known = scutil_nc_labels();
            return Err(if known.is_empty() {
                i18n::tf("pal.scutil_empty", &[("name", name)])
            } else {
                i18n::tf("pal.scutil_missing", &[
                    ("name", name),
                    ("existing", &known.join(", ")),
                ])
            });
        }
        if !m.provider_matched {
            return Err(i18n::tf("pal.scutil_provider_mismatch", &[
                ("name", name),
                ("provider", target.provider().unwrap_or("")),
            ]));
        }
        run("/usr/sbin/scutil", &["--nc", "start", name])
            .map(|_| ())
            .map_err(|e| {
                i18n::tf("pal.scutil_start_failed", &[("name", name), ("error", &e)])
            })
    }

    fn add_route(&self, dest: &str, gw: &str, metric: u32) -> Result<(), String> {
        exec_ops(&[PrivOp::RouteAdd {
            dest: dest.to_string(),
            gateway: gw.to_string(),
            metric,
        }])
    }

    fn delete_route(&self, dest: &str) -> Result<(), String> {
        exec_ops(&[PrivOp::RouteDelete {
            dest: dest.to_string(),
        }])
    }

    fn launch_app(&self, app: &str, args: &[String]) -> Result<(), String> {
        let cmd_args = open_args(app, args);
        let refs: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();
        run("open", &refs).map(|_| ())
    }

    fn run_script(&self, path: &str, args: &[String], elevated: bool) -> Result<(), String> {
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        if elevated {
            // 安全决策：用户脚本**不走** netsense-priv.sh 白名单通道（那套只放行网络配置操作），
            // 提权执行脚本必须显式经由系统授权框，确保用户每次知情。
            run_via_osascript(&elevated_script_line(path, args)).map(|_| ())
        } else {
            run(path, &refs).map(|_| ())
        }
    }

    fn list_known_ssids(&self) -> Option<Vec<String>> {
        let svc = wifi_service()?;
        let out = run("networksetup", &["-listpreferredwirelessnetworks", &svc]).ok()?;
        Some(
            out.lines()
                .skip(1) // 首行是标题
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect(),
        )
    }

    fn list_printers(&self) -> Vec<PrinterInfo> {
        let Ok(names) = run("lpstat", &["-e"]) else {
            return Vec::new();
        };
        // 默认那一行读不到（比如根本没设过默认）不该让整张清单消失，所以按空文本继续
        let default = run("lpstat", &["-d"]).unwrap_or_default();
        printers_from_lpstat(&names, &default)
    }

    fn set_default_printer(&self, printer: &str) -> Result<(), String> {
        // `lpoptions -d` 改的是**当前用户**的 CUPS 默认目的地（落在 ~/.cups/lpoptions），
        // 因此不需要提权 —— 系统级的 `lpadmin -d` 要 root，那等于每换一次网络弹一次授权框。
        // 名字在本机不存在时 lpoptions 自己退非 0（stderr: "Unknown printer or class."），
        // 这条错误原样冒到界面上，比我们先查一遍清单更诚实（清单可能在这一瞬间已经变了）。
        run("lpoptions", &["-d", printer]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn app_arguments_go_behind_a_double_dash_so_open_does_not_misread_them() {
        assert_eq!(open_args("Notes", &[]), v(&["-a", "Notes"]));
        assert_eq!(
            open_args("Notes", &v(&["--new-note", "hello world"])),
            v(&["-a", "Notes", "--args", "--new-note", "hello world"]),
            "不带 --args 时这些会被 open 当成「要用该应用打开的文件」，而 - 开头的还会被 open 自己吃掉"
        );
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

    /// 编辑器表达「DNS 保持不变」的方式是把 `dns` 键整个删掉。因此下发侧必须把
    /// 「没有这个键」和「有一个空串」当成两件事：前者一条 DNS 命令都不能出。
    #[test]
    fn an_absent_dns_key_compiles_to_no_dns_operation() {
        let ops = apply_ops("Wi-Fi", &cfg(Mode::Manual, None)).unwrap();
        assert_eq!(
            ops,
            vec![PrivOp::SetManual {
                svc: "Wi-Fi".into(),
                ip: "192.168.1.100".into(),
                netmask: "255.255.255.0".into(),
                gateway: "192.168.1.1".into(),
            }],
            "缺 dns 键只该下发地址本身"
        );
        let dhcp = apply_ops("Wi-Fi", &cfg(Mode::Dhcp, None)).unwrap();
        assert_eq!(dhcp, vec![PrivOp::SetDhcp { svc: "Wi-Fi".into() }]);
    }

    /// 与上一个用例只差 `Some`：空串是一条**真实**的清空指令（`-setdnsservers … Empty`），
    /// 不是「什么都不做」。
    #[test]
    fn an_empty_dns_string_still_clears_the_service() {
        let ops = apply_ops("Wi-Fi", &cfg(Mode::Manual, Some(""))).unwrap();
        assert!(ops.contains(&PrivOp::SetDns {
            svc: "Wi-Fi".into(),
            servers: vec![]
        }));
        let clear = ops
            .iter()
            .find(|o| matches!(o, PrivOp::SetDns { .. }))
            .unwrap()
            .legacy_shell();
        assert!(
            clear.contains("-setdnsservers 'Wi-Fi' Empty"),
            "清空要真的下发 Empty，而不是省掉这条: {}",
            clear
        );
    }

    #[test]
    fn dns_servers_are_trimmed_and_kept_in_order() {
        let ops = apply_ops("Wi-Fi", &cfg(Mode::Manual, Some(" 8.8.8.8 ,1.1.1.1, "))).unwrap();
        assert!(ops.contains(&PrivOp::SetDns {
            svc: "Wi-Fi".into(),
            servers: v(&["8.8.8.8", "1.1.1.1"]),
        }));
    }

    /// 本机 `scutil --nc list` 的原样输出（Tailscale 已连、ProtonVPN 未连）。
    /// 匹配键必须是双引号里的用户可见标签：UUID 用户不会写，bundle id 又不是厂商名。
    const NC_LIST: &str = "Available network connection services in the current set (*=enabled):\n\
* (Connected)      07CD635E-71A1-4229-9C2F-B485A7702C9A VPN (io.tailscale.ipn.macsys) \"Tailscale\"                      [VPN:io.tailscale.ipn.macsys]\n\
* (Disconnected)   2C5C591F-B2EB-4ADD-8A69-04109E8ABCA1 VPN (ch.protonvpn.mac) \"ProtonVPN\"                      [VPN:ch.protonvpn.mac]";

    fn wg(t: &str) -> TunnelTarget {
        TunnelTarget::WireGuard {
            tunnel: t.to_string(),
        }
    }
    fn vpn(provider: &str, profile: &str) -> TunnelTarget {
        TunnelTarget::Vpn {
            provider: provider.to_string(),
            profile: profile.to_string(),
        }
    }

    #[test]
    fn scutil_rows_are_found_by_the_user_visible_label() {
        let rows = scutil_nc_rows(NC_LIST);
        assert_eq!(rows.len(), 2, "表头没有引号，不该被当成隧道");
        let hit = scutil_nc_row(&rows, &vpn("tailscale", "Tailscale")).expect("按标签找到那一行");
        assert!(hit.provider_matched);
        assert!(scutil_row_connected(hit.text), "第一条是已连接");
        let off = scutil_nc_row(&rows, &vpn("proton", "ProtonVPN")).expect("第二条也能找到");
        assert!(!scutil_row_connected(off.text), "未连的行不得判成已连");
        // 用户写配置时的大小写与空白不该影响命中
        assert!(scutil_nc_row(&rows, &vpn("TAILSCALE", " tailscale ")).is_some());
    }

    /// 厂商提示要传达到判定里：名字对但厂商错，`tunnel_is_up` 不能就此放行。
    #[test]
    fn a_provider_that_does_not_fit_the_row_is_reported_not_silently_ignored() {
        let rows = scutil_nc_rows(NC_LIST);
        let hit = scutil_nc_row(&rows, &vpn("proton", "Tailscale")).expect("名字仍然命中");
        assert!(!hit.provider_matched, "厂商对不上要标出来");
        assert!(scutil_nc_row(&rows, &wg("NoSuchTunnel")).is_none());
        // WireGuard 没有厂商限定，只看名字
        assert!(scutil_nc_row(&rows, &wg("Tailscale")).unwrap().provider_matched);
    }

    /// 清单里混着表头与不带引号的行，它们不能被当成「有个标签叫 …」的隧道。
    #[test]
    fn rows_without_a_quoted_label_are_not_tunnels() {
        assert_eq!(quoted_token("Available network connection services…"), None);
        assert_eq!(quoted_token("* (Connected) 07CD VPN (io.x) Tailscale"), None);
        assert_eq!(quoted_token("unbalanced \"label"), None);
        assert_eq!(quoted_token("\"  \""), None);
        assert_eq!(quoted_token("x \"Office WG\" y"), Some("Office WG"));
    }
}
