// Real-render regression for the "profile applied" popup (applied.html).
//
// Why a real browser, not the jsdom sandbox: the popup is event-driven
// (`netsense://applied` / `netsense://status`); a hand-built DOM can only check
// CSS, never the live render path. This test loads the actual file in system
// Chrome at the popup's real viewport and asserts the current-network card shows
// the live MAC / netmask instead of the "—" placeholder.
//
// 为什么用真浏览器而不是 jsdom 沙箱：弹窗是事件驱动的（`netsense://applied` /
// `netsense://status`），手工拼 DOM 只能验证 CSS、验不了真实渲染路径。
// 这里在真实视口下用系统 Chrome 打开真实文件，断言当前网络卡显示真实 MAC / 子网掩码，
// 而不是「—」占位。
//
// Runs against `playwright-core` (already available from a prior session at
// /tmp/appliedtest/node_modules). Resolve it via NODE_PATH, with a direct-path
// fallback so the script survives even if NODE_PATH is unset.
// 依赖 `playwright-core`（上一轮已装在 /tmp/appliedtest/node_modules）。通过 NODE_PATH
// 解析，并带直接路径兜底，即使没设 NODE_PATH 也能跑。
//
//   NODE_PATH=/tmp/appliedtest/node_modules node scripts/applied-render.mjs

import { createRequire } from "module";
const require = createRequire(import.meta.url);
let chromium;
try {
  chromium = require("playwright-core").chromium;
} catch {
  chromium = require("/tmp/appliedtest/node_modules/playwright-core").chromium;
}

import { pathToFileURL } from "url";

const FILE = "/Users/imonior/Documents/GitHub/netsense/frontend/applied.html";
const url = pathToFileURL(FILE).href;

// Tauri bridge stub. CRITICAL: get_status / get_interfaces return JSON **strings**,
// exactly like the real backend. The v1.9.0 regression in applied.html was that
// refreshPrimary() did `Array.isArray(raw)` on a string (always false) → netPrimary
// stayed null → MAC + netmask rendered as "—". Returning a string here means this
// test would FAIL against the pre-fix code, so a green run genuinely proves the
// JSON.parse fix.
// Tauri 桥桩。关键：get_status / get_interfaces 返回 JSON **字符串**，与后端真实形态一致。
// v1.9.0 的回归是 refreshPrimary() 对字符串直接 Array.isArray（恒 false）→ netPrimary 恒
// 为 null → MAC + 子网掩码显示「—」。这里返回字符串，意味着本测试在修复前的代码上会失败，
// 所以跑绿才是真证明 JSON.parse 修好了。
const STUB = `
window.__NS_LISTENERS__ = {};
const STRINGS = {
  'popup.applied_title': 'Network configuration is in effect',
  'popup.applied_failed_title': 'Network configuration did not take effect',
  'popup.applied_failed_rb': 'Network changes were rolled back',
  'popup.group_actions': 'Actions',
  'popup.actions_running': 'Running',
  'popup.actions_pending': 'Pending',
  'popup.actions_repairs': 'Restored {n} time(s)',
  'popup.applied_nochange': 'No network parameters were changed',
  'popup.time': 'Time',
  'popup.close': 'Close',
  'status.ssid': 'SSID',
  'popup.mac': 'MAC',
  'popup.ip': 'IPv4',
  'popup.netmask': 'Netmask',
  'popup.gateway': 'Gateway',
  'popup.dns': 'DNS',
};
window.__TAURI__ = {
  core: {
    invoke: async (cmd) => {
      if (cmd === 'get_strings') return STRINGS;
      if (cmd === 'get_status') return JSON.stringify({
        status: {
          ssid: 'Home-5G', iface: 'en0', ipv4: '192.168.1.50',
          netmask: '255.255.255.0', gateway: '192.168.1.1',
          dns: '223.5.5.5', v6mode: 'automatic', connected: true,
        },
        v6mode: 'automatic',
      });
      if (cmd === 'get_interfaces') return JSON.stringify([
        {
          kind: 'wireless', name: 'en0', label: 'Wi-Fi', ssid: 'Home-5G',
          mac: '76:94:be:d9:22:63', ipv4: '192.168.1.50', netmask: '255.255.255.0',
          ipv6: 'fe80::1a2b:3c4d:5e6f', prefix6: 'fe80::/64', gateway6: 'fe80::1',
          dns: '223.5.5.5',
        },
        { kind: 'vpn', name: 'utun3', label: 'Tailscale', ipv4: '100.64.0.5' },
      ]);
      if (cmd === 'get_last_applied') return JSON.stringify({ success: true, groups: [] });
      return null;
    },
  },
  event: {
    listen: (ev, fn) => { (window.__NS_LISTENERS__[ev] = window.__NS_LISTENERS__[ev] || []).push(fn); },
  },
};
window.__dispatch = (ev, payload) => { (window.__NS_LISTENERS__[ev] || []).forEach((fn) => fn({ payload })); };
`;

const EXPECT_MAC = "76:94:be:d9:22:63";
const EXPECT_NETMASK = "255.255.255.0";

const browser = await chromium.launch({ channel: "chrome", headless: true });
const page = await browser.newPage({ viewport: { width: 360, height: 620 } });
const errors = [];
page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
page.on("pageerror", (e) => errors.push("pageerror: " + e.message));

await page.addInitScript(STUB);
await page.goto(url, { waitUntil: "load" });
// init runs async (get_status → refreshPrimary → get_last_applied → renderApplied).
// Wait until the MAC cell is filled with the real value.
await page.waitForFunction(
  (exp) => {
    const el = document.getElementById("v-mac");
    return el && el.textContent && el.textContent !== "—" && el.textContent === exp;
  },
  EXPECT_MAC,
  { timeout: 5000 },
);

const mac = (await page.textContent("#v-mac"))?.trim();
const netmask = (await page.textContent("#v-netmask"))?.trim();
const bodyLen = (await page.textContent("#ap-body"))?.trim().length || 0;

await page.screenshot({ path: "/tmp/appliedtest/applied-render.png" });

await browser.close();

const fail = [];
if (mac !== EXPECT_MAC) fail.push(`MAC: expected "${EXPECT_MAC}", got "${mac}"`);
if (netmask !== EXPECT_NETMASK) fail.push(`netmask: expected "${EXPECT_NETMASK}", got "${netmask}"`);
if (errors.length) fail.push(`console errors: ${JSON.stringify(errors)}`);
if (bodyLen < 20) fail.push(`ap-body suspiciously short (${bodyLen})`);

if (fail.length) {
  console.error("FAILED:");
  for (const f of fail) console.error("  - " + f);
  process.exit(1);
}
console.log(`OK: MAC=${mac} netmask=${netmask} ap-body=${bodyLen} chars, console errors=${errors.length}`);
