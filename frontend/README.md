# frontend

NetSense 的界面资源（纯静态 HTML/CSS/JS，无构建步骤，由 `tauri.conf.json` 的
`frontendDist = ../frontend` 直接引用）。

## 文件与承载窗口

| 文件 | 承载窗口（label） | 作用 |
|------|------------------|------|
| `popup.html` | `popup` | **状态栏弹窗面板**（主入口，托盘唯一的交互面）：当前网络信息（含在用网卡明细）+ 其他在用网卡 + VPN/虚拟网卡（标题＝归属软件名，逐张标已连接/未连接、网络出口设备与网关或路由前缀，含装了但没连的条目）+ Profile 快速切换（带实时状态徽标）+ 在线升级 |
| `editor.html` | `main` | **配置编辑器**：三层 —— 顶部「当前状态」条（生效方案 / 命中它的条件 / 当前网络 / 生效的动作，四格与下面四列一一对应）+ 中部四列（Profile 列表含兜底 · 条件 · 3A 网络 · 3B 动作）+ 底部动作键。THEN 与 ELSE **同时呈现**（THEN 在上、ELSE 在下），校验按钮在「3A 网络」列标题里；兜底那一行是单个区块（没有 ELSE）：网络与 3B 都可配，动作只在零命中期间执行 |
| `settings.html` | `settings` | **软件设置**：界面语言（默认跟随系统的界面语言）· 界面配色（跟随系统 / 浅色 / 深色）· 开机启动（读系统实况）· 升级代理（直连 / 跟随系统 / 手填一个地址）· 检查更新与就地升级（和面板同一套判定，同三条命令）· 自动化配置与日志的位置（打开文件夹）· 脚本白名单 · 日志保留天数 · 备份（导出一份 / 从列表恢复） · 提权通道/平台/版本（macOS 且通道已装时可在这里撤销免密） |
| `logs.html` | `logs` | **日志窗口**：按天的日志文件下拉（新→旧）· 只读尾部若干行 · 级别上色 · 子串过滤 · 「跟随」定时刷新（窗口隐藏时不刷） |
| `theme.css` | —（四座窗口共用） | **唯一的颜色真相来源**：`--t-*` 一套 token 写两遍 —— 深色在 `:root`，浅色在 `html[data-theme="light"]`，加上一条 `color-scheme`；四座窗口的内联样式只引用它，不再各自写字面值 |
| `serve.sh` | — | 开发期静态服务器（`http://localhost:1420`），对应 `beforeDevCommand` |

编辑器的界面结构是**有意的**，改动前先读 `DEVELOPMENT.md` §5–§8：Rules 之间 OR、Rule 内
**已启用** Conditions 之间 AND、禁用条件永不算命中、`""` 与「字段缺失」含义不同
（DNS 因此是三态下拉而不是一个输入框：「保持不变」= 删掉 `dns` 键，「自动获取」= 空串，
两端都按这个口径解释，见 §6）。
把这些语义「简化」掉是本项目最容易复发的一类倒退。

另有五条是版面，但同样是有意的取舍：
- **顶部四格与下面四列是同一份事实的两个视图**（`EngineView` 出发的 `renderStrip()` 与
  各列的徽标必须同口径）。所以「生效的那一行发绿」这件事，徽标和行描边要一起跟着广播走 ——
  只更新其中一个就会分裂：用户看到的行说 A 在生效，状态条说 B。
- **「命中」与「生效」是两个问题，行描边只回答后者**：第 2 列的三态徽标（以及第 1 列里
  `not_matched` 那几行被预览升级出来的 `MATCH`）走 `preview_match`，它按**表单当前内容**（未保存的草稿
  也算）用引擎**已采到**的那份快照重算，所以填完最后一个字符就变色，不用等引擎那一轮
  （采样节律 + `change_delay_secs` 去抖，最坏 8 秒以上）。行的绿框与 `ACTIVE`/`CONFLICT`/`ERROR`/`DISABLED`
  仍然只由引擎说 —— 一份填对了的表单既不代表它落过盘，更不代表它被执行过。这两处故意不同源，
  动预览的时候别顺手把绿框也接上去。
