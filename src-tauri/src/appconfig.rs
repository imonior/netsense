//! 软件配置（`settings.json`）：这台机器上这个用户怎么用 NetSense。
//!
//! 与自动化配置（`config.json`，Profile / 条件 / 动作）分成两份，是因为两者的**变更频率
//! 和风险**完全不同：一份是用户反复编辑的自动化意图，另一部分是装完就可能一辈子不动一下
//! 的界面偏好。混在一起的话，改界面语言会惊动配置热重载与网络重评估，而一份写坏的自动化
//! 配置也会连带让「日志留几天」一起读不出来。
//!
//! ## 只有两个字段
//!
//! `language` 与 `log_retention_days`。看起来该有第三个的「开机启动」**不在这里**：
//! 它的真相在操作系统那一侧（LaunchAgent / 登录项 / 注册表 Run 键），用户也可以绕开本应用
//! 直接改它（系统设置里就能关掉某个登录项）。存一份布尔值就等于给自己造一个会说谎的缓存 ——
//! 界面上的勾选框每次都是**读系统**读出来的。
//!
//! ## 三平台的开机启动落地方式
//!
//! 全部走「写一个文件 / 调一条系统命令」，且**平台分支按运行时常量选**而不是
//! `#[cfg]`：三份实现的代码在每个平台上都会被编译、被 clippy 检查、被单元测试覆盖，
//! 只有真正执行的那一条分支属于当前平台。开机启动这件事没有任何平台专属 API 要用，
//! 不值得为它把 trait 契约撑大一份，也不值得让另外两份变成只有 CI 才看得见的代码。
//!
//! 生效时机是**下一次登录**。这不是缺陷而是这个功能的定义：勾上它，用户要的就是
//! 「开机后有」；当场启动一个已经在运行的应用没有意义，而取消勾选时把正在跑的自己杀掉，
//! 更是莫名其妙的行为。

use std::path::{Path, PathBuf};

use crate::i18n;
use serde::{Deserialize, Serialize};

/// 没配过时的日志保留天数。
pub const DEFAULT_LOG_RETENTION_DAYS: u32 = 7;
/// 允许的最小值。留 0 天等于「启动即清空」，那不是保留策略而是个删除按钮。
pub const MIN_LOG_RETENTION_DAYS: u32 = 1;
/// 允许的最大值。一年以上的运行痕迹不该由一个网络切换器长期替用户保管。
pub const MAX_LOG_RETENTION_DAYS: u32 = 365;

/// 启动项在各平台的标识（macOS 的 LaunchAgent 标签 / Windows 的注册表值名 /
/// Linux 的 autostart 文件名）。三处都认得出本应用，用户在系统自己的界面里才知道关的是谁。
const AUTOSTART_LABEL: &str = "com.netsense.app";
const AUTOSTART_REGISTRY_NAME: &str = "NetSense";
const AUTOSTART_REGISTRY_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// 界面语言（en / zh / zh-TW / ja / ko）。缺省 = en：默认语言是英文，
    /// 但配置文件里不写死它，好让「没设过」和「设成英文」在界面上仍然区分得出来。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// 日志保留天数，按文件名的日期判断（见 `log::cleanup_old`）。
    #[serde(default = "default_retention")]
    pub log_retention_days: u32,
}

fn default_retention() -> u32 {
    DEFAULT_LOG_RETENTION_DAYS
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            language: None,
            log_retention_days: DEFAULT_LOG_RETENTION_DAYS,
        }
    }
}

