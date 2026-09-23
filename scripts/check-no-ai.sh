#!/bin/sh
# check-no-ai.sh — publish-hygiene guard.
#
# Blocks commits/PRs that leak assistant-tool (AI-IDE) names or the
# standalone token "AI" into (a) commit messages and (b) published files.
#
# Cross-platform & tool-agnostic: runs under any POSIX sh (Git Bash on
# Windows, /bin/sh on macOS/Linux). No external deps.
#
# Usage:
#   scripts/check-no-ai.sh                  # staged-diff scan  (pre-commit hook)
#   scripts/check-no-ai.sh COMMIT_MSG_FILE  # message + staged  (commit-msg hook)
#   scripts/check-no-ai.sh --ci [RANGE]     # CI mode; RANGE defaults to
#                                           # origin/main..HEAD, else HEAD~1..HEAD
#
# The commit message is only available to the commit-msg hook, which git calls
# with the message file as $1. A pre-commit hook receives no arguments at all,
# so it must call this script with none — see scripts/git-hooks/{pre-commit,
# commit-msg}.
#
# Design notes / false-positive guards — each one is load-bearing here:
#   * "cursor" is excluded from the CONTENT scan because it is a CSS
#     property: frontend/{editor,index,popup}.html all contain
#     `cursor: pointer`. It stays in the COMMIT-MESSAGE scan, where a CSS
#     rule can never appear.
#   * "tencent" is excluded from the CONTENT scan: this repo has no legitimate
#     use for the word, and a vendor name in the content list would flag
#     third-party notices that merely mention a Tencent-hosted resolver.
#   * .gitignore / .git/info/exclude are excluded: ignore rules for
#     assistant-tool dirs are permitted (they are NOT published artifacts).
#     See the assistant-tool block at the end of .gitignore.
#   * DEVELOPMENT.md is excluded: §13 states this very rule and therefore
#     names the tools. Without this exemption the guard blocks the commit
#     that documents the guard.
#   * This script and its hook/workflow files are excluded (self-reference).
#   * The standalone-token scan neutralises the guard's own filename before
#     matching (see SELF_REF): "check-no-ai.sh" contains "-ai.", so naming the
#     guard in a commit message or doc would otherwise be a false positive.
#     Assistant-tool names are a separate rule and stay fully enforced.
#   * The commit that introduced the guard is exempted by SHA (see
#     skip_commit): its message spells the rule out in prose, so it fails its
#     own check and would otherwise make every `--ci` range reaching past the
#     guard permanently red.
#   * Every scan uses `grep -c` + `>/dev/null`, never `grep -q`. `-q` exits at
#     the first match, so when the producer is a large multi-KB diff the writer
#     blocks forever once the pipe buffer fills -- MSYS2/Git-Bash does not
#     deliver SIGPIPE reliably, and the guard hangs mid-commit. `-c` reads the
#     whole input, so it cannot deadlock; its exit status is still 0 on a match.
#     Same reason `sed -n '1p'` replaced `head -1`.
#
# Verify after any edit to this list:
#   sh scripts/check-no-ai.sh --ci HEAD~5..HEAD   # a range wide enough to matter

set -eu

