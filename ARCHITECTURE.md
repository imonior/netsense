# NetSense Architecture

**English / 简体中文** — this single file is the canonical source of truth (both languages inline).

> **Canonical source of truth.** This file — and *only* this file — defines NetSense's  
> architecture, module boundaries, and the invariants the code must hold. README files describe the  
> product for users; `DEVELOPMENT.md` describes how to build, run, and test locally. If any of those  
> three disagree, this file wins.

---

## 1. Scope and the frozen core model

NetSense is a **single-binary, tray-resident network Profile manager**. It evaluates per-network  
Profiles against the live network, keeps *exactly one* of them Active, and pushes the resulting  
configuration through the OS's own tooling (`networksetup` on macOS, PowerShell CIM + `netsh` on  
Windows, `nmcli` on Linux).

### 1.1 What is deliberately out of scope (frozen)

These decisions are settled. Do not reopen them in a refactor:

- **No Profile priority.** With 2+ matches the only honest answer is *Conflict*; NetSense applies  
  nothing rather than silently picking a winner.
- **No async rewrite of the engine.** The engine is a single-threaded state machine driven by one  
  `mpsc` channel; concurrency is modeled with worker threads for long-running 3B actions only.
- **No framework swap (Tauri → Wails, or vice versa).** The UI and IPC contract are bound to Tauri v2.
- **No schema-incompatible config changes** without a migration path (see §9).

### 1.2 范围与冻结的核心模型（简体中文）

NetSense 是一个**单二进制、常驻系统托盘的网络 Profile 管理器**。它把按网络划分的 Profile 对  
照实时网络求值，始终保持**恰好一个** Profile 处于 Active，并通过操作系统自带工具  
（macOS 的 `networksetup`、Windows 的 PowerShell CIM + `netsh`、Linux 的 `nmcli`）下发配置。

以下决定已经定案，重构时不得推翻：

- **没有 Profile 优先级。** 多于一个命中时，唯一诚实的结论是 *Conflict*；NetSense 不二选一，  
  而是什么都不下发。
- **引擎不做 async 重写。** 引擎是由单条 `mpsc` 通道驱动的单线程状态机；只有在 3B 长耗时动作  
  上才用 worker 线程建模并发。
- **不更换框架（Tauri ↔ Wails 互转）。** UI 与 IPC 契约绑定在 Tauri v2。
- **不引入破坏兼容性的配置变更**，除非附带迁移路径（见 §9）。

---

## 2. Module map

```
main.rs            Assembly only: resolve paths → build AppState → spawn threads → wire callbacks.
state.rs           AppState (shared, locked) + the single publish_status() broadcast path.
engine.rs          The state machine: Msg channel, decide(), 3A/3B orchestration, DisplayStatus.
conditions/        Pure evaluation. NetworkSnapshot + Rules/Conditions → 3-state result. No side effects.
detection/         Scheduler: when to re-evaluate (fingerprint diff + per-Profile poll/change_delay).
network/           3A layer: apply_3a() snapshot→apply→routes→readback→rollback; HealthMonitor; readback.
platform/          Platform Abstraction Layer (PAL): NetworkPlatform trait + cfg-selected impls + Mock.
automation/        3B layer: one_shot, persistent workers, provider (action semantics), AllowedScripts.
config/            Config + AppConfig models, schema, load/save (fsync + rotating backup).
i18n/              Key-based localization (en/ja/ko/zh-TW/zh).
ipc.rs, tray.rs, popup.rs, update.rs, netproxy.rs, backup.rs, log.rs, paths.rs, win_dialog.rs
```

### 2.1 模块地图（简体中文）

各模块职责如上。关键约束：**`conditions/` 无副作用**（只读快照、只算匹配）；平台 I/O 一律在  
不持锁时做；广播路径只有 `state::publish_status()` 一条。

---

## 3. The engine state machine