- **条件值来自系统实况**：SSID 绑定的是**输入框**，候选挂在它右端那颗箭头下面（`get_networks`：
  系统里保存过的无线网络）—— 一个组合框，不是行里的两块。浮层不写字段，它选中什么就抄进
  输入框什么，然后收起；展开与否记在 class 上，因为表单随时会被整块重画。隐藏网络与
  「还没连过第二次」的环境要能提前配好，所以输入才是主控件；反过来，若值绑在候选上，
  「候选控件自己的状态」就有机会被当成配置值存出去。
  Windows 这一列交出来的是**空中那个名字**，与面板「当前网络」那一格同源：那里还有一层系统存的
  profile 名，SSID 撞名时它会多出「 2」，而那是这台机器上的档案名，不是网络的名字 —— 三台机器
  对着的必须是同一个网络，否则在 mac 上写好的条件搬过去永远不会命中。**改过一次口径，代价写在
  这里**：从前在 Windows 上按带「 2」的名字存进条件的，现在不再命中，要去下拉重新选一次。
  接口是只能选的下拉（`get_adapters`：本机装着的网卡，**含现在没插线、没连上的口**；
  VPN/虚拟网卡不进候选，因为条件比对的集合里根本没有它们）。下拉里另有一条「任意网卡」，
  存的是通配值 `*`（任何一张在用的普通网卡都算命中）。读得到的值才给「用作条件」按钮 ——
  把读不到的值写进条件，等于凭空造一条永远不成立的条件。同一个口径也管动作的目标：
  「设为默认打印机」的候选来自 `get_printers`（显示「说明 · 位置」，当前默认那台带标注），
  且只能从清单里选 —— 队列名打错，就等于给一台不存在的打印机下发配置；机器上还没有
  那台共享打印机时，先连上对应网络再回来配。
  「启动程序」有些相反：**手输是主路**（绿色免安装的单文件程序不在任何菜单里），
  候选（`get_installed_apps`：macOS 的 /Applications、Windows 的开始菜单、Linux 的 XDG 目录；
  按名字列，选中的是路径）挂在同一个组合框里，旁边再给一颗「浏览…」打开系统文件选择器
  （`pick_app`；取消什么都不写，选择器起不来把原因原文说出来）。三个入口写的是同一个字段。
- **THEN 与 ELSE 在同一列里上下并列**（没有页签）：两支同时可见，改一支不会牵动另一支，
  也让人看见「ELSE 那支还配了常驻动作」这类互相打架的配置。
- **3B 两张列表的顺序就是执行的顺序**（3B1 逐条跑，前一条结束才轮到下一条；3B2 只决定 worker
  谁先起）。改顺序只有一个入口：卡头上的 ↑ / ↓，动的就是同一个数组。跨支、跨类的移动在控件层面
  就做不到 —— ↑/↓ 的 `data-pre` 与 `data-kind` 把它限定在同一支的同一张列表里，不需要额外的拒绝逻辑。
- **状态条第 3 格（当前网络）有两份来源，各自按自己的代价更新**：地址、掩码、网关、DNS 来自
  `status` —— 它就在每一条 `netsense://status` 里，引擎每轮都带一份新采样过来，所以
  这一格天然新鲜，不需要为它再问后端一次；接口标签和这张网卡自己的 MAC 只能问 `get_interfaces`
  （一次子进程），而它们只随「连着的是哪张口、关联到哪个 AP」变化，于是判据用 `EngineView` 的
  身份指纹：指纹不动就不取。反过来，每条广播都取一次会把编辑器拖进引擎的节律里 —— 一轮下发
  连发两条事件，界面就跟着每秒拉一次子进程，而这正是「刷新很慢」的来源之一。

配色也同理：四座窗口共用 `theme.css` 那一套 `--t-*` token，**里面没有一个字面颜色** ——
面、字、状态点、徽标、浮层各有一档，深色写在 `:root`，浅色写在 `html[data-theme="light"]`，
两套各自声明自己的 `color-scheme`（不声明那一句，原生下拉弹层、滚动条、复选框会往深色的窗口里
嵌一块浅色底，或者反过来，Windows 上最明显）。`editor.html` 的 token **名字**（`--bg-panel`、
`--btn-primary` 这一批）与另外三扇不同，只是在内联 `:root` 里别名到 `--t-*`；markup 与
`scripts/editor-smoke.mjs` 都按名字引用，所以改名不如改值。

