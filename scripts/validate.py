#!/usr/bin/env python3
"""NetSense 静态校验（不构建本项目，可在任何平台跑；CI 里也用它守基线）。

检查项：
  1. 所有 JSON 文件合法（tauri.conf.json / config.example.json / i18n 字典）
  2. 5 语 i18n key 完全对齐（无 missing / extra / empty）
  3. 同一 key 在各语言的占位符集合一致（防止 {name} 被漏译）
  4. i18n key 双向对齐：代码（含前端）引用的 key 必须存在，存在的 key 必须被引用
  5. 平台抽象层没有被上层越权绕过（平台层之外的模块里不出现具体平台类型名）
  6. 各平台模块都实现了 NetworkPlatform 的全部方法（按方法名计数兜底检查）；另外把三份
     平台实现单独过一遍 rustc 的词法/语法解析 —— 一次 `cargo test` 只编译宿主那一份，
     另两份写坏了要等 CI 的对应构建腿才暴露（本机装了 rustc 就查，没装则跳过并说明）
  7. tauri.conf.json 的字段是否真实存在（详见 check_tauri_fields 的说明）
  8. 版本号一致性：VERSION / tauri.conf.json.version / Cargo.toml [package].version /
     CHANGELOG 顶部小节版本必须完全相同（顶部为 [Unreleased] 时看下一条带版本号的小节，
     一条都没有则跳过该处；否则打 tag 前易漏改某处）
  9. 文档 ↔ 代码来源：DEVELOPMENT.md §3 的仓库结构树与磁盘双向对齐、文档里引用的仓库路径
     确实存在、frontend/README.md 的命令表与 main.rs 的注册表一致、事件名双向对齐
  10. 文档之间：文档写出的数量（key 数 / 命令数 / 语言数 / 检查项数）等于真实来源的数量；
     README 与 CHANGELOG 的 5 语版本小节层级同构，且英文原文里的技术记号一个都没丢

输出：每组一行「分数 [n] 名称: 通过项/检查项 = x/10」，最后一行总分（每组 10 分，共 100）。
新增检查组请同时更新 GROUPS —— 组数对不上，总分就不是百分制，脚本会自己报。

用法：python scripts/validate.py
"""
from __future__ import annotations

import glob
import json
import os
import re
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "src-tauri", "src")

failures: list[str] = []
# 评分：每组 10 分、总分 100。组内每条 ok()/bad() 是等重的一项，
# 组分 = 10 × 通过项 / 检查项；总分取各组之和。计数用「累计 + 上次结算位置」，
# 新增检查组只要在末尾喊一次 score()，不需要另维护一张登记表。
_passed = 0
_failed = 0
_groups = 0
# 检查组数：每组 10 分 ⇒ 总分 100。新增检查组要同时改这里，并同步 DEVELOPMENT.md §12 的
# 「N checks」—— 那行由本脚本自己检查（第 [10] 项），写漏了会直接报出来。
GROUPS = 10
_mark: tuple[int, int] = (0, 0)
_label = ""
_group_scores: list[tuple[str, float]] = []


def group(label: str) -> None:
    global _label, _groups
    _label = label
    _groups += 1
    print(label)


def ok(msg: str) -> None:
    global _passed
    _passed += 1
    print(f"  OK   {msg}")


def bad(msg: str) -> None:
    global _failed
    _failed += 1
    failures.append(msg)
    print(f"  FAIL {msg}")


def score() -> None:
    """结算上一组：组内通过项 / 检查项，折成 10 分制。"""
    global _mark
    passed = _passed - _mark[0]
    total = (_passed + _failed) - _mark[1]
    _mark = (_passed, _passed + _failed)
    if not total:
        print(f"  --   分数 {_label}: 本次没有可计分项")
        return
    pts = 10.0 * passed / total
    _group_scores.append((_label, pts))
    print(f"  {'OK  ' if pts == 10.0 else 'FAIL'} 分数 {_label}: {passed}/{total} = {pts:.1f}/10")


def check_json() -> dict[str, dict]:
    group("[1] JSON 合法性")
    targets = [
        os.path.join(ROOT, "src-tauri", "tauri.conf.json"),
        os.path.join(ROOT, "config.example.json"),
    ] + sorted(glob.glob(os.path.join(SRC, "i18n", "*.json")))
    dicts: dict[str, dict] = {}
    for t in targets:
        rel = os.path.relpath(t, ROOT)
        try:
            with open(t, encoding="utf-8") as fh:
                data = json.load(fh)
            ok(rel)
            if "i18n" in t:
                dicts[os.path.basename(t)] = data
        except Exception as exc:  # noqa: BLE001
            bad(f"{rel}: {exc}")
    return dicts


