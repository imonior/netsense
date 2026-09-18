//! Linux 平台实现（以 NetworkManager / `nmcli` 为事实标准）。
//!
//! 说明：Linux 发行版差异极大，本实现**假定存在 NetworkManager**（桌面发行版的默认选择）。
//! 无 NetworkManager 的场景（server / systemd-networkd）不在当前范围内。
//!
//! 提权策略与 macOS 对称：优先 `sudo -n`（配合 sudoers 免密规则 → 无弹窗），
//! 不可用时回落 `pkexec`（图形授权框）。

use super::{
    extract_mac, normalize_mac, poll_ssid_watch, run, timeout_secs, Health, InterfaceStatus,
    NetworkPlatform, PrivChannel, ProbeTarget, WatcherHandle,
};
use crate::config::{Mode, Profile, V6Mode};
use std::sync::OnceLock;

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

pub struct LinuxPlatform;

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

    fn apply_profile(&self, p: &Profile) -> Result<(), String> {
        let dev = wifi_iface().ok_or("未找到无线设备（请确认 NetworkManager 与 Wi-Fi 已启用）")?;
        let conn = active_connection(&dev)
            .ok_or_else(|| format!("设备 {} 上没有活动连接", dev))?;

        let dns_csv = p
            .dns
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");

        // 一次 `con mod` 下发 IP/网关/DNS（减少提权次数）
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
                mods.push("ipv4.dns".into());
                mods.push(dns_csv.clone());
            }
            Mode::Dhcp => {
                mods.push("ipv4.method".into());
                mods.push("auto".into());
                // 清空静态项必须传空串，否则旧值会残留
                mods.push("ipv4.addresses".into());
                mods.push(String::new());
                mods.push("ipv4.gateway".into());
                mods.push(String::new());
                mods.push("ipv4.dns".into());
                mods.push(dns_csv.clone());
            }
        }
        // IPv6
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
        let conn = active_connection(&dev).ok_or("没有活动连接")?;
        run_priv(
            "nmcli",
            &[
                "con", "mod", &conn, "ipv4.method", "auto", "ipv4.addresses", "", "ipv4.gateway",
                "", "ipv4.dns", "",
            ],
        )?;
        run_priv("nmcli", &["con", "up", &conn])
    }

    fn resolve_gateway_mac(&self) -> Option<String> {
        self.get_status().gateway_mac
    }

    fn resolve_bssid(&self) -> Option<String> {
        self.get_status().bssid
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
        if is_exec {
            super::run_owned(app, args).map(|_| ())
        } else {
            let mut all = vec![app.to_string()];
            all.extend(args.iter().cloned());
            super::run_owned("xdg-open", &all).map(|_| ())
        }
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
