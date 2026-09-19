#!/usr/bin/env bash
# 从仓库根目录的 VERSION 文件同步版本号到 tauri.conf.json 与 Cargo.toml。
# 对标 wireguideplus 的 tools/bumpversion：VERSION 是唯一真值，避免多文件版本串漂移。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VER="$(tr -d '[:space:]' < "$ROOT/VERSION")"
[ -z "$VER" ] && { echo "ERROR: VERSION 为空" >&2; exit 1; }
echo "bump version -> $VER"

# tauri.conf.json（JSON，保序重写）
python - "$ROOT/src-tauri/tauri.conf.json" "$VER" <<'PY'
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
python - "$ROOT/src-tauri/Cargo.toml" "$VER" <<'PY'
import re, sys
path, ver = sys.argv[1], sys.argv[2]
src = open(path, encoding="utf-8").read()
# 只替换第一个出现的 version = "..."（即 [package] 段）
src = re.sub(r'(?m)^version = "([^"]*)"', f'version = "{ver}"', src, count=1)
open(path, "w", encoding="utf-8").write(src)
PY

echo "synced: tauri.conf.json + Cargo.toml -> $VER"
