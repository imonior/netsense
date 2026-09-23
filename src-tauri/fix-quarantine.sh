#!/bin/bash
# NetSense macOS 隔离属性（quarantine）清理脚本
#
# 适用场景：从 GitHub Releases 直接下载 .dmg 并拖入 /Applications 后，
# 首次打开若被 Gatekeeper 拦截（“无法验证开发者” / “已损坏，无法打开”），
# 运行本脚本即可放行。
#
# 原理：浏览器 / Finder 下载的文件会被打上 com.apple.quarantine 扩展属性，
# 未签名 app 首次打开时 Gatekeeper 就靠它拦截。本脚本仅移除该属性，
# 不改动 app 内容、不移除其它 xattr。
#
# 用法：
#   sudo bash fix-quarantine.sh                 # 默认清理 /Applications/NetSense.app
#   sudo bash fix-quarantine.sh /path/to.app    # 指定路径
#
# 注：通过 Homebrew Cask 安装无需此步骤（brew 用 curl 下载，不打 quarantine 标记）。

set -euo pipefail

APP="${1:-/Applications/NetSense.app}"

if [ ! -d "$APP" ]; then
  echo "未找到 NetSense.app：$APP" >&2
  echo "请先把 NetSense.app 拖到 /Applications，或传入路径：sudo bash $0 /path/to/NetSense.app" >&2
  exit 1
fi

echo "正在移除 $APP 的 quarantine 隔离标记 ..."
sudo xattr -dr com.apple.quarantine "$APP"
echo "完成。现在可以从启动台 / 应用程序正常打开 NetSense 了。"
