//! Windows 提权 helper：GUI 进程（同一个 exe）经一次 UAC 变成常驻的提权进程，
//! 之后的网络配置批次通过命名管道交给它执行 —— UAC 从「每次下发弹一次」变成
//! 「每个 GUI 会话只弹一次」。
//!
//! 这条通道可以常驻的依据（谁能和 helper 说话）：
//!
//! 1. **管道名携带 GUI 自己的 PID**（`\\.\pipe\netsense-h-<pid>`）：只有启动它的那个
//!    GUI 找得到这条路；GUI 退出时句柄关闭、helper 读到 EOF 即退出，不会留下无人认领
//!    的提权进程（首个客户端迟迟不来也有 60 秒自杀兜底）。
//! 2. **管道的安全描述符钉成 `D:(A;;GA;;;CO)S:(ML;;NW;;;MM)`**：DACL 那半句只给创建者
//!    属主（当前登录用户）全权，其他账户与匿名访问在协议层之前就被内核拒掉；SACL 那半句
//!    把完整性标签压到 Medium —— helper 是 High IL 进程，管道默认继承它的标签，而
//!    NO_WRITE_UP 会让一个提权的管道对**没提权的自己人**关门（GUI 侧以 read+write 打开
//!    就回 ERROR_ACCESS_DENIED）。标签留在 Medium 而不是更低：GUI 与它平级所以写得上话，
//!    沙箱里（Low IL）的进程仍然写不进来。
//! 3. **客户端路径核验**：接受连接后，helper 用 `GetNamedPipeClientProcessId` 取对端
//!    进程镜像路径，与 `current_exe()` 归一化比对 —— 只有「同一个 NetSense 可执行文件」
//!    才能投递任务。第 2 条按账户授权，挡不住同账户的普通权限进程；这条补上那个洞：
//!    恶意程序想静默借用这条常提权通道，得先把安装目录里的 exe 换掉，而那一步本身就要管理员。
//! 4. helper **只接受一个操作**：`run_ps` —— 执行的本就是原来经
//!    `Start-Process -Verb RunAs` 下发的 PowerShell 批次，不提供任何额外解析。
//!    用户脚本（`run_script` 的提权分支）**有意**继续逐次 UAC：那种每次都要用户点头，
//!    见 `WindowsPlatform::run_script` 里的注释。
//!
//! 降级链（任何一环失效都不至于把功能弄坏）：授权被点掉 → 报「已取消」，不再弹第二个框；
//! 管道开不了 / helper 半路死掉 → 退回原来的每次 `run_elevated_ps` 弹窗模式，并且**写一条
//! 带 Win32 错误码的日志**（一个会话一条）—— 静默降级是这条通道最坏的失效方式：用户只看
//! 到「怎么又要授权」，谁也说不清为什么。连续两次走不成就不再碰 helper，本会话直接走逐次
//! UAC：拉起 helper 自己就要一次授权，一台用不上它的机器如果每批都重拉一次，等于每次应用
//! 配置多弹一个由我们自己造成的框。
//! 「请求已写出、链路断开、是否执行过不确定」的情况会退回逐次 UAC 重跑一遍 —— 配置批次
//! （`netsh interface ipv4 set …`）都是幂等设定，重发不产生额外副作用。

use super::windows::{encode_command, ps, ps_exec_arr, uac_cancelled};
use super::{priv_channel, PrivChannel};
use crate::i18n;
use crate::platform::run;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// GUI 拉起 helper 时传的隐藏子命令（放在第一个参数位置）。
pub(crate) const HELPER_ARG: &str = "--netsense-helper";

/// 本进程是否处于 helper 模式（`main` 在所有 GUI 装配之前问这个）。
pub(crate) fn helper_mode() -> bool {
    std::env::args().nth(1).as_deref() == Some(HELPER_ARG)
}

