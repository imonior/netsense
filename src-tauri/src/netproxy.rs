//! 升级请求的对外出口：关于「代理」这件事的知识都集中在这里。
//!
//! 本应用只有两处会主动往外发请求：检查更新的 GitHub API、下载安装包（顺带取校验值）。
//! 引擎、条件求值、3A/3B 一次对外请求都不发 —— 所以这份设置改变的是**本应用怎么上网**，
//! 不是自动化行为，它属于 `settings.json` 而不是 `config.json`（判据见 `appconfig.rs`）。
//!
//! ## 三态各自的含义
//!
//! - 直连：明确不要用任何代理。这不只是「不传代理参数」—— 代理也常用**环境变量**交进来
//!   （Linux 桌面会话里 `HTTP_PROXY` 是会跟着 curl 走的），所以这里传的是 `--noproxy "*"`，
//!   一句「不要用代理」而不是「我什么都没说」。
//! - 跟随系统：读操作系统自己的代理设置。macOS 上这是必需的而不是锦上添花：`curl` 完全
//!   不看系统代理，只传代理参数过去才真的走。Linux 上 `curl` 本来就读 `http_proxy`，
//!   这里额外读 GNOME 的系统设置（KDE 那份没有统一的命令行走，读不到时等同没配）。
//!   Windows 上是**唯一一条不需要探测**的分支：检查更新走 PowerShell，而 PowerShell 默认
//!   就跟随系统代理。
//! - 手动：用写死的这一个地址，三个平台、两次请求都只认它。
//!
//! ## 为什么探测函数不收在 trait 里
//!
//! 读系统代理没有任何平台 API 要用，三条分支都是「跑一条系统命令、解析它的输出」，
//! 而 `NetworkPlatform` 契约是网卡/IP/DNS 那一类。为它撑大契约，换到的是每个平台都要
//! 多实现一个与网络切换无关的方法。真正需要小心的解析部分（`scutil` 与 `gsettings` 的
//! 输出形状）都是纯函数，在任何一台机器上都能编译与测试。

use serde::{Deserialize, Serialize};

/// 用户选的出口。`settings.json` 里的 `ProxySetting` 是它的存储形状，这一层是给
/// 具体某一次请求用的（见 [`crate::appconfig::ProxySetting::choice`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyChoice {
    /// 明确直连。
    Direct,
    /// 跟随系统代理设置。
    Follow,
    /// 用这一个地址（已经过 [`normalize_proxy_url`]）。
    Use(String),
}

/// 一个代理地址允许的方案名。这几个正是 `curl --proxy` 认的写法，而本应用的两次对外请求
/// 都是 curl 与 PowerShell 发出去的 —— 存得进来的必须能真的生效。
const PROXY_SCHEMES: [&str; 6] = ["http", "https", "socks4", "socks4a", "socks5", "socks5h"];

/// 地址长度上限。一个 `scheme://host:port` 用不到 512 个字符，超出的一律当写错了处理：
/// 这个值会成为一条命令行的参数，长度本身就是异常信号。
const MAX_PROXY_LEN: usize = 512;

/// 把界面上填的代理地址规范化；认不出来时返回 `None`（调用方决定怎么报错）。
///
/// 只接受 `scheme://host[:port]`，允许 `user:pass@` 前缀，scheme 必须落在
/// [`PROXY_SCHEMES`] 里，路径段（`http://host:port/x`）不接受。三处严格都是有意为之：
/// scheme 白名单顺带挡掉 `file://` / `data://` / `ftp://`，一个「代理地址」不该有本事把更新
/// 请求变成本地文件读取；空白与控制字符一律拒绝，因为这个字符串会原样成为一个进程参数
/// （Windows 上还会进 PowerShell 的单引号串），而空格与换行是拼出第二条参数的唯一途径；
/// 代理地址里的路径段没有任何语义，收下只会让人以为它起了作用。
pub fn normalize_proxy_url(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.len() > MAX_PROXY_LEN {
        return None;
    }
    if s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let (scheme, rest) = s.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if !PROXY_SCHEMES.contains(&scheme.as_str()) {
        return None;
    }
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(format!("{scheme}://{rest}"))
}

