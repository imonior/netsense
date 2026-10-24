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

use tauri::{AppHandle, LogicalSize, Manager, PhysicalPosition, Position, Size, WebviewWindow};

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

/// 最近一次开合面板时用的锚点（托盘图标中心下方）。
///
/// 自动化触发的「把面板叫出来」不在托盘事件里，拿不到当次图标矩形，用上一次的那个 ——
/// 面板出现的位置与用户手点托盘时一致。从没点过托盘（`None`）时退化为左上角。
fn last_anchor() -> &'static Mutex<Option<(f64, f64)>> {
    static LAST: OnceLock<Mutex<Option<(f64, f64)>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

/// 记下本次开合面板的锚点，供 [`show`] 复用。
pub fn note_anchor(anchor: Option<(f64, f64)>) {
    *last_anchor().lock().unwrap_or_else(|e| e.into_inner()) = anchor;
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
    note_anchor(anchor);
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

/// 把面板叫出来（不切换）：自动化刚应用了一个配置时，让用户看见弹窗。
///
/// 与 [`toggle`] 的分工：toggle 是用户的显式开合，这个只负责「该被看见的时候它在」。
/// 已可见时什么都不做 —— 不重摆位置：用户可能刚把它拖到别处，拖到一半被拽回去比不摆更烦。
/// 也不抢焦点：面板本身置顶，露出来就够了；从用户手里抢焦点是托盘常驻进程最惹人烦的那件事。
/// 锚点用上一次点托盘时记下的那个（见 [`note_anchor`]），从没点过就退化为左上角。
pub fn show(app: &AppHandle) {
    let Some(win) = app.get_webview_window(POPUP_LABEL) else {
        crate::log::warn(&crate::i18n::tf(
            "app.window_missing",
            &[("label", POPUP_LABEL)],
        ));
        return;
    };
    if win.is_visible().unwrap_or(false) {
        return;
    }
    let anchor = *last_anchor().lock().unwrap_or_else(|e| e.into_inner());
    position(&win, anchor);
    let _ = win.show();
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
            // 只在窗口**当前不可见**时恢复几何。正开着、用户正在拖大小的窗口不该被一个
            // 旧值拽回去 —— 那会让「拖到一半松手，切走再切回来」变成尺寸跳变。
            if !w.is_visible().unwrap_or(false) {
                apply_geometry(&w, label);
            }
            // 从未显示 / 最小化的窗口直接 `show()` 在 Windows 上常常不会还原到前台，
            // 而 `set_focus()` 在非前台进程里也常被系统拒绝 —— 于是「双击托盘打开编辑器」
            // 看起来像「点了没反应」（窗口其实在别的窗后面）。先 `unminimize` 兜底，再
            // show + set_focus，保证窗口真正被拉到最前。
            let _ = w.unminimize();
            let _ = w.show();
            // Windows 的 SetForegroundWindow 只对**前台进程**生效，而这个应用常年只是个
            // 托盘常驻进程 —— 直接 `set_focus` 会被系统静默丢掉。结果不是"窗口打不开"，
            // 而是"开了，但在别的窗后面"，用户看到的就是双击图标毫无动静。
            // 先把窗口临时抬到最上层把前台资格抢过来，聚焦，再立刻摁回去：一次闪都不闪，
            // 换来的是不依赖本进程此刻是否处在前台。
            #[cfg(target_os = "windows")]
            {
                let _ = w.set_always_on_top(true);
                let _ = w.set_focus();
                let _ = w.set_always_on_top(false);
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = w.set_focus();
            }
        }
        None => crate::log::warn(&crate::i18n::tf(
            "app.window_missing",
            &[("label", label)],
        )),
    }
}

