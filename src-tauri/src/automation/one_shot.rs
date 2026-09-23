//! 3B1 一次性动作：按 priority 分批执行。
//!
//! 调度规则（方案第 19~23 条）：
//!
//! - `priority` 数字越小越先执行；
//! - **相同 priority 并发**执行（例：同时启动代理客户端和工作软件，没必要排队）；
//! - 当前批次**全部结束**（不论成败）才进入下一批；
//! - 单个动作失败**不阻断**后续批次 —— 这与 3A 相反：3A 失败还继续跑自动化会把
//!   一个不通的网络越搞越乱，而「VPN 没连上」不该阻止「打开 Slack」。
//!
//! 因此本模块的结果只有 `Success / Partial / Failed` 三种记录，没有「中断」。
//!
//! ## 等待上限
//!
//! 每个动作都有**自己**的超时（[`timeout_for`]）。超时的含义是「我们不再等了」，
//! 不是「动作被终止」：Rust 没有安全的手段去杀掉一个已经跑起来的用户进程，
//! 而提权脚本等的常常是**用户本人**在授权框上的决定。所以超时只会：把该动作记为失败、
//! 让批次继续往下走；迟到的结果由工作线程自己写进日志，不再计入本次运行。
//!
//! ## 谁调用 `execute`
//!
//! `execute` 会阻塞到全部批次结束（最坏情况是若干超时之和）。引擎线程是唯一能响应
//! 网络变化、下发 3A 的地方，因此**必须**通过 [`spawn`] 把一次运行交给独立线程。

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::{is_script_allowed, resolve_script, AllowedScripts};
use crate::config::{OneShotAction, OneShotActionType};
use crate::log;
use crate::platform::NetworkPlatform;

