// 无头跑 frontend/editor.html：真实脚本 + 最小 DOM + 假 __TAURI__，断言它**准备发给
// 后端的那些 payload**。
//
// 为什么值得有一份 JS 侧的测试：编辑器对后端的全部约定就是一串 JSON，而那串 JSON 没有
// 任何类型检查。字段名拼错、把「留空」写成 `""`、给 launch_app 带上 path —— 后端只会
// 在保存时说「这条 Profile 有问题」，用户看不出是哪一处，而 CI 在此之前一路绿。
//
// 这里刻意不重新实现一遍编辑器的逻辑：跑的就是 editor.html 里的那份脚本，喂给它的是
// config.example.json，看的是它调 `save_profile` / `save_global` 时递出来的 payload。
// payload 的另一半（serde 认不认）交给 `--write-fixtures` 写出的文件，由
// `config::tests::payloads_the_editor_actually_sends_are_the_ones_serde_accepts` 读回去验。
//
// 用法：
//   node scripts/editor-smoke.mjs [--write-fixtures out/payloads.json]

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { argv } from "node:process";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

// —————————————————————— 报告（与 validate.py 同一副面孔） ——————————————————————

let failures = 0;
let checks = 0;
function group(label) {
  console.log(`\n[${label}]`);
}
function check(label, cond, detail = "") {
  checks += 1;
  if (cond) {
    console.log(`  OK   ${label}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${label}${detail ? "：" + detail : ""}`);
  }
}
function eq(label, got, want) {
  const same = JSON.stringify(got) === JSON.stringify(want);
  check(label, same, same ? "" : `得到 ${JSON.stringify(got)}，期望 ${JSON.stringify(want)}`);
  return same;
}

// —————————————————————— 最小 DOM ——————————————————————
//
// 只实现 editor.html 真正碰到的那部分（它全程用 innerHTML 字符串建表单，
// 建完就只读 dataset / value / children —— 没有一处真的改 DOM 结构）。
// `innerHTML` 的 getter 故意返回空串：断言一律走
// 真实函数的返回值或数据，不去解析序列化结果 —— 那种断言只会绑死标签顺序。

const VOID = new Set(["INPUT", "BR", "IMG", "HR", "META", "LINK"]);
const camel = (s) => s.replace(/-([a-z])/g, (_, c) => c.toUpperCase());
const decode = (s) =>
  s
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&times;/g, "×")
    .replace(/&amp;/g, "&");

class Text {
  constructor(t) {
    this.nodeType = 3;
    this.text = t;
    this.parent = null;
  }
  get textContent() {
    return this.text;
  }
}

class El {
  constructor(tag) {
    this.nodeType = 1;
    this.tagName = String(tag).toUpperCase();
    this.attributes = {};
    this.dataset = {};
    this.children = [];
    this.parent = null;
    this.id = "";
    this.className = "";
    this.value = "";
    this.checked = false;
    this.type = "";
    this.title = "";
    this.hidden = false;
    this.style = {};
    this.onclick = null;
    const self = this;
    this.classList = {
      add(c) {
        const set = new Set(self.className.split(/\s+/).filter(Boolean));
        set.add(c);
        self.className = [...set].join(" ");
      },
      remove(c) {
        self.className = self.className
          .split(/\s+/)
          .filter((x) => x && x !== c)
          .join(" ");
      },
      contains(c) {
        return self.className.split(/\s+/).includes(c);
      },
      toggle(c, on) {
        const want = on === undefined ? !this.contains(c) : Boolean(on);
        if (want) this.add(c);
        else this.remove(c);
      },
    };
  }
  get textContent() {
    return this.children.map((c) => c.textContent).join("");
  }
  set textContent(v) {
    this.children = [new Text(String(v))].map((n) => ((n.parent = this), n));
  }
  set innerHTML(html) {
    this.children = [];
    for (const n of parse(String(html))) {
      n.parent = this;
      this.children.push(n);
    }
  }
  get innerHTML() {
    return "";
  }
  setAttribute(name, raw) {
    setAttr(this, name.toLowerCase(), String(raw));
  }
  /// 只支持 editor.html 用到的那两种选择器：`[data-act]` 与一组标签名。
  closest(sel) {
    const tags = String(sel).split(",").map((s) => s.trim().toLowerCase());
    const wantAct = tags[0] === "[data-act]";
    let e = this;
    while (e) {
      if (wantAct && e.dataset && e.dataset.act !== undefined) return e;
      if (!wantAct && e.nodeType === 1 && tags.includes(e.tagName.toLowerCase())) return e;
      e = e.parent;
    }
    return null;
  }
}

function parse(html) {
  const roots = [];
  const stack = [];
  const push = (n) => {
    const p = stack[stack.length - 1];
    if (p) p.children.push(n);
    else roots.push(n);
    n.parent = p || null;
  };
  let i = 0;
  while (i < html.length) {
    const lt = html.indexOf("<", i);
    if (lt < 0) {
      const raw = html.slice(i);
      if (raw.trim()) push(new Text(decode(raw)));
      break;
    }
    if (lt > i) {
      const raw = html.slice(i, lt);
      if (raw.trim()) push(new Text(decode(raw)));
    }
    if (html.startsWith("<!--", lt)) {
      const end = html.indexOf("-->", lt);
      i = end < 0 ? html.length : end + 3;
      continue;
    }
    if (html.startsWith("<!", lt)) {
      const end = html.indexOf(">", lt);
      i = end < 0 ? html.length : end + 1;
      continue;
    }
    let j = lt + 1;
    let quote = "";
    while (j < html.length) {
      const c = html[j];
      if (quote) {
        if (c === quote) quote = "";
      } else if (c === '"' || c === "'") quote = c;
      else if (c === ">") break;
      j += 1;
    }
    const body = html.slice(lt + 1, j);
    i = j + 1;
    if (!body.length) continue;
    if (body[0] === "/") {
      const tag = body.slice(1).trim().toUpperCase();
      for (let k = stack.length - 1; k >= 0; k -= 1) {
        if (stack[k].tagName === tag) {
          stack.length = k;
          break;
        }
      }
      continue;
    }
    const nameMatch = /^([A-Za-z][-\w:.]*)/.exec(body);
    if (!nameMatch) continue;
    const el = new El(nameMatch[1]);
    const attrRe = /([A-Za-z_:@][-.\w:@]*)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+)))?/g;
    const rest = body.slice(nameMatch[0].length);
    let m;
    while ((m = attrRe.exec(rest))) {
      const name = m[1].toLowerCase();
      const raw = m[2] ?? m[3] ?? m[4] ?? "";
      setAttr(el, name, raw);
    }
    push(el);
    if (!VOID.has(el.tagName) && !body.endsWith("/")) stack.push(el);
  }
  return roots;
}

function setAttr(el, name, raw) {
  el.attributes[name] = raw;
  if (name === "id") el.id = raw;
  else if (name === "class") el.className = raw;
  else if (name === "value") el.value = raw;
  else if (name === "type") el.type = raw;
  else if (name === "title") el.title = decode(raw);
  else if (name === "placeholder") el.placeholder = decode(raw);
  else if (name === "hidden") el.hidden = true;
  else if (name === "checked") el.checked = true;
  else if (name === "selected") el.selected = true;
  else if (name === "style") for (const d of raw.split(";")) {
    const [k, v] = d.split(":");
    if (k && v) el.style[camel(k.trim())] = v.trim();
  }
  else if (name.startsWith("data-")) el.dataset[camel(name.slice(5))] = decode(raw);
}

const body = new El("body");

function walk(e, fn) {
  for (const c of e.children) {
    if (c.nodeType === 1) {
      fn(c);
      walk(c, fn);
    }
  }
}
function findAll(id) {
  const out = [];
  const hits = [];
  walk(body, (e) => out.push(e));
  for (const e of out) if (id(e)) hits.push(e);
  return hits;
}
const findById = (id) => findAll((e) => e.id === id)[0] || null;
const byBind = (path) => findAll((e) => e.dataset && e.dataset.bind === path)[0] || null;
const byAct = (act) => findAll((e) => e.dataset && e.dataset.act === act)[0] || null;
/// 某个元素内部的（递归）全部元素 —— 行内的控件包在 <span> 里，只看直接子节点会漏。
const inside = (e) => {
  const out = [];
  const go = (n) => { for (const c of n.children || []) if (c.nodeType === 1) { out.push(c); go(c); } };
  go(e);
  return out;
};
/// 同一列里 THEN 与 ELSE 同时呈现，所以「哪一个控件」只能靠 data-pre 区分。
const byActPre = (act, pre) =>
  findAll((e) => e.dataset?.act === act && e.dataset?.pre === pre)[0] || null;
/// 某个前缀下渲染出来的全部绑定路径 —— 用来断言「THEN 的控件没跑到 ELSE 那一块里」。
const bindsUnder = (pre) =>
  findAll((e) => e.dataset?.bind?.startsWith(pre)).map((e) => e.dataset.bind);

const handlers = {};
const documentElement = new El("html");
const document = {
  body,
  documentElement,
  createElement: (tag) => new El(tag),
  addEventListener(type, fn) {
    (handlers[type] ||= []).push(fn);
  },
  getElementById: findById,
  /// 只支持 `[data-x]` 这种属性存在性选择器（界面文案的 data-i18n 循环用得到），以及
  /// 逗号分隔的一组。始终是全文档查询 —— 与界面脚本目前的用法一致，不假装支持作用域查询。
  querySelectorAll(sel) {
    const names = String(sel).split(",").map((s) =>
      s.trim().replace(/^\[(.*)\]$/, "$1").toLowerCase());
    return findAll((e) => names.some((n) => e.attributes[n] !== undefined));
  },
  dispatch(type, target) {
    const ev = { type, target };
    for (const fn of handlers[type] || []) fn(ev);
  },
};

