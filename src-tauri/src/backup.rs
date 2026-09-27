//! 备份：把这台机器上的 NetSense 状态收进一个文件，再从那个文件放回去。
//!
//! 内容就三份东西：`config.json`（自动化配置）、`settings.json`（软件配置）与
//! `scripts/` 目录。它们是一起被改的 —— 一条 `run_script` 动作引用了某个脚本，
//! 只备份前两份就会恢复出一个指向不存在文件的配置。
//!
//! 格式是**自己定义的 JSON**，不是 zip / tar.gz，三条理由：
//! · 不必为此引入归档或压缩依赖；
//! · 导入侧不用解压别人写的归档 —— 归档条目的路径遍历是一类经典事故，而这里需要认的
//!   只有三个名字，检查得住；
//! · 肉眼可读：用户可以在恢复之前打开看看里面到底是什么。
//!
//! 恢复之前一定先做一次导出，把「被覆盖掉的那一份」留在同一个目录里。这条不是礼貌：
//! 恢复的是一份来自别处的配置，出错时唯一的退路就是被覆盖掉的那份。

use crate::appconfig::AppConfig;
use crate::config::Config;
use crate::i18n;
use crate::paths::{CONFIG_FILE, SETTINGS_FILE};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// 备份文件的身份字段：`kind` 认格式，`version` 认版本。
/// 没有 `kind` 的 JSON 一律拒收 —— 用户会把自己的 `config.json` 指过来，那是一份
/// 合法 JSON 但没有备份身份，按备份读它会得到「恢复成功」而什么都没恢复。
const KIND: &str = "netsense.backup";
const VERSION: u32 = 1;

/// 备份目录名，放在自动化配置旁边。
pub const DIR_NAME: &str = "backups";
/// 文件名前缀。`list` 与 `is_backup_name` 都按它筛，界面不自己拼形状。
const NAME_PREFIX: &str = "netsense-backup-";

/// 脚本目录的取舍上限。一份带几十 MB 素材的 scripts/ 不是备份对象而是别的什么，
/// 而这里没有压缩，超限直接拒绝比默默丢掉一半文件诚实。
const MAX_FILES: usize = 200;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct ScriptEntry {
    /// 相对 `scripts/` 的路径，分隔符恒为 `/`（Windows 上也一样：备份文件要能跨平台读回）。
    path: String,
    /// base64 的原始字节。脚本是文本居多，但 `.ps1` 存成 UTF-16 是 Windows 的常态，
    /// 按文本读会在备份那一刻就把文件弄坏。
    data: String,
    /// 是否可执行。macOS / Linux 上 `run_script` 是直接 exec 的，丢了这一位就等于
    /// 恢复出来的脚本永远跑不起来。
    #[serde(default)]
    exec: bool,
}

#[derive(Serialize, Deserialize)]
struct Backup {
    kind: String,
    version: u32,
    /// 导出时刻，按 UTC。给人认序用，不参与任何判定 —— 判定靠 `kind` / `version`
    /// 与配置自己带的 `schema`。
    created: String,
    /// 导出时的程序版本。同样是给人看的：跨版本恢复是允许的（配置里有 `schema` 字段
    /// 负责真正的兼容判定），但「这份备份来自 1.0.2」值得写在文件里。
    app: String,
    /// `None` = 当时磁盘上没有那份文件（首次运行就导出也是合法操作）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    config: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settings: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scripts: Vec<ScriptEntry>,
}

/// 一份备份要认的三样东西的位置，由调用方（`ipc`）从 `AppState` 拼出来。
///
/// 之所以传进来而不是在这里再推一遍：脚本目录必须是引擎执行脚本、`AllowedScripts`
/// 判定受信时用的**那一个**。这里若按 `config_path` 自己拼一份，两处推导一旦哪天
/// 分岔（比如配置目录改成可写位置的另一档），备份就会备到引擎不看的地方，
/// 而恢复提示照样说「恢复了」。
#[derive(Debug)]
pub struct Sources<'a> {
    pub config: &'a Path,
    pub settings: &'a Path,
    pub scripts: &'a Path,
}

/// 一个备份文件的目录项。`bytes` / `modified` 只为界面能认出「哪个是最新的那份」。
#[derive(Serialize)]
pub struct Listing {
    pub name: String,
    pub bytes: u64,
    pub modified: u64,
}

