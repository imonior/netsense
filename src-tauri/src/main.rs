//! NetSense 入口：只做**装配**。
//!
//! 装配顺序（每一项都有理由，顺序不能随意换）：
//!
//! 1. 字典（i18n）→ 软件配置 → 日志 + panic hook —— 日志最先起，否则后面的失败没有任何
//!    痕迹；字典在日志之前，因为启动那几行日志本身就是要翻译的；软件配置在两者之间，
//!    因为它决定的正是「用什么语言说」和「留几天」；
//! 2. Windows 的 WebView2 预检 —— 缺运行时则创建窗口必然失败，且 release 版没有控制台；
//! 3. 解析两份配置的路径 → 加载校验自动化配置 → 建 [`AppState`]（读不出来时带着告警继续跑，
//!    见 `state`）；
//! 4. 托盘 + 编辑器窗口 —— 先给用户「应用活着」的信号；
//! 5. [`engine::start`] —— 起引擎线程，之后所有网络变化都由它串行处理；
//!    启动时的首次匹配就发生在引擎的第一轮 pass，这里不再自己 apply 一次。
//!
//! 平台差异全部收敛在 `platform` 模块内：本文件不出现任何具体平台类型或系统命令。

// Windows 上发布版不要弹出附着控制台窗口（debug 版保留，方便看日志）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod appconfig;
mod automation;
mod conditions;
mod config;
mod detection;
mod engine;
mod i18n;
mod ipc;
mod log;
mod network;
mod paths;
mod platform;
mod popup;
mod state;
mod tray;
mod update;
mod win_dialog;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::Manager;

use i18n::Language;
use platform::{priv_channel, platform_name};

