//! 常驻动作的「一次检查」怎么做（3B2 的语义层）。
//!
//! 3B1 与 3B2 的差别就是这个抽象的形状：一次性动作是「执行它」，常驻动作是
//! 「**维持一个期望状态**」—— 每轮先问「已经满足了吗」，满足就一条命令都不发。
//! 所以这里没有 priority、没有批次，只有一个可重复调用、可提前停止的 `Tick`。
//!
//! ## `periodic_script` 没有独立的「检查」半边
//!
//! 它的一次 tick 就是**跑那个脚本**，退出码 0 记为「本轮无事」。这不是偷懒：
//! 「已满足就别动手」这件事只有脚本自己判断得了（它才知道要维持的是什么），
//! 把它拆成 Rust 侧的 check + repair 反而要为此再造一种 DSL。
//! 因此 `interval_secs` 的语义是「每隔多久跑一次」，与隧道类的「每隔多久核对一次」
//! 共用同一个 worker，但落到界面上的标签不同（见 [`label_for`]）。
//!
//! ## 安全边界与 3B1 同源
//!
//! 脚本路径同样先 [`resolve_script`] 再 [`is_script_allowed`]，并且**校验与执行用同一条
//! 路径**。常驻动作比一次性动作更需要这道边界：它每 N 秒就重复执行一次，一旦放行错了，
//! 一个来路不明的脚本就有了一个稳定的、自我修复的立足点。

use std::sync::Arc;
use std::time::Duration;

use super::one_shot::SCRIPT_TIMEOUT;
use super::{is_script_allowed, resolve_script, AllowedScripts};
use crate::config::{PersistentAction, PersistentActionType};
use crate::i18n;
use crate::platform::{NetworkPlatform, TunnelTarget};

/// 隧道类动作的一次检查愿意等多久。
///
/// 三条命令（`scutil --nc list` / `Get-NetAdapter` / `nmcli con show`）正常都是百毫秒级，
/// 60 秒留给「系统服务卡住」这一类；超过它就报「还在跑」，而不是把 worker 钉死。
/// WireGuard 的 `installtunnelservice` 有时会顺带拉起界面而不退出，正是靠这个上限兜住。
pub const TUNNEL_TICK_TIMEOUT: Duration = Duration::from_secs(60);

/// 一次 tick 的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    /// 期望状态本来就已满足 —— **什么都没做**。稳态下每个 tick 都该是这个取值。
    Satisfied,
    /// 不满足，已执行恢复动作且成功
    Repaired,
    /// 不满足且恢复失败（或脚本自己失败）
    Faulted(String),
}

/// 一次检查。必须能被反复调用，且每次调用只做「一轮」的事。
pub trait Tick: Send + Sync + 'static {
    fn tick(&self) -> TickOutcome;
}

// —————————————————————————— 脚本类 ——————————————————————————

/// `periodic_script`：跑脚本，退出码定成败。
///
/// `denied` 非空表示这条路径没过 allow-list —— 此时**永远不执行**，每个 tick 都原样
/// 报出同一条错误。为什么不是一次性失败后停掉 worker：用户很可能在配置里改了路径
/// 而 Profile 内容指纹没变到需要重启 worker 的程度，留在界面上的红字比静默消失有用。
///
/// ⚠️ 常驻脚本**一律不提权**，schema 里也就没有 `elevated` 这个字段：提权必须经
/// 系统自己的授权框让用户知情（见 `NetworkPlatform::run_script` 的安全决策），
/// 而 worker 是无人值守、每 N 秒重复一次的 —— 两者放在一起就是「每 30 秒弹一次授权框」。
/// 需要 root 的维持动作请把活交给隧道类动作，或配好免密通道后由脚本自己调用。
struct ScriptTick<P> {
    plat: P,
    path: String,
    args: Vec<String>,
    denied: Option<String>,
}

impl<P: NetworkPlatform + Copy + 'static> Tick for ScriptTick<P> {
    fn tick(&self) -> TickOutcome {
        if let Some(error) = &self.denied {
            return TickOutcome::Faulted(error.clone());
        }
        match self.plat.run_script(&self.path, &self.args, false) {
            Ok(()) => TickOutcome::Satisfied,
            Err(error) => TickOutcome::Faulted(error),
        }
    }
}

// —————————————————————————— 隧道类 ——————————————————————————

/// `keep_wireguard_connected` / `keep_vpn_connected`：核对隧道状态，断了才重连。
struct TunnelTick<P> {
    plat: P,
    target: TunnelTarget,
}

