#!/usr/bin/env bash
# #234/#236 SFU 录制容器化端到端（四格式）：
#   每个 codec：SFU RECORD_DIR → publisher ~8s → SIGTERM finalize → ADREC2 →
#   aerodesk-rec2mp4（按 codec 自动选 mp4/webm）→ ffprobe 验证 codec/时长 →
#   ffmpeg 解码 0 错误。
# 用法: scripts/record-mp4-e2e.sh [codec...]   默认: h264 h265 vp9 av1
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

CODECS=("$@"); [ ${#CODECS[@]} -eq 0 ] && CODECS=(h264 h265 vp9 av1)
EXPECTED_CODEC=(h264 hevc vp9 av1)

fail() { echo "FAIL: $*"; exit 1; }

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli
cargo build -q -p aerodesk-ffmpeg --bin aerodesk-rec2mp4

echo "codec    container   result   ffprobe   frames   duration   errors"
for i in "${!CODECS[@]}"; do
  C="${CODECS[$i]}"; EXP="${EXPECTED_CODEC[$i]}"
  pkill -f 'aerodesk-(sfu|signal|cli)' 2>/dev/null || true
  sleep 1
  REC="$(mktemp -d)"
  RECORD_DIR="$REC" SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
    "$TARGET_DIR/aerodesk-sfu" >/tmp/recmp4-sfu.log 2>&1 &
  SFU=$!
  SIGNAL_PORT=14501 SIGNAL_PLAIN_PORT=14503 SFU_URL=http://127.0.0.1:14502 \
    "$TARGET_DIR/aerodesk-signal" >/tmp/recmp4-sig.log 2>&1 &
  SIG=$!
  for _ in $(seq 1 80); do
    nc -z 127.0.0.1 14502 2>/dev/null && nc -z 127.0.0.1 14503 2>/dev/null && break
    sleep 0.2
  done
  sleep 0.3

  ROOM="rec-${C}-$(date +%s)"
  ENC=vt
  [ "$C" != "h264" ] && ENC=ffmpeg
  "$TARGET_DIR/aerodesk-cli" --role publisher --signal ws://127.0.0.1:14503 --room "$ROOM" \
    --encoder "$ENC" --codec "$C" --noisy >/tmp/recmp4-pub.log 2>&1 &
  PUB=$!
  ok=0
  for _ in $(seq 1 120); do
    grep -q "ICE connected" /tmp/recmp4-pub.log 2>/dev/null && ok=1 && break
    sleep 0.5
  done
  [ "$ok" = "1" ] || fail "${C} publisher 未连上"
  sleep 6
  kill "$PUB" 2>/dev/null || true
  sleep 1

  kill -TERM "$SFU" 2>/dev/null || true
  for _ in $(seq 1 50); do
    kill -0 "$SFU" 2>/dev/null || break
    sleep 0.2
  done
  kill -9 "$SFU" 2>/dev/null || true
  kill "$SIG" 2>/dev/null || true
  wait 2>/dev/null || true

  ADREC="$REC/${ROOM}.adrec"
  [ -f "$ADREC" ] || fail "${C} ADREC 未生成"
  head -c 7 "$ADREC" | grep -q "ADREC2" || fail "${C} magic 非 ADREC2"
  EXT=mp4; [ "$C" = "vp9" -o "$C" = "av1" ] && EXT=webm
  MP4="/tmp/recmp4-${C}-${ROOM}.${EXT}"
  "$TARGET_DIR/aerodesk-rec2mp4" --input "$ADREC" --output "$MP4" >/tmp/recmp4-conv.log 2>&1 \
    || { cat /tmp/recmp4-conv.log; fail "${C} rec2mp4 失败"; }

  PROBE=$(ffprobe -v error -select_streams v:0 -show_entries stream=codec_name -show_entries format=duration \
    -of default=noprint_wrappers=1 "$MP4" 2>&1)
  ACTUAL=$(echo "$PROBE" | grep -oE "codec_name=[A-Za-z0-9]+" | cut -d= -f2)
  DUR=$(echo "$PROBE" | grep -oE "duration=[0-9.]+" | cut -d= -f2)
  FRAMES=$(ffprobe -v error -select_streams v:0 -count_packets -show_entries stream=nb_read_packets \
    -of default=noprint_wrappers=1 "$MP4" | grep -oE "nb_read_packets=[0-9]+" | cut -d= -f2)
  FFOUT=$(ffmpeg -v error -i "$MP4" -f null - 2>&1)
  if [ "$ACTUAL" = "$EXP" ] && [ -z "$FFOUT" ] && [ "${FRAMES:-0}" -gt 0 ]; then
    echo "$C       $EXT        PASS     $ACTUAL    ${FRAMES}     ${DUR}s     0"
  else
    echo "$C       $EXT        FAIL     $ACTUAL    ${FRAMES}     ${DUR}s     ${FFOUT:-?}"
    fail "${C} 转换/解码异常"
  fi
done
echo "== SFU 录制容器化 M1+M2 PASS（${CODECS[*]} → mp4/webm 均可播放）=="
