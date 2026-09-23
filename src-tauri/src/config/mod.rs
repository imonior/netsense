//! 配置读写与校验（schema 1）。
//!
//! 类型定义在 [`model`]；本模块只负责「磁盘上的 JSON 是不是我们认识的东西」。

pub mod model;

use std::path::Path;

pub use model::{
    Branch, Condition, ConditionType, Config, DetectionMode, FallbackConfig, HealthConfig, Mode,
    NetworkConfig, OneShotAction, OneShotActionType, PersistentAction, PersistentActionType,
    Profile, ProbeMode, Rule, V6Mode, SCHEMA,
};

/// 校验产物：一个可展示的告警。
///
/// 有「告警」这一层而不是只有错误：配置**语法**没问题、但语义上有一件事不会按用户
/// 期望发生。`persistent`（3B2）就是典型 —— 它只跟着 Active 的 THEN 分支起 worker，
/// ELSE 分支里配的那些不会被维持。与其静默不执行（用户会以为「配了没生效」，这种
/// 表现最难查），不如在每次评估时明确报出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning(pub String);

impl Config {
    /// 读取配置（不校验；调用方应随后 [`Config::validate`]）。
    ///
    /// schema 缺失或不符时**不**回退成空配置继续跑：一份我们不认识的配置，它的匹配
    /// 语义可能和这一版相反（多命中是自动挑一个，还是判成冲突不应用）。拿猜出来的
    /// 语义往用户的网卡上下发一份没人确认的静态 IP，比停下来危险。
    pub fn load(path: &Path) -> Result<Config, String> {
        let data = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        Self::from_json(&data)
    }

    pub fn from_json(data: &str) -> Result<Config, String> {
        let raw: serde_json::Value =
            serde_json::from_str(data).map_err(|e| format!("JSON 解析失败: {}", e))?;
        let Some(found) = raw.get("schema").and_then(|v| v.as_u64()) else {
            return Err(format!(
                "config.json 顶层缺少 \"schema\" 字段（本版本需要 \"schema\": {}）。\
                 缺省不会被猜成某个版本：请参照 config.example.json 补上。",
                SCHEMA
            ));
        };
        if found as u32 != SCHEMA {
            return Err(format!(
                "config.json 的 schema={found} 不被支持（本版本需要 schema={SCHEMA}）。\
                 本版本把多份 Profile 同时命中判为冲突且不应用任何一支，别的 schema 未必要\
                 这么解释，所以不做自动迁移；请参照 config.example.json 逐项核对。",
            ));
        }
        serde_json::from_value(raw).map_err(|e| format!("配置结构不合法: {}", e))
    }