/// 把窗口恢复到「上次关掉时的尺寸」，并收敛到当前显示器的工作区内。
///
/// 两件事必须一起做才有用：
///   - 只记住不收敛：换到小屏（或拔掉外接显示器）之后，记着的尺寸照样顶到任务栏下面，
///     底部的按钮点不到 —— 这正是记尺寸之前那份默认高度在 1366×768 上的表现；
///   - 只收敛不记住：用户每次打开都得重新拖一遍。
///
/// 收敛在**打开时**实时算，而不是在保存时：工作区会随显示器插拔、分辨率调整、任务栏
/// 自动隐藏而变化，写进配置的那一刻的正确值，下一次打开时未必还正确。
fn apply_geometry(win: &WebviewWindow, label: &str) {
    let saved = saved_size(win, label);
    let (w, h) = match saved {
        Some(v) => v,
        // 没记过 → 用窗口此刻的尺寸（即 `tauri.conf.json` 里声明的那个），它同样要过一遍
        // 收敛：声明值是为大屏挑的，在小屏上一样会溢出。
        None => match current_logical_size(win) {
            Some(v) => v,
            None => return,
        },
    };
    let (w, h) = fit_to_work_area(win, w, h);
    let _ = win.set_size(Size::Logical(LogicalSize::new(w as f64, h as f64)));
    apply_spot(win, label);
}

/// 把窗口恢复到「上次关掉时的位置」，并先确认那点还落在某块屏幕里。
///
/// 只恢复「在屏幕内」的位置：外接屏拔掉、分辨率改过之后，旧坐标可能落得满屏都找不到
/// （用户连改回来的抓手都没有，只能删配置文件）。这种时候不摆 —— 让 Tauri 按默认位置
/// 开，用户再拖一次就是。判据用**整块屏幕**而不是工作区：窗口贴屏幕边缘摆放完全正常，
/// 拿工作区判会把合法位置误杀。
fn apply_spot(win: &WebviewWindow, label: &str) {
    let Some((x, y)) = saved_spot(win, label) else {
        return;
    };
    if !spot_on_some_monitor(win, x, y) {
        return;
    }
    let _ = win.set_position(Position::Physical(PhysicalPosition::new(x, y)));
}

/// 这点（物理像素）是否落在任一块显示器的整块屏幕范围内。读不到屏幕列表时回答 false：
/// 摆不摆是偏好，而摆错地方的代价（窗口整个消失）远比不摆大。
fn spot_on_some_monitor(win: &WebviewWindow, x: i32, y: i32) -> bool {
    let Ok(mons) = win.available_monitors() else {
        return false;
    };
    mons.iter().any(|m| {
        let p = m.position();
        let s = m.size();
        x >= p.x && x < p.x + s.width as i32 && y >= p.y && y < p.y + s.height as i32
    })
}

/// 当前客户区尺寸，换算成**逻辑像素**。
fn current_logical_size(win: &WebviewWindow) -> Option<(u32, u32)> {
    to_logical(win.inner_size().ok()?, win.scale_factor().ok()?)
}

/// 物理像素的客户区尺寸 → 逻辑像素。
fn to_logical(size: tauri::PhysicalSize<u32>, scale: f64) -> Option<(u32, u32)> {
    if scale <= 0.0 {
        return None;
    }
    Some((
        (size.width as f64 / scale) as u32,
        (size.height as f64 / scale) as u32,
    ))
}

/// 配置里记着的这个窗口的尺寸。
fn saved_size(win: &WebviewWindow, label: &str) -> Option<(u32, u32)> {
    let state = win
        .app_handle()
        .try_state::<std::sync::Arc<crate::state::AppState>>()?;
    let s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    match label {
        MAIN_LABEL => s.window_sizes.main,
        SETTINGS_LABEL => s.window_sizes.settings,
        LOGS_LABEL => s.window_sizes.logs,
        _ => None,
    }
}

/// 配置里记着的这个窗口的位置（物理像素屏幕坐标）。
fn saved_spot(win: &WebviewWindow, label: &str) -> Option<(i32, i32)> {
    let state = win
        .app_handle()
        .try_state::<std::sync::Arc<crate::state::AppState>>()?;
    let s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    match label {
        MAIN_LABEL => s.window_spots.main,
        SETTINGS_LABEL => s.window_spots.settings,
        LOGS_LABEL => s.window_spots.logs,
        _ => None,
    }
}

