//! 引擎：把「网络 → 评估 → 唯一 Active / Conflict / 零命中」这条主链集中在一处，
//! 并保证**同一时刻最多只有一个 Active Profile**。
//!
//! 所有状态变更都只发生在**引擎线程**上（外部入口只能往 `Msg` 通道投递请求）。
//! 这不是风格问题：入口一旦同时被 SSID 线程、热重载线程和 IPC 调用，就是三处并发写
//! 「当前 Profile」与幂等指纹。而切换是有顺序的（停监测 → 下发 → 校验 →
//! 一次性动作 → 起监测），并发就会出现「新环境的配置被旧环境的回落动作覆盖」这类
//! 查不出根因的故障。
//!
//! ## 加锁纪律（违反就是自死锁：std Mutex 不可重入）
//!
//! - 顺序恒为 `engine` → `config`，且**只在 `pass()` 内部**同时持有两者；
//! - 平台 I/O（采样、下发、探测）一律在**不持锁**时做；
//! - 广播（`publish_status`）只能在锁全部释放之后调用。
//!
//! 3B1 一次性动作既不在引擎线程、也不碰任何锁：提交时把「动作清单 + allow-list」
//! 的 owned 副本交给独立线程，跑完用 `Msg::RunDone` 把报告送回。原因很实际 ——
//! 一个动作可以等上几分钟（提权脚本还在等用户点授权框），而引擎线程是唯一能响应
//! 网络变化、下发 3A 的地方；留在引擎里跑，一个卡住的脚本就等于整个应用失去反应。
//!
//! 3B2 常驻动作走同一套投递方式（`Msg::Worker`），但生命周期是**跟着 Active 起停**，
//! 不是一次跑完就结束：`Engine::workers` 持有当前那一组 worker，任何一次新的下发
//! 之前都会先把旧的一组叫停 —— 否则旧环境的「保持 VPN 连接」会去抢新环境的路由表，
//! 那是一种日志解释不了的故障（另见 `automation::persistent` 模块头）。
//!
//! ## 四条硬规则
//!
//! 1. **多命中 = Conflict，不自动选择**（方案第 5/6/10 条）。Profile 之间既没有
//!    priority 也没有「更具体者胜」：两个都合理的 Profile 静默二选一，用户完全看不出
//!    自己配重了。
//! 2. **保持 Active 时不重跑 3A / 3B1**（方案第 32/36 条）：只有该 Profile 的**内容**
//!    变了才重下网络配置，且重下不带动作。
//! 3. **3A 失败 → 3B 一条都不执行**（方案第 16/20 条），该 Profile 标 ERROR。
//! 4. **一次只认最新那次 3B1 运行**：新提交会**取代**（不是打断）仍在跑的那次 ——
//!    我们无法安全地杀掉用户的进程，所以旧动作继续跑完，只是它回来时报告不再算数。
//!    选「取代」而不是「排队」：排队的后果是新环境的动作排在旧环境后面，而用户已经
//!    不在旧环境了。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
// 本工具链的 `std::sync::mpsc` 没有 `unbounded_channel`，用 `channel` 即可：
// 这里每条消息只是一次「请走一轮」的提醒，队列长度天然有限（消费者每秒醒一次并 try_recv 收干）。
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use crate::automation::{self, one_shot, persistent, AllowedScripts};
use crate::conditions::{
    evaluate_all, eval_profile, Evaluation, NetworkSnapshot, ProfileEvaluation,
};
use crate::config::{Branch, Config, Profile};
use crate::detection::{self, Scheduler};
use crate::i18n;
use crate::log;
use crate::network::{self, HealthMonitor, Stage3A};
use crate::platform::NetworkPlatform as _;
use crate::state::AppState;

/// 引擎线程接收的消息。所有外部入口（IPC、配置改动、退出）都只能从这里进来。
#[derive(Debug, Clone)]
pub enum Msg {
    /// 有东西变了（配置落盘 / 网络事件），请重新走一轮
    Wake,
    /// 用户点「立即应用」
    Apply { id: String },
    /// 「设为 DHCP」「立即探测」这类后台动作：都涉及提权或秒级等待，
    /// 必须挪到引擎线程，不能让 IPC 工作线程与托盘回调各起一个线程抢同一张网卡。
    Action { kind: ActionKind },
    /// 一条 3B1 动作交回了结果（不论成败）。
    RunStep {
        seq: u64,
        outcome: one_shot::ActionOutcome,
    },
    /// 一次后台 3B1 跑完了。引擎是它的唯一消费者，报告按 `seq` 归因。
    RunDone {
        seq: u64,
        report: one_shot::BatchReport,
    },
    /// 一条常驻 worker 的状态变了（只在真的变了时才报告，所以这条不会刷屏）。
    Worker {
        update: persistent::Update,
    },
    Quit,
}

#[derive(Debug, Clone, Copy)]
pub enum ActionKind {
    SetDhcp,
    Probe,
}

/// Profile 在 UI 上的状态。
///
/// ⚠️ **Conflict 与 ERROR 必须严格区分**（方案第 3 条）：前者是条件层面「同时命中」，
/// 处置方式是改条件；后者是执行层面「下发/校验出错」，处置方式是看日志和权限。
/// 把两者混成一个黄色标记，用户就会去改本来正确的条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayStatus {
    /// 唯一命中且已生效
    Active,
    /// 启用但条件不匹配
    NotMatched,
    /// Profile 被禁用
    Disabled,
    /// 与其它 Profile 同时命中
    Conflict,
    /// 命中了，但执行过程出错
    Error,
}

/// 系统层面的判定结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Decision {
    /// 零命中
    NoActiveProfile,
    /// 恰好一个命中 —— 唯一可以进入 Active 的情况
    Active { id: String },
    /// 两个以上命中 —— 不自动选择，提示用户
    Conflict { ids: Vec<String> },
}

/// 哪一支分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Which {
    Then,
    Else,
}

impl Which {
    pub fn name(self) -> &'static str {
        match self {
            Which::Then => "THEN",
            Which::Else => "ELSE",
        }
    }
    fn of(self, p: &Profile) -> Option<&Branch> {
        match self {
            Which::Then => p.then.as_ref(),
            Which::Else => p.else_branch.as_ref(),
        }
    }
}

/// 纯判定：命中数决定一切，**不看 priority、不看「谁更具体」**。
pub fn decide(matched_ids: &[String]) -> Decision {
    match matched_ids.len() {
        0 => Decision::NoActiveProfile,
        1 => Decision::Active {
            id: matched_ids[0].clone(),
        },
        _ => Decision::Conflict {
            ids: matched_ids.to_vec(),
        },
    }
}

/// 3A 失败后的「别再重试」记档。
///
/// 不记档会怎样：本次激活没成功，于是每一轮评估都判定「还没 Active」并重新下发 ——
/// 用户每 30 秒被授权框打断一次。记「配置内容 + 网络指纹」而不是一个布尔，
/// 是为了让「网络变了」或「用户改了配置」都能自动恢复重试。
#[derive(Debug, Clone)]
struct Blocked {
    id: String,
    config_fp: String,
    fingerprint: String,
}

/// 执行一支分支的开关。
#[derive(Debug, Clone, Copy)]
pub struct RunOpts {
    /// 是否执行 3B1 一次性动作（保持 Active 期间的重下要置 false）
    pub one_shot: bool,
    /// 是否接管持续健康监测（只有成为 Active 的 THEN 分支才该接管）
    pub monitor: bool,
}

impl RunOpts {
    /// 完整激活：下发 + 校验 + 动作 + 监测
    pub const FULL: RunOpts = RunOpts {
        one_shot: true,
        monitor: true,
    };
    /// 配置热重载导致的「同一个 Active Profile 内容变了」：只重下网络，不重跑动作
    pub const RECONFIGURE: RunOpts = RunOpts {
        one_shot: false,
        monitor: true,
    };
    /// 立即应用：跑动作，但不接管监测（Active 归属仍由匹配决定）
    pub const MANUAL: RunOpts = RunOpts {
        one_shot: true,
        monitor: false,
    };
}