def check_i18n(dicts: dict[str, dict]) -> None:
    group("[2] i18n key 对齐")
    if "en.json" not in dicts:
        bad("缺少基准字典 en.json")
        return
    base = dicts["en.json"]
    print(f"  基准 en.json 共 {len(base)} 个 key")
    for name, d in sorted(dicts.items()):
        miss = sorted(set(base) - set(d))
        extra = sorted(set(d) - set(base))
        empty = sorted(k for k, v in d.items() if not str(v).strip())
        if miss or extra or empty:
            bad(f"{name}: missing={miss} extra={extra} empty={empty}")
        else:
            ok(f"{name} ({len(d)} keys)")

    score()
    group("[3] 占位符一致性")
    pat = re.compile(r"\{(\w+)\}")
    mismatched = []
    for key in base:
        sets = {frozenset(pat.findall(str(d.get(key, "")))) for d in dicts.values()}
        if len(sets) > 1:
            mismatched.append(key)
    if mismatched:
        bad(f"占位符不一致: {mismatched}")
    else:
        ok("各语言占位符集合一致")


def check_key_usage(dicts: dict[str, dict]) -> None:
    """i18n key 双向对齐：引用的必须存在，存在的必须被引用。

    为什么两个方向都要查：
    - 只查「引用→存在」时，打错的 key 在前端会静默走 `t(key, fallback)`，界面上看不出异常；
    - 不查「存在→引用」时，字典会不断累积上一代界面留下的死 key。死 key 不只是冗余：
      它们和新 key 混在一起，改名时很容易挑错一个，等于给翻译白白留了维护债。

    扫描范围含前端。key 的识别取「字面量 + 首段命中已知命名空间」这条规则：
    前端的数据绑定路径也是带点的字符串（`"then.mode"`），但它的首段不是命名空间，
    于是被自然排除，不需要维护一张例外表。反引号里的带点串一律不算引用 —— 前端用它
    拼绑定路径，Rust 的文档注释用它标标识符（`app.exit` 就被这么误判过）。
    """
    group("[4] i18n key 双向引用检查")
    if "en.json" not in dicts:
        return
    known = set(dicts["en.json"])
    namespaces = {k.split(".")[0] for k in known}
    cited: set[str] = set()

    def keep(tok: str) -> None:
        if tok.split(".")[0] in namespaces:
            cited.add(tok)

    paths = (sorted(glob.glob(os.path.join(ROOT, "frontend", "*.html")))
             + sorted(glob.glob(os.path.join(ROOT, "frontend", "*.js")))
             + sorted(glob.glob(os.path.join(SRC, "**", "*.rs"), recursive=True)))
    for path in paths:
        text = open(path, encoding="utf-8").read()
        for tok in re.findall(r"""["']([a-z][a-z0-9_]*(?:\.[a-z0-9_]+)+)["']""", text):
            keep(tok)
        for tok in re.findall(r"""i18n::t(?:f)?\(\s*"([^"]+)\"""", text):
            keep(tok)

    missing = sorted(cited - known)
    if missing:
        bad(f"代码引用了不存在的 key: {missing}")
    else:
        ok(f"引用 {len(cited)} 个 key，全部存在")

    dead = sorted(known - cited)
    if dead:
        bad(f"{len(dead)} 个 key 没有任何引用（改了界面没清理的文案）: {dead}")
    else:
        ok(f"{len(known)} 个 key 全部被引用")


def check_pal_boundary() -> None:
    """除平台层本身，其余模块都只能通过 `Platform` + trait 说话。

    扫描范围是「全部 .rs 减去平台层」而不是一个手写白名单：新增上层模块时不需要
    记得来这里登记，漏检的风险留在平台层那三个文件里（它们本来就必须认识具体类型）。
    """
    group("[5] 平台抽象边界（上层不得出现具体平台类型）")
    forbidden = re.compile(r"\b(MacPlatform|WindowsPlatform|LinuxPlatform)\b")
    offenders = []
    for path in sorted(glob.glob(os.path.join(SRC, "**", "*.rs"), recursive=True)):
        rel = os.path.relpath(path, SRC).replace(os.sep, "/")
        if rel == "platform.rs" or rel.startswith("platform/"):
            continue
        if forbidden.search(open(path, encoding="utf-8").read()):
            offenders.append(rel)
    if offenders:
        bad(f"这些文件泄漏了具体平台类型: {offenders}")
    else:
        ok("平台层之外的模块只依赖 Platform 与 trait")


