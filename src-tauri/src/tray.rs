//! 托盘图标。
//!
//! 从 `main.rs` 拆出来的原因是职责而不是行数：托盘是**展示层**，它只负责「图标 + 点开
//! 面板」，不参与任何判定或执行。留在入口文件里时，「图标行为」和「点菜单之后干什么」
//! 混在一起，很容易长出「回调里直接提权下发配置」这种把主线程占死的写法。
//!
//! ## 这里没有菜单，只有图标
//!
//! 原生右键菜单能放的东西（当前网络明细、VPN 段、提权通道、设置 / 日志 / DHCP / 探测 /
//! 退出）全是面板的**子集**：同一屏内容维护在两处，五种语言里每句文案都要漂移两次，而且
//! 菜单要在主线程重建（`run_on_main_thread`），面板却在 WebView 里刷新，两处永远对不齐。
//! 所以这里只留图标：左键、右键都做同一件事 —— 在图标下方开合 [`crate::popup`]。
//!
//! 顺带解决两个观感问题：把「SSID 读不到」和「每次都要授权」并排放在第一行，看起来像
//! 「读 SSID 需要授权」（实际是**写**配置需要提权，两件事）；提权通道这种排障信息待在
//! 设置里，不占用第一眼。
//!
//! ## 语言切换后要重设 tooltip
//!
//! tooltip 是 `build_tray` 里一次性求值的字符串 —— 它不是模板，建好之后字典再怎么换都
//! 与它无关。所以 `set_language` 必须调 [`refresh_tooltip`]，否则切换语言后图标提示会停在
//! 旧语言（面板里的文案换了，托盘自己却没换）。

use crate::i18n;
use crate::log;
use crate::popup;

/// 托盘图标的 id —— `build_tray` 建它时用这个名字，[`refresh_tooltip`] 按它找回。
const TRAY_ID: &str = "main";

/// 建托盘图标。`main.rs` 的 `setup` 里调用，失败只记日志、不阻断启动
/// （托盘只是交互入口，面板窗口仍然可以打开）。
pub fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    // 托盘图标：优先取 bundle 内置图标；缺失时降级为无图标托盘并告警，不阻断启动。
    let icon = app.default_window_icon().cloned();
    if icon.is_none() {
        log::warn(&i18n::t("app.icon_missing"));
    }

    let mut builder = TrayIconBuilder::with_id(TRAY_ID).tooltip(i18n::t("tray.title").as_str());
    if let Some(img) = icon {
        builder = builder.icon(img);
    }

    builder
        .on_tray_icon_event(move |tray, event| {
            // 任意按钮的「抬起」都开合面板：没有菜单，就没有「左键面板 / 右键菜单」这套
            // 平台差异（macOS 上右键本来只会等菜单，而这里没有菜单可等）。
            if let TrayIconEvent::Click {
                button: MouseButton::Left | MouseButton::Right,
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

/// 按当前字典重设托盘 tooltip，由 `ipc::set_language` 在换语言后调用。
///
/// 只重设 tooltip，不重建托盘：图标和事件回调都与语言无关，而重建会把已注册的回调一起
/// 丢掉，图标还会闪一下。找不到 id 时静默返回 —— 那是 `build_tray` 当初就失败了的情形，
/// 用户面前已经没有托盘，语言切换本身仍然成功。
///
/// Linux 的原生托盘不显示 tooltip（平台限制），这里调用成功与否都不影响其余四语界面。
pub fn refresh_tooltip(app: &tauri::AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let text = i18n::t("tray.title");
    if let Err(e) = tray.set_tooltip(Some(text.as_str())) {
        log::warn(&i18n::tf("app.tray_tooltip_failed", &[("error", &e.to_string())]));
    }
}
