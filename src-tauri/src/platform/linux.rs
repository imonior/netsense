//! Linux 平台实现（以 NetworkManager / `nmcli` 为事实标准）。
//!
//! 说明：Linux 发行版差异极大，本实现**假定存在 NetworkManager**（桌面发行版的默认选择）。
//! 无 NetworkManager 的场景（server / systemd-networkd）不在当前范围内。
//!
//! 提权策略与 macOS 对称：优先 `sudo -n`（配合 sudoers 免密规则 → 无弹窗），
//! 不可用时回落 `pkexec`（图形授权框）。

use super::{
    extract_mac, poll_ssid_watch, run, timeout_secs, Health, InterfaceStatus, NetworkPlatform,
    PrivChannel, ProbeTarget, TunnelTarget, WatcherHandle,
};
use crate::config::{Mode, NetworkConfig, V6Mode};
use std::sync::OnceLock;

/// `launch_app` 到底该执行什么。单独成函数是为了把「选路」这件事测到 ——
/// 执行本身在这台机器上没法验（CI 没有图形会话）。
///
/// `xdg-open` 只接**一个**参数（要打开的东西），它不会替你把余下的参数转交给应用。
/// 所以配了 `args` 又走 xdg-open 是一条写错的配置，静悄悄丢掉 args 比报错更难查。
fn launch_plan(app: &str, args: &[String], is_exec: bool) -> Result<(String, Vec<String>), String> {
    if is_exec {
        return Ok((app.to_string(), args.to_vec()));
    }
    if !args.is_empty() {
        return Err(format!(
            "{} 不是可执行文件，只能交给 xdg-open，而它无法向应用传参（收到 {} 个参数）。\
             要给应用传参就把 app 写成可执行文件的绝对路径。",
            app,
            args.len()
        ));
    }
    Ok(("xdg-open".to_string(), vec![app.to_string()]))
}