def check_trait_impls() -> None:
    group("[6] 各平台实现是否覆盖 trait 全部方法")
    trait_file = os.path.join(SRC, "platform.rs")
    text = open(trait_file, encoding="utf-8").read()
    m = re.search(r"pub trait NetworkPlatform[^{]*\{(.*?)\n\}", text, re.S)
    if not m:
        bad("未能在 platform.rs 中定位 NetworkPlatform trait")
        return
    methods = re.findall(r"^\s*fn\s+(\w+)\s*\(", m.group(1), re.M)
    print(f"  trait 方法 {len(methods)} 个: {', '.join(methods)}")
    for plat in ("macos", "windows", "linux"):
        p = os.path.join(SRC, "platform", f"{plat}.rs")
        if not os.path.exists(p):
            bad(f"缺少平台实现 platform/{plat}.rs")
            continue
        body = open(p, encoding="utf-8").read()
        impl = re.search(r"impl NetworkPlatform for \w+\s*\{(.*)\n\}", body, re.S)
        if not impl:
            bad(f"platform/{plat}.rs 没有 impl NetworkPlatform")
            continue
        impl_body = impl.group(1)
        missing = [mth for mth in methods if not re.search(rf"\bfn\s+{re.escape(mth)}\s*\(", impl_body)]
        if missing:
            bad(f"platform/{plat}.rs 未实现: {missing}")
        else:
            ok(f"platform/{plat}.rs 覆盖全部 {len(methods)} 个方法")
    parse_platform_files()


# 单独喂给 rustc 一个 .rs 文件时必然冒出来的那类报错 —— 都是「找不到兄弟模块」的
# 名字解析问题，不是语法问题。语法/词法报错没有 [Exxxx] 编号，所以按编号挡掉解析类。
_RUST_NAME_RESOLUTION = re.compile(
    r"aborting due to|could not compile|couldn't create a temp dir|cannot find|"
    r"not defined at compile time|unresolved (item|path)|\bE\d{4}\b"
)


def parse_platform_files() -> None:
    """把三份平台实现单独过一遍 rustc 的解析。

    宿主上 `cargo test` 只编译 `#[cfg(target_os = ...)]` 选中的那一份，另外两份里一个
    多出来的引号就足够让对应的 CI 构建腿跑完依赖才失败。
    """
    if not shutil.which("rustc"):
        print("  未找到 rustc，跳过平台实现的语法解析（其余断言照常）")
        return
    for plat in ("macos", "windows", "linux"):
        p = os.path.join(SRC, "platform", f"{plat}.rs")
        if not os.path.exists(p):
            continue
        r = subprocess.run(
            ["rustc", "--edition", "2021", "--crate-type", "lib", "--emit", "metadata",
             "-o", os.devnull, p],
            capture_output=True, text=True, check=False,
        )
        syntax = [ln for ln in r.stderr.splitlines()
                  if ln.startswith("error: ") and not _RUST_NAME_RESOLUTION.search(ln)]
        if syntax:
            bad(f"platform/{plat}.rs 连解析都过不去: {syntax[0]}")
        else:
            ok(f"platform/{plat}.rs 单独解析通过（本机不会编译到它）")


def _field_names(*camel_case: str) -> set[str]:
    """Tauri 配置文件同时接受 camelCase 与其 kebab-case 别名，两种都要放行。

    只为「纯 camelCase」（多个首字母小写的单词拼接）生成别名；像 `macOS` / `iOS`
    这种含连续大写的名字并没有 kebab 形式，不能按规则拆开。
    """
    out: set[str] = set()
    pure_camel = re.compile(r"^[a-z]+([A-Z][a-z0-9]*)+$")
    for name in camel_case:
        out.add(name)
        if pure_camel.match(name):
            out.add(re.sub(r"(?<!^)(?=[A-Z])", "-", name).lower())
    return out


