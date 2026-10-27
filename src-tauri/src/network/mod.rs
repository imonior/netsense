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
    /// `change` 是这一轮实际改了系统哪几项的明细（可能是空的：系统本就合规、跳过下发）。
    Applied {
        note: Option<String>,
        change: NetworkChange,
    },
    Failed {
        reason: String,
        /// 失败之前已经改下去的那些项 —— 回滚会改回去，但用户需要看见「动过什么」。
        change: NetworkChange,
        /// 回滚**写回去**的那几项。回滚是一次真实的系统写入，不报出来就等于
        /// 「谁把我的网络改了」这个问题里少了一条答案 —— 而它往往正是答案本身
        /// （本机「DNS 变成空的」就是回滚写出来的）。
        /// What the rollback wrote back. A rollback is a real write to the system;
        /// leaving it out of the report is leaving out the most likely answer to
        /// "what changed my network".
        /// 恢复失败时这里是 `None` —— 写没写成不知道，就不能说写了。
        rolled_back: Option<Box<NetworkChange>>,
    },
}

/// 一次网络下发的**变更明细**：这一轮到底动了系统的哪几项。
///
/// 为什么要有它：「任何一次对网络的改变都要记录并弹窗提示」这条要求，光有
/// 「应用了 Profile X」是不够的 —— 那句话说的是「引擎决定做什么」，而用户真正
/// 要看见的是「**系统的哪些参数被改了**」。地址、掩码、网关、DNS、路由、IPv6 模式
/// 是六件独立的事，各自可能被改、也可能没被改；把它们列出来，用户才能判断
/// 「这次改动是不是我预期的」。
///
/// 三态是这里的核心：每个字段分「没碰」「清空」「设成某值」三种。
/// `dns` 尤其要分清「缺省 = 不动」与「空 = 交回自动」—— 后者是一次**真的清空**
/// （macOS 上是 `networksetup -setdnsservers <svc> Empty`），而前者什么也没做。
/// 把两者混为一谈，用户就会在「什么都没改」的时候看到一条「DNS 被清空」的假警报。
///
/// 变更明细里的每一项都对应一次真实的系统写入，没有写入的项不会出现在这里。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkChange {
    /// 寻址模式被改了（`dhcp` ⇄ `manual`）。
    pub mode: Option<String>,
    /// IPv4 地址被写成某个值。
    pub ip: Option<String>,
    /// 子网掩码被写成某个值。
    pub netmask: Option<String>,
    /// 网关被写成某个值。
    pub gateway: Option<String>,
    /// DNS 被写成某个值（非空）。
    pub dns: Option<String>,
    /// DNS 被**清空**（交回自动获取 / 交回系统）。与 `dns` 互斥。
    pub dns_cleared: bool,
    /// IPv6 模式被改了（`off` / `automatic` / `linklocal` / `manual`）。
    pub v6mode: Option<String>,
    /// IPv6 地址被写成某个值（`v6mode = manual` 时随模式一起写）。
    pub ipv6: Option<String>,
    /// IPv6 前缀长度被写成某个值（同上）。
    pub v6prefix: Option<String>,
    /// IPv6 网关被写成某个值（同上）。
    pub v6gateway: Option<String>,
    /// 静态路由被加了几条。
    pub routes_added: usize,
    /// 静态路由被删了几条。
    pub routes_deleted: usize,
}

impl NetworkChange {
    /// 这一次有没有真的动过系统。
    ///
    /// 「跳过下发」（系统本来就合规）时这里是空的 —— 那种情况下不该有任何
    /// 「网络被改了」的提示，否则用户会以为系统被动过。
    pub fn is_empty(&self) -> bool {
        self.mode.is_none()
            && self.ip.is_none()
            && self.netmask.is_none()
            && self.gateway.is_none()
            && self.dns.is_none()
            && !self.dns_cleared
            && self.v6mode.is_none()
            && self.ipv6.is_none()
            && self.v6prefix.is_none()
            && self.v6gateway.is_none()
            && self.routes_added == 0
            && self.routes_deleted == 0
    }

