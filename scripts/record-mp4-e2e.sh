#!/usr/bin/env bash
# #234 SFU 录制容器化端到端：
#   SFU RECORD_DIR → publisher(H.264) 10s → SIGTERM finalize → ADREC2 落盘 →
#   aerodesk-rec2mp4 → MP4 → ffprobe 验证 H264/时长/帧数 → ffmpeg 解码 0 错误。
# 用法: scripts/record-mp4-e2e.sh
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

fail() { echo "FAIL: $*"; exit 1; }

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli
cargo build -q -p aerodesk-ffmpeg --bin aerodesk-rec2mp4
REC="$(mktemp -d)"
ROOM="rec-mp4-$(date +%s)"

echo "== 启动 SFU + signal（RECORD_DIR=${REC}）"
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

echo "== 发布端（vt H.264，10s）"
"$TARGET_DIR/aerodesk-cli" --role publisher --signal ws://127.0.0.1:14503 --room "$ROOM" \
  --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy \
  >/tmp/recmp4-pub.log 2>&1 &
PUB=$!
ok=0
for _ in $(seq 1 120); do
  grep -q "ICE connected" /tmp/recmp4-pub.log 2>/dev/null && ok=1 && break
  sleep 0.5
done
[ "$ok" = "1" ] || fail "publisher 未连上"
sleep 8
kill "$PUB" 2>/dev/null || true
sleep 1

echo "== finalize 录制（SIGTERM SFU）"
kill -TERM "$SFU" 2>/dev/null || true
for _ in $(seq 1 50); do
  kill -0 "$SFU" 2>/dev/null || break
  sleep 0.2
done
kill -9 "$SFU" 2>/dev/null || true
kill "$SIG" 2>/dev/null || true
wait 2>/dev/null || true

ADREC="$REC/${ROOM}.adrec"
[ -f "$ADREC" ] || fail "ADREC 未生成：${ADREC}（录目录：${REC}）"
head -c 7 "$ADREC" | grep -q "ADREC2" || fail "magic 非 ADREC2"
SIZE=$(stat -f %z "$ADREC")
echo "  ADREC2: $ADREC ($SIZE bytes)"

echo "== rec2mp4"
MP4="/tmp/recmp4-${ROOM}.mp4"
"$TARGET_DIR/aerodesk-rec2mp4" --input "$ADREC" --output "$MP4" >/tmp/recmp4-conv.log 2>&1 || { cat /tmp/recmp4-conv.log; fail "rec2mp4 失败"; }
cat /tmp/recmp4-conv.log

echo "== ffprobe 验证"
PROBE=$(ffprobe -v error -select_streams v:0 -show_entries stream=codec_name,nb_frames -show_entries format=duration -of default=noprint_wrappers=1 "$MP4" 2>&1)
echo "$PROBE"
echo "$PROBE" | grep -q "codec_name=h264" || fail "MP4 非 H264"
DUR=$(echo "$PROBE" | grep -oE "duration=[0-9.]+" | cut -d= -f2)
python3 -c "import sys; sys.exit(0 if float('$DUR') > 0.5 else 1)" || fail "时长异常: $DUR"
echo "  时长: ${DUR}s"

echo "== ffmpeg 解码校验（0 错误）"
FFOUT=$(ffmpeg -v error -i "$MP4" -f null - 2>&1)
echo "$FFOUT" | head -5
[ -z "$FFOUT" ] || fail "ffmpeg 解码报错：$FFOUT"
FRAMES=$(ffprobe -v error -select_streams v:0 -count_packets -show_entries stream=nb_read_packets -of default=noprint_wrappers=1 "$MP4" | grep -oE "nb_read_packets=[0-9]+" | cut -d= -f2)
echo "  ffmpeg 解码 0 错误（packets=${FRAMES}）"
[ "${FRAMES:-0}" -gt 0 ] || fail "MP4 无帧"

echo "== 录制容器化 M1 PASS（ADREC2 → MP4 可播放，${FRAMES} 包，${DUR}s）=="