浅色不是深色的反相：徽标、状态行、路径块这类「底色 + 底上的字」成对出现的东西，在亮底上要换
另一档（深底是压暗的色相配亮字，亮底是淡底配深字），所以每一处成对取值都取 `--t-tint-*` 与
`--t-tint-*-fg` 那一对，不单独挑颜色。文字与它所在的那块底至少差 4.5:1，两套按同一句话核对过
（占位符除外 —— 它是提示不是内容）。

这一句现在是检查而不是注释：`scripts/validate.py` 的第 [12] 组把那些成对的 token 逐项实算比值
（深、浅两档各算一次，取两者里更低的那个），不达标就红。写下这一段之前它只是注释，所以深色档里
有五处掉到 3.7~4.4 而没人发现。四类东西它查不到，别把这一组当视觉验收：半透明的底（toast、选中、
遮罩要先和背后的底合成才知道是什么值）、hover 与选中这些运行时状态、图标本身，以及全部真实几何 ——
无边框的托盘面板（`decorations: false`）在 macOS 上就是一块直角方窗，CSS 里的圆角管不到窗口的边。

尺度跟配色一样收在这份文件里，而且同样只有三档：圆角 `--t-r-sm`（控件：输入框、按钮）、
`--t-r-md`（容器：卡片、面板、弹层）、`--t-r-pill`（只有一粒字高度的徽标与 toast）；阴影两档 ——
居中大弹层用 `--t-shadow`，贴在内容上的小浮层用 `--t-shadow-pop`。四扇窗口从前各写各的：实测
有 7 种圆角值（2/4/5/6/8/10/999），同一个角色在不同窗口圆得不一样，看着就是没对齐；而候选清单
那道阴影从前硬写成黑色，切到浅色档就变成一块灰边。`editor.html` 剩下的 `--radius-sm` /
`--radius-md` 只是别名到 `--t-r-sm` / `--t-r-md`（markup 与冒烟测试按名字引用它们），第 [12] 组
同时守着这条：窗口内联的 `:root` 只准别名，不准自己定义颜色。

改档由后端定：`theme` 存在 `settings.json` 里的是**那一档选择**（`system` / `light` / `dark`），
而窗口渲染的是**算出来的那一个值** —— 「跟随系统」时由 PAL 现问操作系统（见 `platform.rs` 的
`ui_prefers_dark`）。四座窗口于是只有两种拿法：随状态广播里的 `theme` 换（面板），或者自己
`get_theme` 问一次（软件设置、日志窗口）。编辑器两种都用：广播负责**换档**，`get_theme` 负责
**首屏那一帧** —— 表单不能等状态快照，而窗口是按 CSS 默认档（深色）建起来的，等快照回来再换色
就是让浅色档的用户先看一眼深色再被闪一下。

## 与后端的通信

- 调用：`window.__TAURI__.core.invoke(cmd, args)`（依赖 `app.withGlobalTauri = true`）。
- 文案：`get_strings` 一次性拉取当前语言的全部 key（5 语 × 525 key，见 `src-tauri/src/i18n/`），
  前端用 `t(key, vars)` 查表；语言只在**软件设置窗口**里改（`set_language`），面板与编辑器收到
  `netsense://status` 后比较 `language`，变了才重取词表。
  **模板里不内嵌任何文案对象。**
- 首屏文案走 `data-i18n` / `data-i18n-placeholder` / `data-i18n-title` 三个属性：窗口加载完先取词表，
  再按属性把 textContent / `placeholder` / `title` 覆盖一遍（四座窗口都是 `visible = false` 创建的，
  所以这一遍发生在首次绘制之前，看不到语言跳一下）。属性没覆盖到的静态标记，就按标记里那句英文显示 ——
  **那句英文必须与 `en.json` 逐字相同**，否则同一个界面会在同一个画面上混出两种语言。
  `get_language` 只用来给 `<html lang>` 定值，不参与取词。面板、软件设置、日志窗口各问一次；
  编辑器问得**更早**：它读的那一份 `get_status` 里本来就带着同一个 `language`
  （同一句 `i18n::current().code()`），之后每一次语言广播又会送一个过来 —— 但那一趟要等系统
  子进程，而首屏不等它，所以它先单独问一次 `get_language`，快照与广播回来时再把它对一次。
