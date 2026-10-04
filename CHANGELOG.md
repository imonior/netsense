# Changelog

All notable changes to NetSense are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project adheres to
[Semantic Versioning](https://semver.org/).

## [1.0.7] - 2026-10-04

### Added

- **More platforms and a portable build.** CI now builds six targets (was four) and attaches a portable archive to every release:
  - Windows: added **ARM64** (`aarch64-pc-windows-msvc`) and **32-bit x86** (`i686-pc-windows-msvc`); x64 keeps NSIS + MSI.
  - Linux: x64 DEB (unchanged).
  - macOS: Apple Silicon + Intel DMG (unchanged).
  - **Portable**: each target also ships a `NetSense_<label>_portable.zip` — extract and run `NetSense.app` / `netsense` / `netsense.exe` directly, no installer; it uses the same per-user config dir.

### Fixed

- **A polling pass can no longer dismantle a working network.** When one sample reads no network
  identity at all — SSID, gateway MAC and BSSID all empty — the engine used to read that as "not one
  Profile matches" and run the zero-match fallback, so a static address became DHCP-assigned and the
  configured DNS servers were cleared to automatic, while the UI showed nobody doing it. All three being
  empty is far more often "these reads returned nothing" than "we are on an unknown network": macOS has
  exactly one SSID source (CoreWLAN; the CLI reads are blacked out on Sequoia), and the gateway MAC only
  appears when that entry happens to be in the ARP table — which is precisely the shape of a sample taken
  mid-reconnect. An uncorroborated zero match now leaves the Active profile, its health monitor and the
  fallback untouched; two consecutive empty samples are taken as an unreadable network, and the fallback
  works as before. Polling also waits out `change_delay_secs` when the fingerprint has just changed, so it
  can no longer jump ahead of the settle window the event path already respects.

- **Linux: the status query no longer panics.** The native netlink reader resolved its answer channel with `tokio::sync::oneshot::Receiver::blocking_recv()` inside an async Tauri command, which violates the tokio contract and panicked on every status read. The channel is now a `std::sync::mpsc` receiver polled with `recv_timeout`, and the worker is guarded by `tokio::time::timeout` so a half-dead connection cannot hang the whole read path.
- **Linux: network enumeration now compiles and runs.** `list_interfaces` failed to compile because the refactored `device_ip` returns `Option<DeviceIp>` (left unwrapped) and the sibling module `linux_nm` was referenced without an import. Both are fixed, so the Linux backend actually builds instead of being silently excluded by `cfg(linux)`.
- **macOS: gateway MAC resolves for OUIs that drop leading zeros.** `arp -n` prints `0:50:56:c0:0:8` instead of `00:50:56:c0:00:08`, and the old whole-line padding could not restore the first byte fused with the preceding IP text. A new `extract_mac_loose` (cfg-agnostic, unit-tested) restores the canonical MAC, so VMware / Huawei OUIs are matched again.
- **Linux: DNS server addresses were decoded with reversed octets on little-endian hosts.** The `Nameservers` (`au`) bus `guint32` *is* the `in_addr` (its bytes are the network-order octets), so it must be read with `Ipv4Addr::from(n.to_ne_bytes())`. The earlier `Ipv4Addr::from(n)` change (big-endian) reversed the four bytes on x86_64 / ARM and was reverted. Covered by a unit test that derives the bus value from the address octets the way glibc `inet_pton` / socket2 do, so CI on real glibc is the judge.
- **macOS: a failed privileged operation is now observable.** The bootstrap op chain joined with `;` (run all regardless) masked the first failure behind the last command's exit status; it now joins with `&&` and stops at the first failure. The installer prefix `;` is intentionally left unchanged.
- **Settings: saving global options no longer wipes the fallback or script whitelist.** When the payload omitted the `fallback` (or `allowed_scripts`) field, it was defaulted to null/empty and silently cleared. Missing keys now preserve the existing value, consistent across both fields.

## [1.0.6] - 2026-10-02

### Added

- **A WireGuard tunnel is now credited to the software behind it.** Every wireguard-go tunnel used
  to be labelled "WireGuard", because the only thing an unprivileged process can see — the control
  socket upstream keeps at `/var/run/wireguard/<dev>.sock` — says what the device *is*, not which
  app opened it. The passwordless privileged channel gains one read-only query for the process
  holding that socket, and the card shows the application its executable path belongs to. The query
  writes nothing and takes one argument, validated as an interface name (`[A-Za-z0-9]{1,16}`)
  before the filesystem is touched, and it exits without an answer when there is no socket or no
  holder, because no answer beats a guessed one. The app never asks unless the channel is already
  installed, and caches each device's answer — the absence of one included — for 30 seconds, so the
  two-second network refresh never puts a root subprocess behind every repaint. The channel now
  counts as installed only when the script on disk is byte-for-byte the one this build shipped,
  since an older copy has no such query at all; the cost is that the first configuration apply
  after an upgrade that changed the wrapper shows a single authorization prompt, the same one a
  fresh install shows, and every apply after that stays passwordless.
- **The zero-match fallback has its own 3B actions.** The fallback — what NetSense does when no
  profile matches the network it is on — used to hold nothing but a network configuration. It now
  carries the same two action lists a profile carries: a one-shot list that runs once when the
  fallback is entered, and a persistent list whose workers stay up for as long as nothing matches
  and are stopped before a matching profile's own workers start. Applying any profile's
  configuration invalidates that memory, so a fallback that already ran at startup still runs when
  the machine moves to a network where nothing matches. The editor shows it as one section with the
  two lists and no THEN/ELSE split: an else needs something to fall back from, and this *is* that
  case.

### Fixed

- **macOS: a tunnel is no longer named after whichever VPN session happens to be connected.** When
  exactly one tunnel was up and exactly one VPN session was connected, that session was credited
  with the tunnel — unless its own network service reported a different address. Clients that never
  report an address made that counter-proof unable to fire, and in the field this labelled a
  foreign WireGuard tunnel "Tailscale"; a specific wrong name is worse than a generic one, because
  the persistent "keep connected" action adopts whatever the card says as its target. Attribution
  now requires evidence bound to the interface — an address one of the services reports for this
  tunnel, or the process holding its control socket — the socket alone names the implementation, and
  anything unclaimed keeps the generic VPN label.
- **Saved-network candidates are sorted, and the popup opens where there is room.** The list followed
  the order the system reports networks in, which is roughly association history and reads as
  random; it is now sorted by name. Its direction was decided by comparing the input's bottom edge
  with the bottom edge of the *expanded* list — a difference that is always negative, so every popup
  opened upward and the first entries of a list near the top of a column were clipped out of sight.
  The choice is now made from the room above and below the input inside the scrollable area it sits
  in, taking the roomier side when neither fits a full column, and the list's own height is capped to
  that side so the rest stays reachable by its scrollbar.
- **The status strip's second cell says what it shows.** Its English heading read "Why it matched",
  which promises a rationale, while the cell lists the condition values the active profile matched
  on. It reads "What is matched" now. The other four languages already named the conditions, so this
  is a wording fix in the English dictionary.
- **One flaky handshake no longer fails the update check, and a failure says why.** "This update
  cannot be verified: SHA256SUMS could not be downloaded" was the whole report of a single
  interrupted TLS handshake: the fetch was tried once, and the reason was discarded. It is now tried
  up to three times with a short wait between attempts, each attempt gets 15 seconds rather than 10
  (the one fetch that succeeded in the field took 8.7), every retried attempt is logged, and the
  final failure is logged with the error behind it. The retry sits in the app rather than in
  `curl --retry`, which does not retry SSL errors and only learned `--retry-all-errors` after the
  version Windows 10 ships. The same fetch serves the version check, the pre-click verdict and the
  pre-install verification, so all three inherit it.

## [1.0.5] - 2026-10-01

### Added

- **The launch-program target is a combo box: installed programs, a file picker, or typing.** The
  caret at the field's right edge opens the programs installed on this machine — macOS scans its four
  standard application locations for `.app` bundles, Windows reads the Start menu's shortcuts, Linux
  takes `Type=Application` `.desktop` entries with the name picked for the interface language.
  Picking one writes its path; next to it, **Browse…** opens the system's own file dialog for the
  portable single-file programs no menu knows about (on Linux the picker needs zenity or kdialog —
  when neither is installed the error says so and typing stays available). All three entries write
  the same field, and typing stays primary: when the list is empty (a clean machine, a failed
  enumeration), the input and Browse are still there. Only programs with a registered launch intent
  are listed — not a sweep of bare executables.

### Changed

- **Windows: every SSID shown is the one that is on the air.** The current-network SSID now comes
  from `netsh wlan show interfaces`' association state, attributed per adapter MAC so several Wi-Fi
  adapters never cross names; when that read fails it falls back to the WLAN profile file on disk and
  then to the profile name. The saved-network candidates carry the real name too, from the profile
  XML that stores what the network calls itself. The old source was the NLA network name: whenever
  the network signature changes (driver reload, gateway change, a VPN), Windows mints a new object
  and disambiguates it with ` 2`, ` 3` — a profile name on *this* machine, not the network's name,
  while three machines have to be looking at the same network. **This changes what a stored
  condition means**: one saved on Windows earlier against a name with ` 2` no longer matches — pick
  it again from the dropdown.
- **The tray panel's VPN section answers "installed, and connected?"** Clients that are installed but
  not connected get a card with a Status row (Connected / Not connected): macOS lists them from
  `scutil --nc list`'s VPN sessions, Linux from NetworkManager connections that are not active (on
  Windows the disconnected virtual adapters were already listed). A card with no address is a fact,
  not a failed collection. Live tunnels keep their attributed app, and macOS's attribution is one
  notch stricter: the lone-connected-session shortcut now needs a counter-proof — if that session's
  own service reports an address, and it is not this tunnel's, the tunnel is not credited to it. An
  unnamed VPN keeps the generic label; a specific wrong name would be adopted by the persistent
  "keep connected" action as its target.
