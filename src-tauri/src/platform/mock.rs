//! Mock 平台实现 —— 用于集成测试与 3A 事务回滚验证。
//!
//! 它把所有操作记录到 `calls` 里，调用方可按序断言；
//! 状态（IP/掩码/网关/DNS/路由）由调用方通过 `set_state()` 注入，
//! 所以同一个 mock 既能模拟「下发成功」、也能模拟「下发失败」与「回读校验失败」。
//!
//! ## 为什么是零大小 `Copy` 单元结构体（与真实 PAL 同形）
//!
//! 生产代码把 `Platform`（编译期别名）当成**值**使用：真实 PAL 是零大小单元结构体，
//! 因此 `crate::platform::Platform`（作为值）与 `state.plat` 按值传给需要 `Copy` 的
//! `one_shot::spawn` / `HealthMonitor::start` / `Session::start` 都能编译。
//! Mock 必须保持同形，才能让 `engine-mock` 特性下整条生产路径零改动地编译。
//!
//! 状态于是放进 `thread_local!`：每个线程一份，天然隔离并行测试；引擎线程（也是测试线程）
//! 读写的是同一份，断言对得上。`thread_local` 不是 `Sync`，但 `MockPlatform` 本身是单元
//! 结构体（自动 `Send + Sync + Copy`），跨线程传递的只是这个零大小标记。
//!
//! ## Why a zero-sized `Copy` unit struct (same shape as the real PAL)
//!
//! Production code uses `Platform` (a compile-time alias) *as a value*: the real PAL is a
//! zero-sized unit struct, so `crate::platform::Platform` (as a value) and `state.plat` passed
//! by value into `Copy`-requiring `one_shot::spawn` / `HealthMonitor::start` / `Session::start`
//! all compile. The mock must keep the same shape so the whole production path compiles unchanged
//! under `engine-mock`. State therefore lives in a `thread_local!` — one copy per thread, which
//! naturally isolates parallel tests; the engine thread (also the test thread) reads/writes the
//! same copy, so assertions line up.

use crate::config::model::RouteConfig;
use crate::config::NetworkConfig;
use crate::platform::{
    AppEntry, Health, InterfaceStatus, NetworkPlatform, NicInfo, PrinterInfo, ProbeTarget,
    TunnelTarget, WatcherHandle,
};
use std::cell::RefCell;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread_local;

/// 一次平台调用的记录。
///
/// 字段/变体在测试中按需断言；未被某次测试读取的字段属正常情况，
/// 用 `allow(dead_code)` 避免 clippy 在 `-D warnings` 下报错。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum Call {
    GetStatus,
    ApplyNetwork { cfg: NetworkConfig },
    SetDhcp,
    SetDhcpFor { dev: String },
    RestoreFromSnapshot { snap: InterfaceStatus },
    ListInterfaces,
    ListAdapters,
    WatchSsid,
    GetCurrentSsid,
    Probe { target: ProbeTarget, timeout_ms: u64 },
    TunnelIsUp { target: TunnelTarget },
    TunnelConnect { target: TunnelTarget },
    AddRoute { dest: String, gw: String, metric: u32 },
    DeleteRoute { dest: String },
    LaunchApp { app: String, args: Vec<String> },
    RunScript { path: String, args: Vec<String>, elevated: bool },
    ListPrinters,
    SetDefaultPrinter { printer: String },
    ListInstalledApps,
    PickApp,
    ListKnownSsids,
    UiLanguage,
    UiPrefersDark,
}

/// Mock 平台的内部状态。每个线程一份（见 [`MockPlatform`] 头注释）。
/// Per-thread mock state (see the `MockPlatform` header comment).
struct MockInner {
    calls: Vec<Call>,
    state: InterfaceStatus,
    routes: Vec<(String, String, u32)>, // (dest, gw, metric)
    probe_result: Health,
    tunnel_is_up: bool,
    tunnel_connect_result: Result<(), String>,
    apply_network_result: Result<(), String>,
    set_dhcp_result: Result<(), String>,
}

impl Default for MockInner {
    fn default() -> Self {
        Self {
            calls: Vec::new(),
            state: InterfaceStatus::default(),
            routes: Vec::new(),
            probe_result: Health::Ok,
            tunnel_is_up: false,
            tunnel_connect_result: Ok(()),
            apply_network_result: Ok(()),
            set_dhcp_result: Ok(()),
        }
    }
}

