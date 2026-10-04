//! 当前网络的身份快照。
//!
//! 它是条件求值的**唯一**输入：一次采样、多处消费（评估、UI 的「Current Network」、
//! 变化检测）。
//!
//! ⚠️ 构造一次 [`NetworkSnapshot::sample`] 会拉起若干子进程（macOS 上最坏情况含一次
//! 1~4s 的 `system_profiler`，平台层内有 TTL 缓存兜着）。因此**绝不能**在求值循环里
//! 反复采样 —— 由 `detection` 的采样线程定期刷新，其余地方只读最近一份。

use crate::platform::{normalize_mac, NicInfo, NicKind, NetworkPlatform};
use serde::Serialize;

/// 判定当前配置需要快照里的哪几项身份读数。
///
/// 它由配置反推而来（见 `Config::decision_inputs`）：只统计**启用**的 Profile 里
/// **启用**的条件用了哪些类型。默认值取「三项都要」—— 还不知道配置要什么的时候，
/// 宁可认作「判不动」，也不要拿一份残缺快照去拆现网。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecisionInputs {
    pub ssid: bool,
    pub gateway_mac: bool,
    pub bssid: bool,
}

impl DecisionInputs {
    /// 「还不知道配置要读什么」时用的那一份：三项全要。这是保守的一侧 —— 判不出
    /// 「判得动」，引擎就不会动现网。
    pub const EVERYTHING: Self = Self {
        ssid: true,
        gateway_mac: true,
        bssid: true,
    };
}

impl Default for DecisionInputs {
    fn default() -> Self {
        Self::EVERYTHING
    }
}

/// 一次采样的结果。MAC 字段一律已归一化为小写冒号形态，下游只比字符串。
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct NetworkSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_mac: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bssid: Option<String>,
    /// 走默认路由的那张网卡名（`primary_iface()` 的结果）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_interface: Option<String>,
    /// 当前**在用**的普通网卡名（已小写、已排除 VPN 隧道）。
    /// `network_interface` 条件比对的是这个集合，而不是「主网卡」——
    /// 有线 + 无线同时插着时，两张都算「在」。
    pub interfaces: Vec<String>,
    /// VPN 隧道网卡名（小写）。第一版不作为条件，仅进 UI 展示与指纹。
    pub tunnels: Vec<String>,
}

impl NetworkSnapshot {
    /// 从平台层采一份快照。
    pub fn sample<P: NetworkPlatform>(plat: &P) -> Self {
        let st = plat.get_status();
        let nics = plat.list_interfaces();
        let mut interfaces = Vec::new();
        let mut tunnels = Vec::new();
        for n in &nics {
            if !nic_is_up(n) {
                continue;
            }
            let name = n.name.to_ascii_lowercase();
            if n.kind == NicKind::Vpn {
                tunnels.push(name);
            } else {
                interfaces.push(name);
            }
        }
        interfaces.sort();
        tunnels.sort();
        let norm = |v: Option<String>| v.map(|s| normalize_mac(&s));
        Self {
            ssid: st.ssid.clone().or_else(|| plat.get_current_ssid()),
            gateway_mac: norm(st.gateway_mac.clone()),
            bssid: norm(st.bssid.clone()),
            primary_interface: primary_iface(&nics).map(|n| n.name.to_ascii_lowercase()),
            interfaces,
            tunnels,
        }
    }

    /// 这一份快照能不能**判得动**当前配置里的那些条件。
    ///
    /// 判据不是「读到过任何一项身份」，而是「条件要读的那几项都读到了」：三项身份字段
    /// 里任何一项有值，都撑不起一个「一个 Profile 都没命中」的结论 —— Wi-Fi 打盹的那
    /// 一轮，SSID 从 CoreWLAN 拿不到（命令行在 Sequoia 上全部涂黑），网关 MAC 却还能从
    /// ARP 表里读到旧的那一条，于是「唯一条件是 SSID」的 Profile 被判成不匹配，兜底当场
    /// 把正在工作的静态 IP 改成自动获取。反过来，纯有线的机器上 SSID 恒为空，那也确实
    /// 判不出任何 SSID 型条件 —— 由 `Engine` 那边连续 N 轮之后再放行。
    ///
    /// 引擎用它拦住「零命中 → 撤掉现网」那条处置，见 `engine::decide_plan`。
    pub fn can_decide(&self, needs: DecisionInputs) -> bool {
        (!needs.ssid || self.ssid.is_some())
            && (!needs.gateway_mac || self.gateway_mac.is_some())
            && (!needs.bssid || self.bssid.is_some())
    }

    /// 变化检测用的指纹。
    ///
    /// 为什么不能只比 SSID：网关 MAC 变了意味着换了一台路由器（同一
    /// SSID 的 5G/2.4G 双频、或 Mesh 节点），BSSID 变了意味着漫游到了另一个 AP，
    /// 默认路由换了网卡意味着「从 Wi-Fi 切到有线」—— 只比 SSID 时这三种变化都**不**
    /// 会触发重新匹配，于是网关 MAC / BSSID 型条件形同虚设。
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}",
            self.ssid.as_deref().unwrap_or(""),
            self.gateway_mac.as_deref().unwrap_or(""),
            self.bssid.as_deref().unwrap_or(""),
            self.primary_interface.as_deref().unwrap_or(""),
            self.interfaces.join(","),
            self.tunnels.join(","),
        )
    }
}

