//! Linux 平台实现（以 NetworkManager / `nmcli` 为事实标准）。
//!
//! 说明：Linux 发行版差异极大，本实现**假定存在 NetworkManager**（桌面发行版的默认选择）。
//! 无 NetworkManager 的场景（server / systemd-networkd）不在当前范围内。
//!
//! 提权策略与 macOS 对称：优先 `sudo -n`（配合 sudoers 免密规则 → 无弹窗），
//! 不可用时回落 `pkexec`（图形授权框）。
//!
//! 打印机走 CUPS 客户端命令（`lpstat` / `lpoptions`），与 macOS 共用同一套解析。

use super::{
    dedupe_sort_apps, extract_mac, poll_ssid_watch, prefers_dark_from_gsettings, printers_from_lpstat,
    run, run_env,
    timeout_secs, AppEntry, C_LOCALE,
    Health, InterfaceStatus, MAX_ROUTES_PER_IFACE, NetworkPlatform, PrinterInfo, PrivChannel, ProbeTarget, TunnelTarget,
    WatcherHandle,
};
use crate::config::{Mode, NetworkConfig, V6Mode};
use crate::config::model::NetworkTarget;
use crate::i18n;
use crate::platform::linux_nm::{self, DeviceIp};
use std::sync::OnceLock;

/// 目标在这台机器上**到底是什么**，决定交给谁去执行。
///
/// 只有三条出路，因为「拿 `xdg-open` 打开一个不存在的路径」根本不是打开，而是
/// 一次没人看的失败：`spawn_detached` 只看 `xdg-open` 起没起来，不看它的退出码，
/// 于是动作徽标会显示成功。这一层就是把它变成真正的错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// 直接 exec：要么是有执行位的文件，要么是一个应用名（交给 PATH 解析，
    /// 找不到时 `spawn` 自己会报 No such file or directory）
    Run,
    /// 存在但没有执行位（`.desktop`、文档、目录），或带 scheme 的 URL：只能给 `xdg-open`
    OpenWithXdg,
    /// 写成了路径，而系统上没有这个东西
    NotFound,
}

/// 判定只用两条元数据事实（在不在、有没有执行位）加字符串形状，不查机器上装了什么，
/// 所以整条决定能在任何一条腿上测到。
fn classify_target(app: &str, exists: bool, has_exec_bit: bool) -> Target {
    if has_exec_bit {
        return Target::Run;
    }
    // scheme 先判：`https://x/y` 里有 '/'，但它从来就不是本机路径。
    if app.contains("://") {
        return Target::OpenWithXdg;
    }
    if exists {
        return Target::OpenWithXdg;
    }
    if app.contains('/') {
        return Target::NotFound;
    }
    Target::Run
}

/// `launch_app` 到底该执行什么。单独成函数是为了把「选路」这件事测到 ——
/// 执行本身在这台机器上没法验（CI 没有图形会话）。
///
/// `xdg-open` 只接**一个**参数（要打开的东西），它不会替你把余下的参数转交给应用。
/// 所以配了 `args` 又走 xdg-open 是一条写错的配置，静悄悄丢掉 args 比报错更难查。
fn launch_plan(app: &str, args: &[String], target: Target) -> Result<(String, Vec<String>), String> {
    match target {
        Target::Run => Ok((app.to_string(), args.to_vec())),
        Target::OpenWithXdg => {
            if !args.is_empty() {
                return Err(i18n::tf("pal.xdg_no_args", &[
                    ("app", app),
                    ("count", &args.len().to_string()),
                ]));
            }
            Ok(("xdg-open".to_string(), vec![app.to_string()]))
        }
        Target::NotFound => Err(i18n::tf("pal.launch_missing", &[("app", app)])),
    }
}

/// 把进程**丢出去**就返回，不等它退出。
///
/// 与 `run()` 的分工就在这里：`run` 用 `.output()`，会阻塞到子进程结束，而一个浏览器
/// 能开一下午 —— 那样每次「启动应用」都会撞到 30 秒超时、被记成失败，可应用其实跑得好好的。
/// `spawn` 自己失败（文件不存在 / 没有执行位）才是失败：那已经是这个动作能给的全部信息。
/// 正因为「丢出去」看不见子进程的退出码，路径不存在这类错必须在 `launch_plan` 里先拒掉，
/// 不能留给 `xdg-open` 去失败 —— 它失败了也没人看。
/// 另起线程回收，否则这个常驻进程会攒下一堆僵尸。
fn spawn_detached(program: &str, args: &[String]) -> Result<(), String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| {
            i18n::tf(
                "pal.spawn_failed",
                &[("program", program), ("error", &e.to_string())],
            )
        })?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

// —————————————————————————— 启动程序（3B launch_app 的候选） ——————————————————————————

