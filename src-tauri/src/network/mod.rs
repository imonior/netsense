//! 3A 网络配置层：Apply → Verify，失败即阻断 3B（方案第 16/18/19/20 条）。
//!
//! 为什么要「Verify」这一步：命令返回 `Ok` 并不等于配置写进去了 ——
//! macOS 的 `networksetup`、Windows 的 `netsh` 都会**在授权被拒 / 服务名对不上 /
//! 网卡刚断开时静默返回 0**。于是「静态 IP 其实没下上去，但 VPN 已经启动了」——
//! 用户拿到的是一个自称生效、实际不通的环境。3A 的判据因此是**回读实际参数**，
//! 而不是命令退出码。

pub mod health;
pub mod readback;

use crate::config::{Mode, NetworkConfig};
use crate::i18n;
use crate::platform::{InterfaceStatus, NetworkPlatform};
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

/// The mode's name inside a log line: spelled exactly as in the config file (serde
/// lowercase), so a user comparing the log against the editor sees the same word.
///
/// 配置模式在日志里的名字：与配置文件里的写法一致（serde 小写），
/// 用户拿着日志对照编辑器时看到的是同一个词。
fn mode_name(mode: &Mode) -> &'static str {
    match mode {
        Mode::Dhcp => "dhcp",
        Mode::Manual => "manual",
    }
}

/// Roll back and record what actually happened: a successful restore is a warn
/// (the network was changed back, visible at the default level), a failed one is
/// an error — the live system may then sit half-old-half-new, the shape that most
/// needs troubleshooting. The previous three `restore_from_snapshot(&before).ok()`
/// calls silently swallowed restore failures; "claims a rollback that never
/// happened" is worse than no rollback at all.
///
/// 回滚并按实记一行：恢复成功记 warn（网络被改回去了，默认档也该看见），
/// 恢复失败记 error —— 现场可能停在半新半旧之间，那是最需要排查的形状。
/// 原先三处 `restore_from_snapshot(&before).ok()` 把恢复失败静默吞掉，
/// 「说回滚了其实没有」比不回滚更糟。
fn rollback_and_log<P: NetworkPlatform>(plat: &P, before: &InterfaceStatus) {
    match plat.restore_from_snapshot(before) {
        Ok(()) => crate::log::warn(&i18n::t("net.rollback")),
        Err(e) => crate::log::error(&i18n::tf("net.rollback_failed", &[("error", &e)])),
    }
}