- **The editor's condition badges follow your typing.** Every form edit re-evaluates the three-state
  badges against the engine's existing snapshot, without waiting for the engine's sampling beat and
  the per-Profile `change_delay_secs` debounce (5 seconds by default, eight and up worst case). It
  calls the same backend evaluation function, so no second matching rule exists; what is engine-only
  stays engine-only — ACTIVE / CONFLICT / ERROR badges and the green ring still arrive by broadcast,
  and the preview only fills in "this is actually matching right now".
- **The editor paints its form first, waiting on three cheap reads only.** Config, language and
  palette — none of which spawns a subprocess — land together for the first frame; the six external
  reads (saved networks, interfaces, adapters, printers, installed apps, status) go out concurrently
  and fill the status strip, badges and candidates as they arrive. They used to sit on the
  first-paint path, and the price was their *sum* — the multi-second blank seen on Windows.
- **Reordering is the ↑ / ↓ buttons only.** Dragging did not work in practice, so the drag code is
  gone and a card's header is a title again. The arrows still move the same array, and the
  THEN/ELSE, one-shot/persistent boundaries stay enforced by the controls themselves.
- **The SSID condition's input and its candidates are one combo box**: the candidates open under a
  caret at the input's right edge and fold away after a pick. The input remains the only control
  that writes config.
