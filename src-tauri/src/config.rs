//! 配置模型（serde）— 对应 config.example.json 的 schema。

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Dhcp,
    Manual,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum V6Mode {
    #[default]
    Automatic,
    Manual,
    Off,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProbeMode {
    #[default]
    Both,
    Icmp,
    Http,
}

/// 独立匹配条件：三者皆可单独使用，未声明=通配，声明项全部成立(AND)才算命中。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct MatchConditions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    #[serde(rename = "gateway_mac", skip_serializing_if = "Option::is_none")]
    pub gateway_mac: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bssid: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct FallbackConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub fallback: FallbackConfig,
    #[serde(default)]
    pub mode: ProbeMode,
    #[serde(rename = "http_target", skip_serializing_if = "Option::is_none")]
    pub http_target: Option<String>,
    #[serde(rename = "icmp_target", skip_serializing_if = "Option::is_none")]
    pub icmp_target: Option<String>,
    #[serde(default = "default_u64_30")]
    pub interval: u64,
    #[serde(default = "default_u32_3")]
    pub retries: u32,
    #[serde(default = "default_u64_5")]
    pub timeout: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fallback: FallbackConfig::default(),
            mode: ProbeMode::Both,
            http_target: None,
            icmp_target: None,
            interval: 30,
            retries: 3,
            timeout: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AutomationAction {
    Route {
        dest: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        gateway: Option<String>,
        #[serde(default)]
        metric: u32,
        #[serde(default)]
        delete: bool,
    },
    Launch {
        app: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Run {
        path: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        elevated: bool,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AutomationConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 允许脚本的绝对路径白名单；配合 scripts/ 受信目录共同构成 allow-list。
    #[serde(default, rename = "allowed_scripts")]
    pub allowed_scripts: Option<Vec<String>>,
    #[serde(default, rename = "on_apply")]
    pub on_apply: Vec<AutomationAction>,
    #[serde(default, rename = "on_revert")]
    pub on_revert: Vec<AutomationAction>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Profile {
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_cond: Option<MatchConditions>,
    #[serde(default)]
    pub mode: Mode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub netmask: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dns: Option<String>,
    #[serde(rename = "v6mode", skip_serializing_if = "Option::is_none")]
    pub v6mode: Option<V6Mode>,
    #[serde(rename = "ipv6", skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<String>,
    #[serde(rename = "v6prefix", skip_serializing_if = "Option::is_none")]
    pub v6prefix: Option<String>,
    #[serde(rename = "v6gateway", skip_serializing_if = "Option::is_none")]
    pub v6gateway: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automation: Option<AutomationConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    /// 当前 UI 语言（isi18n 语言码：zh/en/zh-TW/ja/ko）；缺省回退 en。
    #[serde(default, rename = "language", skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// key = SSID 或自定义名；__DEFAULT__ 为全局回退
    #[serde(flatten)]
    pub profiles: std::collections::HashMap<String, Profile>,
}

fn default_u64_30() -> u64 { 30 }
fn default_u32_3() -> u32 { 3 }
fn default_u64_5() -> u64 { 5 }

impl Config {
    /// 读取配置（不会校验，调用方应随后 validate）
    pub fn load(path: &Path) -> Result<Config, String> {
        let data = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let cfg: Config = serde_json::from_str(&data).map_err(|e| e.to_string())?;
        Ok(cfg)
    }

    /// 写回磁盘（pretty JSON）
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, s).map_err(|e| e.to_string())
    }

    pub fn default_profile(&self) -> Option<&Profile> {
        self.profiles.get("__DEFAULT__")
    }

    /// 校验：非 __DEFAULT__ 必须声明 ≥1 match 条件；manual 模式需 ip/netmask/gateway；DNS 需为合法 IPv4。
    pub fn validate(&self) -> Result<(), String> {
        for (name, p) in &self.profiles {
            if name == "__DEFAULT__" {
                continue;
            }
            let m = p
                .match_cond
                .as_ref()
                .ok_or_else(|| format!("profile '{}' 缺少 match 条件", name))?;
            if m.ssid.is_none() && m.gateway_mac.is_none() && m.bssid.is_none() {
                return Err(format!(
                    "profile '{}' 的 match 至少需声明 ssid / gateway_mac / bssid 之一",
                    name
                ));
            }
            if p.mode == Mode::Manual {
                if p.ip.is_none() || p.netmask.is_none() || p.gateway.is_none() {
                    return Err(format!(
                        "profile '{}' 为 manual 模式，需填写 ip / netmask / gateway",
                        name
                    ));
                }
            }
            if let Some(dns) = &p.dns {
                for part in dns.split(',') {
                    let t = part.trim();
                    if t.is_empty() {
                        continue;
                    }
                    if !is_ipv4(t) {
                        return Err(format!("profile '{}' DNS 不合法: {}", name, t));
                    }
                }
            }
        }
        Ok(())
    }
}

fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| p.parse::<u8>().map(|_| true).unwrap_or(false))
}