# Tauri v2 `bundle` 配置树的合法字段名。
# 来源：https://schema.tauri.app/config/2 的 definitions.BundleConfig 子树（离线快照）。
# 升级 Tauri 大版本时请重新抓取该 schema 并同步这三组常量。
_BUNDLE_FIELDS = _field_names(
    "active", "targets", "icon", "publisher", "copyright", "license", "licenseFile",
    "homepage", "category", "shortDescription", "longDescription", "resources",
    "externalBin", "fileAssociations", "useLocalToolsDir", "createUpdaterArtifacts",
    "linux", "macOS", "windows", "iOS", "android",
)
_WINDOWS_FIELDS = _field_names(
    "certificateThumbprint", "digestAlgorithm", "timestampUrl", "tsp",
    "webviewInstallMode", "minimumWebview2Version", "allowDowngrades",
    "signCommand", "nsis", "wix",
)
# ▼ 注意：installMode 只属于 nsis，WiX 不支持（WiX 本来就按机器安装到 Program Files）。
#   曾经误把它写进 wix，导致四个平台的构建全部在 build script 阶段失败。
_NSIS_FIELDS = _field_names(
    "template", "languages", "customLanguageFiles", "displayLanguageSelector",
    "installMode", "installerIcon", "uninstallerIcon", "installerHooks",
    "headerImage", "uninstallerHeaderImage", "sidebarImage", "startMenuFolder",
    "compression", "minimumWebview2Version",
)
_WIX_FIELDS = _field_names(
    "version", "upgradeCode", "language", "template", "fragmentPaths",
    "componentGroupRefs", "componentRefs", "featureGroupRefs", "featureRefs",
    "mergeRefs", "bannerPath", "dialogImagePath", "enableElevatedUpdateTask",
    "fipsCompliant",
)


def _report_unknown(scope: str, node: dict, allowed: set[str]) -> None:
    if not isinstance(node, dict):
        if node is not None:
            bad(f"tauri.conf.json: {scope} 应为对象，实际为 {type(node).__name__}")
        return
    unknown = sorted(set(node) - allowed)
    if unknown:
        for u in unknown:
            near = sorted(a for a in allowed if u.lower().replace("-", "") in a.lower().replace("-", ""))
            hint = f"，是否想写 {near[0]}？" if near else ""
            bad(f"tauri.conf.json: {scope} 含未知字段 {u!r}{hint}")


def check_tauri_fields() -> None:
    """tauri.conf.json 的字段合法性。

    为什么需要这一步：未知字段不会让 JSON 解析失败，而是等到 `tauri-build` 的 build script
    运行时才炸 —— 也就是四个平台的 CI 各自装完工具链之后。在这里提前拦住，把一轮失败
    从「几十分钟 × 4」压缩到本地的 0 秒。
    """
    group("[7] tauri.conf.json 字段合法性（避免构建期才炸）")
    path = os.path.join(ROOT, "src-tauri", "tauri.conf.json")
    try:
        with open(path, encoding="utf-8") as fh:
            cfg = json.load(fh)
    except Exception:  # noqa: BLE001  # 解析失败已在 [1] 报过
        return
    before = len(failures)
    bundle = cfg.get("bundle")
    if bundle is None:
        ok("bundle 段缺省（合法）")
        return
    _report_unknown("bundle", bundle, _BUNDLE_FIELDS)
    win = bundle.get("windows")
    if win is None:
        ok("bundle.windows 段缺省（合法）")
        return
    _report_unknown("bundle.windows", win, _WINDOWS_FIELDS)
    _report_unknown("bundle.windows.nsis", win.get("nsis"), _NSIS_FIELDS)
    _report_unknown("bundle.windows.wix", win.get("wix"), _WIX_FIELDS)
    if len(failures) == before:
        ok("bundle / bundle.windows 子树字段均合法")


def _read_trim(path: str) -> str | None:
    try:
        with open(path, encoding="utf-8") as fh:
            return fh.read().strip()
    except Exception:  # noqa: BLE001
        return None


