//! Linux 网络读路径的原生实现：用 netlink（rtnetlink）替掉 `ip link` / `ip neigh` 的子进程
//! 调用，消除冷启动与 locale / 输出格式依赖。
//!
//! 设计要点：
//! - 整个模块只在 Linux 编译（`#![cfg(target_os = "linux")]`），其它平台不引入 rtnetlink。
//! - 每个公开函数都「netlink 优先，`ip` 命令兜底」：netlink 任一环节出错就回退命令行，
//!   行为不退化（最差情况只是没拿到原生加速，功能与今天一致）。
//! - **连接池化**：一条 netlink 连接由一个专用后台线程常驻驱动，所有查询经 mpsc 串行发
//!   过去、用 std `mpsc` 把应答送回。这样既没有「每次调用 new_connection + spawn 一个永不退
//!   出的 task」造成的 fd / task 泄漏，也不会因为复用同一个 socket 而把上一条请求的应答
//!   串到另一条查询头上（审计 L3：早期的那版每调用建连 + abort，虽然不漏但每次都重建，
//!   且并发下应答会错位）。
//!
//! 为什么应答通道用 **std `mpsc` 而不是 tokio `oneshot`**：
//! `link_is_up` / `gateway_mac` 这两个同步函数会被 Tauri 的 **async 命令**（`get_status` 等）
//! 直接调用，函数体跑在 tokio worker 上。`oneshot::Receiver::blocking_recv()` 的契约是
//! 「在 async 执行上下文里调用会 panic」——于是在 Linux 上每一次弹窗 / 编辑器刷新（凡是有
//! 默认网关的那一次）命令的回复通道都被丢掉、前端 `invoke()` 拿不到结果。std 的
//! `recv_timeout` 没有这个断言，UI 路径从此不再炸。

#![cfg(target_os = "linux")]

use netlink_packet_route::link::LinkFlags;
use netlink_packet_route::neighbour::{NeighbourAddress, NeighbourAttribute};
use std::net::IpAddr;
use std::sync::{mpsc as std_mpsc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::stream::TryStreamExt;
use rtnetlink::Handle;
use tokio::sync::mpsc;

/// worker 侧单条 netlink 调用（一次 `link get` / `neigh get`）的硬时限：半死连接、内核
/// 没回这条 dump 时，超时即丢弃这条请求，下一个查询不会被它堵在后面。
const NETLINK_CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// 调用侧等 worker 回话的时限。略大于 [`NETLINK_CALL_TIMEOUT`]：worker 自己超时把 `None`
/// 送回来后，调用侧还有余量在时限内收下并退回 `ip` 命令兜底，不会无谓地触发二次 panic。
const NETLINK_RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// 经专用线程发出的查询请求；应答用 std `mpsc` 送回，避免 async 上下文里 `blocking_recv` 的
/// panic（见模块头）。
enum Req {
    LinkIsUp {
        dev: String,
        resp: std_mpsc::Sender<Option<bool>>,
    },
    GatewayMac {
        ip: String,
        resp: std_mpsc::Sender<Option<String>>,
    },
}

/// 全局请求发送端（专用线程持有接收端）。
///
/// 第一次用到时拉起那个后台线程；之后所有调用共用同一条连接，不再反复 new_connection。
fn sender() -> Option<mpsc::Sender<Req>> {
    static S: OnceLock<Mutex<Option<mpsc::Sender<Req>>>> = OnceLock::new();
    let cell = S.get_or_init(|| Mutex::new(None));
    let mut g = cell.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        let (tx, mut rx) = mpsc::channel::<Req>(8);
        std::thread::Builder::new()
            .name("netsense-netlink".into())
            .spawn(move || drive_netlink(&mut rx))
            .ok()?;
        *g = Some(tx);
    }
    g.clone()
}

