//! 配置读写与校验（schema 1）。
//!
//! 类型定义在 [`model`]；本模块只负责「磁盘上的 JSON 是不是我们认识的东西」。

pub mod model;

use std::collections::HashMap;
use std::path::Path;

use crate::i18n;

pub use model::{
    Branch, Condition, ConditionType, Config, DetectionMode, FallbackConfig, HealthConfig, Mode,
    NetworkConfig, OneShotAction, OneShotActionType, PersistentAction, PersistentActionType,
    Profile, ProbeMode, Rule, V6Mode, FALLBACK_ID, SCHEMA,
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
        // 带上路径：一句「读不到」不告诉用户是哪一份文件读不到，而本版本同时认用户目录
        // 与程序目录两处，光看错误文本分不出是哪一处。
        let data = std::fs::read_to_string(path).map_err(|e| {
            i18n::tf("cfg.read_failed", &[("path", &path.display().to_string()), ("error", &e.to_string())])
        })?;
        Self::from_json(&data)
    }

    pub fn from_json(data: &str) -> Result<Config, String> {
        // 句子的措辞随界面语言走，句中的 `schema` / `config.example.json` 这类**字段名与
        // 文件名原样保留**：用户排查时对着的是配置文件本身，被翻译过的字段名反而找不到。
        let raw: serde_json::Value = serde_json::from_str(data).map_err(|e| {
            i18n::tf("cfg.json_parse", &[("error", &e.to_string())])
        })?;
        let Some(found) = raw.get("schema").and_then(|v| v.as_u64()) else {
            return Err(i18n::tf("cfg.schema_missing", &[("schema", &SCHEMA.to_string())]));
        };
        if found as u32 != SCHEMA {
            return Err(i18n::tf("cfg.schema_unknown", &[
                ("found", &found.to_string()),
                ("schema", &SCHEMA.to_string()),
            ]));
        }
        serde_json::from_value(raw).map_err(|e| {
            i18n::tf("cfg.bad_structure", &[("error", &e.to_string())])
        })
    }

    /// 写回磁盘（pretty JSON，并带上 schema）。
    ///
    /// 需要时先把父目录建出来：配置现在住在用户目录（见 `paths`），首次运行时那个目录
    /// 还不存在 —— 「第一次保存」就是它的创建时机，不能因为目录不在就报错。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut me = self.clone();
        me.schema = SCHEMA;
        let s = serde_json::to_string_pretty(&me).map_err(|e| {
            i18n::tf("cfg.serialize_failed", &[("error", &e.to_string())])
        })?;
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(|e| {
                    i18n::tf("cfg.mkdir_failed", &[
                        ("dir", &dir.display().to_string()),
                        ("error", &e.to_string()),
                    ])
                })?;
            }
        }
        // 原子写：先写 `.part` 临时文件再 `rename` 覆盖，避免崩溃/掉电把 config.json 截成半截。
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config".to_string());
        let tmp = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{file_name}.part"));
        std::fs::write(&tmp, &s).map_err(|e| {
            i18n::tf("cfg.write_failed", &[("path", &tmp.display().to_string()), ("error", &e.to_string())])
        })?;
        std::fs::rename(&tmp, path).map_err(|e| {
            i18n::tf("cfg.write_failed", &[("path", &path.display().to_string()), ("error", &e.to_string())])
        })
    }

    pub fn profile_by_id(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn profile_by_id_mut(&mut self, id: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.id == id)
    }

    /// 判定这份配置需要身份快照里的哪几项读数。
    ///
    /// 只统计**启用**的 Profile 里**启用**的条件 —— 禁用的条件本来就参与不了判定。
    /// `network_interface` 不在这里出现：它比的是「在用的网卡集合」，那是个列表而不是
    /// 一项可能读空的身份证据，读空本身就是「没有网卡在用」这个结论。
    pub fn decision_inputs(&self) -> crate::conditions::identity::DecisionInputs {
        use crate::conditions::identity::DecisionInputs;
        use crate::config::model::ConditionType;
        let mut need = DecisionInputs {
            ssid: false,
            gateway_mac: false,
            bssid: false,
        };
        for p in self.profiles.iter().filter(|p| p.enabled) {
            for r in p.rules.iter().filter(|r| r.enabled) {
                for c in r.conditions.iter().filter(|c| c.enabled) {
                    match c.kind {
                        ConditionType::WifiSsid => need.ssid = true,
                        ConditionType::GatewayMac => need.gateway_mac = true,
                        ConditionType::Bssid => need.bssid = true,
                        ConditionType::NetworkInterface => {}
                    }
                }
            }
        }
        need
    }

    /// 结构性校验：把「加载后必然出错」的配置项在落盘前就拦住。
    ///
    /// 注意它**不**判断「能不能匹配上」—— 没有有效条件的 Profile 是合法的，
    /// 只是永远 `NOT MATCHED`（见 `conditions::evaluator`）。
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(i18n::tf("cfg.schema", &[("schema", &SCHEMA.to_string())]));
        }
        let mut ids: Vec<&str> = Vec::new();
        for p in &self.profiles {
            if p.id.trim().is_empty() {
                return Err(i18n::tf("cfg.no_id", &[("name", &p.name)]));
            }
            if p.name.trim().is_empty() {
                return Err(i18n::tf("cfg.no_name", &[("id", &p.id)]));
            }
            if ids.contains(&p.id.as_str()) {
                return Err(i18n::tf("cfg.dup_id", &[("id", &p.id)]));
            }
            // 兜底的合成身份用的是保留 id（见 [`FALLBACK_ID`]）：真 Profile 占了它，
            // 运行留痕与 worker 归属就会把两件事说成一件。
            if p.id == FALLBACK_ID {
                return Err(i18n::tf("cfg.reserved_id", &[("id", &p.id)]));
            }
            ids.push(&p.id);
            if p.rules.is_empty() {
                return Err(i18n::tf("cfg.no_rules", &[("name", &p.name)]));
            }
            let mut rule_ids: Vec<&str> = Vec::new();
            for r in &p.rules {
                if r.id.trim().is_empty() {
                    return Err(i18n::tf("cfg.rule_no_id", &[("name", &p.name)]));
                }
                if rule_ids.contains(&r.id.as_str()) {
                    return Err(i18n::tf("cfg.rule_dup_id", &[
                        ("name", &p.name),
                        ("id", &r.id),
                    ]));
                }
                rule_ids.push(&r.id);
                if r.conditions.is_empty() {
                    return Err(i18n::tf("cfg.rule_empty", &[
                        ("name", &p.name),
                        ("rule", &r.id),
                    ]));
                }
                for c in &r.conditions {
                    validate_condition(&p.name, c)?;
                }
            }
            if let Some(b) = &p.then {
                validate_branch(&branch_ctx(&p.name, "then"), b)?;
            }
            if let Some(b) = &p.else_branch {
                validate_branch(&branch_ctx(&p.name, "else"), b)?;
                // ELSE 分支的常驻动作直接拒绝：它表达「离开这个环境时要维持什么」，
                // 而离开时并没有一个持续成立的现场，所以那几条永远不会被维持。
                // 与其让用户以为「配了没生效」（这种表现最难查），不如在加载时就拦住。
                let live = b.persistent.iter().filter(|a| a.enabled).count();
                if live > 0 {
                    return Err(i18n::tf("cfg.err_else_persistent", &[
                        ("name", &p.name),
                        ("count", &live.to_string()),
                    ]));
                }
            }
        }
        if let Some(fb) = &self.fallback {
            if let Some(n) = &fb.network {
                validate_network("fallback.network", n)?;
            }
            // 兜底的动作也要过同一道校验：它跑起来和一条 THEN 分支没有区别
            //（3A 屏障、3B1 屏障、3B2 worker 都是同一套代码路径）。
            validate_actions("fallback", &fb.one_shot, &fb.persistent)?;
        }
        Ok(())
    }

    /// 能跑但需要注意的事项（不阻断加载）。
    ///
    /// ELSE 分支的常驻动作已在 `validate()` 中被拒绝，这里不再重复报。
    pub fn warnings(&self) -> Vec<Warning> {
        Vec::new()
    }
}

