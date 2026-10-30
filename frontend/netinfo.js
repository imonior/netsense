// Shared network-info rendering for the two popups:
//   - popup.html   (tray single-click menu — the source of truth)
//   - applied.html (the "profile applied" result popup)
//
// Both windows show the SAME "current network" card and the SAME per-NIC card, so the
// code lives here once. Keep this file free of window-specific logic; callers pass `t`
// (translate) and `esc` (escape) so the dictionary source stays per-window.
//
// 两个弹窗共用的网络信息渲染：托盘菜单是「当前网络」卡的源头，应用结果弹窗的上半部分
// 直接复用这里的同一份代码，而不是各写一套。本文件不含任何窗口专属逻辑，
// `t`(翻译) / `esc`(转义) 由调用方传入，保证字典来源仍各自独立。
(function () {
  "use strict";

  const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

  /// 网卡种类标签（与托盘菜单一字不差）。
  const kindLabel = (kind) => kind === "wireless" ? "popup.kind_wireless"
    : kind === "wired" ? "popup.kind_wired"
    : kind === "vpn" ? "popup.kind_vpn" : "popup.kind_other";

  /// 网卡怎么称呼自己：光有设备名不够，`en0` 后面要跟上「这是哪一口」。
  /// 系统给的设备标签（macOS 的 Hardware Port、Windows 的 InterfaceDescription）本身
  /// 就带类型信息，优先用它；它已经含设备名时不再重复一遍。隧道口在系统服务表里没有
  /// 条目（标签为空），这时退回按种类标注。
  const nicTitle = (n) => {
    const label = (n.label || "").trim();
    if (!label) return `${n.name} (${kindLabelOf(n.kind)})`;
    return label.toLowerCase().includes(n.name.toLowerCase())
      ? label : `${n.name} (${label})`;
  };
  // kindLabel 的 en 兜底在调用方 `t` 里完成；这里只回 key 给 nicTitle 拼字符串用。
  const kindLabelOf = (kind) => {
    const k = kindLabel(kind);
    const en = { "popup.kind_wireless": "Wi-Fi", "popup.kind_wired": "Wired",
      "popup.kind_vpn": "VPN", "popup.kind_other": "Other" }[k];
    return en || "Other";
  };

  /// 路由那一格：最多摆三条，剩下的写成「+N」。
  /// `0.0.0.0/0` / `::/0` 是「一切」而不是一条路由：默认往哪走已由网关行回答，
  /// 这里只剩真正带信息量的前缀要摆。
  const routeText = (list) => {
    const rs = (list || []).filter((r) => r !== "0.0.0.0/0" && r !== "::/0");
    return rs.length > 3
      ? rs.slice(0, 3).join(", ") + " +" + (rs.length - 3)
      : rs.join(", ");
  };

  /// 一张网卡的卡片：名称 + 类型标签 + 连接状态（状态在标题行右侧）+ 关键字段
  /// （取不到的字段整行不显示）。VPN 卡的标题就是**归属软件名**，明细按「接口 → 地址 →
  /// 路由」排；非 VPN 卡按「SSID → MAC → IPv4 → IPv6 → Gateway → DNS」排。
  /// 与托盘菜单 `popup.html` 的 `nicCard` 逐字同构（抽出来共用）。
  function nicCard(n, t, escFn) {
    const e = escFn || esc;
    const vpn = n.kind === "vpn";
    const title = vpn ? (n.app || n.name) : nicTitle(n);
    const tag = (() => {
      const k = kindLabel(n.kind);
      return t(k, k === "popup.kind_wireless" ? "Wi-Fi"
        : k === "popup.kind_wired" ? "Wired"
        : k === "popup.kind_vpn" ? "VPN" : "Other");
    })();
    const state = vpn
      ? (n.up ? t("popup.connected", "Connected") : t("popup.disconnected", "Not connected"))
      : "";
    const rows = [];
    if (vpn) {
      const routes = routeText(n.routes);
      if (n.up) rows.push([t("popup.vpn_egress", "Interface"), n.name]);
      if (n.ipv4) rows.push([t("popup.ip", "IPv4"), n.ipv4 + (n.netmask ? " / " + n.netmask : "")]);
      if (n.ipv6) rows.push([t("popup.ipv6", "IPv6"), n.ipv6]);
      if (routes) rows.push([t("popup.routes", "Routes"), routes]);
      if (n.dns) rows.push([t("popup.dns", "DNS"), n.dns]);
    } else {
      if (n.ssid) rows.push([t("status.ssid", "SSID"), n.ssid]);
      if (n.mac) rows.push([t("popup.mac", "MAC"), n.mac]);
      if (n.ipv4) rows.push([t("popup.ip", "IPv4"), n.ipv4 + (n.netmask ? " / " + n.netmask : "")]);
      if (n.ipv6) rows.push([t("popup.ipv6", "IPv6"), n.ipv6]);
      if (n.gateway) rows.push([t("popup.gateway", "Gateway"), n.gateway]);
      if (n.dns) rows.push([t("popup.dns", "DNS"), n.dns]);
    }
    return `<div class="nic-card">
      <div class="nic-head">
        <span class="nic-name">${e(title)}</span>
        <span class="nic-end">
          <span class="nic-tag${vpn ? " vpn" : ""}">${e(tag)}</span>
          ${state ? `<span class="nic-state${n.up ? " on" : ""}">${e(state)}</span>` : ""}
        </span>
      </div>
      ${rows.map(([k, v]) => `<div class="nic-row"><span class="k">${e(k)}</span><span class="v${k === t("popup.routes", "Routes") ? " route" : ""}">${e(v)}</span></div>`).join("")}
    </div>`;
  }

  /// 「当前网络」卡的行序（与托盘菜单 `popup.html` 的静态卡片一致）：
  /// [id, i18nKey, enDefault]。IPv6 三段（ipv6 / prefix6 / gateway6）由 `v6mode` 控制可见性。
  const ROWS = [
    ["profile",   "popup.profile",   "Applied profile"],
    ["interface", "popup.interface", "Interface"],
    ["ssid",      "status.ssid",     "SSID"],
    ["mac",       "popup.mac",       "MAC"],
    ["ip",        "popup.ip",        "IPv4"],
    ["netmask",   "popup.netmask",   "Netmask"],
    ["gateway",   "popup.gateway",   "Gateway"],
    ["ipv6",      "popup.ipv6",      "IPv6"],
    ["prefix6",   "popup.prefix6",   "IPv6 Prefix"],
    ["gateway6",  "popup.gateway6",  "Gateway (IPv6)"],
    ["dns",       "popup.dns",       "DNS"],
  ];

  /// 生成「当前网络」卡的卡片体（每行带 `id="v-<id>"`），两个窗口共用同一套模板。
  /// `profile` 行由调用方决定是否填（托盘菜单自己算生效 Profile；应用弹窗用本次应用的 Profile）。
  ///
  /// 标签直接用 `t(key, en)` 翻好再写进 HTML：**不依赖宿主页面再跑 `applyI18n`**。
  /// 托盘菜单 `popup.html` 的卡是静态标记（标签由 `applyI18n` 刷），本函数只服务
  /// `applied.html` 动态生成的卡——那扇窗没有 `applyI18n` 遍历，若这里只写英文兜底 +
  /// `data-i18n`，标签会永远停在英文（v1.8.8 那一轮的回归：结果弹窗顶部卡 11 个标签不随语言切换）。
  /// 直接翻掉最稳，两个窗口看到的卡也保一致。
  ///
  /// Labels are translated in place via `t(key, en)` rather than left as English fallback +
  /// `data-i18n`: the applied popup never runs `applyI18n`, so a `data-i18n` attribute here
  /// would stay untranslated. Translating directly keeps both windows' cards identical and
  /// language-correct.
  function cardHtml(t) {
    return `<div class="card">` + ROWS.map(([id, key, en]) =>
      `<div class="row"><span class="k">${esc(t ? t(key, en) : en)}</span><span class="v" id="v-${id}">—</span></div>`
    ).join("") + `</div>`;
  }

  /// 用状态快照 `st`（InterfaceStatus）合并主网卡 `primary`（来自 `get_interfaces`，
  /// 补 mac / ipv6 / prefix6 / gateway6）填这张卡。字段取不到留「—」
  /// （每个字段都在场，排障时才看得出「是空的」和「没读到」）。
  /// `v6mode` 是当前生效配置里的 IPv6 模式：off → IPv6 行显示设置值「Off」、前缀 / 网关行
  /// 整行收起；非 off 三行都摆，读不到的给红色「—」（配置说它该有而实测没有，值得警惕）。
  /// 与托盘菜单 `popup.html` 的填充逻辑逐字同构（抽出来共用）。
  function fill(card, ctx) {
    const { st, primary, v6mode, connected, profile, t } = ctx;
    const setRow = (id, v) => {
      const el = card.querySelector("#v-" + id);
      if (!el) return el;
      el.textContent = (v == null || v === "") ? "—" : v;
      return el;
    };
    if (profile !== undefined) setRow("profile", profile);
    const wireless = !primary || primary.kind === "wireless";
    setRow("interface", primary ? nicTitle(primary) : (st && st.iface) || "");
    let ssid = (st && st.ssid) || (primary && primary.ssid) || "";
    if (!ssid) ssid = connected ? t("status.ssid_unknown", "Unknown") : t("popup.disconnected", "Not connected");
    setRow("ssid", wireless ? ssid : "");
    setRow("mac", primary && primary.mac);
    setRow("ip", (st && st.ipv4) || (primary && primary.ipv4));
    setRow("netmask", (primary && primary.netmask) || (st && st.netmask));
    setRow("gateway", (st && st.gateway) || (primary && primary.gateway));
    const v6off = v6mode === "off";
    const v6known = v6mode === "automatic" || v6mode === "manual" || v6mode === "linklocal";
    const setRow6 = (id, v) => {
      const el = setRow(id, v);
      if (el && el.parentElement) el.parentElement.hidden = !!v6off;
      if (el) el.className = "v" + (v6known && !v ? " bad" : "");
    };
    setRow6("ipv6", v6off ? t("editor.off", "Off") : (primary && primary.ipv6));
    setRow6("prefix6", v6off ? "" : (primary && primary.prefix6));
    setRow6("gateway6", v6off ? "" : ((st && st.gateway6) || (primary && primary.gateway6)));
    setRow("dns", (st && st.dns) || (primary && primary.dns));
  }

  window.NetInfo = { ROWS, cardHtml, fill, nicCard, nicTitle, kindLabel, routeText };
})();
