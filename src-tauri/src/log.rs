//! 日志模块：按天轮转的本地文件日志 + 开发期 stderr 回显。
//!
//! - 日志目录：优先 `<exe>/logs/`（便携运行时就地写）；不可写时依次回退到
//!   Windows `%LOCALAPPDATA%\NetSense\logs`、macOS `~/Library/Logs/NetSense`、
//!   Linux `$XDG_STATE_HOME/netsense/logs`，最后退到系统临时目录。
//! - 文件名：`netsense-YYYY-MM-DD.log`
//! - 启动时清理 7 天前的旧日志
//! - 跨线程安全（全局 Mutex 串行写）
//! - 未调用 `init()` 前，所有日志降级打到 stderr，保证不丢信息
//! - `init()` 同时安装 panic hook：release 版无控制台，panic 默认无处可见，
//!   必须落盘才能定位（详见 `install_panic_hook`）

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{Datelike, Duration, Local, NaiveDate, Timelike};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
        }
    }
}

struct Logger {
    dir: PathBuf,
    today: String, // YYYY-MM-DD
    file: Option<File>,
    to_stderr: bool,
}

static GLOBAL: OnceLock<Mutex<Logger>> = OnceLock::new();

/// 初始化日志系统。应在 App setup 早期调用一次。
/// `preferred_dir` 通常是 exe 同级的 `logs`；若不可写会自动回退到用户目录。
/// `to_stderr` 在开发模式建议 true（便于终端观察），发布模式可 false。
pub fn init(preferred_dir: &Path, to_stderr: bool) {
    let log_dir = resolve_dir(preferred_dir);
    let today = today_str();
    let mut logger = Logger {
        dir: log_dir.clone(),
        today: today.clone(),
        file: None,
        to_stderr,
    };
    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!("[netsense] 无法创建日志目录 {}: {}", log_dir.display(), e);
    }
    cleanup_old(&log_dir, &today);
    if let Err(e) = open_file(&mut logger) {
        eprintln!("[netsense] 无法打开日志文件: {}", e);
    }
    if GLOBAL.set(Mutex::new(logger)).is_err() {
        return;
    }
    install_panic_hook(&log_dir);
}

/// 解析一个「确定可写」的日志目录。
///
/// 顺序：优先用调用方给的目录（通常是 exe 同级 `logs`，便携运行时就地写日志）；
/// 不可写时回退到平台用户目录，最后退到系统临时目录。
///
/// **为什么必须回退**：perMachine 安装把程序放在 `C:\Program Files\NetSense`，
/// 普通用户进程无权在 exe 同级创建目录。若此时不回退，release 版既没有控制台
/// （`windows_subsystem = "windows"`）也没有日志文件 —— 出任何问题都只剩
/// 「安装后无法运行」这种无法排查的用户报告。
pub fn resolve_dir(preferred: &Path) -> PathBuf {
    let mut candidates = vec![preferred.to_path_buf()];
    if let Some(d) = user_log_dir() {
        candidates.push(d);
    }
    candidates.push(std::env::temp_dir().join("NetSense").join("logs"));
    for c in &candidates {
        if is_writable(c) {
            return c.clone();
        }
    }
    // 全部不可写：仍返回首选目录，至少让 stderr 降级路径保持可用
    preferred.to_path_buf()
}

/// 平台默认的用户级日志目录（始终可写，无需提权）。
fn user_log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|p| PathBuf::from(p).join("NetSense").join("logs"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library").join("Logs").join("NetSense"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(|p| PathBuf::from(p).join("netsense").join("logs"))
            .or_else(|| {
                std::env::var_os("HOME").map(|p| {
                    PathBuf::from(p)
                        .join(".local")
                        .join("state")
                        .join("netsense")
                        .join("logs")
                })
            })
    }
    #[cfg(not(any(windows, unix)))]
    {
        None
    }
}

/// 目录能否创建并写入：真写一个探针文件，而不是只看目录是否存在
/// （存在但只读的目录同样会让日志静默消失）。
fn is_writable(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".netsense-write-probe");
    match File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 安装 panic hook：把 panic 消息 + 位置写进日志文件。
