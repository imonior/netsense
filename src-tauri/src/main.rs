//! NetSense 入口：Tauri v2 外壳 + 系统托盘 + SSID 监视 + 配置热加载 + 配置热重载。
//!
//! 运行期数据流：启动即按当前网络身份匹配一次 → 应用 Profile + on_apply；
//! SSID 变化 → 重新匹配 → 应用；配置文件被外部修改 → 热重载线程发现 mtime 变化 →
//! 重新加载+校验 → 重新匹配 → 仅重设网络+健康度（run_on_apply=false，避免重复触发自动化）。
//!
//! 平台差异全部收敛在 `platform` 模块内：本文件不出现任何具体平台类型或系统命令。

// Windows 上发布版不要弹出附着控制台窗口（debug 版保留，方便看日志）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod platform;
mod core;
mod ipc;
mod log;
mod i18n;
mod popup;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

use config::Config;
// `Platform` 是编译期选定的平台实现（macOS / Windows / Linux 之一），
// 上层代码一律只依赖它，不出现任何具体平台类型名。
use platform::{Platform, NetworkPlatform, WatcherHandle};
// 注意：本项目有本地模块 `core`，与标准库 sysroot crate `core` 同名。
// Rust 2018 uniform paths 下 `use core::x` 会触发 E0659 歧义错误，因此必须写成 `crate::core::x`。
use crate::core::automation::{self, AllowedScripts};
use crate::core::health::HealthMonitor;
use crate::core::matcher::{select_profile, NetworkIdentity};
use i18n::Language;

/// 全局共享状态（被托盘菜单、IPC 命令、SSID watcher、热重载线程共同访问）。
pub struct AppState {
    pub config_path: std::path::PathBuf,
    pub config: Mutex<Config>,
    pub scripts_dir: std::path::PathBuf,
    pub plat: Platform,
    pub current: Mutex<Option<String>>,
    pub health_stop: Mutex<Arc<AtomicBool>>,
    /// 上次已应用的 (profile 名 + 内容指纹)。用于幂等保护：
    /// 若内容未变则不重复下发网络设置，避免无关的弹窗/提权。
    pub applied_fp: Mutex<Option<String>>,
    /// Tauri AppHandle（setup 中填充）；用于向 popup / editor 广播状态事件。
    pub app: OnceLock<AppHandle>,
    /// SSID 监视线程句柄；重启监视或退出时用 `stop()` 停掉旧线程。
    pub watcher: Mutex<Option<WatcherHandle>>,
}

impl AppState {
    fn load(config_path: std::path::PathBuf) -> AppState {
        let config = Config::load(&config_path).unwrap_or_else(|e| {
            log::warn(&format!("配置加载失败 ({}); 使用空配置", e));
            Config::default()
        });
        let scripts_dir = config_path
            .parent()
            .map(|p| p.join("scripts"))
            .unwrap_or_else(|| std::path::PathBuf::from("scripts"));
        AppState {
            config_path,
            config: Mutex::new(config),
            scripts_dir,
            plat: Platform,
            current: Mutex::new(None),
            health_stop: Mutex::new(Arc::new(AtomicBool::new(false))),
            applied_fp: Mutex::new(None),
            app: OnceLock::new(),
            watcher: Mutex::new(None),
        }
    }
}

