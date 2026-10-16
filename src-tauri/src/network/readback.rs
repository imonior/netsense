//! 3A 的回读校验：把「期望参数」与平台实际报告的参数逐项比对。
//!
//! 判据刻意保守 —— **只比对「两侧都拿得到」的字段**。取不到的一侧不算失败：
//! 各平台能读回什么差别很大（Linux 的 `nmcli` 不给掩码、macOS 未配自定义 DNS 时
//! `networksetup -getdnsservers` 返回的是一句提示文本而不是地址列表）。
//! 把「读不到」判成失败会让大量本来正确的下发被误报为 ERROR，
//! 而误报的代价（用户从此不敢信状态灯）比漏报更高。

use crate::config::model::RouteConfig;
use crate::config::{Mode, NetworkConfig, V6Mode};
use crate::i18n;
use crate::platform::{InterfaceStatus, NetworkPlatform};
use std::fmt;
use std::net::Ipv4Addr;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyFailure {
    /// 形如 `ip: 期望 192.168.1.100，实际 169.254.23.9`（措辞随界面语言，字段名与地址原样保留）
    pub mismatches: Vec<String>,
    /// 唯一的不符就是「静态地址上请求自动 DNS，而系统里一个 nameserver 都没有」。
    ///
    /// 单独标出来是因为 3A 要据此判断**这一局还能不能救**：只有这一类失败是补一组
    /// DNS 服务器就能翻盘的，其余（地址没下上去、路由没生效）补了也没用。
    /// 靠比对本地化的句子来判是不可靠的 —— 句子随界面语言变。
    pub dns_left_empty: bool,
}

impl fmt::Display for VerifyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mismatches.is_empty() {
            write!(f, "{}", i18n::t("net.no_diff"))
        } else {
            write!(f, "{}", self.mismatches.join(&i18n::t("net.diff_sep")))
        }
    }
}

/// 采样 + 判定。`pre_existing` 是下发前采集到的路由表（`apply_3a` 快照里的那一份），
/// 只用来分辨「路由表读不到」与「表里确实没有这条路由」—— 两者的 [`NetworkPlatform::get_routes`]
/// 都返回空 `Vec`，但前者不能算不符（与 DNS 的 `None` 同一条判据），后者要判失败。
pub fn readback_match<P: NetworkPlatform>(
    plat: &P,
    want: &NetworkConfig,
    pre_existing: &[RouteConfig],
) -> Result<(), VerifyFailure> {
    // `fresh_status` 而不是 `get_status`：校验循环靠的是「每次采样都真的去问一次系统」，
    // 拿到一份 TTL 内的旧快照等于少一次重试机会。
    let mut st = plat.fresh_status();
    // 路由表不在 `get_status` 里（多一次子进程，面板用不到）：只有这份 profile 真的配了
    // 路由时才去采，没配路由的配置不多付这次开销。
    if !want.routes.is_empty() {
        let now = plat.get_routes();
        if now.is_empty() && !pre_existing.is_empty() {
            // 采回来是空的，而下发前同一张表上是有路由的 —— 一台活着的机器不可能一条
            // IPv4 路由都没有，所以这一次是**读取失败**，不是「路由没进表」。判失败会把
            // 本来正确的下发回滚掉，因此摘掉路由期望，让本轮只校验其余字段。
            let mut relaxed = want.clone();
            relaxed.routes.clear();
            return check(&st, &relaxed);
        }
        st.routes = now;
    }
    check(&st, want)
}

