#!/usr/bin/env bash
# 沙箱内一次性工具链引导 + 交叉编译验证（Windows 主机、无 MSVC）。
#
# 关键约束（为什么这么写）：
#   * 本机 C: 盘仅剩 ~164MB，绝不能写入 —— 所有 RUSTUP_HOME/CARGO_HOME/TEMP 显式重定向到 D:。
#   * 本机没有 MSVC（VS Build Tools 未安装），故选择 x86_64-pc-windows-gnu 目标，
#     复用系统已有的 TDM-GCC (x86_64-w64-mingw32) 作为链接器/资源编译器。
#   * crates 走 rsproxy.cn 稀疏索引镜像，避免 crates.io 直连过慢。
#   * 所有传给**原生 exe**（curl / rustup / cargo）的路径必须用 Windows 风格
#     （D:/xxx），MSYS 的 /d/xxx 形式它们不认。
set -euo pipefail

WIN_BASE='D:/netsense-build'
WIN_TARGET='E:/netsense-build/target'
export RUSTUP_HOME="$WIN_BASE/rustup"
export CARGO_HOME="$WIN_BASE/cargo"
export TMP="$WIN_BASE/tmp"
export TEMP="$WIN_BASE/tmp"
export TMPDIR="$WIN_BASE/tmp"
export CARGO_TARGET_DIR="$WIN_TARGET"
export PATH="/d/TDM-GCC-64/bin:/d/netsense-build/cargo/bin:$PATH"

echo "==> 准备目录"
mkdir -p /d/netsense-build/{rustup,cargo,tmp} /e/netsense-build/target

if [ ! -x /d/netsense-build/cargo/bin/cargo.exe ]; then
  echo "==> 下载 rustup-init"
  curl -fL --retry 3 -o "$WIN_BASE/rustup-init.exe" \
    https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe
  echo "==> 安装 rust 工具链（gnu 目标，minimal profile）"
  /d/netsense-build/rustup-init.exe -y --profile minimal \
    --default-host x86_64-pc-windows-gnu \
    --default-toolchain stable-x86_64-pc-windows-gnu \
    --no-modify-path
fi

echo "==> cargo 镜像配置"
cat > /d/netsense-build/cargo/config.toml <<'TOML'
[source.crates-io]
replace-with = 'rsproxy-sparse'

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
git-fetch-with-cli = true
retry = 3
TOML

echo "==> 版本确认"
CARGO=/d/netsense-build/cargo/bin/cargo.exe
"$CARGO" --version
/d/netsense-build/cargo/bin/rustc.exe --version
gcc --version | head -1
windres --version | head -1

echo "==> cargo check（x86_64-pc-windows-gnu）"
cd /d/QS.Zhang/Documents/GitHub/netsense/src-tauri
set +e
"$CARGO" check --target x86_64-pc-windows-gnu 2>&1 | tail -100
echo "CHECK_EXIT=${PIPESTATUS[0]}"
