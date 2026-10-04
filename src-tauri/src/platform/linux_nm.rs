//! NetworkManager D-Bus 原生化（`cfg(target_os = "linux")` 才编译）。
//!
//! 目标：替掉 `linux.rs` 里反复 spawn 的 `nmcli` 子进程——这是 Linux 面板冷加载慢的根因
//! （每次开面板都要起十几个 `nmcli` 进程，每个再各自连一次 NM 守护进程）。直连 NM 的
//! D-Bus API 后，所有读取合成少数几次 D-Bus 往返，写入走 `Settings.Connection.Update`
//! + `NetworkManager.ActivateConnection`，彻底不再经由 `nmcli con mod`。
//!
//! 安全网：本模块每个公开函数都返回 `Option`/`Result`，调用方一律 `or_else(nmcli 兜底)`。
//! 因此即便某个 D-Bus 属性名/签名在这台发行版上不符预期，行为也只是回落到既有的 `nmcli`
//! 实现——功能不退化，只损失原生加速。真正的校验要靠 Linux CI / 真机。

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::sync::OnceLock;

use zbus::blocking::{Connection, Proxy};
use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::platform::MAX_ROUTES_PER_IFACE;

/// 把任意可序列化为 `Value` 的 owned 值转成 `OwnedValue`。
///
/// 注意：`OwnedValue` 没有 `From<String>` / `From<Vec<..>>` / `From<Value>` 的直接实现，
/// 只有 `TryFrom<&Value>`。所以先 `Into<Value<'static>>` 再 `try_from(&value)`（该转换恒成功）。
/// `&str` 等非 `'static` 借用不能直接进 `OwnedValue`，调用方需先 `.to_string()`。
fn ov<V: Into<Value<'static>>>(v: V) -> OwnedValue {
    let val: Value<'static> = v.into();
    OwnedValue::try_from(&val).expect("Value -> OwnedValue 不会失败") // i18n-exempt: 内部断言，不是界面文案
}

const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const DEV_IFACE: &str = "org.freedesktop.NetworkManager.Device";
const IP4_IFACE: &str = "org.freedesktop.NetworkManager.IP4Config";
const IP6_IFACE: &str = "org.freedesktop.NetworkManager.IP6Config";
const SETTINGS_IFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";

/// 一条设备读到的网络配置（原生 D-Bus 或 `nmcli` 兜底都归一成这个结构）。
#[derive(Default, Clone)]
pub struct DeviceIp {
    pub ipv4: Option<String>,
    pub netmask: Option<String>,
    pub ipv6: Option<String>,
    pub gateway: Option<String>,
    pub gateway6: Option<String>,
    pub dns: Option<String>,
    pub mac: Option<String>,
    /// 目的前缀清单（`dst/prefix`），已剥掉多播、去重、限长。
    pub routes: Vec<String>,
}

/// 进程级复用的 system bus 连接。没有 system bus（容器等）就返回 `None`，调用方回落 `nmcli`。
fn sys_conn() -> Option<Connection> {
    static C: OnceLock<Option<Connection>> = OnceLock::new();
    C.get_or_init(|| Connection::system().ok()).clone()
}

