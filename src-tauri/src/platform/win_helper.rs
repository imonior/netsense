//! Windows 提权 helper：GUI 进程（同一个 exe）经一次 UAC 变成常驻的提权进程，
//! 之后的网络配置批次通过命名管道交给它执行 —— UAC 从「每次下发弹一次」变成
//! 「helper 的一生只弹一次」（它按登录用户命名、跨应用重启存活，详见下）。
//!
//! 这条通道可以常驻的依据（谁能和 helper 说话）：
//!
//! 1. **管道名按登录用户命名，不按 GUI 进程**（`\\.\pipe\netsense-h-<SID>`）。按 PID
//!    命名的话管道随 GUI 进程一起消失，重启一次应用就得重新授权一次 —— 那正是「每个
//!    会话弹一次」的由来。改按 SID 之后，同一个用户在**应用重启之后**还能接上同一条
//!    通道，授权从「每次启动一次」降到「每 30 分钟一次」（[`RECONNECT_WAIT`]）。
//!    安全性没有因此变松：DACL 点名的正是这个 SID，而能不能投递任务始终由
//!    [`client_is_self`] 的「同一个 exe」核验把关。
//!    生命周期跟着**拉起它的那个 GUI 进程**走：GUI 还在就无限期等着，GUI 退出后只多留
//!    一段重连宽限（[`RECONNECT_WAIT`]）等一个刚重启的 GUI 接管，然后自己退场 —— 不留
//!    无人认领的提权进程。
//! 2. **管道 ACL 点名 GUI 进程账户的 SID**（`D:(A;;GA;;;<GUI 账户 SID>)S:(ML;;NW;;;MM)`）：
//!    SID 由 helper 在提权侧从 GUI 令牌里读出来（[`gui_user_sid`]），其他账户与匿名访问
//!    在协议层之前就被内核拒掉。这里**不能**用 CO（CREATOR OWNER）：CO 只在**继承**的
//!    ACE 里被替换成对象属主（MS 的 well-known SIDs 定义），而交给 `CreateNamedPipeW` 的
//!    是一份显式描述符，不经过继承 —— CO 原样留着、匹配不上任何令牌，于是 GUI 一律
//!    `CreateFile=5` 打不开：通道自始至终建不起来，每批配置都默默退回逐批 UAC。
//!    读不到 GUI 令牌就没有 SID、连管道名都算不出来，那种情况一律退场（fail-closed）
//!    而不是猜一个名字。
//!    SACL 那半句把完整性标签压到 Medium —— helper 是 High IL 进程，管道默认继承它的标签，
//!    而 NO_WRITE_UP 会让一个提权的管道对**没提权的自己人**关门（GUI 侧以 read+write 打开
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
//! 到「怎么又要授权」，谁也说不清为什么。连续两次拉不起来就不再**拉**（授权框那一步）；
//! 免费的开管尝试每批都留 —— helper 若只是晚一步上线或起死回生，下一批自己就接上了，不必
//! 重启软件。拉起 helper 自己就要一次授权，一台用不上它的机器如果每批都重拉一次，等于每次
//! 应用配置多弹一个由我们自己造成的框。
//! 「请求已写出、链路断开、是否执行过不确定」的情况会退回逐次 UAC 重跑一遍 —— 配置批次
//! （`netsh interface ipv4 set …`）都是幂等设定，重发不产生额外副作用。

use super::windows::{encode_command, ps, ps_exec_arr, psq, uac_cancelled};
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
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
};
// `SYNCHRONIZE` 在 windows-sys 里归属 `Win32::Storage::FileSystem`（它与
// FILE_ACCESS_RIGHTS 同为 u32 新类型，值 0x00100000），不在 Threading 下。
// `Win32_Storage_FileSystem` 本来就已启用（`PIPE_ACCESS_DUPLEX` 来自那里）。
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;

/// GUI 拉起 helper 时传的隐藏子命令（放在第一个参数位置）。
pub(crate) const HELPER_ARG: &str = "--netsense-helper";

/// 管道行协议版本。每次改了请求/响应形状都必须 +1，否则旧 helper（升级前起、同管道名
/// 还活着的那一个）会按旧语义执行这一批，而它和「版本不符」的客户端之间没有任何东西能
/// 拦住这条错配（审计 W4：macOS 那一侧早就有「版本不符」通道检查，Windows 这一侧缺）。
///
/// 握手在 [`open_conn`] 里做：客户端先发 `version`，helper 回自己的 `ver`；对不上就退回
/// 逐次 UAC，绝不把这一批交给旧语义。
const PROTO_VER: u32 = 1;

