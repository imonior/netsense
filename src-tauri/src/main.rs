//! NetSense 入口：只做**装配**。
//!
//! 装配顺序（每一项都有理由，顺序不能随意换）：
//!
//! 1. 日志 + panic hook —— 必须最先，否则后面的失败没有任何痕迹；
//! 2. Windows 的 WebView2 预检 —— 缺运行时则创建窗口必然失败，且 release 版没有控制台；
//! 3. 解析配置路径 → 加载校验 → 建 [`AppState`]（读不出来时带着告警继续跑，见 `state`）；
//! 4. 托盘 + 编辑器窗口 —— 先给用户「应用活着」的信号；
//! 5. [`engine::start`] —— 起引擎线程，之后所有网络变化都由它串行处理；
//!    启动时的首次匹配就发生在引擎的第一轮 pass，这里不再自己 apply 一次。
//!
//! 平台差异全部收敛在 `platform` 模块内：本文件不出现任何具体平台类型或系统命令。

// Windows 上发布版不要弹出附着控制台窗口（debug 版保留，方便看日志）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod automation;
mod conditions;
mod config;
mod detection;
mod engine;
mod i18n;
mod ipc;
mod log;
mod network;
mod platform;
mod popup;
mod state;
mod tray;
mod update;
mod win_dialog;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::Manager;

use i18n::Language;
use platform::{priv_channel, platform_name};

