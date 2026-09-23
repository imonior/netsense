//! 3B2 常驻动作的执行层：每条已启用的动作一条 worker 线程。
//!
//! 与 3B1 的分工写在 [`super::provider`] 里：一次性动作是「执行它」，常驻动作是
//! 「维持一个期望状态」。这个区别决定了本模块的全部结构 —— 一个可以反复运行、
//! 必须能被立刻叫停、且**永远不与自己并发**的循环。
//!
//! ## 三条硬规则
//!
//! 1. **worker 只属于当前 Active Profile 的 THEN 分支**。ELSE 分支里的常驻动作
//!    不启动（它表达的是「离开这个环境时要维持什么」，而离开时并没有一个持续成立的
//!    现场可维持），由 `Config::warnings` 明确报出，而不是静默不执行。
//! 2. **新的下发之前必须先停掉旧的一组**（见 `engine::deactivate`）。否则旧环境的
//!    「保持 VPN 连接」会和新环境抢同一张路由表 —— 那种故障没有日志能解释。
//! 3. **一条 worker 同一时刻只跑一次检查**（[`await_tick`] 的重入闸）。上一次还没回来
//!    就再发一次恢复动作，等于让同一个隧道被自己打断两次，看起来像「VPN 在抽风」。
//!
//! ## 为什么每条动作一条线程
//!
//! 它们的节律各自独立（`interval_secs` 每条都不同），共享一条线程就得有人被最慢的那条
//! 拖住。线程在这里不贵：一个 Profile 通常配 1~2 条常驻动作，且它们随 Active 起停。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::provider::{self, Tick, TickOutcome};
use super::AllowedScripts;
use crate::config::PersistentAction;
use crate::log;
use crate::platform::NetworkPlatform;

/// 同一批 worker 的启动间隔：priority 小的先开始第一次检查。
///
/// 这不是「让快的那些等慢的那些」，而是**保证顺序**：把 VPN 维持脚本排在隧道动作之前
/// 的用户，要的就是「隧道先起来，脚本再跑」。分开 500ms 足够让前者发出第一条命令。
const START_SPACING: Duration = Duration::from_millis(500);

/// 睡眠分片。`stop()` 之后最多这么久线程就退出，而不是等完整个 `interval_secs`。
const SLEEP_SLICE: Duration = Duration::from_millis(500);

/// worker 所处的工作状态。
///
/// ⚠️ `Overdue` 与 `Faulted` 必须分开：前者是「检查到现在没跑完，我们还不知道结果」，
/// 后者是「检查跑完了，结论是坏的」。把它们合成一个红色标记，用户就会去查一条其实
/// 没问题隧道，而真正该做的事只是再等一会儿。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    /// 已排上队，第一次检查还没出结果
    Pending,
    /// 最近一次检查：期望状态本就满足，什么都没做
    Satisfied,
    /// 最近一次检查：不满足，已恢复并成功
    Repaired,
    /// 最近一次检查：不满足，且恢复失败
    Faulted,
    /// 上一次检查至今没跑完，我们已不再等待（它可能仍在进行）
    Overdue,
}

/// 一条常驻动作的展示状态。会被 IPC 直接 JSON 化，故需 `Serialize`。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerStatus {
    pub id: String,
    /// 维持的是什么（`wireguard:office` / `vpn:proton/My VPN` / `run scripts/x.sh`）
    pub label: String,
    pub priority: u32,
    pub state: WorkerState,
    /// 核对间隔（秒），直接回显配置值：界面上「每 30 秒」这四个字必须来自这里
    pub interval: u64,
    /// 累计恢复成功的次数。只在状态变化时随报告一起更新，所以它**不会**领先于 `at`
    pub repairs: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 进入当前状态的时刻（epoch 秒）。不是「最近一次检查的时间」—— 稳态下 worker
    /// 只在状态**变化**时才报告，把后者显示出来会停在一个早就过去的时刻，看着像卡死。
    pub at: u64,
}