/// `.desktop` 的 `Exec=` 行 → 第一个可执行的 token（程序本体）。
///
/// 规则按 desktop 入口规范：引号里整段算一个 token，`\\` 转义下一个字符；`%U` 一族
/// 字段码（`%f`/`%u`/`%F`/`%U`/`%d`/`%D`/`%n`/`%N`/`%i`/`%c`/`%k`/`%v`/`%m`）整个消失，
/// `%%` 是字面百分号；`env VAR=1 prog`（少见的合法写法）里的赋值前缀不是程序名。
fn exec_first_token(exec: &str) -> Option<String> {
    let mut toks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ' ' | '\t' if !quoted => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            '%' => {
                if chars.next() == Some('%') {
                    cur.push('%');
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    let mut it = toks.into_iter();
    let mut head = it.next()?;
    if head == "env" {
        loop {
            head = it.next()?;
            if !head.contains('=') {
                break;
            }
        }
    }
    Some(head)
}

/// 一份 `.desktop` 文件的内容 → 一条可启动程序。
///
/// 只认 `[Desktop Entry]` 这一段：后面的 `[Desktop Action …]` 是右键菜单项，名字与
/// Exec 都不是「程序本身」。`NoDisplay=true`（不想在菜单里露脸）与 `Hidden=true`
/// （已被同名文件遮蔽）是文件自己说别显示，两者都不进下拉；`Type` 不是 `Application`
/// 的（`Link` 是网址）也不要。
///
/// 名字按 `Name[zh_CN]` → `Name[zh]` → `Name` 的顺序挑（GLib 同一套匹配规则），
/// `lang` 为空时只会落到最后那一条。
fn app_from_desktop(text: &str, lang: &str) -> Option<AppEntry> {
    let norm = |s: &str| {
        s.split('.')
            .next()
            .unwrap_or("")
            .replace('-', "_")
            .to_lowercase()
    };
    let want = norm(lang);
    let mut in_entry = false;
    let mut hidden = false;
    let mut is_app = true;
    let mut exec: Option<String> = None;
    let mut name: Option<(u8, String)> = None; // (匹配强度, 名字)
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let rank = if k == "Name" {
            0
        } else if let Some(tag) = k.strip_prefix("Name[").and_then(|r| r.strip_suffix(']')) {
            let t = norm(tag);
            if !want.is_empty() && t == want {
                2
            } else if want.contains('_') && t == want.split('_').next().unwrap_or("") {
                1
            } else {
                continue;
            }
        } else {
            match k {
                "NoDisplay" | "Hidden" => hidden |= v.eq_ignore_ascii_case("true"),
                "Type" => is_app = v == "Application",
                "Exec" => exec = Some(v.to_string()),
                _ => {}
            }
            continue;
        };
        if !v.is_empty() && name.as_ref().map_or(true, |(r, _)| rank > *r) {
            name = Some((rank, v.to_string()));
        }
    }
    if hidden || !is_app {
        return None;
    }
    let path = exec_first_token(exec.as_deref()?)?;
    let (_, name) = name?;
    Some(AppEntry { name, path })
}

/// 三个 XDG 位置的 `.desktop` 清单目录：系统装的两处 + 用户自己放的
/// （`~/.local/share/applications`，AppImage 安装器与 `desktop-file-install` 都写这里）。
fn desktop_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = vec![
        std::path::PathBuf::from("/usr/share/applications"),
        std::path::PathBuf::from("/usr/local/share/applications"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::PathBuf::from(home).join(".local/share/applications"));
    }
    dirs
}

/// `Name[xx]` 要对的标签，与 [`LinuxPlatform::ui_language`] 取同一层环境变量
/// （`LANGUAGE` → `LC_ALL` → `LANG`，多级取第一项）。取不到返回空串：
/// 这时只认无标签的 `Name`，不猜。
fn desktop_lang() -> String {
    for name in ["LANGUAGE", "LC_ALL", "LANG"] {
        let v = std::env::var(name).unwrap_or_default();
        let v = v.split(':').next().unwrap_or("").trim().to_string();
        if !v.is_empty() && v != "C" && v != "POSIX" {
            return v;
        }
    }
    String::new()
}

/// 跑一条「取消也算一种正常结局」的对话框命令：返回 `(是否成功退出, stdout)`。
///
/// 不能用 `run`：它把非 0 一律折进 `Err`（stderr 空时退化成一个模板化报错），而
/// zenity/kdialog 取消时正是「退非 0 且什么都不说」—— 用 `run` 就分不出「用户取消」
/// 与「对话框起不来」。spawn 本身失败（没装）才是 `Err`。
fn run_dialog(program: &str, args: &[&str]) -> Result<(bool, String), String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| {
            i18n::tf(
                "pal.spawn_failed",
                &[("program", program), ("error", &e.to_string())],
            )
        })?;
    Ok((out.status.success(), String::from_utf8_lossy(&out.stdout).to_string()))
}

/// `PATH` 里有没有这个命令（`zenity` / `kdialog` 二选一时用）。
fn in_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|p| p.join(program).is_file())
    })
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
///
/// 多张 Wi-Fi 网卡时优先挑**已连接**的那张（`STATE == connected`），否则落到任意一张
/// 没在用的卡上、下发就打空。没有已连接的再退回第一张 wifi 设备（尽力而为）。
fn wifi_iface() -> Option<String> {
    let out = nmcli(&["-t", "-f", "DEVICE,TYPE,STATE", "dev", "status"]).ok()?;
    let mut fallback: Option<String> = None;
    for line in out.lines() {
        let f = split_t(line);
        if f.len() >= 3 && f[1].trim() == "wifi" {
            let dev = f[0].trim().to_string();
            if f[2].trim() == "connected" {
                return Some(dev);
            }
            if fallback.is_none() {
                fallback = Some(dev);
            }
        }
    }
    fallback
}

/// 当前默认出口设备名。
fn primary_iface() -> Option<String> {
    let out = run("ip", &["-4", "route", "show", "default"]).ok()?;
    out.lines()
        .find(|l| l.starts_with("default"))
        .and_then(|l| l.split_whitespace().nth(4))
        .map(|s| s.to_string())
}