/// 一条分支诊断的主语。`then` / `else` 是配置里的字段名，所以它们不进字典。
fn branch_ctx(profile: &str, branch: &str) -> String {
    i18n::tf("cfg.ctx_branch", &[("profile", profile), ("branch", branch)])
}

fn validate_condition(profile: &str, c: &Condition) -> Result<(), String> {
    if c.id.trim().is_empty() {
        return Err(i18n::tf("cfg.cond_no_id", &[("name", profile)]));
    }
    if c.value.trim().is_empty() {
        return Err(i18n::tf("cfg.cond_empty_value", &[
            ("name", profile),
            ("id", &c.id),
            ("type", c.kind.as_str()),
        ]));
    }
    let v = c.value.trim();
    match c.kind {
        ConditionType::GatewayMac | ConditionType::Bssid => {
            if !looks_like_mac(v) {
                return Err(i18n::tf("cfg.cond_not_mac", &[
                    ("name", profile),
                    ("id", &c.id),
                    ("value", v),
                ]));
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
    let what1 = [("what", what)];
    if h.enabled {
        match h.mode {
            ProbeMode::Icmp if !has(&h.icmp_target) => {
                return Err(i18n::tf("cfg.health_icmp", &what1));
            }
            ProbeMode::Http if !has(&h.http_target) => {
                return Err(i18n::tf("cfg.health_http", &what1));
            }
            ProbeMode::Both if !has(&h.icmp_target) && !has(&h.http_target) => {
                return Err(i18n::tf("cfg.health_both", &what1));
            }
            _ => {}
        }
        if h.interval == 0 {
            return Err(i18n::tf("cfg.health_interval", &what1));
        }
        if h.retries == 0 {
            return Err(i18n::tf("cfg.health_retries", &what1));
        }
        if h.timeout == 0 {
            return Err(i18n::tf("cfg.health_timeout", &what1));
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
        return Err(i18n::tf("cfg.action_no_id", &[("what", what), ("kind", kind)]));
    }
    if seen.contains(&id) {
        return Err(i18n::tf("cfg.action_dup_id", &[("what", what), ("id", id)]));
    }
    seen.push(id);
    Ok(())
}

fn validate_branch(what: &str, b: &Branch) -> Result<(), String> {
    if let Some(n) = &b.network {
        validate_network(what, n)?;
    }
    validate_actions(what, &b.one_shot, &b.persistent)
}

/// 一个分支（或兜底）里的 3B 动作：载荷完整、id 唯一。
///
/// 与 [`validate_branch`] 拆开是因为兜底同样有 3B —— 它没有条件，但动作校验一条都不能少。
fn validate_actions(
    what: &str,
    one_shot: &[OneShotAction],
    persistent: &[PersistentAction],
) -> Result<(), String> {
    // 3B1 与 3B2 共用一套 id。动作结果（`EngineView.last_run`）是按 id 找回对应卡片的，
    // 撞名会让两条动作显示同一个成败；3B2 落地后那条更会成为串台。
    let mut ids: Vec<&str> = Vec::new();
    for a in one_shot {
        claim_action_id(what, &mut ids, &a.id, "one_shot")?;
        let missing = match &a.action {
            OneShotActionType::LaunchApp { app, .. } => {
                if app.trim().is_empty() {
                    Some(("launch_app", "app"))
                } else {
                    None
                }
            }
            OneShotActionType::RunScript { path, .. } => {
                if path.trim().is_empty() {
                    Some(("run_script", "path"))
                } else {
                    None
                }
            }
            OneShotActionType::SetDefaultPrinter { printer } => {
                if printer.trim().is_empty() {
                    Some(("set_default_printer", "printer"))
                } else {
                    None
                }
            }
        };
        if let Some((kind, field)) = missing {
            return Err(i18n::tf("cfg.action_no_field", &[
                ("what", what),
                ("id", &a.id),
                ("type", kind),
                ("field", field),
            ]));
        }
    }
    for a in persistent {
        claim_action_id(what, &mut ids, &a.id, "persistent")?;
        let (kind, interval) = match &a.action {
            PersistentActionType::PeriodicScript { path, interval_secs, .. } => {
                if path.trim().is_empty() {
                    return Err(i18n::tf("cfg.action_no_field", &[
                        ("what", what),
                        ("id", &a.id),
                        ("type", "periodic_script"),
                        ("field", "path"),
                    ]));
                }
                ("periodic_script", interval_secs)
            }
            PersistentActionType::KeepWireGuardConnected { tunnel, interval_secs } => {
                if tunnel.trim().is_empty() {
                    return Err(i18n::tf("cfg.action_no_field", &[
                        ("what", what),
                        ("id", &a.id),
                        ("type", "keep_wireguard_connected"),
                        ("field", "tunnel"),
                    ]));
                }
                ("keep_wireguard_connected", interval_secs)
            }
            PersistentActionType::KeepVpnConnected { provider, profile, interval_secs } => {
                if provider.trim().is_empty() || profile.trim().is_empty() {
                    return Err(i18n::tf("cfg.action_needs_both", &[
                        ("what", what),
                        ("id", &a.id),
                    ]));
                }
                ("keep_vpn_connected", interval_secs)
            }
        };
        // 常驻动作是一根循环轮询的定时器：间隔 0 在语义上就是「无限快地检查」。
        if *interval == 0 {
            return Err(i18n::tf("cfg.action_interval", &[
                ("what", what),
                ("id", &a.id),
                ("type", kind),
            ]));
        }
    }
    Ok(())
}

fn validate_network(what: &str, n: &NetworkConfig) -> Result<(), String> {
    let what1 = [("what", what)];
    if n.mode == Mode::Manual {
        for (field, val) in [
            ("ip", &n.ip),
            ("netmask", &n.netmask),
            ("gateway", &n.gateway),
        ] {
            if val.as_deref().unwrap_or("").trim().is_empty() {
                return Err(i18n::tf("cfg.net_manual_field", &[("what", what), ("field", field)]));
            }
        }
        if !n.ip.as_deref().map(is_ipv4).unwrap_or(false) {
            return Err(i18n::tf("cfg.net_bad_ip", &[
                ("what", what),
                ("ip", n.ip.as_deref().unwrap_or("")),
            ]));
        }
    }
    if let Some(dns) = &n.dns {
        let servers: Vec<&str> = dns
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        // 「自动获取 DNS」只有在这条分支本身就问 DHCP 要地址时才是一个真状态：静态地址下
        // 没有任何一方会递来 nameserver，清空的结果是**一个都拿不到**，而不是「自动」。
        // 三平台同构 —— macOS 的 `-setdnsservers … Empty` 就是系统设置里的「自动」，但它依赖
        // DHCP 客户端在跑；Windows 的 `source=dhcp` 同样要有租约可跟；NetworkManager 清掉
        // `ipv4.dns` 之后也只有 `method=auto` 才从租约里收 DNS。
        if servers.is_empty() && n.mode == Mode::Manual {
            return Err(i18n::tf("cfg.net_manual_auto_dns", &what1));
        }
        for t in servers {
            if !is_ipv4(t) {
                return Err(i18n::tf("cfg.net_bad_dns", &[("what", what), ("dns", t)]));
            }
        }
    }
    if n.v6mode == Some(V6Mode::Manual) && n.ipv6.as_deref().unwrap_or("").trim().is_empty() {
        return Err(i18n::tf("cfg.net_manual_v6", &what1));
    }
    // IPv6 强校验：仅「manual 缺地址」不足以拦截「地址/前缀/网关写成了不是 IPv6 的东西」。
    // 平台层（nmcli / networksetup / netsh）拿到非法值时的表现是各自为政的报错或静默忽略，
    // 而下发前在这里用 Ipv6Addr 语义拦下，报错才能指向配置文件里确切的那一行。
    if let Some(ip) = n.ipv6.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !is_ipv6(ip) {
            return Err(i18n::tf("cfg.net_bad_ipv6", &[("what", what), ("ip", ip)]));
        }
    }
    if let Some(prefix) = n.v6prefix.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match prefix.parse::<u32>() {
            Ok(p) if p <= 128 => {}
            _ => {
                return Err(i18n::tf("cfg.net_bad_v6prefix", &[
                    ("what", what),
                    ("prefix", prefix),
                ]));
            }
        }
    }
    if let Some(gw) = n.v6gateway.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !is_ipv6(gw) {
            return Err(i18n::tf("cfg.net_bad_v6gateway", &[("what", what), ("gateway", gw)]));
        }
    }
    // 冲突检测：同一 dest 出现两条 add 路由、但 gateway 不同，是「同一目的地两个下一跳」，
    // 平台层的选择序各不相同（先加的那条被后加的覆盖，或反过来）。与其让用户赌，
    // 不如在落盘前拦下 —— 他们此刻改一行就行，跑起来之后排查的是「为什么路由没生效」。
    let mut seen_routes: HashMap<String, String> = HashMap::new();
    for r in &n.routes {
        if r.dest.trim().is_empty() {
            return Err(i18n::tf("cfg.net_route_no_dest", &what1));
        }
        if !r.delete && r.gateway.as_deref().unwrap_or("").trim().is_empty() {
            return Err(i18n::tf("cfg.net_route_no_gw", &[
                ("what", what),
                ("dest", &r.dest),
            ]));
        }
        if !r.delete {
            let gw = r.gateway.as_deref().unwrap_or("").to_string();
            if let Some(prev) = seen_routes.get(r.dest.as_str()) {
                if prev != &gw {
                    return Err(i18n::tf("cfg.net_route_conflict", &[
                        ("what", what),
                        ("dest", &r.dest),
                        ("gw1", prev),
                        ("gw2", &gw),
                    ]));
                }
            } else {
                seen_routes.insert(r.dest.clone(), gw);
            }
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

/// 用标准库的 [`std::net::Ipv6Addr`] 做语义校验：`2001:db8::1` 合法、`not-an-ip` 非法。
fn is_ipv6(s: &str) -> bool {
    s.parse::<std::net::Ipv6Addr>().is_ok()
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

    /// 校验消息现在按界面语言生成，所以每条断言前都要保证字典已就位。
    ///
    /// 断言只挑**五种语言里都必须出现**的那部分：JSON 字段名、`then`/`else` 这类分支名、
    /// 以及用户自己写的 id。这正是这条消息的全部用处 —— 用户拿它去配置文件里找那一行，
    /// 所以「措辞可以翻译、锚点不许翻译」本身就是被测的契约。语言不由这里设定：
    /// `i18n` 的测试会并发改全局语言，任何一侧替另一侧定语言都会让结果看线程调度。
    fn dicts_ready() {
        i18n::init();
    }

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
        dicts_ready();
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
        dicts_ready();
        // 空值条件不是「通配」，而是配错了：放过去等于把整个 Rule 变成永不匹配
        let raw = r#"{"schema":1,"profiles":[{"id":"a","name":"A","rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":""}]}]}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        let err = cfg.validate().expect_err("空值条件必须被拒绝");
        assert!(err.contains("c1"), "报错要指向出问题的那条条件: {}", err);
    }

    /// 「判定需要哪几项身份读数」是从配置**反推**出来的，反推时只数启用的那些 ——
    /// 和求值侧同一条「禁用 ≠ 通配」。停用的条件要是也算进需求，闸门就会被一份根本不读的
    /// 条件武装起来：零命中每次都要多等 `UNIDENTIFIED_CONFIRMS` 轮才落成处置，而这份配置
    /// 什么时候都不需要那一项证据。
    #[test]
    fn decision_inputs_only_count_enabled_conditions() {
        dicts_ready();
        let cfg = Config::from_json(
            r#"{"schema":1,"profiles":[
              {"id":"a","name":"A","rules":[
                {"id":"r1","conditions":[
                  {"id":"c1","type":"wifi_ssid","value":"X"},
                  {"id":"c2","type":"gateway_mac","value":"AA:BB:CC:DD:EE:FF","enabled":false},
                  {"id":"c3","type":"network_interface","value":"en0"}]},
                {"id":"r2","enabled":false,"conditions":[
                  {"id":"c4","type":"bssid","value":"11:22:33:44:55:66"}]}]},
              {"id":"b","name":"B","enabled":false,"rules":[{"id":"r3","conditions":[
                  {"id":"c5","type":"gateway_mac","value":"11:22:33:44:55:66"}]}]}]}"#,
        )
        .unwrap();
        let need = cfg.decision_inputs();
        assert!(need.ssid, "启用条件里唯一要的读数就是 SSID");
        assert!(!need.gateway_mac, "停用条件与停用 Profile 都不构成需求");
        assert!(!need.bssid, "停用 Rule 里的条件同样不算");

        // 没有任何条件在场时，什么都判得动 —— 闸门不许变成「永远拦着」。
        let none = Config::default().decision_inputs();
        assert!(
            !none.ssid && !none.gateway_mac && !none.bssid,
            "没有启用条件时不得要求任何身份读数"
        );
    }

    #[test]
    fn manual_network_needs_full_ipv4_settings() {
        dicts_ready();
        let raw = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"network":{"mode":"manual","ip":"10.0.0.1"}}}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        let err = cfg.validate().expect_err("manual 缺 netmask/gateway 必须报错");
        assert!(err.contains("netmask"), "报错内容: {}", err);
    }

    /// 「自动获取 DNS」要有 DHCP 客户端在跑才有东西可跟，所以它只在 `mode: dhcp` 那条分支上是
    /// 一个可达成的状态。静态地址 + 空 `dns` 下发出去是「一个 nameserver 都没有」，而现场长得
    /// 像网络坏了 —— 这种配置不该等到跑起来才发现。
    #[test]
    fn automatic_dns_is_only_an_option_on_the_dhcp_branch() {
        dicts_ready();
        let manual_auto = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"network":{"mode":"manual","ip":"10.0.0.1","netmask":"255.255.255.0",
            "gateway":"10.0.0.254","dns":""}}}]}"#;
        let err = Config::from_json(manual_auto)
            .unwrap()
            .validate()
            .expect_err("静态地址 + 自动 DNS 必须被拒绝");
        // 断言用两个字面量而不是整句英文：报错要在五语下都指得出「是 dns 这一半和 manual 这一半
        // 撞了」，而这条配置其余字段都齐全，别的规则都不会先响。
        assert!(
            err.contains("dns") && err.contains("mode=manual"),
            "报错要同时点名 dns 与 mode=manual: {}",
            err
        );

        // 只剩分隔符的值在拆分后同样是空的，走的是同一条清空路径，就得撞同一道闸。
        let noisy = manual_auto.replace("\"dns\":\"\"", "\"dns\":\" , \"");
        assert!(
            Config::from_json(&noisy).unwrap().validate().is_err(),
            "只剩分隔符的 dns 也是一次清空，同样要拦"
        );

        // 反面必须放行，否则这条闸门就是在拦掉正确写法：
        // 兜底用的就是 `mode: dhcp` + `dns: ""`（`-setdhcp` 不会自己带走上一个环境的 DNS）。
        let dhcp_auto = r#"{"schema":1,"profiles":[],
          "fallback":{"network":{"mode":"dhcp","dns":""}}}"#;
        Config::from_json(dhcp_auto)
            .unwrap()
            .validate()
            .expect("dhcp + 空 dns 才是「交回自动获取」的写法");

        // 静态 + 明确指定 / 静态 + 键缺失（别碰 DNS）都照旧合法。两份都在上面那份已被接受的
        // 配置上改一处得来，避免手抄括号数把「合法用例」测成解析失败。
        for (raw, why) in [
            (
                manual_auto.replace("\"dns\":\"\"", "\"dns\":\"10.0.0.53\""),
                "静态 + 指定 DNS",
            ),
            (
                manual_auto.replace(",\"dns\":\"\"", ""),
                "静态 + 键缺失（不动 DNS）",
            ),
        ] {
            Config::from_json(&raw)
                .unwrap_or_else(|e| panic!("{why} 必须能解析: {e}"))
                .validate()
                .unwrap_or_else(|e| panic!("{why} 必须通过校验: {e}"));
        }
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

    /// 同一 dest 配了两条 add 路由、但 gateway 不同，是「一个目的地两个下一跳」。
    /// 平台层谁覆盖谁不确定，与其让用户赌，不如落盘前拦下；相同 gateway 的重复则不拦（幂等）。
    #[test]
    fn conflicting_routes_share_a_dest_with_different_gateways() {
        dicts_ready();
        let conflict = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"network":{"mode":"dhcp","routes":[
            {"dest":"10.0.0.0/8","gateway":"192.168.1.1"},
            {"dest":"10.0.0.0/8","gateway":"192.168.1.2"}]}}}]}"#;
        let cfg = Config::from_json(conflict).unwrap();
        let err = cfg.validate().expect_err("同 dest 不同 gateway 必须被拒绝");
        assert!(err.contains("10.0.0.0/8"), "报错要点名冲突的目的地: {}", err);

        // 相同 dest + 相同 gateway 是幂等重复，放行。
        let dup = conflict.replace("192.168.1.2", "192.168.1.1");
        Config::from_json(&dup)
            .unwrap()
            .validate()
            .expect("同 dest 同 gateway 只是重复，不该被拦");
    }

    /// IPv6 三个字段（ipv6 / v6prefix / v6gateway）都要过 `Ipv6Addr` / 0~128 的语义校验：
    /// 平台层拿到非法值时的表现各不相同，下发前在这里拦下，报错才能指向确切那一行。
    #[test]
    fn ipv6_fields_are_validated_semantically() {
        dicts_ready();
        let v6_net = |fields: &str| {
            format!(
                r#"{{"schema":1,"profiles":[{{"id":"a","name":"A",
                  "rules":[{{"id":"r1","conditions":[{{"id":"c1","type":"wifi_ssid","value":"X"}}]}}],
                  "then":{{"network":{{"mode":"manual","ip":"10.0.0.1","netmask":"255.255.255.0",
                    "gateway":"10.0.0.254","v6mode":"manual","ipv6":"2001:db8::1",
                    "v6prefix":"64","v6gateway":"2001:db8::1"{}}}}}}}]}}"#,
                fields
            )
        };

        // 反面：合法的 IPv6 配置必须放行。
        Config::from_json(&v6_net(""))
            .unwrap()
            .validate()
            .expect("合法 IPv6 配置必须通过校验");

        // 反面：坏地址。

        let bad_ip = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"network":{"mode":"manual","ip":"10.0.0.1","netmask":"255.255.255.0",
            "gateway":"10.0.0.254","v6mode":"manual","ipv6":"not-an-ip","v6prefix":"64"}}}]}"#;
        let err = Config::from_json(bad_ip).unwrap().validate().expect_err("坏 IPv6 必须被拒绝");
        assert!(err.contains("not-an-ip"), "报错要点名坏地址: {}", err);

        // 反面：前缀超出 0~128。
        let bad_prefix = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"network":{"mode":"manual","ip":"10.0.0.1","netmask":"255.255.255.0",
            "gateway":"10.0.0.254","v6mode":"manual","ipv6":"2001:db8::1","v6prefix":"200"}}}]}"#;
        let err = Config::from_json(bad_prefix).unwrap().validate().expect_err("超范围前缀必须被拒绝");
        assert!(err.contains("v6prefix"), "报错要指向 v6prefix: {}", err);

        // 反面：坏网关地址。
        let bad_gw = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"network":{"mode":"manual","ip":"10.0.0.1","netmask":"255.255.255.0",
            "gateway":"10.0.0.254","v6mode":"manual","ipv6":"2001:db8::1","v6prefix":"64",
            "v6gateway":"999.999.999.999"}}}]}"#;
        let err = Config::from_json(bad_gw).unwrap().validate().expect_err("坏网关必须被拒绝");
        assert!(err.contains("999.999.999.999"), "报错要点名坏网关: {}", err);
    }

    /// 一条动作载荷配错了，跑起来的表现是「什么也没发生」，比加载失败更难排查 ——
    /// 所以空 app / 空 path / 空 printer / 空 tunnel / 0 间隔都在落盘前拦住。
    #[test]
    fn action_payloads_must_be_complete() {
        dicts_ready();
        let cases = [
            (r#"{"type":"launch_app","app":""}"#, "app"),
            (r#"{"type":"run_script","path":"  "}"#, "path"),
            (r#"{"type":"set_default_printer","printer":""}"#, "printer"),
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
        dicts_ready();
        let raw = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"persistent":[{"id":"p1","action":{"type":"periodic_script","path":"scripts/k.sh","interval_secs":0}}]}}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        assert!(cfg.validate().unwrap_err().contains("interval_secs"));
    }

    /// 3B1 与 3B2 共用一套 id：动作结果按 id 找回卡片，跨列表撞名同样会串台。
    #[test]
    fn one_shot_and_persistent_share_one_id_namespace() {
        dicts_ready();
        let raw = r#"{"schema":1,"profiles":[{"id":"a","name":"A",
          "rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}],
          "then":{"one_shot":[{"id":"x","action":{"type":"launch_app","app":"Notes.app"}}],
                  "persistent":[{"id":"x","action":{"type":"keep_wireguard_connected","tunnel":"wg0"}}]}}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        let err = cfg.validate().expect_err("跨列表撞 id 必须被拒绝");
        assert!(err.ends_with("x"), "报错要以撞名的那个 id 收尾（五种语言都是）: {}", err);
    }

    /// 健康度探测关着时不校验内容（先把参数写好再打开是合法用法）；
    /// 开着却没目标 / 零间隔必须拦住 —— 那种配置会在 Active 期间反复触发回落 DHCP。
    #[test]
    fn enabled_health_probe_needs_a_target_and_positive_timing() {
        dicts_ready();
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
        dicts_ready();
        let raw = r#"{"schema":1,"profiles":[
          {"id":"a","name":"A","rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}]},
          {"id":"a","name":"B","rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"Y"}]}]}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.ends_with("a"), "报错要以撞名的那个 id 收尾: {}", err);
    }

    /// 兜底的 3B 现在真的会跑（`engine::apply_fallback`），所以它的动作载荷必须
    /// 和分支里的过同一道校验 —— 「配了却什么都没发生」的来源不分分支还是兜底。
    #[test]
    fn fallback_actions_go_through_the_same_validation() {
        dicts_ready();
        let raw = r#"{"schema":1,"profiles":[],
          "fallback":{"network":{"mode":"dhcp"},
          "one_shot":[{"id":"o1","action":{"type":"launch_app","app":""}}]}}"#;
        let cfg = Config::from_json(raw).unwrap();
        let err = cfg.validate().expect_err("兜底的空动作载荷必须被拒绝");
        assert!(err.contains("app") && err.contains("fallback"), "报错内容: {}", err);

        let raw = r#"{"schema":1,"profiles":[],
          "fallback":{"persistent":[{"id":"x","action":{"type":"keep_wireguard_connected","tunnel":""}}]}}"#;
        let cfg = Config::from_json(raw).unwrap();
        let err = cfg.validate().expect_err("兜底的常驻动作同样要校验");
        assert!(err.contains("tunnel"), "报错内容: {}", err);

        // 3B1 与 3B2 在兜底里也共用一套 id
        let raw = r#"{"schema":1,"profiles":[],
          "fallback":{"one_shot":[{"id":"x","action":{"type":"launch_app","app":"Notes.app"}}],
                      "persistent":[{"id":"x","action":{"type":"keep_wireguard_connected","tunnel":"wg0"}}]}}"#;
        let cfg = Config::from_json(raw).unwrap();
        let err = cfg.validate().expect_err("兜底跨列表撞 id 必须被拒绝");
        assert!(err.ends_with("x"), "报错内容: {}", err);
    }

    /// `__fallback__` 是引擎为兜底合成身份预留的 id：真 Profile 占了它，
    /// 运行留痕与 worker 归属就会把两件事说成一件。
    #[test]
    fn the_fallback_id_is_reserved_for_the_synthetic_identity() {
        dicts_ready();
        let raw = r#"{"schema":1,"profiles":[
          {"id":"__fallback__","name":"F","rules":[{"id":"r1","conditions":[{"id":"c1","type":"wifi_ssid","value":"X"}]}]}]}"#;
        let cfg = Config::from_json(raw).unwrap();
        let err = cfg.validate().expect_err("保留 id 必须被拒绝");
        assert!(err.contains(FALLBACK_ID), "报错要带上那个 id: {}", err);
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
            {"id":"a1","enabled":true,"action":{"type":"launch_app","app":"/Applications/X.app"}},
            {"id":"a2","enabled":true,"action":{"type":"run_script","path":"scripts/x.sh"}},
            {"id":"a4","enabled":true,"action":{"type":"set_default_printer","printer":"HP OfficeJet 476"}}],
            "persistent":[
            {"id":"p1","enabled":true,"action":{"type":"periodic_script","path":"scripts/keep.sh","interval_secs":10}},
            {"id":"p2","enabled":true,"action":{"type":"keep_wireguard_connected","tunnel":"wg0","interval_secs":15}},
            {"id":"p3","enabled":true,"action":{"type":"keep_vpn_connected","provider":"globalprotect","profile":"corp","interval_secs":20}}]}}]}"#;
        let cfg = Config::from_json(raw).expect("编辑器产出的动作标签必须能反序列化");
        cfg.validate().expect("同一份配置必须通过校验");
        let p = cfg.profile_by_id("a").unwrap();
        let then = p.then.as_ref().unwrap();
        assert_eq!(then.one_shot.len(), 3);
        assert!(matches!(
            then.one_shot[2].action,
            model::OneShotActionType::SetDefaultPrinter { ref printer } if printer == "HP OfficeJet 476"
        ));
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
    /// 现在直接在 validate 中拒绝，不再只报 warning。
    #[test]
    fn persistent_actions_on_the_else_branch_are_rejected() {
        dicts_ready();
        let wg = |branch: &str| {
            format!(
                r#"{{"schema":1,"profiles":[{{"id":"a","name":"A",
                  "rules":[{{"id":"r1","conditions":[{{"id":"c1","type":"wifi_ssid","value":"X"}}]}}],
                  "{}":{{"persistent":[{{"id":"p1","action":{{"type":"keep_wireguard_connected","tunnel":"wg0"}}}}]}}}}]}}"#,
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
        assert!(
            els.validate().is_err(),
            "else 分支的常驻动作必须被拒绝: {:?}",
            els.validate()
        );
        assert!(
            els.warnings().is_empty(),
            "else 分支的常驻动作已被 validate 拒绝，不该再报 warning: {:?}",
            els.warnings()
        );
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