def check_version() -> None:
    """VERSION / tauri.conf.json.version / Cargo.toml [package].version /
    CHANGELOG.md 顶部条目版本 必须一致。

    否则打 tag 前很容易漏改某处，发出版本号错乱的安装包。

    顶部允许是 `[Unreleased]`（写好的发布说明还没对应的 tag）：这时拿它下面第一条带版本号的小节
    来比对。一条带版本号的小节都没有时只校验三处代码版本号，并显式说明 CHANGELOG 跳过了本次比对
    —— 那种状态下 CHANGELOG 本来就没有可比的东西，让它报成不一致只会把注意力引向错误的方向。
    """
    group("[8] 版本号一致性")
    v: dict[str, str | None] = {}
    v["VERSION"] = _read_trim(os.path.join(ROOT, "VERSION"))

    try:
        with open(os.path.join(ROOT, "src-tauri", "tauri.conf.json"), encoding="utf-8") as fh:
            v["tauri.conf.json"] = json.load(fh).get("version")
    except Exception:  # noqa: BLE001
        v["tauri.conf.json"] = None

    cargo = _read_trim(os.path.join(ROOT, "src-tauri", "Cargo.toml"))
    if cargo is not None:
        m = re.search(r'^version\s*=\s*"([^"]+)"', cargo, re.M)
        v["Cargo.toml"] = m.group(1) if m else None
    else:
        v["Cargo.toml"] = None

    headings = [ln for ln in (_read_trim(os.path.join(ROOT, "CHANGELOG.md")) or "").splitlines()
                if ln.startswith("## [")]
    numbered = [re.match(r"^##\s*\[(\d+\.\d+\.\d+)\]", ln) for ln in headings]
    released = [m.group(1) for m in numbered if m]
    if not headings:
        v["CHANGELOG.md(顶部)"] = None
    elif headings[0].startswith("## [Unreleased]"):
        if released:
            v["CHANGELOG.md(顶部)"] = released[0]
        else:
            ok("CHANGELOG.md 顶部是 [Unreleased] 且尚无发布条目 —— 本项跳过与它比对")
    else:
        v["CHANGELOG.md(顶部)"] = released[0] if released else None

    for k, val in v.items():
        if val is None:
            bad(f"{k}: 取不到版本号")

    present = {k: val for k, val in v.items() if val is not None}
    if len(set(present.values())) > 1:
        bad("版本号不一致: " + ", ".join(f"{k}={val}" for k, val in present.items()))
    elif present:
        n = "三处" if "CHANGELOG.md(顶部)" not in present else "四处"
        ok(f"{n}版本号一致 = " + next(iter(present.values())))


def _read(path: str) -> str:
    with open(path, encoding="utf-8") as fh:
        return fh.read()


# —————————————————————————————— [9] 文档对齐 ——————————————————————————————

DOC_LANGS = ("", ".zh", ".zh-TW", ".ja", ".ko")  # 空 = English 原文（唯一权威）
TREE_HEADING = "## 3. Repository Layout"
EXTS = (".rs", ".json", ".jsonc", ".toml", ".md", ".html", ".js", ".py", ".sh",
        ".ps1", ".yml", ".yaml", ".png", ".ico", ".icns", ".txt")
HAN = re.compile(r"[\u4e00-\u9fff]")
KANA = re.compile(r"[\u3040-\u30ff]")
HANGUL = re.compile(r"[\uac00-\ud7af]")
# 文档写路径时并不总带仓库根前缀：`platform/macos.rs` 是相对 `src-tauri/src/` 说的。
# 因此「存在性」按这几个锚点逐个试，一个都找不到才算它过期。
PATH_ANCHORS = ("", "src-tauri", "src-tauri/src", "frontend", "scripts", ".github/workflows")


def _strip_fences(text: str) -> str:
    return re.sub(r"```.*?```", "", text, flags=re.S)


def _heading_levels(text: str) -> list[int]:
    """小节层级序列（忽略代码块里以 # 开头的注释行）。"""
    return [len(m.group(1)) for m in re.finditer(r"^(#{1,6}) ", _strip_fences(text), re.M)]


def _technical_tokens(text: str) -> set[str]:
    """反引号里的**技术记号**：路径、文件名、事件名、命令名。

    译文中只有这一类应当逐字保留（它们就是代码里的标识符）；带空格的散文短语不属于这一类。
    """
    out = set()
    for tok in re.findall(r"`([^`\s]{2,80})`", _strip_fences(text)):
        if "/" in tok or tok.endswith(EXTS) or tok.startswith("netsense://"):
            out.add(tok)
    return out


def _all_rust() -> str:
    return "\n".join(_read(p) for p in sorted(glob.glob(os.path.join(SRC, "**", "*.rs"), recursive=True)))


