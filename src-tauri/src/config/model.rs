//! 配置数据模型（schema 1）。
//!
//! 这份模型的三条不变量，容易被无意破坏，集中写在这里：
//!
//! 1. **Profile 之间没有 priority**。多个 Profile 同时命中时不「自动挑一个」，而是
//!    `Conflict` 且不自动应用任何一个（见 `engine`）。定序只存在于动作的
//!    `priority` 上，不存在于 Profile 之间。
//! 2. **条件是「Rules × Conditions」两层**：Rule 之间是 OR，
//!    Rule 内**已启用**的 Condition 之间是 AND。两者都可单独禁用（`enabled`），
//!    禁用的 Condition 不参与 AND、也永远不算命中。
//! 3. **动作分成 3A / 3B1 / 3B2 三层，且各有 THEN / ELSE 两支**。3A（网络配置 +
//!    静态路由 + 校验）是 3B 的硬屏障：3A 失败则 3B 一条都不执行。
//!
//! 零命中时的网络处置是顶层 `fallback`：它不是一个可匹配的
//! Profile（它没有条件），因此不该占用 Profile 名额。

use serde::{Deserialize, Serialize};

/// 本版本认识的配置 schema。
pub const SCHEMA: u32 = 1;

/// 顶层配置。落盘为 `config.json`。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    /// schema 版本；**必须**等于 [`SCHEMA`]，缺失也视为不合法（见 `Config::load`）。
    #[serde(default)]
    pub schema: u32,
    /// 当前 UI 语言（zh/en/zh-TW/ja/ko）；缺省回退 en。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// 有序：托盘与设置面板按此顺序展示，不再有「谁优先级高」的隐含排序。
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// 零命中时的网络处置（不是 Profile，故不参与匹配与冲突判定）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<FallbackConfig>,
    /// 脚本 allow-list（绝对路径）。它是**安全边界**而不是某个环境的属性，所以在顶层：
    /// 挂在某一份 Profile 的动作下，改来改去总会漏掉某一支。
    #[serde(default, rename = "allowed_scripts", skip_serializing_if = "Vec::is_empty")]
    pub allowed_scripts: Vec<String>,
}

/// 零命中时的处置。`network: None` = 什么都不做（保持现状）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FallbackConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
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

fn default_true() -> bool {
    true
}
fn default_priority() -> u32 {
    100
}
fn default_interval() -> u64 {
    30
}
fn default_retries() -> u32 {
    3
}
fn default_timeout() -> u64 {
    5
}

// —————————————————————————————— Profile ——————————————————————————————

/// 一个网络环境（Home / Office / Public / …）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Profile {
    /// 稳定标识：`enabled` 之外的所有跨进程引用（立即应用、状态上报）都按 id 找，
    /// 改名不会把状态指到别的 Profile 上。
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 检测方式属于**每个 Profile 自己**：家里希望切了 SSID 立刻生效，
    /// 办公室那套静态 IP 宁可多等几秒也不要漫游抖动时反复下发。
    #[serde(default)]
    pub detection: DetectionConfig,
    /// Rule 之间是 OR。
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// 命中时执行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then: Option<Branch>,
    /// 「本 Profile 被选中但条件不成立」时执行 —— 只有正在处理的那个 Profile 会走
    /// ELSE，其它不匹配的 Profile 不会被连带执行（见 `engine`）。
    #[serde(default, rename = "else", skip_serializing_if = "Option::is_none")]
    pub else_branch: Option<Branch>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            detection: DetectionConfig::default(),
            rules: Vec::new(),
            then: None,
            else_branch: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionMode {
    /// 只在网络状态变化时重新评估。
    #[default]
    NetworkEvents,
    /// 变化立即评估，同时周期性重新评估。
    NetworkEventsAndPolling,
    /// 不依赖变化事件，仅周期性评估。
    PollingOnly,
}

/// Profile 的检测节律。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DetectionConfig {
    #[serde(default)]
    pub mode: DetectionMode,
    /// 网络变化后再等这么久才评估（漫游抖动 / DHCP 还没发完地址的窗口）。
    #[serde(default = "default_change_delay", rename = "change_delay_secs")]
    pub change_delay_secs: u64,
    /// 周期评估间隔；`NetworkEvents` 模式下忽略。
    #[serde(default = "default_interval", rename = "poll_interval_secs")]
    pub poll_interval_secs: u64,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            mode: DetectionMode::NetworkEvents,
            change_delay_secs: 5,
            poll_interval_secs: 30,
        }
    }
}

fn default_change_delay() -> u64 {
    5
}

// ———————————————————————————— Rules / Conditions ————————————————————————————

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Rule {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// 一条条件的种类。第一版只有这四种，全部是「与当前网络的某个标识做字符串比较」。
/// 新增种类只需在此加变体 + 在 `conditions::evaluator` 里加一个比较分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionType {
    /// 在用网卡设备名（en0 / Wi-Fi / wlan0）
    #[default]
    NetworkInterface,
    /// 默认网关的 MAC
    GatewayMac,
    /// 当前关联的 SSID
    WifiSsid,
    /// 当前 AP 的 BSSID
    Bssid,
}

