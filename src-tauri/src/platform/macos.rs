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
//! | 15.6+ | 已删除 | 同上 | **SSID 被涂成 `<redacted>`** | **SSID 同样被涂黑**（只剩 RSSI） |
//!
//! 15.6 起 CLI 来源在现连 SSID 上全军覆没，所以这里的第一来源是 **CoreWLAN**
//! （`CWWiFiClient`，见 `ssid_via_corewlan`）：Sequoia 上它仍然给出真实网络名，
//! 代价是系统把 SSID 视为位置信息 —— 必须申请定位授权（`request_location_authorization`）
//! 并在 Info.plist 里声明用途，NetSense 才会出现在「定位服务」授权列表。
//! 授权没给之前它返回 `None`，降级链照旧走完三个 CLI 来源
//! （`networksetup` → `ipconfig getsummary` → `system_profiler`，最后那个慢，故带 2s 缓存）。
//!
//! 提权策略：优先 `/etc/sudoers.d/netsense` 授权的白名单包装脚本（免密、无弹窗）。
//! 通道还没装上时，**第一次**应用配置的那一个授权框顺手把通道装好（见
//! [`exec_ops_bootstrap`]），此后同一台机器上的每一次应用都不再问；免密不可用
//! （被撤销 / 需密码 / 装不上）、或这一批的形状白名单表达不了时才回落到逐次授权。

use super::{
    extract_mac, parse_kv, poll_ssid_watch, prefers_dark_from_defaults, printers_from_lpstat, run, run_env,
    sh_q, timeout_secs, C_LOCALE, Health, InterfaceStatus, NetworkPlatform, PrinterInfo, PrivChannel,
    ProbeTarget,
    TunnelTarget, WatcherHandle,
};
use crate::config::{Mode, NetworkConfig, V6Mode};
use crate::i18n;
use objc2::rc::Retained;
use objc2::{class, msg_send};
use objc2_core_location::CLLocationManager;
use objc2_core_wlan::CWWiFiClient;
use std::io::Write as _;
use std::path::{Path, PathBuf};
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

/// 一个字段能否原样放进免密通道的那一行：非空、不以 `-` 开头、不含 `|` 与换行。
/// 与包装脚本的 `is_service` 同一套规则。
fn slot_ok(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && !s.contains(['|', '\n', '\r'])
}

/// `a.b.c.d`：恰好四段，每段 1~3 位十进制且 ≤255，且不收 `010` 这种前导零写法 ——
/// 脚本的 `is_ipv4` 就是这么拒的（那种写法容易被读成八进制）。这边的判定只许比脚本严、
/// 不许比脚本松：松了就是整批交给 root 之后再被脚本退回来。
fn ipv4(s: &str) -> bool {
    let mut parts = s.split('.');
    let (Some(a), Some(b), Some(c), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    [a, b, c, d].iter().all(|p| {
        !p.is_empty()
            && p.len() <= 3
            && p.bytes().all(|b| b.is_ascii_digit())
            && !(p.len() > 1 && p.starts_with('0'))
            && p.parse::<u8>().is_ok()
    })
}

/// 路由目标：`a.b.c.d` 或 `a.b.c.d/len`（len 为十进制且 ≤32）—— 包装脚本的 `is_route_dest`。
fn route_dest_ok(s: &str) -> bool {
    match s.split_once('/') {
        Some((addr, len)) => {
            ipv4(addr)
                && !len.is_empty()
                && len.len() <= 2
                && len.bytes().all(|b| b.is_ascii_digit())
                && len.parse::<u8>().unwrap_or(255) <= 32
        }
        None => ipv4(s),
    }
}

/// 宽松 IPv6：含 `:` 且只用十六进制、点、冒号 —— 包装脚本的 `is_ipv6`。
fn ipv6_loose_ok(s: &str) -> bool {
    s.contains(':') && s.bytes().all(|b| b.is_ascii_hexdigit() || matches!(b, b':' | b'.'))
}

/// 前缀长度：十进制且 ≤128 —— 包装脚本的 `is_prefix`。
fn prefix_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 3
        && s.bytes().all(|b| b.is_ascii_digit())
        && s.parse::<u8>().unwrap_or(255) <= 128
}

/// DNS 列表：空表示清空；否则每一项都得是 IPv4（这里比脚本更严 —— 空项也算不合法）。
fn dns_ok(servers: &[String]) -> bool {
    servers.iter().all(|s| ipv4(s))
}

/// 这一批操作能否走免密通道：**逐条比对包装脚本的形状规则**。
///
/// 判错的方向是安全的那一侧。判成「不能走」而其实能走，代价只是这一批多弹一次授权框；
/// 反过来（以为能走、root 那边却按白名单拒了）会让这一批**直接失败** —— 而白名单一旦在
/// 第一次下发时自动装上，这种失败就成了新用户会撞上的那条路。所以凡拿不准的都算不行。
///
/// 唯一真正走得通却在这里被挡下的是路由目标：配置层只要求 `dest` 非空，`route -n add
/// -net default <gw>` 之类的写法在 shell 那边有效，白名单却只收 IPv4 / IPv4/len。
fn allow_list_takes(ops: &[PrivOp]) -> bool {
    ops.iter().all(|o| match o {
        PrivOp::SetDhcp { svc } | PrivOp::SetV6Off { svc } | PrivOp::SetV6Auto { svc } => {
            slot_ok(svc)
        }
        PrivOp::SetManual {
            svc,
            ip,
            netmask,
            gateway,
        } => slot_ok(svc) && ipv4(ip) && ipv4(netmask) && ipv4(gateway),
        PrivOp::SetDns { svc, servers } => slot_ok(svc) && dns_ok(servers),
        PrivOp::SetV6Manual {
            svc,
            addr,
            prefix,
            gateway,
        } => slot_ok(svc) && ipv6_loose_ok(addr) && prefix_ok(prefix) && ipv6_loose_ok(gateway),
        PrivOp::RouteAdd { dest, gateway, .. } => route_dest_ok(dest) && ipv4(gateway),
        PrivOp::RouteDelete { dest } => route_dest_ok(dest),
    })
}