- **All four windows share one scale and one palette — and this time it is computed.** Radii collapse
  to three steps (control / container / badge and toast) and shadows to two (modal / popover); the
  windows had measured seven distinct radii, the same role rounding differently per window. Five
  dark-mode pairs sat below 4.5:1 and were fixed — white on the primary and on the warning button,
  the neutral badge, the "off" badge, and the off tone on a raised row — and `scripts/validate.py`
  now computes every listed text/background pair in both themes, so the rule is a check instead of a
  comment. Among the edges fixed along the way: the popup's toast sits above the confirm dialog, long
  errors truncate instead of being cut off on both sides, narrow rows in the log and settings windows
  wrap instead of squeezing buttons out of view, disabled buttons look disabled, a profile the
  engine has not reported on yet shows no empty pill, and a candidate popover near the bottom edge
  flips upward.

### Fixed

- **Linux: `launch_app` on a missing target reports an error instead of a fake success.** Every path
  used to be handed to `xdg-open` and the child's exit code was never read, so a mistyped path was
  recorded as a success and the badge went green for a program that never started. Executables and
  bare program names are spawned directly now (PATH resolves the name, a miss is an error);
  `.desktop` entries, documents and URLs still go to `xdg-open`; a path that is not on this machine
  is an error naming that path.

