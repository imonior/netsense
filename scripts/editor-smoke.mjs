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
// 只实现 editor.html 真正碰到的那部分（它全程用 innerHTML 字符串建表单，从不
// createElement / querySelector）。`innerHTML` 的 getter 故意返回空串：断言一律走
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
    for (const fn of handlers[type] || []) fn({ type, target, preventDefault() {} });
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

const invokeLog = [];
let saveSeq = 0;
const saves = [];
/** `saves` 会被各段断言清空以便「看这一次保存发了什么」，交付给 Rust 的那份要一直攒着。 */
const emitted = [];
function fakeInvoke(cmd, args) {
  invokeLog.push({ cmd, args });
  switch (cmd) {
    case "get_strings":
      return Promise.resolve({ ...(langCode === "zh" ? zhStrings : strings) });
    case "get_language":
      return Promise.resolve(langCode);
    case "get_config":
      return Promise.resolve(JSON.stringify(configFixture));
    case "get_networks":
      return Promise.resolve(["Office_5G", "Café"]);
    case "get_interfaces":
      // 条件里「接口」的候选。后端只报「在用且不是内部设备」的网卡，并按 无线→有线→VPN 排好序，
      // 所以这里给出的就是编辑器应当照单全收的那一份。
      return Promise.resolve(JSON.stringify([
        { name: "en0", kind: "wireless" },
        { name: "en5", kind: "wired" },
        { name: "utun3", kind: "vpn" },
      ]));
    case "get_printers":
      // 「设为默认打印机」的候选。这里返回 JSON 字符串，是为了让 asArray 那条兼容分支
      // 一直有人走：后端改形状时这边会立刻红。
      return Promise.resolve(JSON.stringify([
        { name: "Office LaserJet", is_default: true },
        { name: "Home Inkjet", is_default: false },
      ]));

    case "get_status":
      return Promise.resolve(
        JSON.stringify({
          status: { connected: true, ssid: "Office_5G", ipv4: "192.168.1.100/24", dns: "192.168.1.1", rssi: -50 },
          engine: viewFixture,
          profiles: configFixture.profiles.map((p) => ({ id: p.id, name: p.name, enabled: p.enabled })),
          language: "en",
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
  " applyPlan, planList, planPersistent, profileView, get cfg() { return cfg }, get draft() { return draft }," +
  " get sel() { return sel }, get view() { return view }, set view(v) { view = v }," +
  " get wifiList() { return wifiList }, get nicList() { return nicList }," +
  " get printerList() { return printerList }," +
  " get langShown() { return langShown }, get dirty() { return dirty }," +
  " get globalDraft() { return globalDraft } };";
vm.runInContext(code, ctx, { filename: "frontend/editor.html" });
const h = ctx.__h;

const settle = async (n = 6) => {
  for (let i = 0; i < n; i += 1) await new Promise((r) => setImmediate(r));
};
const lastSave = () => saves[saves.length - 1];
const saveOf = (cmd) => saves.filter((s) => s.cmd === cmd).pop();
const resetSaves = () => {
  saves.length = 0;
};

// —————————————————————— 断言 ——————————————————————

group("装载");
await settle();
check("启动脚本无异常地跑完（顶层 IIFE 已读到配置）", h.cfg.profiles.length === 3);
eq(
  "启动时按顺序读了后端：文案 / 配置 / 已存网络 / 硬件网卡 / 系统打印机 / 状态",
  invokeLog.map((c) => c.cmd).slice(0, 6),
  ["get_strings", "get_config", "get_networks", "get_interfaces", "get_printers", "get_status"],
);
eq("已存 SSID、网卡与打印机快照各取一份（条件值与动作目标的候选）",
  [h.wifiList, h.nicList.map((n) => n.name), h.printerList.map((p) => p.name)],
  [["Office_5G", "Café"], ["en0", "en5", "utun3"], ["Office LaserJet", "Home Inkjet"]]);
check("第 1 列渲染出 Profile 列表", findAll((e) => e.dataset?.act === "sel-profile").length === 3);
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
check("VPN 隧道单独成行，不和硬件出口混在一起", findById("st-net").textContent.includes("utun3"));

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
eq("同一行里：启用勾选 + 类型下拉 + 值下拉 + 徽标 + 删除按钮",
  [crow0.filter((e) => e.tagName === "INPUT").length,
   crow0.filter((e) => e.tagName === "SELECT").length,
   crow0.filter((e) => e.className?.includes("badge")).length,
   crow0.filter((e) => e.dataset?.act === "del-cond").length],
  [1, 2, 1, 1]);
const ssidInput = byBind("rules.0.conditions.0.value");
eq("SSID 值是下拉选择框", ssidInput?.tagName, "SELECT");
eq("候选来自系统已保存的 SSID，外加手动输入项", ssidInput?.children.map((o) => o.value), ["", "Office_5G", "Café", "__manual__"]);
const ifaceSelect = byBind("rules.0.conditions.2.value");
eq("接口值只能是选出来的", ifaceSelect?.tagName, "SELECT");
eq("下拉里只有非 VPN 的硬件出口，外加一个空选项",
  ifaceSelect?.children.map((o) => o.value), ["", "en0", "en5"]);
// 配置里写着这台机器现在没有的网卡是合法的（换了笔记本、拔了扩展坞）。
h.draft.rules[0].conditions[2].value = "en9";
h.renderAll();
const gone = byBind("rules.0.conditions.2.value");
eq("已存但本机已经没有的网卡：原样列出并选中，不会被悄悄改成别的卡",
  [gone?.children.map((o) => o.value), gone?.children.filter((o) => o.selected).map((o) => o.value)],
  [["", "en9", "en0", "en5"], ["en9"]]);
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
eq("priority 原样保留（分批依据）", (p?.then?.one_shot || []).map((a) => a.priority), [1, 1, 2, 2]);
resetSaves();
toggle(byBind("then.one_shot.1.action.elevated"), true);
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("提权标记只贴在 run_script 上", p?.then?.one_shot?.[1]?.action, { type: "run_script", path: "scripts/office-vpn.sh", elevated: true });
check("其他动作没有被连带标上提权", p?.then?.one_shot?.[0]?.action?.elevated === undefined);

group("3B 动作：默认打印机的候选来自系统，不是让用户手抄名字");
const prInput = byBind("then.one_shot.3.action.printer");
eq("打印机那一项是下拉选择框", prInput?.tagName, "SELECT");
eq("候选就是系统里的打印机名（顺序照后端），外加手动输入项",
  prInput?.children.map((o) => o.value), ["", "Office LaserJet", "Home Inkjet", "__manual__"]);
eq("当前默认的那台在候选里带说明，其余留空",
  findById("printer-list")?.children.map((o) => o.textContent), [strings["editor.printer_is_default"], ""]);
check("整列只挂一份候选清单：THEN 与 ELSE 共用同一个 id，重复的那份会被浏览器忽略",
  findAll((e) => e.id === "printer-list").length === 1);
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
eq("选择的打印机原样送达",
  [p?.then?.one_shot?.[4]?.action, p?.then?.one_shot?.[4]?.priority, "args" in (p?.then?.one_shot?.[4]?.action || {})],
  [{ type: "set_default_printer", printer: "Home Inkjet" }, 100, false]);
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
  (p?.then?.persistent || []).map((a) => [a.id, a.enabled, a.priority, a.action.type, a.action.tunnel, a.action.interval_secs]),
  [["p1", true, 1, "keep_wireguard_connected", "wg0", 15]],
);

const activeOffice = (workers) =>
  engineView(
    [{ id: "office", name: "Office_5G", enabled: true, matched: true, status: "active", rules: [] }],
    { state: "active", id: "office" },
    null,
    workers,
  );
const status = (over) => ({
  id: "p1", label: "wireguard:wg0", priority: 1, state: "repaired",
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

h.view = activeOffice([{ id: "p1", label: "wireguard:wg0", priority: 1, state: "satisfied", interval: 15, repairs: 0, at: 100 }]);
h.refreshLive();
eq("satisfied 与 repaired 用词不同：一个什么都没做，一个修过一次",
  findById("wchip-then.persistent.0")?.textContent, strings["editor.worker_satisfied"]);

h.view = activeOffice([{ id: "p1", label: "wireguard:wg0", priority: 1, state: "pending", interval: 15, repairs: 0, at: 100 }]);
h.refreshLive();
eq("pending 由后端发（worker 起了，第一次核对还没回）",
  findById("wchip-then.persistent.0")?.textContent, strings["editor.worker_pending"]);
check("未知的状态取值不会串成一条假的成功徽标", (() => {
  h.view = activeOffice([{ id: "p1", label: "x", priority: 1, state: "some_future_state", interval: 15, at: 1 }]);
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
h.draft.else.persistent = [{ id: "pX", enabled: true, priority: 1, action: { type: "periodic_script", path: "keepalive.sh", interval_secs: 30 } }];
h.renderAll();
h.refreshLive();
check("ELSE 那一块也有自己的卡片（表单是完整的）", !!findById("wchip-else.persistent.0"));
eq("但 worker 徽标只属于 Active 的 THEN：ELSE 的卡片一律留空",
  findById("wchip-else.persistent.0")?.textContent, "");
eq("同一时刻 THEN 的徽标是亮的", findById("wchip-then.persistent.0")?.textContent, strings["editor.worker_repaired"]);
h.pick("profile", "office");   // 丢掉上面那条临时草稿

const withPersistent = (list) => ({ which: "then", branch: { persistent: list } });
const held = h.planPersistent(withPersistent([
  { id: "p1", enabled: true, priority: 1, action: { type: "keep_wireguard_connected", tunnel: "wg0", interval_secs: 15 } },
  { id: "p2", enabled: true, priority: 1, action: { type: "periodic_script", path: "/opt/ops/keepalive.sh", interval_secs: 60 } },
]));
check("执行清单里的常驻段说得出每条查什么、多久一次",
  held.includes("wg0") && held.includes("15") && held.includes("keepalive.sh") && held.includes("60"));
check("禁用的常驻动作不进清单（点了也不会起 worker）", !h.planPersistent(withPersistent([
  { id: "p9", enabled: false, priority: 1, action: { type: "keep_wireguard_connected", tunnel: "wg9", interval_secs: 15 } },
])).includes("wg9"));
check("走 ELSE 时清单改口：这一支一条都不会起",
  h.planPersistent({ which: "else", branch: { persistent: [{ id: "p1", enabled: true, priority: 1, action: { type: "periodic_script", path: "/opt/ops/k.sh", interval_secs: 30 } }] } })
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
const batch = (n) => `${strings["editor.priority"]} ${n}`;
const count = (hay, needle) => hay.split(needle).length - 1;
const ordered = h.planList(
  [
    { id: "b", priority: 2, action: { type: "launch_app", app: "Second" } },
    { id: "a", priority: 1, action: { type: "launch_app", app: "First" } },
    { id: "c", priority: 1, action: { type: "launch_app", app: "AlsoFirst" } },
  ],
  null,
);
check("数值小的批次排在前面", ordered.indexOf(batch(1)) < ordered.indexOf(batch(2)));
check("同一个 priority 归成一批（并发的那一批）", count(ordered, batch(1)) === 1 && count(ordered, batch(2)) === 1);
check("禁用动作不进清单（点了也不会跑）", !listHtml.includes("Test.app"));
const elevatedLine = h.planList([{ id: "x", priority: 1, action: { type: "run_script", path: "p.sh", elevated: true } }], null);
check("提权动作在清单上明说要授权", elevatedLine.includes(strings["editor.action_elevated"]));
check("没提权的动作不会被顺手标上提权", !h.planList([{ id: "y", priority: 1, action: { type: "launch_app", app: "Foo" } }], null).includes(strings["editor.action_elevated"]));
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