/// `last_connect_err` 里给「协议版本对不上」留的一个哨兵码（不在 CreateFile 的正常码表里）。
const VERSION_MISMATCH: u32 = 130;

/// 本进程是否处于 helper 模式（`main` 在所有 GUI 装配之前问这个）。
pub(crate) fn helper_mode() -> bool {
    std::env::args().nth(1).as_deref() == Some(HELPER_ARG)
}

/// 管道名按**登录用户**而不是按 GUI 进程 PID 命名。
///
/// 换掉的理由见模块头第 1 条：按 PID 命名会让管道随 GUI 进程一起消失，于是重启一次
/// 应用就得重新授权一次。SID 形如 `S-1-5-21-…-1001`，把 `-` 换成 `_` 只是让管道名读
/// 起来像个名字，两侧算出的名字必须逐字相同。
fn pipe_path(sid: &str) -> String {
    format!(r"\\.\pipe\netsense-h-{}", sid.replace('-', "_"))
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

/// 拉起之后等第一个客户端的上限：60 秒还连不上，说明 GUI 在连上之前就已经不在了，
/// 一个提权进程留在后台毫无意义。
const FIRST_CLIENT_WAIT: Duration = Duration::from_secs(60);

/// GUI 进程还活着时的轮询间隔。醒来只是为了重新看一眼「它还在不在」，这一轮里若
/// 有客户端连上会立刻被收到，间隔长一点只是把判活这件事做粗一点（10 分钟内退出的话，
/// 宽限判定会晚最多 10 分钟生效，对一个兜底退场来说无关紧要）。
const LIVING_TICK: Duration = Duration::from_secs(600);

/// GUI 进程已经退出之后，继续等一个「刚重启的 GUI」来接管的上限。
///
/// 这段时间就是「关掉再打开不必再授权」的窗口，也是「一个提权进程在无人使用后仍然
/// 存在」的上限 —— 两者是同一个数字，因为它们说的是同一件事：helper 愿意为一个可能
/// 马上回来的客户端多留多久。取 30 分钟：一次普通的「关掉应用、过一会儿再打开」一定
/// 落在里面，而一台真的不再用 NetSense 的机器不会在半小时后还留着它。
const RECONNECT_WAIT: Duration = Duration::from_secs(30 * 60);

/// 处在重连宽限里时的轮询间隔：这时要把「宽限到期」判得准一点，所以比 [`LIVING_TICK`]
/// 密得多。宽限最多 30 分钟，也就是每小时多醒 360 次 —— 一个空转的提权进程，这点开销
/// 可以忽略，而宽限判早判晚直接影响上面那个「多留多久」。
const RECONNECT_TICK: Duration = Duration::from_secs(5);

/// helper 主循环：建管道 → 反复验客户端并服务 → 按 GUI 的生命周期决定何时退场。
///
/// **为什么服务多轮、而不是「一个会话完事」**：GUI 侧把连接缓存起来长期复用，正常
/// 情况下这里一个 GUI 只连一次；但连接可能因为任何一端的原因断掉（GUI 崩了、管道被
/// 别的同账户进程挤掉、helper 上一次自己退了）。原来「服务一轮就退出」在这些情况下
/// 都要 GUI 重新拉一次授权，而重拉的成本是一次 UAC 弹框 —— 于是断一次就多弹一次。
/// 改成循环服务之后，断掉的 GUI 只要还在重连窗口内回来，接上的是同一个提权进程。
///
/// **退场判据**（这是本函数真正的职责，也是「不留下无人认领的提权进程」那条纪律的落点）：
///
/// - 拉起后的第一轮只等 [`FIRST_CLIENT_WAIT`]：GUI 在连上之前就没了的话，没有任何理由
///   让一个提权进程继续挂着。
/// - 之后每一轮：GUI 进程还活着就无限期等（每 [`LIVING_TICK`] 醒一次只为重新判活，
///   期间有客户端连上会立刻被服务）；GUI 已经退出就开始数 [`RECONNECT_WAIT`] 的重连
///   宽限，到点退场。
///   判「GUI 还在不在」用的是 [`WaitForSingleObject`] 对 GUI 进程句柄的等待，而不是
///   「客户端还在不在」—— 两者在这里是同一件事的两种问法，但只有前者能覆盖到「GUI
///   崩了、根本没走bye」这种情况：那之后没有任何客户端会再来，只等客户端的话这个提权
///   进程会一直挂着。
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
    // 管道名与 DACL 都以 GUI 那个账户的 SID 为准（[`gui_user_sid`]）。读不出来就没有
    // SID —— 管道名算不出来、DACL 也没法点名谁，一律退场（fail-closed）。这与旧实现
    // 「退到 IU（交互式登录组）」是相反的选择：IU 也许能覆盖真正的登录用户，但它同时
    // 把这条常提权通道开放给了机器上所有交互式会话，而现在这条通道已经按 SID 命名、
    // 授权范围必须跟着收窄，不能靠一个更宽的兜底来「提高成功率」。
    let Some(sid) = gui_user_sid(gui_pid) else {
        return EXIT_NO_CLIENT;
    };
    let name = to_wide(&pipe_path(&sid));
    let sddl = format!("D:(A;;GA;;;{sid})S:(ML;;NW;;;MM)");
    // GUI 进程句柄（带 SYNCHRONIZE），用于判活。句柄在本函数结束时随进程一起消失，不需要
    // 显式 CloseHandle —— 而这里也**不能**提前关：整个退场判据都建立在它身上。
    let Some(gui) = open_gui_handle(gui_pid) else {
        return EXIT_NO_CLIENT;
    };

    let (tx, rx) = std::sync::mpsc::channel::<Result<File, i32>>();
    // accept 工作线程：反复建实例、直到接上**同一个 exe**（`accept_one` 内部核验），
    // 把接上的那一个交给主线程服务；主线程退场时这个线程还阻塞在 ConnectNamedPipe 上，
    // 随进程一起结束，不必（也无法）去唤醒它。
    let accept_name = name.clone();
    let accept_sddl = sddl.clone();
    std::thread::spawn(move || loop {
        match unsafe { accept_one(&accept_name, &accept_sddl) } {
            Accept::Client(pipe) => {
                if tx.send(Ok(pipe)).is_err() {
                    // 主线程已经走了（退场中）：这一轮连上的客户端没人服务，
                    // 交给下一次重连去做，不为它继续挂着。
                    return;
                }
            }
            Accept::Fatal(code) => {
                let _ = tx.send(Err(code));
                return;
            }
            // 这一次实例没等到合法客户端：换一个实例接着等（deadline 由主线程管）。
            Accept::Retry => {}
        }
    });

    // 第一轮：等 GUI 来连上，最多 FIRST_CLIENT_WAIT。
    match rx.recv_timeout(FIRST_CLIENT_WAIT) {
        Ok(Ok(pipe)) => session(pipe),
        Ok(Err(code)) => return code,
        Err(_) => return EXIT_NO_CLIENT,
    }

    // 之后：GUI 还在就无限等，GUI 走了再给一段重连宽限。
    // 「GUI 退出的时刻」要在**进入宽限之前**记下来 —— 判活每 RECONNECT_TICK 一次，
    // 拿「这一轮开始时」当基准会把退出时刻一次次往后推，宽限就永远不到期。
    let mut gone_since: Option<Instant> = None;
    loop {
        let tick = if gone_since.is_some() {
            RECONNECT_TICK
        } else {
            LIVING_TICK
        };
        if let Some(since) = gone_since {
            if since.elapsed() >= RECONNECT_WAIT {
                return 0;
            }
        }
        match rx.recv_timeout(tick) {
            Ok(Ok(pipe)) => {
                // 有客户端接上了：宽限重新开始计（客户端来了就说明 GUI 回来了，
                // 先前判定的「GUI 已退出」不再成立）。
                session(pipe);
                gone_since = None;
            }
            Ok(Err(code)) => return code,
            Err(_) => {
                if gui_is_gone(gui) && gone_since.is_none() {
                    gone_since = Some(Instant::now());
                }
                // gui_is_gone() 为 false 时什么都不做：GUI 还活着，继续等下一轮。
            }
        }
    }
}

