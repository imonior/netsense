# Changelog

All notable changes to NetSense are documented here. The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [0.2.1] - 2026-09-19

### ✨ Added
- Multi-language documentation with **English as the default**: `README.md` (en) plus `README.zh.md` / `README.zh-TW.md` / `README.ja.md` / `README.ko.md`; `CHANGELOG.md` (en) plus `CHANGELOG.zh.md` / `CHANGELOG.zh-TW.md` / `CHANGELOG.ja.md` / `CHANGELOG.ko.md`.
- `VERSION` file as the single source of truth for the version number; `scripts/bump_version.sh` syncs it into `tauri.conf.json` and `Cargo.toml`.

### 🔧 Changed
- **Windows installer now installs per-machine** (`bundle.windows.nsis.installMode` / `wix.installMode` = `perMachine`). The app installs to `C:\Program Files\NetSense` and requires administrator privileges at install time (previously it installed per-user under `%LOCALAPPDATA%`).
- Release notes are generated from the English `CHANGELOG.md` section for the matching version, so published releases are English by default.

### 📝 Docs
- `DEVELOPMENT.md` rewritten in English (default).

## [0.2.0] - 2026-09-18

### ✨ Added
- **True cross-platform support** (macOS / Windows / Linux) via a Platform Abstraction Layer (PAL): all OS differences are confined to `platform/{macos,windows,linux}.rs` behind a `NetworkPlatform` trait, selected at compile time. The upper engine depends only on the trait.
- **Four-target CI build matrix** (`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`). Pushing a `v*` tag triggers a Draft Release with per-platform installers (NSIS + MSI for Windows, DMG for macOS, DEB for Linux) and bare binaries.
- **In-app multi-language UI** (en / zh / zh-TW / ja / ko): 79 keys × 5 languages, with parity validation in `scripts/validate.py`.
- **Privilege escalation channels** with graceful fallback: macOS passwordless after a `sudoers` allow-list is installed; Windows passwordless when already admin (otherwise UAC); Linux passwordless with `sudo -n` (otherwise `pkexec`).
- **Health monitoring**: ICMP / HTTP / both probes; on consecutive failures with fallback enabled, automatically reverts to DHCP as a safety net.
- **Network-triggered automation**: `route` / `launch` / `run` actions fired on `on_apply` / `on_revert`, with scripts constrained by an allow-list.

### 🐛 Fixed
- Windows CI packaging: WiX + NSIS are now installed explicitly on `windows-latest` (they are not preinstalled), and bundle targets are scoped per OS.

### 🛠 Internal
- `scripts/validate.py` adds checks for JSON validity, 5-language i18n parity (79×5), PAL boundary, and 3-platform trait coverage.
