//! Tauri 命令：前端与引擎之间的唯一入口。
//!
//! 两条纪律，都是踩过坑换来的：
//!
//! 1. **会做 I/O 或可能阻塞的命令一律标 `async`**。不带 async 的 Tauri 命令跑在
//!    **主线程**上，而这里几乎每个命令都要采样网络或落盘；主线程一卡，整个 UI
//!    （连同正在展开的面板）就冻住。
//! 2. **命令不改引擎状态，也不直接碰网卡**。写配置只落盘 + 往引擎投消息；
//!    「立即应用 / 设为 DHCP / 立即探测」都投给引擎线程串行执行 —— 否则 IPC 工作线程、
//!    托盘回调和引擎会同时抢同一张网卡，出现「新环境的配置被旧环境的动作覆盖」。
//!    因此这些命令的 `Ok(())` 只表示「已受理」，结果由 `netsense://action` 播报。

use serde_json::json;
use tauri::{Manager, State};

use crate::config::{FallbackConfig, Profile};
use crate::engine::{ActionKind, Msg};
use crate::i18n;
use crate::log;
// `list_known_ssids` 是 PAL trait 方法，需把 trait 引入作用域
use crate::platform::{platform_name, priv_channel, NetworkPlatform as _, AppEntry, NicInfo, PrinterInfo};
use crate::state::{self, AppState};

/// 前端读取当前网络状态 + 引擎视图（Active / Conflict / 每个 Profile 的三态）+ 语言 + 提权通道。
///
/// `async` 是必需的，不是风格偏好：本命令要采样网络状态（macOS 15.6+ 的降级链里含一次
/// 1~4s 的 `system_profiler`）。
#[tauri::command(async)]
pub fn get_status(state: State<'_, std::sync::Arc<AppState>>) -> String {
    let st = state.plat.get_status();
    let payload = state::status_payload(&state, &st);
    serde_json::to_string(&payload).unwrap_or_default()
}

/// 只读引擎视图。事件可能赶在窗口监听之前发生（例如编辑器刚被打开），
/// 前端用这条命令补齐；它**不**重新采样网络，所以很便宜。
#[tauri::command]
pub fn get_engine_status(state: State<'_, std::sync::Arc<AppState>>) -> String {
    let eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
    serde_json::to_string(&eng.view(&cfg)).unwrap_or_else(|_| "{}".to_string())
}

/// 编辑器用：拿**引擎此刻的快照**给表单里正填着的条件算一遍三态。
///
/// 存在的理由是那几秒的等待：引擎一轮要走自己的采样节律与 `change_delay_secs` 去抖
/// （默认 5 秒，最坏 8 秒以上），而「我填的这条 SSID 现在对不对得上」本来不需要等它。
/// `payload` 是**表单当前内容**（含没保存的草稿）的 Profile 数组 —— 判据仍然只有
/// [`crate::conditions::eval_profile`] 那一份，前端不重写第二套匹配规则。
///
/// 这里用的是已经采到的那一份快照，不在这一问一答里再采一次：一次采样要拉若干子进程
/// （macOS 的降级链里含一次几秒的 `system_profiler`），那样「即时预览」反而成了最慢的一条。
/// 引擎还没跑完过第一轮时作答不了（快照是全空的默认值，拿它算出来的「都不匹配」只是没测过），
/// 于是返回空数组，让界面继续用引擎广播的那份徽标。
#[tauri::command(async)]
pub fn preview_match(
    state: State<'_, std::sync::Arc<AppState>>,
    payload: String,
) -> Result<String, String> {
    let profiles: Vec<Profile> = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
    let snap = {
        let eng = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        if !eng.sampled() {
            return Ok("[]".to_string());
        }
        eng.snapshot.clone()
    };
    let evals: Vec<_> = profiles
        .iter()
        .map(|p| crate::conditions::eval_profile(p, &snap))
        .collect();
    serde_json::to_string(&evals).map_err(|e| e.to_string())
}

/// 前端读取全部在用网卡（有线 / 无线 / VPN），供面板与设置窗口的「网络硬件信息」列展示。
///
/// **返回列表的第一张一定是「在用的那张」**：判据只在 Rust 里有一份
/// （`automation::primary_nic` =
/// 走默认路由的非 VPN 网卡），这里把它转到首位，前端取 `nics[0]` 就行。让 JS 再实现一遍
/// 选主网卡的规则，会得到一份和「设为 DHCP」不一致的副本。
///
/// `async` 的理由同 `get_status`：枚举网卡要拉起子进程（平台层内有 TTL 缓存，
/// 但缓存缺失的那一次仍是 I/O），不能占住主线程。
#[tauri::command(async)]
pub fn get_interfaces(state: State<'_, std::sync::Arc<AppState>>) -> String {
    let mut nics = state.plat.list_interfaces();
    let primary = crate::automation::primary_nic(&nics)
        .map(|p| p.name.clone())
        .and_then(|name| nics.iter().position(|n| n.name == name));
    if let Some(i) = primary {
        nics.rotate_left(i);
    }
    serde_json::to_string(&nics).unwrap_or_else(|_| "[]".to_string())
}

/// 前端触发「将当前网络设置成 DHCP」（与面板按钮同一入口）。
#[tauri::command(async)]
pub fn force_dhcp(state: State<'_, std::sync::Arc<AppState>>) -> Result<(), String> {
    state::post(&state, Msg::Action { kind: ActionKind::SetDhcp });
    Ok(())
}

/// 前端触发「强制探测当前网络」（与面板按钮同一入口）。
///
/// 结果不在这里返回：探测要几秒，完成后由 `netsense://action` 广播给前端做 toast。
#[tauri::command(async)]
pub fn probe_network(state: State<'_, std::sync::Arc<AppState>>) -> Result<(), String> {
    state::post(&state, Msg::Action { kind: ActionKind::Probe });
    Ok(())
}

/// 前端读取完整配置（含 profile 全量字段），供编辑器加载与回填。
#[tauri::command]
pub fn get_config(state: State<'_, std::sync::Arc<AppState>>) -> String {
    let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
    let payload = json!({
        "schema": cfg.schema,
        "profiles": cfg.profiles,
        "fallback": cfg.fallback,
        "allowed_scripts": cfg.allowed_scripts,
        "config_path": state.config_path.display().to_string(),
        // 代际号在这把锁内读（写者也在锁内自增，见 [`AppState::config_replaced`]）：
        // 编辑器拿到的「内容 + 号」是同一代，重读后不会把旧内容当成新的。
        "config_rev": state.config_rev.load(std::sync::atomic::Ordering::SeqCst),
    });
    serde_json::to_string(&payload).unwrap_or_default()
}

/// 保存/新增一个 Profile（按 `id` upsert）。payload = 一个完整的 Profile 对象。
///
/// 事务式更新：先在校验通过的副本上改，落盘成功后才替换内存态 —— 否则一次校验失败
/// 会把内存里的配置改脏，而磁盘上还是旧的，两边从此对不上。
/// 整体校验（而不是只校验这一条）是刻意的：rule id 在 Profile 内唯一、profile id 全局
/// 唯一，这些约束只有看整份配置才判得出来。
#[tauri::command(async)]
pub fn save_profile(
    state: State<'_, std::sync::Arc<AppState>>,
    payload: String,
) -> Result<(), String> {
    let profile: Profile = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
    if profile.id.trim().is_empty() {
        // 与校验器同一条文案：这里能报出 name，比校验器那条还具体一点。
        return Err(i18n::tf("cfg.no_id", &[("name", &profile.name)]));
    }
    let id = profile.id.clone();
    {
        let mut cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = cfg.clone();
        match next.profile_by_id_mut(&id) {
            Some(slot) => *slot = profile,
            None => next.profiles.push(profile),
        }
        next.validate()?;
        next.save(&state.config_path)?;
        *cfg = next;
        state.config_replaced();
    }
    // 落盘即唤醒引擎：由它统一做「重新评估 + 广播」，避免这里再广播一次
    //（两处广播会各采样一次网络状态，还会让前端看到两个先后顺序不确定的 status）。
    state::post(&state, Msg::Wake);
    Ok(())
}