impl AppConfig {
    /// 读软件配置。**文件不存在不是错误**：首次运行时它还没有被写过。
    ///
    /// 读出来了却少字段（用户手改过）也不算错误 —— `#[serde(default)]` 会补齐。
    /// 只有「这不是 JSON」或「类型对不上」才算：那种情况下连补什么都无从下手。
    ///
    /// 错误文本故意保持 `路径: 系统错误` 的机器形状，不在这里本地化：这条加载发生在
    /// `i18n::set_language` **之前**（语言就存在这份文件里，得先读它才知道用哪种语言），
    /// 此刻字典还停在默认语种，在这里译出来的句子等于替用户挑了语言。本地化点是拿到
    /// 这个错误的调用方（`main` 里的 `app.settings_failed`），那时语种已经定下了。
    pub fn load(path: &Path) -> Result<AppConfig, String> {
        if !path.is_file() {
            return Ok(AppConfig::default());
        }
        let data = std::fs::read_to_string(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg: AppConfig =
            serde_json::from_str(&data).map_err(|e| format!("{}: {}", path.display(), e))?;
        Ok(cfg.clamped())
    }

    /// 把越界的保留天数收进允许区间。
    ///
    /// 为什么加载时收而不是拒绝：这份文件是用户可以直接编辑的，一句 `0` 或 `9999`
    /// 不该让整个应用起不来；但放过去又会让日志模块按一个荒谬的值删文件（或永远不删）。
    pub fn clamped(mut self) -> Self {
        self.log_retention_days = clamp_retention(self.log_retention_days);
        self
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
            }
        }
        std::fs::write(path, s).map_err(|e| e.to_string())
    }
}

pub fn clamp_retention(days: u32) -> u32 {
    days.clamp(MIN_LOG_RETENTION_DAYS, MAX_LOG_RETENTION_DAYS)
}

// —————————————————————————————— 开机启动 ——————————————————————————————

/// 一个平台机制的完整描述：要么是一个「存在即生效」的文件，要么是一组注册表命令。
///
/// 做成数据而不是三个 `#[cfg]` 分支，是为了让**三份实现在每个平台上都被类型检查**
/// （见模块头）。执行侧只看 `os` 选中的那一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutostartTarget {
    /// macOS `~/Library/LaunchAgents/com.netsense.app.plist`、
    /// Linux `~/.config/autostart/netsense.desktop`
    File { path: PathBuf, content: String },
    /// Windows `HKCU\...\Run` 的一个字符串值
    Registry {
        probe: Vec<String>,
        enable: Vec<String>,
        disable: Vec<String>,
    },
}

/// 该平台机制的具体形态。`os` 是 `std::env::consts::OS`（编译期由目标平台决定），
/// `home` 是当前用户的 home，`config_home` 是 Linux 上的配置根目录（见 [`xdg_config_home`]，
/// 另外两个平台不看它）。
///
/// `None` = 本平台没有开机启动机制。三个受支持的平台都有，所以这条分支今天到不了；
/// 留着它，是为了将来多出一个平台时**宁可显示「不支持」**，也不要拿别的平台的文件路径
/// 去用户机器上乱写。
pub fn autostart_target(
    os: &str,
    home: &Path,
    config_home: &Path,
    exe: &Path,
) -> Option<AutostartTarget> {
    let exe = exe.to_string_lossy().to_string();
    match os {
        "macos" => Some(AutostartTarget::File {
            path: home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{AUTOSTART_LABEL}.plist")),
            content: plist(&exe),
        }),
        "linux" => Some(AutostartTarget::File {
            path: config_home
                .join("autostart")
                .join("netsense.desktop"),
            content: desktop_entry(&exe),
        }),
        "windows" => Some(AutostartTarget::Registry {
            probe: reg_args(&["query", AUTOSTART_REGISTRY_KEY, "/v", AUTOSTART_REGISTRY_NAME]),
            enable: reg_args(&[
                "add",
                AUTOSTART_REGISTRY_KEY,
                "/v",
                AUTOSTART_REGISTRY_NAME,
                "/t",
                "REG_SZ",
                "/d",
                &registry_value(&exe),
                "/f",
            ]),
            disable: reg_args(&["delete", AUTOSTART_REGISTRY_KEY, "/v", AUTOSTART_REGISTRY_NAME, "/f"]),
        }),
        _ => None,
    }
}

