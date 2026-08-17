#!/usr/bin/env bash
# #470 Windows 服务生命周期冒烟：install → status → config → remove（需管理员）。
# 前置：cargo build -p aerodesk-host（FFmpeg DLL 需在 PATH，见 ci.yml Windows 步骤）。
# 登录界面在线/锁屏/让位切换属人工联调（VM），步骤见
# docs/PRELOGIN_WINDOWS_SERVICE.md §9。
set -euo pipefail
cd "$(dirname "$0")/.."
BIN="${AERODESK_HOST:-target/debug/aerodesk-host.exe}"
[ -f "$BIN" ] || { echo "未找到 $BIN（先 cargo build -p aerodesk-host）" >&2; exit 1; }

echo "== 初始状态（未装应为 not installed）"
"$BIN" --service-status

echo "== 安装并启动（需管理员）"
if ! "$BIN" --install-service; then
  EXE="$(pwd)/$BIN"
  echo "安装失败：须以管理员运行。PowerShell 提权示例：" >&2
  echo "  Start-Process -Verb RunAs -FilePath '$EXE' -ArgumentList '--install-service'" >&2
  exit 2
fi

echo "== 状态（应为 运行中）"
"$BIN" --service-status

echo "== 生效配置"
"$BIN" --service-config

echo "== 服务日志（最近 5 行）"
LOG="$PROGRAMDATA/AeroDesk/logs/service.log"
[ -f "$LOG" ] && tail -5 "$LOG" || echo "(日志文件未生成：$LOG)"

echo "== 移除"
"$BIN" --remove-service

echo "== 终态（应为 not installed）"
"$BIN" --service-status

echo "PASS：服务生命周期冒烟通过"