/// 应用一个 profile。
/// - `run_on_apply=true`：跨网络切换 / 强制应用时执行 `on_apply` 自动化（开软件、跑脚本、设路由）。
/// - `run_on_apply=false`：配置热重载导致的「同 profile 更新」，仅重设网络+健康度，
///   不重复触发 `on_apply`（避免重复开软件/跑脚本）；健康度回落时的 `on_revert` 始终执行。
pub fn apply_named(state: &Arc<AppState>, name: &str, run_on_apply: bool) {
    let profile = {
        let cfg = state.config.lock().unwrap();
        match cfg.profiles.get(name) {
            Some(p) => p.clone(),
            None => {
                log::warn(&format!("未找到 profile: {}", name));
                return;
            }
        }
    };

    // 幂等保护：内容与上次完全一致且非强制 → 跳过下发。
    // 典型场景：只改了 language 等与网络无关的字段触发热重载，不应再次弹提权框。
    let fp = format!(
        "{}|{}",
        name,
        serde_json::to_string(&profile).unwrap_or_default()
    );
    if !run_on_apply {
        let last = state.applied_fp.lock().unwrap();
        if last.as_deref() == Some(fp.as_str()) {
            log::debug(&format!("profile '{}' 内容未变，跳过网络重设", name));
            return;
        }
    }

    // 停止上一个健康度 loop
    let new_stop = {
        let mut stop = state.health_stop.lock().unwrap();
        stop.store(true, Ordering::SeqCst);
        let s = Arc::new(AtomicBool::new(false));
        *stop = s.clone();
        s
    };

    // 健康度（连续失败且 fallback 开启 → DHCP 保底 + on_revert）
    if let Some(h) = &profile.health {
        if h.enabled {
            let fb_state = state.clone();
            let fb_profile = profile.clone();
            HealthMonitor::start(Platform, h, new_stop, move || {
                let _ = fb_state.plat.set_dhcp();
                // 已回落到 DHCP，当前状态已不等于该 profile，指纹作废：
                // 否则下次热重载会因「内容未变」跳过重设，卡在 DHCP 上。
                *fb_state.applied_fp.lock().unwrap() = None;
                log::warn(&i18n::t("notify.fallback"));
                if let Some(auto) = &fb_profile.automation {
                    if auto.enabled {
                        let allowed = AllowedScripts {
                            scripts_dir: fb_state.scripts_dir.clone(),
                            explicit: auto.allowed_scripts.clone().unwrap_or_default(),
                        };
                        let _ = automation::run_actions(&fb_state.plat, &auto.on_revert, &allowed);
                    }
                }
            });
        }
    }

    // 应用网络 + （可选）on_apply 自动化
    let mut results: Vec<Result<(), String>> = Vec::new();
    if let Err(e) = state.plat.apply_profile(&profile) {
        results.push(Err(format!("应用 profile '{}' 失败: {}", name, e)));
    }
    if run_on_apply {
        if let Some(auto) = &profile.automation {
            if auto.enabled {
                let allowed = AllowedScripts {
                    scripts_dir: state.scripts_dir.clone(),
                    explicit: auto.allowed_scripts.clone().unwrap_or_default(),
                };
                results.extend(automation::run_actions(
                    &state.plat,
                    &auto.on_apply,
                    &allowed,
                ));
            }
        }
    }
    for r in results {
        if let Err(e) = r {
            log::error(&e);
        }
    }
    *state.current.lock().unwrap() = Some(name.to_string());
    *state.applied_fp.lock().unwrap() = Some(fp);

    // 广播给 popup / editor，并刷新托盘菜单里的「当前配置」
    publish_status(state);
}

/// 组装对外暴露的状态载荷（IPC `get_status` 与事件广播共用同一形状）。
pub fn status_payload(state: &Arc<AppState>) -> serde_json::Value {
    let st = state.plat.get_status();
    let cfg = state.config.lock().unwrap();
    serde_json::json!({
        "status": st,
        "current": state.current.lock().unwrap().clone(),
        "profiles": cfg.profiles.keys().cloned().collect::<Vec<String>>(),
        "language": i18n::current().code(),
        "priv": platform::priv_channel().code(),
    })
}

/// 广播状态：向所有窗口发 `netsense://status`（popup / editor 监听到后自行刷新，避免前端轮询），
/// 并把托盘菜单文案刷新到最新（当前 profile / 权限通道）。
///
/// 注意：本函数会被 SSID 监视线程、热重载线程等**后台线程**调用，
/// 而托盘菜单更新会触及 AppKit，因此必须经 `run_on_main_thread` 回到主线程执行。
pub fn publish_status(state: &Arc<AppState>) {
    let Some(app) = state.app.get().cloned() else {
        return;
    };
    let _ = app.emit("netsense://status", status_payload(state));

    let state_for_menu = state.clone();
    let app_for_menu = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(tray) = app_for_menu.tray_by_id("main") {
            if let Ok(menu) = tray_menu(&app_for_menu, &state_for_menu) {
                let _ = tray.set_menu(Some(menu));
            }
        }
    });
}

