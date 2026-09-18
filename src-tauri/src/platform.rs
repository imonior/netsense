//! 平台抽象层（PAL）— 统一契约，三平台各自独立实现。
//!
//! ```text
//! platform.rs            本文件：trait 契约 + 共享类型 + 共享工具 + 编译期平台选择
//! platform/macos.rs      networksetup / arp / airport / route / osascript
//! platform/windows.rs    netsh / PowerShell(CIM) / route / UAC 提权
//! platform/linux.rs      nmcli / ip / pkexec
//! ```
//!
//! 上层（`main` / `ipc` / `core`）只依赖 [`Platform`]（编译期选定的实现）
//! 与 [`NetworkPlatform`] trait，不直接接触任何系统命令 —— 这是跨平台的关键边界。

use crate::config::Profile;
use serde::Serialize;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
compile_error!("NetSense 仅支持 macOS / Windows / Linux 三个平台");

/// 网卡/网络状态快照。会被 IPC 直接 JSON 化返回给前端，故需 `Serialize`。
#[derive(Debug, Clone, Default, Serialize)]
pub struct InterfaceStatus {
    pub connected: bool,
    pub ssid: Option<String>,
    /// 信号强度（dBm）。Windows 由百分比换算；Linux 取 nmcli SIGNAL。
    pub rssi: Option<i32>,
    pub ipv4: Option<String>,
    pub netmask: Option<String>,
    pub gateway: Option<String>,
    /// 网关 MAC（默认网关 IP → ARP/邻居表解析），独立匹配条件之一
    pub gateway_mac: Option<String>,
    /// 当前 AP 的 BSSID，独立匹配条件之一
    pub bssid: Option<String>,
    pub dns: Option<String>,
    pub v6mode: Option<String>,
    /// 当前无线接口名（macOS: en0 / Windows: Wi-Fi / Linux: wlan0）
    pub iface: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProbeTarget {
    pub mode: crate::config::ProbeMode,
    pub http_target: Option<String>,
    pub icmp_target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    Fail,
}

/// SSID 监视句柄：`stop()` 后后台线程会在下一轮退出。
pub struct WatcherHandle {
    stop: Arc<AtomicBool>,
}

impl WatcherHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// 特权执行通道（跨平台统一语义，各平台自行映射到本地机制）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivChannel {
    /// 已具备提权能力，操作不弹授权框。
    /// macOS: `/etc/sudoers.d/netsense` 授权的白名单脚本（NOPASSWD）
    /// Windows: 进程本身以管理员身份运行
    /// Linux: `sudo -n` 可用
    Direct,
    /// 每次操作需系统授权。
    /// macOS: `osascript ... with administrator privileges`
    /// Windows: UAC (`Start-Process -Verb RunAs`)
    /// Linux: `pkexec`
    Prompt,
}

impl PrivChannel {
    pub fn code(self) -> &'static str {
        match self {
            PrivChannel::Direct => "direct",
            PrivChannel::Prompt => "prompt",
        }
    }
}

/// PAL 契约：Core Engine 只依赖此 trait，不直接碰系统命令。
pub trait NetworkPlatform: Send + Sync {
    fn watch_ssid(&self, cb: Box<dyn Fn(Option<String>) + Send + Sync>) -> WatcherHandle;
    fn get_current_ssid(&self) -> Option<String>;
    fn get_status(&self) -> InterfaceStatus;

    /// 下发网络配置（IP/掩码/网关/DNS/IPv6）
    fn apply_profile(&self, p: &Profile) -> Result<(), String>;
    /// 回落保底：切回 DHCP + 清空自定义 DNS
    fn set_dhcp(&self) -> Result<(), String>;

    // 扩展1：独立匹配条件
    fn resolve_gateway_mac(&self) -> Option<String>;
    fn resolve_bssid(&self) -> Option<String>;

    // 扩展2：健康度监测（Fail = 判定网络不可达）
    fn probe(&self, target: &ProbeTarget, timeout_ms: u64) -> Health;

    // 扩展3：自动化任务
    fn add_route(&self, dest: &str, gw: &str, metric: u32) -> Result<(), String>;
    fn delete_route(&self, dest: &str) -> Result<(), String>;
    fn launch_app(&self, app: &str, args: &[String]) -> Result<(), String>;
    /// 受 allow-list 约束（调用方 automation 层先校验，再调用本方法执行）
    fn run_script(&self, path: &str, args: &[String], elevated: bool) -> Result<(), String>;