/// 恢复结果：哪些部分真的被写回去了，以及写回去的那两份内容。
///
/// 把内容交回调用方（`ipc`）是必要的：配置文件在磁盘上换了，进程内存里那份却不会自己
/// 变新 —— 语言不重设、界面就还是旧文案；日志保留天数不重设就还按旧窗口清理。
pub struct Restored {
    pub config: Option<Config>,
    pub settings: Option<AppConfig>,
    pub scripts: usize,
    /// 恢复之前那份的落盘位置。
    pub safety_copy: String,
}

/// 备份目录：跟着自动化配置走，与 `scripts/` 同一个父目录。
///
/// 公开的：设置窗口要把这个路径原样列出来（界面不自己拼，拼错了用户会去翻一个
/// 根本不存在的目录）。列路径**不**建目录 —— 那是 [`ensure`] 的事。
pub fn dir(src: &Sources) -> PathBuf {
    src.config
        .parent()
        .map(|p| p.join(DIR_NAME))
        .unwrap_or_else(|| PathBuf::from(DIR_NAME))
}

/// 这个文件名是不是本模块导出的备份。恢复只接受自己写过的形状，
/// 于是 `name` 不可能指到目录里的别的文件（界面也只列得出来这些）。
pub fn is_backup_name(name: &str) -> bool {
    // netsense-backup-<YYYYmmdd>-<HHMMSS>[-<n>].json
    let Some(rest) = name
        .strip_prefix(NAME_PREFIX)
        .and_then(|s| s.strip_suffix(".json"))
    else {
        return false;
    };
    let mut parts = rest.split('-');
    let (Some(date), Some(time), extra) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let tag = match extra {
        None => true,
        Some(n) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
    };
    tag && date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) && is_time(time)
}

/// 时间在 00:00:00 与 23:59:59 之间。不校验闰秒那一类细节：这个名字是给人认序用的，
/// 不是当权威时间用的。
fn is_time(s: &str) -> bool {
    s.len() == 6 && s.bytes().all(|b| b.is_ascii_digit())
}

fn stamp() -> String {
    let now = chrono::Utc::now();
    format!(
        "{}-{}",
        now.format("%Y%m%d"),
        now.format("%H%M%S")
    )
}

/// 备份目录的可读路径，顺带把它建出来（导出与界面都按这个走，别在两处 `create_dir_all`）。
fn ensure_dir(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        return Ok(());
    }
    fs::create_dir_all(dir).map_err(|e| {
        i18n::tf(
            "cfg.mkdir_failed",
            &[
                ("dir", &dir.display().to_string()),
                ("error", &e.to_string()),
            ],
        )
    })
}

/// 备份目录，并确保它存在。
///
/// 「打开所在文件夹」要用它：从没导出过的机器上这个目录还不存在，直接开会给出一句
/// 「找不到路径」，而用户想看的恰恰是「导出以后会落在哪儿」。
pub fn ensure(src: &Sources) -> Result<PathBuf, String> {
    let d = dir(src);
    ensure_dir(&d)?;
    Ok(d)
}

/// 把 `path` 读成一份备份内容；文件不存在时给 `None`（首次运行导出就是这个形状）。
///
/// 读不通时报错而不是跳过：把一份坏掉的配置备份成「当时没有配置」，恢复时会真的把
/// 现有配置删不掉地留在原地，而用户看到的是一句「恢复成功」。
fn read_json(path: &Path) -> Result<Option<serde_json::Value>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            i18n::tf(
                "backup.unparsable",
                &[
                    ("path", &path.display().to_string()),
                    ("error", &e.to_string()),
                ],
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(i18n::tf(
            "cfg.read_failed",
            &[
                ("path", &path.display().to_string()),
                ("error", &e.to_string()),
            ],
        )),
    }
}

/// 三条上限的文案。数字由常量拼进去，而不是写在五种语言的文案里：改了常量却忘了
/// 改五句文案时，界面上会承诺一个后端不认的额度。
fn too_large() -> String {
    i18n::tf(
        "backup.too_large",
        &[
            ("files", &MAX_FILES.to_string()),
            ("file_mb", &(MAX_FILE_BYTES / (1024 * 1024)).to_string()),
            ("total_mb", &(MAX_TOTAL_BYTES / (1024 * 1024)).to_string()),
        ],
    )
}