AI_TOKEN='\bAI\b'
TOOLS_MSG='(workbuddy|codebuddy|tencent|trae|cursor|claude|copilot|windsurf|codeium|aider)'
TOOLS_CONTENT='(workbuddy|codebuddy|trae|claude|copilot|windsurf|codeium|aider)'
# This script is literally named "check-no-ai.sh" -- the "-ai." inside that
# filename satisfies the standalone-token rule, so since the message scan went
# live (commit-msg hook) any commit message that names the guard would be
# blocked, including the one that edits it. Neutralise that single literal
# before the token scan. Assistant-tool names are still caught by TOOLS_MSG,
# so this cannot be used to smuggle one in. The pattern is spelled one
# character class per letter so it stays case-insensitive under any POSIX sed
# (the GNU-only `I` flag is not portable).
SELF_REF='s/[Cc][Hh][Ee][Cc][Kk]-[Nn][Oo]-[Aa][Ii]/check-publish-hygiene/g'
# Files allowed to mention assistant-tool names / the AI token: self-referential
# guard files, ignore files, and the doc that states the rule.
# NOTE: a `case "$f" in $SKIP)` pattern built from a variable does not honour the
# `|` alternation in every sh, and a bare `scripts/git-hooks/` (no wildcard) cannot
# match its own subfiles — so we use an explicit function with literal patterns.
skip_file() {
  case "$1" in
    scripts/check-no-ai.sh) return 0 ;;
    scripts/git-hooks/*) return 0 ;;
    scripts/setup-hooks.sh) return 0 ;;
    .github/workflows/publish-hygiene.yml) return 0 ;;
    DEVELOPMENT.md) return 0 ;;
    .gitignore) return 0 ;;
    .git/info/exclude) return 0 ;;
  esac
  return 1
}

# Commits whose *message* is exempt from the scan, for the same reason
# DEVELOPMENT.md is exempt from the file scan: the commit that introduced this
# guard necessarily writes the rule out in prose ("the standalone token AI",
# and it names "cursor" while explaining the CSS false positive), so it trips
# its own check. It is immutable history, so skipping it cannot hide anything
# new -- but without it a `--ci` range that reaches back past the guard is red
# forever, which would make the CI half unusable.
skip_commit() {
  case "$1" in
    52886fa*) return 0 ;;
  esac
  return 1
}

err=0
report() { echo "publish-hygiene: $1" >&2; err=1; }

# Reads text on stdin and prints it with any self-reference to the guard's own
# filename neutralised, so naming the guard in a message/doc is not a leak.
# See SELF_REF.
strip_self_ref() { sed -e "$SELF_REF"; }

scan_msg_file() {
  f="$1"
  if grep -Eic "$TOOLS_MSG" "$f" >/dev/null; then
    report "commit message references an assistant tool: $(grep -Eio "$TOOLS_MSG" "$f" | sed -n '1p')"
  fi
  if strip_self_ref < "$f" | grep -Eic "$AI_TOKEN" >/dev/null; then
    report "commit message contains the standalone token 'AI' (forbidden)"
  fi
}

scan_staged() {
  for f in $(git diff --cached --name-only --diff-filter=ACM); do
    if skip_file "$f"; then continue; fi
    diff=$(git diff --cached -- "$f" || true)
    if echo "$diff" | grep -Eic "$TOOLS_CONTENT" >/dev/null; then
      report "staged change to '$f' leaks an assistant-tool name"
    fi
    case "$f" in
      *.md|*.markdown)
        if echo "$diff" | strip_self_ref | grep -Eic "$AI_TOKEN" >/dev/null; then
          report "staged doc '$f' contains the standalone token 'AI'"
        fi ;;
    esac
  done
}

scan_range() {
  range="$1"
  echo "publish-hygiene: scanning $range" >&2
  for c in $(git rev-list "$range" 2>/dev/null || true); do
    if skip_commit "$c"; then continue; fi
    short=$(git rev-parse --short "$c")
    body=$(git log -1 --format=%B "$c")
    hit=$(echo "$body" | grep -Eio "$TOOLS_MSG" | sed -n '1p' || true)
    if [ -n "$hit" ]; then
      report "commit $short references an assistant tool: $hit"
    fi
    if echo "$body" | strip_self_ref | grep -Eic "$AI_TOKEN" >/dev/null; then
      report "commit $short message contains the standalone token 'AI'"
    fi
  done
  for f in $(git diff --name-only "$range" --diff-filter=ACM 2>/dev/null || true); do
    if skip_file "$f"; then continue; fi
    diff=$(git diff "$range" -- "$f" 2>/dev/null || true)
    if echo "$diff" | grep -Eic "$TOOLS_CONTENT" >/dev/null; then
      report "change to '$f' in range leaks an assistant-tool name"
    fi
    case "$f" in
      *.md|*.markdown)
        if echo "$diff" | strip_self_ref | grep -Eic "$AI_TOKEN" >/dev/null; then
          report "doc '$f' in range contains the standalone token 'AI'"
        fi ;;
    esac
  done
}

case "${1:-}" in
  --ci)
    shift
    range="${1:-}"
    if [ -z "$range" ]; then
      if git rev-parse origin/main >/dev/null 2>&1 && [ "$(git rev-parse origin/main)" != "$(git rev-parse HEAD)" ]; then
        range="origin/main..HEAD"
      elif git rev-parse --verify -q 'HEAD~1^{commit}' >/dev/null 2>&1; then
        # No range given and origin/main is level with HEAD: check the tip only.
        # Deliberately NOT a fixed lookback window -- that re-scans history which
        # predates the guard and goes red on the guard's own commit message.
        range="HEAD~1..HEAD"
      else
        range="HEAD"   # root commit: `git log` walks nothing but this commit
      fi
    fi
    scan_range "$range"
    ;;
  "")
    echo "publish-hygiene: staged-diff scan (commit message is checked by the commit-msg hook)" >&2
    scan_staged
    ;;
  *)
    scan_msg_file "$1"
    scan_staged
    ;;
esac

if [ "$err" -ne 0 ]; then
  echo "publish-hygiene: BLOCKED. Remove assistant-tool references / the 'AI' token from the commit message and changes." >&2
  exit 1
fi
echo "publish-hygiene: OK"
