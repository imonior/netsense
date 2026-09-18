//! 匹配引擎：SSID / 网关 MAC / BSSID 三者独立条件，AND 关系，最具体者胜。
//!
//! 三个条件都是**可选且独立**的：声明即参与判定，未声明即通配；
//! 已声明的条件必须全部成立（AND）才算命中。
//! 非 `__DEFAULT__` 的 Profile 至少要声明一个条件，否则不参与匹配。
//!
//! 多命中时的定序：先比「命中的条件数」（越具体越优先），再比 `priority`。

use crate::config::{Config, MatchConditions, Profile};
use crate::platform::{normalize_mac, NetworkPlatform};

/// 当前网络身份（由 PAL 解析）。MAC 字段一律已归一化为小写冒号形态。
pub struct NetworkIdentity {
    pub ssid: Option<String>,
    pub gateway_mac: Option<String>,
    pub bssid: Option<String>,
}

/// MAC 比较：两侧都归一化后再比，避免 `AA-BB-..` / `aa:bb:..` 这类
/// 平台差异（Windows `arp -a` 用连字符，macOS/Linux 用冒号）造成漏匹配。
fn mac_eq(a: Option<&str>, b: &str) -> bool {
    match a {
        Some(x) => normalize_mac(x) == normalize_mac(b),
        None => false,
    }
}

fn cond_matches(cond: &MatchConditions, id: &NetworkIdentity) -> bool {
    if let Some(ssid) = &cond.ssid {
        // SSID 区分大小写（802.11 的 SSID 本身大小写敏感）
        if id.ssid.as_deref() != Some(ssid.as_str()) {
            return false;
        }
    }
    if let Some(mac) = &cond.gateway_mac {
        if !mac_eq(id.gateway_mac.as_deref(), mac) {
            return false;
        }
    }
    if let Some(bssid) = &cond.bssid {
        if !mac_eq(id.bssid.as_deref(), bssid) {
            return false;
        }
    }
    true
}

fn specificity(cond: &MatchConditions) -> u32 {
    let mut n = 0;
    if cond.ssid.is_some() {
        n += 1;
    }
    if cond.gateway_mac.is_some() {
        n += 1;
    }
    if cond.bssid.is_some() {
        n += 1;
    }
    n
}

