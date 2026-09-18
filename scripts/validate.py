#!/usr/bin/env python3
"""NetSense 静态校验（无需 Rust 工具链，可在任何平台跑；CI 里也用它守基线）。

检查项：
  1. 所有 JSON 文件合法（tauri.conf.json / config.example.json / i18n 字典）
  2. 5 语 i18n key 完全对齐（无 missing / extra / empty）
  3. 同一 key 在各语言的占位符集合一致（防止 {name} 被漏译）
  4. 代码里 i18n::t("...") / i18n::tf("...") 引用的 key 都真实存在
  5. 平台抽象层没有被上层越权绕过（main/ipc/core 里不出现具体平台类型名）
  6. 各平台模块都实现了 NetworkPlatform 的全部方法（按方法名计数兜底检查）

用法：python scripts/validate.py
"""
from __future__ import annotations

import glob
import json
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "src-tauri", "src")

failures: list[str] = []


def ok(msg: str) -> None:
    print(f"  OK   {msg}")


def bad(msg: str) -> None:
    failures.append(msg)
    print(f"  FAIL {msg}")


def check_json() -> dict[str, dict]:
    print("[1] JSON 合法性")
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
    print("[2] i18n key 对齐")
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

    print("[3] 占位符一致性")
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
    print("[4] 代码引用的 i18n key 是否都存在")
    if "en.json" not in dicts:
        return
    known = set(dicts["en.json"])
    used: set[str] = set()
    used_typed: set[str] = set()
    for path in glob.glob(os.path.join(SRC, "**", "*.rs"), recursive=True):
        text = open(path, encoding="utf-8").read()
        used |= set(re.findall(r"i18n::t(?:f)?\(\s*\"([^\"]+)\"", text))
        # 动态 key（如按 PrivChannel 选 key）单独收集，见下
        used_typed |= set(re.findall(r"\"(tray\.priv_(?:direct|prompt))\"", text))
    used |= used_typed
    missing = sorted(k for k in used if k not in known)
    if missing:
        bad(f"代码引用了不存在的 key: {missing}")
    else:
        ok(f"引用 {len(used)} 个 key，全部存在")


def check_pal_boundary() -> None:
    print("[5] 平台抽象边界（上层不得出现具体平台类型）")
    forbidden = re.compile(r"\b(MacPlatform|WindowsPlatform|LinuxPlatform)\b")
    offenders = []
    for name in ("main.rs", "ipc.rs"):
        p = os.path.join(SRC, name)
        if os.path.exists(p) and forbidden.search(open(p, encoding="utf-8").read()):
            offenders.append(name)
    for name in ("matcher.rs", "health.rs", "automation.rs"):
        p = os.path.join(SRC, "core", name)
        if os.path.exists(p) and forbidden.search(open(p, encoding="utf-8").read()):
            offenders.append(f"core/{name}")
    if offenders:
        bad(f"这些文件泄漏了具体平台类型: {offenders}")
    else:
        ok("main / ipc / core 只依赖 Platform 与 trait")


def check_trait_impls() -> None:
    print("[6] 各平台实现是否覆盖 trait 全部方法")
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


def main() -> int:
    print("=" * 62)
    print("NetSense 静态校验")
    print("=" * 62)
    dicts = check_json()
    check_i18n(dicts)
    check_key_usage(dicts)
    check_pal_boundary()
    check_trait_impls()
    print("=" * 62)
    if failures:
        print(f"结果: {len(failures)} 项不通过")
        for f in failures:
            print("  -", f)
        return 1
    print("结果: 全部通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
