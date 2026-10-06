//! Mock 平台实现 —— 用于集成测试与 3A 事务回滚验证。
//!
//! 它把所有操作记录到 `calls` 里，调用方可按序断言；
//! 状态（IP/掩码/网关/DNS/路由）由调用方通过 `set_state()` 注入，
//! 所以同一个 mock 既能模拟「下发成功」、也能模拟「下发失败」与「回读校验失败」。
//!
//! 线程安全：内部用 `Arc<Mutex<MockInner>>`，可按 `Arc<MockPlatform>` 跨线程传递。

use crate::config::NetworkConfig;
use crate::platform::{
    AppEntry, Health, InterfaceStatus, NetworkPlatform, NicInfo, PrinterInfo,
    ProbeTarget, TunnelTarget, WatcherHandle,
};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

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

/// Mock 平台实现。
#[derive(Clone)]
pub struct MockPlatform {
    inner: Arc<Mutex<MockInner>>,
}

// 注入用的 setter 是测试夹具 API，部分在当前测试集中未被调用，属正常情况。
#[allow(dead_code)]
impl MockPlatform {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockInner::default())),
        }
    }

    /// 注入当前网络状态（`get_status` 的返回值）。
    pub fn set_state(&self, state: InterfaceStatus) {
        self.inner.lock().unwrap().state = state;
    }

    /// 注入 `probe` 的返回值。
    pub fn set_probe_result(&self, h: Health) {
        self.inner.lock().unwrap().probe_result = h;
    }

    /// 注入 `tunnel_is_up` 的返回值。
    pub fn set_tunnel_is_up(&self, up: bool) {
        self.inner.lock().unwrap().tunnel_is_up = up;
    }

    /// 注入 `tunnel_connect` 的返回值。
    pub fn set_tunnel_connect_result(&self, r: Result<(), String>) {
        self.inner.lock().unwrap().tunnel_connect_result = r;
    }

    /// 注入 `apply_network` 的返回值。
    pub fn set_apply_network_result(&self, r: Result<(), String>) {
        self.inner.lock().unwrap().apply_network_result = r;
    }

    /// 注入 `set_dhcp` 的返回值。
    pub fn set_set_dhcp_result(&self, r: Result<(), String>) {
        self.inner.lock().unwrap().set_dhcp_result = r;
    }

    /// 获取所有已记录的调用。
    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().unwrap().calls.clone()
    }

    /// 清空调用记录。
    pub fn clear_calls(&self) {
        self.inner.lock().unwrap().calls.clear();
    }

    /// 获取当前已添加的路由。
    pub fn routes(&self) -> Vec<(String, String, u32)> {
        self.inner.lock().unwrap().routes.clone()
    }

    fn record(&self, call: Call) {
        self.inner.lock().unwrap().calls.push(call);
    }
}

impl NetworkPlatform for MockPlatform {
    fn get_status(&self) -> InterfaceStatus {
        self.record(Call::GetStatus);
        self.inner.lock().unwrap().state.clone()
    }

    fn apply_network(&self, cfg: &NetworkConfig) -> Result<(), String> {
        self.record(Call::ApplyNetwork { cfg: cfg.clone() });
        let r = self.inner.lock().unwrap().apply_network_result.clone();
        if r.is_ok() {
            let mut inner = self.inner.lock().unwrap();
            inner.state.ipv4 = cfg.ip.clone();
            inner.state.netmask = cfg.netmask.clone();
            inner.state.gateway = cfg.gateway.clone();
            inner.state.dns = cfg.dns.clone();
        }
        r
    }

    fn set_dhcp(&self) -> Result<(), String> {
        self.record(Call::SetDhcp);
        let r = self.inner.lock().unwrap().set_dhcp_result.clone();
        if r.is_ok() {
            let mut inner = self.inner.lock().unwrap();
            inner.state.ipv4 = None;
            inner.state.netmask = None;
            inner.state.gateway = None;
            inner.state.dns = None;
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
        self.inner.lock().unwrap().state.ssid.clone()
    }

    fn probe(&self, target: &ProbeTarget, timeout_ms: u64) -> Health {
        self.record(Call::Probe {
            target: target.clone(),
            timeout_ms,
        });
        self.inner.lock().unwrap().probe_result
    }

    fn tunnel_is_up(&self, target: &TunnelTarget) -> bool {
        self.record(Call::TunnelIsUp {
            target: target.clone(),
        });
        self.inner.lock().unwrap().tunnel_is_up
    }

    fn tunnel_connect(&self, target: &TunnelTarget) -> Result<(), String> {
        self.record(Call::TunnelConnect {
            target: target.clone(),
        });
        self.inner.lock().unwrap().tunnel_connect_result.clone()
    }

    fn add_route(&self, dest: &str, gw: &str, metric: u32) -> Result<(), String> {
        self.record(Call::AddRoute {
            dest: dest.to_string(),
            gw: gw.to_string(),
            metric,
        });
        let mut inner = self.inner.lock().unwrap();
        // 幂等：路由已存在时不报错（与真实平台行为一致）
        if inner.routes.iter().any(|(d, g, _)| d == dest && g == gw) {
            return Ok(());
        }
        inner.routes.push((dest.to_string(), gw.to_string(), metric));
        Ok(())
    }

    fn delete_route(&self, dest: &str) -> Result<(), String> {
        self.record(Call::DeleteRoute {
            dest: dest.to_string(),
        });
        let mut inner = self.inner.lock().unwrap();
        inner.routes.retain(|(d, _, _)| d != dest);
        Ok(())
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