- 编辑器首屏只等**一批三份便宜的**取数：`get_config` + `get_language` + `get_theme`
  （`Promise.all`，背后都不拉子进程：配色那一档读的是平台层里带 TTL 的缓存）。表单、语言、
  配色在这一批里全部落地，第一帧就是能编辑的界面；`get_config` 还顺带带回配置代际号
  `config_rev`，编辑器留着它与广播比对（见「广播何时会重建表单」）。
- 另外六份外部数据（`get_networks` + `get_interfaces` + `get_adapters` + `get_printers` +
  `get_installed_apps` + `get_status`）在同一时刻并发发出，但**首屏不等它们**：它们各要拉起一次系统子进程，串起来的
  代价是它们**之和**（Windows 上实测能走到几秒的空白窗口），而它们喂的是状态条、徽标与下拉候选，
  不是表单本身。它们回来后再画一次；这一瞬用户已经动过表单时只走广播那条轻刷新（`refreshLive`），
  不把焦点从他正在填的那一格拿走。只有 `get_status` 的失败会让编辑器停在报错上：
  「当前网络」那几项地址和引擎视图全在它身上。
- 命令返回：返回**大对象快照**的四条命令（`get_status` / `get_engine_status` / `get_interfaces`
  / `get_config`）给出 JSON 字符串，前端自己 `JSON.parse`；其余命令返回结构化值。这条分界
  与「是否采样网络」无关：`get_engine_status` 与 `get_config` 不采样也返回字符串，
  `get_networks` 读的是系统里的已保存网络，却返回数组。要判断某条命令的形状，看 `ipc.rs`
  的返回类型，别看命名。

### 事件（后端 → 前端）

| 事件 | payload | 何时 |
|------|---------|------|
| `netsense://status` | `status_payload`（见下） | 每次评估结束、配置落盘/热重载、语言切换、动作完成；编辑器还比对载荷里的 `config_rev`，据此决定要不要整趟重读（见「广播何时会重建表单」） |
| `netsense://evaluation` | `EngineView` | 引擎刚做完一轮评估（编辑器据此刷新徽标），3B1 每交回**一条**动作结果，以及 3B2 worker 每报出一次**状态变化**（连续两次相同结果不会重发） |
| `netsense://conflict` | `{ profiles: [name] }` | **每个冲突事件只发一次**（引擎按 Profile 的 **id 集合**签名去重，换了名字也不重发；payload 里给的是展示名），前端弹窗 |
| `netsense://action` | `{ kind, ok, message }` | `kind` 为 `apply` / `dhcp` / `probe`（这三条来自手动入口 —— 命令本身只是把请求排进队列）或 `monitor`（健康度监测自动回落 DHCP，没有人点过任何按钮）。**逐条动作的进度不在这里**，在 `evaluation` 的 `last_run` |
| `netsense://update_progress` | `{ phase, percent }` | 在线升级：`download` / `refresh` / `install` |