/// 执行 3A：快照当前状态 → 下发 → 路由 → 回读校验（必要时补 DNS 兜底）。
///
/// 回读比对覆盖地址/掩码/网关/DNS/静态路由/IPv6 模式；任一步失败都尝试回滚到快照状态，
/// 再返回 [`Stage3A::Failed`]。调用方据此**不要**执行 3B。
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

    // 1b. 下发前比对当前快照与期望配置：系统已符合期望就**跳过下发**。
    //     读取（get_status / get_routes）不需要提权，于是标准用户 / 无法提权的机器上，
    //     「配置本就合规」的那一类开机不再弹 UAC、也不再等常驻 helper 的 15 秒轮询。
    //     这条比对与下方下发后的回读校验是同一份逻辑（readback_match），因此「跳过」与
    //     「下发成功且回读通过」结果完全等价 —— 只是省了一次（可能提权的）下发。
    // 1b. Pre-apply diff: when the live system already matches the desired config, skip the
    //     apply. Reads need no privilege, so on standard-user / non-elevatable machines a
    //     "config already satisfied" boot no longer pops a UAC or waits on the 15s helper poll.
    //     The diff is the same logic used for post-apply verification (readback_match), so
    //     "skip" is identical in outcome to "applied and verified" — only a (possibly
    //     elevated) apply is saved.
    if readback_match(plat, cfg, &before.routes).is_ok() {
        crate::log::debug(&i18n::t("net.skip_already_applied"));
        return Stage3A::Applied { note: None };
    }

    // 2. Apply the network configuration. Log "apply begins" first: it is the start
    //    marker of every execution action, pairing with the apply's success
    //    (engine.pass_3a) / failure (engine.error) into one complete timeline.
    // 2. 下发网络配置。先记一行「开始下发」：它是每一次执行动作的起点标记，
    //    与下发的成功（engine.pass_3a）/ 失败（engine.error）配成一条完整时间线。
    crate::log::info(&i18n::tf("net.apply_start", &[("mode", mode_name(&cfg.mode))]));
    if let Err(e) = plat.apply_network(cfg) {
        // 下发失败：尝试回滚
        rollback_and_log(plat, &before);
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
            rollback_and_log(plat, &before);
            return Stage3A::Failed {
                reason: i18n::tf("net.route_failed", &[("dest", &r.dest), ("error", &e)]),
            };
        }
        // One line per route step (debug level): when troubleshooting "the route was
        // added, why is it not in effect", the log must show which route and from
        // which step onward it went missing.
        // 每一步路由都留一行（debug 档）：排查「路由明明加了怎么没生效」时，
        // 要能看出是哪一条、从哪一步开始不见的。
        if r.delete {
            crate::log::debug(&i18n::tf("net.route_deleted", &[("dest", &r.dest)]));
        } else if let Some(g) = r.gateway.as_deref() {
            crate::log::debug(&i18n::tf(
                "net.route_added",
                &[("dest", &r.dest), ("gw", g)],
            ));
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
        match readback_match(plat, cfg, &before.routes) {
            Ok(()) => return Stage3A::Applied { note: None },
            Err(f) => {
                // One line per mismatching read-back (debug level): inside the
                // 4×800ms settle window, "which attempt and what differed" is
                // exactly what troubleshooting a non-effective apply needs.
                // 每一次没对上的回读都留一行（debug 档）：4×800ms 的沉降期内
                // 「第几次、差在哪」正是排查下发不生效时要看的东西。
                if attempt + 1 < READBACK_ATTEMPTS {
                    crate::log::debug(&i18n::tf(
                        "net.readback_retry",
                        &[
                            ("n", &(attempt + 1).to_string()),
                            ("detail", &f.to_string()),
                        ],
                    ));
                    std::thread::sleep(READBACK_SETTLE);
                }
                last = Some(f);
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
            && readback_match(plat, cfg, &before.routes).is_ok()
        {
            let servers = dns_fallback.join(", ");
            crate::log::warn(&i18n::tf("net.dns_fallback_used", &[("servers", &servers)]));
            return Stage3A::Applied {
                note: Some(i18n::tf("net.dns_fallback_note", &[("servers", &servers)])),
            };
        }
    }

    // 校验失败：回滚到快照状态
    rollback_and_log(plat, &before);
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

    /// 下发前 diff：系统当前状态已等于期望配置时，必须跳过 `apply_network`，
    /// 直接返回 `Applied` —— 这样标准用户 / 无法提权的机器在「配置本就合规」的开机上
    /// 不会触发提权下发（无谓的 UAC + 15 秒 helper 轮询）。
    /// 跳过与「下发成功且回读通过」结果等价，只是省了一次下发。
    #[test]
    fn already_satisfied_config_skips_the_elevated_apply() {
        use crate::platform::{Call, InterfaceStatus, MockPlatform};
        let mock = MockPlatform::new();
        mock.clear_calls();
        let cfg = NetworkConfig {
            mode: crate::config::Mode::Manual,
            ip: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1,8.8.8.8".into()),
            ..Default::default()
        };
        // 系统当前状态已经等于期望配置
        // The live system already equals the desired config.
        mock.set_state(InterfaceStatus {
            connected: true,
            ipv4: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1 8.8.8.8".into()),
            ..Default::default()
        });
        let res = apply_3a(&mock, &cfg, &[]);
        assert!(
            matches!(res, Stage3A::Applied { .. }),
            "已符合期望应直接 Applied: {:?}",
            res
        );
        let calls = mock.calls();
        assert!(
            !calls.iter().any(|c| matches!(c, Call::ApplyNetwork { .. })),
            "已符合期望时不应调用 apply_network（避免无谓的提权下发）: {:?}",
            calls
        );
    }

    /// 下发前 diff 的反面：系统状态与期望不符时，仍必须走 `apply_network`。
    /// 这条保证「真的有差异（如重启后非持久路由丢失）时该弹 UAC 还是弹」，不会误跳。
    #[test]
    fn mismatch_still_triggers_apply() {
        use crate::platform::{Call, InterfaceStatus, MockPlatform};
        let mock = MockPlatform::new();
        mock.clear_calls();
        let cfg = NetworkConfig {
            mode: crate::config::Mode::Manual,
            ip: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1,8.8.8.8".into()),
            ..Default::default()
        };
        // 系统停在旧 / 错误地址
        // The system is stuck on a stale address.
        mock.set_state(InterfaceStatus {
            connected: true,
            ipv4: Some("169.254.23.9".into()),
            ..Default::default()
        });
        let res = apply_3a(&mock, &cfg, &[]);
        assert!(
            matches!(res, Stage3A::Applied { .. }),
            "下发后应通过: {:?}",
            res
        );
        let calls = mock.calls();
        assert!(
            calls.iter().any(|c| matches!(c, Call::ApplyNetwork { .. })),
            "状态不符时必须仍然调用 apply_network: {:?}",
            calls
        );
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

    /// The mode name in the log must match the config-file spelling (serde
    /// lowercase): a user comparing the log against the editor has to see the same
    /// word. When `Mode` gains a variant, this test forces the log name to be
    /// added along with it.
    ///
    /// 日志里的模式名必须与配置文件里的写法一致（serde 小写）：
    /// 用户拿着日志对照编辑器时，看到的得是同一个词。
    /// `Mode` 加新变体时这条测试会逼着把日志名字一起补上。
    #[test]
    fn mode_name_matches_the_config_file_spelling() {
        assert_eq!(mode_name(&crate::config::Mode::Dhcp), "dhcp");
        assert_eq!(mode_name(&crate::config::Mode::Manual), "manual");
    }
}
