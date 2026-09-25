# NetSense

**English** · [简体中文](README.zh.md) · [繁體中文](README.zh-TW.md) · [日本語](README.ja.md) · [한국어](README.ko.md)

A cross-platform network **Profile** manager for macOS, Windows and Linux.

Each Profile answers three questions: *which network am I on?* (`Rules` / `Conditions`), *what
should this network look like?* (`THEN` when the Profile matches, `ELSE` when it does not), and
*what should happen afterwards?* (automation actions). NetSense evaluates those Profiles against
the live network, keeps exactly one of them Active, and pushes the resulting configuration through
the system's own tooling — `networksetup` on macOS, PowerShell CIM + `netsh` on Windows, `nmcli` on
Linux.

It lives in the tray. The editor window opens once at launch — the unambiguous signal that the app
started — and closing it only hides it again. No daemon to configure, no telemetry.

## How a decision is made

```
            ┌──────────── enabled Profiles ────────────┐
network ──▶ │  Rule = enabled Conditions ANDed         │ ──▶ 1 match  ──▶ that Profile is Active
   change   │  Rules  ORed  ·  re-checked per snapshot │ ──▶ 2+ match ──▶ Conflict (dialog, apply nothing)
            └──────────────────────────────────────────┘ ──▶ 0 match  ──▶ global fallback (not a Profile)
```

- **Matching** keys on network identity: Wi‑Fi SSID, gateway MAC, AP BSSID — so same-named SSIDs at
  different sites, and spoofed hotspots, stay distinct. A `network_interface` condition is available
  too, for wired links and for the cases where the NIC itself is what you mean.
- **There is no Profile priority** — deliberately. With more than one match the only honest answer
  is *Conflict*: NetSense shows it and applies nothing rather than silently picking a winner.
- **Applying is one barrier (3A)**: IPv4 / netmask / gateway / DNS / IPv6 and static routes go out
  together, and — when the branch configures `verify` — come back verified by reading the system
  state, optionally plus an ICMP/HTTP health probe that reverts to DHCP if the network keeps failing.
- **Automation (3B) runs only after 3A passed.** One-shot actions (`launch_app`, `run_script`,
  `set_default_printer`) execute in `priority` batches — lower first, equal priorities concurrent,
  a batch finishes before the next starts, and a failure inside one batch does not block the later
  ones. Setting a default printer touches only *your* default, so switching networks never raises an
  authorization prompt. A match failure (Conflict) and an execution failure (Error) are two different
  states and are reported separately.
- **Detection is per Profile**: react to network events, poll on an interval, or both, each with
  its own change delay — so a flaky reconnect does not rewrite the adapter repeatedly.
- **Persistent actions hold a state instead of repeating a command.** Each enabled one gets its own
  worker, started with the Active Profile's THEN branch and stopped before anything new is applied;
  a tunnel check that finds the tunnel already up issues no command at all, and a worker never raises
  an authorization prompt. (`periodic_script` is the one kind with nothing to check — its tick *is*
  running the script, so it repeats by design.)

## Features

- Static IP / DHCP / custom DNS / IPv6 (automatic, manual, off) / static routes, per Profile, with
  a separate THEN and ELSE branch.
- Read-back verification and health monitoring, configured per branch inside the apply step, so a
  Profile only reports "applied" when the system agrees it did.
- Tray popup panel: the network actually in use (interface, SSID + signal, MAC,
  IPv4 / netmask / gateway / IPv6 / DNS), the other active interfaces, VPN tunnels, one-click
  Profile switching with match badges, and every entry point - settings, logs, DHCP, probe,
  update, quit. Any click on the icon opens it, blur collapses it; there is no native tray menu.
- A configuration editor covering the whole model — Rules, Conditions, 3A, routes, actions, ELSE
  and the global fallback — with the engine's live verdicts rendered in place.
- A software settings window, holding the settings that are not about any network: interface language,
  launch at login (read from the operating system every time the window opens, never from a copy in a
  file), where `config.json` and the logs live, and how many days of logs to keep.
- Passwordless where it can be: a macOS `sudoers` allow-list installed once, an elevated Windows
  run, or `sudo -n` on Linux. Where it cannot, NetSense falls back to the system authorization
  dialog instead of failing.
- Online upgrade: checks GitHub Releases and picks this platform's asset. A Homebrew install
  upgrades through `brew upgrade --cask` and never downloads anything; otherwise NetSense downloads
  the asset and installs it only once its SHA256 matches this release's `SHA256SUMS` — when that check
  cannot be made (the release has no `SHA256SUMS`, this asset is not listed in it, or it cannot be
  fetched), the update stops and you are pointed at the release page instead.
- Five interface languages (English, 简体中文, 繁體中文, 日本語, 한국어), validated for parity;
  English is the default. Every window on every platform follows the selection — tray panel, editor,
  settings, log viewer, the tray tooltip and the native error dialogs included — and a static-text
  check in `scripts/validate.py` keeps it that way.

## Tech stack

Tauri v2 + Rust, system WebView, plain static HTML/CSS/JS on the frontend (no Node build chain).
The Core Engine (detection / conditions / matching / network / automation) never calls a system
command directly: everything platform-specific sits behind one trait in the PAL
(`src-tauri/src/platform/{macos,windows,linux}.rs`), selected at compile time.

