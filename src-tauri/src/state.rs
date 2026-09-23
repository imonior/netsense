//! 共享状态与广播出口。
//!
//! 为什么从 `main.rs` 拆出来：`main` 应该只做「装配」（解析路径 → 建状态 → 起线程 →
//! 挂回调），而状态的所有权、加锁方式和「怎么把一次变化告诉前端」是全部模块共同的
//! 依赖。留在 `main.rs` 里会让 `engine` 反过来依赖入口文件，装配关系就倒过来了。
//!
//! ## 广播只有一条路径
//!
//! [`publish_status`] 每次刷新**只采样一次** `get_status()`、一次 `list_interfaces()`，
//! 然后把同一份快照分给面板事件与托盘菜单。两处各自采样的话，macOS 15.6+ 上每次刷新
//! 要多付 1~4 秒（`system_profiler`），还可能显示不一致。
//!
//! ## 谁可以改状态
//!
//! 只有引擎线程改写 [`Engine`]；其它入口（IPC、托盘菜单、退出）一律通过 [`post`]
//! 往引擎投消息。这是「同一时刻最多一个 Active Profile」成立的前提。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use tauri::{AppHandle, Emitter};

use crate::config::Config;
use crate::engine::{Engine, Msg};
// `Platform` 是编译期选定的平台实现（macOS / Windows / Linux 之一），
// 上层代码一律只依赖它，不出现任何具体平台类型名。
use crate::platform::{priv_channel, NetworkPlatform as _, Platform, WatcherHandle};
use crate::{i18n, platform};

/// 全局共享状态（被托盘菜单、IPC 命令、引擎线程共同访问）。
pub struct AppState {
    pub config_path: PathBuf,
    pub config: Mutex<Config>,
    pub scripts_dir: PathBuf,
    pub plat: Platform,
    /// 引擎状态机。锁顺序恒为 `engine` → `config`（见 `engine` 模块头）。
    pub engine: Mutex<Engine>,
    /// 引擎线程入口。`OnceLock`：线程在 `setup` 里才起，但 IPC 命令随时要投消息。
    pub engine_tx: OnceLock<Sender<Msg>>,
    /// 配置文件已观察到的 mtime；由**引擎线程**单点轮询，用于热重载。
    pub config_mtime: Mutex<Option<SystemTime>>,
    /// Tauri AppHandle（setup 中填充）；用于向 popup / editor 广播状态事件。
    pub app: OnceLock<AppHandle>,
    /// SSID 监视线程句柄；重启监视或退出时用 `stop()` 停掉旧线程。
    pub watcher: Mutex<Option<WatcherHandle>>,
}

impl AppState {
    /// 加载状态。
    ///
    /// 配置读不出来时**不**回退成「空配置照常跑」：加载失败几乎总是 schema 不符，
    /// 静默跑起来等于当着用户的面什么都不做，而报错能直接告诉他该干嘛。
    pub fn load(config_path: PathBuf) -> Result<AppState, String> {
        let config = Config::load(&config_path).and_then(|c| c.validate().map(|_| c))?;
        let scripts_dir = config_path
            .parent()
            .map(|p| p.join("scripts"))
            .unwrap_or_else(|| PathBuf::from("scripts"));
        let warnings: Vec<String> = config.warnings().iter().map(|w| w.0.clone()).collect();
        let mut engine = Engine::new();
        engine.set_warnings(warnings);
        Ok(AppState {
            config_path,
            config: Mutex::new(config),
            scripts_dir,
            plat: Platform,
            engine: Mutex::new(engine),
            engine_tx: OnceLock::new(),
            config_mtime: Mutex::new(None),
            app: OnceLock::new(),
            watcher: Mutex::new(None),
        })
    }

    /// 配置读不出来时的兜底状态。
    ///
    /// 为什么不能直接退出：托盘应用静默消失与「装完打不开」在用户眼里是同一件事，
    /// 而这里的原因只是磁盘上有一份本版本不认识的配置（schema 不符或结构不合法）。
    /// 跑起来、把错误原文
    /// 放进 `warnings`（设置面板与日志都看得到），才是可操作的结局。
    pub fn fallback(config_path: PathBuf, error: String) -> AppState {
        let cfg = Config {
            schema: crate::config::SCHEMA,
            ..Default::default()
        };
        let mut engine = Engine::new();
        engine.set_warnings(vec![error]);
        AppState {
            config_path,
            config: Mutex::new(cfg),
            scripts_dir: std::path::Path::new("scripts").to_path_buf(),
            plat: Platform,
            engine: Mutex::new(engine),
            engine_tx: OnceLock::new(),
            config_mtime: Mutex::new(None),
            app: OnceLock::new(),
            watcher: Mutex::new(None),
        }
    }
}

/// 给引擎线程投一条消息。引擎还没起来（启动极早期）时返回 false，调用方忽略即可。
pub fn post(state: &AppState, msg: Msg) -> bool {
    match state.engine_tx.get() {
        Some(tx) => tx.send(msg).is_ok(),
        None => false,
    }
}