/// 一次已提交、尚未交回的 3B1 运行。
///
/// 记下归属（哪个 Profile 的哪一支）而不是只有结果：动作失败要在「它本该属于的那次
/// 应用」的上下文里说才有意义，而报告回来时可能已经换了 Profile。
#[derive(Debug, Clone)]
pub struct RunningRun {
    pub seq: u64,
    pub profile_id: String,
    pub profile_name: String,
    pub branch: Which,
}

/// 这次执行里 3A 做了什么。
///
/// 没有 `Failed` 这个取值，这不是遗漏：3A 失败时 3B 一条都不执行，也就没有一次
/// 「执行」可留痕，失败原因走 `DisplayStatus::Error` + `ProfileView.error`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreeAOutcome {
    /// 这一支没配网络配置，直接进 3B
    Skipped,
    /// 下发并回读校验通过
    Applied,
}

/// 最近一次「走到 3B」的执行留痕。
///
/// 为什么要有它：动作的结果原本只进日志文件，而日志是**给排查用的**，不是界面。
/// 用户点完「立即应用」看到的应该是「哪条动作成功、哪条超时」，不是「自己去翻日志」。
///
/// `outcomes` 是**边跑边长**的：每条动作一交回结果就推进来一条，所以「运行中」也能
/// 看到已完成的那几条，而不是等整批跑完才一次性刷新。
#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    /// 归因用：这份记录对应第几次提交。前端不需要。
    #[serde(skip)]
    pub seq: u64,
    pub profile_id: String,
    pub profile: String,
    pub branch: Which,
    /// 提交时刻（epoch 秒）。用墙上时间而不是 `Instant`：UI 要显示「几点跑的」，
    /// 而 `Instant` 的零点每台机器、每次启动都不一样。
    pub at: u64,
    pub three_a: ThreeAOutcome,
    /// `true` = 还有动作在后台线程上跑
    pub running: bool,
    /// 整批判定；`None` = 还没到能下结论的时候。与 `DisplayStatus` 无关 ——
    /// 它描述的是「这批动作」，不是「这个 Profile」。
    pub status: Option<one_shot::BatchStatus>,
    /// 这批里启用的动作条数。运行中时 `outcomes` 只有已交回的那几条，没有这个分母
    /// 界面就报不出「3/5」，只能说「有 3 条结果」—— 而用户想知道的恰恰是还剩几条。
    pub total: usize,
    pub outcomes: Vec<one_shot::ActionOutcome>,
}

impl RunRecord {
    fn begin(
        seq: u64,
        profile: &Profile,
        branch: Which,
        three_a: ThreeAOutcome,
        total: usize,
    ) -> Self {
        RunRecord {
            seq,
            profile_id: profile.id.clone(),
            profile: profile.name.clone(),
            branch,
            at: epoch_seconds(),
            three_a,
            running: true,
            status: None,
            total,
            outcomes: Vec::new(),
        }
    }
}

/// 墙上时间（epoch 秒）。时钟倒退到 1970 之前是不可能的，取不到就用 0：
/// 这个值只用于展示「几点跑的」，不参与任何判定。
pub(crate) fn epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 引擎状态。除构造外只由引擎线程改写。
pub struct Engine {
    pub snapshot: NetworkSnapshot,
    pub evaluation: Evaluation,
    pub decision: Decision,
    scheduler: Scheduler,
    last_sample: Option<Instant>,
    first_pass_done: bool,
    /// 当前生效的 Profile id
    active_id: Option<String>,
    /// 上次成功下发的 Profile 内容指纹（内容没变就别再弹一次授权框）
    applied_fp: Option<String>,
    /// 上次应用的 fallback 内容指纹。与 `applied_fp` **互相作废**：
    /// 下发过任何 Profile 配置就得清空 fallback 记档 —— 否则「Home 下了静态 IP →
    /// 到了零命中的咖啡馆」会因为 fallback 内容与启动时那次相同而被跳过，
    /// 网络永远停在 Home 的静态地址上。
    fallback_fp: Option<String>,
    blocked: Option<Blocked>,
    /// 每个 Profile 最近一次执行错误。只保留「仍然命中」或「仍是 Active」的条目，
    /// 否则一次偶发失败会永久挂着红叉。
    errors: HashMap<String, String>,
    health_stop: Arc<AtomicBool>,
    monitoring: bool,
    /// 已提交、仍在后台跑的 3B1（`None` = 没有）。
    running_run: Option<RunningRun>,
    /// 已提交过的运行编号，单调递增 —— 用它认得出「迟到的结果属于哪一次」。
    run_seq: u64,
    /// 最近一次走到 3B 的执行留痕（可能仍在跑）。
    last_run: Option<RunRecord>,
    /// 当前 Active 的那一组 3B2 worker；`None` = 此刻什么都没在维持。
    workers: Option<persistent::Session>,
    /// 已发放的 worker 组编号，单调递增 —— 迟到的报告靠它认出属于哪一次激活。
    worker_gen: u64,
    /// 已提示过的冲突集合（避免每轮评估重复弹窗）
    conflict_shown: String,
    warnings: Vec<String>,
}

impl Default for Engine {
    fn default() -> Self {
        Engine {
            snapshot: NetworkSnapshot::default(),
            evaluation: Evaluation::default(),
            decision: Decision::NoActiveProfile,
            scheduler: Scheduler::default(),
            last_sample: None,
            first_pass_done: false,
            active_id: None,
            applied_fp: None,
            fallback_fp: None,
            blocked: None,
            errors: HashMap::new(),
            health_stop: Arc::new(AtomicBool::new(false)),
            monitoring: false,
            running_run: None,
            run_seq: 0,
            last_run: None,
            workers: None,
            worker_gen: 0,
            conflict_shown: String::new(),
            warnings: Vec::new(),
        }
    }
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// 采样是否到期。真正的 I/O 由调用方在**不持锁**时做，所以这里只做判断。
    pub fn sample_due(&self, now: Instant) -> bool {
        match self.last_sample {
            None => true,
            Some(at) => now.duration_since(at) >= detection::SAMPLE_INTERVAL,
        }
    }

    pub fn note_sampled(&mut self, snap: NetworkSnapshot, now: Instant) {
        self.last_sample = Some(now);
        self.snapshot = snap;
        self.scheduler.observe(&self.snapshot, now);
    }

    /// 本轮到期的 Profile（空 = 什么都不用做）。
    pub fn due(&self, cfg: &Config, now: Instant) -> Vec<String> {
        self.scheduler.due(&cfg.profiles, now, !self.first_pass_done)
    }

    /// 走一轮评估并推进节律。
    pub fn evaluate(&mut self, cfg: &Config, now: Instant) {
        let due = self.due(cfg, now);
        self.evaluation = evaluate_all(&cfg.profiles, &self.snapshot);
        self.first_pass_done = true;
        self.scheduler.after_evaluation(&cfg.profiles, &due, now);
        self.decision = decide(&self.evaluation.matched_ids);
        let fingerprint = self.scheduler.fingerprint().to_string();
        self.prune_errors(&fingerprint);
    }

    /// 只保留还说得过去的错误：命中的 Profile 才有资格是 ERROR；网络指纹变了说明
    /// 现场已经不同，旧错误也不该继续挂着。
    fn prune_errors(&mut self, fingerprint: &str) {
        let stale_fingerprint = self
            .blocked
            .as_ref()
            .map(|b| b.fingerprint != fingerprint)
            .unwrap_or(false);
        if stale_fingerprint {
            self.blocked = None;
        }
        let matched = self.evaluation.matched_ids.clone();
        let active = self.active_id.clone();
        self.errors
            .retain(|id, _| matched.contains(id) || active.as_deref() == Some(id.as_str()));
    }

    pub fn set_warnings(&mut self, w: Vec<String>) {
        self.warnings = w;
    }

    /// 单个 Profile 的展示状态。
    pub fn status_of(&self, ev: &ProfileEvaluation) -> DisplayStatus {
        if !ev.enabled {
            return DisplayStatus::Disabled;
        }
        if self.errors.contains_key(&ev.id) {
            return DisplayStatus::Error;
        }
        match &self.decision {
            Decision::Conflict { ids } if ids.contains(&ev.id) => DisplayStatus::Conflict,
            _ => {
                if self.active_id.as_deref() == Some(ev.id.as_str()) {
                    DisplayStatus::Active
                } else if ev.matched {
                    // 命中但既不是 Active 也没进冲突名单 = 被 3A 失败挡住了
                    DisplayStatus::Error
                } else {
                    DisplayStatus::NotMatched
                }
            }
        }
    }

