//! 共享状态与广播出口。
//!
//! 为什么从 `main.rs` 拆出来：`main` 应该只做「装配」（解析路径 → 建状态 → 起线程 →
//! 挂回调），而状态的所有权、加锁方式和「怎么把一次变化告诉前端」是全部模块共同的
//! 依赖。留在 `main.rs` 里会让 `engine` 反过来依赖入口文件，装配关系就倒过来了。
//!
//! ## 广播只有一条路径
//!
//! [`publish_status`] 每次刷新**只采样一次** `get_status()`，把这一份快照发给面板与编辑器。
//! 采样会拉起子进程（macOS 15.6+ 还包含一次 1~4 秒的 `system_profiler`），所以平台层自带
//! TTL 缓存，且这条路径上绝不重复采样 —— 网卡明细由前端另外调 `get_interfaces` 取，
//! 那份列表有它自己的缓存与失效节奏。
//!
//! ## 谁可以改状态
//!
//! 只有引擎线程改写 [`Engine`]；其它入口（IPC、托盘图标、退出）一律通过 [`post`]
//! 往引擎投消息。这是「同一时刻最多一个 Active Profile」成立的前提。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use tauri::{AppHandle, Emitter};

use crate::appconfig::AppConfig;
use crate::config::Config;
use crate::engine::{Engine, Msg};
// `Platform` 是编译期选定的平台实现（macOS / Windows / Linux 之一），
// 上层代码一律只依赖它，不出现任何具体平台类型名。
use crate::platform::{priv_channel, NetworkPlatform as _, Platform, WatcherHandle};
use crate::{i18n, platform};

/// 全局共享状态（被面板广播、IPC 命令、引擎线程共同访问）。
pub struct AppState {
    pub config_path: PathBuf,
    pub config: Mutex<Config>,
    /// 软件配置文件的位置（见 [`crate::appconfig`]）。与 `config_path` 分开存：
    /// 它没有「同目录兜底」那一档，也不参与热重载 —— 改它只可能来自这个进程自己。
    pub settings_path: PathBuf,
    pub settings: Mutex<AppConfig>,
    pub scripts_dir: PathBuf,
    pub plat: Platform,
    /// 引擎状态机。锁顺序恒为 `engine` → `config`（见 `engine` 模块头）。
    pub engine: Mutex<Engine>,
    /// 引擎线程入口。`OnceLock`：线程在 `setup` 里才起，但 IPC 命令随时要投消息。
    pub engine_tx: OnceLock<Sender<Msg>>,
    /// 配置文件已观察到的 mtime；由**引擎线程**单点轮询，用于热重载。
    /// `None` = 磁盘上根本没有这个文件（首次运行），建状态时按当时的真实值播种。
    pub config_mtime: Mutex<Option<SystemTime>>,
    /// Tauri AppHandle（setup 中填充）；用于向 popup / editor 广播状态事件。
    pub app: OnceLock<AppHandle>,
    /// SSID 监视线程句柄；重启监视或退出时用 `stop()` 停掉旧线程。
    pub watcher: Mutex<Option<WatcherHandle>>,
}

impl AppState {
    /// 由「两份配置文件的位置 + 两份已加载的配置」拼出状态。
    ///
    /// 脚本目录跟着自动化配置文件走（`<配置目录>/scripts`）：`automation::resolve_script` 与
    /// `AllowedScripts` 都按配置目录解释相对路径，两者必须同源，否则通过校验的脚本和
    /// 真正执行的不是同一份。
    fn assembled(
        config_path: PathBuf,
        config: Config,
        settings_path: PathBuf,
        settings: AppConfig,
        warnings: Vec<String>,
    ) -> AppState {
        let scripts_dir = config_path
            .parent()
            .map(|p| p.join("scripts"))
            .unwrap_or_else(|| PathBuf::from("scripts"));
        let mut engine = Engine::new();
        engine.set_warnings(warnings);
        // 用当前 mtime 作初值：否则引擎第一轮会把刚读出来的配置再「重载」一次，
        // 日志里凭空多出一条 Config reloaded，界面上凭空多一次广播。
        let mtime = std::fs::metadata(&config_path).ok().and_then(|m| m.modified().ok());
        AppState {
            config_path,
            config: Mutex::new(config),
            settings_path,
            settings: Mutex::new(settings),
            scripts_dir,
            plat: Platform,
            engine: Mutex::new(engine),
            engine_tx: OnceLock::new(),
            config_mtime: Mutex::new(mtime),
            app: OnceLock::new(),
            watcher: Mutex::new(None),
        }
    }