/** 模拟用户在框里打字 / 在下拉里选 / 勾上复选框，然后点一个 data-act 按钮。 */
function type(el, value) {
  el.value = value;
  document.dispatch("input", el);
}
function choose(el, value) {
  el.value = value;
  document.dispatch("change", el);
}
function toggle(el, on) {
  el.checked = on;
  document.dispatch("change", el);
}
function click(el) {
  document.dispatch("click", el);
}

// —————————————————————— 假后端 ——————————————————————

const configFixture = JSON.parse(readFileSync(join(ROOT, "config.example.json"), "utf8"));
const strings = JSON.parse(readFileSync(join(ROOT, "src-tauri/src/i18n/en.json"), "utf8"));
const zhStrings = JSON.parse(readFileSync(join(ROOT, "src-tauri/src/i18n/zh.json"), "utf8"));
/** 假后端「当前」的语言：编辑器跟着它走，测试用它来模拟软件设置里换了语言。 */
let langCode = "en";
/** 系统文件选择器这一次回什么：正常是一条路径，null 是取消，错误是「它起不来」。 */
let pickAppResult = "/Users/me/Desktop/GreenThing.app";
let pickAppError = null;

const engineView = (profiles, state, lastRun, workers) => ({
  state,
  active: state.state === "active" ? state.id : undefined,
  snapshot: {
    ssid: "Office_5G",
    gateway_mac: "aa:bb:cc:dd:ee:ff",
    bssid: "00:11:22:33:44:55",
    primary_interface: "en0",
    interfaces: ["en0", "en5"],
    tunnels: ["utun3"],
  },
  profiles,
  last_run: lastRun,
  workers: workers || [],
  warnings: [],
});

let viewFixture = engineView(
  [
    { id: "home", name: "HomeWiFi", enabled: true, matched: false, status: "not_matched", rules: [] },
    {
      id: "office", name: "Office_5G", enabled: true, matched: true, status: "active",
      rules: [{ id: "r1", status: "match", conditions: [{ id: "c1", kind: "wifi_ssid", value: "Office_5G", status: "match" }] }],
    },
    { id: "router_only", name: "IdentifyByRouterOnly", enabled: false, matched: false, status: "disabled", rules: [] },
  ],
  { state: "active", id: "office" },
);

/** `preview_match` 的替身。真的那一份在 Rust（`conditions::eval_profile` 对引擎已采到的
    快照），这里照同一份 `viewFixture.snapshot` 把三态算回来 —— 为的是检查**接线**：
    编辑器发的是不是表单当前内容、徽标是不是就地换掉了、有没有把引擎说的「已生效」盖掉。
    匹配判据本身由 Rust 的单元测试守，不靠这里。 */
function previewFixture(profiles) {
  const snap = viewFixture.snapshot;
  const condStatus = (c) => {
    if (!c.enabled) return "disabled";
    const want = String(c.value || "").toLowerCase();
    const hit = c.type === "wifi_ssid"
      ? c.value === snap.ssid
      : c.type === "network_interface"
        ? (c.value === "*" ? snap.interfaces.length > 0 : snap.interfaces.includes(want))
        : c.type === "bssid" || c.type === "gateway_mac"
          ? snap[c.type] === want
          : false;
    return hit ? "match" : "no_match";
  };
  return profiles.map((p) => {
    const rules = (p.rules || []).map((r) => {
      const conditions = (r.conditions || []).map((c) => ({
        id: c.id, kind: c.type, value: c.value, status: condStatus(c),
      }));
      const live = conditions.filter((c) => c.status !== "disabled");
      return {
        id: r.id,
        status: !r.enabled || !live.length ? "inactive"
          : live.every((c) => c.status === "match") ? "match" : "no_match",
        conditions,
      };
    });
    return {
      id: p.id, name: p.name, enabled: p.enabled,
      matched: p.enabled && rules.some((r) => r.status === "match"), rules,
    };
  });
}

const invokeLog = [];/// 「当前网络」那一格的网卡明细来源。可变：最后那一组会改它，验证广播之后界面跟着变新。
let nicFixture = [
  { name: "en0", kind: "wireless", ipv4: "192.168.1.100/24", netmask: "255.255.255.0", dns: "192.168.1.1" },
  { name: "en5", kind: "wired" },
  { name: "utun3", kind: "vpn" },
];
let saveSeq = 0;
const saves = [];
/** `saves` 会被各段断言清空以便「看这一次保存发了什么」，交付给 Rust 的那份要一直攒着。 */
const emitted = [];
/** 「发出」与「落回」共用一根时钟：开机那几项外部数据若是串行取的，`get_networks`
    会在 `get_interfaces` 发出之前就落回来 —— 这一位就是并发与否唯一看得见的判据。 */
let ipcClock = 0;
/** @type {{cmd:string,issued:number,resolved:number}[]} */
const ipcTime = [];
function fakeInvoke(cmd, args) {
  invokeLog.push({ cmd, args });
  const issued = (ipcClock += 1);
  return answer(cmd, args).then(
    (v) => { ipcTime.push({ cmd, issued, resolved: (ipcClock += 1) }); return v; },
    (e) => { ipcTime.push({ cmd, issued, resolved: (ipcClock += 1) }); throw e; },
  );
}
/** 按住某一条命令：让它停在半路，直到测试放行。用来把「首屏等不等外部数据」变成
    看得见的断言 —— 一个永远不回头的 `get_status` 就是现场那几秒空白的替身。
    两位记：`holdArmed` 是「下一趟打算按住谁」，`holdLive` 是「此刻真停在哪一路上」。
    只记一位的话，发出时就把这一路从手上擦掉了，`releaseHeld` 再也找不到要放行的那一个。 */
let holdArmed = null;
let holdLive = null;
const holdCmd = (cmd) => { holdArmed = cmd; };
const releaseHeld = async () => {
  const g = holdLive;
  // 按住却没咬住，就是首屏那一组断言在查一个不存在的过程：宁可在这里炸，
  // 也不要让「编辑器不再问 get_status」读成「首屏很快」。
  if (!g) throw new Error(`holdCmd(${holdArmed ?? "?"}) 没有拦住任何调用`);
  holdLive = null;
  g.release();
  await settle();
};
function answer(cmd, args) {
  const value = answerValue(cmd, args);
  if (holdArmed === cmd) {
    holdArmed = null;
    return new Promise((resolve) => { holdLive = { cmd, release: () => resolve(value) }; });
  }
  return value;
}
function answerValue(cmd, args) {
  switch (cmd) {
    case "get_strings":
      return Promise.resolve({ ...(langCode === "zh" ? zhStrings : strings) });
    case "get_language":
      return Promise.resolve(langCode);
    case "get_theme":
      // 编辑器首绘前只问这三份便宜的（配置 / 语言 / 配色）。配色给 light 的理由同
      // `get_status` 里那一份：深色是 CSS 默认档，给 dark 分不清「写了属性」还是「没人写」。
      return Promise.resolve("light");
    case "get_config":
      return Promise.resolve(JSON.stringify(configFixture));
    case "get_networks":
      return Promise.resolve(["Office_5G", "Café"]);
    case "get_interfaces":
      // 「当前网络」那一格的事实来源：后端只报「在用且不是内部设备」的网卡，
      // 并按 无线→有线→VPN 排好序，所以这里给出的就是编辑器应当照单全收的那一份。
      return Promise.resolve(JSON.stringify(nicFixture));
    case "get_adapters":
      // 接口条件值的候选：本机装着的口，**含现在没插线的**（en7 就是那个 up:false），
      // 且平台层已经把 VPN 隧道滤掉了 —— 下拉里没有 utun3 是这份契约的一部分。
      return Promise.resolve([
        { name: "en0", label: "Wi-Fi", kind: "wireless", up: true },
        { name: "en5", label: "USB 10/100/1000 LAN", kind: "wired", up: true },
        { name: "en7", label: "Thunderbolt Ethernet", kind: "wired", up: false },
      ]);
    case "get_printers":
      // 「设为默认打印机」的候选。这里返回 JSON 字符串，是为了让 asArray 那条兼容分支
      // 一直有人走：后端改形状时这边会立刻红。
      return Promise.resolve(JSON.stringify([
        { name: "Office LaserJet", info: "LaserJet 476 · 3F", is_default: true },
        { name: "Home Inkjet", is_default: false },
      ]));
    case "get_installed_apps":
      // 「启动程序」的候选（macOS 上是 /Applications 里带启动项的那些 .app）。返回 JSON
      // 字符串的理由同上一档。故意夹带一条没有路径的记录：后端本不该给，但界面也不该把它
      // 渲染成一条点了没反应的候选。
      return Promise.resolve(JSON.stringify([
        { name: "Broken", path: "" },
        { name: "Firefox", path: "/Applications/Firefox.app" },
        { name: "Keka", path: "/Applications/Keka.app" },
        { name: "Slack", path: "/Applications/Slack.app" },
      ]));
    case "pick_app":
      return pickAppError
        ? Promise.reject(new Error(pickAppError))
        : Promise.resolve(pickAppResult);

    case "get_status":
      return Promise.resolve(
        JSON.stringify({
          status: { connected: true, ssid: "Office_5G", ipv4: "192.168.1.100/24", dns: "192.168.1.1", rssi: -50 },
          engine: viewFixture,
          profiles: configFixture.profiles.map((p) => ({ id: p.id, name: p.name, enabled: p.enabled })),
          language: "en",
          // 与 `sett.theme` 那三档不同：这里是**算出来的那一套**。故意给 light ——
          // 深色是 CSS 的默认档，给 dark 就分不清「代码写了属性」还是「没人写过」。
          theme: "light",
          priv: "direct",
          config_path: "/tmp/config.json",
        }),
      );
    case "save_profile":
    case "save_global": {
      const rec = { cmd, payload: JSON.parse(args.payload), at: (saveSeq += 1) };
      saves.push(rec);
      emitted.push(rec);
      return Promise.resolve();
    }
    case "preview_match":
      return Promise.resolve(JSON.stringify(previewFixture(JSON.parse(args.payload))));
    case "apply_profile":
    case "delete_profile":
    case "close_editor":
      return Promise.resolve();
    default:
      return Promise.reject(new Error(`未登记的命令: ${cmd}`));
  }
}