fn main() {
    // 0) 字典先于第一条日志。
    //
    //    日志文案是用户看的（也是远程定位时唯一看的），而 `i18n::t` 在字典装好之前
    //    只会把 key 原样返回 —— 所以 `init` 必须排在 `log::init` 前面。
    i18n::init();

    // 0.5) 软件配置：语言与日志保留天数都要在**第一条日志之前**拿到。
    //
    //    这两样都不是日志模块自己的事：留几天由用户说，用什么语言说也由用户说。
    //    读不出来时不终止进程 —— 一份写坏的 `settings.json` 影响的是界面语言，
    //    不该让用户连网络都切不了；但一定要在日志里留一条，否则「我明明设成中文」
    //    就成了一个查不到原因的抱怨。
    let settings_path = paths::settings_path();
    let (settings, settings_error) = match appconfig::AppConfig::load(&settings_path) {
        Ok(s) => (s, None),
        Err(e) => (appconfig::AppConfig::default(), Some(e)),
    };
    if let Some(l) = &settings.language {
        i18n::set_language(Language::from_code(l));
    }

    // 1) 日志 + panic hook 必须排在其余装配之前。
    //
    //    这一步一旦排到 setup() 里，setup 之前的失败（窗口 / WebView 创建）就没有
    //    日志、也没有任何可见提示 —— release 版在 Windows 上是 GUI 子系统（无控制台），
    //    最终表现为「点了一下，什么都没发生」。这类「装完无法运行」之所以迟迟定位不到，
    //    根源就在这里：不是没有原因，而是原因无处可见。
    //
    //    目录取**用户级日志目录**（见 [`paths`]）：程序目录在 perMachine 安装与 macOS
    //    的 .app 包里都可能是只读或被签名保护的，写进去等于改自己的安装包。
    //    用户目录万一也不可写时（漫游配置被禁用之类），由 log::init 退到系统临时目录 ——
    //    日志宁可在临时目录里，也不能没有。
    let preferred_log_dir = paths::user_log_dir().unwrap_or_else(|| PathBuf::from("logs"));
    log::init(
        &preferred_log_dir,
        true,
        settings.log_retention_days,
    );
    if let Some(e) = &settings_error {
        log::error(&i18n::tf(
            "app.settings_failed",
            &[("path", &settings_path.display().to_string()), ("error", e)],
        ));
    }
    log::info(&i18n::tf(
        "app.startup",
        &[
            ("version", env!("CARGO_PKG_VERSION")),
            ("backend", platform_name()),
            ("dir", &log::log_dir().display().to_string()),
        ],
    ));

    // 2) Windows：WebView2 运行时缺失 → 给出可操作的下载提示，而不是静默退出。
    //
    //    安装包不再内嵌运行时（bundle.windows.webviewInstallMode = skip），Windows
    //    setup.exe 从 ~218MB 降到几 MB；代价是本机缺失时必须由我们给出明确指引 ——
    //    否则 Tauri 创建 WebView 失败、进程静默结束，用户只会看到「装完打不开」。
    #[cfg(target_os = "windows")]
    if !platform::webview2_available() {
        log::error(&i18n::t("app.webview2_missing"));
        win_dialog::fatal(
            &i18n::t("dlg.webview2_title"),
            &i18n::tf(
                "dlg.webview2_body",
                &[
                    ("bootstrapper", platform::WEBVIEW2_BOOTSTRAPPER_URL),
                    ("download", platform::WEBVIEW2_DOWNLOAD_URL),
                ],
            ),
        );
        std::process::exit(2);
    }

    tauri::Builder::default()
        .setup(move |app| {
            // 配置文件：用户目录里那份优先，其次才是同目录那份（见 `paths::config_path`）
            let config_path = paths::config_path();
            // 软件配置在主流程里已经读过（语言、日志保留要用），这里只把那份结果带进状态
            let shared = Arc::new(match state::AppState::load(
                config_path.clone(),
                settings_path.clone(),
                settings.clone(),
            ) {
                Ok(s) => {
                    // 「加载成功」只在真的成功时打。此前这行是无条件执行的，于是
                    // 一份根本没读起来的配置也能在日志里留下 "Config loaded from …"，
                    // 紧跟着一串 "Config invalid" —— 排障时最先看到的反而是假信号。
                    log::info(&i18n::tf(
                        "app.config_loaded",
                        &[("path", &config_path.display().to_string())],
                    ));
                    s
                }
                Err(e) => {
                    if !config_path.is_file() {
                        // 首次运行：磁盘上还没有配置。这不是故障 —— 界面照常起来，
                        // 由用户在编辑器里建第一个 Profile，第一次保存会写出这个文件。
                        log::warn(&i18n::tf(
                            "app.config_absent",
                            &[("path", &config_path.display().to_string())],
                        ));
                        state::AppState::no_config_yet(
                            config_path.clone(),
                            settings_path.clone(),
                            settings.clone(),
                        )
                    } else {
                        // 带着错误继续跑：配置是本版本拒绝自动迁移的旧文件时，
                        // 用户仍然要能看到界面、看到该改什么 —— 静默退出只会变成「程序坏了」。
                        log::error(&i18n::tf("app.config_failed", &[("error", &e)]));
                        // 正文直接用 e：那已经是字典里取出的本地化句子（见 cfg.* / app.*），
                        // 再包一层只会把两种语言缝在同一句里。
                        win_dialog::warn(&i18n::t("dlg.config_unreadable_title"), &e);
                        state::AppState::fallback(
                            config_path.clone(),
                            settings_path.clone(),
                            settings.clone(),
                            e,
                        )
                    }
                }
            });

            // i18n key parity 校验（仅告警，不阻断启动）。
            let (missing, extra, empty) = i18n::check_parity();
            if !(missing.is_empty() && extra.is_empty() && empty.is_empty()) {
                // i18n-exempt: 这条不翻译。它给的是 `Debug` 输出的 key 清单，只有对着
                // 字典才读得懂 —— 翻成任何一种语言，读的仍然是那批英文标识符。
                log::error(&format!(
                    "i18n parity mismatch: missing={:?} extra={:?} empty={:?}",
                    missing, extra, empty
                ));
            }

            // 平台标识打进日志，便于跨平台排障时一眼确认跑的是哪个后端
            log::info(&i18n::tf(
                "app.backend",
                &[
                    ("version", env!("CARGO_PKG_VERSION")),
                    ("backend", platform_name()),
                    ("channel", priv_channel().code()),
                ],
            ));

            app.manage(shared.clone());

            // 保存 AppHandle 供后续事件广播 / 面板刷新使用
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
            if let Err(e) = tray::build_tray(app) {
                log::error(&i18n::tf("app.tray_failed", &[("error", &e.to_string())]));
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
            // 关掉编辑器 / 设置 / 日志窗口 = 收回菜单栏常驻，而不是退出应用
            tauri::WindowEvent::CloseRequested { api, .. }
                if window.label() == popup::MAIN_LABEL
                    || window.label() == popup::SETTINGS_LABEL
                    || window.label() == popup::LOGS_LABEL =>
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
            ipc::get_printers,
            ipc::open_logs,
            ipc::get_log_files,
            ipc::read_log,
            ipc::open_log_viewer,
            ipc::close_log_viewer,
            ipc::set_language,
            ipc::get_app_settings,
            ipc::set_autostart,
            ipc::set_log_retention,
            ipc::open_config_folder,
            ipc::open_settings,
            ipc::close_settings,
            ipc::get_strings,
            ipc::get_language,
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
            log::error(&i18n::tf("app.create_failed", &[("error", &e.to_string())]));
            win_dialog::fatal(
                &i18n::t("dlg.fatal_title"),
                &i18n::tf(
                    "dlg.create_failed_body",
                    &[
                        ("error", &e.to_string()),
                        ("dir", &log::log_dir().display().to_string()),
                    ],
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
            tauri::RunEvent::Exit => log::info(&i18n::t("app.exited")),
            _ => {}
        });
}