/// 选出应应用的 Profile（不含 `__DEFAULT__`）；全不命中返回 `None`。
///
/// 返回的 `&Profile` 借自 `cfg`，因此必须显式标注生命周期：
/// 入参有两个引用（`cfg` / `id`），省略规则无法判断该借谁。
pub fn select_profile<'a>(
    cfg: &'a Config,
    id: &NetworkIdentity,
) -> Option<(String, &'a Profile)> {
    let mut best: Option<(String, &'a Profile, u32, i32)> = None;
    for (name, p) in cfg.profiles.iter() {
        if name == "__DEFAULT__" {
            continue;
        }
        let cond = match &p.match_cond {
            Some(c) => c,
            // 没有 match 块 → 不参与匹配
            None => continue,
        };
        let spec = specificity(cond);
        // 非 `__DEFAULT__` 必须至少声明一个条件。全空的 match 块因所有条件都被
        // 当作通配而恒真，会以 specificity=0 混进「最具体者胜」的定序里，故直接跳过。
        if spec == 0 {
            continue;
        }
        if !cond_matches(cond, id) {
            continue;
        }
        let prio = p.priority;
        match &best {
            Some((_, _, bspec, bprio)) if *bspec > spec || (*bspec == spec && *bprio >= prio) => {}
            _ => best = Some((name.clone(), p, spec, prio)),
        }
    }
    best.map(|(name, p, _, _)| (name, p))
}

/// 解析当前网络身份（依赖 PAL）。MAC 在此统一归一化，下游只比字符串。
pub fn resolve_identity<P: NetworkPlatform>(plat: &P) -> NetworkIdentity {
    let norm = |v: Option<String>| v.map(|s| normalize_mac(&s));
    NetworkIdentity {
        ssid: plat.get_current_ssid(),
        gateway_mac: norm(plat.resolve_gateway_mac()),
        bssid: norm(plat.resolve_bssid()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;

    fn id(ssid: Option<&str>, gwm: Option<&str>, bssid: Option<&str>) -> NetworkIdentity {
        NetworkIdentity {
            ssid: ssid.map(|s| s.to_string()),
            gateway_mac: gwm.map(|s| s.to_string()),
            bssid: bssid.map(|s| s.to_string()),
        }
    }

    fn cfg_with(entries: &[(&str, Option<&str>, Option<&str>, Option<&str>, i32)]) -> Config {
        let mut cfg = Config::default();
        for (name, ssid, gwm, bssid, prio) in entries {
            let p = Profile {
                match_cond: Some(MatchConditions {
                    ssid: ssid.map(|s| s.to_string()),
                    gateway_mac: gwm.map(|s| s.to_string()),
                    bssid: bssid.map(|s| s.to_string()),
                }),
                priority: *prio,
                ..Default::default()
            };
            cfg.profiles.insert((*name).to_string(), p);
        }
        cfg
    }

    #[test]
    fn mac_matches_across_separator_and_case() {
        // 配置里写连字符大写，系统返回冒号小写 —— 必须命中
        let cfg = cfg_with(&[("Home", None, Some("AA-BB-CC-DD-EE-FF"), None, 0)]);
        let got = select_profile(&cfg, &id(None, Some("aa:bb:cc:dd:ee:ff"), None));
        assert_eq!(got.map(|(n, _)| n).as_deref(), Some("Home"));
    }

    #[test]
    fn most_specific_wins_over_priority() {
        // A: 只按 SSID；B: SSID+网关MAC。两者都命中时 B 更具体，应胜出（即使 A 优先级更高）
        let cfg = cfg_with(&[
            ("A", Some("Office"), None, None, 99),
            ("B", Some("Office"), Some("aa:bb:cc:dd:ee:ff"), None, 0),
        ]);
        let got = select_profile(&cfg, &id(Some("Office"), Some("aa:bb:cc:dd:ee:ff"), None));
        assert_eq!(got.map(|(n, _)| n).as_deref(), Some("B"));
    }

    #[test]
    fn priority_breaks_tie_at_same_specificity() {
        let cfg = cfg_with(&[("A", Some("Office"), None, None, 1), ("B", Some("Office"), None, None, 5)]);
        let got = select_profile(&cfg, &id(Some("Office"), None, None));
        assert_eq!(got.map(|(n, _)| n).as_deref(), Some("B"));
    }

    #[test]
    fn declared_conditions_are_anded() {
        // SSID 命中但 BSSID 不符 → 不应命中
        let cfg = cfg_with(&[(
            "Strict",
            Some("Office"),
            None,
            Some("00:11:22:33:44:55"),
            0,
        )]);
        let got = select_profile(&cfg, &id(Some("Office"), None, Some("66:77:88:99:aa:bb")));
        assert!(got.is_none());
    }

    #[test]
    fn router_only_profile_matches_without_ssid() {
        // 只靠网关 MAC 也能识别（SSID 隐藏/改名场景）
        let cfg = cfg_with(&[("RouterOnly", None, Some("aa:bb:cc:dd:ee:ff"), None, 0)]);
        let got = select_profile(&cfg, &id(Some("AnythingElse"), Some("aa:bb:cc:dd:ee:ff"), None));
        assert_eq!(got.map(|(n, _)| n).as_deref(), Some("RouterOnly"));
    }

    #[test]
    fn no_match_returns_none_then_caller_uses_default() {
        let cfg = cfg_with(&[("A", Some("Office"), None, None, 0)]);
        assert!(select_profile(&cfg, &id(Some("Cafe"), None, None)).is_none());
    }

    #[test]
    fn profile_without_match_conditions_is_skipped() {
        let cfg = cfg_with(&[("NoCond", None, None, None, 100)]);
        assert!(select_profile(&cfg, &id(Some("Office"), None, None)).is_none());
    }
}