/// `OwnedValue` → `String` 的安全取出。
fn str_of(v: &OwnedValue) -> Option<String> {
    v.downcast_ref::<String>().ok()
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

// DNS 读取见 `read_ip4`：优先 `NameserverData`（`aa{sv}`，字符串地址，无字节序问题），
// 老 NM 回落 `Nameservers`（`au`）时交给 cfg 无关的 `decode_nm_nameservers` 还原。
// 总线上的 `u32` 就是 `in_addr.s_addr` 的 4 字节：内存布局即网络序八位组（1.2.3.4 → [01 02 03 04]），
// 小端机读成数值后反序（0x04030201），必须用 `to_ne_bytes()` 还原——见 `decode_nm_nameservers` 注释。

/// 读单个设备的 IPv4/IPv6 配置，原生 D-Bus。失败返回 `None`（调用方回落 `nmcli`）。
pub fn device_ip(dev: &str) -> Option<DeviceIp> {
    let conn = sys_conn()?;
    let nm = Proxy::new(&conn, NM_SERVICE, NM_PATH, NM_IFACE).ok()?;

    // 设备清单 → 按 Interface 名定位目标设备。
    let devs: Vec<OwnedObjectPath> = nm.get_property("Devices").ok()?;
    let mut dev_proxy = None;
    for p in &devs {
        let d = match Proxy::new(&conn, NM_SERVICE, p.as_str(), DEV_IFACE) {
            Ok(d) => d,
            // 单个设备代理取不到（VPN / 临时 veth 等常见）不该拖累整次原生读，
            // 跳过它，调用方会对这台设备回落 nmcli。
            Err(_) => continue,
        };
        let iface = match d.get_property::<String>("Interface") {
            Ok(i) => i,
            Err(_) => continue,
        };
        if iface == dev {
            dev_proxy = Some(d);
            break;
        }
    }
    let d = dev_proxy?;

    let mut out = DeviceIp::default();

    // 硬件地址。
    if let Ok(hw) = d.get_property::<String>("HwAddress") {
        out.mac = if hw.is_empty() { None } else { Some(hw) };
    }

    // IPv4。
    if let Ok(ip4p) = d.get_property::<OwnedObjectPath>("Ip4Config") {
        if ip4p.as_str() != "/" {
            if let Some(ip4) = read_ip4(&conn, ip4p.as_str()) {
                out.ipv4 = ip4.ipv4;
                out.netmask = ip4.netmask;
                out.gateway = ip4.gateway;
                out.dns = ip4.dns;
                out.routes = ip4.routes;
            }
        }
    }

    // IPv6。
    if let Ok(ip6p) = d.get_property::<OwnedObjectPath>("Ip6Config") {
        if ip6p.as_str() != "/" {
            if let Some(ip6) = read_ip6(&conn, ip6p.as_str()) {
                out.ipv6 = ip6.ipv6;
                out.gateway6 = ip6.gateway6;
            }
        }
    }

    Some(out)
}

struct Ip4Parts {
    ipv4: Option<String>,
    netmask: Option<String>,
    gateway: Option<String>,
    dns: Option<String>,
    routes: Vec<String>,
}

fn read_ip4(conn: &Connection, path: &str) -> Option<Ip4Parts> {
    let ip = Proxy::new(conn, NM_SERVICE, path, IP4_IFACE).ok()?;
    let mut out = Ip4Parts {
        ipv4: None,
        netmask: None,
        gateway: None,
        dns: None,
        routes: Vec::new(),
    };

    // `AddressData`（`aa{sv}`）比老式 `Addresses`（`aau`）安全：地址直接是字符串，无需处理字节序。
    if let Ok(addrs) = ip.get_property::<Vec<HashMap<String, OwnedValue>>>("AddressData") {
        if let Some(first) = addrs.first() {
            let addr = first.get("address").and_then(str_of);
            let prefix = first
                .get("prefix")
                .and_then(|v| v.downcast_ref::<u32>().ok());
            if let (Some(a), Some(p)) = (addr, prefix) {
                out.ipv4 = Some(a);
                out.netmask = prefix_to_mask(p);
            }
        }
    }

    if let Ok(gw) = ip.get_property::<String>("Gateway") {
        out.gateway = if gw.is_empty() { None } else { Some(gw) };
    }

    // DNS：优先 `NameserverData`（`aa{sv}`，`address` 为字符串，无字节序问题）；
    // 老 NM 没有该属性时回落 `Nameservers`（`au`）。总线 `u32` 即 `in_addr.s_addr`
    // （内存 4 字节 = 网络序八位组，小端数值反序，如 1.2.3.4 → 0x04030201），
    // 交给 cfg 无关的 `decode_nm_nameservers` 用 `to_ne_bytes()` 还原——切勿直接用
    // `Ipv4Addr::from(n)`，否则小端机得到 4.3.2.1 这类反序地址（旧实现即此 bug）。
    if let Ok(list) = ip.get_property::<Vec<HashMap<String, OwnedValue>>>("NameserverData") {
        let parts: Vec<String> = list
            .iter()
            .filter_map(|m| m.get("address").and_then(str_of))
            .filter(|s| !s.is_empty())
            .collect();
        if !parts.is_empty() {
            out.dns = Some(parts.join(","));
        }
    }
    if out.dns.is_none() {
        if let Ok(ns) = ip.get_property::<Vec<u32>>("Nameservers") {
            if !ns.is_empty() {
                out.dns = Some(crate::platform::decode_nm_nameservers(&ns).join(","));
            }
        }
    }

    // 路由：`RouteData`（`aa{sv}`）每条带 `dest`(s) 与 `prefix`(u)。
    if let Ok(routes) = ip.get_property::<Vec<HashMap<String, OwnedValue>>>("RouteData") {
        for r in &routes {
            let dest = r.get("dest").and_then(str_of);
            let prefix = r
                .get("prefix")
                .and_then(|v| v.downcast_ref::<u32>().ok());
            if let (Some(d), Some(p)) = (dest, prefix) {
                // 剥多播段（224.0.0.0/4 起），与 `linux.rs::route_prefixes` 同判据。
                let first = d.split('.').next().unwrap_or("").parse::<u8>().unwrap_or(0);
                if first >= 224 {
                    continue;
                }
                let pfx = format!("{}/{}", d, p);
                if !out.routes.contains(&pfx) && out.routes.len() < MAX_ROUTES_PER_IFACE {
                    out.routes.push(pfx);
                }
            }
        }
    }

    Some(out)
}

struct Ip6Parts {
    ipv6: Option<String>,
    gateway6: Option<String>,
}

fn read_ip6(conn: &Connection, path: &str) -> Option<Ip6Parts> {
    let ip = Proxy::new(conn, NM_SERVICE, path, IP6_IFACE).ok()?;
    let mut out = Ip6Parts {
        ipv6: None,
        gateway6: None,
    };

    if let Ok(addrs) = ip.get_property::<Vec<HashMap<String, OwnedValue>>>("AddressData") {
        // 跳过 link-local（`fe80:`），取第一条真正的全局地址（与 `linux.rs` 同判据）。
        for a in &addrs {
            if let Some(addr) = a.get("address").and_then(str_of) {
                if !addr.trim().to_ascii_lowercase().starts_with("fe80:") {
                    out.ipv6 = Some(addr.split('/').next().unwrap_or(&addr).trim().to_string());
                    break;
                }
            }
        }
    }

    if let Ok(gw) = ip.get_property::<String>("Gateway") {
        out.gateway6 = if gw.is_empty() || gw == "::" {
            None
        } else {
            Some(gw)
        };
    }

    Some(out)
}

/// 按连接名找它的 D-Bus 对象路径：NM 没有「名字→路径」的直接查询，只能
/// `ListConnections` 后逐条 `GetSettings` 比对 `connection.id`。仅在用户点「应用」时走一次，
/// 成本可接受。
fn connection_path(conn: &Connection, name: &str) -> Option<String> {
    let settings = Proxy::new(conn, NM_SERVICE, "/org/freedesktop/NetworkManager/Settings",
        "org.freedesktop.NetworkManager.Settings").ok()?;
    let conns: Vec<OwnedObjectPath> = settings.get_property("Connections").ok()?;
    for p in &conns {
        let c = match Proxy::new(conn, NM_SERVICE, p.as_str(), SETTINGS_IFACE) {
            Ok(c) => c,
            // 某条连接取不到代理（老/异常连接常见）不该让整次查找失败，跳过它即可，
            // 调用方会对该连接名回落 nmcli。
            Err(_) => continue,
        };
        let cfg: HashMap<String, HashMap<String, OwnedValue>> = match c
            .call_method("GetSettings", &())
            .ok()
            .and_then(|m| m.body().deserialize().ok())
        {
            Some(cfg) => cfg,
            None => continue,
        };
        if let Some(conn_sec) = cfg.get("connection") {
            if conn_sec.get("id").and_then(str_of).as_deref() == Some(name) {
                return Some(p.as_str().to_string());
            }
        }
    }
    None
}

/// 按设备名找它的 D-Bus 对象路径（用于 `ActivateConnection` 指定设备）。
fn device_path(conn: &Connection, name: &str) -> Option<OwnedObjectPath> {
    let nm = Proxy::new(conn, NM_SERVICE, NM_PATH, NM_IFACE).ok()?;
    let devs: Vec<OwnedObjectPath> = nm.get_property("Devices").ok()?;
    for p in &devs {
        let d = Proxy::new(conn, NM_SERVICE, p.as_str(), DEV_IFACE).ok()?;
        if let Ok(iface) = d.get_property::<String>("Interface") {
            if iface == name {
                return Some(p.clone());
            }
        }
    }
    // 找不到就返回根路径，让 NM 自己挑设备。
    OwnedObjectPath::try_from("/").ok()
}

/// 把 `NetworkConfig` 的 IPv4 段写进一个 NM setting 字典（只覆盖 `con_mod_props` 会动的键，
/// 保留用户其它设置，与 `nmcli con mod` 语义一致）。
///
/// ⚠️ 这里**只发 `address-data` / `gateway` / `dns-data`，一个 deprecated 键都不发**。
/// NM 文档对 `addresses` 的原文是：
///
/// > Deprecated in favor of the 'address-data' and 'gateway' properties … **Note that if
/// > you send this property the daemon will ignore 'address-data' and 'gateway'.**
///
/// 也就是说，发 `addresses`（哪怕是空数组）会让 NM 把我们真正想要的 `address-data`
/// **整个忽略**；而 `method=manual` 下空 `addresses` 还会被 NM 直接拒绝
/// （`ipv4.addresses: this property cannot be empty for 'method=manual'`），
/// 于是 `Update` 整体失败 —— 静态 IP 这个核心用例在原生路径上必然走不通。
/// `dns` / `routes` 是同一类 deprecated 键（`dns` 更是「追加」到自动 DNS 而非替换，
/// 发空数组并不能清空什么），一律不碰；清空语义由 `address-data: []` 与
/// `dns-data: []` 自己表达。
fn fill_ipv4(
    ipv4: &mut HashMap<String, OwnedValue>,
    mode: &str,
    addr: Option<&str>,
    prefix: Option<u32>,
    gateway: Option<&str>,
    dns: Option<&str>,
) {
    ipv4.insert("method".into(), ov(mode.to_string()));
    if mode == "manual" {
        let mut entry: HashMap<String, OwnedValue> = HashMap::new();
        if let (Some(a), Some(p)) = (addr, prefix) {
            entry.insert("address".into(), ov(a.to_string()));
            entry.insert("prefix".into(), ov(p));
        }
        ipv4.insert("address-data".into(), ov(vec![entry]));
        ipv4.insert("gateway".into(), ov(gateway.unwrap_or("").to_string()));
    } else {
        // DHCP：清空静态项（`address-data: []`），与 `nmcli con mod ipv4.addresses ""` 等价。
        ipv4.insert(
            "address-data".into(),
            ov(Vec::<HashMap<String, OwnedValue>>::new()),
        );
        ipv4.insert("gateway".into(), ov(String::new()));
    }
    if let Some(d) = dns {
        // 空串 = 清空 DNS（`set_dhcp` 走的就是这条路）；非空则逐个拆成 `dns-data`。
        let list: Vec<String> = d
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        ipv4.insert("dns-data".into(), ov(list));
    }
}

/// 原生应用一份网络配置：读现有 settings → 改 `ipv4`/`ipv6` → `Update` → `ActivateConnection`。
///
/// 返回 `Err` 时调用方必须回落到 `nmcli con mod`（这是用户显式动作，不能静默失败）。
pub fn apply(
    conn_name: &str,
    dev: &str,
    mode: &str,
    ip: Option<&str>,
    prefix: Option<u32>,
    gateway: Option<&str>,
    dns: Option<&str>,
    v6mode: Option<&str>,
    v6addr: Option<&str>,
    v6prefix: Option<u32>,
    v6gateway: Option<&str>,
) -> Result<(), String> {
    let conn = sys_conn().ok_or_else(|| "no system bus".to_string())?;
    let cpath = connection_path(&conn, conn_name)
        .ok_or_else(|| format!("connection '{}' not found on D-Bus", conn_name))?;
    let c = Proxy::new(&conn, NM_SERVICE, cpath.as_str(), SETTINGS_IFACE)
        .map_err(|e| e.to_string())?;

    let mut settings: HashMap<String, HashMap<String, OwnedValue>> = c
        .call_method("GetSettings", &())
        .map_err(|e| e.to_string())?
        .body()
        .deserialize::<HashMap<String, HashMap<String, OwnedValue>>>()
        .map_err(|e| e.to_string())?;

    let mut ipv4 = settings.remove("ipv4").unwrap_or_default();
    fill_ipv4(&mut ipv4, mode, ip, prefix, gateway, dns);
    settings.insert("ipv4".into(), ipv4);

    if let Some(v6) = v6mode {
        let mut ipv6 = settings.remove("ipv6").unwrap_or_default();
        ipv6.insert("method".into(), ov(v6.to_string()));
        if v6 == "manual" {
            let mut entry: HashMap<String, OwnedValue> = HashMap::new();
            if let (Some(a), Some(p)) = (v6addr, v6prefix) {
                entry.insert("address".into(), ov(a.to_string()));
                entry.insert("prefix".into(), ov(p));
            }
            ipv6.insert("address-data".into(), ov(vec![entry]));
            ipv6.insert("gateway".into(), ov(v6gateway.unwrap_or("").to_string()));
        }
        // `auto` / `disabled` 只改 method，**不**清空地址 —— 与 `linux.rs::con_mod_props`
        // （nmcli 兜底路径）逐键一致。这不是「清不干净」的问题：两条路径必须对同一份配置
        // 给出同一个结果，否则哪条生效取决于这台机器有没有 system bus，行为就不可预测了。
        // IPv4 那侧之所以清 `address-data`，是因为 nmcli 侧的 `ipv4.addresses ""` 确实清。
        settings.insert("ipv6".into(), ipv6);
    }

    c.call_method("Update", &settings)
        .map_err(|e| format!("nm Update failed: {}", e))?;

    // 重新激活使配置生效，指定设备（找不到就交给 NM 自选）。
    let nm = Proxy::new(&conn, NM_SERVICE, NM_PATH, NM_IFACE).map_err(|e| e.to_string())?;
    let dpath = device_path(&conn, dev).unwrap_or_else(|| OwnedObjectPath::try_from("/").unwrap());
    let c_obj = ObjectPath::try_from(cpath.as_str()).map_err(|e| e.to_string())?;
    let root = ObjectPath::try_from("/").map_err(|e| e.to_string())?;
    nm.call_method("ActivateConnection", &(c_obj, dpath, root))
        .map_err(|e| format!("nm ActivateConnection failed: {}", e))?;

    Ok(())
}

/// 原生切回 DHCP（设备 → 活动连接 → 改 `ipv4.method=auto` 并清空静态项）。
pub fn set_dhcp(conn_name: &str, dev: &str) -> Result<(), String> {
    let conn = sys_conn().ok_or_else(|| "no system bus".to_string())?;
    let cpath = connection_path(&conn, conn_name)
        .ok_or_else(|| format!("connection '{}' not found on D-Bus", conn_name))?;
    let c = Proxy::new(&conn, NM_SERVICE, cpath.as_str(), SETTINGS_IFACE)
        .map_err(|e| e.to_string())?;

    let mut settings: HashMap<String, HashMap<String, OwnedValue>> = c
        .call_method("GetSettings", &())
        .map_err(|e| e.to_string())?
        .body()
        .deserialize::<HashMap<String, HashMap<String, OwnedValue>>>()
        .map_err(|e| e.to_string())?;

    let mut ipv4 = settings.remove("ipv4").unwrap_or_default();
    // 与 `nmcli con mod ipv4.dns ""` 一致：切回 DHCP 时一并清空静态 DNS。
    fill_ipv4(&mut ipv4, "auto", None, None, None, Some(""));
    settings.insert("ipv4".into(), ipv4);

    c.call_method("Update", &settings)
        .map_err(|e| format!("nm Update failed: {}", e))?;

    let nm = Proxy::new(&conn, NM_SERVICE, NM_PATH, NM_IFACE).map_err(|e| e.to_string())?;
    let dpath = device_path(&conn, dev).unwrap_or_else(|| OwnedObjectPath::try_from("/").unwrap());
    let c_obj = ObjectPath::try_from(cpath.as_str()).map_err(|e| e.to_string())?;
    let root = ObjectPath::try_from("/").map_err(|e| e.to_string())?;
    nm.call_method("ActivateConnection", &(c_obj, dpath, root))
        .map_err(|e| format!("nm ActivateConnection failed: {}", e))?;

    Ok(())
}