/// 保存「Profile 之外」的全局项：零命中兜底（fallback）与脚本白名单。
///
/// 为什么单独一条命令：fallback 没有条件、不参与匹配也不参与冲突判定，把它塞进
/// Profile 就等于把「零命中时的处置」重新变成「一个可能命中的环境」，
/// 于是它不能跟着 `save_profile` 一起写。
/// payload = `{"fallback": FallbackConfig|null, "allowed_scripts": [abs path]}`。
#[tauri::command(async)]
pub fn save_global(
    state: State<'_, std::sync::Arc<AppState>>,
    payload: String,
) -> Result<(), String> {
    let patch: serde_json::Value = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
    // 缺字段即保留既有值：fallback 与 allowed_scripts 语义必须一致，绝不能因为一份
    // 不含某字段的保存就悄悄清掉用户之前的配置（否则保存全局项时会无声撤销零命中兜底
    // 或脚本白名单）。
    let (existing_fallback, existing_scripts) = {
        let cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        (cfg.fallback.clone(), cfg.allowed_scripts.clone())
    };
    let fallback: Option<FallbackConfig> = match patch.get("fallback") {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("fallback: {}", e))?,
        None => existing_fallback,
    };
    let scripts: Vec<String> = patch
        .get("allowed_scripts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or(existing_scripts);
    {
        let mut cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = cfg.clone();
        next.fallback = fallback;
        next.allowed_scripts = scripts;
        next.validate()?;
        next.save(&state.config_path)?;
        *cfg = next;
        state.config_replaced();
    }
    state::post(&state, Msg::Wake);
    Ok(())
}

/// 删除一个 Profile 并写回磁盘。
#[tauri::command(async)]
pub fn delete_profile(
    state: State<'_, std::sync::Arc<AppState>>,
    id: String,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = cfg.clone();
        let before = next.profiles.len();
        next.profiles.retain(|p| p.id != id);
        if next.profiles.len() == before {
            return Err(i18n::tf("engine.unknown_profile", &[("name", &id)]));
        }
        next.save(&state.config_path)?;
        *cfg = next;
        state.config_replaced();
    }
    state::post(&state, Msg::Wake);
    Ok(())
}

/// 请求「立即应用」某个 Profile。
///
/// 这条命令**不**绕过条件判定：引擎仍会按该 Profile 自己的 Rules/Conditions 决定跑
/// THEN 还是 ELSE，并且当前存在冲突时直接拒绝（否则用户可以绕过「多命中不自动选择」
/// 这条核心约束）。详见 `engine::manual_apply`。
#[tauri::command(async)]
pub fn apply_profile(
    state: State<'_, std::sync::Arc<AppState>>,
    id: String,
) -> Result<(), String> {
    state::post(&state, Msg::Apply { id });
    Ok(())
}

/// 显示主编辑器窗口。
#[tauri::command]
pub fn open_editor(state: State<'_, std::sync::Arc<AppState>>) {
    if let Some(app) = state.app.get() {
        crate::popup::show_main(app);
    }
}

/// 隐藏主编辑器窗口（收回菜单栏常驻，不退出应用）。
#[tauri::command]
pub fn close_editor(state: State<'_, std::sync::Arc<AppState>>) {
    if let Some(app) = state.app.get() {
        if let Some(w) = app.get_webview_window(crate::popup::MAIN_LABEL) {
            let _ = w.hide();
        }
    }
}

/// 显示软件配置窗口（面板的「设置」按钮调用）。
#[tauri::command]
pub fn open_settings(state: State<'_, std::sync::Arc<AppState>>) {
    if let Some(app) = state.app.get() {
        crate::popup::show_settings(app);
    }
}

/// 隐藏软件配置窗口（收回菜单栏常驻，不退出应用）。
#[tauri::command]
pub fn close_settings(state: State<'_, std::sync::Arc<AppState>>) {
    if let Some(app) = state.app.get() {
        if let Some(w) = app.get_webview_window(crate::popup::SETTINGS_LABEL) {
            let _ = w.hide();
        }
    }
}

/// 退出应用（同时停掉 SSID 监视线程与引擎线程）。
#[tauri::command]
pub fn quit_app(state: State<'_, std::sync::Arc<AppState>>) {
    if let Some(app) = state.app.get() {
        // 必须走 request_quit：它会置位 QUITTING，否则 run() 里的
        // ExitRequested 守卫会把这次退出也拦下（应用将无法退出）。
        state::request_quit(app, &state);
    }
}

/// 用系统默认程序打开日志目录（面板与软件配置窗口都有这个入口）。
#[tauri::command(async)]
pub fn open_logs() -> Result<(), String> {
    let dir = crate::log::log_dir();
    let text = dir.display().to_string();
    // 回给前端的是**已翻译**的一句：这条会被原样弹成 toast，界面并不会再替它套模板。
    crate::platform::open_path(&text)
        .map_err(|e| i18n::tf("notify.open_failed", &[("path", &text), ("error", &e)]))
}

/// 日志窗口：列出目录里的日志文件（新→旧）连同目录本身。
///
/// 目录一并返回，是因为它可能不是配置里写的那个：日志目录不可写时 `log::init` 会退到
/// 系统临时目录，界面若自己拼路径就会把人引到一个根本没有日志的地方。
#[tauri::command(async)]
pub fn get_log_files() -> serde_json::Value {
    json!({
        "log_dir": crate::log::log_dir().display().to_string(),
        "files": log::list_files(),
        "max_lines": log::MAX_TAIL_LINES,
    })
}

/// 读取某个日志文件的尾部若干行。
///
/// 界面只能给文件名，不能给路径 —— 名字的形状在 `log::read_tail` 里重新校验一次，
/// 所以这条命令没有「把任意文件内容读进界面」的入口。
#[tauri::command(async)]
pub fn read_log(name: String, lines: Option<usize>) -> Result<serde_json::Value, String> {
    let content = log::read_tail(&name, lines.unwrap_or(500).min(log::MAX_TAIL_LINES))?;
    serde_json::to_value(content).map_err(|e| e.to_string())
}

/// 显示日志窗口。
#[tauri::command]
pub fn open_log_viewer(state: State<'_, std::sync::Arc<AppState>>) {
    if let Some(app) = state.app.get() {
        crate::popup::show_logs(app);
    }
}

/// 隐藏日志窗口（收回菜单栏常驻，不退出应用）。
#[tauri::command]
pub fn close_log_viewer(state: State<'_, std::sync::Arc<AppState>>) {
    if let Some(app) = state.app.get() {
        if let Some(w) = app.get_webview_window(crate::popup::LOGS_LABEL) {
            let _ = w.hide();
        }
    }
}

/// 前端读取已保存无线网络列表（填充编辑器下拉）。
///
/// `async` 不是可选项：`list_known_ssids` 会拉起系统子进程（macOS 上
/// `networksetup -listpreferredwirelessnetworks`），而非 async 命令跑在主线程，
/// 编辑器一打开就会把主线程按住几百毫秒（见 DEVELOPMENT.md §9.6 第 5 条）。
#[tauri::command(async)]
pub fn get_networks(state: State<'_, std::sync::Arc<AppState>>) -> Vec<String> {
    // 走 TTL 缓存：这份清单只在用户连过新网络之后才会变，而枚举它要拉一次子进程。
    // 详见 `platform::cached_known_ssids`。
    crate::platform::cached_known_ssids(&state.plat)
}

/// 本机**装着**的网卡（含现在没插线、没连上的），填充编辑器的接口条件下拉。
///
/// 与 `get_interfaces` 的分工照 [`crate::platform::NetworkPlatform::list_adapters`]：
/// 那条回答「现在连着哪些网」（面板与硬件列），这条回答「有哪几口可以选」（下拉）。
/// 下拉用前者的话，拔了网线就只剩 `en0`，用户没法提前给另一个口配好网络。
///
/// 只填 `name` / `label` / `kind` / `up`，地址类字段一律 `None`。`async` 的理由同
/// `get_networks`：枚举要拉系统子进程，而且这里刻意不套 TTL 缓存 —— 刚插上扩展坞之后
/// 那一次刷新就该看到新口。
#[tauri::command(async)]
pub fn get_adapters(state: State<'_, std::sync::Arc<AppState>>) -> Vec<NicInfo> {
    state.plat.list_adapters()
}