def _ipc_commands() -> list[str]:
    m = re.search(r"generate_handler!\[(.*?)\]\s*\)", _read(os.path.join(SRC, "main.rs")), re.S)
    if not m:
        bad("main.rs 里找不到 invoke_handler 的 generate_handler! 列表")
        return []
    return re.findall(r"ipc::(\w+)", m.group(1))


def _rust_events() -> set[str]:
    return set(re.findall(r'"netsense://([a-z_]+)"', _all_rust()))


def _tree_paths(md_text: str) -> list[str] | None:
    """把 DEVELOPMENT.md §3 的目录树还原成相对仓库根的路径。

    `│   ` 与 `    ` 各占 4 个字符，所以「深度 = 前缀长度 / 4 + 1」；条目名后面的 `#` 注释丢掉。
    """
    m = re.search(r"^" + re.escape(TREE_HEADING) + r".*?\n```[a-z]*\n(.*?)\n```", md_text, re.S | re.M)
    if not m:
        bad("DEVELOPMENT.md 里找不到 §3 仓库结构树的代码块")
        return None
    stack: dict[int, str] = {}
    out: list[str] = []
    for raw in m.group(1).splitlines():
        if not raw.strip():
            continue
        mm = re.match(r"^([│ \t]*)(?:├──|└──)\s*(.+)$", raw)
        if not mm:
            if raw.strip().rstrip("/") == "netsense":
                continue
            bad(f"仓库结构树：无法解析的一行 {raw!r}")
            continue
        depth = len(mm.group(1)) // 4 + 1
        name = mm.group(2).split("#")[0].strip()
        if not name:
            continue
        parent = "" if depth == 1 else stack.get(depth - 1, "")
        path = f"{parent}/{name}" if parent else name
        stack[depth] = path.rstrip("/")
        out.append(path)
    return out


def _expand_braces(tok: str) -> list[str]:
    m = re.search(r"\{([^}]*)\}", tok)
    if not m:
        return [tok]
    return [tok[:m.start()] + alt + tok[m.end():] for alt in m.group(1).split(",")]


def _path_claim(tok: str) -> list[str] | None:
    """这个反引号记号是「本仓库某个文件的路径」吗？是则返回展开后的候选路径。

    文档里带斜杠的东西大多不是路径：`error/warn/info/debug` 是日志级别，`<exe>/logs/` 是
    运行时目录模板，`$XDG_STATE_HOME/…` 是环境变量，`src/index.ts` 是**别的仓库**的文件
    （本仓库没有 .ts，所以不认这个扩展名）。宁可少查，也不要让假报警把真过期淹没。
    """
    if "/" not in tok or tok.startswith(("/", "~", "./")) or "://" in tok:
        return None
    if any(c in tok for c in "$<>*| "):
        return None
    if not any(seg.endswith(EXTS) for seg in tok.split("/")[-1:]):
        return None
    return _expand_braces(tok)


def _check_tree(docs: dict[str, str]) -> None:
    tree = _tree_paths(docs.get("DEVELOPMENT.md", ""))
    if tree is None:
        return
    listed = {p.rstrip("/") for p in tree}
    stale = [p for p in sorted(listed) if not os.path.exists(os.path.join(ROOT, p))]
    if stale:
        bad(f"结构树里有磁盘上不存在的路径（文件已改名或删除）: {stale}")
    else:
        ok(f"结构树的 {len(listed)} 个条目都存在于磁盘")

    real: set[str] = set()
    for path in sorted(glob.glob(os.path.join(SRC, "**", "*.rs"), recursive=True)):
        rel = os.path.relpath(path, ROOT).replace(os.sep, "/")
        if not rel.endswith("main.rs"):
            real.add(rel)
    for d in ("frontend", "scripts"):
        for entry in sorted(os.listdir(os.path.join(ROOT, d))):
            # `.DS_Store` / `__pycache__` 这类本地残留不是要写进文档的东西
            if entry.startswith((".", "__")):
                continue
            real.add(f"{d}/{entry}")
    real.add("src-tauri/src/main.rs")
    missing = sorted(real - listed)
    if missing:
        bad(f"仓库里有、但结构树没写的文件（新增模块要登记）: {missing}")
    else:
        ok("src / frontend / scripts 下的每个文件都进了结构树")