    /// 已保存的无线网络列表（编辑器下拉填充）。平台不支持时返回 `None`。
    fn list_known_ssids(&self) -> Option<Vec<String>>;
}

// —————————————————————————— 共享工具 ——————————————————————————

/// 普通执行（无需提权），返回 stdout 文本，失败返回 stderr 文本。
pub(crate) fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("spawn {}: {}", program, e))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            format!("{} 退出码 {:?}", program, out.status.code())
        } else {
            err
        })
    }
}

/// 以 `String` 参数调用（参数来自配置，需要 owned 形态）。
pub(crate) fn run_owned(program: &str, args: &[String]) -> Result<String, String> {
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run(program, &refs)
}

/// 通用 SSID 轮询监视（三平台共用）：未连接 2s / 已连接 5s 自适应，
/// 分片睡眠保证 `stop()` 后最多 500ms 退出；首轮只建基线不回调
/// （启动时的首次应用由 `main.rs` 负责，避免重复触发 `on_apply`）。
pub(crate) fn poll_ssid_watch<F>(
    get: F,
    cb: Box<dyn Fn(Option<String>) + Send + Sync>,
) -> WatcherHandle
where
    F: Fn() -> Option<String> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    std::thread::spawn(move || {
        let mut last: Option<String> = None;
        let mut initialized = false;
        while !stop_thread.load(Ordering::SeqCst) {
            let cur = get();
            if !initialized {
                initialized = true;
                last = cur;
            } else if cur != last {
                last = cur.clone();
                cb(cur);
            }
            let secs = if last.is_none() { 2 } else { 5 };
            let mut ticks = 0;
            while ticks < secs * 2 && !stop_thread.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(500));
                ticks += 1;
            }
        }
    });
    WatcherHandle { stop }
}

/// 解析 `"Key: value"` 形式的行，返回 value。
pub(crate) fn parse_kv(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(key) {
            let rest = rest.trim_start_matches(':').trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

pub(crate) fn is_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// 在任意文本行里找出第一个 `aa:bb:cc:dd:ee:ff` 形态的 MAC。
pub(crate) fn extract_mac(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    if bytes.len() < 17 {
        return None;
    }
    let mut i = 0;
    while i + 17 <= bytes.len() {
        let s = &line[i..i + 17];
        if is_mac(s) {
            return Some(s.to_string());
        }
        i += 1;
    }
    None
}

/// MAC 归一化：统一小写 + 冒号分隔，供匹配比较使用。
/// 各平台/各命令返回的 MAC 大小写与分隔符可能不一致（`AA-BB-...` / `aa:bb:...`）。
pub fn normalize_mac(s: &str) -> String {
    let hex: String = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if hex.len() != 12 {
        return s.trim().to_ascii_lowercase();
    }
    hex.as_bytes()
        .chunks(2)
        .map(|c| std::str::from_utf8(c).unwrap_or("").to_string())
        .collect::<Vec<_>>()
        .join(":")
}

/// 秒级超时（供 ping 等以秒为单位的命令使用，最小 1 秒）。
pub(crate) fn timeout_secs(ms: u64) -> String {
    ((ms.max(1000) / 1000).max(1)).to_string()
}

/// 用系统默认程序打开文件/目录/URL（编辑器"打开日志"等场景）。
#[cfg(target_os = "macos")]
pub fn open_path(path: &str) -> Result<(), String> {
    run("open", &[path]).map(|_| ())
}

#[cfg(target_os = "windows")]
pub fn open_path(path: &str) -> Result<(), String> {
    // explorer 是 Windows 上唯一无需额外依赖即可打开目录/文件的入口。
    // 注意 explorer 对成功常返回非 0 退出码，故只看 spawn 是否成功。
    Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("explorer {}: {}", path, e))
}

#[cfg(target_os = "linux")]
pub fn open_path(path: &str) -> Result<(), String> {
    run("xdg-open", &[path]).map(|_| ())
}

/// 当前平台标识（日志 / UI / 诊断用）。
pub const fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

// —————————————————————————— 编译期平台选择 ——————————————————————————

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
pub use macos::{priv_channel, MacPlatform as Platform};
#[cfg(target_os = "windows")]
pub use windows::{priv_channel, WindowsPlatform as Platform};
#[cfg(target_os = "linux")]
pub use linux::{priv_channel, LinuxPlatform as Platform};