/// `reg.exe` 的参数表（程序名固定，故不进 argv[0]）。
fn reg_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// 注册表值：带引号的可执行文件路径。
///
/// 引号是必需的而不是装饰：`C:\Program Files\...` 里就有空格，没有引号时 Windows 会在
/// 第一个空格处截断，用户看到的是「勾了开机启动但下次开机什么都没发生」。
fn registry_value(exe: &str) -> String {
    format!("\"{}\"", exe.replace('"', ""))
}

/// LaunchAgent：`RunAtLoad` 的纯声明式启动项，不需要本进程在场就能被 launchd 读取。
fn plist(exe: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>\n\
         \x20 <string>{label}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         \x20   <string>{exe}</string>\n\
         \x20 </array>\n\
         \x20 <key>RunAtLoad</key>\n\
         \x20 <true/>\n\
         </dict>\n\
         </plist>\n",
        label = xml_escape(AUTOSTART_LABEL),
        exe = xml_escape(exe),
    )
}

/// XDG autostart 条目。`Name` 用产品名，用户在桌面环境的「开机启动」界面里认得出它。
fn desktop_entry(exe: &str) -> String {
    // .desktop 的值里 `%` 是字段码（`%U` 之类）的起始，反斜杠是转义符，两者都必须翻倍，
    // 否则路径碰巧含它们时产出来的是一个语法无效的条目 —— 桌面环境会静默忽略它。
    let esc = exe.replace('\\', "\\\\").replace('%', "%%");
    format!(
        "[Desktop Entry]\nType=Application\nName=NetSense\nExec=\"{esc}\"\nTerminal=false\nX-GNOME-Autostart-enabled=true\nNoDisplay=false\nComment=SSID-aware network profile switcher\n"
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 本机的 home 目录。POSIX 侧就是 `$HOME`，Windows 用 `%USERPROFILE%`
/// （开机启动写的是 HKCU / 当前用户目录，两者都必须跟着**当前用户**而不是管理员）。
fn home_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
    }
}

/// Linux 的配置根目录：`$XDG_CONFIG_HOME`，缺省 `~/.config`。
///
/// 桌面环境找 autostart 条目时只认这个变量，写死 `~/.config` 的话，设了它的用户勾上开关
/// 也不会生效 —— 而界面下次读的仍是同一个错位置，于是显示「已开启」。判据与 [`crate::paths`]
/// 一致：非绝对路径的值不采信（相对值会随工作目录漂移）。另两个平台不看返回值。
fn xdg_config_home(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
}

/// 开机启动现在是否开着（读系统，不读配置文件）。
///
/// 报错只在「连读都没法读」时出现（没有 home 目录 / `reg query` 拉起失败）。
/// 「读到了但值不在」是正常的关闭态，不是错误。
pub fn autostart_enabled() -> Result<bool, String> {
    let home = home_dir().ok_or_else(|| i18n::t("app.no_home"))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    match autostart_target(std::env::consts::OS, &home, &xdg_config_home(&home), &exe) {
        None => Err(i18n::t("app.no_autostart")),
        Some(AutostartTarget::File { path, .. }) => Ok(path.is_file()),
        Some(AutostartTarget::Registry { probe, .. }) => {
            match crate::platform::run("reg.exe", &refs(&probe)) {
                Ok(_) => Ok(true),
                // `reg query` 查不到值时以退出码 1 结束 —— 那是「没开」而不是「读不到」。
                // 这里分不开两者（`run` 把非零退出与 spawn 失败都收敛成 Err），所以取
                // 更保守的显示：报成关闭态。真正的差别在下一步就会显现 —— 用户勾开的
                // 那次 `reg add` 如果连进程都拉不起来，错误会照实返回到界面上。
                Err(_) => Ok(false),
            }
        }
    }
}

