# Changelog

All notable changes to NetSense are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/).

## [1.0.1] - 2026-09-26

### Changed

- **Editor current-network block** now displays full NIC details matching the tray popup (Interface → SSID → Signal → MAC → IPv4 → Netmask → Gateway → IPv6 → DNS)
- **SSID picker** converted from datalist to real `<select>` with manual entry option for hidden networks
- **Interface picker** shows human-readable labels (e.g., "en0 (Wi-Fi)") instead of bare device names
- **Printer picker** converted from datalist to real `<select>` with human-readable labels, excluding auto-discovered queues
- Removed unused i18n keys `editor.nic_gateway_mac`, `editor.printer_manual`; added `editor.ssid_manual`, `editor.printer_placeholder`

## [1.0.0] - 2026-09-23

A tray-resident network Profile manager for macOS, Windows and Linux: networks are matched by
identity rather than by interface name.

### Added

- **Profile model.** A Profile is `enabled` + `quick` + `detection` + `rules` + `THEN` + `ELSE`. Rules are
  OR-ed; the enabled Conditions inside one Rule are AND-ed. Condition kinds: `wifi_ssid`,
  `gateway_mac`, `bssid`, `network_interface`. Every Condition, Rule and action carries its own
  `enabled` flag and reports a live state.
- **Engine.** Exactly one Active Profile: 1 match activates it, 2+ matches is a **Conflict**
  (reported, nothing applied), 0 matches applies the global `fallback`. Matching failures
  (Conflict) and execution failures (Error) are separate states; re-evaluation is driven by an
  identity fingerprint that also notices gateway and interface-set changes.
- **3A network configuration as one barrier.** IPv4 / netmask / gateway / DNS / IPv6 and static
  routes, followed by verification: read the system back and compare, optionally with an
  ICMP/HTTP health probe that reverts to DHCP on sustained failure. A 3A failure blocks 3B.
- **3B1 one-shot actions** (`launch_app`, `run_script`, `set_default_printer`) in `priority` batches — lower first, equal
  priorities concurrent, batches sequential, a failure in one batch not blocking the next. They run
  off the engine thread under a per-action timeout, and every run leaves a structured per-action
  trace that the editor and the panel render as live status. Script paths are restricted by an
  allow-list, with a relative path resolved against the configuration directory before it is
  checked; elevated scripts always go through the system authorization dialog, one prompt per
  action. Setting the default printer addresses the printer **by name** (the editor lists the
  system's printers, marking the current default) and writes only the *current user's* default —
  `lpoptions` on macOS / Linux, the per-user CIM method on Windows — so a network switch never
  raises an authorization prompt.
- **3B2 persistent actions** (`periodic_script`, `keep_wireguard_connected`, `keep_vpn_connected`) hold a
  desired state instead of repeating a command: one worker per enabled action, started with the Active
  Profile's THEN branch and stopped before any new configuration is applied. Each check answers
  `satisfied`, `repaired` or `faulted` — a tunnel that is already up issues no command at all — and the
  editor and the panel show that per-action state. A worker never raises an authorization prompt: a
  tunnel that needs more privilege reports an error rather than asking again every interval.
- **Per-Profile detection strategy**: `network_events`, `polling_only`, or both, with its own
  change delay so a transient reconnect does not thrash the adapter.
- **Tray popup panel** with live status, every active interface, VPN tunnels, one-click Profile
  switching with match badges, settings, logs, DHCP/probe actions and the updater.
- **Two configuration files, split by what they change**: `config.json` holds the automation model
  (profiles, conditions, actions) the editor writes; `settings.json` holds how the application itself
  behaves (interface language, log retention) and is edited in its own window. Launch at login belongs
  to neither file - it is asked of the operating system each time that window opens, because a copy
  stored here would be a second truth free to disagree with the system.
- **Configuration editor** for the whole model, rendering the engine's verdicts in place, with a
  conflict dialog, a three-state DNS control, and a manual
  apply that first lists the 3A target and the batches it is about to run.
- **Platform abstraction layer** implementing reads, writes, interface enumeration and elevation on
  macOS (`networksetup` / `ipconfig` / `arp` / `system_profiler`), Windows (PowerShell CIM + `netsh`)
  and Linux (`nmcli` / `ip`), with passwordless channels where available and the system
  authorization dialog as fallback.
- **Online upgrade**: release check, native install and relaunch, streaming progress to the panel —
  asset download with SHA256 verification, or `brew upgrade --cask` for a Homebrew install.
- **Five interface languages** (en / zh / zh-TW / ja / ko), English by default, with key-parity and
  placeholder validation, switched in the settings window — every window, the tray tooltip and the
  native error dialogs draw from that one dictionary. Daily-rotating local logs whose retention
  days are configured there too; GitHub Actions matrix build producing
  installers for all four targets from one tag.

### Known gaps

- Reconnecting tunnels (`scutil --nc`, `rasdial`, `wireguard.exe`, `nmcli`) is exercised only by unit
  tests and by the CI compile of each platform: like the rest of the platform layer it still needs one
  real-device pass per OS, and on macOS some VPN configurations only accept `scutil --nc start` after
  the user has connected once by hand.
- Applying a configuration has no rollback beyond the blanket DHCP fallback: a 3A that lands but
  then fails its health probe reverts to automatic configuration rather than restoring the
  previous working settings.
