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
//! 所以这里只留图标：左键、右键都做同一件事 —— 在图标下方开合 [`crate::popup`]；
//! 双击则收起面板、直接打开自动化配置（见 [`crate::popup::open_automation`]）。
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
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 托盘图标的 id —— `build_tray` 建它时用这个名字，[`refresh_tooltip`] 按它找回。
const TRAY_ID: &str = "main";

/// 两次「抬起」相隔不到这么久，就当作双击。
///
/// 判定自己做，是因为 `TrayIconEvent::DoubleClick` **只有 Windows 会发**：macOS 与 Linux
/// 的原生托盘上，双击到手的仍然是两次 `Click`。自己数一遍，「双击图标 = 打开自动化配置」
/// 才是三种系统上的同一个行为，而不是只在一家有。500ms 与系统默认双击间隔同量级，比人手
/// 有意的「关掉再点开」短，误判方向也温和（开出来的编辑器面板上一键就能关）。
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

fn last_click() -> &'static Mutex<Option<Instant>> {
    static LAST_CLICK: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    LAST_CLICK.get_or_init(|| Mutex::new(None))
}

/// 图标抬起的单调计数。每记一次点击 +1；「延迟单击」的等待线程拿它当取消令牌 ——
/// 醒来时发现计数已经不是自己那一下，说明期间又来了一次抬起（双击的后半），
/// 这一次的单击动作就该作废，不再开合面板。
fn click_seq() -> &'static std::sync::atomic::AtomicU64 {
    static SEQ: OnceLock<std::sync::atomic::AtomicU64> = OnceLock::new();
    SEQ.get_or_init(|| std::sync::atomic::AtomicU64::new(0))
}

/// 「刚处理过一次原生双击」的标记（时间戳）。Windows 的双击事件顺序（tray-icon 0.25
/// 的 Windows 实现，已对照源码核实：`WM_LBUTTONDBLCLK` 在**第二次按下**时发出）是：
///
/// ```text
/// Click(Down) → Click(Up)① → DoubleClick(第二次按下) → Click(Up)②
/// ```
///
/// 末尾这个 `Click(Up)②` 是双击的**尾巴**，不是新一轮单击的开始：若照常处理，它会
/// 再调度一个「500ms 后开合面板」的延迟任务，而此时自动化界面已被双击叫出来 ——
/// 面板随后又冒出来（用户报告的「双击后既开了自动化界面、又弹了单击面板」）。
/// 处理过 `DoubleClick` 就记下时间戳，紧随其后的第一次 `Click(Up)` 若落在
/// [`TAIL_WINDOW`] 内即吞掉。The tail `Click(Up)` following a native double-click on
/// Windows is swallowed here so it cannot schedule the delayed panel toggle.
fn dbl_tail() -> &'static Mutex<Option<Instant>> {
    static DBL_TAIL: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    DBL_TAIL.get_or_init(|| Mutex::new(None))
}

/// 吞尾巴的时间窗。`DoubleClick`（第二次按下）与其后的 `Click(Up)` 之间只隔一次
/// 鼠标抬起，正常远小于 300ms；真正的第三次点击从第二次按下算起至少要再按一轮，
/// 落进这个窗口的概率可以忽略（三连击本就不在交互设计内）。
const TAIL_WINDOW: Duration = Duration::from_millis(300);

/// 记一次图标「抬起」，返回这次是否构成双击。命中双击后清空 —— 三连击是
/// 「双击 + 单击」，那一下单击该去开合面板，不该把编辑器再叫一遍。
fn register_click() -> bool {
    let mut last = last_click().lock().unwrap_or_else(|e| e.into_inner());
    let doubled = last.map(|t| t.elapsed() < DOUBLE_CLICK).unwrap_or(false);
    *last = if doubled { None } else { Some(Instant::now()) };
    doubled
}

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
                // 先看是不是原生双击的尾巴（仅 Windows 会命中，见 [`dbl_tail`] 的时序）。
                // 是就吞掉：它不是新一轮单击，不该开合面板。
                {
                    let mut tail = dbl_tail().lock().unwrap_or_else(|e| e.into_inner());
                    let hit = tail
                        .take()
                        .map(|t| t.elapsed() < TAIL_WINDOW)
                        .unwrap_or(false);
                    if hit {
                        return;
                    }
                }
                // 就地折叠成 f64 像素，避免依赖 tray::Rect 的具体导出路径
                let (x, y) = match rect.position {
                    tauri::Position::Physical(p) => (p.x as f64, p.y as f64),
                    tauri::Position::Logical(p) => (p.x, p.y),
                };
                let (w, h) = match rect.size {
                    tauri::Size::Physical(s) => (s.width as f64, s.height as f64),
                    tauri::Size::Logical(s) => (s.width, s.height),
                };
                let app = tray.app_handle().clone();
                let anchor = popup::anchor_center_bottom(x, y, w, h);
                // 单击动作**延迟一个双击窗口**再执行，执行前再确认期间没有第二次抬起。
                //
                // 不能不延迟：直接开面板的话，第二下物理点击会落在面板里（面板某一行
                // 恰好与图标同高），变成「点应用了一条配置」—— 用户以为的双击从未抵达
                // 图标。延迟之后，双击过程里面板始终不出现，第二下仍落在图标上。
                // 代价是单次点击开合面板多等 500ms，换取「双击图标 = 打开自动化配置」
                // 这个约定在三平台上一真正成立。
                let seq = click_seq().fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if register_click() {
                    // 双击的后半：上面这次 +1 已经把所有待触发的单击作废（它们的
                    // 醒来判据都是「计数仍等于自己那一下」）。
                    popup::open_automation(&app);
                } else {
                    std::thread::spawn(move || {
                        std::thread::sleep(DOUBLE_CLICK);
                        // 期间又有一次抬起（双击的后半）/ 原生 DoubleClick：作废。
                        if click_seq().load(std::sync::atomic::Ordering::SeqCst) != seq {
                            return;
                        }
                        // 醒来后仍要回到主线程：面板显隐是 GUI 操作，且 popup::toggle
                        // 内部要读窗口可见性 —— 从任意线程调 Tauri 窗口 API 不安全。
                        //
                        // `app` 已 move 进本线程，这里 clone 出两个：一个给 `run_on_main_thread`
                        // 当 `&self`（借用），另一个 move 进内层闭包给 `popup::toggle` 借用 ——
                        // 不能让同一个 `app` 既被 `&self` 借用又被内层 `move` 捕获，否则 E0505。
                        let app_rt = app.clone();
                        let app_cl = app.clone();
                        let _ = app_rt.run_on_main_thread(move || {
                            if click_seq().load(std::sync::atomic::Ordering::SeqCst) != seq {
                                return;
                            }
                            popup::toggle(&app_cl, anchor);
                        });
                    });
                }
                return;
            }
            // Windows 的原生 `DoubleClick` 在**第二次按下**时发出（见 [`dbl_tail`] 的
            // 时序），它后面还跟着一次 `Click(Up)` 尾巴 —— 记下时间戳让 Click 分支把它
            // 吞掉，否则那一下会调度「500ms 后开合面板」，自动化界面开完面板又冒出来。
            // 作废待触发的延迟单击照旧：双击的前半（第一次抬起）若还在等，就不该再开面板。
            if let TrayIconEvent::DoubleClick { .. } = &event {
                *dbl_tail().lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                click_seq().fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *last_click().lock().unwrap_or_else(|e| e.into_inner()) = None;
                popup::open_automation(tray.app_handle());
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
