//! Tauri 命令：前端与引擎之间的唯一入口。
//!
//! 两条纪律，都是踩过坑换来的：
//!
//! 1. **会做 I/O 或可能阻塞的命令一律标 `async`**。不带 async 的 Tauri 命令跑在
//!    **主线程**上，而这里几乎每个命令都要采样网络或落盘；主线程一卡，整个 UI
//!    （连同正在展开的托盘菜单）就冻住。
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
use crate::platform::NetworkPlatform as _;
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

/// 前端读取全部在用网卡（有线 / 无线 / VPN），供面板与设置窗口的「网络硬件信息」列展示。
///
/// `async` 的理由同 `get_status`：枚举网卡要拉起子进程（平台层内有 TTL 缓存，
/// 但缓存缺失的那一次仍是 I/O），不能占住主线程。
#[tauri::command(async)]
pub fn get_interfaces(state: State<'_, std::sync::Arc<AppState>>) -> String {
    let nics = state.plat.list_interfaces();
    serde_json::to_string(&nics).unwrap_or_else(|_| "[]".to_string())
}

/// 前端触发「将当前网络设置成 DHCP」（与托盘菜单同一入口）。
#[tauri::command(async)]
pub fn force_dhcp(state: State<'_, std::sync::Arc<AppState>>) -> Result<(), String> {
    state::post(&state, Msg::Action { kind: ActionKind::SetDhcp });
    Ok(())
}

/// 前端触发「强制探测当前网络」（与托盘菜单同一入口）。
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
        "language": cfg.language,
        "profiles": cfg.profiles,
        "fallback": cfg.fallback,
        "allowed_scripts": cfg.allowed_scripts,
        "config_path": state.config_path.display().to_string(),
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
        return Err("profile 缺少 id".to_string());
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
    let fallback: Option<FallbackConfig> = serde_json::from_value(
        patch.get("fallback").cloned().unwrap_or(serde_json::Value::Null),
    )
    .map_err(|e| format!("fallback: {}", e))?;
    let scripts: Vec<String> = patch
        .get("allowed_scripts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    {
        let mut cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = cfg.clone();
        next.fallback = fallback;
        next.allowed_scripts = scripts;
        next.validate()?;
        next.save(&state.config_path)?;
        *cfg = next;
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
            return Err(format!("unknown profile: {}", id));
        }
        next.save(&state.config_path)?;
        *cfg = next;
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

/// 退出应用（同时停掉 SSID 监视线程与引擎线程）。
#[tauri::command]
pub fn quit_app(state: State<'_, std::sync::Arc<AppState>>) {
    if let Some(app) = state.app.get() {
        // 必须走 request_quit：它会置位 QUITTING，否则 run() 里的
        // ExitRequested 守卫会把这次退出也拦下（应用将无法退出）。
        state::request_quit(app, &state);
    }
}

/// 用系统默认程序打开日志目录（与托盘菜单「查看日志」同一入口）。
#[tauri::command]
pub fn open_logs() -> Result<(), String> {
    let dir = crate::log::log_dir();
    crate::platform::open_path(&dir.display().to_string())
        .map_err(|e| format!("打开日志目录失败 {}: {}", dir.display(), e))
}

/// 前端读取已保存无线网络列表（填充编辑器下拉）。
///
/// `async` 不是可选项：`list_known_ssids` 会拉起系统子进程（macOS 上
/// `networksetup -listpreferredwirelessnetworks`），而非 async 命令跑在主线程，
/// 编辑器一打开就会把主线程按住几百毫秒（见 DEVELOPMENT.md §9.6 第 5 条）。
#[tauri::command(async)]
pub fn get_networks(state: State<'_, std::sync::Arc<AppState>>) -> Vec<String> {
    state.plat.list_known_ssids().unwrap_or_default()
}

/// 切换 UI 语言并写回配置（language 字段）。
///
/// 不在这里广播：落盘后引擎会因为 mtime 变化重新加载（语言偏好本就存在配置里），
/// 由那一次重载统一广播。
/// 另外语言变化不会触发网络重设 —— 「立即应用」与匹配都按内容指纹幂等。
#[tauri::command(async)]
pub fn set_language(
    state: State<'_, std::sync::Arc<AppState>>,
    code: String,
) -> Result<(), String> {
    let lang = i18n::Language::from_code(&code);
    i18n::set_language(lang);
    {
        let mut cfg = state.config.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = cfg.clone();
        next.language = Some(lang.code().to_string());
        next.save(&state.config_path)?;
        *cfg = next;
    }
    log::info(&i18n::tf("notify.language_set", &[("lang", lang.code())]));
    state::post(&state, Msg::Wake);
    Ok(())
}

/// 前端一次性拉取当前语言的全部文案，避免逐 key 往返 invoke。
#[tauri::command]
pub fn get_strings() -> std::collections::HashMap<String, String> {
    i18n::all_strings()
}

/// 用系统默认程序打开一个 URL（Release 页 / 下载链接 / 文档）。
/// 直接复用平台层的 `open_path`，与「查看日志」走同一套系统打开器。
#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    crate::platform::open_path(&url)
}