/// 当前以太网设备名（挑已连接的）。
fn ethernet_iface() -> Option<String> {
    let out = nmcli(&["-t", "-f", "DEVICE,TYPE,STATE", "dev", "status"]).ok()?;
    let mut fallback: Option<String> = None;
    for line in out.lines() {
        let f = split_t(line);
        if f.len() >= 3 && f[1].trim() == "ethernet" {
            let dev = f[0].trim().to_string();
            if f[2].trim() == "connected" {
                return Some(dev);
            }
            if fallback.is_none() {
                fallback = Some(dev);
            }
        }
    }
    fallback
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

/// 读某条已保存连接的某个设置字段（`nmcli -t -f <field> connection show <name>`）。
/// 用于取「真实 SSID」这类藏在连接设置里、不在设备视图中的字段。
fn get_connection_field(conn: &str, field: &str) -> Option<String> {
    nmcli(&["-t", "-f", field, "connection", "show", conn])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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

/// `nmcli -g IP4.ROUTE` 的行 → 目的前缀清单（去重、剥掉多播/广播、上限
/// [`MAX_ROUTES_PER_IFACE`]）。
///
/// nmcli 给的是 `dst = 10.0.2.0/24, nh = 0.0.0.0, mt = 100`，界面那一格只要目的前缀；
/// 多播段（224.0.0.0/4 起）与全网广播每条链路都有，写出来只会把真正管的那几个网挤掉。
fn route_prefixes(rows: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for row in rows {
        let rest = match row.split_once("dst =") {
            Some((_, rest)) => rest,
            None => row,
        };
        let pfx = rest.split(',').next().unwrap_or("").trim();
        if !pfx.contains('/') {
            continue;
        }
        let first = pfx.split('/').next().unwrap_or("").split('.').next().unwrap_or("");
        if first.parse::<u8>().map(|o| o >= 224).unwrap_or(false) {
            continue;
        }
        if out.len() < MAX_ROUTES_PER_IFACE && !out.iter().any(|p| p == pfx) {
            out.push(pfx.to_string());
        }
    }
    out
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
    /// `TYPE` 列（`vpn` / `wireguard` / `802-11-wireless` / …）。
    ty: String,
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
                ty: f[1].trim().to_string(),
            }
        })
        .collect()
}

/// NM 的连接类型是否属于 VPN。
///
/// 与 [`LinuxPlatform::list_interfaces`] 里给在用设备分类型用的是同一组取值：`vpn` 是 NM
/// 的第三方 VPN 大类，另外三种是隧道设备类。
fn is_vpn_conn_type(ty: &str) -> bool {
    matches!(ty, "vpn" | "wireguard" | "tun" | "tap")
}