thread_local! {
    /// 当前线程的 Mock 状态。引擎线程（测试线程）与断言读同一份。
    /// The mock state for the current thread; the engine thread (the test thread) and the
    /// assertions read the same copy.
    static INNER: RefCell<MockInner> = RefCell::new(MockInner::default());
}

/// Mock 平台实现：零大小 `Copy` 单元结构体，与真实 PAL 同形（见模块头）。
/// Mock platform: a zero-sized `Copy` unit struct, same shape as the real PAL (see module header).
#[derive(Clone, Copy, Default)]
pub struct MockPlatform;

// 注入用的 setter 是测试夹具 API，部分在当前测试集中未被调用，属正常情况。
#[allow(dead_code)]
impl MockPlatform {
    pub fn new() -> Self {
        Self
    }

    /// 注入当前网络状态（`get_status` 的返回值）。
    pub fn set_state(&self, state: InterfaceStatus) {
        INNER.with(|i| i.borrow_mut().state = state);
    }

    /// 注入「下发前已在网卡上的旧路由」（3A 回滚测试用：验证回滚不会误删这些路由）。
    pub fn set_routes(&self, routes: Vec<(String, String, u32)>) {
        INNER.with(|i| i.borrow_mut().routes = routes);
    }

    /// 注入 `probe` 的返回值。
    pub fn set_probe_result(&self, h: Health) {
        INNER.with(|i| i.borrow_mut().probe_result = h);
    }

    /// 注入 `tunnel_is_up` 的返回值。
    pub fn set_tunnel_is_up(&self, up: bool) {
        INNER.with(|i| i.borrow_mut().tunnel_is_up = up);
    }

    /// 注入 `tunnel_connect` 的返回值。
    pub fn set_tunnel_connect_result(&self, r: Result<(), String>) {
        INNER.with(|i| i.borrow_mut().tunnel_connect_result = r);
    }

    /// 注入 `apply_network` 的返回值。
    pub fn set_apply_network_result(&self, r: Result<(), String>) {
        INNER.with(|i| i.borrow_mut().apply_network_result = r);
    }

    /// 注入 `set_dhcp` 的返回值。
    pub fn set_set_dhcp_result(&self, r: Result<(), String>) {
        INNER.with(|i| i.borrow_mut().set_dhcp_result = r);
    }

    /// 获取所有已记录的调用。
    pub fn calls(&self) -> Vec<Call> {
        INNER.with(|i| i.borrow().calls.clone())
    }

    /// 清空调用记录。
    pub fn clear_calls(&self) {
        INNER.with(|i| i.borrow_mut().calls.clear());
    }

    /// 获取当前已添加的路由。
    pub fn routes(&self) -> Vec<(String, String, u32)> {
        INNER.with(|i| i.borrow().routes.clone())
    }

    fn record(&self, call: Call) {
        INNER.with(|i| i.borrow_mut().calls.push(call));
    }
}

impl NetworkPlatform for MockPlatform {
    fn get_status(&self) -> InterfaceStatus {
        self.record(Call::GetStatus);
        INNER.with(|i| i.borrow().state.clone())
    }

    fn apply_network(&self, cfg: &NetworkConfig) -> Result<(), String> {
        self.record(Call::ApplyNetwork { cfg: cfg.clone() });
        let r = INNER.with(|i| i.borrow().apply_network_result.clone());
        if r.is_ok() {
            INNER.with(|i| {
                let mut inner = i.borrow_mut();
                inner.state.ipv4 = cfg.ip.clone();
                inner.state.netmask = cfg.netmask.clone();
                inner.state.gateway = cfg.gateway.clone();
                inner.state.dns = cfg.dns.clone();
            });
        }
        r
    }

    fn set_dhcp(&self) -> Result<(), String> {
        self.record(Call::SetDhcp);
        let r = INNER.with(|i| i.borrow().set_dhcp_result.clone());
        if r.is_ok() {
            INNER.with(|i| {
                let mut inner = i.borrow_mut();
                inner.state.ipv4 = None;
                inner.state.netmask = None;
                inner.state.gateway = None;
                inner.state.dns = None;
            });
        }
        r
    }

    fn list_interfaces(&self) -> Vec<NicInfo> {
        self.record(Call::ListInterfaces);
        vec![]
    }