/// 本机打印机清单（编辑器里「设为默认打印机」的候选，含现在哪台是默认）。
///
/// 空数组是合法答案：这台机器可能没有打印机，也可能打印子系统没在跑。界面因此把这一栏
/// 留成可手输，而不是转成「加载失败」。`async` 的理由同 `get_networks` —— 枚举要拉子进程。
#[tauri::command(async)]
pub fn get_printers(state: State<'_, std::sync::Arc<AppState>>) -> Vec<PrinterInfo> {
    // 走 TTL 缓存：接一台新打印机是分钟级的事，而枚举它（Windows 上是 CIM 查询）要
    // 一次完整的子进程冷启动。详见 `platform::cached_printers`。
    crate::platform::cached_printers(&state.plat)
}

/// 本机已装程序清单（编辑器里启动程序动作的候选）。
///
/// 空数组是合法答案：枚举范围是「有启动意图登记」的那些（macOS 的 `.app`、Windows
/// 开始菜单的快捷方式、Linux 的 `.desktop`），一台干净机器上一条都没有是可能的。
/// 界面因此把这栏留成可手输，另一个入口是 [`pick_app`]（磁盘上任意一个程序）。
/// `async` 的理由同 `get_networks` —— 枚举要拉子进程或遍历目录。
#[tauri::command(async)]
pub fn get_installed_apps(state: State<'_, std::sync::Arc<AppState>>) -> Vec<AppEntry> {
    // 走 TTL 缓存：这是开屏最贵的那一项（Windows 上要递归遍历两个开始菜单目录找 .lnk），
    // 而它的答案在装新程序之前不会变。详见 `platform::cached_apps`。
    crate::platform::cached_apps(&state.plat)
}

/// 弹系统自己的文件选择器挑一个程序，返回其路径；用户取消 → `None`。
///
/// `async` 不是可选项：这是**模态**对话框，会阻塞到用户做出选择（可能好几分钟），
/// 而非 async 命令跑在主线程，等待期间整个界面会失去响应。
#[tauri::command(async)]
pub fn pick_app(state: State<'_, std::sync::Arc<AppState>>) -> Result<Option<String>, String> {
    state.plat.pick_app()
}

/// 切换 UI 语言并写回软件配置（`language` 字段）。
///
/// `code` 为 `"system"` 时是「跟随系统」：字段从 `settings.json` 里**删掉**（缺省就是
/// 跟随，配置里不留一个会被误当成硬编码的值），本次生效的语言现问操作系统。
///
/// 与自动化配置无关，所以它**不**经过 `Config`：换语言不该让热重载引擎、更不该让
/// 一次网络重评估被触发（那是「改菜单语言，结果 IP 被重新下发了一遍」的来源）。
/// 这里显式广播一次状态，是因为没有别的路径会替我们广播 —— 前端靠 `status_payload`
/// 里的 `language` 判断要不要重拉 `get_strings`。
#[tauri::command(async)]
pub fn set_language(
    state: State<'_, std::sync::Arc<AppState>>,
    code: String,
) -> Result<(), String> {
    let follow_system = code.eq_ignore_ascii_case("system");
    let lang = if follow_system {
        i18n::Language::from_code(&crate::platform::system_ui_language().unwrap_or_default())
    } else {
        i18n::Language::from_code(&code)
    };
    // 先落盘，再切字典。反过来的话，一次写失败（只读目录、权限）会在 `?` 处提前返回，
    // 而全局字典已经换好了：托盘不重设、广播不发、设置也没存住 —— 同一份界面停在两种
    // 语言里，且再点一次也修不回来（字典没法回退）。存不上就当这次没发生。
    {
        let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = s.clone();
        next.language = if follow_system {
            None
        } else {
            Some(lang.code().to_string())
        };
        next.save(&state.settings_path)?;
        *s = next;
    }
    i18n::set_language(lang);
    log::info(&i18n::tf("notify.language_set", &[("lang", lang.code())]));
    // 托盘 tooltip 是 `build_tray` 当场求值出来的字符串，字典换了它不会自己换 ——
    // 面板文案走广播（下面那条），托盘只能在这里重设。
    if let Some(app) = state.app.get() {
        crate::tray::refresh_tooltip(app);
    }
    crate::state::publish_status(state.inner());
    Ok(())
}

/// 界面现在该用哪套配色：`"light"` 或 `"dark"`。
///
/// 给的是**算出来的那一套**，不是 `settings.json` 里存的那一档：`system` 这一档要说清
/// 「现在是浅色还是深色」必须问操作系统，而那一句问话属于后端 —— 决定在这里做一次，
/// 四座窗口才拿得到同一份答案，不必在 CSS 里让每座窗口各自解释一遍
/// `prefers-color-scheme`（见 [`crate::platform::NetworkPlatform::ui_prefers_dark`]）。
#[tauri::command(async)]
pub fn get_theme(state: State<'_, std::sync::Arc<AppState>>) -> String {
    let setting = state
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .theme
        .clone();
    crate::appconfig::theme_now(&setting).to_string()
}

/// 切换界面配色并写回软件配置（`theme` 字段）。
///
/// 存的是用户选的**那一档**（`system` / `light` / `dark`），返回值才是当下生效的那套：
/// 系统把深色换成浅色时，配置文件里那句 `system` 不该跟着改口。
///
/// 与语言同一条判据：它不碰 `config.json`，因此不触发热重载与网络重评估；改完只广播一次
/// `netsense://status`，前端比着载荷里的 `theme` 换掉 `<html data-theme>`。
///
/// 认不出的 `code` 一律拒绝，而不是退回默认档：这一句改的是用户刚刚点中的那个选项，
/// 界面发出一个后端不认的值说明前端有 bug，静默收下只会把它藏起来。
#[tauri::command(async)]
pub fn set_theme(
    state: State<'_, std::sync::Arc<AppState>>,
    code: String,
) -> Result<String, String> {
    if !crate::appconfig::THEMES.contains(&code.as_str()) {
        return Err(i18n::t("sett.theme_rejected"));
    }
    let next = code.clone();
    {
        let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = s.clone();
        cfg.theme = next;
        cfg.save(&state.settings_path)?;
        *s = cfg;
    }
    let now = crate::appconfig::theme_now(&code);
    log::info(&i18n::tf("notify.theme_set", &[("theme", now)]));
    crate::state::publish_status(state.inner());
    Ok(now.to_string())
}

/// 软件配置窗口的一次性快照：能改的、只能看的、以及它们在各平台上的真实位置。
///
/// `autostart` 是**问操作系统**问出来的，不是读配置文件读出来的（见 `appconfig` 模块头）；
/// 问不出来时 `autostart` 给 `false`、原因放进 `autostart_error`，界面因此不会显示一个
/// 假的关闭态而不给解释。
#[tauri::command(async)]
pub fn get_app_settings(state: State<'_, std::sync::Arc<AppState>>) -> String {
    let (language, language_auto, theme, log_retention_days) = {
        let s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
        (
            i18n::current().code().to_string(),
            s.language.is_none(),
            crate::appconfig::normalize_theme(&s.theme).to_string(),
            s.log_retention_days,
        )
    };
    let (autostart_on, autostart_error) = match crate::appconfig::autostart_enabled() {
        Ok(on) => (on, None),
        Err(e) => (false, Some(e)),
    };
    let config_dir = state
        .config_path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    // 备份目录只做「报路径」，不做创建：这个命令每次窗口获得焦点都会跑一遍。
    let backup_dir = crate::backup::dir(&backup_sources(&state)).display().to_string();
    // 代理只报存下来的那一态，不在这里探测系统：这个命令每次窗口获得焦点都会跑一遍，
    // 而「读一次系统代理」在 Linux 上是好几条 `gsettings` 进程。实况走 `get_proxy_state`。
    let (proxy_mode, proxy_url) = {
        let s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
        match &s.proxy {
            crate::netproxy::ProxySetting::Direct => ("direct", None),
            crate::netproxy::ProxySetting::System => ("system", None),
            crate::netproxy::ProxySetting::Manual { url } => ("manual", Some(url.clone())),
        }
    };
    let payload = json!({
        "language": language,
        "language_auto": language_auto,
        // 存下来的那一档（system / light / dark），不是当下渲染的那套：后者要问操作系统，
        // 而这个命令每次窗口获得焦点都会跑一遍（同下面 proxy 那句理由）。界面渲染用的
        // 具体值走 `get_theme`，这里只负责把下拉放回用户选过的位置。
        "theme": theme,
        "autostart": autostart_on,
        "autostart_error": autostart_error,
        "config_path": state.config_path.display().to_string(),
        "config_dir": config_dir,
        // 受信的脚本目录：`run_script` 的白名单规则里有一项是「这个目录之下的都放行」，
        // 而它跟着自动化配置文件走，界面自己拼出来的分隔语不一定对（Windows 是反斜杠）。
        "scripts_dir": state.scripts_dir.display().to_string(),
        "backup_dir": backup_dir,
        "proxy_mode": proxy_mode,
        "proxy_url": proxy_url,
        "settings_path": state.settings_path.display().to_string(),
        "log_dir": crate::log::log_dir().display().to_string(),
        "log_retention_days": log_retention_days,
        "min_retention_days": crate::appconfig::MIN_LOG_RETENTION_DAYS,
        "max_retention_days": crate::appconfig::MAX_LOG_RETENTION_DAYS,
        "priv": priv_channel().code(),
        "platform": platform_name(),
        "version": env!("CARGO_PKG_VERSION"),
    });
    serde_json::to_string(&payload).unwrap_or_default()
}