/// 「装了、此刻没连」的 VPN 连接：给界面一张不带地址、状态写着没连的卡。
///
/// 在用的那些来自 `dev status`，而未激活的连接**根本没有设备**，那条路走不到它们 —— 可用户
/// 问的正是「我装的那些 VPN 现在连着没有」，这份答案只在 `con show` 里有。
/// 只收 VPN 类型：没连的 Wi-Fi／有线连接列进来只会把面板挤成一堆用不上的名字。
///
/// 名字用连接名：Linux 上那就是用户在桌面网络里看到的那个大名，与在用的那条同源，所以
/// 归属判定在这里给得出来（不像 macOS 要等证据，那边只能确认会话叫什么、不能确认隧道是谁建的）。
fn idle_vpn_conns(conns: &[NmConn], live: &[super::NicInfo]) -> Vec<super::NicInfo> {
    let mut seen: Vec<&str> = Vec::new();
    conns
        .iter()
        .filter(|c| !c.active && is_vpn_conn_type(&c.ty))
        .map(|c| c.name.as_str())
        .filter(|&name| {
            !live.iter().any(|n| {
                n.kind == super::NicKind::Vpn
                    && (n.name == name
                        || n.label.as_deref() == Some(name)
                        || n.app.as_deref() == Some(name))
            })
        })
        .filter(|&name| {
            if seen.contains(&name) {
                return false;
            }
            seen.push(name);
            true
        })
        .map(|name| super::NicInfo {
            name: name.to_string(),
            // 未激活的连接没有设备名，标题能用的只有连接名；`label` 同时填上，面板的标题
            // 才是这个名字本身，而不是在后面再缀一遍「(VPN)」—— 类型由旁边那枚标签说。
            label: Some(name.to_string()),
            kind: super::NicKind::Vpn,
            up: false,
            app: super::vpn_app_for(name),
            ..Default::default()
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
    // 原生 netlink 优先；netlink 不可用（容器、权限、解析失败）时回退 `ip link show` 文本解析。
    crate::platform::linux_netlink::link_is_up(dev).or_else(|| {
        let out = run("ip", &["link", "show", dev]).ok()?;
        ip_link_ready(&out, dev)
    })
}

/// `nmcli -g device show <dev>` 的逐设备字段读取，归一成 [`DeviceIp`]。
///
/// 这是原生 D-Bus 读取（`linux_nm::device_ip`）的兜底：当这台发行版的 NM D-Bus 属性签名
/// 与预期不符、或没有 system bus 时回落到这里，行为完全等价于原生改造前的实现。
fn device_ip_nmcli(dev: &str) -> DeviceIp {
    let mut out = DeviceIp::default();
    if let Some(addr) = get_field(dev, "IP4.ADDRESS") {
        let (ip, prefix) = match addr.split_once('/') {
            Some((a, p)) => (
                Some(a.to_string()),
                p.trim().parse::<u32>().ok().and_then(prefix_to_mask),
            ),
            None => (Some(addr.clone()), None),
        };
        out.ipv4 = ip;
        out.netmask = prefix;
    }
    out.gateway = get_field(dev, "IP4.GATEWAY");
    let dns = get_field_all(dev, "IP4.DNS");
    if !dns.is_empty() {
        out.dns = Some(dns.join(","));
    }
    // 全局 IPv6：跳过 link-local（`fe80:`），取第一条真正全局地址（与 `list_interfaces` 同判据）。
    out.ipv6 = get_field_all(dev, "IP6.ADDRESS")
        .into_iter()
        .find(|a| !a.trim().to_ascii_lowercase().starts_with("fe80:"))
        .map(|a| a.split('/').next().unwrap_or("").trim().to_string())
        .filter(|a| !a.is_empty());
    out.gateway6 = get_field(dev, "IP6.GATEWAY").filter(|g| g != "::");
    out.mac = get_field(dev, "GENERAL.HWADDR");
    out.routes = route_prefixes(&get_field_all(dev, "IP4.ROUTE"));
    out
}

/// 设备当前的 IP 配置：原生 NetworkManager D-Bus 优先，失败回落 `nmcli -g device show`。
///
/// 返回 `Some` 始终成立（nmcli 兜底总能产出一份结构，字段取不到即 `None`）；
/// 仅在连 system bus 都失败时原生返回 `None`，此时由 [`device_ip_nmcli`] 兜底。
fn device_ip(dev: &str) -> Option<DeviceIp> {
    linux_nm::device_ip(dev).or_else(|| Some(device_ip_nmcli(dev)))
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
            let prefix = mask_to_prefix(&mask)
                .ok_or_else(|| i18n::tf("pal.mask_invalid", &[("mask", &mask)]))?;
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
            let addr = p
                .ipv6
                .clone()
                .ok_or_else(|| i18n::tf("pal.v6_manual_missing", &[("field", "ipv6")]))?;
            let prefix = p
                .v6prefix
                .clone()
                .ok_or_else(|| i18n::tf("pal.v6_manual_missing", &[("field", "v6prefix")]))?;
            let gw = p
                .v6gateway
                .clone()
                .ok_or_else(|| i18n::tf("pal.v6_manual_missing", &[("field", "v6gateway")]))?;
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
        // 连接名（`GENERAL.CONNECTION`）可能被用户改过，不可信；真正的 SSID 在该连接
        // 的 `802-11-wireless.ssid` 设置里。先取真实 SSID，取不到再退回到连接名。
        let ssid = get_connection_field(&conn, "802-11-wireless.ssid")
            .or_else(|| get_field(&dev, "GENERAL.CONNECTION"))
            .map(|s| s.trim().to_string());
        ssid.filter(|s| !s.is_empty())
    }

    fn get_status(&self) -> InterfaceStatus {
        let mut st = InterfaceStatus::default();
        let Some(dev) = wifi_iface() else {
            return st;
        };
        st.iface = Some(dev.clone());
        st.ssid = self.get_current_ssid();
        st.connected = st.ssid.is_some();

        // IP / 网关 / DNS / IPv6 走原生 D-Bus（失败回落 `nmcli -g device show`）。
        if let Some(dip) = device_ip(&dev) {
            st.ipv4 = dip.ipv4;
            st.netmask = dip.netmask;
            st.gateway = dip.gateway;
            st.dns = dip.dns;
        }
        // `v6mode` 需要从连接 setting 推导，原生读取较重，保留这一条 `nmcli` 字段查询。
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

        // 网关 MAC：邻居表（netlink 优先，`ip neigh` 兜底）
        if let Some(gw) = st.gateway.clone() {
            st.gateway_mac = crate::platform::linux_netlink::gateway_mac(&gw)
                .or_else(|| run("ip", &["neigh", "show", &gw]).ok().and_then(|o| extract_mac(&o)));
        }
        st
    }

    /// 这一腿的快照从来不留缓存（上面每次都是现场 `nmcli`），所以「新鲜的」与「现在这份」
    /// 是同一个东西。写出来而不是省略，是因为契约要求三份实现各自表态：哪天这一腿也加了
    /// 缓存，这一行就是必须跟着改的地方，而继承默认实现不会留下任何提醒。
    fn fresh_status(&self) -> InterfaceStatus {
        self.get_status()
    }

    fn apply_network(&self, p: &NetworkConfig) -> Result<(), String> {
        let dev = match p.target.unwrap_or_default() {
            NetworkTarget::Primary => primary_iface().ok_or_else(|| i18n::t("pal.no_primary_iface"))?,
            NetworkTarget::Wifi => wifi_iface().ok_or_else(|| i18n::t("pal.no_wifi_device_hint"))?,
            NetworkTarget::Ethernet => ethernet_iface().ok_or_else(|| i18n::t("pal.no_ethernet_iface"))?,
        };
        let conn = active_connection(&dev)
            .ok_or_else(|| i18n::tf("pal.no_active_conn_on", &[("dev", &dev)]))?;

        // 原生 D-Bus 优先：`Settings.Connection.Update` + `ActivateConnection` 一次下发，
        // 不再 spawn `nmcli con mod`。任何一步失败（无 system bus、属性签名不符、
        // 连接路径匹配不上）都回落到下面的 `nmcli` 实现，功能不退化。
        let native = linux_nm::apply(
            &conn,
            &dev,
            match p.mode {
                Mode::Manual => "manual",
                Mode::Dhcp => "auto",
            },
            p.ip.as_deref(),
            p.netmask.as_deref().and_then(mask_to_prefix),
            p.gateway.as_deref(),
            p.dns.as_deref(),
            p.v6mode.as_ref().map(|m| match m {
                V6Mode::Off => "disabled",
                V6Mode::Automatic => "auto",
                V6Mode::Manual => "manual",
            }),
            p.ipv6.as_deref(),
            p.v6prefix.as_deref().and_then(|s| s.parse::<u32>().ok()),
            p.v6gateway.as_deref(),
        );
        if native.is_ok() {
            return Ok(());
        }

        // 兜底：`nmcli con mod`（沿用既有逻辑）
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
        let dev = wifi_iface().ok_or_else(|| i18n::t("pal.no_wifi_iface"))?;
        self.set_dhcp_for(&dev)
    }

    /// 按**设备名**切回 DHCP：设备 → 活动连接（nmcli 的操作对象是连接而不是设备）。
    fn set_dhcp_for(&self, dev: &str) -> Result<(), String> {
        let conn = active_connection(dev)
            .ok_or_else(|| i18n::tf("pal.no_active_conn_on", &[("dev", dev)]))?;

        // 原生 D-Bus 优先（一并清空静态 DNS，与 `nmcli ipv4.dns ""` 一致）。
        if linux_nm::set_dhcp(&conn, dev).is_ok() {
            return Ok(());
        }

        // 兜底：`nmcli con mod`
        run_priv(
            "nmcli",
            &[
                "con", "mod", &conn, "ipv4.method", "auto", "ipv4.addresses", "", "ipv4.gateway",
                "", "ipv4.dns", "",
            ],
        )?;
        run_priv("nmcli", &["con", "up", &conn])
    }

    /// 枚举当前在用的全部网卡，外加「装了但没连」的 VPN 连接。
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

                let ssid = if kind == super::NicKind::Wireless && Some(&dev) == wifi.as_ref() {
                    self.get_current_ssid()
                } else {
                    None
                };

                // IP / 网关 / DNS / IPv6 / MAC：原生 D-Bus 优先，失败回落 `nmcli -g device show`。
                // `device_ip` 在 D-Bus 与 nmcli 都拿不到时返回 `None` —— 这种接口没有可展示的
                // 地址信息，且本机又没连 SSID 时不值得占面板一格，直接跳过。
                if let Some(dip) = device_ip(&dev) {
                    if dip.ipv4.is_none() && ssid.is_none() {
                        continue;
                    }

                    let gateway_mac = dip.gateway.as_deref().and_then(|gw| {
                        crate::platform::linux_netlink::gateway_mac(gw)
                            .or_else(|| run("ip", &["neigh", "show", gw]).ok().and_then(|o| extract_mac(&o)))
                    });
                    // 路由前缀只有 VPN 那一格会显示（面板的「网关或路由」），所以也只问隧道。
                    let routes = if kind == super::NicKind::Vpn {
                        dip.routes.clone()
                    } else {
                        Vec::new()
                    };

                    out.push(super::NicInfo {
                        name: dev,
                        label: if conn.is_empty() { None } else { Some(conn.clone()) },
                        kind,
                        up: true,
                        ssid,
                        mac: dip.mac.clone(),
                        ipv4: dip.ipv4.clone(),
                        netmask: dip.netmask.clone(),
                        ipv6: dip.ipv6.clone(),
                        gateway: dip.gateway.clone(),
                        gateway6: dip.gateway6.clone(),
                        routes,
                        gateway_mac,
                        dns: dip.dns.clone(),
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
            }

            // 「装了、没连」的那几个 VPN 客户端补在最后：它们没有设备，上面那条按设备枚举的
            // 路走不到它们。多一次 `nmcli con show`，整体在 TTL 缓存里，摊到每次面板开合上
            // 就是几秒一次。
            let idle = idle_vpn_conns(&nm_connections(), &out);
            out.extend(idle);

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
        let tcp = || -> bool {
            let t = match target.tcp_target.as_deref() {
                Some(s) if !s.is_empty() => s,
                _ => return false,
            };
            let secs = timeout_secs(timeout_ms);
            run("timeout", &[&secs, "nc", "-z", "-w", "1", t]).is_ok()
        };
        let dns = || -> bool {
            let t = match target.dns_target.as_deref() {
                Some(s) if !s.is_empty() => s,
                _ => return false,
            };
            let secs = timeout_secs(timeout_ms);
            run("timeout", &[&secs, "nslookup", t]).is_ok()
        };
        let dead = match target.mode {
            ProbeMode::Icmp => !icmp(),
            ProbeMode::Http => !http(),
            ProbeMode::Tcp => !tcp(),
            ProbeMode::Dns => !dns(),
            ProbeMode::Both => !(icmp() || http() || tcp() || dns()),
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
                i18n::tf("pal.nm_up_failed", &[("name", name), ("error", &e)])
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
                    .map_err(|e| {
                        i18n::tf("pal.sudo_ip_link_failed", &[("name", name), ("error", &e)])
                    });
            }
            return Err(i18n::tf("pal.ip_link_needs_root", &[("name", name)]));
        }
        let known = nm_connection_names();
        Err(if known.is_empty() {
            i18n::tf("pal.tunnel_missing_no_nm", &[("name", name)])
        } else {
            i18n::tf("pal.tunnel_missing_nm_list", &[
                ("name", name),
                ("conns", &known.join(", ")),
            ])
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
        match run_priv("ip", &refs) {
            Ok(()) => Ok(()),
            Err(e) if e.contains("File exists") || e.contains("RTNETLINK") => {
                // 路由已存在：不视为失败，也不由本事务回滚时删除。
                // 这避免了「Profile A 加了路由，Profile B 又加一次，
                // Profile B 回滚时把 Profile A 的路由删了」的故障。
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn delete_route(&self, dest: &str) -> Result<(), String> {
        run_priv("ip", &["route", "del", dest])
    }

    fn launch_app(&self, app: &str, args: &[String]) -> Result<(), String> {
        // 三条出路：可执行文件直接跑；存在但不能 exec 的（.desktop、文档）与 URL 交给
        // xdg-open；写成路径却根本不在机器上的，在这里报错 —— 见 classify_target。
        let meta = std::path::Path::new(app).metadata().ok();
        let has_exec_bit = meta
            .as_ref()
            .map(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && (m.permissions().mode() & 0o111) != 0
            })
            .unwrap_or(false);
        let target = classify_target(app, meta.is_some(), has_exec_bit);
        let (program, argv) = launch_plan(app, args, target)?;
        spawn_detached(&program, &argv)
    }

    fn list_installed_apps(&self) -> Vec<AppEntry> {
        // 只列 `.desktop` 登记过的程序（系统两处 + 用户一处）：那既是「安装器替用户登记过
        // 的入口」，也正好是 `Name`/`Exec` 都有据可查的形状。PATH 里的裸命令不列 ——
        // 那是几百个开发工具与系统组件，不是「这台机器装了什么程序」的答案。
        let lang = desktop_lang();
        let mut out = Vec::new();
        for d in desktop_dirs() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for ent in rd.flatten() {
                if ent.path().extension().and_then(|e| e.to_str()) != Some("desktop") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(ent.path()) else { continue };
                if let Some(e) = app_from_desktop(&text, &lang) {
                    out.push(e);
                }
            }
        }
        dedupe_sort_apps(out)
    }

    fn pick_app(&self) -> Result<Option<String>, String> {
        // zenity（GNOME 世界）→ kdialog（KDE）：两个桌面世界各用各的通用对话框。
        // 取消：zenity 退 1、kdialog 退 1，都**没有输出** —— `run_dialog` 把「退非 0」
        // 与「起不来」分开，取消因此是 `Ok(None)` 而不是一条错误。
        let title = i18n::t("pal.pick_app_title");
        if in_path("zenity") {
            let (ok, out) = run_dialog("zenity", &["--file-selection", &format!("--title={title}")])?;
            let p = out.trim();
            return Ok((ok && !p.is_empty()).then(|| p.to_string()));
        }
        if in_path("kdialog") {
            let (ok, out) = run_dialog("kdialog", &["--getopenfilename", ".", &title])?;
            let p = out.trim();
            return Ok((ok && !p.is_empty()).then(|| p.to_string()));
        }
        // 两个都没有：这台机器没有能用的文件对话框（非桌面会话），照实报错。
        // 手输路径那条路不受影响。
        Err(i18n::t("pal.no_file_dialog"))
    }

    fn run_script(&self, path: &str, args: &[String], elevated: bool) -> Result<(), String> {
        if elevated {
            // 用户脚本提权：显式走授权框（sudo -n 失败则 pkexec），与网络配置操作区别对待。
            // 只把真正的参数交给 `run_priv`；脚本路径本身由 `run_priv` 负责前缀，
            // 不能把 `path` 再塞进 args（否则脚本的 $1 会变成自己的路径）。
            let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
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

    /// 本机装着的网卡（含现在没连上的），供编辑器的接口条件下拉。
    ///
    /// 数据源是 NetworkManager 的设备表而不是 `/sys/class/net`：那里直接有 TYPE，
    /// 而条件下拉要给出的集合必须和 `NetworkSnapshot::sample`（同样读 `dev status`）
    /// 说得上一句话 —— 一个口在 sysfs 里存在、却因为没被 NM 托管而永远不会进快照，
    /// 把它列进下拉等于让用户存下一个永不命中的条件。
    /// 排除项与快照同源：回环、VPN 隧道（条件比的是 `interfaces`，隧道在 `tunnels`）。
    fn list_adapters(&self) -> Vec<super::NicInfo> {
        let Ok(status) = nmcli(&["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "dev", "status"])
        else {
            return Vec::new();
        };
        let mut out: Vec<super::NicInfo> = status
            .lines()
            .map(split_t)
            .filter(|f| f.len() >= 3)
            .filter_map(|f| {
                let dev = f[0].trim().to_string();
                let ty = f[1].trim().to_string();
                let state = f[2].trim().to_string();
                let conn = f.get(3).map(|s| s.trim().to_string()).unwrap_or_default();
                if dev.is_empty() || dev == "lo" {
                    return None;
                }
                let kind = match ty.as_str() {
                    "wifi" => super::NicKind::Wireless,
                    "ethernet" => super::NicKind::Wired,
                    "vpn" | "tun" | "tap" | "wireguard" => super::NicKind::Vpn,
                    _ if super::is_tunnel_device(&dev) => super::NicKind::Vpn,
                    _ => super::NicKind::Other,
                };
                if kind == super::NicKind::Vpn {
                    return None;
                }
                Some(super::NicInfo {
                    name: dev,
                    label: if conn.is_empty() { None } else { Some(conn) },
                    kind,
                    up: state == "connected",
                    ..Default::default()
                })
            })
            .collect();
        // 设备名字典序（eth0 / wlan0）：下拉要的是稳定顺序，分组交给界面
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// 桌面环境自己是怎么说「用哪种语言」的：`LANGUAGE` 优先（GTK 系的多级回退列表，
    /// 形如 `zh_CN:en`，取第一项），然后 `LC_ALL` / `LANG`（`zh_CN.UTF-8` 这一类 POSIX
    /// 标签）。都不成时才问一句 `locale`—— 那是给「从 systemd 服务里起来、环境被清过」
    /// 的情形留的兜底。
    fn ui_language(&self) -> Option<String> {
        for name in ["LANGUAGE", "LC_ALL", "LANG"] {
            let v = std::env::var(name).unwrap_or_default();
            let v = v.split(':').next().unwrap_or("").trim().to_string();
            if !v.is_empty() && v != "C" && v != "POSIX" {
                return Some(v);
            }
        }
        let out = run("locale", &["-es"]).unwrap_or_default();
        match out.trim() {
            "" | "C" | "POSIX" => None,
            other => Some(other.to_string()),
        }
    }

    /// 先问 `color-scheme`（GNOME 42 起正答「深还是浅」的那一条），它给出 `gtk` 或压根读不到
    /// 时才去问 `gtk-theme` —— 后面那一条是它的前身，只能从主题名里猜。
    ///
    /// 两条都读不到就是 `None`（KDE / 没装 dconf-tools / 非桌面会话），调用方退深色。
    /// 这里不猜：Linux 上「深色」这件事没有第二个权威来源，猜一套出来等于把用户的界面
    /// 钉在一个谁都没说过的档位上。
    fn ui_prefers_dark(&self) -> Option<bool> {
        let get = |key: &str| {
            run("gsettings", &["get", "org.gnome.desktop.interface", key]).ok()
        };
        let scheme = get("color-scheme");
        match prefers_dark_from_gsettings(scheme.as_deref(), None) {
            Some(dark) => Some(dark),
            None => prefers_dark_from_gsettings(None, get("gtk-theme").as_deref()),
        }
    }

    fn list_printers(&self) -> Vec<PrinterInfo> {
        // CUPS 的客户端命令，与 macOS 同一套（Linux 上 CUPS 就是打印子系统本身）。
        // 没装 CUPS 时 `lpstat` 起不来 → 三条都拿到空文本 → 空清单；界面上表现为
        // 「没有候选，请手输」。
        let long = run_env("lpstat", &["-l", "-p"], &C_LOCALE).unwrap_or_default();
        let names = run_env("lpstat", &["-e"], &C_LOCALE).unwrap_or_default();
        let default = run_env("lpstat", &["-d"], &C_LOCALE).unwrap_or_default();
        printers_from_lpstat(&long, &names, &default)
    }

    fn set_default_printer(&self, printer: &str) -> Result<(), String> {
        // 与 macOS 同理：`lpoptions -d` 写的是当前用户的 ~/.cups/lpoptions，不提权。
        // 系统级默认（`lpadmin -d`）要 root，而这个动作每次进入 Active 都会跑。
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
    fn an_executable_is_run_as_is_and_keeps_its_arguments() {
        assert_eq!(
            launch_plan("/usr/bin/foo", &v(&["--flag", "a b"]), Target::Run).unwrap(),
            ("/usr/bin/foo".to_string(), v(&["--flag", "a b"]))
        );
    }

    #[test]
    fn anything_else_goes_to_xdg_open_and_only_alone() {
        // 断言的是渲染后的文案，而字典要 init() 之后才有内容：不叫这一句，t() 回退成 key，
        // 这条用例就变成了「赌别的用例先跑过 init」。
        i18n::init();
        assert_eq!(
            launch_plan("readme.txt", &[], Target::OpenWithXdg).unwrap(),
            ("xdg-open".to_string(), v(&["readme.txt"]))
        );
        // xdg-open 不会替我们把余下参数转交给应用：静悄悄丢掉比报错更难查，所以直接报错。
        // 只挑五种语言里都出现的字面量断言，语言被别的用例切走也不影响。
        assert!(launch_plan("readme.txt", &v(&["--flag"]), Target::OpenWithXdg)
            .unwrap_err()
            .contains("xdg-open"));
    }

    /// 徽标不能替用户说谎：路径写错、东西不在机器上，必须是一次失败。
    /// 这一步发生在任何 spawn 之前，所以它不挑机器 —— 没有图形会话也照样成立。
    #[test]
    fn a_path_that_is_not_there_is_an_error_not_a_quiet_success() {
        i18n::init();
        assert_eq!(
            classify_target("/opt/foo/firefox", false, false),
            Target::NotFound
        );
        let err = launch_plan("/opt/foo/firefox", &[], Target::NotFound).unwrap_err();
        assert!(err.contains("/opt/foo/firefox"), "错误里要点名是哪个目标：{err}");
        // 带不带 args 都该是同一个错：先判存在性，再谈怎么打开
        assert!(launch_plan("/opt/foo/firefox", &v(&["--kiosk"]), Target::NotFound).is_err());
    }

    /// 判定只看「在不在 + 有没有执行位 + 字符串形状」，这四条挡住四种不同配置：
    /// 目录里的文档、URL、PATH 里的应用名、以及唯一那个真正该报错的形状。
    #[test]
    fn target_classification_covers_the_four_shapes_a_user_can_type() {
        // 有执行位：直接跑，args 原样传，不看别的
        assert_eq!(classify_target("/usr/bin/foo", true, true), Target::Run);
        // 存在但没执行位（.desktop / 文档 / 目录）：只能给 xdg-open
        assert_eq!(classify_target("/usr/share/applications/foo.desktop", true, false), Target::OpenWithXdg);
        // URL 里有 '/'，但它从来就不是本机路径 —— scheme 必须先于路径判定
        assert_eq!(classify_target("https://example.com/x", false, false), Target::OpenWithXdg);
        // 裸应用名：交给 PATH 解析，真找不到时 spawn 自己会报 No such file or directory
        assert_eq!(classify_target("firefox", false, false), Target::Run);
        // 写成路径却不在机器上
        assert_eq!(classify_target("/usr/bin/firefox", false, false), Target::NotFound);
    }

    /// `Exec=` 到「程序本体」的几种真实写法：字段码消失、引号里的空格留住、
    /// `env` 赋值前缀剥掉、只有字段码的行没有可指的程序。
    #[test]
    fn exec_line_yields_the_program_itself() {
        assert_eq!(
            exec_first_token("/usr/bin/firefox %u").as_deref(),
            Some("/usr/bin/firefox")
        );
        assert_eq!(
            exec_first_token("env BAMF_DESKTOP_FILE_HINT=x /usr/bin/gedit --new-window %F").as_deref(),
            Some("/usr/bin/gedit")
        );
        assert_eq!(
            exec_first_token("\"/opt/My App/run.sh\" --flag %U").as_deref(),
            Some("/opt/My App/run.sh")
        );
        // `%%` 是字面百分号，不是字段码
        assert_eq!(
            exec_first_token("sh -c \"echo 100%% ok\"").as_deref(),
            Some("sh")
        );
        assert_eq!(exec_first_token("%U"), None);
        assert_eq!(exec_first_token("  "), None);
    }

    /// `.desktop` 的取舍：只认 `[Desktop Entry]` 段、`NoDisplay`/`Hidden` 不列、
    /// `Name[zh_CN]` → `Name[zh]` → `Name` 的挑选顺序、缺 `Exec` 就没有程序可指。
    #[test]
    fn desktop_entries_yield_apps_with_localized_names() {
        let firefox = "[Desktop Entry]\nType=Application\nName=Firefox\nName[zh_CN]=火狐浏览器\nName[ja]=Firefox JP\nExec=/usr/lib/firefox/firefox %u\nIcon=firefox\n\n[Desktop Action new-window]\nName=New Window\nExec=/usr/lib/firefox/firefox --new-window\n";
        let e = app_from_desktop(firefox, "zh_CN.UTF-8").unwrap();
        assert_eq!(
            (e.name.as_str(), e.path.as_str()),
            ("火狐浏览器", "/usr/lib/firefox/firefox")
        );
        // 完整标签没命中：zh_TW 落回无标签 Name（`Name[zh]` 不存在），ja 命中 `Name[ja]`
        assert_eq!(app_from_desktop(firefox, "zh_TW").unwrap().name, "Firefox");
        assert_eq!(app_from_desktop(firefox, "ja").unwrap().name, "Firefox JP");
        assert_eq!(app_from_desktop(firefox, "").unwrap().name, "Firefox");
        // 右键菜单项（[Desktop Action …]）的 Name/Exec 不是程序本身
        assert_eq!(app_from_desktop(firefox, "zh_CN").unwrap().path, "/usr/lib/firefox/firefox");
        // NoDisplay / Hidden 是文件自己说别露脸；Type=Link 是网址
        for flag in ["NoDisplay=true", "Hidden=True"] {
            let t = format!("[Desktop Entry]\nType=Application\nName=X\nExec=/x\n{flag}\n");
            assert!(app_from_desktop(&t, "").is_none(), "{flag}");
        }
        assert!(app_from_desktop("[Desktop Entry]\nType=Link\nName=Doc\nURL=https://example.com\n", "").is_none());
        // 缺 Exec：没有程序可指
        assert!(app_from_desktop("[Desktop Entry]\nType=Application\nName=X\n", "").is_none());
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

    /// 面板「网关或路由」那一格读的是这个清单：只留目的前缀，剥掉 nmcli 的 `dst = …, nh = …`
    /// 外壳，丢掉每条链路都有的多播/广播段，并且有上限（WireGuard 的 allowed-ips 可以是一片
    /// /32）。挡不住的是 nmcli 换格式 —— 那只会让这一格变空，不会写成别的值。
    #[test]
    fn nm_route_rows_reduce_to_destination_prefixes() {
        let rows: Vec<String> = [
            "dst = 0.0.0.0/0, nh = 10.0.0.1, mt = 100",
            "dst = 10.30.35.0/24, nh = 0.0.0.0, mt = 100",
            "dst = 10.30.35.0/24, nh = 0.0.0.0, mt = 100",
            "dst = 255.255.255.255/32, nh = 0.0.0.0, mt = 100",
            "dst = 224.0.0.0/4, nh = 0.0.0.0, mt = 100",
            "IP4.ROUTE[6]: dst = 192.168.9.0/24, nh = 0.0.0.0, mt = 100",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            route_prefixes(&rows).join(","),
            "0.0.0.0/0,10.30.35.0/24,192.168.9.0/24"
        );
        let many: Vec<String> = (0..30)
            .map(|i| format!("dst = 10.0.{i}.0/24, nh = 0.0.0.0, mt = 100"))
            .collect();
        assert_eq!(route_prefixes(&many).len(), MAX_ROUTES_PER_IFACE);
        assert!(route_prefixes(&[]).is_empty());
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

    fn conn(name: &str, ty: &str, active: bool) -> NmConn {
        NmConn {
            name: name.to_string(),
            ty: ty.to_string(),
            active,
            hay: format!("{ty}{}", if active { " wg0" } else { "" }),
        }
    }

    fn nic(name: &str, label: Option<&str>, app: Option<&str>) -> super::super::NicInfo {
        super::super::NicInfo {
            name: name.to_string(),
            label: label.map(str::to_string),
            kind: super::super::NicKind::Vpn,
            up: true,
            app: app.map(str::to_string),
            ..Default::default()
        }
    }

    /// 面板要说「这几个 VPN 现在没连」，靠的正是 `con show` 里那些**没有设备**的连接：
    /// 上面那条按 `dev status` 枚举设备的路永远走不到它们，也就是说不论有几条隧道在用，
    /// 没连的那几个客户端都得由这份清单补出来。
    #[test]
    fn an_idle_vpn_connection_still_gets_a_card() {
        let live = [nic("wg0", Some("Office WG"), Some("WireGuard"))];
        let conns = [
            conn("Office WG", "wireguard", true),
            conn("Forti", "vpn", false),
            conn("Café", "802-11-wireless", false),
        ];
        let idle = idle_vpn_conns(&conns, &live);
        assert_eq!(
            idle.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            v(&["Forti"]),
            "在用的那条不补第二张卡；没连的 Wi-Fi 连接不收"
        );
        assert!(!idle[0].up, "未连的连接要能被界面判成「没连」");
        assert!(idle[0].ipv4.is_none(), "没连的链路不该编出任何地址");
        // 连接名只写了厂商名前缀，产品名那一格才是信息
        assert_eq!(idle[0].app.as_deref(), Some("FortiClient"));
    }

    /// 在用的那条连接可能因为设备名写法不同而留在清单里（连接名在 `label` 上），
    /// 去重要比对名称、标签、归属三样；漏了就是一张在用的卡和一张「没连」的卡同时说同一个 VPN。
    #[test]
    fn an_idle_connection_is_dropped_when_a_live_row_already_says_it() {
        for live in [
            nic("NordVPN Home", None, None),
            nic("tun0", Some("NordVPN Home"), None),
            nic("tun0", None, Some("NordVPN Home")),
        ] {
            assert!(
                idle_vpn_conns(&[conn("NordVPN Home", "vpn", false)], &[live]).is_empty(),
                "在用清单里已经有这一条，却还是补了第二张卡"
            );
        }
        // 名字不相干的照旧补
        assert_eq!(
            idle_vpn_conns(
                &[conn("Home Forti", "vpn", false)],
                &[nic("wg0", Some("Office WG"), None)]
            )
            .len(),
            1
        );
    }

    /// 同一个连接名的重复条目（NM 里可能有同名配置）只给一张卡。
    #[test]
    fn duplicate_idle_names_are_collapsed() {
        let conns = [conn("Forti", "vpn", false), conn("Forti", "vpn", false)];
        assert_eq!(idle_vpn_conns(&conns, &[]).len(), 1);
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