/// 以 `SYNCHRONIZE` 打开 GUI 进程，只为判活。
///
/// 只申请这一个权限：不申请 `PROCESS_QUERY_LIMITED_INFORMATION` 之外的任何东西，也
/// 不申请会失败在「提权进程读低完整性进程」上的那些权限 —— 这里要的仅仅是「等这个
/// 进程结束」的能力，`SYNCHRONIZE` 正好只给这个。
fn open_gui_handle(gui_pid: u32) -> Option<HANDLE> {
    let h = unsafe { OpenProcess(SYNCHRONIZE, 0, gui_pid) };
    (!h.is_null()).then_some(h)
}

/// GUI 进程是否已经结束。
///
/// 判不出来一律回答「还活着」（`Some(h)` 为假才算gone）。这个方向的保守性是对的：
/// 误判成「已退出」只是让宽限开始计时（后果：helper 在 [`RECONNECT_WAIT`] 后退场，
/// 下次用多弹一次框）；误判成「还活着」则相反（后果：提权进程多留一会儿），而这一侧
/// 的代价更小。
fn gui_is_gone(gui: HANDLE) -> bool {
    // 超时给 0 = 只查状态、不阻塞：这必须是一个纯查询，不能让判活把主循环卡住。
    unsafe { WaitForSingleObject(gui, 0) == WAIT_OBJECT_0 }
}