/// 打开 / 关闭「登录时启动」。返回系统里**实际**的状态，界面按它回显。
///
/// 不要 `State`：这项设置的真相在操作系统那边，本进程的内存态里没有它的位置。
#[tauri::command(async)]
pub fn set_autostart(enable: bool) -> Result<bool, String> {
    let now = crate::appconfig::set_autostart(enable)?;
    // 两条 key 而不是一条带 `{state}` 的：占位符里塞 "on"/"off" 会让日志在
    // 中文界面下变成「开机启动已设置为 on」这种半截翻译。
    log::info(&i18n::t(if now {
        "notify.autostart_on"
    } else {
        "notify.autostart_off"
    }));
    Ok(now)
}

/// 撤销 macOS 的免密通道：删掉白名单包装脚本与 `/etc/sudoers.d/netsense`，需要一次授权。
///
/// 这条命令只负责「撤」。重新装上不需要单独入口 —— 通道不在的时候，下一次应用配置会
/// 在那个本来就要弹的授权框里顺手装好（见 `platform::macos::exec_ops_bootstrap`），
/// 所以撤销之后不会「再也装不上」，而重新安装始终需要用户当场输一次密码。
#[tauri::command(async)]
pub fn uninstall_priv_channel() -> Result<(), String> {
    crate::platform::uninstall_priv_channel()?;
    log::info(&i18n::t("notify.priv_removed"));
    Ok(())
}

/// 设置日志保留天数：写软件配置 + 立刻按新值清一次。
#[tauri::command(async)]
pub fn set_log_retention(
    state: State<'_, std::sync::Arc<AppState>>,
    days: u32,
) -> Result<u32, String> {
    let days = crate::appconfig::clamp_retention(days);
    {
        let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = s.clone();
        next.log_retention_days = days;
        next.save(&state.settings_path)?;
        *s = next;
    }
    crate::log::set_retention_days(days);
    log::info(&i18n::tf("notify.retention_set", &[("days", &days.to_string())]));
    Ok(days)
}

/// 代理这一栏的实况：存下来的选择，加上「跟随系统」此刻**实际**会用到的地址。
///
/// 只在真的选了跟随系统时才去问操作系统。这个命令由设置窗口在打开与保存之后各调一次 ——
/// 探测一次要跑一条系统命令（Linux 上是好几条 `gsettings`），不该塞进每次窗口聚焦都要跑
/// 的 `get_app_settings` 里。
#[tauri::command(async)]
pub fn get_proxy_state(state: State<'_, std::sync::Arc<AppState>>) -> String {
    use crate::netproxy::ProxySetting;
    let setting = state
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .proxy
        .clone();
    let (mode, url, system) = match &setting {
        ProxySetting::Direct => ("direct", None, None),
        ProxySetting::System => ("system", None, crate::netproxy::system_proxy()),
        ProxySetting::Manual { url } => ("manual", Some(url.clone()), None),
    };
    serde_json::to_string(&json!({
        "mode": mode,
        "url": url,
        "system_proxy": system,
    }))
    .unwrap_or_default()
}

/// 设「升级请求走哪条出口」：`direct` / `system` / `manual` + 一个地址。
///
/// 认不出的 `mode` 一律拒绝，而不是退回默认值：这一句改的是这台机器的对外流量走哪条路，
/// 猜错的代价远大于界面上多一条错误。地址形状由 [`crate::netproxy::normalize_proxy_url`]
/// 判，拒绝时给同一句文案 —— 用户要的是「这个填法不行」，不是背一套错误码。
///
/// 保存后**不**惊动引擎：这份设置不参与任何条件求值或下发（判据见 `appconfig.rs`）。
#[tauri::command(async)]
pub fn set_update_proxy(
    state: State<'_, std::sync::Arc<AppState>>,
    mode: String,
    url: String,
) -> Result<String, String> {
    use crate::netproxy::ProxySetting;
    let rejected = || i18n::t("sett.proxy_rejected");
    let next = match mode.as_str() {
        "direct" => ProxySetting::Direct,
        "system" => ProxySetting::System,
        "manual" => ProxySetting::Manual {
            url: crate::netproxy::normalize_proxy_url(&url).ok_or_else(rejected)?,
        },
        _ => return Err(rejected()),
    };
    {
        let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = s.clone();
        cfg.proxy = next.clone();
        cfg.save(&state.settings_path)?;
        *s = cfg;
    }
    let via = match &next {
        ProxySetting::Direct => i18n::t("sett.proxy_direct"),
        ProxySetting::System => i18n::t("sett.proxy_system"),
        ProxySetting::Manual { url } => url.clone(),
    };
    let msg = i18n::tf("notify.proxy_set", &[("via", &via)]);
    log::info(&msg);
    Ok(msg)
}

/// 在系统文件管理器里打开自动化配置所在的那个目录。
///
/// 目录由 `config_path` 的父目录现算，界面不自己拼 —— 用户看到的和真正打开的必须是同一个位置。
#[tauri::command(async)]
pub fn open_config_folder(state: State<'_, std::sync::Arc<AppState>>) -> Result<(), String> {
    let text = match state.config_path.parent() {
        Some(p) => p.display().to_string(),
        None => return Err(i18n::t("notify.no_config_dir")),
    };
    crate::platform::open_path(&text)
        .map_err(|e| i18n::tf("notify.open_failed", &[("path", &text), ("error", &e)]))
}

/// 在系统文件管理器里打开软件配置（`settings.json`）所在的那个目录。
///
/// 与 [`open_config_folder`] 是**两条**命令而不是一条：两份文件通常同目录，但自动化配置允许
/// 来自可执行文件同级（开发期或便携运行），那时按 `config_path` 打开的目录里根本没有
/// `settings.json`。界面在这张卡片下列的是 `settings_path`，按钮就必须开这一个。
/// 名字跟着 `get_app_settings` 走，因为「软件设置窗口」在这个进程里就叫 app settings ——
/// 旁边那条 `open_settings` 开的是**窗口**，差一个词，指的却是两回事。
#[tauri::command(async)]
pub fn open_app_settings_folder(state: State<'_, std::sync::Arc<AppState>>) -> Result<(), String> {
    let text = match state.settings_path.parent() {
        Some(p) => p.display().to_string(),
        None => return Err(i18n::t("notify.no_settings_dir")),
    };
    crate::platform::open_path(&text)
        .map_err(|e| i18n::tf("notify.open_failed", &[("path", &text), ("error", &e)]))
}

/// 前端一次性拉取当前语言的全部文案，避免逐 key 往返 invoke。
#[tauri::command]
pub fn get_strings() -> std::collections::HashMap<String, String> {
    i18n::all_strings()
}

/// 备份要认的三处位置，一律取自 `AppState`。
///
/// 不在 `backup` 里按配置路径自己推：脚本目录必须是引擎执行脚本、`AllowedScripts`
/// 判定受信的**那一个**，软件配置也没有「同目录兜底」那一档（见 [`crate::paths`]）。
/// 两处各推一遍，迟早会分岔，而分岔的表现是「备份成功了」——恢复的却是别处的文件。
fn backup_sources(state: &AppState) -> crate::backup::Sources<'_> {
    crate::backup::Sources {
        config: &state.config_path,
        settings: &state.settings_path,
        scripts: &state.scripts_dir,
    }
}

