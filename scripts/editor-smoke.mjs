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
  /// 只支持 editor.html 用到的那一种选择器：`[data-act]`。
  closest(sel) {
    let e = this;
    while (e) {
      if (sel === "[data-act]" && e.dataset && e.dataset.act !== undefined) return e;
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

const handlers = {};
const document = {
  body,
  addEventListener(type, fn) {
    (handlers[type] ||= []).push(fn);
  },
  getElementById: findById,
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
      return Promise.resolve({ ...strings });
    case "get_config":
      return Promise.resolve(JSON.stringify(configFixture));
    case "get_networks":
      return Promise.resolve(["Office_5G", "Café"]);
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

const window = {
  __TAURI__: {
    core: { invoke: fakeInvoke },
    event: { listen: async () => () => {} },
  },
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
  " get sel() { return sel }, get branch() { return branch }, set view(v) { view = v } };";
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
  "启动时按顺序读了后端：文案 / 配置 / 已存网络 / 状态",
  invokeLog.map((c) => c.cmd).slice(0, 4),
  ["get_strings", "get_config", "get_networks", "get_status"],
);
check("侧栏渲染出 Profile 列表", findAll((e) => e.dataset?.act === "sel-profile").length === 3);
eq("按钮文案取自后端字典（不是 key 本身）", findById("btn-apply").textContent, strings["editor.force_apply"]);

group("选中与表单回填");
// 走真实的事件委托：点侧栏那一行，而不是直接调 pick()
const officeRow = findAll((e) => e.dataset?.act === "sel-profile" && e.dataset?.id === "office")[0];
check("侧栏里有 office 这一行", !!officeRow);
click(officeRow);
eq("点击列表项后选中态切到 office", h.sel, { kind: "profile", id: "office" });
eq("名称框回填自配置", byBind("name")?.value, "Office_5G");
eq("默认页签是 THEN", h.branch, "then");
check(
  "3A 的模式下拉按当前值选中",
  byBind("then.network.mode")?.children.filter((c) => c.selected).map((c) => c.value).join(",") === "manual",
);

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
choose(byAct("dns-tri"), "keep");
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
check("选「不改」= 整个 dns 字段消失", p && !("dns" in p.then.network), JSON.stringify(p?.then?.network));
resetSaves();
choose(byAct("dns-tri"), "auto");
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("选「交回系统」= 显式写 dns: \"\"", p?.then?.network?.dns, "");
resetSaves();
choose(byAct("dns-tri"), "set");
type(byBind("then.network.dns"), "192.168.1.1, 8.8.8.8");
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("选「指定」= 按输入框的值下发", p?.then?.network?.dns, "192.168.1.1, 8.8.8.8");

group("THEN / ELSE 两分支互不串台");
click(findAll((e) => e.dataset?.act === "branch" && e.dataset?.b === "else")[0]);
eq("切到 ELSE", h.branch, "else");
check("ELSE 页没有把 THEN 的动作列出来", byBind("else.one_shot.0.action.app") === null);
resetSaves();
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("ELSE 分支保留自己的 DHCP + 删除路由", p?.else?.network?.routes, [{ dest: "10.0.0.0/8", metric: 0, delete: true }]);
check("THEN 的动作仍在原处（切页签不会丢数据）", JSON.stringify(p?.then?.one_shot?.map((a) => a.id)) === JSON.stringify(["a1", "a2", "a3"]));

group("3B 动作载荷");
const acts = p?.then?.one_shot || [];
eq("动作类型原样送达", acts.map((a) => a.action.type), ["launch_app", "run_script", "launch_app"]);
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
eq("未声明 args 时不写出空数组", (p?.then?.one_shot || []).every((a) => !("args" in a.action)), true);
eq("disabled 的动作照样保存，只是标成禁用", p?.then?.one_shot?.[2]?.enabled, false);
eq("priority 原样保留（分批依据）", (p?.then?.one_shot || []).map((a) => a.priority), [1, 1, 2]);
click(findAll((e) => e.dataset?.act === "branch" && e.dataset?.b === "then")[0]);
resetSaves();
toggle(byBind("then.one_shot.1.action.elevated"), true);
await h.$("btn-save").onclick();
p = saveOf("save_profile")?.payload;
eq("提权标记只贴在 run_script 上", p?.then?.one_shot?.[1]?.action, { type: "run_script", path: "scripts/office-vpn.sh", elevated: true });
check("其他动作没有被连带标上提权", p?.then?.one_shot?.[0]?.action?.elevated === undefined);

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
click(findAll((e) => e.dataset?.act === "branch" && e.dataset?.b === "else")[0]);
check("ELSE 页签没有常驻卡片（worker 只随 THEN 起，列出来是假承诺）",
  findAll((e) => e.id.startsWith("wchip-")).length === 0);
click(findAll((e) => e.dataset?.act === "branch" && e.dataset?.b === "then")[0]);

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

group("全局项：白名单与兜底");
click(byAct("sel-global"));
eq("切到全局白名单", h.sel, { kind: "global", id: undefined });
resetSaves();
const box = findById("scripts-box");
box.value = "  /opt/ops/a.sh \n\n/opt/ops/b.sh  ";
document.dispatch("input", box);
await h.$("btn-save").onclick();
let g = saveOf("save_global")?.payload;
eq("每行一条，去空白并丢弃空行", g?.allowed_scripts, ["/opt/ops/a.sh", "/opt/ops/b.sh"]);
check("save_global 不会顺手写 profiles", g && !("profiles" in g));
click(byAct("sel-fallback"));
resetSaves();
await h.$("btn-save").onclick();
g = saveOf("save_global")?.payload;
eq("兜底网络按 cleanNetwork 归一", g?.fallback?.network?.mode, "dhcp");
check("兜底不是 Profile：payload 里没有 rules / detection", g && !("rules" in (g.fallback || {})));

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