enum Accept {
    Client(File),
    /// 这一次实例没等到合法客户端（对端不是我们 / 瞬时错误），换新实例再来
    Retry,
    Fatal(i32),
}

/// GUI 进程（`gui_pid`）的登录账户 SID，转成 SDDL 可用的字符串形态。
///
/// 为什么管道 ACL 必须点名这个 SID、而不能用 CO：CO 只在**继承**的 ACE 里被替换成对象
/// 属主（MS well-known SIDs 的原文是「used as a placeholder in an inheritable ACE. When
/// the ACE is inherited, the system replaces the CREATOR_OWNER SID with the SID of the
/// object's creator」），而 `CreateNamedPipeW` 收下的是一份显式描述符 —— 那个替换根本
/// 没有机会发生，CO 原样留在 DACL 里，可它不匹配任何访问令牌：核验直接拒绝，GUI 每次都
/// 拿到 CreateFile=5，通道永远建不起来。换成从 GUI 令牌里读出的真实 SID 后，同账户
/// （拆分令牌的常规情形）与 over-the-shoulder（标准用户 + 管理员凭据，helper 运行在另一个
/// 账户下）两种抬升方式都指向「那个正在连上来的账户」。
///
/// 读不出来（进程已退出、句柄拿不到）返回 `None`，由调用方 fail-closed退场 —— 不留在
/// CO 上（一条永远匹配不上的 ACE 等于把通道关死），也不退到 IU（那是比 SID 宽得多的
/// 授权范围，见 [`serve`] 里为什么这次不再兜底）。
fn gui_user_sid(gui_pid: u32) -> Option<String> {
    unsafe {
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, gui_pid);
        if p.is_null() {
            return None;
        }
        let mut tok: HANDLE = std::ptr::null_mut();
        let got_tok = OpenProcessToken(p, TOKEN_QUERY, &mut tok) != 0;
        CloseHandle(p);
        if !got_tok {
            return None;
        }
        let sid = token_user_sid_string(tok);
        CloseHandle(tok);
        sid
    }
}

