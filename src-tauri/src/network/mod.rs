//! 3A 网络配置层：Apply → Verify，失败即阻断 3B（方案第 16/18/19/20 条）。
//!
//! 为什么要「Verify」这一步：命令返回 `Ok` 并不等于配置写进去了 ——
//! macOS 的 `networksetup`、Windows 的 `netsh` 都会**在授权被拒 / 服务名对不上 /
//! 网卡刚断开时静默返回 0**。于是「静态 IP 其实没下上去，但 VPN 已经启动了」——
//! 用户拿到的是一个自称生效、实际不通的环境。3A 的判据因此是**回读实际参数**，
//! 而不是命令退出码。

pub mod health;
pub mod readback;

use crate::config::NetworkConfig;
use crate::i18n;
use crate::platform::NetworkPlatform;
use std::time::Duration;

pub use health::HealthMonitor;
pub use readback::{readback_match, VerifyFailure};

/// 3A 的结果。`Failed` 是唯一会阻断 3B 的结果（方案第 20 条）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage3A {
    /// 下发并回读都过了。`note` 是给用户看的一句补充，只在「成功但走了非预期路径」
    /// 时才有值（目前只有 DNS 兜底这一种）—— 用户有权知道自己现在用的不是他填的解析器。
    Applied { note: Option<String> },
    Failed { reason: String },
}

/// 回读校验的采样节律：下发后网卡要重新关联、DHCP 还要拿地址，立刻读会误判失败。
const READBACK_ATTEMPTS: usize = 4;
const READBACK_SETTLE: Duration = Duration::from_millis(800);