/// 后台线程主体：在当前线程 tokio 运行时里，常驻驱动一条 netlink 连接。
///
/// 连接一旦断开（`driver` future 结束）就重连；请求逐个串行处理，绝不让两条查询共用
/// 同一个 socket 的应答流。
fn drive_netlink(rx: &mut mpsc::Receiver<Req>) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return, // 起不来就彻底退回 ip 命令，调用方会自己 fallback
    };
    rt.block_on(async {
        loop {
            // 三元组：连接本体、请求句柄、以及「 unsolicited 消息」接收端。第三个必须
            // 一直握在手上 —— 它由 netlink-proto 从 socket 上读出来喂的，丢掉它等于
            // 没人再消费 socket 上的事件，连接很快就断（症状是重连循环空转）。
            let (conn, handle, _events) = match rtnetlink::new_connection() {
                Ok(c) => c,
                // 建连失败（极端：netlink 不可用）：等一会重试，期间调用方各自退回 ip 命令。
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
            let mut driver = tokio::spawn(conn);
            let worker = async {
                while let Some(req) = rx.recv().await {
                    match req {
                        Req::LinkIsUp { dev, resp } => {
                            let _ = resp.send(link_is_up_async(&handle, &dev).await);
                        }
                        Req::GatewayMac { ip, resp } => {
                            let _ = resp.send(gateway_mac_async(&handle, &ip).await);
                        }
                    }
                }
            };
            // driver 先结束（连接断了）就重连；worker 结束只在发送端全被丢弃时发生，
            // 但发送端是静态常驻的，正常不会走到——真走到了同样重连（静默恢复）。
            tokio::select! {
                _ = &mut driver => {}
                _ = worker => {}
            }
            driver.abort();
        }
    });
}

async fn link_is_up_async(handle: &Handle, dev: &str) -> Option<bool> {
    let fut = async {
        let mut links = handle.link().get().match_name(dev).execute();
        let msg = links.try_next().await.ok().flatten()?;
        let flags: LinkFlags = msg.header.flags;
        Some(flags.contains(LinkFlags::Up) && flags.contains(LinkFlags::LowerUp))
    };
    // 半死连接 / 内核没回这条 dump 时，超时丢弃，绝不把整条读路径挂住。
    tokio::time::timeout(NETLINK_CALL_TIMEOUT, fut)
        .await
        .ok()
        .flatten()
}

/// 按名查链路是否就绪（`UP && LOWER_UP`）。对应 `ip link show <dev>` 尖括号里的标志位。
///
/// WireGuard 接口正常工作时 `state` 打印 UNKNOWN，但 `LOWER_UP` 是置位的，所以必须看
/// 标志位而非 state 字段（见 linux.rs 的 `ip_link_ready` 注释）。
pub fn link_is_up(dev: &str) -> Option<bool> {
    let tx = sender()?;
    let (r_tx, r_rx) = std_mpsc::channel();
    tx.try_send(Req::LinkIsUp {
        dev: dev.to_string(),
        resp: r_tx,
    })
    .ok()?;
    // std 的 recv_timeout 没有 async 上下文断言：从 Tauri async 命令里调用也不会 panic。
    r_rx.recv_timeout(NETLINK_RECV_TIMEOUT).ok().flatten()
}

async fn gateway_mac_async(handle: &Handle, ip: &str) -> Option<String> {
    let target: IpAddr = ip.parse().ok()?;
    let fut = async {
        let mut neigh = handle.neighbours().get().execute();
        while let Some(msg) = neigh.try_next().await.ok().flatten() {
            let mut ip_seen = None;
            let mut mac = None;
            for nla in msg.attributes.into_iter() {
                match nla {
                    NeighbourAttribute::Destination(NeighbourAddress::Inet(a)) => {
                        ip_seen = Some(IpAddr::V4(a))
                    }
                    NeighbourAttribute::Destination(NeighbourAddress::Inet6(a)) => {
                        ip_seen = Some(IpAddr::V6(a))
                    }
                    NeighbourAttribute::LinkLayerAddress(b) if b.len() == 6 => mac = Some(b),
                    _ => {}
                }
            }
            if ip_seen == Some(target) {
                if let Some(b) = mac {
                    return Some(format!(
                        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                        b[0], b[1], b[2], b[3], b[4], b[5]
                    ));
                }
            }
        }
        None
    };
    tokio::time::timeout(NETLINK_CALL_TIMEOUT, fut)
        .await
        .ok()
        .flatten()
}

/// 查某 IP 在邻居表里的 MAC（对应 `ip neigh show <ip>`）。查不到 / netlink 不可用返回 `None`。
pub fn gateway_mac(ip: &str) -> Option<String> {
    let tx = sender()?;
    let (r_tx, r_rx) = std_mpsc::channel();
    tx.try_send(Req::GatewayMac {
        ip: ip.to_string(),
        resp: r_tx,
    })
    .ok()?;
    r_rx.recv_timeout(NETLINK_RECV_TIMEOUT).ok().flatten()
}
