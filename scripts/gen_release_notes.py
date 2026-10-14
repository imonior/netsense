#!/usr/bin/env python3
"""Generate the GitHub Release body for a NetSense release.

Why a script instead of an inline block in build.yml: the body has three parts
that must agree with each other -- the localized-changelog switch line, the
English release notes, and the per-platform download block. Keeping them in one
place means the asset names can be checked against what tauri-action actually
uploads, instead of discovering a 404 link after a Release has been published.

Two constraints worth knowing before editing the asset names below:

* The names are what **tauri-action** decides to upload, not what Tauri names
  locally, and per architecture they are not uniform: Windows uses
  ``_x64`` / ``_x86`` / ``_arm64``, the macOS dmg uses ``_aarch64`` / ``_x64``,
  and the Linux deb uses ``_amd64`` / ``_arm64``. They were taken from the real
  v1.6.4 asset list; changing anything here without re-checking a live Release
  page silently produces dead links.
* The portable archives are not built by tauri-action -- "Build portable
  archive" zips them itself, so those names follow the matrix labels
  (``matrix.label``, e.g. ``windows-x64``) rather than the arch tokens above.

Usage:

    python scripts/gen_release_notes.py --version 1.6.4 --tag v1.6.4 \\
        --repo imonior/netsense --out release-notes.md

``--prev`` defaults to the version below this one in CHANGELOG.md; it only feeds
the "Full Changelog" compare link.
"""

import argparse
import io
import re
import sys

HEADING_RE = re.compile(r"^## \[([^\]]+)\]")

# (Tauri arch token used in the file name, human label)
WINDOWS_ARCHES = [
    ("x64", "64-bit (x86_64)"),
    ("x86", "32-bit (x86)"),
    ("arm64", "ARM64 (aarch64)"),
]

MACOS_DMGS = [
    ("aarch64", "Apple Silicon (aarch64)"),
    ("x64", "Intel (x86_64)"),
]

LINUX_DEBS = [
    ("amd64", "Debian/Ubuntu, x86_64"),
    ("arm64", "Debian/Ubuntu, arm64"),
]

# matrix.label -> description. Must stay in sync with the build matrix.
PORTABLES = [
    ("windows-x64", "Windows 64-bit (x86_64)"),
    ("windows-x86", "Windows 32-bit (x86)"),
    ("windows-arm64", "Windows ARM64 (aarch64)"),
    ("macos-x64", "macOS Intel (x86_64)"),
    ("macos-arm64", "macOS Apple Silicon (aarch64)"),
    ("linux-x64", "Linux x86_64"),
    ("linux-arm64", "Linux arm64"),
]

STATS_START = "<!-- downloads-stats-start -->"
STATS_END = "<!-- downloads-stats-end -->"


def version_headings(text):
    """Every (version, line) pair for `## [x.y.z]` headings, newest first."""
    out = []
    for line in text.splitlines():
        m = HEADING_RE.match(line)
        if m:
            out.append(m.group(1).strip())
    return out


def extract_section(text, version):
    """The bullet list under `## [version]`, stopping at the next version."""
    head = "## [%s]" % version
    collected, inside = [], False
    for line in text.splitlines():
        if line.startswith("## ["):
            if inside:
                break
            # `## [1.6.4] - 2026-10-10`: the version run is followed by a space.
            if line.startswith(head) and (len(line) == len(head) or line[len(head)] == " "):
                inside = True
            continue
        if inside:
            collected.append(line)
    return "\n".join(collected).strip("\n")


def previous_version(text, version):
    vers = version_headings(text)
    try:
        i = vers.index(version)
    except ValueError:
        return None
    return vers[i + 1] if i + 1 < len(vers) else None


def link(base, name, tail=""):
    return "- [%s](%s/%s)%s" % (name, base, name, " — " + tail if tail else "")


