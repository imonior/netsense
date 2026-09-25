//! 3B 自动化执行层。
//!
//! 分成两类，语义完全不同（方案第 18/23/25 条）：
//!
//! - [`one_shot`]：**一次性动作**。每次「进入 Active」跑一遍，保持 Active 期间的
//!   重新评估不会再跑（否则每 30s 弹一次软件）。
//! - [`persistent`]：**常驻 worker**。跟着 Active 生命周期起停，按 `interval_secs`
//!   周期性调 [`provider`] 出的 `Tick`，把「期望状态」持续维护下去。
//! - [`provider`]：动作**语义**层 —— 一条常驻动作到底「检查什么、怎么补」，
//!   以及它落到具体平台时该调谁。

pub mod one_shot;
pub mod persistent;
pub mod provider;

use std::path::PathBuf;


/// 脚本允许列表：`<config 所在目录>/scripts` 受信目录 + 配置显式登记的路径。
///
/// 这是**安全边界**：Profile 的动作里可以写任意路径，而 `run_script` 能提权。
/// 没有这层校验，一份来路不明的 config.json 就等于一次任意代码执行。
pub struct AllowedScripts {
    pub scripts_dir: PathBuf,
    pub explicit: Vec<String>,
}

impl Default for AllowedScripts {
    fn default() -> Self {
        Self {
            scripts_dir: PathBuf::from("scripts"),
            explicit: Vec::new(),
        }
    }
}

/// 纯字面量地折叠 `.` 与 `..`，不查文件系统。
///
/// 只给 [`canonicalize_best`] 的退化分支用。**为什么必须有它**：`starts_with` 按分量比较，
/// 而没被折叠的 `scripts/../nested/../../x.sh` 的分量序列里，前几段恰好是
/// `<配置目录>/scripts` —— 一次字面前缀命中就把「指向受信目录之外」的路径放行了，
/// 而 exec 时操作系统会按 `..` 真的走到外面那份文件。两侧解析必须同样严格。
fn collapse_dots(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    // out 末尾可被 `..` 弹出的普通分量数；用它保证 `..` 不会穿过锚点（根/盘符）。
    let mut depth = 0usize;
    let mut anchored = false;
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if depth > 0 {
                    out.pop();
                    depth -= 1;
                } else if !anchored {
                    // 相对写法开头的 `..` 是语义的一部分，保留下来；
                    // 它同样不可能与绝对受信目录前缀相等。
                    out.push(c.as_os_str());
                }
            }
            Component::Normal(_) => {
                out.push(c.as_os_str());
                depth += 1;
            }
            Component::RootDir | Component::Prefix(_) => {
                out.push(c.as_os_str());
                anchored = true;
            }
        }
    }
    out
}

/// 尽量把路径规约到规范形式。
///
/// 不能直接 `canonicalize` 就算了之：目标文件常常尚未落盘（配置里登记、稍后才生成），
/// 此时 canonicalize 会失败。退化为「父目录 canonicalize + 文件名」即可保持一致——
/// 重要的是**两侧用同一套规则**，否则 macOS 上 `/var`（实为 `/private/var` 符号链接）
/// 这类差异会让合法路径被误判为非法。
///
/// 父目录也解析不动时（整条链都还没落盘），退到 [`collapse_dots`]：字面折叠至少是
/// 确定的，不依赖这条路径在某个平台上能不能解析。
fn canonicalize_best(path: &std::path::Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(p) = std::fs::canonicalize(parent) {
            return p.join(name);
        }
    }
    collapse_dots(path)
}

/// 配置里的脚本路径 → 执行用的绝对路径。
///
/// 「相对于哪里」必须在一处定死。留给进程 CWD 去解释，等于让「启动时所在目录」决定
/// 跑哪个脚本，而允许列表按 `<config 目录>/scripts` 判定 —— 两侧一旦落在不同目录，
/// 通过校验的文件和真正 exec 的文件就不是同一份。
///
/// 规则：绝对路径原样返回；相对路径按配置目录（`scripts_dir` 的父目录）展开，所以配置
/// 里写 `scripts/x.sh` 命中受信目录，写 `x.sh` 落在配置目录、会被拒绝。
pub fn resolve_script(path: &str, allowed: &AllowedScripts) -> PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    match allowed.scripts_dir.parent() {
        Some(base) if !base.as_os_str().is_empty() => base.join(p),
        // scripts_dir 自己就是相对路径时（配置从工作目录读），两侧同按 CWD 解释，
        // 保持解析一致比强行凑出绝对路径更重要。
        _ => p.to_path_buf(),
    }
}

/// allow-list 校验：脚本路径在显式登记列表内，或位于 scripts/ 受信目录下，才放行。
///
/// 入参必须是 [`resolve_script`] 的输出：校验一个路径、执行另一个路径的写法，这道
/// 边界等于不存在。
///
/// ⚠️ `Path::starts_with` 按**路径分量**比较，天然带分隔边界，所以 `.../scripts2/evil.sh`
/// 不会被 `.../scripts` 误判为前缀。旧实现在此额外补了一段裸字符串 `starts_with` 兜底，
/// 恰恰绕过了这层保护：任何时候都不要再退回字符串前缀比较。
pub fn is_script_allowed(resolved: &std::path::Path, allowed: &AllowedScripts) -> bool {
    let canon = canonicalize_best(resolved);
    if allowed
        .explicit
        .iter()
        .any(|p| canonicalize_best(std::path::Path::new(p)) == canon)
    {
        return true;
    }
    let dir = canonicalize_best(&allowed.scripts_dir);
    canon.starts_with(&dir)
}