/// 纯函数版本：把采样与判定分开，便于用假数据覆盖各种不符组合。
pub fn check(got: &InterfaceStatus, want: &NetworkConfig) -> Result<(), VerifyFailure> {
    let mut mismatches: Vec<String> = Vec::new();
    let mut dns_left_empty = false;

    match want.mode {
        Mode::Dhcp => {
            // DHCP 环境下唯一能确证的是「拿到了地址」；具体地址由服务器决定，无从比对。
            if got.ipv4.as_deref().unwrap_or("").trim().is_empty() {
                mismatches.push(i18n::t("net.dhcp_no_ipv4"));
            }
        }
        Mode::Manual => {
            if let Some(want_ip) = want.ip.as_deref() {
                match got.ipv4.as_deref() {
                    Some(g) if g.trim() == want_ip.trim() => {}
                    Some(g) => mismatches.push(mismatch("ip", want_ip, Some(g.trim()))),
                    None => mismatches.push(mismatch("ip", want_ip, None)),
                }
            }
            check_optional("netmask", want.netmask.as_deref(), got.netmask.as_deref(), &mut mismatches);
            check_optional("gateway", want.gateway.as_deref(), got.gateway.as_deref(), &mut mismatches);
        }
    }

    // DNS 有三条分支，判据各不相同：
    //   1. 期望具体服务器 —— 逐个查是否真的出现（缺一个即不符）。
    //   2. 期望空 + `mode: dhcp` —— **不比**。DHCP 客户端会把路由器递来的地址填进来，
    //      拿它跟「我要自动」比必然失败，而那恰恰是自动该有的结果。
    //   3. 期望空 + `mode: manual` —— 静态地址这一支上没有任何一方会递来 nameserver，
    //      「交回自动」真实落到系统里的结果是一个都没有。读到空就是货真价实的失败：
    //      用户拿到的是「IP 通、网关通、网页打不开」，而这正是最该在 3A 拦下的现场。
    //      豁免只给 dhcp 那一半，不能因为「清空的命令返回了 0」就宣称自动 DNS 成功。
    //   另外 `got.dns` 为 None 一律不比 —— 读不到（平台不支持 / 权限不足）不等于不符；
    //   注意与 `Some("")` 区分：读到「明确没有」才是可判定的事实。
    if let Some(dns) = want.dns.as_deref() {
        let want_list = split_list(dns);
        if !want_list.is_empty() {
            if let Some(got_dns) = got.dns.as_deref() {
                let got_list = split_list(got_dns);
                let missing: Vec<&String> =
                    want_list.iter().filter(|w| !got_list.contains(w)).collect();
                if !missing.is_empty() {
                    // 列表按 `, ` 拼开：Rust 的 `{:?}` 会把调试格式带进界面文案。
                    mismatches.push(mismatch(
                        "dns",
                        &want_list.join(", "),
                        Some(&got_list.join(", ")),
                    ));
                }
            }
        } else if want.mode == Mode::Manual {
            if let Some(got_dns) = got.dns.as_deref() {
                if split_list(got_dns).is_empty() {
                    mismatches.push(i18n::t("net.dns_left_empty"));
                    dns_left_empty = true;
                }
            }
        }
    }

    // 静态路由：逐条核对「加的在了、删的没了」。
    //
    // 这一步曾经缺席，于是 `apply_3a` 对路由只认命令退出码 —— 而 `netsh` / `route` /
    // `ip route` 在「路由已存在」时都返回 0 且不做任何事，「期望的没进去」与「进去了」
    // 在退出码上完全同形。判据是 dest（归一化成标准 CIDR）+ gateway；**不比 metric** ——
    // macOS 的 `netstat` 根本不给度量值（恒 0），拿它判等于全平台误报。
    for r in &want.routes {
        let hit = got.routes.iter().any(|g| route_matches(r, g));
        if r.delete {
            if hit {
                mismatches.push(i18n::tf("net.route_left", &[("dest", &r.dest)]));
            }
        } else if !hit {
            mismatches.push(i18n::tf(
                "net.route_missing",
                &[
                    ("dest", &r.dest),
                    ("gw", r.gateway.as_deref().unwrap_or("-")),
                ],
            ));
        }
    }

    // IPv6 模式：期望与实读都是 off / automatic / manual 时才比。读不到一律不判 ——
    // Windows 的「关 IPv6」是禁用 ms_tcpip6 绑定，之后 `Get-NetIPInterface` 连 IPv6 行
    // 都没有，`v6mode` 于是是 None：那不是「模式不对」，是「这一支上读不回来」。
    if let (Some(want_v6), Some(got_v6)) = (want.v6mode.as_ref(), got.v6mode.as_deref()) {
        let want_s = match want_v6 {
            V6Mode::Off => "off",
            V6Mode::Automatic => "automatic",
            V6Mode::Manual => "manual",
        };
        if got_v6.trim() != want_s {
            mismatches.push(mismatch("v6mode", want_s, Some(got_v6.trim())));
        }
    }

    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(VerifyFailure {
            // 只在「除了这一条没有任何其它不符」时置位：地址/网关还没下上去的情况
            // 补一组 DNS 也救不回来，置位只会让 3A 误以为兜底能翻盘。
            dns_left_empty: dns_left_empty && mismatches.len() == 1,
            mismatches,
        })
    }
}