const listeners = {};
const window = {
  __TAURI__: {
    core: { invoke: fakeInvoke },
    event: {
      listen: async (ev, fn) => {
        (listeners[ev] ||= []).push(fn);
        return () => {};
      },
    },
  },
};
/** 让假后端主动广播一次，模拟引擎的 `netsense://status`。 */
const broadcast = async (ev, payload) => {
  for (const fn of listeners[ev] || []) await fn({ payload });
  await settle();
};

// —————————————————————— 装载脚本 ——————————————————————

const html = readFileSync(join(ROOT, "frontend/editor.html"), "utf8");
const script = /<script>([\s\S]*?)<\/script>/.exec(html);
if (!script) {
  console.error("editor.html 里找不到 <script> 段");
  process.exit(1);
}
body.innerHTML = /<body[^>]*>([\s\S]*)<\/body>/.exec(html)[1].replace(/<script>[\s\S]*?<\/script>/g, "");

const sandbox = {
  window,
  document,
  console: { log: () => {}, warn: () => {}, error: (...a) => console.error("editor console.error:", ...a) },
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (id) => clearTimeout(id),
  confirm: () => true,
  alert: () => {},
};
const ctx = vm.createContext(sandbox);
// 追加的一行与脚本同属一个 script，因此能看见顶层的 let/const（另起一次 runInContext 就看不见了）。
const code =
  script[1] +
  "\n;globalThis.__h = { $: (id) => document.getElementById(id), pick, loadAll, renderAll, refreshLive," +
  " applyPlan, planList, planPersistent, profileView, comboPlan," +
  " get cfg() { return cfg }, get draft() { return draft }," +
  " get sel() { return sel }, get view() { return view }, set view(v) { view = v }," +
  " get wifiList() { return wifiList }, get nicList() { return nicList }," +
  " get adapterList() { return adapterList }," +
  " get printerList() { return printerList }," +
  " get appList() { return appList }, set appList(v) { appList = v }," +
  " get langShown() { return langShown }, get dirty() { return dirty }," +
  " get globalDraft() { return globalDraft } };";
// 按住最慢的那一份外部数据。现场那句「自动化配置点开以后空白好几秒」在真机上就是这几趟子进程
// （Windows 上每趟是一整条 PowerShell 启动链），替身只有让它停在半路，才看得出来首绘等不等它。
holdCmd("get_status");
vm.runInContext(code, ctx, { filename: "frontend/editor.html" });
const h = ctx.__h;

const settle = async (n = 6) => {
  for (let i = 0; i < n; i += 1) await new Promise((r) => setImmediate(r));
};
/** 真等一会儿。条件预览是合并过的（改一下表单等 200 ms 才问后端），而 `settle` 只转微任务、
    等不到定时器 —— 这一位是「徽标跟不跟表单走」唯一看得见的窗口。 */
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const lastSave = () => saves[saves.length - 1];
const saveOf = (cmd) => saves.filter((s) => s.cmd === cmd).pop();
const resetSaves = () => {
  saves.length = 0;
};

// —————————————————————— 断言 ——————————————————————

group("装载");
await settle();
// 首屏那一帧：外部六份里最慢的一份还停在半路。这一段断言的是「表单先画、系统清单后到」这条
// 分工 —— 它挡得住的回归是「把外部数据又请回首绘的关键路径」（用户看到的正是那一句空白），
// 以及第一帧漏掉语言/配色这两份便宜数据。它挡不住的是「第一帧画错了内容」：那时 view 同样是
// null，界面却已经不该长这样 —— 那一条由下面「四格与徽标来自快照」那几组负责。
check("引擎快照还没回来时，Profile 列表与表单已经画好",
  h.view === null &&
  findAll((e) => e.dataset?.act === "sel-profile").length === 3 && !!byBind("name"),
  `view=${JSON.stringify(h.view)}，行数 ${findAll((e) => e.dataset?.act === "sel-profile").length}`);
eq("语言与配色由首屏那一批便宜取数写上，不等状态快照",
  [documentElement.lang, documentElement.dataset.theme], ["en", "light"]);
await releaseHeld();
check("启动脚本无异常地跑完（顶层 IIFE 已读到配置）", h.cfg.profiles.length === 3);
eq(
  "启动时按顺序读了后端：文案 → 六份外部数据一起发 → 首屏那三份（配置 / 语言 / 配色）",
  invokeLog.map((c) => c.cmd).slice(0, 10),
  ["get_strings", "get_networks", "get_interfaces", "get_adapters", "get_printers", "get_installed_apps",
   "get_status", "get_config", "get_language", "get_theme"],
);
eq("已存 SSID、在用网卡、本机网卡、打印机与已装程序各取一份（条件值与动作目标的候选）",
  [h.wifiList, h.nicList.map((n) => n.name), h.adapterList.map((n) => n.name), h.printerList.map((p) => p.name),
   h.appList.map((a) => a.name)],
  [["Office_5G", "Café"], ["en0", "en5", "utun3"], ["en0", "en5", "en7"], ["Office LaserJet", "Home Inkjet"],
   ["Broken", "Firefox", "Keka", "Slack"]]);
// 上面那条只说得出「读了哪几项」，说不出「是不是排队读的」，而排队读的代价就是现场那句
// 「自动化管理打开要好几秒空白」：六项各要拉一次子进程，串起来就是它们之和。
// 判据是并发与否唯一看得见的痕迹：最后发出的那一项，早于最早落回的那一项 —— 被按住的那份
// 落回得最晚，取 min 时自然不会算进最早的那一个，所以它按住的是首绘、压不垮这条判据。
const bootIpc = ["get_networks", "get_interfaces", "get_adapters", "get_printers", "get_installed_apps",
                 "get_status", "get_config", "get_language", "get_theme"];
const bootBatch = ipcTime.filter((t) => bootIpc.includes(t.cmd)).slice(0, 9);
eq("开机那九份各发一次（外部六份 + 首屏三份，没有哪一项被读了两遍）", bootBatch.length, 9);
check("它们是并发发出的：最后发出的那一项，早于最早落回的那一项",
  Math.min(...bootBatch.map((t) => t.resolved)) > Math.max(...bootBatch.map((t) => t.issued)),
  `发出 ${bootBatch.map((t) => t.issued)}，落回 ${bootBatch.map((t) => t.resolved)}`);
check("第 1 列渲染出 Profile 列表", findAll((e) => e.dataset?.act === "sel-profile").length === 3);
// 配色：`theme.css` 只认 `<html data-theme>`，所以这一条断言的是「后端给的那一档真的落地了」，
// 而不是样式表里写了什么 —— 属性没写上时窗口会安静地停在深色档，谁也不会报错。
// 首屏那一帧已经查过同一个属性；这里查的是**快照回来以后又对了一次**（两处来源本是同一句
// `theme_now()`，但首绘那一瞬正好有人换档时，快照里的那一份才是新的）。
eq("状态快照里的配色档写进了 <html data-theme>", documentElement.dataset.theme, "light");
check("兜底排在列表末尾，作为一条特殊的 Profile 行", !!byAct("sel-fallback"));
eq("按钮文案取自后端字典（不是 key 本身）", findById("btn-apply").textContent, strings["editor.force_apply"]);

group("第一层：当前状态条与四列一一对应");
eq("四格的小标题取自字典", ["sk-1", "sk-2", "sk-3", "sk-4"].map((id) => findById(id).textContent),
  [strings["editor.state_active_profile"], strings["editor.state_conditions"],
   strings["editor.state_network"], strings["editor.state_actions"]]);
// 状态条讲的是引擎的事实：它说「现在生效的是谁」，与用户正在编辑哪一条无关。
check("格 1 讲的是「现在生效的是谁」，与用户正在编辑哪一条无关（此刻选中的是 home）",
  h.sel.id === "home" && findById("st-profile").textContent.includes("Office_5G"));
check("格 2 摊开的是它命中所用的条件值", findById("st-cond").textContent.includes("Office_5G"));
check("格 3 是当前网络信息", findById("st-net").textContent.includes("Office_5G"));
eq("格 4：后端什么都没跑过 → 列出 Active 的 THEN 动作，但不给任何成败徽标",
  [findById("st-act").textContent.includes("Slack.app"),
   inside(findById("st-act")).filter((e) => e.className?.includes("achip") && e.textContent).length],
  [true, 0]);
// 隧道不挤进「当前网络」这一格：条件区里没有对应可选项，列在这里只会让人以为能按它下条件
// （托盘面板照旧列它，那边看的是实况）。
check("VPN 隧道不再出现在编辑器的当前网络摘要里",
  !findById("st-net").textContent.includes("utun3"));

group("选中与表单回填");
// 走真实的事件委托：点第 1 列那一行，而不是直接调 pick()
const officeRow = findAll((e) => e.dataset?.act === "sel-profile" && e.dataset?.id === "office")[0];
check("列表里有 office 这一行", !!officeRow);
click(officeRow);
eq("点击列表项后选中态切到 office", h.sel, { kind: "profile", id: "office" });
eq("名称框回填自配置", byBind("name")?.value, "Office_5G");
check("名称与「启用」勾选只出现在第 1 列那一行，条件列不再重复一遍",
  !!byBind("name") && !!byBind("enabled") && findAll((e) => e.dataset?.bind === "name").length === 1);
eq("快速切换勾选回填自配置", byBind("quick")?.checked, true);

group("第二层：THEN 与 ELSE 同时呈现，不再靠页签切换");
const titlesIn = (id) => {
  const out = [];
  walk(findById(id), (e) => { if (e.className?.includes("branch-title")) out.push(e.textContent); });
  return out;
};
const THEN_T = strings["editor.then"], ELSE_T = strings["editor.else_branch"];
eq("3A 列：THEN 块在上、ELSE 块在下", titlesIn("col-net"), [THEN_T, ELSE_T]);
eq("3B 列：同样的上下次序", titlesIn("col-act"), [THEN_T, ELSE_T]);
check("两分支的控件互不覆盖：THEN 与 ELSE 各有自己的模式下拉",
  !!byBind("then.network.mode") && !!byBind("else.network.mode"));