fn pipe_path(pid: u32) -> String {
    format!(r"\\.\pipe\netsense-h-{pid}")
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// —————————————————————————— helper 服务端（提权侧） ——————————————————————————

/// 服务端退出码：参数不对。
const EXIT_BAD_ARG: i32 = 2;
/// 服务端退出码：安全描述符或管道实例创建失败（环境不允许常驻通道）。
const EXIT_SERVER_FAIL: i32 = 3;
/// 服务端退出码：60 秒内没有等到合法客户端（GUI 在连上之前崩了），自杀兜底。
const EXIT_NO_CLIENT: i32 = 4;
/// 服务端退出码：这个进程实际并没有被提权（手动不带 UAC 跑了 helper）。
/// 未提权的 helper 毫无意义，还会在同名管道上留一个只会失败的应答者 —— fail-fast。
const EXIT_NOT_ADMIN: i32 = 5;

/// helper 主循环：建管道 → 验客户端 → 服务这一个会话 → 客户端断开即退出。
///
/// 「等客户端」这一步没有超时参数可用（同步 `ConnectNamedPipe` 一阻塞就是到底），所以它
/// 放在工作线程里做，主线程只按 deadline 结束进程：60 秒等不到客户端，说明 GUI 在连上之前
/// 就已经不在了，一个提权进程留在后台毫无意义。
///
/// 不装日志、不起 Tauri：错误文本一律通过管道回给 GUI，由那边统一落日志和上界面
/// （两个进程追写同一个日志文件是要打架的）。
pub(crate) fn serve() -> i32 {
    // 字典装起来只有一个理由：spawn 子进程失败时的错误文本是给用户看的（经管道回到
    // GUI 界面），没装字典就只剩裸 key。
    i18n::init();
    let raw = std::env::args().nth(2).unwrap_or_default();
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return EXIT_BAD_ARG;
    }
    let Ok(gui_pid) = raw.parse::<u32>() else {
        return EXIT_BAD_ARG;
    };
    if !matches!(priv_channel(), PrivChannel::Direct) {
        return EXIT_NOT_ADMIN;
    }
    let name = to_wide(&pipe_path(gui_pid));
    let deadline = Instant::now() + Duration::from_secs(60);
    let (tx, rx) = std::sync::mpsc::channel::<Result<File, i32>>();
    std::thread::spawn(move || loop {
        match unsafe { accept_one(&name) } {
            Accept::Client(pipe) => {
                let _ = tx.send(Ok(pipe));
                return;
            }
            Accept::Fatal(code) => {
                let _ = tx.send(Err(code));
                return;
            }
            // 这一次实例没等到合法客户端：换一个实例接着等（deadline 由主线程管）。
            Accept::Retry => {}
        }
    });
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(pipe)) => {
            session(pipe);
            0
        }
        Ok(Err(code)) => code,
        // Err = 到点没等到客户端，或者工作线程已经没了（它 panic 时不会留下应答者）。
        Err(_) => EXIT_NO_CLIENT,
    }
}

enum Accept {
    Client(File),
    /// 这一次实例没等到合法客户端（对端不是我们 / 瞬时错误），换新实例再来
    Retry,
    Fatal(i32),
}

unsafe fn accept_one(name: &[u16]) -> Accept {
    // DACL 只给创建者属主（当前登录账户）全权；SACL 把强制完整性标签钉死成 Medium。
    // 后半句不是可选项：helper 是被 UAC 拉起来的 High IL 进程，它建的管道默认继承自己的
    // 标签，而完整性策略里的 NO_WRITE_UP 说的是「低完整性进程不许往高完整性对象写」——
    // GUI 那一侧是 Medium，于是 `OpenOptions` 那句 read+write 在管道**已经建好**的情况下
    // 照样回 ERROR_ACCESS_DENIED(5)：授权弹了、管道开了、话递不过去。
    // 降到 Medium 而不是 Low：GUI 与它平级（平级可写），沙箱里的 Low 进程仍然写不进来。
    let sddl = to_wide("D:(A;;GA;;;CO)S:(ML;;NW;;;MM)");
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let mut size = 0u32;
    if ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl.as_ptr(),
        SDDL_REVISION_1,
        &mut sd,
        &mut size,
    ) == 0
    {
        return Accept::Fatal(EXIT_SERVER_FAIL);
    }
    let sa = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    // 不带 FILE_FLAG_OVERLAPPED：这个句柄随后是交给 `std::fs::File` 读写行协议的，而标准库
    // 的 read/write 传的 OVERLAPPED 是 NULL —— 重叠句柄 + NULL 属于「行为不保证」那一类
    // （可能把还没完成的操作报成已完成）。宁要一个会老实阻塞的同步句柄。
    let h = CreateNamedPipeW(
        name.as_ptr(),
        PIPE_ACCESS_DUPLEX,
        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
        PIPE_UNLIMITED_INSTANCES,
        8 * 1024,
        8 * 1024,
        0,
        &sa,
    );
    let _ = LocalFree(sd as HLOCAL);
    if h == INVALID_HANDLE_VALUE {
        return Accept::Fatal(EXIT_SERVER_FAIL);
    }
    if ConnectNamedPipe(h, std::ptr::null_mut()) == 0 && GetLastError() != ERROR_PIPE_CONNECTED {
        CloseHandle(h);
        return Accept::Retry;
    }
    if !client_is_self(h) {
        // 不是同一个 exe 连上来的（同账户的其他程序）：掐掉，换新实例继续等。
        CloseHandle(h);
        return Accept::Retry;
    }
    Accept::Client(File::from_raw_handle(h as RawHandle))
}

