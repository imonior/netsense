# frontend

NetSense 的界面资源（纯静态 HTML/CSS/JS，无构建步骤，由 `tauri.conf.json` 的
`frontendDist = ../frontend` 直接引用）。

## 文件与承载窗口

| 文件 | 承载窗口（label） | 作用 |
|------|------------------|------|
| `popup.html` | `popup` | **状态栏弹窗面板**（主入口）：当前网络 + Active/Conflict 状态 + 网卡/VPN 清单 + Profile 快速切换（带实时状态徽标）+ 语言 + 在线升级 |
| `editor.html` | `main` | **配置编辑器**：侧栏 Profile / 兜底 / 全局白名单 · 「条件」列（Rules × Conditions + 实时三态）· 「3A 网络」列 · 「3B 动作」列，THEN/ELSE 用页签切换 |
| `serve.sh` | — | 开发期静态服务器（`http://localhost:1420`），对应 `beforeDevCommand` |

编辑器的界面结构是**有意的**，改动前先读 `DEVELOPMENT.md` §5–§8：Rules 之间 OR、Rule 内
**已启用** Conditions 之间 AND、禁用条件永不算命中、`""` 与「字段缺失」含义不同
（DNS 因此是三态下拉而不是一个输入框：「保持不变」= 删掉 `dns` 键，「自动获取」= 空串，
两端都按这个口径解释，见 §6）。
把这些语义「简化」掉是本项目最容易复发的一类倒退。

## 与后端的通信

- 调用：`window.__TAURI__.core.invoke(cmd, args)`（依赖 `app.withGlobalTauri = true`）。
- 文案：`get_strings` 一次性拉取当前语言的全部 key（5 语 × 211 key，见 `src-tauri/src/i18n/`），
  前端用 `t(key, vars)` 查表；语言切换调 `set_language`。**模板里不内嵌任何文案对象。**
- 命令返回：返回**大对象快照**的四条命令（`get_status` / `get_engine_status` / `get_interfaces`
  / `get_config`）给出 JSON 字符串，前端自己 `JSON.parse`；其余命令返回结构化值。这条分界
  与「是否采样网络」无关：`get_engine_status` 与 `get_config` 不采样也返回字符串，
  `get_networks` 读的是系统里的已保存网络，却返回数组。要判断某条命令的形状，看 `ipc.rs`
  的返回类型，别看命名。

### 事件（后端 → 前端）

| 事件 | payload | 何时 |
|------|---------|------|
| `netsense://status` | `status_payload`（见下） | 每次评估结束、配置落盘/热重载、语言切换、动作完成 |
| `netsense://evaluation` | `EngineView` | 引擎刚做完一轮评估（编辑器据此刷新徽标），3B1 每交回**一条**动作结果，以及 3B2 worker 每报出一次**状态变化**（连续两次相同结果不会重发） |
| `netsense://conflict` | `{ profiles: [name] }` | **每个冲突事件只发一次**（引擎按 Profile 的 **id 集合**签名去重，换了名字也不重发；payload 里给的是展示名），前端弹窗 |
| `netsense://action` | `{ kind, ok, message }` | `kind` 为 `apply` / `dhcp` / `probe`（这三条来自手动入口 —— 命令本身只是把请求排进队列）或 `monitor`（健康度监测自动回落 DHCP，没有人点过任何按钮）。**逐条动作的进度不在这里**，在 `evaluation` 的 `last_run` |
| `netsense://update_progress` | `{ phase, percent }` | 在线升级：`download` / `refresh` / `install` |

编辑器只在**会换掉整片字段**的变化后重建表单：切换选中项、增删条目、保存成功、切 THEN/ELSE
页签，以及改动 `mode` / `v6mode` / 动作类型这几类下拉和 DNS 三态开关；广播只换徽标与侧栏，
从不碰表单 DOM —— 否则用户正在输入的框会被自己填的内容刷掉。

### `status_payload`（`get_status` 与 `netsense://status` 同一形状）

