//! 托盘菜单与托盘图标。
//!
//! 从 `main.rs` 拆出来的原因是职责而不是行数：托盘是**展示层**，它读引擎算好的结果，
//! 不参与任何判定或执行。留在入口文件里时，「菜单文案」和「点菜单之后干什么」混在
//! 一起，很容易长出「菜单回调里直接提权下发配置」这种把主线程占死的写法。
//!
//! ## 两条硬约束
//!
//! 1. [`tray_menu`] 内**不得做任何 I/O** —— 它只在主线程被调用（见 `state::publish_status`
//!    里的 `run_on_main_thread`），而这里的数据（`st` / `nics` / `current`）都由调用方
//!    在后台线程采样后传进来。macOS 15.6+ 上一次 `system_profiler` 要 1~4 秒，
//!    在主线程采一次就等于把整个 UI（连同正在展开的菜单）卡死那么久。
//! 2. 菜单里的「设为 DHCP / 立即探测」都不直接执行，只往引擎投消息 —— 提权授权框
//!    可能要用户输入密码几十秒，绝不能占用菜单回调。

use std::sync::Arc;

use tauri::{AppHandle, Manager};

use crate::engine::{ActionKind, Msg};
use crate::i18n;
use crate::log;
use crate::platform::{self, priv_channel, NetworkPlatform as _, NicKind, PrivChannel};
use crate::popup;
use crate::state::{self, AppState};

/// 托盘右键菜单。
///
/// 结构（自上而下）：
/// 1. **当前网络**：每张在用网卡一段（无线另带 SSID），逐项列出 MAC / IP / 掩码 /
///    网关 / 网关 MAC / DNS —— 多网卡同时连接时全部展示，不再只报一张无线网卡；
/// 2. **VPN**：软件名 + 网卡名 + 网关 + IP；未连接时给一行"无"；
/// 3. 当前生效 Profile（或冲突提示）与提权通道；
/// 4. 四个操作项：打开设置 / 查看日志 / 将当前网络设置为 DHCP / 强制探测当前网络；
/// 5. 退出。
///
/// `st`、`nics`、`current` 由调用方采样后传入：菜单构造发生在主线程，**本函数内不得有 I/O**。
pub fn tray_menu<R: tauri::Runtime>(
    app: &AppHandle<R>,
    st: &platform::InterfaceStatus,
    nics: &[platform::NicInfo],
    current: &str,
) -> tauri::Result<tauri::menu::Menu<R>> {
    use tauri::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem};

    // 提权通道文案（跨平台统一语义：Direct = 免密/已提权，Prompt = 每次授权）
    let priv_key = match priv_channel() {
        PrivChannel::Direct => "tray.priv_direct",
        PrivChannel::Prompt => "tray.priv_prompt",
    };

    // ——— 段 1：当前网络（每张网卡一段） ———
    let kind_label = |k: NicKind| -> String {
        i18n::t(match k {
            NicKind::Wireless => "tray.kind_wireless",
            NicKind::Wired => "tray.kind_wired",
            NicKind::Vpn => "tray.kind_vpn",
            NicKind::Other => "tray.kind_other",
        })
    };
    /// 明细行：两空格缩进，挂在网卡标题之下
    fn detail(k: String, v: String) -> String {
        format!("  {}: {}", k, v)
    }

    // 三段文案分开收集，最后再统一建菜单项：`MenuItem::with_id` 需要 `app`，
    // 而构造过程里要穿插分隔符（separator 不是 MenuItem），用一个 Vec<String> 表达不了。
    let mut net_rows: Vec<String> = Vec::new();
    let mut vpn_rows: Vec<String> = Vec::new();
    let mut tail_rows: Vec<String> = Vec::new();

    let live: Vec<&platform::NicInfo> = nics.iter().filter(|n| n.kind != NicKind::Vpn).collect();
    if live.is_empty() {
        // 采样不到网卡（权限受限 / 全断开）时，退回"主无线网卡"视角，至少别留空白
        net_rows.push(format!(
            "{}: {}",
            i18n::t("tray.kind_wireless"),
            if st.connected {
                i18n::t("popup.connected")
            } else {
                i18n::t("popup.disconnected")
            }
        ));
        if let Some(v) = st.ipv4.as_ref() {
            net_rows.push(detail(i18n::t("popup.ip"), v.clone()));
        }
        if let Some(v) = st.gateway.as_ref() {
            net_rows.push(detail(i18n::t("popup.gateway"), v.clone()));
        }
        if let Some(v) = st.dns.as_ref() {
            net_rows.push(detail(i18n::t("popup.dns"), v.clone()));
        }
    } else {
        for nic in live {
            net_rows.push(format!(
                "{}  {}  ·  {}",
                kind_label(nic.kind),
                nic.name,
                if nic.up {
                    i18n::t("popup.connected")
                } else {
                    i18n::t("popup.disconnected")
                }
            ));
            if let Some(v) = nic.ssid.as_ref() {
                net_rows.push(detail(i18n::t("status.ssid"), v.clone()));
            }
            if let Some(v) = nic.mac.as_ref() {
                net_rows.push(detail(i18n::t("tray.mac"), v.clone()));
            }
            if let Some(v) = nic.ipv4.as_ref() {
                net_rows.push(detail(i18n::t("popup.ip"), v.clone()));
            }
            if let Some(v) = nic.netmask.as_ref() {
                net_rows.push(detail(i18n::t("tray.netmask"), v.clone()));
            }
            if let Some(v) = nic.gateway.as_ref() {
                net_rows.push(detail(i18n::t("popup.gateway"), v.clone()));
            }
            if let Some(v) = nic.gateway_mac.as_ref() {
                net_rows.push(detail(i18n::t("tray.gateway_mac"), v.clone()));
            }
            if let Some(v) = nic.dns.as_ref() {
                net_rows.push(detail(i18n::t("popup.dns"), v.clone()));
            }
        }
    }

    // ——— 段 2：VPN 网卡 ———
    vpn_rows.push(i18n::t("tray.section_vpn"));
    let vpns: Vec<&platform::NicInfo> = nics.iter().filter(|n| n.kind == NicKind::Vpn).collect();
    if vpns.is_empty() {
        vpn_rows.push(detail(i18n::t("tray.vpn"), i18n::t("tray.vpn_none")));
    } else {
        for v in vpns {
            // 标题行：软件名（取不到时退化为设备名）· 网卡名
            vpn_rows.push(detail(
                v.app.clone().unwrap_or_else(|| i18n::t("tray.kind_vpn")),
                v.name.clone(),
            ));
            if let Some(gw) = v.gateway.as_ref() {
                vpn_rows.push(detail(i18n::t("popup.gateway"), gw.clone()));
            }
            if let Some(ip) = v.ipv4.as_ref() {
                vpn_rows.push(detail(i18n::t("popup.ip"), ip.clone()));
            }
        }
    }

    // ——— 段 3：当前 Profile / 提权通道 ———
    // 「当前：{name}」是带占位符的模板，故单独构造，不能当成 key/value 行拼。
    // `current` 可能是 Profile 名，也可能是「冲突：A、B」或「无匹配 Profile」——
    // 引擎给出的是已经本地化好的一行文案，托盘不重复拼语义。
    tail_rows.push(i18n::tf("tray.current", &[("name", current)]));
    tail_rows.push(format!("{}: {}", i18n::t("tray.priv"), i18n::t(priv_key)));

    // 只读信息行（enabled=false）
    let mut info_items: Vec<MenuItem<R>> = Vec::new();
    for text in net_rows
        .iter()
        .chain(vpn_rows.iter())
        .chain(tail_rows.iter())
    {
        info_items.push(MenuItem::with_id(
            app,
            format!("info{}", info_items.len()).as_str(),
            text.as_str(),
            false,
            None::<&str>,
        )?);
    }
    // VPN 段起点：定位到 net_rows 结束处
    let vpn_at = net_rows.len();

    // ——— 段 4：四个操作项 ———
    let settings = MenuItem::with_id(
        app,
        "settings",
        i18n::t("tray.open_settings").as_str(),
        true,
        None::<&str>,
    )?;
    let logs = MenuItem::with_id(
        app,
        "logs",
        i18n::t("tray.open_logs").as_str(),
        true,
        None::<&str>,
    )?;
    let dhcp = MenuItem::with_id(
        app,
        "dhcp",
        i18n::t("tray.force_dhcp").as_str(),
        true,
        None::<&str>,
    )?;
    let probe = MenuItem::with_id(
        app,
        "probe",
        i18n::t("tray.probe_now").as_str(),
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(
        app,
        "quit",
        i18n::t("tray.quit").as_str(),
        true,
        None::<&str>,
    )?;

    let sep_net = PredefinedMenuItem::separator(app)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;

    // 用 push 逐个放入而非 `map(|it| it as &dyn IsMenuItem<R>)`：push 的实参位置是
    // 强制转换点，`&MenuItem<R>` / `&PredefinedMenuItem<R>` 会在这里自动退化成 trait 对象。
    let mut items: Vec<&dyn IsMenuItem<R>> = Vec::new();
    for (i, it) in info_items.iter().enumerate() {
        // 网卡段与 VPN 段之间插一条分隔线，两段职责不同，视觉上要断开
        if i == vpn_at {
            items.push(&sep_net);
        }
        items.push(it);
    }
    if info_items.is_empty() {
        items.push(&sep_net);
    }
    items.push(&sep1);
    items.push(&settings);
    items.push(&logs);
    items.push(&dhcp);
    items.push(&probe);
    items.push(&sep2);
    items.push(&quit);
    Menu::with_items(app, &items)
}