**广播何时会重建表单。** 编辑器只在**会换掉整片字段**的变化后重建表单：切换选中项、增删条目、
保存成功，以及改动 `mode` / `v6mode` / 动作类型这几类下拉和 DNS 三态开关（THEN/ELSE 两支同时在场，
在它们之间来回看并不触发重绘）；广播只换状态条、徽标与第 1 列的行描边，
从不碰表单 DOM —— 否则用户正在输入的框会被自己填的内容刷掉。唯一的例外是**配置代际号**：
四座窗口都是开机就建、显示/隐藏不重建页面，编辑器的表单只在首屏与自己的保存后重读；期间
配置在别处被换掉了（手改文件触发的热重载、导入备份），广播里的 `config_rev` 就比它手上
那份（`get_config` 同趟带回的同名键）新 —— 此时只要用户没有未保存的改动（`dirty`），
就整趟重读、按新内容重建，这正是他要的；有未保存改动就让画面旧着，不覆盖他的草稿。
重读在途时到达的广播不叠趟，由这一趟收尾时按代际号补读，理由见 `editor.html` 里
`loadAll` 的注释。

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
                    "status": "active|not_matched|conflict|disabled|error|suspended",
                    "error": "…",                 // 仅 error 时
                    "rules": [ { "id":"r1", "status":"match|no_match|inactive",
                                 "conditions":[{"id":"c1","kind":"wifi_ssid","value":"…","status":"match|no_match|inactive"}] } ] } ],
    "last_run": {                                 // 最近一次「走到 3B」的执行；没走到过就省略
      "profile_id": "office", "profile": "Office_5G", "branch": "then|else",
                                                  // 兜底那次的 profile_id 是 "__fallback__"（branch 仍是 then）
      "at": 1770000000, "three_a": "applied|skipped",
      "running": true,                           // true = 还有动作在后台跑
      "status": null,                            // null | empty | success | partial | failed
      "total": 2,                                // 提交时就定下的分母，运行中靠它报「1/2」
      "outcomes": [ { "id": "a1", "label": "…", "ok": true,
                      "error": "…" } ] },            // error 只在失败时出现，成功的那条没有这个键；
                                                     // 数组顺序就是执行顺序，界面上的「第几条」数下标
    "workers": [                                  // 3B2 此刻在维持什么；没有 worker 时是空数组，不会省略
      { "id": "p1", "profile_id": "office",        // 归属：动作 id 只在各自表单内唯一，列表是平的
        "label": "wireguard:wg0",
        "state": "pending|satisfied|repaired|faulted|overdue",
        "interval": 15, "repairs": 2, "at": 1770000000,
        "error": "…" } ],                         // 同样：缺席 = 没有错误
    "warnings": [ "…else 分支配了 persistent 动作，这一支不会被维持…" ]
  },
  "profiles": [ { "id": "...", "name": "...", "enabled": true } ],  // 只是目录，状态看 engine
  "language": "en", "priv": "direct|prompt|outdated", "config_path": "...",
                                       // outdated 只有 macOS 会报：免密通道装着，但不是这一版
  "config_rev": 7                       // 配置代际号：`state.config` 每被成功替换一次 +1。
                                        // 与 `get_config` 里的同名键配对 —— 对不上且编辑器
                                        // 没有未保存改动时，编辑器整趟重读（见「广播何时会重建表单」）
}
```

`CONFLICT`（条件层）与 `ERROR`（执行层）是两种不同颜色的状态，不要合并显示。
`ERROR` **只**来自 3A（网络下发/回读没过，或健康度监测回落 DHCP）；3B1 跑成 `partial` / `failed`
不改 `profiles[].status`，Profile 依然是 `active` —— 网络配置确实落地了，动作失败只是要在
`last_run` 里说清楚的一条信息。所以 `last_run.status` 与 `DisplayStatus` 是两个维度，别拿一个去算另一个。

`SUSPENDED`（`profiles[].status == "suspended"`）是第三种展示态：面板上的「DHCP」按钮成功之后，
引擎进入暂停 —— 网卡在 DHCP 手上，条件仍可能命中，但引擎不再自动评估、下发或跑兜底，直到网络
下一次变化（采样照旧，指纹一变就解除）。它既不是 `active`（网卡此刻不在任何配置手上）也不是
`error`（没有任何下发失败），界面把它显示成 PAUSED 而不是红叉；判定快照 `state` 在暂停期间保持
原样，暂停只体现在 `profiles[].status` 上。
`workers[]` 同理：一条 `faulted` 的常驻动作说的是「有个东西没维持住」，不是「环境错了」，
所以它只染徽标、不动状态。`pending`（worker 起了、第一次核对还没回）由后端报，前端不替它补；
表单里有这条动作、`workers[]` 里却没有 = 引擎此刻没在维持它，这时徽标留空。
`workers[]` 是**全部 Profile 平铺在一起**的一张表：动作 id 只在各自表单内唯一，所以每条都带
`profile_id`，界面先按归属过滤（Active 的 THEN，或零命中的兜底组 `__fallback__`）再染徽标 ——
不按归属过滤，两个 Profile 里同名的动作会互相串台。

### 后端命令一览（`src-tauri/src/ipc.rs`，46 条）

| 命令 | 入参 | 说明 |
|------|------|------|
| `get_status` | — | 上面的 `status_payload`（JSON 字符串） |
| `get_engine_status` | — | 只取 `EngineView`（JSON 字符串），**不**重新采样，补齐广播漏掉的窗口 |
| `preview_match` | `payload: Profile[]`（JSON 字符串，**表单当前内容**，含未保存的草稿） | 用引擎**已采到**的那份快照把这批 Profile 跑一遍 `conditions::eval_profile`，回 `ProfileEvaluation[]`（JSON 字符串），让第 2 列的三态徽标即时跟上，不必等引擎那一轮（采样节律 + `change_delay_secs` 去抖，最坏 8 秒以上）。**不**在这里再采一次样：一次采样要拉若干子进程，那样「即时预览」反而成了最慢的一条。引擎还没跑完第一轮时答不了（快照是全空的默认值，算出来的「都不匹配」只是没测过）→ 返回 `[]`，界面继续用引擎广播的那份徽标。**只回答「条件命中」**：`active`/`conflict`/`error`/`disabled` 与行的绿框仍然只由引擎说 |
| `get_interfaces` | — | 全部在用网卡（有线/无线/VPN）+「装了但没连」的 VPN 条目（`up:false`、不带地址），**首位＝Rust 判定的主网卡**（`automation::primary_nic`），平台层 TTL 缓存（JSON 字符串）。面板的「其他在用网卡」与「VPN/虚拟网卡」两段都来自这里。两段的口径不一样是刻意的：**连着没有**每一张都写（那是系统直接给的），**程序名**只在证据落到这张卡上时才写，认不出就不写、由面板退回通用的「VPN」标签 —— 少一格信息可以，给这台设备安一个具体到某家软件的假答案不行，那个名字接下来会被 3B2 的「维持连接」拿去当对象 |
| `get_adapters` | — | 本机**装着**的网卡 `[{name,label,kind,up}]`（第 2 列「接口」条件值的候选）：含现在没插线、没连上的物理口，好让用户提前给另一个口配好网络；没有地址类字段，也不做 TTL 缓存（刚插上扩展坞之后那次刷新就该看到新口） |
| `get_config` | — | 全量自动化配置 JSON（schema 1），编辑器回填用。里面**没有**界面语言 —— 那是软件配置。另带一个不在 schema 里的 `config_rev`（编辑器读进来就摘掉）：与状态广播里的同名键配对，配置在别处被换掉时编辑器靠它决定重读（见「广播何时会重建表单」） |
| `save_profile` | `payload: Profile`（JSON 字符串） | 按 `id` 新增或替换一个 Profile；**整份配置**校验通过才落盘 |
| `save_global` | `payload: {fallback, allowed_scripts}` | 保存 Profile 之外的全局项（零命中兜底 + 脚本白名单）。兜底**不是** Profile：它没有条件，不参与匹配与冲突；但网络与 3B 动作（`one_shot` / `persistent`）和 THEN 分支同形，引擎会真跑、真起 worker。**整份替换**这两个字段，所以两侧都必须带齐 —— 白名单的编辑面在软件设置窗口，编辑器保存兜底时必须把读到的 `allowed_scripts` 原样传回去（兜底的动作同理：编辑器带着整份动作清单保存），反之亦然，否则一次保存就清空另一项 |
| `delete_profile` | `id` | 删除并按 id 找不到时报错 |
| `apply_profile` | `id` | 请求「立即应用」。**不绕过条件**：引擎重评该 Profile 自己的 Rules/Conditions，禁用中直接拒、冲突中拒绝并回 `netsense://action` |
| `force_dhcp` | — | 把当前网络（主网卡）切回 DHCP **并让自动化暂停**：引擎停止自动评估、下发与健康监测，直到网络下一次变化（采样照旧，变化即解除；面板/编辑器显示 PAUSED）。异步，结果走 `netsense://action` |
| `probe_network` | — | 重新采样并立即评估当前网络：唯一命中就走「立即应用」那条链路（网络 + 3B），多命中只报告名字、零命中只报告，都不动网卡。不受 DHCP 暂停约束，也不解除暂停（同上） |
| `get_networks` | — | 系统已保存的无线网络列表（第 2 列 SSID 条件值的候选，仍可手输列表外的名字）。交的是**空中那个名字**，与面板「当前网络」那一格同源，判据见上面那段 |
| `get_printers` | — | 本机打印机清单 `[{name, info, is_default}]`（第 4 列「设为默认打印机」的候选，只能从清单选：`name` 是下发用的队列名，`info` 是给人看的「说明 · 位置」，可能缺失）。枚举不到就是空表：没装打印系统的机器是正常状态 |
| `get_installed_apps` | — | 本机登记了启动项的程序 `[{name, path}]`（第 4 列「启动程序」动作的候选）：macOS 是 /Applications 一带的 `.app`，Windows 是开始菜单的 `.lnk`（`Start-Process` 直接受理它），Linux 是 XDG 目录里 `Type=Application` 的 `.desktop`。`path` 是平台自己受理的形状（`open -a` / `Start-Process` / exec），界面按 `name` 列出、选中的是它。枚举不到就是空表：候选只是捷径，手输与「浏览…」始终在 |
| `pick_app` | — | 打开系统文件选择器挑一个可启动目标，返回选中路径。**取消回 `null`**（什么都不该发生）；选择器起不来才是错误，且错误原文就是给用户看的话 —— 两者分开报，才能把「用户反悔」与「这台机器没有可用的选择器」（Linux 上 zenity / kdialog 都没有时回 `pal.no_file_dialog`）分清楚 |
| `set_language` | `code` | 切换 UI 语言并写进**软件配置**；`code` 为 `system` 时是「跟随系统」—— 字段从 `settings.json` 里删掉，本次生效的语言现问操作系统。不碰 `config.json`，因此不触发热重载，只广播一次 `netsense://status` |
| `set_theme` | `code` | 切换界面配色并写进**软件配置**：`system` / `light` / `dark` 三档之一，存的就是用户选的这一档，返回值才是当下生效的那一套（`light`/`dark`）。认不出的 `code` 一律拒绝（`sett.theme_rejected`）而不是退回默认档 —— 界面发出一个后端不认的值说明前端有 bug，静默收下只会把它藏起来。与语言同一条判据：不碰 `config.json`，不触发热重载，只广播一次 `netsense://status`（载荷里的 `theme` 是算出来的那一套），并记一条 `notify.theme_set` |
| `get_theme` | — | 界面**此刻**该用哪套配色（`light` / `dark`）。给的是算出来的那一套，不是 `settings.json` 里存的那一档：`system` 要说清现在是什么颜色必须问操作系统（`platform::ui_prefers_dark`，答案缓存 60s），而这一句问话属于后端 —— 决定做一次，四座窗口才是同一份答案。软件设置与日志窗口用它；编辑器在首屏那一批里问一次
（表单不能等状态快照），此后跟着状态广播里的 `theme` 换档；面板只跟着广播走，不单独问 |
| `get_app_settings` | — | 软件设置窗口的一次性快照（JSON 字符串）：语言（以及它是不是「跟随系统」在起作用）、配色那一档（存的是选择，不是算出来的那一个值 —— 这条命令每次窗口获得焦点都会被调，不在这里问操作系统）、开机启动的**系统实况**（问不出来时 `autostart:false` + `autostart_error`）、三份路径与受信脚本目录、备份目录、日志保留天数及上下限、提权通道、平台、版本 |
| `set_autostart` | `enable` | 开 / 关「登录时启动」，返回系统里**实际**的状态（写 plist / `.desktop` / 注册表，真相不在本进程里） |
| `set_log_retention` | `days` | 设日志保留天数（越界按 1–365 夹紧），落盘 + 立刻按新窗口清一次，返回夹紧后的值 |
| `uninstall_priv_channel` | — | 撤销 macOS 的免密通道：在一次授权之内删掉白名单包装脚本与 `/etc/sudoers.d/netsense`，并把 `notify.priv_removed` 记进日志。**没有配套的「安装」命令**：通道不在的时候，下一次应用配置会在那个本来就要弹的授权框里顺手把它装好（`platform::macos` 的 bootstrap 分支），所以装是下发的副产品，不是独立入口。非 macOS 平台回一条 `pal.priv_unsupported`；界面上那颗按钮在 `platform == "macos"` 且 `priv` 为 `"direct"` 或 `"outdated"` 时出现（旧版通道也留着那条 sudoers 规则，删得掉；等一次应用它会先被换成本版） |
| `open_config_folder` | — | 在系统文件管理器里打开 `config.json` 所在目录（路径由后端现算，界面不自己拼） |
| `open_app_settings_folder` | — | 打开 `settings.json` 所在目录。**不**复用上一条：自动化配置可以来自可执行文件同级，那时两条路径不是一个目录 |
| `get_proxy_state` | — | 升级请求出口的现状，JSON 字符串 `{mode,url,system_proxy}`：`mode` 是磁盘上那三态之一，`system_proxy` 只在 mode 为 system 时**现问**操作系统（macOS 一条 `scutil --proxy`，Linux 几条 `gsettings`），问不到就是 `null`。这条命令只在设置窗口打开与保存之后各调一次 —— 不进 `get_app_settings`，那个每次窗口获得焦点都会被调，popup.html 也调它 |
| `set_update_proxy` | `mode`, `url` | 设升级请求走哪条出口：`direct` / `system` / `manual` + 一个 `scheme://host[:port]` 形状的地址。认不出的 mode 或地址一律拒绝（`sett.proxy_rejected`）而不是退回默认值 —— 这一句改的是这台机器的对外流量走哪条路。落盘 + 更新内存里的 `settings`，返回并记一条 `notify.proxy_set`；**不**惊动引擎，这份设置不参与条件求值与下发 |
| `export_backup` | — | 导出一份备份：把自动化配置、软件配置、受信脚本目录里的脚本打成一个 JSON 文件写进 `<配置目录>/backups/`，返回文件名。脚本报错只回文本 —— 那里是 `run_script` 的 allow-list，一次静默失败会让用户以为备份是全的 |
| `get_backups` | — | 现存备份清单（JSON 字符串，新→旧）：`[{name,bytes,modified}]`。只列形状认得出的名字，手放进去的文件不会出现在恢复选项里 |
| `import_backup` | `name` | 恢复这份备份。先解析 + `validate()` 再动盘，落盘前把当前文件另存一份进同一个目录；成功文本走 `notify.backup_restored`。恢复后的 `settings.json` 会重设语言与日志保留，并回推 `netsense://status` |
| `open_backups_folder` | — | 在系统文件管理器里打开备份目录（需要时先建出来） |
| `get_strings` | — | 当前语言全部文案（一次拉全，避免逐 key 往返） |
| `get_language` | — | 当前语言的代码，只用于给 `<html lang>` 定值；取词表一律走 `get_strings` |
| `open_editor` / `close_editor` | — | 显示 / 隐藏编辑器（隐藏 = 收回菜单栏常驻） |
| `open_settings` / `close_settings` | — | 显示 / 隐藏软件设置窗口（面板的「设置」走前者） |
| `open_log_viewer` / `close_log_viewer` | — | 显示 / 隐藏日志窗口（面板的「日志」走前者） |
| `open_logs` | — | 用系统默认程序打开**当前实际使用**的日志目录 |
| `get_log_files` | — | `{ log_dir, files[], max_lines }`：目录里的日志文件清单（新→旧）+ 单次行数上限。目录一并返回，因为日志目录不可写时后端会退到临时目录，界面自己拼的那个路径可能是错的 |
| `read_log` | `name`, `lines?` | 读某个日志文件的**尾部**若干行（`{ name, text, lines, truncated }`）。只接受文件名，名字形状在 Rust 侧重新校验，所以这里递不出目录外的路径 |
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
才看得见的绑定规则（dns 的三态、禁用动作不进清单、拖卡头与 ↑/↓ 改的只是数组顺序且不许跨支跨类、
常驻动作的字段要原样回到 payload 里、
worker 徽标只在 Active 方案的 THEN 分支或兜底出现且按 `profile_id` 归属过滤、广播不重建用户正在输入的表单）。
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

软件配置（`settings.json`）走完全不同的口径，因为它没有「 schema 」这类概念可言：文件不存在就是全部
默认值，格式写坏也只是记一条日志后用默认值起界面 —— 一份读不出来的 `settings.json` 不该让人打不开
NetSense。两个字段之外不放东西的判据，以及「开机启动为什么不进这个文件」，见 `DEVELOPMENT.md` §10.4。