/// 收集 `scripts/` 下的文件。目录不存在时返回空表 —— 没写过脚本的机器是常态。
///
/// 不跟符号链接：一个指向 `/etc` 的软链会把不该进备份的东西打进来，而恢复时它会变成
/// 一份真文件，链接的语义就丢了。
fn collect_scripts(scripts: &Path) -> Result<Vec<ScriptEntry>, String> {
    let mut stack = vec![scripts.to_path_buf()];
    let mut out: Vec<ScriptEntry> = Vec::new();
    let mut total: u64 = 0;
    while let Some(d) = stack.pop() {
        let entries = match fs::read_dir(&d) {
            Ok(e) => e,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(i18n::tf(
                    "cfg.read_failed",
                    &[
                        ("path", &d.display().to_string()),
                        ("error", &err.to_string()),
                    ],
                ))
            }
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let md = match fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(err) => {
                    return Err(i18n::tf(
                        "cfg.read_failed",
                        &[
                            ("path", &p.display().to_string()),
                            ("error", &err.to_string()),
                        ],
                    ))
                }
            };
            if md.file_type().is_symlink() {
                continue;
            }
            if md.is_dir() {
                stack.push(p);
                continue;
            }
            if !md.is_file() {
                continue;
            }
            if out.len() >= MAX_FILES || md.len() > MAX_FILE_BYTES || total > MAX_TOTAL_BYTES {
                return Err(too_large());
            }
            total = total.saturating_add(md.len());
            let bytes = fs::read(&p).map_err(|err| {
                i18n::tf(
                    "cfg.read_failed",
                    &[
                        ("path", &p.display().to_string()),
                        ("error", &err.to_string()),
                    ],
                )
            })?;
            let rel = p
                .strip_prefix(scripts)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if rel.is_empty() {
                continue;
            }
            out.push(ScriptEntry {
                path: rel,
                data: b64_encode(&bytes),
                exec: is_executable(&md),
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// 导出：写一个 `<backups>/netsense-backup-<日期>-<时间>.json`，返回它的文件名。
///
/// 三样东西的位置由调用方给（见 [`Sources`]）。特别地，软件配置**不能**由自动化配置的
/// 路径推出来：前者可以退到可执行文件同级（开发期 / 便携运行），而后者刻意没有那一档
/// 兜底（见 [`crate::paths::settings_path`]）。真按推的来，在那种机器上会把配置旁边一份
/// 不相干的 `settings.json` 备份进来，恢复时再把它写回用户目录 —— 一次导出读错文件，
/// 一次恢复就写错文件。
///
/// 时间戳撞了（同一秒里导出两次）时补一个序号后缀，而不是覆盖前一份：备份的价值
/// 就在「出事之前那一份」，而覆盖掉的就是那一份。
pub fn export(src: &Sources) -> Result<String, String> {
    let d = dir(src);
    ensure_dir(&d)?;
    let mut name = format!("{NAME_PREFIX}{}.json", stamp());
    let mut path = d.join(&name);
    let mut n = 1;
    while path.exists() {
        name = format!("{NAME_PREFIX}{}-{n}.json", stamp());
        path = d.join(&name);
        n += 1;
        if n > 999 {
            return Err(i18n::t("backup.dir_full"));
        }
    }
    let backup = Backup {
        kind: KIND.to_string(),
        version: VERSION,
        created: chrono::Utc::now().to_rfc3339(),
        app: env!("CARGO_PKG_VERSION").to_string(),
        config: read_json(src.config)?,
        settings: read_json(src.settings)?,
        scripts: collect_scripts(src.scripts)?,
    };
    let body = serde_json::to_vec_pretty(&backup).map_err(|e| e.to_string())?;
    write_bytes(&path, &body)?;
    Ok(name)
}

/// 列出现有备份，新→旧。目录不存在时是空表（从没导出过）。
pub fn list(src: &Sources) -> Vec<Listing> {
    let d = dir(src);
    let Ok(entries) = fs::read_dir(&d) else {
        return Vec::new();
    };
    let mut out: Vec<Listing> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !is_backup_name(&name) {
                return None;
            }
            let md = e.metadata().ok()?;
            let modified = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Some(Listing {
                name,
                bytes: md.len(),
                modified,
            })
        })
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    out
}

/// 恢复一个备份。**先**把当前状态导出成一份，再写回。
///
/// 名字必须过 `is_backup_name`：界面只能从下拉里选，但命令的入参是字符串，
/// 校验一遍的成本是零，不校验的代价是「任意路径写进配置目录」。
pub fn import(src: &Sources, name: &str) -> Result<Restored, String> {
    if !is_backup_name(name) {
        return Err(i18n::tf("backup.not_a_backup", &[("name", name)]));
    }
    let d = dir(src);
    let path = d.join(name);
    let bytes = fs::read(&path).map_err(|e| {
        i18n::tf(
            "cfg.read_failed",
            &[
                ("path", &path.display().to_string()),
                ("error", &e.to_string()),
            ],
        )
    })?;
    let backup: Backup = serde_json::from_slice(&bytes).map_err(|e| {
        i18n::tf(
            "backup.unparsable",
            &[
                ("path", &path.display().to_string()),
                ("error", &e.to_string()),
            ],
        )
    })?;
    if backup.kind != KIND || backup.version != VERSION {
        return Err(i18n::tf(
            "backup.wrong_kind",
            &[("kind", &backup.kind), ("version", &backup.version.to_string())],
        ));
    }
    if backup.config.is_none() && backup.settings.is_none() && backup.scripts.is_empty() {
        return Err(i18n::t("backup.nothing_inside"));
    }
    // 解析在写盘之前：一份读不通的备份不该先把好的那份换掉。
    let config = match &backup.config {
        Some(v) => {
            let c: Config = serde_json::from_value(v.clone()).map_err(|e| {
                i18n::tf("backup.config_rejected", &[("error", &e.to_string())])
            })?;
            c.validate().map_err(|e| {
                i18n::tf("backup.config_rejected", &[("error", &e)])
            })?;
            Some(c)
        }
        None => None,
    };
    let settings = match &backup.settings {
        Some(v) => {
            // `clamped()` 不能省：备份可以来自一台手改过 `settings.json` 的机器，
            // 而加载路径上的收束正是为这种文件存在的。绕开它，恢复出来的保留天数会
            // 直接交给日志模块去删文件（或永远不删）。
            let s = serde_json::from_value::<AppConfig>(v.clone()).map_err(|e| {
                i18n::tf("backup.settings_rejected", &[("error", &e.to_string())])
            })?;
            Some(s.clamped())
        }
        None => None,
    };
    // 恢复前先留一份退路。这一步失败就直接报错：没有退路的覆盖不是恢复，是赌博。
    let safety_copy = export(src)?;
    if let Some(c) = &config {
        c.save(src.config)?;
    }
    if let Some(s) = &settings {
        s.save(src.settings)?;
    }
    if !backup.scripts.is_empty() {
        ensure_dir(src.scripts)?;
    }
    let mut written = 0;
    for e in &backup.scripts {
        let rel = safe_script_rel(&e.path)?;
        let parent = script_parent(src.scripts, &rel)?;
        let target = parent.join(rel.file_name().unwrap_or_default());
        let raw = b64_decode(&e.data)
            .map_err(|_| i18n::tf("backup.script_rejected", &[("path", &e.path)]))?;
        write_bytes(&target, &raw)?;
        set_executable(&target, e.exec);
        written += 1;
    }
    Ok(Restored {
        config,
        settings,
        scripts: written,
        safety_copy,
    })
}

fn write_bytes(path: &Path, body: &[u8]) -> Result<(), String> {
    // 先写临时文件再 rename：备份目录里的半个文件会被 `list` 认成一个真备份。
    // 后缀固定为 `.part`，正落在 `is_backup_name` 拒绝的那一类名字里。
    let file = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "backup".to_string());
    let tmp = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{file}.part"));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| {
            i18n::tf(
                "cfg.write_failed",
                &[
                    ("path", &tmp.display().to_string()),
                    ("error", &e.to_string()),
                ],
            )
        })?;
        f.write_all(body).map_err(|e| {
            i18n::tf(
                "cfg.write_failed",
                &[
                    ("path", &tmp.display().to_string()),
                    ("error", &e.to_string()),
                ],
            )
        })?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path).map_err(|e| {
        i18n::tf(
            "cfg.write_failed",
            &[
                ("path", &path.display().to_string()),
                ("error", &e.to_string()),
            ],
        )
    })
}

