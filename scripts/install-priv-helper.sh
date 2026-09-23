#!/bin/sh
# 安装 / 卸载 NetSense 特权通道
# ---------------------------------------------------------------------------
# 作用：把 netsense-priv.sh 安装为 root 属主、并把当前用户加入
#      /etc/sudoers.d/netsense 的 NOPASSWD 白名单（仅限该脚本）。
#      安装后 GUI 改网络不再每次弹授权框。
#
# 用法：
#   sh scripts/install-priv-helper.sh            # 安装
#   sh scripts/install-priv-helper.sh uninstall  # 卸载（移除脚本与 sudoers 规则）
#
# 安全说明：脚本本身是白名单包装器（见 netsense-priv.sh 头部），
#          sudoers 只授权该脚本的绝对路径，不授予用户任意 root 命令。
# ---------------------------------------------------------------------------
set -eu

TARGET_DIR=/usr/local/libexec
SCRIPT_NAME=netsense-priv.sh
SUDOERS_FILE=/etc/sudoers.d/netsense
REQUIRED_USER="${NETSENSE_USER:-$(id -un)}"

DIR=$(cd "$(dirname "$0")" && pwd)

need_root() {
    if [ "$(id -u)" -eq 0 ]; then
        SUDO=""
    elif command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
    else
        echo "需要 root 权限，且未找到 sudo；请以 root 运行本脚本。" >&2
        exit 1
    fi
}

case "${1:-install}" in
    install)
        need_root
        echo "==> 安装特权包装脚本到 $TARGET_DIR/$SCRIPT_NAME"
        $SUDO install -d -o root -g wheel -m 0755 "$TARGET_DIR"
        $SUDO install -o root -g wheel -m 0755 "$DIR/$SCRIPT_NAME" "$TARGET_DIR/$SCRIPT_NAME"

        echo "==> 写入 sudoers 规则：$REQUIRED_USER NOPASSWD: $TARGET_DIR/$SCRIPT_NAME"
        _tmp=$(mktemp)
        printf '%s ALL=(root) NOPASSWD: %s/%s\n' \
            "$REQUIRED_USER" "$TARGET_DIR" "$SCRIPT_NAME" > "$_tmp"
        $SUDO install -o root -g wheel -m 0440 "$_tmp" "$SUDOERS_FILE"
        rm -f "$_tmp"

        echo "==> 校验 sudoers 语法"
        $SUDO visudo -cf "$SUDOERS_FILE"

        echo "==> 自检：以 $REQUIRED_USER 身份测试免密通道"
        if [ "$(id -u)" -eq 0 ]; then
            "$TARGET_DIR/$SCRIPT_NAME" setdhcp Nothing >/dev/null 2>&1 || true
            echo "    （当前为 root，跳过 sudo -n 自检）"
        elif sudo -n "$TARGET_DIR/$SCRIPT_NAME" --batch </dev/null >/dev/null 2>&1; then
            echo "    OK：免密通道可用"
        else
            echo "    警告：sudo -n 自检未通过，请确认 $REQUIRED_USER 有 sudo 权限。" >&2
        fi

        echo
        echo "完成。NetSense 将优先走免密通道，失败时自动回落系统授权框。"
        echo "提示：目前脚本位于 $TARGET_DIR，已限制白名单操作；如需撤销运行："
        echo "      sh scripts/install-priv-helper.sh uninstall"
        ;;

    uninstall)
        need_root
        echo "==> 移除 sudoers 规则 $SUDOERS_FILE"
        $SUDO rm -f "$SUDOERS_FILE"
        echo "==> 移除包装脚本 $TARGET_DIR/$SCRIPT_NAME"
        $SUDO rm -f "$TARGET_DIR/$SCRIPT_NAME"
        echo "完成。已回退为系统授权框模式。"
        ;;

    *)
        echo "用法: sh install-priv-helper.sh [install|uninstall]" >&2
        exit 2
        ;;
esac