eq("THEN 按配置里的值选中 manual",
  byBind("then.network.mode")?.children.filter((c) => c.selected).map((c) => c.value).join(","), "manual");
check("校验（readback / 健康检查）不摊在主列里，只在弹窗中",
  byBind("then.network.verify.readback") === null && byBind("else.network.verify.readback") === null);
const groupTitles = findAll((e) => e.className?.includes("group-title")).map((e) => e.textContent);
const pos = (s) => groupTitles.findIndex((g) => g.includes(s));
check("3A 的顺序是 IPv4 → IPv6 → DNS → 静态路由",
  pos(strings["editor.ipv4_config"]) >= 0 &&
  pos(strings["editor.ipv4_config"]) < pos(strings["editor.ipv6_config"]) &&
  pos(strings["editor.ipv6_config"]) < pos(strings["editor.dns_config"]) &&
  pos(strings["editor.dns_config"]) < pos(strings["editor.routes_title"]));

group("第二层：校验弹窗（按钮在列标题右边）");
click(byAct("open-verify"));
check("打开后 THEN 与 ELSE 各自的校验块都在",
  !!byBind("then.network.verify.readback") && !!byBind("else.network.verify.readback"));
check("THEN 的健康检查按配置里的值勾着",
  byBind("then.network.verify__health")?.checked === true);
eq("弹窗标题取字典", findById("verify-modal").children.find((c) => c.tagName === "H3")?.textContent,
  strings["editor.verify_title"]);
click(findAll((e) => e.dataset?.act === "close-verify")[0]);
check("关掉后遮罩收起：弹窗里的控件不再呈现", findById("verify-mask").hidden === true);

group("第 2 列：条件行 = 勾选框 + 类型 + 值 + 徽标 + 删除，同一行");
const crows = findAll((e) => e.className?.includes("crow"));
eq("office 的两条规则里一共 4 个条件，一行一个", crows.length, 4);
const crow0 = inside(crows[0]);
eq("同一行里：启用勾选 + SSID 输入框（候选是它的浮层，不占第二格）+ 类型下拉 + 徽标 + 删除按钮",
  [crow0.filter((e) => e.tagName === "INPUT").length,
   crow0.filter((e) => e.tagName === "SELECT").length,
   crow0.filter((e) => e.className?.includes("badge")).length,
   crow0.filter((e) => e.dataset?.act === "del-cond").length],
  [2, 1, 1, 1]);
const ssidInp = byBind("rules.0.conditions.0.value");
const ssidList = findAll((e) => e.dataset?.comboList === "rules.0.conditions.0.value")[0];
const ssidOpts = (ssidList?.children || []).filter((e) => e.dataset?.act === "combo-pick");
eq("SSID 是能直接打字的输入框，不再是一个只能挑的下拉", ssidInp?.tagName, "INPUT");
eq("输入框显示的就是配置里的值", [ssidInp?.value, ssidInp?.placeholder],
  [h.draft.rules[0].conditions[0].value, strings["status.ssid"]]);
// 这一条是「候选和输入框在一起」的结构判据：同一个 `.combo-box` 之下，而不是行里的两块。
check("候选浮层挂在输入框同一个盒子里：它就是这一条条件的附属控件",
  !!ssidList && ssidList.parent === ssidInp.parent);
// 排序：系统给的顺序是 ["Office_5G", "Café"]（见装载那一段），候选要按名字排，C 在 O 前 ——
// 于是这里恰好与系统顺序相反；断言的是展示清单，而不是 `get_networks` 的原始返回。
eq("候选只列系统已保存的 SSID，不掺任何哨兵项，且按名字排序",
  ssidOpts.map((o) => o.dataset.v), ["Café", "Office_5G"]);
eq("候选显示的文本就是它要填进去的那个名字", ssidOpts.map((o) => o.textContent), ["Café", "Office_5G"]);
check("默认收起：没点开之前浮层不占位（.open 不在）", ssidList?.classList?.contains("open") === false);
eq("候选自己不绑定字段：它写不进配置", ssidList?.dataset?.bind, undefined);
const caret0 = findAll((e) => e.dataset?.act === "combo-toggle" &&
  e.dataset?.path === "rules.0.conditions.0.value")[0];
click(caret0);
check("点右端箭头：这一条的浮层展开", ssidList.classList.contains("open"));
click(caret0);
check("同一个箭头再点一次：收起", !ssidList.classList.contains("open"));
click(caret0);
click(findById("col-net"));
check("点别处（这里是第三列的空白处）：浮层收起，不会一直挂着", !ssidList.classList.contains("open"));

group("浮层朝哪边开、能长多高：纯函数喂数（真几何冒烟测试量不到）");
// 现场回归：判据曾拿「清单展开后的矩形」与锚点比，可它展开后的 bottom 本就在锚点下方
// 一列处，减法永远是负数 —— 每一列都朝上开，条件行靠顶部时头几行落在 `.panel-content`
// 的裁剪边外面（「第一行看不见」就是这么来的）。下面喂的是锚点两侧的余量。
eq("下方放得下一整列：朝下，按上限", h.comboPlan(400, 500), { up: false, maxHeight: 168 });
eq("下方不够、上方更宽裕：朝上，高度仍是上限", h.comboPlan(80, 300), { up: true, maxHeight: 168 });
eq("两边都不够：挑更宽的一边，高度收到那一边的余量（-4 是离裁剪边留的缝）",
  h.comboPlan(80, 120), { up: true, maxHeight: 116 });
eq("上方更窄时不为所动：朝下，高度收到下方的余量", h.comboPlan(100, 80), { up: false, maxHeight: 96 });
eq("两侧都快贴边：守住三行的底线，余下的交给清单自己滚", h.comboPlan(30, 20), { up: false, maxHeight: 66 });

const ifaceSelect = byBind("rules.0.conditions.2.value");
eq("接口值只能是选出来的", ifaceSelect?.tagName, "SELECT");
eq("候选来自本机网卡：没插线的口（en7）也在，VPN 隧道（utun3）不在",
  ifaceSelect?.children.map((o) => o.value), ["", "*", "en0", "en5", "en7"]);
eq("第一项是占位文案，「任意网卡」是一条真能存的候选",
  ifaceSelect?.children[0].textContent, strings["editor.iface_placeholder"]);
choose(ifaceSelect, "*");
eq("选「任意网卡」写进配置的是通配值 *（旧版的「—」空选项存不下去）",
  h.draft.rules[0].conditions[2].value, "*");
// 配置里写着这台机器现在没有的网卡是合法的（换了笔记本、拔了扩展坞）。
h.draft.rules[0].conditions[2].value = "en9";
h.renderAll();
const gone = byBind("rules.0.conditions.2.value");
eq("已存但本机已经没有的网卡：原样列出并选中，不会被悄悄改成别的卡",
  [gone?.children.map((o) => o.value), gone?.children.filter((o) => o.selected).map((o) => o.value)],
  [["", "*", "en9", "en0", "en5", "en7"], ["en9"]]);
// 换类型 = 换值的语义。旧值不跟着清，就会被新类型的输入框当成「已存的值」显示出来
// —— SSID 与接口互相串台就是这么来的。
choose(byBind("rules.0.conditions.2.type"), "wifi_ssid");
const ssids = findAll((e) => e.dataset?.comboList === "rules.0.conditions.2.value")[0];
const ssidOpts2 = (ssids?.children || []).filter((e) => e.dataset?.act === "combo-pick");
eq("接口条件切成 SSID 类型后：en9 不混进候选，值也被清空",
  [ssidOpts2.map((o) => o.dataset.v), byBind("rules.0.conditions.2.value")?.value,
   h.draft.rules[0].conditions[2].value],
  [["Café", "Office_5G"], "", undefined]);
// 浮层只是选取器：点开、选中，名字交给输入框，浮层自己收起；直接打字则完全不经过它。
const ssidInp2 = byBind("rules.0.conditions.2.value");
click(findAll((e) => e.dataset?.act === "combo-toggle" &&
  e.dataset?.path === "rules.0.conditions.2.value")[0]);
click(ssidOpts2[0]);
eq("从候选里选一个：输入框与配置收到的都是那个 SSID",
  [ssidInp2?.value, h.draft.rules[0].conditions[2].value], ["Café", "Café"]);
check("选完浮层收起：它不留下「自己选中了什么」这份状态冒充配置",
  !ssids.classList.contains("open"));
type(byBind("rules.0.conditions.2.value"), "");
eq("清空输入框是不写 value，而不是留下一个能和「空名字网络」Match 上的空串",
  h.draft.rules[0].conditions[2].value, undefined);
type(byBind("rules.0.conditions.2.value"), "Hidden Lab WiFi");
resetSaves();
await h.$("btn-save").onclick();
eq("直接输入含空格的 SSID 原样送达（不用先点任何「手动」项）",
  saveOf("save_profile")?.payload?.rules?.[0]?.conditions?.[2]?.value, "Hidden Lab WiFi");
h.pick("profile", "office");

group("文本编辑与 save_profile");
resetSaves();
type(byBind("name"), "Office 5G（工位）");
await h.$("btn-save").onclick();
let p = saveOf("save_profile")?.payload;
eq("改名字后保存出去的是新名字", p?.name, "Office 5G（工位）");
eq("id 不跟着名字变（它是主键）", p?.id, "office");
check("每条规则与条件都带 id（校验要求唯一）", (p?.rules || []).every((r) => r.id && r.conditions.every((c) => c.id)));