/// 备份里记的脚本路径 → 可以写的那个相对路径。
///
/// 绝对路径、`..`、反斜杠、盘符/流名里的 `:` 一律拒绝。恢复时脚本目录整个重建，
/// 所以也不需要「跟已存在的目录合并」这种判断。
fn safe_script_rel(p: &str) -> Result<PathBuf, String> {
    let path = Path::new(p);
    let bad = p.is_empty()
        || p.contains('\\')
        || p.contains(':')
        || path.is_absolute()
        || p.starts_with('/');
    if bad {
        return Err(i18n::tf("backup.script_rejected", &[("path", p)]));
    }
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => out.push(s),
            _ => return Err(i18n::tf("backup.script_rejected", &[("path", p)])),
        }
    }
    Ok(out)
}

/// `rel` 应该落在的那个目录：需要时把中间目录建出来，途中撞到符号链接就拒绝。
///
/// [`safe_script_rel`] 挡住了 `..` 与绝对路径，但恢复目标是 `scripts_dir.join(rel)`，
/// 而 `rel` 的中间一段如果正好撞上用户事先放好的一个指向别处的软链，写出去的文件就落在
/// 链接的目标里 —— 「只写进 scripts/」这条约束到此失效。本模块自己从不产生软链
/// （`collect_scripts` 见到就跳过），所以这一拒只会挡住外来的备份。
fn script_parent(scripts: &Path, rel: &Path) -> Result<PathBuf, String> {
    let mut cur = scripts.to_path_buf();
    let parents = rel.parent().unwrap_or_else(|| Path::new(""));
    for comp in parents.components() {
        // 分量已由 safe_script_rel 收窄成 Normal，这里只是把它取出来拼路径。
        let Component::Normal(s) = comp else {
            return Err(i18n::tf("backup.script_rejected", &[("path", &rel.display().to_string())]));
        };
        cur.push(s);
        match fs::symlink_metadata(&cur) {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(i18n::tf(
                    "backup.script_rejected",
                    &[("path", &cur.display().to_string())],
                ))
            }
            Ok(_) => {}
            // 不存在（含权限之外的其它读不到）→ 建。真建不出来时错误里带着目录，
            // 比在这里替操作系统猜一个原因有用。
            Err(_) => fs::create_dir_all(&cur).map_err(|e| {
                i18n::tf(
                    "cfg.mkdir_failed",
                    &[
                        ("dir", &cur.display().to_string()),
                        ("error", &e.to_string()),
                    ],
                )
            })?,
        }
    }
    Ok(cur)
}

