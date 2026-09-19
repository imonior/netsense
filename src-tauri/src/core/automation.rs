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

/// 尽量把路径规约到规范形式。
///
/// 不能直接 `canonicalize` 就算了之：目标文件常常尚未落盘（配置里登记、稍后才生成），
/// 此时 canonicalize 会失败。退化为「父目录 canonicalize + 文件名」即可保持一致——
/// 重要的是**两侧用同一套规则**，否则 macOS 上 `/var`（实为 `/private/var` 符号链接）
/// 这类差异会让合法路径被误判为非法。
fn canonicalize_best(path: &std::path::Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(p) = std::fs::canonicalize(parent) {
            return p.join(name);
        }
    }
    path.to_path_buf()
}

/// allow-list 校验：脚本路径在显式登记列表内，或位于 scripts/ 受信目录下，才放行。
///
/// ⚠️ `Path::starts_with` 按**路径分量**比较，天然带分隔边界，所以 `.../scripts2/evil.sh`
/// 不会被 `.../scripts` 误判为前缀。旧实现在此额外补了一段裸字符串 `starts_with` 兜底，
/// 恰恰绕过了这层保护：任何时候都不要再退回字符串前缀比较。
pub fn is_script_allowed(path: &str, allowed: &AllowedScripts) -> bool {
    if allowed.explicit.iter().any(|p| p == path) {
        return true;
    }
    let canon = canonicalize_best(std::path::Path::new(path));
    let dir = canonicalize_best(&allowed.scripts_dir);
    canon.starts_with(&dir)
}

#[cfg(test)]
mod tests {
    use super::{is_script_allowed, AllowedScripts};
    use std::path::PathBuf;

    fn sut(dir: &str) -> AllowedScripts {
        AllowedScripts {
            scripts_dir: PathBuf::from(dir),
            explicit: vec![],
        }
    }

    #[test]
    fn trusts_only_paths_inside_the_scripts_dir() {
        let base = std::env::temp_dir().join(format!("ns-allow-{}", std::process::id()));
        let trusted = base.join("scripts");
        let elsewhere = base.join("scripts2");
        std::fs::create_dir_all(&trusted).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();

        let allowed = sut(trusted.to_str().unwrap());
        // 受信目录内 → 放行
        assert!(is_script_allowed(trusted.join("ok.sh").to_str().unwrap(), &allowed));
        // 关键：兄弟目录名字以 scripts 开头，不得被前缀比较放过
        assert!(!is_script_allowed(elsewhere.join("evil.sh").to_str().unwrap(), &allowed));
        // 受信目录之外 → 拒绝
        assert!(!is_script_allowed(base.join("evil.sh").to_str().unwrap(), &allowed));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn explicit_entries_bypass_directory_membership() {
        let mut allowed = sut("/nonexistent/nope");
        // 显式登记要求**整串相等**，不是前缀
        assert!(!is_script_allowed("/nonexistent/nope-other/x.sh", &allowed));
        allowed.explicit = vec!["/opt/ops/apply.sh".to_string()];
        assert!(is_script_allowed("/opt/ops/apply.sh", &allowed));
        assert!(!is_script_allowed("/opt/ops/apply.sh.bak", &allowed));
    }
}