/// 对端进程是否就是「这个安装目录里的同一个 Netsense 可执行文件」。
/// 判不出来一律算不是（fail-closed）。
fn client_is_self(pipe: HANDLE) -> bool {
    let Some(image) = (unsafe { client_image(pipe) }) else {
        return false;
    };
    let Ok(mine) = std::env::current_exe() else {
        return false;
    };
    norm_path(&image) == norm_path(&mine.display().to_string())
}

/// `GetNamedPipeClientProcessId` → 对端镜像全路径。任何一步判不了都返回 `None`。
unsafe fn client_image(pipe: HANDLE) -> Option<String> {
    let mut pid: u32 = 0;
    if GetNamedPipeClientProcessId(pipe, &mut pid) == 0 {
        return None;
    }
    let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if p.is_null() {
        return None;
    }
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(p, 0, buf.as_mut_ptr(), &mut len) != 0;
    CloseHandle(p);
    if ok {
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    } else {
        None
    }
}

/// 归一化比较用的路径形态：去掉 `\\?\` 前缀、去尾分隔符、忽略大小写。
fn norm_path(s: &str) -> String {
    s.strip_prefix(r"\\?\")
        .unwrap_or(s)
        .trim_end_matches(['/', '\\'])
        .to_lowercase()
}

/// 服务已核验的客户端：一行 JSON 进、一行 JSON 出，直到客户端断开（EOF）或被要求退出。
fn session(pipe: File) {
    let Ok(mut reader) = pipe.try_clone().map(BufReader::new) else {
        return;
    };
    let mut writer = pipe;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (resp, stop) = handle_request(trimmed);
        let sent = writer
            .write_all(resp.as_bytes())
            .and_then(|_| writer.write_all(b"\n"))
            .and_then(|_| writer.flush());
        if sent.is_err() || stop {
            break;
        }
    }
}

fn handle_request(line: &str) -> (String, bool) {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return (err_resp(&format!("bad request: {e}")), false),
    };
    match v.get("op").and_then(|o| o.as_str()) {
        // 与原 `run_elevated_ps` 完全同款的内层脚本：同样的 ErrorActionPreference、
        // 同样的 -EncodedCommand 编码，helper 只是「已经提过权的那一侧」。
        Some("run_ps") => {
            let Some(body) = v.get("body").and_then(|b| b.as_str()) else {
                return (err_resp("run_ps needs body"), false);
            };
            let inner = format!("$ErrorActionPreference='Stop';\r\n{body}\r\n");
            let b64 = encode_command(&inner);
            match run(
                "powershell.exe",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-EncodedCommand",
                    &b64,
                ],
            ) {
                Ok(_) => (json!({ "ok": true }).to_string(), false),
                Err(e) => (json!({ "ok": false, "error": e }).to_string(), false),
            }
        }
        Some("bye") => (json!({ "ok": true }).to_string(), true),
        Some(other) => (err_resp(&format!("unknown op: {other}")), false),
        None => (err_resp("missing op"), false),
    }
}

fn err_resp(msg: &str) -> String {
    json!({ "ok": false, "error": msg }).to_string()
}

// —————————————————————————— GUI 侧客户端（普通权限） ——————————————————————————

/// 一次管道连接：读写端各持一份句柄（`try_clone` 复制出来的），行协议两端对齐。
struct Conn {
    w: File,
    r: BufReader<File>,
}