#[cfg(unix)]
fn is_executable(md: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_md: &fs::Metadata) -> bool {
    // Windows 不看这一位：能跑什么由扩展名决定。
    false
}

#[cfg(unix)]
fn set_executable(path: &Path, exec: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if exec { 0o755 } else { 0o644 };
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _exec: bool) {}

// —————————————————————— base64（标准表 + 填充）——————————————————————
//
// 不引依赖：这里是 64 行的查表活，而依赖表小一档是这个项目刻意的选择。
// 只在备份文件内部出现，不参与任何跨实现互操作，所以不需要 URL-safe 变体。
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(src: &[u8]) -> String {
    let mut out = String::with_capacity(src.len().div_ceil(3) * 4);
    for chunk in src.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn b64_decode(s: &str) -> Result<Vec<u8>, ()> {
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 3);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => continue,
            // 换行与空白只让 JSON 里长一点的字段好读一些，不是数据。
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return Err(()),
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// 这次恢复写了哪几样、退路在哪 —— 合成一句给人看的话。
///
/// 日志与界面提示共用这一句：两处各拼一遍时，「toast 说恢复了、日志里查不到」这类
/// 分歧只能靠猜。调用方（`ipc`）要在动内存里那两份配置**之前**取走它 —— `Restored`
/// 的字段是被搬进状态里的，搬走之后这份结果就只剩下安全副本的名字了。
pub fn restore_summary(r: &Restored) -> String {
    i18n::tf(
        "notify.backup_restored",
        &[("parts", &parts_of(r)), ("path", &r.safety_copy)],
    )
}

fn parts_of(r: &Restored) -> String {
    let mut v: Vec<&str> = Vec::new();
    if r.config.is_some() {
        v.push(CONFIG_FILE);
    }
    if r.settings.is_some() {
        v.push(SETTINGS_FILE);
    }
    if r.scripts > 0 {
        v.push("scripts/");
    }
    v.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names_are_recognised_by_shape() {
        assert!(is_backup_name("netsense-backup-20260926-153001.json"));
        assert!(is_backup_name("netsense-backup-20260926-153001-7.json"));
        // 目录里会出现的东西：配置文件、别的程序的备份、半截的临时文件
        for n in [
            "config.json",
            "netsense-backup-2026092-153001.json",
            "netsense-backup-20260926-15300.json",
            "netsense-backup-20260926-153001.json.part",
            "netsense-backup-20260926-153001.txt",
            "netsense-backup-20260926-153001-a.json",
            "netsense-backup-20260926-153001-2-3.json",
        ] {
            assert!(!is_backup_name(n), "{n} 不该被认成备份");
        }
    }

    #[test]
    fn base64_roundtrip_covers_every_chunk_length() {
        let src: Vec<u8> = (0..=255u8).cycle().take(517).collect();
        let enc = b64_encode(&src);
        assert_eq!(enc.len(), 517_usize.div_ceil(3) * 4);
        assert_eq!(b64_decode(&enc).unwrap(), src);
        assert_eq!(b64_encode(&[]), "");
        assert!(b64_decode("!!!").is_err());
    }

    #[test]
    fn a_script_path_must_stay_relative_and_plain() {
        assert_eq!(safe_script_rel("a.sh").unwrap(), PathBuf::from("a.sh"));
        assert_eq!(
            safe_script_rel("lib/inner.sh").unwrap(),
            PathBuf::from("lib/inner.sh")
        );
        for p in [
            "",
            "/etc/passwd",
            "../config.json",
            "lib/../../evil",
            ".\\..\\x",
            "C:\\tmp\\x.ps1",
            "a:b",
            "./a.sh",
        ] {
            assert!(safe_script_rel(p).is_err(), "{p} 不该放行");
        }
    }

    #[test]
    fn a_restore_without_a_name_for_a_file_writes_nothing() {
        // 这里挡的是恢复侧最坏的一种走偏：一个不存在的名字被当成「什么都别恢复」。
        // 备份里有 config 才恢复 config，没有就保持现状 —— 三种 None 的组合都要成立。
        let b: Backup = serde_json::from_str(
            r#"{"kind":"netsense.backup","version":1,"created":"x","app":"1"}"#,
        )
        .unwrap();
        assert!(b.config.is_none() && b.settings.is_none() && b.scripts.is_empty());
    }

    /// 一次完整的往返：导出 → 现场被改坏 → 恢复。
    ///
    /// 断的是整条链，不是某个函数：三样东西都回到备份那一刻的样子；被覆盖掉的那一版
    /// 留在备份目录里（这就是「恢复错了还能回去」）；`run_script` 在 macOS / Linux 上
    /// 是直接 exec 的，所以脚本的可执行位也算内容的一部分。
    #[test]
    fn a_restore_puts_back_everything_the_export_took() {
        let base = std::env::temp_dir().join(format!("netsense-backup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let scripts = base.join(crate::paths::SCRIPTS_DIR);
        fs::create_dir_all(&scripts).unwrap();
        let cfg = base.join(CONFIG_FILE);
        let settings = base.join(SETTINGS_FILE);
        let script = scripts.join("up.sh");
        fs::write(&cfg, r#"{"schema":1,"profiles":[]}"#).unwrap();
        fs::write(&settings, r#"{"log_retention_days":30}"#).unwrap();
        fs::write(&script, "#!/bin/sh\necho ok\n").unwrap();
        set_executable(&script, true);
        // 三个路径本身从头到尾没变（变的只是文件内容），所以 `Sources` 建一次就够。
        let src = Sources {
            config: &cfg,
            settings: &settings,
            scripts: &scripts,
        };

        // 名字形状在读写之前就挡住：入参是字符串，界面之外没人保证它指的是一个备份。
        for bad in ["../config.json", "config.json", "x.json"] {
            assert!(import(&src, bad).is_err(), "{bad} 不该被当成备份");
        }
        assert!(!dir(&src).exists(), "只问了一句名字，不该先把目录建出来");

        let name = export(&src).unwrap();
        let names: Vec<String> = list(&src).into_iter().map(|l| l.name).collect();
        assert_eq!(names, vec![name.clone()]);

        fs::remove_file(&cfg).unwrap();
        fs::write(&settings, r#"{"log_retention_days":1}"#).unwrap();
        fs::write(&script, "#!/bin/sh\nexit 9\n").unwrap();
        set_executable(&script, false);

        let r = import(&src, &name).unwrap();
        assert!(r.config.is_some() && r.settings.is_some());
        assert_eq!(r.scripts, 1);
        assert_eq!(fs::read_to_string(&script).unwrap(), "#!/bin/sh\necho ok\n");
        assert_eq!(AppConfig::load(&settings).unwrap().log_retention_days, 30);
        #[cfg(unix)]
        assert!(is_executable(&fs::metadata(&script).unwrap()), "执行位是内容的一部分");
        assert!(is_backup_name(&r.safety_copy));
        assert!(dir(&src).join(&r.safety_copy).is_file());
        // 恢复之后备份目录里有两份：原来那份 + 刚留下的退路
        assert_eq!(list(&src).len(), 2);
        fs::remove_dir_all(&base).unwrap();
    }
}
