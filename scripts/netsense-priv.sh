#!/bin/sh
# NetSense 特权操作包装脚本
# ---------------------------------------------------------------------------
# 设计约束（安全边界，勿放宽）：
#   1. 本脚本必须由 root 拥有、位于 root 属主目录（默认 /usr/local/libexec），
#      mode 0755；普通用户不可写。
#   2. 仅接受下面 case 分支中列出的白名单子命令；**不接受任何任意 shell 透传**。
#   3. 每个参数都做形状校验；以 '-' 开头的参数一律拒绝（防选项注入）。
#   4. 由 /etc/sudoers.d/netsense 授权为 NOPASSWD，安装见 install-priv-helper.sh。
#
# 用法：
#   netsense-priv.sh --batch          # 从 stdin 逐行读操作（GUI 走这条）
#   netsense-priv.sh <op> [args...]   # 单条操作（便于人工排查）
#   netsense-priv.sh tunowner <dev>   # 只读查询：打印持有该隧道控制套接字的进程可执行路径
#
# 操作行格式（字段以 '|' 分隔）：
#   setdhcp|Wi-Fi
#   setmanual|Wi-Fi|192.168.1.100|255.255.255.0|192.168.1.1
#   setdns|Wi-Fi|192.168.1.1,8.8.8.8        (空串=清空为 Empty)
#   setv6off|Wi-Fi / setv6auto|Wi-Fi
#   setv6manual|Wi-Fi|fe80::1|64|fe80::ff
#   routeadd|10.0.0.0/8|192.168.1.1|0       (第4字段为 metric，可空)
#   routedel|10.0.0.0/8
#
# 只读查询（tunowner）不进 --batch：那一条路只回 "OK"，答案在 stdout 里会被丢掉。
# 它同样按白名单分支走，参数只有「接口名」这一种形状，且不做任何写操作。
# ---------------------------------------------------------------------------

set -eu
set -f                      # 关闭 glob，避免字段中的 * ? 被展开
PATH=/usr/sbin:/sbin:/usr/bin:/bin
export PATH

die() { echo "ERR $*" >&2; exit 1; }
ok()  { echo "OK"; }

# ---------- 校验函数 ----------

# 服务名（Wi-Fi / AirPort）：非空、不以 '-' 开头、不含 '|' 与换行
is_service() {
    [ -n "$1" ] || return 1
    case "$1" in
        -*) return 1 ;;
        *'|'*) return 1 ;;
    esac
    return 0
}

# 宽松 IPv6：只需含 ':' 且字符集合法，避免手写解析器出错
is_ipv6() {
    [ -n "$1" ] || return 1
    case "$1" in
        *:*) ;;
        *) return 1 ;;
    esac
    case "$1" in
        *[!0-9a-fA-F:.]*) return 1 ;;
    esac
    return 0
}

# IPv4：a.b.c.d，每段 0-255
# 注意：POSIX sh 没有局部变量，下面保存 IFS 的变量名必须与该脚本其它函数互不重复，
#      否则嵌套调用（is_dns_list → is_ipv4）会互相覆盖，导致 IFS 泄漏给调用方。
is_ipv4() {
    [ -n "$1" ] || return 1
    case "$1" in
        *[!0-9.]*) return 1 ;;
    esac
    _iv4_ifs=$IFS
    IFS=.
    # shellcheck disable=SC2086
    set -- $1
    IFS=$_iv4_ifs
    [ $# -eq 4 ] || return 1
    for _o in "$@"; do
        [ -n "$_o" ] || return 1
        case "$_o" in
            *[!0-9]*) return 1 ;;
        esac
        [ "$_o" -ge 0 ] 2>/dev/null || return 1
        [ "$_o" -le 255 ] 2>/dev/null || return 1
        # 拒绝 010 这类前导零写法（易被误读为八进制）
        case "$_o" in
            0) ;;
            0*) return 1 ;;
        esac
    done
    return 0
}