    /// 交给前端的完整状态视图（IPC 查询与事件广播共用一份构造代码）。
    pub fn view(&self, cfg: &Config) -> EngineView {
        let name_of = |id: &str| {
            cfg.profile_by_id(id)
                .map(|p| p.name.clone())
                .or_else(|| {
                    self.evaluation
                        .profiles
                        .iter()
                        .find(|p| p.id == id)
                        .map(|p| p.name.clone())
                })
                .unwrap_or_else(|| id.to_string())
        };
        let profiles: Vec<ProfileView> = self
            .evaluation
            .profiles
            .iter()
            .map(|ev| ProfileView {
                id: ev.id.clone(),
                name: ev.name.clone(),
                enabled: ev.enabled,
                matched: ev.matched,
                status: self.status_of(ev),
                error: self.errors.get(&ev.id).cloned(),
                rules: ev.rules.clone(),
            })
            .collect();
        let (active, conflict) = match &self.decision {
            Decision::Active { id } => (Some(name_of(id)), Vec::new()),
            Decision::Conflict { ids } => (None, ids.iter().map(|i| name_of(i)).collect()),
            Decision::NoActiveProfile => (None, Vec::new()),
        };
        EngineView {
            state: self.decision.clone(),
            active,
            conflict,
            snapshot: self.snapshot.clone(),
            profiles,
            warnings: self.warnings.clone(),
            last_run: self.last_run.clone(),
            // 只列此刻真在维持的东西：停掉的 worker 报的是「已经没了」，留着会在界面上
            // 挂着一排旧状态，看起来像它们还在跑。
            workers: self
                .workers
                .as_ref()
                .map(|s| s.statuses().to_vec())
                .unwrap_or_default(),
        }
    }

    fn stop_monitor(&mut self) {
        if self.monitoring {
            self.health_stop.store(true, Ordering::SeqCst);
            self.health_stop = Arc::new(AtomicBool::new(false));
            self.monitoring = false;
        }
    }

    fn scheduler_reset(&mut self) {
        self.scheduler.reset();
        self.first_pass_done = false;
    }

    /// 叫停当前这一组常驻 worker。返回叫停条数（0 = 本来就没有）。
    ///
    /// 不 join、也不等：worker 可能正卡在某条系统命令里（`installtunnelservice` 有时
    /// 不立即退出），而这里的大部分调用点紧接着就要下发新的 3A —— 等它等于让一次网络
    /// 切换挂在一个无人值守的超时上。分片睡眠保证它最多一片之后就什么都不再发。
    fn stop_workers(&mut self) -> usize {
        let Some(session) = self.workers.as_mut() else {
            return 0;
        };
        let n = session.stop();
        self.workers = None;
        if n > 0 {
            log::debug(&format!("已叫停 {} 条常驻动作 worker", n));
        }
        n
    }

    /// 为这一支分支的常驻动作起一组 worker。**只有 THEN 分支会走到这里** ——
    /// ELSE 表达的是「离开这个环境时要维持什么」，而离开时并没有一个持续成立的现场。
    ///
    /// 传出去的全是 owned 副本（动作 + `Arc<AllowedScripts>` + owned sink），worker
    /// 线程绝不回头 lock config：否则「用户在编辑器里保存」与「worker 报告」互相等就是死锁。
    fn start_workers(
        &mut self,
        state: &Arc<AppState>,
        profile: &Profile,
        branch: &Branch,
        allowed: &Arc<AllowedScripts>,
    ) {
        if !branch.persistent.iter().any(|a| a.enabled) {
            return;
        }
        let Some(tx) = state.engine_tx.get().cloned() else {
            log::error("引擎通道尚未就绪，常驻动作 worker 未启动");
            return;
        };
        self.worker_gen += 1;
        let sink: Arc<persistent::Sink> = Arc::new(move |u: &persistent::Update| {
            let _ = tx.send(Msg::Worker {
                update: u.clone(),
            });
        });
        let session = persistent::Session::start(
            state.plat,
            allowed,
            &branch.persistent,
            self.worker_gen,
            &profile.id,
            &profile.name,
            sink,
        );
        log::info(&format!(
            "常驻动作 worker 已启动 {} 条: {} · gen {}",
            session.statuses().len(),
            profile.name,
            self.worker_gen
        ));
        self.workers = Some(session);
    }

    /// 归因一条 worker 报告。返回 false = 它属于已被取代的那一组，丢弃即可。
    fn note_worker(&mut self, update: &persistent::Update) -> bool {
        match &mut self.workers {
            Some(session) => session.note(update),
            None => false,
        }
    }

    fn is_blocked(&mut self, id: &str, config_fp: &str, fingerprint: &str) -> bool {
        let keep = match &self.blocked {
            Some(b) => b.id == id && b.config_fp == config_fp && b.fingerprint == fingerprint,
            None => false,
        };
        if !keep {
            self.blocked = None;
        }
        keep
    }

    fn mark_applied(&mut self, fp: String) {
        self.applied_fp = Some(fp);
        self.fallback_fp = None;
        self.blocked = None;
    }

    fn record_failure(&mut self, id: &str, config_fp: String, reason: String) {
        let fingerprint = self.scheduler.fingerprint().to_string();
        self.errors.insert(id.to_string(), reason);
        self.blocked = Some(Blocked {
            id: id.to_string(),
            config_fp,
            fingerprint,
        });
        self.stop_monitor();
        // 常驻 worker 不必在这里停：每一条能走到 `record_failure` 的路都先过了
        // `execute_branch` 的开头，那里已经在新 3A 下发之前叫停了旧的一组。
        self.active_id = None;
        self.applied_fp = None;
    }

    /// 离开当前 Profile：停监测、叫停常驻 worker、清记档。
    ///
    /// 停 worker 必须发生在任何新下发**之前**（方案第 31/33 条）：旧环境的
    /// 「保持 VPN 连接」会去抢新环境的路由，而两边各自的日志都写着成功。
    fn deactivate(&mut self) {
        self.stop_monitor();
        self.stop_workers();
        self.active_id = None;
        self.applied_fp = None;
    }

    /// 执行一支分支：叫停旧 worker → 3A 硬屏障 → 3B1（异步提交）→ 3B2 worker → 持续监测。
    ///
    /// 返回 `Err` 只可能来自 3A；3B1 连「部分失败」都不返回 —— 它此刻还在别的线程上，
    /// 结果稍后经 `Msg::RunDone` 回来（方案第 22 条：动作失败不改变 Active）。3B2 同理，
    /// 而且它根本不会失败返回：worker 起不来是配置问题，报在 `workers` 那一栏里。
    /// `allowed` 由调用方在持锁期间准备好 —— 这里绝不再去 lock config。
    fn execute_branch(
        &mut self,
        state: &Arc<AppState>,
        profile: &Profile,
        which: Which,
        opts: RunOpts,
        allowed: &Arc<AllowedScripts>,
    ) -> Result<(), String> {
        // —— 3B2：先叫停旧的一组，再动网络 ——
        // 顺序是这里的全部要点：旧环境那组「保持 VPN 连接」若还活着，会在新配置下发的
        // 同时把旧网关塞回路由表，而两件事各自的日志都写着「成功」。
        // ELSE 分支不碰 worker：它表达的是「离开这个环境时要维持什么」，而离开时并没有
        // 一个持续成立的现场可维持 —— 当前 Active 的那一组必须继续跑。
        let hosts_workers = which == Which::Then;
        if hosts_workers {
            self.stop_workers();
        }
        let Some(branch) = which.of(profile) else {
            return Ok(());
        };
        // —— 3A：下发 + 回读校验 ——
        let mut three_a = ThreeAOutcome::Skipped;
        if let Some(net) = &branch.network {
            match network::apply_3a(&state.plat, net) {
                Stage3A::Failed { reason } => {
                    // 保底：探测里开了 fallback 就回落 DHCP（3A 失败处置的一部分）
                    let degrade = net
                        .verify
                        .as_ref()
                        .and_then(|v| v.health.as_ref())
                        .map(|h| h.enabled && h.fallback.enabled)
                        .unwrap_or(false);
                    if degrade {
                        match state.plat.set_dhcp() {
                            Ok(()) => log::warn(&i18n::t("notify.fallback")),
                            Err(e) => log::error(&i18n::tf(
                                "notify.dhcp_failed",
                                &[("error", &e)],
                            )),
                        }
                    }
                    return Err(reason);
                }
                Stage3A::Applied => {
                    three_a = ThreeAOutcome::Applied;
                    log::debug(&format!(
                        "3A 通过: {} · {}",
                        profile.name,
                        which.name()
                    ));
                }
            }
        }
        // —— 3B1：交给独立线程，引擎继续跑 ——
        if opts.one_shot {
            self.submit_one_shot(state, profile, which, branch, allowed, three_a);
        }
        // —— 3B2：每条已启用的常驻动作一条 worker，3A 通过才起 ——
        if hosts_workers {
            self.start_workers(state, profile, branch, allowed);
        }
        // —— 持续监测：只属于「已成为 Active」的 THEN 分支 ——
        if opts.monitor {
            self.start_monitor(state, profile, branch);
        }
        Ok(())
    }