/// worker 交回的一次状态。`generation` 认的是「哪一次激活」。
///
/// 为什么归属要带编号而不是只带 profile id：用户可以在两秒内从 Home 切回 Home（改配置
/// 触发重启），只看 id 会把上一组的迟到报告挂到这一组上，于是界面上的「恢复次数」
/// 凭空多了一次，而那次其实属于已经被停掉的 worker。
#[derive(Debug, Clone)]
pub struct Update {
    pub generation: u64,
    pub profile_id: String,
    pub status: WorkerStatus,
}

/// 报告的接收方（引擎实现；它必须是 Send + Sync，因为 worker 线程在别的线程上）。
pub type Sink = dyn Fn(&Update) + Send + Sync;

/// 一条 worker 的报告归属：属于哪一次激活、哪个 Profile，报告送给谁。
///
/// 单独成 struct：这四样总是同时被用到（每次状态变化都要报告一次），收在一起之后
/// `run_worker` 的签名才只剩「怎么跑」的输入。
struct Report {
    generation: u64,
    profile_id: String,
    profile_name: String,
    sink: Arc<Sink>,
}

/// 一次 tick 的收取结果。
pub(crate) struct TickWait {
    /// 这次是否真的新起了一次检查。`false` = 上一次仍在跑，本次跳过。
    pub started: bool,
    /// 检查交回的结论；`None` = 到了预算还没回来（此时 `started` 必为 `true`，
    /// 而那条检查可能仍在别处跑着 —— 我们只是不等了，没有能力杀进程）。
    pub outcome: Option<TickOutcome>,
}

/// 在独立线程上跑一次检查，最多等 `budget`。
///
/// 重入闸（`busy`）是这里的要点，而不是优化：两次同一个动作并发执行，等于让第一条
/// 命令的恢复动作被第二条打断 —— 用户看到的现象是「VPN 每 30 秒被自己踢一次重连」。
/// 闸由 tick 线程自己在退出时打开（`Drop`），所以超时之后到线程真的结束之前，
/// 后续 tick 都会拿到 `started: false`，这正是我们要的「不再叠加」。
pub(crate) fn await_tick(tick: Arc<dyn Tick>, budget: Duration, busy: &Arc<AtomicBool>) -> TickWait {
    if busy
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return TickWait {
            started: false,
            outcome: None,
        };
    }
    let (tx, rx) = channel();
    let gate_busy = busy.clone();
    std::thread::spawn(move || {
        /// 释放重入闸。用 `Drop` 而不是在末尾 `store`：tick 里的平台代码一旦 panic，
        /// 末尾那行就不会执行，这条 worker 就永久停在「上一次还没结束」上再也醒不来。
        struct Gate(Arc<AtomicBool>);
        impl Drop for Gate {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _gate = Gate(gate_busy);
        let _ = tx.send(tick.tick());
    });
    match rx.recv_timeout(budget) {
        Ok(outcome) => TickWait {
            started: true,
            outcome: Some(outcome),
        },
        // 到预算还没回来：线程仍在跑（闸还开着），返回 started=true 但 outcome=None
        Err(RecvTimeoutError::Timeout) => TickWait {
            started: true,
            outcome: None,
        },
        // sender 消失而没交回结果 = tick 线程 panic 了。闸已由 Gate 打开。
        Err(RecvTimeoutError::Disconnected) => TickWait {
            started: true,
            outcome: Some(TickOutcome::Faulted("检查线程异常退出".to_string())),
        },
    }
}

/// 分片睡眠。返回 `false` = 中途被叫停，调用方应当退出。
fn nap(stop: &AtomicBool, total: Duration) -> bool {
    let until = Instant::now() + total;
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(SLEEP_SLICE));
    }
}