unsafe fn accept_one(name: &[u16], sddl: &str) -> Accept {
    // SACL 那半句把强制完整性标签钉死成 Medium。它不是可选项：helper 是被 UAC 拉起来的
    // High IL 进程，它建的管道默认继承自己的标签，而完整性策略里的 NO_WRITE_UP 说的是
    // 「低完整性进程不许往高完整性对象写」—— GUI 那一侧是 Medium，于是 `OpenOptions` 那句
    // read+write 在管道**已经建好**的情况下照样回 ERROR_ACCESS_DENIED(5)：授权弹了、
    // 管道开了、话递不过去。降到 Medium 而不是 Low：GUI 与它平级（平级可写），沙箱里的
    // Low 进程仍然写不进来。DACL 那半句由调用方按 GUI 账户的 SID 拼好传进来。
    let sddl = to_wide(sddl);
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
        // 版本握手：客户端连上来先问一次，回的 `ver` 必须和自己的 [`PROTO_VER`] 一致，
        // 否则客户端退回逐次 UAC，绝不把这一批交给旧语义执行（审计 W4）。
        Some("version") => (json!({ "ok": true, "ver": PROTO_VER }).to_string(), false),
        // 与原 `run_elevated_ps` 完全同款的内层脚本：同样的 ErrorActionPreference、
        // 同样的 -EncodedCommand 编码，helper 只是「已经提过权的那一侧」。
        Some("run_ps") => {
            let Some(body) = v.get("body").and_then(|b| b.as_str()) else {
                return (err_resp("run_ps needs body"), false);
            };
            let inner = format!(
                "$ErrorActionPreference='Stop';\r\n[Console]::OutputEncoding=[System.Text.Encoding]::UTF8;\r\n{body}\r\n"
            );
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
        // 客户端道别：结束这一轮`session`，但**不是**让helper 进程退出。
        // `serve` 的主循环会接着进入下一轮（GUI 还活着就继续等，重连宽限内回来就接上），
        // 所以「bye」准确的含义是「这一条连接我不用了」，不是「你可以退场了」。
        // 真正决定退场的是 [`serve`] 里的判活与宽限计时，不是这个 op。
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
/// 就不再**拉起** helper（封顶的只是会弹授权框的那一步；免费的开管尝试每批都留，见
/// [`ensure_conn`]）。封顶的理由不是省几次重试，而是**少弹框** —— 拉起 helper 本身
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
/// 5 = 名字在但内核不肯让我们开得动 —— 完整性标签已由服务端钉成 Medium，DACL 按 GUI
/// 账户的 SID 授权（读不到令牌才兜底 IU），所以这一码意味着开管的那一侧不在授权名单里，
/// 231 = 对面的实例全被占着，109 = 连上过、这一批的时候对面已经不在了。现场日志只要留下
/// 这一串数字就能定方向 —— 分不开它们，「helper 没弹出来」和「弹出来了却不肯收活」在
/// 界面上长得一模一样。
fn connect_reason() -> String {
    match last_connect_err().load(Ordering::Relaxed) {
        0 => "helper was never contacted".to_string(),
        2 => "helper pipe never appeared (CreateFile=2)".to_string(),
        5 => "helper pipe refused this account (CreateFile=5)".to_string(),
        109 => "helper dropped the connection (win32=109)".to_string(),
        231 => "helper pipe has no free instance (CreateFile=231)".to_string(),
        130 => "helper protocol version mismatch (old helper still running)".to_string(),
        other => format!("helper pipe could not be opened (CreateFile={other})"),
    }
}

/// 记一次失败，并给出这一批的诊断。计数钉在 [`GIVE_UP_AFTER`] 封顶：它只用来管「拉不拉」，
/// 而 u8 回绕会把闸门重新打开 —— 与其放任涨破，不如到顶就不再动。
fn note_failure() -> String {
    #[allow(clippy::incompatible_msrv)] // try_update stable since 1.95, MSRV is 1.77
    let _ = helper_fails().try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
        (v < GIVE_UP_AFTER).then_some(v + 1)
    });
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
    // 放弃「拉」不等于放弃这条路：上面免费的开管每批照做，helper 晚一步上线或起死回生，
    // 下一批自己就接上了。封顶的只有会弹授权框的 spawn 那一步。
    if helper_fails().load(Ordering::Relaxed) >= GIVE_UP_AFTER {
        return Ok(None);
    }
    if helper_seen().swap(false, Ordering::Relaxed) {
        // 拉起过、现在却开不了：helper 死了（或 GUI 刚重启过）。这次退回逐次 UAC，
        // 下一批会重新走一遍「拉起」—— 罕见事件多付一次授权，换实现里没有定时器。
        return Ok(None);
    }
    spawn_helper()?;
    // 提权进程从被批准到建好管道有落盘/加载的开销，轮询等它，而不是赌一次。
    // 上限 15 秒：这条循环跑在用户点了「应用」之后的那条同步路径上，等满 30 秒的体验是
    // 「点了没反应」，而真到 15 秒还没建起来，再等下去也只是把逐次 UAC 那条退路推迟 ——
    // 那一批照样能下发，只是多弹一次框。
    for _ in 0..150 {
        if let Some(c) = open_conn() {
            helper_seen().store(true, Ordering::Relaxed);
            return Ok(Some(c));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(None)
}

/// 本进程所属账户的 SID，管道名要用它。
///
/// 取的是**自己**的令牌（`OpenProcessToken(GetCurrentProcess())`），不是某个 pid 的 ——
/// 调用方问的是「我是谁」，让它去查别人反而会引入「那个进程还在不在」这种与本问题
/// 无关的失败模式。helper 侧同样按 GUI 的 SID 命名（[`pipe_path`]），两侧算出的名字
/// 必须逐字相同。
fn own_user_sid() -> Option<String> {
    let mut tok: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) } == 0 {
        return None;
    }
    // `GetCurrentProcess()` 返回的是伪句柄，**不能** CloseHandle —— 真正要关的是
    // OpenProcessToken 打开的令牌句柄。
    let sid = token_user_sid_string(tok);
    unsafe { CloseHandle(tok) };
    sid
}

