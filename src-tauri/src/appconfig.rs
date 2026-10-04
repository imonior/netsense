//! 软件配置（`settings.json`）：这台机器上这个用户怎么用 NetSense。
//!
//! 与自动化配置（`config.json`，Profile / 条件 / 动作）分成两份，是因为两者的**变更频率
//! 和风险**完全不同：一份是用户反复编辑的自动化意图，另一部分是装完就可能一辈子不动一下
//! 的界面偏好。混在一起的话，改界面语言会惊动配置热重载与网络重评估，而一份写坏的自动化
//! 配置也会连带让「日志留几天」一起读不出来。
//!
//! ## 只有四个字段
//!
//! `language`、`theme`、`log_retention_days`、`proxy`。看起来该有第五个的「开机启动」**不在这里**：
//! 它的真相在操作系统那一侧（LaunchAgent / 登录项 / 注册表 Run 键），用户也可以绕开本应用
//! 直接改它（系统设置里就能关掉某个登录项）。存一份布尔值就等于给自己造一个会说谎的缓存 ——
//! 界面上的勾选框每次都是**读系统**读出来的。
//!
//! `theme` 和 `language` 同一条判据，也同一个口径：认不出的值（手改的、从别的机器备过来的）
//! 退回「跟随系统」，而不是让整份文件一起读不出来 —— 配色错了可远比语言丢了容易发现，
//! 但它同样不该让用户打不开应用。
//!
//! `proxy` 进来是因为它过得了同一句判据：改了它不会改变自动化行为，只改变「检查更新、下载
//! 安装包」这两次对外请求走哪条路。放在 `config.json` 里就等于让一台机器的网络出口跟着
//! 自动化配置一起同步给另一台机器。
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

/// 没配过时的界面配色档位。
pub const DEFAULT_THEME: &str = "system";
/// 三档配色。认不出的值一律按这一档处理，别让一份手坏的 `settings.json` 拖累整个应用。
pub const THEMES: [&str; 3] = ["system", "light", "dark"];

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

/// 各窗口上次关闭时的**客户区尺寸**（逻辑像素，宽×高）。缺省 = 用 `tauri.conf.json` 里的声明值。
///
/// 只记尺寸、不记位置，是刻意的：位置要跟着「窗口在哪块屏、那块屏还在不在」走 —— 外接屏
/// 拔掉之后照着旧坐标摆，窗口会落在所有屏幕之外，用户只能靠改分辨率或删配置把它救回来。
/// 尺寸没有这个病史：它唯一的约束是**当前这块屏**的工作区，而那正是打开时要实时收敛的
/// 东西（见 `popup::apply_geometry`）。所以这里存的是用户调过的那一部分，与屏幕有关的
/// 收敛一律留到打开时算。
///
/// 存逻辑像素而非物理像素：同一份配置在 100% 与 200% 缩放的显示器之间搬动时，物理值会
/// 还原出一个两倍大的窗口，逻辑值在任何缩放比下都是同一个「看起来这么大」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WindowSizes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<(u32, u32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<(u32, u32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<(u32, u32)>,
}

impl WindowSizes {
    /// 三个窗口都没记过尺寸。
    ///
    /// 序列化用它做跳过条件：用户从来没拖过窗口的配置文件里不该出现一个空的
    /// `window_sizes` —— 它既不是配置也不是诊断信息，写进去只是噪声，还让人分不清
    /// 「没调过」和「调过又清掉了」。
    fn is_empty(&self) -> bool {
        self.main.is_none() && self.settings.is_none() && self.logs.is_none()
    }

    /// 把越界的尺寸收进允许区间。
    pub fn clamped(self) -> Self {
        Self {
            main: self.main.map(clamp_size),
            settings: self.settings.map(clamp_size),
            logs: self.logs.map(clamp_size),
        }
    }
}

/// 窗口尺寸的允许区间。下限取「还看得见内容」的值，上限挡住手写配置里的荒谬数字 ——
/// 真正起约束作用的始终是显示器的工作区，那一步在打开时实时收敛。
const MIN_WINDOW_SIZE: u32 = 320;
const MAX_WINDOW_SIZE: u32 = 16384;