impl<P: NetworkPlatform + Copy + 'static> Tick for TunnelTick<P> {
    fn tick(&self) -> TickOutcome {
        let target = self.target.clone();
        let plat = self.plat;
        tunnel_decision(plat.tunnel_is_up(&target), || plat.tunnel_connect(&target))
    }
}

/// 已连着就什么都不做；没连着才去连。
///
/// 单独成函数是因为这条判据正是「常驻」两个字的含义，而它没法用真实的 VPN 隧道测 ——
/// 测试里造不出一条「连着但一会被切断」的隧道。
fn tunnel_decision(
    connected: bool,
    connect: impl FnOnce() -> Result<(), String>,
) -> TickOutcome {
    if connected {
        return TickOutcome::Satisfied;
    }
    match connect() {
        Ok(()) => TickOutcome::Repaired,
        Err(error) => TickOutcome::Faulted(error),
    }
}

// —————————————————————————— 构造 ——————————————————————————

/// 这条常驻动作的展示标签（日志与界面都用它认「是哪件事在维持」）。
///
/// 刻意保持简短：前半是随界面语言变的动词，后半是目标本身。`wireguard:` 与 `vpn:`
/// 是目标标识符的前缀而不是句子，所以不进字典；状态词由前端按 `state` 本地化。
/// 与日志同理，标签在创建那一刻按当时的语言定下，之后切换语言不会改写历史记录。
pub fn label_for(action: &PersistentAction) -> String {
    match &action.action {
        PersistentActionType::PeriodicScript { path, .. } => i18n::tf("act.label_run", &[("path", path)]),
        PersistentActionType::KeepWireGuardConnected { tunnel, .. } => {
            format!("wireguard:{}", tunnel)
        }
        PersistentActionType::KeepVpnConnected { provider, profile, .. } => {
            format!("vpn:{}/{}", provider, profile)
        }
    }
}

fn interval_of(action: &PersistentAction) -> u64 {
    match &action.action {
        PersistentActionType::PeriodicScript { interval_secs, .. } => *interval_secs,
        PersistentActionType::KeepWireGuardConnected { interval_secs, .. } => *interval_secs,
        PersistentActionType::KeepVpnConnected { interval_secs, .. } => *interval_secs,
    }
}

/// 这条动作多久核对一次。`validate` 已保证 > 0，这里再兜一次 0（配置可能从别的入口进来）。
pub fn interval_for(action: &PersistentAction) -> Duration {
    Duration::from_secs(interval_of(action).max(1))
}

/// 一次 tick 的等待上限。
///
/// 脚本类沿用 3B1 的预算（它可能就是耗时几分钟的正常脚本）；隧道类只问几条系统命令。
/// 常驻动作没有提权那条更宽的预算，因为这里根本不存在提权路径（见 [`ScriptTick`]）。
pub fn budget_for(action: &PersistentAction) -> Duration {
    match &action.action {
        PersistentActionType::PeriodicScript { .. } => SCRIPT_TIMEOUT,
        _ => TUNNEL_TICK_TIMEOUT,
    }
}

