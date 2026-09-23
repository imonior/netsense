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

fn nic_is_up(n: &NicInfo) -> bool {
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