    /// 提交一次后台 3B1 执行。
    ///
    /// 传出去的是**owned 副本**（动作清单 + `Arc<AllowedScripts>`）：工作线程绝不能
    /// 回头 lock config，否则「用户在编辑器里保存」与「动作跑完」互相等就成了死锁。
    fn submit_one_shot(
        &mut self,
        state: &Arc<AppState>,
        profile: &Profile,
        which: Which,
        branch: &Branch,
        allowed: &Arc<AllowedScripts>,
        three_a: ThreeAOutcome,
    ) {
        if !one_shot::any_enabled(&branch.one_shot) {
            // 一条都没启用：这次执行当场就是「空且已完成」。仍要留痕 —— 否则面板上
            // 挂着的是上一个 Profile 的运行，看起来就像它刚刚又跑了一遍。
            self.last_run = Some(RunRecord {
                running: false,
                status: Some(one_shot::BatchStatus::Empty),
                ..RunRecord::begin(self.next_seq(), profile, which, three_a, 0)
            });
            return;
        }
        let Some(tx) = state.engine_tx.get().cloned() else {
            log::error("引擎通道尚未就绪，3B1 动作未提交");
            return;
        };
        let seq = self.begin_run(profile, which);
        let enabled = branch.one_shot.iter().filter(|a| a.enabled).count();
        self.last_run = Some(RunRecord::begin(seq, profile, which, three_a, enabled));
        let step_tx = tx.clone();
        let on_step: Arc<one_shot::Step> = Arc::new(move |o: &one_shot::ActionOutcome| {
            let _ = step_tx.send(Msg::RunStep { seq, outcome: o.clone() });
        });
        one_shot::spawn(
            state.plat,
            allowed.clone(),
            branch.one_shot.clone(),
            on_step,
            move |report| {
                // 引擎已退出（进程正在消失）：报告没人读了，丢弃即可。
                let _ = tx.send(Msg::RunDone { seq, report });
            },
        );
    }

    /// 下一个运行编号。空运行也要占号：编号唯一是「迟到报告不会被误认」的前提。
    fn next_seq(&mut self) -> u64 {
        self.run_seq += 1;
        self.run_seq
    }

    /// 记下这次提交并返回它的编号。**后一次取代前一次**（见模块头第 4 条硬规则）。
    fn begin_run(&mut self, profile: &Profile, which: Which) -> u64 {
        if let Some(old) = &self.running_run {
            log::warn(&format!(
                "上一次 3B1 仍未结束（{} · {}），本次提交取代其归因：旧动作不会被中止，但结果不再计入",
                old.profile_name,
                old.branch.name()
            ));
        }
        let seq = self.next_seq();
        self.running_run = Some(RunningRun {
            seq,
            profile_id: profile.id.clone(),
            profile_name: profile.name.clone(),
            branch: which,
        });
        seq
    }

    /// 归因一条动作进度。返回 false = 它不属于当前那次运行（已被取代）。
    fn note_run_step(&mut self, seq: u64, outcome: &one_shot::ActionOutcome) -> bool {
        let current = self.running_run.as_ref().map(|r| r.seq) == Some(seq);
        if !current {
            return false;
        }
        if let Some(rec) = &mut self.last_run {
            if rec.seq == seq {
                rec.outcomes.push(outcome.clone());
            }
        }
        true
    }

    /// 归因一份 3B1 报告：填进留痕并释放运行槽。
    /// 返回 `None` = 它已被后来的运行取代，调用方丢弃即可。
    fn note_run_done(&mut self, seq: u64, report: &one_shot::BatchReport) -> Option<RunningRun> {
        let slot = match &self.running_run {
            Some(r) if r.seq == seq => self.running_run.take(),
            _ => return None,
        };
        if let Some(rec) = &mut self.last_run {
            if rec.seq == seq {
                rec.running = false;
                rec.status = Some(report.status);
                // 收尾以报告为准，而不是把 steps 再拼一遍：超时项只存在于报告里，
                // 而两者必须同源，否则「界面少一条」这种偏差永远查不动。
                rec.outcomes = report.outcomes.clone();
            }
        }
        slot
    }

    fn start_monitor(
        &mut self,
        state: &Arc<AppState>,
        profile: &Profile,
        branch: &Branch,
    ) {
        self.stop_monitor();
        let Some(h) = branch
            .network
            .as_ref()
            .and_then(|n| n.verify.as_ref())
            .and_then(|v| v.health.as_ref())
            .filter(|h| h.enabled)
            .cloned()
        else {
            return;
        };
        let fb_state = state.clone();
        let name = profile.name.clone();
        // 监测线程只做「探测 + 通知」，落到的动作在这里现成：回落 DHCP 并作废记档。
        HealthMonitor::start(state.plat, &h, self.health_stop.clone(), move || {
            if let Err(e) = fb_state.plat.set_dhcp() {
                log::error(&i18n::tf("notify.dhcp_failed", &[("error", &e)]));
            }
            {
                let mut eng = fb_state.engine.lock().unwrap_or_else(|e| e.into_inner());
                // 连同常驻 worker 一起收：网络都被判不通了，还留着「保持 VPN 连接」
                // 去维持一个已经作废的现场，只会让用户在回落之后又被拉回坏环境。
                eng.stop_workers();
                eng.applied_fp = None;
                eng.fallback_fp = None;
                eng.active_id = None;
                eng.monitoring = false;
            } // 广播必须在锁外（见模块头的加锁纪律）
            log::warn(&i18n::tf("engine.monitor_fallback", &[("name", &name)]));
            crate::state::emit_action(
                &fb_state,
                "monitor",
                false,
                i18n::t("engine.monitor_fallback_short"),
            );
            crate::state::publish_status(&fb_state);
        });
        self.monitoring = true;
    }
}

// —————————————————————————————— 前端视图 ——————————————————————————————

#[derive(Debug, Clone, Serialize)]
pub struct ProfileView {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub matched: bool,
    pub status: DisplayStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub rules: Vec<crate::conditions::RuleReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineView {
    pub state: Decision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    pub conflict: Vec<String>,
    pub snapshot: NetworkSnapshot,
    pub profiles: Vec<ProfileView>,
    pub warnings: Vec<String>,
    /// 最近一次走到 3B 的执行；`one_shot: null` 表示动作还在后台跑。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<RunRecord>,
    /// 当前 Active 的 3B2 worker 实时状态。空数组 = 没有在维持的东西。
    ///
    /// 刻意不像 `last_run` 那样省略：前端要按「有没有 worker 在跑」决定这一栏是显示
    /// 「未配置常驻动作」还是显示一串芯片，`null` 和 `[]` 分不开这两种情况就会猜。
    pub workers: Vec<persistent::WorkerStatus>,
}

// —————————————————————————————— 线程与主循环 ——————————————————————————————

/// 启动引擎线程，并把「网络事件」源接到它上面。
pub fn start(state: Arc<AppState>) {
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = channel();
    let _ = state.engine_tx.set(tx.clone());

    // SSID 监视的价值是**及时性**：它一发现变化就唤醒引擎，而不是等下一个采样周期；
    // 完整的指纹差分（网关 MAC / BSSID / 网卡集合）在引擎侧做，见 `detection`。
    let wake_tx = tx.clone();
    let handle = state.plat.watch_ssid(Box::new(move |_ssid| {
        let _ = wake_tx.send(Msg::Wake);
    }));
    *state.watcher.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);