impl Conn {
    fn call(&mut self, req: &str) -> std::io::Result<Value> {
        self.w.write_all(req.as_bytes())?;
        self.w.write_all(b"\n")?;
        self.w.flush()?;
        let mut line = String::new();
        if self.r.read_line(&mut line)? == 0 {
            // 对端在应答前退场（109 = ERROR_BROKEN_PIPE 的语义）。
            return Err(std::io::Error::from_raw_os_error(109));
        }
        serde_json::from_str(&line).map_err(std::io::Error::other)
    }
}

pub(crate) enum Outcome {
    /// 批次经 helper 给出终局（成功，或带用户可读文本的失败）。
    Done(Result<(), String>),
    /// 够不着 helper —— 调用方应当退回原来的逐次 UAC 模式。带的是给人看的**诊断**
    /// （卡在哪个 Win32 错误码上），不是界面文案；界面文案由调用方按字典组装。
    Unavailable(String),
}

fn client_slot() -> &'static Mutex<Option<Conn>> {
    static C: OnceLock<Mutex<Option<Conn>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// 本会话是否已经成功拉起过 helper（决定「管道开不开」该重试还是该放弃）。
fn helper_seen() -> &'static AtomicBool {
    static A: AtomicBool = AtomicBool::new(false);
    &A
}

/// 连续「这一批没能走常驻通道」的次数：一次成功的调用清零，攒到 [`GIVE_UP_AFTER`]
/// 就整个会话不再尝试。封顶的理由不是省几次重试，而是**少弹框** —— 拉起 helper 本身
/// 就要一次授权，一台用不上这条通道的机器如果每批都重拉一次，等于每次应用配置多弹一个
/// 由我们自己造成的框，那比「每次都弹」的旧行为还糟。
fn helper_fails() -> &'static AtomicU8 {
    static A: AtomicU8 = AtomicU8::new(0);
    &A
}

/// 连不上几次就放弃本会话。2 次留给「helper 崩过 / GUI 刚重启」这类一次性的意外，
/// 又不至于让人连着挨两个我们自找的框。
const GIVE_UP_AFTER: u8 = 2;

/// 最近一次连不上管道的 Win32 错误码（`open_conn` 写，断链那一步补写）。
fn last_connect_err() -> &'static AtomicU32 {
    static A: AtomicU32 = AtomicU32::new(0);
    &A
}

/// 把「够不着」拆成互相分得开的几种：2 = 管道从来没出现（helper 没起来，或起来就退了），
/// 5 = 名字在但内核不肯让我们开得动 —— 完整性标签已由服务端钉成 Medium，所以这一码如今
/// 只剩账户不对（DACL 只放创建者属主，即 UAC 那一步用的是另一个账户的凭据），
/// 231 = 对面的实例全被占着，109 = 连上过、这一批的时候对面已经不在了。现场日志只要留下
/// 这一串数字就能定方向 —— 分不开它们，「helper 没弹出来」和「弹出来了却不肯收活」在
/// 界面上长得一模一样。
fn connect_reason() -> String {
    match last_connect_err().load(Ordering::Relaxed) {
        0 => "helper was never contacted".to_string(),
        2 => "helper pipe never appeared (CreateFile=2)".to_string(),
        5 => "helper pipe belongs to another account (CreateFile=5)".to_string(),
        109 => "helper dropped the connection (win32=109)".to_string(),
        231 => "helper pipe has no free instance (CreateFile=231)".to_string(),
        other => format!("helper pipe could not be opened (CreateFile={other})"),
    }
}

/// 记一次失败，并给出这一批的诊断。
fn note_failure() -> String {
    helper_fails().fetch_add(1, Ordering::Relaxed);
    connect_reason()
}

/// 退回逐次 UAC 这件事一个会话只记一条：每批都写会把日志刷满，而这些批次失败的原因
/// 从头到尾只有一个。
pub(crate) fn log_fallback(reason: &str) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        crate::log::warn(&i18n::tf("logs.win_helper_fallback", &[("reason", reason)]));
    }
}

