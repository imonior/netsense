//! 在线升级：检查 →（Homebrew 渠道）`brew upgrade --cask` /（独立安装）下载 + SHA256
//! 校验 + 平台安装器（提权）→ 重启。
//!
//! 交互模型：默认动作就是「下载并调用安装流程」，而不是只给一条命令或链接。下发过程中
//! 通过 `netsense://update_progress` 事件广播阶段与进度，前端据此显示进度。
//! 任一环节失败都回退到「打开 Release 页」这条永远可用的手动路径，不会把用户卡死。
//!
//! 安全：这是一条**两层信任链**（精简版 B+），不是「直接比对哈希」：
//!
//! 1. 二进制里只内嵌一把 **root 公钥**（`ROOT_PUBKEY_HEX`，编译期注入）。它是指南针，
//!    唯一的信任锚，镜像和 Release 资产都换不掉它。
//! 2. 信任清单 `UPDATE-TRUST.json` 由 **root 私钥离线签发**，App 每次更新都用内置 root
//!    公钥验它，从里面读出「当前有效的 signing 公钥」。
//! 3. `SHA256SUMS` 由 **signing 私钥签名**（CI 持有，可轮换），App 用上一步取到的
//!    signing 公钥验 `SHA256SUMS.sig`，再比哈希。
//!
//! 由此得到两个原方案拿不到的性质：
//! - **镜像全断仍能更新**：清单、签名、校验和全都走镜像，而「什么算合法」只由内置公钥决定。
//! - **signing 私钥泄露可轮换**：用 root 私钥签发一份指向新 signing 公钥的新清单即可，
//!   已发布的二进制下次更新时自动改信新密钥，**不必重新分发二进制、用户不必手动重装**。
//!
//! 一道防回滚（裸 Ed25519 只验「签名对不对」，不验「新不新」）：
//! - 清单 `seq` 单调不减（记录在本地状态文件，拒绝旧清单覆盖新清单）。
//!   它**不是**可选的附加功能，而是轮换能成立的**前提**：没有它，攻击者把轮换之前那份
//!   同样由 root 合法签过的旧清单反复投喂，被换掉的旧 signing 密钥就重新变得可信。
//!
//! 刻意**不做**的两件事 —— 收益小、却各自多一处必须落盘且跨版本保持一致的状态，
//! 那一处坏了就是把正常更新卡死（完整取舍见 DEVELOPMENT.md §13.3）：
//! - 不做「已撤销密钥列表 `revoked[]`」：有了 `seq`，轮换本身已让旧密钥失效，撤销表与它
//!   高度重叠，却要为每个已发布清单都维护一份列表。
//! - 不做「最低版本 / 已装版本水位」：它挡的场景（镜像投喂一个更旧但仍合法签名的安装包）
//!   概率低，而版本号格式一旦不一致（带前缀、带预发布后缀）就会误拒正常更新。
//!
//! 任一环节拿不到可信结果就**中止安装**，不会退化成「只比哈希」——那正是恶意镜像唯一能利用的缝。
//! 无 root 公钥的 dev 构建是唯一的例外，且仅在未置 `require_signed()`（NETSENSE_RELEASE_SIGN）时降级为仅 SHA256。
//! CI 在四条平台腿跑完后统一上传根级 `SHA256SUMS` 与 `SHA256SUMS.sig`（见 build.yml），
//! 正常发布永远读得到它们；读不到说明手上的东西不是我们发出去的那份。
//! 下载与落地全程使用**独占创建**的私有临时路径（见「私有临时路径」一节）：共享 `/tmp` 上
//! 被别人抢先占用的名字，后果是提权执行的脚本内容可在我们写入之后再被改一次。
//! macOS 还会额外校验 app 的 codesign：
//! 新包可验证即放行；不可验证时，只有当**当前安装包同样未签名**（无 Apple 证书的
//! 分发形态）才告警放行，否则拒绝覆盖已签名版本。详见 `verify_codesign`。

use crate::i18n;
use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use tauri::{AppHandle, Emitter};

// ——— 发布签名：两层信任链（精简版 B+）———
//
// 内置的**只有 root 公钥**：它是唯一的信任锚，由 CI 经环境变量注入（见 build.rs）。
// 空串表示本构建未嵌入 root 公钥（dev / 本地），此时按 require_signed() 决定是否中止。
//
// 信任清单 UPDATE-TRUST.json 由 root 私钥离线签发；App 验过清单后，才从里面取出
// signing 公钥去验 SHA256SUMS.sig。因此 signing 私钥可以放心放进 CI（泄露时用 root
// 签一份指向新密钥的新清单即可轮换），而**真正的信任根（root 私钥）永远不接触任何
// 自动化环节**。
// `option_env!().unwrap_or()` 与 `Some("1")` 字面量匹配在 const 上下文里都还是不稳定特性
// （E0658），稳定版只能用「绑定式 match」取值；「是否要求签名」不是编译期常量能表达的了，
// 退成运行时函数——CI 注入环境变量在编译期就定了，运行时读的仍是同一个编译期字面量。
const ROOT_PUBKEY_HEX: &str = match option_env!("NETSENSE_ROOT_PUBKEY") {
    Some(s) => s,
    None => "",
};
// 「无签名即拒绝」：仅当 CI 置位 NETSENSE_RELEASE_SIGN 时为真（release 构建）。
fn require_signed() -> bool {
    option_env!("NETSENSE_RELEASE_SIGN") == Some("1")
}

/// 信任清单的固定位置：仓库树里的 `update/UPDATE-TRUST.json`。
///
/// 它由 root 私钥离线签发后**提交进仓库**，而不是当 Release 资产上传——因为签它需要 root
/// 私钥，而 CI 按设计就拿不到 root 私钥。清单内容不随每个 Release 变，只在密钥轮换 / 撤销时变。
const TRUST_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/imonior/netsense/main/update/UPDATE-TRUST.json";

/// 让子进程**不分配控制台窗口**。
///
/// 本进程是 GUI 子系统（`windows_subsystem = "windows"`，自身没有控制台）。这种进程去拉
/// `curl` / `powershell` / `taskkill` 这类**控制台子系统**的子进程时，Windows 会给它现开
/// 一个可见控制台 —— 表现为升级时屏幕上闪出一个黑窗（curl 的那条甚至什么都不显示，因为
/// 我们只把 stderr 接走了）。`CREATE_NO_WINDOW` 让子进程无窗口运行，管道照样能读。
///
/// `platform::run` 里早就有同一行（那里是 `.output()` 的路径），而 `update.rs` 这条下载链
/// 走的是 `spawn()` 轮询进度，绕开了 `run`，于是漏掉了它。其余平台没有控制台这个概念，
/// 退化成原样返回。
#[cfg(windows)]
fn hide_console(c: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    c.creation_flags(0x08000000); // CREATE_NO_WINDOW
    c
}

#[cfg(not(windows))]
fn hide_console(c: &mut Command) -> &mut Command {
    c
}

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
///
/// `choice` 是软件设置里选的网络出口，它由调用方（`ipc::run_update`）与 `check_update`
/// **同一个来源**取出：检查更新用什么出口，下载与取校验值就用什么出口。Homebrew 渠道不带
/// 它 —— brew 有自己的代理配置，本应用不该越过那条配置。
///
/// `mirror` 是软件设置里选的下载镜像（见 [`crate::appconfig::UpdateMirror`]），与 `choice`
/// 正交：一个换地址、一个换出口。带签名的 `SHA256SUMS`/`.sig` 也经镜像取回，但签名锚在
/// 内置公钥上 —— 镜像换不掉校验值，反而能让「GitHub 被墙」时更新仍可完成。
pub fn run_update(
    app: &AppHandle,
    t: &UpdateTarget,
    choice: &crate::netproxy::ProxyChoice,
    mirror: &crate::appconfig::UpdateMirror,
) -> Result<(), String> {
    if is_brew_install() {
        return run_brew(app, t);
    }
    native_update(app, t, choice, mirror)
}

