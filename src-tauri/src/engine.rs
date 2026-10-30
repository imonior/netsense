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
// `run_on_main_thread` 把「拉起结果弹窗」这件事派回主线程执行（GUI 操作必须在主线程）；
// `get_webview_window` 等窗口查询由 `AppHandle` 自身提供，无需额外 trait import。
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use crate::automation::{self, one_shot, persistent, AllowedScripts};
use crate::conditions::{
    evaluate_all, eval_profile, DecisionInputs, Evaluation, NetworkSnapshot, ProfileEvaluation,
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

/// "No value" in a log line: a single dash, not `None` and not an empty string —
/// when troubleshooting, "this field was not read" must be distinguishable at a
/// glance from "this field is genuinely empty".
///
/// 日志里的「没有值」：一个短横，而不是 `None` 或空串 ——
/// 排查时一眼看得出「这一项没读到」，而不是「这一项就是空的」。
fn or_dash(v: Option<&str>) -> String {
    v.unwrap_or("-").to_string()
}

/// Identity summary of one snapshot: SSID / gateway MAC / primary interface.
/// The fingerprint-change log line renders its `{from}` / `{to}` with this — those
/// three fields are exactly the answer to "which network did we move to", while the
/// raw fingerprint additionally carries BSSID, the interface set and the tunnel set,
/// which would only get in the way inside a log line.
///
/// 一份快照的身份摘要：SSID / 网关 MAC / 主网卡。
/// 指纹变化日志的 `{from}` / `{to}` 用它渲染 —— 这三项正是「换到了哪张网」的答案，
/// 而原始指纹还额外缠着 BSSID、网卡集合与隧道集合，进了日志只会碍事。
fn ident_of(s: &NetworkSnapshot) -> String {
    format!(
        "{} / {} / {}",
        or_dash(s.ssid.as_deref()),
        or_dash(s.gateway_mac.as_deref()),
        or_dash(s.primary_interface.as_deref()),
    )
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
    /// 连续多少轮采样**判不动**当前配置的那些条件（条件要读的身份字段没读到）。
    /// 零命中要拆现网之前，用它确认「真的是一个陌生网络」而不是「这几轮什么都没读到」，
    /// 见 `NetworkSnapshot::can_decide` 与 `Engine::no_match_is_actionable`。
    undecidable_rounds: usize,
    /// 当前配置的判定需要快照里的哪几项读数（[`Config::decision_inputs`]）。
    ///
    /// 每轮评估时刷新；首份配置还没读进来时是 [`DecisionInputs::EVERYTHING`]，也就是
    /// 「先按最严的那一侧拦着」—— 还不知道条件要读什么的时候，不拿残缺快照去拆现网。
    decision_inputs: DecisionInputs,
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
            undecidable_rounds: 0,
            decision_inputs: DecisionInputs::default(),
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
        // Identity summary taken before the snapshot is replaced: the fingerprint-change
        // log line has to say "from which network to which network", and the raw
        // six-field pipe-joined fingerprint (NetworkSnapshot::fingerprint) is unreadable.
        // 覆盖前的身份摘要：指纹变化日志要能说出「从哪张网到哪张网」，
        // 而六段管道分隔的原始指纹（`NetworkSnapshot::fingerprint`）人读不了。
        let previous_ident = ident_of(&self.snapshot);
        self.last_sample = Some(now);
        self.undecidable_rounds = if snap.can_decide(self.decision_inputs) {
            0
        } else {
            self.undecidable_rounds + 1
        };
        self.snapshot = snap;
        // Sampling itself is not logged: a line every 2s repeats the same identity
        // fields until something changes, and the moment it changes is already
        // covered by the fingerprint-change line below (default level).
        // 采样本身不记日志：每 2 秒重复同一份身份字段，而变化时刻已由下面的
        // 指纹变化行（默认档）记下。
        let changed = self.scheduler.observe(&self.snapshot, now);
        if changed {
            // A fingerprint change is the authoritative "the network connection
            // changed" signal: recorded at the default level too — it is the one
            // line in the whole automation chain most worth checking timestamps against.
            // 指纹变化是「网络连接改变」的权威信号：默认档也记，
            // 它是整条自动化链路里最值得对时间戳的那一行。
            log::info(&i18n::tf("net.changed", &[
                ("from", &previous_ident),
                ("to", &ident_of(&self.snapshot)),
            ]));
        }
        if changed && self.hold {
            // 网络真的变了：DHCP 后的手动暂停到此结束，自动化恢复正常节律。
            // （判定仍走各 Profile 自己的 change_delay —— 变化刚发生，暂不「稳定」。）
            self.hold = false;
            log::info(&i18n::t("engine.hold_released"));
        }
    }

    /// 零命中这一判定能不能落成处置（撤销 Active、跑兜底）。
    ///
    /// 判据不是「命中数为 0」，而是「命中数为 0，**且**这一份快照读到了判定所需要的那
    /// 几项身份」。只要有一项没读到，「都没命中」就只是「什么都没读到」的形状：macOS 的
    /// SSID 只有 CoreWLAN 一个来源（命令行在 Sequoia 上全部涂黑），网关 MAC 要靠 ARP 表
    /// 里恰好有默认路由那一条，BSSID 同理 —— 而 Wi-Fi 打盹的那一轮，SSID 读不出来、旧的
    /// 网关 MAC 还挂在表里，此时按零命中处置就是**当着正常工作的网络**拆掉现网：静态 IP
    /// 变自动获取、指定 DNS 被清空，界面上还看不出是谁改的。
    ///
    /// 判不动持续到 `UNIDENTIFIED_CONFIRMS` 轮之后就放行：那时候它已经是「这个网络给不出
    /// 这些条件所要的证据」的稳定事实（网线拔了、纯有线却没有 SSID），兜底必须照常起作用。
    pub fn no_match_is_actionable(&self) -> bool {
        self.snapshot.can_decide(self.decision_inputs)
            || self.undecidable_rounds >= UNIDENTIFIED_CONFIRMS
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
        // 判定需要什么证据，是**配置**决定的：只有一条 SSID 型条件的配置，读不到 SSID
        // 就是判不动；只有网关 MAC 型条件的配置，同一份快照照样能出结论。放在这里刷新
        // 而不是在加载配置时做，是因为采样早于评估 —— 首份配置进来之前先按最严的判。
        self.decision_inputs = cfg.decision_inputs();
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
        // Instrumentation: which schedules fired this round (default level), and the
        // per-profile results (debug level). The ask was "record every detection":
        // the default level gives "which cadences triggered this round", while the
        // per-profile hit/miss goes to Debug — under a polling config that is N lines
        // every 30 seconds, too much for the default level; when actually asking
        // "why is this profile not taking effect", switch the level to Debug.
        // —— 埋点：这一轮评估了谁（默认档），逐 Profile 结果如何（debug 档）——
        // 用户要的是「每一次检测都有记录」。默认档给「哪几条节律触发了本轮」，
        // 逐条命中与否放 Debug：轮询配置下它每 30 秒就是 N 行，默认档放不下 ——
        // 而真的排查「某条 Profile 怎么不生效」时，把级别调到 Debug 就看得到。
        if !due.is_empty() {
            let names: Vec<String> = due
                .iter()
                .map(|id| {
                    cfg.profile_by_id(id)
                        .map(|p| p.name.clone())
                        .unwrap_or_else(|| id.clone())
                })
                .collect();
            log::info(&i18n::tf("engine.eval_due", &[("names", &names.join(", "))]));
        }
        for ev in &self.evaluation.profiles {
            if !ev.enabled {
                continue; // 禁用的 Profile 不参与检测，记它「未命中」是误导
            }
            if ev.matched {
                log::debug(&i18n::tf("engine.eval_matched", &[("name", &ev.name)]));
            } else {
                log::debug(&i18n::tf("engine.eval_unmatched", &[("name", &ev.name)]));
            }
        }
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
            fallback_active: self.fallback_fp.is_some(),
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
            Some(Stage3A::Applied { .. }) => ThreeAOutcome::Applied,
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
            // 通道没了（理论上是退出期的竞态）：动作提交不出去，不会有 RunDone 来
            // 解除弹窗的推迟。这里就地清掉等待标志并把窗口叫出来 —— 网络结果已经
            // 事件发到隐藏窗口了，别让用户盯着一扇永远不出现的弹窗。
            if state
                .applied_show_pending
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                show_applied_window(state);
            }
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
            let dhcp_ok = match fb_state.plat.set_dhcp() {
                Ok(()) => true,
                Err(e) => {
                    log::error(&i18n::tf("notify.dhcp_failed", &[("error", &e)]));
                    false
                }
            };
            if dhcp_ok {
                // 回落 DHCP 是一次真实的网络变更（macOS 上顺带把 DNS 交回自动获取），
                // 用户必须看见它 —— 否则「我填的静态 IP 怎么没了」只能靠猜。
                announce_change(
                    &fb_state,
                    &network::NetworkChange {
                        mode: Some("dhcp".into()),
                        dns_cleared: true,
                        ..Default::default()
                    },
                    true,
                );
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
    /// 兜底当前是否处于「已生效」状态。`true` 表示零命中且兜底已落地
    ///（网络回落 / 3B 已跑）。前端据此把兜底当成「当前生效的那一套」来呈现
    ///（active profile、列表高亮、what-it-runs 等）。
    ///
    /// 来源是 [`Engine::fallback_fp`]（一旦被某个 Profile 接管、设为 DHCP 或改配置即清空），
    /// 而不是一次性的 `decision` —— 后者每轮评估都会被 [`decide`] 重置成 `NoActiveProfile`，
    /// 撑不到下一轮广播，所以不能直接用它判断「兜底是否在生效」。
    pub fallback_active: bool,
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
                // Every completed 3B1 action is recorded (default level): this is
                // the line the ask "record every execution action" refers to;
                // failures have their own error-level line.
                // 每一条 3B1 动作的完成都记（默认档）：用户要的「每一个执行动作
                // 都有记录」指的就是这一行，失败那条另有 error 档。
                None => log::info(&i18n::tf("engine.action_done", &[("label", &outcome.label)])),
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
                // 3B1 收尾 = 这一轮自动化（网络配置 + 一次性动作）真正结束：结果弹窗
                // 若在等动作结果（`emit_applied` 推迟了显示），此刻叫出来 —— 用户看到
                // 的就是完整的「动作成败 + 网络配置」，而不是先弹一半、结果慢慢补。
                // 已被取代的那次运行（None 分支）不算：等最新那次的收尾。
                if state
                    .applied_show_pending
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    show_applied_window(state);
                }
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
    // DNS 兜底来自软件设置：`enabled` 为假就给空列表（`clamped` 已保证「开着却没有一台
    // 合法服务器」不会存在），空列表在 3A 里就是「没有兜底」。
    // 这里只短暂持锁取一次值：3A 本身（一串子进程 + 回读采样）是在锁外跑的。
    let fallback = state
        .settings
        .lock()
        .map(|g| g.dns_fallback.servers.clone())
        .unwrap_or_default();
    Some(match network::apply_3a(&state.plat, net, &fallback) {
        Stage3A::Failed {
            reason,
            change,
            rolled_back,
        } => {
            // 保底：探测里开了 fallback 就回落 DHCP（3A 失败处置的一部分）
            if degrade {
                match state.plat.set_dhcp() {
                    Ok(()) => {
                        log::warn(&i18n::t("notify.fallback"));
                        // 回落 DHCP 是一次**真实的网络变更**（macOS 上还会顺带把 DNS
                        // 交回自动获取），必须让用户看见 —— 否则界面上写着的配置和
                        // 现场实际生效的配置就此对不上，而用户毫不知情。
                        //
                        // 这里传 `true`：走得到这个分支就说明 `set_dhcp()` **成功**了，
                        // 回落确实落地了（DNS 也真的被交回自动获取）。传 `false` 会让
                        // 弹窗说「未生效」，而现场恰恰是生效的 —— 那比不说更糟。
                        // 3A 本身失败这件事由 `engine.error` / `engine.fallback_failed`
                        // 那几条消息负责说，两件事不必挤进同一句话。
                        announce_change(
                            state,
                            &network::NetworkChange {
                                mode: Some("dhcp".into()),
                                dns_cleared: true,
                                ..Default::default()
                            },
                            true,
                        );
                    }
                    Err(e) => {
                        log::error(&i18n::tf("notify.dhcp_failed", &[("error", &e)]));
                    }
                }
            }
            Stage3A::Failed {
                reason,
                change,
                rolled_back,
            }
        }
        ok => ok,
    })
}