/// 为一条常驻动作造出它的 tick。
pub fn tick_for<P>(plat: P, allowed: &AllowedScripts, action: &PersistentAction) -> Arc<dyn Tick>
where
    P: NetworkPlatform + Copy + 'static,
{
    match &action.action {
        PersistentActionType::PeriodicScript { path, args, .. } => {
            // 先解析再校验：喂给校验和喂给执行的必须是同一条路径（与 3B1 同一条规矩）
            let script = resolve_script(path, allowed);
            let denied = (!is_script_allowed(&script, allowed)).then(|| {
                i18n::tf(
                    "act.script_denied",
                    &[("path", path), ("resolved", &script.display().to_string())],
                )
            });
            Arc::new(ScriptTick {
                plat,
                path: script.to_string_lossy().to_string(),
                args: args.clone(),
                denied,
            })
        }
        PersistentActionType::KeepWireGuardConnected { tunnel, .. } => Arc::new(TunnelTick {
            plat,
            target: TunnelTarget::WireGuard {
                tunnel: tunnel.clone(),
            },
        }),
        PersistentActionType::KeepVpnConnected { provider, profile, .. } => {
            Arc::new(TunnelTick {
                plat,
                target: TunnelTarget::Vpn {
                    provider: provider.clone(),
                    profile: profile.clone(),
                },
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(kind: PersistentActionType) -> PersistentAction {
        PersistentAction {
            id: "p1".into(),
            enabled: true,
            priority: 100,
            action: kind,
        }
    }

    /// 「已连着就别动」是常驻动作的全部要点：这条判据必须能被直接测到，
    /// 否则测的就只剩真实 VPN 隧道 —— 而那个在 CI 里根本没有。
    #[test]
    fn an_up_tunnel_triggers_no_connect_call() {
        let mut called = false;
        let out = tunnel_decision(true, || {
            called = true;
            Ok(())
        });
        assert_eq!(out, TickOutcome::Satisfied);
        assert!(!called, "已连着还去 connect，就是每 N 秒把用户的隧道踢重连一次");

        let out = tunnel_decision(false, || Ok(()));
        assert_eq!(out, TickOutcome::Repaired);

        let out = tunnel_decision(false, || Err("需要管理员权限".into()));
        assert_eq!(out, TickOutcome::Faulted("需要管理员权限".into()));
    }

    #[test]
    fn labels_carry_the_target_and_only_the_verb_is_localized() {
        i18n::init();
        let script = label_for(&action(PersistentActionType::PeriodicScript {
            path: "scripts/keep.sh".into(),
            args: vec![],
            interval_secs: 30,
        }));
        assert!(script.contains("scripts/keep.sh"), "标签要认得出是哪件事: {script}");
        assert!(!script.starts_with("act."), "动词取自字典，不是键名: {script}");
        // 隧道目标本身就是标识符，五种语言里都不该被翻译
        assert_eq!(
            label_for(&action(PersistentActionType::KeepWireGuardConnected {
                tunnel: "office-wg".into(),
                interval_secs: 30,
            })),
            "wireguard:office-wg"
        );
        assert_eq!(
            label_for(&action(PersistentActionType::KeepVpnConnected {
                provider: "proton".into(),
                profile: "My VPN".into(),
                interval_secs: 30,
            })),
            "vpn:proton/My VPN"
        );
    }

    #[test]
    fn budgets_and_intervals_come_from_the_action() {
        let wg = action(PersistentActionType::KeepWireGuardConnected {
            tunnel: "office".into(),
            interval_secs: 45,
        });
        assert_eq!(interval_for(&wg), Duration::from_secs(45));
        assert_eq!(budget_for(&wg), TUNNEL_TICK_TIMEOUT);
        assert!(
            TUNNEL_TICK_TIMEOUT < SCRIPT_TIMEOUT,
            "核对状态只是问三条命令，不该比跑脚本还宽容"
        );

        // 0 间隔在校验里就被拒了，但 worker 的节律不能依赖校验（配置可能从别的入口进来）
        let zero = action(PersistentActionType::KeepWireGuardConnected {
            tunnel: "office".into(),
            interval_secs: 0,
        });
        assert_eq!(interval_for(&zero), Duration::from_secs(1));

        let script = action(PersistentActionType::PeriodicScript {
            path: "scripts/x.sh".into(),
            args: vec![],
            interval_secs: 10,
        });
        assert_eq!(budget_for(&script), SCRIPT_TIMEOUT);
    }

    /// allow-list 之外的脚本永不被执行 —— 这条测试是「常驻」版本的安全边界：
    /// 一次性的执行层已经测过同一条路径，而 worker 会把同一动作重复几百次，
    /// 漏一次就等于给了来路不明的脚本一个自我修复的立足点。
    #[test]
    fn a_script_outside_the_allow_list_is_reported_and_never_run() {
        i18n::init();
        let allowed = AllowedScripts {
            scripts_dir: std::env::temp_dir().join(format!("ns-never-{}", std::process::id())),
            explicit: vec![],
        };
        let a = action(PersistentActionType::PeriodicScript {
            path: "/etc/ppp/peers/wvdial".into(),
            args: vec![],
            interval_secs: 5,
        });
        let tick = tick_for(crate::platform::Platform, &allowed, &a);
        for _ in 0..3 {
            let out = tick.tick();
            match out {
                TickOutcome::Faulted(e) => {
                    // 措辞随界面语言走，所以这里锚的是「句子里必须有原文路径」这一条：
                    // 只给解析结果的话，用户无从知道我们把它当成了谁。
                    assert!(e.contains("/etc/ppp/peers/wvdial"), "实际报错: {}", e);
                    assert!(!e.starts_with("act."), "报错该是一句译文而不是键名: {}", e);
                }
                other => panic!("被拒绝的脚本不该报成功: {:?}", other),
            }
        }
    }
}
