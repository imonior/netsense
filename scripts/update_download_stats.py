#!/usr/bin/env python3
"""Refresh the "Download counts" block of every published Release.

GitHub exposes a per-asset download counter but shows it nowhere on the Release
page, so the number has to be rendered into the body instead. This script
rewrites **only** what sits between the markers written by
`scripts/gen_release_notes.py` -- anything else a Release author edited by hand
is left alone -- and PATCHes nothing unless the rendered text actually differs,
so a daily run does not needlessly churn (and bump `updated_at` on) every
Release.

It is driven by `gh` so it inherits whatever credentials the environment already
has: CI sets GH_TOKEN, and locally it uses your `gh auth` login.

Usage:

    python scripts/update_download_stats.py                     # update all releases
    python scripts/update_download_stats.py --dry-run           # print, PATCH nothing
    python scripts/update_download_stats.py --repo owner/name
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile

STATS_START = "<!-- downloads-stats-start -->"
STATS_END = "<!-- downloads-stats-end -->"


def gh(*args, expect="json"):
    p = subprocess.run(["gh", "api"] + list(args), capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit("gh api %s failed:\n%s" % (" ".join(args), p.stderr.strip()))
    if expect == "text":
        return p.stdout
    if not p.stdout.strip():
        return None
    return json.loads(p.stdout)


def render_counts(assets):
    """The markdown inside the markers: per-asset table plus a total."""
    ordered = sorted(assets, key=lambda a: -a.get("download_count", 0))
    total = sum(a.get("download_count", 0) for a in assets)
    lines = [
        "### 📊 Download counts",
        "",
        "| Asset | Downloads |",
        "| --- | ---: |",
    ]
    for a in ordered:
        lines.append("| `%s` | %d |" % (a["name"], a.get("download_count", 0)))
    lines += [
        "",
        "**Total: %d downloads** across %d assets." % (total, len(assets)),
        "",
        "_Counted from the GitHub API and refreshed once a day._",
    ]
    return "\n".join(lines)


def rewrite(body, assets):
    """Return (new_body, changed). None for new_body means 'markers absent'."""
    i = body.find(STATS_START)
    j = body.find(STATS_END)
    if i < 0 or j < 0 or j < i:
        return None, False
    new = body[:i] + STATS_START + "\n" + render_counts(assets) + "\n" + STATS_END + body[j + len(STATS_END):]
    return new, new != body


def patch_release(repo, rid, body):
    fd, path = tempfile.mkstemp(suffix=".json")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump({"body": body}, f)
        gh("-X", "PATCH", "repos/%s/releases/%d" % (repo, rid), "--input", path)
    finally:
        if os.path.exists(path):
            os.unlink(path)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""),
                    help="owner/name (defaults to $GITHUB_REPOSITORY)")
    ap.add_argument("--dry-run", action="store_true",
                    help="render and print what would change, but PATCH nothing")
    a = ap.parse_args()
    if not a.repo:
        sys.exit("error: --repo is required when $GITHUB_REPOSITORY is not set")

    releases = gh("repos/%s/releases?per_page=100" % a.repo) or []
    if isinstance(releases, dict):  # a single object comes back as a dict
        releases = [releases]

    updated = skipped = missing = 0
    for rel in releases:
        if rel.get("draft"):
            continue
        rid = rel["id"]
        tag = rel.get("tag_name", "?")
        full = gh("repos/%s/releases/%d" % (a.repo, rid))
        body = full.get("body") or ""
        if STATS_START not in body:
            print("%-12s no markers -- skipping (published before this block existed)" % tag)
            missing += 1
            continue
        new, changed = rewrite(body, full.get("assets") or [])
        if new is None:
            missing += 1
            continue
        if not changed:
            print("%-12s unchanged" % tag)
            skipped += 1
            continue
        print("%-12s updating (%d assets)" % (tag, len(full.get("assets") or [])))
        if a.dry_run:
            start = new.find(STATS_START)
            end = new.find(STATS_END) + len(STATS_END)
            print(new[start:end])
        else:
            patch_release(a.repo, rid, new)
            updated += 1

    print("done: %d updated, %d unchanged, %d without markers" % (updated, skipped, missing))


if __name__ == "__main__":
    main()