/// 用系统默认程序打开日志目录（三平台各自映射，见 `platform::open_path`）。
fn open_logs_dir() {
    let dir = log::log_dir();
    if let Err(e) = platform::open_path(&dir.display().to_string()) {
        log::error(&format!("打开日志目录失败 {}: {}", dir.display(), e));
    }
}

pub fn build_tray(app: &tauri::App, state: &Arc<AppState>) -> tauri::Result<()> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let handle = app.handle().clone();
    // 启动时采样一次网络状态（此后每次刷新由 publish_status 把快照传进来）。
    let st = state.plat.get_status();
    let nics = state.plat.list_interfaces();
    let current = crate::engine::active_display_name(state);
    let menu = tray_menu(&handle, &st, &nics, &current)?;

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

    let tray_state = state.clone();
    builder
        .menu(&menu)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "quit" => state::request_quit(app, &tray_state),
            // 「打开设置」= 显示主编辑器窗口
            "settings" => popup::show_main(app),
            "logs" => open_logs_dir(),
            // 下面两项都会走提权 / 网络探测（秒级）：只投消息给引擎线程执行，
            // 否则 macOS 授权框弹出期间整个菜单回调被占住，UI 假死。
            "dhcp" => {
                if let Some(s) = app.try_state::<Arc<AppState>>() {
                    state::post(&s, Msg::Action { kind: ActionKind::SetDhcp });
                }
            }
            "probe" => {
                if let Some(s) = app.try_state::<Arc<AppState>>() {
                    state::post(&s, Msg::Action { kind: ActionKind::Probe });
                }
            }
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