/// 导出一份备份：自动化配置、软件配置与受信脚本目录收进备份目录里的一个新文件。
///
/// 返回给用户看的那句话由后端拼（`notify.backup_exported`）：日志里的是同一句，界面上
/// 看到的就不会和日志对不上。`async`：整份备份的读写是磁盘 I/O。
#[tauri::command(async)]
pub fn export_backup(state: State<'_, std::sync::Arc<AppState>>) -> Result<String, String> {
    let src = backup_sources(&state);
    let name = crate::backup::export(&src)?;
    let msg = i18n::tf("notify.backup_exported", &[("name", &name)]);
    log::info(&msg);
    Ok(msg)
}

/// 列出已有的备份，新→旧。目录还不存在时是空数组 —— 那是「从没导出过」，不是错误。
#[tauri::command(async)]
pub fn get_backups(state: State<'_, std::sync::Arc<AppState>>) -> String {
    let src = backup_sources(&state);
    serde_json::to_string(&crate::backup::list(&src)).unwrap_or_else(|_| "[]".to_string())
}

/// 恢复一个备份。
///
/// 磁盘写回去之后，内存里那两份配置也必须追上，否则这次恢复只完成了一半：
/// · `config` 不替换，引擎下一轮评估用的还是旧配置（而下文那次 `Wake` 会立刻按它动作）；
/// · `settings` 不重放，界面文案与日志保留窗口就还按旧的那份走。
///
/// 收尾两条都发：`Msg::Wake` 让引擎按新配置重新评估（该下发的下发），再显式播报一次
/// 状态 —— 后者是编辑器比对代际号（`config_rev`）的入口：引擎那趟播不播报取决于有
/// 没有到期的东西，安静的系统里它收工就走，光靠它编辑器就可能一直停在旧那一份。
/// 导入是用户点出来的稀罕事，多付一次采样可以接受；若引擎那趟也播了，编辑器按号
/// 比较后不会重读第二遍。
#[tauri::command(async)]
pub fn import_backup(
    state: State<'_, std::sync::Arc<AppState>>,
    name: String,
) -> Result<String, String> {
    let src = backup_sources(&state);
    let r = crate::backup::import(&src, &name)?;
    let msg = crate::backup::restore_summary(&r);
    if let Some(c) = r.config {
        {
            let mut cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
            *cfg = c;
            state.config_replaced();
        }
        // 配置文件刚被写过，mtime 变了；引擎的热重载会在下一轮把同一份内容再读一遍，
        // 日志里凭空多出一条「配置已重载」。内存已经是它了，就把观察值对齐到写盘之后。
        if let Ok(m) = std::fs::metadata(&state.config_path) {
            *state.config_mtime.lock().unwrap_or_else(|e| e.into_inner()) = m.modified().ok();
        }
    }
    if let Some(s) = r.settings {
        let lang = match &s.language {
            Some(code) => i18n::Language::from_code(code),
            None => i18n::Language::from_code(
                &crate::platform::system_ui_language().unwrap_or_default(),
            ),
        };
        i18n::set_language(lang);
        log::set_retention_days(s.log_retention_days);
        *state.settings.lock().unwrap_or_else(|e| e.into_inner()) = s;
        // 托盘 tooltip 是当场求值出来的字符串，字典换了它不会自己换（同 `set_language`）。
        if let Some(app) = state.app.get() {
            crate::tray::refresh_tooltip(app);
        }
        log::info(&i18n::tf("notify.language_set", &[("lang", lang.code())]));
    }
    log::info(&msg);
    state::post(&state, Msg::Wake);
    crate::state::publish_status(state.inner());
    // 同 `export_backup`：界面上那句提示就是日志里那句，由后端一次拼好。
    Ok(msg)
}

/// 在系统文件管理器里打开备份目录。
///
/// 目录还没有时先建出来再打开：用户点这颗按钮时想知道的正是「导出以后会落在哪儿」。
#[tauri::command(async)]
pub fn open_backups_folder(state: State<'_, std::sync::Arc<AppState>>) -> Result<(), String> {
    let src = backup_sources(&state);
    let text = crate::backup::ensure(&src)?.display().to_string();
    crate::platform::open_path(&text)
        .map_err(|e| i18n::tf("notify.open_failed", &[("path", &text), ("error", &e)]))
}

/// 当前界面语言的代码（`en` / `zh` / `zh-TW` / `ja` / `ko`）。前端拿它写
/// `<html lang>`：那是给读屏软件和字形选择的，不是给文案查表用的。
#[tauri::command]
pub fn get_language() -> String {
    i18n::current().code().to_string()
}

/// 用系统默认程序打开一个 URL（Release 页 / 下载链接 / 文档）。
/// 直接复用平台层的 `open_path`，与「查看日志」走同一套系统打开器。
#[tauri::command(async)]
pub fn open_url(url: String) -> Result<(), String> {
    crate::platform::open_path(&url)
}