/// 从一个令牌句柄读出 SID 的字符串形态。
///
/// 分离出来是因为两条路径都要用（自己 / 某个 pid），而只有「打开令牌」那一步不同。
/// 读不出就返回 `None`，调用方一律 fail-closed。
fn token_user_sid_string(tok: HANDLE) -> Option<String> {
    unsafe {
        // `TOKEN_USER` 是指针对齐的结构，缓冲区用 u64 数组而不是 [u8; N]。
        let mut buf = [0u64; 16];
        let mut ret = 0u32;
        let ok = GetTokenInformation(
            tok,
            TokenUser,
            buf.as_mut_ptr().cast(),
            std::mem::size_of_val(&buf) as u32,
            &mut ret,
        ) != 0;
        if !ok {
            return None;
        }
        let sid = (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid;
        let mut w: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut w) == 0 || w.is_null() {
            return None;
        }
        // ConvertSidToStringSidW 保证产物是 NUL 结尾的宽字符串。
        let mut len = 0usize;
        while *w.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(w, len));
        LocalFree(w as HLOCAL);
        Some(s)
    }
}

/// 只连接已存在的 helper，绝不拉起。连不上的错误码留给 [`connect_reason`]。
fn open_conn() -> Option<Conn> {
    // 读不到自己的 SID 就没法算出管道名，也就没有「只连不拉」这条路可走 ——
    // 这种情况必须让 ensure_conn 继续往下走去拉起 helper，而不是静默返回 None 把这一批
    // 悄悄送去逐次 UAC。错误码仍然记成「管道从来没出现」，与现状一致。
    let path = pipe_path(&own_user_sid()?);
    let f = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            last_connect_err().store(e.raw_os_error().unwrap_or(0) as u32, Ordering::Relaxed);
            return None;
        }
    };
    let r = f.try_clone().ok().map(BufReader::new)?;
    let mut conn = Conn { w: f, r };
    // 版本握手：老版本 helper（升级前起、同管道名还活着）不认 `version` op，会回
    // `ok:false`；协议版本对不上也必须拒绝 —— 绝不能把这一批交给旧语义执行（审计 W4）。
    // 握手失败就当连不上，退回逐次 UAC。
    match conn.call(&json!({ "op": "version" }).to_string()) {
        Ok(resp)
            if resp.get("ok").and_then(Value::as_bool).unwrap_or(false)
                && resp.get("ver").and_then(Value::as_u64) == Some(PROTO_VER as u64) =>
        {
            Some(conn)
        }
        _ => {
            last_connect_err().store(VERSION_MISMATCH, Ordering::Relaxed);
            None
        }
    }
}

/// 用同一个 exe、带着自己的 PID，提权拉起 helper。
///
/// `Start-Process -Verb RunAs` 会在函数内部等用户答复授权 —— 授权框挂着的时候
/// 这条 PowerShell 不返回，所以这里不需要额外的等待逻辑；取消的退出码约定（1223）
/// 沿用 [`run_elevated_ps`] 那套。
fn spawn_helper() -> Result<(), String> {
    let pid = std::process::id();
    let pid_s = pid.to_string();
    // exe 路径由 Rust 自己给出，而不是到 PowerShell 里问 `Get-Process -Id $pid`：
    // 那条路要读**另一个进程**的句柄，权限不足或进程信息被系统截短时 `.Path` 返回空，
    // `Start-Process -FilePath` 于是拿到空串、脚本走进 catch、exit 1223 —— 上层把它
    // 读成「用户点了取消」，表现是「每次下发都报已取消」，而真相只是没问到自己的路径。
    // `current_exe()` 问的是自己，没有这层依赖，也就不会因为环境差异而时灵时不灵。
    let exe = std::env::current_exe()
        .map_err(|e| crate::i18n::tf("pal.elevate_failed", &[("error", &e.to_string())]))?;
    let script = format!(
        "$ErrorActionPreference='Stop';\
         try {{ $p = Start-Process -FilePath {} -ArgumentList {} -Verb RunAs -WindowStyle Hidden -PassThru; \
         if (-not $p) {{ exit 1 }} }} \
         catch {{ $ex = $_.Exception; \
         if ($ex.HResult -eq -2147023673 -or ($ex.Message -match 'cancel')) {{ exit 1223 }} else {{ exit 1 }} }}",
        psq(&exe.to_string_lossy()),
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