    /// 拆成给弹窗用的「逐项动作」：每一项是一句人话（与 [`summary`] 同一批措辞），
    /// 前端据此渲染「动作 | ✅/❌」两列清单，而不是把整段摘要压成一行、混在看板里。
    ///
    /// 这里列的是**实际写进系统的那几项**（来自 3A 下发后的回读记账），不是下发前的
    /// 计划态，也不是配置文件原文 —— 用户要看的是「最终落地的设置」，不是「我想设什么」。
    pub fn items(&self) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(m) = &self.mode {
            parts.push(i18n::tf("net.change_mode", &[("mode", m)]));
        }
        if let Some(v) = &self.ip {
            parts.push(i18n::tf("net.change_ip", &[("value", v)]));
        }
        if let Some(v) = &self.netmask {
            parts.push(i18n::tf("net.change_netmask", &[("value", v)]));
        }
        if let Some(v) = &self.gateway {
            parts.push(i18n::tf("net.change_gateway", &[("value", v)]));
        }
        if self.dns_cleared {
            parts.push(i18n::t("net.change_dns_cleared"));
        } else if let Some(v) = &self.dns {
            parts.push(i18n::tf("net.change_dns", &[("value", v)]));
        }
        if let Some(v) = &self.v6mode {
            parts.push(i18n::tf("net.change_v6mode", &[("mode", v)]));
        }
        if let Some(v) = &self.ipv6 {
            parts.push(i18n::tf("net.change_ipv6", &[("value", v)]));
        }
        if let Some(v) = &self.v6prefix {
            parts.push(i18n::tf("net.change_v6prefix", &[("value", v)]));
        }
        if let Some(v) = &self.v6gateway {
            parts.push(i18n::tf("net.change_v6gateway", &[("value", v)]));
        }
        if self.routes_added > 0 {
            parts.push(i18n::tf("net.change_routes_added", &[("count", &self.routes_added.to_string())]));
        }
        if self.routes_deleted > 0 {
            parts.push(i18n::tf(
                "net.change_routes_deleted",
                &[("count", &self.routes_deleted.to_string())],
            ));
        }
        parts
    }

    /// 一句给用户看的摘要：动了哪几项（各项用分隔符连成一行）。
    ///
    /// 空摘要（什么也没改）返回 `None` —— 调用方据此决定「不发提示」。
    /// 这份摘要同时进日志与弹窗：两处必须是同一句话，否则用户对照日志时
    /// 会以为日志记的和界面弹的不是一回事。
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        Some(Self::items(self).join(&i18n::t("net.change_sep")))
    }
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
/// 回滚，并按实记一行，同时**返回回滚写回去的那几项**（恢复失败时返回 `None`）。
///
/// 返回值不是可选的装饰：回滚本身是一次真实的系统写入（地址、掩码、网关、DNS、
/// 路由都可能被重写），它必须和「下发」一样被报告出来 —— 否则用户在界面上看到的
/// 配置和现场实际生效的配置就此对不上，而日志里只有一行「已回滚」，看不出改回了什么。
///
/// 恢复失败时返回 `None`：写没写成不知道，就不能声称任何「写回了什么」；
/// 调用方（弹窗与回滚播报）据此不说「已回滚」。恢复成功但快照什么都没带时，
/// 返回 `Some(空明细)` —— 成功这件事是真的，只是没写。
///
/// Roll back and record what actually happened. Returns what the rollback wrote,
/// or `None` when the snapshot restore itself failed — nothing may be claimed
/// about writes whose outcome is unknown. A successful restore that had nothing
/// to write still yields `Some` (with an empty list).
fn rollback_and_report<P: NetworkPlatform>(
    plat: &P,
    before: &InterfaceStatus,
) -> Option<NetworkChange> {
    let rolled_back = crate::platform::rollback_change(before);
    match plat.restore_from_snapshot(before) {
        Ok(()) => {
            crate::log::warn(&i18n::t("net.rollback"));
            // 回滚**写回了哪几项**要单独成行：与「下发改了哪几项」是两次不同的写入，
            // 合进一句话就说不清谁改了什么。空的明细（快照什么都没带 ⇒ 回滚什么都没写）
            // 不补这一行 —— 没写就不能说写了。
            if let Some(detail) = rolled_back.summary() {
                crate::log::warn(&i18n::tf("net.rolled_back", &[("detail", &detail)]));
            }
            Some(rolled_back)
        }
        Err(e) => {
            crate::log::error(&i18n::tf("net.rollback_failed", &[("error", &e)]));
            None
        }
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
        return Stage3A::Applied {
            note: None,
            change: NetworkChange::default(),
        };
    }

    // 2. Apply the network configuration. Log "apply begins" first: it is the start
    //    marker of every execution action, pairing with the apply's success
    //    (engine.pass_3a) / failure (engine.error) into one complete timeline.
    // 2. 下发网络配置。先记一行「开始下发」：它是每一次执行动作的起点标记，
    //    与下发的成功（engine.pass_3a）/ 失败（engine.error）配成一条完整时间线。
    crate::log::info(&i18n::tf("net.apply_start", &[("mode", mode_name(&cfg.mode))]));
    // 本轮实际会动系统的哪几项 —— 先算出来，好让「变更明细」在下发失败时
    // 也能报出来（那次系统可能已经被改了一半）。
    // What this round will actually write to the system: computed up front so the
    // change list is available even when the apply itself fails halfway.
    let planned = planned_change(cfg);
    if let Err(e) = plat.apply_network(cfg) {
        // 下发失败：尝试回滚
        let rolled_back = rollback_and_report(plat, &before).map(Box::new);
        return Stage3A::Failed {
            reason: i18n::tf("net.apply_failed", &[("error", &e)]),
            change: planned,
            rolled_back,
        };
    }

    // 3. 下发静态路由（记录已添加的路由，失败时回滚）
    let mut added_routes: Vec<(String, String)> = Vec::new();
    let mut change = planned;
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
            let rolled_back = rollback_and_report(plat, &before).map(Box::new);
            return Stage3A::Failed {
                reason: i18n::tf("net.route_failed", &[("dest", &r.dest), ("error", &e)]),
                change,
                rolled_back,
            };
        }
        // One line per route step (debug level): when troubleshooting "the route was
        // added, why is it not in effect", the log must show which route and from
        // which step onward it went missing.
        // 每一步路由都留一行（debug 档）：排查「路由明明加了怎么没生效」时，
        // 要能看出是哪一条、从哪一步开始不见的。
        if r.delete {
            crate::log::debug(&i18n::tf("net.route_deleted", &[("dest", &r.dest)]));
            change.routes_deleted += 1;
        } else if let Some(g) = r.gateway.as_deref() {
            crate::log::debug(&i18n::tf(
                "net.route_added",
                &[("dest", &r.dest), ("gw", g)],
            ));
            change.routes_added += 1;
        }
        if !r.delete {
            if let Some(g) = r.gateway.as_deref() {
                added_routes.push((r.dest.clone(), g.to_string()));
            }
        }
    }

    let verify = cfg.verify.clone().unwrap_or_default();
    if !verify.readback {
        return Stage3A::Applied {
            note: None,
            change,
        };
    }
    let mut last: Option<VerifyFailure> = None;
    for attempt in 0..READBACK_ATTEMPTS {
        match readback_match(plat, cfg, &before.routes) {
            Ok(()) => {
                return Stage3A::Applied {
                    note: None,
                    change,
                }
            }
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
            // 兜底是**另一次** DNS 写入：把生效的服务器记进变更明细，
            // 否则用户看到的仍是「DNS 被清空」，而实际生效的是兜底那几台。
            let mut change = change;
            change.dns = Some(servers.clone());
            change.dns_cleared = false;
            return Stage3A::Applied {
                note: Some(i18n::tf("net.dns_fallback_note", &[("servers", &servers)])),
                change,
            };
        }
    }

    // 校验失败：回滚到快照状态
    let rolled_back = rollback_and_report(plat, &before).map(Box::new);
    Stage3A::Failed {
        reason: i18n::tf(
            "net.verify_failed",
            &[
                ("tries", &READBACK_ATTEMPTS.to_string()),
                ("detail", &last.map(|f| f.to_string()).unwrap_or_default()),
            ],
        ),
        change,
        rolled_back,
    }
}

