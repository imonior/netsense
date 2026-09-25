//! 3A 的回读校验：把「期望参数」与平台实际报告的参数逐项比对。
//!
//! 判据刻意保守 —— **只比对「两侧都拿得到」的字段**。取不到的一侧不算失败：
//! 各平台能读回什么差别很大（Linux 的 `nmcli` 不给掩码、macOS 未配自定义 DNS 时
//! `networksetup -getdnsservers` 返回的是一句提示文本而不是地址列表）。
//! 把「读不到」判成失败会让大量本来正确的下发被误报为 ERROR，
//! 而误报的代价（用户从此不敢信状态灯）比漏报更高。

use crate::config::{Mode, NetworkConfig};
use crate::i18n;
use crate::platform::{InterfaceStatus, NetworkPlatform};
use std::fmt;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyFailure {
    /// 形如 `ip: 期望 192.168.1.100，实际 169.254.23.9`（措辞随界面语言，字段名与地址原样保留）
    pub mismatches: Vec<String>,
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

pub fn readback_match<P: NetworkPlatform>(
    plat: &P,
    want: &NetworkConfig,
) -> Result<(), VerifyFailure> {
    // `fresh_status` 而不是 `get_status`：校验循环靠的是「每次采样都真的去问一次系统」，
    // 拿到一份 TTL 内的旧快照等于少一次重试机会。
    check(&plat.fresh_status(), want)
}

/// 纯函数版本：把采样与判定分开，便于用假数据覆盖各种不符组合。
pub fn check(got: &InterfaceStatus, want: &NetworkConfig) -> Result<(), VerifyFailure> {
    let mut mismatches: Vec<String> = Vec::new();

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

    // DNS 有两个「不比」的条件，各自对应一类误报：
    //   · 期望为空 —— 清空自定义 DNS 后系统仍会从 DHCP 读到路由器地址，拿它比必然失败；
    //   · `got.dns` 为 None —— 这一侧根本读不到（平台不支持 / 权限不足），读不到不等于不符。
    //     注意与 `Some("")` 区分：读到「明确没有 DNS 记录」而期望有值，是货真价实的下发失败。
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
        }
    }

    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(VerifyFailure { mismatches })
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

    #[test]
    fn clearing_dns_is_not_verified_against_dhcp_supplied_servers() {
        let want = NetworkConfig {
            mode: Mode::Manual,
            ip: Some("192.168.1.100".into()),
            dns: Some("".into()),
            ..Default::default()
        };
        let got = InterfaceStatus {
            ipv4: Some("192.168.1.100".into()),
            dns: Some("192.168.1.1".into()),
            ..Default::default()
        };
        check(&got, &want).expect("期望为空时不该拿路由器下发的 DNS 来判失败");
    }
}