/// 一条「期望与实际不符」的描述。`field` 是配置里的字段名，五种语言里都原样保留，
/// 界面与日志因此始终能用 `ip:` / `dns:` 认出行首。
fn mismatch(field: &str, want: &str, got: Option<&str>) -> String {
    match got {
        Some(g) => i18n::tf(
            "net.mismatch",
            &[("field", field), ("want", want), ("got", g)],
        ),
        None => i18n::tf("net.mismatch_missing", &[("field", field), ("want", want)]),
    }
}

fn check_optional(
    field: &str,
    want: Option<&str>,
    got: Option<&str>,
    out: &mut Vec<String>,
) {
    let (Some(w), Some(g)) = (want, got) else { return };
    if w.trim().is_empty() || g.trim().is_empty() {
        return;
    }
    if w.trim() != g.trim() {
        out.push(mismatch(field, w.trim(), Some(g.trim())));
    }
}

fn split_list(s: &str) -> Vec<String> {
    s.split([',', ' '])
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// 一条配置路由与一条实采路由是否指向同一件事。
///
/// dest 的书写形状三平台各不一样：配置与 Windows `Get-NetRoute` / Linux `ip route` 都是
/// `10.0.0.0/8`，而 **macOS 的 `netstat -rn` 是压缩写法**（`10/8`，`192.168.1` 表示 /24）
/// —— 直接比字符串会让 macOS 上每一条路由都不相符，所以先归一化再比。
/// 配置这一支没带网关（`delete`）时 dest 对上了就算；带了就必须是同一个地址。
fn route_matches(want: &RouteConfig, got: &RouteConfig) -> bool {
    if !dest_eq(&want.dest, &got.dest) {
        return false;
    }
    match want.gateway.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(w) => got.gateway.as_deref().map(str::trim).filter(|s| !s.is_empty()) == Some(w),
        None => true,
    }
}

