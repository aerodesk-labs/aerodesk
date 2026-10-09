#!/usr/bin/env bash
# 检查本次改动是否新增 clippy lint（对比 docs/clippy-baseline.txt 基线）
#
# 背景：main 上有大量历史存量 clippy 告警（见 docs/clippy-baseline.txt），
# `cargo clippy -- -D warnings` 在未改动的 main 上就是红的，无法作为
# 「本次改动是否新增 lint」的判据。本脚本改用「集合差集」：跑与基线生成时
# 相同的 clippy 口径，提取 <文件>:<行>: <lint> 集合，与本基线求差；
# 差集非空 = 新增 lint（退出码 1），否则 0。
#
# 用法：
#   scripts/check-clippy-baseline.sh            # 检查全仓（本机可编译的 crate）
# 退出码：0 = 无新增；1 = 有新增（列在 stderr）；2 = 用法/工具链错误
set -euo pipefail
cd "$(dirname "$0")/.."

BASELINE="docs/clippy-baseline.txt"
# 与基线生成时一致：平台专用 crate 本机不编译（与 CI macos 矩阵一致）
EXCLUDES=(--exclude aerodesk-ios --exclude aerodesk-android --exclude aerodesk-ohos)

if [[ ! -f "$BASELINE" ]]; then
  echo "错误：找不到基线文件 $BASELINE（先按 docs/clippy-baseline.txt 头注重新生成）" >&2
  exit 2
fi

tmp_current="$(mktemp)"
tmp_new="$(mktemp)"
trap 'rm -f "$tmp_current" "$tmp_new"' EXIT

# 1) 跑与基线生成时相同的 clippy，提取本仓（crates/ + vendor/）的 file:line:lint
cargo clippy --workspace "${EXCLUDES[@]}" --all-targets --message-format=json 2>/dev/null \
  | python3 -c '
import json, sys
seen = set()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        m = json.loads(line)
    except Exception:
        continue
    if m.get("reason") != "compiler-message":
        continue
    msg = m.get("message", {})
    if msg.get("level") not in ("warning", "error"):
        continue
    code = msg.get("code", {})
    lint = code.get("code", "") if isinstance(code, dict) else str(code)
    spans = msg.get("spans", [])
    if not spans:
        continue
    fname = spans[0].get("file_name", "")
    lnum = spans[0].get("line_start", "")
    # 只统计本仓代码（crates/ 或 vendor/ 开头；第三方 registry 依赖是绝对路径）
    if not (fname.startswith("crates/") or fname.startswith("vendor/")):
        continue
    key = (fname, lnum, lint)
    if key in seen:
        continue
    seen.add(key)
    print(f"{fname}:{lnum}: {lint}")
' > "$tmp_current"

# 2) 集合差集：当前 − 基线 = 新增
grep -v '^#' "$BASELINE" | sed 's/[[:space:]]*$//' | sort -u > "$tmp_new"  # 基线（去掉注释/空行）
comm -23 <(sort -u "$tmp_current") <(sort -u "$tmp_new") > "$tmp_new.added" 2>/dev/null || true
added="$(sort -u "$tmp_new.added" | grep -v '^$' || true)"

if [[ -n "$added" ]]; then
  echo "❌ 本次改动新增了 clippy lint（与基线 ${BASELINE} 的差集）：" >&2
  echo "$added" | sed 's/^/    /' >&2
  echo "" >&2
  echo "提示：若输出里出现大量『行号漂移』（同一 lint 只是行号变了），说明代码整体位移，" >&2
  echo "      请按基线文件头注重新生成基线，而不是手工改基线。" >&2
  exit 1
fi

echo "✅ 无新增 clippy lint（当前告警集合 ⊆ 基线 ${BASELINE}）"
exit 0