/// 检查 GitHub 上的最新发布版本，与本地版本比较，挑出当前平台最佳安装包，并判断渠道。
///
/// 后端用系统自带的 `curl`（macOS / Linux）或 PowerShell（Windows）拉取
/// `releases/latest`，零额外依赖。走哪条网络出口由软件设置里的代理三态决定（见
/// [`crate::netproxy`]）；这一条命令与随后的 `run_update` 用同一个值，检查与下载不会
/// 一个走代理一个直连。返回的 `download_url` / `asset_name` /
/// `checksum_url` / `size` 直接喂给 `run_update` 走「下载 + 安装」全流程；
/// `homebrew` 为 true 时 `run_update` 改走 `brew upgrade --cask netsense`。
/// `installable` / `install_note` 是同一个问题在检查阶段的答案：这次更新到底能不能
/// 就地装上了（见 [`install_verdict`]），界面据此决定给不给「立即更新」按钮。
/// GitHub API 未认证限速 60 次/小时/IP，对本应用（仅手动点击或启动后静默查一次）足够。
#[tauri::command(async)]
pub fn check_update(state: State<'_, std::sync::Arc<AppState>>) -> Result<serde_json::Value, String> {
    let url = "https://api.github.com/repos/imonior/netsense/releases/latest";
    let choice = proxy_choice_of(&state);
    let body = fetch_url(url, &choice)?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| i18n::tf("upd.bad_json", &[("error", &e.to_string())]))?;
    let latest = v["tag_name"].as_str().unwrap_or("").trim_start_matches('v').to_string();
    let current = env!("CARGO_PKG_VERSION");
    let html_url = v["html_url"].as_str().unwrap_or("").to_string();
    let notes = v["body"].as_str().unwrap_or("").to_string();
    let homebrew = is_homebrew_install();
    let update_available = is_newer(&latest, current);

    let assets: Vec<serde_json::Value> = v["assets"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let name = a["name"].as_str()?;
                    let dl = a["browser_download_url"].as_str()?;
                    let size = a["size"].as_i64();
                    Some(json!({
                        "name": name,
                        "url": dl,
                        "platform": platform_of(name),
                        "size": size,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    // 挑出当前平台的最佳安装包（含 SHA256SUMS 链接，供 run_update 校验）。
    let (download_url, asset_name, checksum_url, size) = pick_install_asset(&v, &assets);

    // 「能不能装、验不验得了」在这里就判完：`run_update` 是 fail-closed 的，校验值拿不到
    // 就拒绝安装，所以让用户点下去才发现，等于把他的一次下载白白花掉。
    let note = if update_available {
        install_verdict(
            homebrew,
            download_url.as_ref(),
            asset_name.as_ref(),
            checksum_url.as_ref(),
            &choice,
        )
    } else {
        // 已是最新：不会有人点安装，不必为一次用不上的判定多发请求。
        None
    };

    Ok(json!({
        "current": current,
        "latest": latest,
        "update_available": update_available,
        "html_url": html_url,
        "notes": notes,
        "homebrew": homebrew,
        "assets": assets,
        "download_url": download_url,
        "asset_name": asset_name,
        "checksum_url": checksum_url,
        "size": size,
        "installable": note.is_none(),
        "install_note": note,
    }))
}

/// 这次更新能不能就地完成。`None` = 能；`Some(码)` = 不能，码是给界面看的
/// （`popup.no_installable` / `popup.cannot_verify`），不参与任何判定逻辑。
fn install_verdict(
    homebrew: bool,
    download_url: Option<&String>,
    asset_name: Option<&String>,
    checksum_url: Option<&String>,
    choice: &crate::netproxy::ProxyChoice,
) -> Option<&'static str> {
    // Homebrew 渠道什么都不下载，来源校验由 brew 自己负责。
    if homebrew {
        return None;
    }
    let name = match (download_url, asset_name) {
        (Some(_), Some(n)) => n,
        _ => return Some("no_asset"),
    };
    let cu = match checksum_url.filter(|u| !u.trim().is_empty()) {
        Some(u) => u,
        None => return Some("no_sums"),
    };
    if crate::update::require_https(cu).is_err() {
        return Some("no_sums");
    }
    // 这一次拉取同时回答两个问题：拿不拿得到、里面列没列我们这个资产。
    match fetch_url(cu, choice) {
        Err(e) => {
            // 重试的 warn 只覆盖中间的尝试（见 [`fetch_url`]），最后一次的失败原因要在
            // 这里落一条 —— 否则用户报「SHA256SUMS 无法下载」时，日志里只有前两次的。
            log::warn(&i18n::tf("upd.sums_fetch", &[("error", &e)]));
            Some("sums_unreachable")
        }
        Ok(sums) if crate::update::parse_hash(&sums, name).is_some() => None,
        Ok(_) => Some("no_sums"),
    }
}

/// 从 release JSON 里挑当前 OS+arch 的安装包，同时取 SHA256SUMS 的下载链接。
/// 平台判定拆出去做纯函数（[`pick_asset_for`]），这样三个平台的挑法在任何一台机器上
/// 都能被同一次 `cargo test` 覆盖到 —— 否则「mac 挑不到包」这类只在某个平台成立的错，
/// 在其余两个平台的 runner 上是测不出来的。
fn pick_install_asset(
    release: &serde_json::Value,
    assets: &[serde_json::Value],
) -> (Option<String>, Option<String>, Option<String>, Option<u64>) {
    let goos = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    let goarch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "other"
    };

    let best = pick_asset_for(assets, goos, goarch);

    let checksum_url = release["assets"]
        .as_array()
        .and_then(|arr| {
            arr.iter().find(|a| {
                let n = a["name"].as_str().unwrap_or("").to_ascii_lowercase();
                matches!(n.as_str(), "sha256sums" | "sha256sums.txt" | "checksums.txt" | "checksums" | "sha256.txt" | "shasums")
            })
        })
        .and_then(|a| a["browser_download_url"].as_str().map(|s| s.to_string()));

    match best {
        Some(a) => (
            a["url"].as_str().map(|s| s.to_string()),
            a["name"].as_str().map(|s| s.to_string()),
            checksum_url,
            a["size"].as_i64().map(|s| s as u64),
        ),
        None => (None, None, checksum_url, None),
    }
}

/// 在 `assets` 里为 `goos`+`goarch` 挑一个装得上的包：先要架构与平台都对得上，在同为
/// 「装得上」的候选里先取带本平台扩展名的，只靠 OS token 认出来的次之。
/// 没有任何候选就返回 `None`，由 `install_verdict` 如实报「本平台没有可安装的包」；
/// 不再退回「不分架构、只要有 OS token 就算」的那一轮 —— 那一轮会在 arm64 机器上挑走
/// x86_64 的包，装不上不说，还显得像是成功找到了。
fn pick_asset_for<'a>(
    assets: &'a [serde_json::Value],
    goos: &str,
    goarch: &str,
) -> Option<&'a serde_json::Value> {
    let candidates: Vec<&serde_json::Value> = assets
        .iter()
        .filter(|a| asset_matches(asset_name(a), goos, goarch))
        .collect();
    candidates
        .iter()
        .find(|a| installable_ext(asset_name(a), goos))
        .or_else(|| candidates.first())
        .copied()
}

fn asset_name(a: &serde_json::Value) -> &str {
    a["name"].as_str().unwrap_or("")
}

fn platform_of(name: &str) -> &'static str {
    let n = name.to_ascii_lowercase();
    if n.contains("mac") || n.contains("darwin") || n.ends_with(".dmg") || n.ends_with(".zip") {
        "macos"
    } else if n.contains("windows") || n.ends_with(".exe") || n.ends_with(".msi") {
        "windows"
    } else if n.ends_with(".deb") || n.ends_with(".rpm") || n.contains("linux") {
        "linux"
    } else {
        "other"
    }
}

/// 文件名是否说明这是当前平台**装得上**的包：先认扩展名，扩展名说不出所以然时才退到
/// OS token。顺序不能反 —— Tauri 的默认产物名（`NetSense_1.0.3_aarch64.dmg`、
/// `..._amd64.deb`）里根本没有 OS token，只按 token 判会让 mac 与 linux 永远挑不到包，
/// 而这两种包正是我们的安装器按扩展名分派处理的（见 `update::install_macos` /
/// `update::install_linux`）。
fn asset_matches(name: &str, goos: &str, goarch: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if !arch_token_present(&n, goarch) {
        return false;
    }
    installable_ext(&n, goos) || os_token_present(&n, goos)
}

/// 各平台「有对应安装代码」的扩展名。未知平台返回 false：宁可挑不到包并如实说
/// 「本平台没有可安装的包」，也不要拿着一个没人能装的文件名往下走。
fn installable_ext(name: &str, goos: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let n = n.as_str();
    match goos {
        "macos" => n.ends_with(".dmg") || n.ends_with(".zip"),
        "windows" => n.ends_with(".exe") || n.ends_with(".msi"),
        "linux" => n.ends_with(".deb") || n.ends_with(".rpm"),
        _ => false,
    }
}

fn arch_token_present(n: &str, goarch: &str) -> bool {
    match goarch {
        "arm64" => token_anchored(n, "arm64") || token_anchored(n, "aarch64"),
        "x86_64" => token_anchored(n, "x86_64") || token_anchored(n, "amd64") || token_anchored(n, "x64"),
        _ => token_anchored(n, goarch),
    }
}

fn os_token_present(n: &str, goos: &str) -> bool {
    match goos {
        "macos" => token_anchored(n, "macos") || token_anchored(n, "mac") || token_anchored(n, "darwin") || token_anchored(n, "osx"),
        "windows" => token_anchored(n, "windows") || token_anchored(n, "win"),
        "linux" => token_anchored(n, "linux"),
        _ => false,
    }
}

/// token 必须出现在 -/_/. 或字符串边界处，防止 `arm` 误命中 `arm64`。
fn token_anchored(n: &str, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let mut idx = 0;
    while let Some(pos) = n[idx..].find(token) {
        let start = idx + pos;
        let end = start + token.len();
        let left_ok = start == 0 || matches!(n.as_bytes()[start - 1], b'-' | b'_' | b'.');
        let right_ok = end == n.len() || matches!(n.as_bytes()[end], b'-' | b'_' | b'.');
        if left_ok && right_ok {
            return true;
        }
        idx = start + 1;
    }
    false
}

/// 前端点击「立即更新」后调用：下载并安装最新版（Homebrew 渠道则 brew upgrade），
/// 过程通过 `netsense://update_progress` 事件广播阶段/进度，失败时返回 Err 供前端
/// 回退到「打开发布页」。
#[tauri::command(async)]
pub fn run_update(
    app: tauri::AppHandle,
    state: State<'_, std::sync::Arc<AppState>>,
    target: String,
) -> Result<(), String> {
    let t: crate::update::UpdateTarget = serde_json::from_str(&target)
        .map_err(|e| i18n::tf("upd.bad_target", &[("error", &e.to_string())]))?;
    let choice = proxy_choice_of(&state);
    crate::update::run_update(&app, &t, &choice)
}

/// [`fetch_url`] 的总尝试次数。三次覆盖的是秒级抖动；真断网就是三次都失败。
const FETCH_ATTEMPTS: u32 = 3;

