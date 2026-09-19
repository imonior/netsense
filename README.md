# NetSense

> A cross-platform standalone desktop app that automatically matches and applies network profiles (static IP / DHCP / DNS / IPv6) from the current network identity (SSID / gateway MAC / BSSID), with health monitoring (fallback to DHCP on disconnect) and network-triggered automation (set routes / launch apps / run scripts).

Evolved from [hammerspoon-wifi-switcher](https://github.com/imonior/hammerspoon-wifi-switcher) into a **Hammerspoon-free** standalone app for macOS / Windows / Linux.

## Features

- **Network identity matching**: SSID, gateway MAC, and AP BSSID each work as an independent condition (alone or combined, AND logic). Correctly distinguishes same-named SSIDs across scenes and spoofed hotspots (evil twin).
- **Per-SSID profile**: static IP / DHCP / custom DNS / IPv6 (automatic / manual / off).
- **Global fallback**: `__DEFAULT__` applies to any unconfigured network.
- **Health monitoring**: ICMP / HTTP / both probes; on consecutive failures with fallback enabled, automatically reverts to DHCP as a safety net without dropping connectivity.
- **Automation**: network-triggered `route` / `launch` / `run` actions (netsetman-style), fired on `on_apply` / `on_revert`; scripts are constrained by an allow-list.
- **Tray popup panel**: left-click the status-bar / tray icon to open the panel (status + one-click profile switch + language); auto-collapses on blur; right-click for the native menu.
- **One codebase for three platforms**: all platform differences are confined to the PAL (`platform/{macos,windows,linux}.rs`); the upper layer depends only on the trait.
- **Privilege escalation**: macOS gains passwordless sudo after installing a `sudoers` allow-list; Windows runs passwordless once launched as admin (no UAC); Linux is passwordless with `sudo -n` configured. Falls back to the system authorization dialog when unavailable — no feature breakage.
- **Multi-language (en / zh / zh-TW / ja / ko)**: 79 keys × 5 languages, with parity validation.

## Tech Stack

- **Tauri v2 (Rust)** + system WebView (reuses the existing HTML/CSS editor; no Node build chain)
- Rust backend: Core Engine (matching / applying / health / automation) + PAL (platform abstraction layer)
- Platform implementations (same upper-layer code, selected at compile time):

| Platform | Read | Write | Privilege |
|----------|------|-------|-----------|
| macOS | `networksetup` / `arp` / `airport` | `networksetup` / `route` | passwordless sudoers allow-list, fallback `osascript` dialog |
| Windows | PowerShell CIM (`Get-NetAdapter` / `Get-NetConnectionProfile` …) + `netsh` | `netsh` / `New-NetRoute` | passwordless if already admin, otherwise UAC |
| Linux | `nmcli` / `ip neigh` | `nmcli con mod` / `ip route` | passwordless with `sudo -n`, otherwise `pkexec` |

## Quick Start

### Method 1: Cloud build (recommended, zero local dependencies)

Pushing a tag triggers CI to produce installers and binaries for all three platforms at once (`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`):

```bash
git tag v0.3.0 && git push origin v0.3.0
```

You can also trigger it manually from GitHub → Actions → build → Run workflow.

### Method 2: Local build

```bash
# Prerequisites: Rust + per-platform build deps (Windows also needs MSVC C++ build tools + WebView2)

# Windows (the script first checks Rust/MSVC/SDK/WebView2/disk and tells you what is missing)
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1            # bare exe
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer # msi/nsis

# macOS / Linux
cd src-tauri && cargo build --release   # bare executable
cd src-tauri && cargo tauri build       # installer (needs tauri-cli)

# macOS optional: install the passwordless privilege channel to remove the auth prompt on every network change
sh scripts/install-priv-helper.sh
```

### Validate (no compile needed, runs on any platform)

```bash
python scripts/validate.py     # JSON / 5-language i18n parity / PAL boundary / 3-platform trait coverage
cd src-tauri && cargo test       # this is a pure bin crate (no lib target), use cargo test not --lib
```

After launch the app lives in the tray (no main window pops up): **left-click the tray icon** opens the popup panel,
right-click opens the menu (show editor / open log folder / view privilege channel / quit).

> **Windows prompts UAC once on the first network change.** Launching as administrator once removes it (after that the privilege channel shows "no authorization needed").

## Configuration

Edit `config.json` (copied from `config.example.json`). Key structure:

```json
{
  "__DEFAULT__": { "mode": "dhcp", "dns": "", "v6mode": "automatic" },
  "Office_5G": {
    "match": { "ssid": "Office_5G", "gateway_mac": "aa:bb:cc:dd:ee:ff", "bssid": "00:11:22:33:44:55" },
    "mode": "manual", "ip": "192.168.1.100", "gateway": "192.168.1.1", "dns": "192.168.1.1,8.8.8.8",
    "health":   { "enabled": true, "fallback": { "enabled": true }, "mode": "both" },
    "automation": { "enabled": true, "on_apply": [ { "type": "route", "dest": "10.0.0.0/8", "gateway": "192.168.1.1" } ] }
  }
}
```

See [DEVELOPMENT.md](DEVELOPMENT.md) for details.

## License

MIT
