<#
.SYNOPSIS
    NetSense Windows 构建脚本（x64）。自动体检环境 → 缺什么告诉你装什么 → 出包。

.DESCRIPTION
    为什么需要这么多前置检查：Tauri 在 Windows 上**必须**有 MSVC 工具链（link.exe）
    与 WebView2 运行时，缺任一项都会在链接阶段报 LNK1104 之类的错，且报错信息不直观。
    本脚本先把体检做完，再决定能不能编译，避免你浪费几十分钟才发现少装东西。

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1            # 出裸 exe（快）
    powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer # 出 msi/nsis 安装包
#>
[CmdletBinding()]
param(
    [switch]$Installer,   # 出安装包（msi + nsis），需要 tauri-cli
    [switch]$Debug        # debug 构建（带控制台窗口，方便看日志）
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$tauriDir = Join-Path $repoRoot 'src-tauri'

function Write-Head($t) { Write-Host "`n=== $t ===" -ForegroundColor Cyan }
function Write-Ok($t)   { Write-Host "  [OK]   $t" -ForegroundColor Green }
function Write-Bad($t)  { Write-Host "  [缺]   $t" -ForegroundColor Red }
function Write-Info($t) { Write-Host "  [提示] $t" -ForegroundColor Yellow }

Write-Head 'NetSense Windows 构建体检'

# ---- 1. Rust ----
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if ($cargo) {
    Write-Ok "Rust: $(& cargo --version)"
} else {
    Write-Bad 'Rust 未安装'
    Write-Info '装法：https://rustup.rs 下载 rustup-init.exe 运行（默认 x86_64-pc-windows-msvc 即可）'
    Write-Info '或：winget install Rustlang.Rustup'
    exit 1
}

$target = (& rustc -vV | Select-String '^host:').ToString().Split(':')[1].Trim()
Write-Ok "默认目标: $target"
if ($target -notlike '*msvc*') {
    Write-Info "当前默认目标是 $target；Tauri 官方只支持 msvc 目标。"
    Write-Info "如构建失败，执行：rustup default stable-x86_64-pc-windows-msvc"
}

# ---- 2. MSVC 链接器（关键） ----
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$hasMsvc = $false
if (Test-Path $vswhere) {
    $vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
    if ($vsPath) {
        $hasMsvc = $true
        Write-Ok "MSVC: $vsPath"
    }
}
if (-not $hasMsvc) {
    Write-Bad 'MSVC C++ 生成工具未安装（Tauri 必需，没有替代方案）'
    Write-Info '安装：下载 “Build Tools for Visual Studio 2022”，勾选「使用 C++ 的桌面开发」'
    Write-Info '  下载页: https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022'
    Write-Info '  或: winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"'
    Write-Info '约需 3–6 GB 磁盘空间。'
    exit 1
}

# ---- 3. Windows SDK ----
$sdk = Get-ChildItem 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Microsoft SDKs\Windows\v10.0' -ErrorAction SilentlyContinue
if ($sdk) { Write-Ok 'Windows SDK 10: 已安装' }
else { Write-Info '未检测到 Windows SDK 10 —— MSVC 工作负载通常会自动带上；若链接报 rc.exe 缺失再单独装' }

# ---- 4. WebView2 运行时 ----
$wvKey = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
$wv = Get-ItemProperty -Path $wvKey -ErrorAction SilentlyContinue
if ($wv) { Write-Ok "WebView2 运行时: $($wv.pv)" }
else {
    Write-Bad 'WebView2 运行时未安装（Win10 1803+ / Win11 通常自带）'
    Write-Info '下载 “Evergreen Bootstrapper”: https://developer.microsoft.com/microsoft-edge/webview2/'
    exit 1
}

# ---- 5. 磁盘余量（Rust 编译很吃空间） ----
$free = (Get-PSDrive -Name ($tauriDir.Substring(0,1))).Free / 1GB
if ($free -lt 8) {
    Write-Info ("目标盘剩余 {0:N1} GB，Tauri 首次构建（含依赖）通常需要 5–10 GB；不足可能中途失败" -f $free)
} else {
    Write-Ok ("目标盘剩余 {0:N1} GB" -f $free)
}

# ---- 6. 构建 ----
$profileArgs = if ($Debug) { @() } else { @('--release') }

if ($Installer) {
    $tauri = Get-Command cargo-tauri -ErrorAction SilentlyContinue
    if (-not $tauri) {
        Write-Info '未安装 tauri-cli，正在安装（首次约几分钟）…'
        & cargo install tauri-cli --version '^2' --locked
        if ($LASTEXITCODE -ne 0) { Write-Bad 'tauri-cli 安装失败'; exit 1 }
    }
    Write-Head 'cargo tauri build（产出 msi / nsis 安装包）'
    Push-Location $tauriDir
    try { & cargo tauri build @profileArgs; $code = $LASTEXITCODE } finally { Pop-Location }
    if ($code -eq 0) { Write-Ok "产物在 $tauriDir\target\release\bundle\" }
} else {
    Write-Head 'cargo build（产出裸 exe，最快）'
    Push-Location $tauriDir
    try { & cargo build @profileArgs; $code = $LASTEXITCODE } finally { Pop-Location }
    if ($code -eq 0) {
        $sub = if ($Debug) { 'debug' } else { 'release' }
        Write-Ok "产物: $tauriDir\target\$sub\netsense.exe"
        Write-Info '首次运行会在 exe 同目录找 config.json；没有就手动放一个（可复制 config.example.json 改名）'
    }
}

if ($code -ne 0) { Write-Bad "构建失败（退出码 $code）"; exit $code }
Write-Host "`n构建完成。" -ForegroundColor Green
