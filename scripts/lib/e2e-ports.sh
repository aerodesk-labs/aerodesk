#!/usr/bin/env bash
# e2e 端口统一：SIP / ops 端口可被外层覆盖，客户端自动跟随。
#
# 为什么需要它（2026-10-08 实测，macOS 开发机）：
#   本机 127.0.0.1:5060 被别的 SIP 服务占用（实测是 FreeSWITCH）。
#   aerodesk-signal 绑 0.0.0.0:5060 后，内核把 REGISTER 的回包交给更具体的
#   127.0.0.1:5060（FreeSWITCH），客户端因此收到 403 Forbidden，
#   日志提示「检查 signal 的 SIP 端口/口令」——指向完全错误的方向，
#   于是所有把 5060 写死的 e2e 都假红。
#
# 用法：在脚本完成 `cd "$(dirname "$0")/.."`（或设定 ROOT）之后 source 本文件：
#
#   source scripts/lib/e2e-ports.sh          # 形如 cd "$(dirname "$0")/.."
#   source "$ROOT/scripts/lib/e2e-ports.sh"  # 形如 ROOT="$(cd "$(dirname "$0")/.." && pwd)"
#
# 然后：
#   - 服务端：SIP_UDP_PORT="$SIP_PORT" ./target/debug/aerodesk-signal &
#   - 客户端：无需逐条传参——AERO_SIP_PORT 已 export，**aerodesk-agent** 直接读它；
#     **aerodesk-desktop 不读该变量**（它取 `~/.aerodesk-settings.json` 的 sip_port），
#     所以 UI e2e（macos/linux/windows-ui-e2e.sh）在 seed 设置时用 SIP_PORT 写入
#     sip_port——不要写死 5060，否则端口覆盖只生效一半。
#   - 端口冲突时：SIP_PORT=15060 SIGNAL_OPS_PORT=13001 bash scripts/xxx-e2e.sh
#
# 不要再在 e2e 里写死 5060 / 3001。

SIP_PORT="${SIP_PORT:-5060}"
SIGNAL_OPS_PORT="${SIGNAL_OPS_PORT:-3001}"

export SIP_PORT
export SIGNAL_OPS_PORT
# aerodesk-agent 取 SIP 端口的通道（aerodesk-desktop 不读这个变量，见上）。
export AERO_SIP_PORT="$SIP_PORT"
# SIP/TCP 口与 UDP 同号。**必须导出**：`sip-default-tcp` 起客户端默认走 TCP，而脚本只把
# `SIP_UDP_PORT` 指到 $SIP_PORT——不导这个变量的话，agent 会按 TCP 拨到 signal 的*默认*
# 5060（本机被 FreeSWITCH 占）而假红。两种时代都安全：旧 build 不认识该变量（忽略），
# 新 build 才会据此把 TCP 监听开到同一端口。
export SIP_TCP_PORT="$SIP_PORT"
