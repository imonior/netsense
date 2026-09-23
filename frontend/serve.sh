#!/usr/bin/env bash
# 开发期静态服务器：在 frontend/ 上起 http.server:1420，供 Tauri devUrl 加载。
# 后续接入 Vite/editor.html 时可替换 beforeDevCommand 为 vite  dev。
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"
exec python3 -m http.server 1420
