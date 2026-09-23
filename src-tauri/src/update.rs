//! 在线升级：检查 →（Homebrew 渠道）`brew upgrade --cask` /（独立安装）下载 + SHA256
//! 校验 + 平台安装器（提权）→ 重启。
//!
//! 交互模型：默认动作就是「下载并调用安装流程」，而不是只给一条命令或链接。下发过程中
//! 通过 `netsense://update_progress` 事件广播阶段与进度，前端据此显示进度。
//! 任一环节失败都回退到「打开 Release 页」这条永远可用的手动路径，不会把用户卡死。
//!
//! 安全：下载后必须用 Release 附带的 `SHA256SUMS` 校验，**拿不到可信哈希就中止安装**
//! （CI 在四条平台腿跑完后统一上传一份根级 SHA256SUMS，见 build.yml，所以正常发布永远
//! 读得到它；读不到说明手上的东西不是我们发出去的那份）。本项目暂无 Ed25519 签名密钥，
//! 故仅做 SHA256 —— 它与资产同经一次 HTTPS，防的是传输损坏而非发布方被篡改。
//! 下载与落地全程使用**独占创建**的私有临时路径（见「私有临时路径」一节）：共享 `/tmp` 上
//! 被别人抢先占用的名字，后果是提权执行的脚本内容可在我们写入之后再被改一次。
//! macOS 还会额外校验 app 的 codesign：
//! 新包可验证即放行；不可验证时，只有当**当前安装包同样未签名**（无 Apple 证书的
//! 分发形态）才告警放行，否则拒绝覆盖已签名版本。详见 `verify_codesign`。

use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use tauri::{AppHandle, Emitter};

/// 前端从 `check_update` 拿到后原样回传给 `run_update` 的最小描述。
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateTarget {
    pub download_url: String,
    pub asset_name: String,
    #[serde(default)]
    pub checksum_url: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    pub version: String,
}

/// 进度事件阶段：download（下载中）/ refresh（brew tap 刷新）/ install（安装器已启动）/
/// 空串 + percent 0 表示清除阶段（如回退到发布页）。
fn emit_progress(app: &AppHandle, phase: &str, percent: u32) {
    let _ = app.emit(
        "netsense://update_progress",
        serde_json::json!({ "phase": phase, "percent": percent }),
    );
}

// ———————————————————————————————— 入口 ————————————————————————————————

/// 端到端执行升级：Homebrew 安装走 `brew upgrade`，其余走「下载 + 校验 + 安装」。
/// 出错时返回 Err（前端据此回退到打开 Release 页）。
pub fn run_update(app: &AppHandle, t: &UpdateTarget) -> Result<(), String> {
    if is_brew_install() {
        return run_brew(app, t);
    }
    native_update(app, t)
}

fn native_update(app: &AppHandle, t: &UpdateTarget) -> Result<(), String> {
    // `UpdateTarget` 是前端回传的，按外部输入对待：scheme 只认 https。
    require_https(&t.download_url)?;
    let cu = t
        .checksum_url
        .as_ref()
        .filter(|u| !u.trim().is_empty())
        .ok_or("release carries no SHA256SUMS: refusing to install an unverified asset")?;
    require_https(cu)?;

    emit_progress(app, "download", 0);
    let tmp = download_file(app, &t.download_url, &t.asset_name, t.size)?;

    // 校验 SHA256：任何一环拿不到可信哈希都中止，不「告警后继续」。
    let sums = crate::ipc::fetch_url(cu).map_err(|e| format!("fetch SHA256SUMS failed: {e}"))?;
    let expected = parse_hash(&sums, &t.asset_name)
        .ok_or_else(|| format!("{} not listed in SHA256SUMS", t.asset_name))?;
    let actual = sha256_file(&tmp)?;
    if !eq_ignore_case(&actual, &expected) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "checksum mismatch for {}: expected {}, got {}",
            t.asset_name, expected, actual
        ));
    }
    crate::log::info(&format!("update: {} SHA256 verified", t.asset_name));
    // 校验过的字节和随后安装的字节必须是同一份：共享 temp 上若文件属主不是我们，
    // 对方可以在这两步之间换掉内容。
    #[cfg(unix)]
    if let Err(e) = verify_private(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    emit_progress(app, "install", 0);
    let r = install(&tmp);
    // 安装器已启动/已替换：延迟删除临时文件（Windows 安装器可能仍锁定它，忽略错误）。
    let _ = std::fs::remove_file(&tmp);
    r
}

// ———————————————————————————————— 下载 ————————————————————————————————

const MIN_ASSET: u64 = 1 << 20; // 1 MB：打包应用不可能更小，过小视为损坏/占位