    std::thread::spawn(move || loop_body(state, rx));
}

/// 一轮主循环里从通道收下的东西。收成结构体而不是四个 `&mut` 参数：
/// 变体只会继续增加，参数列表不该跟着涨。
#[derive(Default)]
struct Inbox {
    manual: Option<String>,
    actions: Vec<ActionKind>,
    steps: Vec<(u64, one_shot::ActionOutcome)>,
    runs: Vec<(u64, one_shot::BatchReport)>,
    workers: Vec<persistent::Update>,
}

fn loop_body(state: Arc<AppState>, rx: Receiver<Msg>) {
    loop {
        if crate::state::QUITTING.load(Ordering::SeqCst) {
            // 退出前先收尾：进程正在消失，而一条「每 30 秒重连 VPN」不该在托盘菜单
            // 都已经关掉之后还发出最后一条命令。
            stop_background(&state);
            log::info("引擎线程退出");
            break;
        }
        let mut inbox = Inbox::default();
        match rx.recv_timeout(detection::TICK) {
            Ok(m) => queue(m, &mut inbox),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // 排队中的请求一次收干，合并成一轮：否则连点两次「立即应用」会下发两次。
        while let Ok(m) = rx.try_recv() {
            queue(m, &mut inbox);
        }
        // 先收下后台动作的结果，再吸收配置改动与评估：让本轮广播的视图就是最新的。
        // 逐条进度只重发视图（前端在听），收尾才付一次完整广播的采样代价。
        if !inbox.steps.is_empty() && apply_steps(&state, &inbox.steps) {
            emit_evaluation(&state);
        }
        if !inbox.runs.is_empty() && apply_runs(&state, &inbox.runs) {
            publish_view(&state);
        }
        // worker 的状态变化只刷新视图，**不**触发完整广播：它每 N 秒可能变一次，
        // 而 `publish_status` 要付一次平台采样（macOS 上以秒计）的代价。
        if !inbox.workers.is_empty() && apply_workers(&state, &inbox.workers) {
            emit_evaluation(&state);
        }
        // 先吸收配置改动再评估，否则会用旧的 Profile 集合做判定。
        reload_if_changed(&state);
        for kind in inbox.actions {
            run_action(&state, kind);
        }
        pass(&state, inbox.manual);
    }
}

fn queue(m: Msg, inbox: &mut Inbox) {
    match m {
        Msg::Wake => {}
        Msg::Apply { id } => inbox.manual = Some(id),
        Msg::Action { kind } => inbox.actions.push(kind),
        Msg::RunStep { seq, outcome } => inbox.steps.push((seq, outcome)),
        Msg::RunDone { seq, report } => inbox.runs.push((seq, report)),
        Msg::Worker { update } => inbox.workers.push(update),
        Msg::Quit => {
            crate::state::QUITTING.store(true, Ordering::SeqCst);
        }
    }
}

/// 收下逐条动作进度。已被取代的那次运行的进度一律不收。
fn apply_steps(state: &Arc<AppState>, steps: &[(u64, one_shot::ActionOutcome)]) -> bool {
    let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    let mut attributed = false;
    for (seq, outcome) in steps {
        if eng.note_run_step(*seq, outcome) {
            attributed = true;
            match &outcome.error {
                Some(e) => log::error(&format!("动作失败 {} : {}", outcome.label, e)),
                None => log::debug(&format!("动作完成: {}", outcome.label)),
            }
        }
    }
    attributed
}

/// 收下后台 3B1 的收尾报告。只改留痕与日志，不动 Active。返回是否有报告被归因。
fn apply_runs(state: &Arc<AppState>, runs: &[(u64, one_shot::BatchReport)]) -> bool {
    let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    let mut attributed = false;
    for (seq, report) in runs {
        match eng.note_run_done(*seq, report) {
            Some(slot) => {
                log_run(&slot, report);
                attributed = true;
            }
            None => log::warn(&format!(
                "3B1 报告 #{seq} 已被后来的运行取代，只记日志不改留痕"
            )),
        }
    }
    attributed
}

/// 收下常驻 worker 的状态变化。已被取代的那一组一律不收。
///
/// 与 3B1 的收尾不同，这里**不写日志** —— worker 的每一次状态转换由 worker 线程自己
/// 记（`persistent::log_transition`），因为只有它知道「从哪个状态来」。
fn apply_workers(state: &Arc<AppState>, updates: &[persistent::Update]) -> bool {
    let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    let mut attributed = false;
    for update in updates {
        if eng.note_worker(update) {
            attributed = true;
        }
    }
    attributed
}

/// 退出收尾：停掉健康监测与全部常驻 worker。
///
/// 两个 stop 都只是置标志位（不 join，理由见 `Session::stop`），所以能在锁里做完。
/// 之后 worker 线程可能还有一次没醒来的睡眠 —— 它醒来看到标志位就退出，而进程已经没了。
fn stop_background(state: &Arc<AppState>) {
    let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    eng.stop_monitor();
    eng.stop_workers();
}

/// 把一次运行的收尾说清楚：谁的哪一支、成没成一半。逐条动作已在进度里记过。
fn log_run(slot: &RunningRun, report: &one_shot::BatchReport) {
    log::debug(&format!(
        "3B1 结束: {} ({}) · {}",
        slot.profile_name, slot.profile_id, slot.branch.name()
    ));
    if report.status == one_shot::BatchStatus::Partial
        || report.status == one_shot::BatchStatus::Failed
    {
        let ok = report.outcomes.iter().filter(|o| o.ok).count();
        let total = report.outcomes.len();
        log::warn(&i18n::tf("engine.one_shot_partial", &[
            ("name", &slot.profile_name),
            ("ok", &ok.to_string()),
            ("total", &total.to_string()),
        ]));
    }
}

/// 配置文件 mtime 变了就重新加载并校验；失败则保留旧配置（绝不能半路换成坏配置）。
fn reload_if_changed(state: &Arc<AppState>) -> bool {
    let path = state.config_path.clone();
    let mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
    {
        let mut last = state.config_mtime.lock().unwrap_or_else(|e| e.into_inner());
        if mtime.is_some() && mtime == *last {
            return false;
        }
        *last = mtime;
    }
    let (new_cfg, warnings) = match Config::load(&path).and_then(|c| c.validate().map(|_| c)) {
        Ok(c) => {
            let w: Vec<String> = c.warnings().iter().map(|x| x.0.clone()).collect();
            (c, w)
        }
        Err(e) => {
            log::error(&i18n::tf("notify.config_invalid", &[("error", &e)]));
            log::error("配置重载失败，保留旧配置");
            return false;
        }
    };
    if let Some(l) = &new_cfg.language {
        i18n::set_language(i18n::Language::from_code(l));
    }
    for w in &warnings {
        log::warn(&format!("配置告警: {}", w));
    }
    {
        let mut cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        *cfg = new_cfg;
    }
    {
        let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        eng.scheduler_reset();
        eng.set_warnings(warnings);
    }
    log::info(&i18n::t("notify.config_reloaded"));
    publish_status_now(state);
    true
}

/// 一轮：采样（锁外）→ 评估 → 迁移 → 广播（锁外）。
fn pass(state: &Arc<AppState>, manual: Option<String>) {
    let now = Instant::now();
    if state
        .engine
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sample_due(now)
    {
        let snap = NetworkSnapshot::sample(&state.plat);
        state
            .engine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .note_sampled(snap, now);
    }
    let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
    if manual.is_none() && eng.due(&cfg, now).is_empty() {
        return;
    }
    eng.evaluate(&cfg, now);
    let allowed = Arc::new(AllowedScripts {
        scripts_dir: state.scripts_dir.clone(),
        explicit: cfg.allowed_scripts.clone(),
    });
    if let Some(id) = &manual {
        manual_apply(state, &mut eng, &cfg, id, &allowed);
    }
    reconcile(state, &mut eng, &cfg, &allowed);
    drop(cfg);
    drop(eng);
    publish_view(state);
}

/// 把引擎视图广播给前端。先在锁内算好，放开锁后再发（见模块头加锁纪律）。
fn emit_evaluation(state: &Arc<AppState>) {
    let view = {
        let eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        eng.view(&cfg)
    };
    if let Some(app) = state.app.get() {
        let _ = tauri::Emitter::emit(app, "netsense://evaluation", &view);
    }
}

/// 视图 + 一次完整状态广播（含托盘菜单）。会付平台采样的代价，别逐条动作进度就发一次。
fn publish_view(state: &Arc<AppState>) {
    emit_evaluation(state);
    publish_status_now(state);
}

/// 把判定结果落到网络与动作上。
fn reconcile(
    state: &Arc<AppState>,
    eng: &mut Engine,
    cfg: &Config,
    allowed: &Arc<AllowedScripts>,
) {
    match eng.decision.clone() {
        Decision::Conflict { ids } => {
            // 冻结现状：不撤销已生效的配置（撤销会让用户当场断网，而冲突只是
            // 「不确定该用哪个」），但也绝不往下走任何一支。
            let sig = ids.join("|");
            if sig == eng.conflict_shown {
                return;
            }
            eng.conflict_shown = sig;
            let names: Vec<String> = ids
                .iter()
                .map(|id| {
                    cfg.profile_by_id(id)
                        .map(|p| p.name.clone())
                        .unwrap_or_else(|| id.clone())
                })
                .collect();
            log::warn(&i18n::tf(
                "engine.conflict",
                &[("names", &names.join(", "))],
            ));
            crate::state::emit(
                state,
                "netsense://conflict",
                serde_json::json!({ "profiles": names }),
            );
        }
        Decision::NoActiveProfile => {
            eng.conflict_shown.clear();
            if eng.active_id.take().is_some() {
                eng.deactivate();
                log::info(&i18n::t("engine.no_match"));
            } else {
                eng.stop_monitor();
            }
            apply_fallback(state, eng, cfg);
        }
        Decision::Active { id } => {
            eng.conflict_shown.clear();
            let Some(profile) = cfg.profile_by_id(&id) else {
                eng.active_id = None;
                return;
            };
            let fp = fingerprint_of(profile);
            let fingerprint = eng.scheduler_fingerprint();
            if eng.is_blocked(&id, &fp, &fingerprint) {
                return; // 这个「配置 + 网络」组合已经失败过一次，别反复弹授权框
            }
            let staying = eng.active_id.as_deref() == Some(id.as_str());
            if staying && eng.applied_fp.as_deref() == Some(fp.as_str()) {
                // 方案第 36 条：保持 Active 不重复下发、不重复跑一次性动作
                return;
            }
            if staying {
                log::info(&i18n::tf("engine.reconfigure", &[("name", &profile.name)]));
                match eng.execute_branch(state, profile, Which::Then, RunOpts::RECONFIGURE, allowed)
                {
                    Ok(()) => {
                        eng.errors.remove(&id);
                        eng.mark_applied(fp);
                    }
                    Err(e) => {
                        log::error(&i18n::tf("engine.error", &[
                            ("name", &profile.name),
                            ("error", &e),
                        ]));
                        eng.record_failure(&id, fp, e);
                    }
                }
                return;
            }
            let from = eng.active_display_name(cfg);
            eng.deactivate();
            match eng.execute_branch(state, profile, Which::Then, RunOpts::FULL, allowed) {
                Ok(()) => {
                    eng.active_id = Some(id.clone());
                    eng.errors.remove(&id);
                    eng.mark_applied(fp);
                    log::info(&i18n::tf("engine.switch", &[
                        ("from", from.as_deref().unwrap_or("-")),
                        ("to", &profile.name),
                    ]));
                    log::info(&i18n::tf("notify.applied", &[("name", &profile.name)]));
                }
                Err(e) => {
                    let msg = i18n::tf("engine.error", &[("name", &profile.name), ("error", &e)]);
                    log::error(&msg);
                    eng.record_failure(&id, fp, e);
                    crate::state::emit_action(state, "apply", false, msg);
                }
            }
        }
    }
}

/// 零命中时应用 fallback 网络配置（如果配了）。
///
/// 它不是 Profile：没有条件、不参与匹配、永远不会 Conflict。存在理由只有一个 ——
/// 上一个 Profile 可能下发了静态 IP，零命中时必须有「回到自动获取」的落点。
fn apply_fallback(state: &Arc<AppState>, eng: &mut Engine, cfg: &Config) {
    let Some(net) = cfg
        .fallback
        .as_ref()
        .filter(|f| f.enabled)
        .and_then(|f| f.network.as_ref())
    else {
        return;
    };
    let fp = format!("fallback|{}", serde_json::to_string(net).unwrap_or_default());
    if eng.fallback_fp.as_deref() == Some(fp.as_str()) {
        return;
    }
    if let Stage3A::Failed { reason } = network::apply_3a(&state.plat, net) {
        log::error(&i18n::tf("engine.fallback_failed", &[("error", &reason)]));
        return;
    }
    eng.fallback_fp = Some(fp);
    eng.applied_fp = None;
    eng.active_id = None;
    log::info(&i18n::t("engine.fallback_applied"));
}

/// 「立即应用」：**仍然要经过该 Profile 自己的条件判定**（方案第 38/39/40 条）。
///
/// 它不是后门：命中就跑 THEN、不命中就跑 ELSE；若此刻有多个 Profile 命中则直接拒绝
/// 并列出冲突名单 —— 否则用户可以绕过「多命中不自动选择」这条核心约束。
fn manual_apply(
    state: &Arc<AppState>,
    eng: &mut Engine,
    cfg: &Config,
    id: &str,
    allowed: &Arc<AllowedScripts>,
) {
    let Some(profile) = cfg.profile_by_id(id) else {
        let msg = i18n::tf("engine.unknown_profile", &[("name", id)]);
        log::warn(&msg);
        crate::state::emit_action(state, "apply", false, msg);
        return;
    };
    if !profile.enabled {
        let msg = i18n::tf("engine.disabled", &[("name", &profile.name)]);
        crate::state::emit_action(state, "apply", false, msg);
        return;
    }
    let others: Vec<String> = eng
        .evaluation
        .matched_ids
        .iter()
        .filter(|m| m.as_str() != id)
        .map(|m| {
            cfg.profile_by_id(m)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| m.clone())
        })
        .collect();
    if !others.is_empty() {
        let msg = i18n::tf("engine.conflict_blocked", &[
            ("name", &profile.name),
            ("others", &others.join(", ")),
        ]);
        log::warn(&msg);
        crate::state::emit_action(state, "apply", false, msg);
        return;
    }
    let matched = eval_profile(profile, &eng.snapshot).matched;
    let which = if matched { Which::Then } else { Which::Else };
    if which.of(profile).is_none() {
        let msg = i18n::tf("engine.no_branch", &[
            ("name", &profile.name),
            ("branch", which.name()),
        ]);
        crate::state::emit_action(state, "apply", false, msg);
        return;
    }
    let fp = fingerprint_of(profile);
    match eng.execute_branch(state, profile, which, RunOpts::MANUAL, allowed) {
        Ok(()) => {
            let msg = i18n::tf("engine.manual_ok", &[
                ("name", &profile.name),
                ("branch", which.name()),
            ]);
            log::info(&msg);
            crate::state::emit_action(state, "apply", true, msg);
            if matched {
                // 唯一命中：手动应用等价于正常激活，记档让引擎别再重下一遍
                eng.active_id = Some(id.to_string());
                eng.applied_fp = Some(fp);
                eng.fallback_fp = None;
                eng.decision = Decision::Active { id: id.to_string() };
            }
        }
        Err(e) => {
            let msg = i18n::tf("engine.error", &[("name", &profile.name), ("error", &e)]);
            log::error(&msg);
            if matched {
                eng.record_failure(id, fp, e);
            }
            crate::state::emit_action(state, "apply", false, msg);
        }
    }
}