///
/// 必要性：release 版在 Windows 上是 GUI 子系统（无控制台），macOS 从 Finder
/// 启动也没有终端，默认的 panic 输出**无处可见**，进程静默消失 —— 用户只能报
/// 「装完打不开」。落盘后连 `.expect()` 造成的一行 panic 都能被远程定位。
fn install_panic_hook(dir: &Path) {
    let dir = dir.to_path_buf();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        let today = today_str();
        let line = format!("[{}] [PANIC] {} @ {}\n", today, msg, loc);
        // 刻意绕开全局 Logger 的 Mutex：panic 可能就发生在持有该锁的线程里，
        // 再去 lock() 会死锁。直接以追加方式另开一个句柄写。
        let p = dir.join(format!("netsense-{}.log", today));
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
        eprint!("[netsense]{}", line);

        // 启动**尚未完成**就 panic → 进程必然消失，而且没有任何窗口能承载错误信息。
        // release 版在 Windows 上没有控制台，用户只会看到「点了一下，什么都没发生」。
        // 因此这里弹一个原生对话框，把 panic 消息、位置和日志路径直接摆到用户面前。
        // 启动完成后的 panic（例如某个后台线程）不弹窗：那不该用模态框打断一个仍在工作的应用。
        if !crate::state::APP_STARTED.load(std::sync::atomic::Ordering::SeqCst) {
            crate::win_dialog::fatal(
                "NetSense 启动失败",
                &format!(
                    "NetSense 在启动阶段发生错误，进程即将退出。\n\n\
                     错误: {}\n位置: {}\n\n\
                     完整日志:\n{}",
                    msg,
                    loc,
                    p.display()
                ),
            );
        }

        // 保留默认行为（RUST_BACKTRACE=1 时打印回溯）
        prev(info);
    }));
}

/// 当前日志目录（供「打开日志目录」菜单项使用）。
pub fn log_dir() -> PathBuf {
    GLOBAL
        .get()
        .map(|g| g.lock().unwrap_or_else(|e| e.into_inner()).dir.clone())
        .unwrap_or_else(|| PathBuf::from("logs"))
}

fn today_str() -> String {
    let now = Local::now();
    format!("{:04}-{:02}-{:02}", now.year(), now.month(), now.day())
}

fn open_file(l: &mut Logger) -> std::io::Result<()> {
    let p = l.dir.join(format!("netsense-{}.log", l.today));
    let f = OpenOptions::new().create(true).append(true).open(&p)?;
    l.file = Some(f);
    Ok(())
}

/// 删除 7 天前的日志文件（按文件名日期判断）。
fn cleanup_old(dir: &Path, today: &str) {
    let cutoff = match NaiveDate::parse_from_str(today, "%Y-%m-%d") {
        Ok(d) => d - Duration::days(7),
        Err(_) => return,
    };
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(date_part) = name
            .strip_prefix("netsense-")
            .and_then(|s| s.strip_suffix(".log"))
        {
            if let Ok(d) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d") {
                if d < cutoff {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// 核心写日志。任何地方调用，未初始化则降级到 stderr。
pub fn log(level: Level, msg: &str) {
    if let Some(g) = GLOBAL.get() {
        let mut l = g.lock().unwrap_or_else(|e| e.into_inner());
        let today = today_str();
        if today != l.today {
            l.today = today.clone();
            let _ = open_file(&mut l);
            cleanup_old(&l.dir, &today);
        }
        let now = Local::now();
        let ts = format!(
            "{:02}:{:02}:{:02}",
            now.hour(),
            now.minute(),
            now.second()
        );
        let line = format!("[{} {}] [{}] {}\n", l.today, ts, level.as_str(), msg);
        if let Some(f) = l.file.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
        if l.to_stderr {
            eprint!("{}", line);
        }
    } else {
        eprintln!("[netsense][{}] {}", level.as_str(), msg);
    }
}

#[inline]
pub fn error(msg: &str) {
    log(Level::Error, msg);
}
#[inline]
pub fn warn(msg: &str) {
    log(Level::Warn, msg);
}
#[inline]
pub fn info(msg: &str) {
    log(Level::Info, msg);
}
#[inline]
pub fn debug(msg: &str) {
    log(Level::Debug, msg);
}