/// 提权执行一组结构化操作。
/// 优先 `sudo -n <priv script> --batch`（无弹窗，参数经脚本白名单二次校验）；
/// 通道还没装好时走 `exec_ops_bootstrap` —— 那**一次**授权顺手把通道装上，此后不再问；
/// 免密不可用（被撤销 / 需密码 / 装不上）或这一批**白名单表达不了**时回落 osascript
/// 授权框，功能不中断。
pub fn exec_ops(ops: &[PrivOp]) -> Result<(), String> {
    if ops.is_empty() {
        return Ok(());
    }
    let r = match priv_channel() {
        PrivChannel::Direct if allow_list_takes(ops) => match exec_ops_sudoers(ops) {
            Ok(()) => Ok(()),
            Err(e) if is_nopasswd_unavailable(&e) => exec_ops_osascript(ops),
            Err(e) => Err(e),
        },
        // 装好通道的机器上仍会有表达不了的一批（例如目标是 `default` 的路由）：
        // 这种批次照旧走授权框，不能因为「通道在」就把它判死。
        PrivChannel::Direct => exec_ops_osascript(ops),
        PrivChannel::Prompt => exec_ops_bootstrap(ops),
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

/// 免密通道的两个落点：包装脚本（`PRIV_SCRIPT`）与授权它的那一行 sudoers 规则。
/// 安装与卸载只碰这两个路径，不碰别的。
const SUDOERS_FILE: &str = "/etc/sudoers.d/netsense";

/// 白名单包装脚本的**唯一真源**：编译进二进制的就是仓库 `scripts/netsense-priv.sh`
/// 那一份字节，安装时按字节写出。在 Rust 里重抄一份会产生两份校验规则，而两份规则
/// 一旦分叉，后果是「界面下发的行」与「root 那边接受的行」不是同一套东西。
const PRIV_SCRIPT_SRC: &str = include_str!("../../../scripts/netsense-priv.sh");

/// sudoers 行的用户名形状校验。
///
/// sudoers 是按空白切词的一行：用户名里只要出现空格、`=`、引号、反斜杠或换行，写下去的
/// 就不再是「授权某一个用户」，而是一条被截断过、甚至多出一段规则的文本。首字符还必须
/// 是字母数字或下划线，否则 `-` 之类的开头会被读成别的东西。取不到合形的名字就宁可不装。
fn safe_sudoers_user(raw: &str) -> Option<&str> {
    let name = raw.trim();
    if name.is_empty() || name.len() > 256 {
        return None;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() || c == '_' => {}
        _ => return None,
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        return None;
    }
    Some(name)
}

/// 当前登录用户名（写进 sudoers 的那一个）。形状不合的一律当作「拿不到」。
fn current_user_name() -> Option<String> {
    let raw = run("id", &["-un"]).ok()?;
    safe_sudoers_user(raw.trim()).map(str::to_string)
}

/// 一次提权要执行的安装脚本：装好包装脚本与 sudoers 规则，然后删掉自己的临时输入。
///
/// 里面没有一句来自配置文件的自由文本 —— `wrapper` 是本进程刚独占写出的私有临时文件，
/// `user` 已过 [`safe_sudoers_user`]，两个都再单独 `sh_q`。`visudo -cf` 排在落盘**之前**：
/// 一个语法错的 `/etc/sudoers.d/*` 会让整台机器的 `sudo` 报错，这里不能赌自己拼得对。
fn priv_install_script(wrapper: &Path, user: &str) -> String {
    // 安装目录从脚本路径本身推出来，不留第二个常量：两处各写一遍
    // `/usr/local/libexec` 的话，改一处就会 `install -d` 一个目录、把脚本装进另一个。
    let dir = PRIV_SCRIPT
        .rsplit_once('/')
        .map_or("/usr/local/libexec", |(d, _)| d);
    format!(
        "set -eu\n\
         PATH=/usr/sbin:/sbin:/usr/bin:/bin; export PATH\n\
         umask 022\n\
         install -d -o root -g wheel -m 0755 {dir}\n\
         install -o root -g wheel -m 0755 {wrapper} {script}\n\
         rm -f {wrapper}\n\
         _t=$(mktemp)\n\
         printf '%s ALL=(root) NOPASSWD: {script}\\n' {user} > \"$_t\"\n\
         visudo -cf \"$_t\"\n\
         install -o root -g wheel -m 0440 \"$_t\" {sudoers}\n\
         rm -f \"$_t\"\n",
        script = PRIV_SCRIPT,
        wrapper = sh_q(&wrapper.display().to_string()),
        user = sh_q(user),
        sudoers = SUDOERS_FILE,
    )
}

/// 独占写出「包装脚本 + 安装脚本」两份临时文件，返回安装脚本的路径。
///
/// 两份都是 0600、属主为当前用户：root 读得到，别的用户改不了（`verify_private` 查的
/// 就是这两点，见 §9.3）。包装脚本由安装脚本装完后自己删；用户在授权框上点了取消时
/// 它会留在临时目录里 —— 那是系统自管的一块地方，内容又是仓库里那份脚本的原文，
/// 不值得为它加一条清理路径，更不该因此把下发挡住。安装脚本自己写不出来时，
/// 先写出的那份由这里删掉：那种失败是本进程的问题，不留半成品。
fn write_priv_installer(user: &str) -> Result<PathBuf, String> {
    let wrapper = crate::update::write_private_script("priv-wrapper", PRIV_SCRIPT_SRC)?;
    match crate::update::write_private_script(
        "priv-install",
        &priv_install_script(&wrapper, user),
    ) {
        Ok(installer) => Ok(installer),
        Err(e) => {
            let _ = std::fs::remove_file(&wrapper);
            Err(e)
        }
    }
}

/// 免密通道自检：以当前用户跑一次空批。
///
/// 装完就查，是因为「文件都到位」不等于「这台机器的 sudoers 真的免密」——目录服务里的
/// 账号名、已被别人占用的 `/etc/sudoers.d/netsense`、或压根不给该用户 sudo 的策略，都会
/// 让通道看起来装好了而每次仍要密码。查出来的结果只写日志：下发侧本来就有回落。
fn nopasswd_probe() -> Result<(), String> {
    let out = Command::new("sudo")
        .arg("-n")
        .arg(PRIV_SCRIPT)
        .arg("--batch")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() {
        i18n::t("pal.privileged_no_stderr")
    } else {
        err
    })
}

/// 「问一次，之后不再问」：把**装通道**和**下发本批**放进同一个授权框。
///
/// 这两件事要的权限是同一个（root），所以没有理由让用户输两次密码：安装脚本串在操作链
/// 前面、用 `;` 隔开，用户第一次改网络时输那一次密码，此后每一次都走 `sudo -n` 免密通道。
/// 装不上（用户名取不到、临时目录不可用）时本批仍照常下发 —— 安装是顺手做的，不是前提。
fn exec_ops_bootstrap(ops: &[PrivOp]) -> Result<(), String> {
    let chain = ops
        .iter()
        .map(|o| o.legacy_shell())
        .collect::<Vec<_>>()
        .join(" ; ");
    let installer = current_user_name().and_then(|u| write_priv_installer(&u).ok());
    let cmd = match &installer {
        Some(p) => format!("/bin/sh {} ; {}", sh_q(&p.display().to_string()), chain),
        None => chain,
    };
    let r = run_via_osascript(&cmd).map(|_| ());
    if let Some(p) = &installer {
        let _ = std::fs::remove_file(p);
    }
    if r.is_ok() && priv_channel() == PrivChannel::Direct {
        match nopasswd_probe() {
            Ok(()) => crate::log::info(&i18n::t("notify.priv_installed")),
            Err(e) => crate::log::warn(&i18n::tf("notify.priv_unverified", &[("error", &e)])),
        }
    }
    r
}

/// 撤销免密通道：删掉包装脚本与那条 sudoers 规则，一次系统授权。
///
/// 之后应用配置回到「每次都问」。**下一次**应用配置会再问一次密码并顺手把通道装回来 ——
/// 撤销不该变成「以后再也装不上」，而重新安装始终需要用户当场授权。
pub fn uninstall_priv_channel() -> Result<(), String> {
    run_via_osascript(&format!(
        "rm -f {} {}",
        sh_q(PRIV_SCRIPT),
        sh_q(SUDOERS_FILE)
    ))
    .map(|_| ())
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

/// 应用启动时调一次：向系统申请「使用期间」定位授权，NetSense 才会出现在
/// 系统设置 → 隐私与安全性 → 定位服务 的列表里，用户放行后 CoreWLAN 才读得到 SSID
/// （macOS 14 起把网络名当作位置信息对待）。
///
/// 不挂 delegate 等回调 —— SSID 监视线程每 2~5s 轮询一次，授权一变，下一轮自然读到。
/// `CLLocationManager` 要活得和进程一样久（系统按「管理器是否仍存活」判定授权会话），
/// 而 ObjC 对象不是 `Sync` 的，所以套一层壳再放进 static：引用永不跨线程传递，
/// 也没有 delegate 回调可被触发，共享只读是安全的。
pub fn request_location_authorization() {
    struct LocationManager(Retained<CLLocationManager>);
    // SAFETY: 见函数注释 —— 该对象只在这一个函数里被触碰（启动时的主线程），
    // static 只是为了续命，跨线程共享的实际上只有它的存在性。
    unsafe impl Send for LocationManager {}
    unsafe impl Sync for LocationManager {}
    static MANAGER: OnceLock<LocationManager> = OnceLock::new();
    let mgr = MANAGER.get_or_init(|| unsafe {
        LocationManager(msg_send![class!(CLLocationManager), new])
    });
    unsafe { mgr.0.requestWhenInUseAuthorization() };
}

/// 来源 0：CoreWLAN（`CWWiFiClient`）。macOS 14.4 起唯一可靠取现连 SSID 的来源 ——
/// 15.6+ 三个 CLI 来源全被涂黑（版本矩阵见文件头注释）。
///
/// 未授权、没有无线网卡、未关联网络时都返回 `None`，调用方继续走 CLI 降级链；
/// 所以「用户还没放行定位授权」不会让状态读取整体失败，只是 SSID 一时为空。
fn ssid_via_corewlan() -> Option<String> {
    let client = unsafe { CWWiFiClient::sharedWiFiClient() };
    let iface = unsafe { client.interface() }?;
    let ssid = unsafe { iface.ssid() }?;
    sanitize_ssid(&ssid.to_string())
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

/// 一条接口在 `ifconfig` 眼里的三个事实。
#[derive(Debug, PartialEq)]
struct IfaceSnap {
    /// 链路层是否 UP（拔了网线的以太网卡是 false）
    running: bool,
    /// `inet` 地址。`ipconfig getifaddr` 只认得部分接口，隧道上要靠这里兜住。
    inet: Option<String>,
    /// 是否有非 link-local 的 `inet6` 地址（有些 VPN 隧道只在 v6 上有地址）
    v6_global: bool,
}

/// 一次 `ifconfig` 取回所有接口的状态，替代逐条 `ifconfig <dev>`。
fn iface_snapshot() -> std::collections::HashMap<String, IfaceSnap> {
    let Ok(out) = run("ifconfig", &[]) else {
        return std::collections::HashMap::new();
    };
    parse_iface_snapshot(&out)
}

/// 解析 `ifconfig` 的输出。设备行形如
/// `en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500`，
/// 缩进行是它的属性。`fe80::` 每个接口都有一条，不能当成「这条接口有地址」。
fn parse_iface_snapshot(out: &str) -> std::collections::HashMap<String, IfaceSnap> {
    let mut map: std::collections::HashMap<String, IfaceSnap> = std::collections::HashMap::new();
    let mut cur: Option<String> = None;
    for line in out.lines() {
        if !line.starts_with(' ') && !line.starts_with('\t') {
            let dev = line.split(':').next().unwrap_or("").trim().to_string();
            if dev.is_empty() {
                cur = None;
            } else {
                map.insert(
                    dev.clone(),
                    IfaceSnap {
                        running: line.contains("RUNNING"),
                        inet: None,
                        v6_global: false,
                    },
                );
                cur = Some(dev);
            }
            continue;
        }
        let Some(dev) = &cur else { continue };
        let Some(e) = map.get_mut(dev) else { continue };
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("inet ") {
            e.inet = rest.split_whitespace().next().map(|s| s.to_string());
        } else if let Some(rest) = t.strip_prefix("inet6 ") {
            let a = rest.split_whitespace().next().unwrap_or("");
            if !a.is_empty() && !a.starts_with("fe80") {
                e.v6_global = true;
            }
        }
    }
    map
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

/// 隧道归属的证据。一次网卡枚举只采集一遍（每条候选服务一个 `networksetup` 子进程）。
///
/// 字段只装**能对上号**的事实：`svc_ip` 里不含有地址的服务，`connected` 里不含有未连的会话。
#[derive(Debug, Default)]
struct VpnEvidence {
    /// 非硬件网络服务 → 它自己 `-getinfo` 报出的 IPv4。
    svc_ip: Vec<(String, String)>,
    /// `scutil --nc list` 里处于 `(Connected)` 的会话标签。
    connected: Vec<String>,
}

/// 采集 [`VpnEvidence`]。
///
/// 只对「不是硬件端口」的网络服务问 `-getinfo`：VPN 客户端创建的服务都以自己的名字
/// 出现在 `-listallnetworkservices` 里而不在 `-listallhardwareports` 里，硬件口既问不
/// 出隧道的地址、又要多起一倍子进程。
fn collect_vpn_evidence(hw_ports: &[HwPort]) -> VpnEvidence {
    let hw: std::collections::HashSet<&str> = hw_ports.iter().map(|p| p.port.as_str()).collect();
    let mut svc_ip = Vec::new();
    for svc in network_services() {
        if hw.contains(svc.as_str()) {
            continue;
        }
        let info = run("networksetup", &["-getinfo", &svc]).unwrap_or_default();
        if let Some(ip) = value_of(&info, "IP address") {
            if !ip.is_empty() {
                svc_ip.push((svc, ip));
            }
        }
    }
    VpnEvidence {
        svc_ip,
        connected: scutil_nc_connected(),
    }
}

/// 判定一条隧道的归属软件；证据对不上就返回 `None`（界面退回通用的「VPN」标签）。
///
/// macOS 没有公开的「utun → 进程」映射，所以这里只承认两条**和这台设备有关**的线索：
/// 1. **按 IP 认领**：哪个网络服务报出了这条隧道的 IPv4，这条隧道就是它建的。服务名是
///    客户端自己写的，照原样显示（不做关键词归一：用户自建的 "MyVPN" 归一成 "VPN" 是
///    丢信息，不是提纯）。
/// 2. **唯一的已连接会话**：`alone`（系统上只有这一条隧道在用）且 `scutil --nc list`
///    里只有一条 `(Connected)`。两个「唯一」同时成立时它们指的几乎必然是同一件事；
///    缺任何一个都不再推断。
///
/// 曾经还有第三条路，它是错的，别再加回来：无人认领时按关键词扫一遍全部服务、命中即返回。
/// 装了某家客户端的机器上，那个服务**一直**在清单里（断开也在、也没有地址），于是任何一条
/// 认领不上的隧道都会被说成是它建的 —— 那个结果和这台设备没有任何关系，界面上得到的却
/// 是一个**听起来很具体的错答案**。认不出来只是少一格信息，认错才是事故。
fn attribute_vpn(ev: &VpnEvidence, ip: Option<&str>, alone: bool) -> Option<String> {
    if let Some(addr) = ip.filter(|a| !a.is_empty()) {
        if let Some((svc, _)) = ev.svc_ip.iter().find(|(_, a)| a == addr) {
            return Some(svc.clone());
        }
    }
    if alone && ev.connected.len() == 1 {
        return Some(ev.connected[0].clone());
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

/// 已连接的那些会话标签。
fn connected_labels(rows: &[(String, String)]) -> Vec<String> {
    rows.iter()
        .filter(|(_, line)| scutil_row_connected(line))
        .map(|(label, _)| label.clone())
        .collect()
}

/// 当前处于已连接状态的 VPN 会话标签（未连的一律不进结果）。
fn scutil_nc_connected() -> Vec<String> {
    let Ok(out) = run("/usr/sbin/scutil", &["--nc", "list"]) else {
        return Vec::new();
    };
    connected_labels(&scutil_nc_rows(&out))
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

    /// 逐级降级取 SSID，顺序 = 由准到糙、由快到慢（版本矩阵见文件头注释）：
    /// CoreWLAN → `networksetup` → `ipconfig getsummary` → `system_profiler`。
    ///
    /// 单点取值的代价：只用 `networksetup` 时，macOS 15.0 起
    /// 会稳定拿到 "You are not associated with an AirPort network."，于是 SSID 区永远空白；
    /// 而 15.6+ 起三个 CLI 来源一起失效，SSID 必须优先走 CoreWLAN（需定位授权）。
    fn get_current_ssid(&self) -> Option<String> {
        ssid_via_corewlan().or_else(|| {
            let dev = wifi_iface();
            ssid_via_networksetup(&dev)
                .or_else(|| ssid_via_ipconfig(&dev))
                .or_else(|| system_profiler_info().ssid)
        })
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
    /// 「在用」= 链路 UP，并且有一条真实地址（IPv4 / 非 link-local IPv6）或已关联上无线
    /// 网络（刚连上还没拿到地址的那一瞬也要能看到）。
    /// 每次调用会拉起若干子进程，故整体经 [`super::cached_nics`] 做 TTL 缓存
    /// —— 面板每次状态广播都要一份快照。
    fn list_interfaces(&self) -> Vec<super::NicInfo> {
        super::cached_nics(|| {
            let hw = hardware_ports();
            let routes = default_routes();
            let wifi_dev = wifi_iface();
            let snaps = iface_snapshot();
            let mut out: Vec<super::NicInfo> = Vec::new();

            for dev in all_devices() {
                if is_noise_device(&dev) {
                    continue;
                }
                let port = hw.iter().find(|p| p.dev == dev);
                let kind = kind_of(&dev, port.map(|p| p.port.as_str()));
                let snap = snaps.get(&dev);
                let running = snap.map(|s| s.running).unwrap_or(false);
                // 判据不是「有没有 IPv4」：VPN 隧道常常只在 IPv6 上有地址，`ipconfig
                // getifaddr` 也认不全这些设备 —— 只看 IPv4 的话用户那条 VPN 就凭空消失。
                let addressed = snap.map(|s| s.inet.is_some() || s.v6_global).unwrap_or(false);
                let ipv4 = run("ipconfig", &["getifaddr", &dev])
                    .ok()
                    .map(|o| o.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .or_else(|| snap.and_then(|s| s.inet.clone()));
                let netmask = run("ipconfig", &["getoption", &dev, "subnet_mask"])
                    .ok()
                    .map(|o| o.trim().to_string())
                    .filter(|s| !s.is_empty());
                let ssid = if kind == super::NicKind::Wireless && dev == wifi_dev {
                    self.get_current_ssid()
                } else {
                    None
                };
                if !running || (ipv4.is_none() && ssid.is_none() && !addressed) {
                    continue;
                }

                let gateway = routes.get(&dev).cloned();
                let gateway_mac = gateway.as_deref().and_then(gateway_mac_for);
                let dns = port.and_then(|p| dns_of_service(&p.port));
                let ipv6 = port.and_then(|p| v6_of_service(&p.port));
                out.push(super::NicInfo {
                    name: dev,
                    label: port.map(|p| p.port.clone()),
                    kind,
                    up: running,
                    ssid,
                    mac: port.and_then(|p| p.mac.clone()),
                    ipv4,
                    netmask,
                    ipv6,
                    gateway,
                    gateway_mac,
                    dns,
                    // 隧道归属留到清单收齐后再填（下面那段）
                    app: None,
                });
            }

            // 隧道归属要在清单收齐以后再判：判据里那条「本机只有这一条隧道在用」必须知道
            // 一共几条在用，而这要等循环跑完才知道（边枚举边判会低估，进而认错）。
            let vpn_rows: Vec<usize> = out
                .iter()
                .enumerate()
                .filter(|(_, n)| n.kind == super::NicKind::Vpn)
                .map(|(i, _)| i)
                .collect();
            if !vpn_rows.is_empty() {
                let ev = collect_vpn_evidence(&hw);
                let alone = vpn_rows.len() == 1;
                for i in vpn_rows {
                    let ip = out[i].ipv4.clone();
                    out[i].app = attribute_vpn(&ev, ip.as_deref(), alone);
                }
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
        // `-listpreferredwirelessnetworks` 要的是**设备名**（en0），不是网络服务名（"Wi-Fi"）。
        // 传服务名时命令退 10 并打印 "Wi-Fi is not a Wi-Fi interface."，于是这里 `?` 成
        // None，编辑器上「系统保存过的 SSID」下拉整列是空的（v1.0.2 用户报的 MAC3-②）。
        let dev = discover_wifi_device()?;
        let out = run("networksetup", &["-listpreferredwirelessnetworks", &dev]).ok()?;
        Some(
            out.lines()
                .skip(1) // 首行是标题
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect(),
        )
    }

    fn list_adapters(&self) -> Vec<super::NicInfo> {
        // 硬件端口表就是「有哪几口」的权威答案：它列的是系统装着的口，拔没拔线都在上面。
        let snaps = iface_snapshot();
        let mut out: Vec<super::NicInfo> = hardware_ports()
            .into_iter()
            .filter(|p| !is_noise_device(&p.dev))
            .map(|p| {
                let running = snaps.get(&p.dev).map(|s| s.running).unwrap_or(false);
                // 端口名要参与判断：`en0` 在带以太网的机型上是 Wi-Fi，只看设备名会把它
                // 归成有线，下拉里的类型标签就指错了网卡。
                let kind = kind_of(&p.dev, Some(p.port.as_str()));
                super::NicInfo {
                    name: p.dev,
                    label: Some(p.port),
                    kind,
                    up: running,
                    mac: p.mac,
                    ..Default::default()
                }
            })
            .collect();
        // 按设备名排（en0 先于 en3）：类型分组是面板的事，下拉要的是稳定的顺序。
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// 首选语言取 `AppleLanguages` 的第一项 —— 那就是「系统设置 → 语言与地区」排最前的
    /// 那条，形如 `zh-Hans-CN`。
    ///
    /// 不靠 `LANG`：从 Finder / 登录项起来的进程，环境里那份 LANG 常常还是登录时写下的
    /// 旧值，用户后来在系统设置里换过语言它也不跟着变（开发构建从终端起才会有今天的值）。
    /// 所以只在 `defaults` 那条读不到时才退回去看一眼，比什么都没有强。
    fn ui_language(&self) -> Option<String> {
        let out = run("defaults", &["read", "-g", "AppleLanguages"]).unwrap_or_default();
        if let Some(tag) = out.split('"').nth(1) {
            if !tag.is_empty() {
                return Some(tag.to_string());
            }
        }
        let lang = std::env::var("LANG").unwrap_or_default();
        match lang.as_str() {
            "" | "C" | "POSIX" => None,
            other => Some(other.to_string()),
        }
    }

    /// 深/浅问 `defaults` 的**整域**转储，不是单读 `AppleInterfaceStyle` 那一个键：
    /// 浅色模式下那个键根本不存在，`defaults` 以非 0 退出，那份错误和「读不到」在
    /// `run` 的分界里是同一个形状，于是浅色会被当成「问不到」而退成深色。整域转储在
    /// 两种模式下都成功，判据见 [`prefers_dark_from_defaults`]。
    fn ui_prefers_dark(&self) -> Option<bool> {
        let out = run("defaults", &["read", "-g"]).ok()?;
        Some(prefers_dark_from_defaults(&out))
    }

    fn list_printers(&self) -> Vec<PrinterInfo> {
        // 三条命令各司其职（为什么主清单换成 `-l -p`，见 `printers_from_lpstat`）。任何一条
        // 拿不到都按空文本继续：`-l -p` 失败时还有 `-e` 兜底，默认行失败只是没人亮。
        let long = run_env("lpstat", &["-l", "-p"], &C_LOCALE).unwrap_or_default();
        let names = run_env("lpstat", &["-e"], &C_LOCALE).unwrap_or_default();
        let default = run_env("lpstat", &["-d"], &C_LOCALE).unwrap_or_default();
        printers_from_lpstat(&long, &names, &default)
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

    /// sudoers 那一行是按空白切词的，所以用户名只能是一个「合形的 token」。
    /// 这条校验挡住的不是攻击，而是把整机 `sudo` 写坏：一个多出来的空格或 `=` 就能把
    /// 一行规则变成两行，或者变成一条谁都看不懂的规则。
    #[test]
    fn sudoers_user_accepts_one_token_only() {
        assert_eq!(safe_sudoers_user("imonior"), Some("imonior"));
        assert_eq!(safe_sudoers_user("  _u.1-2  "), Some("_u.1-2"));
        for bad in [
            "",
            "   ",
            "root admin",
            "a=b",
            "a\"b",
            "a\\b",
            "a\nb",
            "-root",
            ".hidden",
            &"u".repeat(300),
        ] {
            assert_eq!(safe_sudoers_user(bad), None, "accepted {bad:?}");
        }
    }

    /// 安装脚本里三个可变量（临时路径、用户名、固定路径）之外没有任何自由文本，
    /// 且 `visudo -cf` 必须排在 sudoers 落盘**之前**：语法错的文件会让整机 `sudo` 报错。
    #[test]
    fn priv_install_script_quotes_slots_and_checks_before_installing() {
        let script = priv_install_script(Path::new("/tmp/o'x.sh"), "a b");
        // 单引号按 shell 规则翻倍续接，绝不留下未闭合的引号
        assert!(script.contains("'/tmp/o'\\''x.sh'"), "{script}");
        assert!(script.contains("printf '%s ALL=(root) NOPASSWD: "), "{script}");
        assert!(script.contains("/etc/sudoers.d/netsense\n"), "{script}");
        let check = script.find("visudo -cf").expect("no visudo check");
        let land = script
            .find("-m 0440")
            .expect("sudoers never installed");
        assert!(check < land, "sudoers installed before being validated");
        // 包装脚本必须是 root 属主、0755，且装完就把临时输入清掉
        assert!(script.contains("-o root -g wheel -m 0755"), "{script}");
        assert!(script.contains("rm -f '"), "{script}");
    }

    /// 形状镜像的**单向**核对：Rust 判「白名单装得下」的每一个值，包装脚本自己也得判合法。
    /// 判反了的后果不对称 —— 误判成装不下只是这一批多弹一次授权框，误判成装得下则是
    /// root 那边把整批拒掉、配置直接应用失败。所以断言只压这个危险方向。
    ///
    /// 取的是烤进二进制的那份脚本原文里「校验函数」那一段（不含任何真命令），样本同样
    /// 按 shell 的规则转义。这里不放含换行的样本：换行在校验里就不可能通过（一行一条
    /// 操作，换行等于凭空多出一条），那一面由 [`shape_helpers_reject_everything_the_channel_cannot_hold`] 单独钉住。
    #[test]
    fn rust_never_admits_a_batch_the_wrapper_would_reject() {
        let head = PRIV_SCRIPT_SRC
            .split("# ---------- 单条操作执行 ----------")
            .next()
            .expect("wrapper has no validator section");
        // (包装脚本里的校验函数, 样本)：样本按脚本的规则转义后交给 `/bin/sh`，
        // 这边的判定由同名分支给出 —— 表里没有第三个字段，就不会出现「样本配错了函数」。
        let cases = [
            ("is_ipv4", "192.168.1.1"),
            ("is_ipv4", "0.1.2.3"),
            ("is_ipv4", "1.2.3.04"),
            ("is_ipv4", "10.0.0"),
            ("is_ipv4", "256.0.0.1"),
            ("is_ipv4", "1.2.3.4."),
            ("is_ipv4", ""),
            ("is_service", "Thunderbolt Bridge"),
            ("is_service", "Wi-Fi"),
            ("is_service", "-rf"),
            ("is_service", ""),
            ("is_route_dest", "10.0.0.0/8"),
            ("is_route_dest", "192.0.2.0/24"),
            ("is_route_dest", "0.0.0.0/0"),
            ("is_route_dest", "192.168.1.1"),
            ("is_route_dest", "default"),
            ("is_route_dest", "10.0.0.0/33"),
            ("is_ipv6", "fe80::1"),
            ("is_ipv6", "fd00::1234"),
            ("is_ipv6", "fe80::1%en0"),
            ("is_prefix", "64"),
            ("is_prefix", "128"),
            ("is_prefix", "129"),
            ("is_prefix", ""),
        ];
        let mut script = String::from(head);
        for (chk, sample) in cases {
            script.push_str(&format!(
                "if {chk} {}; then echo 1; else echo 0; fi\n",
                sh_q(sample)
            ));
        }
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("no /bin/sh to check the shape rules with");
        assert!(out.status.success(), "validator section failed to run");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let verdicts: Vec<&str> = stdout.lines().collect();
        assert_eq!(verdicts.len(), cases.len(), "one line per sample");
        for ((chk, sample), verdict) in cases.iter().zip(&verdicts) {
            let admitted = match *chk {
                "is_ipv4" => ipv4(sample),
                "is_service" => slot_ok(sample),
                "is_route_dest" => route_dest_ok(sample),
                "is_ipv6" => ipv6_loose_ok(sample),
                "is_prefix" => prefix_ok(sample),
                other => panic!("no Rust mirror for {other}"),
            };
            if admitted {
                assert_eq!(
                    *verdict, "1",
                    "Rust admitted {sample:?} to {chk}, the wrapper did not"
                );
            }
        }
    }

    /// 这些判定各自挡住什么：值要么来自配置文件，要么来自操作系统报出的服务名，两边都
    /// 不由这段代码控制 —— 而它们会拼进 root 执行的那一行里。
    #[test]
    fn shape_helpers_reject_everything_the_channel_cannot_hold() {
        // 换行与 `|` 都能把「一行一条操作」变成两条操作或多出参数；`-` 开头会被下游
        // 命令当成选项。
        for bad in ["en0\nroute add", "en0|setdns", "-DHCP", ""] {
            assert!(!slot_ok(bad), "accepted service {bad:?}");
        }
        // 三段的、五段的、前导零的、超 255 的、带空格的，都不是 IPv4。
        for bad in ["1.2.3", "1.2.3.4.5", "1.2.3.04", "300.1.1.1", " 1.2.3.4"] {
            assert!(!ipv4(bad), "accepted address {bad:?}");
        }
        // 白名单只收 IPv4 与 IPv4/len：`default`、主机名、IPv6 目标一律留给授权框。
        for bad in ["default", "host.example.com", "10.0.0.0/", "10.0.0.0/x", "10.0.0.256/8"] {
            assert!(!route_dest_ok(bad), "accepted route dest {bad:?}");
        }
        assert!(dns_ok(&[]));
        assert!(!dns_ok(&["1.1.1.1".into(), "".into()]));
        assert!(dns_ok(&["1.1.1.1".into(), "8.8.8.8".into()]));

        // 一整批常见操作都在通道内；掺一条 `default` 路由后整批改走授权框。
        let everyday = vec![
            PrivOp::SetManual {
                svc: "Wi-Fi".into(),
                ip: "192.168.1.20".into(),
                netmask: "255.255.255.0".into(),
                gateway: "192.168.1.1".into(),
            },
            PrivOp::SetDns {
                svc: "Wi-Fi".into(),
                servers: v(&["1.1.1.1", "8.8.8.8"]),
            },
            PrivOp::RouteAdd {
                dest: "10.0.0.0/8".into(),
                gateway: "192.168.1.1".into(),
                metric: 300,
            },
        ];
        assert!(allow_list_takes(&everyday));
        let mut with_default = everyday.clone();
        with_default.push(PrivOp::RouteAdd {
            dest: "default".into(),
            gateway: "192.168.1.1".into(),
            metric: 0,
        });
        assert!(!allow_list_takes(&with_default));
    }

    /// 这两段文本最终都由 root 执行，语法错的表现形式却是「授权框报一个看不懂的错」，
    /// 而其中一段还是编译时从仓库原文烤进二进制的。所以让 `/bin/sh -n` 先看一眼 ——
    /// 只解析、不执行，成本是两条子进程。
    #[test]
    fn the_scripts_root_runs_parse_as_shell() {
        let generated = priv_install_script(Path::new("/tmp/netsense-priv-wrapper.sh"), "imonior");
        for (name, body) in [
            ("installer", generated.as_str()),
            ("wrapper", PRIV_SCRIPT_SRC),
        ] {
            let path = std::env::temp_dir().join(format!(
                "netsense-shell-check-{}-{name}.sh",
                std::process::id()
            ));
            std::fs::write(&path, body).expect("cannot stage the script under test");
            let out = Command::new("/bin/sh")
                .args(["-n", &path.display().to_string()])
                .output()
                .expect("no /bin/sh to parse with");
            let _ = std::fs::remove_file(&path);
            assert!(
                out.status.success(),
                "{name}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// 在**不碰系统目录、也不提权**的沙箱里把生成的安装脚本真跑一遍：目标路径换成临时
    /// 目录、所有权参数去掉，其余文本（`mktemp`、`printf` 那一行、`visudo -cf`、落盘顺序）
    /// 与 root 那边执行的完全同源。这条挡住的是「拼出来的脚本跑得起来但装错东西」——
    /// 例如 sudoers 行少一个字段、包装脚本没落到 `PRIV_SCRIPT` 说的那个路径、引号把路径
    /// 截断。它**挡不住**：`-o root -g wheel` 是否被接受、`/usr/local/libexec` 是否可写、
    /// 以及这台机器的 sudo 策略最后让不让免密（那要现场输一次密码才知道）。
    #[test]
    fn the_installer_script_lands_the_wrapper_and_one_sudoers_line() {
        let dir = std::env::temp_dir().join(format!("netsense-priv-dryrun-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("no temp dir for the dry run");
        let staged_wrapper = dir.join("staged.sh");
        std::fs::write(&staged_wrapper, PRIV_SCRIPT_SRC).expect("cannot stage the wrapper");
        let installed = dir.join("netsense-priv.sh");
        let sudoers = dir.join("netsense");
        let script = priv_install_script(&staged_wrapper, "tester")
            .replace(PRIV_SCRIPT, &installed.display().to_string())
            // 先换完整路径，剩下的那一处才是 `install -d` 的目标目录
            .replace("/usr/local/libexec", &dir.display().to_string())
            .replace(SUDOERS_FILE, &sudoers.display().to_string())
            .replace("-o root -g wheel", "");
        let runner = dir.join("install.sh");
        std::fs::write(&runner, &script).expect("cannot stage the installer");
        let out = Command::new("/bin/sh")
            .arg(&runner)
            .output()
            .expect("no /bin/sh to run with");
        assert!(
            out.status.success(),
            "installer failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // 包装脚本按字节落到了它该落的路径，临时输入被安装脚本自己清掉了
        assert_eq!(
            std::fs::read(&installed).expect("wrapper never installed"),
            PRIV_SCRIPT_SRC.as_bytes()
        );
        assert!(!staged_wrapper.exists(), "staged wrapper left behind");
        // sudoers 里就是那一行：用户名、(root)、NOPASSWD、脚本的绝对路径
        assert_eq!(
            std::fs::read_to_string(&sudoers).expect("sudoers never written"),
            format!("tester ALL=(root) NOPASSWD: {}\n", installed.display())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `PRIV_SCRIPT_SRC` 是仓库里那份包装脚本的原文：界面下发的行格式（`encode()`）与
    /// 脚本认的子命令（case 分支）必须一直对齐。这条挡住的是「只改了一边」——
    /// 分叉之后的后果是配置在免密通道上静默失败。
    #[test]
    fn every_encoded_op_is_whitelisted_in_the_embedded_wrapper() {
        let ops = [
            PrivOp::SetDhcp { svc: "Wi-Fi".into() },
            PrivOp::SetManual {
                svc: "Wi-Fi".into(),
                ip: "192.168.1.10".into(),
                netmask: "255.255.255.0".into(),
                gateway: "192.168.1.1".into(),
            },
            PrivOp::SetDns {
                svc: "Wi-Fi".into(),
                servers: v(&["192.168.1.1"]),
            },
            PrivOp::SetV6Off { svc: "Wi-Fi".into() },
            PrivOp::SetV6Auto { svc: "Wi-Fi".into() },
            PrivOp::SetV6Manual {
                svc: "Wi-Fi".into(),
                addr: "fe80::1".into(),
                prefix: "64".into(),
                gateway: "fe80::ff".into(),
            },
            PrivOp::RouteAdd {
                dest: "10.0.0.0/8".into(),
                gateway: "192.168.1.1".into(),
                metric: 0,
            },
            PrivOp::RouteDelete {
                dest: "10.0.0.0/8".into(),
            },
        ];
        assert!(PRIV_SCRIPT_SRC.contains("--batch"), "no batch mode");
        for op in &ops {
            let name = op.encode().split('|').next().unwrap_or_default().to_string();
            assert!(
                PRIV_SCRIPT_SRC.contains(&format!("{name})")),
                "{name} is encoded by Rust but not whitelisted by the wrapper"
            );
        }
    }

    /// VPN 隧道在 `ifconfig` 里长得跟以太网口不一样：有的只有 `inet6`，有的干脆没地址。
    /// 网卡枚举直接读这张表，所以解析错一条就等于面板上少一类网卡。
    #[test]
    fn iface_snapshot_reads_running_state_and_real_addresses() {
        let out = concat!(
            "en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n",
            "\tinet 10.20.20.168 netmask 0xffffff00 broadcast 10.20.20.255\n",
            "\tinet6 fe80::18ae:1ff:fe2b:1c1%en0 prefixlen 64 secured scopeid 0x4\n",
            "en4: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n",
            "\tether d6:0f:20:b7:9a:f9\n",
            "utun3: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1380\n",
            "utun4: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1280\n",
            "\tinet6 fd1c:b7b7:1::2 prefixlen 64 \n",
            "ppp0: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1500\n",
            "\tinet 192.0.2.7 --> 192.0.2.8 netmask 0xffffffff\n",
            "anpi1: flags=8963<UP,BROADCAST,SMART,RUNNING,PROMISC,SIMPLEX,MULTICAST> mtu 1500\n",
            "\tstatus: nocarrier\n",
        );
        let m = parse_iface_snapshot(out);
        // 有 IPv4 的无线口
        assert_eq!(m.get("en0").unwrap().inet.as_deref(), Some("10.20.20.168"));
        // link-local 那一条不算地址：只有 fe80:: 的口（en4）仍然算「没地址」
        assert_eq!(m.get("en4").unwrap().inet, None);
        assert!(!m.get("en4").unwrap().v6_global);
        // 建了但没起来的隧道：RUNNING 为真、地址为空 → 枚举时被丢掉的正是这种
        assert_eq!(
            m.get("utun3"),
            Some(&IfaceSnap { running: true, inet: None, v6_global: false })
        );
        // 只有 IPv6 地址的隧道必须留下，否则用户的 VPN 在面板上凭空消失
        assert!(m.get("utun4").unwrap().v6_global);
        // 隧道行的 `inet A --> B` 取本端地址
        assert_eq!(m.get("ppp0").unwrap().inet.as_deref(), Some("192.0.2.7"));
        assert_eq!(m.len(), 6);
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

    /// 现场形态：一条只有 inet6 的隧道（没有 IPv4 可比），机器上还装着另一家客户端 ——
    /// 它的服务**常驻** `-listallnetworkservices`，断开也在，而且报不出地址。
    /// 这一条挡住的是「扫一遍服务名、命中谁就说谁建的」：那样界面会给出一个
    /// 和这台设备毫无关系的、听起来很具体的错答案。
    #[test]
    fn a_service_sitting_in_the_list_is_not_evidence_about_this_tunnel() {
        let ev = VpnEvidence {
            svc_ip: vec![("ProtonVPN".into(), "10.64.0.2".into())],
            connected: Vec::new(),
        };
        // 隧道自己没有 IPv4（只有 inet6）→ 地址这条线连不上，认不出
        assert_eq!(attribute_vpn(&ev, None, true), None);
        assert_eq!(attribute_vpn(&ev, Some(""), true), None);
        // 地址是别人的隧道（本例里那条服务）的，也不算这条的证据
        assert_eq!(attribute_vpn(&ev, Some("100.84.1.2"), true), None);
        // 连清单是空的也一样：认不出就是 None，交给界面退回通用标签
        assert_eq!(
            attribute_vpn(&VpnEvidence::default(), None, true),
            None
        );
    }

    /// IPv4 是唯一能把「这条隧道」和「那个服务」连起来的证据，所以它优先于任何推断。
    #[test]
    fn a_service_that_reports_the_tunnel_address_owns_it() {
        let ev = VpnEvidence {
            svc_ip: vec![
                ("AnyConnect".into(), "192.0.2.9".into()),
                ("Tailscale".into(), "100.84.1.2".into()),
            ],
            connected: v(&["ProtonVPN"]),
        };
        // 服务名照原样显示，不做关键词归一（自建名 "MyVPN" 归一成 "VPN" 是丢信息）
        assert_eq!(
            attribute_vpn(&ev, Some("100.84.1.2"), false).as_deref(),
            Some("Tailscale")
        );
        // 另一条隧道在用（alone=false）也照样认得出：IP 证据是设备绑定的
        assert_eq!(
            attribute_vpn(&ev, Some("192.0.2.9"), false).as_deref(),
            Some("AnyConnect")
        );
    }

    /// 「唯一的已连接会话」要两个唯一同时成立才算数：只有一条隧道在用，且只有一条会话连着。
    #[test]
    fn the_single_connected_session_counts_only_when_nothing_else_is_up() {
        let ev = VpnEvidence {
            svc_ip: Vec::new(),
            connected: v(&["Tailscale"]),
        };
        assert_eq!(
            attribute_vpn(&ev, None, true).as_deref(),
            Some("Tailscale")
        );
        // 还有第二条隧道在用 → 说不准这一条是谁建的
        assert_eq!(attribute_vpn(&ev, None, false), None);
        // 已连接会话有两条 → 同样说不准
        let two = VpnEvidence {
            svc_ip: Vec::new(),
            connected: v(&["Tailscale", "ProtonVPN"]),
        };
        assert_eq!(attribute_vpn(&two, None, true), None);
        assert_eq!(attribute_vpn(&two, None, false), None);
    }

    /// 会话标签的筛法：清单里未连的那几行不能进候选（本例只有一条已连）。
    #[test]
    fn only_connected_sessions_are_kept() {
        assert_eq!(connected_labels(&scutil_nc_rows(NC_LIST)), v(&["Tailscale"]));
        assert!(connected_labels(&[]).is_empty());
    }
}