/// `open -a` / `start` 这类动作正常是秒级；给 30 秒是为了让冷启动、磁盘唤醒
/// 这类「慢但正常」的情况也有机会完成。
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
/// 普通脚本可以正当耗时（连 VPN、下发路由表），120 秒才算「明显卡住」。
pub const SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);
/// 提权脚本要先等用户在系统授权框上做决定，那是人的耗时，不是进程的耗时。
pub const ELEVATED_SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    /// 没有已启用的动作
    Empty,
    Success,
    /// 部分成功
    Partial,
    /// 全都失败
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionOutcome {
    pub id: String,
    /// 人类可读的动作描述（日志与 UI 用）
    pub label: String,
    /// 它属于哪一批（= 配置里的 priority）。报告是摊平的，前端要按批还原分组就得靠它。
    pub priority: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchReport {
    pub status: BatchStatus,
    pub outcomes: Vec<ActionOutcome>,
}

impl Default for BatchReport {
    fn default() -> Self {
        BatchReport {
            status: BatchStatus::Empty,
            outcomes: Vec::new(),
        }
    }
}

/// 把「已按 priority 升序稳定排序」的动作切成批：相邻同 priority 归为一批。
///
/// 单独成函数是因为这正是调度语义本身（同批并发、批间串行），值得被直接测到，
/// 而不是藏在一段带副作用的循环里。
pub(crate) fn group_batches<'a>(ordered: &'a [&'a OneShotAction]) -> Vec<&'a [&'a OneShotAction]> {
    let mut out: Vec<&[&OneShotAction]> = Vec::new();
    let mut start = 0usize;
    while start < ordered.len() {
        let prio = ordered[start].priority;
        let mut end = start + 1;
        while end < ordered.len() && ordered[end].priority == prio {
            end += 1;
        }
        out.push(&ordered[start..end]);
        start = end;
    }
    out
}

pub(crate) fn status_of(outcomes: &[ActionOutcome]) -> BatchStatus {
    if outcomes.is_empty() {
        return BatchStatus::Empty;
    }
    let failed = outcomes.iter().filter(|o| !o.ok).count();
    if failed == 0 {
        BatchStatus::Success
    } else if failed == outcomes.len() {
        BatchStatus::Failed
    } else {
        BatchStatus::Partial
    }
}

fn label_of(kind: &OneShotActionType) -> String {
    match kind {
        OneShotActionType::LaunchApp { app, .. } => format!("launch {}", app),
        OneShotActionType::RunScript { path, elevated, .. } => {
            if *elevated {
                format!("run(提权) {}", path)
            } else {
                format!("run {}", path)
            }
        }
    }
}

/// 单个动作愿意等多久。
pub fn timeout_for(a: &OneShotAction) -> Duration {
    match &a.action {
        OneShotActionType::LaunchApp { .. } => LAUNCH_TIMEOUT,
        OneShotActionType::RunScript { elevated, .. } => {
            if *elevated {
                ELEVATED_SCRIPT_TIMEOUT
            } else {
                SCRIPT_TIMEOUT
            }
        }
    }
}

fn outcome_of<P: NetworkPlatform>(
    plat: P,
    allowed: &AllowedScripts,
    a: &OneShotAction,
) -> ActionOutcome {
    let label = label_of(&a.action);
    let res = match &a.action {
        OneShotActionType::LaunchApp { app, args } => plat.launch_app(app, args),
        OneShotActionType::RunScript {
            path,
            args,
            elevated,
        } => {
            // allow-list 在这里判，而不是在平台层判：平台层的 `run_script` 还要给
            // 「立即运行脚本」等内部入口用，把安全边界塞进它会顺带放开那些入口。
            // 先解析再校验：喂给校验和喂给执行的必须是同一条路径。
            let script = resolve_script(path, allowed);
            if is_script_allowed(&script, allowed) {
                plat.run_script(&script.to_string_lossy(), args, *elevated)
            } else {
                // 报错里回显配置原文 + 解析结果：只给前者的话，用户没法知道
                // 我们把它当成了哪个目录下的文件
                Err(format!(
                    "脚本不在允许列表，已拒绝执行: {}（解析为 {}）",
                    path,
                    script.display()
                ))
            }
        }
    };
    ActionOutcome {
        id: a.id.clone(),
        label,
        priority: a.priority,
        ok: res.is_ok(),
        error: res.err(),
    }
}

/// 一批的收账结果。
pub(crate) struct BatchWait {
    /// 与批次等长；`None` = 这个动作没在期限内交回结果
    pub got: Vec<Option<ActionOutcome>>,
    /// 工作线程是否已全部结束。为 true 时那些 `None` 是「线程没了」（panic），
    /// 为 false 时是「还在跑，但我们不等了」—— 两者的用户处置方式完全不同。
    pub drained: bool,
}

/// 收一批结果，直到每个动作要么交回、要么超过**它自己的**等待上限。
///
/// 结果一到就交给 `on_step`，而不是攒着整批返回 —— 界面要的是「这条跑完了」，
/// 不是「这一批结束了才知道前面那条其实三分钟前就成功了」。
///
/// 单独成函数是为了让这段超时账目可测：真实的 `launch_app` / `run_script` 要么快得
/// 无法观察，要么慢得没法等，测试里造不出「刚好卡住」的那个动作。
pub(crate) fn await_batch(
    rx: &Receiver<(usize, ActionOutcome)>,
    started: Instant,
    budgets: &[Duration],
    on_step: &mut dyn FnMut(&ActionOutcome),
) -> BatchWait {
    let mut got: Vec<Option<ActionOutcome>> = vec![None; budgets.len()];
    loop {
        let elapsed = started.elapsed();
        // 还要等谁：取剩余时间最短的那个作为本次阻塞时长，到期即换下一轮判定
        let wait = got
            .iter()
            .zip(budgets)
            .filter(|(o, _)| o.is_none())
            .filter_map(|(_, b)| b.checked_sub(elapsed))
            .min();
        let Some(wait) = wait else {
            return BatchWait { got, drained: false };
        };
        match rx.recv_timeout(wait) {
            Ok((idx, outcome)) => {
                on_step(&outcome);
                got[idx] = Some(outcome);
            }
            // 某个动作到期了：回到循环顶部重算窗口（其余动作可能还有预算）
            Err(RecvTimeoutError::Timeout) => {}
            // 所有 sender 都没了 = 工作线程全部返回（含 panic）：没有结果会再来了
            Err(RecvTimeoutError::Disconnected) => return BatchWait { got, drained: true },
        }
    }
}

/// 每条动作交回结果时的回调（引擎用它把进度实时推给界面）。
pub type Step = dyn Fn(&ActionOutcome) + Send + Sync;

fn missing_outcome(a: &OneShotAction, drained: bool) -> ActionOutcome {
    ActionOutcome {
        id: a.id.clone(),
        label: label_of(&a.action),
        priority: a.priority,
        ok: false,
        error: Some(if drained {
            "动作线程异常退出".to_string()
        } else {
            format!(
                "超过 {} 秒未完成，已不再等待（动作可能仍在运行）",
                timeout_for(a).as_secs()
            )
        }),
    }
}

/// 执行一批（同 priority）：批内并发，各自受自己的超时约束。
///
/// 用「分离线程 + 通道」而不是 `thread::scope`：scope 退出时必然 join，一个卡住的
/// 动作会把整批钉在原地，超时也就无从谈起。线程 panic 时它的 sender 随之消失，
/// `await_batch` 据此把该动作记成失败，而不是让异常冒到调用方。
fn run_batch<P>(
    plat: P,
    allowed: &Arc<AllowedScripts>,
    batch: &[OneShotAction],
    on_step: &Step,
) -> Vec<ActionOutcome>
where
    P: NetworkPlatform + Copy + Send + Sync + 'static,
{
    let (tx, rx) = channel();
    let started = Instant::now();
    let mut budgets = Vec::with_capacity(batch.len());
    for (idx, a) in batch.iter().enumerate() {
        let budget = timeout_for(a);
        let (tx, plat, allowed, action) = (tx.clone(), plat, allowed.clone(), a.clone());
        std::thread::spawn(move || {
            let outcome = outcome_of(plat, &allowed, &action);
            let label = outcome.label.clone();
            if tx.send((idx, outcome)).is_err() {
                // 协调者已按超时收工。结果不再计入本次运行，但必须留下痕迹 ——
                // 否则「脚本其实跑成功了、界面却记它失败」会变成无从解释的悬案。
                log::warn(&format!("超时后动作才结束: {}", label));
            }
        });
        budgets.push(budget);
    }
    // 协调者自己那份 sender 先丢掉，否则「线程全退出」永远检测不到
    drop(tx);
    let mut arrived = |o: &ActionOutcome| on_step(o);
    let wait = await_batch(&rx, started, &budgets, &mut arrived);
    wait.got
        .into_iter()
        .enumerate()
        .map(|(idx, got)| {
            got.or_else(|| {
                let m = missing_outcome(&batch[idx], wait.drained);
                // 超时项也要推进度：它就是「这条到现在还没回来」这条信息本身
                on_step(&m);
                Some(m)
            })
            .expect("await_batch 之后每项要么有结果、要么补了超时记录")
        })
        .collect()
}

/// 按 priority 分批执行一次性动作。**会阻塞**到全部批次结束。
pub fn execute<P>(
    plat: P,
    allowed: &Arc<AllowedScripts>,
    actions: &[OneShotAction],
    on_step: &Step,
) -> BatchReport
where
    P: NetworkPlatform + Copy + Send + Sync + 'static,
{
    let mut ordered: Vec<&OneShotAction> = actions.iter().filter(|a| a.enabled).collect();
    if ordered.is_empty() {
        return BatchReport::default();
    }
    // 稳定排序：同 priority 时保持配置里的书写顺序，行为可预期。
    ordered.sort_by_key(|a| a.priority);

    let mut outcomes: Vec<ActionOutcome> = Vec::with_capacity(ordered.len());
    for batch in group_batches(&ordered) {
        let owned: Vec<OneShotAction> = batch.iter().map(|a| (*a).clone()).collect();
        outcomes.extend(run_batch(plat, allowed, &owned, on_step));
    }
    let status = status_of(&outcomes);
    BatchReport { status, outcomes }
}

/// 在独立线程上跑一次 3B1：每条动作交回结果时调 `on_step`，全部结束调 `on_done`。
///
/// 没有返回句柄，也不需要：一次运行的归因由调用方用序号自己记（见 `engine`），
/// 而线程自己会走完。持有 JoinHandle 只会诱导出「那要不要 join 一下」的回头路。
pub fn spawn<P, F>(
    plat: P,
    allowed: Arc<AllowedScripts>,
    actions: Vec<OneShotAction>,
    on_step: Arc<Step>,
    on_done: F,
) where
    P: NetworkPlatform + Copy + Send + Sync + 'static,
    F: FnOnce(BatchReport) + Send + 'static,
{
    std::thread::spawn(move || on_done(execute(plat, &allowed, &actions, on_step.as_ref())));
}

/// 这条配置里有没有需要执行的动作（全禁用时引擎不该为它占一个运行槽）。
pub fn any_enabled(actions: &[OneShotAction]) -> bool {
    actions.iter().any(|a| a.enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OneShotActionType;

    fn act(id: &str, priority: u32) -> OneShotAction {
        OneShotAction {
            id: id.to_string(),
            enabled: true,
            priority,
            action: OneShotActionType::LaunchApp {
                app: format!("/Applications/{}.app", id),
                args: vec![],
            },
        }
    }

    #[test]
    fn same_priority_forms_one_batch_and_order_is_preserved() {
        let a = act("a", 1);
        let b = act("b", 2);
        let c = act("c", 2);
        let d = act("d", 3);
        let refs: Vec<&OneShotAction> = vec![&a, &b, &c, &d];
        let batches = group_batches(&refs);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(
            batches[1].iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"],
            "同批内保持书写顺序"
        );
        assert_eq!(batches[2].len(), 1);
    }

    #[test]
    fn status_distinguishes_all_ok_all_bad_and_mixed() {
        let o = |id: &str, ok: bool| ActionOutcome {
            id: id.to_string(),
            label: String::new(),
            priority: 1,
            ok,
            error: None,
        };
        assert_eq!(status_of(&[]), BatchStatus::Empty);
        assert_eq!(status_of(&[o("a", true), o("b", true)]), BatchStatus::Success);
        assert_eq!(status_of(&[o("a", false), o("b", false)]), BatchStatus::Failed);
        assert_eq!(status_of(&[o("a", true), o("b", false)]), BatchStatus::Partial);
    }

    fn noop(_: &ActionOutcome) {}

    #[test]
    fn disabled_actions_never_reach_the_scheduler() {
        let mut off = act("off", 1);
        off.enabled = false;
        let report = execute(
            crate::platform::Platform,
            &Arc::new(AllowedScripts::default()),
            &[off],
            &noop,
        );
        assert_eq!(report.status, BatchStatus::Empty);
        assert!(report.outcomes.is_empty());
    }

    #[test]
    fn the_wait_budget_grows_with_how_much_a_human_might_be_involved() {
        let budget = |kind: OneShotActionType| timeout_for(&act_with(kind));
        assert!(
            budget(OneShotActionType::LaunchApp {
                app: "Foo".into(),
                args: vec![]
            }) < SCRIPT_TIMEOUT,
            "启动应用只是把进程丢出去，不该和脚本一样长"
        );
        assert!(
            SCRIPT_TIMEOUT < ELEVATED_SCRIPT_TIMEOUT,
            "提权脚本还要等用户在授权框上做决定"
        );
    }

    fn act_with(kind: OneShotActionType) -> OneShotAction {
        OneShotAction {
            id: "a".into(),
            enabled: true,
            priority: 1,
            action: kind,
        }
    }

    #[test]
    fn await_batch_takes_what_arrives_and_flags_what_did_not() {
        let (tx, rx) = channel();
        let outcome = |idx: usize| ActionOutcome {
            id: format!("a{idx}"),
            label: format!("label{idx}"),
            priority: idx as u32,
            ok: true,
            error: None,
        };
        tx.send((0, outcome(0))).unwrap();
        // tx 不丢：a1 就是「还在跑但没交回」的那种动作
        let started = Instant::now();
        let mut seen: Vec<String> = Vec::new();
        let wait = await_batch(
            &rx,
            started,
            &[Duration::from_millis(30), Duration::from_millis(60)],
            &mut |o| seen.push(o.id.clone()),
        );
        assert!(wait.got[0].is_some(), "先回来的那个要收下");
        assert!(wait.got[1].is_none());
        assert!(!wait.drained, "线程还在，不是没了");
        assert_eq!(seen, vec!["a0".to_string()], "到达即上报，不是攒到批尾");
        assert!(started.elapsed() >= Duration::from_millis(60), "最后一个的预算要用满");
    }

    #[test]
    fn await_batch_stops_waiting_when_every_worker_is_gone() {
        let (tx, rx) = channel::<(usize, ActionOutcome)>();
        drop(tx);
        let wait = await_batch(&rx, Instant::now(), &[Duration::from_secs(60); 2], &mut |_| {});
        assert!(wait.drained, "sender 全消失 = 不会再有结果，60 秒不该白等");
        assert!(wait.got.iter().all(|o| o.is_none()));
    }

    #[test]
    fn a_missing_result_says_whether_it_timed_out_or_the_thread_died() {
        let launch = act_with(OneShotActionType::LaunchApp {
            app: "Foo".into(),
            args: vec![],
        });
        let timed_out = missing_outcome(&launch, false);
        assert!(!timed_out.ok);
        assert!(
            timed_out.error.as_deref().unwrap().contains("仍在运行"),
            "超时要说清「我们不等了」而不是「它失败了」：{:?}",
            timed_out.error
        );
        assert!(missing_outcome(&launch, true)
            .error
            .as_deref()
            .unwrap()
            .contains("异常退出"));
    }

    #[test]
    fn labels_show_the_target_and_whether_it_needs_privileges() {
        assert_eq!(
            label_of(&OneShotActionType::RunScript {
                path: "scripts/x.sh".into(),
                args: vec![],
                elevated: true,
            }),
            "run(提权) scripts/x.sh"
        );
    }

    #[test]
    fn a_branch_with_nothing_enabled_does_not_take_a_run_slot() {
        let mut off = act("off", 1);
        off.enabled = false;
        assert!(!any_enabled(&[off.clone()]));
        assert!(any_enabled(&[off, act("on", 1)]));
    }
}
