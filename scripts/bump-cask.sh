#!/usr/bin/env bash
# 把某个**已发布** Release 的两个 macOS dmg 的 SHA256 写进 homebrew-tap 的 Casks/netsense.rb。
#
# 这里是 cask 模板的唯一一份副本：build.yml 的 cask 作业（跟着发布自动跑）和 cask.yml
# （人在页面上点 Publish、或手动补跑）都调用本脚本。模板抄两遍，迟早会写出两份不一致的
# cask —— 而每次 bump 的 diff 本该只有版本号与哈希两行。
#
# 为什么必须等 Release 真的发布之后再动 cask：tag 推送时 tauri-action 建的是 **Draft**，
# 而 `releases/download/<tag>/...` 对未发布的 Draft 返回 404。那一刻 bump，
# `brew install --cask netsense` 会在「已推 tag、还没点发布」的整个窗口里装不上。
#
# 用法：scripts/bump-cask.sh <tag>              # 改写并 push tap
#       scripts/bump-cask.sh <tag> --dry-run    # 只打印将写入的文件与 diff，什么都不 push
#
# 环境变量：
#   TAP_PUSH_TOKEN  对 $TAP_REPO 有 Contents: Read and write 的 PAT。没给就跳过（exit 0）——
#                   一个没配 token 的仓库不该让整条流水线变红。
#   TAP_REPO        默认 imonior/homebrew-tap
#   GH_TOKEN        可选。Release 元数据是公开的，没 token 也读得到；有就用上，避开匿名限流。
#
# SHA256 取自 Release 资产自带的 digest 字段 —— 服务端已经算好，不必下载 5MB 的 dmg，
# 也就不受「代理把下载截断成一个残文件、于是哈希算错」这类问题影响。
set -euo pipefail

SRC_REPO="imonior/netsense"
TAP_REPO="${TAP_REPO:-imonior/homebrew-tap}"
DRY_RUN=0
TAG="${1:-}"
if [ "${2:-}" = "--dry-run" ]; then DRY_RUN=1; fi
if [ -z "$TAG" ]; then
  echo "usage: $(basename "$0") <release-tag> [--dry-run]" >&2
  exit 2
fi
# 把选项当成正向参数递进来的话（bump-cask.sh --dry-run v1.0.4），下面那个 '-' 判定
# 会把它认成预发布 tag 然后静默跳过 —— 一个说清楚用法的错误比这有用。
case "$TAG" in
  -*) echo "usage: $(basename "$0") <release-tag> [--dry-run]  (tag came first: '$TAG')" >&2; exit 2 ;;
esac
VERSION="${TAG#v}"

# tag 是要写进 tap 里那个 cask 文件的，而 cask 由 brew 用 Ruby 求值 —— 一个带引号的
# "tag" 就成了对所有 `brew upgrade` 用户的代码执行。所以这里只放过 semver 形状，
# 其余（包括手敲错的 workflow_dispatch 输入）一律在这里挡住，而不是写进 tap。
if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]]; then
  echo "error: '$TAG' is not a release tag (expected vMAJOR.MINOR.PATCH[-prerelease])" >&2
  exit 2
fi

# 预发布版本不动稳定 cask（tag 里带 '-' 就是预发布）。
case "$TAG" in
  *-*) echo "notice: $TAG is a prerelease tag; the stable cask stays where it is"; exit 0 ;;
esac

if [ "$DRY_RUN" = "0" ] && [ -z "${TAP_PUSH_TOKEN:-}" ]; then
  echo "notice: TAP_PUSH_TOKEN is not set; skipping the cask update."
  exit 0
fi

# token 只进下面那一条临时 clone 的 remote URL，本文件从不打印含它的字符串。
release_json="$(mktemp)"
# shellcheck disable=SC2064  # 这里要在 trap 设定时就把 $release_json 展开成当前值
trap "rm -f '$release_json'" EXIT
api_url="https://api.github.com/repos/$SRC_REPO/releases/tags/$TAG"
# 手动 dispatch 时 tag 是人敲的，打错就会 404。curl 在这里只吐一行错误码，
# 说清楚「查不到这个已发布的 Release」比那行有用。
fetch_release() {
  if [ -n "${GH_TOKEN:-}" ]; then
    curl -fsSL -H "Authorization: Bearer $GH_TOKEN" "$api_url" -o "$release_json"
  else
    curl -fsSL "$api_url" -o "$release_json"
  fi
}
if ! fetch_release; then
  echo "error: no release found for $TAG in $SRC_REPO. A cask bump needs a *published* release; " >&2
  echo "       a draft is not reachable by tag, and a typo'd tag is not a tag at all." >&2
  exit 1
fi