/// 这一次请求该给 curl 追加哪些代理参数。`system` 是**现在**读到的系统代理地址
/// （只在 [`ProxyChoice::Follow`] 时用到；读不到就是 `None`，等同没配代理）。
///
/// 纯函数，平台与探测都不在这里 —— 见 [`curl_proxy_args`]。
pub fn proxy_args(choice: &ProxyChoice, system: Option<&str>) -> Vec<String> {
    match choice {
        // `--noproxy "*"` 而不是「什么都不传」：环境变量里的代理要能被明确否掉。
        ProxyChoice::Direct => vec!["--noproxy".to_string(), "*".to_string()],
        ProxyChoice::Use(url) => vec!["--proxy".to_string(), url.clone()],
        ProxyChoice::Follow => match system {
            Some(url) => vec!["--proxy".to_string(), url.to_string()],
            None => vec![],
        },
    }
}

/// 当前平台上 [`ProxyChoice::Follow`] 此刻对应的系统代理地址；读不到（或这个平台不需要
/// 探测）时 `None`。
///
/// 每次调用都现问一次系统：代理是用户切个网络、开关一次代理客户端就会变的东西，缓存它
/// 等于造出一个「昨天还通、今天不明不白不通」的来源。
pub fn system_proxy() -> Option<String> {
    match std::env::consts::OS {
        "macos" => crate::platform::run("scutil", &["--proxy"])
            .ok()
            .and_then(|text| parse_scutil(&text)),
        "linux" => gnome_proxy(),
        // Windows：检查更新走 PowerShell，它本来就跟随系统代理；下载那一步读不到 WinINET
        // 是已知边界（`DEVELOPMENT.md` §13 的升级流程里写明了要手动指定代理的情形）。
        _ => None,
    }
}

/// 给一次真实的 curl 调用用的参数表。
pub fn curl_proxy_args(choice: &ProxyChoice) -> Vec<String> {
    let system = matches!(choice, ProxyChoice::Follow)
        .then(system_proxy)
        .flatten();
    proxy_args(choice, system.as_deref())
}

// —————————————————————————————— 探测与解析 ——————————————————————————————

/// `scutil --proxy` 的输出 → 代理地址。
///
/// 优先级 HTTPS 代理 → HTTP 代理 → SOCKS 代理：本应用要发的两条请求都是 https，
/// 系统里配了「HTTPS 代理」时按它走才是用户的意思。SOCKS 写成 `socks5://`，
/// 与 macOS 自己那份设置里 SOCKS 的默认语义一致。
///
/// 只看得到显式配置。`ProxyAutoConfigEnable : 1`（PAC）不会被解析成某个地址 —— 求值一段
/// JavaScript 不是这个函数该做的事，那种情况下返回 `None`，界面上会写「没读到系统代理」。
pub fn parse_scutil(text: &str) -> Option<String> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        // 形状是 `  HTTPProxy : 127.0.0.1`；分隔符两侧都可能有空格。
        if let Some((k, v)) = line.split_once(':') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let enabled = |flag: &str| map.get(flag).map(|v| v == "1").unwrap_or(false);
    let host = |key: &str| map.get(key).filter(|v| !v.is_empty() && **v != *"").cloned();
    let port = |key: &str| {
        map.get(key)
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|p| *p > 0)
    };
    for (scheme, host_key, port_key, flag) in [
        ("http", "HTTPSProxy", "HTTPSPort", "HTTPSEnable"),
        ("http", "HTTPProxy", "HTTPPort", "HTTPEnable"),
        ("socks5", "SOCKSProxy", "SOCKSPort", "SOCKSEnable"),
    ] {
        if !enabled(flag) {
            continue;
        }
        if let (Some(h), Some(p)) = (host(host_key), port(port_key)) {
            return Some(format!("{scheme}://{h}:{p}"));
        }
    }
    None
}

/// GNOME 的系统代理：`org.gnome.system.proxy mode` 为 `manual` 时，取 http 代理，
/// 其次 socks 代理。`none` / `auto`（PAC）都返回 `None`。
fn gnome_proxy() -> Option<String> {
    let mode = gsettings_get("org.gnome.system.proxy", "mode")?;
    if mode != "manual" {
        return None;
    }
    if let Some(url) = gnome_entry("org.gnome.system.proxy.http").map(|(h, p)| format!("http://{h}:{p}")) {
        return Some(url);
    }
    gnome_entry("org.gnome.system.proxy.socks").map(|(h, p)| format!("socks5://{h}:{p}"))
}