fn main() {
    // 1) 日志 + panic hook 必须**最先**装好。
    //
    //    这一步一旦排到 setup() 里，setup 之前的失败（窗口 / WebView 创建）就没有
    //    日志、也没有任何可见提示 —— release 版在 Windows 上是 GUI 子系统（无控制台），
    //    最终表现为「点了一下，什么都没发生」。这类「装完无法运行」之所以迟迟定位不到，
    //    根源就在这里：不是没有原因，而是原因无处可见。
    //
    //    首选 **exe 同级** logs（便携运行时就地写日志）；不可写时由 log::init 回退到
    //    用户目录（%LOCALAPPDATA% / ~/Library/Logs / ~/.local/state）或临时目录。
    //    这里刻意不用 config_path.parent()：config.json 未随包发布时 config_path 会退化
    //    成相对路径，日志便落到 CWD 下 —— perMachine 安装的 CWD 常在 Program Files，
    //    普通用户写不进去，日志会彻底消失。
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let preferred_log_dir = exe_dir
        .unwrap_or_else(|| PathBuf::from("."))
        .join("logs");
    log::init(&preferred_log_dir, true);
    log::info(&format!(
        "NetSense v{} 启动（平台后端: {}，日志目录: {}）",
        env!("CARGO_PKG_VERSION"),
        platform_name(),
        log::log_dir().display()
    ));

    // 2) Windows：WebView2 运行时缺失 → 给出可操作的下载提示，而不是静默退出。
    //
    //    安装包不再内嵌运行时（bundle.windows.webviewInstallMode = skip），Windows
    //    setup.exe 从 ~218MB 降到几 MB；代价是本机缺失时必须由我们给出明确指引 ——
    //    否则 Tauri 创建 WebView 失败、进程静默结束，用户只会看到「装完打不开」。
    #[cfg(target_os = "windows")]
    if !platform::webview2_available() {
        log::error("未检测到 WebView2 运行时，无法创建界面");
        win_dialog::fatal(
            "NetSense 缺少运行组件",
            &format!(
                "NetSense 需要 Microsoft Edge WebView2 运行时才能显示界面，当前系统未检测到该组件。\n\n\
                 请安装后重新启动 NetSense（下载 Evergreen Bootstrapper 即可，安装很快）：\n  {}\n\n\
                 下载页（需要其它版本或离线安装包时）：\n  {}\n\n\
                 提示：Windows 10 1803 及以上、Windows 11 通常已自带该组件。\
                 若你看到此提示，多为精简版系统，或该组件被安全软件移除。",
                platform::WEBVIEW2_BOOTSTRAPPER_URL,
                platform::WEBVIEW2_DOWNLOAD_URL
            ),
        );
        std::process::exit(2);
    }

    tauri::Builder::default()
        .setup(|app| {
            // 配置路径：与可执行文件同目录的 config.json，回退到工作目录
            let config_path = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("config.json")))
                .filter(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("config.json"));

            // i18n（默认 en，随后应用配置语言偏好）
            i18n::init();

            let shared = Arc::new(match state::AppState::load(config_path.clone()) {
                Ok(s) => s,
                Err(e) => {
                    // 带着错误继续跑：配置是本版本拒绝自动迁移的旧文件时，
                    // 用户仍然要能看到界面、看到该改什么 —— 静默退出只会变成「程序坏了」。
                    log::error(&format!("配置加载失败: {}", e));
                    win_dialog::warn("NetSense 配置无法读取", &e);
                    state::AppState::fallback(config_path.clone(), e)
                }
            });

            // 应用配置文件里的语言偏好
            {
                let cfg = shared.config.lock().unwrap_or_else(|e| e.into_inner());
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
                platform_name(),
                priv_channel().code()
            ));
            log::info(&i18n::tf(
                "app.config_loaded",
                &[("path", &config_path.display().to_string())],
            ));

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

            // 托盘创建失败不该让整个应用陪葬：托盘只是交互入口，而 setup 返回 Err 会让
            // run() 返回 Err → 末尾 .expect() panic → release 版静默退出（无控制台）。
            // 记一条 error 后继续，至少编辑器窗口还能打开，日志里也能看到原因。
            if let Err(e) = tray::build_tray(app, &shared) {
                log::error(&format!(
                    "托盘创建失败（应用继续运行，请检查图标与托盘菜单配置）: {}",
                    e
                ));
            }

            // 启动即显示**编辑器窗口** —— 唯一无歧义的「应用已启动」信号。
            //
            // 反面教材（「安装后无法运行」最难排查的那种形态）：这里如果弹无边框的
            // popup 面板，它会被本文件 on_window_event 里「失焦即收起」规则在毫秒级
            // 收回；再叠加 popup 的 skipTaskbar + main 的 visible:false + Windows 托盘图标
            // 默认收进“隐藏的图标”溢出区 —— 用户只看到“闪一下然后什么都没有”，判定为闪退。
            //
            // 关闭该窗口走 CloseRequested → prevent_close + hide：收回托盘常驻，不退出应用。
            popup::show_main(app.handle());

            // 起引擎线程：SSID 监视、配置热重载、采样与匹配全部由它串行驱动。
            engine::start(shared.clone());

            // 启动走完了。此后发生的 panic 不再弹模态框 —— 不该为一个仍在工作的
            // 应用弹阻塞对话框（那会把「一个后台线程出错」升级成「用户以为程序坏了」）。
            state::APP_STARTED.store(true, Ordering::SeqCst);
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
            ipc::get_engine_status,
            ipc::get_interfaces,
            ipc::get_config,
            ipc::save_profile,
            ipc::save_global,
            ipc::delete_profile,
            ipc::apply_profile,
            ipc::force_dhcp,
            ipc::probe_network,
            ipc::get_networks,
            ipc::open_logs,
            ipc::set_language,
            ipc::get_strings,
            ipc::open_editor,
            ipc::close_editor,
            ipc::quit_app,
            ipc::check_update,
            ipc::run_update,
            ipc::open_url
        ])
        .build(tauri::generate_context!())
        .unwrap_or_else(|e| {
            // 走到这里 = 连应用都建不起来（窗口 / WebView 创建失败等）。release 版没有
            // 控制台，不弹窗就又是「装完打不开」；日志里留下完整错误供远程定位。
            log::error(&format!("创建应用失败: {}", e));
            win_dialog::fatal(
                "NetSense 启动失败",
                &format!(
                    "NetSense 无法创建应用窗口，进程即将退出。\n\n错误: {}\n\n日志目录:\n{}",
                    e,
                    log::log_dir().display()
                ),
            );
            std::process::exit(1);
        })
        .run(|_app, event| match event {
            // 托盘型应用的经典坑：所有窗口隐藏/销毁后 Tauri 也会发出 ExitRequested。
            // 只放行「用户主动退出」（QUITTING 已置位）；其余一律阻止 —— 否则用户关掉
            // 编辑器窗口就等于杀掉整个后台服务，“收回托盘常驻”根本无从谈起。
            tauri::RunEvent::ExitRequested { api, .. } => {
                if !state::QUITTING.load(Ordering::SeqCst) {
                    api.prevent_exit();
                }
            }
            // 事件循环真的结束了（正常只会在用户显式退出时走到）。
            // 记一条，让日志能区分「用户自己退的」和「它自己死了」。
            tauri::RunEvent::Exit => log::info("事件循环结束，进程正常退出"),
            _ => {}
        });
}