/// 一次变更播报要用的两句话：日志那一条与弹窗那一条。
///
/// 抽成纯函数是为了能被单测直接盯住 —— 这个契约（「失败路径不得说成
/// 「已变更」）正是 v1.8.5 修掉的一个真缺陷：改动被回滚了，弹窗却报
/// 「网络配置已变更」，用户据此以为自己的静态 IP 还在生效。
///
/// The two sentences a change announcement needs: the log line and the toast.
/// Pure so a test can pin the contract — the failure path must never claim
/// the settings *changed*, because those writes were rolled back.
/// Returns `None` when nothing changed (no announcement at all).
fn change_lines(change: &network::NetworkChange, ok: bool) -> Option<(String, String)> {
    let summary = change.summary()?;
    let (log_line, notice) = if ok {
        (
            i18n::tf("net.change", &[("detail", &summary)]),
            i18n::tf("net.change_notice", &[("detail", &summary)]),
        )
    } else {
        (
            i18n::tf("net.change_undone", &[("detail", &summary)]),
            i18n::tf("net.change_undone_notice", &[("detail", &summary)]),
        )
    };
    Some((log_line, notice))
}

/// 把一次「动了系统哪些参数」既记进日志、又弹窗给用户。
///
/// 为什么日志与弹窗要合到一个函数里：这两处必须说的是**同一句话**。
/// 分头写的话，迟早会出现「日志里写的是 DNS 被清空、界面弹的是另一回事」，
/// 用户对照两边时会以为日志和界面在说两场不同的网络变更 —— 而实际只有一次。
///
/// `ok` 决定弹窗的语气：成功是一次**已落地**的变更（用户要看见「我改了什么」），
/// 失败是「改动没能落地 / 已被回滚」（同样要看见，否则用户以为改动生效了）。
///
/// 两句话必须**不同**：失败路径上的那几项已经被回滚改回去了，若也说成
/// 「网络配置已变更」，用户会以为自己的静态 IP 还在生效 —— 而现场恰恰不是那样。
/// 报「未生效」既不隐瞒「动过」（明细照旧列出），也不谎报「改成了」。
///
/// 空的变更明细（什么都没改，例如系统本就合规而跳过了下发）**不发任何提示**：
/// 「网络被改了」这句话必须是假的反面 —— 没改就不能说改了。
fn announce_change(state: &Arc<AppState>, change: &network::NetworkChange, ok: bool) {
    let Some((log_line, notice)) = change_lines(change, ok) else {
        return;
    };
    // 日志永远记一条：它是「这台机器的网络被谁改了」这件事的权威留痕。
    // 失败那条记 warn 而不是 info —— 「动过又撤回」比「改成了」更需要被看见。
    if ok {
        log::info(&log_line);
    } else {
        log::warn(&log_line);
    }
    crate::state::emit_action(state, "network", ok, notice);
}

/// 一次「下发失败 + 已回滚」的播报：两条日志、一条弹窗。
///
/// 为什么弹窗只有一条：前端的 toast 是**单条覆盖**的（`clearTimeout` 后覆盖内容），
/// 连发两条用户只看得见最后一条 —— 于是「动过什么没生效」会被「已回滚到什么」盖掉。
/// 两句话必须挤进同一条，而日志不怕多，两条各记一行才说得清谁改了什么。
///
/// 回滚那一项为空（快照什么都没带 ⇒ 回滚什么都没写）时退化成普通失败播报：
/// 没写就不能说写了。
fn announce_rollback(
    state: &Arc<AppState>,
    change: &network::NetworkChange,
    rolled_back: Option<&network::NetworkChange>,
) {
    // 恢复失败（None）= 回滚写没写成不知道：那半句就整个不说，不谎报。
    let rolled_detail = rolled_back.and_then(|rb| rb.summary());
    let Some((log_line, notice)) = change_lines(change, false) else {
        // 「动过什么」是空的 ⇒ 这次连下发都没真正写下去，只有回滚值得一提。
        if let Some(detail) = &rolled_detail {
            let line = i18n::tf("net.rolled_back", &[("detail", detail)]);
            log::warn(&line);
            crate::state::emit_action(
                state,
                "network",
                false,
                i18n::tf("net.rolled_back_notice", &[("detail", detail)]),
            );
        }
        return;
    };
    log::warn(&log_line);
    let Some(detail) = &rolled_detail else {
        crate::state::emit_action(state, "network", false, notice);
        return;
    };
    // 回滚写回去的那几项（日志那条已在 `rollback_and_report` 里记过，这里只补弹窗）。
    let rolled = i18n::tf("net.rolled_back_notice", &[("detail", detail)]);
    let sep = i18n::t("net.change_sep");
    crate::state::emit_action(state, "network", false, format!("{notice}{sep}{rolled}"));
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
            // Held rounds stay silent too: the pause is already bracketed by the
            // user's own action line at the start and engine.hold_released at the
            // end; a line per second inside the bracket would only be filler.
            // 暂停轮同样沉默：暂停区间已由用户自己的动作行开头、engine.hold_released
            // 收尾，区间内每秒一行只是填充物。
            return;
        }
        if manual.is_none() && !forced && eng.due(&cfg, now).is_empty() {
            // An idle round stays silent on purpose: the loop ticks once a second,
            // so a line here would be eighty thousand identical lines a day. "The
            // engine is alive" is already answered by the startup line, every
            // net.changed and every engine.eval_due; silence is the correct
            // expression of "nothing due".
            // 空转轮刻意沉默：主循环每秒一跳，在这里记一行就是一天八万行复读。
            // 「引擎活着」已由启动行、每条 net.changed 与每条 engine.eval_due 回答，
            // 沉默本身就是「什么都没到期」的正确表达。
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

/// 「自动化应用了一个配置」时发给面板的弹窗内容（事件 `netsense://applied`）。
///
/// 只报**最终结果**：这一轮到底往系统里写了哪几项（`change`，来自 3A 下发后的回读记账），
/// 而不是下发前的计划态、也不是配置文件原文。版式是一份**分组清单**
/// （系统 / IPv4 / IPv6 / DNS）：每行 = 标签 + 值 + ✅/❌，失败行另带红色明细，
/// 底部是错误行 —— 对照用户给的参考图（hammerspoon 版的「网络配置仅部分生效」弹窗）。
///
/// 「弹窗」与「引擎真的动了手」是同一件事：轮询中没有任何规则触发改变时 [`decide_plan`]
/// 直接返回 `None`，根本到不了 commit，所以不会出现「什么都没改却弹窗」的噪音。
///
/// `name = None` 表示零命中兜底。网络身份取引擎本轮采到的快照（`eng.snapshot`）。
fn applied_notice(
    name: Option<&str>,
    snap: &NetworkSnapshot,
    change: Option<&network::NetworkChange>,
    ok: bool,
    restored: bool,
    note: Option<&str>,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "profile": name,
        "fallback": name.is_none(),
        "ssid": snap.ssid,
        "iface": snap.primary_interface,
        "success": ok,
        "restored": restored,
        // 「跳过下发」（系统本就合规）时 change 为空：前端据此显示「未更改任何参数」，
        // 而不是一张只有系统身份的空清单。
        "nochange": change.map(|c| c.is_empty()).unwrap_or(true),
    });
    v["groups"] = serde_json::json!(match change {
        // 一分写入都没有（跳过下发 / THEN 缺省）：整份清单留空，前端据此显示
        // 「未更改任何参数」—— 一张只有系统身份的空清单只会让人误读。
        Some(c) if !c.is_empty() => applied_groups(c, snap, ok, restored, name),
        _ => Vec::new(),
    });
    // 降级附注（DNS 兜底等）或失败原因：成功但没走预期路径时也要说出来。
    // 前端按成败分流渲染：失败 = 红色 ❌ 错误行，成功 = 琥珀色附注行。
    if let Some(n) = note {
        v["note"] = serde_json::json!(n);
    }
    v
}