| Platform | Reads | Writes | Elevation |
|----------|-------|--------|-----------|
| macOS | `networksetup` / `ipconfig` / `arp` / `system_profiler` (`airport`, where it still exists) | `networksetup` / `route` | sudoers allow-list, else `osascript` prompt |
| Windows | PowerShell CIM (`Get-NetAdapter` …) + `netsh` | `netsh` / `New-NetRoute` | none when already admin, else UAC |
| Linux | `nmcli` / `ip neigh` | `nmcli con mod` / `ip route` | `sudo -n`, else `pkexec` |

## Getting started

### Build in the cloud (no local dependencies)

Pushing a version tag runs CI for all four targets at once
(`windows-x64` / `macos-arm64` / `macos-x64` / `linux-x64`) and opens a **Draft** Release that the
installers attach to; it goes public on its own once every asset is there and `SHA256SUMS` was
generated:

```bash
git tag v1.0.0 && git push origin v1.0.0
```

The same workflow can be started manually from GitHub → Actions → build → Run workflow, which
produces bare executables and no Release.

### Build locally

```bash
# Requires Rust plus the platform's build dependencies
# (Windows additionally needs the MSVC C++ build tools and WebView2).

# Windows — the script checks Rust/MSVC/SDK/WebView2/disk first and reports what is missing
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1            # executable
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 -Installer # msi / nsis

# macOS / Linux
cd src-tauri && cargo build --release   # executable
cd src-tauri && cargo tauri build       # installer (needs tauri-cli)

# macOS, optional: install the passwordless privilege channel so changing a network
# no longer asks for authorization every time
sh scripts/install-priv-helper.sh
```

### Checks (no compilation, runs anywhere)

```bash
python3 scripts/validate.py    # JSON, i18n parity and placeholders in 5 languages, key usage in both
                               # directions, PAL boundary, trait coverage on all 3 platforms,
                               # tauri.conf.json fields, version consistency, docs↔code alignment,
                               # and that every UI string comes from the dictionary
node scripts/editor-smoke.mjs  # the editor's data binding, headlessly (needs Node, no build step)
cd src-tauri && cargo test     # pure bin crate — use `cargo test`, not `--lib`
```

NetSense lives in the tray: click the icon - either button - for the panel, which carries every entry
point (settings · open log folder · set current network to DHCP · probe now · quit) above the live
interfaces and tunnels it shows. It is configured by two files, because the two kinds of setting have
nothing in common. Which network gets which treatment is the automation configuration, a single
`config.json` you edit in the editor window. How the application itself behaves - interface language,
log retention, launch at login - is the software configuration, `settings.json`, edited in the settings
window that the panel's "Settings" button opens; changing it never re-applies a network setting.
`config.json` and `settings.json` both sit in NetSense's own per-user directory —
`~/Library/Application Support/NetSense` on macOS, `%APPDATA%\NetSense` on Windows,
`~/.config/netsense` on Linux — and never next to the executable: a signed macOS bundle and a
read-only `Program Files` must not be written to. Logs go to NetSense's per-user log directory.
`config.example.json` is a complete worked example.

> On Windows the first network change prompts for UAC once. Running as administrator once removes
> it; the privilege channel then reports "no authorization needed".

## Configuration

```jsonc
{
  "schema": 1,
  "allowed_scripts": ["/opt/ops/office-init.sh"],
  "profiles": [{
    "id": "office", "name": "Office_5G", "enabled": true,
    "detection": { "mode": "network_events_and_polling", "poll_interval_secs": 30 },
    "rules": [{
      "id": "r1", "enabled": true,
      "conditions": [
        { "id": "c1", "enabled": true, "type": "wifi_ssid",   "value": "Office_5G" },
        { "id": "c2", "enabled": true, "type": "gateway_mac", "value": "aa:bb:cc:dd:ee:ff" }
      ]
    }],
    "then": {
      "network": {
        "mode": "manual", "ip": "192.168.1.100", "netmask": "255.255.255.0",
        "gateway": "192.168.1.1", "dns": "192.168.1.1,8.8.8.8", "v6mode": "off",
        "routes": [{ "dest": "10.0.0.0/8", "gateway": "192.168.1.1", "metric": 0 }],
        "verify": { "readback": true,
                    "health": { "enabled": true, "mode": "both",
                                "icmp_target": "192.168.1.1",
                                "http_target": "http://192.168.1.1/",
                                "interval": 30, "retries": 3, "timeout": 5,
                                "fallback": { "enabled": true } } }
      },
      "one_shot": [
        { "id": "a1", "enabled": true, "priority": 1,
          "action": { "type": "run_script", "path": "/opt/ops/office-init.sh", "elevated": false } }
      ]
    },
    "else": { "network": { "mode": "dhcp", "dns": "", "routes": [{ "dest": "10.0.0.0/8", "delete": true }] } }
  }],
  "fallback": { "enabled": true, "network": { "mode": "dhcp", "dns": "", "v6mode": "automatic" } }
}
```

Two conventions are worth knowing. **An empty string clears a field; a *missing* field is left
alone.** `dns` is modelled the same way, and that is what makes the editor's three-way DNS select
honest: "Leave unchanged" deletes the key, so no platform issues a DNS command; "System Auto"
writes the empty string, so the servers are cleared; "DNS Servers" pushes what you typed. Second:
`run_script` only executes a path that lives under `<config dir>/scripts` or appears in
`allowed_scripts`; a relative path is read against the directory holding `config.json`, not the
directory the process was started from.

`DEVELOPMENT.md` documents the model, the engine pipeline and the per-platform traps in detail.

## License

MIT