pub(crate) fn nic_is_up(n: &NicInfo) -> bool {
    n.up || n.ipv4.is_some()
}

/// 主网卡 = 走默认路由的那张（排除 VPN 隧道）；没有默认路由时退回第一张非 VPN 网卡。
fn primary_iface(nics: &[NicInfo]) -> Option<&NicInfo> {
    let live = || nics.iter().filter(|n| n.kind != NicKind::Vpn && nic_is_up(n));
    live()
        .find(|n| n.gateway.is_some())
        .or_else(|| nics.iter().find(|n| n.kind != NicKind::Vpn && nic_is_up(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(ssid: Option<&str>, gw: Option<&str>, ifaces: &[&str]) -> NetworkSnapshot {
        NetworkSnapshot {
            ssid: ssid.map(|s| s.to_string()),
            gateway_mac: gw.map(|s| s.to_string()),
            bssid: None,
            primary_interface: ifaces.first().map(|s| s.to_string()),
            interfaces: ifaces.iter().map(|s| s.to_string()).collect(),
            tunnels: Vec::new(),
        }
    }

    #[test]
    fn a_sample_without_any_identity_field_cannot_decide_anything() {
        // 现场在线（en0 仍在网卡集合里），但 SSID / 网关 MAC / BSSID 一个都没读到。
        // 这一份不是「换到了一个陌生网络」的观察，而是身份读取失败 —— 拿它判零命中，
        // 兜底就会把正在工作的静态配置拆掉（回落 DHCP、清空 DNS）。
        let s = snap(None, None, &["en0"]);
        assert!(
            !s.can_decide(DecisionInputs {
                ssid: true,
                gateway_mac: false,
                bssid: false
            }),
            "条件要读 SSID，而这份快照里没有它"
        );
        assert!(
            s.fingerprint().contains("en0"),
            "网卡集合照常进指纹：身份没读到不等于什么都没读到"
        );
    }

    #[test]
    fn an_unread_ssid_does_not_become_a_strange_network_just_because_the_arp_table_has_one() {
        // Wi-Fi 打盹的那一轮：CoreWLAN 报不出 SSID，ARP 表里旧网关的 MAC 却还挂着。
        // 「读到过任何一项身份」在这里成立，而它根本判不动一条 SSID 型条件 —— 这正是
        // 现网被误拆的那条路，所以判据按条件要读的那几项来。
        let s = snap(None, Some("aa:bb:cc:dd:ee:ff"), &["en0"]);
        assert!(
            !s.can_decide(DecisionInputs {
                ssid: true,
                gateway_mac: false,
                bssid: false
            }),
            "只有 SSID 型条件在场时，读不到 SSID 就是判不动"
        );
        // 同网关 MAC 才是网关 MAC 型条件要的证据，那份证据这里确实读到了
        assert!(s.can_decide(DecisionInputs {
            ssid: false,
            gateway_mac: true,
            bssid: false
        }));
    }

    #[test]
    fn each_identity_condition_type_needs_its_own_reading() {
        let only_ssid = DecisionInputs {
            ssid: true,
            gateway_mac: false,
            bssid: false,
        };
        let only_gateway = DecisionInputs {
            ssid: false,
            gateway_mac: true,
            bssid: false,
        };
        let only_bssid = DecisionInputs {
            ssid: false,
            gateway_mac: false,
            bssid: true,
        };

        // 缺的那一项只让「要读它」的配置判不动，其余两项齐全时照常放行。
        let partial = snap(Some("Office"), Some("aa:bb:cc:dd:ee:ff"), &["en0"]);
        assert!(partial.can_decide(only_ssid));
        assert!(partial.can_decide(only_gateway));
        assert!(
            !partial.can_decide(only_bssid),
            "BSSID 型条件要的读数，SSID 与网关 MAC 替不了"
        );
        assert!(
            !partial.can_decide(DecisionInputs::EVERYTHING),
            "三项都要时，缺 BSSID 就判不动"
        );

        let mut complete = partial.clone();
        complete.bssid = Some("11:22:33:44:55:66".into());
        assert!(complete.can_decide(DecisionInputs::EVERYTHING));
    }

    #[test]
    fn fingerprint_sees_gateway_and_interface_changes() {
        let a = snap(Some("Office"), Some("aa:bb:cc:dd:ee:ff"), &["en0"]);
        // 同一 SSID、不同网关（换路由器 / 双频）—— 只盯 SSID 就看不见这次变化
        let b = snap(Some("Office"), Some("11:22:33:44:55:66"), &["en0"]);
        assert_ne!(a.fingerprint(), b.fingerprint());
        // 同一网络、插了网线
        let c = snap(Some("Office"), Some("aa:bb:cc:dd:ee:ff"), &["en0", "en5"]);
        assert_ne!(a.fingerprint(), c.fingerprint());
        assert_eq!(a.fingerprint(), snap(Some("Office"), Some("aa:bb:cc:dd:ee:ff"), &["en0"]).fingerprint());
    }
}