    /// 写回磁盘（pretty JSON，并带上 schema）
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut me = self.clone();
        me.schema = SCHEMA;
        let s = serde_json::to_string_pretty(&me).map_err(|e| e.to_string())?;
        std::fs::write(path, s).map_err(|e| e.to_string())
    }

    pub fn profile_by_id(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn profile_by_id_mut(&mut self, id: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.id == id)
    }

    /// 结构性校验：把「加载后必然出错」的配置项在落盘前就拦住。
    ///
    /// 注意它**不**判断「能不能匹配上」—— 没有有效条件的 Profile 是合法的，
    /// 只是永远 `NOT MATCHED`（见 `conditions::evaluator`）。
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!("schema 必须为 {}", SCHEMA));
        }
        let mut ids: Vec<&str> = Vec::new();
        for p in &self.profiles {
            if p.id.trim().is_empty() {
                return Err(format!("profile '{}' 缺少 id", p.name));
            }
            if p.name.trim().is_empty() {
                return Err(format!("profile '{}' 缺少 name", p.id));
            }
            if ids.contains(&p.id.as_str()) {
                return Err(format!("profile id 重复: {}", p.id));
            }
            ids.push(&p.id);
            if p.rules.is_empty() {
                return Err(format!(
                    "profile '{}' 没有任何 rule（无 rule 的 profile 永远不匹配，请直接禁用或删除它）",
                    p.name
                ));
            }
            let mut rule_ids: Vec<&str> = Vec::new();
            for r in &p.rules {
                if r.id.trim().is_empty() {
                    return Err(format!("profile '{}' 有 rule 缺少 id", p.name));
                }
                if rule_ids.contains(&r.id.as_str()) {
                    return Err(format!(
                        "profile '{}' 内 rule id 重复: {}",
                        p.name, r.id
                    ));
                }
                rule_ids.push(&r.id);
                if r.conditions.is_empty() {
                    return Err(format!(
                        "profile '{}' 的 rule '{}' 为空（空 rule 恒不匹配，请删除它或加条件）",
                        p.name, r.id
                    ));
                }
                for c in &r.conditions {
                    validate_condition(&p.name, c)?;
                }
            }
            if let Some(b) = &p.then {
                validate_branch(&format!("profile '{}' then", p.name), b)?;
            }
            if let Some(b) = &p.else_branch {
                validate_branch(&format!("profile '{}' else", p.name), b)?;
            }
        }
        if let Some(fb) = &self.fallback {
            if let Some(n) = &fb.network {
                validate_network("fallback.network", n)?;
            }
        }
        Ok(())
    }

    /// 能跑但需要注意的事项（不阻断加载）。
    ///
    /// THEN 分支的常驻动作不需要告警 —— 它现在真的会起 worker（`automation::persistent`）。
    /// 剩下这一种是真需要注意的：ELSE 表达「离开这个环境时要维持什么」，而离开时并没有
    /// 一个持续成立的现场，所以那几条永远不会被维持。
    pub fn warnings(&self) -> Vec<Warning> {
        let mut out = Vec::new();
        for p in &self.profiles {
            let Some(b) = p.else_branch.as_ref() else {
                continue;
            };
            let live = b.persistent.iter().filter(|a| a.enabled).count();
            if live > 0 {
                out.push(Warning(format!(
                    "profile '{}' 的 else 分支配了 {} 个 persistent 动作：常驻动作只随 Active 的 then 分支启动，这一支不会被维持",
                    p.name, live
                )));
            }
        }
        out
    }
}

fn validate_condition(profile: &str, c: &Condition) -> Result<(), String> {
    if c.id.trim().is_empty() {
        return Err(format!("profile '{}' 有条件缺少 id", profile));
    }
    if c.value.trim().is_empty() {
        return Err(format!(
            "profile '{}' 的条件 '{}' ({}) 值为空：禁用它，或填上比较值",
            profile,
            c.id,
            c.kind.as_str()
        ));
    }
    let v = c.value.trim();
    match c.kind {
        ConditionType::GatewayMac | ConditionType::Bssid => {
            if !looks_like_mac(v) {
                return Err(format!(
                    "profile '{}' 的条件 '{}' 需要 MAC 形态的值（当前: {}）",
                    profile, c.id, v
                ));
            }
        }
        ConditionType::NetworkInterface | ConditionType::WifiSsid => {}
    }
    Ok(())
}

/// 一条 3A 校验策略里的健康度部分。
///
/// 关着（`enabled: false`）时**不**校验内容：探测参数是随分支一起存的，用户完全可以
/// 先写好再开。开了却没目标 / 没间隔，跑起来就是「每 0 秒探测一次且必然失败」，
/// 那种配置会在 Active 期间反复触发回落 DHCP —— 必须在落盘前拦住。
fn validate_health(what: &str, h: &HealthConfig) -> Result<(), String> {
    let has = |s: &Option<String>| {
        s.as_deref().map(|v| !v.trim().is_empty()).unwrap_or(false)
    };
    if h.enabled {
        match h.mode {
            ProbeMode::Icmp if !has(&h.icmp_target) => {
                return Err(format!("{}: 健康度探测 mode=icmp 却没填 icmp_target", what));
            }
            ProbeMode::Http if !has(&h.http_target) => {
                return Err(format!("{}: 健康度探测 mode=http 却没填 http_target", what));
            }
            ProbeMode::Both if !has(&h.icmp_target) && !has(&h.http_target) => {
                return Err(format!("{}: 健康度探测 mode=both 至少要有一个探测目标", what));
            }
            _ => {}
        }
        if h.interval == 0 {
            return Err(format!("{}: 健康度探测 interval 必须大于 0", what));
        }
        if h.retries == 0 {
            return Err(format!(
                "{}: 健康度探测 retries 必须大于 0（0 意味着永远不会判定为失败）",
                what
            ));
        }
        if h.timeout == 0 {
            return Err(format!("{}: 健康度探测 timeout 必须大于 0", what));
        }
    }
    Ok(())
}