group("DNS 三态：缺省 ≠ 空串");
resetSaves();
choose(byActPre("dns-tri", "then."), "keep");
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
check("选「不改」= 整个 dns 字段消失", p && !("dns" in p.then.network), JSON.stringify(p?.then?.network));
resetSaves();
choose(byActPre("dns-tri", "then."), "auto");
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("选「交回系统」= 显式写 dns: \"\"", p?.then?.network?.dns, "");
resetSaves();
choose(byActPre("dns-tri", "then."), "set");
type(byBind("then.network.dns"), "192.168.1.1, 8.8.8.8");
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("选「指定」= 按输入框的值下发", p?.then?.network?.dns, "192.168.1.1, 8.8.8.8");
eq("另一分支的三态不被牵连：else 的 dns 仍是「交回系统」",
  byActPre("dns-tri", "else.")?.children.find((c) => c.selected)?.value, "auto");

group("THEN / ELSE 同时呈现，改一支不会牵动另一支");
check("ELSE 的那一块里没有 THEN 的动作卡", byBind("else.one_shot.0.action.app") === null);
resetSaves();
choose(byBind("else.network.mode"), "manual");
type(byBind("else.network.ip"), "10.10.0.20");
// 静态地址得填全（ip / netmask / gateway 缺一后端就整份拒绝）：而这个 harness 的产物
// 要喂给 Rust 侧做 serde 往返 —— 收下一条注定被拒的 payload 只能证明「前端会少填」，
// 证明不了任何契约。
type(byBind("else.network.netmask"), "255.255.255.0");
type(byBind("else.network.gateway"), "10.10.0.1");
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("ELSE 改成了静态地址",
  [p?.else?.network?.mode, p?.else?.network?.ip, p?.else?.network?.netmask, p?.else?.network?.gateway],
  ["manual", "10.10.0.20", "255.255.255.0", "10.10.0.1"]);
eq("ELSE 分支保留自己的删除路由", p?.else?.network?.routes, [{ dest: "10.0.0.0/8", metric: 0, delete: true }]);
eq("THEN 的动作一条没少（改 ELSE 不会把 THEN 挤掉）",
  p?.then?.one_shot?.map((a) => a.id), ["a1", "a2", "a3", "a4"]);

group("3B 动作载荷");
const acts = p?.then?.one_shot || [];
eq("动作类型原样送达", acts.map((a) => a.action.type),
  ["launch_app", "run_script", "launch_app", "set_default_printer"]);
check(
  "launch_app 只带 app，不会顺手写出 path",
  acts.every((a) =>
    a.action.type === "launch_app" ? "app" in a.action && !("path" in a.action) : true,
  ),
);
check(
  "run_script 只带 path，不会顺手写出 app",
  acts.every((a) => (a.action.type === "run_script" ? "path" in a.action && !("app" in a.action) : true)),
);
check(
  "set_default_printer 只带 printer：一台打印机既不是可执行文件，也没有参数",
  acts.every((a) => (a.action.type === "set_default_printer"
    ? JSON.stringify(a.action) === JSON.stringify({ type: "set_default_printer", printer: "Office LaserJet" })
    : true)),
  JSON.stringify(acts[3]),
);
eq("未声明 args 时不写出空数组", (p?.then?.one_shot || []).every((a) => !("args" in a.action)), true);
eq("disabled 的动作照样保存，只是标成禁用", p?.then?.one_shot?.[2]?.enabled, false);
// 「谁先跑」没有单独的字段可写：数组顺序本身就是执行顺序，所以一条动作能带也只有三个键。
eq("每条动作只有 id / enabled / action 三个键",
  (p?.then?.one_shot || []).map((a) => Object.keys(a).sort()),
  Array(4).fill(["action", "enabled", "id"]));
resetSaves();
toggle(byBind("then.one_shot.1.action.elevated"), true);
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("提权标记只贴在 run_script 上", p?.then?.one_shot?.[1]?.action, { type: "run_script", path: "scripts/office-vpn.sh", elevated: true });
check("其他动作没有被连带标上提权", p?.then?.one_shot?.[0]?.action?.elevated === undefined);

group("3B 动作：启动程序 = 已装程序候选 + 浏览… + 手输，三路写同一个字段");
// 「启动程序」这一格有三个入口：候选里挑一个、系统文件选择器挑一个、键盘直接打。它们写的
// 都是 `action.app` 这一个字段 —— 这一组查的就是三个入口没有各写各的，以及取消/起不来
// 这两条失败路不会把输入框或配置改坏。
const appInp = byBind("then.one_shot.0.action.app");
eq("启动目标是能直接打字的输入框（绿色免安装程序不在任何菜单里）", appInp?.tagName, "INPUT");
eq("输入框显示的就是配置里的值（launch_app 要的是平台受理的那个形状）",
  [appInp?.value, appInp?.placeholder], ["/Applications/Slack.app", strings["editor.app_placeholder"]]);
const appDrop = findAll((e) => e.dataset?.comboList === "then.one_shot.0.action.app")[0];
const appOpts = (appDrop?.children || []).filter((e) => e.dataset?.act === "combo-pick");
eq("候选给人看的是名字", appOpts.map((o) => o.textContent), ["Firefox", "Keka", "Slack"]);
eq("写回去的是路径（名字只是这一格的显示面）",
  appOpts.map((o) => o.dataset.v),
  ["/Applications/Firefox.app", "/Applications/Keka.app", "/Applications/Slack.app"]);
check("悬停说明露出完整路径：同名程序不少，选错一条就起错程序",
  appOpts.every((o) => o.title === o.dataset.v));
eq("没有路径的记录不渲染成一条点了没反应的候选", appOpts.some((o) => !o.dataset.v), false);
const appCaret = findAll((e) => e.dataset?.act === "combo-toggle" &&
  e.dataset?.path === "then.one_shot.0.action.app")[0];
check("同一个组合框机制：箭头在输入框右端，打开前浮层收起",
  !!appCaret && appDrop?.classList?.contains("open") === false);
click(appCaret);
check("点箭头展开", appDrop.classList.contains("open"));
click(appOpts[0]);
eq("从候选里选一个：输入框与配置收到的都是那条路径",
  [appInp?.value, h.draft.then.one_shot[0].action.app], ["/Applications/Firefox.app", "/Applications/Firefox.app"]);
check("选完收起", !appDrop.classList.contains("open"));
const browseBtn = findAll((e) => e.dataset?.act === "browse-app" &&
  e.dataset?.path === "then.one_shot.0.action.app")[0];
check("「浏览…」与输入框同一行（它是这一格的第三个入口，不是另一处控件）",
  !!browseBtn && browseBtn.parent === appInp.parent.parent);
eq("按钮文案取自字典", browseBtn?.textContent, strings["editor.browse"]);
const picksBefore = invokeLog.filter((c) => c.cmd === "pick_app").length;
click(browseBtn);
await settle();
eq("点浏览问的是 pick_app", invokeLog.filter((c) => c.cmd === "pick_app").length, picksBefore + 1);
eq("选择器给回来的路径抄进同一个输入框，配置跟着改",
  [appInp?.value, h.draft.then.one_shot[0].action.app],
  ["/Users/me/Desktop/GreenThing.app", "/Users/me/Desktop/GreenThing.app"]);
pickAppResult = null;
click(browseBtn);
await settle();
eq("取消（null）什么都不写：输入框保持原样",
  [appInp?.value, h.draft.then.one_shot[0].action.app],
  ["/Users/me/Desktop/GreenThing.app", "/Users/me/Desktop/GreenThing.app"]);
pickAppError = "zenity: command not found";
click(browseBtn);
await settle();
check("选择器起不来：错误原文说给用户，不是静默无反应",
  findById("toast").textContent.includes("zenity"));
pickAppError = null;
type(appInp, "/opt/tools/foo");
eq("手输仍然直接写配置（下拉与浏览都只是加速器）",
  h.draft.then.one_shot[0].action.app, "/opt/tools/foo");
resetSaves();
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("手输的路径原样送达", p?.then?.one_shot?.[0]?.action, { type: "launch_app", app: "/opt/tools/foo" });
// 一台候选都没有（干净机器 / 枚举失败）：下拉整个不出现，但手输与浏览必须还在 ——
// 少了候选只是少一条捷径，不是把这一格关掉。对照值现取草稿：上面那次保存会把表单
// 从磁盘回填一遍（替身里的 get_config 不跟着 save 变），写死的路径会被它冲掉。
const allApps = h.appList;
h.appList = [];
h.renderAll();
const curApp = h.draft.then.one_shot[0].action.app;
check("零候选时：输入框与「浏览…」都还在，只有箭头消失",
  byBind("then.one_shot.0.action.app")?.value === curApp &&
  !findAll((e) => e.dataset?.act === "combo-toggle" && e.dataset?.path === "then.one_shot.0.action.app")[0] &&
  !!findAll((e) => e.dataset?.act === "browse-app" && e.dataset?.path === "then.one_shot.0.action.app")[0],
  `value=${JSON.stringify(byBind("then.one_shot.0.action.app")?.value)} 期望=${JSON.stringify(curApp)} ` +
  `caret=${!!findAll((e) => e.dataset?.act === "combo-toggle" && e.dataset?.path === "then.one_shot.0.action.app")[0]} ` +
  `browse=${!!findAll((e) => e.dataset?.act === "browse-app" && e.dataset?.path === "then.one_shot.0.action.app")[0]}`);
h.appList = allApps;
h.renderAll();

group("3B 动作：默认打印机的候选来自系统，不是让用户手抄名字");
const prInput = byBind("then.one_shot.3.action.printer");
eq("打印机那一项是下拉选择框（只能从系统已添加的打印机中选择）", prInput?.tagName, "SELECT");
eq("候选就是系统里的打印机名（顺序照后端），无手动输入项",
  prInput?.children.map((o) => o.value), ["", "Office LaserJet", "Home Inkjet"]);
eq("下拉文字说人话：有标签用标签、当前默认那台带标注；value 仍是下发用的队列名",
  prInput?.children.map((o) => o.textContent),
  [strings["editor.printer_placeholder"],
   `LaserJet 476 · 3F ${strings["editor.printer_is_default"]}`, "Home Inkjet"]);