/// 拉一个 URL 的正文；失败会重试（见 [`FETCH_ATTEMPTS`]）。
///
/// 重试只能放在这里：现场（macOS，LibreSSL）release-assets 的 TLS 握手会偶发
/// `SSL_ERROR_SYSCALL`（三次里两次），而 `curl --retry` 不覆盖 SSL 错误（退出码 35 不在
/// 它的重试列表里），Windows 10 自带的 curl 7.60.1 又不认 `--retry-all-errors`。
///
/// `pub(crate)`：`update.rs` 取 SHA256SUMS 走的是同一个函数，两次对外请求因此
/// 用同一个出口，不会出现「检查更新走了代理、下载没走」。
pub(crate) fn fetch_url(url: &str, choice: &crate::netproxy::ProxyChoice) -> Result<String, String> {
    let mut last_err = String::new();
    for attempt in 1..=FETCH_ATTEMPTS {
        if attempt > 1 {
            std::thread::sleep(std::time::Duration::from_millis(400 * u64::from(attempt - 1)));
        }
        match fetch_url_once(url, choice) {
            Ok(body) => return Ok(body),
            Err(e) => {
                // 中间的失败不吞掉：现场报告只有「SHA256SUMS 无法下载」一句，没有任何可查
                // 的线索 —— 失败原因（TLS？超时？404？）至少要落到日志里。
                if attempt < FETCH_ATTEMPTS {
                    log::warn(&i18n::tf("upd.fetch_retry", &[
                        ("url", url),
                        ("attempt", &attempt.to_string()),
                        ("error", &e),
                    ]));
                }
                last_err = e;
            }
        }
    }
    Err(last_err)
}

/// 一次拉取尝试（不含重试）。两条实现（Windows 的 PowerShell 与其余平台的 curl）都在这里，
/// 因为它们对代理的态度不同 —— 见 [`crate::netproxy`] 与下面那条分支上的注释。
fn fetch_url_once(url: &str, choice: &crate::netproxy::ProxyChoice) -> Result<String, String> {
    #[cfg(windows)]
    {
        // 「跟随系统」继续交给 PowerShell：它按 WinINET 的系统设置走，正是这个选择的意思。
        // 另外两态要求**明确表态**（直连 / 只用这一个地址），而 PowerShell 的 `-Proxy` 不认
        // socks，于是那两态交给 curl.exe —— 下载安装包用的本来就是它。
        if matches!(*choice, crate::netproxy::ProxyChoice::Follow) {
            return fetch_url_powershell(url);
        }
        fetch_url_curl(url, &crate::netproxy::proxy_args(choice, None))
    }
    #[cfg(not(windows))]
    {
        fetch_url_curl(url, &crate::netproxy::curl_proxy_args(choice))
    }
}

