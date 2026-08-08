#!/usr/bin/env bash
# macos-vdd-smoke.sh —— BetterDisplay CLI 虚拟屏冒烟（ADR-0002 / #140）
#
# 用法：scripts/macos-vdd-smoke.sh
# 前置：BetterDisplay 2.2.x+ 已安装并运行，betterdisplaycli 在 PATH。
set -euo pipefail

if ! command -v betterdisplaycli >/dev/null 2>&1; then
    echo "FAIL: betterdisplaycli 不存在（需安装 BetterDisplay 并启用 CLI）" >&2
    exit 2
fi
if ! pgrep -x BetterDisplay >/dev/null 2>&1 && ! pgrep -x "BetterDisplay Helper" >/dev/null 2>&1; then
    echo "FAIL: BetterDisplay app 未运行（虚拟屏依赖 app 常驻）" >&2
    exit 2
fi

NAME="aerodesk-smoke-$(date +%s)"
echo "== create virtual screen: $NAME (16:9)"
betterdisplaycli create -devicetype=virtualscreen -virtualscreenname="$NAME" -aspectWidth=16 -aspectHeight=9

echo "== verify in display list"
system_profiler SPDisplaysDataType 2>/dev/null | grep -i "aerodesk-smoke" && echo "PASS: 虚拟屏出现在系统显示器列表" || {
    echo "FAIL: 未在显示器列表中找到 $NAME" >&2
    exit 1
}

cat <<EOF
== 清理提示
虚拟屏已创建；请在 BetterDisplay app 中删除，或用 betterdisplaycli 删除命令
（语法随版本而异，先看 \`betterdisplaycli --help\` / \`betterdisplaycli create --help\`）。
EOF
