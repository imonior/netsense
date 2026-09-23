//! 条件求值：Rule 之间 OR，Rule 内**已启用**条件之间 AND。
//!
//! 三条容易写错、且错了就静默误判的规则（都在这里集中实现）：
//!
//! 1. **禁用 ≠ 通配**。把禁用的条件当成「不参与判定」，就等于没有「先配好、暂时
//!    停用」这回事。`enabled=false` 的条件永远不算命中，也**不**参与 AND。
//! 2. **没有有效条件的 Rule / Profile 是「不匹配」，不是「匹配一切」**。若把它当成
//!    恒真，一个空 Profile 会在任何网络下都命中，进而让每个网络都变成 Conflict。
//! 3. **MAC 比较两侧都要归一化**（`AA-BB-..` vs `aa:bb:..`）。平台差异见
//!    DEVELOPMENT.md 的 cross-platform matcher traps。

use serde::Serialize;

use super::identity::NetworkSnapshot;
use crate::config::{Condition, Profile, Rule};

/// 单条条件的实时状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionStatus {
    Match,
    NoMatch,
    /// 条件被禁用：既不 Match，也不能当成 Match
    Disabled,
}

/// 单条 Rule 的整体状态（UI 用它给 Rule 外框上色）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleStatus {
    Match,
    NoMatch,
    /// Rule 本身被禁用，或它的所有条件都被禁用
    Inactive,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConditionReport {
    pub id: String,
    pub kind: String,
    pub value: String,
    pub status: ConditionStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleReport {
    pub id: String,
    pub status: RuleStatus,
    pub conditions: Vec<ConditionReport>,
}

/// 一个 Profile 的条件求值结果（**不含** Active/Conflict —— 那是 engine 的事）。
#[derive(Debug, Clone, Serialize)]
pub struct ProfileEvaluation {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// 至少一条 Rule 处于 [`RuleStatus::Match`]
    pub matched: bool,
    pub rules: Vec<RuleReport>,
}

pub fn eval_condition(c: &Condition, snap: &NetworkSnapshot) -> ConditionStatus {
    if !c.enabled {
        return ConditionStatus::Disabled;
    }
    let hit = match c.kind {
        crate::config::ConditionType::WifiSsid => {
            // SSID 大小写敏感（802.11 的 SSID 本身就是字节串）
            snap.ssid.as_deref() == Some(c.value.as_str())
        }
        crate::config::ConditionType::GatewayMac => mac_eq(snap.gateway_mac.as_deref(), &c.value),
        crate::config::ConditionType::Bssid => mac_eq(snap.bssid.as_deref(), &c.value),
        crate::config::ConditionType::NetworkInterface => snap
            .interfaces
            .iter()
            .any(|n| n.eq_ignore_ascii_case(c.value.trim())),
    };
    if hit {
        ConditionStatus::Match
    } else {
        ConditionStatus::NoMatch
    }
}

pub fn eval_rule(r: &Rule, snap: &NetworkSnapshot) -> RuleReport {
    let reports: Vec<ConditionReport> = r
        .conditions
        .iter()
        .map(|c| ConditionReport {
            id: c.id.clone(),
            kind: c.kind.as_str().to_string(),
            value: c.value.clone(),
            status: eval_condition(c, snap),
        })
        .collect();
    let status = rule_status(r.enabled, &reports);
    RuleReport {
        id: r.id.clone(),
        status,
        conditions: reports,
    }
}

fn rule_status(enabled: bool, reports: &[ConditionReport]) -> RuleStatus {
    if !enabled {
        return RuleStatus::Inactive;
    }
    let live: Vec<&ConditionReport> = reports
        .iter()
        .filter(|c| c.status != ConditionStatus::Disabled)
        .collect();
    if live.is_empty() {
        // 所有条件都被禁用 —— 这条 Rule 什么都没主张，不能算 Match
        return RuleStatus::Inactive;
    }
    if live.iter().all(|c| c.status == ConditionStatus::Match) {
        RuleStatus::Match
    } else {
        RuleStatus::NoMatch
    }
}

pub fn eval_profile(p: &Profile, snap: &NetworkSnapshot) -> ProfileEvaluation {
    let rules: Vec<RuleReport> = p.rules.iter().map(|r| eval_rule(r, snap)).collect();
    let matched = p.enabled && rules.iter().any(|r| r.status == RuleStatus::Match);
    ProfileEvaluation {
        id: p.id.clone(),
        name: p.name.clone(),
        enabled: p.enabled,
        matched,
        rules,
    }
}

/// 全部 Profile 的求值结果。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Evaluation {
    pub profiles: Vec<ProfileEvaluation>,
    /// 命中的 Profile id（按配置顺序）。长度决定 No Active / Active / Conflict。
    pub matched_ids: Vec<String>,
}