/// 领取一个动作 id：非空且在本分支内唯一。
fn claim_action_id<'a>(
    what: &str,
    seen: &mut Vec<&'a str>,
    id: &'a str,
    kind: &str,
) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err(format!("{}: {} 动作缺少 id", what, kind));
    }
    if seen.contains(&id) {
        return Err(format!("{}: 动作 id 重复: {}", what, id));
    }
    seen.push(id);
    Ok(())
}

fn validate_branch(what: &str, b: &Branch) -> Result<(), String> {
    if let Some(n) = &b.network {
        validate_network(what, n)?;
    }
    // 3B1 与 3B2 共用一套 id。动作结果（`EngineView.last_run`）是按 id 找回对应卡片的，
    // 撞名会让两条动作显示同一个成败；3B2 落地后那条更会成为串台。
    let mut ids: Vec<&str> = Vec::new();
    for a in &b.one_shot {
        claim_action_id(what, &mut ids, &a.id, "one_shot")?;
        match &a.action {
            OneShotActionType::LaunchApp { app, .. } => {
                if app.trim().is_empty() {
                    return Err(format!("{}: 动作 '{}' 是 launch_app 却没填 app", what, a.id));
                }
            }
            OneShotActionType::RunScript { path, .. } => {
                if path.trim().is_empty() {
                    return Err(format!("{}: 动作 '{}' 是 run_script 却没填 path", what, a.id));
                }
            }
        }
    }
    for a in &b.persistent {
        claim_action_id(what, &mut ids, &a.id, "persistent")?;
        let (kind, interval) = match &a.action {
            PersistentActionType::PeriodicScript { path, interval_secs, .. } => {
                if path.trim().is_empty() {
                    return Err(format!("{}: 动作 '{}' 是 periodic_script 却没填 path", what, a.id));
                }
                ("periodic_script", interval_secs)
            }
            PersistentActionType::KeepWireGuardConnected { tunnel, interval_secs } => {
                if tunnel.trim().is_empty() {
                    return Err(format!(
                        "{}: 动作 '{}' 是 keep_wireguard_connected 却没填 tunnel", what, a.id
                    ));
                }
                ("keep_wireguard_connected", interval_secs)
            }
            PersistentActionType::KeepVpnConnected { provider, profile, interval_secs } => {
                if provider.trim().is_empty() || profile.trim().is_empty() {
                    return Err(format!(
                        "{}: 动作 '{}' 是 keep_vpn_connected，provider 与 profile 都要填", what, a.id
                    ));
                }
                ("keep_vpn_connected", interval_secs)
            }
        };
        // 常驻动作是一根循环轮询的定时器：间隔 0 在语义上就是「无限快地检查」。
        if *interval == 0 {
            return Err(format!("{}: 动作 '{}' ({}) 的 interval_secs 必须大于 0", what, a.id, kind));
        }
    }
    Ok(())
}