/// 主网卡 = 走默认路由的那张（排除 VPN 隧道）。没有默认路由时退回第一张非 VPN 网卡。
///
/// 「将当前网络设为 DHCP」必须作用于这张网卡：写死无线网卡时，插着网线的用户点它，
/// 改动落在一台没在用的网卡上，看起来就像「点了没反应」。
pub fn primary_nic(nics: &[crate::platform::NicInfo]) -> Option<&crate::platform::NicInfo> {
    let mut live = nics
        .iter()
        .filter(|n| n.kind != crate::platform::NicKind::Vpn);
    live.clone()
        .find(|n| n.gateway.is_some())
        .or_else(|| live.next())
}

#[cfg(test)]
mod tests {
    use super::{is_script_allowed, resolve_script, AllowedScripts};
    use std::path::{Path, PathBuf};

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
        assert!(is_script_allowed(&trusted.join("ok.sh"), &allowed));
        // 关键：兄弟目录名字以 scripts 开头，不得被前缀比较放过
        assert!(!is_script_allowed(&elsewhere.join("evil.sh"), &allowed));
        assert!(!is_script_allowed(&base.join("evil.sh"), &allowed));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn explicit_entries_bypass_directory_membership() {
        let mut allowed = sut("/nonexistent/nope");
        // 显式登记要求**整条路径相等**，不是前缀
        assert!(!is_script_allowed(Path::new("/nonexistent/nope-other/x.sh"), &allowed));
        allowed.explicit = vec!["/opt/ops/apply.sh".to_string()];
        assert!(is_script_allowed(Path::new("/opt/ops/apply.sh"), &allowed));
        assert!(!is_script_allowed(Path::new("/opt/ops/apply.sh.bak"), &allowed));
    }

    /// 相对路径的基准必须写死在一处：allow-list 判的文件和 exec 跑的文件是同一个。
    /// 配置里的 `scripts/x.sh` 就是「配置目录旁边的那个」，与进程从哪儿启动无关。
    #[test]
    fn relative_scripts_are_anchored_to_the_config_directory() {
        let base = std::env::temp_dir().join(format!("ns-rel-{}", std::process::id()));
        let trusted = base.join("scripts");
        std::fs::create_dir_all(trusted.join("nested")).unwrap();

        let allowed = sut(trusted.to_str().unwrap());
        // 配置目录下的相对写法 → 落在受信目录，允许
        let ok = resolve_script("scripts/office-vpn.sh", &allowed);
        assert_eq!(ok, base.join("scripts/office-vpn.sh"));
        assert!(is_script_allowed(&ok, &allowed));
        // 裸文件名 → 落在配置目录，不在受信目录内，拒绝（而不是悄悄按 CWD 找）
        let bare = resolve_script("office-vpn.sh", &allowed);
        assert_eq!(bare, base.join("office-vpn.sh"));
        assert!(!is_script_allowed(&bare, &allowed));
        // `..` 也不能用来逃出去
        let up = resolve_script("scripts/../nested/../../escape.sh", &allowed);
        assert!(!is_script_allowed(&up, &allowed));
        // 绝对路径原样保留（显式登记那条路径依赖这个）。
        // 根按平台取：Windows 上 `/opt/...` 缺盘符，按 Windows 的规则本来就不算绝对路径，
        // 会被上面的相对分支重新锚到当前盘符的配置目录下 —— 这正是 resolve_script 的语义，
        // 拿它当「绝对路径」断言只会得到一次平台相关的失败。
        let abs = if cfg!(windows) { "C:/opt/ops/apply.sh" } else { "/opt/ops/apply.sh" };
        assert_eq!(resolve_script(abs, &allowed), Path::new(abs));
        // 受信目录本身是相对路径时（配置从工作目录读），不强改成绝对，
        // 因为 exec 也按 CWD 解释，一致比绝对更重要
        let rel = sut("scripts");
        assert_eq!(resolve_script("scripts/x.sh", &rel), Path::new("scripts/x.sh"));

        std::fs::remove_dir_all(&base).ok();
    }

    /// 整条链都还没落盘时（配置登记了稍后才生成的脚本），文件系统给不出任何规约，
    /// 折叠只能按字面做。这条断言必须与临时目录、符号链接、平台分隔符都无关，
    /// 否则「`..` 逃不出受信目录」这件事就只在某些平台上成立。
    #[test]
    fn a_path_that_does_not_exist_yet_still_cannot_escape() {
        let allowed = sut("/nonexistent-base/scripts");
        assert!(!is_script_allowed(
            Path::new("/nonexistent-base/scripts/../../escape.sh"),
            &allowed
        ));
        assert!(!is_script_allowed(
            Path::new("/nonexistent-base/scripts/sub/../../escape.sh"),
            &allowed
        ));
        // 折叠后仍在受信目录内的，照旧放行
        assert!(is_script_allowed(
            Path::new("/nonexistent-base/scripts/./sub/x.sh"),
            &allowed
        ));
    }

    /// macOS 上临时目录是 `/var/...`，而它其实是 `/private/var/...` 的符号链接：
    /// 配置里登记哪一种写法都必须放行，这要求校验两侧走同一套规约规则。
    #[test]
    fn a_symlinked_base_directory_does_not_split_the_two_sides() {
        let base = std::env::temp_dir().join(format!("ns-link-{}", std::process::id()));
        let trusted = base.join("scripts");
        std::fs::create_dir_all(&trusted).unwrap();
        let real = std::fs::canonicalize(&trusted).unwrap();

        let mut allowed = sut(trusted.to_str().unwrap());
        assert!(is_script_allowed(&real.join("ok.sh"), &allowed));
        allowed.scripts_dir = real.clone();
        assert!(is_script_allowed(&trusted.join("ok.sh"), &allowed));

        std::fs::remove_dir_all(&base).ok();
    }
}
