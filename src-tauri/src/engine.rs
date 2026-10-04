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
//! 1. **多命中 = Conflict，不自动选择**（方案第 5/6/10 条）。Profile 之间没有先后，
//!    也没有「更具体者胜」：两个都合理的 Profile 静默二选一，用户完全看不出
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
use crate::config::{Branch, Config, FallbackConfig, NetworkConfig, Profile, FALLBACK_ID};
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
    /// 命中了，但引擎处于「设为 DHCP」后的暂停中：不会去动网卡，所以既不是
    /// Active 也不是 Error —— 后者在前端意味着「已经对网卡动过手」。
    Suspended,
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

/// 纯判定：命中数决定一切，**不挑「谁排得靠前」、不挑「谁更具体」**。
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
    /// 零命中兜底：跑动作 + 常驻 worker，但**不**接管健康监测 ——
    /// 兜底没有「当前环境」可探测（用户正是要离开任何环境），也没有 Profile 可归属。
    pub const FALLBACK: RunOpts = RunOpts {
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

/// 零命中要落成处置之前，允许「连续读不到身份」被当成既成事实的轮数。
///
/// 为什么是 2：一节采样约 2 s，而误判的代价是「把用户的静态 IP 与指定 DNS 拆成
/// 自动获取」，代价的反方向只是「多等一节才回落」。2 轮足够把「CoreWLAN 这一次没
/// 返回 / ARP 表恰好空了」这类单轮抖动滤掉，又不至于让真的换了陌生网络时等太久。
const UNIDENTIFIED_CONFIRMS: usize = 2;

/// 引擎状态。除构造外只由引擎线程改写。
pub struct Engine {
    pub snapshot: NetworkSnapshot,
    pub evaluation: Evaluation,
    pub decision: Decision,
    scheduler: Scheduler,
    last_sample: Option<Instant>,
    /// 本机网络刚被本进程改过（3A 下发 / 回落 DHCP），下一轮必须**先重采样再判定**，
    /// 且不能因为「没有 Profile 到期」而提前收工。不设这个标记时：静态 IP 下发成功之后，
    /// 指纹里的每一项（SSID / 网关 MAC / BSSID / 网卡集合）都没动，于是没有任何 Profile
    /// 到期、这一轮什么都不广播，界面上挂着的仍是下发前那份快照。
    resample_wanted: bool,
    first_pass_done: bool,
    /// 连续多少轮采样都没读到任何身份字段（SSID / 网关 MAC / BSSID 全空）。
    /// 零命中要拆现网之前，用它确认「真的是一个陌生网络」而不是「这几轮什么都没读到」，
    /// 见 `NetworkSnapshot::has_identity` 与 `Engine::no_match_is_actionable`。
    identity_misses: usize,
    /// 当前生效的 Profile id
    active_id: Option<String>,
    /// 上次成功下发的 Profile 内容指纹（内容没变就别再弹一次授权框）
    applied_fp: Option<String>,
    /// 「设为 DHCP」后的手动暂停（见 [`Engine::hold_automation`]）。
    hold: bool,
    /// 上次应用的 fallback 内容指纹。与 `applied_fp` **互相作废**：
    /// 下发过任何 Profile 配置就得清空 fallback 记档 —— 否则「Home 下了静态 IP →
    /// 到了零命中的咖啡馆」会因为 fallback 内容与启动时那次相同而被跳过，
    /// 网络永远停在 Home 的静态地址上。
    fallback_fp: Option<String>,
    blocked: Option<Blocked>,
    /// 兜底 3A 失败后的「别再重试」记档（与 `blocked` 同机制，但独立于 Profile 的 3A 记档，
    /// 避免两者互相清掉对方的屏障）。用户持续缺席时若兜底网络块反复回读失败，没有它就会
    /// 每轮都重新跑 3A（含弹授权框）。
    fallback_blocked: Option<Blocked>,
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
    /// 决策代数：每次 `evaluate` 重算判定时 +1。放锁跑 I/O 的「下发阶段」靠它辨认
    /// 「我放锁前算出的计划，放锁后还是不是当前该执行的那个」—— 见 [`pass`] 的
    /// decide / apply / commit 三段式（审计 B2：平台 I/O 不得在持锁时做）。
    generation: u64,
}

impl Default for Engine {
    fn default() -> Self {
        Engine {
            snapshot: NetworkSnapshot::default(),
            evaluation: Evaluation::default(),
            decision: Decision::NoActiveProfile,
            scheduler: Scheduler::default(),
            last_sample: None,
            resample_wanted: false,
            first_pass_done: false,
            identity_misses: 0,
            active_id: None,
            applied_fp: None,
            hold: false,
            fallback_fp: None,
            blocked: None,
            fallback_blocked: None,
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
            generation: 0,
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
        self.identity_misses = if snap.has_identity() {
            0
        } else {
            self.identity_misses + 1
        };
        self.snapshot = snap;
        if self.scheduler.observe(&self.snapshot, now) && self.hold {
            // 网络真的变了：DHCP 后的手动暂停到此结束，自动化恢复正常节律。
            // （判定仍走各 Profile 自己的 change_delay —— 变化刚发生，暂不「稳定」。）
            self.hold = false;
            log::info(&i18n::t("engine.hold_released"));
        }
    }

    /// 零命中这一判定能不能落成处置（撤销 Active、跑兜底）。
    ///
    /// 判据不是「命中数为 0」，而是「命中数为 0，**且**这一份快照确实读到了此刻在哪个
    /// 网络上」。三项身份字段全空时，更常见的解释是那几次读取都没拿到东西：macOS 的
    /// SSID 只有 CoreWLAN 一个来源（命令行在 Sequoia 上全部涂黑），网关 MAC 要靠 ARP
    /// 表里恰好有默认路由那一条，BSSID 同理。把「没读到」当成「到了一个陌生网络」的
    /// 代价是当场拆掉现网 —— 静态 IP 变自动获取、指定 DNS 变自动，而界面上看不出是
    /// 谁改的。
    ///
    /// 连续空到 `UNIDENTIFIED_CONFIRMS` 轮之后就放行：那时候它已经是「这个网络读不出
    /// 身份」的稳定事实（网线拔了、纯有线且 ARP 表空着），兜底必须照常起作用。
    pub fn no_match_is_actionable(&self) -> bool {
        self.snapshot.has_identity() || self.identity_misses >= UNIDENTIFIED_CONFIRMS
    }

    /// 进入「设为 DHCP」后的手动暂停：引擎不再自动评估与下发，直到下一次网络变化。
    ///
    /// 为什么要有它：DHCP 与某个 Profile 的静态 IP 正是互相抵消的两件事 —— 没有暂停时，
    /// 「设为 DHCP」成功后的重采样轮会立刻重新命中该 Profile 并把静态 IP 又下回去，
    /// 用户的每一次点击都是白点。暂停期间照常采样（见 `note_sampled`），所以网络一变，
    /// 这里就解除。
    ///
    /// 暂停只拦「引擎自己发起」的轮询与下发（含本进程改网络触发的强制重采）；
    /// 用户的显式动作 —— 面板的「立即应用」与「探测」—— 不受它约束。
    pub fn hold_automation(&mut self) {
        self.hold = true;
    }

    pub fn automation_held(&self) -> bool {
        self.hold
    }

    /// 记一笔「本机网络刚被改过」：下一轮无条件重采样并重算判定。
    ///
    /// 由**本进程动过网络**的那些点调用（3A 下发、fallback 下发、面板与健康监测的
    /// 回落 DHCP），成败都记 —— 失败分支里可能已经回落过一次，现场同样变了。
    /// 3B1 的动作不改网络，别把它挂进来：那会让每 N 秒一次的启动脚本也拖着引擎重采一遍。
    pub fn request_resample(&mut self) {
        self.resample_wanted = true;
    }

    /// 取走并清掉 [`Engine::request_resample`] 的请求。取走而不是读：一次下发只需要
    /// 一次强制轮，留在身上会让之后每一轮都绕过节律判定。
    fn take_resample_request(&mut self) -> bool {
        std::mem::replace(&mut self.resample_wanted, false)
    }

    /// 本轮到期的 Profile（空 = 什么都不用做）。
    ///
    /// 暂停期间恒为空：这样 `evaluate` 也不会推进任何 Profile 的节律（`after_evaluation`
    /// 只认真正到期的那些），解除后各条 Profile 按自己的 timing 自然到期。
    pub fn due(&self, cfg: &Config, now: Instant) -> Vec<String> {
        if self.hold {
            return Vec::new();
        }
        self.scheduler.due(&cfg.profiles, now, !self.first_pass_done)
    }

    /// 引擎是否已经采到过至少一份快照。
    ///
    /// 条件预览（`ipc::preview_match`）靠它决定要不要作答：一轮都没跑完时 `snapshot`
    /// 还是全空的默认值，拿它算出来的「什么都不匹配」不是判定、而是没采过样 —— 那种
    /// 答案发给界面只会把引擎本来正确的徽标擦掉。
    pub fn sampled(&self) -> bool {
        self.first_pass_done
    }

    /// 走一轮评估并推进节律。
    pub fn evaluate(&mut self, cfg: &Config, now: Instant) {
        let due = self.due(cfg, now);
        // 每重算一次判定就翻一代（审计 B2）：放锁跑 I/O 的「下发阶段」靠它辨认
        // 「我放锁前算出的计划，放锁后还是不是当前该执行的那个」。
        self.generation += 1;
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
                } else if self.hold && ev.matched {
                    // 暂停期间不会去动网卡 —— 这里不能沿用「命中却没生效 = 3A 失败」的红叉
                    DisplayStatus::Suspended
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
            log::debug(&i18n::tf("engine.workers_stopped", &[("n", &n.to_string())]));
        }
        n
    }

    /// 只叫停属于兜底的那一组 worker。返回叫停条数（0 = 没有或不属于兜底）。
    ///
    /// 存在的理由：兜底的启用/内容变化只该动自己那一组 —— 当前这一组若属于某个
    /// Active Profile（用户在零命中判定落地前的一瞬间改的配置），停它就是误伤。
    fn stop_fallback_workers(&mut self) -> usize {
        let mine = self
            .workers
            .as_ref()
            .map(|s| s.owner_id() == FALLBACK_ID)
            .unwrap_or(false);
        if mine {
            self.stop_workers()
        } else {
            0
        }
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
            log::error(&i18n::t("engine.channel_no_workers"));
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
        log::info(&i18n::tf(
            "engine.workers_started",
            &[
                ("n", &session.statuses().len().to_string()),
                ("names", &profile.name),
                ("gen", &self.worker_gen.to_string()),
            ],
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

    fn is_fallback_blocked(&mut self, config_fp: &str, fingerprint: &str) -> bool {
        let keep = match &self.fallback_blocked {
            Some(b) => b.config_fp == config_fp && b.fingerprint == fingerprint,
            None => false,
        };
        if !keep {
            self.fallback_blocked = None;
        }
        keep
    }

    fn record_fallback_failure(&mut self, config_fp: String, fingerprint: String) {
        // 与 Profile 的 `record_failure` 同理：兜底 3A 失败记档，下一轮（网络或配置一变）
        // 之前都不再重跑 3A —— 否则用户持续缺席时会被反复要求授权。记档独立于 `blocked`，
        // 不被 Profile 的成功应用清掉，也不会去清掉 Profile 的屏障。
        self.fallback_blocked = Some(Blocked {
            id: "fallback".to_string(),
            config_fp,
            fingerprint,
        });
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
        // `branch_prepare` 的开头，那里已经在新 3A 下发之前叫停了旧的一组。
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

    /// 下发前在锁内做的准备：THEN 分支先叫停旧 worker（顺序见 `Engine::deactivate`，
    /// 停 worker 必须在任何新下发之前）；含网络配置时举重采样旗、作废兜底记档。
    /// 返回将要下发的 `network` 克隆（`None` = 这支本就不含网络配置，3A 视为 Skipped）。
    fn branch_prepare(&mut self, which: Which, branch: &Branch) -> Option<NetworkConfig> {
        // 顺序是这里的全部要点：旧环境那组「保持 VPN 连接」若还活着，会在新配置下发的
        // 同时把旧网关塞回路由表，而两件事各自的日志都写着「成功」。
        // ELSE 分支不碰 worker：它表达的是「离开这个环境时要维持什么」，而离开时并没有
        // 一个持续成立的现场可维持 —— 当前 Active 的那一组必须继续跑。
        if which == Which::Then {
            self.stop_workers();
        }
        let net = branch.network.clone();
        if net.is_some() {
            // 只要真下发过，就得让下一轮重看一遍现场：3A 的读回校验用的是 `fresh_status`
            // （绕过缓存），而引擎这份快照走的是缓存路径，两者在这里不是一回事。
            self.request_resample();
            // 这次下发一碰网卡，旧的兜底记档就不再描述当前网络了（哪怕下发失败，
            // 现场也可能已经被改了一半）—— 作废它，否则下一次零命中会误以为
            // 「兜底早应用过」而跳过重新下发。
            self.fallback_fp = None;
        }
        net
    }

    /// 3A 之后的 3B 落地：3B1 异步提交 + 3B2 常驻 worker + 持续监测。
    ///
    /// 调用方必须已经**在锁外**跑完 3A（`run_3a_lockfree`），并把结果透传进来
    /// （审计 B2：平台 I/O 不得在持锁时做）。`three_a`：
    /// - `None` = 这支本就不含网络配置（网络视为 Skipped，3B 照跑）；
    /// - `Some(Applied)` = 3A 成功，3B 照跑；
    /// - `Some(Failed)` = 3A 失败，**3B 一条都不跑**（方案第 16/20 条硬屏障）。
    ///
    /// 分支自己从 `which.of(profile)` 取，不再单独传：少一个参数，也免得「传进来的
    /// branch 与 which 对不上」这种不一致。THEN 缺省（Profile 没配 THEN）时整段返回 ——
    /// 这一支本来就什么都不做，连「空运行」留痕都不该有。
    ///
    /// 3B1 连「部分失败」都不返回 —— 它此刻还在别的线程上，结果稍后经 `Msg::RunDone`
    /// 回来（方案第 22 条：动作失败不改变 Active）。3B2 同理，而且它根本不会失败返回：
    /// worker 起不来是配置问题，报在 `workers` 那一栏里。`allowed` 由调用方在持锁期间
    /// 准备好 —— 这里绝不再去 lock config。
    fn branch_3b(
        &mut self,
        state: &Arc<AppState>,
        profile: &Profile,
        which: Which,
        opts: RunOpts,
        allowed: &Arc<AllowedScripts>,
        three_a: Option<Stage3A>,
    ) {
        // 3A 失败 → 3B 全停（硬屏障）。
        let outcome = match three_a {
            Some(Stage3A::Failed { .. }) => return,
            Some(Stage3A::Applied) => ThreeAOutcome::Applied,
            // 无网络配置（Skipped）：仍要跑一次性动作与常驻 worker，只是留痕里
            // 没有「已下发网络」这一项。
            None => ThreeAOutcome::Skipped,
        };
        let Some(branch) = which.of(profile) else {
            return;
        };
        // —— 3B1：交给独立线程，引擎继续跑 ——
        if opts.one_shot {
            self.submit_one_shot(state, profile, which, branch, allowed, outcome);
        }
        // —— 3B2：每条已启用的常驻动作一条 worker，3A 通过才起 ——
        if which == Which::Then {
            self.start_workers(state, profile, branch, allowed);
        }
        // —— 持续监测：只属于「已成为 Active」的 THEN 分支 ——
        if opts.monitor {
            self.start_monitor(state, profile, branch);
        }
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
            log::error(&i18n::t("engine.channel_no_actions"));
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
            log::warn(&i18n::tf(
                "engine.run_superseded",
                &[
                    ("name", &old.profile_name),
                    ("branch", old.branch.name()),
                ],
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
        // 捕获「我启动时是哪个 Profile 在 Active」：监测线程阻塞探测期间可能已切到别的
        // Profile（或已被手动停用），迟到回落若还去 set_dhcp()/清 active_id，会拆掉新环境的现场。
        let expected = profile.id.clone();
        HealthMonitor::start(state.plat, &h, self.health_stop.clone(), move || {
            // 迟到回落护栏：此刻「我还是不是那个 Active」？不是就什么都不做，避免拆掉
            // 新 Active 的现场（含它自己的监测线程，否则会留下一个永远收不掉的后台线程）。
            let still_active = {
                let eng = fb_state.engine.lock().unwrap_or_else(|e| e.into_inner());
                eng.active_id.as_deref() == Some(expected.as_str())
            };
            if !still_active {
                return;
            }
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
                // 和面板上那颗「设为 DHCP」按钮同理：网络刚被改掉，下一轮必须先重采样。
                eng.request_resample();
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
    //
    // 光唤醒只值一半：`Msg::Wake` 跳过的是 1s 主循环节拍，**不**越过 2s 采样节律，
    // 于是新 SSID 最坏还要等下一轮才进快照，而 `detection` 的「再等 change_delay_secs」
    // 是从那一刻才开始计时的 —— 界面等 ACTIVE 的 5 秒里混进了我们自己排的这两三秒队。
    // 所以这条来源在唤醒之前先举旗：这一轮无条件重采、重算。
    //
    // 旗只给**它**：`Msg::Wake` 还有别的发送方（配置落盘、语言切换），它们举旗就等于
    // 每次保存都要在引擎线程里付一次平台采样（macOS 的降级链最坏走到几秒）。
    let wake_tx = tx.clone();
    let wake_state = state.clone();
    let handle = state.plat.watch_ssid(Box::new(move |_ssid| {
        wake_state
            .engine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .request_resample();
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
            // 退出前先收尾：进程正在消失，而一条「每 30 秒重连 VPN」不该在面板
            // 都已经关掉之后还发出最后一条命令。
            stop_background(&state);
            log::info(&i18n::t("engine.thread_exit"));
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
                Some(e) => log::error(&i18n::tf(
                    "engine.action_failed",
                    &[("label", &outcome.label), ("error", e)],
                )),
                None => log::debug(&i18n::tf("engine.action_done", &[("label", &outcome.label)])),
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
            None => log::warn(&i18n::tf("engine.stale_report", &[("seq", &seq.to_string())])),
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
    log::debug(&i18n::tf(
        "engine.run_done",
        &[
            ("name", &slot.profile_name),
            ("id", &slot.profile_id),
            ("branch", slot.branch.name()),
        ],
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
        // `None`（文件根本不在）也是一种状态，必须一起比较：只看 `is_some()` 的话，
        // 首次运行那种「一直没有配置文件」的合法状态下，每一轮都会走下去读一次、
        // 失败一次、连打两行 error —— 用户看到的就是日志被同一句 ENOENT 刷满。
        let mut last = state.config_mtime.lock().unwrap_or_else(|e| e.into_inner());
        if mtime == *last {
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
            log::error(&i18n::t("notify.reload_kept_old"));
            return false;
        }
    };
    // 这里**不**应用语言：语言属于软件配置，热重载一份自动化配置不该把用户的界面
    // 语言换掉（那会让「我刚改的是 profile，怎么菜单变中文了」变成无法解释的观感）。
    for w in &warnings {
        log::warn(&i18n::tf("notify.config_warning", &[("warning", w)]));
    }
    {
        let mut cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        *cfg = new_cfg;
        state.config_replaced();
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

/// 锁外跑 3A：下发 + 路由 + 回读校验，含失败时可选的回落 DHCP（审计 B2）。
///
/// 只吃 `plat` 与一份 owned 的 [`NetworkConfig`] 克隆 —— 这正是它能在**不持
/// engine / config 锁**时安全运行的原因（模块头第 2 条加锁纪律）。
/// `apply_3a` 最坏耗时 = 4 次回读 × 800ms 沉降 + 一串子进程，持锁跑它会让
/// `get_engine_status` / `status_payload` 这些只读 IPC 一起等上好几秒。
///
/// - `net = None` → 返回 `None`（这支不含网络配置，3A 视为 Skipped，3B 照跑）；
/// - `Some(Applied)` → 下发并回读成功；
/// - `Some(Failed{..})` → 阻断 3B；`degrade` 为真时先回落 DHCP 再返回。
fn run_3a_lockfree(
    state: &Arc<AppState>,
    net: Option<&NetworkConfig>,
    degrade: bool,
) -> Option<Stage3A> {
    let net = net?;
    Some(match network::apply_3a(&state.plat, net) {
        Stage3A::Failed { reason } => {
            // 保底：探测里开了 fallback 就回落 DHCP（3A 失败处置的一部分）
            if degrade {
                match state.plat.set_dhcp() {
                    Ok(()) => log::warn(&i18n::t("notify.fallback")),
                    Err(e) => {
                        log::error(&i18n::tf("notify.dhcp_failed", &[("error", &e)]));
                    }
                }
            }
            Stage3A::Failed { reason }
        }
        ok => ok,
    })
}

/// 「开启了健康探测 + 探测失败回落 DHCP」时，3A 失败要顺带回落。
fn degrade_enabled(net: &NetworkConfig) -> bool {
    net.verify
        .as_ref()
        .and_then(|v| v.health.as_ref())
        .map(|h| h.enabled && h.fallback.enabled)
        .unwrap_or(false)
}

/// 一轮：采样（锁外）→ 评估 → 迁移 → 广播（锁外）。
fn pass(state: &Arc<AppState>, manual: Option<String>) {
    let now = Instant::now();
    // 上一次下发说过「现场被我改过了」：这一轮跳过采样节律，也跳过「没有 Profile 到期」
    // 的提前收工，否则会拿旧快照再广播一次，界面继续显示下发之前的参数。
    let forced = state
        .engine
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take_resample_request();
    if forced
        || state
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
    // 评估仍在锁内（要读 snapshot / due），但**下发与兜底放到锁外**：见
    // `reconcile` 的三段式。这里只把「评估结果 + 允许脚本集 + 是否暂停」带出来。
    let (allowed, auto) = {
        let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        // DHCP 暂停：只采样（上面已完成），不评估、不下发、不跑兜底。连 `forced` 也拦 ——
        // 「设为 DHCP」自己触发的强制轮若放行，暂停会被当场推翻（评估 → 重新命中 → 下回去）。
        // 用户显式点的「立即应用」不走这条捷径，不受这里约束。
        if manual.is_none() && eng.automation_held() {
            return;
        }
        if manual.is_none() && !forced && eng.due(&cfg, now).is_empty() {
            return;
        }
        eng.evaluate(&cfg, now);
        let allowed = Arc::new(AllowedScripts {
            scripts_dir: state.scripts_dir.clone(),
            explicit: cfg.allowed_scripts.clone(),
        });
        // 暂停期间连 reconcile 都不跑：切换、兜底都是引擎的自动行为，
        // 而暂停的全部意义就是让它们在网络变化前住手。
        let auto = !eng.automation_held();
        (allowed, auto)
    };
    if let Some(id) = &manual {
        manual_apply(state, id, &allowed);
    }
    if auto {
        reconcile(state, &allowed);
    }
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

/// 视图 + 一次完整状态广播（面板是唯一的消费方）。会付平台采样的代价，别逐条动作进度就发一次。
fn publish_view(state: &Arc<AppState>) {
    emit_evaluation(state);
    publish_status_now(state);
}

/// 「这一轮要下发什么」的锁内产物。
///
/// 之所以拆成 owned 快照：网络配置被克隆出来之后，下发阶段（`run_3a_lockfree`）
/// 只认这份快照 + `plat`，与 engine / config 两把锁完全无关（审计 B2）。
/// `gen` 记住决策代数，提交时用它确认「放锁这一会儿，没人改过判定」。
///
/// `Profile` / `FallbackConfig` 装箱：两者都远大于本枚举其余字段，不装箱会让
/// 枚举尺寸被最大变体撑到 1.4KB（`clippy::large_enum_variant`）。它只在引擎线程
/// 栈上活一轮，装箱换来的是两个变体尺寸相当。
enum Plan {
    /// 唯一命中：切到 / 重下某个 Profile
    Profile {
        profile: Box<Profile>,
        net: Option<NetworkConfig>,
        opts: RunOpts,
        /// 同一个 Active、只是内容变了（RECONFIGURE）—— 与真正的切换处置不同：
        /// 重下失败不弹「应用失败」气泡。
        staying: bool,
        /// 切换前的 Active 名字，只用于日志
        from: Option<String>,
        fp: String,
        gen: u64,
    },
    /// 零命中：跑兜底
    Fallback {
        fb: Box<FallbackConfig>,
        net: Option<NetworkConfig>,
        fp: String,
        fingerprint: String,
        gen: u64,
    },
}

impl Plan {
    fn gen(&self) -> u64 {
        match self {
            Plan::Profile { gen, .. } | Plan::Fallback { gen, .. } => *gen,
        }
    }
    fn net(&self) -> Option<&NetworkConfig> {
        match self {
            Plan::Profile { net, .. } | Plan::Fallback { net, .. } => net.as_ref(),
        }
    }
}

/// 把判定结果落到网络与动作上。**三段式**（审计 B2）：
///
/// 1. [`decide_plan`] 持锁算出「这一轮该下发什么」，并把 `branch.network` 克隆成 owned 快照；
/// 2. [`run_3a_lockfree`] **放锁**跑 3A（子进程下发 + 4×800ms 回读），engine / config
///    两把锁全程空着 —— `get_engine_status` / `status_payload` 这些只读 IPC 不会被挂住；
/// 3. [`commit_plan`] 重新持锁，用 `generation` 复查这份计划还是不是当前该执行的那个，
///    是才提交（起 3B / 记档 / 切 Active）。
///
/// 引擎是单线程的，放锁那一会儿不会有别的 `evaluate` 插进来；但把「计划作废」
/// 这件事用代数复查显式表达出来，将来真引入了并发评估也不必重新推演。
fn reconcile(state: &Arc<AppState>, allowed: &Arc<AllowedScripts>) {
    let plan = {
        let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        decide_plan(state, &mut eng, &cfg)
    };
    let Some(plan) = plan else { return; };
    // —— apply：锁外。3A 失败时的回落 DHCP 也在这一步（同样是平台 I/O）——
    let three_a = {
        let degrade = plan.net().map(degrade_enabled).unwrap_or(false);
        run_3a_lockfree(state, plan.net(), degrade)
    };
    // —— commit：重新持锁，复查决策代数 ——
    let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    if eng.generation != plan.gen() {
        log::debug(&i18n::t("engine.plan_superseded"));
        return;
    }
    commit_plan(state, &mut eng, plan, three_a, allowed);
}

/// 锁内：把这一轮的判定翻译成一份 owned 的下发计划。`None` = 这一轮什么都不做。
///
/// Conflict 那支也在锁内把提示发掉：它是纯广播，没有任何下发。
fn decide_plan(state: &Arc<AppState>, eng: &mut Engine, cfg: &Config) -> Option<Plan> {
    match eng.decision.clone() {
        Decision::Conflict { ids } => {
            // 冻结现状：不撤销已生效的配置（撤销会让用户当场断网，而冲突只是
            // 「不确定该用哪个」），但也绝不往下走任何一支。
            let sig = ids.join("|");
            if sig == eng.conflict_shown {
                return None;
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
            None
        }
        Decision::NoActiveProfile => {
            eng.conflict_shown.clear();
            if !eng.no_match_is_actionable() {
                // 这一份快照连「此刻在哪个网络」都没读到 —— 保持现状：Active 不动、
                // 健康监测不停、兜底下发也不跑。等下一轮读到身份，或空转到
                // `UNIDENTIFIED_CONFIRMS` 轮之后再按零命中处置。
                log::debug(&i18n::t("engine.no_match_unread"));
                return None;
            }
            if eng.active_id.take().is_some() {
                eng.deactivate();
                log::info(&i18n::t("engine.no_match"));
            } else {
                eng.stop_monitor();
            }
            decide_fallback(eng, cfg)
        }
        Decision::Active { id } => {
            eng.conflict_shown.clear();
            let Some(profile) = cfg.profile_by_id(&id) else {
                eng.active_id = None;
                return None;
            };
            let profile = profile.clone();
            let fp = fingerprint_of(&profile);
            let fingerprint = eng.scheduler_fingerprint();
            if eng.is_blocked(&id, &fp, &fingerprint) {
                return None; // 这个「配置 + 网络」组合已经失败过一次，别反复弹授权框
            }
            let staying = eng.active_id.as_deref() == Some(id.as_str());
            if staying && eng.applied_fp.as_deref() == Some(fp.as_str()) {
                // 方案第 36 条：保持 Active 不重复下发、不重复跑一次性动作
                return None;
            }
            // THEN 缺省等于「这一支什么都不做」：它照样算 Active（条件命中了），
            // 只是没有网卡配置、也没有动作。给 commit 留一个空 net 去记档。
            let branch = Which::Then.of(&profile).cloned().unwrap_or_default();
            let (from, opts) = if staying {
                log::info(&i18n::tf("engine.reconfigure", &[("name", &profile.name)]));
                (None, RunOpts::RECONFIGURE)
            } else {
                let from = eng.active_display_name(cfg);
                eng.deactivate();
                (from, RunOpts::FULL)
            };
            let net = eng.branch_prepare(Which::Then, &branch);
            Some(Plan::Profile {
                profile: Box::new(profile),
                net,
                opts,
                staying,
                from,
                fp,
                gen: eng.generation,
            })
        }
    }
}

/// 锁内：零命中时算出兜底下发计划（`None` = 兜底也什么都不做）。
///
/// 零命中时的处置：3A（如果配了网卡配置）+ 3B（一次性动作与常驻 worker）。
/// 它不是 Profile：没有条件、不参与匹配、永远不会 Conflict。网络配置的存在理由只有一个 ——
/// 上一个 Profile 可能下发了静态 IP，零命中时必须有「回到自动获取」的落点；3B 则是
/// 「零命中期间该维持什么」，与 THEN 分支同一套执行链路。
///
/// 指纹覆盖整份 fallback 内容（网络 + 动作）：改任何一项都会重新走一遍，
/// 没改就一个字都不动 —— 否则每一轮评估都会重跑动作、重启 worker。
fn decide_fallback(eng: &mut Engine, cfg: &Config) -> Option<Plan> {
    let Some(fb) = cfg.fallback.as_ref().filter(|f| f.enabled) else {
        // 兜底被禁用/清空：之前那一组 worker 不该继续维持 —— 它守的是一个
        // 用户已经撤销的期望。记档一并作废：重新启用时要重新下发，而不是被
        // 「内容没变」挡住。
        eng.stop_fallback_workers();
        eng.fallback_fp = None;
        return None;
    };
    let has_actions =
        one_shot::any_enabled(&fb.one_shot) || fb.persistent.iter().any(|a| a.enabled);
    if fb.network.is_none() && !has_actions {
        // 全空的兜底 = 什么都不做（保持现状）。空配置的语义是「不干预」，
        // 不是「把之前干预过的东西撤掉」。
        eng.stop_fallback_workers();
        eng.fallback_fp = None;
        return None;
    }
    let fp = format!("fallback|{}", serde_json::to_string(fb).unwrap_or_default());
    if eng.fallback_fp.as_deref() == Some(fp.as_str()) {
        return None;
    }
    // 兜底 3A 失败记档：同一份配置 + 同一张网络指纹下不再重跑 3A，
    // 否则用户持续缺席时每轮都会被要求授权（与 Profile 的 `blocked` 同机制）。
    let fingerprint = eng.scheduler.fingerprint().to_string();
    if eng.is_fallback_blocked(&fp, &fingerprint) {
        return None;
    }
    // 新的一组起来之前先把旧的叫停 —— 与 `branch_prepare` 同一条顺序规则：
    // 旧配置的「保持 VPN 连接」不能在 3A 下发的同时还去抢路由表。
    eng.stop_fallback_workers();
    let net = fb.network.clone();
    if net.is_some() {
        // 下发的就是本机真实的网络改动：下一轮必须先重采样，否则「零命中 → 回落 DHCP」
        // 之后引擎还拿着静态 IP 时代的旧快照，指纹里的 primary/接口列表都可能是老的。
        eng.request_resample();
    }
    Some(Plan::Fallback {
        fb: Box::new(fb.clone()),
        net,
        fp,
        fingerprint,
        gen: eng.generation,
    })
}

/// 锁内：3A 已在锁外跑完，这里只做「记账 + 起 3B」。
///
/// `generation` 的复查已在 [`reconcile`] 里做完，进到这里时这份计划仍然有效。
fn commit_plan(
    state: &Arc<AppState>,
    eng: &mut Engine,
    plan: Plan,
    three_a: Option<Stage3A>,
    allowed: &Arc<AllowedScripts>,
) {
    match plan {
        Plan::Profile {
            profile,
            opts,
            staying,
            from,
            fp,
            ..
        } => {
            let profile = *profile; // 拆箱：下面只用 &profile 与它的字段
            let failure = match &three_a {
                Some(Stage3A::Failed { reason }) => Some(reason.clone()),
                _ => None,
            };
            match failure {
                Some(reason) => {
                    let msg =
                        i18n::tf("engine.error", &[("name", &profile.name), ("error", &reason)]);
                    log::error(&msg);
                    eng.record_failure(&profile.id, fp, reason);
                    // 重下失败不弹「应用失败」气泡：多半是用户改完配置就等着生效，
                    // 报错已经在日志和该 Profile 的红叉上。真正的切换才通知。
                    if !staying {
                        crate::state::emit_action(state, "apply", false, msg);
                    }
                }
                None => {
                    if let Some(Stage3A::Applied) = &three_a {
                        log::debug(&i18n::tf(
                            "engine.pass_3a",
                            &[("name", &profile.name), ("branch", Which::Then.name())],
                        ));
                    }
                    if !staying {
                        eng.active_id = Some(profile.id.clone());
                    }
                    eng.errors.remove(&profile.id);
                    eng.mark_applied(fp);
                    if !staying {
                        log::info(&i18n::tf("engine.switch", &[
                            ("from", from.as_deref().unwrap_or("-")),
                            ("to", &profile.name),
                        ]));
                        log::info(&i18n::tf("notify.applied", &[("name", &profile.name)]));
                    }
                }
            }
            // 3A 失败时 branch_3b 自己会整段跳过（硬屏障）。
            eng.branch_3b(state, &profile, Which::Then, opts, allowed, three_a);
        }
        Plan::Fallback { fb, fp, fingerprint, .. } => {
            if let Some(Stage3A::Failed { reason }) = &three_a {
                log::error(&i18n::tf("engine.fallback_failed", &[("error", reason)]));
                // 3A 失败 = 3B 一条都不跑（与分支同一道硬屏障）；记档不写，
                // 下一轮（网络或配置一变）会自动重试。
                eng.record_fallback_failure(fp, fingerprint);
                return;
            }
            eng.fallback_fp = Some(fp);
            eng.applied_fp = None;
            eng.active_id = None;
            log::info(&i18n::t("engine.fallback_applied"));
            // —— 3B：合成一个说得清归属的身份 ——
            // 兜底不接管健康监测：它没有「当前环境」可探测，也没有 Profile 可归属。
            // holder 必然挂着一支 THEN（见 fallback_holder），branch_3b 由此取到那一支。
            let holder = fallback_holder(&fb);
            eng.branch_3b(
                state,
                &holder,
                Which::Then,
                RunOpts::FALLBACK,
                allowed,
                three_a,
            );
        }
    }
}

/// 给兜底合成一个「说得清归属」的 Profile。
///
/// 3B1 的留痕（`RunRecord`）与 3B2 的 worker 归属都按 Profile id 记录；兜底没有 Profile，
/// 合成一个（id 用保留名 [`FALLBACK_ID`]）之后，界面与日志不必为「这条记录是不是兜底的」
/// 另写一套特判。`network` 留空：3A 已由 [`apply_fallback`] 亲自跑过，挂上去会二次下发。
fn fallback_holder(fb: &FallbackConfig) -> Profile {
    Profile {
        id: FALLBACK_ID.to_string(),
        name: i18n::t("engine.fallback_name"),
        then: Some(Branch {
            network: None,
            one_shot: fb.one_shot.clone(),
            persistent: fb.persistent.clone(),
        }),
        ..Default::default()
    }
}

/// 「立即应用」：**仍然要经过该 Profile 自己的条件判定**（方案第 38/39/40 条）。
///
/// 它不是后门：命中就跑 THEN、不命中就跑 ELSE；若此刻有多个 Profile 命中则直接拒绝
/// 并列出冲突名单 —— 否则用户可以绕过「多命中不自动选择」这条核心约束。
///
/// 与 [`reconcile`] 同样是三段式（审计 B2）：锁内判定 + 快照网络 → **放锁**跑 3A →
/// 重新持锁记档。「立即应用」尤其不该持锁下发：这一问一答本来就在等结果。
fn manual_apply(state: &Arc<AppState>, id: &str, allowed: &Arc<AllowedScripts>) {
    struct Manual {
        profile: Profile,
        net: Option<NetworkConfig>,
        matched: bool,
        which: Which,
        fp: String,
        gen: u64,
    }
    // —— decide（锁内）——
    let plan = {
        let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
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
        let Some(branch) = which.of(profile).cloned() else {
            let msg = i18n::tf("engine.no_branch", &[
                ("name", &profile.name),
                ("branch", which.name()),
            ]);
            crate::state::emit_action(state, "apply", false, msg);
            return;
        };
        let profile = profile.clone();
        let fp = fingerprint_of(&profile);
        let net = eng.branch_prepare(which, &branch);
        Manual {
            profile,
            net,
            matched,
            which,
            fp,
            gen: eng.generation,
        }
    };
    // —— apply（锁外）——
    let three_a = {
        let degrade = plan.net.as_ref().map(degrade_enabled).unwrap_or(false);
        run_3a_lockfree(state, plan.net.as_ref(), degrade)
    };
    // —— commit（重新持锁，复查决策代数）——
    let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    if eng.generation != plan.gen {
        log::debug(&i18n::t("engine.plan_superseded"));
        return;
    }
    let Manual {
        profile,
        matched,
        which,
        fp,
        ..
    } = plan;
    match &three_a {
        Some(Stage3A::Failed { reason }) => {
            let msg = i18n::tf("engine.error", &[("name", &profile.name), ("error", reason)]);
            log::error(&msg);
            if matched {
                eng.record_failure(&profile.id, fp, reason.clone());
            }
            crate::state::emit_action(state, "apply", false, msg);
        }
        _ => {
            if let Some(Stage3A::Applied) = &three_a {
                log::debug(&i18n::tf(
                    "engine.pass_3a",
                    &[("name", &profile.name), ("branch", which.name())],
                ));
            }
            let msg = i18n::tf("engine.manual_ok", &[
                ("name", &profile.name),
                ("branch", which.name()),
            ]);
            log::info(&msg);
            crate::state::emit_action(state, "apply", true, msg);
            if matched {
                // 唯一命中：手动应用等价于正常激活，记档让引擎别再重下一遍
                // 先停掉任何旧 Profile 残留的监测线程，否则它会继续 armed 并在下次探测时
                // 拆掉刚刚手动激活的这个现场。
                eng.stop_monitor();
                eng.active_id = Some(profile.id.clone());
                eng.applied_fp = Some(fp);
                eng.fallback_fp = None;
                eng.decision = Decision::Active {
                    id: profile.id.clone(),
                };
            }
        }
    }
    // 3A 失败时 branch_3b 自己会整段跳过（硬屏障）。
    eng.branch_3b(state, &profile, which, RunOpts::MANUAL, allowed, three_a);
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

/// 面板/IPC 的两个后台动作。都在引擎线程执行：都涉及提权或秒级等待。
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
            // 平台状态缓存已由 exec_ops 丢弃，立刻采一份就是切 DHCP 之后的现场。
            // 采样在锁外：macOS 上一次 get_status 最坏含一次数秒的 system_profiler。
            let snap = NetworkSnapshot::sample(&state.plat);
            {
                let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
                // 已切回 DHCP：当前生效状态不再等于任何 manual 配置，记档全部作废。
                // worker 同属那份配置 —— 用户手动接管了这张网卡，就不能再让后台每 30 秒
                // 把某个 Profile 的状态改回来；健康监测同理（它唯一能做的事就是
                // 「探测失败 → 再回落一次 DHCP」，此刻既多余又吓人）。
                eng.stop_monitor();
                eng.stop_workers();
                eng.applied_fp = None;
                eng.fallback_fp = None;
                eng.active_id = None;
                // 先把这份快照记成新基线，再武装暂停：这样「暂停等到下一次网络变化」
                // 说的是**之后**的变化 —— 切 DHCP 自己引起的现场变动不算数。
                eng.note_sampled(snap, Instant::now());
                eng.hold_automation();
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

/// 「探测当前网络」：重新采样 → 立即评估 → 按判定执行。
///
/// 它回答的是「现在这台网络该不该跑自动化、该跑哪条」：恰中一条就按那一条的
/// THEN 执行（网络 + 3B，与面板上的「立即应用」同一条链路），多命中只报告不挑人，
/// 零命中只报告不动网卡 —— 兜底属于引擎的自动行为，等它自己按节律走。
///
/// 它是用户的显式动作，不受 DHCP 暂停约束（暂停只拦引擎自己发起的轮询与下发），
/// 也不会解除暂停。
fn probe(state: &Arc<AppState>) {
    log::info(&i18n::t("notify.probe_running"));
    let now = Instant::now();
    // 采样在锁外，理由同 set_dhcp。
    let snap = NetworkSnapshot::sample(&state.plat);
    // 评估在锁内，冲突名单也在锁内解析好 —— manual_apply 自己管加锁（它要放锁跑 3A），
    // 所以这里只把「判定 + 名字 + allow-list」带出来，不把锁传下去。
    let (allowed, decision, conflict_names) = {
        let mut eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        eng.note_sampled(snap, now);
        eng.evaluate(&cfg, now);
        let allowed = Arc::new(AllowedScripts {
            scripts_dir: state.scripts_dir.clone(),
            explicit: cfg.allowed_scripts.clone(),
        });
        let decision = eng.decision.clone();
        let conflict_names: Vec<String> = match &decision {
            Decision::Conflict { ids } => ids
                .iter()
                .map(|id| {
                    cfg.profile_by_id(id)
                        .map(|p| p.name.clone())
                        .unwrap_or_else(|| id.clone())
                })
                .collect(),
            _ => Vec::new(),
        };
        (allowed, decision, conflict_names)
    };
    match decision {
        Decision::Active { id } => manual_apply(state, &id, &allowed),
        Decision::Conflict { .. } => {
            let msg = i18n::tf("notify.probe_conflict", &[("names", &conflict_names.join(", "))]);
            log::warn(&msg);
            crate::state::emit_action(state, "probe", false, msg);
        }
        Decision::NoActiveProfile => {
            let msg = i18n::t("notify.probe_no_match");
            log::info(&msg);
            crate::state::emit_action(state, "probe", false, msg);
        }
    }
    publish_view(state);
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

    /// 「设为 DHCP」后的暂停：轮询停摆，直到网络真的变了才恢复。
    /// 判据落在 `due()` 上（`pass` 的提前收工与 `evaluate` 的节律推进都经由它），
    /// 所以这里只钉 due 的空与非空。
    #[test]
    fn dhcp_hold_stops_the_schedule_until_the_network_changes() {
        let mut eng = Engine::new();
        let mut office = profile("office");
        office.detection.mode = crate::config::DetectionMode::PollingOnly;
        office.detection.poll_interval_secs = 30;
        let cfg = Config {
            profiles: vec![office],
            ..Default::default()
        };
        let t0 = Instant::now();
        let snap = |ssid: &str| NetworkSnapshot {
            ssid: Some(ssid.to_string()),
            ..Default::default()
        };
        eng.note_sampled(snap("A"), t0);
        eng.evaluate(&cfg, t0);
        assert!(eng.due(&cfg, t0).is_empty(), "刚评估过，没到下一个轮询点");

        // set_dhcp 的收尾顺序：先把这份快照记成新基线、再武装暂停 ——
        // 切 DHCP 自己引起的现场变动不算「网络变化」。
        eng.note_sampled(snap("A"), t0);
        eng.hold_automation();
        let later = t0 + std::time::Duration::from_secs(3600);
        assert!(
            eng.due(&cfg, later).is_empty(),
            "暂停期间轮询必须停摆，这就是「自动化暂时失效」"
        );

        // 网络变化 → 解除；节律交还给 Profile 自己的 timing。
        // 解除的同一瞬间不许评估：变化刚发生，正处在 change_delay 的稳定窗口里，
        // 而这一窗口对轮询同样生效（见 `Scheduler::is_due`）—— 暂停结束后多等 5 s
        // 再判，换来的是「判的时候网络已经停了」。
        eng.note_sampled(snap("B"), later);
        assert!(!eng.automation_held(), "观察到变化就要解除暂停");
        assert!(
            eng.due(&cfg, later).is_empty(),
            "变化还没稳定，恢复自动化不等于立刻判定"
        );
        assert_eq!(
            eng.due(&cfg, later + std::time::Duration::from_secs(5)),
            vec!["office".to_string()]
        );
    }

    /// 一份读不到身份的快照不构成「零命中」的证据，`decide_plan` 的 NoActiveProfile
    /// 那一支第一件事就是这个闸门：闸门不放行时既不撤销 Active，也不下发兜底。
    ///
    /// 不加这道判据时会发生什么：macOS 上 SSID 只有 CoreWLAN 一个来源，网关 MAC 要靠
    /// ARP 表里恰好有默认路由那一条 —— 重连过程中的某一轮它们可以同时为空。那一份
    /// 快照判出来就是「一个 Profile 都没命中」，于是兜底把正在工作的静态 IP 改成
    /// 自动获取、把配置好的 DNS 清空成系统自动，而界面上一切看起来都正常。
    ///
    /// 放行条件刻意不是「永远别动」：连续空到 `UNIDENTIFIED_CONFIRMS` 轮之后必须照常
    /// 兜底，否则真遇上读不出身份的网络（拔网线、纯有线且 ARP 空表）就再也没有落点。
    #[test]
    fn a_zero_match_needs_an_observed_identity_before_it_can_teardown() {
        let mut eng = Engine::new();
        let t0 = Instant::now();
        let blank = |ssid: Option<&str>| NetworkSnapshot {
            ssid: ssid.map(|s| s.to_string()),
            ..Default::default()
        };

        // 第一项读到就够：有线网络本来就没有 SSID，网关 MAC 就是它的身份。
        eng.note_sampled(blank(None), t0);
        assert_eq!(eng.identity_misses, 1);
        assert!(
            !eng.no_match_is_actionable(),
            "单轮读空是读取失败的形状，不是「换了个陌生网络」的形状"
        );
        eng.note_sampled(blank(Some("Office")), t0 + std::time::Duration::from_secs(2));
        assert_eq!(eng.identity_misses, 0, "读到身份就要立刻清零计数");
        assert!(eng.no_match_is_actionable());

        // 连续读空到第 `UNIDENTIFIED_CONFIRMS` 轮：认了，兜底必须还能起作用。
        for i in 0..UNIDENTIFIED_CONFIRMS {
            eng.note_sampled(blank(None), t0 + std::time::Duration::from_secs(4 + i as u64 * 2));
        }
        assert!(
            eng.no_match_is_actionable(),
            "稳定读不出身份是既成事实，拦住它等于让兜底永久失效"
        );
    }

    /// 暂停里的 Profile 展示成 Suspended（前端 PAUSED），不是 ERROR：
    /// 暂停是用户在「设为 DHCP」里亲自点的，而 ERROR 在前端的含义是
    /// 「对网卡动过手且失败了」—— 这两件事混成一个红叉，用户会去改本来正确的条件。
    #[test]
    fn a_held_profile_reads_as_suspended_not_error() {
        let mut eng = Engine::new();
        let cfg = Config {
            profiles: vec![profile("office")],
            ..Default::default()
        };
        eng.evaluation = Evaluation {
            profiles: vec![ev("office", true)],
            matched_ids: ids(&["office"]),
        };
        // set_dhcp 之后的现场：判定快照还停在它身上，但 active_id 已被清掉、
        // 暂停已武装（这正是「命中却没生效」在暂停期的固有形态）。
        eng.decision = Decision::Active { id: "office".into() };
        eng.active_id = None;
        eng.hold_automation();

        assert_eq!(eng.status_of(&ev("office", true)), DisplayStatus::Suspended);
        let view = eng.view(&cfg);
        assert_eq!(view.profiles[0].status, DisplayStatus::Suspended);
        assert_eq!(view.profiles[0].error, None);
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(v["profiles"][0]["status"], "suspended", "前端按这个字符串分叉");
        assert_eq!(v["state"]["state"], "active", "暂停不改判定，只让引擎住手");
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

    /// 静态 IP 下发之后，指纹里的每一项都没动（SSID / 网关 MAC / BSSID / 网卡集合），
    /// 于是没有任何 Profile「到期」，引擎会提前收工、界面继续挂着下发前那份快照。
    /// `resample_wanted` 就是为了跳过那条捷径，而它必须**只**管住下一轮。
    #[test]
    fn a_resample_request_hands_over_to_exactly_one_pass() {
        let mut eng = Engine::new();
        assert!(
            !eng.take_resample_request(),
            "没人动过网络，就不该有强制轮"
        );
        eng.request_resample();
        assert!(eng.take_resample_request(), "下发之后那一轮必须被强制");
        assert!(
            !eng.take_resample_request(),
            "取走即清：再往后该回到自己的采样节律"
        );
    }

    /// 兜底的身份是合成的：id 用保留名（运行留痕与 worker 归属都认它）、名字来自字典
    /// （用户没有可改的 name）、网络必须留空 —— 3A 由 `decide_fallback` 亲自下发过，
    /// 挂在这支分支上会让 `commit_plan` 再下发一次。
    #[test]
    fn the_fallback_holder_is_a_synthetic_identity_for_attribution_only() {
        use crate::config::{OneShotAction, OneShotActionType, PersistentAction, PersistentActionType};
        let fb = FallbackConfig {
            enabled: true,
            network: None,
            one_shot: vec![OneShotAction {
                id: "o1".into(),
                enabled: true,
                action: OneShotActionType::LaunchApp {
                    app: "Notes.app".into(),
                    args: Vec::new(),
                },
            }],
            persistent: vec![PersistentAction {
                id: "p1".into(),
                enabled: true,
                action: PersistentActionType::KeepWireGuardConnected {
                    tunnel: "wg0".into(),
                    interval_secs: 30,
                },
            }],
        };
        let holder = fallback_holder(&fb);
        assert_eq!(holder.id, FALLBACK_ID, "归属记录用的就是保留 id");
        assert!(holder.enabled, "合成身份永远可执行：enabled 描述的是用户的选择");
        assert!(holder.else_branch.is_none(), "兜底没有 ELSE 可言");
        assert!(holder.rules.is_empty(), "它不参与匹配，条件一个都不该有");
        let then = holder.then.as_ref().expect("3B 挂在这支分支上");
        assert!(then.network.is_none(), "3A 不能挂在合成分支上（见函数文档）");
        assert_eq!(then.one_shot.len(), 1, "动作按原样带过去");
        assert_eq!(then.persistent.len(), 1);
    }
}