fn native_update(
    app: &AppHandle,
    t: &UpdateTarget,
    choice: &crate::netproxy::ProxyChoice,
    mirror: &crate::appconfig::UpdateMirror,
) -> Result<(), String> {
    // `UpdateTarget` 是前端回传的，按外部输入对待：scheme 只认 https，且必须落在
    // 本项目自己的 Release 锚点上 —— 否则「自己托管二进制 + 自己给 SHA256SUMS」
    // 能让校验一步都不落地（见 require_release_asset）。
    require_release_asset(&t.download_url)?;
    let cu = t
        .checksum_url
        .as_ref()
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| i18n::t("upd.no_sums"))?;
    require_release_asset(cu)?;

    // 镜像只换「去哪儿取字节」。来源校验已在上面按 GitHub 原始 URL 判完。
    // 现在二进制、SHA256SUMS、`.sig` 都经镜像取回 —— 因为 SHA256SUMS 带 Ed25519 签名，
    // 信任锚在内置公钥上，镜像换不掉（伪造不出 `.sig`），所以镜像也能服务于校验这一步。
    let dl = mirrored_url(&t.download_url, mirror);
    if dl != t.download_url {
        crate::log::info(&i18n::tf("upd.mirror_used", &[("mirror", &dl)]));
    }

    emit_progress(app, "download", 0);
    let tmp = download_file(app, &dl, &t.asset_name, t.size, choice)?;

    // SHA256SUMS 与配套的 `.sig`：都走镜像，与二进制同一条路。
    let sums_url = mirrored_url(cu, mirror);
    // 取原始字节（不是 `fetch_url` 的 lossy 文本）：SHA256SUMS 虽是文本，但它的**签名**
    // 是对文件字节逐字节验的；若经 lossy UTF-8 解码无影响（本就是 ASCII），但 `.sig` 是二进制，
    // 必须走 `fetch_bytes` 原样取回，否则非 UTF-8 字节被替换成 U+FFFD，签名必败。
    let sums_bytes = crate::ipc::fetch_bytes(&sums_url, choice)
        .map_err(|e| i18n::tf("upd.sums_fetch", &[("error", &e)]))?;
    // 镜像对 `text/*` 做 CRLF/行尾归一会让逐字节验签直接 INVALID（与裸 openssl 验文本清单同源）。
    // 这里把 `\r` 归一为 `\n` 再验签：CI 生成的 SHA256SUMS 本就是 LF，归一对其是恒等变换，
    // 直连 GitHub 照过；镜像弄花行尾也能还原回签名时的字节。不删尾换行（见 `normalize_crlf`）。
    let sums_canon = normalize_crlf(&sums_bytes);

    // 信任链门（root → signing 两层）：有内置 root 公钥就必须整条走通，否则拒绝安装。
    // 先取信任清单定出「该信哪把 signing 密钥」，再用那把密钥验 SHA256SUMS 的签名 ——
    // 顺序不能反：清单验不过时我们还不知道该信谁，拿内置的 signing 公钥去验就成了单密钥
    // 模型，轮换能力也就没了。
    //
    // 无 root 公钥且本构建要求签名 → 拒绝；无 root 公钥且非发布构建 → 仅 SHA256 并告警（dev）。
    let trust = match embedded_root_pubkey() {
        Some(_) => Some(verify_trust_chain(choice, mirror)?),
        None => {
            if require_signed() {
                return Err(i18n::t("upd.sig_no_key"));
            }
            crate::log::warn(&i18n::t("upd.sig_skipped"));
            None
        }
    };
    if let Some(chain) = &trust {
        let sig_url = mirrored_url(&append_sig_suffix(cu), mirror);
        // `.sig` 是 64 字节裸二进制：必须走 `fetch_bytes` 原样取回，绝不能经 lossy UTF-8 解码。
        let sig = crate::ipc::fetch_bytes(&sig_url, choice)
            .map_err(|e| i18n::tf("upd.sums_fetch", &[("error", &e)]))?;
        // 验签针对**规范化后**的 SHA256SUMS 字节（见 `normalize_crlf`）：直连 GitHub 时规范化为
        // 恒等，镜像改写行尾也能还原，签名照过。
        match verify_ed25519(&sums_canon, &sig, &chain.signing_key) {
            Ok(()) => crate::log::info(&i18n::t("upd.sig_ok")),
            Err("bad") => return Err(i18n::t("upd.sig_bad")),
            Err(_) => return Err(i18n::t("upd.sig_failed")),
        }
    }

    // 校验 SHA256：任何一环拿不到可信哈希都中止，不「告警后继续」。解析用规范化后的文本，
    // `parse_hash` 本身对 CRLF 也稳健，所以镜像弄花行尾不影响哈希抽取。
    let sums_text = String::from_utf8_lossy(&sums_canon);
    let expected = parse_hash(&sums_text, &t.asset_name)
        .ok_or_else(|| i18n::tf("upd.not_in_sums", &[("asset", &t.asset_name)]))?;
    let actual = sha256_file(&tmp)?;
    if !eq_ignore_case(&actual, &expected) {
        let _ = std::fs::remove_file(&tmp);
        return Err(i18n::tf(
            "upd.checksum_mismatch",
            &[
                ("asset", &t.asset_name),
                ("want", &expected),
                ("got", &actual),
            ],
        ));
    }
    crate::log::info(&i18n::tf("upd.sha256_ok", &[("asset", &t.asset_name)]));

    // 走到这里签名与哈希都已确认，才允许推进 `seq` 水位。中途任何一步失败都不会记，
    // 所以一次失败的更新不会把水位提前抬高、把合法的旧清单挡在门外。
    if let Some(chain) = &trust {
        commit_trust_state(chain.seq);
    }
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
    choice: &crate::netproxy::ProxyChoice,
) -> Result<PathBuf, String> {
    let dest = new_private_file("update", &format!("-{}", sanitize(name)))?;

    // 用 spawn 而非 output：curl 下载大包可能持续数十秒，期间轮询临时文件大小，
    // 把「已下载/总大小」换算成百分比推给前端（GitHub 资产带 size，故总大小已知）。
    //
    // `--proto =https` 让 `-L` 的跳转目标也必须是 https（GitHub 的资产下载正是 302）。这里
    // **故意不加** `--proto-redir`：它要 curl 7.65.2，而 Windows 10 1803 —— 也就是 `main.rs`
    // 承诺的下限 —— 自带 curl 7.60.1，认不出的选项会让 curl 直接退出，把「能更新」变成「下载失败」，
    // 换来的是 `--proto` 已经覆盖不到的零额外约束。
    let mut cmd = Command::new("curl");
    hide_console(&mut cmd);
    let mut child = cmd
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
        .args(crate::netproxy::curl_proxy_args(choice))
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| i18n::tf("upd.curl_start", &[("error", &e.to_string())]))?;

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
                return Err(i18n::tf("upd.curl_wait", &[("error", &e.to_string())]));
            }
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| i18n::tf("upd.curl_wait", &[("error", &e.to_string())]))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&dest);
        return Err(i18n::tf(
            "upd.download_failed",
            &[("error", String::from_utf8_lossy(&out.stderr).trim())],
        ));
    }

    let meta = std::fs::metadata(&dest)
        .map_err(|e| i18n::tf("upd.stat_temp", &[("error", &e.to_string())]))?;
    if let Some(sz) = expect_size {
        if meta.len() != sz {
            let _ = std::fs::remove_file(&dest);
            return Err(i18n::tf(
                "upd.size_mismatch",
                &[("want", &sz.to_string()), ("got", &meta.len().to_string())],
            ));
        }
    } else if meta.len() < MIN_ASSET {
        let _ = std::fs::remove_file(&dest);
        return Err(i18n::tf(
            "upd.too_small",
            &[("bytes", &meta.len().to_string())],
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
        Err(i18n::tf("upd.non_https", &[("url", url)]))
    }
}

/// 自更新的信任锚：升级资产必须来自**本项目自己的** GitHub Release 目录。
///
/// [`UpdateTarget`] 整个由前端回传，而校验值又来自同一个 `checksum_url` ——
/// 两者是同一条输入。只查 scheme 等于「攻击者自己托管一个 https 站点，同时给出
/// 二进制与配套的 SHA256SUMS」，`parse_hash` 逐字节对上，校验步骤形同虚设。
/// 真正的信任只能来自「这个 URL 是不是我们自己仓库的 Release 资产」，
/// 那需要攻陷本仓库的发布权限才能伪造。
///
/// 锚点取 `https://github.com/<owner>/<repo>/releases/download/`：GitHub 的
/// `browser_download_url` 一律是这个形状（`check_update` 读的就是它）。
/// 资产字节经 302 落到 `objects.githubusercontent.com`，但那是 `curl -L` 自己
/// 跟过去的落点、我们既不提供也不校验它，因此**不**把它列进可接受的输入 host ——
/// 放行它等于放行任意仓库的任意资产。
///
/// 比较用 `eq_ignore_ascii_case`：scheme 与 host 大小写不敏感，GitHub 的
/// owner / repo 也是。资产名（`<tag>/<asset>`）只要求非空、不含 `..` 段。
fn release_asset_pinned(url: &str) -> bool {
    const ANCHOR: &str = "https://github.com/imonior/netsense/releases/download/";
    let u = url.trim();
    let Some(rest) = u.get(..ANCHOR.len()) else {
        return false;
    };
    if !rest.eq_ignore_ascii_case(ANCHOR) {
        return false;
    }
    let asset = &u[ANCHOR.len()..];
    // 非空、无前导 `/`（tag 段不能为空）、无路径回退、无空段、无空白 ——
    // 剩下的交给 GitHub 自己判。
    !asset.is_empty()
        && !asset.starts_with('/')
        && !asset.contains("..")
        && !asset.contains("//")
        && !asset.contains(char::is_whitespace)
}

/// 升级来源的完整准入：https + 落在本项目 Release 锚点内。
///
/// 比 [`require_https`] 严一档，**这一档才是 `run_update` 该用的**：前者只回答
/// 「加密通道有没有」，后者回答「对面是不是我们自己」。
pub(crate) fn require_release_asset(url: &str) -> Result<(), String> {
    require_https(url)?;
    if release_asset_pinned(url) {
        Ok(())
    } else {
        Err(i18n::tf("upd.bad_origin", &[("url", url)]))
    }
}

/// 把一条**已经验过来源**的 URL 改写成镜像地址（前缀式：`https://host/` + 原 URL）。
///
/// 只在 `require_release_asset` 之后调用：这个函数不参与任何来源判定，它换的只是走哪条路，
/// 而「这个包是什么」由直连 GitHub 取来的 SHA256SUMS 说了算。
///
/// 前缀不合法时**原样返回**（即退回直连），而不是报错：走到这里时软件设置已经被
/// `AppConfig::clamped` 收过一次，这里再挡一层是因为这条路径同样吃外部输入，而一个拼不出
/// URL 的前缀只该让下载失败一次，不该让「检查更新」整条路走不下去。
pub(crate) fn mirrored_url(original: &str, mirror: &crate::appconfig::UpdateMirror) -> String {
    use crate::appconfig::UpdateMirror;
    match mirror {
        UpdateMirror::Direct => original.to_string(),
        UpdateMirror::Prefix(p) => {
            if !crate::appconfig::is_mirror_prefix(p) {
                return original.to_string();
            }
            // 末尾斜杠各家写法不一（预设有的带、有的不带），拼出来只能有一个 `/`：
            // 多出来的那个会被部分镜像当成一层空 host 段，请求直接 404。
            format!("{}/{}", p.trim().trim_end_matches('/'), original)
        }
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
            Err(e) => return Err(i18n::tf("upd.temp_file_create", &[("error", &e.to_string())])),
        }
    }
    Err(i18n::t("upd.temp_file_name"))
}