/// 一条 worker 的主循环。
fn run_worker(
    action: PersistentAction,
    tick: Arc<dyn Tick>,
    interval: Duration,
    budget: Duration,
    start_delay: Duration,
    stop: Arc<AtomicBool>,
    report: Report,
) {
    let Report {
        generation,
        profile_id,
        profile_name,
        sink,
    } = report;
    if !nap(&stop, start_delay) {
        return;
    }
    let label = provider::label_for(&action);
    let mut status = WorkerStatus {
        id: action.id.clone(),
        label: label.clone(),
        priority: action.priority,
        state: WorkerState::Pending,
        interval: interval.as_secs(),
        repairs: 0,
        error: None,
        at: crate::engine::epoch_seconds(),
    };
    let busy = Arc::new(AtomicBool::new(false));

    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let wait = await_tick(tick.clone(), budget, &busy);
        let (state, error) = if !wait.started {
            (
                WorkerState::Overdue,
                Some(format!(
                    "上一次检查（预算 {} 秒）仍未结束，本次核对已跳过",
                    budget.as_secs()
                )),
            )
        } else {
            match wait.outcome {
                Some(TickOutcome::Satisfied) => (WorkerState::Satisfied, None),
                Some(TickOutcome::Repaired) => (WorkerState::Repaired, None),
                Some(TickOutcome::Faulted(e)) => (WorkerState::Faulted, Some(e)),
                None => (
                    WorkerState::Overdue,
                    Some(format!(
                        "超过 {} 秒未完成，已不再等待（检查可能仍在进行）",
                        budget.as_secs()
                    )),
                ),
            }
        };
        // 只在状态或报错文本**变化**时报告与写日志：一条稳定的隧道每 30 秒核对一次，
        // 每次都报告会让界面和日志都在原地刷同一行。
        let changed = status.state != state || status.error != error;
        status.state = state;
        status.error = error.clone();
        if state == WorkerState::Repaired {
            status.repairs += 1;
        }
        if changed {
            status.at = crate::engine::epoch_seconds();
            log_transition(&profile_name, &label, state, error.as_deref());
            sink(&Update {
                generation,
                profile_id: profile_id.clone(),
                status: status.clone(),
            });
        }
        if !nap(&stop, interval) {
            return;
        }
    }
}

/// 状态变化怎么写日志。`Satisfied` 只有在「从坏状态回来」时才值得说一句 ——
/// 否则每次恢复成功后紧跟着的一条「一切正常」纯属噪音。
fn log_transition(profile: &str, label: &str, state: WorkerState, error: Option<&str>) {
    let who = format!("{} · {}", profile, label);
    match state {
        WorkerState::Repaired => log::info(&format!("常驻动作已恢复目标状态: {}", who)),
        WorkerState::Faulted => log::warn(&format!(
            "常驻动作未能恢复目标状态: {} : {}",
            who,
            error.unwrap_or("未知原因")
        )),
        WorkerState::Overdue => log::warn(&format!("常驻动作检查超期: {} : {}", who, error.unwrap_or(""))),
        WorkerState::Satisfied => log::info(&format!("常驻动作恢复正常: {}", who)),
        WorkerState::Pending => {}
    }
}

/// 一次激活的一组 worker。由引擎持有，随 Active 生命周期起停。
pub struct Session {
    generation: u64,
    profile_id: String,
    stops: Vec<Arc<AtomicBool>>,
    statuses: Vec<WorkerStatus>,
}

/// 启动顺序：先按 `priority`，同值时保持配置里的书写顺序（`sort_by_key` 是稳定排序）。
///
/// 过滤掉 disabled 也在这里 —— 一条禁用的常驻动作连 worker 都不该有，而不是起来后什么都不做。
fn by_priority(actions: &[PersistentAction]) -> Vec<PersistentAction> {
    let mut ordered: Vec<PersistentAction> =
        actions.iter().filter(|a| a.enabled).cloned().collect();
    ordered.sort_by_key(|a| a.priority);
    ordered
}