fn fingerprint_of(p: &Profile) -> String {
    serde_json::to_string(p).unwrap_or_default()
}

/// `Engine` 上需要跨模块借用的两个小 accessor。
impl Engine {
    fn scheduler_fingerprint(&self) -> String {
        self.scheduler.fingerprint().to_string()
    }

    fn active_display_name(&self, cfg: &Config) -> Option<String> {
        let id = self.active_id.as_deref()?;
        Some(
            cfg.profile_by_id(id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| id.to_string()),
        )
    }
}

/// 托盘与状态广播用的「当前生效 Profile 名」。必须在**不持有**任何锁时调用。
pub fn active_display_name(state: &Arc<AppState>) -> String {
    let eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
    match eng.active_id.as_deref() {
        Some(id) => cfg
            .profile_by_id(id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| id.to_string()),
        None => match &eng.decision {
            Decision::Conflict { ids } => {
                let names: Vec<String> = ids
                    .iter()
                    .map(|i| {
                        cfg.profile_by_id(i)
                            .map(|p| p.name.clone())
                            .unwrap_or_else(|| i.clone())
                    })
                    .collect();
                i18n::tf("status.conflict", &[("names", &names.join(", "))])
            }
            Decision::NoActiveProfile => i18n::t("status.no_profile"),
            Decision::Active { id } => id.clone(),
        },
    }
}