fn validate_network(what: &str, n: &NetworkConfig) -> Result<(), String> {
    if n.mode == Mode::Manual {
        for (field, val) in [
            ("ip", &n.ip),
            ("netmask", &n.netmask),
            ("gateway", &n.gateway),
        ] {
            if val.as_deref().unwrap_or("").trim().is_empty() {
                return Err(format!("{}: manual 模式需要填写 {}", what, field));
            }
        }
        if !n.ip.as_deref().map(is_ipv4).unwrap_or(false) {
            return Err(format!("{}: ip 不是合法 IPv4: {:?}", what, n.ip));
        }
    }
    if let Some(dns) = &n.dns {
        for part in dns.split(',') {
            let t = part.trim();
            if t.is_empty() {
                continue;
            }
            if !is_ipv4(t) {
                return Err(format!("{}: DNS 不合法: {}", what, t));
            }
        }
    }
    if n.v6mode == Some(V6Mode::Manual) && n.ipv6.as_deref().unwrap_or("").trim().is_empty() {
        return Err(format!("{}: v6mode=manual 需要 ipv6 地址", what));
    }
    for r in &n.routes {
        if r.dest.trim().is_empty() {
            return Err(format!("{}: routes 里有空的 dest", what));
        }
        if !r.delete && r.gateway.as_deref().unwrap_or("").trim().is_empty() {
            return Err(format!(
                "{}: 路由 {} 需要 gateway（删除路由才允许省略）",
                what, r.dest
            ));
        }
    }
    if let Some(h) = n.verify.as_ref().and_then(|v| v.health.as_ref()) {
        validate_health(&format!("{}.verify.health", what), h)?;
    }
    Ok(())
}

fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok())
}