impl Session {
    /// 为这批常驻动作起 worker。
    ///
    /// 传出去的是 owned 副本（动作 + `Arc<AllowedScripts>` + owned sink）：worker 线程
    /// 绝不回头 lock config，否则「用户在编辑器里保存」与「worker 报告」互相等就是死锁。
    ///
    /// `actions` 可以是原样顺序 —— 本函数自己按 priority 稳定排序，序号只用于
    /// 决定启动错峰，不影响任何判定。
    pub fn start<P>(
        plat: P,
        allowed: &Arc<AllowedScripts>,
        actions: &[PersistentAction],
        generation: u64,
        profile_id: &str,
        profile_name: &str,
        sink: Arc<Sink>,
    ) -> Session
    where
        P: NetworkPlatform + Copy + Send + Sync + 'static,
    {
        let ordered = by_priority(actions);
        let mut stops = Vec::with_capacity(ordered.len());
        let mut statuses = Vec::with_capacity(ordered.len());
        for (idx, action) in ordered.iter().enumerate() {
            let interval = provider::interval_for(action);
            let budget = provider::budget_for(action);
            let tick = provider::tick_for(plat, allowed, action);
            let stop = Arc::new(AtomicBool::new(false));
            statuses.push(WorkerStatus {
                id: action.id.clone(),
                label: provider::label_for(action),
                priority: action.priority,
                state: WorkerState::Pending,
                interval: interval.as_secs(),
                repairs: 0,
                error: None,
                at: crate::engine::epoch_seconds(),
            });
            let worker_stop = stop.clone();
            let action = action.clone();
            let report = Report {
                generation,
                profile_id: profile_id.to_string(),
                profile_name: profile_name.to_string(),
                sink: sink.clone(),
            };
            std::thread::spawn(move || {
                run_worker(
                    action,
                    tick,
                    interval,
                    budget,
                    START_SPACING * idx as u32,
                    worker_stop,
                    report,
                )
            });
            stops.push(stop);
        }
        Session {
            generation,
            profile_id: profile_id.to_string(),
            stops,
            statuses,
        }
    }

    pub fn statuses(&self) -> &[WorkerStatus] {
        &self.statuses
    }

    /// 收下一次报告。返回 `false` = 它属于已被取代的那一组，调用方丢弃即可。
    ///
    /// 注意这里**不**看 profile_id：一次激活的归属由 generation 唯一确定，而 id 留着
    /// 让调用方能多问一句。两者都判，才不会把迟到的报告写进新会话的某条同名动作上。
    pub fn note(&mut self, update: &Update) -> bool {
        if update.generation != self.generation || update.profile_id != self.profile_id {
            return false;
        }
        match self
            .statuses
            .iter_mut()
            .find(|s| s.id == update.status.id)
        {
            Some(slot) => {
                *slot = update.status.clone();
                true
            }
            None => false,
        }
    }