static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

fn download_file(
    app: &AppHandle,
    url: &str,
    name: &str,
    expect_size: Option<u64>,
) -> Result<PathBuf, String> {
    let dest = new_private_file("update", &format!("-{}", sanitize(name)))?;

    // 用 spawn 而非 output：curl 下载大包可能持续数十秒，期间轮询临时文件大小，
    // 把「已下载/总大小」换算成百分比推给前端（GitHub 资产带 size，故总大小已知）。
    //
    // `--proto =https` 让 `-L` 的跳转目标也必须是 https（GitHub 的资产下载正是 302）。这里
    // **故意不加** `--proto-redir`：它要 curl 7.65.2，而 Windows 10 1803 —— 也就是 `main.rs`
    // 承诺的下限 —— 自带 curl 7.60.1，认不出的选项会让 curl 直接退出，把「能更新」变成「下载失败」，
    // 换来的是 `--proto` 已经覆盖不到的零额外约束。
    let mut child = Command::new("curl")
        .args([
            "-fsSL",
            "--proto",
            "=https",
            "--retry",
            "2",
            "--max-time",
            "300",
            "-o",
        ])
        .arg(&dest)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("download failed to start curl: {e}"))?;

    let mut last = 0u32;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if let Some(total) = expect_size.filter(|s| *s > 0) {
                    if let Ok(m) = std::fs::metadata(&dest) {
                        let p = ((m.len().saturating_mul(100) / total).min(100)) as u32;
                        if p > last {
                            last = p;
                            emit_progress(app, "download", p);
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                return Err(format!("download wait failed: {e}"));
            }
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("download wait: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&dest);
        return Err(format!(
            "download failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let meta = std::fs::metadata(&dest).map_err(|e| format!("download: stat temp: {e}"))?;
    if let Some(sz) = expect_size {
        if meta.len() != sz {
            let _ = std::fs::remove_file(&dest);
            return Err(format!(
                "download size mismatch: expected {sz}, got {}",
                meta.len()
            ));
        }
    } else if meta.len() < MIN_ASSET {
        let _ = std::fs::remove_file(&dest);
        return Err(format!(
            "downloaded file too small ({} bytes) — refusing to install",
            meta.len()
        ));
    }
    Ok(dest)
}

/// 仅保留文件名中的安全字符，避免路径穿越/奇怪字符破坏临时文件名。
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect()
}

// ———————————————————————————— 私有临时路径 ————————————————————————————

/// 只接受 https 的升级来源。
pub(crate) fn require_https(url: &str) -> Result<(), String> {
    // `get(..8)` 而不是 `&url[..8]`：非 ASCII 的输入不能在此 panic。
    if url.get(..8).map(|p| p.eq_ignore_ascii_case("https://")) == Some(true) {
        Ok(())
    } else {
        Err(format!("refusing non-https update url: {url}"))
    }
}

/// 造一个别人猜不到、也没法抢先占用的临时路径名：pid + 纳秒时戳 + 进程内单调序号。
///
/// 光有 pid 不够，但**不可预测的名字本身不是防线，独占创建才是**：`File::create` 落在
/// 别人已建好的文件上时属主仍是对方的，共享 `/tmp` 的粘滞位又不允许我们去删它，于是对方
/// 随时能在我们写完之后、root 执行之前把内容再改一次（CWE-377）。所以候选名一律配合
/// `create_new` / `mkdir` 使用 —— 撞上就换一个，绝不复用。
fn temp_candidate(prefix: &str, suffix: &str) -> PathBuf {
    let seq = TMP_SEQ.fetch_add(1, Ordering::SeqCst);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "netsense-{prefix}-{}-{stamp}-{seq}{suffix}",
        std::process::id()
    ))
}

/// 独占创建（已存在、甚至只是一个同名符号链接，都直接失败）。
fn open_private_new(p: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    // umask 只会让实际权限比这更小、不会更大，所以显式 0600 已经保证「不宽于 0600」。
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(p)
}

/// 在临时目录里独占开一个私有文件，返回路径（内容留给调用方写）。
fn new_private_file(prefix: &str, suffix: &str) -> Result<PathBuf, String> {
    for _ in 0..8 {
        let p = temp_candidate(prefix, suffix);
        match open_private_new(&p) {
            Ok(_) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create private temp file: {e}")),
        }
    }
    Err("no unoccupied temp file name after 8 attempts".to_string())
}

/// 确认路径确实归当前用户所有，且组/其他位没有任何权限。
///
/// 独占创建已经保证了这两点，这里查的是**换目录**那一手：`$TMPDIR` 若指向一个他人可替换
/// 的目录，我们的写入发生在旧目录，而提权后按路径重新解析的 `/bin/sh` 会落到对方摆好的
/// 同名文件上。名字里有纳秒时戳，对方要猜中才能摆，但这一检查是常数成本，不赌。
#[cfg(unix)]
fn verify_private(p: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let md = p.metadata().map_err(|e| format!("stat {}: {e}", p.display()))?;
    if md.uid() != read_uid() {
        return Err(format!("{} is owned by another user", p.display()));
    }
    if md.mode() & 0o077 != 0 {
        return Err(format!("{} is accessible to other users", p.display()));
    }
    Ok(())
}

#[cfg(unix)]
fn read_uid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    // 与 `libc_kill` 同样的理由：为一个调用点不值得引入 `libc`。
    unsafe { getuid() }
}