fn gnome_entry(schema: &str) -> Option<(String, u32)> {
    parse_gnome_entry(
        &gsettings_get(schema, "host")?,
        &gsettings_get(schema, "port")?,
    )
}

/// `gsettings` 的两行输出 → `(host, port)`。输出形状是 `'127.0.0.1'` 与 `int32 7890`，
/// 而「没配」表现为 `''` 与 `int32 0` —— 端口为 0 时这个条目不能用来发请求，返回 `None`
/// 让调用方去看下一个条目，而不是拼出一个 `http://host:0`。
pub fn parse_gnome_entry(host_text: &str, port_text: &str) -> Option<(String, u32)> {
    let host = host_text.trim().trim_matches('\'').trim();
    let port = port_text
        .trim()
        .strip_prefix("int32")
        .unwrap_or(port_text)
        .trim()
        .parse::<u32>()
        .ok()?;
    if host.is_empty() || port == 0 {
        return None;
    }
    Some((host.to_string(), port))
}

/// 一次 `gsettings get <schema> <key>`。命令不存在（非 GNOME 桌面）或报错时 `None`。
fn gsettings_get(schema: &str, key: &str) -> Option<String> {
    crate::platform::run("gsettings", &["get", schema, key])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 存储形状：`settings.json` 里 `proxy` 字段的三种写法。
///
/// 三个状态都存，且**不缓存**探测到的系统代理值 —— 那样一来会有两个会说不同话的来源
/// （用户可以在系统设置里就地改掉代理）。`manual` 里的 `url` 是唯一被存下来的地址。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxySetting {
    Direct,
    /// 缺省：没配过这个字段的老文件读出来也是它，代理客户端开着时检查更新才能真的用上它。
    #[default]
    System,
    Manual { url: String },
}

impl ProxySetting {
    /// 存下来的三态 → 这一次请求的出口。
    pub fn choice(&self) -> ProxyChoice {
        match self {
            ProxySetting::Direct => ProxyChoice::Direct,
            ProxySetting::System => ProxyChoice::Follow,
            ProxySetting::Manual { url } => ProxyChoice::Use(url.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机 `scutil --proxy` 的真实形状（本地代理客户端：http 与 socks 各一个端口）。
    /// 这里断言的是**取哪一条、拼成什么 scheme**：scheme 拼错时 curl 会安静地用错出口，
    /// 用户只看到「检查更新失败」，看不到原因。
    #[test]
    fn scutil_output_prefers_https_then_http_then_socks() {
        let both = "\
<dictionary> {
  ExcludeSimpleHostnames : 0
  HTTPEnable : 1
  HTTPPort : 7890
  HTTPProxy : 127.0.0.1
  HTTPSEnable : 1
  HTTPSPort : 7890
  HTTPSProxy : 127.0.0.1
  ProxyAutoConfigEnable : 0
  SOCKSEnable : 1
  SOCKSPort : 7891
  SOCKSProxy : 127.0.0.1
}";
        assert_eq!(parse_scutil(both).as_deref(), Some("http://127.0.0.1:7890"));

        // 只开了 SOCKS：scheme 必须是 socks5，否则拿 HTTP 语法去连一个 SOCKS 端口。
        let socks_only = "\
<dictionary> {
  HTTPEnable : 0
  HTTPPort : 7890
  HTTPProxy : 127.0.0.1
  SOCKSEnable : 1
  SOCKSPort : 7891
  SOCKSProxy : 127.0.0.1
}";
        assert_eq!(
            parse_scutil(socks_only).as_deref(),
            Some("socks5://127.0.0.1:7891")
        );

        // 只有 PAC：求值一段 JavaScript 不是这里的事，宁可不带代理。
        let pac = "\
<dictionary> {
  ProxyAutoConfigEnable : 1
  ProxyAutoConfigURLString : http://intranet/proxy.pac
}";
        assert_eq!(parse_scutil(pac), None);
        assert_eq!(parse_scutil("<dictionary> {\n}"), None);
        // 开了但没配端口：`host:0` 发不出请求，等同没配。
        let broken = "<dictionary> {\n  HTTPEnable : 1\n  HTTPProxy : 127.0.0.1\n}";
        assert_eq!(parse_scutil(broken), None);
    }

