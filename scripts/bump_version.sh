#!/usr/bin/env bash
# 从仓库根目录的 VERSION 文件同步版本号到 tauri.conf.json 与 Cargo.toml。
# 对标 wireguideplus 的 tools/bumpversion：VERSION 是唯一真值，避免多文件版本串漂移。
#
# ⚠️ Windows / Git Bash 注意：必须先把工作目录切到 $ROOT，再用**相对路径**调用 python。
# `cd ... && pwd` 在 Git Bash 里给出的是 MSYS 风格路径（形如 /d/<仓库目录>），而这里调用的是
# **原生 Windows python**，它不认这种路径 —— 会直接抛
#   FileNotFoundError: '/d/<仓库目录>/src-tauri/tauri.conf.json'
# 相对路径不经过 MSYS 的参数转换，MSYS 与原生 Windows 两边都能用。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VER="$(tr -d '[:space:]' < "$ROOT/VERSION")"
[ -z "$VER" ] && { echo "ERROR: VERSION 为空" >&2; exit 1; }
echo "bump version -> $VER"

cd "$ROOT"

# macOS/Linux 上常常只有 python3；Windows 的官方安装包只注册 python。
PY="$(command -v python3 || command -v python || true)"
[ -z "$PY" ] && { echo "ERROR: 找不到 python3 / python" >&2; exit 1; }

# tauri.conf.json（JSON，保序重写）
"$PY" - "src-tauri/tauri.conf.json" "$VER" <<'PY'
import json, sys
path, ver = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as f:
    data = json.load(f)
data["version"] = ver
with open(path, "w", encoding="utf-8") as f:
    json.dump(data, f, indent=2, ensure_ascii=False)
    f.write("\n")
PY

# Cargo.toml（仅改 [package] 段首行的 version）
"$PY" - "src-tauri/Cargo.toml" "$VER" <<'PY'
import re, sys
path, ver = sys.argv[1], sys.argv[2]
src = open(path, encoding="utf-8").read()
# 只替换第一个出现的 version = "..."（即 [package] 段）
src = re.sub(r'(?m)^version = "([^"]*)"', f'version = "{ver}"', src, count=1)
open(path, "w", encoding="utf-8").write(src)
PY

echo "synced: tauri.conf.json + Cargo.toml -> $VER"