/// 独占写出一个待执行的私有脚本（0600：`/bin/sh <path>` 不需要执行位）。
#[cfg(unix)]
fn write_private_script(prefix: &str, content: &str) -> Result<PathBuf, String> {
    let p = new_private_file(prefix, ".sh")?;
    std::fs::write(&p, content).map_err(|e| {
        let _ = std::fs::remove_file(&p);
        format!("write {prefix} script: {e}")
    })?;
    if let Err(e) = verify_private(&p) {
        let _ = std::fs::remove_file(&p);
        return Err(e);
    }
    Ok(p)
}

/// 独占创建（0700）一个私有临时目录。同名目录已存在 —— 包括对方预先摆好的 —— 就换名重试。
#[cfg(target_os = "macos")]
fn new_private_dir(prefix: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    for _ in 0..8 {
        let p = temp_candidate(prefix, "");
        match std::fs::DirBuilder::new().mode(0o700).create(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create private temp dir: {e}")),
        }
    }
    Err("no unoccupied temp dir name after 8 attempts".to_string())
}

// ———————————————————————————————— 校验 ————————————————————————————————

fn sha256_file(p: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(p).map_err(|e| format!("sha256 open: {e}"))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("sha256 read: {e}"))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(to_hex(&h.finalize()))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// 从 SHA256SUMS 风格文本里取 `asset_name` 对应的哈希。支持 GNU（`hash  file` /
/// `hash *file`）与 BSD（`SHA256 (file) = hash`）两种格式。
pub(crate) fn parse_hash(body: &str, asset_name: &str) -> Option<String> {
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // BSD 风格
        if let Some(m) = line.strip_prefix("SHA256 (") {
            if let Some((name, hash)) = m.split_once(") = ") {
                if name.eq_ignore_ascii_case(asset_name) {
                    return Some(hash.trim().to_string());
                }
            }
            continue;
        }
        // GNU 风格：hash 之后第一个空白分隔文件名
        let idx = line.find([' ', '\t'])?;
        let hash = &line[..idx];
        let filename = line[idx..].trim_start().trim_start_matches('*');
        if filename.eq_ignore_ascii_case(asset_name) && is_hex(hash) {
            return Some(hash.to_string());
        }
    }
    None
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// ———————————————————————————————— 平台安装 ————————————————————————————————

/// 把已经落盘、已经校验过的资产装进系统。签名里没有 `UpdateTarget`：到了安装阶段，
/// 该信的只有那个验过哈希的文件，前端回传的描述已经用完了。
fn install(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        install_macos(path)
    }
    #[cfg(target_os = "windows")]
    {
        return install_windows(path);
    }
    #[cfg(target_os = "linux")]
    {
        return install_linux(path);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = path;
        Err("unsupported OS for native in-place update".to_string())
    }
}

// ———————————————————————————————— macOS ————————————————————————————————

/// 本次升级的临时落地目录（dmg 挂载点 / 解压目录），离开作用域即回收。
#[cfg(target_os = "macos")]
struct Staging {
    root: PathBuf,
    /// true = `hdiutil` 挂进来的卷：必须先 detach，否则 `remove_dir_all` 只删得掉挂载点，
    /// 卷还挂在那儿，每升级一次攒一个。
    mount: bool,
}

