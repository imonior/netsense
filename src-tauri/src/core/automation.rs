//! 自动化任务执行器：netsetman 式 route / launch / run，run 受 allow-list 约束。

use crate::config::AutomationAction;
use crate::platform::NetworkPlatform;
use std::path::PathBuf;

/// 脚本允许列表：scripts/ 受信目录 + 配置显式登记路径。
pub struct AllowedScripts {
    pub scripts_dir: PathBuf,
    pub explicit: Vec<String>,
}

/// 执行一组动作（on_apply / on_revert）。返回每个动作的结果。
pub fn run_actions<P: NetworkPlatform>(
    plat: &P,
    actions: &[AutomationAction],
    allowed: &AllowedScripts,
) -> Vec<Result<(), String>> {
    actions
        .iter()
        .map(|a| match a {
            AutomationAction::Route {
                dest,
                gateway,
                metric,
                delete,
            } => {
                if *delete {
                    plat.delete_route(dest)
                } else {
                    match gateway {
                        Some(g) => plat.add_route(dest, g, *metric),
                        None => Err("route add 需要 gateway".into()),
                    }
                }
            }
            AutomationAction::Launch { app, args } => plat.launch_app(app, args),
            AutomationAction::Run {
                path,
                args,
                elevated,
            } => {
                if is_script_allowed(path, allowed) {
                    plat.run_script(path, args, *elevated)
                } else {
                    Err(format!("脚本不在允许列表，已拒绝执行: {}", path))
                }
            }
        })
        .collect()
}

/// allow-list 校验：脚本路径在显式登记列表内，或位于 scripts/ 受信目录下，才放行。
pub fn is_script_allowed(path: &str, allowed: &AllowedScripts) -> bool {
    if allowed.explicit.iter().any(|p| p == path) {
        return true;
    }
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| std::path::PathBuf::from(path));
    if canon.starts_with(&allowed.scripts_dir) {
        return true;
    }
    if let Some(s) = canon.to_str() {
        if let Some(prefix) = allowed.scripts_dir.to_str() {
            if s.starts_with(prefix) {
                return true;
            }
        }
    }
    false
}