# 掩码：合法 IPv4（不做连续性校验，networksetup 自身会拒绝非法值）
is_netmask() { is_ipv4 "$1"; }

# 前缀长度：0-128 十进制
is_prefix() {
    [ -n "$1" ] || return 1
    case "$1" in
        *[!0-9]*) return 1 ;;
    esac
    [ "$1" -le 128 ] 2>/dev/null || return 1
    return 0
}

# 路由目标：a.b.c.d 或 a.b.c.d/len（len 0-32）
is_route_dest() {
    [ -n "$1" ] || return 1
    case "$1" in
        */*)
            _addr=${1%%/*}
            _len=${1#*/}
            is_ipv4 "$_addr" || return 1
            [ -n "$_len" ] || return 1
            case "$_len" in *[!0-9]*) return 1 ;; esac
            [ "$_len" -le 32 ] 2>/dev/null || return 1
            ;;
        *)
            is_ipv4 "$1" || return 1
            ;;
    esac
    return 0
}

# metric：空 或 0-4294967295
is_metric() {
    [ -z "$1" ] && return 0
    case "$1" in
        *[!0-9]*) return 1 ;;
    esac
    return 0
}

# DNS 列表：逗号分隔的 IPv4 集合；允许空（表示清空）
is_dns_list() {
    [ -z "$1" ] && return 0
    _dns_ifs=$IFS
    IFS=,
    # shellcheck disable=SC2086
    set -- $1
    IFS=$_dns_ifs
    for _s in "$@"; do
        [ -n "$_s" ] || return 1
        is_ipv4 "$_s" || return 1
    done
    return 0
}

# 接口名（只读查询用）：1~16 个字母数字，且不以 '-' 开头。
# 它会被拼进 root 去 stat 的那一个路径，所以这里比服务名那条更严：'/'、'.'、空白、引号
# 一律不收 —— `..`、绝对路径、别人的套接字都进不来，能拼出的只有下面那一个约定路径。
is_iface() {
    [ -n "$1" ] || return 1
    [ ${#1} -le 16 ] || return 1
    case "$1" in
        -*) return 1 ;;
        *[!0-9a-zA-Z]*) return 1 ;;
    esac
    return 0
}

# 执行前统一断言：服务名合法
need_service() { is_service "$1" || die "bad service: $1"; }

# ---------- 单条操作执行 ----------

exec_op() {
    _op=$1
    shift
    case "$_op" in
        setdhcp)
            need_service "$1"; [ $# -eq 1 ] || die "setdhcp arg count"
            networksetup -setdhcp "$1"
            ;;
        setmanual)
            need_service "$1"; [ $# -eq 4 ] || die "setmanual arg count"
            is_ipv4 "$2"   || die "bad ip: $2"
            is_netmask "$3" || die "bad netmask: $3"
            is_ipv4 "$4"   || die "bad gateway: $4"
            networksetup -setmanual "$1" "$2" "$3" "$4"
            ;;
        setdns)
            need_service "$1"; [ $# -eq 2 ] || die "setdns arg count"
            is_dns_list "$2" || die "bad dns: $2"
            if [ -z "$2" ]; then
                networksetup -setdnsservers "$1" Empty
            else
                _setdns_ifs=$IFS
                IFS=,
                # shellcheck disable=SC2086
                set -- "$1" $2
                IFS=$_setdns_ifs
                _svc=$1; shift
                networksetup -setdnsservers "$_svc" "$@"
            fi
            ;;
        setv6off)
            need_service "$1"; [ $# -eq 1 ] || die "setv6off arg count"
            networksetup -setv6off "$1"
            ;;
        setv6auto)
            need_service "$1"; [ $# -eq 1 ] || die "setv6auto arg count"
            networksetup -setv6automatic "$1"
            ;;
        setv6manual)
            need_service "$1"; [ $# -eq 4 ] || die "setv6manual arg count"
            is_ipv6 "$2"   || die "bad ipv6: $2"
            is_prefix "$3" || die "bad prefix: $3"
            is_ipv6 "$4"   || die "bad v6 gateway: $4"
            networksetup -setv6manual "$1" "$2" "$3" "$4"
            ;;
        routeadd)
            [ $# -eq 3 ] || die "routeadd arg count"
            is_route_dest "$1" || die "bad dest: $1"
            is_ipv4 "$2"       || die "bad gw: $2"
            is_metric "$3"     || die "bad metric: $3"
            if [ -n "$3" ] && [ "$3" != "0" ]; then
                route -n add -net "$1" "$2" -metrics "$3"
            else
                route -n add -net "$1" "$2"
            fi
            ;;
        routedel)
            [ $# -eq 1 ] || die "routedel arg count"
            is_route_dest "$1" || die "bad dest: $1"
            route -n delete -net "$1"
            ;;
        *)
            die "unknown op: $_op"
            ;;
    esac
}