# 两个 dmg 的 digest 必须都在，且都是 64 位十六进制：宁可这里报错，
# 也不要写进一个装不上的 cask（brew 会在校验哈希时失败）。
read_shas() {
  python3 - "$release_json" <<'PY'
import json, re, sys
data = json.load(open(sys.argv[1]))
want = {"aarch64": None, "x64": None}
for a in data.get("assets", []):
    n = a["name"]
    if not n.endswith(".dmg"):
        continue
    for arch in want:
        if n.endswith("_%s.dmg" % arch):
            want[arch] = a.get("digest") or ""
for arch, d in want.items():
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", d):
        sys.exit("missing or malformed dmg digest for %s (asset name(s) not found?)" % arch)
print(want["aarch64"][7:])
print(want["x64"][7:])
PY
}
shas="$(read_shas)"
ARM_SHA="$(printf '%s\n' "$shas" | sed -n 1p)"
INTEL_SHA="$(printf '%s\n' "$shas" | sed -n 2p)"

WORK="$(mktemp -d)"
# shellcheck disable=SC2064
trap "rm -rf '$release_json' '$WORK'" EXIT
# dry run 只是要读 tap，不需要凭证；真跑才把 PAT 放进 clone 的 URL。
if [ "$DRY_RUN" = "1" ]; then
  clone_url="https://github.com/$TAP_REPO"
else
  clone_url="https://x-access-token:${TAP_PUSH_TOKEN}@github.com/$TAP_REPO"
fi
git clone "$clone_url" "$WORK/tap" >/dev/null
cd "$WORK/tap"
CASK_PATH="Casks/netsense.rb"

# 下面这块非 version / sha256 / url 的文本必须与 tap 里现存的 Casks/netsense.rb 逐字节一致，
# 这样每次 bump 的 diff 就只有版本与哈希。反引号要转义，否则 heredoc 会把 \`xattr ...\` 当命令执行。
#
# 这里**故意不写** depends_on macos:。NetSense 要求 10.15+，而 10.15 已在 Homebrew 自己的
# 运行下限（Big Sur / 11）之下；这种"下限以下"的显式最低版本既冗余、又会被 Homebrew 7.0
# 直接拒绝：
#     Calling `depends_on macos: :catalina` is disabled! There is no replacement.
# 一旦出现，`brew install --cask netsense` 连解析 cask 都过不去。官方 Cask Cookbook
# 对这种情况的指引就是"该声明冗余，删掉"——所以这里没有这一行。
cat > "$CASK_PATH" <<EOF
cask "netsense" do
  version "$VERSION"
  arch arm: "aarch64", intel: "x64"
  sha256 arm: "$ARM_SHA",
         intel: "$INTEL_SHA"

  url "https://github.com/$SRC_REPO/releases/download/v#{version}/NetSense_#{version}_#{arch}.dmg"
  name "NetSense"
  desc "Cross-platform SSID-aware network profile switcher"
  homepage "https://github.com/$SRC_REPO"

  # No depends_on macos: here -- NetSense needs 10.15+, which is at or below
  # Homebrew's own floor (Big Sur), so it is redundant and Homebrew 7.0 rejects
  # it outright ("Calling depends_on macos: :catalina is disabled!").
  app "NetSense.app"

  # Unsigned build: strip the Gatekeeper quarantine flag after install
  # (equivalent to the user running \`xattr -dr com.apple.quarantine\`).
  postflight do
    system_command "/usr/bin/xattr",
                   args: ["-dr", "com.apple.quarantine", "#{appdir}/NetSense.app"]
  end

  uninstall quit: "com.netsense.app"

  zap trash: [
    "~/Library/Application Support/com.netsense.app",
    "~/Library/Caches/com.netsense.app",
    "~/Library/Preferences/com.netsense.app.plist",
  ]
end
EOF

echo "---- Casks/netsense.rb ----"
cat "$CASK_PATH"

if [ "$DRY_RUN" = "1" ]; then
  echo "---- diff against the tap's current file (dry run; nothing pushed) ----"
  git diff -- Casks/netsense.rb || true
  echo "dry run: arm=$ARM_SHA"
  echo "dry run: intel=$INTEL_SHA"
  exit 0
fi

git config user.name  "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
# no-op 守卫：同 tag 重跑时跳过空 diff。只查本项目的 cask 文件，因此 rebase 带上来的
# 别家改动不会产生空提交。
if git diff --quiet -- "$CASK_PATH"; then
  echo "cask already up to date for $TAG; nothing to commit."
  exit 0
fi
git add "$CASK_PATH"
git commit -m "netsense ${TAG}: cask bump"

# tap 上共享 main 分支的另一个项目可能在本次运行期间 push 过 cask bump，使裸 `git push`
# non-fast-forward 失败。先 rebase 到最新 tap main 再重试，吸收这个并发窗口。
# `|| true` 让 rebase 的偶发失败也进入下一次重试，而不是被 set -e 判死。
pushed=false
for i in 1 2 3 4 5; do
  git pull --rebase origin main || true
  if git push origin main; then
    pushed=true
    break
  fi
  echo "push attempt $i failed; rebasing onto latest tap main and retrying..."
  sleep 3
done
if [ "$pushed" != "true" ]; then
  echo "::error::could not push the netsense cask bump to $TAP_REPO after 5 attempts"
  exit 1
fi
echo "pushed $TAG to $TAP_REPO"