```jsonc
{
  "status":  { "connected": true, "ssid": "...", "ipv4": "...", "dns": "...", "rssi": -50, ... },
  "engine":  {                                   // EngineView：界面状态的唯一来源
    "state": { "state": "active", "id": "office" }  // | {"state":"conflict","ids":[..]} | {"state":"no_active_profile"}
    "active": "office",                          // 冲突/零命中时省略
    "conflict": ["HomeWiFi", "Office_5G"],       // 展示名
    "snapshot": { "ssid": "...", "gateway_mac": "...", "bssid": "...",
                  "primary_interface": "en0", "interfaces": [...], "tunnels": [...] },
    "profiles": [ { "id": "...", "name": "...", "enabled": true, "matched": true,
                    "status": "active|not_matched|conflict|disabled|error",
                    "error": "…",                 // 仅 error 时
                    "rules": [ { "id":"r1", "status":"match|no_match|inactive",
                                 "conditions":[{"id":"c1","kind":"wifi_ssid","value":"…","status":"match|no_match|inactive"}] } ] } ],
    "last_run": {                                 // 最近一次「走到 3B」的执行；没走到过就省略
      "profile_id": "office", "profile": "Office_5G", "branch": "then|else",
      "at": 1770000000, "three_a": "applied|skipped",
      "running": true,                           // true = 还有动作在后台跑
      "status": null,                            // null | empty | success | partial | failed
      "total": 2,                                // 提交时就定下的分母，运行中靠它报「1/2」
      "outcomes": [ { "id": "a1", "label": "…", "priority": 1, "ok": true,
                      "error": "…" } ] },          // error 只在失败时出现，成功的那条没有这个键
    "workers": [                                  // 3B2 此刻在维持什么；没有 worker 时是空数组，不会省略
      { "id": "p1", "label": "wireguard:wg0", "priority": 1,
        "state": "pending|satisfied|repaired|faulted|overdue",
        "interval": 15, "repairs": 2, "at": 1770000000,
        "error": "…" } ],                         // 同样：缺席 = 没有错误
    "warnings": [ "…else 分支配了 persistent 动作，这一支不会被维持…" ]
  },
  "profiles": [ { "id": "...", "name": "...", "enabled": true } ],  // 只是目录，状态看 engine
  "language": "en", "priv": "direct|prompt", "config_path": "..."
}
```

`CONFLICT`（条件层）与 `ERROR`（执行层）是两种不同颜色的状态，不要合并显示。
`ERROR` **只**来自 3A（网络下发/回读没过，或健康度监测回落 DHCP）；3B1 跑成 `partial` / `failed`
不改 `profiles[].status`，Profile 依然是 `active` —— 网络配置确实落地了，动作失败只是要在
`last_run` 里说清楚的一条信息。所以 `last_run.status` 与 `DisplayStatus` 是两个维度，别拿一个去算另一个。
`workers[]` 同理：一条 `faulted` 的常驻动作说的是「有个东西没维持住」，不是「环境错了」，
所以它只染徽标、不动状态。`pending`（worker 起了、第一次核对还没回）由后端报，前端不替它补；
表单里有这条动作、`workers[]` 里却没有 = 引擎此刻没在维持它，这时徽标留空。

### 后端命令一览（`src-tauri/src/ipc.rs`，20 条）