def _check_ipc_table(docs: dict[str, str]) -> None:
    cmds = _ipc_commands()
    text = docs.get("frontend/README.md", "")
    m = re.search(r"^###\s*后端命令一览.*?$(.*?)(?=^##\s|\Z)", text, re.S | re.M)
    if not m:
        bad("frontend/README.md 里找不到「后端命令一览」小节")
        return
    # 只看表格每行的第一格：正文里出现的 `status_payload` 之类的名字不是命令。
    cited: set[str] = set()
    for row in re.findall(r"^\|\s*(.+?)\s*\|", m.group(1), re.M):
        cited |= set(re.findall(r"`(\w+)`", row.split("|")[0]))
    cited.discard("命令")
    undocumented = sorted(set(cmds) - cited)
    invented = sorted(c for c in cited if c not in cmds)
    if undocumented:
        bad(f"已注册但前端契约文档没写的命令: {undocumented}")
    if invented:
        bad(f"文档写了但 main.rs 里没有的命令（改名或删命令时漏同步）: {invented}")
    if not undocumented and not invented:
        ok(f"命令表与 main.rs 注册表一致（{len(cmds)} 条）")


def _check_events(docs: dict[str, str]) -> None:
    cited: set[str] = set()
    for name, text in docs.items():
        cited |= set(re.findall(r"`netsense://([a-z_]+)`", text))
    rust = _rust_events()
    if cited - rust:
        bad(f"文档描述了代码里并不存在的事件: {sorted(cited - rust)}")
    if rust - cited:
        bad(f"代码会广播、但没有任何文档的事件: {sorted(rust - cited)}")
    if not (cited - rust) and not (rust - cited):
        ok(f"事件名双向对齐（{len(rust)} 个: {', '.join(sorted(rust))}）")


def _check_claims(docs: dict[str, str], n_keys: int, langs: int) -> None:
    """文档里写出来的数量必须等于真实来源的数量。

    只对齐这几类最容易烂掉的：i18n key 数、IPC 命令数、界面语言数、本脚本的检查项数。
    「实现进度」「迁移」两节是历史快照，按小节标题跳过 —— 否则每次加文案都要去改写半年前的记录。
    单元测试数不在其列：cfg-gated 的平台测试让它在各台上本来就不同，写死一个数反而是假精确。
    """
    rules = [
        (re.compile(r"(?<![A-Za-z0-9])(\d+)\s*(?:个\s*|個\s*)?keys?\b"), "key 数", lambda ln: True, n_keys),
        (re.compile(r"(\d+)\s*条"), "命令数", lambda ln: "命令" in ln, len(_ipc_commands())),
        (re.compile(r"(\d+)\s*(?:languages?\b|-language\b|語[言種]|言語|语言|개 언어|(?<![定漢語])语(?![法])|語)"),
         "语言数", lambda ln: True, langs),
        (re.compile(r"(\d+)\s+checks\b"), "检查项数", lambda ln: True, GROUPS),
    ]
    stale = []
    for name, text in docs.items():
        for ln in text.splitlines():
            for rx, label, guard, want in rules:
                if not guard(ln):
                    continue
                for got in rx.findall(ln):
                    if int(got) != want:
                        stale.append(f"{name}: 写 {got} {label}，实际 {want}")
    if stale:
        for s in stale:
            bad(s)
    else:
        ok("文档中的数量声明（key 数 / 命令数 / 语言数 / 检查项数）与来源一致")


def _check_translations(docs: dict[str, str]) -> None:
    """5 语文档必须同构：小节层级一一对应，且英文原文里的技术记号一个都不能丢。

    为什么盯这两样：译文最容易出的问题不是措辞，而是**整节没翻**（小节就少了）与
    **抄漏标识符**（`config.example.json` 变成一句中文描述）。两者都能机械判定。
    """
    script_of = {".zh": HAN, ".zh-TW": HAN, ".ja": KANA, ".ko": HANGUL}
    for base in ("README", "CHANGELOG"):
        en_name = f"{base}.md"
        if en_name not in docs:
            bad(f"缺少英文原文 {en_name}")
            continue
        levels = _heading_levels(docs[en_name])
        tokens = {tk for tk in _technical_tokens(docs[en_name]) if not tk.endswith("/")}
        for suf in DOC_LANGS[1:]:
            name = f"{base}{suf}.md"
            if name not in docs:
                bad(f"缺少译文 {name}")
                continue
            text = docs[name]
            if _heading_levels(text) != levels:
                bad(f"{name}: 小节层级与 {en_name} 不一致（{len(_heading_levels(text))} vs {len(levels)}）—— 多半是有小节没译")
            lost = sorted(tk for tk in tokens if tk not in _technical_tokens(text))
            if lost:
                bad(f"{name}: 丢了英文原文里的技术记号 {lost}")
            rx = script_of[suf]
            if len(rx.findall(text)) < 40:
                bad(f"{name}: 正文里几乎没有目标语言字符（{len(rx.findall(text))} 个）—— 像是没翻译的英文副本")
        ok(f"{base}: 5 语小节同构、技术记号齐全")