/// 把目标尺寸（逻辑像素）收敛到窗口所在显示器的**工作区**内，四周留一点余量。
///
/// 用工作区而不是整块屏幕：Windows 的它是整屏减掉任务栏，macOS 的它是整屏减掉菜单栏。
/// 按整屏收敛的话，窗口底边正好压在任务栏上 —— 那正是「按钮被挡住」的成因。
fn fit_to_work_area(win: &WebviewWindow, w: u32, h: u32) -> (u32, u32) {
    let Some((_, _, ww, wh)) = work_area_at(win, None) else {
        return (w, h);
    };
    let Ok(scale) = win.scale_factor() else {
        return (w, h);
    };
    if scale <= 0.0 {
        return (w, h);
    }
    let avail_w = (ww as f64 / scale) as i64;
    let avail_h = (wh as f64 / scale) as i64;
    // 四周各留一点：贴着工作区边缘的窗口既不好看，也容易压住任务栏那一条。
    const MARGIN: i64 = 24;
    let max_w = (avail_w - MARGIN * 2).max(1);
    let max_h = (avail_h - MARGIN * 2).max(1);
    ((w as i64).min(max_w) as u32, (h as i64).min(max_h) as u32)
}

/// 窗口被关掉（收回托盘）时记下它的尺寸与位置，下次打开照原样恢复。
///
/// 存**逻辑像素尺寸 + 物理像素坐标**：尺寸换缩放比显示器后逻辑值还原出同一个「看起来
/// 这么大」；位置是操作系统给的屏幕坐标，配一道打开时的「这点还在不在某块屏里」判定
/// 就够（见 [`apply_spot`]）—— 外接屏拔掉之后照旧坐标摆会落得满屏都找不到，那件事交给
/// 打开时收敛，与尺寸用工作区收敛同一套路。
///
/// 写不进去只记一条日志：几何是偏好，不是配置 —— 为了它打断「关窗口」这个动作不值得，
/// 而静默失败又会让「为什么下次又变回去了」无从查起。
pub fn remember_geometry(window: &tauri::Window) {
    let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) else {
        return;
    };
    let Some((w, h)) = to_logical(size, scale) else {
        return;
    };
    let Some(state) = window
        .app_handle()
        .try_state::<std::sync::Arc<crate::state::AppState>>()
    else {
        return;
    };
    let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    let mut next = s.clone();
    match window.label() {
        MAIN_LABEL => {
            next.window_sizes.main = Some((w, h));
            next.window_spots.main = spot_of(window);
        }
        SETTINGS_LABEL => {
            next.window_sizes.settings = Some((w, h));
            next.window_spots.settings = spot_of(window);
        }
        LOGS_LABEL => {
            next.window_sizes.logs = Some((w, h));
            next.window_spots.logs = spot_of(window);
        }
        _ => return,
    }
    next.window_sizes = next.window_sizes.clamped();
    match next.save(&state.settings_path) {
        Ok(()) => *s = next,
        Err(e) => crate::log::warn(&crate::i18n::tf(
            "cfg.write_failed",
            &[
                ("path", &state.settings_path.display().to_string()),
                ("error", &e),
            ],
        )),
    }
}

/// 关闭这一刻的窗口位置（物理像素）。最小化中的 Windows 会给出 `(-32000, -32000)`
/// 这种哨兵坐标，存它等于存一个「下次一定落在屏幕外」的值 —— 直接不记，位置留着上次
/// 那个正常值（或没有），由打开时的收敛决定。
fn spot_of(window: &tauri::Window) -> Option<(i32, i32)> {
    let pos = window.outer_position().ok()?;
    if pos.x <= -32_000 || pos.y <= -32_000 {
        return None;
    }
    Some((pos.x, pos.y))
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