# 一行为一条操作：拆分 '|' 后交给 exec_op
run_line() {
    _line=$1
    [ -n "$_line" ] || return 0
    case "$_line" in
        '#'*) return 0 ;;   # 允许注释行
    esac
    _rl_ifs=$IFS
    IFS='|'
    # shellcheck disable=SC2086
    set -- $_line
    IFS=$_rl_ifs
    exec_op "$@"
}

# ---------- 只读查询 ----------
#
# 查询不改任何系统状态：只 stat 一个约定路径下的套接字、只读进程表。它单独走一支入口，
# 因为答案就是 stdout 本身（写操作那一支只回 "OK"，塞不进回答）。
#
# tunowner <dev>：打印「正握着 wireguard-go 为 <dev> 留的那枚控制套接字」的进程可执行
# 路径。没有套接字、没有持有者、拿不到进程名 —— 一律非 0 退出：调用方对这条只有一个
# 要求，问不出答案时不许猜。
query_op() {
    _qop=$1
    shift
    case "$_qop" in
        tunowner)
            [ $# -eq 1 ] || die "tunowner arg count"
            is_iface "$1" || die "bad iface: $1"
            _sock=/var/run/wireguard/"$1".sock
            [ -S "$_sock" ] || die "no socket for $1"
            # -t 只要 pid。持有者可能不止一个（fork 出它的父进程也握着同一个 fd），取第一个
            # 就够：这里要的是「哪个软件」，不是「几个进程」。
            _pid=$(lsof -t "$_sock" 2>/dev/null | head -n 1)
            [ -n "$_pid" ] || die "no holder for $1"
            # 这一串要进 `ps -p`，所以照第 3 条约束防一手选项注入：只收数字。
            case "$_pid" in *[!0-9]*) die "bad pid for $1" ;; esac
            # macOS 的 comm 给的是可执行文件全路径；进程刚好退出时这里会拿不到。
            _comm=$(ps -o comm= -p "$_pid" 2>/dev/null) || die "no process $_pid"
            [ -n "$_comm" ] || die "empty comm for $_pid"
            echo "$_comm"
            ;;
        *)
            die "unknown query: $_qop"
            ;;
    esac
}

# ---------- 入口 ----------

# 只读查询：白名单里的另一支，答案就是 stdout，因此不走末尾那句 ok。
if [ "${1:-}" = "tunowner" ]; then
    query_op "$@"
    exit 0
fi

if [ "${1:-}" = "--batch" ]; then
    [ $# -eq 1 ] || die "--batch takes no args"
    _fail=0
    while IFS= read -r _line; do
        if ! run_line "$_line" >/dev/null 2>&1; then
            echo "ERR failed: $_line" >&2
            _fail=1
        fi
    done
    [ "$_fail" -eq 0 ] || exit 1
    ok
    exit 0
fi

[ $# -ge 1 ] || die "usage: netsense-priv.sh --batch | tunowner <dev> | <op> [args...]"
exec_op "$@"
ok