/// 把一批 PowerShell 指令交给 helper。锁被前一个持锁线程的 panic 弄污时照常继续：
/// 里面没有跨调用不变的账本，一个 `Option<Conn>` 丢了重建就是。
pub(crate) fn run_batch(body: &str) -> Outcome {
    let req = json!({ "op": "run_ps", "body": body }).to_string();
    let mut g = client_slot().lock().unwrap_or_else(|p| p.into_inner());
    if helper_fails().load(Ordering::Relaxed) >= GIVE_UP_AFTER {
        return Outcome::Unavailable(format!(
            "gave up after {GIVE_UP_AFTER} failed batches, last: {}",
            connect_reason()
        ));
    }
    // 连接取出使用：跑成功的放回槽里留给下一批，断掉的绝不放回（对面已经把这个连接
    // 视为结束，留在手里只会让下一批再白摔一次）。
    let mut conn = match g.take() {
        Some(c) => c,
        None => match ensure_conn() {
            Err(msg) => return Outcome::Done(Err(msg)),
            Ok(None) => return Outcome::Unavailable(note_failure()),
            Ok(Some(c)) => c,
        },
    };
    match conn.call(&req) {
        Ok(resp) => {
            *g = Some(conn);
            helper_fails().store(0, Ordering::Relaxed);
            if resp.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                return Outcome::Done(Ok(()));
            }
            let msg = resp
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("helper failed")
                .to_string();
            Outcome::Done(Err(msg))
        }
        Err(e) => {
            helper_seen().store(false, Ordering::Relaxed);
            // 没有 raw_os_error 的那一路是我们自己造的（应答不是 JSON / 应答前对面退场）。
            last_connect_err().store(e.raw_os_error().unwrap_or(109) as u32, Ordering::Relaxed);
            Outcome::Unavailable(note_failure())
        }
    }
}

fn ensure_conn() -> Result<Option<Conn>, String> {
    if let Some(c) = open_conn() {
        return Ok(Some(c));
    }
    if helper_seen().swap(false, Ordering::Relaxed) {
        // 拉起过、现在却开不了：helper 死了（或 GUI 刚重启过）。这次退回逐次 UAC，
        // 下一批会重新走一遍「拉起」—— 罕见事件多付一次授权，换实现里没有定时器。
        return Ok(None);
    }
    spawn_helper()?;
    // 提权进程从被批准到建好管道有落盘/加载的开销，轮询等它，而不是赌一次。
    for _ in 0..300 {
        if let Some(c) = open_conn() {
            helper_seen().store(true, Ordering::Relaxed);
            return Ok(Some(c));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(None)
}

/// 只连接已存在的 helper，绝不拉起。连不上的错误码留给 [`connect_reason`]。
fn open_conn() -> Option<Conn> {
    let path = pipe_path(std::process::id());
    let f = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            last_connect_err().store(e.raw_os_error().unwrap_or(0) as u32, Ordering::Relaxed);
            return None;
        }
    };
    let r = f.try_clone().ok().map(BufReader::new)?;
    Some(Conn { w: f, r })
}

/// 用同一个 exe、带着自己的 PID，提权拉起 helper。
///
/// `Start-Process -Verb RunAs` 会在函数内部等用户答复授权 —— 授权框挂着的时候
/// 这条 PowerShell 不返回，所以这里不需要额外的等待逻辑；取消的退出码约定（1223）
/// 沿用 [`run_elevated_ps`] 那套。
fn spawn_helper() -> Result<(), String> {
    let pid = std::process::id();
    let pid_s = pid.to_string();
    let script = format!(
        "$ErrorActionPreference='Stop';\
         try {{ $x = (Get-Process -Id {pid}).Path; \
         $p = Start-Process -FilePath $x -ArgumentList {} -Verb RunAs -WindowStyle Hidden -PassThru; \
         if (-not $p) {{ exit 1223 }} }} catch {{ exit 1223 }}",
        ps_exec_arr(&[HELPER_ARG, &pid_s])
    );
    match ps(&script) {
        Ok(_) => Ok(()),
        // 用户点了「否」：报已取消，本批不再另弹第二个框。
        Err(e) if e.contains("1223") => Err(uac_cancelled(e)),
        // 其他拉起失败（罕见，比如 exe 路径都读不出来）：交给轮询超时去退回逐次 UAC。
        Err(_) => Ok(()),
    }
}

