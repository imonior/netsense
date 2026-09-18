//! 健康度监测：后台 probe loop，连续失败则按 fallback 开关回落 DHCP。
//! 对标 wireguide-plus 延迟探测，但动作改为"保活上网"（回落 DHCP）。

use crate::config::HealthConfig;
use crate::platform::{Health, NetworkPlatform, ProbeTarget};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct HealthMonitor;

impl HealthMonitor {
    /// 后台线程循环探测；连续失败 ≥ `retries` 时调用 `on_fallback`（调用方据 `fallback.enabled` 决定动作）。
    /// `stop` 用于 SSID 变化或退出时中止上一个 loop。
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
        let interval = cfg.interval;
        let retries = cfg.retries;
        let do_fallback = cfg.fallback.enabled;

        std::thread::spawn(move || {
            let mut fails: u32 = 0;
            loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let h = plat.probe(&target, timeout * 1000);
                if h == Health::Fail {
                    fails += 1;
                } else {
                    fails = 0;
                }
                if fails >= retries {
                    if do_fallback {
                        on_fallback();
                    }
                    return; // 回落后保持现状，直到下次 SSID 变化重启监测
                }
                std::thread::sleep(Duration::from_secs(interval));
            }
        });
    }
}
