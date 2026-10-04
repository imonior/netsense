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

    /// 这一份快照有没有读到「现在是哪个网络」的任何一项证据。
    ///
    /// 三项全空**不能**读成「此刻在一个陌生网络上」：SSID 这一侧在 macOS 上只有
    /// CoreWLAN 一个来源（命令行在 Sequoia 上全部涂黑），网关 MAC 要靠 ARP 表里恰好
    /// 有那一条，BSSID 同理 —— 三者同时为空更常见的原因是这几次读取都没拿到东西。
    /// 引擎用它拦住「零命中 → 撤掉现网」那条处置，见 `engine::decide_plan`。
    pub fn has_identity(&self) -> bool {
        self.ssid.is_some() || self.gateway_mac.is_some() || self.bssid.is_some()
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
    fn a_sample_without_any_identity_field_is_not_an_observation() {
        // 现场在线（en0 仍在网卡集合里），但 SSID / 网关 MAC / BSSID 一个都没读到。
        // 这一份不是「换到了一个陌生网络」的观察，而是身份读取失败 —— 拿它判零命中，
        // 兜底就会把正在工作的静态配置拆掉（回落 DHCP、清空 DNS）。
        let s = snap(None, None, &["en0"]);
        assert!(
            !s.has_identity(),
            "三项身份字段全空时，这份快照不该被当成读到了网络身份"
        );
        assert!(
            s.fingerprint().contains("en0"),
            "网卡集合照常进指纹：身份没读到不等于什么都没读到"
        );
    }

    #[test]
    fn any_single_identity_field_counts_as_read() {
        assert!(snap(Some("Office"), None, &["en0"]).has_identity());
        assert!(snap(None, Some("aa:bb:cc:dd:ee:ff"), &["en0"]).has_identity());
        let mut b = snap(None, None, &["en0"]);
        b.bssid = Some("11:22:33:44:55:66".into());
        assert!(b.has_identity(), "BSSID 单独有值也算读到了身份");
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
