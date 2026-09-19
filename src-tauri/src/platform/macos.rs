//! macOS 平台实现。
//!
//! 依赖系统命令：`networksetup` / `ipconfig` / `route` / `arp` / `ping` / `curl`，
//! 以及 `airport`（RSSI/BSSID）与 `osascript`（授权框提权）。
//!
//! 提权策略：优先 `/etc/sudoers.d/netsense` 授权的白名单包装脚本（免密、无弹窗），
//! 不可用时自动回落 `osascript ... with administrator privileges`，功能不中断。

use super::{
    extract_mac, parse_kv, poll_ssid_watch, run, sh_q, timeout_secs, Health, InterfaceStatus,
    NetworkPlatform, PrivChannel, ProbeTarget, WatcherHandle,
};
use crate::config::{Mode, Profile, V6Mode};
use std::io::Write as _;
use std::process::{Command, Stdio};

const WIFI_IFACE: &str = "en0";
const AIRPORT: &str =
    "/System/Library/PrivateFrameworks/Apple80211.framework/Versions/Current/Resources/airport";

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
    match priv_channel() {
        PrivChannel::Direct => match exec_ops_sudoers(ops) {
            Ok(()) => Ok(()),
            Err(e) if is_nopasswd_unavailable(&e) => exec_ops_osascript(ops),
            Err(e) => Err(e),
        },
        PrivChannel::Prompt => exec_ops_osascript(ops),
    }
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
        .map_err(|e| format!("spawn sudo: {}", e))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(payload.as_bytes())
            .map_err(|e| format!("write priv stdin: {}", e))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait sudo: {}", e))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            "特权脚本执行失败（无 stderr）".to_string()
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

/// 提权执行用户脚本时拼给 root shell 的命令行。
///
/// 与 `legacy_shell()` 一样，这里的结果最终会进 `/bin/sh`（且是 root），所以每个分量
/// 都必须单独 `sh_q` 引用——不能先 join 再整体转义，那样各 token 边界丢失，`;`、`$()`
/// 依然会被 shell 按语法解释。
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

/// 解析 `airport -I` 输出，得到 (RSSI, BSSID)。
/// macOS 14.4+ 已移除 airport 命令，届时返回 (None, None)，不影响其余状态读取。
fn airport_info() -> (Option<i32>, Option<String>) {
    let out = match run(AIRPORT, &["-I", WIFI_IFACE]) {
        Ok(o) => o,
        Err(_) => return (None, None),
    };
    let rssi = parse_kv(&out, "agrCtlRSSI").and_then(|v| v.trim().parse::<i32>().ok());
    let bssid = parse_kv(&out, "BSSID");
    (rssi, bssid)
}

// —————————————————————————— macOS 实现 ——————————————————————————

pub struct MacPlatform;

impl NetworkPlatform for MacPlatform {
    fn watch_ssid(&self, cb: Box<dyn Fn(Option<String>) + Send + Sync>) -> WatcherHandle {
        // MacPlatform 为 ZST，直接 move 进线程（不捕获 &self，生命周期非 'static）
        poll_ssid_watch(|| MacPlatform.get_current_ssid(), cb)
    }

    fn get_current_ssid(&self) -> Option<String> {
        let out = run("networksetup", &["-getairportnetwork", WIFI_IFACE]).ok()?;
        // 输出形如：Current Wi-Fi Network: Office_5G
        parse_kv(&out, "Current Wi-Fi Network:")
    }

    fn get_status(&self) -> InterfaceStatus {
        let ssid = self.get_current_ssid();
        let (rssi, bssid) = airport_info();
        let mut st = InterfaceStatus {
            connected: ssid.is_some(),
            ssid,
            rssi,
            bssid,
            iface: Some(WIFI_IFACE.to_string()),
            ..Default::default()
        };
        if let Some(svc) = wifi_service() {
            if let Ok(o) = run("ipconfig", &["getifaddr", WIFI_IFACE]) {
                let v = o.trim();
                if !v.is_empty() {
                    st.ipv4 = Some(v.to_string());
                }
            }
            if let Ok(o) = run("ipconfig", &["getoption", WIFI_IFACE, "subnet_mask"]) {
                let v = o.trim();
                if !v.is_empty() {
                    st.netmask = Some(v.to_string());
                }
            }
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

    fn apply_profile(&self, p: &Profile) -> Result<(), String> {
        let svc = wifi_service().ok_or("找不到 Wi-Fi 网络服务")?;
        let mut ops: Vec<PrivOp> = Vec::new();
        match p.mode {
            Mode::Manual => {
                let ip = p.ip.clone().ok_or("manual 模式缺 ip")?;
                let netmask = p.netmask.clone().ok_or("manual 模式缺 netmask")?;
                let gateway = p.gateway.clone().ok_or("manual 模式缺 gateway")?;
                ops.push(PrivOp::SetManual {
                    svc: svc.clone(),
                    ip,
                    netmask,
                    gateway,
                });
            }
            Mode::Dhcp => ops.push(PrivOp::SetDhcp { svc: svc.clone() }),
        }
        // DNS（空 = 交回系统自动获取）
        let servers: Vec<String> = p
            .dns
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        ops.push(PrivOp::SetDns {
            svc: svc.clone(),
            servers,
        });
        // IPv6
        match p.v6mode {
            Some(V6Mode::Off) => ops.push(PrivOp::SetV6Off { svc: svc.clone() }),
            Some(V6Mode::Automatic) => ops.push(PrivOp::SetV6Auto { svc: svc.clone() }),
            Some(V6Mode::Manual) => {
                let addr = p.ipv6.clone().ok_or("v6 manual 缺 ipv6")?;
                let prefix = p.v6prefix.clone().ok_or("v6 manual 缺 v6prefix")?;
                let gateway = p.v6gateway.clone().ok_or("v6 manual 缺 v6gateway")?;
                ops.push(PrivOp::SetV6Manual {
                    svc: svc.clone(),
                    addr,
                    prefix,
                    gateway,
                });
            }
            None => {}
        }
        exec_ops(&ops)
    }

    fn set_dhcp(&self) -> Result<(), String> {
        let svc = wifi_service().ok_or("找不到 Wi-Fi 网络服务")?;
        exec_ops(&[
            PrivOp::SetDhcp { svc: svc.clone() },
            PrivOp::SetDns {
                svc,
                servers: Vec::new(),
            },
        ])
    }

    fn resolve_gateway_mac(&self) -> Option<String> {
        let gw = default_gateway()?;
        gateway_mac_for(&gw)
    }

    fn resolve_bssid(&self) -> Option<String> {
        airport_info().1
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
        let mut cmd_args: Vec<String> = vec!["-a".into(), app.into()];
        cmd_args.extend(args.iter().cloned());
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
}