/// 把进程**丢出去**就返回，不等它退出。
///
/// 与 `run()` 的分工就在这里：`run` 用 `.output()`，会阻塞到子进程结束，而一个浏览器
/// 能开一下午 —— 那样每次「启动应用」都会撞到 30 秒超时、被记成失败，可应用其实跑得好好的。
/// `spawn` 自己失败（文件不存在 / 没有执行位）才是失败：那已经是这个动作能给的全部信息。
/// 另起线程回收，否则这个常驻进程会攒下一堆僵尸。
fn spawn_detached(program: &str, args: &[String]) -> Result<(), String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn {}: {}", program, e))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// `nmcli -t` 用反斜杠转义分隔符（`\:` / `\\`），取值后需要还原。
fn unescape_t(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut esc = false;
    for c in s.chars() {
        if esc {
            out.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// 取 `nmcli -t -f A,B ...` 的一行并切成字段（每字段做反转义）。
fn split_t(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut esc = false;
    for c in line.chars() {
        if esc {
            cur.push('\\');
            cur.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == ':' {
            fields.push(unescape_t(&cur));
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    fields.push(unescape_t(&cur));
    fields
}

/// 点分掩码 → 前缀长度。
fn mask_to_prefix(mask: &str) -> Option<u32> {
    let mut bits = 0u32;
    let mut seen_zero = false;
    for part in mask.split('.') {
        let o: u32 = part.parse().ok()?;
        if o > 255 {
            return None;
        }
        for i in (0..8).rev() {
            if (o >> i) & 1 == 1 {
                if seen_zero {
                    return None; // 掩码不连续
                }
                bits += 1;
            } else {
                seen_zero = true;
            }
        }
    }
    Some(bits)
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

fn nmcli(args: &[&str]) -> Result<String, String> {
    run("nmcli", args)
}

/// 当前无线设备名（如 wlan0）。
fn wifi_iface() -> Option<String> {
    let out = nmcli(&["-t", "-f", "DEVICE,TYPE", "dev", "status"]).ok()?;
    for line in out.lines() {
        let f = split_t(line);
        if f.len() >= 2 && f[1].trim() == "wifi" {
            return Some(f[0].trim().to_string());
        }
    }
    None
}

/// 当前活动连接名（`nmcli con mod` 需要它）。
fn active_connection(dev: &str) -> Option<String> {
    let out = nmcli(&["-t", "-f", "NAME,DEVICE", "con", "show", "--active"]).ok()?;
    for line in out.lines() {
        let f = split_t(line);
        if f.len() >= 2 && f[1].trim() == dev {
            return Some(f[0].trim().to_string());
        }
    }
    None
}

/// 单个字段取值（`nmcli -g`）。
fn get_field(dev: &str, field: &str) -> Option<String> {
    let out = nmcli(&["-g", field, "device", "show", dev]).ok()?;
    let v = out.lines().map(|l| l.trim()).find(|l| !l.is_empty())?;
    // nmcli 对空值可能输出 "--"
    if v == "--" {
        None
    } else {
        Some(v.to_string())
    }
}

fn get_field_all(dev: &str, field: &str) -> Vec<String> {
    nmcli(&["-g", field, "device", "show", dev])
        .map(|o| {
            o.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty() && l != "--")
                .collect()
        })
        .unwrap_or_default()
}

/// 当前特权通道：`sudo -n` 可用 → Direct；否则 → Prompt（pkexec）。
///
/// 结果用 `OnceLock` 缓存：`tray_menu` 每次刷新都会问一次，
/// 不缓存的话每次都要起一个 `sudo` 进程。
pub fn priv_channel() -> PrivChannel {
    static C: OnceLock<PrivChannel> = OnceLock::new();
    *C.get_or_init(|| {
        if run("sudo", &["-n", "true"]).is_ok() {
            PrivChannel::Direct
        } else {
            PrivChannel::Prompt
        }
    })
}

fn is_nopasswd_unavailable(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("password is required")
        || s.contains("a terminal is required")
        || s.contains("no tty present")
        || s.contains("not allowed to execute")
        || s.contains("a password is required")
}

/// 提权执行一条命令：优先 `sudo -n`（无弹窗），失败回落 `pkexec`（弹授权框）。
fn run_priv(program: &str, args: &[&str]) -> Result<(), String> {
    if priv_channel() == PrivChannel::Direct {
        let mut full: Vec<&str> = vec!["-n", program];
        full.extend_from_slice(args);
        match run("sudo", &full) {
            Ok(_) => return Ok(()),
            Err(e) if is_nopasswd_unavailable(&e) => {}
            Err(e) => return Err(e),
        }
    }
    let mut full: Vec<&str> = vec![program];
    full.extend_from_slice(args);
    run("pkexec", &full).map(|_| ())
}

/// 无状态：`Copy` 让它可以按值传给后台线程（健康度监测）而不必套 `Arc`。
#[derive(Debug, Clone, Copy)]
pub struct LinuxPlatform;

/// NetworkManager 清单里的一条连接。
struct NmConn {
    name: String,
    /// `true` = 当前挂着活动设备。
    active: bool,
    /// 供厂商提示匹配的文本（连接类型 + 设备名）。
    hay: String,
}

/// nmcli 的连接清单。
///
/// `-t`（terse）用 `:` 分隔并把名字里的 `:` 转义成 `\:`，所以必须走 [`split_t`] +
/// [`unescape_t`]；直接用 `line.split(':')` 会在名字含冒号时切错列。
fn nm_connections() -> Vec<NmConn> {
    let Ok(out) = nmcli(&["-t", "-f", "NAME,TYPE,DEVICE", "con", "show"]) else {
        return Vec::new();
    };
    out.lines()
        .map(split_t)
        .filter(|f| f.len() >= 3)
        .map(|f| {
            let device = f[2].trim().to_string();
            NmConn {
                name: unescape_t(&f[0]),
                // `con show` 对未激活的连接把 DEVICE 留空，所以「有没有设备」就是活动判据
                active: !device.is_empty(),
                hay: format!("{} {}", f[1].trim(), device),
            }
        })
        .collect()
}

/// 这条隧道在 NetworkManager 里的状态：`None` = NM 不管它。
///
/// Linux 上厂商提示只能对「连接类型 + 设备名」做包含匹配（NM 清单里没有厂商字段），
/// 所以它**不参与否决**：对不上就退回纯名字匹配。NM 没有厂商概念，硬要求命中等于让
/// 所有 Linux 配置都报「找不到隧道」。
fn nm_tunnel_state(target: &TunnelTarget) -> Option<bool> {
    let conns = nm_connections();
    let by_name = |c: &NmConn| super::tunnel_name_eq(&c.name, target.name());
    conns
        .iter()
        .find(|c| by_name(c) && super::provider_in(target.provider(), &c.hay))
        .or_else(|| conns.iter().find(|c| by_name(c)))
        .map(|c| c.active)
}

/// 从 `ip link show <dev>` 的输出判断该接口是否已就绪。`None` = 输出里没有这个设备。
///
/// ⚠️ 判据是尖括号里的**标志位**（`UP,LOWER_UP`），不是 `state` 字段：WireGuard 接口
/// 正常工作时 `state` 打印的是 `UNKNOWN`（内核把它归类为 no-carrier 的非广播类型），
/// 只看 `state == UP` 会让所有 wg 隧道永远判成「没连上」，worker 于是每 N 秒重连一次。
///
/// 两个标志都要：`UP` = 管理上已启用，`LOWER_UP` = 链路本身可用（wg-quick 连上后的
/// 形态就是 `POINTOPOINT,NOARP,UP,LOWER_UP`）。
fn ip_link_ready(out: &str, dev: &str) -> Option<bool> {
    let flags = out
        .lines()
        // 设备定义行的形态是 `5: wg0: <POINTOPOINT,NOARP,UP,LOWER_UP> …`，
        // 名字在第二个冒号段里；缩进的 `link/none` 那行没有这一列，自然不会被选中
        .find(|l| l.split(':').nth(1).map(str::trim) == Some(dev))
        .and_then(|l| l.split_once('<'))?
        .1
        .split('>')
        .next()?;
    let has = |flag: &str| flags.split(',').any(|f| f.trim() == flag);
    Some(has("UP") && has("LOWER_UP"))
}

/// 内核里这个网络设备是否已就绪。`None` = 没有这个设备。
///
/// 这是给 `wg-quick` 那类**不归 NetworkManager 管**的 WireGuard 接口兜底的：
/// 它们在 nmcli 清单里根本不存在，只查 NM 就会永远判成「找不到隧道」。
fn ip_link_is_up(dev: &str) -> Option<bool> {
    let out = run("ip", &["link", "show", dev]).ok()?;
    ip_link_ready(&out, dev)
}

/// 列出 NM 已知的连接名，供「找不到隧道」的报错用（用户最需要的是名字清单）。
fn nm_connection_names() -> Vec<String> {
    nm_connections().into_iter().map(|c| c.name).collect()
}

/// 把一份配置编译成 `nmcli con mod` 的「键 值」参数对，不做任何 I/O。
///
/// DNS 是三态字段：`dns` 缺失就不产生 `ipv4.dns`（连接里现在挂着什么就继续用什么），
/// `dns: ""` 才是一条清空指令 —— nmcli 只有显式收到空串才会覆盖掉旧值。
fn con_mod_props(p: &NetworkConfig) -> Result<Vec<String>, String> {
    let mut mods: Vec<String> = Vec::new();
    match p.mode {
        Mode::Manual => {
            let ip = p.ip.clone().ok_or("manual 模式缺 ip")?;
            let mask = p.netmask.clone().ok_or("manual 模式缺 netmask")?;
            let gw = p.gateway.clone().ok_or("manual 模式缺 gateway")?;
            let prefix = mask_to_prefix(&mask).ok_or_else(|| format!("掩码不合法: {}", mask))?;
            mods.push("ipv4.method".into());
            mods.push("manual".into());
            mods.push("ipv4.addresses".into());
            mods.push(format!("{}/{}", ip, prefix));
            mods.push("ipv4.gateway".into());
            mods.push(gw);
        }
        Mode::Dhcp => {
            mods.push("ipv4.method".into());
            mods.push("auto".into());
            // 清空静态项必须传空串，否则旧值会残留
            mods.push("ipv4.addresses".into());
            mods.push(String::new());
            mods.push("ipv4.gateway".into());
            mods.push(String::new());
        }
    }
    if let Some(dns) = p.dns.as_deref() {
        mods.push("ipv4.dns".into());
        mods.push(
            dns.split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    match p.v6mode {
        Some(V6Mode::Off) => {
            mods.push("ipv6.method".into());
            mods.push("disabled".into());
        }
        Some(V6Mode::Automatic) => {
            mods.push("ipv6.method".into());
            mods.push("auto".into());
        }
        Some(V6Mode::Manual) => {
            let addr = p.ipv6.clone().ok_or("v6 manual 缺 ipv6")?;
            let prefix = p.v6prefix.clone().ok_or("v6 manual 缺 v6prefix")?;
            let gw = p.v6gateway.clone().ok_or("v6 manual 缺 v6gateway")?;
            mods.push("ipv6.method".into());
            mods.push("manual".into());
            mods.push("ipv6.addresses".into());
            mods.push(format!("{}/{}", addr, prefix));
            mods.push("ipv6.gateway".into());
            mods.push(gw);
        }
        None => {}
    }
    Ok(mods)
}

impl NetworkPlatform for LinuxPlatform {
    fn watch_ssid(&self, cb: Box<dyn Fn(Option<String>) + Send + Sync>) -> WatcherHandle {
        poll_ssid_watch(|| LinuxPlatform.get_current_ssid(), cb)
    }

    fn get_current_ssid(&self) -> Option<String> {
        // 从设备视角读，比 `dev wifi list` 更快也更准（不受扫描结果影响）
        let dev = wifi_iface()?;
        let conn = active_connection(&dev)?;
        // 连接的 802-11-wireless.ssid 才是真实 SSID（连接名可能被用户改过）
        let ssid = get_field(&dev, "GENERAL.CONNECTION")
            .unwrap_or(conn)
            .trim()
            .to_string();
        if ssid.is_empty() {
            None
        } else {
            Some(ssid)
        }
    }

    fn get_status(&self) -> InterfaceStatus {
        let mut st = InterfaceStatus::default();
        let Some(dev) = wifi_iface() else {
            return st;
        };
        st.iface = Some(dev.clone());
        st.ssid = self.get_current_ssid();
        st.connected = st.ssid.is_some();

        if let Some(addr) = get_field(&dev, "IP4.ADDRESS") {
            // 形如 192.168.1.5/24
            let (ip, prefix) = match addr.split_once('/') {
                Some((a, p)) => (a.to_string(), p.trim().parse::<u32>().ok()),
                None => (addr.clone(), None),
            };
            st.ipv4 = Some(ip);
            st.netmask = prefix.and_then(prefix_to_mask);
        }
        st.gateway = get_field(&dev, "IP4.GATEWAY");
        let dns = get_field_all(&dev, "IP4.DNS");
        if !dns.is_empty() {
            st.dns = Some(dns.join(","));
        }
        st.v6mode = get_field(&dev, "IP6.METHOD").map(|m| match m.as_str() {
            "auto" => "automatic".to_string(),
            "manual" => "manual".to_string(),
            "disabled" | "ignore" => "off".to_string(),
            other => other.to_string(),
        });

        // BSSID / 信号：`nmcli dev wifi list` 中 IN-USE 为 `*` 的那行
        if let Ok(out) = nmcli(&[
            "-t",
            "-f",
            "IN-USE,BSSID,SIGNAL",
            "dev",
            "wifi",
            "list",
            "--rescan",
            "no",
        ]) {
            for line in out.lines() {
                let f = split_t(line);
                if f.len() >= 3 && f[0].trim() == "*" {
                    let b = f[1].trim();
                    st.bssid = if b.is_empty() { None } else { Some(b.to_string()) };
                    if let Ok(sig) = f[2].trim().parse::<i32>() {
                        // nmcli SIGNAL 是 0-100 百分比 → dBm 近似
                        st.rssi = Some(sig / 2 - 100);
                    }
                    break;
                }
            }
        }

        // 网关 MAC：邻居表
        if let Some(gw) = st.gateway.clone() {
            if let Ok(out) = run("ip", &["neigh", "show", &gw]) {
                st.gateway_mac = extract_mac(&out);
            }
        }
        st
    }

    fn apply_network(&self, p: &NetworkConfig) -> Result<(), String> {
        let dev = wifi_iface().ok_or("未找到无线设备（请确认 NetworkManager 与 Wi-Fi 已启用）")?;
        let conn = active_connection(&dev)
            .ok_or_else(|| format!("设备 {} 上没有活动连接", dev))?;

        // 一次 `con mod` 下发 IP/网关/DNS（减少提权次数）
        let mods = con_mod_props(p)?;
        let mut args: Vec<&str> = vec!["con", "mod", &conn];
        for m in &mods {
            args.push(m.as_str());
        }
        run_priv("nmcli", &args)?;

        // 重新激活使配置生效
        run_priv("nmcli", &["con", "up", &conn])
    }

    fn set_dhcp(&self) -> Result<(), String> {
        let dev = wifi_iface().ok_or("未找到无线设备")?;
        self.set_dhcp_for(&dev)
    }

    /// 按**设备名**切回 DHCP：设备 → 活动连接（nmcli 的操作对象是连接而不是设备）。
    fn set_dhcp_for(&self, dev: &str) -> Result<(), String> {
        let conn = active_connection(dev).ok_or("没有活动连接")?;
        run_priv(
            "nmcli",
            &[
                "con", "mod", &conn, "ipv4.method", "auto", "ipv4.addresses", "", "ipv4.gateway",
                "", "ipv4.dns", "",
            ],
        )?;
        run_priv("nmcli", &["con", "up", &conn])
    }

    /// 枚举当前在用的全部网卡。
    ///
    /// 先取 `dev status`（设备 + 类型 + 状态 + 连接名），再对 `connected` 的设备逐个
    /// 取地址细节。VPN 的「软件名」在 Linux 上就是连接名（Tailscale / WireGuard / 自建
    /// 连接名），因此直接用它，并用 [`super::guess_vpn_app`] 归一化成产品名。
    fn list_interfaces(&self) -> Vec<super::NicInfo> {
        super::cached_nics(|| {
            let mut out: Vec<super::NicInfo> = Vec::new();
            let Ok(status) = nmcli(&["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "dev", "status"])
            else {
                return out;
            };
            let wifi = wifi_iface();
            for line in status.lines() {
                let f = split_t(line);
                if f.len() < 4 {
                    continue;
                }
                let dev = f[0].trim().to_string();
                let ty = f[1].trim().to_string();
                let state = f[2].trim().to_string();
                let conn = f[3].trim().to_string();
                if dev.is_empty() || dev == "lo" || state != "connected" {
                    continue;
                }
                let kind = match ty.as_str() {
                    "wifi" => super::NicKind::Wireless,
                    "ethernet" => super::NicKind::Wired,
                    "vpn" | "tun" | "tap" | "wireguard" => super::NicKind::Vpn,
                    _ if super::is_tunnel_device(&dev) => super::NicKind::Vpn,
                    _ => super::NicKind::Other,
                };

                let addr = get_field(&dev, "IP4.ADDRESS");
                let (ipv4, netmask) = match addr.as_deref().and_then(|a| a.split_once('/')) {
                    Some((ip, pfx)) => (
                        Some(ip.to_string()),
                        pfx.trim().parse::<u32>().ok().and_then(prefix_to_mask),
                    ),
                    None => (addr, None),
                };
                let gateway = get_field(&dev, "IP4.GATEWAY");
                let dns = get_field_all(&dev, "IP4.DNS");
                // 取 MAC 必须在 `dev` 被搬进 `NicInfo` 之前：`name: dev` 一移动，后面再
                // `&dev` 就是 use-after-move（这个函数体只有 Linux 腿会编译，本机看不到）。
                let mac = get_field(&dev, "GENERAL.HWADDR");
                let ssid = if kind == super::NicKind::Wireless && Some(&dev) == wifi.as_ref() {
                    self.get_current_ssid()
                } else {
                    None
                };
                if ipv4.is_none() && ssid.is_none() {
                    continue;
                }

                let gateway_mac = gateway.as_deref().and_then(|gw| {
                    run("ip", &["neigh", "show", gw])
                        .ok()
                        .and_then(|o| extract_mac(&o))
                });

                out.push(super::NicInfo {
                    name: dev,
                    label: if conn.is_empty() { None } else { Some(conn.clone()) },
                    kind,
                    up: true,
                    ssid,
                    mac,
                    ipv4,
                    netmask,
                    gateway,
                    gateway_mac,
                    dns: if dns.is_empty() { None } else { Some(dns.join(",")) },
                    app: if kind == super::NicKind::Vpn {
                        Some(
                            super::guess_vpn_app(&conn)
                                .map(|s| s.to_string())
                                .unwrap_or(conn),
                        )
                    } else {
                        None
                    },
                });
            }

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
        let icmp = || -> bool {
            let t = target.icmp_target.as_deref().unwrap_or("223.5.5.5");
            let secs = timeout_secs(timeout_ms);
            run("ping", &["-c", "1", "-W", &secs, t]).is_ok()
        };
        let http = || -> bool {
            let url = match target.http_target.as_deref() {
                Some(u) if !u.is_empty() => u,
                _ => return false,
            };
            let secs = timeout_secs(timeout_ms);
            match run(
                "curl",
                &["-sS", "-m", &secs, "-o", "/dev/null", "-w", "%{http_code}", url],
            ) {
                Ok(code) => matches!(code.trim().parse::<u16>(), Ok(c) if (200..400).contains(&c)),
                Err(_) => false,
            }
        };
        let dead = match target.mode {
            ProbeMode::Icmp => !icmp(),
            ProbeMode::Http => !http(),
            ProbeMode::Both => !(icmp() || http()),
        };
        if dead {
            Health::Fail
        } else {
            Health::Ok
        }
    }

    /// Linux 上隧道有两个来源：NetworkManager 的连接（含 NM 托管的 wireguard 类型），
    /// 以及 `wg-quick` 直接建出来的内核接口 —— 后者根本不在 NM 清单里。只查一边就会
    /// 出现「隧道明明连着，worker 却每 30 秒去重连一次」。
    fn tunnel_is_up(&self, target: &TunnelTarget) -> bool {
        match nm_tunnel_state(target) {
            Some(true) => true,
            // NM 不认识（或不活动）时再看内核接口状态
            _ => ip_link_is_up(target.name()).unwrap_or(false),
        }
    }

    /// ⚠️ 不走 `run_priv`：那条通道在 `sudo -n` 不可用时会回落 pkexec，而 pkexec
    /// 每次都要用户点授权框 —— worker 每 N 秒就弹一次，等于把界面钉死在授权窗上。
    /// 因此权限不够时照实报错，由用户决定是配 sudoers 免密还是自己连。
    fn tunnel_connect(&self, target: &TunnelTarget) -> Result<(), String> {
        let name = target.name();
        if nm_tunnel_state(target).is_some() {
            return nmcli(&["connection", "up", name]).map(|_| ()).map_err(|e| {
                format!("nmcli connection up {} 失败: {}（需要密码或权限时本应用不会代答）", name, e)
            });
        }
        if ip_link_is_up(name).is_some() {
            // 接口在，只是没起来。先按普通用户试；失败后只有在 `sudo -n` 已经免密
            // （PrivChannel::Direct）时才走 sudo —— 这条分支永远不会弹框。
            if run("ip", &["link", "set", name, "up"]).is_ok() {
                return Ok(());
            }
            if priv_channel() == PrivChannel::Direct {
                return run("sudo", &["-n", "ip", "link", "set", name, "up"])
                    .map(|_| ())
                    .map_err(|e| format!("sudo -n ip link set {} up 失败: {}", name, e));
            }
            return Err(format!(
                "内核接口 {} 存在但没能拉起，而 `ip link set up` 需要 root。\
                 本应用不在后台弹授权框：请给该接口建一条 NetworkManager 连接，或配好 sudoers 免密。",
                name
            ));
        }
        let known = nm_connection_names();
        Err(if known.is_empty() {
            format!(
                "Linux 上找不到隧道 {}：NetworkManager 里没有任何连接，内核也没有这个名字的设备",
                name
            )
        } else {
            format!(
                "Linux 上找不到隧道 {}；NM 已有连接：{}",
                name,
                known.join(", ")
            )
        })
    }

    fn add_route(&self, dest: &str, gw: &str, metric: u32) -> Result<(), String> {
        let args: Vec<String> = if metric > 0 {
            vec![
                "route".into(),
                "add".into(),
                dest.into(),
                "via".into(),
                gw.into(),
                "metric".into(),
                metric.to_string(),
            ]
        } else {
            vec![
                "route".into(),
                "add".into(),
                dest.into(),
                "via".into(),
                gw.into(),
            ]
        };
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_priv("ip", &refs)
    }

    fn delete_route(&self, dest: &str) -> Result<(), String> {
        run_priv("ip", &["route", "del", dest])
    }

    fn launch_app(&self, app: &str, args: &[String]) -> Result<(), String> {
        // 可执行文件直接跑；否则交给 xdg-open（能处理 .desktop 与文档/URL）
        let is_exec = std::path::Path::new(app)
            .metadata()
            .map(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && (m.permissions().mode() & 0o111) != 0
            })
            .unwrap_or(false);
        let (program, argv) = launch_plan(app, args, is_exec)?;
        spawn_detached(&program, &argv)
    }

    fn run_script(&self, path: &str, args: &[String], elevated: bool) -> Result<(), String> {
        let mut full = vec![path.to_string()];
        full.extend(args.iter().cloned());
        if elevated {
            // 用户脚本提权：显式走授权框（sudo -n 失败则 pkexec），与网络配置操作区别对待
            let refs: Vec<&str> = full.iter().map(|s| s.as_str()).collect();
            run_priv(path, &refs)
        } else {
            super::run_owned(path, args).map(|_| ())
        }
    }

    fn list_known_ssids(&self) -> Option<Vec<String>> {
        // 取所有 802-11-wireless 类型的已保存连接名（≈ 已保存的 SSID）
        let out = nmcli(&["-t", "-f", "NAME,TYPE", "con", "show"]).ok()?;
        let list: Vec<String> = out
            .lines()
            .map(split_t)
            .filter(|f| f.len() >= 2 && f[1].trim() == "802-11-wireless")
            .map(|f| f[0].trim().to_string())
            .filter(|s| !s.is_empty())
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
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_executable_is_run_as_is_and_keeps_its_arguments() {
        assert_eq!(
            launch_plan("/usr/bin/foo", &v(&["--flag", "a b"]), true).unwrap(),
            ("/usr/bin/foo".to_string(), v(&["--flag", "a b"]))
        );
    }

    #[test]
    fn anything_else_goes_to_xdg_open_and_only_alone() {
        assert_eq!(
            launch_plan("firefox", &[], false).unwrap(),
            ("xdg-open".to_string(), v(&["firefox"]))
        );
        // xdg-open 不会替我们把余下参数转交给应用：静悄悄丢掉比报错更难查，所以直接报错
        assert!(launch_plan("firefox", &v(&["--new-window"]), false)
            .unwrap_err()
            .contains("xdg-open"));
    }

    /// 真实形态：WireGuard 接口连上后 `state` 是 `UNKNOWN`，就绪信息只在标志位里。
    /// 所以「只看 state」会把正常工作的隧道判成没连上，worker 于是每 N 秒重连一次。
    const WG_UP: &str = "5: wg0: <POINTOPOINT,NOARP,UP,LOWER_UP> mtu 1420 qdisc noqueue state UNKNOWN mode DEFAULT group default qlen 1000\n    link/none";
    const WG_DOWN: &str =
        "5: wg0: <POINTOPOINT,NOARP> mtu 1420 qdisc noop state DOWN mode DEFAULT group default qlen 1000\n    link/none";
    const ETH_CABLE_OUT: &str =
        "2: eth0: <BROADCAST,MULTICAST,UP> mtu 1500 qdisc fq_codel state DOWN mode DEFAULT group default qlen 1000\n    link/ether 00:11:22:33:44:55";

    #[test]
    fn a_ready_tunnel_needs_both_flags_not_the_state_word() {
        assert_eq!(ip_link_ready(WG_UP, "wg0"), Some(true));
        assert_eq!(ip_link_ready(WG_DOWN, "wg0"), Some(false));
        // 管理上启用但对端不通：LOWER_UP 缺失，就是「还没连上」
        assert_eq!(ip_link_ready(ETH_CABLE_OUT, "eth0"), Some(false));
        // 查的不是这台设备时不能瞎答
        assert_eq!(ip_link_ready(WG_UP, "wg1"), None);
    }

    /// nmcli 的 terse 输出把名字里的 `:` 转义成 `\:`，一列一个字段。
    #[test]
    fn nmcli_terse_rows_keep_their_columns() {
        let line = "Office\\:VPN:wireguard:wg0";
        let f = split_t(line);
        assert_eq!(f.len(), 3, "转义的冒号不该被当成分隔符: {:?}", f);
        assert_eq!(unescape_t(&f[0]), "Office:VPN");
        // 未激活的连接 DEVICE 为空（`name:type:`），这正是 nm_tunnel_state 的活动判据
        let idle = split_t("Cafe:wireguard:");
        assert_eq!(idle.len(), 3);
        assert!(idle[2].trim().is_empty());
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

    /// 编辑器表达「DNS 保持不变」的方式是把 `dns` 键整个删掉。nmcli 只在收到
    /// `ipv4.dns` 这一对参数时才动 DNS，所以缺键时连那一对参数都不该出现。
    #[test]
    fn an_absent_dns_key_emits_no_ipv4_dns_pair() {
        for mode in [Mode::Manual, Mode::Dhcp] {
            let props = con_mod_props(&cfg(mode.clone(), None)).unwrap();
            assert!(
                !props.iter().any(|p| p == "ipv4.dns"),
                "{:?} 缺 dns 键却下发了 DNS: {:?}",
                mode,
                props
            );
        }
        // 地址本身照旧下发，别把「不动 DNS」误读成「什么都不动」
        let manual = con_mod_props(&cfg(Mode::Manual, None)).unwrap();
        assert!(manual
            .windows(2)
            .any(|w| w[0] == "ipv4.addresses" && w[1] == "192.168.1.100/24"));
    }

    /// 与上一个用例只差一个 `Some`：空串是一对真实的清空参数。
    #[test]
    fn an_empty_dns_string_still_clears_the_connection() {
        let props = con_mod_props(&cfg(Mode::Manual, Some(""))).unwrap();
        let i = props
            .iter()
            .position(|p| p == "ipv4.dns")
            .expect("空串 dns 必须下发清空指令");
        assert_eq!(props[i + 1], "", "清空就是空串，旧值才会被覆盖掉");
        let props = con_mod_props(&cfg(Mode::Manual, Some(" 8.8.8.8 ,1.1.1.1, "))).unwrap();
        let i = props.iter().position(|p| p == "ipv4.dns").unwrap();
        assert_eq!(props[i + 1], "8.8.8.8 1.1.1.1", "nmcli 那边是空格分隔");
    }
}