/// 托盘右键菜单：当前 profile 摘要 + 显示编辑器 + 打开日志 + 权限通道 + 退出。
fn tray_menu<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
) -> tauri::Result<tauri::menu::Menu<R>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};

    let name = state
        .current
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| "__DEFAULT__".to_string());
    // 提权通道文案（跨平台统一语义：Direct = 免密/已提权，Prompt = 每次授权）
    let priv_key = match platform::priv_channel() {
        platform::PrivChannel::Direct => "tray.priv_direct",
        platform::PrivChannel::Prompt => "tray.priv_prompt",
    };

    // 前两项为只读信息行（enabled=false），让菜单本身就能回答问题：现在是哪套配置、提权走哪条通道
    let header = MenuItem::with_id(
        app,
        "info",
        i18n::tf("tray.current", &[("name", name.as_str())]).as_str(),
        false,
        None::<&str>,
    )?;
    let priv_item = MenuItem::with_id(
        app,
        "priv",
        format!("{}: {}", i18n::t("tray.priv"), i18n::t(priv_key)).as_str(),
        false,
        None::<&str>,
    )?;
    let show = MenuItem::with_id(app, "show", i18n::t("tray.show").as_str(), true, None::<&str>)?;
    let logs = MenuItem::with_id(app, "logs", i18n::t("tray.open_logs").as_str(), true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", i18n::t("tray.quit").as_str(), true, None::<&str>)?;

    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    Menu::with_items(
        app,
        &[&header, &priv_item, &sep1, &show, &logs, &sep2, &quit],
    )
}

/// 用系统默认程序打开日志目录（三平台各自映射，见 `platform::open_path`）。
fn open_logs_dir(app: &AppHandle) {
    let _ = app;
    let dir = log::log_dir();
    if let Err(e) = platform::open_path(&dir.display().to_string()) {
        log::error(&format!("打开日志目录失败 {}: {}", dir.display(), e));
    }
}

fn build_tray(app: &tauri::App, state: &Arc<AppState>) -> tauri::Result<()> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let handle = app.handle().clone();
    let menu = tray_menu(&handle, state)?;

    // 托盘图标：优先取 bundle 内置图标；缺失时降级为无图标托盘并告警，不阻断启动。
    let icon = app.default_window_icon().cloned();
    if icon.is_none() {
        log::warn("未取到窗口图标，将以无图标方式创建托盘（见 DEVELOPMENT.md 生成 icons）");
    }

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip(i18n::t("tray.title").as_str())
        // 左键留给弹窗面板；右键（macOS 上按住/右键）才出原生菜单
        .show_menu_on_left_click(false);
    if let Some(img) = icon {
        builder = builder.icon(img);
    }

    builder
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "quit" => app.exit(0),
            "show" => popup::show_main(app),
            "logs" => open_logs_dir(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 左键点击 → 在图标正下方弹出/收起面板
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = &event
            {
                // 就地折叠成 f64 像素，避免依赖 tray::Rect 的具体导出路径
                let (x, y) = match rect.position {
                    tauri::Position::Physical(p) => (p.x as f64, p.y as f64),
                    tauri::Position::Logical(p) => (p.x, p.y),
                };
                let (w, h) = match rect.size {
                    tauri::Size::Physical(s) => (s.width as f64, s.height as f64),
                    tauri::Size::Logical(s) => (s.width, s.height),
                };
                popup::toggle(tray.app_handle(), popup::anchor_center_bottom(x, y, w, h));
            }
        })
        .build(app)?;
    Ok(())
}

/// 解析当前网络身份并匹配 profile 名（不应用）。
fn resolve_current_name(state: &Arc<AppState>) -> String {
    let id = NetworkIdentity {
        ssid: state.plat.get_current_ssid(),
        gateway_mac: state.plat.resolve_gateway_mac(),
        bssid: state.plat.resolve_bssid(),
    };
    let cfg = state.config.lock().unwrap();
    match select_profile(&cfg, &id) {
        Some((n, _)) => n,
        None => "__DEFAULT__".to_string(),
    }
}

