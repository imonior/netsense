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
#
# 操作行格式（字段以 '|' 分隔）：
#   setdhcp|Wi-Fi
#   setmanual|Wi-Fi|192.168.1.100|255.255.255.0|192.168.1.1
#   setdns|Wi-Fi|192.168.1.1,8.8.8.8        (空串=清空为 Empty)
#   setv6off|Wi-Fi / setv6auto|Wi-Fi
#   setv6manual|Wi-Fi|fe80::1|64|fe80::ff
#   routeadd|10.0.0.0/8|192.168.1.1|0       (第4字段为 metric，可空)
#   routedel|10.0.0.0/8
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

# ---------- 入口 ----------

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

[ $# -ge 1 ] || die "usage: netsense-priv.sh --batch | <op> [args...]"
exec_op "$@"
ok