/// `aa:bb:cc:dd:ee:ff` / `aa-bb-cc-dd-ee-ff`（大小写不限）。
fn looks_like_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 整份示例配置必须能被加载并校验通过。
    ///
    /// 这条测试不是形式主义：示例即契约，一个字段表示与 Rust 枚举不一致就会让**整份**
    /// 配置反序列化失败，而那种失败的表现是「改了配置却完全没生效」，最难查。
    #[test]
    fn example_config_loads_and_validates() {
        let raw = include_str!("../../../config.example.json");
        let cfg = Config::from_json(raw).expect("config.example.json 必须能加载");
        cfg.validate().expect("config.example.json 必须能通过校验");
        assert_eq!(cfg.profiles.len(), 3);
        let office = cfg
            .profile_by_id("office")
            .expect("示例含 id=office 的 profile");
        assert_eq!(office.rules.len(), 2, "示例的 office 演示了 Rule 间 OR");
        assert!(office.then.as_ref().unwrap().one_shot.len() >= 2);
    }

    /// 一份本版本不认识的配置要明确拒绝，而不是回退成空配置照常跑。
    #[test]
    fn an_unrecognised_schema_is_rejected_with_an_actionable_message() {
        // 形状不对且没有 schema 字段：顶层直接是 profile 名的 map
        let no_schema = r#"{"__DEFAULT__":{"mode":"dhcp"},"Home":{"match":{"ssid":"Home"},"priority":5}}"#;
        let err = Config::from_json(no_schema).expect_err("缺 schema 字段必须被拒绝");
        assert!(err.contains("schema"), "报错要指向 schema: {}", err);

        // 有字段但不是本版本认识的号：报错要带上读到的那个号，用户才知道该改哪
        let other = Config::from_json(r#"{"schema":9,"profiles":[]}"#)
            .expect_err("未知 schema 必须被拒绝");
        assert!(other.contains("9"), "报错要带上实际的号: {}", other);
    }

    #[test]
    fn roundtrip_keeps_semantics() {
        let raw = include_str!("../../../config.example.json");
        let cfg = Config::from_json(raw).unwrap();
        let text = serde_json::to_string(&cfg).unwrap();
        let back = Config::from_json(&text).expect("序列化后必须能读回来");
        back.validate().unwrap();
        assert_eq!(back.profiles.len(), cfg.profiles.len());
    }

    #[test]
    fn empty_condition_value_is_rejected() {
        // 空值条件不是「通配」，而是配错了：放过去等于把整个 Rule 变成永不匹配
        let raw = r#"{"schema":1,"profiles":[{"id":"a","name":"A","rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":""}]}]}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        let err = cfg.validate().expect_err("空值条件必须被拒绝");
        assert!(err.contains("值为空"), "报错内容: {}", err);
    }

    #[test]
    fn manual_network_needs_full_ipv4_settings() {
        let raw = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"network":{"mode":"manual","ip":"10.0.0.1"}}}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        let err = cfg.validate().expect_err("manual 缺 netmask/gateway 必须报错");
        assert!(err.contains("netmask"), "报错内容: {}", err);
    }

    #[test]
    fn route_add_requires_gateway_but_delete_does_not() {
        let raw = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "else":{"network":{"mode":"dhcp","routes":[{"dest":"10.0.0.0/8","delete":true}]}}}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        cfg.validate().expect("删除路由允许省略 gateway");

        let bad = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"network":{"mode":"dhcp","routes":[{"dest":"10.0.0.0/8"}]}}}]}"#;
        let cfg = Config::from_json(bad).unwrap();
        assert!(cfg.validate().is_err(), "加路由必须有 gateway");
    }

    /// 一条动作载荷配错了，跑起来的表现是「什么也没发生」，比加载失败更难排查 ——
    /// 所以空 app / 空 path / 空 tunnel / 0 间隔都在落盘前拦住。
    #[test]
    fn action_payloads_must_be_complete() {
        let cases = [
            (r#"{"type":"launch_app","app":""}"#, "app"),
            (r#"{"type":"run_script","path":"  "}"#, "path"),
        ];
        for (action, field) in cases {
            let raw = format!(
                r#"{{"schema":1,"profiles":[{{"id":"a","name":"A",
                  "rules":[{{"id":"r1","conditions":[{{"id":"c1","type":"wifi_ssid","value":"X"}}]}}],
                  "then":{{"one_shot":[{{"id":"o1","action":{}}}]}}}}]}}"#,
                action
            );
            let cfg = Config::from_json(&raw).unwrap();
            let err = cfg.validate().expect_err("空动作载荷必须被拒绝");
            assert!(err.contains(field), "报错要指向 {}: {}", field, err);
        }

        let raw = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"persistent":[{"id":"p1","action":{"type":"keep_wireguard_connected","tunnel":"","interval_secs":10}}]}}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        assert!(cfg.validate().unwrap_err().contains("tunnel"));

        let raw = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"persistent":[{"id":"p1","action":{"type":"keep_vpn_connected","provider":"gp","profile":"","interval_secs":10}}]}}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        assert!(cfg.validate().unwrap_err().contains("provider"));
    }

    /// 常驻动作的 `interval_secs: 0` 是一根没有间隔的轮询循环。
    #[test]
    fn persistent_actions_need_a_nonzero_interval() {
        let raw = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"persistent":[{"id":"p1","action":{"type":"periodic_script","path":"scripts/k.sh","interval_secs":0}}]}}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        assert!(cfg.validate().unwrap_err().contains("interval_secs"));
    }

    /// 3B1 与 3B2 共用一套 id：动作结果按 id 找回卡片，跨列表撞名同样会串台。
    #[test]
    fn one_shot_and_persistent_share_one_id_namespace() {
        let raw = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"one_shot":[{"id":"x","action":{"type":"launch_app","app":"Notes.app"}}],
                  "persistent":[{"id":"x","action":{"type":"keep_wireguard_connected","tunnel":"wg0"}}]}}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        let err = cfg.validate().expect_err("跨列表撞 id 必须被拒绝");
        assert!(err.contains("重复"), "报错内容: {}", err);
    }

    /// 健康度探测关着时不校验内容（先把参数写好再打开是合法用法）；
    /// 开着却没目标 / 零间隔必须拦住 —— 那种配置会在 Active 期间反复触发回落 DHCP。
    #[test]
    fn enabled_health_probe_needs_a_target_and_positive_timing() {
        // 用占位符而不是 format!：这段 JSON 里的花括号已经够多了，再叠一层 `{{` 转义只会让测试自己出错。
        let with = |verify: &str| {
            r#"{"schema":1,"profiles":[{"id":"a","name":"A",
               "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
               "then":{"network":{"mode":"dhcp","verify":__VERIFY__}}}]}"#
                .replace("__VERIFY__", verify)
        };
        let cfg = Config::from_json(&with(r#"{"health":{"enabled":false,"mode":"icmp"}}"#)).unwrap();
        cfg.validate().expect("关掉的探测不该拦落盘");

        let cfg = Config::from_json(&with(r#"{"health":{"enabled":true,"mode":"icmp"}}"#)).unwrap();
        let err = cfg.validate().expect_err("icmp 模式必须有目标");
        assert!(err.contains("icmp_target"), "报错内容: {}", err);

        let cfg = Config::from_json(
            &with(r#"{"health":{"enabled":true,"mode":"both","icmp_target":"1.1.1.1","interval":0}}"#),
        )
        .unwrap();
        let err = cfg.validate().expect_err("0 间隔等于自旋");
        assert!(err.contains("interval"), "报错内容: {}", err);

        let cfg = Config::from_json(
            &with(r#"{"health":{"enabled":true,"mode":"both","http_target":"http://1.1.1.1"}}"#),
        )
        .unwrap();
        cfg.validate().expect("both 模式有一个目标即可");
    }

    #[test]
    fn duplicate_profile_ids_are_rejected() {
        let raw = r#"{"schema":1,"profiles":[
          {"id":"a","name":"A","rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}]},
          {"id":"a","name":"B","rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"Y"}]}]}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        assert!(cfg.validate().unwrap_err().contains("重复"));
    }

    /// 前端 `clean*` 序列化出来的动作标签必须能被 serde 认出来。
    ///
    /// 这是一条**跨语言契约**测试：标签字符串在 `frontend/editor.html` 与这里的
    /// serde 重命名规则各写一遍，任何一侧改名（例如 `KeepVpnConnected` 的
    /// snake_case 是 `keep_vpn_connected` 而不是 `keep_vpn`）都会让整份配置加载失败。
    #[test]
    fn action_tags_the_editor_emits_are_the_tags_serde_accepts() {
        let raw = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"one_shot":[
            {"id":"a1","enabled":true,"priority":1,"action":{"type":"launch_app","app":"/Applications/X.app"}},
            {"id":"a2","enabled":true,"priority":2,"action":{"type":"run_script","path":"scripts/x.sh"}}],
            "persistent":[
            {"id":"p1","enabled":true,"priority":1,"action":{"type":"periodic_script","path":"scripts/keep.sh","interval_secs":10}},
            {"id":"p2","enabled":true,"priority":2,"action":{"type":"keep_wireguard_connected","tunnel":"wg0","interval_secs":15}},
            {"id":"p3","enabled":true,"priority":2,"action":{"type":"keep_vpn_connected","provider":"globalprotect","profile":"corp","interval_secs":20}}]}}]}"#;
        let cfg = Config::from_json(raw).expect("编辑器产出的动作标签必须能反序列化");
        cfg.validate().expect("同一份配置必须通过校验");
        let p = cfg.profile_by_id("a").unwrap();
        let then = p.then.as_ref().unwrap();
        assert_eq!(then.one_shot.len(), 2);
        assert!(matches!(
            then.persistent[0].action,
            model::PersistentActionType::PeriodicScript { interval_secs: 10, .. }
        ));
        assert!(matches!(
            then.persistent[1].action,
            model::PersistentActionType::KeepWireGuardConnected { ref tunnel, .. } if tunnel == "wg0"
        ));
        assert!(matches!(
            then.persistent[2].action,
            model::PersistentActionType::KeepVpnConnected { ref profile, interval_secs: 20, .. } if profile == "corp"
        ));
    }

    /// THEN 的常驻动作现在真的会起 worker，所以它不该再有告警；ELSE 的那几条不会跑，
    /// 必须报出来 —— 「配了却什么都不发生」是最难查的一类反馈。
    #[test]
    fn persistent_actions_on_the_else_branch_are_reported_as_warnings() {
        let wg = |branch: &str| {
            format!(
                r#"{{"schema":1,"profiles":[{{"id":"a","name":"A",
                  "rules":[{{"id":"r1","conditions":[{{"id":"c1","type":"wifi_ssid","value":"X"}}]}}],
                  "{}":{{"persistent":[{{"id":"p1","priority":1,"action":{{"type":"keep_wireguard_connected","tunnel":"wg0"}}}}]}}}}]}}"#,
                branch
            )
        };
        let then = Config::from_json(&wg("then")).expect("persistent 的语法必须合法");
        then.validate().expect("常驻动作不该让配置加载失败");
        assert!(
            then.warnings().is_empty(),
            "then 分支的常驻动作会被维持，不该告警: {:?}",
            then.warnings()
        );

        let els = Config::from_json(&wg("else")).expect("同上");
        els.validate().expect("同上");
        let w = els.warnings();
        assert_eq!(w.len(), 1, "必须报出一条告警: {:?}", w);
        assert!(w[0].0.contains("persistent") && w[0].0.contains("else"), "{:?}", w[0].0);
    }

    /// 上一条测试手写的是「编辑器**应该**发出的 payload」，这条验收的是它**实际**发出的那些：
    /// `scripts/editor-smoke.mjs --write-fixtures <file>` 在 stubbed DOM 里真跑一遍
    /// `frontend/editor.html`，把它递给 `save_profile` / `save_global` 的 payload 落成文件。
    ///
    /// 为什么非得走这一趟：serde 对不认识的键是静默忽略的，所以前端改错一处（把 `elevated`
    /// 挪到动作对象外、给 launch_app 带上 path）在浏览器里毫无痕迹，只有把它真喂进反序列化
    /// 再整份校验一次才看得见。复用的是 `ipc.rs` 那条 upsert → validate 的路径，
    /// 基线取仓库里的 `config.example.json` —— 两侧各存一份基线本身就是漂移的起点。
    ///
    /// 没有 `NS_EDITOR_FIXTURES` 时直接返回：本地 `cargo test` 不该被迫依赖 Node。
    #[test]
    fn payloads_the_editor_actually_sends_are_the_ones_serde_accepts() {
        let Ok(path) = std::env::var("NS_EDITOR_FIXTURES") else {
            return;
        };
        #[derive(serde::Deserialize)]
        struct Fixture {
            kind: String,
            payload: serde_json::Value,
        }
        #[derive(serde::Deserialize)]
        struct Doc {
            fixtures: Vec<Fixture>,
        }
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取 {path} 失败: {e}"));
        let doc: Doc =
            serde_json::from_str(&raw).expect("fixture 文件必须是 {\"fixtures\":[{\"kind\",\"payload\"}]}");
        assert!(
            !doc.fixtures.is_empty(),
            "一条 payload 都没有：前端脚本大概没走到保存路径"
        );
        for (i, f) in doc.fixtures.iter().enumerate() {
            let mut cfg = Config::from_json(include_str!("../../../config.example.json"))
                .expect("config.example.json 必须能加载");
            match f.kind.as_str() {
                "profile" => {
                    let p: Profile = serde_json::from_value(f.payload.clone())
                        .unwrap_or_else(|e| panic!("第 {i} 条 save_profile 的 payload 反序列化失败: {e}"));
                    match cfg.profile_by_id_mut(&p.id) {
                        Some(slot) => *slot = p,
                        None => cfg.profiles.push(p),
                    }
                }
                "global" => {
                    let fb: Option<FallbackConfig> = serde_json::from_value(
                        f.payload
                            .get("fallback")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    )
                    .unwrap_or_else(|e| panic!("第 {i} 条 save_global 的 fallback 反序列化失败: {e}"));
                    cfg.fallback = fb;
                    cfg.allowed_scripts = f
                        .payload
                        .get("allowed_scripts")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                }
                other => panic!("未知 fixture 类型: {other}"),
            }
            cfg.validate()
                .unwrap_or_else(|e| panic!("第 {i} 条（{}）payload 未通过校验: {e}", f.kind));
        }
    }
}