fn clamp_size(s: (u32, u32)) -> (u32, u32) {
    (
        s.0.clamp(MIN_WINDOW_SIZE, MAX_WINDOW_SIZE),
        s.1.clamp(MIN_WINDOW_SIZE, MAX_WINDOW_SIZE),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// 界面语言（en / zh / zh-TW / ja / ko）。缺省 = en：默认语言是英文，
    /// 但配置文件里不写死它，好让「没设过」和「设成英文」在界面上仍然区分得出来。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// 界面配色：`system`（跟随系统）/ `light` / `dark`。缺省 = `system`。
    ///
    /// 存的是**用户选的那一档**，不是当下生效的那套颜色：后者是这一档加上操作系统的实况
    /// 算出来的（见 [`theme_for`]），把它写下来就又造出一个会说谎的缓存 —— 用户在
    /// 系统设置里把深色改成浅色，本应用记着的那个值立刻就不成立了，界面还照旧。
    #[serde(default = "default_theme")]
    pub theme: String,
    /// 日志保留天数，按文件名的日期判断（见 `log::cleanup_old`）。
    #[serde(default = "default_retention")]
    pub log_retention_days: u32,
    /// 「检查更新 / 下载安装包」这两次对外请求走哪条路。缺省 = 跟随系统的代理设置。
    ///
    /// 它是唯一一个会影响对外字节流的字段，判据仍然成立：它改变的是本应用怎么上网，
    /// 不改变任何 Profile 的匹配结果、也不改变任何下发内容。三态与探测都在
    /// [`crate::netproxy`] 里，这里只管存什么。
    #[serde(default)]
    pub proxy: crate::netproxy::ProxySetting,
    /// 三个主窗口上次关闭时的尺寸（见 [`WindowSizes`]）。
    ///
    /// 它改变的是界面怎么显示，不改任何下发内容 —— 与上面几个字段同一条判据。
    #[serde(default, skip_serializing_if = "WindowSizes::is_empty")]
    pub window_sizes: WindowSizes,
}

fn default_retention() -> u32 {
    DEFAULT_LOG_RETENTION_DAYS
}

fn default_theme() -> String {
    DEFAULT_THEME.to_string()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            language: None,
            theme: DEFAULT_THEME.to_string(),
            log_retention_days: DEFAULT_LOG_RETENTION_DAYS,
            proxy: crate::netproxy::ProxySetting::default(),
            window_sizes: WindowSizes::default(),
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
    ///
    /// 代理地址走同一套思路，只是「收」的方向不同：一个认不出形状的手填地址**退回直连**，
    /// 而不是退回「跟随系统」。跟随系统等于把这台机器的出口换成这台机器上配着的那个代理 ——
    /// 那正是用户填一个手动地址时要绕开的东西；直连则至少是「没代理时本来会怎么走」。
    ///
    /// 配色退的是**默认档**（跟随系统）：它和日志天数一样，越界值没有「更接近用户意图」的
    /// 方向可猜，而三档里只有它是「让用户自己挑」以外的兜底。
    pub fn clamped(mut self) -> Self {
        use crate::netproxy::ProxySetting;
        self.theme = normalize_theme(&self.theme).to_string();
        self.log_retention_days = clamp_retention(self.log_retention_days);
        self.proxy = match &self.proxy {
            ProxySetting::Manual { url } => match crate::netproxy::normalize_proxy_url(url) {
                Some(u) => ProxySetting::Manual { url: u },
                None => ProxySetting::Direct,
            },
            other => other.clone(),
        };
        self.window_sizes = self.window_sizes.clamped();
        self
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
            }
        }
        // 原子写：先写 `.part` 临时文件再 `rename` 覆盖，避免崩溃/掉电把 settings.json 截成半截。
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "settings".to_string());
        let tmp = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{file_name}.part"));
        std::fs::write(&tmp, &s).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }
}

pub fn clamp_retention(days: u32) -> u32 {
    days.clamp(MIN_LOG_RETENTION_DAYS, MAX_LOG_RETENTION_DAYS)
}

/// 配色档位收敛：三档之外的值都当没设过。
pub fn normalize_theme(s: &str) -> &'static str {
    THEMES.iter().copied().find(|t| *t == s).unwrap_or(DEFAULT_THEME)
}

/// 这一档此刻该渲染成哪一套颜色（`"light"` / `"dark"`，写进 `<html data-theme>`）。
///
/// `os_prefers_dark` 是 [`crate::platform::NetworkPlatform::ui_prefers_dark`] 的回答，
/// 由调用方递进来而不是在这里自己问：「跟随系统」要问系统，而**问不到**的时候得有人决定
/// 按哪套渲染 —— 深色是这套界面的设计基准，也是没有任何线索时的答案。把它做成参数，
/// 这一条判据就有了单元测试，而不是只能在某台特定机器上眼看。
pub fn theme_for(setting: &str, os_prefers_dark: Option<bool>) -> &'static str {
    match normalize_theme(setting) {
        "light" => "light",
        "dark" => "dark",
        // system
        _ => {
            if os_prefers_dark.unwrap_or(true) {
                "dark"
            } else {
                "light"
            }
        }
    }
}