/// 托盘/IPC 的两个后台动作。都在引擎线程执行：都涉及提权或秒级等待。
fn run_action(state: &Arc<AppState>, kind: ActionKind) {
    match kind {
        ActionKind::SetDhcp => set_dhcp(state),
        ActionKind::Probe => probe(state),
    }
}

/// 「将当前网络设置成 DHCP」：作用于**主网卡**，而不是写死无线网卡。
fn set_dhcp(state: &Arc<AppState>) {
    let nics = state.plat.list_interfaces();
    let dev = automation::primary_nic(&nics).map(|n| n.name.clone());
    let res = match dev {
        Some(d) => state.plat.set_dhcp_for(&d),
        None => state.plat.set_dhcp(),
    };
    match res {
        Ok(()) => {
            {
                let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
                // 已切回 DHCP：当前生效状态不再等于任何 manual 配置，记档全部作废。
                // worker 同属那份配置 —— 用户手动接管了这张网卡，就不能再让后台每 30 秒
                // 把某个 Profile 的状态改回来。
                eng.stop_workers();
                eng.applied_fp = None;
                eng.fallback_fp = None;
                eng.active_id = None;
            }
            let msg = i18n::t("notify.dhcp_done");
            log::info(&msg);
            crate::state::emit_action(state, "dhcp", true, msg);
        }
        Err(e) => {
            let msg = i18n::tf("notify.dhcp_failed", &[("error", &e)]);
            log::error(&msg);
            crate::state::emit_action(state, "dhcp", false, msg);
        }
    }
    publish_status_now(state);
}

/// 「强制探测当前网络」：按当前 Active Profile 的健康度目标探一次；
/// 没配置时用内置默认值，保证这个菜单项在任何配置下都有明确结果。
fn probe(state: &Arc<AppState>) {
    let (target, timeout_ms) = {
        let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        let eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        let active = eng.active_id.as_deref();
        let health = active.and_then(|id| cfg.profile_by_id(id)).and_then(|p| {
            p.then
                .as_ref()
                .and_then(|b| b.network.as_ref())
                .and_then(|n| n.verify.as_ref())
                .and_then(|v| v.health.as_ref())
                .cloned()
        });
        match health {
            Some(h) => network::probe_target(&h),
            None => network::default_probe_target(),
        }
    };
    log::info(&i18n::t("notify.probe_running"));
    let ok = network::probe_ok(&state.plat, &target, timeout_ms);
    let msg = i18n::t(if ok {
        "notify.probe_ok"
    } else {
        "notify.probe_fail"
    });
    if ok {
        log::info(&msg);
    } else {
        log::warn(&msg);
    }
    crate::state::emit_action(state, "probe", ok, msg);
}