/// 一份配置**打算**动系统的哪几项（不含路由 —— 路由在 `apply_3a` 里逐条统计）。
///
/// 为什么单独算一遍而不是在改完之后反推：改失败时也要报「动过什么」，
/// 而失败的那一刻系统可能已经被改了一半 —— 那正是用户最需要知道「到底动了哪几项」
/// 的时刻（回滚会改回去，但「动过」这件事发生过）。
///
/// DNS 的三态在这里被摊开成两种可报形态：
/// - 缺省 → 什么都不记（这一轮没碰 DNS）；
/// - 空串 → `dns_cleared`（一次**真的清空**：macOS 上是
///   `networksetup -setdnsservers <svc> Empty`）；
/// - 非空 → `dns`（写成了哪几台）。
///
/// 缺省与空串在 `networksetup` 那边长得完全不同（前者什么都不做，后者是清空），
/// 所以它们必须在这里就是两种结果，不能合并。
fn planned_change(cfg: &NetworkConfig) -> NetworkChange {
    let mut c = NetworkChange {
        mode: Some(mode_name(&cfg.mode).to_string()),
        ip: cfg.ip.clone(),
        netmask: cfg.netmask.clone(),
        gateway: cfg.gateway.clone(),
        ..Default::default()
    };
    match cfg.dns.as_deref() {
        None => {}
        Some(d) if d.trim().is_empty() => c.dns_cleared = true,
        Some(d) => c.dns = Some(d.to_string()),
    }
    if let Some(v) = &cfg.v6mode {
        c.v6mode = Some(
            match v {
                crate::config::V6Mode::Off => "off",
                crate::config::V6Mode::Automatic => "automatic",
                crate::config::V6Mode::Manual => "manual",
                crate::config::V6Mode::LinkLocal => "linklocal",
            }
            .to_string(),
        );
        // manual 的地址三件套随模式一起记：弹窗的 IPv6 组要给出「地址 / 前缀 / 网关」
        // 三行，日志的变更明细同理 —— 只报一个 "manual" 用户无从核对。
        if let crate::config::V6Mode::Manual = v {
            c.ipv6 = cfg.ipv6.clone();
            c.v6prefix = cfg.v6prefix.clone();
            c.v6gateway = cfg.v6gateway.clone();
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_carries_the_reason_that_blocks_stage_3b() {
        // 引擎按这个 reason 给 Profile 标 ERROR，所以它必须是给人看的完整句子
        let s = Stage3A::Failed {
            reason: "回读校验未通过".into(),
            change: NetworkChange::default(),
            rolled_back: Some(Box::default()),
        };
        match s {
            Stage3A::Failed { reason, .. } => assert_eq!(reason, "回读校验未通过"),
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
            Stage3A::Applied { note, change } => {
                assert!(
                    note.as_deref().unwrap_or_default().contains("10.0.0.53"),
                    "降级成功必须写明用的是哪台兜底 DNS: {:?}",
                    note
                );
                // 变更明细里生效的必须是**兜底那几台**，而不是「DNS 被清空」：
                // 用户看到的就是此刻系统里真正在用的解析器。
                assert_eq!(
                    change.dns.as_deref(),
                    Some("10.0.0.53"),
                    "兜底生效后变更明细要报兜底服务器: {change:?}"
                );
                assert!(
                    !change.dns_cleared,
                    "兜底已经补上了，不能还说 DNS 被清空: {change:?}"
                );
            }
            Stage3A::Failed { reason, .. } => panic!("有兜底且确实补上了就该通过: {reason}"),
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

    /// 变更明细的三态：DNS **缺省** = 什么也没做，**空串** = 一次真的清空，
    /// **非空** = 写成了哪几台。
    ///
    /// 这三种在 `networksetup` 那边长得完全不同（前者什么都不做，后者是
    /// `-setdnsservers <svc> Empty`），混为一谈就会在「什么都没改」的时候
    /// 报一条「DNS 被清空」的假警报 —— 而假警报多了，用户就不再看真警报了。
    #[test]
    fn the_change_list_tells_dns_untouched_from_dns_cleared() {
        let untouched = planned_change(&NetworkConfig {
            mode: crate::config::Mode::Dhcp,
            ..Default::default()
        });
        assert!(
            untouched.dns.is_none() && !untouched.dns_cleared,
            "DNS 缺省 = 不动，不能说成清空: {untouched:?}"
        );

        let cleared = planned_change(&NetworkConfig {
            mode: crate::config::Mode::Dhcp,
            dns: Some(String::new()),
            ..Default::default()
        });
        assert!(
            cleared.dns_cleared && cleared.dns.is_none(),
            "DNS 空串 = 一次真的清空: {cleared:?}"
        );

        let set = planned_change(&NetworkConfig {
            mode: crate::config::Mode::Manual,
            dns: Some("10.0.0.1,10.0.0.2".into()),
            ..Default::default()
        });
        assert!(
            !set.dns_cleared && set.dns.as_deref() == Some("10.0.0.1,10.0.0.2"),
            "DNS 非空 = 写成了哪几台: {set:?}"
        );
    }

    /// 变更明细必须说清**哪些**参数被改了 —— 这是「任何一次对网络的改变都要记录
    /// 和弹窗提示」这条要求能落地的前提：只有笼统的「应用了 Profile X」，
    /// 用户无法判断这次改动是不是自己预期的。
    #[test]
    fn the_change_list_names_every_field_the_apply_writes() {
        let c = planned_change(&NetworkConfig {
            mode: crate::config::Mode::Manual,
            ip: Some("10.20.20.168".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("10.20.20.20".into()),
            dns: Some("10.20.20.20,1.1.1.1".into()),
            v6mode: Some(crate::config::V6Mode::Off),
            ..Default::default()
        });
        assert_eq!(c.mode.as_deref(), Some("manual"));
        assert_eq!(c.ip.as_deref(), Some("10.20.20.168"));
        assert_eq!(c.netmask.as_deref(), Some("255.255.255.0"));
        assert_eq!(c.gateway.as_deref(), Some("10.20.20.20"));
        assert_eq!(c.dns.as_deref(), Some("10.20.20.20,1.1.1.1"));
        assert_eq!(c.v6mode.as_deref(), Some("off"));
        assert!(!c.dns_cleared);
        // 摘要必须把六项都说出来，否则用户只看到一半改动。
        let s = c.summary().expect("有变更就该有摘要");
        for needle in ["10.20.20.168", "255.255.255.0", "10.20.20.20", "1.1.1.1"] {
            assert!(s.contains(needle), "摘要里缺 {needle}: {s}");
        }
    }

    /// 什么都没改 → 没有摘要 → 上层据此**不发**任何提示。
    /// 「网络被改了」这句话必须是假的反面：没改就不能说改了。
    #[test]
    fn nothing_changed_produces_no_summary_and_no_notice() {
        assert!(NetworkChange::default().is_empty());
        assert!(NetworkChange::default().summary().is_none());
        // 跳过下发的那条路径（系统本就合规）返回的就是空明细。
        use crate::platform::MockPlatform;
        let mock = MockPlatform::new();
        mock.clear_calls();
        let cfg = NetworkConfig {
            mode: crate::config::Mode::Manual,
            ip: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1".into()),
            ..Default::default()
        };
        mock.set_state(crate::platform::InterfaceStatus {
            connected: true,
            ipv4: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: Some("192.168.1.1".into()),
            ..Default::default()
        });
        match apply_3a(&mock, &cfg, &[]) {
            Stage3A::Applied { change, .. } => {
                assert!(change.is_empty(), "跳过下发时不该有变更明细: {change:?}");
                assert!(change.summary().is_none(), "跳过下发时不该有摘要");
            }
            other => panic!("已符合期望应直接 Applied: {other:?}"),
        }
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