/// [`theme_for`] 的现问系统版本。
pub fn theme_now(setting: &str) -> &'static str {
    theme_for(setting, crate::platform::system_prefers_dark())
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
    use crate::netproxy::ProxySetting;

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

        let cfg = AppConfig {
            language: Some("ja".into()),
            theme: "light".into(),
            log_retention_days: 30,
            proxy: ProxySetting::Manual {
                url: "http://127.0.0.1:7890".into(),
            },
            window_sizes: WindowSizes::default(),
        };
        cfg.save(&p).unwrap();
        assert_eq!(AppConfig::load(&p).unwrap(), cfg);

        // 部分字段：缺 retention 用默认值，而不是 0
        std::fs::write(&p, r#"{"language":"ko"}"#).unwrap();
        let got = AppConfig::load(&p).unwrap();
        assert_eq!(got.language.as_deref(), Some("ko"));
        assert_eq!(got.log_retention_days, DEFAULT_LOG_RETENTION_DAYS);
        // 同样地，缺 proxy 是「跟随系统」而不是「直连」：一份从来没有过这个字段的旧文件
        // 不该在升级后变成「代理客户端开着也不走代理」。
        assert_eq!(got.proxy, ProxySetting::System);
        // 缺 theme 是「跟随系统」。一份从来没有过这个字段的文件不该在升级后被钉死成
        // 某一套配色 —— 用户要的是界面跟着他自己系统的样子走。
        assert_eq!(got.theme, DEFAULT_THEME);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 手改坏（或从别的机器备过来）的代理地址在加载时就收掉，而且退向是**直连**：
    /// 跟随系统等于换回用户刚亲手否掉的那个出口。
    #[test]
    fn a_nonsense_proxy_address_falls_back_to_direct() {
        let bad = AppConfig {
            language: None,
            theme: DEFAULT_THEME.into(),
            log_retention_days: 7,
            proxy: ProxySetting::Manual {
                url: "file:///etc/passwd".into(),
            },
            window_sizes: WindowSizes::default(),
        }
        .clamped();
        assert_eq!(bad.proxy, ProxySetting::Direct);
        let good = AppConfig {
            language: None,
            theme: DEFAULT_THEME.into(),
            log_retention_days: 7,
            proxy: ProxySetting::Manual {
                url: "SOCKS5://10.0.0.1:1080".into(),
            },
            window_sizes: WindowSizes::default(),
        }
        .clamped();
        assert_eq!(
            good.proxy,
            ProxySetting::Manual {
                url: "socks5://10.0.0.1:1080".into()
            }
        );
    }

    /// 手改出来的窗口尺寸在加载时就收住：0 会让窗口缩到看不见，而一个荒谬的大值会在
    /// 下次打开时把窗口撑到工作区之外（真正的收敛发生在打开时，这里挡的是配置文件里
    /// 那些连「看起来像个尺寸」都算不上的值）。
    #[test]
    fn window_sizes_are_clamped_on_load() {
        let sizes = WindowSizes {
            main: Some((0, 999_999)),
            settings: Some((640, 480)),
            logs: None,
        }
        .clamped();
        assert_eq!(sizes.main, Some((MIN_WINDOW_SIZE, MAX_WINDOW_SIZE)));
        assert_eq!(sizes.settings, Some((640, 480)));
        assert_eq!(sizes.logs, None);
    }

    /// 从来没调过窗口大小的配置里不出现 `window_sizes`：它是偏好而不是配置，写进去
    /// 只会让每一份 settings.json 多一块噪声，也让「用户到底调过没有」看不出来。
    #[test]
    fn window_sizes_stay_out_of_the_file_until_a_window_is_resized() {
        let s = serde_json::to_string(&AppConfig::default()).unwrap();
        assert!(!s.contains("window_sizes"), "unexpected key in: {s}");

        let mut with = AppConfig::default();
        with.window_sizes.main = Some((1180, 700));
        let s = serde_json::to_string(&with).unwrap();
        assert!(s.contains("window_sizes"), "size was dropped: {s}");
        // 读回来还是同一个值 —— 尺寸走的是同一份文件、同一条 serde 链路
        let back: AppConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.window_sizes.main, Some((1180, 700)));
    }

    #[test]
    fn retention_is_clamped_instead_of_refused_or_trusted() {
        assert_eq!(clamp_retention(0), MIN_LOG_RETENTION_DAYS);
        assert_eq!(clamp_retention(9_999), MAX_LOG_RETENTION_DAYS);
        assert_eq!(clamp_retention(14), 14);
        // 手改出来的越界值也要在加载时就收住：调用方拿到的永远是可执行的值
        assert_eq!(
            AppConfig {
                language: None,
                theme: DEFAULT_THEME.into(),
                log_retention_days: 0,
                proxy: ProxySetting::default(),
                window_sizes: WindowSizes::default(),
            }
            .clamped()
            .log_retention_days,
            MIN_LOG_RETENTION_DAYS
        );
    }

    /// 配色档位的两条退路：认不出的值当没设过（不能让一份手改坏的 `settings.json` 连带
    /// 语言与日志保留一起读不出来），而「跟随系统」在问不到系统时按深色渲染 —— 深色是这套
    /// 界面的设计基准，也是没有任何线索时该给的样子。
    #[test]
    fn theme_normalizes_unknown_settings_and_resolves_the_system_step() {
        assert_eq!(normalize_theme("light"), "light");
        assert_eq!(normalize_theme("Dark"), DEFAULT_THEME);
        assert_eq!(normalize_theme("blue"), DEFAULT_THEME);
        assert_eq!(theme_for("light", Some(true)), "light");
        assert_eq!(theme_for("dark", Some(false)), "dark");
        assert_eq!(theme_for("system", Some(true)), "dark");
        assert_eq!(theme_for("system", Some(false)), "light");
        assert_eq!(theme_for("system", None), "dark");
        // 认不出的那一档走的就是「跟随系统」这条路，不是另发明一个第四态。
        assert_eq!(theme_for("blue", Some(false)), "light");
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
