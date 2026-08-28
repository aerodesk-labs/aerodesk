#!/usr/bin/env bash
# #228 SFU 音频转发 + 音视频同步端到端验收：
#   {直连, TURN 中继} × {PCMU, Opus} 四组合——publisher（ffmpeg h264 + 音频）经
#   SFU 转发，viewer 断言 AUDIO frames>0、played>0、|drift_ms|≤300、视频 DECODED>0。
# 用法: scripts/sfu-audio-e2e.sh [mode...] [audio...]
#   mode: direct|turn（默认两者）；audio: pcmu|opus（默认两者）
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

MODES=(); AUDIOS=()
for a in "$@"; do
  case "$a" in
    direct|turn) MODES+=("$a") ;;
    pcmu|opus) AUDIOS+=("$a") ;;
    *) echo "unknown arg $a"; exit 2 ;;
  esac
done
[ ${#MODES[@]} -eq 0 ] && MODES=(direct turn)
[ ${#AUDIOS[@]} -eq 0 ] && AUDIOS=(pcmu opus)
DRIFT_LIMIT=300

fail() { echo "FAIL: $*"; exit 1; }

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

echo "mode     audio   result   video_decoded   audio_frames   played   drift_ms   errors"
for MODE in "${MODES[@]}"; do
for AUDIO in "${AUDIOS[@]}"; do
  pkill -f 'aerodesk-(sfu|signal|cli)' 2>/dev/null || true
  sleep 1
  REC="$(mktemp -d)"
  AUDIO_EXTRA=""
  [ "$AUDIO" = "opus" ] && AUDIO_EXTRA="--audio-opus"
  TURN_ENV=()
  if [ "$MODE" = "turn" ]; then
    export AERODESK_FORCE_RELAY
    AERODESK_FORCE_RELAY=1
    RECORD_DIR="$REC" SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
      TURN_SECRET=testsecret SFU_TURN_PORT=14789 \
      "$TARGET_DIR/aerodesk-sfu" >/tmp/audio-sfu.log 2>&1 &
    SFU=$!
    SIGNAL_PORT=14501 SFU_URL=http://127.0.0.1:14502 \
      TURN_SECRET=testsecret TURN_URLS="turn:127.0.0.1:14789?transport=udp" \
      "$TARGET_DIR/aerodesk-signal" >/tmp/audio-sig.log 2>&1 &
    SIG=$!
  else
    AERODESK_FORCE_RELAY=0
    RECORD_DIR="$REC" SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
      "$TARGET_DIR/aerodesk-sfu" >/tmp/audio-sfu.log 2>&1 &
    SFU=$!
    SIGNAL_PORT=14501 SFU_URL=http://127.0.0.1:14502 \
      "$TARGET_DIR/aerodesk-signal" >/tmp/audio-sig.log 2>&1 &
    SIG=$!
  fi
  for _ in $(seq 1 80); do
    nc -z 127.0.0.1 14502 2>/dev/null && nc -z 127.0.0.1 14503 2>/dev/null && break
    sleep 0.2
  done
  sleep 0.3

  "$TARGET_DIR/aerodesk-agent" --role publisher --signal ws://127.0.0.1:14503 --room "audio-${MODE}-${AUDIO}" \
    --encoder ffmpeg --codec h264 --noisy --audio $AUDIO_EXTRA >/tmp/audio-pub.log 2>&1 &
  PUB=$!
  "$TARGET_DIR/aerodesk-agent" --role viewer --signal ws://127.0.0.1:14503 --room "audio-${MODE}-${AUDIO}" \
    >/tmp/audio-view.log 2>&1 &
  VIEW=$!

  # 等视频解码 + 音频播放（最多 120s；AV1 等慢编码不在此场景）
  ok=0
  for _ in $(seq 1 240); do
    if grep -qE "DECODED: [1-9]" /tmp/audio-view.log 2>/dev/null \
       && grep -qE "played=[1-9]" /tmp/audio-view.log 2>/dev/null; then ok=1; break; fi
    if ! kill -0 "$PUB" 2>/dev/null || ! kill -0 "$VIEW" 2>/dev/null; then break; fi
    sleep 0.5
  done
  # 再等 10s 让 drift 收敛稳定后取最后一行统计
  sleep 10
  LAST=$(grep "AVSYNC:" /tmp/audio-view.log | tail -1)
  DECODED=$(echo "$LAST" | grep -oE "DECODED: [0-9]+" | grep -oE "[0-9]+" || echo 0)
  AFRAMES=$(echo "$LAST" | grep -oE "AUDIO: [0-9]+ frames" | grep -oE "[0-9]+" || echo 0)
  PLAYED=$(echo "$LAST" | grep -oE "played=[0-9]+" | cut -d= -f2 || echo 0)
  DRIFT=$(echo "$LAST" | grep -oE "drift=[-0-9.]+ms" | sed -E 's/drift=([-0-9.]+)ms/\1/' || echo 999)
  ABSDRIFT=$(python3 -c "print(f'{abs(float(\"$DRIFT\")):.0f}')")
  ERR=$(grep -ciE "panic|abort" /tmp/audio-sfu.log /tmp/audio-pub.log /tmp/audio-view.log 2>/dev/null | awk '{s+=$1} END{print s+0}')
  if [ "$ok" = "1" ] && [ "$ABSDRIFT" -le "$DRIFT_LIMIT" ] && [ "${AFRAMES:-0}" -gt 0 ] && [ "$ERR" = "0" ]; then
    echo "$MODE     $AUDIO   PASS     $DECODED          $AFRAMES         $PLAYED    ${DRIFT}     $ERR"
  else
    echo "$MODE     $AUDIO   FAIL     $DECODED          $AFRAMES         $PLAYED    ${DRIFT}     $ERR"
    echo "--- $MODE/$AUDIO view tail ---"; tail -6 /tmp/audio-view.log
    kill "$VIEW" "$PUB" "$SFU" "$SIG" 2>/dev/null || true
    wait 2>/dev/null || true
    exit 1
  fi
  kill "$VIEW" "$PUB" "$SFU" "$SIG" 2>/dev/null || true
  wait 2>/dev/null || true
done
done
echo "== SFU 音频转发 + 音视频同步 PASS（${MODES[*]} × ${AUDIOS[*]}）=="
