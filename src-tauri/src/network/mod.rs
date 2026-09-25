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
use crate::platform::{Health, NetworkPlatform, ProbeTarget};
use std::time::Duration;

pub use health::HealthMonitor;
pub use readback::{readback_match, VerifyFailure};

/// 3A 的结果。`Failed` 是唯一会阻断 3B 的结果（方案第 20 条）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage3A {
    Applied,
    Failed { reason: String },
}

/// 回读校验的采样节律：下发后网卡要重新关联、DHCP 还要拿地址，立刻读会误判失败。
const READBACK_ATTEMPTS: usize = 4;
const READBACK_SETTLE: Duration = Duration::from_millis(800);

/// 执行 3A：下发 → 路由 → 回读校验。
///
/// 任一步失败都返回 [`Stage3A::Failed`]，调用方据此**不要**执行 3B。
pub fn apply_3a<P: NetworkPlatform>(plat: &P, cfg: &NetworkConfig) -> Stage3A {
    if let Err(e) = plat.apply_network(cfg) {
        return Stage3A::Failed {
            reason: i18n::tf("net.apply_failed", &[("error", &e)]),
        };
    }
    // 静态路由属于 3A：路由没加上时继续跑自动化只会放大问题 —— 放在 on_apply 里，
    // 「路由失败」和「软件启动失败」就会混在同一份结果里，后者还照样执行。
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
            return Stage3A::Failed {
                reason: i18n::tf("net.route_failed", &[("dest", &r.dest), ("error", &e)]),
            };
        }
    }

    let verify = cfg.verify.clone().unwrap_or_default();
    if !verify.readback {
        return Stage3A::Applied;
    }
    let mut last: Option<VerifyFailure> = None;
    for attempt in 0..READBACK_ATTEMPTS {
        match readback_match(plat, cfg) {
            Ok(()) => return Stage3A::Applied,
            Err(f) => {
                last = Some(f);
                if attempt + 1 < READBACK_ATTEMPTS {
                    std::thread::sleep(READBACK_SETTLE);
                }
            }
        }
    }
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

/// 一次性探测目标（用于「立即探测」与健康度校验）。与持续监测共用同一份配置形状。
pub fn probe_target(cfg: &crate::config::HealthConfig) -> (ProbeTarget, u64) {
    (
        ProbeTarget {
            mode: cfg.mode.clone(),
            http_target: cfg.http_target.clone(),
            icmp_target: cfg.icmp_target.clone(),
        },
        cfg.timeout.max(1) * 1000,
    )
}

/// 默认探测目标：没配健康度时也让「立即探测」有明确结果，而不是静默什么都不做。
pub fn default_probe_target() -> (ProbeTarget, u64) {
    (
        ProbeTarget {
            mode: crate::config::ProbeMode::Both,
            http_target: Some("http://cp.cloudflare.com".to_string()),
            icmp_target: Some("223.5.5.5".to_string()),
        },
        5000,
    )
}

pub fn probe_ok<P: NetworkPlatform>(plat: &P, target: &ProbeTarget, timeout_ms: u64) -> bool {
    matches!(plat.probe(target, timeout_ms), Health::Ok)
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
            Stage3A::Applied => panic!("Applied 不应携带失败原因"),
        }
    }
}