resetSaves();
choose(byBind("then.one_shot.0.action.type"), "set_default_printer");
eq("换类型会重建卡片：app 框没了，换成打印机下拉框",
  [byBind("then.one_shot.0.action.app"), byBind("then.one_shot.0.action.printer")?.tagName], [null, "SELECT"]);
choose(byBind("then.one_shot.0.action.printer"), "Home Inkjet");
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("改了类型的动作只带自己那一个字段（app / args 都不会残留）",
  p?.then?.one_shot?.[0]?.action, { type: "set_default_printer", printer: "Home Inkjet" });

resetSaves();
const addPrinter = findAll((e) => e.dataset?.act === "add-one" && e.dataset?.t === "set_default_printer")[0];
check("动作区有「+ 打印机」这一颗按钮", !!addPrinter);
click(addPrinter);
eq("新加的是一张干净的空卡片：没有 args，也没有 app/path 占位",
  h.draft.then.one_shot[4].action, { type: "set_default_printer", printer: "" });
choose(byBind("then.one_shot.4.action.printer"), "Home Inkjet");
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("选择的打印机原样送达，且新加的那条也只有三个键",
  [p?.then?.one_shot?.[4]?.action, "args" in (p?.then?.one_shot?.[4]?.action || {}),
   Object.keys(p?.then?.one_shot?.[4]).sort()],
  [{ type: "set_default_printer", printer: "Home Inkjet" }, false, ["action", "enabled", "id"]]);
check("执行清单讲得出这条动作：类型词 + 目标名字",
  h.planList(p.then.one_shot.slice(4), null).includes(strings["editor.action_printer"]) &&
  h.planList(p.then.one_shot.slice(4), null).includes("Home Inkjet"));

group("3B2 常驻动作：表单往返与 worker 徽标");
resetSaves();
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("常驻动作不会因为编辑别的字段而从 payload 里消失", p?.then?.persistent?.length, 1);
eq(
  "3B2 载荷逐字段送达（type / tunnel / interval_secs 都靠它们才能起 worker）",
  (p?.then?.persistent || []).map((a) => [a.id, a.enabled, a.action.type, a.action.tunnel, a.action.interval_secs]),
  [["p1", true, "keep_wireguard_connected", "wg0", 15]],
);
// worker 起来的先后也由数组顺序表达：载荷里没有第二个「谁先起」的键。
check(
  "常驻动作只有 id / enabled / action 三个键",
  (p?.then?.persistent || []).every((a) => JSON.stringify(Object.keys(a).sort()) === '["action","enabled","id"]'),
  JSON.stringify((p?.then?.persistent || []).map((a) => Object.keys(a))),
);

const activeOffice = (workers) =>
  engineView(
    [{ id: "office", name: "Office_5G", enabled: true, matched: true, status: "active", rules: [] }],
    { state: "active", id: "office" },
    null,
    workers,
  );
const status = (over) => ({
  id: "p1", label: "wireguard:wg0", state: "repaired",
  interval: 15, repairs: 2, at: 100, ...over,
});

h.view = activeOffice([status({})]);
h.refreshLive();
const wchip = findById("wchip-then.persistent.0");
check("Active 方案的 THEN 分支渲染了常驻动作卡片", !!wchip);
eq("徽标文案取自后端报的 worker 状态", wchip?.textContent, strings["editor.worker_repaired"]);
check("repaired 是绿的（维持住了，只是动过一次手）", !!wchip?.className.includes("ok"));
check("悬停说明讲清是哪条 tunnel、多久查一次", wchip?.title.includes("wireguard:wg0") && wchip?.title.includes("15"));

h.view = activeOffice([status({ state: "faulted", error: "tunnel wg0 not found" })]);
h.refreshLive();
const werr = findById("werr-then.persistent.0");
check("faulted 的徽标是红的", findById("wchip-then.persistent.0").className.includes("bad"));
eq("失败原文摊在卡片上，不留给用户猜", werr?.textContent, "tunnel wg0 not found");
check("失败原文不是 hidden（藏着等于没说）", werr?.hidden === false);

h.view = activeOffice([{ id: "p1", label: "wireguard:wg0", state: "satisfied", interval: 15, repairs: 0, at: 100 }]);
h.refreshLive();
eq("satisfied 与 repaired 用词不同：一个什么都没做，一个修过一次",
  findById("wchip-then.persistent.0")?.textContent, strings["editor.worker_satisfied"]);

h.view = activeOffice([{ id: "p1", label: "wireguard:wg0", state: "pending", interval: 15, repairs: 0, at: 100 }]);
h.refreshLive();
eq("pending 由后端发（worker 起了，第一次核对还没回）",
  findById("wchip-then.persistent.0")?.textContent, strings["editor.worker_pending"]);
check("未知的状态取值不会串成一条假的成功徽标", (() => {
  h.view = activeOffice([{ id: "p1", label: "x", state: "some_future_state", interval: 15, at: 1 }]);
  h.refreshLive();
  return findById("wchip-then.persistent.0").textContent === strings["editor.worker_pending"];
})());

h.view = activeOffice([]);
h.refreshLive();
eq("表单里有、worker 集合里没有 = 徽标留空（后端要说是 pending 会自己说）",
  findById("wchip-then.persistent.0")?.textContent, "");

h.view = engineView(
  [{ id: "home", name: "HomeWiFi", enabled: true, matched: true, status: "active", rules: [] }],
  { state: "active", id: "home" }, null, [status({})],
);
h.refreshLive();
eq("Active 的是别的方案：这一支的徽标不能借它的状态来显示",
  findById("wchip-then.persistent.0")?.textContent, "");

h.view = activeOffice([status({})]);
// 常驻动作只随 Active 的 THEN 起。哪怕用户在 ELSE 那一块里填了一条，也不许给它挂徽标。
h.draft.else ||= {};
h.draft.else.persistent = [{ id: "pX", enabled: true, action: { type: "periodic_script", path: "keepalive.sh", interval_secs: 30 } }];
h.renderAll();
h.refreshLive();
check("ELSE 那一块也有自己的卡片（表单是完整的）", !!findById("wchip-else.persistent.0"));
eq("但 worker 徽标只属于 Active 的 THEN：ELSE 的卡片一律留空",
  findById("wchip-else.persistent.0")?.textContent, "");
eq("同一时刻 THEN 的徽标是亮的", findById("wchip-then.persistent.0")?.textContent, strings["editor.worker_repaired"]);
h.pick("profile", "office");   // 丢掉上面那条临时草稿

const withPersistent = (list) => ({ which: "then", branch: { persistent: list } });
const held = h.planPersistent(withPersistent([
  { id: "p1", enabled: true, action: { type: "keep_wireguard_connected", tunnel: "wg0", interval_secs: 15 } },
  { id: "p2", enabled: true, action: { type: "periodic_script", path: "/opt/ops/keepalive.sh", interval_secs: 60 } },
]));
check("执行清单里的常驻段说得出每条查什么、多久一次",
  held.includes("wg0") && held.includes("15") && held.includes("keepalive.sh") && held.includes("60"));
check("禁用的常驻动作不进清单（点了也不会起 worker）", !h.planPersistent(withPersistent([
  { id: "p9", enabled: false, action: { type: "keep_wireguard_connected", tunnel: "wg9", interval_secs: 15 } },
])).includes("wg9"));
check("走 ELSE 时清单改口：这一支一条都不会起",
  h.planPersistent({ which: "else", branch: { persistent: [{ id: "p1", enabled: true, action: { type: "periodic_script", path: "/opt/ops/k.sh", interval_secs: 30 } }] } })
    .includes(strings["editor.persistent_else"]));

group("第 1 列的勾选：不动选中态，直接把磁盘上那一份改掉");
click(officeRow);
const homeQuick = findAll((e) => e.dataset?.rowQuick === "home")[0];
check("未选中的那一行也有自己的启用与快速切换勾选框", !!homeQuick && !!findAll((e) => e.dataset?.rowEn === "home")[0]);
resetSaves();
toggle(homeQuick, false);
await settle();
p = saveOf("save_profile")?.payload;
eq("取消快速切换 = 立刻写回这一条 Profile", [p?.id, p?.quick, p?.enabled], ["home", false, true]);
eq("其它字段照原样带回去（不是只发改动的那一个）", [p?.name, p?.rules?.length, p?.then?.network?.dns], ["HomeWiFi", 1, "127.0.0.1"]);
eq("勾选的是别人这一行，选中态不该因此改变", h.sel, { kind: "profile", id: "office" });

group("第 1 列末尾：兜底是一条特殊的 Profile");
click(byAct("sel-fallback"));
eq("切到兜底", h.sel, { kind: "fallback" });
check("兜底的条件区是空的，并说明「空 = 任意条件」",
  findAll((e) => e.dataset?.bind?.startsWith("rules.")).length === 0 &&
  findById("col-cond").textContent.includes(strings["editor.any_conditions"]));
check("兜底的 3A 没有分支前缀（它没有 THEN / ELSE 之分）",
  !!byBind("network.mode") && byBind("then.network.mode") === null);
check("Action 列对兜底关门：它只处置网卡，不跑动作",
  findById("col-act").textContent === strings["editor.action_none"]);
resetSaves();
await h.$("btn-save").onclick();
let g = saveOf("save_global")?.payload;
eq("兜底网络按 cleanNetwork 归一", g?.fallback?.network?.mode, "dhcp");
check("兜底不是 Profile：payload 里没有 rules / detection", g && !("rules" in (g.fallback || {})));
eq("save_global 是整份替换：白名单必须原样带过去，不能因为这里没编辑就丢掉",
  g?.allowed_scripts, configFixture.allowed_scripts);
check("save_global 不会顺手写 profiles", g && !("profiles" in g));
check("脚本白名单的编辑界面已经搬到软件设置", findById("scripts-box") === null);