def check_docs(dicts: dict[str, dict]) -> None:
    """[9]/[10] 文档对齐。

    文档会骗人：代码改了、名字换了、key 涨了，markdown 还停在上周。这两组把文档里
    **可机械判定**的那部分（文件路径、命令名、事件名、数量、译文中保留的标识符）
    逐条对回真实来源 —— 剩下的（措辞、解释是否还准确）只有人来看。
    """
    paths = sorted(glob.glob(os.path.join(ROOT, "*.md"))) + sorted(glob.glob(os.path.join(ROOT, "frontend", "*.md")))
    docs = {os.path.relpath(p, ROOT).replace(os.sep, "/"): _read(p) for p in paths}
    group("[9] 文档 ↔ 代码来源（结构树 / 路径 / 命令表 / 事件名）")
    print(f"  覆盖 {len(docs)} 份文档: {', '.join(sorted(docs))}")
    _check_tree(docs)
    _check_path_claims(docs)
    _check_ipc_table(docs)
    _check_events(docs)
    score()

    group("[10] 文档之间（数量声明 + 5 语同构）")
    n_keys = len(dicts.get("en.json", {}))
    _check_claims(docs, n_keys, len(glob.glob(os.path.join(SRC, "i18n", "*.json"))))
    _check_translations(docs)
    score()


def _check_path_claims(docs: dict[str, str]) -> None:
    # 文档里反引号括起来的**带目录**路径必须真的存在。裸文件名（`popup.html`）不查 ——
    # 它可能指的是别人的仓库或系统里的文件，结构树检查已经覆盖本仓库的文件。
    stale_paths = set()
    n_paths = 0
    for name, text in docs.items():
        for ln in text.splitlines():
            for tok in _technical_tokens(ln):
                cands = _path_claim(tok)
                if not cands:
                    continue
                for cand in cands:
                    n_paths += 1
                    if not any(os.path.exists(os.path.join(ROOT, a, cand)) for a in PATH_ANCHORS):
                        stale_paths.add(f"{name}: `{tok}`")
    if stale_paths:
        bad(f"文档引用了仓库里不存在的路径: {sorted(stale_paths)}")
    else:
        ok(f"文档引用的 {n_paths} 个仓库路径都存在")


def main() -> int:
    print("=" * 62)
    print("NetSense 静态校验")
    print("=" * 62)
    dicts = check_json()
    score()
    check_i18n(dicts)
    score()
    check_key_usage(dicts)
    score()
    check_pal_boundary()
    score()
    check_trait_impls()
    score()
    check_tauri_fields()
    score()
    check_version()
    score()
    check_docs(dicts)
    print("=" * 62)
    if _groups != GROUPS:
        bad(f"检查组数与 GROUPS 不符：本次跑了 {_groups} 组，声明 {GROUPS} 组（每组 10 分才凑得满 100）")
    got = sum(pts for _, pts in _group_scores)
    maxed = 10.0 * len(_group_scores)
    skipped = _groups - len(_group_scores)
    if not _group_scores:
        print("总分: 没有可计分的检查组")
        return 1
    print(f"总分: {100.0 * got / maxed:.1f}/100"
          + (f"（{len(_group_scores)} 组计分，另有 {skipped} 组本次无可计分断言）" if skipped else "（每组 10 分）"))
    for label, pts in _group_scores:
        if pts < 10.0:
            print(f"  未满组: {label.split(']')[0]}] = {pts:.1f}/10")
    if failures:
        print(f"结果: {len(failures)} 项不通过")
        for f in failures:
            print("  -", f)
        return 1
    print("结果: 全部通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
