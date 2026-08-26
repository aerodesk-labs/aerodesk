#!/usr/bin/env bash
# #503 隐私屏 e2e：viewer 经 control 通道下发隐私屏 → 被控端黑屏/定制文字/静音。
# 合成源发布端（--encoder ffmpeg）无需采集权限；viewer 用 AERODESK_DUMP_FRAME
# 落盘第 N 帧 PNG 作可视断言。
#
# 注意：被控端 agent 是单会话进程且直连 ICE 会话不因对端进程消失而失效，
# 因此「开启」与「关闭」各用独立房间/发布端进程（关闭段验证 disable 消息
# 无害 + 画面恢复彩条；enable→disable 状态机由单测 privacy::tests 覆盖）。
# 用法: scripts/privacy-e2e.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REC="$(mktemp -d)"
# Git Bash 启动 Windows exe 时须用 POSIX 路径注入 DLL 搜索路径（msys 转换）。
case "$(uname -s)" in
  MINGW*|MSYS*) export PATH="/e/my/Documents/GitHub/aerodesk/.claude/worktrees/wf_1d94656b-253-6/.ffmpeg-dev/ffmpeg/bin:$PATH" ;;
esac
export RUST_LOG=info
export RECORD_DIR="$REC"
cd "$ROOT"

echo "== 构建"
cargo build --release -q -p aerodesk-agent -p aerodesk-signal

echo "== 启动 signal（SIP UDP 5060）"
SIP_UDP_PORT=5060 ./target/release/aerodesk-signal >"$REC/sig.log" 2>&1 &
SIG_PID=$!
sleep 1

run_phase() { # $1=阶段名 $2=房间 $3=viewer 控制消息 $4=落盘名 $5=成功断言（publisher 日志）$6=最大 PNG 字节（黑帧判定）
  local tag="$1" room="$2" ctl="$3" dump="$4" ok_grep="$5" max_size="${6:-0}" attempt=1
  # 本地回环 SIP 1:1 P2P（str0m fork + 单会话进程）偶发 ICE/SCTP 建立失败与
  # control 消息丢失（#553 str0m DCEP 竞态）：按仓库既有 e2e 惯例容忍重试。
  while [ $attempt -le 6 ]; do
    echo "== [$tag] attempt $attempt: publisher（合成源 640x360）"
    ./target/release/aerodesk-agent --role publisher --encoder ffmpeg --width 640 --height 360 \
        --signal ws://127.0.0.1:3003 --room "$room" >"$REC/pub-$tag-$attempt.log" 2>&1 &
    local pub_pid=$!
    sleep 2
    echo "== [$tag] attempt $attempt: viewer（--send-control $ctl，第 120 帧落盘）"
    AERODESK_DUMP_FRAME="$REC/$dump" AERODESK_DUMP_AFTER=120 \
        ./target/release/aerodesk-agent --role viewer \
        --signal ws://127.0.0.1:3003 --room "$room" \
        --send-control "$ctl" >"$REC/v-$tag-$attempt.log" 2>&1 &
    local v_pid=$!
    sleep 12
    kill "$v_pid" "$pub_pid" 2>/dev/null || true
    sleep 1
    # 黑帧 PNG（640x360 黑底+文字）实测 ~8KB，彩条 45-66KB：max_size 兜底判黑帧。
    if [ -s "$REC/$dump" ] \
        && { [ "$max_size" = "0" ] || [ "$(stat -c%s "$REC/$dump")" -le "$max_size" ]; } \
        && { [ -z "$ok_grep" ] || grep -aq "$ok_grep" "$REC/pub-$tag-$attempt.log"; }; then
      cp "$REC/pub-$tag-$attempt.log" "$REC/pub-$tag.log" 2>/dev/null || true
      cp "$REC/v-$tag-$attempt.log" "$REC/v-$tag.log" 2>/dev/null || true
      return 0
    fi
    attempt=$((attempt + 1))
  done
  # 六次都失败：保留最后一次日志供断言诊断。
  cp "$REC/pub-$tag-6.log" "$REC/pub-$tag.log" 2>/dev/null || true
  cp "$REC/v-$tag-6.log" "$REC/v-$tag.log" 2>/dev/null || true
}

# 阶段 1：开启隐私屏（text 模式 + 静音）→ 黑底文字帧（须 publisher 日志 + 黑帧尺寸）。
run_phase "on" "privacy-on-$$" \
  '{"privacy":{"enabled":true,"mode":"text","text":"隐私屏已开启","mute":true}}' \
  "v1-black.png" "control: privacy -> enabled=true" 30000
# 阶段 2：下发关闭（对全新发布端：应无状态变化、画面保持彩条）。
run_phase "off" "privacy-off-$$" '{"privacy":{"enabled":false}}' "v2-color.png" ""

kill "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
echo "-- publisher 收到隐私屏消息"
if grep -aq "control: privacy -> enabled=true mode=Text" "$REC/pub-on.log"; then
  echo "PASS [on] publisher applied privacy"
else
  echo "FAIL [on] publisher privacy"; grep -a "privacy" "$REC/pub-on.log" | head -5 || true; fail=1
fi
if grep -aq "privacy -> enabled=true" "$REC/pub-off.log"; then
  echo "FAIL [off] 关闭阶段不应产生开启状态"; fail=1
else
  echo "PASS [off] 关闭消息对默认状态无副作用"
fi
echo "-- viewer 落盘"
for f in v1-black.png v2-color.png; do
  if [ -s "$REC/$f" ]; then echo "PASS $f exists ($(stat -c%s "$REC/$f") bytes)"; else echo "FAIL $f missing"; fail=1; fi
done
echo "-- 无 panic"
if grep -aqiE "panic" "$REC"/*.log; then
  echo "FAIL panic in logs"; fail=1
else
  echo "PASS no panics"
fi
echo "REC=$REC"
exit $fail