| 命令 | 入参 | 说明 |
|------|------|------|
| `get_status` | — | 上面的 `status_payload`（JSON 字符串） |
| `get_engine_status` | — | 只取 `EngineView`（JSON 字符串），**不**重新采样，补齐广播漏掉的窗口 |
| `get_interfaces` | — | 全部在用网卡（有线/无线/VPN），平台层 TTL 缓存（JSON 字符串） |
| `get_config` | — | 全量配置 JSON（schema 1），编辑器回填用 |
| `save_profile` | `payload: Profile`（JSON 字符串） | 按 `id` 新增或替换一个 Profile；**整份配置**校验通过才落盘 |
| `save_global` | `payload: {fallback, allowed_scripts}` | 保存 Profile 之外的全局项（零命中兜底 + 脚本白名单）。兜底**不是** Profile：它没有条件，不参与匹配与冲突 |
| `delete_profile` | `id` | 删除并按 id 找不到时报错 |
| `apply_profile` | `id` | 请求「立即应用」。**不绕过条件**：引擎重评该 Profile 自己的 Rules/Conditions，禁用中直接拒、冲突中拒绝并回 `netsense://action` |
| `force_dhcp` | — | 把当前网络切回 DHCP（异步，结果走 `netsense://action`） |
| `probe_network` | — | 手动探测（同上） |
| `get_networks` | — | 系统已保存的无线网络列表（侧栏「尚未配置的网络」） |
| `set_language` | `code` | 切换 UI 语言并写回配置（由配置热重载那一次统一广播） |
| `get_strings` | — | 当前语言全部文案（一次拉全，避免逐 key 往返） |
| `open_editor` / `close_editor` | — | 显示 / 隐藏编辑器（隐藏 = 收回菜单栏常驻） |
| `open_logs` | — | 用系统默认程序打开**当前实际使用**的日志目录 |
| `open_url` | `url` | 打开 URL（Release 页 / 下载链接） |
| `check_update` | — | 查 GitHub 最新版、挑本平台安装包、判断 Homebrew 渠道，并给出「这次能不能就地装」的判定（`installable` / `install_note`） |
| `run_update` | `target`（`check_update` 给出的下载信息，JSON 字符串） | 下载 + 校验 + 安装；进度走 `netsense://update_progress` |
| `quit_app` | — | 停监视、退出 |

## 验证方式

`cargo test` 里有一条 `config::tests::action_tags_the_editor_emits_are_the_tags_serde_accepts`
专门盯前端与 serde 的标签字符串（例如 `keep_vpn_connected` 的 snake_case 不是 `keep_vpn`）：
这类错法的表现是「保存报错 / 整份配置加载失败」，而不是某个字段没生效，排查成本很高。

想在不启动 Tauri 的情况下验渲染与序列化，这套 harness 已经入库：`scripts/editor-smoke.mjs` 用 Node 起
`vm` 上下文加载 `<script>`，stub 一个最小 DOM + `window.__TAURI__`（`get_config` 喂
`config.example.json`、`get_status` 喂一份冲突态 `EngineView`），驱动点击/输入后断言那些只有跑起来
才看得见的绑定规则（dns 的三态、禁用动作不进清单、常驻动作的字段要原样回到 payload 里、
worker 徽标只在 Active 方案的 THEN 分支出现、广播不重建用户正在输入的表单）。
`--write-fixtures` 再把它这一次真正发出的 `save_profile` / `save_global` 载荷写成一个文件，
交给 `config::tests::payloads_the_editor_actually_sends_are_the_ones_serde_accepts`，
沿 `ipc.rs` 的 upsert + validate 路径跑一遍 —— 从前端到 serde 的完整链路，不需要任何平台权限。
CI 两处都跑：`validate` job 只跑断言（几秒，免得四条 build leg 各自编译完依赖才发现问题），
每个 build leg 先生成载荷、再 `cargo test`。

## 配置的加载口径

顶层 `schema` 必须是 `1`：字段缺失、或者不是这个数，都不加载 —— 报错里直接写明该改哪里。
不做自动迁移：一份解释不出来的配置，它对「多个 Profile 同时命中」的解释可能和这一版相反，
凭猜出来的语义往网卡上写静态 IP 比停下来危险。编辑器因此从不自己拼 `schema`：它每次只提交**一条**
载荷 —— 一个 Profile（`save_profile`），或 `{ fallback, allowed_scripts }`（`save_global`）—— 由
`ipc.rs` 把它 upsert 进内存里那份已加载的配置，落盘时再统一由 `Config::save` 盖上 `schema`。
整份配置从不从前端往返。
