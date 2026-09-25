//! 日志模块：按天轮转的本地文件日志 + 开发期 stderr 回显。
//!
//! - 日志目录：用户级日志目录（[`crate::paths::user_log_dir`]：Windows
//!   `%LOCALAPPDATA%\NetSense\logs`、macOS `~/Library/Logs/NetSense`、
//!   Linux `$XDG_STATE_HOME/netsense/logs`）；连它都不可写时退到系统临时目录。
//! - 文件名：`netsense-YYYY-MM-DD.log`
//! - 启动时按软件配置的保留天数清理旧日志（[`crate::appconfig::AppConfig::log_retention_days`]）
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
    retention_days: u32,
}

static GLOBAL: OnceLock<Mutex<Logger>> = OnceLock::new();

/// 初始化日志系统。应在 App setup 早期调用一次。
/// `preferred_dir` 是用户级日志目录（见 [`crate::paths`]）；万一不可写会自动退到临时目录。
/// `to_stderr` 在开发模式建议 true（便于终端观察），发布模式可 false。
/// `retention_days` 来自软件配置（见 [`crate::appconfig`]）：留几天由用户说了算，
/// 而不是由我们在每次启动时替他清掉一周以外的东西。
pub fn init(preferred_dir: &Path, to_stderr: bool, retention_days: u32) {
    let log_dir = resolve_dir(preferred_dir);
    let today = today_str();
    let mut logger = Logger {
        dir: log_dir.clone(),
        today: today.clone(),
        file: None,
        to_stderr,
        retention_days,
    };
    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!(
            "[netsense] {}",
            crate::i18n::tf(
                "app.log_dir_failed",
                &[
                    ("dir", &log_dir.display().to_string()),
                    ("error", &e.to_string())
                ]
            )
        );
    }
    cleanup_old(&log_dir, &today, retention_days);
    if let Err(e) = open_file(&mut logger) {
        eprintln!(
            "[netsense] {}",
            crate::i18n::tf("app.log_open_failed", &[("error", &e.to_string())])
        );
    }
    if GLOBAL.set(Mutex::new(logger)).is_err() {
        return;
    }
    install_panic_hook(&log_dir);
}

/// 解析一个「确定可写」的日志目录。
///
/// 顺序：调用方给的目录（正常就是用户级日志目录）→ 系统临时目录。
///
/// **为什么必须回退**：用户目录本身也可能是只读的（受限账户、漫游配置损坏、磁盘满），
/// 而这时 release 版既没有控制台（`windows_subsystem = "windows"`）也没有日志文件 ——
/// 出任何问题都只剩「安装后无法运行」这种无法排查的用户报告。退到临时目录至少留下一份
/// 能看的痕迹。
pub fn resolve_dir(preferred: &Path) -> PathBuf {
    if is_writable(preferred) {
        return preferred.to_path_buf();
    }
    let fallback = std::env::temp_dir().join("NetSense").join("logs");
    if is_writable(&fallback) {
        return fallback;
    }
    // 两处都不可写：仍返回首选目录，至少让 stderr 降级路径保持可用
    preferred.to_path_buf()
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
                &crate::i18n::t("dlg.fatal_title"),
                &crate::i18n::tf(
                    "dlg.panic_body",
                    &[
                        ("message", &msg),
                        ("location", &loc),
                        ("path", &p.display().to_string()),
                    ],
                ),
            );
        }

        // 保留默认行为（RUST_BACKTRACE=1 时打印回溯）
        prev(info);
    }));
}

/// 当前日志目录（供软件配置界面的「打开日志文件夹」使用）。
pub fn log_dir() -> PathBuf {
    GLOBAL
        .get()
        .map(|g| g.lock().unwrap_or_else(|e| e.into_inner()).dir.clone())
        .unwrap_or_else(|| PathBuf::from("logs"))
}

