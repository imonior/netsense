# frontend

NetSense 的界面资源（纯静态 HTML/CSS/JS，无构建步骤，由 `tauri.conf.json` 的
`frontendDist = ../frontend` 直接引用）。

## 文件与承载窗口

| 文件 | 承载窗口（label） | 作用 |
|------|------------------|------|
| `popup.html` | `popup` | **状态栏弹窗面板**（主入口）：当前网络状态 + profile 快速切换 + 语言 + 打开编辑器/退出 |
| `editor.html` | `main` | 配置编辑器：匹配条件（SSID / 网关 MAC / BSSID / priority）、IPv4、IPv6、DNS |
| `index.html` | （未挂载） | P1 阶段的诊断控制台，保留用于排查；如需临时启用，把某个窗口的 `url` 指过来即可 |
| `serve.sh` | — | 开发期静态服务器（`http://localhost:1420`），对应 `beforeDevCommand` |

## 与后端的通信

- 调用：`window.__TAURI__.core.invoke(cmd, args)`（依赖 `app.withGlobalTauri = true`）。
- 订阅：`window.__TAURI__.event.listen('netsense://status', cb)`。
  后端在「应用 profile / SSID 变化 / 配置热重载 / 切换语言 / 增删配置」后广播该事件，
  前端据此刷新，**不需要轮询**。
- 文案：`get_strings` 一次性拉取当前语言的全部 key（5 语，见 `src-tauri/src/i18n/`），
  前端用 `t(key)` 查表；语言切换调 `set_language`。

### 后端命令一览（`src-tauri/src/ipc.rs`）

| 命令 | 入参 | 说明 |
|------|------|------|
| `get_status` | — | 网络状态 + 当前 profile + profile 名列表 + 语言 + 提权通道（`direct` = 免密/已提权，`prompt` = 每次授权） |
| `get_config` | — | 全量配置（含每个 profile 的所有字段），供编辑器回填 |
| `save_scene` | `payload: {name, profile}` | 新增/更新 profile（校验通过才落盘） |
| `delete_profile` | `name` | 删除 profile |
| `force_apply` | `payload: {name}` | 强制把某 profile 应用到当前网卡 |
| `get_networks` | — | 系统已保存的无线网络列表（编辑器左侧「未配置网络」） |
| `set_language` | `code` | 切换 UI 语言并写回配置 |
| `get_strings` | — | 当前语言全部文案 |
| `open_editor` / `close_editor` | — | 显示 / 隐藏编辑器窗口（隐藏 = 收回菜单栏常驻） |
| `quit_app` | — | 停止 SSID 监视并退出 |

## 从 `hammerspoon_wifi_switcher` 迁移的改动

原模板是「Lua 拼模板字符串 + `hammerspoon://` URL scheme 回调」模式，移植时做了三类改动：

1. **回调**：`callLua('save_wifi_scene', data)` → `invoke('save_scene', { payload })`；
   全部 `hammerspoon://xxx` 链接删除。
2. **数据注入**：原来的 `%NETWORKS_PLACEHOLDER%` / `%CONFIG_PLACEHOLDER%` /
   `%LOCALE_PLACEHOLDER%` 占位符改为启动时 `get_networks` / `get_config` / `get_strings` 异步拉取。
3. **推送模型**：原来的 `updateCurrentNetworkUI()` 由 Lua 主动调用，改为 `listen('netsense://status')`；
   编辑器的重试轮询（原 `maxRetries = 5`）也随之删除。

另外新增了原模板没有的**匹配条件编辑区**（SSID / 网关 MAC / BSSID / 优先级），
这是本项目相对原工具的核心差异。

## 已知待办

- 健康度（probe 参数）与自动化任务（route/launch/run + 开关）尚未进入编辑器 UI，
  目前只能在 `config.json` 里手写；下一轮补。
- `popups.html`（日志查看弹窗）尚未移植，日志当前通过托盘菜单「打开日志目录」查看。
