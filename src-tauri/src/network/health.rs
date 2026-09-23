//! 持续健康监测：Active 期间周期性探测，连续失败则按 `fallback` 开关回落。
//!
//! 它是 3A 校验的**时间延伸**：`apply_3a` 的回读校验只证明「下发那一刻参数写进去了」，
//! 而回落逻辑真正值钱的场景是「连上了一个不通的网络」（portal 拦截、上级交换机挂了、
//! 静态地址配错一位）。所以它跟着 Active 生命周期起停，而不是一个独立的全局后台任务 ——
//! 后者会留下一个真实的坑：切换 Profile 时才顺手停掉旧 loop，于是零命中与手动
//! 「设为 DHCP」之后，旧环境的探测线程还在后台跑，并在用户意想不到的时刻把网卡
//! 改回 DHCP。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::HealthConfig;
use crate::platform::{Health, NetworkPlatform, ProbeTarget};

pub struct HealthMonitor;

impl HealthMonitor {
    /// 后台线程循环探测；连续失败 ≥ `retries` 时调用 `on_fallback`（是否真的回落由
    /// 调用方在 `cfg.fallback.enabled` 上把关，这里只负责「什么时候该处置」）。
    ///
    /// `stop` 用于离开 Active 或切换 Profile 时中止上一个 loop；线程会在下一轮醒来时退出。
    pub fn start<P: NetworkPlatform + 'static>(
        plat: P,
        cfg: &HealthConfig,
        stop: Arc<AtomicBool>,
        on_fallback: impl Fn() + Send + 'static,
    ) {
        if !cfg.enabled {
            return;
        }
        let target = ProbeTarget {
            mode: cfg.mode.clone(),
            http_target: cfg.http_target.clone(),
            icmp_target: cfg.icmp_target.clone(),
        };
        let timeout = cfg.timeout;
        let interval = cfg.interval.max(1);
        let retries = cfg.retries.max(1);
        let do_fallback = cfg.fallback.enabled;

        std::thread::spawn(move || {
            let mut fails: u32 = 0;
            loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                // 探测成功即清零：判据是「连续」失败，中间通过一次说明网络是通的，
                // 否则一次抖动积累的计数会在很久以后误触发回落。
                if plat.probe(&target, timeout * 1000) == Health::Fail {
                    fails += 1;
                } else {
                    fails = 0;
                }
                if fails >= retries {
                    if do_fallback {
                        on_fallback();
                    }
                    return; // 处置完就收工，等下一次激活重新起监测
                }
                // 分片睡眠：`stop()` 之后最多 1s 退出，而不是等完整个 interval
                let mut slept = 0u64;
                while slept < interval {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let step = (interval - slept).min(1);
                    std::thread::sleep(Duration::from_secs(step));
                    slept += step;
                }
            }
        });
    }
}