/// 改保留天数，并**立刻**按新值清一次。
///
/// 光把值存下来不够：轮转清理发生在跨天的那一次写入上，用户改完设置当天看到的还是
/// 一目录老文件，只会认为「改了没生效」。
pub fn set_retention_days(days: u32) {
    let Some(g) = GLOBAL.get() else {
        return; // 还没 init：init 会带着软件配置里的值来
    };
    let mut l = g.lock().unwrap_or_else(|e| e.into_inner());
    if l.retention_days == days {
        return;
    }
    l.retention_days = days;
    let today = today_str();
    cleanup_old(&l.dir, &today, days);
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

/// 删除保留期之外的日志文件（按文件名日期判断）。
///
/// 只认 `netsense-YYYY-MM-DD.log` 这个形状的名字：目录里用户自己放进去的东西
/// （一份导出的诊断包、一个笔记）不该被一个日志轮转逻辑删掉。
fn cleanup_old(dir: &Path, today: &str, retention_days: u32) {
    let cutoff = match NaiveDate::parse_from_str(today, "%Y-%m-%d") {
        Ok(d) => d - Duration::days(retention_days.max(1) as i64),
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

/// 一个日志文件的条目，交给界面显示。
///
/// 没有 mtime 字段：文件名里就是 ISO 日期，按名字排序等于按时间排序，而 mtime 会说谎
/// （复制一份旧日志进来，它的 mtime 是刚刚 —— 保留期逻辑因此从来不看它）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogFile {
    pub name: String,
    pub size_bytes: u64,
    pub today: bool,
}

/// 一次读取的结果。`text` 已经是用户要看的那一段，`truncated` 说清有没有被裁过。
#[derive(Debug, Clone, serde::Serialize)]
pub struct LogContent {
    pub name: String,
    pub text: String,
    pub lines: usize,
    pub truncated: bool,
}

/// 单次最多返回多少行。界面要的是「最近这一段」，不是整个文件 —— 上限存在是为了
/// 让一次 IPC 往返不会把几 MB 文本塞进前端。
pub const MAX_TAIL_LINES: usize = 5000;
/// 从文件尾部往前最多读这么多字节，再从中取尾部行。
const TAIL_READ_BYTES: u64 = 512 * 1024;

/// 这个文件名是不是本模块写出来的日志名。
///
/// 保留期清理、这里的列表和这里的读取共用一条判据：只有 `netsense-YYYY-MM-DD.log`
/// 才算日志。目录里用户自己放的文件不出现在界面里，也就不会被读；而「读取」这一侧
/// 重新校验一次名字，意味着前端传什么都拼不出目录外的路径。
fn log_file_date(name: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(
        name.strip_prefix("netsense-")?.strip_suffix(".log")?,
        "%Y-%m-%d",
    )
    .ok()
}

/// 列出日志目录里的日志文件，**新→旧**。
pub fn list_files() -> Vec<LogFile> {
    list_files_in(&log_dir())
}

fn list_files_in(dir: &Path) -> Vec<LogFile> {
    let today_name = format!("netsense-{}.log", today_str());
    let mut out: Vec<LogFile> = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            let err = err.to_string();
            warn(&crate::i18n::tf(
                "logs.read_dir_failed",
                &[("dir", &dir.display().to_string()), ("error", &err)],
            ));
            return out;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if log_file_date(&name).is_none() {
            continue;
        }
        let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let today = name == today_name;
        out.push(LogFile { name, size_bytes, today });
    }
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

/// 读取某个日志文件的**尾部**若干行。`name` 必须长成日志名的样子。
pub fn read_tail(name: &str, max_lines: usize) -> Result<LogContent, String> {
    read_tail_in(&log_dir(), name, max_lines)
}

/// 读日志失败时给用户的那句话。
///
/// 本地化只发生在这里，不在 Tauri 命令里再套一层：`read_tail` 的返回值已经是界面可以直接
/// 显示的文字，调用方再包一句「读取失败：…」就等于把一句已译好的句子塞进 `{error}` 里，
/// 五种语言会各自拼出不同形状的混合物。`{error}` 保留 `路径: 系统错误`，排查要的是那一段。
fn read_failed(path: &Path, e: &std::io::Error) -> String {
    crate::i18n::tf("logs.read_failed", &[("error", &format!("{}: {e}", path.display()))])
}

fn read_tail_in(dir: &Path, name: &str, max_lines: usize) -> Result<LogContent, String> {
    if log_file_date(name).is_none() {
        return Err(crate::i18n::tf("logs.not_a_log_file", &[("name", name)]));
    }
    let max_lines = max_lines.clamp(1, MAX_TAIL_LINES);
    let path = dir.join(name);
    let size = fs::metadata(&path).map_err(|e| read_failed(&path, &e))?.len();
    // 只从尾部取一段：一个跑了几天的日志文件可以有几十 MB，而要看的是最后那几十行。
    let skip = size.saturating_sub(TAIL_READ_BYTES);
    let mut buf: Vec<u8> = Vec::new();
    {
        use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
        let mut r = BufReader::new(File::open(&path).map_err(|e| read_failed(&path, &e))?);
        if skip > 0 {
            r.seek(SeekFrom::Start(skip)).map_err(|e| read_failed(&path, &e))?;
            // 从中间下刀会切出半行：丢掉第一段，宁可少看一行也不给界面一条碎尾巴。
            let mut discard = Vec::new();
            let _ = r.read_until(b'\n', &mut discard);
        }
        r.read_to_end(&mut buf).map_err(|e| read_failed(&path, &e))?;
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let all: Vec<&str> = text.lines().collect();
    let body = all[all.len().saturating_sub(max_lines)..].join("\n");
    Ok(LogContent {
        name: name.to_string(),
        truncated: skip > 0 || all.len() > max_lines,
        lines: body.lines().count(),
        text: body,
    })
}

/// 核心写日志。任何地方调用，未初始化则降级到 stderr。
pub fn log(level: Level, msg: &str) {
    if let Some(g) = GLOBAL.get() {
        let mut l = g.lock().unwrap_or_else(|e| e.into_inner());
        let today = today_str();
        if today != l.today {
            l.today = today.clone();
            let _ = open_file(&mut l);
            cleanup_old(&l.dir, &today, l.retention_days);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 保留期按**文件名里的日期**算，且只动这个形状的文件。
    ///
    /// 两条都容易写坏：按 mtime 判断的话，用户复制一份旧日志进来就会立刻被删（mtime 是
    /// 刚刚）；不看名字前缀的话，同一个目录里用户放的其他东西会被连带删掉。
    #[test]
    fn cleanup_honours_the_retention_window_and_the_file_name_shape() {
        let dir = std::env::temp_dir().join(format!("netsense-log-cleanup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let names = [
            "netsense-2026-01-04.log", // 第 10 天留 5 天：过期
            "netsense-2026-01-05.log", // 边界：正好第 5 天前，留下
            "netsense-2026-01-09.log",
            "netsense-old.log", // 不是这个日期形状：不认，也不删
            "notes.txt",
        ];
        for n in names {
            fs::write(dir.join(n), "x").unwrap();
        }
        cleanup_old(&dir, "2026-01-10", 5);
        let left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let _ = fs::remove_dir_all(&dir);
        for keep in [&names[1], &names[2], &names[3], &names[4]] {
            assert!(left.iter().any(|f| f == keep), "留下: {left:?}，缺 {keep}");
        }
        assert!(!left.iter().any(|f| f == names[0]), "过期的那份必须被删: {left:?}");
    }

    /// 日志窗口读到的，必须正好是保留期逻辑管的那几个文件 —— 两边共用同一个名字判据，
    /// 所以「界面里列出来的」和「会被清理的」是同一批，用户自己放进这个目录的别的文件
    /// 既不出现在列表里，也读不到（名字判据在 read_tail 里重新校验一次，传什么都拼不出
    /// 目录外的路径）。
    #[test]
    fn the_viewer_lists_only_log_shaped_files_and_reads_the_tail() {
        let dir = std::env::temp_dir().join(format!("netsense-log-viewer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("netsense-2026-01-08.log"), "old\n").unwrap();
        fs::write(dir.join("notes.txt"), "secret\n").unwrap();
        fs::write(dir.join("netsense-old.log"), "not a date\n").unwrap();
        let today = format!("netsense-{}.log", today_str());
        // 行格式照 log() 真实写出的样子：`[YYYY-MM-DD HH:MM:SS] [LEVEL] 正文`
        let stamp = today_str();
        let body: String = (1..=10)
            .map(|i| format!("[{stamp} 00:00:0{i}] [INFO] line {i}\n"))
            .collect();
        fs::write(dir.join(&today), &body).unwrap();

        let listed = list_files_in(&dir);
        assert_eq!(
            listed.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            [today.as_str(), "netsense-2026-01-08.log"],
            "只认日志名，且新的在前"
        );
        assert_eq!([listed[0].today, listed[1].today], [true, false]);

        let tail = read_tail_in(&dir, &today, 4).unwrap();
        assert_eq!(tail.lines, 4);
        assert!(tail.text.starts_with(&format!("[{stamp} 00:00:07] [INFO] line 7")), "{:?}", tail.text);
        assert!(tail.text.ends_with("line 10") && !tail.text.contains("line 6"), "{:?}", tail.text);
        assert!(tail.truncated, "10 行里只给了 4 行，必须说清有没读到的部分");
        assert_eq!(read_tail_in(&dir, &today, 99999).unwrap().lines, 10);

        for bad in ["notes.txt", "netsense-old.log", "../netsense-2026-01-08.log"] {
            assert!(read_tail_in(&dir, bad, 10).is_err(), "{bad} 不是日志文件名");
        }
        assert!(read_tail_in(&dir, &today, 0).is_ok(), "行数下限夹到 1，不该报错");
        let _ = fs::remove_dir_all(&dir);
    }
}
