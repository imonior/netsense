//! 检测层：决定「现在要不要重新评估」，以及每个 Profile 各自按什么节律被评估。
//!
//! 两件事分开，是因为它们的语义完全不同（方案第 4.4 条）：
//!
//! - **变化事件**：网络指纹变了 → 再等 `change_delay_secs` 秒 → 评估。等待是为了躲开
//!   漫游/重连过程中「SSID 已变但网关还没到位」的中间态。
//! - **轮询**：每隔 `poll_interval_secs` 秒无条件重新评估一次，与有没有变化无关。
//!
//! ⚠️ 「网络事件」目前由**指纹差分**实现，不是系统原生事件（macOS 的
//! `SCNetworkReachability`、Windows 的 `NotifyIpInterfaceChange`、Linux 的 rtnetlink
//! 各自一套，且都要额外依赖）。指纹包含 SSID / 网关 MAC / BSSID / 主网卡 / 在用网卡
//! 集合 / 隧道集合 —— 只比 SSID 会漏掉的三类变化这里都能感知。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::conditions::NetworkSnapshot;
use crate::config::{DetectionMode, Profile};

/// 采样节律。平台层的取值本身带 TTL 缓存，这里只控制「多久愿意再问一次」。
///
/// 2s 是折中：短到不会把漫游/重连的中间态拖成一次明显的延迟，
/// 长到不至于把取值的子进程打爆。
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// 引擎主循环节拍：没有事件时每 1s 醒一次，检查有没有到期的轮询/延迟。
pub const TICK: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct Schedule {
    /// 上次为该 Profile 评估时网络所处的指纹（判断「这次变化是否已经处理过」）
    evaluated_fingerprint: String,
    /// 下一次轮询到期的时刻
    next_poll: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct Scheduler {
    fingerprint: String,
    /// 最近一次指纹变化的时刻；None = 启动以来没变过
    changed_at: Option<Instant>,
    per_profile: HashMap<String, Schedule>,
}

impl Scheduler {
    /// 记下一份新快照。指纹变化则重置「变化时刻」。
    ///
    /// 返回值表示**这次是否观察到变化**（调用方据此决定要不要立刻再采一次，
    /// 例如 SSID 事件到达时不该等到下一个采样周期）。
    pub fn observe(&mut self, snap: &NetworkSnapshot, now: Instant) -> bool {
        let fp = snap.fingerprint();
        if fp == self.fingerprint && !self.fingerprint.is_empty() {
            return false;
        }
        let changed = !self.fingerprint.is_empty();
        self.fingerprint = fp;
        if changed {
            self.changed_at = Some(now);
        }
        changed
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// 该 Profile 当前是否应当被重新评估。
    ///
    /// `first` = 启动后的第一次评估，必须无条件放行：否则冷启动要白等一个
    /// change_delay 才应用当前网络。
    pub fn is_due(&self, p: &Profile, now: Instant, first: bool) -> bool {
        if first {
            return true;
        }
        let Some(sch) = self.per_profile.get(&p.id) else {
            // 没见过这个 id：新配置的 Profile，或刚被改名/新增 —— 立刻评估一次
            return true;
        };
        let d = &p.detection;
        let wants_events = matches!(
            d.mode,
            DetectionMode::NetworkEvents | DetectionMode::NetworkEventsAndPolling
        );
        let wants_polling = matches!(
            d.mode,
            DetectionMode::PollingOnly | DetectionMode::NetworkEventsAndPolling
        );

        if wants_events {
            let pending_change = sch.evaluated_fingerprint != self.fingerprint;
            let settled = match self.changed_at {
                // 还没观察到过变化（刚启动）：不因为「延迟已到」而误判为需要评估
                None => false,
                Some(at) => now.duration_since(at) >= Duration::from_secs(d.change_delay_secs),
            };
            if pending_change && settled {
                return true;
            }
        }
        if wants_polling {
            return now >= sch.next_poll;
        }
        false
    }

    /// 本轮到期的 Profile id 列表。
    pub fn due(&self, profiles: &[Profile], now: Instant, first: bool) -> Vec<String> {
        profiles
            .iter()
            .filter(|p| self.is_due(p, now, first))
            .map(|p| p.id.clone())
            .collect()
    }

    /// 评估完成后推进各 Profile 的节律状态。
    ///
    /// 只推进 `evaluated_ids`（本轮真正到期的那些）：把还没到 change_delay 的 Profile
    /// 也标成「已处理这个指纹」会让它的事件永远丢失。
    pub fn after_evaluation(&mut self, profiles: &[Profile], evaluated_ids: &[String], now: Instant) {
        for p in profiles {
            let entry = self.per_profile.entry(p.id.clone()).or_insert_with(|| Schedule {
                evaluated_fingerprint: String::new(),
                next_poll: now,
            });
            if evaluated_ids.contains(&p.id) {
                entry.evaluated_fingerprint = self.fingerprint.clone();
                let d = &p.detection;
                if matches!(
                    d.mode,
                    DetectionMode::PollingOnly | DetectionMode::NetworkEventsAndPolling
                ) {
                    entry.next_poll =
                        now + Duration::from_secs(d.poll_interval_secs.max(SAMPLE_INTERVAL.as_secs()));
                }
            }
        }
        // 配置里已删除的 Profile 别把状态留在内存里
        let alive: Vec<&str> = profiles.iter().map(|p| p.id.as_str()).collect();
        self.per_profile.retain(|id, _| alive.contains(&id.as_str()));
    }

    /// 配置变更后丢掉所有节律状态：新配置里的 delay/interval 应当立即生效，
    /// 而不是等到上一份配置的计时器自然到期。
    pub fn reset(&mut self) {
        self.per_profile.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::DetectionConfig;
    use crate::config::Profile;

    fn profile(id: &str, mode: DetectionMode, delay: u64, interval: u64) -> Profile {
        Profile {
            id: id.to_string(),
            name: id.to_string(),
            enabled: true,
            quick: true,
            detection: DetectionConfig {
                mode,
                change_delay_secs: delay,
                poll_interval_secs: interval,
            },
            rules: Vec::new(),
            then: None,
            else_branch: None,
        }
    }

    fn fp(tag: &str) -> NetworkSnapshot {
        NetworkSnapshot {
            ssid: Some(tag.to_string()),
            ..Default::default()
        }
    }

    /// 手工推进时间：`Instant` 没法伪造，于是用「相对某个起点的偏移」构造。
    struct Clock(Instant);
    impl Clock {
        fn new() -> Self {
            Clock(Instant::now())
        }
        fn at(&self, secs: u64) -> Instant {
            self.0 + Duration::from_secs(secs)
        }
    }

    #[test]
    fn first_pass_is_always_due() {
        let c = Clock::new();
        let mut s = Scheduler::default();
        s.observe(&fp("A"), c.at(0));
        let p = profile("x", DetectionMode::NetworkEvents, 5, 30);
        assert!(s.is_due(&p, c.at(0), true), "启动首次必须立即评估");
    }

    #[test]
    fn unknown_profile_is_due_immediately() {
        let c = Clock::new();
        let mut s = Scheduler::default();
        s.observe(&fp("A"), c.at(0));
        let p = profile("newly-added", DetectionMode::PollingOnly, 5, 60);
        assert!(s.is_due(&p, c.at(0), false), "配置里新增的 Profile 不该等一个轮询周期");
    }

    #[test]
    fn events_only_waits_for_the_change_delay_and_not_for_polling() {
        let c = Clock::new();
        let mut s = Scheduler::default();
        let p = profile("h", DetectionMode::NetworkEvents, 5, 30);
        // 建立基线
        s.observe(&fp("A"), c.at(0));
        s.after_evaluation(std::slice::from_ref(&p), &["h".to_string()], c.at(0));
        // 网络变了
        s.observe(&fp("B"), c.at(10));
        assert!(!s.is_due(&p, c.at(12), false), "延迟未到不该评估");
        assert!(s.is_due(&p, c.at(15), false), "延迟到了就该评估");
        // 评估过之后，同一指纹不再重复到期（纯 events 模式没有轮询）
        s.after_evaluation(std::slice::from_ref(&p), &["h".to_string()], c.at(15));
        assert!(!s.is_due(&p, c.at(600), false), "events-only 不该退化成轮询");
    }

    #[test]
    fn polling_only_ignores_events_but_keeps_its_cadence() {
        let c = Clock::new();
        let mut s = Scheduler::default();
        let p = profile("o", DetectionMode::PollingOnly, 5, 30);
        s.observe(&fp("A"), c.at(0));
        s.after_evaluation(std::slice::from_ref(&p), &["o".to_string()], c.at(0));
        // 变化发生了，但 polling_only 不看事件
        s.observe(&fp("B"), c.at(10));
        assert!(!s.is_due(&p, c.at(29), false), "没到 30s 就不该评估");
        assert!(s.is_due(&p, c.at(30), false), "到点就必须评估，哪怕网络没变");
        s.after_evaluation(std::slice::from_ref(&p), &["o".to_string()], c.at(30));
        assert!(!s.is_due(&p, c.at(59), false));
        assert!(s.is_due(&p, c.at(60), false));
    }

    #[test]
    fn events_and_polling_fires_on_whichever_comes_first() {
        let c = Clock::new();
        let mut s = Scheduler::default();
        let p = profile("b", DetectionMode::NetworkEventsAndPolling, 5, 30);
        s.observe(&fp("A"), c.at(0));
        s.after_evaluation(std::slice::from_ref(&p), &["b".to_string()], c.at(0));
        s.observe(&fp("B"), c.at(4));
        assert!(s.is_due(&p, c.at(9), false), "变化 + 延迟先到，不等轮询");
    }

    #[test]
    fn one_profiles_delay_does_not_leak_into_anothers() {
        // 方案第 4 条：Detection 属于每个 Profile。这里验证两份节律互不干扰
        let c = Clock::new();
        let mut s = Scheduler::default();
        let quick = profile("quick", DetectionMode::NetworkEvents, 1, 30);
        let slow = profile("slow", DetectionMode::NetworkEvents, 20, 30);
        s.observe(&fp("A"), c.at(0));
        s.after_evaluation(&[quick.clone(), slow.clone()], &["quick".to_string(), "slow".to_string()], c.at(0));
        s.observe(&fp("B"), c.at(10));
        let due = s.due(&[quick, slow], c.at(11), false);
        assert_eq!(due, vec!["quick".to_string()], "只有延迟到点的该被评估");
    }

    #[test]
    fn reset_drops_stale_timers() {
        let c = Clock::new();
        let mut s = Scheduler::default();
        let p = profile("x", DetectionMode::PollingOnly, 5, 600);
        s.observe(&fp("A"), c.at(0));
        s.after_evaluation(std::slice::from_ref(&p), &["x".to_string()], c.at(0));
        assert!(!s.is_due(&p, c.at(1), false));
        s.reset();
        assert!(s.is_due(&p, c.at(1), false), "改完配置后新的节律要立刻生效");
    }
}