## [1.0.4] - 2026-09-29

### Changed

- **3B actions run in the order they are listed; the `priority` field is gone.** A one-shot list executes
  top to bottom, one action at a time — the next starts once the current one has finished or timed out,
  and a failure still never blocks the actions below it. A persistent list's order decides which worker
  starts first; after that they run independently. The editor changes that order two ways, and both move
  the same array: drag a card's header onto another card, or use the ↑ / ↓ buttons in it. A drag stays
  inside one branch and one kind of list — putting a one-shot action into the persistent list, or a THEN
  card onto the ELSE branch, decides *who executes it*, which is not a question of position. The
  pre-apply confirmation list now numbers the actions instead of grouping them. A config file that still
  carries `priority` loads as before; the field is simply no longer read.
- **The automation editor opens in one concurrent batch.** Its six initial reads each shell out (saved
  Wi-Fi networks, interfaces in use, installed adapters, printers, the current status, the config), and
  awaiting them one after another priced the window at their *sum* — the multi-second blank it had. Only
  the status read is fatal now: it carries the language, the palette and the address rows.
- **A status broadcast no longer starts a subprocess.** The address, netmask, gateway, DNS and signal in
  the editor's "current network" cell come from the broadcast itself, which the engine refreshes every
  pass. The two facts only `get_interfaces` can supply — the interface label and that NIC's own MAC —
  move with the connected interface and association, so that cell is refetched on the **identity
  fingerprint** instead of on every event (one engine pass emits two).
- **The SSID condition is a text field with the saved networks beside it.** Hidden networks and an
  environment you have not joined twice must be configurable in advance, so typing is the primary
  control and the dropdown only copies a name into the field. Clearing the field clears the condition —
  previously an emptied select could store an empty string, which then compares against a network whose
  SSID really is empty.
- **A printer label always keeps a subject.** When CUPS fills a queue's description with the queue name —
  what it does for every newly created queue — the old rule dropped the description and left only the
  location, so a list of printers read as a list of room numbers. It is now `CanonG3860 · XSMS`.
- **macOS credits a VPN adapter to an app only on evidence about *that* interface**: the network service
  reporting the tunnel's IPv4, or the single connected session when exactly one tunnel is up. The
  installed-but-disconnected client that sits in the service list used to win every unclaimed tunnel,
  which is a specific-looking wrong answer; now an unclaimed one shows the generic VPN label instead.
- **The editor's "current network" summary no longer appends a VPN tunnel row.** A tunnel is an adapter,
  not a network you are connected to, and no condition can key on it. The tray panel still lists tunnels.

### Fixed