/// 弹窗清单的一行：标签 + 值 + ✅/❌（`ok = None` 表示该行没有标记 —— 系统身份行）。
fn applied_row(
    label: &str,
    value: String,
    ok: Option<bool>,
    detail: Option<String>,
) -> serde_json::Value {
    serde_json::json!({ "label": label, "value": value, "ok": ok, "detail": detail })
}

/// 把 [`NetworkChange`] 摊成分组清单。
///
/// 失败路径上**每一行都是 ❌**：那些写入已被回滚、等同于没生效，逐行明细写
/// 「已回滚，未生效」。**恢复失败（`restored = false`）时明细留空** —— 那一刻
/// 写入可能还留在系统里，说「已回滚」才是谎报；整体状态交给标题与底部红色错误行。
///
/// 模式值翻译成编辑器里同一个词（`editor.dhcp` / `editor.manual` / `editor.automatic` …），
/// 用户拿着弹窗对照编辑器看到的是同一批字。
fn applied_groups(
    c: &network::NetworkChange,
    snap: &NetworkSnapshot,
    ok: bool,
    restored: bool,
    name: Option<&str>,
) -> Vec<serde_json::Value> {
    // 失败行的逐行明细：恢复成功 = 「已回滚，未生效」；恢复失败 = 不写
    // （写入状态未知，不谎报）。成功行永远没有明细。
    let detail = if !ok && restored {
        Some(i18n::t("popup.rolled_back"))
    } else {
        None
    };
    let row = |label: &str, value: String, ok: Option<bool>| {
        applied_row(label, value, ok, detail.clone())
    };
    let mut groups: Vec<serde_json::Value> = Vec::new();
    // 空组不入列：没写的项不给占位行。
    let mut group = |title: &str, rows: Vec<serde_json::Value>| {
        if !rows.is_empty() {
            groups.push(serde_json::json!({ "title": title, "rows": rows }));
        }
    };

    // —— 系统：配置名 / SSID / 接口。身份行没有成败可言，不带标记。 ——
    //
    // 排**第一组**：这一组回答的是「这是哪一套配置、连的是哪个网」，先说身份再说改了
    // 什么 —— 反过来用户得从清单末尾往上找身份才能对上号。
    // System first: it answers "which profile, which network", so the reader knows what
    // they are looking at before reading what was written.
    let mut sys = Vec::new();
    let profile_disp = match name {
        Some(n) => n.to_string(),
        None => i18n::t("popup.applied_fallback"),
    };
    sys.push(applied_row("popup.profile", profile_disp, None, None));
    if let Some(s) = &snap.ssid {
        sys.push(applied_row("status.ssid", s.clone(), None, None));
    }
    if let Some(i) = &snap.primary_interface {
        sys.push(applied_row("popup.interface", i.clone(), None, None));
    }
    group(&i18n::t("popup.group_system"), sys);

    // —— IPv4：模式 / 地址 / 掩码 / 网关 / 路由增删 ——
    let mut v4 = Vec::new();
    if let Some(m) = &c.mode {
        let disp = match m.as_str() {
            "manual" => i18n::t("editor.manual"),
            _ => i18n::t("editor.dhcp"),
        };
        v4.push(row("popup.row_mode", disp, Some(ok)));
    }
    if let Some(v) = &c.ip {
        v4.push(row("popup.ip", v.clone(), Some(ok)));
    }
    if let Some(v) = &c.netmask {
        v4.push(row("popup.netmask", v.clone(), Some(ok)));
    }
    if let Some(v) = &c.gateway {
        v4.push(row("popup.gateway", v.clone(), Some(ok)));
    }
    if c.routes_added > 0 {
        v4.push(row("popup.routes_added", c.routes_added.to_string(), Some(ok)));
    }
    if c.routes_deleted > 0 {
        v4.push(row(
            "popup.routes_removed",
            c.routes_deleted.to_string(),
            Some(ok),
        ));
    }
    group(&i18n::t("popup.group_ipv4"), v4);

    // —— IPv6：模式 / 地址 / 前缀 / 网关 ——
    let mut v6 = Vec::new();
    if let Some(m) = &c.v6mode {
        let disp = match m.as_str() {
            "manual" => i18n::t("editor.manual_v6"),
            "linklocal" => i18n::t("editor.linklocal"),
            "off" => i18n::t("editor.off"),
            _ => i18n::t("editor.automatic"),
        };
        v6.push(row("popup.row_mode", disp, Some(ok)));
    }
    if let Some(v) = &c.ipv6 {
        v6.push(row("popup.v6_address", v.clone(), Some(ok)));
    }
    if let Some(v) = &c.v6prefix {
        v6.push(row("popup.prefix6", v.clone(), Some(ok)));
    }
    if let Some(v) = &c.v6gateway {
        v6.push(row("popup.gateway6", v.clone(), Some(ok)));
    }
    group(&i18n::t("popup.group_ipv6"), v6);

    // —— DNS：指定 = 那几台；清空 = 交回自动获取 ——
    if c.dns_cleared {
        group(
            &i18n::t("popup.group_dns"),
            vec![row("popup.dns", i18n::t("editor.dhcp_provided"), Some(ok))],
        );
    } else if let Some(v) = &c.dns {
        group(
            &i18n::t("popup.group_dns"),
            vec![row("popup.dns", v.clone(), Some(ok))],
        );
    }

    groups
}

/// 把独立的结果弹窗窗口叫出来（窗口操作回主线程）。
///
/// 两条路径共用：`emit_applied` 的立即显示路径，与 3B1 动作收尾后的补显示路径
/// （[`apply_runs`]）。窗口操作必须回主线程，而调用方在引擎线程上 —— 与托盘的
/// 延迟单击同一个模式（两个 handle：一个借给 `run_on_main_thread` 当 `&self`，
/// 一个 move 进闭包；同一个 `app` 不能既被借用又被 move 捕获，否则 E0505）。
fn show_applied_window(state: &Arc<AppState>) {
    // 没有载荷就不叫窗口：一扇「没内容可显示」的置顶窗是最糟的形态 ——
    // 用户既看不到结果，又得去别处找怎么关掉它（1.8.6 那扇黑窗的教训）。
    let payload = state
        .last_applied
        .lock()
        .ok()
        .and_then(|v| v.as_ref().cloned());
    let Some(payload) = payload else {
        log::warn(&i18n::t("engine.applied_no_payload"));
        return;
    };
    if let Some(app) = state.app.get().cloned() {
        let app_cl = app.clone();
        let _ = app.run_on_main_thread(move || {
            // 定向重发一次最新载荷：窗口页面可能是在上一次全局广播**之后**才加载完的
            // （冷启动首评正是这种情形），那一发它没收到。重发让「显示」这件事
            // 不再依赖页面加载时序 —— 页面若仍没就绪，它加载完会自己来取
            // （`ipc::get_last_applied`），两条路都通向同一份内容。
            let _ = tauri::Emitter::emit_to(
                &app_cl,
                crate::popup::APPLIED_LABEL,
                "netsense://applied",
                payload,
            );
            // 结果弹窗是一扇独立置顶窗口（applied）：任何时候都浮在最上、且不抢焦点，
            // 与编辑器 / 托盘面板 / 设置 / 日志互不隶属。直接叫它出来，无需判断别的窗是否开着。
            crate::popup::show_applied(&app_cl);
        });
    }
}