group("立即应用前的执行清单");
click(officeRow);
const saved = h.cfg.profiles.find((x) => x.id === "office");
let plan = h.applyPlan(saved);
eq("命中 → 清单讲的是 THEN", plan.which, "then");
let listHtml = h.planList(plan.branch.one_shot, null);
// 清单的行序就是执行顺序：后端按数组序逐条跑，前一条结束（成功、失败或超过它自己的
// 等待上限）才轮到下一条。这里故意让 id 序、字母序都和数组序不一致，
// 好让「被重排了」这种回归一定留下痕迹。
const liText = (html) => (html.match(/<li>[\s\S]*?<\/li>/g) || []).map((s) => s.replace(/<[^>]*>/g, ""));
const ordered = h.planList(
  [
    { id: "b", action: { type: "launch_app", app: "Second" } },
    { id: "a", action: { type: "launch_app", app: "First" } },
    { id: "c", action: { type: "launch_app", app: "AlsoFirst" } },
  ],
  null,
);
check("清单是有序列表：行首编号说的是「第几条」", ordered.startsWith("<ol>") && ordered.endsWith("</ol>"));
eq("一条一行", liText(ordered).length, 3);
check("行序 = 数组序（谁写在上面谁先跑）",
  ["Second", "First", "AlsoFirst"].every((n, i) => (liText(ordered)[i] || "").includes(n)),
  JSON.stringify(liText(ordered)));
check("禁用动作不进清单（点了也不会跑）", !listHtml.includes("Test.app"));
const elevatedLine = h.planList([{ id: "x", action: { type: "run_script", path: "p.sh", elevated: true } }], null);
check("提权动作在清单上明说要授权", elevatedLine.includes(strings["editor.action_elevated"]));
check("没提权的动作不会被顺手标上提权", !h.planList([{ id: "y", action: { type: "launch_app", app: "Foo" } }], null).includes(strings["editor.action_elevated"]));
listHtml = h.planList(plan.branch.one_shot, { outcomes: [], running: true, total: 3 });
check("还在跑的动作标成 running，而不是成功", listHtml.includes(strings["editor.action_running"]));
h.view = engineView(
  [
    { id: "home", name: "HomeWiFi", enabled: true, matched: true, status: "conflict", rules: [] },
    { id: "office", name: "Office_5G", enabled: true, matched: true, status: "conflict", rules: [] },
  ],
  { state: "conflict", ids: ["home", "office"] },
);
plan = h.applyPlan(h.cfg.profiles.find((x) => x.id === "office"));
check("冲突时清单直接拒绝，并说明还有谁命中", !!plan.refuse && plan.refuse.includes("HomeWiFi"));
h.view = engineView(
  [{ id: "router_only", name: "IdentifyByRouterOnly", enabled: false, matched: false, status: "disabled", rules: [] }],
  { state: "no_active_profile" },
);
plan = h.applyPlan(h.cfg.profiles.find((x) => x.id === "router_only"));
check("禁用的 Profile 不进清单", !!plan.refuse);

group("卡片头部的上下移动：这一列从上到下就是执行顺序");
click(officeRow);
const mv = (kind, pre, i, d) =>
  findAll((e) => e.dataset?.act === `move-${kind}` && e.dataset?.pre === pre &&
    e.dataset?.i === String(i) && e.dataset?.d === String(d))[0] || null;
const oneIds = () => (h.draft.then.one_shot || []).map((a) => a.id);
eq("回填出来的动作顺序照配置", oneIds(), ["a1", "a2", "a3", "a4"]);
check("第一张的「上移」是禁用状态：到头了得让人看出来，而不是点了没反应",
  mv("one", "then.", 0, -1)?.attributes?.disabled === "");
check("最后一张的「下移」同样禁用", mv("one", "then.", 3, 1)?.attributes?.disabled === "");
check("中间的卡两头都能点",
  mv("one", "then.", 1, -1)?.attributes?.disabled === undefined &&
  mv("one", "then.", 1, 1)?.attributes?.disabled === undefined);
resetSaves();
click(mv("one", "then.", 0, 1));
eq("点第一张的「下移」= 它与下一条交换位置", oneIds(), ["a2", "a1", "a3", "a4"]);
click(mv("one", "then.", 1, -1));
eq("再点一次就换回来（交换是对合的）", oneIds(), ["a1", "a2", "a3", "a4"]);
click(mv("one", "then.", 0, 1));
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("保存出去的就是屏幕上这个顺序：后端按数组序逐条跑，没有别处的开关",
  p?.then?.one_shot?.map((a) => a.id), ["a2", "a1", "a3", "a4"]);

const plIds = () => (h.draft.then.persistent || []).map((a) => a.id);
click(byAct("add-persist"));
eq("常驻段现在是两条，原有的那条仍在最前（它先起 worker）", plIds()[0], "p1");
check("单条时两头都到头；现在两条，第一张的「下移」该能点了",
  mv("persist", "then.", 0, 1)?.attributes?.disabled === undefined);
click(mv("persist", "then.", 1, -1));
eq("新加的那条被移到前面，先起的就是它", plIds()[1], "p1");
h.pick("profile", "office");   // 丢掉这条还没填路径的临时动作，别把它送进 fixture

group("广播只换徽标，不重建表单");
click(officeRow);
type(byBind("name"), "打字打到一半");
const before = byBind("name");
h.view = engineView(
  [{ id: "office", name: "Office_5G", enabled: true, matched: true, status: "active", rules: [] }],
  { state: "active", id: "office" },
);
h.refreshLive();
await settle(2);
check("刷新后原来那个输入框还在（没有被重绘掉）", byBind("name") === before);
eq("未保存的输入没有被广播覆盖", before.value, "打字打到一半");

group("生效的那一行描边，且与状态条同口径");
const lastRun = {
  profile_id: "office", profile: "Office_5G", branch: "then", total: 3, running: false,
  outcomes: [{ id: "a1", ok: true }, { id: "a2", ok: false, error: "exit 1" }],
};
h.view = engineView(
  [
    { id: "home", name: "HomeWiFi", enabled: true, matched: false, status: "not_matched", rules: [] },
    { id: "office", name: "Office_5G", enabled: true, matched: true, status: "active", rules: [] },
  ],
  { state: "active", id: "office" }, lastRun,
);
h.refreshLive();
const rowCls = (id) => findAll((e) => e.dataset?.act === "sel-profile" && e.dataset?.id === id)[0]?.className || "";
check("ACTIVE 那一行有 live 描边", rowCls("office").split(/\s+/).includes("live"));
check("没命中的行不该跟着发绿", !rowCls("home").split(/\s+/).includes("live"));
eq("行里的徽标与状态条第 1 格用同一个词",
  [findById("pbadge-office").textContent, findById("st-profile").textContent.includes(strings["editor.status_active"])],
  [strings["editor.status_active"], true]);

group("3B1 结果：跑完一条亮一条");
h.refreshLive();
eq("成功的一条标 OK", findById("achip-then.one_shot.0")?.textContent, strings["editor.action_ok"]);
eq("失败的一条标 Failed", findById("achip-then.one_shot.1")?.textContent, strings["editor.action_failed"]);
eq("失败原文挂在卡片里", findById("aerr-then.one_shot.1")?.textContent, "exit 1");
eq("禁用的那一条不参与运行，也就没有徽标", findById("achip-then.one_shot.2")?.textContent, "");
check("汇总行说清是哪个方案、哪一支、几条里成了几条",
  !findById("run-line-then").hidden && findById("run-line-then").textContent.includes("1/3"));
eq("状态条第 4 格与卡片同一份事实",
  [findById("st-act").textContent.includes(strings["editor.action_ok"]),
   findById("st-act").textContent.includes(strings["editor.action_failed"])], [true, true]);
// 结果按「当前选中的方案 + 这一支」来挂靠，而不是按「引擎当下 Active 的是谁」：
// 正在编辑 HomeWiFi 时看到 Office 跑出来的 OK，比看不到徽标更容易骗人。
h.view = engineView(
  [{ id: "office", name: "Office_5G", enabled: true, matched: true, status: "active", rules: [] }],
  { state: "active", id: "office" },
  { ...lastRun, profile_id: "home", profile: "HomeWiFi" },
);
h.refreshLive();
eq("别的方案跑出来的结果不许挂在这一支上", findById("achip-then.one_shot.0")?.textContent, "");
h.view = engineView(
  [{ id: "office", name: "Office_5G", enabled: true, matched: true, status: "active", rules: [] }],
  { state: "active", id: "office" },
  { ...lastRun, branch: "else" },
);
h.refreshLive();
check("别的分支（else）跑出来的结果同样不许串过来", findById("achip-then.one_shot.0")?.textContent === "");
click(officeRow);

/// 界面上真实渲染出来的中日韩文本节点。两条语言断言共用它：中文那一趟必须**扫得到**，
/// 否则英文那一趟的「一个都没有」就只是空转。
const cjkInDom = () => {
  const seen = [];
  const scan = (n) => {
    for (const c of n.children || []) {
      if (c.nodeType === 3) {
        if (/[　-〿㐀-鿿가-힯]/.test(c.text)) seen.push(c.text.trim().slice(0, 40));
      } else scan(c);
    }
  };
  scan(body);
  return seen;
};

group("跟随全局语言：软件设置里换了语言，这个窗口当下就改口");
// 重新敲一遍：上面那次 click 是从磁盘回填表单的，未保存的输入本来就该被它冲掉。
type(byBind("name"), "打字打到一半");
langCode = "zh";
await broadcast("netsense://status", { language: "zh", engine: h.view });
check("重新取了一次词表", invokeLog.filter((c) => c.cmd === "get_strings").length >= 2);
eq("按钮换成了中文", findById("btn-apply").textContent, zhStrings["editor.force_apply"]);
eq("状态条的小标题也换了", findById("sk-1").textContent, zhStrings["editor.state_active_profile"]);
eq("表单没有被这次重绘制丢：正在编辑的名字还在", byBind("name")?.value, "打字打到一半");
eq("记下当前语言，下一次广播不再重复取词", h.langShown, "zh");
check("这一趟确实扫到了中文（否则下面的英文断言是空转）", cjkInDom().length > 0);
const before2 = invokeLog.filter((c) => c.cmd === "get_strings").length;
await broadcast("netsense://status", { language: "zh", engine: h.view });
eq("语言没变就不再多问一次后端", invokeLog.filter((c) => c.cmd === "get_strings").length, before2);