fn publish_status_now(state: &Arc<AppState>) {
    crate::state::publish_status(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn zero_match_yields_no_active_profile() {
        assert_eq!(decide(&[]), Decision::NoActiveProfile);
    }

    #[test]
    fn exactly_one_match_becomes_active() {
        assert_eq!(
            decide(&ids(&["home"])),
            Decision::Active {
                id: "home".to_string()
            }
        );
    }

    #[test]
    fn two_or_more_matches_are_conflict_and_keep_every_name() {
        match decide(&ids(&["home", "office", "cafe"])) {
            Decision::Conflict { ids } => assert_eq!(ids.len(), 3, "冲突名单要完整报出来"),
            other => panic!("多命中必须判成 Conflict，实际 {:?}", other),
        }
    }

    #[test]
    fn conflict_never_picks_a_winner() {
        // 不挑「条件更多的那个」，也不挑任何一支 —— 谁都不应用
        assert!(matches!(
            decide(&ids(&["loose", "strict"])),
            Decision::Conflict { .. }
        ));
    }

    #[test]
    fn decision_serialises_to_a_stable_tagged_shape() {
        // 前端按 `state.state.state` 分支渲染，标签名是契约的一部分
        let v = serde_json::to_value(decide(&ids(&["a", "b"]))).unwrap();
        assert_eq!(v["state"], "conflict");
        assert_eq!(v["ids"], serde_json::json!(["a", "b"]));
        assert_eq!(serde_json::to_value(decide(&[])).unwrap()["state"], "no_active_profile");
    }

    fn profile(id: &str) -> Profile {
        Profile {
            id: id.to_string(),
            name: id.to_string(),
            ..Default::default()
        }
    }

    fn report(status: one_shot::BatchStatus) -> one_shot::BatchReport {
        one_shot::BatchReport {
            status,
            outcomes: Vec::new(),
        }
    }

    fn begun(eng: &mut Engine, id: &str, which: Which) -> u64 {
        let seq = eng.begin_run(&profile(id), which);
        eng.last_run = Some(RunRecord::begin(
            seq,
            &profile(id),
            which,
            ThreeAOutcome::Applied,
            3,
        ));
        seq
    }

    #[test]
    fn a_late_report_from_the_superseded_run_is_not_attributed() {
        let mut eng = Engine::new();
        let first = begun(&mut eng, "home", Which::Then);
        let second = begun(&mut eng, "office", Which::Then);
        assert!(second > first, "每次提交都要拿到更新的编号");
        assert!(
            eng.note_run_done(first, &report(one_shot::BatchStatus::Success)).is_none(),
            "旧运行的报告只能丢弃：它对应的现场已经不在了"
        );
        assert_eq!(
            eng.last_run.as_ref().unwrap().profile_id, "office",
            "被取代的那次绝不能把留痕改回旧 Profile"
        );
        let slot = eng
            .note_run_done(second, &report(one_shot::BatchStatus::Partial))
            .expect("当前那次要被收下");
        assert_eq!(slot.profile_id, "office");
        assert_eq!(
            eng.last_run.as_ref().unwrap().status,
            Some(one_shot::BatchStatus::Partial),
            "报告要落进留痕，否则界面只能一直显示「还在跑」"
        );
        assert!(!eng.last_run.as_ref().unwrap().running);
        assert!(
            eng.note_run_done(second, &report(one_shot::BatchStatus::Success)).is_none(),
            "同一份报告不该被归因两次"
        );
    }

    #[test]
    fn progress_from_a_superseded_run_never_mixes_into_the_current_record() {
        let mut eng = Engine::new();
        let stale = begun(&mut eng, "home", Which::Then);
        let current = begun(&mut eng, "office", Which::Then);
        let outcome = |id: &str| one_shot::ActionOutcome {
            id: id.to_string(),
            label: id.to_string(),
            priority: 1,
            ok: true,
            error: None,
        };
        assert!(!eng.note_run_step(stale, &outcome("旧环境的动作")));
        assert!(eng.note_run_step(current, &outcome("新环境的动作")));
        let rec = eng.last_run.as_ref().unwrap();
        assert_eq!(rec.outcomes.len(), 1, "被取代那次的进度不能混进来");
        assert_eq!(rec.outcomes[0].id, "新环境的动作");
        assert_eq!(rec.total, 3, "分母在提交时就定了，跑完前 UI 靠它报「1/3」");
        assert!(rec.running, "还没收尾，运行标志要保持为真");
    }

    #[test]
    fn run_numbers_never_repeat_even_across_switches() {
        let mut eng = Engine::new();
        let mut seen = std::collections::HashSet::new();
        for i in 0..5 {
            let seq = eng.begin_run(&profile(&format!("p{i}")), Which::Else);
            assert!(seen.insert(seq), "编号重复会让迟到的报告被误认成现役那次");
            eng.note_run_done(seq, &report(one_shot::BatchStatus::Empty));
        }
    }

    #[test]
    fn last_run_serialises_the_shape_the_frontend_reads() {
        let mut eng = Engine::new();
        let seq = begun(&mut eng, "home", Which::Else);
        eng.note_run_done(seq, &report(one_shot::BatchStatus::Success));
        let rec = eng.last_run.clone().unwrap();
        let v = serde_json::to_value(&rec).unwrap();
        // 分支与 3A 用机器可读的小写标签，前端不必再去解析日志文案
        assert_eq!(v["branch"], "else");
        assert_eq!(v["three_a"], "applied");
        assert_eq!(v["profile_id"], "home");
        assert_eq!(v["status"], "success");
        assert_eq!(v["running"], false);
        assert_eq!(v["total"], 3);
        assert!(v["outcomes"].is_array());
        assert!(
            v.get("seq").is_none(),
            "内部归因编号不该漏进前端契约：它每次都在变，前端拿了也没用"
        );
        assert!(v["at"].is_number());
    }

    /// `workers` 是数组而不是可省略字段：前端要能分辨「此刻没有 worker 在维持」与
    /// 「这一栏还没渲染过」。发 `[]` 才能让前者成立，省略只会被读成后者。
    #[test]
    fn the_view_always_carries_a_workers_array() {
        let eng = Engine::new();
        let v = serde_json::to_value(eng.view(&Config::default())).unwrap();
        assert!(v["workers"].is_array(), "空集也必须发出去: {}", v["workers"]);
        assert!(v["workers"].as_array().unwrap().is_empty());
    }

    fn ev(id: &str, matched: bool) -> ProfileEvaluation {
        ProfileEvaluation {
            id: id.to_string(),
            name: id.to_string(),
            enabled: true,
            matched,
            rules: Vec::new(),
        }
    }

    /// 现场：home 已经 Active，且它的一次 3B1 还在后台线程上跑。
    fn active_with_a_run_in_flight(eng: &mut Engine) -> u64 {
        eng.decision = Decision::Active {
            id: "home".to_string(),
        };
        eng.active_id = Some("home".to_string());
        eng.applied_fp = Some("fp".to_string());
        begun(eng, "home", Which::Then)
    }

    /// 方案第 22 条：3B 属于执行层里「可以失败」的那一半。
    ///
    /// 「VPN 没连上」不是「这个网络环境不对」：网络配置已经确实落地了，把 Profile
    /// 踢成 ERROR 等于让用户去改本来正确的条件，还会让引擎重新下一遍静态 IP。
    /// 所以失败只进留痕（`last_run`），状态一个字都不改。
    #[test]
    fn a_failed_batch_is_a_record_not_a_state_change() {
        for status in [one_shot::BatchStatus::Partial, one_shot::BatchStatus::Failed] {
            let mut eng = Engine::new();
            let seq = active_with_a_run_in_flight(&mut eng);
            eng.note_run_done(seq, &report(status));
            assert_eq!(
                eng.decision,
                Decision::Active { id: "home".into() },
                "{status:?} 不该动系统层判定"
            );
            assert_eq!(
                eng.active_id.as_deref(),
                Some("home"),
                "{status:?} 不该把 Profile 踢下 Active"
            );
            assert!(
                !eng.errors.contains_key("home"),
                "{status:?} 不该产生执行错误标记"
            );
            assert_eq!(eng.status_of(&ev("home", true)), DisplayStatus::Active);
            let rec = eng.last_run.as_ref().unwrap();
            assert_eq!(rec.status, Some(status), "失败必须看得见，只是不以状态的形式");
            assert!(!rec.running);
        }
    }

    /// 与之相对：只有 3A 能把一个 Profile 从 Active 上拉下来。
    #[test]
    fn only_a_3a_failure_turns_the_profile_into_error() {
        let mut eng = Engine::new();
        let cfg = Config {
            profiles: vec![profile("office")],
            ..Default::default()
        };
        eng.evaluation = Evaluation {
            profiles: vec![ev("office", true)],
            matched_ids: ids(&["office"]),
        };
        eng.decision = Decision::Active { id: "office".into() };
        eng.active_id = Some("office".to_string());
        eng.applied_fp = Some("stale".to_string());

        eng.record_failure("office", "fp".to_string(), "3A 回读不符".to_string());

        assert_eq!(eng.status_of(&ev("office", true)), DisplayStatus::Error);
        let view = eng.view(&cfg);
        assert_eq!(
            view.profiles[0].error.as_deref(),
            Some("3A 回读不符"),
            "失败原因必须跟着状态一起到前端，否则用户只能去翻日志"
        );
        assert!(eng.active_id.is_none() && eng.applied_fp.is_none(), "没通过校验的下发不算生效");
        assert!(
            eng.blocked.is_some(),
            "同一个「配置 + 网络」组合要记档，否则每轮评估都重新弹一次授权框"
        );

        // 反过来也要成立：下一次成功必须把红叉和记档一起清掉
        eng.errors.remove("office");
        eng.active_id = Some("office".to_string());
        eng.mark_applied("fp".to_string());
        assert_eq!(eng.status_of(&ev("office", true)), DisplayStatus::Active);
        assert!(eng.blocked.is_none(), "成功后记档就该失效，不然改回坏配置不会再拦");
    }

    /// 健康度监测回落 DHCP 也是执行层的事：它把 Active 撤掉，但**不**写 `errors`。
    /// 所以这条 Profile 显示为 ERROR 却没有 `error` 文案 —— 原因是那次回落当场就用
    /// `netsense://action` + 日志说过了，重复塞进徽标只会盖掉真正的失败信息。
    #[test]
    fn a_health_fallback_drops_active_without_an_error_message() {
        let mut eng = Engine::new();
        let cfg = Config {
            profiles: vec![profile("office")],
            ..Default::default()
        };
        eng.evaluation = Evaluation {
            profiles: vec![ev("office", true)],
            matched_ids: ids(&["office"]),
        };
        eng.decision = Decision::Active { id: "office".into() };
        eng.active_id = Some("office".to_string());
        eng.applied_fp = Some("fp".to_string());
        // 监测线程收尾时做的三件事（见 `start_monitor`）
        eng.applied_fp = None;
        eng.fallback_fp = None;
        eng.active_id = None;
        eng.monitoring = false;

        assert_eq!(eng.status_of(&ev("office", true)), DisplayStatus::Error);
        assert_eq!(eng.view(&cfg).profiles[0].error, None);
    }

    /// 条件层与执行层各说各话：一个 Profile 的失败不传染给别的 Profile。
    #[test]
    fn conflict_and_error_stay_two_different_layers() {
        let mut eng = Engine::new();
        eng.decision = Decision::Conflict {
            ids: ids(&["home", "office"]),
        };
        assert_eq!(eng.status_of(&ev("home", true)), DisplayStatus::Conflict);
        // 冲突期间不执行任何一支，所以这里手工放一条历史失败
        eng.errors.insert("home".to_string(), "上一次的 3A 失败".to_string());
        assert_eq!(
            eng.status_of(&ev("home", true)),
            DisplayStatus::Error,
            "同一条 Profile 上执行错误比冲突更该先被看到"
        );
        assert_eq!(
            eng.status_of(&ev("office", true)),
            DisplayStatus::Conflict,
            "home 的失败不能把 office 也染成红色"
        );
        assert_eq!(eng.status_of(&ev("cafe", false)), DisplayStatus::NotMatched);
        let mut off = ev("cafe", true);
        off.enabled = false;
        assert_eq!(eng.status_of(&off), DisplayStatus::Disabled, "禁用永远优先于其它判定");
    }
}
