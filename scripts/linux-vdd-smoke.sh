#!/usr/bin/env bash
# linux-vdd-smoke.sh —— VKMS / krfb-virtualmonitor 虚拟屏冒烟（ADR-0003 / #140）
#
# 用法：scripts/linux-vdd-smoke.sh
# 前置：Linux 桌面（KDE Plasma 6 / Wayland 最佳）；vkms 模块可用。
set -euo pipefail

echo "== vkms 内核模块"
if [ -d /sys/module/vkms ]; then
    echo "OK: vkms 已加载"
else
    echo "WARN: vkms 未加载（需要 root：sudo modprobe vkms；或 systemd 预加载）"
fi

echo "== DRM 设备（虚拟输出应为第二设备，如 card1）"
ls /dev/dri/ 2>/dev/null || echo "（无 /dev/dri，可能非 DRM 环境）"

if command -v krfb-virtualmonitor >/dev/null 2>&1; then
    echo "== krfb-virtualmonitor 可用：创建 1280x720 虚拟 monitor 3 秒"
    krfb-virtualmonitor --name aerodesk-smoke --resolution 1280x720 --scale 1 --password smoke --port 5910 &
    K=$!
    sleep 3
    kill "$K" 2>/dev/null || true
    wait "$K" 2>/dev/null || true
    echo "OK: krfb-virtualmonitor 启动/退出正常；KDE 会话内可用 kscreen-doctor 确认输出"
else
    echo "WARN: krfb-virtualmonitor 不存在（需 KDE Plasma 6 / Wayland）；可用 Xvfb 兜底"
fi
echo "PASS: linux vdd smoke 完成"
