#!/usr/bin/env bash
# record-mp4-e2e.sh —— SFU 录制转封装 e2e（#266）：
# ADREC2 → rec2mp4（按访问单元聚合）→ MP4，断言：
#   1) 帧数 < 视频 NAL 总数（按 NAL 写 sample 会放大帧数）
#   2) 帧数 ≈ 录制墙钟时长 × 30fps（±40%）
# 依赖：ffmpeg/ffprobe（CI 已有）。
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="rec-mp4-$(date +%s)"
REC="$(mktemp -d)"
FAIL=0

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

echo "== 启动 sfu（录制）+ signal（独立端口）"
RECORD_DIR="$REC" SFU_MEDIA_PORT=1478 SFU_SIGNAL_PORT=14000 SFU_INTERNAL_PORT=14002 \
  ./target/debug/aerodesk-sfu >/tmp/recmp4-sfu.log 2>&1 &
SFU=$!
SIGNAL_PORT=14001 SIGNAL_PLAIN_PORT=14003 SFU_URL=http://127.0.0.1:14002 \
  ./target/debug/aerodesk-signal >/tmp/recmp4-sig.log 2>&1 &
SIG=$!
trap 'kill $SFU $SIG 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do
  if nc -z 127.0.0.1 14002 2>/dev/null && nc -z 127.0.0.1 14003 2>/dev/null; then break; fi
  sleep 0.2
done
sleep 0.3

check_codec() {
  local codec="$1"  # h264 | h265
  # 每 codec 独立房间，避免包追加进同一 .adrec（混合 codec 干扰断言）。
  local room="rec-mp4-$codec-$(date +%s)"
  echo "=== codec=$codec room=$room"
  ./target/debug/aerodesk-cli --role publisher --encoder ffmpeg --codec "$codec" \
    --signal ws://127.0.0.1:14003 --room "$room" >/tmp/recmp4-pub.log 2>&1 &
  local PUB=$!
  sleep 5
  kill "$PUB" 2>/dev/null || true; wait "$PUB" 2>/dev/null || true
  sleep 1
  local ADREC="$REC/$room.adrec"
  [ -s "$ADREC" ] || { echo "FAIL: $codec adrec 为空"; FAIL=1; return; }

  # 视频包数 + 视频 NAL 总数（聚合后帧数应 < NAL 总数：SPS/PPS/SEI/AUD 不是帧）。
  local packets nals wall_dur
  read -r packets nals wall_dur < <(python3 - "$ADREC" <<'PYEOF'
import struct, sys
d = open(sys.argv[1],'rb').read()
assert d[:7] == b"ADREC2\n"
i, packets, nals = 7, 0, 0
first_wall = last_wall = None
while i + 24 <= len(d):
    kind, codec = d[i], d[i+1]
    wall = struct.unpack_from('<Q', d, i+4)[0]
    ln = struct.unpack_from('<I', d, i+20)[0]
    payload = d[i+24:i+24+ln]
    i += 24 + ln
    if kind != 0:
        continue
    packets += 1
    if first_wall is None:
        first_wall = wall
    last_wall = wall
    j = 0
    while j + 3 <= len(payload):
        if payload[j:j+3] == b'\x00\x00\x01' and (j+3 == len(payload) or payload[j+3] != 0):
            nals += 1
            j += 3
        else:
            j += 1
wall_dur = max(0.001, ((last_wall or 0) - (first_wall or 0)) / 1e6)
print(packets, nals, f"{wall_dur:.3f}")
PYEOF
)
  echo "  video packets=$packets nals=$nals wall_dur=${wall_dur}s"

  local OUT="$REC/$ROOM-$codec.mp4"
  ./target/debug/rec2mp4 "$ADREC" "$OUT" 2>/tmp/recmp4-conv.log || { echo "FAIL: rec2mp4 $codec"; tail -3 /tmp/recmp4-conv.log; FAIL=1; return; }
  cat /tmp/recmp4-conv.log
  [ -s "$OUT" ] || { echo "FAIL: mp4 为空"; FAIL=1; return; }

  local frames
  frames=$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$OUT" 2>/dev/null || echo 0)
  echo "  mp4: frames=$frames"
  [ "${frames:-0}" -gt 0 ] || { echo "FAIL: $codec 帧数为 0"; FAIL=1; return; }

  # 帧数 < NAL 总数：按 NAL 写 sample 时 frames≈nals（放大），聚合后应明显更小。
  if [ "$frames" -ge "$nals" ]; then
    echo "FAIL: $codec 帧数($frames) >= NAL 总数($nals)——未按访问单元聚合"
    FAIL=1; return
  fi

  # 帧数 ≈ 录制墙钟时长×30fps（±40%；合成源稳定 30fps）。
  local expect lo hi
  expect=$(python3 -c "print(int(${wall_dur:-0} * 30))")
  lo=$((expect * 60 / 100)); hi=$((expect * 140 / 100 + 1))
  if [ "$frames" -lt "$lo" ] || [ "$frames" -gt "$hi" ]; then
    echo "FAIL: $codec 帧数 $frames 不在预期 [$lo,$hi]（墙钟 ${wall_dur}s × 30fps）"
    FAIL=1; return
  fi
  echo "PASS codec=$codec frames=$frames ≈ ${wall_dur}s×30fps（nals=${nals} packets=${packets}）"
}

check_codec h264
check_codec h265

if [ "$FAIL" = "0" ]; then
  echo "RECORD-MP4 E2E PASS"
else
  echo "RECORD-MP4 E2E FAIL"; exit 1
fi
