//! 状态栏（托盘）弹窗面板 —— 本项目的主交互入口（三平台一致）。
//!
//! 交互模型：
//!   - 点托盘图标（左键或右键）→ 在图标正下方开合面板；
//!   - 双击托盘图标 → 收起面板，直接打开自动化配置（编辑器）；
//!   - 面板失焦（点到别处）→ 自动收起；
//!   - 托盘没有原生菜单：所有入口（设置 / 日志 / DHCP / 探测 / 退出）都在面板里，
//!     见 `tray.rs` 的说明 —— 同一屏内容不该维护两份。
//!
//! 关键实现点：
//!   1. 面板窗口在 `tauri.conf.json` 里声明为 `visible: false` + 无边框 + 置顶 + 不进任务栏，
//!      启动时创建好、只做显隐，避免每次点击都建窗口（秒开）。
//!   2. 定位用托盘图标的矩形（`TrayIconEvent::Click.rect`），并收敛到显示器工作区内，
//!      防止在屏幕边缘或外接屏上弹到看不见的位置。
//!   3. 失焦隐藏会带来"点图标先触发 blur 收起、随后又 toggle 展开"的抖动，
//!      用 300ms 防抖窗口消除（见 `last_hide`）。

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, PhysicalPosition, Position, WebviewWindow};

/// 面板窗口 label（与 tauri.conf.json 的 windows[].label 对应）。
pub const POPUP_LABEL: &str = "popup";
/// 主编辑器窗口 label（自动化配置）。
pub const MAIN_LABEL: &str = "main";
/// 软件配置窗口 label。
pub const SETTINGS_LABEL: &str = "settings";
/// 日志窗口 label。
pub const LOGS_LABEL: &str = "logs";

/// 失焦收起后忽略托盘点击的时间窗口，避免"点一下反而先关后开"。
const REOPEN_DEBOUNCE: Duration = Duration::from_millis(300);

fn last_hide() -> &'static Mutex<Option<Instant>> {
    static LAST_HIDE: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    LAST_HIDE.get_or_init(|| Mutex::new(None))
}

/// 记录一次"面板已收起"（由失焦事件或 toggle 调用），用于防抖。
pub fn mark_hidden() {
    *last_hide().lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
}

/// 由托盘图标的矩形（物理像素：x, y, w, h）算出「面板锚点」——
/// 图标水平中心、垂直底边。矩形无效（宽高都为 0）时返回 None，调用方退化为兜底位置。
pub fn anchor_center_bottom(x: f64, y: f64, w: f64, h: f64) -> Option<(f64, f64)> {
    if w <= 0.0 && h <= 0.0 {
        return None;
    }
    Some((x + w / 2.0, y + h))
}

/// 切换面板显隐。
pub fn toggle(app: &AppHandle, anchor: Option<(f64, f64)>) {
    let Some(win) = app.get_webview_window(POPUP_LABEL) else {
        crate::log::warn(&crate::i18n::tf(
            "app.window_missing",
            &[("label", POPUP_LABEL)],
        ));
        return;
    };

    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
        mark_hidden();
        return;
    }

    // 刚因失焦收起 → 这次点击多半是"点击穿透"余波，忽略
    if let Some(t) = *last_hide().lock().unwrap_or_else(|e| e.into_inner()) {
        if t.elapsed() < REOPEN_DEBOUNCE {
            return;
        }
    }

    position(&win, anchor);
    let _ = win.show();
    let _ = win.set_focus();
}

/// 展示主编辑器窗口（自动化配置）。
pub fn show_main(app: &AppHandle) {
    show_window(app, MAIN_LABEL);
}

/// 展示软件配置窗口。
pub fn show_settings(app: &AppHandle) {
    show_window(app, SETTINGS_LABEL);
}

/// 展示日志窗口。
pub fn show_logs(app: &AppHandle) {
    show_window(app, LOGS_LABEL);
}