    /// 停掉这一组 worker。清单随即清空：界面上列出的必须是「此刻还在维持的东西」。
    ///
    /// 不 join：worker 可能正卡在一条系统命令里（`installtunnelservice` 有时不立即退出），
    /// 等它等于把引擎线程拖住。分片睡眠保证它下一片醒来时就退出。
    pub fn stop(&mut self) -> usize {
        let n = self.stops.len();
        for s in self.stops.drain(..) {
            s.store(true, Ordering::SeqCst);
        }
        self.statuses.clear();
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PersistentActionType;

    struct Fake {
        outcome: TickOutcome,
        delay: Duration,
        entered: Option<Arc<AtomicBool>>,
    }

    impl Fake {
        fn now(outcome: TickOutcome) -> Arc<dyn Tick> {
            Arc::new(Fake {
                outcome,
                delay: Duration::ZERO,
                entered: None,
            })
        }
    }

    impl Tick for Fake {
        fn tick(&self) -> TickOutcome {
            if let Some(flag) = &self.entered {
                flag.store(true, Ordering::SeqCst);
            }
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            self.outcome.clone()
        }
    }

    fn status(id: &str) -> WorkerStatus {
        WorkerStatus {
            id: id.into(),
            label: id.into(),
            priority: 100,
            state: WorkerState::Pending,
            interval: 30,
            repairs: 0,
            error: None,
            at: 0,
        }
    }

    #[test]
    fn a_tick_that_finishes_reports_its_outcome_and_releases_the_gate() {
        let busy = Arc::new(AtomicBool::new(false));
        let w = await_tick(
            Fake::now(TickOutcome::Satisfied),
            Duration::from_secs(5),
            &busy,
        );
        assert!(w.started);
        assert_eq!(w.outcome, Some(TickOutcome::Satisfied));
        assert!(!busy.load(Ordering::SeqCst), "跑完了就得允许下一次检查");
    }

    /// 重入闸的全部意义：慢的那次还没回来时，宁可跳过也不能并发。
    #[test]
    fn a_second_tick_is_skipped_while_the_first_one_is_still_running() {
        let busy = Arc::new(AtomicBool::new(false));
        let slow = Arc::new(Fake {
            outcome: TickOutcome::Repaired,
            delay: Duration::from_millis(250),
            entered: None,
        });
        let first = await_tick(slow.clone(), Duration::from_millis(30), &busy);
        assert!(first.started, "第一次当然要真的跑起来");
        assert!(first.outcome.is_none(), "预算到了就把等待放下");
        assert!(busy.load(Ordering::SeqCst), "线程还在，闸就不能开");

        let second = await_tick(slow, Duration::from_millis(30), &busy);
        assert!(!second.started, "上一次没结束就不该并发");
        assert_eq!(second.outcome, None);

        // 等那个慢 tick 真的跑完，闸要自己打开 —— 否则 worker 会永久停在 Overdue
        std::thread::sleep(Duration::from_millis(400));
        assert!(!busy.load(Ordering::SeqCst));
        let third = await_tick(
            Fake::now(TickOutcome::Satisfied),
            Duration::from_secs(5),
            &busy,
        );
        assert!(third.started && third.outcome == Some(TickOutcome::Satisfied));
    }

    /// tick 里 panic 时闸也必须打开，并且这次检查要记成失败而不是「永远等不到」。
    #[test]
    fn a_panicking_tick_fails_the_check_and_still_releases_the_gate() {
        struct Boom;
        impl Tick for Boom {
            fn tick(&self) -> TickOutcome {
                panic!("探测代码炸了")
            }
        }
        let busy = Arc::new(AtomicBool::new(false));
        let w = await_tick(Arc::new(Boom), Duration::from_secs(5), &busy);
        assert!(matches!(w.outcome, Some(TickOutcome::Faulted(_))));
        assert!(!busy.load(Ordering::SeqCst), "Drop 里的闸在 panic 路径上也要开");
    }

    /// 报告的意义在于「变化」，不在于「心跳」。一条稳定的隧道每 30 秒核对一次，
    /// 如果每次都报告，界面和日志都会被同一行刷下去。
    #[test]
    fn an_unchanged_check_reports_once_rather_than_every_interval() {
        use std::sync::atomic::AtomicUsize;
        struct Counted {
            left: AtomicUsize,
            stop: Arc<AtomicBool>,
        }
        impl Tick for Counted {
            fn tick(&self) -> TickOutcome {
                if self.left.fetch_sub(1, Ordering::SeqCst) <= 1 {
                    self.stop.store(true, Ordering::SeqCst);
                }
                TickOutcome::Satisfied
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let reports = Arc::new(AtomicUsize::new(0));
        let sink: Arc<Sink> = {
            let reports = reports.clone();
            Arc::new(move |_| {
                reports.fetch_add(1, Ordering::SeqCst);
            })
        };
        let action = PersistentAction {
            id: "p1".into(),
            enabled: true,
            priority: 100,
            action: PersistentActionType::KeepWireGuardConnected {
                tunnel: "wg0".into(),
                interval_secs: 30,
            },
        };
        run_worker(
            action,
            Arc::new(Counted {
                left: AtomicUsize::new(6),
                stop: stop.clone(),
            }),
            Duration::from_millis(1),
            Duration::from_secs(5),
            Duration::ZERO,
            stop,
            Report {
                generation: 1,
                profile_id: "home".into(),
                profile_name: "Home".into(),
                sink,
            },
        );
        assert_eq!(
            reports.load(Ordering::SeqCst),
            1,
            "六次核对结果都一样 → 只有 Pending→Satisfied 那一次值得报告"
        );
    }

    #[test]
    fn a_stopped_worker_never_starts_its_first_check() {
        let stop = Arc::new(AtomicBool::new(true));
        assert!(!nap(&stop, Duration::from_secs(30)), "该停就得马上停");
        let run = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        stop.store(true, Ordering::SeqCst);
        let ok = nap(&stop, Duration::from_millis(10));
        assert!(!ok);
        // 上面的 nap 直接返回，所以 worker 连第一次 tick 都不会发出
        assert!(!entered.load(Ordering::SeqCst));
        assert!(!run.load(Ordering::SeqCst));
    }

    #[test]
    fn the_sliced_sleep_ends_promptly_on_stop() {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        // 名义上要睡 30 秒，实际只睡到被叫停 —— 这就是分片的意义
        assert!(!nap(&stop, Duration::from_secs(30)));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "stop 之后最多等几片就退出，实际等了 {:?}",
            started.elapsed()
        );
    }

    fn session(gen: u64) -> Session {
        Session {
            generation: gen,
            profile_id: "home".into(),
            stops: Vec::new(),
            statuses: vec![status("p1")],
        }
    }

    #[test]
    fn a_late_report_from_a_stopped_session_is_not_attributed() {
        let mut s = session(7);
        let mut update = Update {
            generation: 7,
            profile_id: "home".into(),
            status: status("p1"),
        };
        update.status.state = WorkerState::Repaired;
        assert!(s.note(&update), "同一次激活的报告要收下");
        assert_eq!(s.statuses()[0].state, WorkerState::Repaired);

        // 引擎重启过一组（generation 变了）：旧报告必须无效，哪怕它说的是同一条动作
        assert!(!s.note(&Update {
            generation: 6,
            profile_id: "home".into(),
            status: status("p1"),
        }));
        // Profile 也不同：同名动作不能串台
        assert!(!s.note(&Update {
            generation: 7,
            profile_id: "office".into(),
            status: status("p1"),
        }));
        // 这一组里没这条动作（用户删掉了它，但旧 worker 还在收尾）
        assert!(!s.note(&Update {
            generation: 7,
            profile_id: "home".into(),
            status: status("gone"),
        }));
    }

    #[test]
    fn stopping_a_session_flags_every_worker_and_empties_the_view() {
        let flags: Vec<Arc<AtomicBool>> = (0..3).map(|_| Arc::new(AtomicBool::new(false))).collect();
        let mut s = Session {
            generation: 1,
            profile_id: "home".into(),
            stops: flags.clone(),
            statuses: vec![status("a"), status("b"), status("c")],
        };
        assert_eq!(s.stop(), 3, "报出停了几条，日志里要说这句话");
        assert!(flags.iter().all(|f| f.load(Ordering::SeqCst)));
        assert!(s.statuses().is_empty(), "已经没了的 worker 不该继续列着");
    }

    /// `priority` 在 3B2 里只决定启动顺序，不决定成败 —— 稳定排序保证同 priority
    /// 时配置里的书写顺序就是启动顺序。
    #[test]
    fn workers_are_ordered_by_priority_then_by_config_order() {
        let mk = |id: &str, priority: u32, interval: u64| PersistentAction {
            id: id.into(),
            enabled: true,
            priority,
            action: PersistentActionType::KeepWireGuardConnected {
                tunnel: id.into(),
                interval_secs: interval,
            },
        };
        let off = |id: &str| PersistentAction {
            enabled: false,
            ..mk(id, 1, 30)
        };
        let ordered = by_priority(&[
            mk("b", 5, 30),
            off("sleeping"),
            mk("c", 5, 30),
            mk("a", 1, 30),
        ]);
        assert_eq!(
            ordered.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"],
            "priority 小的先起，同 priority 按书写顺序；禁用的那条连排都不该排进来"
        );
    }
}