/// 求值所有 Profile。**不**在这里挑「该用哪个」—— 方案第 5/6 条：多命中不自动选择。
pub fn evaluate_all(profiles: &[Profile], snap: &NetworkSnapshot) -> Evaluation {
    let evals: Vec<ProfileEvaluation> = profiles
        .iter()
        .map(|p| eval_profile(p, snap))
        .collect();
    let matched_ids = evals
        .iter()
        .filter(|e| e.matched)
        .map(|e| e.id.clone())
        .collect();
    Evaluation {
        profiles: evals,
        matched_ids,
    }
}

/// MAC 比较：两侧都归一化后再比，避免 `AA-BB-..` / `aa:bb:..` 这类
/// 平台差异（Windows `arp -a` 用连字符，macOS/Linux 用冒号）造成漏匹配。
fn mac_eq(actual: Option<&str>, expected: &str) -> bool {
    match actual {
        Some(a) => crate::platform::normalize_mac(a) == crate::platform::normalize_mac(expected),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::DetectionConfig;
    use crate::config::{Condition, ConditionType, Profile, Rule};

    fn cond(id: &str, enabled: bool, kind: ConditionType, value: &str) -> Condition {
        Condition {
            id: id.to_string(),
            enabled,
            kind,
            value: value.to_string(),
        }
    }

    fn rule(id: &str, enabled: bool, conditions: Vec<Condition>) -> Rule {
        Rule {
            id: id.to_string(),
            enabled,
            conditions,
        }
    }

    fn profile(id: &str, enabled: bool, rules: Vec<Rule>) -> Profile {
        Profile {
            id: id.to_string(),
            name: id.to_uppercase(),
            enabled,
            detection: DetectionConfig::default(),
            rules,
            then: None,
            else_branch: None,
        }
    }

    fn snap() -> NetworkSnapshot {
        NetworkSnapshot {
            ssid: Some("Office".to_string()),
            gateway_mac: Some("aa:bb:cc:dd:ee:ff".to_string()),
            bssid: Some("00:11:22:33:44:55".to_string()),
            primary_interface: Some("en0".to_string()),
            interfaces: vec!["en0".to_string(), "en7".to_string()],
            tunnels: vec!["utun4".to_string()],
        }
    }

    #[test]
    fn enabled_conditions_are_anded() {
        let p = profile(
            "o",
            true,
            vec![rule(
                "r1",
                true,
                vec![
                    cond("c1", true, ConditionType::WifiSsid, "Office"),
                    cond("c2", true, ConditionType::Bssid, "99:99:99:99:99:99"),
                ],
            )],
        );
        let e = eval_profile(&p, &snap());
        assert_eq!(e.rules[0].status, RuleStatus::NoMatch);
        assert!(!e.matched, "SSID 对但 BSSID 不符，AND 之后不得命中");
    }

    #[test]
    fn rules_are_ored() {
        let p = profile(
            "o",
            true,
            vec![
                rule(
                    "r1",
                    true,
                    vec![cond("c1", true, ConditionType::WifiSsid, "Nope")],
                ),
                rule(
                    "r2",
                    true,
                    vec![cond("c2", true, ConditionType::WifiSsid, "Office")],
                ),
            ],
        );
        let e = eval_profile(&p, &snap());
        assert_eq!(e.rules[0].status, RuleStatus::NoMatch);
        assert_eq!(e.rules[1].status, RuleStatus::Match);
        assert!(e.matched);
    }

    #[test]
    fn disabled_condition_does_not_wildcard_the_rule() {
        // 关掉那条对不上的 BSSID 条件后，Rule 应当命中
        let p = profile(
            "o",
            true,
            vec![rule(
                "r1",
                true,
                vec![
                    cond("c1", true, ConditionType::WifiSsid, "Office"),
                    cond("c2", false, ConditionType::Bssid, "99:99:99:99:99:99"),
                ],
            )],
        );
        let e = eval_profile(&p, &snap());
        assert_eq!(e.rules[0].conditions[1].status, ConditionStatus::Disabled);
        assert_eq!(e.rules[0].status, RuleStatus::Match);
    }

    #[test]
    fn all_conditions_disabled_makes_rule_inactive_and_profile_unmatched() {
        let p = profile(
            "o",
            true,
            vec![rule(
                "r1",
                true,
                vec![
                    cond("c1", false, ConditionType::WifiSsid, "Office"),
                    cond("c2", false, ConditionType::GatewayMac, "aa:bb:cc:dd:ee:ff"),
                ],
            )],
        );
        let e = eval_profile(&p, &snap());
        assert_eq!(e.rules[0].status, RuleStatus::Inactive);
        assert!(
            !e.matched,
            "没有有效条件 = 不匹配，绝不能恒真（否则每个网络都会因它而 Conflict）"
        );
    }

    #[test]
    fn mac_comparison_normalises_case_and_separator() {
        let p = profile(
            "o",
            true,
            vec![rule(
                "r1",
                true,
                vec![cond("c1", true, ConditionType::GatewayMac, "AA-BB-CC-DD-EE-FF")],
            )],
        );
        assert!(eval_profile(&p, &snap()).matched);
    }

    #[test]
    fn interface_condition_checks_all_live_nics_not_just_primary() {
        let p = profile(
            "o",
            true,
            vec![rule(
                "r1",
                true,
                vec![cond("c1", true, ConditionType::NetworkInterface, "EN7")],
            )],
        );
        assert!(
            eval_profile(&p, &snap()).matched,
            "有线+无线同时插着时，第二张网卡也算「在」"
        );
    }

    #[test]
    fn disabled_profile_never_matches_but_still_reports_rule_states() {
        let p = profile(
            "o",
            false,
            vec![rule(
                "r1",
                true,
                vec![cond("c1", true, ConditionType::WifiSsid, "Office")],
            )],
        );
        let e = eval_profile(&p, &snap());
        assert!(!e.matched);
        assert_eq!(
            e.rules[0].status,
            RuleStatus::Match,
            "禁用的 Profile 仍要给出实时条件状态，否则 UI 全是灰的、看不出「一启用就会命中」"
        );
    }

    #[test]
    fn evaluate_all_collects_every_match_without_choosing() {
        let a = profile(
            "a",
            true,
            vec![rule(
                "r1",
                true,
                vec![cond("c1", true, ConditionType::WifiSsid, "Office")],
            )],
        );
        let b = profile(
            "b",
            true,
            vec![rule(
                "r1",
                true,
                vec![cond("c1", true, ConditionType::GatewayMac, "aa:bb:cc:dd:ee:ff")],
            )],
        );
        let c = profile(
            "c",
            true,
            vec![rule(
                "r1",
                true,
                vec![cond("c1", true, ConditionType::WifiSsid, "Home")],
            )],
        );
        let ev = evaluate_all(&[a, b, c], &snap());
        assert_eq!(ev.matched_ids, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            ev.profiles.iter().find(|p| p.id == "c").unwrap().rules[0].status,
            RuleStatus::NoMatch
        );
    }
}