/// 确认路径确实归当前用户所有，且组/其他位没有任何权限。
///
/// 独占创建已经保证了这两点，这里查的是**换目录**那一手：`$TMPDIR` 若指向一个他人可替换
/// 的目录，我们的写入发生在旧目录，而提权后按路径重新解析的 `/bin/sh` 会落到对方摆好的
/// 同名文件上。名字里有纳秒时戳，对方要猜中才能摆，但这一检查是常数成本，不赌。
#[cfg(unix)]
fn verify_private(p: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let md = p
        .metadata()
        .map_err(|e| i18n::tf("upd.stat_path", &[("path", &p.display().to_string()), ("error", &e.to_string())]))?;
    if md.uid() != read_uid() {
        return Err(i18n::tf("upd.owned_by_other", &[("path", &p.display().to_string())]));
    }
    if md.mode() & 0o077 != 0 {
        return Err(i18n::tf(
            "upd.world_accessible",
            &[("path", &p.display().to_string())],
        ));
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
///
/// `pub(crate)`：macOS 的免密通道安装也走这条路 —— 要交给 root 执行的临时脚本，
/// 独占创建与 `verify_private` 这两条规则没有「只有升级用得上」的道理。
#[cfg(unix)]
pub(crate) fn write_private_script(prefix: &str, content: &str) -> Result<PathBuf, String> {
    let p = new_private_file(prefix, ".sh")?;
    std::fs::write(&p, content).map_err(|e| {
        let _ = std::fs::remove_file(&p);
        i18n::tf("upd.script_write", &[("prefix", prefix), ("error", &e.to_string())])
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
            Err(e) => {
                return Err(i18n::tf("upd.temp_dir_create", &[("error", &e.to_string())]))
            }
        }
    }
    Err(i18n::t("upd.temp_dir_name"))
}

// ———————————————————————————————— 校验 ————————————————————————————————

fn sha256_file(p: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(p)
        .map_err(|e| i18n::tf("upd.sha256_open", &[("error", &e.to_string())]))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| i18n::tf("upd.sha256_read", &[("error", &e.to_string())]))?;
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
        // GNU 风格：hash 之后第一个空白分隔文件名。
        // 认不出分隔符的行要**跳过**，不是中止：一份清单里只要有一行没有空白（一句表头、
        // 一段被别的编码换坏的内容），整份清单就都读不到了 —— 而这条路径的失败说法是
        // 「这个资产没在 SHA256SUMS 里」，把「我没读完」报成了「它没列」。
        let Some(idx) = line.find([' ', '\t']) else {
            continue;
        };
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

/// 对 SHA256SUMS 的字节做**最小**规范化：仅把 `\r\n` 与孤立 `\r` 归一为 `\n`。
///
/// 目的：镜像若对 `text/*` 做行尾归一（CRLF），会让「对原始文件字节逐字节验签」直接
/// `INVALID` —— 这正是裸 `openssl dgst -verify` 验文本清单会栽的那一类。SHA256SUMS 在 CI 里
/// 本就按 LF 生成，这里归一对它是恒等变换：直连 GitHub（逐字节）照过；镜像弄花行尾也能还原回
/// 签名时的字节，验证照过。
///
/// 故意**不**删尾换行 / 不 `trim`：CI 生成的清单本就以 LF 结尾，删了反而令「规范化后字节」与
/// 「签名时的原始字节」不一致，验证必败。只动 `\r`，不动别的。
fn normalize_crlf(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\r' {
            out.push(b'\n');
            if i + 1 < b.len() && b[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

// ———————————————————————————————— Ed25519 验签 ————————————————————————————————

/// 把十六进制串解码进定长缓冲区（校验长度与字符）。
fn hex_decode(s: &str, out: &mut [u8]) -> Result<(), String> {
    if s.len() != out.len() * 2 {
        return Err("bad hex length".into());
    }
    let b = s.as_bytes();
    for i in 0..out.len() {
        let hi = hex_nibble(b[2 * i])?;
        let lo = hex_nibble(b[2 * i + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(())
}

fn hex_nibble(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err("bad hex char".into()),
    }
}

/// 本构建内置的 **root** 公钥（32 字节）；未嵌入时返回 None（dev / 本地构建）。
fn embedded_root_pubkey() -> Option<[u8; 32]> {
    if ROOT_PUBKEY_HEX.len() != 64 {
        return None;
    }
    let mut b = [0u8; 32];
    if hex_decode(ROOT_PUBKEY_HEX, &mut b).is_err() {
        return None;
    }
    Some(b)
}

/// 在 URL 路径段后追加 `.sig`，保留 `?query` / `#fragment`。
/// 与 wireguide-plus 的 `appendSigSuffix` 同语义。
fn append_sig_suffix(raw: &str) -> String {
    let cut = raw.find(['?', '#']).unwrap_or(raw.len());
    let (base, tail) = raw.split_at(cut);
    format!("{base}.sig{tail}")
}

/// 用内置公钥校验 Ed25519 签名。
/// `Ok(())` 通过；`Err("bad")` 公钥/签名格式非法；`Err("mismatch")` 验签不通过。
///
/// 用 `verify_strict` 而不是 `verify`：后者允许**弱公钥**（低阶点），而弱公钥能为几乎任意
/// 消息生成有效签名。对 root 公钥而言这不是威胁（它编译期内嵌，出自我们自己的 OsRng，
/// 攻击者换不掉），但对**信任清单里的 signing_key** 就是现实风险 —— root 私钥一旦泄露，
/// 攻击者可以签一份「signing_key 取某个低阶点」的清单，让它对任意内容验签通过。
/// `verify_strict` 多做的群元素检查正好堵掉这一条，且单签名场景零性能代价。
fn verify_ed25519(content: &[u8], sig: &[u8], pubkey: &[u8; 32]) -> Result<(), &'static str> {
    if sig.len() != 64 {
        return Err("bad");
    }
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(sig);
    let signature = ed25519_dalek::Signature::from_bytes(&sig_arr);
    let vk = ed25519_dalek::VerifyingKey::from_bytes(pubkey).map_err(|_| "bad")?;
    vk.verify_strict(content, &signature).map_err(|_| "mismatch")
}

// ———————————————————————— 信任链：root 签清单 → signing 签校验和 ————————————————————————

/// 信任清单 `UPDATE-TRUST.json`。
///
/// ⚠️ 字段名与顺序必须与 `tools/updatesign` 里的 `TrustBody` **完全一致**：被签名的对象是
/// 「除 `signature` 外所有字段按声明顺序紧凑序列化」的字节，两端必须算出同一串字节。改名字、
/// 调顺序、加字段都会让所有已发布清单验签失败 —— 这是本文件里最不能随手改的一处。
#[derive(serde::Deserialize)]
struct TrustManifest {
    #[serde(default)]
    schema: u64,
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    signing_key: String,
    #[serde(default)]
    issued_at: String,
    /// 缺失即视为非法：解析会失败，而不是给一个「没签过」的默认值。
    signature: String,
}

/// 信任清单的被签名主体。字段顺序即规范，与工具侧一一对应。
#[derive(serde::Serialize)]
struct TrustBody<'a> {
    schema: u64,
    seq: u64,
    signing_key: &'a str,
    issued_at: &'a str,
}

/// 走完信任链后得到的东西：本次该用的 signing 公钥，以及清单的 `seq` 水位。
struct TrustChain {
    signing_key: [u8; 32],
    seq: u64,
}

/// 本地记住的信任水位：见过的最大清单 `seq`。
///
/// 只记 `seq` 一个数字，是因为它承载的是**轮换能不能生效**：没有它，攻击者可以把一份
/// 曾经合法（root 确实签过）的旧清单反复投喂，让已经轮换掉的旧 signing 密钥重新可信，
/// 而签名验得完全通过 —— 裸 Ed25519 只证明「是官方签的」，不证明「是最新的」。
///
/// 刻意**不记**已安装版本：那挡的是「镜像投喂更旧但仍合法签名的安装包」，概率低，却要求
/// 版本号格式在 Release 元数据与本地状态之间永远一致，一旦不一致就会误拒正常更新。
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct TrustState {
    #[serde(default)]
    seq: u64,
}

/// 本地信任水位文件名。放用户配置目录（与 settings.json 同级），不是 Release 资产 ——
/// 它必须比任何单个 Release 活得久，否则每次发版都要「记住」上一版是什么。
const TRUST_STATE_FILE: &str = "update-trust-state.json";

fn trust_state_path() -> Option<PathBuf> {
    crate::paths::user_config_dir().map(|d| d.join(TRUST_STATE_FILE))
}

impl TrustState {
    fn load() -> Self {
        // 读不出来（首次运行 / 文件损坏）当作「没有水位」：这不是放行漏洞，只是让防回滚
        // 从零开始。真正的信任由签名决定，这里只影响「能否装比之前更旧的包」。
        let Some(p) = trust_state_path() else {
            return Self::default();
        };
        std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let Some(p) = trust_state_path() else {
            return;
        };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // 先写临时文件再 rename：rename 在 Unix 与 Windows 上都会覆盖同名目标，
        // 因此这一步是原子的 —— 半截的 JSON 读回来会被当成「无水位」，而不是
        // 把已记住的水位悄悄清零。
        let Ok(body) = serde_json::to_vec(self) else {
            return;
        };
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, &body).is_ok() && std::fs::rename(&tmp, &p).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// 校验通过后推进 `seq` 水位。**必须**在签名与哈希都过了之后才调用：中途失败不该把水位推高。
fn commit_trust_state(seq: u64) {
    let mut st = TrustState::load();
    if seq > st.seq {
        st.seq = seq;
        st.save();
    }
}

/// 信任清单的来源必须是本仓库 `main` 分支上的那一个固定路径。
///
/// 它刻意**不是** Release 资产：签它需要 root 私钥，而 CI 按设计就拿不到 root 私钥
/// （见 DEVELOPMENT.md §13.3）。清单只在密钥轮换时变，由维护者提交进仓库；
/// 而二进制与校验和仍然锁死在 Release 锚点上（见 `require_release_asset`）。
fn require_trust_origin(url: &str) -> Result<(), String> {
    const ANCHOR: &str = "https://raw.githubusercontent.com/imonior/netsense/";
    let rest = url.strip_prefix(ANCHOR).ok_or_else(|| i18n::t("upd.trust_origin"))?;
    if rest.contains("..") || rest.contains("//") || !rest.ends_with("update/UPDATE-TRUST.json") {
        return Err(i18n::t("upd.trust_origin"));
    }
    Ok(())
}

/// 取回信任清单并用**内置 root 公钥**验签。
///
/// 失败一律 Err，绝不「验不过也放行」：验不过意味着我们无法判断该信哪把 signing 密钥，
/// 此时任何放行都是盲信 —— 那恰好是恶意镜像唯一能利用的缝。
fn fetch_trust_manifest(
    choice: &crate::netproxy::ProxyChoice,
    mirror: &crate::appconfig::UpdateMirror,
) -> Result<TrustManifest, String> {
    require_trust_origin(TRUST_MANIFEST_URL)?;
    // 清单也走镜像：它同样只是「取字节」，安全边界在内置公钥上而不在地址上。
    let raw = crate::ipc::fetch_url(&mirrored_url(TRUST_MANIFEST_URL, mirror), choice)
        .map_err(|e| i18n::tf("upd.trust_fetch", &[("error", &e)]))?;
    let m: TrustManifest =
        serde_json::from_str(&raw).map_err(|e| i18n::tf("upd.trust_parse", &[("error", &e.to_string())]))?;
    // 认识的 schema 只有 1。将来改格式时这里会拒收 —— 宁可停在「看不懂」，也不能
    // 按旧格式误解一份新清单。
    if m.schema != 1 {
        return Err(i18n::tf("upd.trust_parse", &[("error", &format!("schema {}", m.schema))]));
    }
    let root = embedded_root_pubkey().ok_or_else(|| i18n::t("upd.sig_no_key"))?;
    let mut sig = [0u8; 64];
    if hex_decode(&m.signature, &mut sig).is_err() {
        return Err(i18n::t("upd.trust_bad_sig"));
    }
    let bytes = serde_json::to_vec(&TrustBody {
        schema: m.schema,
        seq: m.seq,
        signing_key: &m.signing_key,
        issued_at: &m.issued_at,
    })
    .map_err(|e| i18n::tf("upd.trust_fetch", &[("error", &e.to_string())]))?;
    match verify_ed25519(&bytes, &sig, &root) {
        Ok(()) => Ok(m),
        Err("bad") => Err(i18n::t("upd.trust_bad_sig")),
        Err(_) => Err(i18n::t("upd.trust_failed")),
    }
}

/// 完整走一遍信任链，返回本次该用的 signing 公钥。
///
/// 顺序：验清单（root 签） → 查 `seq` 回滚 → 取 signing 公钥。
/// 每一步都可独立阻断，且都不依赖网络以外的东西，所以镜像改不了其中任何一条。
fn verify_trust_chain(
    choice: &crate::netproxy::ProxyChoice,
    mirror: &crate::appconfig::UpdateMirror,
) -> Result<TrustChain, String> {
    let m = fetch_trust_manifest(choice, mirror)?;
    let state = TrustState::load();

    // 防「清单回滚」：seq 只能单调不减。这是轮换能生效的唯一保证 —— 否则攻击者把一份
    // 「轮换之前」的旧清单反复投喂，已经换掉的旧 signing 密钥就重新变得可信。
    if m.seq < state.seq {
        return Err(i18n::tf(
            "upd.trust_stale",
            &[("seen", &state.seq.to_string()), ("got", &m.seq.to_string())],
        ));
    }

    let mut signing_key = [0u8; 32];
    if hex_decode(&m.signing_key, &mut signing_key).is_err() {
        return Err(i18n::t("upd.trust_bad_key"));
    }

    Ok(TrustChain { signing_key, seq: m.seq })
}

// ———————————————————————————————— 平台安装 ————————————————————————————————

/// 把已经落盘、已经校验过的资产装进系统。签名里没有 `UpdateTarget`：到了安装阶段，
/// 该信的只有那个验过哈希的文件，前端回传的描述已经用完了。
fn install(path: &Path) -> Result<(), String> {
    // 每个平台恰好留一个「尾表达式」块：写成 `return ...;` 的话，在非 macOS 腿上所有分支都被
    // 裁掉后函数就没有返回值了（E0308），而补上 return 又触发 clippy 的 unneeded_return。
    // 三个分支写成同一种形状，任何一条腿都是「一个块表达式收尾」。
    #[cfg(target_os = "macos")]
    {
        install_macos(path)
    }
    #[cfg(target_os = "windows")]
    {
        install_windows(path)
    }
    #[cfg(target_os = "linux")]
    {
        install_linux(path)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = path;
        Err(i18n::t("upd.unsupported_os"))
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
        return Err(i18n::t("upd.bad_bundle"));
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
            .map_err(|e| i18n::tf("upd.elevated_start", &[("error", &e.to_string())]))
    } else {
        Command::new("/bin/sh")
            .arg(&script_path)
            .output()
            .map_err(|e| i18n::tf("upd.install_start", &[("error", &e.to_string())]))
    };
    let _ = std::fs::remove_file(&script_path);
    let out = out?;
    if out.status.success() {
        // 安装成功也要留痕：脚本每步的 echo 原样进日志。「升级后界面全白」那次的
        // 日志里只有一行「SHA256 verified」就开始跑新版本了 —— rm/ditto/open 到底
        // 各自成没成功、open 用的是哪个用户，全靠猜。这些行让下一次排障有据可查。
        // Log the script's step echoes on success too. The all-blank-windows incident
        // after an in-app upgrade left nothing between "SHA256 verified" and the new
        // version starting — whether rm/ditto/open each succeeded and which user open
        // ran as was pure guesswork. These lines make the next investigation concrete.
        let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !so.is_empty() {
            crate::log::info(&so);
        }
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(i18n::tf(
        if elevated { "upd.elevated_failed" } else { "upd.install_failed" },
        &[("error", &err)],
    ))
}

#[cfg(target_os = "macos")]
fn extract_mac_app(asset_path: &Path) -> Result<(PathBuf, Staging), String> {
    match Path::new(asset_path).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("dmg") => extract_dmg(asset_path),
        Some("zip") => extract_zip(asset_path),
        _ => Err(i18n::tf(
            "upd.bad_mac_asset",
            &[("path", &asset_path.display().to_string())],
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
        .map_err(|e| i18n::tf("upd.mount_dmg", &[("error", &e.to_string())]))?;
    if !out.status.success() {
        return Err(i18n::tf(
            "upd.mount_dmg",
            &[("error", String::from_utf8_lossy(&out.stderr).trim())],
        ));
    }
    let app = find_app_bundle(&mount);
    if app.as_os_str().is_empty() {
        return Err(i18n::t("upd.no_app_in_dmg"));
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
        .map_err(|e| i18n::tf("upd.extract_zip", &[("error", &e.to_string())]))?;
    if !out.status.success() {
        return Err(i18n::tf(
            "upd.extract_zip",
            &[("error", String::from_utf8_lossy(&out.stderr).trim())],
        ));
    }
    let app = find_app_bundle(&dest);
    if app.as_os_str().is_empty() {
        return Err(i18n::t("upd.no_app_in_zip"));
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
        return Err(i18n::tf(
            "upd.codesign_refuse",
            &[("path", &app.display().to_string())],
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
        // 用真实路径而非进程名杀：Tauri 二进制名随 productName 走（「NetSense」/「netsense」
        // 皆可能，取决于构建期），而 macOS 的 `killall` 区分大小写 —— 写死小写会匹配不到、
        // 旧实例残留。旧实例残留时 `open` 只会把仍在跑的旧进程提到前台（LaunchServices 不会
        // 起第二份同 bundle），而它发起升级后窗口已被拆除 → 升级后界面全白。这正是 1.8.9
        // 应用内升级白屏的根因。
        // Kill by the real bundle path, not the process name: Tauri's binary name follows
        // productName (case varies) and macOS `killall` is case-sensitive, so a hard-coded
        // lowercase name can miss and leave the old instance alive. A surviving old instance
        // is merely brought to front by `open` (LaunchServices won't launch a second copy of
        // the same bundle), and since it tore its windows down after starting the update the
        // user sees a blank window — the 1.8.9 in-app-upgrade white-screen root cause.
        format!(
            "for p in $(pgrep -f {}/Contents/MacOS/ 2>/dev/null); do kill -9 \"$p\" 2>/dev/null || true; done",
            q(target_app)
        ),
        // 兜底：再按进程名（大小写不敏感）补一刀，覆盖 pgrep 没列全的边界。
        // Fallback: also try the process name case-insensitively for any edge pgrep missed.
        "killall -I netsense 2>/dev/null || true".to_string(),
        "/bin/sleep 1".to_string(),
        format!("/bin/rm -rf {}", q(target_app)),
        // 每步留一行 echo：输出会随 Command::output() 回到 install_macos，成功也写进
        // 应用日志 ——「升级后界面全白」那次的日志在安装期间只有一行校验记录，无从排查。
        // One echo per step: output flows back through Command::output() into the app
        // log even on success (the all-blank-windows incident had only a checksum line).
        "echo \"install: old bundle removed\"".to_string(),
        format!("/usr/bin/ditto {} {}", q(new), q(target_app)),
        "echo \"install: new bundle copied\"".to_string(),
        // 下载来的包没有 quarantine（非浏览器下载），但剥除是幂等的，
        // 与 brew cask postflight 一致，防止新副本被 Gatekeeper 拦。
        format!("/usr/bin/xattr -dr com.apple.quarantine {} 2>/dev/null || true", q(target_app)),
        // 剥完再查一遍是否还有残留 quarantine：有则记下来，供下次白屏定位
        // （被 Gatekeeper 拦的 app 启动后 WebView 会直接失败、界面空白）。
        // Re-check quarantine after stripping: if any remains, log it so a future blank
        // launch can be traced to Gatekeeper (a quarantined app's WebView fails to start).
        format!(
            "if /usr/bin/xattr -l {} 2>/dev/null | /usr/bin/grep -q quarantine; then echo \"install: WARN quarantine still present\"; else echo \"install: quarantine cleared\"; fi",
            q(target_app)
        ),
        // 装后校验：读出刚落地的版本号，确认「新版本真的在盘上」——半途失败的 ditto 可能
        // 留下旧 / 残缺 bundle，这一步把这种情形暴露出来。
        // Post-install check: read the just-copied bundle's version to confirm the NEW
        // version is actually on disk; a half-finished ditto can leave a stale/broken
        // bundle, and this surfaces it.
        format!(
            "echo \"install: installed version = $(/usr/bin/defaults read {}/Contents/Info.plist CFBundleShortVersionString 2>/dev/null || echo UNKNOWN)\"",
            q(target_app)
        ),
        // codesign 自检：落地的二进制若损坏 / 签名不符，open 起来也是坏的（白屏或崩）。
        // 仅记录不阻断（adhoc 签名本就 verify 不过，参见 verify_codesign 的放行策略）。
        // Self codesign check: a corrupted/mismatched binary would launch broken. Log only,
        // never block (ad-hoc signatures fail --verify by design; see verify_codesign).
        format!(
            "if /usr/bin/codesign --verify --deep --strict {} >/dev/null 2>&1; then echo \"install: codesign OK\"; else echo \"install: codesign WARN ($(/usr/bin/codesign -v {} 2>&1 | head -1))\"; fi",
            q(target_app), q(target_app)
        ),
        // 以登录用户（非 root）重新打开：脚本若被 osascript 提权，裸 open 会成 root。
        "OPEN_USER=$(/usr/bin/stat -f '%Su' /dev/console 2>/dev/null || /usr/bin/whoami)".to_string(),
        "echo \"install: relaunch as user $OPEN_USER\"".to_string(),
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
    std::fs::create_dir_all(&dir)
        .map_err(|e| i18n::tf("upd.updates_dir", &[("error", &e.to_string())]))?;
    // 清掉上次的残留安装器
    if let Ok(glob) = std::fs::read_dir(&dir) {
        for e in glob.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let dest = dir.join(format!("netsense-update-installer{}", Path::new(src).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default()));
    std::fs::copy(src, &dest)
        .map_err(|e| i18n::tf("upd.copy_installer", &[("error", &e.to_string())]))?;
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
    let mut cmd = Command::new("powershell");
    hide_console(&mut cmd);
    let out = cmd
        .args(["-NoProfile", "-Command", &ps])
        .output()
        .map_err(|e| i18n::tf("upd.win_elevated_start", &[("error", &e.to_string())]))?;
    if !out.status.success() {
        return Err(i18n::tf(
            "upd.win_elevated_failed",
            &[("error", String::from_utf8_lossy(&out.stderr).trim())],
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
            run_pkexec("dpkg", &["-i", staged.to_str().unwrap_or_default()])?;
            relaunch_linux_app()
        }
        "rpm" => {
            run_pkexec("rpm", &["-U", staged.to_str().unwrap_or_default()])?;
            relaunch_linux_app()
        }
        other => Err(i18n::tf(
            "upd.bad_asset_format",
            &[("ext", other)],
        )),
    }
}

#[cfg(target_os = "linux")]
fn stage_linux_installer(src: &Path) -> Result<PathBuf, String> {
    let dir = if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(d).join("netsense").join("updates")
    } else if let Some(h) = std::env::var_os("HOME") {
        PathBuf::from(h).join(".local").join("share").join("netsense").join("updates")
    } else {
        return Err(i18n::t("upd.no_updates_dir"));
    };
    std::fs::create_dir_all(&dir)
        .map_err(|e| i18n::tf("upd.updates_dir", &[("error", &e.to_string())]))?;
    if let Ok(glob) = std::fs::read_dir(&dir) {
        for e in glob.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let ext = Path::new(src).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let dest = dir.join(format!("netsense-update-installer{ext}"));
    std::fs::copy(src, &dest)
        .map_err(|e| i18n::tf("upd.copy_installer", &[("error", &e.to_string())]))?;
    Ok(dest)
}

#[cfg(target_os = "linux")]
fn run_pkexec(prog: &str, args: &[&str]) -> Result<(), String> {
    let mut cmd = Command::new("pkexec");
    cmd.arg(prog);
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().map_err(|e| {
        i18n::tf("upd.pkexec_start", &[("prog", prog), ("error", &e.to_string())])
    })?;
    if !out.status.success() {
        return Err(i18n::tf(
            "upd.pkexec_failed",
            &[
                ("prog", prog),
                ("error", String::from_utf8_lossy(&out.stderr).trim()),
            ],
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
        Err(e) => Err(i18n::tf("upd.relaunch_failed", &[("error", &e.to_string())])),
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
    // 渠道与目标版本先记一行日志：brew 的失败原因往往只剩 exit code，事后要靠这行判断
    // 当时到底想升到哪儿。同时这也让 `t` 在非 macOS 腿上算「被读过」——那个平台
    // 做不了 Info.plist 回读校验，字段除了日志就没别的用处。
    crate::log::info(&i18n::tf("upd.brew_target", &[("version", &t.version)]));
    let brew = brew_path().ok_or_else(|| crate::i18n::t("upd.no_brew"))?;

    // `brew update` 纯网络，90s 上限；卡住就说明 DNS/API 出问题，宁可超时报错。
    emit_progress(app, "refresh", 0);
    let upd = Command::new(&brew)
        .arg("update")
        .output_timeout(90);
    if let Err(e) = upd {
        crate::log::warn(&i18n::tf("upd.brew_update_failed", &[("error", &e.to_string())]));
    }

    emit_progress(app, "install", 0);
    let run_upgrade = || -> Result<(), String> {
        let out = Command::new(&brew)
            .args(["upgrade", "--cask", "--greedy", "netsense"])
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .output_timeout(300)
            .map_err(|e| i18n::tf("upd.brew_start", &[("error", &e.to_string())]))?;
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
            crate::log::info(&i18n::t("upd.brew_trust_retry"));
            let trust = Command::new(&brew).args(["trust", "imonior/tap"]).output_timeout(30);
            if let Err(trust_err) = trust {
                return Err(i18n::tf("upd.brew_trust", &[("error", &trust_err.to_string())]));
            }
            run_upgrade()?;
        }
        Err(e) => return Err(i18n::tf("upd.brew_failed", &[("error", &e)])),
    }

    // brew 退出 0 但 cask postflight 没杀掉本进程（即没真装）：严格校验磁盘上的版本。
    #[cfg(target_os = "macos")]
    if let Some(installed) = installed_bundle_version() {
        if installed != t.version {
            return Err(crate::i18n::tf(
                "upd.brew_no_effect",
                &[("installed", &installed), ("want", &t.version)],
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
                    i18n::tf("upd.command_timeout", &[("secs", &secs.to_string())]),
                ))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::other(
                i18n::t("upd.wait_disconnected"),
            )),
        }
    }
}

#[cfg(windows)]
fn kill_pid(pid: u32) -> std::io::Result<()> {
    let mut cmd = Command::new("taskkill");
    hide_console(&mut cmd);
    cmd.args(["/PID", &pid.to_string(), "/T", "/F"]).output().map(|_| ())
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
    use ed25519_dalek::{SecretKey, Signer, SigningKey};

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

    /// 升级安装脚本必须是合法 shell，且包含「按真实路径杀旧进程 + 装后校验」三处关键行。
    /// 1.8.9 应用内升级白屏的根因就是 `killall netsense`（小写）大小写敏感匹配不到
    /// `NetSense` 进程、旧实例残留——脚本本身若非法 shell 更会让升级直接半途而废。
    /// The install script must be valid shell and must kill the old process by real path
    /// plus verify the new bundle after copy. The 1.8.9 white-screen root cause was
    /// `killall netsense` (lowercase) missing the `NetSense` process under case-sensitive
    /// `killall`; an invalid script would abort the upgrade midway.
    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_install_script_is_valid_shell_and_kills_by_path() {
        let script = darwin_install_script(
            Path::new("/tmp/netsense-update-staging/NetSense.app"),
            "/Applications/NetSense.app",
        );
        let dir = std::env::temp_dir();
        let p = dir.join("netsense-install-script.sh");
        std::fs::write(&p, &script).unwrap();
        // `sh -n` 只做语法检查、不执行，安全。
        let out = std::process::Command::new("sh").arg("-n").arg(&p).output().unwrap();
        assert!(
            out.status.success(),
            "install script is not valid shell:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // 按真实路径杀（而非写死小写进程名），size 不敏感兜底。
        assert!(script.contains("pgrep -f '/Applications/NetSense.app'/Contents/MacOS/"));
        assert!(script.contains("killall -I netsense"));
        // 装后校验：版本号 / quarantine / codesign 都要回显，供白屏事后定位。
        assert!(script.contains("installed version ="));
        assert!(script.contains("quarantine still present"));
        assert!(script.contains("codesign"));
    }

    #[test]
    fn release_assets_are_pinned_to_this_repository() {
        // 真身：GitHub 的 browser_download_url 就是这个形状。
        assert!(release_asset_pinned(
            "https://github.com/imonior/netsense/releases/download/v1.0.6/NetSense-1.0.6.dmg"
        ));
        assert!(release_asset_pinned(
            "https://github.com/imonior/netsense/releases/download/v1.0.6/SHA256SUMS"
        ));
        // scheme / host / owner / repo 大小写不敏感。
        assert!(release_asset_pinned(
            "HTTPS://GitHub.com/Imonior/NetSense/releases/download/v1/x.dmg"
        ));
    }

    #[test]
    fn a_foreign_release_or_cdn_cannot_pass_as_our_own() {
        for bad in [
            // 攻击者自建站点：二进制与 SUMS 都由他控制，校验形同虚设。
            "https://evil.invalid/imonior/netsense/releases/download/v1/x.dmg",
            // 看着像、其实换成了别人的仓库。
            "https://github.com/attacker/netsense/releases/download/v1/x.dmg",
            // 仓库名带后缀骗过前缀。
            "https://github.com/imonior/netsense-evil/releases/download/v1/x.dmg",
            // 用户信息段伪造 host。
            "https://github.com.evil.invalid/imonior/netsense/releases/download/v1/x.dmg",
            // 302 的落点：放行它等于放行任意仓库的任意资产。
            "https://objects.githubusercontent.com/github-production-release-asset/x",
            // 路径回退，锚点只是前缀。
            "https://github.com/imonior/netsense/releases/download/../../../evil/x.dmg",
            // 空资产名 / 空段。
            "https://github.com/imonior/netsense/releases/download/",
            "https://github.com/imonior/netsense/releases/download//x.dmg",
            "",
        ] {
            assert!(!release_asset_pinned(bad), "should reject {bad:?}");
            assert!(require_release_asset(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn the_anchor_is_stricter_than_https_alone() {
        // 同一串输入：https 放行、锚点拒绝 —— 这正是本次要堵的那道口子。
        let hostile = "https://evil.invalid/x.dmg";
        assert!(require_https(hostile).is_ok());
        assert!(require_release_asset(hostile).is_err());
    }

    /// 镜像只改走法、不改资产：拼出来的地址末尾必然还挂着完整的原始 URL，
    /// 而原始 URL 在此之前已经过 `require_release_asset`。
    #[test]
    fn a_mirror_changes_the_route_and_nothing_else() {
        use crate::appconfig::UpdateMirror;
        const ASSET: &str =
            "https://github.com/imonior/netsense/releases/download/v1.6.3/NetSense_1.6.3_x64-setup.exe";
        assert_eq!(mirrored_url(ASSET, &UpdateMirror::Direct), ASSET);
        // 预设带不带末尾斜杠都一样，且中间只能有一个 `/`
        assert_eq!(
            mirrored_url(ASSET, &UpdateMirror::Prefix("https://gh-proxy.com/".into())),
            format!("https://gh-proxy.com/{ASSET}")
        );
        assert_eq!(
            mirrored_url(ASSET, &UpdateMirror::Prefix("https://ghfast.top".into())),
            format!("https://ghfast.top/{ASSET}")
        );
        // 不合法的前缀原样返回：调用方已经验过原始来源，这里退回「直连」而不是报错
        for bad in ["", "not-a-url", "http://x/", "https://x/?a=1"] {
            assert_eq!(
                mirrored_url(ASSET, &UpdateMirror::Prefix(bad.into())),
                ASSET,
                "should ignore {bad:?}"
            );
        }
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

    #[test]
    fn one_unreadable_line_does_not_void_the_whole_manifest() {
        // 这一条的形状来自 1.0.2 那次 Windows 报「校验失败」时的现场：正文里混进了
        // 不含空白的行（一句表头、或者一段被代码页弄坏的字节）。旧实现遇到第一行就
        // 整份放弃，于是错误说法是「这个资产没在 SHA256SUMS 里」—— 把「我没读完」
        // 报成了「它没列」。现在只跳过读不懂的行。
        let body = concat!(
            "SHA256SUMS\ngarbage-without-any-whitespace\n",
            "4a6974398b6d34b53aa9c3635a1aa528f193f173312adb8fabc92f5372f889f9 *NetSense_1.0.2_x64-setup.exe\n",
        );
        assert_eq!(
            parse_hash(body, "NetSense_1.0.2_x64-setup.exe").as_deref(),
            Some("4a6974398b6d34b53aa9c3635a1aa528f193f173312adb8fabc92f5372f889f9")
        );
        // 跳过不等于放行：真正没列出的资产仍然要返回 None。
        assert_eq!(parse_hash(body, "NetSense_1.0.2_aarch64.dmg"), None);
    }

    /// 镜像若对 `text/*` 做行尾归一（CRLF），会让「对文件字节逐字节验签」直接 INVALID。
    /// `normalize_crlf` 必须在 CI 生成的 LF 清单上是**恒等变换**（否则直连 GitHub 也会验不过），
    /// 同时能把 CRLF / 孤立 `\r` 还原回 LF（镜像改写后仍能验过）。
    #[test]
    fn normalize_crlf_is_identity_on_lf_and_repairs_crlf() {
        // CI 生成的 SHA256SUMS：每行一条、以 LF 结尾、末尾一个 LF。
        let lf = b"a3b1c2d4e5f6  NetSense_x64.exe\n4a6974  NetSense_arm64.dmg\n";
        assert_eq!(normalize_crlf(lf), lf.to_vec());

        // 镜像把 LF 全改成 CRLF：规范化后必须等于原始 LF 字节，签名才能验过。
        let crlf = b"a3b1c2d4e5f6  NetSense_x64.exe\r\n4a6974  NetSense_arm64.dmg\r\n";
        assert_eq!(normalize_crlf(crlf), lf.to_vec());

        // 孤立 \r（老 Mac 风格）也要归一，且不删任何合法换行。
        let odd = b"a3b1c2d4e5f6  NetSense_x64.exe\rb\n4a6974  NetSense_arm64.dmg\n";
        assert_eq!(
            normalize_crlf(odd),
            b"a3b1c2d4e5f6  NetSense_x64.exe\nb\n4a6974  NetSense_arm64.dmg\n".to_vec()
        );

        // 关键：绝不能删尾换行。CI 清单以一个 LF 结尾，删了会让「规范化后字节」与
        // 「签名时的原始字节」不一致，验证必败。
        let trailing = b"a3b1c2d4e5f6  NetSense_x64.exe\n";
        assert_eq!(normalize_crlf(trailing), trailing.to_vec());
    }

    #[test]
    fn sig_suffix_appends_to_path_only() {
        assert_eq!(
            append_sig_suffix("https://github.com/imonior/netsense/releases/download/v1/SHA256SUMS"),
            "https://github.com/imonior/netsense/releases/download/v1/SHA256SUMS.sig"
        );
        assert_eq!(
            append_sig_suffix("https://x.com/SHA256SUMS?token=abc"),
            "https://x.com/SHA256SUMS.sig?token=abc"
        );
        assert_eq!(
            append_sig_suffix("https://x.com/SHA256SUMS#frag"),
            "https://x.com/SHA256SUMS.sig#frag"
        );
    }

    #[test]
    fn hex_decode_rejects_bad_input() {
        let mut b = [0u8; 2];
        assert!(hex_decode("dead", &mut b).is_ok());
        assert_eq!(b, [0xde, 0xad]);
        assert!(hex_decode("xyz", &mut b).is_err());
        assert!(hex_decode("de", &mut b).is_err());
        assert!(hex_decode("de ad", &mut b).is_err());
    }

    #[test]
    fn ed25519_signature_roundtrip_and_tamper() {
        // 固定种子，无需 RNG；仅验证 verify_ed25519 的验签路径。
        let secret = [0x42u8; 32];
        let sk = SigningKey::from(SecretKey::from(secret));
        let pk = sk.verifying_key().to_bytes();
        let msg = b"SHA256SUMS contents";
        let sig: ed25519_dalek::Signature = sk.sign(msg);
        assert!(verify_ed25519(msg, &sig.to_bytes(), &pk).is_ok());
        // 篡改内容必须验不过
        assert!(verify_ed25519(b"tampered", &sig.to_bytes(), &pk).is_err());
        // 长度不对的签名按格式非法拒绝
        assert!(verify_ed25519(msg, &[0u8; 32], &pk).is_err());
        // 公钥不匹配必须验不过
        let other = [0x11u8; 32];
        assert!(verify_ed25519(msg, &sig.to_bytes(), &other).is_err());
    }

    #[test]
    fn weak_low_order_public_keys_are_refused() {
        // Ed25519 的低阶点（weak key）能为**几乎任意**消息产生有效签名，所以普通的
        // `verify` 会放行 —— 它只做标量检查，不做群元素检查。
        //
        // 对 root 公钥这不是威胁（编译期内嵌，出自我们自己的 OsRng）。但信任清单里的
        // signing_key 由清单内容决定：root 私钥一旦泄露，攻击者可以签一份「signing_key
        // 取某个低阶点」的清单，让它对任意内容验签通过。所以这里必须用 verify_strict。
        //
        // 全零公钥是最小的低阶点，用它对任意消息签名，verify 放行、verify_strict 拒绝 ——
        // 两者行为之差正是这条测试要钉住的东西。
        let weak = [0u8; 32];
        let sk = SigningKey::from(SecretKey::from([0x42u8; 32]));
        let msg = b"anything the attacker likes";
        let sig: ed25519_dalek::Signature = sk.sign(msg);
        assert!(verify_ed25519(msg, &sig.to_bytes(), &weak).is_err());

        // 反面对照：正常密钥对仍然必须通过（防止把严格校验用过头，误杀合法发布）。
        let good = sk.verifying_key().to_bytes();
        assert!(verify_ed25519(msg, &sig.to_bytes(), &good).is_ok());
    }

    // ————————————————————————— 信任链（B+）—————————————————————————

    /// 测试侧的小工具：字节数组 → 小写 hex。
    ///
    /// 不给 `src-tauri` 加 `hex` 运行时依赖只为测试 —— 生产代码一律用手写的 `hex_decode`
    /// （它的严格长度/字符校验正是安全边界，多引一个宽松的 hex 反而绕开它）。
    fn to_hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// 生成一份「像样的」信任清单：root 私钥固定，签名有效。
    /// 返回 (root公钥, signing公钥, 已签名的清单 JSON)。
    fn signed_manifest(root_seed: u8, seq: u64, signing_seed: u8) -> ([u8; 32], [u8; 32], String) {
        let root = SigningKey::from(SecretKey::from([root_seed; 32]));
        let signing = SigningKey::from(SecretKey::from([signing_seed; 32]));
        let signing_hex = to_hex(&signing.verifying_key().to_bytes());
        let body = TrustBody {
            schema: 1,
            seq,
            signing_key: &signing_hex,
            issued_at: "2026-01-01T00:00:00Z",
        };
        let bytes = serde_json::to_vec(&body).expect("serialize");
        let sig: ed25519_dalek::Signature = root.sign(&bytes);
        let mut map = serde_json::Map::new();
        map.insert("schema".into(), serde_json::json!(body.schema));
        map.insert("seq".into(), serde_json::json!(body.seq));
        map.insert("signing_key".into(), serde_json::json!(body.signing_key));
        map.insert("issued_at".into(), serde_json::json!(body.issued_at));
        map.insert("signature".into(), serde_json::json!(to_hex(&sig.to_bytes())));
        (
            root.verifying_key().to_bytes(),
            signing.verifying_key().to_bytes(),
            serde_json::to_string(&serde_json::Value::Object(map)).expect("to_string"),
        )
    }

    #[test]
    fn trust_body_bytes_match_between_signer_and_verifier() {
        // 这是整条信任链的地基：两端必须对同一份清单算出**逐字节相同**的待签名字节。
        // 一旦字段顺序或名字漂移，验签会恒定失败，且表现为「所有更新都被拒」而非报错。
        let (root_pk, _, json) = signed_manifest(7, 3, 9);
        let m: TrustManifest = serde_json::from_str(&json).expect("parse");
        let bytes = serde_json::to_vec(&TrustBody {
            schema: m.schema,
            seq: m.seq,
            signing_key: &m.signing_key,
            issued_at: &m.issued_at,
        })
        .expect("serialize");
        let mut sig = [0u8; 64];
        assert!(hex_decode(&m.signature, &mut sig).is_ok());
        assert!(verify_ed25519(&bytes, &sig, &root_pk).is_ok());
    }

    #[test]
    fn a_tampered_trust_manifest_fails_verification() {
        let (root_pk, _, json) = signed_manifest(7, 1, 9);
        let m: TrustManifest = serde_json::from_str(&json).expect("parse");
        let mut sig = [0u8; 64];
        hex_decode(&m.signature, &mut sig).expect("sig hex");

        // 攻击者把 seq 抬到 999（想绕过单调水位），但没 root 私钥改签名。
        let body = TrustBody {
            schema: m.schema,
            seq: 999,
            signing_key: &m.signing_key,
            issued_at: &m.issued_at,
        };
        let forged = serde_json::to_vec(&body).expect("serialize");
        assert!(verify_ed25519(&forged, &sig, &root_pk).is_err());
        // 换一个 root 公钥同样验不过。
        let other_root = SigningKey::from(SecretKey::from([0x77u8; 32])).verifying_key().to_bytes();
        assert!(verify_ed25519(&forged, &sig, &other_root).is_err());
    }

    #[test]
    fn trust_manifest_origin_is_pinned_to_our_repository() {
        assert!(require_trust_origin(TRUST_MANIFEST_URL).is_ok());
        for bad in [
            // 别人的仓库：可以自己签一份清单，把所有密钥换成他-controlled 的。
            "https://raw.githubusercontent.com/attacker/netsense/main/update/UPDATE-TRUST.json",
            // 仓库名带后缀骗过前缀。
            "https://raw.githubusercontent.com/imonior/netsense-evil/main/update/UPDATE-TRUST.json",
            // 换目录（清单挪到别处，或指向一个二进制）。
            "https://raw.githubusercontent.com/imonior/netsense/main/other/UPDATE-TRUST.json",
            // 路径穿越。
            "https://raw.githubusercontent.com/imonior/netsense/main/../../evil/UPDATE-TRUST.json",
            // 换成 Release 资产：那里的字节不由 root 签发。
            "https://github.com/imonior/netsense/releases/download/v1/UPDATE-TRUST.json",
            "https://evil.invalid/update/UPDATE-TRUST.json",
        ] {
            assert!(require_trust_origin(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn trust_state_roundtrips_and_never_moves_backwards() {
        let mut st = TrustState { seq: 5 };
        st.seq = st.seq.max(3); // 更旧的 seq 不该把水位拉回去
        assert_eq!(st.seq, 5);
        assert_eq!(serde_json::to_string(&st).unwrap(), r#"{"seq":5}"#);
        let back: TrustState = serde_json::from_str(r#"{"seq":5}"#).unwrap();
        assert_eq!(back.seq, 5);
        // 缺字段的旧文件 / 坏文件都退回「无水位」，而不是 panic。
        let d: TrustState = serde_json::from_str("{}").unwrap();
        assert_eq!(d.seq, 0);
        // 精简之前写过的状态文件里还带着 `version`：未知字段必须被忽略而不是让解析失败，
        // 否则升级上来的用户会静默丢掉水位（表现为「旧清单又能用了」）。
        let legacy: TrustState = serde_json::from_str(r#"{"seq":7,"version":"1.4.0"}"#).unwrap();
        assert_eq!(legacy.seq, 7);
    }
}