#[cfg(target_os = "macos")]
impl Drop for Staging {
    fn drop(&mut self) {
        if self.mount {
            let _ = Command::new("hdiutil")
                .args(["detach", self.root.to_str().unwrap_or_default(), "-force", "-quiet"])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(target_os = "macos")]
fn install_macos(asset_path: &Path) -> Result<(), String> {
    // `_staging` 必须活到本函数结束：安装脚本里的 `ditto` 还在读它，回收只能在其后。
    // 原来那个「按 pid 反推路径再 cleanup」的函数因此可以整块删掉 —— 目录名现在带纳秒，
    // 反推不出来，也不再需要反推。
    let (app_path, _staging) = extract_mac_app(asset_path)?;
    if app_path.file_name().map(|n| n.to_string_lossy().to_string()) != Some("NetSense.app".to_string()) {
        return Err("unexpected app bundle name (expected NetSense.app)".to_string());
    }
    verify_codesign(&app_path)?;

    let target = darwin_install_target();
    let script = darwin_install_script(&app_path, &target);
    let script_path = write_private_script("update-install", &script)?;
    let elevated = !dir_writable(Path::new(&target).parent().unwrap_or_else(|| Path::new("/Applications")));

    let out = if elevated {
        // /Applications 等系统目录需提权：osascript 弹标准密码框，以 root 跑脚本
        let apa = applescript_quote(script_path.to_str().unwrap_or_default());
        Command::new("osascript")
            .args(["-e", &format!("do shell script {apa} with administrator privileges")])
            .output()
            .map_err(|e| format!("elevated install start: {e}"))
    } else {
        Command::new("/bin/sh")
            .arg(&script_path)
            .output()
            .map_err(|e| format!("install failed: {e}"))
    };
    let _ = std::fs::remove_file(&script_path);
    let out = out?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "{} failed: {}",
        if elevated { "elevated install" } else { "install" },
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

#[cfg(target_os = "macos")]
fn extract_mac_app(asset_path: &Path) -> Result<(PathBuf, Staging), String> {
    match Path::new(asset_path).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("dmg") => extract_dmg(asset_path),
        Some("zip") => extract_zip(asset_path),
        _ => Err(format!(
            "unsupported macOS update asset (expected .dmg or .zip): {}",
            asset_path.display()
        )),
    }
}

#[cfg(target_os = "macos")]
fn extract_dmg(dmg: &Path) -> Result<(PathBuf, Staging), String> {
    let mount = new_private_dir("update-mnt")?;
    let staging = Staging { root: mount.clone(), mount: true };
    let out = Command::new("hdiutil")
        .args(["attach", dmg.to_str().unwrap_or_default(), "-nobrowse", "-readonly", "-mountpoint"])
        .arg(&mount)
        .output()
        .map_err(|e| format!("mount dmg: {e}"))?;
    if !out.status.success() {
        return Err(format!("mount dmg: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let app = find_app_bundle(&mount);
    if app.as_os_str().is_empty() {
        return Err("no .app bundle found inside the dmg".to_string());
    }
    Ok((app, staging))
}

#[cfg(target_os = "macos")]
fn extract_zip(zip: &Path) -> Result<(PathBuf, Staging), String> {
    // 解进独占创建的 0700 空目录：解压进一个预先被人摆好的目录时，`find_app_bundle`
    // 挑到的可能是对方放的 `NetSense.app`，而它随后会被 root 的 `ditto` 装进 /Applications。
    let dest = new_private_dir("update-unzip")?;
    let staging = Staging { root: dest.clone(), mount: false };
    let out = Command::new("ditto")
        .args(["-x", "-k", zip.to_str().unwrap_or_default(), dest.to_str().unwrap_or_default()])
        .output()
        .map_err(|e| format!("extract zip: {e}"))?;
    if !out.status.success() {
        return Err(format!("extract zip: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let app = find_app_bundle(&dest);
    if app.as_os_str().is_empty() {
        return Err("no .app bundle found inside the zip".to_string());
    }
    Ok((app, staging))
}

/// 校验新包的代码签名。
///
/// ⚠️ 不能无条件硬失败：本项目在**未配置 Apple 证书**时产出的是未签名/adhoc 版 dmg
/// （见 build.yml 的「未配置 Apple 证书：跳过签名」分支），硬性 `codesign --verify`
/// 会恒失败，等于把所有人的应用内自更新直接废掉。
///
/// 采用「随当前安装包对齐」的策略（等价于构建期二选一的 require_signed_release /
/// require_signed_dev 开关，只是改成运行时判定）：
///   - 新包签名可验证 → 通过；
///   - 新包签名不可验证，且**当前已安装的包同样不可验证**（未签名分发）→ 告警放行，
///     安全性仍由 Release 的 SHA256SUMS 兜底；
///   - 新包签名不可验证，但当前包是已签名版 → 拒绝，防止用未签名包覆盖可信版本。
#[cfg(target_os = "macos")]
fn verify_codesign(app: &Path) -> Result<(), String> {
    if codesign_ok(app) {
        return Ok(());
    }
    let installed = PathBuf::from(darwin_install_target());
    if codesign_ok(&installed) {
        return Err(format!(
            "code signature verification failed for {} — refusing to overwrite a signed install",
            app.display()
        ));
    }
    crate::log::warn(
        "update: new bundle is not signed and the installed bundle is not signed either; \
         skipping codesign check (SHA256SUMS still enforced)",
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn codesign_ok(app: &Path) -> bool {
    if !app.exists() {
        return false;
    }
    Command::new("codesign")
        .args(["--verify", "--deep", "--strict", app.to_str().unwrap_or_default()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn darwin_install_target() -> String {
    if let Ok(exe) = std::env::current_exe() {
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        // .../NetSense.app/Contents/MacOS/netsense → 三级上是 bundle
        let app = exe
            .ancestors()
            .find(|p| p.extension().map(|e| e == "app").unwrap_or(false));
        if let Some(app) = app {
            return app.display().to_string();
        }
    }
    // 否则优先用户级 ~/Applications，再 /Applications
    if let Some(home) = std::env::var_os("HOME") {
        let u = Path::new(&home).join("Applications").join("NetSense.app");
        if u.is_dir() {
            return u.display().to_string();
        }
    }
    "/Applications/NetSense.app".to_string()
}

#[cfg(target_os = "macos")]
fn find_app_bundle(dir: &Path) -> PathBuf {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && p.extension().map(|x| x == "app").unwrap_or(false) {
                return p;
            }
        }
    }
    PathBuf::new()
}

#[cfg(target_os = "macos")]
fn darwin_install_script(new_app: &Path, target_app: &str) -> String {
    let q = shell_quote;
    let new = new_app.to_str().unwrap_or_default();
    let lines = [
        "#!/bin/sh".to_string(),
        "set -e".to_string(),
        "/usr/bin/killall netsense 2>/dev/null || true".to_string(),
        "/bin/sleep 1".to_string(),
        format!("/bin/rm -rf {}", q(target_app)),
        format!("/usr/bin/ditto {} {}", q(new), q(target_app)),
        // 下载来的包没有 quarantine（非浏览器下载），但剥除是幂等的，
        // 与 brew cask postflight 一致，防止新副本被 Gatekeeper 拦。
        format!("/usr/bin/xattr -dr com.apple.quarantine {} 2>/dev/null || true", q(target_app)),
        // 以登录用户（非 root）重新打开：脚本若被 osascript 提权，裸 open 会成 root。
        "OPEN_USER=$(/usr/bin/stat -f '%Su' /dev/console 2>/dev/null || /usr/bin/whoami)".to_string(),
        "if [ -n \"$OPEN_USER\" ] && [ \"$OPEN_USER\" != \"root\" ]; then".to_string(),
        format!(
            "  /bin/launchctl asuser \"$(/usr/bin/id -u \"$OPEN_USER\")\" /usr/bin/open {}",
            q(target_app)
        ),
        "else".to_string(),
        format!("/usr/bin/open {}", q(target_app)),
        "fi".to_string(),
    ];
    lines.join("\n") + "\n"
}

#[cfg(target_os = "macos")]
fn dir_writable(dir: &Path) -> bool {
    // 探针路径自己拼好再删：`File::path()` 是 unstable API，稳定版编译不过。
    let probe = dir.join(format!(".netsense-wt-{}.tmp", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(f) => {
            drop(f);
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "macos")]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// ———————————————————————————————— Windows ————————————————————————————————

#[cfg(target_os = "windows")]
fn install_windows(path: &Path) -> Result<(), String> {
    // 先把安装器拷到持久位置：调用方在 Install 返回后即删临时下载，而 runas 弹 UAC 时
    // 安装器尚未锁定文件，窗口期内删临时文件会让提权进程找不到文件。
    let staged = stage_installer(path)?;
    let ext = Path::new(&staged)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let mut args: Vec<String> = Vec::new();
    let mut exe = staged.to_string_lossy().into_owned();
    if &ext == "msi" {
        // msiexec 自身需管理员；/qn 静默
        exe = "msiexec".to_string();
        args.push("/i".into());
        args.push(staged.to_string_lossy().into_owned());
        args.push("/qn".into());
    } else {
        args.push("/S".into());
        args.push("/AUTOSTART".into());
        if let Some(dir) = current_install_dir() {
            args.push(format!("/D={dir}"));
        }
    }
    run_elevated(&exe, &args)
}

#[cfg(target_os = "windows")]
fn current_install_dir() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.display().to_string())
}

#[cfg(target_os = "windows")]
fn stage_installer(src: &Path) -> Result<PathBuf, String> {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| std::env::temp_dir().display().to_string());
    let dir = Path::new(&base).join("netsense").join("updates");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create updates dir: {e}"))?;
    // 清掉上次的残留安装器
    if let Ok(glob) = std::fs::read_dir(&dir) {
        for e in glob.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let dest = dir.join(format!("netsense-update-installer{}", Path::new(src).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default()));
    std::fs::copy(src, &dest).map_err(|e| format!("copy installer: {e}"))?;
    Ok(dest)
}

#[cfg(target_os = "windows")]
fn run_elevated(exe: &str, args: &[String]) -> Result<(), String> {
    // 用 PowerShell Start-Process -Verb RunAs 触发 UAC；参数逐个 -ArgumentList 传入。
    let mut ps_args: Vec<String> = vec!["Start-Process".into(), "-Verb".into(), "RunAs".into(), "-FilePath".into(),
        format!("'{}'", exe.replace('\'', "''"))];
    if !args.is_empty() {
        ps_args.push("-ArgumentList".into());
        let joined = args
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        ps_args.push(joined);
    }
    let ps = ps_args.join(" ");
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .output()
        .map_err(|e| format!("start installer (elevated): {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "elevated installer failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

// ———————————————————————————————— Linux ————————————————————————————————

#[cfg(target_os = "linux")]
fn install_linux(path: &Path) -> Result<(), String> {
    let staged = stage_linux_installer(path)?;
    let ext = Path::new(&staged)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "deb" => {
            if let Err(e) = run_pkexec("dpkg", &["-i", staged.to_str().unwrap_or_default()]) {
                return Err(e);
            }
            relaunch_linux_app()
        }
        "rpm" => {
            if let Err(e) = run_pkexec("rpm", &["-U", staged.to_str().unwrap_or_default()]) {
                return Err(e);
            }
            relaunch_linux_app()
        }
        other => Err(format!("unsupported update asset format .{other}: expected .deb or .rpm")),
    }
}

#[cfg(target_os = "linux")]
fn stage_linux_installer(src: &Path) -> Result<PathBuf, String> {
    let dir = if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(d).join("netsense").join("updates")
    } else if let Some(h) = std::env::var_os("HOME") {
        PathBuf::from(h).join(".local").join("share").join("netsense").join("updates")
    } else {
        return Err("cannot resolve a persistent updates directory".to_string());
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("create updates dir: {e}"))?;
    if let Ok(glob) = std::fs::read_dir(&dir) {
        for e in glob.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let ext = Path::new(src).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let dest = dir.join(format!("netsense-update-installer{ext}"));
    std::fs::copy(src, &dest).map_err(|e| format!("copy installer: {e}"))?;
    Ok(dest)
}

#[cfg(target_os = "linux")]
fn run_pkexec(prog: &str, args: &[&str]) -> Result<(), String> {
    let mut cmd = Command::new("pkexec");
    cmd.arg(prog);
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().map_err(|e| format!("{prog} via pkexec failed to start: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{prog} via pkexec failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn relaunch_linux_app() -> Result<(), String> {
    // 旧二进制仍被 dpkg/rpm 原地覆盖，但本进程跑的是旧版本，需重启才能用上新版。
    // 用独立 shell 脚本 detached 启动：脚本先 pkill 本进程（含自身），再 nohup 起新二进制。
    let bin = "/usr/local/bin/netsense";
    let resolved = if Path::new(bin).exists() {
        bin.to_string()
    } else {
        match Command::new("which").arg("netsense").output() {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            _ => return Ok(()), // 装好了但找不到二进制：不失败，用户可手动启动
        }
    };
    let script = format!(
        "#!/bin/sh\npkill -x netsense 2>/dev/null || true\n/bin/sleep 1\nnohup {} >/dev/null 2>&1 &\n",
        shell_quote(&resolved)
    );
    let p = write_private_script("relaunch", &script)?;
    let cmd = Command::new("/bin/sh").arg(&p).spawn();
    let _ = std::fs::remove_file(&p);
    match cmd {
        Ok(mut c) => {
            let _ = c.wait();
            Ok(())
        }
        Err(e) => Err(format!("relaunch failed: {e}")),
    }
}

#[cfg(target_os = "linux")]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ———————————————————————————————— Homebrew 渠道 ————————————————————————————————

/// 是否通过 Homebrew 安装（cask 把 app 拷到 /Applications，但 Caskroom 收据目录是证据）。
///
/// ⚠️ 不能用 `current_exe` 路径判断：cask 安装后 app 落在 `/Applications/NetSense.app`，
/// 与 `/opt/homebrew` 无关，路径法必然误判为「非 brew」。必须看 Caskroom 收据。
/// 本函数同时被 `ipc::check_update` 复用，保证前端显示的渠道与 `run_update` 的实际路由一致。
pub(crate) fn is_brew_install() -> bool {
    for base in [
        "/opt/homebrew/Caskroom/netsense",
        "/usr/local/Caskroom/netsense",
        "/home/linuxbrew/.linuxbrew/Caskroom/netsense",
    ] {
        if Path::new(base).is_dir() {
            return true;
        }
    }
    false
}

fn brew_path() -> Option<String> {
    for p in [
        "/opt/homebrew/bin/brew",
        "/usr/local/bin/brew",
        "/home/linuxbrew/.linuxbrew/bin/brew",
    ] {
        if Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    None
}

fn run_brew(app: &AppHandle, t: &UpdateTarget) -> Result<(), String> {
    // 「brew 退出 0 但没真装上」的落地检查只有 macOS 做得出（读 .app 的 Info.plist），
    // 所以其余平台上 `t` 到这里就没用了。
    #[cfg(not(target_os = "macos"))]
    let _ = t;
    let brew = brew_path().ok_or("未找到 brew：Homebrew 安装请先安装 Homebrew")?;

    // `brew update` 纯网络，90s 上限；卡住就说明 DNS/API 出问题，宁可超时报错。
    emit_progress(app, "refresh", 0);
    let upd = Command::new(&brew)
        .arg("update")
        .output_timeout(90);
    if let Err(e) = upd {
        crate::log::warn(&format!("brew update failed, continuing: {e}"));
    }

    emit_progress(app, "install", 0);
    let run_upgrade = || -> Result<(), String> {
        let out = Command::new(&brew)
            .args(["upgrade", "--cask", "--greedy", "netsense"])
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .output_timeout(300)
            .map_err(|e| format!("brew upgrade failed to start: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    };

    match run_upgrade() {
        Ok(()) => {}
        Err(e) if e.contains("untrusted tap") => {
            // Homebrew 6 对第三方 tap 要求显式信任；用户已从此 tap 安装并点了升级，信任即授权。
            crate::log::info("update: tap untrusted — trusting imonior/tap and retrying");
            let trust = Command::new(&brew).args(["trust", "imonior/tap"]).output_timeout(30);
            if let Err(trust_err) = trust {
                return Err(format!("brew trust imonior/tap failed: {trust_err}"));
            }
            run_upgrade()?;
        }
        Err(e) => return Err(format!("brew upgrade failed: {e}")),
    }

    // brew 退出 0 但 cask postflight 没杀掉本进程（即没真装）：严格校验磁盘上的版本。
    #[cfg(target_os = "macos")]
    if let Some(installed) = installed_bundle_version() {
        if installed != t.version {
            return Err(format!(
                "brew 退出 0 但 /Applications/NetSense.app 仍是 {installed}（期望 {}）",
                t.version
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn installed_bundle_version() -> Option<String> {
    let p = "/Applications/NetSense.app/Contents/Info.plist";
    let out = Command::new("defaults")
        .args(["read", p, "CFBundleShortVersionString"])
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    } else {
        None
    }
}

/// 给 `Command` 包一层带超时的 output（标准库无该 API）。
trait OutputTimeout {
    fn output_timeout(&mut self, secs: u64) -> std::io::Result<std::process::Output>;
}

impl OutputTimeout for Command {
    fn output_timeout(&mut self, secs: u64) -> std::io::Result<std::process::Output> {
        // 与 `Command::output` 一致：显式管道化 stdout/stderr，否则 wait_with_output 拿不到内容。
        self.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = self.spawn()?;
        let pid = child.id();
        // 用独立线程等待 + 超时杀进程，避免阻塞 UI（Tauri 命令在 async worker 上，本就不堵主线程，
        // 但超时杀进程能保证 brew 卡死时升级不会永久挂起）。
        // ⚠️ 必须用 `wait_with_output`（返回 Output 并读取管道）而不是 `wait`（只返回 ExitStatus）——
        // 调用方要读 `out.stderr` 才能把 brew 的失败原因带回前端。
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
            Ok(r) => r,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let _ = kill_pid(pid);
                // 给一点时间让子进程退出
                let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("command timed out after {secs}s"),
                ))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::other(
                "wait thread disconnected before the command finished",
            )),
        }
    }
}

#[cfg(windows)]
fn kill_pid(pid: u32) -> std::io::Result<()> {
    Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output().map(|_| ())
}

#[cfg(not(windows))]
fn kill_pid(pid: u32) -> std::io::Result<()> {
    // 向进程组发 SIGKILL；这里简单向 pid 发 SIGKILL。
    unsafe {
        libc_kill(pid as i32, 9);
    }
    Ok(())
}

#[cfg(not(windows))]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, sig)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_urls_are_accepted() {
        assert!(require_https("https://github.com/owner/repo/releases/download/v1/x.dmg").is_ok());
        assert!(require_https("HTTPS://Example.INVALID/a").is_ok()); // scheme 大小写不敏感
        for bad in ["http://example.invalid/a", "file:///etc/passwd", "", "https:/x", "ttps://a"] {
            assert!(require_https(bad).is_err(), "should reject {bad:?}");
        }
        // 非 ASCII 前缀必须是 Err，而不是切片 panic。
        assert!(require_https("～https://a").is_err());
    }

    #[test]
    fn candidate_names_carry_entropy_and_never_repeat() {
        let a = temp_candidate("t", ".sh");
        let b = temp_candidate("t", ".sh");
        assert_ne!(a, b);
        let stem = format!("netsense-t-{}-", std::process::id());
        for p in [&a, &b] {
            let n = p.file_name().unwrap().to_string_lossy().to_string();
            assert!(n.starts_with(&stem), "{n} should start with {stem}");
            // 只有 pid 的名字旁人枚举得出来：熵段必须有实际长度。
            assert!(n.len() > stem.len() + 12, "{n} looks like it lost its entropy");
        }
    }

    #[test]
    fn a_pre_occupied_name_fails_instead_of_reusing_the_owner_inode() {
        // CWE-377 的核心：`File::create` 会落在已存在的 inode 上（属主仍是对方的），
        // `create_new` 必须报错。
        let p = temp_candidate("t", ".taken");
        std::fs::File::create(&p).unwrap();
        let e = open_private_new(&p).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        let _ = std::fs::remove_file(&p);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_of_the_same_name_is_not_followed() {
        use std::os::unix::fs::symlink;
        let victim = temp_candidate("t", ".victim");
        std::fs::File::create(&victim).unwrap();
        let link = temp_candidate("t", ".link");
        symlink(&victim, &link).unwrap();
        // O_EXCL 不跟随符号链接：对方摆好的链接不能变成我们的写入目标。
        assert!(open_private_new(&link).is_err());
        assert_eq!(std::fs::metadata(&victim).unwrap().len(), 0);
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&victim);
    }

    #[cfg(unix)]
    #[test]
    fn private_files_and_scripts_are_ours_and_unreachable_by_others() {
        let f = new_private_file("t", ".txt").unwrap();
        verify_private(&f).unwrap();
        let s = write_private_script("t", "#!/bin/sh\necho ok\n").unwrap();
        verify_private(&s).unwrap();
        assert_eq!(std::fs::read_to_string(&s).unwrap(), "#!/bin/sh\necho ok\n");
        let _ = std::fs::remove_file(&f);
        let _ = std::fs::remove_file(&s);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn private_temp_dirs_are_exclusive_and_0700() {
        let d = new_private_dir("t").unwrap();
        verify_private(&d).unwrap();
        assert!(d.is_dir());
        // 第二次调用必须另起一名：压缩包不该解进同一个别人看得见的位置。
        let d2 = new_private_dir("t").unwrap();
        assert_ne!(d, d2);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&d2);
    }

    #[test]
    fn sha256sums_parsing_covers_gnu_bsd_and_binary_modes() {
        let h = "a3b1c2d4e5f60718293a4b5c6d7e8f90123456789012345678901234567890ab";
        let body = concat!(
            "# comment\n",
            "\n",
            "deadbeef  other.dmg\n",
            "a3b1c2d4e5f60718293a4b5c6d7e8f90123456789012345678901234567890ab  NetSense_1.0.0_aarch64.dmg\n",
            "a3b1c2d4e5f60718293a4b5c6d7e8f90123456789012345678901234567890ab *binary-mode.txt\n",
            "SHA256 (bsd.txt) = a3b1c2d4e5f60718293a4b5c6d7e8f90123456789012345678901234567890ab\n",
        );
        assert_eq!(parse_hash(body, "NetSense_1.0.0_aarch64.dmg").as_deref(), Some(h));
        assert_eq!(parse_hash(body, "binary-mode.txt").as_deref(), Some(h));
        assert_eq!(parse_hash(body, "bsd.txt").as_deref(), Some(h));
        // 未列出必须返回 None —— fail-closed 的判定就靠它。
        assert_eq!(parse_hash(body, "missing.dmg"), None);
        assert_eq!(parse_hash("not-hex  x.txt\n", "x.txt"), None);
    }
}
