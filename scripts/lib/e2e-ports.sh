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
#   - 客户端：无需逐条传参——AERO_SIP_PORT 已 export，子进程自动继承。
#   - 端口冲突时：SIP_PORT=15060 SIGNAL_OPS_PORT=13001 bash scripts/xxx-e2e.sh
#
# 不要再在 e2e 里写死 5060 / 3001。

SIP_PORT="${SIP_PORT:-5060}"
SIGNAL_OPS_PORT="${SIGNAL_OPS_PORT:-3001}"

export SIP_PORT
export SIGNAL_OPS_PORT
# 客户端（aerodesk-agent / aerodesk-desktop）取 SIP 端口的通道。
export AERO_SIP_PORT="$SIP_PORT"