/// 执行 3A：快照当前状态 → 下发 → 路由 → 回读校验（必要时补 DNS 兜底）。
///
/// 任一步失败都尝试回滚到快照状态，再返回 [`Stage3A::Failed`]。
/// 调用方据此**不要**执行 3B。
///
/// `dns_fallback` 是设置里那份兜底服务器列表（**空 = 没启用**，见
/// [`crate::appconfig::DnsFallback`]）。它只在一种失败上起作用：静态地址 + 「DNS 交回
/// 自动获取」，而系统里最终一个 nameserver 都没有 —— 那正是补一组服务器能翻盘的唯一情形。
pub fn apply_3a<P: NetworkPlatform>(
    plat: &P,
    cfg: &NetworkConfig,
    dns_fallback: &[String],
) -> Stage3A {
    // 1. 快照当前网络状态（含目标网卡与已存在路由，用于失败时按同一张网卡、连同旧路由回滚）
    // 1. Snapshot current state (including target NIC and pre-existing routes) so a failed
    //    apply can roll back on the same interface and restore the original routes.
    let mut before = plat.get_status();
    before.target = cfg.target;
    before.routes = plat.get_routes();
    
    // 2. 下发网络配置
    if let Err(e) = plat.apply_network(cfg) {
        // 下发失败：尝试回滚
        plat.restore_from_snapshot(&before).ok();
        return Stage3A::Failed {
            reason: i18n::tf("net.apply_failed", &[("error", &e)]),
        };
    }
    
    // 3. 下发静态路由（记录已添加的路由，失败时回滚）
    let mut added_routes: Vec<(String, String)> = Vec::new();
    for r in &cfg.routes {
        let res = if r.delete {
            plat.delete_route(&r.dest)
        } else {
            match r.gateway.as_deref() {
                Some(g) => plat.add_route(&r.dest, g, r.metric),
                None => Err(i18n::tf("net.route_no_gw", &[("dest", &r.dest)])),
            }
        };
        if let Err(e) = res {
            // 路由失败：回滚已添加的路由 + 网络配置
            for (dest, _gw) in added_routes {
                plat.delete_route(&dest).ok();
            }
            plat.restore_from_snapshot(&before).ok();
            return Stage3A::Failed {
                reason: i18n::tf("net.route_failed", &[("dest", &r.dest), ("error", &e)]),
            };
        }
        if !r.delete {
            if let Some(g) = r.gateway.as_deref() {
                added_routes.push((r.dest.clone(), g.to_string()));
            }
        }
    }

    let verify = cfg.verify.clone().unwrap_or_default();
    if !verify.readback {
        return Stage3A::Applied { note: None };
    }
    let mut last: Option<VerifyFailure> = None;
    for attempt in 0..READBACK_ATTEMPTS {
        match readback_match(plat, cfg) {
            Ok(()) => return Stage3A::Applied { note: None },
            Err(f) => {
                last = Some(f);
                if attempt + 1 < READBACK_ATTEMPTS {
                    std::thread::sleep(READBACK_SETTLE);
                }
            }
        }
    }

    // —— 最后一次补救：DNS 兜底 ——
    //
    // 只在这一类失败上试：期望「交回自动获取」+ 静态地址 + 系统里一个 nameserver 都没有。
    // 其余不符（地址没下上去、路由没生效）补一组 DNS 也救不回来，试了只是把失败推迟到
    // 回滚之后，还白白多一次提权。
    //
    // 补完**必须再回读一次**：`apply_dns_only` 返回 Ok 只代表命令被接受，不代表 DNS 真的
    // 到位了。这里若不看结果就放行，「说补上了其实没补」比老实报失败更糟。
    if let Some(f) = &last {
        if f.dns_left_empty
            && !dns_fallback.is_empty()
            && plat.apply_dns_only(cfg.target, dns_fallback).is_ok()
            && readback_match(plat, cfg).is_ok()
        {
            let servers = dns_fallback.join(", ");
            crate::log::warn(&i18n::tf("net.dns_fallback_used", &[("servers", &servers)]));
            return Stage3A::Applied {
                note: Some(i18n::tf("net.dns_fallback_note", &[("servers", &servers)])),
            };
        }
    }

    // 校验失败：回滚到快照状态
    plat.restore_from_snapshot(&before).ok();
    Stage3A::Failed {
        reason: i18n::tf(
            "net.verify_failed",
            &[
                ("tries", &READBACK_ATTEMPTS.to_string()),
                ("detail", &last.map(|f| f.to_string()).unwrap_or_default()),
            ],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_carries_the_reason_that_blocks_stage_3b() {
        // 引擎按这个 reason 给 Profile 标 ERROR，所以它必须是给人看的完整句子
        let s = Stage3A::Failed {
            reason: "回读校验未通过".into(),
        };
        match s {
            Stage3A::Failed { reason } => assert_eq!(reason, "回读校验未通过"),
            Stage3A::Applied { .. } => panic!("Applied 不应携带失败原因"),
        }
    }

    /// 「静态 IP + DNS 交回自动获取」在静态地址这一支上必然读到空 DNS。
    /// 没配兜底 → 判失败；配了且补上了 → 通过，但必须如实告诉用户用的是兜底。
    #[test]
    fn dns_fallback_rescues_a_static_address_that_ended_up_with_no_dns() {
        use crate::platform::MockPlatform;
        let mock = MockPlatform::new();
        mock.clear_calls();
        let cfg = NetworkConfig {
            mode: crate::config::Mode::Manual,
            ip: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            // 「交回自动获取」：mock 照实把 dns 写成 ""，于是回读必然读到空。
            dns: Some("".into()),
            ..Default::default()
        };

        // 没给兜底：不能放行 —— 放行的结果是「IP 通、网关通、网页全打不开」。
        let res = apply_3a(&mock, &cfg, &[]);
        assert!(
            matches!(res, Stage3A::Failed { .. }),
            "没有兜底时必须判失败，不能假装自动 DNS 成功了"
        );

        // 给了兜底：补上之后回读通过，且降级这件事要写在 note 里。
        match apply_3a(&mock, &cfg, &["10.0.0.53".to_string()]) {
            Stage3A::Applied { note } => assert!(
                note.as_deref().unwrap_or_default().contains("10.0.0.53"),
                "降级成功必须写明用的是哪台兜底 DNS: {:?}",
                note
            ),
            Stage3A::Failed { reason } => panic!("有兜底且确实补上了就该通过: {reason}"),
        }
    }

    /// 兜底没补上就是没补上：`apply_dns_only` 返回 Ok 只代表命令被接受，
    /// 回读仍然为空时必须照旧判失败 —— 「说补上了其实没补」比老实报失败更糟。
    #[test]
    fn a_fallback_that_does_not_land_is_still_a_failure() {
        use crate::platform::MockPlatform;
        let mock = MockPlatform::new();
        mock.set_apply_dns_only_result(Err("permission denied".into()));
        let cfg = NetworkConfig {
            mode: crate::config::Mode::Manual,
            ip: Some("192.168.1.100".into()),
            dns: Some("".into()),
            ..Default::default()
        };
        let res = apply_3a(&mock, &cfg, &["10.0.0.53".to_string()]);
        assert!(
            matches!(res, Stage3A::Failed { .. }),
            "兜底没补上就必须判失败: {:?}",
            res
        );
    }
}