/// 打开 / 关闭开机启动。成功后再读一次系统，把**实际**状态返回给调用方 ——
/// 勾选框显示的是操作结果，不是操作意图。
pub fn set_autostart(enable: bool) -> Result<bool, String> {
    let home = home_dir().ok_or_else(|| i18n::t("app.no_home"))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = autostart_target(std::env::consts::OS, &home, &xdg_config_home(&home), &exe)
        .ok_or_else(|| i18n::t("app.no_autostart"))?;
    match target {
        AutostartTarget::File { path, content } => {
            if enable {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
                }
                std::fs::write(&path, content).map_err(|e| format!("{}: {}", path.display(), e))?;
            } else if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(format!("{}: {}", path.display(), e));
                }
            }
        }
        AutostartTarget::Registry { enable: add, disable, .. } => {
            let args = if enable { &add } else { &disable };
            // 关一个本来就没开的项，`reg delete` 会以非 0 退出码拒绝。那不是失败：
            // 结束状态就是用户要的样子，所以这里只在「确实还开着」时才报错。
            if let Err(e) = crate::platform::run("reg.exe", &refs(args)) {
                if enable || autostart_enabled()? {
                    return Err(e);
                }
            }
        }
    }
    autostart_enabled()
}

fn refs(args: &[String]) -> Vec<&str> {
    args.iter().map(|s| s.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/u")
    }
    fn exe() -> PathBuf {
        PathBuf::from("/Applications/NetSense.app/Contents/MacOS/NetSense")
    }

    /// 三平台各自的落点必须落在**当前用户**的可写位置，且文件名认得出本应用 ——
    /// perMachine 安装下如果写到机器级位置，就需要管理员权限，而勾选框不该要管理员。
    #[test]
    fn each_platform_targets_a_per_user_location() {
        let t = autostart_target("macos", &home(), &home(), &exe()).expect("macOS 支持开机启动");
        assert_eq!(
            t,
            AutostartTarget::File {
                path: PathBuf::from("/home/u/Library/LaunchAgents/com.netsense.app.plist"),
                content: plist(&exe().to_string_lossy()),
            }
        );
        // Linux 传一个与 home 不同的配置根：落点必须跟着它走，而不是跟着 `~/.config` 写死。
        let t = autostart_target("linux", &home(), &PathBuf::from("/xdg"), &exe())
            .expect("Linux 支持开机启动");
        assert_eq!(
            t.map_path(),
            Some(PathBuf::from("/xdg/autostart/netsense.desktop"))
        );
        let t = autostart_target(
            "windows",
            &home(),
            &home(),
            &PathBuf::from(r"C:\Program Files\NetSense\NetSense.exe"),
        )
        .expect("Windows 支持开机启动");
        assert!(
            t.argvs()
                .iter()
                .all(|args| args.iter().any(|a| a.starts_with(r"HKCU\"))),
            "必须写在当前用户而不是机器级: {:?}",
            t.argvs()
        );
        assert_eq!(
            t.registry_value(),
            Some(r#""C:\Program Files\NetSense\NetSense.exe""#.to_string())
        );
        assert!(autostart_target("plan9", &home(), &home(), &exe()).is_none());
    }

    /// 生成的两份文件内容都要是**各自格式**里合法的关键结构：写坏一个字节，
    /// launchd / 桌面环境会静默不认这个启动项，用户只看到「勾了没生效」。
    #[test]
    fn generated_files_carry_the_executable_and_are_self_consistent() {
        let p = plist("/some/dir/NetSense");
        assert!(p.starts_with("<?xml"));
        assert!(p.contains("<key>RunAtLoad</key>\n  <true/>"), "{p}");
        assert!(p.contains("<string>/some/dir/NetSense</string>"), "{p}");
        assert!(p.contains("<string>com.netsense.app</string>"), "{p}");
        // 路径里的 & 与 < 必须转义，否则整个 plist 解析失败
        let tricky = plist("/a & b/<x>");
        assert!(tricky.contains("/a &amp; b/&lt;x&gt;"), "{tricky}");
        assert!(!tricky.contains("a & b"), "未转义的路径会破坏 XML: {tricky}");

        let d = desktop_entry("/opt/nets sense/netsense");
        assert!(d.starts_with("[Desktop Entry]\n"), "{d}");
        assert!(d.contains("Exec=\"/opt/nets sense/netsense\""), "{d}");
        assert!(d.contains("Type=Application"), "{d}");
        // .desktop 里 % 是字段码起始，路径含它时必须写成 %%
        assert!(desktop_entry("/a%b").contains("Exec=\"/a%%b\""));
    }

    /// 注册表参数：值名固定，`/f` 让重复开启不必先删（否则第二次勾开会弹确认并失败）。
    #[test]
    fn registry_commands_are_complete() {
        let t = autostart_target("windows", &home(), &home(), &exe()).unwrap();
        let a = t.argvs();
        assert_eq!(a[0][0], "query");
        assert_eq!(a[1][0], "add");
        assert!(a[1].iter().any(|x| x == "/f"), "开启必须免确认: {:?}", a[1]);
        assert_eq!(a[2][0], "delete");
        assert!(a[2].iter().any(|x| x == AUTOSTART_REGISTRY_NAME));
    }

    #[test]
    fn a_registry_path_with_quotes_cannot_escape_its_own_value() {
        // 手滑/恶意的 exe 路径（自带引号）不能把注册表值闭合出去，变成一条独立命令的参数
        let v = registry_value(r#"C:\x\" ; calc.exe ; ""#);
        assert_eq!(v.matches('"').count(), 2, "只允许首尾各一个引号: {v}");
    }

    #[test]
    fn an_absent_file_is_defaults_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!("netsense-appconfig-{}", std::process::id()));
        let p = dir.join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(AppConfig::load(&p).unwrap(), AppConfig::default());

        let cfg = AppConfig { language: Some("ja".into()), log_retention_days: 30 };
        cfg.save(&p).unwrap();
        assert_eq!(AppConfig::load(&p).unwrap(), cfg);

        // 部分字段：缺 retention 用默认值，而不是 0
        std::fs::write(&p, r#"{"language":"ko"}"#).unwrap();
        let got = AppConfig::load(&p).unwrap();
        assert_eq!(got.language.as_deref(), Some("ko"));
        assert_eq!(got.log_retention_days, DEFAULT_LOG_RETENTION_DAYS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_is_clamped_instead_of_refused_or_trusted() {
        assert_eq!(clamp_retention(0), MIN_LOG_RETENTION_DAYS);
        assert_eq!(clamp_retention(9_999), MAX_LOG_RETENTION_DAYS);
        assert_eq!(clamp_retention(14), 14);
        // 手改出来的越界值也要在加载时就收住：调用方拿到的永远是可执行的值
        assert_eq!(
            AppConfig { language: None, log_retention_days: 0 }.clamped().log_retention_days,
            MIN_LOG_RETENTION_DAYS
        );
    }

    /// 一份不是 JSON 的文件要报出来：它是用户手改坏的结果，静默用默认值等于
    /// 把他设定的语言悄悄换掉，而日志里也不留痕迹。
    #[test]
    fn a_malformed_settings_file_is_an_error_not_a_silent_default() {
        let dir = std::env::temp_dir().join(format!("netsense-appconfig-bad-{}", std::process::id()));
        let p = dir.join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&p, "{ not json").unwrap();
        let err = AppConfig::load(&p).unwrap_err();
        assert!(err.contains("settings.json"), "报错要指名是哪份文件: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    trait Describe {
        fn map_path(&self) -> Option<PathBuf>;
        fn argvs(&self) -> Vec<Vec<String>>;
        fn registry_value(&self) -> Option<String>;
    }
    impl Describe for AutostartTarget {
        fn map_path(&self) -> Option<PathBuf> {
            match self {
                AutostartTarget::File { path, .. } => Some(path.clone()),
                AutostartTarget::Registry { .. } => None,
            }
        }
        fn argvs(&self) -> Vec<Vec<String>> {
            match self {
                AutostartTarget::File { .. } => vec![],
                AutostartTarget::Registry { probe, enable, disable } => {
                    vec![probe.clone(), enable.clone(), disable.clone()]
                }
            }
        }
        fn registry_value(&self) -> Option<String> {
            match self {
                AutostartTarget::Registry { enable, .. } => {
                    let i = enable.iter().position(|a| a == "/d")?;
                    enable.get(i + 1).cloned()
                }
                _ => None,
            }
        }
    }
}
