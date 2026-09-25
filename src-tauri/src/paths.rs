//! 用户级数据位置：配置与日志落在系统为每个用户准备的目录里，而不是程序目录。
//!
//! 为什么不写在 exe 同级：程序目录在三个平台上都可能是**只读**的，或者不该被写的。
//! macOS 的 `NetSense.app` 是一个整体被签名过的包，往 `Contents/MacOS/` 里写
//! `config.json` 与 `logs/` 等于修改自己的 bundle（校验、备份、同一台机器上的第二个
//! 用户都会因此出错）；Windows 的 perMachine 安装直接把程序放在 `C:\Program Files`，
//! 普通用户进程没有写权限；Linux 的 `/opt` 与 `/usr` 同理。
//! 反过来，用户目录天然是「每个用户一份、始终可写、卸载程序也留着」的位置 ——
//! 配置与日志正是这种东西。
//!
//! 程序目录仍然被认作**次级来源**：开发时把 `config.json` 放在可执行文件旁边照样生效
//! （见 [`config_path`] 的优先级），这样「仓库里带一份配置跑一跑」不需要先改环境变量。

use std::path::{Path, PathBuf};

/// 自动化配置的文件名（三平台一致）。
pub const CONFIG_FILE: &str = "config.json";
/// 软件配置的文件名（三平台一致）。
pub const SETTINGS_FILE: &str = "settings.json";

/// `$HOME`，POSIX 两侧的用户目录都从它拼出来。
#[allow(dead_code)] // Windows 腿用不到：那里两个目录直接来自 %APPDATA% / %LOCALAPPDATA%
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// 环境变量里的目录，仅当它是绝对路径时才可信（相对值会随 CWD 漂移）。
#[allow(dead_code)] // 只有 Windows / Linux 腿走它：macOS 两个目录都从 $HOME 拼出来
fn from_env(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// 每个用户的配置目录：macOS `~/Library/Application Support/NetSense`、
/// Windows `%APPDATA%\NetSense`、Linux `$XDG_CONFIG_HOME/netsense`（缺省 `~/.config/netsense`）。
pub fn user_config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        from_env("APPDATA").map(|p| p.join("NetSense"))
    }
    #[cfg(target_os = "macos")]
    {
        home().map(|p| p.join("Library").join("Application Support").join("NetSense"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        from_env("XDG_CONFIG_HOME")
            .or_else(|| home().map(|p| p.join(".config")))
            .map(|p| p.join("netsense"))
    }
}

/// 每个用户的日志目录：macOS `~/Library/Logs/NetSense`、Windows
/// `%LOCALAPPDATA%\NetSense\logs`、Linux `$XDG_STATE_HOME/netsense/logs`
/// （缺省 `~/.local/state/netsense/logs`）。
///
/// 与配置目录分开是各平台自己的惯例：日志是可丢弃的运行痕迹，macOS 甚至为它留了
/// `~/Library/Logs` 这个由「控制台」应用统一浏览的位置。
pub fn user_log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        from_env("LOCALAPPDATA")
            .map(|p| p.join("NetSense").join("logs"))
    }
    #[cfg(target_os = "macos")]
    {
        home().map(|p| p.join("Library").join("Logs").join("NetSense"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        from_env("XDG_STATE_HOME")
            .or_else(|| home().map(|p| p.join(".local").join("state")))
            .map(|p| p.join("netsense").join("logs"))
    }
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// 本次运行使用哪个配置文件。
///
/// 优先级是「用户的那份赢」：
/// 1. 用户配置目录里已存在的 `config.json` —— 界面上任何一次保存都写到这里；
/// 2. 可执行文件同级的 `config.json`（仅当它存在）—— 开发期与便携运行时的入口；
/// 3. 都没有时返回用户目录那个**尚不存在**的路径：首次运行不是错误（`app.config_absent`
///    按这条打日志），而第一次保存会在那里把文件建出来。
///
/// 反过来的顺序就是 bug：随包带一份默认配置时，用户改过的设置每次启动都会被它盖掉。
pub fn config_path() -> PathBuf {
    let in_user = user_config_dir().map(|d| d.join(CONFIG_FILE));
    if let Some(p) = &in_user {
        if p.is_file() {
            return p.clone();
        }
    }
    if let Some(dir) = exe_dir() {
        let p = dir.join(CONFIG_FILE);
        if p.is_file() {
            return p;
        }
    }
    // 连用户目录都解析不出来（环境里没有 HOME）才退到相对路径：
    // 它按 CWD 解释，但至少进程还能起来、还能把它在用的路径告诉用户。
    in_user.unwrap_or_else(|| PathBuf::from(CONFIG_FILE))
}

/// 软件配置文件的位置（见 [`crate::appconfig`]）。
///
/// 与 [`config_path`] 的差别是刻意的：**没有**「可执行文件同级」那一档。那份兜底是为
/// 「仓库里带一份自动化配置跑一跑」准备的，而语言与日志保留天数是**这台机器上这个用户**
/// 的偏好，不是项目内容 —— 从源码树里带一份出来，等于让仓库替每个用户决定他的界面语言。
pub fn settings_path() -> PathBuf {
    user_config_dir()
        .map(|d| d.join(SETTINGS_FILE))
        .unwrap_or_else(|| PathBuf::from(SETTINGS_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用户目录必须是绝对路径、带产品名，且配置与日志不同目录 ——
    /// 相对路径会让数据落到 CWD，而从 Finder 启动与从终端启动的 CWD 不是一个目录。
    #[test]
    fn user_dirs_are_absolute_named_and_distinct() {
        let cfg = user_config_dir().expect("测试环境里 HOME / APPDATA 必须存在");
        let logs = user_log_dir().expect("同上");
        for p in [&cfg, &logs] {
            assert!(p.is_absolute(), "必须是绝对路径: {}", p.display());
            assert!(
                p.to_string_lossy().to_lowercase().contains("netsense"),
                "路径里要认得出本应用，免得和别的程序共用一个目录: {}",
                p.display()
            );
        }
        assert_ne!(cfg, logs);
    }

    /// 配置文件不存在时，`config_path()` 仍要指向用户目录（第一次保存要落在那里），
    /// 除非同目录那份确实存在（开发期形态，同样是绝对路径）。
    #[test]
    fn an_absent_config_still_resolves_to_an_absolute_path() {
        let got = config_path();
        assert!(got.is_absolute() || got == Path::new(CONFIG_FILE), "{got:?}");
        let in_user = user_config_dir().map(|d| d.join(CONFIG_FILE));
        assert!(
            got == in_user.unwrap_or_default() || got.is_file(),
            "既不是用户目录那份、也不是磁盘上真实存在的同目录配置: {got:?}"
        );
    }

    /// 两份配置住在同一个用户目录里、文件名不同。同目录是界面能「打开所在文件夹」
    /// 一次看到两份的前提；重名则会让一次保存覆盖另一份。
    #[test]
    fn settings_file_sits_next_to_the_config_file() {
        let settings = settings_path();
        assert_eq!(
            settings.file_name().map(|n| n.to_string_lossy().into_owned()),
            Some(SETTINGS_FILE.to_string())
        );
        let dir = user_config_dir().expect("测试环境里 HOME / APPDATA 必须存在");
        assert_eq!(
            settings.parent(),
            Some(dir.as_path()),
            "软件配置要与自动化配置同目录: {}",
            settings.display()
        );
        assert_ne!(config_path().file_name(), settings.file_name());
    }
}