// 换回 English 再走一遍。这一步查的是「界面里剩下的中文是不是全都出自字典」：
// 文案只有两个来源 —— 静态标记里的兜底英文，和 applyI18n 从后端词表写进去的值。
// 用户自己打的中文字在 value 上，不在 textContent 上，所以扫 textContent 不会误伤。
langCode = "en";
await broadcast("netsense://status", { language: "en", engine: h.view });
eq("换回 English 后按钮又改口", findById("btn-apply").textContent, strings["editor.force_apply"]);
eq("<html lang> 跟着后端换回 en", document.documentElement.lang, "en");
eq("English 界面里不残留写死的中文字面", cjkInDom(), []);

group("广播之后，「当前网络」那一格跟着变新");
// 这一格有两份来源，分开是因为它们的「变新」代价完全不同：
//   地址/掩码/网关/DNS/信号 —— 引擎每轮广播都自带一份新采样（`status`），零代价、零延迟；
//   接口标签与这张口自己的 MAC —— 只能问 `get_interfaces`（子进程），而它只在**身份**
//   变了的时候才会变。所以判据是身份指纹，不是「来了一条广播就再问一次后端」。
// 早先的版本是每条广播问一次：一轮下发会连发两条（evaluation + status），编辑器于是
// 跟着每秒拉一次子进程 —— 现场feedback「刷新太慢」的一部分就是这个。
const askedNics = () => invokeLog.filter((c) => c.cmd === "get_interfaces").length;
check("开窗时显示的是取到那份地址", findById("st-net").textContent.includes("192.168.1.100/24"));
const nicsAsked = askedNics();
await broadcast("netsense://status", {
  language: "en",
  status: { connected: true, ssid: "Office_5G", ipv4: "10.20.30.40/24", netmask: "255.255.255.0",
    gateway: "10.20.30.1", dns: "10.20.30.1", rssi: -47, iface: "en0" },
  engine: h.view,
});
check("下发过静态 IP 之后地址立刻见新，而编辑器没有为它多问一次后端",
  askedNics() === nicsAsked, `get_interfaces 从 ${nicsAsked} 次变成了 ${askedNics()} 次`);
check("新地址现在就摆在格 3 里", findById("st-net").textContent.includes("10.20.30.40/24"));
check("旧地址不再留在界面上", !findById("st-net").textContent.includes("192.168.1.100/24"));
check("DNS 也跟着换了", findById("st-net").textContent.includes("10.20.30.1"));
check("信号强度用的是广播里那一份", findById("st-net").textContent.includes("-47 dBm"));

group("换了口才重取网卡明细，同一条身份不重复取");
// 身份指纹里的 `primary_interface` / `interfaces` 一变，就说明连着的是另一张口了：
// 这时候接口标签、这张口自己的 MAC 才可能不同，才值得付一次子进程的代价。
nicFixture = [
  { name: "en5", kind: "wired", label: "USB 10/100/1000 LAN", mac: "aa:00:00:00:00:05", ipv4: "10.20.30.40/24" },
  { name: "en0", kind: "wireless", ipv4: "192.168.1.100/24", netmask: "255.255.255.0", dns: "192.168.1.1" },
];
const movedView = {
  ...h.view,
  snapshot: { ...h.view.snapshot, primary_interface: "en5", interfaces: ["en5"], tunnels: [] },
};
const beforeMove = askedNics();
await broadcast("netsense://status", {
  language: "en",
  status: { connected: true, ipv4: "10.20.30.40/24", dns: "10.20.30.1", iface: "en5" },
  engine: movedView,
});
check("身份一变，网卡明细重取了一次", askedNics() === beforeMove + 1);
check("新接口那一张口的标签上了界面", findById("st-net").textContent.includes("USB 10/100/1000 LAN"));
check("它的 MAC 也带出来了", findById("st-net").textContent.includes("aa:00:00:00:00:05"));
const afterMove = askedNics();
await broadcast("netsense://status", {
  language: "en",
  status: { connected: true, ipv4: "10.20.30.40/24", dns: "10.20.30.1", iface: "en5" },
  engine: movedView,
});
await broadcast("netsense://evaluation", movedView);
check("同一份身份再来两条广播，一次都不再多问", askedNics() === afterMove);
check("有线口不再显示那一行空着的 SSID 值", !findById("st-net").textContent.includes("Office_5G"));

// —————————————————————— 条件即时预览 ——————————————————————

group("条件徽标跟着表单立刻预览，不等引擎那一轮");
// 现场那句「matched 和绿框出现得太慢」慢在引擎那一轮：它要按自己的采样节律走，还要等
// `change_delay_secs` 去抖（默认 5 秒）。这一组只问**接线**：表单改了以后徽标有没有就地跟上、
// 发出去的是不是草稿、合并有没有生效、「已经生效」有没有被预览冒充。
// 匹配本身算得对不对不在这里 —— 那由 `conditions::evaluator` 的 Rust 单元测试守，
// 这里的替身只是把同一份快照的三态报回来。
const previewCalls = () => invokeLog.filter((c) => c.cmd === "preview_match").length;
const lastPreviewPayload = () =>
  JSON.parse(invokeLog.filter((c) => c.cmd === "preview_match").pop().args.payload);
// 前面几组把 `view` 换成过别的替身（worker 状态那几组），这一组从基准那份重新开始：
// 「引擎说已生效」那几条断言要的是 `viewFixture` 里 office = active 这个事实。
h.view = viewFixture;
h.pick("profile", "home");
await wait(300);
await settle();
check("选中一条以后编辑器问过条件预览", previewCalls() >= 1, `一次都没问（${previewCalls()}）`);
check("发出去的是整份清单，不只是选中那一条",
  lastPreviewPayload().length === h.cfg.profiles.length,
  `清单 ${lastPreviewPayload().length} 条，配置 ${h.cfg.profiles.length} 条`);
check("home 开局没命中：徽标写着未命中，规则卡也没有绿描边",
  findById("pbadge-home").textContent === strings["editor.status_not_matched"] &&
  !findById("rbox-r1").className.includes("r-match"));
// home 那条 Rule 是两个条件的 AND：只把 SSID 改成此刻连着的这台，规则还不该命中。
// 这一步挡的是「前端自己凑了一个匹配出来」—— AND 是后端算的，界面无权提前替它收工。
type(byBind("rules.0.conditions.0.value"), "Office_5G");
await wait(300);
await settle();
eq("发出去的是草稿，不是磁盘上那一份",
  lastPreviewPayload().find((p) => p.id === "home").rules[0].conditions[0].value, "Office_5G");
eq("这一条条件的徽标改成 MATCH（不发任何广播）",
  findById("cbs-r1-0").textContent, strings["editor.status_match"]);
check("但另一条条件还没对上：规则卡不提前变绿",
  !findById("rbox-r1").className.includes("r-match") &&
  findById("rbs-r1").textContent === strings["editor.status_nomatch"]);
// 网关 MAC 也对上以后，AND 才成立。
type(byBind("rules.0.conditions.1.value"), "aa:bb:cc:dd:ee:ff");
await wait(300);
await settle();
check("两个条件都对上：规则卡立刻描上绿边", findById("rbox-r1").className.includes("r-match"));
eq("规则徽标改成 MATCH", findById("rbs-r1").textContent, strings["editor.status_match"]);
eq("列表那一行也跟着说命中", findById("pbadge-home").textContent, strings["editor.status_match"]);
check("但那一行的「已生效」绿描边没有被预览冒充：它只跟引擎走",
  !findById("prow-home").className.includes("live"));
eq("引擎说已生效的那一条，徽标仍是 ACTIVE、不被预览改写",
  findById("pbadge-office").textContent, strings["editor.status_active"]);
const callsBeforeTyping = previewCalls();
for (const v of ["A", "AB", "ABC", "ABCD", "ABCDE"]) type(byBind("rules.0.conditions.0.value"), v);
await wait(400);
await settle();
eq("连打五个字符只多问一次后端（每个按键一趟 IPC 的那种写法没有回来）",
  previewCalls(), callsBeforeTyping + 1);
type(byBind("rules.0.conditions.0.value"), "NotThisNetwork");
await wait(300);
await settle();
eq("改回一个对不上的名字，那一行的徽标跟着退回未命中",
  findById("pbadge-home").textContent, strings["editor.status_not_matched"]);
check("规则卡的绿边也一起退掉", !findById("rbox-r1").className.includes("r-match"));

// —————————————————————— 交给 Rust 那半边的材料 ——————————————————————

const wf = argv.indexOf("--write-fixtures");
if (wf >= 0) {
  const out = resolve(argv[wf + 1]);
  mkdirSync(dirname(out), { recursive: true });
  // 只写「用户真会点出来的」那些载荷。基线配置由 Rust 那半边自己 include_str!，
  // 这里不重复带一份 —— 两边各写一份基线，正是漂移的起点。
  const fixtures = emitted.map((s) => ({ kind: s.cmd === "save_profile" ? "profile" : "global", payload: s.payload }));
  writeFileSync(out, JSON.stringify({ fixtures }, null, 2) + "\n");
  console.log(`\n  已写出 ${fixtures.length} 条 payload → ${out}`);
}

console.log(
  `\n${"=".repeat(58)}\n检查 ${checks} 项，失败 ${failures} 项` +
    (failures ? "\n结果：编辑器与后端契约不一致，先看上面的 FAIL 行" : "\n结果：全部通过"),
);
process.exit(failures ? 1 : 0);