    fn list_adapters(&self) -> Vec<NicInfo> {
        self.record(Call::ListAdapters);
        vec![]
    }

    fn watch_ssid(&self, _cb: Box<dyn Fn(Option<String>) + Send + Sync>) -> WatcherHandle {
        self.record(Call::WatchSsid);
        WatcherHandle {
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    fn get_current_ssid(&self) -> Option<String> {
        self.record(Call::GetCurrentSsid);
        INNER.with(|i| i.borrow().state.ssid.clone())
    }

    fn probe(&self, target: &ProbeTarget, timeout_ms: u64) -> Health {
        self.record(Call::Probe {
            target: target.clone(),
            timeout_ms,
        });
        INNER.with(|i| i.borrow().probe_result)
    }

    fn tunnel_is_up(&self, target: &TunnelTarget) -> bool {
        self.record(Call::TunnelIsUp {
            target: target.clone(),
        });
        INNER.with(|i| i.borrow().tunnel_is_up)
    }

    fn tunnel_connect(&self, target: &TunnelTarget) -> Result<(), String> {
        self.record(Call::TunnelConnect {
            target: target.clone(),
        });
        INNER.with(|i| i.borrow().tunnel_connect_result.clone())
    }

    fn add_route(&self, dest: &str, gw: &str, metric: u32) -> Result<(), String> {
        self.record(Call::AddRoute {
            dest: dest.to_string(),
            gw: gw.to_string(),
            metric,
        });
        // 幂等：路由已存在时不报错（与真实平台行为一致）
        let exists = INNER.with(|i| {
            i.borrow()
                .routes
                .iter()
                .any(|(d, g, _)| d == dest && g == gw)
        });
        if exists {
            return Ok(());
        }
        INNER.with(|i| {
            i.borrow_mut()
                .routes
                .push((dest.to_string(), gw.to_string(), metric));
        });
        Ok(())
    }

    fn delete_route(&self, dest: &str) -> Result<(), String> {
        self.record(Call::DeleteRoute {
            dest: dest.to_string(),
        });
        INNER.with(|i| i.borrow_mut().routes.retain(|(d, _, _)| d != dest));
        Ok(())
    }

    fn get_routes(&self) -> Vec<RouteConfig> {
        INNER.with(|i| {
            i.borrow()
                .routes
                .iter()
                .map(|(d, g, m)| RouteConfig {
                    dest: d.clone(),
                    gateway: Some(g.clone()),
                    metric: *m,
                    delete: false,
                })
                .collect()
        })
    }

    fn restore_from_snapshot(&self, snap: &InterfaceStatus) -> Result<(), String> {
        // 记录回滚动作，并把真正的恢复逻辑委派给共享实现（与三份真实平台一致）。
        // Record the rollback action and delegate the real restore to the shared impl (same as
        // the three real platforms).
        self.record(Call::RestoreFromSnapshot { snap: snap.clone() });
        crate::platform::restore_snapshot_impl(self, snap)
    }

    fn launch_app(&self, app: &str, args: &[String]) -> Result<(), String> {
        self.record(Call::LaunchApp {
            app: app.to_string(),
            args: args.to_vec(),
        });
        Ok(())
    }

    fn run_script(&self, path: &str, args: &[String], elevated: bool) -> Result<(), String> {
        self.record(Call::RunScript {
            path: path.to_string(),
            args: args.to_vec(),
            elevated,
        });
        Ok(())
    }

    fn list_printers(&self) -> Vec<PrinterInfo> {
        self.record(Call::ListPrinters);
        vec![]
    }

    fn set_default_printer(&self, printer: &str) -> Result<(), String> {
        self.record(Call::SetDefaultPrinter {
            printer: printer.to_string(),
        });
        Ok(())
    }

    fn list_installed_apps(&self) -> Vec<AppEntry> {
        self.record(Call::ListInstalledApps);
        vec![]
    }

    fn pick_app(&self) -> Result<Option<String>, String> {
        self.record(Call::PickApp);
        Ok(None)
    }

    fn list_known_ssids(&self) -> Option<Vec<String>> {
        self.record(Call::ListKnownSsids);
        Some(vec![])
    }

    fn ui_language(&self) -> Option<String> {
        self.record(Call::UiLanguage);
        None
    }

    fn ui_prefers_dark(&self) -> Option<bool> {
        self.record(Call::UiPrefersDark);
        None
    }
}