All state mutation happens **only on the engine thread**. Every external entry point — IPC command,  
config hot-reload, tray callback, quit — can *only* post a `Msg` into the engine's channel. This is  
what makes "at most one Active Profile at a time" provable rather than hoped-for.

### 3.1 Message protocol

```rust
pub enum Msg {
    Wake,                       // something changed → run one pass
    Apply { id: String },       // user clicked "apply now"
    Action { kind: ActionKind },// SetDhcp | Probe (privileged / seconds-long → must be on engine thread)
    RunStep { seq, outcome },   // one 3B1 action reported back
    RunDone { seq, report },    // a 3B1 batch finished
    Worker { update },          // a persistent worker's status changed
    Quit,
}
```

### 3.2 The decision pipeline (one pass = `engine::pass`)

```
network snapshot ──▶ evaluate_all(profiles, snapshot)
        │
        ├─ 0 matches ──▶ global Fallback (not a Profile) ──▶ 3A (if fallback.network set)
        ├─ 1 match   ──▶ that Profile is Active ───────────▶ 3A → 3B
        └─ 2+ matches ─▶ Conflict (apply nothing, show dialog)
```

Hard rules (violating any is a correctness bug, not a style choice):

1. **Multiple matches = Conflict; never auto-select.** Profiles have no order and no "more specific  
   wins" — silently picking one hides a real user misconfiguration.
2. **While Active, do not re-run 3A/3B1** unless that Profile's *content* actually changed; content  
   change re-applies 3A *without* actions.
3. **3A failure → zero 3B actions run**; the Profile is marked `Error`.
4. **Only the latest 3B1 run counts.** A new submit *supersedes* (does not kill) the still-running  
   one — we cannot safely terminate the user's processes; the old run finishes but its report is  
   discarded on return.

### 3.3 DisplayStatus vs the system Decision

`DisplayStatus` (UI-facing: `Active` / `NotMatched` / `Disabled` / `Conflict` / `Error` / `Suspended`)  
must be kept strictly distinct from the system-level `Decision` (`NoActiveProfile` / `Active` /  
`Conflict`). **Conflict ≠ Error**: the former is a *condition* problem (fix your rules); the latter is  
an *execution* problem (check logs/permissions).

### 3.4 引擎状态机（简体中文）

所有状态变更**只发生在引擎线程**。每个外部入口都只能通过往引擎通道投递 `Msg` 来请求。这是  
"同一时刻最多一个 Active Profile" 成立的前提。`engine::pass` 的单轮流程与四条硬规则如上。  
`DisplayStatus` 与系统级 `Decision` 必须严格区分：Conflict 是条件层问题，Error 是执行层问题。

---

## 4. Platform Abstraction Layer (PAL)

The platform layer is the only place that touches OS-specific APIs. Three things matter:

### 4.1 `NetworkPlatform` trait

Every platform implements `trait NetworkPlatform`, exposing: `get_status()`, `get_routes()`,  
`apply_network(&NetworkConfig)`, `add_route` / `delete_route`, `restore_from_snapshot(&Snapshot)`,  
`set_dhcp`, and probe helpers. `validate.py` is a **hard CI gate** that greps for the textual  
`trait NetworkPlatform { fn <method>` definitions inside each of `macos.rs` / `windows.rs` /  
`linux.rs` — trait *defaults* do not satisfy it, so each PAL must define every method explicitly.

### 4.2 `Platform` is a compile-time alias, not a trait object

```rust
// platform.rs
#[cfg(not(feature = "engine-mock"))]
pub use crate::platform::macos::MacPlatform as Platform;     // etc., per cfg(target_os)
#[cfg(feature = "engine-mock")]
pub use crate::platform::mock::MockPlatform as Platform;
```

Upper layers depend only on `Platform` and the `NetworkPlatform` trait. There is **no `dyn`** and no  
runtime dispatch — module boundaries have zero runtime cost. Each PAL is gated by  
`cfg(target_os = "...")` and its OS-only dependencies are declared under the matching  
`[target.'cfg(...)'.dependencies]` in `Cargo.toml`, so a Linux build never compiles the Windows  
module or pulls `windows-sys`.