- **The ACTIVE badge appears seconds earlier after you join a network.** The SSID watcher woke the engine
  but the sampling beat still decided when the new SSID entered the snapshot, and each Profile's change
  delay counts from *that* pass — so the wait was the delay you configured plus our own queue. A wake
  from the SSID watcher now forces one unconditional resample and re-evaluation; the change delay itself
  is untouched (it is flap protection, per Profile, editable in the editor's condition layer).
- **Windows: the update dialog is no longer garbled and checksums verify again.** The PowerShell leg read
  `Invoke-WebRequest`'s `.Content`: for `SHA256SUMS` that is a byte array, which printed as one decimal
  per line, and for text it re-encoded into the console code page (CP936 on zh-CN Windows) while we
  decoded UTF-8. The raw response bytes are passed through now. Separately, one line in the manifest
  that had no whitespace used to abandon the whole file, and the error then claimed the asset "is not
  listed" — unreadable lines are skipped, an asset that is genuinely absent is still refused.
- **macOS and Linux find their installer again.** Asset matching is decided by extension first, because
  the shipped file names carry no OS token at all (an aarch64 dmg, an amd64 deb); the check
  also no longer falls back to a package for a different architecture, which used to look like success
  and then not install. The choice is a pure function now, so all three platforms' rules are covered by
  `cargo test` on whichever host runs it.
- **`<html lang>` follows the language switch.** The editor asked the backend for it on every repaint and
  never on the broadcast that announces a new language, so the attribute kept the language the window
  opened with; screen readers and CJK glyph selection read that attribute.

## [1.0.3] - 2026-09-28

### Added

- **Tray popup: an "Automation" button, and the running version in the title.** The automation
  editor used to be reachable only from the software-settings window, and "which build is this"
  needed that window too
- **Double-click the tray icon opens the automation editor** (the popup closes first); a single
  click still opens and closes the popup
- **Settings: "Follow system" language, which is also what a first run does.** The system's own UI
  language is read once at startup — AppleLanguages on macOS, the user interface language on
  Windows, LANGUAGE / LC_ALL / LANG on Linux — and a language NetSense has no dictionary for still
  ends up in English. Picking a language from the list pins it
- **Settings: the software settings file gets its own "Open folder" button.** It opens the directory
  settings.json lives in — usually the same folder the automation config button opens, but not
  always: a config read from next to the executable is a different place
- **Editor: "Any adapter" as an interface condition value.** It holds as soon as one ordinary NIC
  is in use, so a profile no longer has to name a card the dock may take away
- **Settings: export and restore a backup.** One JSON file holds the automation config, the
  software config and the scripts in the trusted folder. A restore parses it and runs `validate()`
  before touching the disk, and writes the files it replaces into that same folder first
- **Settings: an exit choice for update traffic.** The update check and the installer download are
  the only two requests this app makes, and each can now go direct, follow the operating system's own
  proxy, or use one address you type in. "Follow system" is asked at the moment of the request rather
  than cached, and goes direct when it finds nothing; "Direct" sends an explicit no-proxy flag, so a
  proxy left behind in the environment is bypassed too. A typed address counts only as
  scheme://host[:port] in one of the six forms curl acts on, and one it cannot recognise is stored as
  direct — never as "follow system"
- **Settings: an Updates card.** It runs the same three commands the panel's update modal runs and shows the same
  verdict, so "can this update be installed here" has exactly one answer. The window never checks on its
  own - prompting once per session stays the panel's job
- **A light color scheme, and a switch for it: Follow system / Light / Dark.** The four windows now read
  one palette file (`theme.css`) — dark on `:root`, light on `html[data-theme="light"]` — and each window's
  own variables are only aliases of it, so no name in the markup had to change. Light is not dark inverted:
  chips and status badges take another ladder on a bright background (a pale one under dark text), and every
  piece of text was checked against the background it sits on. "Follow system" is resolved by the backend
  rather than by CSS — `ui_prefers_dark` asks the operating system once, and the answer travels on the
  `netsense://status` every window already listens to, so the four change together. `set_theme` stores the
  choice (`system` / `light` / `dark`) in settings.json — the choice, never the resolved value, which would
  start lying the moment the system flips on its own — and `get_theme` answers for a window just opened

### Changed

- **macOS: one authorization, then none.** On a machine with no passwordless channel, the first
  configuration change installs it inside the very dialog that change already had to raise: the prompt
  that used to repeat for every apply now writes the allow-list wrapper and its sudoers rule, and every
  later apply the allow-list can express goes through `sudo -n` without asking. When the channel cannot
  be staged (no usable temporary path, a login name that does not fit a sudoers token), that change is
  applied exactly as it was before. Settings → Runtime hands the access back (**Remove password-free
  changes**); the next apply asks once more and sets it up again
- **The tray popup scrolls as one panel.** Each section used to scroll inside itself, so the wheel
  moved only whatever the cursor was over
- **Windows and Linux: the popup is placed within the monitor's work area,** so it no longer covers
  the taskbar
- **The editor's current-network block follows every status broadcast,** adapter details included.
  It was read once when the window opened, so an address applied a moment later still showed up as
  the old one, and match states lagged until the window was reopened
- **Settings: the allowed-scripts card says what it guards.** It names the two trusted places,
  prints the real path of the scripts folder, and states that the backend re-checks every run_script
  — engine-fired or applied by hand — since a config file you import must not run anything alone
- **The current-network block lost its "Network Hardware" row:** it repeated interfaces the rows
  above already name. VPN tunnels keep their own row
- **One dark palette across all four windows.** The automation editor sat on its own colour set, so
  one app looked like two of them: panels, borders, text, buttons and status chips now read from a
  single palette, and every window tells the operating system it is dark — so a native dropdown
  list, scrollbar or checkbox no longer drops a light rectangle into a dark window. Placeholder text
  and the keyboard focus ring are visible in every window now, too

### Fixed

- **Editor: the interface dropdown lists the adapters this machine carries, not only the ones with
  a cable in them** — unplugged, it offered a single entry. The empty "—" choice, which could never
  be saved, is now a real placeholder plus "Any adapter"
- **macOS: the SSID dropdown lists the networks this system has saved.** The command that reads
  them wants a device name (en0) rather than a network service name ("Wi-Fi") and was being handed
  the latter, so the list came back empty
- **Windows: a machine that cannot use the resident elevation helper no longer pays for the attempt.**
  Reaching for that helper costs a UAC prompt of its own, so a channel that never answers used to be
  re-tried — and re-prompted — at every apply, with a fallback nobody could explain. Two failed batches
  now close the channel for the rest of the session, and the first one records in the log what it hit
- **Windows: the elevation helper's pipe can now be reached by the app that spawned it.** The helper
  runs elevated, and a named pipe inherits its creator's integrity level, so the *non-elevated* GUI of
  that same user — the account the pipe's own DACL had just admitted — was refused write access to it.
  The authorization was spent, the pipe was listening, and the batch still went the long way round, one
  prompt per apply. The pipe now carries an explicit Medium label: level with the app, still out of
  reach for a sandboxed process. This is the failure that matches "UAC every apply" on a machine where
  the helper does come up; the log line above names any other one

## [1.0.2] - 2026-09-26

### Added

- **macOS Wi-Fi SSID via CoreWLAN.** macOS 15.6+ redacts the SSID from every CLI NetSense used to
  read it (`networksetup` / `ipconfig` / `system_profiler`), so the current network showed as
  unknown. NetSense now asks CoreWLAN directly, requesting Location permission once (Profiles do
  need the network name); until that is granted the old CLI fallback chain still runs, so nothing
  gets worse
- **Windows: UAC now asked once per app run** for network config. The first apply elevates a
  resident helper — the same executable, re-launched once via UAC, taking batches over an
  owner-only named pipe that only accepts work from that very executable. Declining the prompt, or
  losing the helper, falls back to the previous per-batch prompt; elevated **user scripts** keep
  asking every run on purpose

### Changed

- The elevated batch path now returns the real command error text (e.g. what `netsh` complained)
  instead of just a bare exit code

### Fixed

- **Windows printer list: a queue's Location no longer masquerades as its name.** While parsing
  `Win32_Printer` rows, an empty Comment column was dropped and the Location shifted up into the
  description slot — a front-desk queue could be displayed as just its location. Rows are now
  taken by column position

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