/// 两种 dest 写法是否指向同一个网络。任一侧归一化失败时退回原样比字符串。
fn dest_eq(a: &str, b: &str) -> bool {
    match (normalize_dest(a), normalize_dest(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a.trim() == b.trim(),
    }
}

/// 收敛成 `a.b.c.d/n` 的标准形状：`default` → `0.0.0.0/0`，主机位掩掉；
/// 无斜杠的按八位组数推前缀（macOS netstat 的 `10` → /8、`192.168.1` → /24），
/// 段数不足的地址补零（`10` → `10.0.0.0`）。解析不成 IPv4 返回 `None`，
/// 由调用方退回字符串比对。
fn normalize_dest(dest: &str) -> Option<String> {
    let s = dest.trim();
    if s.eq_ignore_ascii_case("default") {
        return Some("0.0.0.0/0".to_string());
    }
    let (addr, prefix) = match s.split_once('/') {
        Some((a, p)) => (a, p.trim().parse::<u32>().ok()?),
        None => {
            let octets = s.split('.').count();
            (s, match octets {
                1 => 8,
                2 => 16,
                3 => 24,
                4 => 32,
                _ => return None,
            })
        }
    };
    if prefix > 32 {
        return None;
    }
    let ip = pad_ipv4(addr)?;
    // prefix == 0 时 `!0u32 << 32` 是溢出移位，单独兜底。
    let mask: u32 = if prefix == 0 { 0 } else { !0u32 << (32 - prefix) };
    Some(format!("{}/{}", Ipv4Addr::from(u32::from(ip) & mask), prefix))
}

/// 段数不足四段的点分地址补零后解析：`10` → `10.0.0.0`，`192.168.1` → `192.168.1.0`
/// （macOS `netstat -rn` 的目的地就是这种压缩形状）。超过四段或某段不是数字 → `None`。
fn pad_ipv4(addr: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = addr.split('.').collect();
    if parts.len() > 4 {
        return None;
    }
    let mut nums = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        nums[i] = p.trim().parse().ok()?;
    }
    Some(Ipv4Addr::from(nums))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::V6Mode;

    fn manual() -> NetworkConfig {
        NetworkConfig {
            mode: Mode::Manual,
            ip: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1,8.8.8.8".into()),
            v6mode: Some(V6Mode::Off),
            ..Default::default()
        }
    }

    #[test]
    fn exact_match_passes() {
        let got = InterfaceStatus {
            connected: true,
            ipv4: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1 8.8.8.8".into()),
            ..Default::default()
        };
        check(&got, &manual()).expect("参数一致必须通过");
    }

    #[test]
    fn stale_address_is_a_failure() {
        // 典型场景：命令返回 0 但授权其实被拒了，地址还停在旧的
        let got = InterfaceStatus {
            ipv4: Some("169.254.23.9".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1 8.8.8.8".into()),
            ..Default::default()
        };
        let f = check(&got, &manual()).expect_err("IP 不符必须判失败");
        assert!(f.to_string().contains("ip:"), "报错要指出哪个字段: {}", f);
    }

    #[test]
    fn dns_order_does_not_matter() {
        let got = InterfaceStatus {
            ipv4: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("8.8.8.8,192.168.1.1".into()),
            ..Default::default()
        };
        check(&got, &manual()).expect("DNS 只看是否包含，顺序无关");
    }

    #[test]
    fn missing_dns_server_is_a_failure() {
        let got = InterfaceStatus {
            ipv4: Some("192.168.1.100".into()),
            dns: Some("8.8.8.8".into()),
            ..Default::default()
        };
        assert!(check(&got, &manual()).is_err(), "缺一个 DNS 仍是不符");
    }

    #[test]
    fn unreadable_fields_are_not_treated_as_mismatch() {
        // 平台只报得上 IP，掩码/网关/DNS 全空 —— 不能因此判失败
        let got = InterfaceStatus {
            ipv4: Some("192.168.1.100".into()),
            ..Default::default()
        };
        check(&got, &manual()).expect("读不到的字段不参与判定");
    }

    #[test]
    fn empty_dns_report_when_dns_expected_is_a_failure() {
        // 与上一个用例只差一处：这里 `dns` 是 Some("")，即「读到了，而且确实没有」——
        // 说明自定义 DNS 压根没下上去，必须判失败。None 与 Some("") 不能混为一谈。
        let got = InterfaceStatus {
            ipv4: Some("192.168.1.100".into()),
            dns: Some("".into()),
            ..Default::default()
        };
        let f = check(&got, &manual()).expect_err("读到空 DNS 列表而期望有值，必须判失败");
        assert!(f.to_string().contains("dns:"), "报错要指名 DNS: {}", f);
    }

    #[test]
    fn dhcp_mode_requires_a_leased_address() {
        let want = NetworkConfig {
            mode: Mode::Dhcp,
            ..Default::default()
        };
        assert!(check(&InterfaceStatus::default(), &want).is_err());
        let got = InterfaceStatus {
            ipv4: Some("10.0.0.24".into()),
            ..Default::default()
        };
        check(&got, &want).expect("拿到地址即认为 DHCP 成功");
    }

    /// 豁免只属于 dhcp 那一半：那里确实有 DHCP 客户端往里填地址，读到什么都不算不符。
    #[test]
    fn clearing_dns_is_not_verified_against_dhcp_supplied_servers() {
        let want = NetworkConfig {
            mode: Mode::Dhcp,
            dns: Some("".into()),
            ..Default::default()
        };
        let got = InterfaceStatus {
            ipv4: Some("10.0.0.24".into()),
            dns: Some("192.168.1.1".into()),
            ..Default::default()
        };
        check(&got, &want).expect("期望为空时不该拿路由器下发的 DNS 来判失败");
    }

    /// 静态地址这一支没有来源，「交回自动」落到系统里就是**一个 nameserver 都没有**。
    /// 读到空必须判失败 —— 否则 3A 会放过一个「IP 通、网关通、域名全解析不了」的现场。
    #[test]
    fn automatic_dns_on_a_static_address_must_actually_produce_a_server() {
        let want = NetworkConfig {
            mode: Mode::Manual,
            ip: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("".into()),
            ..Default::default()
        };
        let got = InterfaceStatus {
            ipv4: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("".into()),
            ..Default::default()
        };
        let f = check(&got, &want).expect_err("静态地址 + 自动 DNS 却读到空，必须判失败");
        assert!(
            f.to_string().to_lowercase().contains("dns"),
            "报错要指名 DNS: {}",
            f
        );
        // 反面：系统确实给出了（比如 Windows 静态地址仍收到 DHCP 的 DNS），那就算成功。
        let ok = InterfaceStatus {
            dns: Some("192.168.1.1".into()),
            ..got.clone()
        };
        check(&ok, &want).expect("自动真的取到了服务器就该通过");
        // 读不到（None）仍然不判失败 —— 读不到不等于不符，这条判据没有变。
        let unreadable = InterfaceStatus { dns: None, ..got };
        check(&unreadable, &want).expect("读不到 DNS 时不参与判定");
    }

    /// 一条静态路由的完整期望（地址/DNS 都对齐，只留路由这一项在检验）。
    fn with_route(routes: Vec<RouteConfig>) -> (NetworkConfig, InterfaceStatus) {
        let want = NetworkConfig {
            mode: Mode::Manual,
            ip: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1".into()),
            routes,
            ..Default::default()
        };
        let got = InterfaceStatus {
            connected: true,
            ipv4: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1".into()),
            ..Default::default()
        };
        (want, got)
    }

    fn route(dest: &str, gw: &str, delete: bool) -> RouteConfig {
        RouteConfig {
            dest: dest.into(),
            gateway: Some(gw.into()),
            metric: 0,
            delete,
        }
    }

    #[test]
    fn a_route_that_landed_passes() {
        let (want, mut got) = with_route(vec![route("10.0.0.0/8", "192.168.1.1", false)]);
        got.routes = vec![route("10.0.0.0/8", "192.168.1.1", false)];
        check(&got, &want).expect("路由在表里就必须通过");
    }

    /// 命令返回 0 但路由没进表（三平台的 route 工具对「已存在」都返回 0）——
    /// 这正是回读要拦的那类静默失败，不能只认退出码。
    #[test]
    fn a_route_that_never_landed_is_a_failure() {
        let (want, got) = with_route(vec![route("10.0.0.0/8", "192.168.1.1", false)]);
        let f = check(&got, &want).expect_err("路由不在表里必须判失败");
        assert!(
            f.to_string().contains("10.0.0.0/8"),
            "报错要指名是哪条路由（dest 各语言逐字保留）: {}",
            f
        );
    }

    #[test]
    fn a_deleted_route_must_actually_be_gone() {
        let (want, mut got) = with_route(vec![route("10.0.0.0/8", "192.168.1.1", true)]);
        got.routes = vec![route("10.0.0.0/8", "192.168.1.1", false)];
        check(&got, &want).expect_err("该删的路由还在表里必须判失败");
        // 删干净了就通过。
        got.routes = vec![route("192.168.0.0/16", "192.168.1.1", false)];
        check(&got, &want).expect("目标路由不在表里即通过");
    }

    /// macOS `netstat -rn` 写的是压缩形状（`10/8`、`192.168.1`），与配置里的
    /// `10.0.0.0/8` / `192.168.1.0/24` 是同一条路由 —— 判据必须是网络而非字符串。
    #[test]
    fn route_dest_is_compared_as_a_network_not_a_string() {
        let (want, mut got) = with_route(vec![route("10.0.0.0/8", "192.168.1.1", false)]);
        got.routes = vec![route("10/8", "192.168.1.1", false)];
        check(&got, &want).expect("macOS 的压缩写法 10/8 就是 10.0.0.0/8");
        got.routes = vec![route("10.1.2.3/8", "192.168.1.1", false)];
        check(&got, &want).expect("主机位不同但同一网络（掩回 10.0.0.0/8）算相符");
        got.routes = vec![route("11.0.0.0/8", "192.168.1.1", false)];
        check(&got, &want).expect_err("不同网络必须判失败");
    }

    #[test]
    fn route_gateway_must_match_the_one_asked_for() {
        let (want, mut got) = with_route(vec![route("10.0.0.0/8", "192.168.1.1", false)]);
        got.routes = vec![route("10.0.0.0/8", "10.1.1.1", false)];
        check(&got, &want).expect_err("dest 对上网关不对，仍不是我们要的那条");
    }

    #[test]
    fn v6_mode_mismatch_is_a_failure() {
        let mut want = manual();
        want.v6mode = Some(V6Mode::Off);
        let got = InterfaceStatus {
            ipv4: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1,8.8.8.8".into()),
            v6mode: Some("automatic".into()),
            ..Default::default()
        };
        let f = check(&got, &want).expect_err("IPv6 模式不符必须判失败");
        assert!(f.to_string().contains("v6mode"), "报错要指名 v6mode: {}", f);
    }

    /// Windows「关 IPv6」是禁用 ms_tcpip6 绑定，之后 `Get-NetIPInterface` 没有 IPv6 行，
    /// 读回来是 None —— 那是读不到，不是模式不对。
    #[test]
    fn v6_mode_unreadable_is_not_a_failure() {
        let mut want = manual();
        want.v6mode = Some(V6Mode::Off);
        let got = InterfaceStatus {
            ipv4: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1,8.8.8.8".into()),
            v6mode: None,
            ..Default::default()
        };
        check(&got, &want).expect("IPv6 模式读不到时不参与判定");
        let got2 = InterfaceStatus {
            v6mode: Some("off".into()),
            ..got
        };
        check(&got2, &want).expect("读到的模式与期望一致时通过");
    }
}