/// 显式退出标志：只有「用户要求退出」的入口（托盘菜单「退出」、IPC `quit_app`）会置位。
///
/// 用途：`RunEvent::ExitRequested` 同时被两种情况触发 —— 用户主动退出，以及
/// **所有窗口都不见了**（Tauri 的默认语义）。托盘型应用必须阻止后者（否则关掉编辑器
/// 窗口就退出整个服务），又必须放行前者。判据用这个自己控制的标志，而不是依赖
/// `ExitRequested.code` 是否为 `None`：一旦某版本对显式退出也传 `None`，
/// 应用会变得无法退出，那是比误退出更糟的故障。
pub static QUITTING: AtomicBool = AtomicBool::new(false);

/// 启动是否已走完（`setup` 末尾置位）。
///
/// 用途：区分「启动阶段就崩了」和「运行期某个后台线程崩了」—— 只有前者需要弹模态
/// 对话框（进程必然消失，且没有任何窗口能承载错误信息）。
pub static APP_STARTED: AtomicBool = AtomicBool::new(false);

/// 置位显式退出标志并退出应用。所有「退出」入口都应走这里，不要直接 `app.exit(0)`。
pub fn request_quit(app: &AppHandle, state: &Arc<AppState>) {
    QUITTING.store(true, Ordering::SeqCst);
    // 先让引擎自己收尾（停监测线程），再结束进程：`app.exit` 不等后台线程，
    // 但发一条 Quit 至少能让引擎退出主循环并留下「引擎线程退出」的日志。
    post(state, Msg::Quit);
    if let Some(h) = state
        .watcher
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        h.stop();
    }
    app.exit(0);
}

/// 向所有窗口发一个事件（popup / editor 各自监听后刷新）。
pub fn emit(state: &Arc<AppState>, event: &str, payload: serde_json::Value) {
    if let Some(app) = state.app.get() {
        let _ = app.emit(event, payload);
    }
}

/// 后台动作（设为 DHCP / 立即探测 / 立即应用）的结果广播给前端做 toast。
pub fn emit_action(state: &Arc<AppState>, kind: &str, ok: bool, message: String) {
    emit(
        state,
        "netsense://action",
        serde_json::json!({ "kind": kind, "ok": ok, "message": message }),
    );
}

/// 组装对外暴露的状态载荷（IPC 查询与事件广播共用同一形状）。
///
/// 显式接收已采到的 `st` 而不是内部自行采样：`get_status()` 会拉起若干子进程，
/// 在 macOS 15.6+ 上还包括一次 1~4s 的 `system_profiler`；同一次刷新里面板广播与
/// 托盘菜单必须共用同一份快照，否则既多付一次代价、又可能出现两处显示不一致。
pub fn status_payload(state: &Arc<AppState>, st: &platform::InterfaceStatus) -> serde_json::Value {
    let engine_view = {
        let eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::to_value(eng.view(&cfg)).unwrap_or_default()
    };
    let (profiles, config_path) = {
        let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        (
            cfg.profiles
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "id": p.id, "name": p.name, "enabled": p.enabled,
                    })
                })
                .collect::<Vec<_>>(),
            state.config_path.display().to_string(),
        )
    };
    serde_json::json!({
        "status": st,
        "engine": engine_view,
        "profiles": profiles,
        "language": i18n::current().code(),
        "priv": priv_channel().code(),
        "config_path": config_path,
    })
}

/// 广播状态：向所有窗口发 `netsense://status`（前端监听到后自行刷新，避免轮询），
/// 并把托盘菜单文案刷新到最新（网卡明细 / 当前 Profile / 权限通道）。
///
/// 本函数会被**引擎线程**调用，而托盘菜单更新会触及 AppKit，因此必须经
/// `run_on_main_thread` 回到主线程执行。
pub fn publish_status(state: &Arc<AppState>) {
    let Some(app) = state.app.get().cloned() else {
        return;
    };
    let st = state.plat.get_status();
    let _ = app.emit("netsense://status", status_payload(state, &st));

    // 网卡清单同样只在这里采样一次：托盘菜单要按**每张**网卡分别展示，
    // 而菜单构造在主线程，绝不能在主线程里再采一次（见下方注释）。
    let nics = state.plat.list_interfaces();
    let current = crate::engine::active_display_name(state);
    let app_for_menu = app.clone();
    // ⚠️ 托盘菜单必须在**主线程**重建（会触 AppKit），而菜单里的网络信息直接取自上面
    // 那次采样并 move 进闭包 —— 绝不能在主线程里再采一次。否则 macOS 15.6+ 上每次
    // 状态广播都会在主线程跑一次 system_profiler（1~4s），把 UI（连同弹窗展开）卡死。
    let _ = app.run_on_main_thread(move || {
        if let Some(tray) = app_for_menu.tray_by_id("main") {
            if let Ok(menu) = crate::tray::tray_menu(&app_for_menu, &st, &nics, &current) {
                let _ = tray.set_menu(Some(menu));
            }
        }
    });
}
