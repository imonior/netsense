# Changelog

All notable changes to NetSense are documented here. The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [0.2.1] - 2026-09-19

### ✨ Added
- Multi-language documentation with **English as the default**: `README.md` (en) plus `README.zh.md` / `README.zh-TW.md` / `README.ja.md` / `README.ko.md`; `CHANGELOG.md` (en) plus `CHANGELOG.zh.md` / `CHANGELOG.zh-TW.md` / `CHANGELOG.ja.md` / `CHANGELOG.ko.md`.
- `VERSION` file as the single source of truth for the version number; `scripts/bump_version.sh` syncs it into `tauri.conf.json` and `Cargo.toml`.
- Application artwork: `app-icon.png` (1024² master) is now the source of the window, tray and installer icons, and `scripts/gen_icons.py` derives the full set from it — PNG sizes, a 7-frame ICO (16 → 256, BMP frames plus a PNG frame) and an 8-chunk ICNS (ic07–ic14).

### 🔧 Changed
- **Windows installer now installs per-machine** (`bundle.windows.nsis.installMode` = `perMachine`). The app installs to `C:\Program Files\NetSense` and requires administrator privileges at install time (previously it installed per-user under `%LOCALAPPDATA%`). The MSI bundle has no equivalent option — WiX already targets `%PROGRAMFILES%` — so nothing is set there.
- Release notes are generated from the English `CHANGELOG.md` section for the matching version, so published releases are English by default.
- `scripts/gen_icons.py` no longer paints a placeholder mark: it resamples the artwork master, applies the rounded-corner mask at every target size and packs the PNG/ICO/ICNS containers (pure stdlib, no third-party dependencies).

### 🐛 Fixed
- **Windows: no more stray console window.** The PAL ran `powershell.exe` / `netsh` through `Command::output()` without a creation flag, so Windows allocated a visible console (complete with its own title bar) for every status read — the process is now started with `CREATE_NO_WINDOW`.
- **Windows: the app looked like it never opened.** NetSense is tray-resident and neither window is shown at launch, so once those console windows disappeared nothing was visible. The status panel is now opened on startup and collapses back to the tray on blur or close.
- CI: the Windows `choco install wixtoolset nsis` step is bounded by a timeout, so a stalled download can no longer hang the job until the runner's limit.

### 🔒 Security
- **Every privileged path is shell-quoted.** Values that come from the config file or from the OS (profile names, SSIDs, gateway addresses, routes) are passed through `osascript`, `sudo` or a shell. They are now POSIX single-quoted at the point of interpolation, so a value containing `'`, `$(…)`, a backtick or `;` can no longer inject a command into a root shell.
- **macOS elevation branch quoted too.** The password-free `osascript … with administrator privileges` path built its command line by string concatenation; script path and arguments are now quoted token by token.
- **Windows `v6prefix` is parsed, not interpolated.** A prefix returned by the OS used to be spliced straight into the PowerShell command line; it is now accepted only as an integer and the option is dropped otherwise.
- **Automation script allow-list can no longer be escaped with a path prefix.** A directory such as `scripts2/` used to match the `scripts/` prefix by plain string comparison; both paths are now canonicalised and compared component-wise.

### 🛠 Internal
- `scripts/validate.py` gained check **[7] `tauri.conf.json` field validity**: the `bundle`, `bundle.windows`, `nsis` and `wix` subtrees are validated against the official Tauri v2 schema, so an unsupported field fails the cheap 10-minute validation job instead of a 4-platform build.
- The release-publishing step is restricted to tag pushes, so a manual `workflow_dispatch` verification run can no longer rewrite the tag of an existing Draft Release.

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