impl ConditionType {
    /// 与 serde 表示同名的稳定字符串，供 UI 与日志使用。
    pub fn as_str(self) -> &'static str {
        match self {
            ConditionType::NetworkInterface => "network_interface",
            ConditionType::GatewayMac => "gateway_mac",
            ConditionType::WifiSsid => "wifi_ssid",
            ConditionType::Bssid => "bssid",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Condition {
    pub id: String,
    /// 禁用 ≠ 通配：禁用的条件**永远不算命中**（见 `conditions::evaluator`）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(rename = "type")]
    pub kind: ConditionType,
    /// 比较值。MAC 类条件两侧都会归一化（大小写与 `:`/`-` 分隔符差异）。
    pub value: String,
}

// —————————————————————————————— 3A / 3B ——————————————————————————————

/// 一个分支（THEN 或 ELSE）里要做的事。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Branch {
    /// 3A：硬性执行阶段，先于 3B，且失败即阻断 3B。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,
    /// 3B1：一次性动作，按 priority 分批。
    #[serde(default, rename = "one_shot", skip_serializing_if = "Vec::is_empty")]
    pub one_shot: Vec<OneShotAction>,
    /// 3B2：常驻动作（维持某个期望状态），按 priority 决定启动顺序。
    #[serde(default, rename = "persistent", skip_serializing_if = "Vec::is_empty")]
    pub persistent: Vec<PersistentAction>,
}

/// 3A 网络配置。字段名被平台层直接消费，改动它们要同步三份实现。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct NetworkConfig {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub netmask: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    /// 逗号分隔的 IPv4 DNS。**三态**：缺失 = 不下发任何 DNS 操作（沿用系统现在的），
    /// 空串 = 显式清空、交回系统自动获取，非空 = 就用这几台。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<String>,
    #[serde(default, rename = "v6mode", skip_serializing_if = "Option::is_none")]
    pub v6mode: Option<V6Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<String>,
    #[serde(default, rename = "v6prefix", skip_serializing_if = "Option::is_none")]
    pub v6prefix: Option<String>,
    #[serde(default, rename = "v6gateway", skip_serializing_if = "Option::is_none")]
    pub v6gateway: Option<String>,
    /// 静态路由属于 3A 而不是一种自动化动作：那样「路由没加上」也能让整支自动化
    /// 跑下去。放在这里它才受 Apply + Verify（回读比对）约束。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<RouteConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifyConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouteConfig {
    pub dest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    #[serde(default)]
    pub metric: u32,
    /// true = 撤销该路由（用于「离开某环境」的 ELSE 分支）。
    #[serde(default)]
    pub delete: bool,
}

/// 3A 的校验策略。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct VerifyConfig {
    /// 下发后回读实际参数并与期望比对。这是 3A 的**基本**校验，默认开。
    #[serde(default = "default_true")]
    pub readback: bool,
    /// 主动探测（ICMP/HTTP）+ 连续失败时的处置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthConfig>,
}

/// 健康度探测：既是 3A 的可选校验手段，也是 Active 期间的持续监测。
/// 连续失败 `retries` 次且 `fallback.enabled` → 回落 DHCP（见 `network`）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub fallback: FallbackProbe,
    #[serde(default)]
    pub mode: ProbeMode,
    #[serde(default, rename = "http_target", skip_serializing_if = "Option::is_none")]
    pub http_target: Option<String>,
    #[serde(default, rename = "icmp_target", skip_serializing_if = "Option::is_none")]
    pub icmp_target: Option<String>,
    #[serde(default = "default_interval")]
    pub interval: u64,
    #[serde(default = "default_retries")]
    pub retries: u32,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fallback: FallbackProbe::default(),
            mode: ProbeMode::Both,
            http_target: None,
            icmp_target: None,
            interval: 30,
            retries: 3,
            timeout: 5,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FallbackProbe {
    #[serde(default)]
    pub enabled: bool,
}

/// 3B1：一次性动作。每次进入 Active 跑一次；保持 Active 期间的重新评估**不会**重跑。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OneShotAction {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 数字越小越先执行；相同数字在同一批里并发执行。
    #[serde(default = "default_priority")]
    pub priority: u32,
    pub action: OneShotActionType,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OneShotActionType {
    LaunchApp {
        app: String,
        #[serde(default)]
        args: Vec<String>,
    },
    RunScript {
        path: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        elevated: bool,
    },
}

/// 3B2：常驻动作 —— 不是「每隔 N 秒重复执行命令」，而是**持续维护一个期望状态**：
/// 每 N 秒检查一次，已满足就什么都不做，不满足才恢复。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PersistentAction {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 只决定 worker 的**启动顺序**；worker 起来后独立运行，不会阻塞后续 priority。
    #[serde(default = "default_priority")]
    pub priority: u32,
    pub action: PersistentActionType,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PersistentActionType {
    PeriodicScript {
        path: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default = "default_interval", rename = "interval_secs")]
        interval_secs: u64,
    },
    // 显式改名：serde 的 snake_case 会把 WireGuard 拆成 `wire_guard`。
    #[serde(rename = "keep_wireguard_connected")]
    KeepWireGuardConnected {
        tunnel: String,
        #[serde(default = "default_interval", rename = "interval_secs")]
        interval_secs: u64,
    },
    KeepVpnConnected {
        provider: String,
        profile: String,
        #[serde(default = "default_interval", rename = "interval_secs")]
        interval_secs: u64,
    },
}