### 4.3 `engine-mock` feature (test-only seam)

`cargo test --features engine-mock` swaps `Platform` for `MockPlatform`, which records every call  
into a `Call` log and lets tests inject `InterfaceStatus` / pre-existing routes. Production code is  
**byte-identical** with the feature off. `AppState::assembled` constructs `plat: Platform::default()`  
only under the feature; otherwise it takes the real `plat` argument. See §8 for the tests it enables.

### 4.4 平台抽象层（简体中文）

平台层是唯一接触 OS 专属 API 的地方。`NetworkPlatform` trait 的每个方法都必须在三个真实 PAL 里  
**显式定义**（`validate.py` 门禁靠正则核对，trait 默认值不算数）。`Platform` 是**编译期别名**  
而非 trait object，模块边界零运行时开销。`engine-mock` 特性只在测试时把 `Platform` 换成  
`MockPlatform`，关闭时生产代码路径零改动。

---

## 5. The 3A transaction and rollback

`network::apply_3a<P: NetworkPlatform>(plat, cfg)` is a single barrier. Command success ≠ config  
applied: `networksetup` / `netsh` return `0` *silently* when authorization is denied or the NIC just  
disconnected. So 3A's criterion is **read-back of actual system state**, not the exit code.

```
1. snapshot before = get_status() + get_routes()        // includes target NIC + pre-existing routes
2. apply_network(cfg)
     └─ Err → restore_from_snapshot(&before); return Failed
3. for each route: add_route / delete_route
     └─ Err → delete added routes; restore_from_snapshot(&before); return Failed
4. if cfg.verify.readback: readback_match() up to READBACK_ATTEMPTS (settle 800ms each)
     └─ still mismatched → restore_from_snapshot(&before); return Failed
5. return Applied
```

`restore_from_snapshot` restores both the network config **and the pre-existing routes on the same  
NIC** — a failed apply must never strip a third-party route the user already had. `Stage3A::Failed`  
is the **only** result that blocks 3B (rule 3 in §3.2).

### 5.1 3A 事务与回滚（简体中文）

3A 是一道单一屏障。命令返回 `Ok` 不等于配置写进去了，所以判据是**回读实际系统状态**。失败时  
`restore_from_snapshot` 同时还原网络配置**与目标网卡上已有的第三方路由**，绝不可因一次失败下发  
而把用户原有的路由清掉。`Stage3A::Failed` 是唯一会阻断 3B 的结果。

---

## 6. The decision invariant (unit-tested)

`engine::decide` must hold for **any** number of matches:

| matches | result             |
| ------- | ------------------ |
| 0       | `NoActiveProfile`  |
| 1       | `Active { id }`    |
| 2+      | `Conflict { ids }` |

This is covered by `decision_invariant_holds_for_any_match_count` (n = 0..=8) in `engine.rs`  
`mod integration_tests`.

### 6.1 判定不变量（简体中文）

`engine::decide` 对任意命中数都成立：0 → `NoActiveProfile`，1 → `Active`，2+ → `Conflict`（绝不  
选胜者）。该不变量由 `engine.rs` 的 `mod integration_tests` 单测覆盖。

---

## 7. Concurrency and locking discipline

Violating the lock order self-deadlocks — `std::sync::Mutex` is not reentrant.

- Order is always `engine` → `config`, and both are held **only inside `pass()`**.
- Platform I/O (sampling, apply, probe) runs **without holding any lock**.
- `publish_status` is called **only after all locks are released**.
- 3B1 one-shot actions run on a separate thread with an owned copy of (action list + allow-list);  
  they never touch engine/config locks or the engine thread.