    /// `gsettings` 的输出是带引号的值加 `int32` 前缀，而「没配」表现为 `''` 与 `int32 0`。
    #[test]
    fn gnome_values_are_unquoted_and_zero_port_means_unset() {
        assert_eq!(
            parse_gnome_entry("'127.0.0.1'", "int32 7890"),
            Some(("127.0.0.1".to_string(), 7890))
        );
        assert_eq!(parse_gnome_entry("''", "int32 7890"), None);
        assert_eq!(parse_gnome_entry("'proxy.internal'", "int32 0"), None);
    }

    /// 「直连」必须传 `--noproxy "*"`：什么都不传只等于**没有表态**，环境变量里的代理照样生效。
    #[test]
    fn each_choice_becomes_a_distinct_curl_argument_set() {
        assert_eq!(proxy_args(&ProxyChoice::Direct, None), ["--noproxy", "*"]);
        assert_eq!(
            proxy_args(
                &ProxyChoice::Use("socks5://127.0.0.1:7891".into()),
                Some("http://10.0.0.1:3128")
            ),
            ["--proxy", "socks5://127.0.0.1:7891"]
        );
        assert_eq!(
            proxy_args(&ProxyChoice::Follow, Some("http://10.0.0.1:3128")),
            ["--proxy", "http://10.0.0.1:3128"]
        );
        assert!(proxy_args(&ProxyChoice::Follow, None).is_empty());
        // 三种存储形状各自翻成哪一个出口。
        assert_eq!(ProxySetting::Direct.choice(), ProxyChoice::Direct);
        assert_eq!(ProxySetting::System.choice(), ProxyChoice::Follow);
        assert_eq!(
            ProxySetting::Manual {
                url: "http://a:1".into()
            }
            .choice(),
            ProxyChoice::Use("http://a:1".into())
        );
    }

    /// 存下来的写法就是界面上那三个选择；旧文件没有这个字段时读出来是「跟随系统」。
    #[test]
    fn the_stored_shape_is_these_three_names() {
        let p = |s: ProxySetting| serde_json::to_string(&s).unwrap();
        assert_eq!(p(ProxySetting::Direct), r#""direct""#);
        assert_eq!(p(ProxySetting::System), r#""system""#);
        assert_eq!(
            p(ProxySetting::Manual { url: "http://a:1".into() }),
            r#"{"manual":{"url":"http://a:1"}}"#
        );
        assert_eq!(
            serde_json::from_str::<ProxySetting>(r#""system""#).unwrap(),
            ProxySetting::System
        );
        assert_eq!(
            serde_json::from_str::<ProxySetting>(r#"{"manual":{"url":"http://a:1"}}"#).unwrap(),
            ProxySetting::Manual {
                url: "http://a:1".into()
            }
        );
        assert_eq!(ProxySetting::default(), ProxySetting::System);
    }

    /// 接受/拒绝那张表里，被拒的不是「不好看」而是**不该发出去**：scheme 白名单挡掉
    /// `file://` 与 `data://`，空白检查挡掉把一个参数变成两条命令的写法。
    #[test]
    fn a_proxy_address_is_a_scheme_plus_an_authority() {
        for ok in [
            "http://127.0.0.1:7890",
            "  http://127.0.0.1:7890  ",
            "socks5://10.0.0.1:1080",
            "socks5h://10.0.0.1",
            "http://user:pw@10.0.0.1:8080",
        ] {
            assert!(normalize_proxy_url(ok).is_some(), "{ok} 该收下");
        }
        // scheme 小写存下来：显示的那一份和传出去的那一份必须是同一个字符串。
        assert_eq!(
            normalize_proxy_url("HTTPS://proxy.internal:3128").as_deref(),
            Some("https://proxy.internal:3128")
        );
        for bad in [
            "",
            "   ",
            "127.0.0.1:7890",
            "ftp://10.0.0.1:21",
            "file:///etc/passwd",
            "data://x",
            "http://",
            "http://127.0.0.1:7890/some/path",
            "http://127.0. 0.1:7890",
            "http://127.0.0.1:7890\n--proxy=http://evil",
            "http://\u{1}127.0.0.1:1",
            &format!("http://{}:1", "a".repeat(MAX_PROXY_LEN)),
        ] {
            assert_eq!(normalize_proxy_url(bad), None, "{bad} 不该收下");
        }
    }
}