/// 配置热重载：每 3 秒轮询 config 文件 mtime，变化则重新加载 + 校验 + 重新匹配应用。
fn reload_watch_loop(state: Arc<AppState>) {
    let path = state.config_path.clone();
    let mut last = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
    loop {
        std::thread::sleep(Duration::from_secs(3));
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = meta.modified().ok();
        if modified == last {
            continue;
        }
        last = modified;

        match Config::load(&path).and_then(|c| c.validate().map(|_| c)) {
            Ok(new_cfg) => {
                if let Some(l) = &new_cfg.language {
                    i18n::set_language(Language::from_code(l));
                }
                *state.config.lock().unwrap() = new_cfg;
                let name = resolve_current_name(&state);
                // 热重载：仅重设网络 + 健康度，避免重复触发 on_apply 自动化
                apply_named(&state, &name, false);
                log::info(&i18n::tf("notify.config_reloaded", &[("name", name.as_str())]));
            }
            Err(e) => {
                log::error(&i18n::tf("notify.config_invalid", &[("error", &e)]));
                log::error("配置热重载失败，保留旧配置");
            }
        }
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            // 配置路径：与可执行文件同目录的 config.json，回退到工作目录
            let config_path = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("config.json")))
                .filter(|p| p.exists())
                .unwrap_or_else(|| std::path::PathBuf::from("config.json"));

            // 1) 日志（先于配置加载，确保加载失败也有记录）；开发期回显 stderr
            let log_dir = config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("logs");
            log::init(&log_dir, true);

            // 2) i18n（默认 en，随后应用配置语言偏好）
            i18n::init();

            let shared = Arc::new(AppState::load(config_path.clone()));

            // 应用配置文件里的语言偏好
            {
                let cfg = shared.config.lock().unwrap();
                if let Some(l) = &cfg.language {
                    i18n::set_language(Language::from_code(l));
                }
            }

            // i18n key parity 校验（仅告警，不阻断启动）
            let (missing, extra, empty) = i18n::check_parity();
            if !(missing.is_empty() && extra.is_empty() && empty.is_empty()) {
                log::error(&format!(
                    "i18n parity 异常: missing={:?} extra={:?} empty={:?}",
                    missing, extra, empty
                ));
            }

            // 平台标识打进日志，便于跨平台排障时一眼确认跑的是哪个后端
            log::info(&format!(
                "NetSense v{} 平台后端: {} (提权通道: {})",
                env!("CARGO_PKG_VERSION"),
                platform::platform_name(),
                platform::priv_channel().code()
            ));

            let path_str = config_path.display().to_string();
            log::info(&i18n::tf("app.config_loaded", &[("path", path_str.as_str())]));

            app.manage(shared.clone());

            // 保存 AppHandle 供后续事件广播 / 托盘菜单刷新使用
            let _ = shared.app.set(app.handle().clone());

            // 菜单栏应用（macOS）：不占 Dock，只有状态栏图标 + 弹窗面板
            #[cfg(target_os = "macos")]
            {
                let _ = app
                    .handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            build_tray(app, &shared).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;

            // 首次应用（启动即按当前网络身份匹配一次）
            {
                let name = resolve_current_name(&shared);
                apply_named(&shared, &name, true);
                log::info(&i18n::tf("notify.applied", &[("name", name.as_str())]));
            }

            // 起 SSID 监视（捕获 Arc clone，shared 仍可用于后续热重载线程）
            let ssid_state = shared.clone();
            let handle = shared.plat.watch_ssid(Box::new(move |_ssid: Option<String>| {
                let name = resolve_current_name(&ssid_state);
                apply_named(&ssid_state, &name, true);
                log::info(&i18n::tf("notify.applied", &[("name", name.as_str())]));
            }));
            *shared.watcher.lock().unwrap() = Some(handle);

            // 起配置热重载线程
            {
                let reload_state = shared.clone();
                std::thread::spawn(move || reload_watch_loop(reload_state));
            }

            log::info(&i18n::t("app.started"));

            Ok(())
        })
        .on_window_event(|window, event| match event {
            // 面板失焦（点到别处）→ 自动收起；mark_hidden 记录时间做防抖
            tauri::WindowEvent::Focused(false) if window.label() == popup::POPUP_LABEL => {
                let _ = window.hide();
                popup::mark_hidden();
            }
            // 关掉编辑器窗口 = 收回菜单栏常驻，而不是退出应用
            tauri::WindowEvent::CloseRequested { api, .. }
                if window.label() == popup::MAIN_LABEL =>
            {
                api.prevent_close();
                let _ = window.hide();
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            ipc::get_status,
            ipc::get_config,
            ipc::save_scene,
            ipc::delete_profile,
            ipc::force_apply,
            ipc::get_networks,
            ipc::set_language,
            ipc::get_strings,
            ipc::open_editor,
            ipc::close_editor,
            ipc::quit_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running NetSense");
}