- 3B2 persistent workers share the same post mechanism (`Msg::Worker`) but live with the Active  
  Profile: any new apply first stops the old worker group (otherwise "keep VPN up" from the old  
  environment would fight the new environment's routing table).

### 7.1 并发与加锁纪律（简体中文）


锁顺序恒为 `engine` → `config`，且只在 `pass()` 内部同时持有两者；平台 I/O 不持锁；广播只在锁
全部释放后调用；3B1/3B2 不碰引擎线程与锁。

---

## 8. Testing strategy

| Layer             | What                                                        | Command                                  |
|-------------------|-------------------------------------------------------------|------------------------------------------|
| Unit (existing)   | config model, conditions eval, detection scheduler          | `cargo test`                             |
| Integration (B)   | full Engine loop on `MockPlatform`: snapshot→decide→3A→3B   | `cargo test --features engine-mock`      |
| 3A rollback (B)   | failed apply restores pre-existing routes, records `Restore`| `cargo test --features engine-mock`      |
| Decision invariant| 0/1/2+ matches → correct `Decision`                         | `cargo test`                             |
| PAL gate          | every `NetworkPlatform` method defined in each real PAL     | `python3 scripts/validate.py`            |
| Lint              | no warnings on any target                                   | `cargo clippy --all-targets -- -D warnings` |
| Editor smoke      | frontend editor loads without console errors               | `node editor-smoke.mjs`                  |

The `engine-mock` integration tests (`mod mock_engine`) drive the real `pass()` loop end-to-end:
- `engine_loop_applies_matching_profile_through_mock` — a WifiSsid-matching Profile records
  `Call::ApplyNetwork`.
- `engine_loop_never_applies_on_conflict` — two equal-SSID Profiles → **no** `ApplyNetwork`,
  decision is `Conflict`.
- `engine_loop_applies_fallback_when_nothing_matches` — zero match with `FallbackConfig{network}`
  set → `ApplyNetwork` recorded.

### 8.1 测试策略（简体中文）

分层如上。`engine-mock` 集成测试（`mod mock_engine`）端到端驱动真实 `pass()` 循环，覆盖匹配下发、
Conflict 不下发、空命中走 Fallback 三条主链；3A rollback 测试验证失败下发会还原已有路由并记录
`Restore`。

---

## 9. Configuration persistence (fsync + rotating backup)

Both `Config::save` and `AppConfig::save` write **atomically and durably**:

1. `rotate_backup(path)` — copy the current file to `.{name}.{unix_seconds}.bak`, keep the last
   `BACKUP_KEEP = 5`, best-effort (a backup failure never blocks the real save).
2. Write to `{name}.part`, `f.write_all(...)`, then **`f.sync_all()`** so a crash/power-loss cannot
   leave `config.json` half-truncated.
3. `rename` the temp file over the target (atomic on POSIX/Windows).

### 9.1 配置持久化（fs体ync + 轮转备份，简体中文）

`Config::save` 与 `AppConfig::save` 都做原子且耐久的写：先轮转时间戳备份（保留最近 5 份，
best-effort），再写临时文件、`sync_all()` 落盘，最后 `rename` 原子覆盖，避免崩溃/掉电把配置截成
半截。

---

## 10. What lives where (anti-drift rules)

- **Product story, features, screenshots** → `README.md` (+ language variants).
- **Build / run / test / per-platform dev traps** → `DEVELOPMENT.md`.
- **Architecture, module boundaries, invariants** → this file (`ARCHITECTURE.md`). ← canonical.

If a doc states an architecture fact (e.g. "rollback is not implemented"), it is stale the moment the
code implements it; fix the code, then sync the doc here first, then `DEVELOPMENT.md`.

### 10.1 文档归属（防漂移，简体中文）

产品介绍归 README，开发/测试/构建归 DEVELOPMENT.md，架构与不变式归本文件（唯一真源）。任何文档
声称的架构事实一旦被代码实现即过时——先改代码，再先同步本文件，最后同步 DEVELOPMENT.md。