def build_notes(repo, tag, version, prev, changelog_text, include_sig=True):
    section = extract_section(changelog_text, version)
    if not section.strip():
        sys.exit("error: no `## [%s]` section found in CHANGELOG.md" % version)

    blob = "https://github.com/%s/blob/%s" % (repo, tag)
    down = "https://github.com/%s/releases/download/%s" % (repo, tag)
    v = version

    out = []
    # Language switch. It points at the *localized changelogs*, not the READMEs --
    # a reader clicking 简体中文 here wants this release's notes in Chinese, and
    # linking the project README instead was the bug this replaced. Absolute blob
    # URLs pinned to this tag are required: a Release page has no document base,
    # so a relative README.zh.md resolves against .../releases and 404s.
    out.append("**English** · [简体中文](%s/CHANGELOG.zh.md) · "
               "[繁體中文](%s/CHANGELOG.zh-TW.md) · "
               "[日本語](%s/CHANGELOG.ja.md) · "
               "[한국어](%s/CHANGELOG.ko.md)" % (blob, blob, blob, blob))
    out.append("")
    out.append(section)
    out.append("")
    out.append("---")
    out.append("")
    out.append("## 📦 Downloads")
    out.append("")
    out.append("_Click a file name to download it directly; every asset is also "
               "attached to this Release page below._")
    out.append("")
    # The daily `download-stats` workflow rewrites only what sits between the two
    # markers, so an editor-friendly placeholder is fine here.
    out.append(STATS_START)
    out.append("### 📊 Download counts")
    out.append("")
    out.append("_Download counts are collected once a day; this table is refreshed "
               "by the `download-stats` workflow._")
    out.append(STATS_END)
    out.append("")
    out.append("### Windows")
    out.append("")
    out.append("**Installers (recommended)** — bundle everything, no extra downloads:")
    for arch, label in WINDOWS_ARCHES:
        out.append(link(down, "NetSense_%s_%s-setup.exe" % (v, arch),
                        "NSIS installer, %s" % label))
        out.append(link(down, "NetSense_%s_%s_en-US.msi" % (v, arch),
                        "MSI installer, %s" % label))
    out.append("")
    out.append("**Portable builds (no installation)** — extract and run `netsense.exe`:")
    for lbl, label in [(l, d) for l, d in PORTABLES if l.startswith("windows-")]:
        out.append(link(down, "NetSense_%s_portable.zip" % lbl, label))
    out.append("")
    out.append("### macOS")
    out.append("")
    for arch, label in MACOS_DMGS:
        out.append(link(down, "NetSense_%s_%s.dmg" % (v, arch),
                        "drag-and-drop installer, %s" % label))
    out.append("")
    out.append("**Portable builds (no installation)** — extract and run `NetSense.app`:")
    for lbl, label in [(l, d) for l, d in PORTABLES if l.startswith("macos-")]:
        out.append(link(down, "NetSense_%s_portable.zip" % lbl, label))
    out.append("")
    out.append("### Linux")
    out.append("")
    for arch, label in LINUX_DEBS:
        out.append(link(down, "NetSense_%s_%s.deb" % (v, arch), label))
    out.append("")
    out.append("**Portable builds (no installation)** — extract and run `netsense`:")
    for lbl, label in [(l, d) for l, d in PORTABLES if l.startswith("linux-")]:
        out.append(link(down, "NetSense_%s_portable.zip" % lbl, label))
    out.append("")
    out.append("### Checksums")
    out.append("")
    out.append(link(down, "SHA256SUMS", "expected hash of every file above"))
    if include_sig:
        out.append(link(down, "SHA256SUMS.sig", "Ed25519 signature over SHA256SUMS"))
    out.append("")
    if include_sig:
        out.append("**Verify**: check each file against `SHA256SUMS`, and check that file "
                   "against `SHA256SUMS.sig` — the public key is embedded in the app.")
    else:
        # Only meaningful for releases published before UPDATE_SIGNING_KEY existed.
        # Linking a .sig that was never uploaded is a silently dead link.
        out.append("**Verify**: compare each file against `SHA256SUMS`. This release has no "
                   "`.sig` — it was published before update signing was set up.")
    out.append("")
    if prev:
        out.append("**Full Changelog**: https://github.com/%s/compare/v%s...%s"
                   % (repo, prev, tag))
    return "\n".join(out).rstrip("\n") + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True, help="numeric version, e.g. 1.6.4")
    ap.add_argument("--tag", required=True, help="release tag, e.g. v1.6.4")
    ap.add_argument("--repo", required=True, help="owner/name")
    ap.add_argument("--changelog", default="CHANGELOG.md")
    ap.add_argument("--prev", default=None, help="previous version for the compare link")
    ap.add_argument("--omit-sig", action="store_true",
                    help="omit the SHA256SUMS.sig link (for releases published before "
                         "update signing existed, where that asset was never uploaded)")
    ap.add_argument("--out", default=None, help="write here instead of stdout")
    a = ap.parse_args()

    text = io.open(a.changelog, encoding="utf-8").read()
    prev = a.prev or previous_version(text, a.version)
    body = build_notes(a.repo, a.tag, a.version, prev, text,
                       include_sig=not a.omit_sig)

    if a.out:
        io.open(a.out, "w", encoding="utf-8", newline="\n").write(body)
    else:
        sys.stdout.write(body)


if __name__ == "__main__":
    main()
