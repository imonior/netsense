//! Tauri 命令（替代原 hs.urlevent）。

use tauri::{Manager, State};
use serde_json::json;

use crate::AppState;
use crate::config::Profile;
use crate::i18n;
use crate::log;
// `list_known_ssids` 现在是 PAL trait 方法，需把 trait 引入作用域
use crate::platform::NetworkPlatform as _;

/// 前端读取当前网络状态 + 已加载 profile 名列表 + 当前语言 + 提权通道。
#[tauri::command]
pub fn get_status(state: State<'_, std::sync::Arc<AppState>>) -> String {
    let payload = crate::status_payload(&state);
    serde_json::to_string(&payload).unwrap_or_default()
}

/// 前端读取完整配置（profile 全量字段），供编辑器加载与回填。
#[tauri::command]
pub fn get_config(state: State<'_, std::sync::Arc<AppState>>) -> String {
    let cfg = state.config.lock().unwrap();
    let payload = json!({
        "language": cfg.language,
        "profiles": cfg.profiles,
        "config_path": state.config_path.display().to_string(),
    });
    serde_json::to_string(&payload).unwrap_or_default()
}

/// 删除一个 profile 并写回磁盘。
#[tauri::command]
pub fn delete_profile(
    state: State<'_, std::sync::Arc<AppState>>,
    name: String,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        if !cfg.profiles.contains_key(&name) {
            return Err(format!("unknown profile: {}", name));
        }
        let mut next = cfg.clone();
        next.profiles.remove(&name);
        next.save(&state.config_path)?;
        *cfg = next;
    }
    crate::publish_status(&state);
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

/// 退出应用（同时停掉 SSID 监视线程）。
#[tauri::command]
pub fn quit_app(state: State<'_, std::sync::Arc<AppState>>) {
    if let Some(h) = state.watcher.lock().unwrap().as_ref() {
        h.stop();
    }
    if let Some(app) = state.app.get() {
        app.exit(0);
    }
}

/// 前端保存/新增一个 profile（替代 save_wifi_scene）。payload: { "name": "...", "profile": {...} }
#[tauri::command]
pub fn save_scene(
    state: State<'_, std::sync::Arc<AppState>>,
    payload: String,
) -> Result<(), String> {
    let v: serde_json::Value = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .ok_or("missing name")?
        .to_string();
    let profile_val = v
        .get("profile")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let profile: Profile = serde_json::from_value(profile_val).map_err(|e| e.to_string())?;

    // 事务式更新：先在校验通过的副本上改，落盘成功后再替换内存态，
    // 避免"校验/落盘失败但内存已被改脏"。（Config 的 Clone 成本可忽略）
    let mut cfg = state.config.lock().unwrap();
    let mut next = cfg.clone();
    next.profiles.insert(name, profile);
    next.validate()?;
    next.save(&state.config_path)?;
    *cfg = next;
    drop(cfg);
    crate::publish_status(&state);
    Ok(())
}

/// 前端强制应用某个 profile（替代 force_apply_network）。payload: { "name": "..." }
#[tauri::command]
pub fn force_apply(
    state: State<'_, std::sync::Arc<AppState>>,
    payload: String,
) -> Result<(), String> {
    let v: serde_json::Value = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .ok_or("missing name")?
        .to_string();
    {
        let cfg = state.config.lock().unwrap();
        if cfg.profiles.get(&name).is_none() {
            return Err(format!("unknown profile: {}", name));
        }
    }
    crate::apply_named(&*state, &name, true);
    Ok(())
}

/// 前端读取已保存无线网络列表（填充编辑器下拉）。
#[tauri::command]
pub fn get_networks(state: State<'_, std::sync::Arc<AppState>>) -> Vec<String> {
    state.plat.list_known_ssids().unwrap_or_default()
}

/// 切换 UI 语言并写回配置（language 字段）。
/// 注意：该改动会被热重载线程感知，但因为 profile 内容未变，
/// `apply_named` 的幂等保护会跳过网络重设，不会触发二次提权。
#[tauri::command]
pub fn set_language(
    state: State<'_, std::sync::Arc<AppState>>,
    code: String,
) -> Result<(), String> {
    let lang = i18n::Language::from_code(&code);
    i18n::set_language(lang);
    {
        let mut cfg = state.config.lock().unwrap();
        let mut next = cfg.clone();
        next.language = Some(lang.code().to_string());
        next.save(&state.config_path)?;
        *cfg = next;
    }
    log::info(&i18n::tf("notify.language_set", &[("lang", lang.code())]));
    // 语言是纯 UI 状态：热重载会因 profile 内容未变而跳过重设，所以这里显式广播一次
    crate::publish_status(&state);
    Ok(())
}

/// 前端一次性拉取当前语言的全部文案，避免逐 key 往返 invoke。
#[tauri::command]
pub fn get_strings() -> std::collections::HashMap<String, String> {
    i18n::all_strings()
}