/// 检查 GitHub 上的最新发布版本，与本地版本比较，挑出当前平台最佳安装包，并判断渠道。
///
/// 后端用系统自带的 `curl`（macOS / Linux）或 PowerShell（Windows）拉取
/// `releases/latest`，零额外依赖。返回的 `download_url` / `asset_name` /
/// `checksum_url` / `size` 直接喂给 `run_update` 走「下载 + 安装」全流程；
/// `homebrew` 为 true 时 `run_update` 改走 `brew upgrade --cask netsense`。
/// `installable` / `install_note` 是同一个问题在检查阶段的答案：这次更新到底能不能
/// 就地装上了（见 [`install_verdict`]），界面据此决定给不给「立即更新」按钮。
/// GitHub API 未认证限速 60 次/小时/IP，对本应用（仅手动点击或启动后静默查一次）足够。
#[tauri::command(async)]
pub fn check_update() -> Result<serde_json::Value, String> {
    let url = "https://api.github.com/repos/imonior/netsense/releases/latest";
    let body = fetch_url(url)?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("bad json: {}", e))?;
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
        install_verdict(homebrew, download_url.as_ref(), asset_name.as_ref(), checksum_url.as_ref())
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
    match fetch_url(cu) {
        Err(_) => Some("sums_unreachable"),
        Ok(sums) if crate::update::parse_hash(&sums, name).is_some() => None,
        Ok(_) => Some("no_sums"),
    }
}

/// 从 release JSON 里挑当前 OS+arch 的安装包：优先 .dmg/.zip（mac）/ .exe/.msi（win）/
/// .deb/.rpm（linux），其次任意带 OS+arch token 的文件；同时取 SHA256SUMS 的下载链接。
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

    let mut candidates: Vec<&serde_json::Value> = assets
        .iter()
        .filter(|a| asset_matches(a["name"].as_str().unwrap_or(""), goos, goarch))
        .collect();
    // 没有精确匹配就退而求其次：只要有当前 OS 的 token 即可。
    if candidates.is_empty() {
        candidates = assets
            .iter()
            .filter(|a| os_token_present(a["name"].as_str().unwrap_or(""), goos))
            .collect();
    }
    let best = candidates
        .iter()
        .find(|a| preferred_ext(a["name"].as_str().unwrap_or(""), goos))
        .or_else(|| candidates.first());

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

fn preferred_ext(name: &str, goos: &str) -> bool {
    let n = name.to_ascii_lowercase();
    match goos {
        "macos" => n.ends_with(".dmg") || n.ends_with(".zip"),
        "windows" => n.ends_with(".exe") || n.ends_with(".msi"),
        "linux" => n.ends_with(".deb") || n.ends_with(".rpm"),
        _ => true,
    }
}

/// 文件名是否同时含 OS token 与 arch token（锚定在 -/_/. 边界，避免 arm 命中 arm64）。
fn asset_matches(name: &str, goos: &str, goarch: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if !arch_token_present(&n, goarch) {
        return false;
    }
    os_token_present(&n, goos)
        // Windows 资产常不带 OS token（如 netsense-1.0.0-x86_64-installer.exe），
        // arch 已证明是运行架构，带 .exe/.msi 即可认定为 Windows 包。
        || (goos == "windows" && (n.ends_with(".exe") || n.ends_with(".msi")))
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
pub fn run_update(app: tauri::AppHandle, target: String) -> Result<(), String> {
    let t: crate::update::UpdateTarget =
        serde_json::from_str(&target).map_err(|e| format!("bad update target: {e}"))?;
    crate::update::run_update(&app, &t)
}

#[cfg(windows)]
pub(crate) fn fetch_url(url: &str) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    // URL 用单引号包住，并把里面的 `'` 按 PowerShell 的规则翻倍：传进来的不总是我们自己拼的
    // 常量（`run_update` 的 `checksum_url` 是前端回传的），不转义就能闭合字符串再拼命令。
    // `@{...}` 在 Rust format! 里用 `{{`/`}}` 转义为字面大括号。
    let ps = format!(
        "(Invoke-WebRequest -Uri '{}' -Headers @{{Accept='application/vnd.github+json'; UserAgent='netsense'}} -TimeoutSec 10 -UseBasicParsing).Content",
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

#[cfg(not(windows))]
pub(crate) fn fetch_url(url: &str) -> Result<String, String> {
    let out = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "10",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: netsense",
            url,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
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
    use super::install_verdict;

    fn url(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn a_homebrew_install_is_always_installable() {
        // brew 渠道不下载任何东西，也就没有「拿不到校验值」这一说。
        // 这条同时钉住：判定不会为 Homebrew 多发一次请求。
        assert_eq!(install_verdict(true, None, None, None), None);
    }

    #[test]
    fn a_missing_asset_is_reported_before_checksums_are_considered() {
        assert_eq!(
            install_verdict(
                false,
                None,
                None,
                url("https://example.invalid/SHA256SUMS").as_ref()
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
                None
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
                url("http://example.invalid/SHA256SUMS").as_ref()
            ),
            Some("no_sums")
        );
        assert_eq!(
            install_verdict(
                false,
                url("https://example.invalid/NetSense.dmg").as_ref(),
                url("NetSense.dmg").as_ref(),
                url("   ").as_ref()
            ),
            Some("no_sums")
        );
    }
}