#[cfg(windows)]
fn fetch_url_powershell(url: &str) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    // URL 用单引号包住，并把里面的 `'` 按 PowerShell 的规则翻倍：传进来的不总是我们自己拼的
    // 常量（`run_update` 的 `checksum_url` 是前端回传的），不转义就能闭合字符串再拼命令。
    // `@{...}` 在 Rust format! 里用 `{{`/`}}` 转义为字面大括号。
    // 首句把 TLS 1.2 **并入**当前协议集：GitHub 只接受 TLS 1.2 及以上，而 Windows PowerShell 5.1
    // 在老 .NET Framework 上默认不协商它。用 `-bor` 而不是直接赋值，是为了只在现状之上补齐、
    // 绝不让某个本来能用的配置变差。
    //
    // 取正文时不写 `.Content` 就完事，原因有两个，都够让整条更新链路说谎：
    // 1. `.Content` 的类型跟着响应走。GitHub 的 release 资产（`SHA256SUMS`）是
    //    `application/octet-stream`，PowerShell 于是返回 **byte[]**；byte[] 被当字符串打印到
    //    stdout 时会被逐元素展开成一行一个十进制数，而我们随后把这些数字当成清单去解析 ——
    //    解析的结果是「这个资产没在 SHA256SUMS 里」，其实是「我读到的根本不是正文」。
    // 2. 就算正文是字符串，PowerShell 写 stdout 用的仍是控制台代码页（简体中文 Windows 是
    //    cp936），我们这边按 UTF-8 读：发行说明里的中文先被编成 GBK、再被当成 UTF-8 解码，
    //    于是弹窗里出现乱码。
    // 解决办法是让 PowerShell 别做文本转换：把响应**原始字节**直接写进 stdout，代码页就不再
    // 参与这条链路。RawContentStream 是响应体的字节流，不经任何解码；Content 是字符串时
    // 退化成它的 UTF-8 编码。
    let ps = format!(
        "[Net.ServicePointManager]::SecurityProtocol=[Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12;$r=Invoke-WebRequest -Uri '{}' -Headers @{{Accept='application/vnd.github+json'; UserAgent='netsense'}} -TimeoutSec 15 -UseBasicParsing;$b=$null;$s=$r.RawContentStream;if($s){{$b=$s.ToArray()}};if(-not $b){{if($r.Content -is [byte[]]){{$b=$r.Content}}else{{$b=[Text.Encoding]::UTF8.GetBytes([string]$r.Content)}}}};$o=[Console]::OpenStandardOutput();$o.Write($b,0,$b.Length);$o.Flush()",
        url.replace('\'', "''")
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW：避免偶发检查更新时闪出控制台
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// 用 curl 拉一个 URL。`proxy_args` 是代理那一小段参数（见
/// [`crate::netproxy::proxy_args`]），由调用方决定内容，这里只负责拼进去。
fn fetch_url_curl(url: &str, proxy_args: &[String]) -> Result<String, String> {
    use std::process::Command;
    // `--proto =https`：`-L` 跟随时，重定向落点也必须是 https。只校验我们传进来的那个 URL
    // 是不够的 —— 一个 https→http 的跳转会把明文请求送出去。这条筛选不会被重定向放宽
    // （curl 对跳转目标用的是同一份协议集，实测报 `Protocol "http" disabled (in redirect)`）。
    // `--max-time 15`：现场成功拉取的那一次总耗时 8.66s，10s 的旧值把一个只是慢了一点的
    // 连接直接记成失败。与 PowerShell 腿的 `-TimeoutSec` 取同一个数。
    let mut cmd = Command::new("curl");
    cmd.args([
        "-fsSL",
        "--proto",
        "=https",
        "--max-time",
        "15",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: netsense",
    ])
    .args(proxy_args)
    .arg(url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // 同 PowerShell 那条分支：不闪控制台
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// 短超时纯文本 GET：公网 IP 探测专用，不复用 [`fetch_url_curl`] 的 GitHub 专用头与 15s 上限。
/// Short-timeout plain-text GET for the public-IP probe (no GitHub headers, 4s cap).
fn fetch_text(url: &str, proxy_args: &[String]) -> Result<String, String> {
    use std::process::Command;
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--proto", "=https", "--max-time", "5", "-H", "User-Agent: netsense"])
        .args(proxy_args)
        .arg(url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // 不闪控制台 / no console flash
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// 出口公网 IP 是否像样（架构上的 Egress / Public IP 层）：只放行 IPv4 / IPv6 文本，
/// 挡掉任何带空格或异常字符的响应（外部服务被劫持时不能把脏东西喂给面板）。
/// Whether a probed value looks like a public IP: IPv4 / IPv6 text only, rejecting anything odd.
fn is_plausible_ip(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 45
        && s.bytes().all(|b| b == b'.' || b == b':' || b.is_ascii_digit())
}

/// 探测当前出口公网 IP（Egress / Public IP 层，独立于任何网卡）。
///
/// 走系统代理设置（与更新下载同一套出口），短超时，结果只用于面板展示，不落盘、不参与条件匹配。
/// 端点可换（见 [`PUBLIC_IP_URL`]）；取不到时返回 `None`，前端整行收起。
/// Probes the current egress public IP. Respects system proxy, short timeout, display-only.
#[tauri::command]
pub fn get_public_ip(state: State<'_, std::sync::Arc<AppState>>) -> Option<String> {
    // 出口公网 IP 探测：依次尝试多个端点，第一个拿到像样结果即用。
    // 单一第三方端点可能在部分网络下被墙 / 限流 / 超时，链起来更稳；任一端点返回非 IP 文本
    // （如 JSON）会被 `is_plausible_ip` 挡掉、继续下一个。取不到时返回 `None`，前端整行保持
    // 可见并显示「—」（不再整行收起，免得看起来像功能没做）。
    // Probes egress public IP: tries several endpoints in order, first plausible wins. A single
    // third-party host can be blocked/rate-limited/timeout on some networks, so we chain; any
    // non-IP reply is skipped. None => frontend keeps the row visible showing a placeholder.
    const ENDPOINTS: &[&str] = &[
        "https://api.ipify.org",
        "https://checkip.amazonaws.com",
        "https://icanhazip.com",
        "https://wtfismyip.com/text",
    ];
    let choice = proxy_choice_of(&state);
    let proxy = if cfg!(windows) {
        crate::netproxy::curl_proxy_args(&choice)
    } else {
        crate::netproxy::proxy_args(&choice, None)
    };
    for url in ENDPOINTS {
        if let Ok(s) = fetch_text(url, &proxy) {
            let ip = s.trim();
            if is_plausible_ip(ip) {
                return Some(ip.to_string());
            }
        }
    }
    None
}

/// 这一次对外请求走哪个出口（软件设置里的那三态）。
///
/// 「跟随系统」不缓存：每次发请求现问一次操作系统，代理客户端开关与换网络因此立刻生效，
/// 不会留下一个昨天读到的地址。
pub(crate) fn proxy_choice_of(state: &AppState) -> crate::netproxy::ProxyChoice {
    state
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .proxy
        .choice()
}

/// 是否通过 Homebrew 安装（用于给出 `brew upgrade` 而非下载链接）。
///
/// 复用 `update::is_brew_install`，使 `check_update` 回报的渠道与 `run_update` 的实际
/// 路由判定完全同源 —— 否则前端可能显示「下载安装」而实际走了 brew（或反之）。
fn is_homebrew_install() -> bool {
    crate::update::is_brew_install()
}

fn parse_ver(v: &str) -> Vec<u32> {
    v.split('.')
        .filter_map(|p| p.trim().parse::<u32>().ok())
        .collect()
}

/// 语义化版本比较：latest 是否比 current 新（仅比前三个数字段）。
fn is_newer(latest: &str, current: &str) -> bool {
    let a = parse_ver(latest);
    let b = parse_ver(current);
    for i in 0..3 {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        if x != y {
            return x > y;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{asset_matches, install_verdict, installable_ext, pick_asset_for, platform_of, token_anchored};
    use crate::netproxy::ProxyChoice;
    use serde_json::json;

    fn url(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn a_homebrew_install_is_always_installable() {
        // brew 渠道不下载任何东西，也就没有「拿不到校验值」这一说。
        // 这条同时钉住：判定不会为 Homebrew 多发一次请求。
        assert_eq!(install_verdict(true, None, None, None, &ProxyChoice::Direct), None);
    }

    #[test]
    fn a_missing_asset_is_reported_before_checksums_are_considered() {
        assert_eq!(
            install_verdict(
                false,
                None,
                None,
                url("https://example.invalid/SHA256SUMS").as_ref(),
                &ProxyChoice::Direct,
            ),
            Some("no_asset")
        );
    }

    #[test]
    fn a_release_without_checksums_is_not_installable() {
        assert_eq!(
            install_verdict(
                false,
                url("https://example.invalid/NetSense.dmg").as_ref(),
                url("NetSense.dmg").as_ref(),
                None,
                &ProxyChoice::Direct,
            ),
            Some("no_sums")
        );
    }

    #[test]
    fn an_unusable_checksum_link_is_rejected_before_leaving_the_process() {
        // 非 https 的校验链接等同于没有校验链接 —— 而且必须在拨号之前判掉，
        // 否则这个测试就要真的去下载了。
        assert_eq!(
            install_verdict(
                false,
                url("https://example.invalid/NetSense.dmg").as_ref(),
                url("NetSense.dmg").as_ref(),
                url("http://example.invalid/SHA256SUMS").as_ref(),
                &ProxyChoice::Direct,
            ),
            Some("no_sums")
        );
        assert_eq!(
            install_verdict(
                false,
                url("https://example.invalid/NetSense.dmg").as_ref(),
                url("NetSense.dmg").as_ref(),
                url("   ").as_ref(),
                &ProxyChoice::Direct,
            ),
            Some("no_sums")
        );
    }

    /// 1.0.3 那次发布里真实存在的五个资产。名字里**没有** OS token：只按 token 认平台
    /// 的话，mac 与 linux 会一个都挑不出来。
    fn shipped_assets() -> Vec<serde_json::Value> {
        [
            "NetSense_1.0.3_aarch64.dmg",
            "NetSense_1.0.3_amd64.deb",
            "NetSense_1.0.3_x64.dmg",
            "NetSense_1.0.3_x64-setup.exe",
            "NetSense_1.0.3_x64_en-US.msi",
        ]
        .into_iter()
        .map(|n| json!({ "name": n, "url": format!("https://example.invalid/{n}") }))
        .collect()
    }

    #[test]
    fn an_asset_is_matched_by_extension_before_os_token() {
        let assets = shipped_assets();
        // 这三条是 1.0.3 弹窗上那句 "No installable package for this platform" 的正面：
        // mac 的两个架构和 linux 都要能挑到自己那个包。
        assert_eq!(
            pick_asset_for(&assets, "macos", "arm64").unwrap()["name"],
            "NetSense_1.0.3_aarch64.dmg"
        );
        assert_eq!(
            pick_asset_for(&assets, "macos", "x86_64").unwrap()["name"],
            "NetSense_1.0.3_x64.dmg"
        );
        assert_eq!(
            pick_asset_for(&assets, "linux", "x86_64").unwrap()["name"],
            "NetSense_1.0.3_amd64.deb"
        );
        // Windows 的 .exe 与 .msi 都有安装实现（NSIS / msiexec），取清单里先出现的那个。
        assert_eq!(
            pick_asset_for(&assets, "windows", "x86_64").unwrap()["name"],
            "NetSense_1.0.3_x64-setup.exe"
        );
    }

    #[test]
    fn a_wrong_architecture_is_declined_rather_than_downgraded_to() {
        // 钉住被删掉的那轮退让：本架构挑不到就是挑不到，不能拿别的架构的包凑数 ——
        // 凑来的包装不上，却会显得「找到了」。
        let assets = shipped_assets();
        assert_eq!(pick_asset_for(&assets, "windows", "arm64"), None);
        assert_eq!(pick_asset_for(&assets, "macos", "other"), None);
        assert!(!asset_matches("NetSense_1.0.3_x64.dmg", "macos", "arm64"));
    }

    #[test]
    fn a_token_only_name_still_qualifies_but_only_for_its_platform() {
        // 非标准命名（带 darwin 而没有我们的扩展名）走 os token 兜底，架构仍须对得上；
        // 而一个平台都不认的扩展名不该被当成「装得上」。
        let loose = json!({ "name": "netsense-1.0.3-darwin-arm64", "url": "https://example.invalid/a" });
        let assets = vec![loose];
        assert!(asset_matches("netsense-1.0.3-darwin-arm64", "macos", "arm64"));
        assert!(!asset_matches("netsense-1.0.3-darwin-arm64", "macos", "x86_64"));
        assert_eq!(
            pick_asset_for(&assets, "macos", "arm64").unwrap()["name"],
            "netsense-1.0.3-darwin-arm64"
        );
        assert!(!installable_ext("NetSense.zip", "plan9"));
    }

    #[test]
    fn a_token_has_to_sit_on_a_separator_boundary() {
        // 防的是半个词冒充整个词：`arm` 命中 `arm64`、`mac` 命中 `macos`。
        assert!(token_anchored("netsense_x64.dmg", "x64"));
        assert!(token_anchored("netsense_mac.dmg", "mac"));
        assert!(token_anchored("netsense_macos.dmg", "macos"));
        assert!(!token_anchored("netsense_arm64.dmg", "arm"));
        assert!(!token_anchored("netsense_macos.dmg", "mac"));
        assert!(!token_anchored("", "x64"));
        assert!(!token_anchored("netsense_x64.dmg", ""));
    }

    #[test]
    fn the_asset_list_labels_platforms_by_their_installer_kind() {
        // popup 的资产清单用这个标签分组；SHA256SUMS 自己也混在资产里，不能被打成某个平台。
        assert_eq!(platform_of("NetSense_1.0.3_aarch64.dmg"), "macos");
        assert_eq!(platform_of("NetSense_1.0.3_x64-setup.exe"), "windows");
        assert_eq!(platform_of("NetSense_1.0.3_amd64.deb"), "linux");
        assert_eq!(platform_of("SHA256SUMS"), "other");
    }
}
