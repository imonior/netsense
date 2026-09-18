//! 日志模块：按天轮转的本地文件日志 + 开发期 stderr 回显。
//!
//! - 日志目录：`<config_dir>/logs/`
//! - 文件名：`netsense-YYYY-MM-DD.log`
//! - 启动时清理 7 天前的旧日志
//! - 跨线程安全（全局 Mutex 串行写）
//! - 未调用 `init()` 前，所有日志降级打到 stderr，保证不丢信息

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

    /// 数值越低越严重；用于按等级过滤（保留 >= threshold 的）。
    fn rank(self) -> u8 {
        match self {
            Level::Error => 0,
            Level::Warn => 1,
            Level::Info => 2,
            Level::Debug => 3,
        }
    }
}

struct Logger {
    dir: PathBuf,
    today: String, // YYYY-MM-DD
    file: Option<File>,
    to_stderr: bool,
    threshold: u8, // 仅写出 rank >= threshold 的（默认全部）
}

static GLOBAL: OnceLock<Mutex<Logger>> = OnceLock::new();

/// 初始化日志系统。应在 App setup 早期调用一次。
/// `to_stderr` 在开发模式建议 true（便于终端观察），发布模式可 false。
pub fn init(log_dir: &Path, to_stderr: bool) {
    let today = today_str();
    let mut logger = Logger {
        dir: log_dir.to_path_buf(),
        today: today.clone(),
        file: None,
        to_stderr,
        threshold: 0,
    };
    if let Err(e) = fs::create_dir_all(log_dir) {
        eprintln!("[netsense] 无法创建日志目录 {}: {}", log_dir.display(), e);
    }
    cleanup_old(log_dir, &today);
    if let Err(e) = open_file(&mut logger) {
        eprintln!("[netsense] 无法打开日志文件: {}", e);
    }
    let _ = GLOBAL.set(Mutex::new(logger));
}

/// 设定最低写出等级（低于该等级的日志被忽略）。
pub fn set_level(level: Level) {
    if let Some(g) = GLOBAL.get() {
        g.lock().unwrap().threshold = level.rank();
    }
}

/// 当前日志目录（供「打开日志目录」菜单项使用）。
pub fn log_dir() -> PathBuf {
    GLOBAL
        .get()
        .map(|g| g.lock().unwrap().dir.clone())
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
        let mut l = g.lock().unwrap();
        if level.rank() < l.threshold {
            return;
        }
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