/// 双击托盘图标 = 直接进自动化配置：面板收起，编辑器叫到前台。
///
/// 收起这一步不能省：面板是置顶无边框窗口，让编辑器在它后面开出来，用户看到的还是面板，
/// 只会以为「双击没反应」。
pub fn open_automation(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(POPUP_LABEL) {
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
            mark_hidden();
        }
    }
    show_main(app);
}

/// 把一个已声明的窗口叫到前台。窗口不存在时只记一条 warn：
/// 三个窗口都在 `tauri.conf.json` 里声明，找不到就是配置被改坏了，
/// 而为一个入口崩掉整个进程是拿用户的网络自动化去换一个按钮。
fn show_window(app: &AppHandle, label: &str) {
    match app.get_webview_window(label) {
        Some(w) => {
            let _ = w.show();
            let _ = w.set_focus();
        }
        None => crate::log::warn(&crate::i18n::tf(
            "app.window_missing",
            &[("label", label)],
        )),
    }
}

/// 定位：面板水平居中对齐图标，垂直紧贴图标下方，并收敛到显示器**工作区**内。
fn position(win: &WebviewWindow, anchor: Option<(f64, f64)>) {
    let size = match win.outer_size() {
        Ok(s) => s,
        Err(_) => return,
    };
    let w = size.width as i32;
    let h = size.height as i32;

    let (mut x, mut y) = match anchor {
        Some((ax, ay)) => (ax as i32 - w / 2, ay as i32 + 6),
        // 拿不到图标位置（部分平台/事件不带 rect）→ 左上角兜底
        None => (16, 16),
    };

    // (x, y, w, h) 是该屏上「放得下东西」的那块矩形：Windows 的它是整屏减掉任务栏，
    // macOS 的它是整屏减掉菜单栏。用整屏 `size()` 收敛的话，面板底边会压到任务栏上 ——
    // 图标就长在任务栏里，锚点算出的 y 天然比工作区下缘还低。
    if let Some((wx, wy, ww, wh)) = work_area_at(win, anchor) {
        let (min_x, min_y) = (wx + 8, wy + 8);
        let max_x = wx + ww - w - 8;
        let max_y = wy + wh - h - 8;
        x = x.clamp(min_x, max_x.max(min_x));
        y = y.clamp(min_y, max_y.max(min_y));
    }

    let _ = win.set_position(Position::Physical(PhysicalPosition::new(x, y)));
}

/// 图标所在显示器的**工作区**（物理像素：x, y, w, h）。找不到任何屏幕时返回 None，
/// 调用方就按锚点原样摆放。
///
/// 选屏判据用**整块屏幕**而不是工作区：托盘图标画在任务栏那条带上，那里恰恰不在工作区里，
/// 按工作区找会把「图标在副屏」判成「哪儿都不在」。而 `current_monitor()` 问的是**窗口上次
/// 所在**的屏 —— 用户把鼠标移到副屏点托盘时，它的回答还是上次那块屏，于是面板被拽回错的屏。
fn work_area_at(win: &WebviewWindow, anchor: Option<(f64, f64)>) -> Option<(i32, i32, i32, i32)> {
    let area = |m: &tauri::Monitor| {
        let wa = m.work_area();
        (
            wa.position.x,
            wa.position.y,
            wa.size.width as i32,
            wa.size.height as i32,
        )
    };
    if let Some((ax, ay)) = anchor {
        if let Ok(mons) = win.available_monitors() {
            let (px, py) = (ax as i32, ay as i32);
            if let Some(m) = mons.iter().find(|m| {
                let (mp, ms) = (m.position(), m.size());
                let (mw, mh) = (ms.width as i32, ms.height as i32);
                px >= mp.x && px < mp.x + mw && py >= mp.y && py < mp.y + mh
            }) {
                return Some(area(m));
            }
        }
    }
    win.current_monitor().ok().flatten().as_ref().map(area)
}