    /// 加载状态。
    ///
    /// 配置读不出来时**不**回退成「空配置照常跑」：加载失败几乎总是 schema 不符，
    /// 静默跑起来等于当着用户的面什么都不做，而报错能直接告诉他该干嘛。
    pub fn load(
        config_path: PathBuf,
        settings_path: PathBuf,
        settings: AppConfig,
    ) -> Result<AppState, String> {
        let config = Config::load(&config_path).and_then(|c| c.validate().map(|_| c))?;
        let warnings: Vec<String> = config.warnings().iter().map(|w| w.0.clone()).collect();
        Ok(Self::assembled(config_path, config, settings_path, settings, warnings))
    }

    /// 磁盘上**还没有**配置（首次运行）。
    ///
    /// 这与 [`Self::fallback`] 不是一回事：那里是「有一份我们不敢用的配置」，这里是什么
    /// 都还没有，界面上要说的是「去建第一个 Profile」，而不是复述一条 ENOENT。
    pub fn no_config_yet(
        config_path: PathBuf,
        settings_path: PathBuf,
        settings: AppConfig,
    ) -> AppState {
        Self::assembled(
            config_path,
            Config {
                schema: crate::config::SCHEMA,
                ..Default::default()
            },
            settings_path,
            settings,
            vec![i18n::t("app.no_config_hint")],
        )
    }

    /// 配置读不出来时的兜底状态。
    ///
    /// 为什么不能直接退出：托盘应用静默消失与「装完打不开」在用户眼里是同一件事，
    /// 而这里的原因只是磁盘上有一份本版本不认识的配置（schema 不符或结构不合法）。
    /// 跑起来、把错误原文
    /// 放进 `warnings`（设置面板与日志都看得到），才是可操作的结局。
    pub fn fallback(
        config_path: PathBuf,
        settings_path: PathBuf,
        settings: AppConfig,
        error: String,
    ) -> AppState {
        Self::assembled(
            config_path,
            Config {
                schema: crate::config::SCHEMA,
                ..Default::default()
            },
            settings_path,
            settings,
            vec![error],
        )
    }
}

/// 给引擎线程投一条消息。引擎还没起来（启动极早期）时返回 false，调用方忽略即可。
pub fn post(state: &AppState, msg: Msg) -> bool {
    match state.engine_tx.get() {
        Some(tx) => tx.send(msg).is_ok(),
        None => false,
    }
}

/// 显式退出标志：只有「用户要求退出」的入口（面板的退出按钮 / IPC `quit_app`）会置位。
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
/// 事件监听必须看到同一份快照，否则既多付一次代价、又可能两处显示不一致。
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
                        "id": p.id, "name": p.name, "enabled": p.enabled, "quick": p.quick,
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

/// 广播状态：向所有窗口发 `netsense://status`（前端监听到后自行刷新，避免轮询）。
///
/// 载荷只含**主网卡快照 + 引擎视图**：面板的网卡明细另走 `get_interfaces`（那边有自己的
/// TTL 缓存），VPN 段与快速切换区都从那里来。本函数由**引擎线程**调用，采样 `get_status()`
/// 会拉起子进程，所以它必须在后台线程做完 —— 事件本身只是把已经算好的 JSON 发出去。
pub fn publish_status(state: &Arc<AppState>) {
    let Some(app) = state.app.get().cloned() else {
        return;
    };
    let st = state.plat.get_status();
    let _ = app.emit("netsense://status", status_payload(state, &st));
}