/// 发「配置已应用」弹窗，并按需把独立的结果弹窗窗口叫出来（窗口操作回主线程）。
///
/// 闸门「有网络才弹」：冷启动时一张网卡都没亮，兜底也会走到 commit，那样的弹窗只是开机
/// 噪音 —— 需求说的是「首次连上网络或网络连接改变后」。SSID 与主网卡任一可读即算有网
/// （纯有线没有 SSID，只有网卡名）。
///
/// `change` 是这一轮**实际写下的几项**（3A 回读记账），`ok` 标记整体成败，
/// `note` 携带降级附注或失败原因。`restored` 只在失败路径上有意义：
/// 它决定逐行明细能不能说「已回滚，未生效」—— 快照恢复本身失败时明细必须留空。
///
/// `actions_pending` = 这一轮已经（或即将）把 3B1 一次性动作提交给后台线程。需求：
/// 结果弹窗要在**整轮自动化**（网络配置 + 一次性动作）都结束后出现，一并显示动作
/// 结果 —— 这种情况下事件照发（隐藏窗口先收下并渲染），窗口显示推迟到动作收尾
/// （`apply_runs` 归因报告的那一刻，见 [`show_applied_window`]）。
// 参数与 `applied_notice` 的事件载荷一一对应：它们本来就不是一个可以随便收拢的
// 「配置项清单」，而是同一条事件的逐字段展开 —— 收进结构体只会多一层搬运，不减少
// 任何调用方的认知负担。9 > clippy 默认阈值 7，故显式豁免。
// Parameters mirror the `applied_notice` event payload field-for-field: bundling
// them into a struct would only add a layer of shuffling, so exempt explicitly.
#[allow(clippy::too_many_arguments)]
fn emit_applied(
    state: &Arc<AppState>,
    eng: &Engine,
    name: Option<&str>,
    change: Option<&network::NetworkChange>,
    ok: bool,
    restored: bool,
    note: Option<&str>,
    actions_pending: bool,
) {
    // 系统设置里「应用后弹窗」关掉了：自动与手动两条路径都从这里走，关门一次等于两条都不弹。
    // 这里只读 settings（config 锁），而本函数的调用方至多只持 engine 锁，故属于
    // engine→config 顺序，不会与任何 config→engine 站点冲突（见模块头锁顺序分析）。
    let show = state
        .settings
        .lock()
        .map(|s| s.show_applied_popup)
        .unwrap_or(true);
    if !show {
        return;
    }
    let snap = &eng.snapshot;
    if snap.ssid.is_none() && snap.primary_interface.is_none() {
        return;
    }
    // 先存一份再广播：结果弹窗窗口可能还没加载完（冷启动首评就跑在它之前），那一发
    // 全局广播会丢。缓存下来后，「显示」与「内容」不再依赖页面加载时序 —— 显示时
    // 定向重发一次，页面加载完也会自己来取（见 [`show_applied_window`] 与 `ipc::get_last_applied`）。
    let payload = applied_notice(name, snap, change, ok, restored, note);
    if let Ok(mut last) = state.last_applied.lock() {
        *last = Some(payload.clone());
    }
    crate::state::emit(state, "netsense://applied", payload);
    if actions_pending {
        // 动作还在后台跑：窗口先不显示，等 3B1 收尾报告归因后由 `apply_runs` 叫出来。
        // 事件已发，隐藏窗口此刻就把网络部分渲染好了 —— 显示时数据已就绪。
        state
            .applied_show_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    } else {
        show_applied_window(state);
    }
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
                // This settings + network combination already failed once; do not
                // pop the authorization dialog again. The skip is logged because
                // without this line the log stays silent while the user sees
                // "it matched, yet nothing happens".
                // 这个「配置 + 网络」组合已经失败过一次，别反复弹授权框。
                // 为什么跳过要说出来：没有这一行，日志里只剩沉默，
                // 而用户看到的是「明明命中了却什么都不做」。
                log::debug(&i18n::tf("engine.skip_blocked", &[("name", &profile.name)]));
                return None;
            }
            let staying = eng.active_id.as_deref() == Some(id.as_str());
            if staying && eng.applied_fp.as_deref() == Some(fp.as_str()) {
                // 方案第 36 条：保持 Active 不重复下发、不重复跑一次性动作
                log::debug(&i18n::tf("engine.skip_unchanged", &[("name", &profile.name)]));
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
        log::debug(&i18n::t("engine.skip_fb_empty"));
        eng.stop_fallback_workers();
        eng.fallback_fp = None;
        return None;
    };
    let has_actions =
        one_shot::any_enabled(&fb.one_shot) || fb.persistent.iter().any(|a| a.enabled);
    if fb.network.is_none() && !has_actions {
        // 全空的兜底 = 什么都不做（保持现状）。空配置的语义是「不干预」，
        // 不是「把之前干预过的东西撤掉」。
        log::debug(&i18n::t("engine.skip_fb_empty"));
        eng.stop_fallback_workers();
        eng.fallback_fp = None;
        return None;
    }
    let fp = format!("fallback|{}", serde_json::to_string(fb).unwrap_or_default());
    if eng.fallback_fp.as_deref() == Some(fp.as_str()) {
        log::debug(&i18n::t("engine.skip_fb_unchanged"));
        return None;
    }
    // 兜底 3A 失败记档：同一份配置 + 同一张网络指纹下不再重跑 3A，
    // 否则用户持续缺席时每轮都会被要求授权（与 Profile 的 `blocked` 同机制）。
    let fingerprint = eng.scheduler.fingerprint().to_string();
    if eng.is_fallback_blocked(&fp, &fingerprint) {
        log::debug(&i18n::t("engine.skip_fb_blocked"));
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
            net: _,
            opts,
            staying,
            from,
            fp,
            ..
        } => {
            let profile = *profile; // 拆箱：下面只用 &profile 与它的字段
            let failure = match &three_a {
                Some(Stage3A::Failed { reason, .. }) => Some(reason.clone()),
                _ => None,
            };
            match failure {
                Some(reason) => {
                    let msg =
                        i18n::tf("engine.error", &[("name", &profile.name), ("error", &reason)]);
                    log::error(&msg);
                    eng.record_failure(&profile.id, fp, reason.clone());
                    // 失败之前动过的那几项也要报出来：回滚会把它们改回去，但
                    // 「动过」这件事发生过 —— 用户需要知道自己的网络被写过。
                    // 回滚本身也是一次写入，一并报出来（`announce_rollback`）。
                    if let Some(Stage3A::Failed {
                        change,
                        rolled_back,
                        ..
                    }) = &three_a
                    {
                        announce_rollback(state, change, rolled_back.as_deref());
                    }
                    // 失败也弹「最终结果」弹窗：列出这一轮**试图写下**的几项（全部 ❌，
                    // 已被回滚、等同于没生效），让用户看见「我动了什么、为什么没成」——
                    // 与成功弹窗共用同一套分组 UI。
                    //
                    // 这里**不受 `staying` 约束**：需求要的是「任何一项应用不成功都要弹」，
                    // 而改完配置等着生效却失败（RECONFIGURE）恰恰是最需要被看见的那一类 ——
                    // 界面上写着「已应用」，现场却是旧的。它也不会刷屏：失败已记档
                    // （`record_failure`），下一轮指纹没变时 `decide_plan` 直接返回 None。
                    if let Some(Stage3A::Failed {
                        change,
                        rolled_back,
                        ..
                    }) = &three_a
                    {
                        // 3A 失败 = 3B 一条都不跑（硬屏障）：没有动作在等，弹窗立即显示。
                        emit_applied(
                            state,
                            eng,
                            Some(profile.name.as_str()),
                            Some(change),
                            false,
                            // `rolled_back = None` ⇒ 快照恢复本身失败：那一刻写入可能
                            // 还留在系统里，弹窗不能说「已回滚，未生效」。
                            rolled_back.is_some(),
                            Some(&reason),
                            false,
                        );
                    }
                    // 重下失败不弹「应用失败」**气泡**（与上面那个弹窗是两件事）：多半是
                    // 用户改完配置就等着生效，报错已经在日志、弹窗和该 Profile 的红叉上。
                    // 真正的切换才额外走气泡通知。
                    if !staying {
                        crate::state::emit_action(state, "apply", false, msg);
                    }
                }
                None => {
                    // 这一支 3A 成功：把「实际写下的几项」抽出来供日志与弹窗共用，
                    // 两处说的是同一批动作（见 `change_lines`）。
                    let (applied_change, applied_note) = match &three_a {
                        Some(Stage3A::Applied { note, change }) => {
                            (Some(change), note.as_deref())
                        }
                        _ => (None, None),
                    };
                    if let Some(change) = applied_change {
                        // The 3A result is the closing marker of every "execution
                        // action": recorded at the default level, it pairs with
                        // net.apply_start (apply begins) into one complete timeline.
                        // 3A 的结果是每一次「执行动作」的收尾标记：默认档也记，
                        // 与 net.apply_start（开始下发）配成一条完整时间线。
                        log::info(&i18n::tf(
                            "engine.pass_3a",
                            &[("name", &profile.name), ("branch", Which::Then.name())],
                        ));
                        // 降级成功（DNS 兜底）必须出声：用户现在用的不是他填的解析器，
                        // 而界面上写的是「已应用」，不说就没人知道这件事发生过。
                        if let Some(n) = applied_note {
                            log::warn(&i18n::tf(
                                "engine.applied_degraded",
                                &[("name", &profile.name), ("note", n)],
                            ));
                        }
                        // 「这一轮到底改了系统的哪几项」—— 记日志 + 弹窗。
                        // 「应用了 Profile X」说的是引擎决定做什么；用户要看见的是
                        // 「地址/DNS/路由这几项里哪几项真的被写了」。
                        announce_change(state, change, true);
                    }
                    if !staying {
                        eng.active_id = Some(profile.id.clone());
                    }
                    eng.errors.remove(&profile.id);
                    eng.mark_applied(fp);
                    // 弹窗：自动化刚把一个配置落到当前网络上，只报**最终落地的设置**
                    // （来自 `change`，不是计划态/配置原文）。走到这里只有三种来路 ——
                    // 首评、网络指纹变了、配置内容变了；轮询中什么都没变时 decide_plan
                    // 直接返回 None，到不了这里（「轮询不弹」由此成立，无需另设状态）。
                    // THEN 分支的 3B1 马上要提交（branch_3b 紧随其后）：有启用的动作时
                    // 弹窗推迟到动作收尾（需求：整轮自动化结束后再弹，一并显示动作结果）。
                    emit_applied(
                        state,
                        eng,
                        Some(profile.name.as_str()),
                        applied_change,
                        true,
                        true,
                        applied_note,
                        opts.one_shot
                            && profile
                                .then
                                .as_ref()
                                .map(|b| one_shot::any_enabled(&b.one_shot))
                                .unwrap_or(false),
                    );
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
            if let Some(Stage3A::Failed {
                reason,
                change,
                rolled_back,
                ..
            }) = &three_a
            {
                let msg = i18n::tf("engine.fallback_failed", &[("error", reason)]);
                log::error(&msg);
                // 失败之前动过的那几项要报出来（回滚会改回去，但「动过」发生过），
                // 回滚写回去的那几项同样要报 —— 它是一次真实的系统写入。
                announce_rollback(state, change, rolled_back.as_deref());
                // 兜底失败也弹「最终结果」弹窗：试图下写的几项全部 ❌，让用户看见
                // 「这一轮打算改什么、为什么没成」。与成功弹窗共用同一套分组 UI。
                // 3A 失败 = 3B 不跑：没有动作在等，弹窗立即显示。
                emit_applied(
                    state,
                    eng,
                    None,
                    Some(change),
                    false,
                    rolled_back.is_some(),
                    Some(reason),
                    false,
                );
                // 兜底也要出声：这一支没有 Profile 的红叉可挂，而「零命中 + 兜底没落地」
                // 意味着现场仍是上一个网络的配置 —— 用户看不出任何名堂，只觉得网络莫名能用。
                // Profile 那一支的「重下失败不弹」在这里不成立：兜底没有 staying 的概念，
                // 走到这一支就说明它本该改而没改成。
                crate::state::emit_action(state, "apply", false, msg);
                // 3A 失败 = 3B 一条都不跑（与分支同一道硬屏障）；记档不写，
                // 下一轮（网络或配置一变）会自动重试。
                eng.record_fallback_failure(fp, fingerprint);
                return;
            }
            eng.fallback_fp = Some(fp);
            eng.applied_fp = None;
            eng.active_id = None;
            log::info(&i18n::t("engine.fallback_applied"));
            // 兜底同样报出「改了哪几项」：兜底这一支最容易被忽略（界面上它不带
            // Profile 名字，只有一句「兜底」），而它一样会改地址与 DNS。
            let fb_change = match &three_a {
                Some(Stage3A::Applied { change, .. }) => Some(change),
                _ => None,
            };
            if let Some(change) = fb_change {
                announce_change(state, change, true);
            }
            // 兜底也是一次「成功应用的配置」：弹窗里标成兜底，用户看得出此刻没有
            // Profile 命中。与 Profile 分支同一个闸门（有网络才弹）。只报**最终落地的设置**。
            // 兜底的 3B1 马上要提交（branch_3b 紧随其后）：有启用的动作时推迟显示，
            // 与 Profile 分支同一需求 —— 整轮自动化结束（动作出结果）后再弹。
            emit_applied(
                state,
                eng,
                None,
                fb_change,
                true,
                true,
                None,
                one_shot::any_enabled(&fb.one_shot),
            );
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
        Some(Stage3A::Failed {
            reason,
            change,
            rolled_back,
            ..
        }) => {
            let msg = i18n::tf("engine.error", &[("name", &profile.name), ("error", reason)]);
            log::error(&msg);
            if matched {
                eng.record_failure(&profile.id, fp, reason.clone());
            }
            // 失败之前动过的那几项要报出来（回滚会改回去，但「动过」发生过），
            // 回滚写回去的那几项同样要报 —— 它是一次真实的系统写入。
            announce_rollback(state, change, rolled_back.as_deref());
            crate::state::emit_action(state, "apply", false, msg);
            // 手动应用也要弹「配置已应用」结果弹窗（失败态）：用户在编辑器点了应用、在等一个
            // 明确的结果，只给一个 toast 等于让他自己去猜改动有没有真的落地。与自动应用路径
            // commit_plan 同一个入口、同一个「系统设置可关」闸门。
            // 3A 失败 = 3B 不跑（branch_3b 自行跳过）：没有动作在等，弹窗立即显示。
            emit_applied(
                state,
                &eng,
                Some(profile.name.as_str()),
                Some(change),
                false,
                rolled_back.is_some(),
                Some(reason),
                false,
            );
        }
        _ => {
            if let Some(Stage3A::Applied { note, change }) = &three_a {
                log::info(&i18n::tf(
                    "engine.pass_3a",
                    &[("name", &profile.name), ("branch", which.name())],
                ));
                // 同上：降级成功要说出来，手动应用尤其 —— 用户是点了按钮在等结果的。
                if let Some(n) = note {
                    log::warn(&i18n::tf(
                        "engine.applied_degraded",
                        &[("name", &profile.name), ("note", n)],
                    ));
                }
                // 手动应用同样报出「改了哪几项」：用户点了按钮在等结果，
                // 只告诉他「应用成功」而不说清改了哪些参数，等于让他自己猜。
                announce_change(state, change, true);
                // 手动应用也要弹「配置已应用」结果弹窗：用户在编辑器点了应用、在等一个
                // 明确的结果，只给一个 toast 等于让他自己去猜改动有没有真的落地。
                // 与自动应用路径 commit_plan 同一个入口、同一个「系统设置可关」闸门。
                // 生效分支的 3B1 马上要提交（branch_3b 在函数尾部）：有启用的动作时
                // 推迟显示，与自动路径同一需求 —— 动作出结果后再弹、一并显示成败。
                emit_applied(
                    state,
                    &eng,
                    Some(profile.name.as_str()),
                    Some(change),
                    true,
                    true,
                    note.as_deref(),
                    which
                        .of(&profile)
                        .map(|b| one_shot::any_enabled(&b.one_shot))
                        .unwrap_or(false),
                );
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
            // 切 DHCP 顺带把 DNS 交回自动获取（macOS 上是 `-setdnsservers … Empty`），
            // 那是**一次真的清空**：用户填过的解析器此刻已经不在生效状态里，
            // 必须报出来，否则「我明明填了 DNS」和「系统里没有 DNS」会同时成立。
            announce_change(
                state,
                &network::NetworkChange {
                    mode: Some("dhcp".into()),
                    dns_cleared: true,
                    ..Default::default()
                },
                true,
            );
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
    use crate::config::{Condition, ConditionType, Rule};

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

    /// 一个 Profile、一条 Rule、一条**启用**的条件 —— 用来把
    /// [`Config::decision_inputs`] 缩到「这一种证据」，判定闸门的测试只需要这个形状。
    fn one_condition_cfg(kind: ConditionType, value: &str) -> Config {
        Config {
            profiles: vec![Profile {
                id: "office".to_string(),
                name: "office".to_string(),
                rules: vec![Rule {
                    id: "r1".to_string(),
                    enabled: true,
                    conditions: vec![Condition {
                        id: "c1".to_string(),
                        enabled: true,
                        kind,
                        value: value.to_string(),
                    }],
                }],
                ..Default::default()
            }],
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
        eng.note_sampled(snap(Some("A"), None), t0);
        eng.evaluate(&cfg, t0);
        assert!(eng.due(&cfg, t0).is_empty(), "刚评估过，没到下一个轮询点");

        // set_dhcp 的收尾顺序：先把这份快照记成新基线、再武装暂停 ——
        // 切 DHCP 自己引起的现场变动不算「网络变化」。
        eng.note_sampled(snap(Some("A"), None), t0);
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
        eng.note_sampled(snap(Some("B"), None), later);
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

    /// 一份「一个 Profile 都没命中」的判定，得先由快照给出**配置里那些条件所要的**
    /// 那几项证据，才能落成处置（撤销 Active、下发兜底）。
    ///
    /// 不加这道判据时会发生什么：macOS 的 SSID 只有 CoreWLAN 一个来源，网关 MAC 要靠
    /// ARP 表里恰好有默认路由那一条 —— Wi-Fi 打盹的那一轮 SSID 读不出来、旧网关 MAC 还
    /// 挂在表里，于是「唯一条件是 SSID」的 Profile 被判成不匹配，兜底当着正常工作的网络
    /// 把静态 IP 改成自动获取、把配置好的 DNS 清空成系统自动，而界面上看不出是谁改的。
    ///
    /// 「任一项身份读到过」撑不起这个闸门（上面那一份快照恰恰读到过一项），
    /// 「永远拦着」也不行：真遇上给不出证据的网络（拔网线、纯有线且 ARP 空表）时，
    /// 拦住它等于兜底永久失效。所以连续判不动到 `UNIDENTIFIED_CONFIRMS` 轮就放行。
    #[test]
    fn a_zero_match_needs_the_evidence_its_own_conditions_ask_for() {
        let mut eng = Engine::new();
        let t0 = Instant::now();
        // 只有一条 SSID 型条件的配置：判不判得动，只看 SSID 读到没有。
        eng.evaluate(&one_condition_cfg(ConditionType::WifiSsid, "Office"), t0);
        let flap = NetworkSnapshot {
            ssid: None,
            // 打盹轮里网关 MAC 往往还是「上一次连通」时那一条：它有值，但它不是 SSID
            // 条件所要的证据，拿它放行就是拿旧现场判新网络。
            gateway_mac: Some("aa:bb:cc:dd:ee:ff".to_string()),
            ..Default::default()
        };

        eng.note_sampled(flap.clone(), t0);
        assert_eq!(eng.undecidable_rounds, 1);
        assert!(
            !eng.no_match_is_actionable(),
            "单轮判不动是读取抖动的形状，不是「换了个陌生网络」的形状"
        );

        eng.note_sampled(
            NetworkSnapshot {
                ssid: Some("Office".to_string()),
                ..flap.clone()
            },
            t0 + std::time::Duration::from_secs(2),
        );
        assert_eq!(eng.undecidable_rounds, 0, "读到条件要的证据就要立刻清零计数");
        assert!(eng.no_match_is_actionable());

        // 连续判不动到第 `UNIDENTIFIED_CONFIRMS` 轮：认了，兜底必须还能起作用。
        for i in 0..UNIDENTIFIED_CONFIRMS {
            eng.note_sampled(
                flap.clone(),
                t0 + std::time::Duration::from_secs(4 + i as u64 * 2),
            );
        }
        assert!(
            eng.no_match_is_actionable(),
            "稳定给不出 SSID 是既成事实，拦住它等于让兜底永久失效"
        );

        // 反过来：条件只要网关 MAC 时，同一份「没有 SSID」的快照本来就能判 ——
        // 纯有线机器上 SSID 恒为空，闸门若比配置所需更严，现网就永远拆不掉了。
        let mut wired = Engine::new();
        wired.evaluate(
            &one_condition_cfg(ConditionType::GatewayMac, "aa:bb:cc:dd:ee:ff"),
            t0,
        );
        wired.note_sampled(flap, t0);
        assert_eq!(wired.undecidable_rounds, 0, "网关 MAC 型判定不缺证据");
        assert!(wired.no_match_is_actionable());
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

    fn snap(ssid: Option<&str>, iface: Option<&str>) -> NetworkSnapshot {
        NetworkSnapshot {
            ssid: ssid.map(|s| s.to_string()),
            primary_interface: iface.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    /// 组与行都按**位置**取（组按 IPv4 → IPv6 → DNS → 系统 的顺序生成），
    /// 断言只落在原始值与 ok 标志上 —— i18n 在单跑与全量跑之间状态不同
    /// （全量套件里有用例会初始化 DICTS），标题/标签可能是 key 也可能是
    /// 本地化文本，按文案匹配必然时灵时不灵。
    fn grp(v: &serde_json::Value, n: usize) -> &serde_json::Value {
        &v["groups"].as_array().unwrap()[n]
    }
    /// 取一组的第 n 行。
    fn row(g: &serde_json::Value, n: usize) -> &serde_json::Value {
        &g["rows"].as_array().unwrap()[n]
    }
    /// 每一行都要有非空的标签与值（无论 i18n 初始化与否）。
    fn assert_rows_shaped(g: &serde_json::Value, ok: Option<bool>) {
        for r in g["rows"].as_array().unwrap() {
            assert!(!r["label"].as_str().unwrap().is_empty());
            assert!(!r["value"].as_str().unwrap().is_empty());
            match ok {
                Some(want) => assert_eq!(r["ok"], want),
                None => assert!(r["ok"].is_null()),
            }
        }
    }

    /// 弹窗载荷：分组清单列出这一轮**实际写下的几项**（来自 `NetworkChange`），
    /// 成功路径每行 ✅；系统 / IPv4 / IPv6 / DNS 四组，没写的组不出现。
    #[test]
    fn applied_notice_lists_the_actual_changes_and_the_network() {
        let change = crate::network::NetworkChange {
            mode: Some("manual".into()),
            ip: Some("192.168.1.5".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1, 8.8.8.8".into()),
            v6mode: Some("manual".into()),
            ipv6: Some("2001:db8::1".into()),
            v6prefix: Some("64".into()),
            v6gateway: Some("fe80::1".into()),
            routes_added: 2,
            ..Default::default()
        };
        let v = applied_notice(
            Some("home"),
            &snap(Some("Home"), Some("wi-fi")),
            Some(&change),
            true,
            true,
            None,
        );
        assert_eq!(v["profile"], "home");
        assert_eq!(v["fallback"], false);
        assert_eq!(v["success"], true);
        assert_eq!(v["ssid"], "Home");
        assert_eq!(v["iface"], "wi-fi");
        assert_eq!(v["nochange"], false);
        // 四组都在，顺序固定：**系统**在最上面（先说身份），其后 IPv4 / IPv6 / DNS。
        assert_eq!(v["groups"].as_array().unwrap().len(), 4);
        // 系统组（第 0 组）：配置名 / SSID / 接口，身份行不带标记（ok = null）。
        let gs = grp(&v, 0);
        assert_eq!(gs["rows"].as_array().unwrap().len(), 3);
        assert_eq!(row(gs, 0)["value"], "home");
        assert_eq!(row(gs, 1)["value"], "Home");
        assert_eq!(row(gs, 2)["value"], "wi-fi");
        assert_rows_shaped(gs, None);
        // IPv4 组：模式 / 地址 / 掩码 / 网关 / 新增路由 = 5 行，原始值逐行核对。
        let g4 = grp(&v, 1);
        assert_eq!(g4["rows"].as_array().unwrap().len(), 5);
        assert_eq!(row(g4, 1)["value"], "192.168.1.5");
        assert_eq!(row(g4, 2)["value"], "255.255.255.0");
        assert_eq!(row(g4, 3)["value"], "192.168.1.1");
        assert_eq!(row(g4, 4)["value"], "2");
        // IPv6 组：manual 的地址三件套随模式一起进清单。
        let g6 = grp(&v, 2);
        assert_eq!(g6["rows"].as_array().unwrap().len(), 4);
        assert_eq!(row(g6, 1)["value"], "2001:db8::1");
        assert_eq!(row(g6, 2)["value"], "64");
        assert_eq!(row(g6, 3)["value"], "fe80::1");
        // DNS 组：写成了那几台。
        assert_eq!(row(grp(&v, 3), 0)["value"], "192.168.1.1, 8.8.8.8");
        // 三个网络组每行 ✅ 且无明细；系统组（无标记）已在上面核过。
        for n in 1..4 {
            assert_rows_shaped(grp(&v, n), Some(true));
        }
    }

    /// DHCP 只产生「寻址模式」一行，没有地址/DNS 行；DNS 组整组不出现。
    #[test]
    fn applied_notice_for_dhcp_lists_only_the_mode_action() {
        let change = crate::network::NetworkChange {
            mode: Some("dhcp".into()),
            ..Default::default()
        };
        let v = applied_notice(
            Some("office"),
            &snap(None, Some("eth0")),
            Some(&change),
            true,
            true,
            None,
        );
        assert_eq!(v["success"], true);
        // 两组：系统在前（SSID 读不到 ⇒ 配置名 + 接口两行）+ IPv4（1 行）。
        assert_eq!(v["groups"].as_array().unwrap().len(), 2);
        let gs = grp(&v, 0);
        assert_eq!(gs["rows"].as_array().unwrap().len(), 2, "配置名 + 接口");
        assert_rows_shaped(gs, None);
        let g4 = grp(&v, 1);
        assert_eq!(g4["rows"].as_array().unwrap().len(), 1, "只有寻址模式一行");
        assert_rows_shaped(g4, Some(true));
    }

    /// DNS 两态：指定服务器与清空（交回自动）各占一行，二者互斥。
    #[test]
    fn applied_notice_shows_dns_set_or_cleared_as_one_action() {
        // 指定 DNS
        let set = crate::network::NetworkChange {
            dns: Some("192.168.1.1, 8.8.8.8".into()),
            ..Default::default()
        };
        let v = applied_notice(
            Some("cafe"),
            &snap(Some("Cafe"), None),
            Some(&set),
            true,
            true,
            None,
        );
        // 两组：系统在前 + DNS（1 行，值 = 那几台）。
        assert_eq!(v["groups"].as_array().unwrap().len(), 2);
        assert_eq!(row(grp(&v, 1), 0)["value"], "192.168.1.1, 8.8.8.8");

        // 清空 DNS（交回自动获取）：一行，值非空（本地化的「DHCP 提供」）。
        let cleared = crate::network::NetworkChange {
            dns_cleared: true,
            ..Default::default()
        };
        let v2 = applied_notice(
            Some("cafe"),
            &snap(Some("Cafe"), None),
            Some(&cleared),
            true,
            true,
            None,
        );
        assert_eq!(v2["groups"].as_array().unwrap().len(), 2);
        assert!(!row(grp(&v2, 1), 0)["value"].as_str().unwrap().is_empty());
    }

    /// 不带 change 的分支（THEN 缺省 / 系统本就合规跳过下发）：没有网络组，
    /// nochange = true，前端据此显示「未更改任何参数」。
    #[test]
    fn applied_notice_without_a_change_has_no_actions() {
        let v = applied_notice(
            Some("label-only"),
            &snap(Some("Home"), None),
            None,
            true,
            true,
            None,
        );
        assert_eq!(v["profile"], "label-only");
        assert_eq!(v["fallback"], false);
        assert_eq!(v["success"], true);
        assert_eq!(v["nochange"], true);
        assert!(v["groups"].as_array().unwrap().is_empty(), "无 change 则无网络组");

        // change 有值但全空（Default）同样算 nochange：一分写入都没有。
        let empty = crate::network::NetworkChange::default();
        let v2 = applied_notice(
            Some("label-only"),
            &snap(Some("Home"), None),
            Some(&empty),
            true,
            true,
            None,
        );
        assert_eq!(v2["nochange"], true);
        assert!(v2["groups"].as_array().unwrap().is_empty());
    }

    /// 兜底：profile 字段为空、身份行显示「兜底」文案；失败路径每行 ❌ 并带
    /// 「已回滚」明细 —— 但恢复本身失败时明细必须留空（写入状态未知，不谎报）。
    #[test]
    fn applied_notice_marks_the_fallback_and_failure() {
        let change = crate::network::NetworkChange {
            mode: Some("dhcp".into()),
            ..Default::default()
        };
        // 成功兜底：profile 字段为空、身份行非空。
        let v = applied_notice(
            None,
            &snap(Some("Starbucks"), None),
            Some(&change),
            true,
            true,
            None,
        );
        assert_eq!(v["fallback"], true);
        assert!(v["profile"].is_null());
        assert_eq!(v["success"], true);
        assert_eq!(v["ssid"], "Starbucks");
        assert!(!row(grp(&v, 1), 0)["value"].as_str().unwrap().is_empty());

        // 失败兜底（恢复成功）：同一批行全部 ❌，明细非空（「已回滚，未生效」）。
        let v2 = applied_notice(
            None,
            &snap(Some("Starbucks"), None),
            Some(&change),
            false,
            true,
            Some("boom"),
        );
        assert_eq!(v2["success"], false);
        assert_eq!(v2["restored"], true);
        for g in v2["groups"].as_array().unwrap() {
            for r in g["rows"].as_array().unwrap() {
                if !r["ok"].is_null() {
                    assert_eq!(r["ok"], false, "失败路径每行 ❌");
                    assert!(!r["detail"].as_str().unwrap_or("").is_empty());
                }
            }
        }
        assert_eq!(v2["note"], "boom");

        // 失败且恢复也失败：明细留空 —— 那时写入可能还留在系统里。
        let v3 = applied_notice(
            None,
            &snap(Some("Starbucks"), None),
            Some(&change),
            false,
            false,
            Some("boom"),
        );
        assert_eq!(v3["restored"], false);
        for g in v3["groups"].as_array().unwrap() {
            for r in g["rows"].as_array().unwrap() {
                assert!(r["detail"].is_null(), "恢复失败时不谎报「已回滚」");
            }
        }
    }

    /// The identity summary in the fingerprint-change log line: a missing field is a
    /// dash, not `None` and not an empty string — when troubleshooting, "this field
    /// was not read" must be distinguishable at a glance from "this field is
    /// genuinely empty". The order is always SSID / gateway MAC / primary interface,
    /// the identity part rendered by the `net.changed` line.
    ///
    /// 指纹变化日志的身份摘要：缺项是一个短横，不是 `None` 也不是空串 ——
    /// 排查时要能一眼看出「这一项没读到」与「这一项就是空的」的区别。
    /// 顺序恒为 SSID / 网关 MAC / 主网卡，即 `net.changed` 行渲染的身份段。
    #[test]
    fn ident_of_renders_the_three_identity_fields_with_a_dash_for_missing() {
        assert_eq!(
            ident_of(&snap(Some("Home"), Some("en0"))),
            "Home / - / en0",
            "没读到网关 MAC（纯有线常见）时是短横"
        );
        assert_eq!(
            ident_of(&NetworkSnapshot::default()),
            "- / - / -",
            "一张网卡都没亮时三个都是短横，而不是 None"
        );
        assert_eq!(
            ident_of(&snap(None, Some("en5"))),
            "- / - / en5",
            "SSID 读不到（Wi-Fi 打盹）但网卡在：摘要把这件事说清楚"
        );
        assert_eq!(or_dash(None), "-");
        assert_eq!(or_dash(Some("x")), "x");
    }
}

/// 集成测试（评审 #1 头号建议）：用 MockPlatform 把「快照 → 判定 → 下发 → 3B」整条 Engine
/// 链路自动跑起来。这里放的是「不需要 engine-mock feature」的两项——决策不变量与 3A 回滚，
/// 它们在普通 `cargo test` 下也会跑。
/// Integration tests (review #1): drive the whole Engine chain (snapshot → decide → apply → 3B)
/// on MockPlatform. This module holds the two feature-independent cases (decision invariants and
/// the 3A rollback); they run under plain `cargo test`.
#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::config::{Mode, NetworkConfig};
    use crate::config::model::RouteConfig;
    use crate::network::{apply_3a, Stage3A};
    use crate::platform::MockPlatform;
    use crate::platform::{Call, InterfaceStatus};

    /// 评审 §50：任意匹配规模下的不变量 —— 0→NoActiveProfile, 1→Active, 2+→Conflict（永不挑边）。
    #[test]
    fn decision_invariant_holds_for_any_match_count() {
        let ids = ["a", "b", "c", "d", "e", "f", "g", "h"];
        for n in 0..=ids.len() {
            let matched: Vec<String> = ids[..n].iter().map(|s| s.to_string()).collect();
            match decide(&matched) {
                Decision::NoActiveProfile => assert_eq!(n, 0, "只有 0 命中才该是无 Active"),
                Decision::Active { id } => {
                    assert_eq!(n, 1, "只有恰好 1 命中才该是 Active");
                    assert_eq!(id, "a", "唯一的命中者必须是它");
                }
                Decision::Conflict { ids } => {
                    assert!(n >= 2, "2+ 命中才该是 Conflict");
                    assert_eq!(ids.len(), n, "冲突名单要完整报出每个候选");
                }
            }
        }
    }

    /// 评审 P0 #2/#3 + #1：3A 下发失败时，回滚必须恢复旧静态配置、且保留系统原有的第三方路由
    /// （不能只 `delete <dest>` 误删）。直接针对 `apply_3a` 跑在 MockPlatform 上验证。
    #[test]
    fn apply_3a_rolls_back_and_keeps_pre_existing_routes_on_failure() {
        let mock = MockPlatform::new();
        mock.set_state(InterfaceStatus {
            ipv4: Some("192.168.1.10".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            ..Default::default()
        });
        // 下发前系统已有的第三方路由：回滚必须保留它。
        mock.set_routes(vec![("10.20.20.0/24".to_string(), "192.168.1.254".to_string(), 10)]);
        // 让 apply_network 失败 → 触发回滚
        mock.set_apply_network_result(Err("simulated failure".into()));
        let net = NetworkConfig {
            mode: Mode::Manual,
            ip: Some("10.0.0.5".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("10.0.0.1".into()),
            dns: Some("1.1.1.1".into()),
            routes: vec![RouteConfig {
                dest: "172.16.0.0/12".into(),
                gateway: Some("10.0.0.9".into()),
                metric: 20,
                delete: false,
            }],
            ..Default::default()
        };
        let res = apply_3a(&mock, &net, &[]);
        assert!(matches!(res, Stage3A::Failed { .. }), "下发失败必须阻断 3B");
        assert!(
            mock.calls()
                .iter()
                .any(|c| matches!(c, Call::RestoreFromSnapshot { .. })),
            "失败必须调用 restore_from_snapshot 回滚"
        );
        let routes = mock.routes();
        assert!(
            routes
                .iter()
                .any(|(d, g, _)| d == "10.20.20.0/24" && g == "192.168.1.254"),
            "回滚必须保留下发前系统已有的路由，不能误删第三方路由（评审 #28/#29）"
        );
    }
}

/// 完整 Engine 集成测试：把 MockPlatform 顶替真实 PAL（`engine-mock` feature），用 `pass()`
/// 驱动真实引擎循环（采样 → 评估 → 决策 → 3A 下发 → 3B），断言 mock 记录了对应的平台调用。
/// Full Engine integration: MockPlatform stands in for the real PAL (feature `engine-mock`), and
/// `pass()` drives the real engine loop; we assert the mock recorded the expected platform calls.
#[cfg(all(test, feature = "engine-mock"))]
mod mock_engine {
    use super::*;
    use crate::appconfig::AppConfig;
    use crate::config::{
        Branch, Condition, ConditionType, Config, FallbackConfig, Mode, NetworkConfig, Profile,
        Rule, SCHEMA,
    };
    use crate::platform::{Call, InterfaceStatus};
    use crate::state::AppState;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// 「WifiSsid == <ssid> 命中」的 Profile：THEN 含静态 IP（无 3B 动作，避免测试残留线程）。
    fn ssid_profile(id: &str, ssid: &str) -> Profile {
        Profile {
            id: id.to_string(),
            name: id.to_string(),
            enabled: true,
            rules: vec![Rule {
                id: format!("{id}-r1"),
                enabled: true,
                conditions: vec![Condition {
                    id: format!("{id}-c1"),
                    enabled: true,
                    kind: ConditionType::WifiSsid,
                    value: ssid.to_string(),
                }],
            }],
            then: Some(Branch {
                network: Some(NetworkConfig {
                    mode: Mode::Manual,
                    ip: Some("10.0.0.5".to_string()),
                    netmask: Some("255.255.255.0".to_string()),
                    gateway: Some("10.0.0.1".to_string()),
                    dns: Some("1.1.1.1".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// 顶着 mock PAL 建 AppState 并塞入配置（`engine-mock` 下 `plat` 即 MockPlatform）。
    fn state_with(cfg: Config) -> Arc<AppState> {
        let mut s = AppState::no_config_yet(
            PathBuf::from("netsense_test_config.json"),
            PathBuf::from("netsense_test_settings.json"),
            AppConfig::default(),
        );
        s.config = Mutex::new(cfg);
        Arc::new(s)
    }

    /// 让 mock 当前网络状态报告某个 SSID（`pass` 采样时读到它）。
    fn set_mock_ssid(state: &AppState, ssid: &str) {
        state.plat.set_state(InterfaceStatus {
            ssid: Some(ssid.to_string()),
            ..Default::default()
        });
    }

    #[test]
    fn engine_loop_applies_matching_profile_through_mock() {
        let cfg = Config {
            schema: SCHEMA,
            profiles: vec![ssid_profile("home", "TestNet")],
            ..Default::default()
        };
        let state = state_with(cfg);
        set_mock_ssid(&state, "TestNet");
        pass(&state, None);
        assert!(
            state
                .plat
                .calls()
                .iter()
                .any(|c| matches!(c, Call::ApplyNetwork { .. })),
            "唯一命中的 Profile 应经 3A 下发到 mock 平台（评审 #1：完整 Engine 集成链）"
        );
    }

    #[test]
    fn engine_loop_never_applies_on_conflict() {
        let cfg = Config {
            schema: SCHEMA,
            profiles: vec![ssid_profile("home", "TestNet"), ssid_profile("office", "TestNet")],
            ..Default::default()
        };
        let state = state_with(cfg);
        set_mock_ssid(&state, "TestNet");
        pass(&state, None);
        assert!(
            !state
                .plat
                .calls()
                .iter()
                .any(|c| matches!(c, Call::ApplyNetwork { .. })),
            "多命中（Conflict）绝不能下发任何 Profile"
        );
        let eng = state.engine.lock().unwrap();
        assert!(
            matches!(eng.decision, Decision::Conflict { .. }),
            "判定必须是 Conflict"
        );
    }

    #[test]
    fn engine_loop_applies_fallback_when_nothing_matches() {
        let cfg = Config {
            schema: SCHEMA,
            profiles: vec![ssid_profile("home", "TestNet")],
            fallback: Some(FallbackConfig {
                enabled: true,
                network: Some(NetworkConfig {
                    mode: Mode::Manual,
                    ip: Some("192.168.0.5".to_string()),
                    netmask: Some("255.255.255.0".to_string()),
                    gateway: Some("192.168.0.1".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let state = state_with(cfg);
        set_mock_ssid(&state, "SomeUnknownNet");
        pass(&state, None);
        assert!(
            state
                .plat
                .calls()
                .iter()
                .any(|c| matches!(c, Call::ApplyNetwork { .. })),
            "零命中应走兜底并把兜底网络下发到 mock"
        );
    }

    /// 回归（问题 3A「兜底命中不显示」）：零命中走兜底后，`EngineView.fallback_active`
    /// 必须为 true。前端 panel 顶部「当前生效配置」与编辑器 profile 行的激活徽标都读这
    /// 一个字段；它若停在 false，兜底明明生效、界面却像没有任何配置在跑。三端共用同一份
    /// `view()`，所以这一条同时锁定 Mac / Windows / Linux 的兜底显示前提。
    #[test]
    fn fallback_applied_reports_fallback_active_in_view() {
        let cfg = Config {
            schema: SCHEMA,
            profiles: vec![ssid_profile("home", "TestNet")],
            fallback: Some(FallbackConfig {
                enabled: true,
                network: Some(NetworkConfig {
                    mode: Mode::Manual,
                    ip: Some("192.168.0.5".to_string()),
                    netmask: Some("255.255.255.0".to_string()),
                    gateway: Some("192.168.0.1".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let state = state_with(cfg);
        set_mock_ssid(&state, "SomeUnknownNet");
        pass(&state, None);

        // `view()` 的入参是配置快照；引擎与配置各一把锁，pass() 已返回、两把锁都未持有。
        let cfg = state.config.lock().unwrap().clone();
        let eng = state.engine.lock().unwrap();
        let view = eng.view(&cfg);
        assert!(
            view.fallback_active,
            "零命中兜底生效后 fallback_active 必须为 true，否则前端顶部/名称区显示不出兜底（问题 3A）"
        );
    }

    /// 播报文案必须随成败**分流**：改动被回滚了，就不能说成「已变更」。
    ///
    /// 这条契约是 v1.8.5 修掉的真缺陷：`announce_change` 原本成功与失败用同一条
    /// `net.change_notice`（「网络配置已变更」），而失败路径上那几项已经被回滚改回去了。
    /// 用户据此以为自己的静态 IP 还在生效 —— 现场恰恰不是那样。
    ///
    /// 断言只依赖「两句必须不同」与「明细必须照样列出来」，不绑具体语言：
    /// `i18n` 的当前语言是进程级全局状态，并行用例可能把它翻掉，
    /// 拿英文原文做断言就是间歇性失败。
    #[test]
    fn a_rolled_back_change_is_never_reported_as_a_successful_one() {
        crate::i18n::init();
        let change = network::NetworkChange {
            mode: Some("manual".into()),
            ip: Some("10.0.0.2".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("10.0.0.1".into()),
            dns_cleared: true,
            ..Default::default()
        };

        let (ok_log, ok_notice) =
            change_lines(&change, true).expect("有变更明细就必须有话说");
        let (bad_log, bad_notice) =
            change_lines(&change, false).expect("有变更明细就必须有话说");

        assert_ne!(
            ok_log, bad_log,
            "成功与失败的日志不能是同一句话：失败路径上的改动已被回滚改回去了"
        );
        assert_ne!(
            ok_notice, bad_notice,
            "成功与失败的弹窗不能是同一句话：说成「已变更」会让用户以为改动还生效着"
        );

        // 明细照旧要列出来 —— 失败也不隐瞒「动过什么」，只是不说成「改成了」。
        for line in [&ok_log, &ok_notice, &bad_log, &bad_notice] {
            assert!(
                line.contains("10.0.0.2"),
                "变更明细必须出现在话里（动过的 IP 看不见，用户无从判断）：{line}"
            );
        }
    }

    /// 什么都没改 ⇒ 不发任何提示。「网络被改了」这句话必须是假的反面。
    ///
    /// 空明细（系统本就合规、跳过了下发）若也弹一条「网络配置已变更」，
    /// 用户就得学会忽略这类提示 —— 假警报一多，真警报就没人看了。
    #[test]
    fn an_empty_change_produces_no_notice_at_all() {
        crate::i18n::init();
        let empty = network::NetworkChange::default();
        assert!(change_lines(&empty, true).is_none(), "空明细不该产生成功提示");
        assert!(
            change_lines(&empty, false).is_none(),
            "空明细不该产生失败提示"
        );
    }
}
