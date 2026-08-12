#!/usr/bin/env bash
# #216 M1：跨 PoP 媒体桥接端到端（本地双 SFU 模拟双 PoP）。
#   PoP-A（14600 系）：CLI publisher 发布 room → bridge（view PoP-A + publish PoP-B）
#   → PoP-B（14700 系）：CLI viewer 加入同 room → 断言不经 Redirect 收到 PoP-A 媒体
#   （RECEIVED/DECODED > 0），bridge 原样重打包（无重编码：不链接任何编码器）。
# 断言：A/B 各 2 客户端；bridge forwarded>0 且含关键帧；无 panic。
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

ROOM="bridge-$(date +%s)"
# PoP-A
SIG_A=14600; INT_A=14602; PLAIN_A=14603; MEDIA_A=14678
# PoP-B
SIG_B=14700; INT_B=14702; PLAIN_B=14703; MEDIA_B=14778

fail() { echo "FAIL: $*"; exit 1; }
cleanup() {
  pkill -f 'aerodesk-bridge' 2>/dev/null || true
  pkill -f 'aerodesk-cli' 2>/dev/null || true
  [ -n "${SFU_A:-}" ] && kill "$SFU_A" 2>/dev/null || true
  [ -n "${SFU_B:-}" ] && kill "$SFU_B" 2>/dev/null || true
  [ -n "${SIG_A_PID:-}" ] && kill "$SIG_A_PID" 2>/dev/null || true
  [ -n "${SIG_B_PID:-}" ] && kill "$SIG_B_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli -p aerodesk-bridge
REC_A="$(mktemp -d)"; REC_B="$(mktemp -d)"

echo "== 启动 PoP-A（14600 系）+ PoP-B（14700 系）"
RECORD_DIR="$REC_A" SFU_MEDIA_PORT="$MEDIA_A" SFU_SIGNAL_PORT="$SIG_A" SFU_INTERNAL_PORT="$INT_A" \
  "$TARGET_DIR/aerodesk-sfu" >/tmp/bridge-sfu-a.log 2>&1 &
SFU_A=$!
SIGNAL_PORT=14601 SIGNAL_PLAIN_PORT="$PLAIN_A" SFU_URL="http://127.0.0.1:${INT_A}" \
  "$TARGET_DIR/aerodesk-signal" >/tmp/bridge-sig-a.log 2>&1 &
SIG_A_PID=$!
RECORD_DIR="$REC_B" SFU_MEDIA_PORT="$MEDIA_B" SFU_SIGNAL_PORT="$SIG_B" SFU_INTERNAL_PORT="$INT_B" \
  "$TARGET_DIR/aerodesk-sfu" >/tmp/bridge-sfu-b.log 2>&1 &
SFU_B=$!
SIGNAL_PORT=14701 SIGNAL_PLAIN_PORT="$PLAIN_B" SFU_URL="http://127.0.0.1:${INT_B}" \
  "$TARGET_DIR/aerodesk-signal" >/tmp/bridge-sig-b.log 2>&1 &
SIG_B_PID=$!
for _ in $(seq 1 80); do
  nc -z 127.0.0.1 "$INT_A" 2>/dev/null && nc -z 127.0.0.1 "$INT_B" 2>/dev/null && break
  sleep 0.2
done
sleep 0.3

echo "== PoP-A：启动 publisher（room=${ROOM}）"
"$TARGET_DIR/aerodesk-cli" --role publisher --signal "ws://127.0.0.1:${PLAIN_A}" --room "$ROOM" \
  --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy \
  >/tmp/bridge-pub-a.log 2>&1 &
PUB_A=$!
ok=0
for _ in $(seq 1 120); do
  grep -q "ICE connected" /tmp/bridge-pub-a.log 2>/dev/null && ok=1 && break
  sleep 0.5
done
[ "$ok" = "1" ] || fail "PoP-A publisher 未连上"; echo "  publisher connected"

echo "== 启动 bridge（view PoP-A + publish PoP-B）"
"$TARGET_DIR/aerodesk-bridge" --remote-signal "ws://127.0.0.1:${PLAIN_A}" \
  --local-signal "ws://127.0.0.1:${PLAIN_B}" --room "$ROOM" --codec h264 \
  >/tmp/bridge.log 2>&1 &
BRIDGE=$!
ok=0
for _ in $(seq 1 120); do
  grep -q "publisher leg:" /tmp/bridge.log 2>/dev/null && ok=1 && break
  sleep 0.5
done
[ "$ok" = "1" ] || fail "bridge 未连上双腿"; echo "  bridge 双腿已连"

echo "== PoP-B：启动 viewer（加入同 room，期望不经 Redirect 看到 PoP-A 媒体）"
"$TARGET_DIR/aerodesk-cli" --role viewer --signal "ws://127.0.0.1:${PLAIN_B}" --room "$ROOM" \
  >/tmp/bridge-view-b.log 2>&1 &
VIEW_B=$!
ok=0
for _ in $(seq 1 180); do
  if grep -qE "DECODED: [1-9]" /tmp/bridge-view-b.log 2>/dev/null; then ok=1; break; fi
  sleep 0.5
done
[ "$ok" = "1" ] || fail "PoP-B viewer 未解码到跨 PoP 媒体（见 /tmp/bridge-view-b.log）"
FRAMES=$(grep -oE "RECEIVED: [0-9]+ frames" /tmp/bridge-view-b.log | tail -1)
DECODED=$(grep -oE "DECODED: [0-9]+" /tmp/bridge-view-b.log | tail -1)
echo "  viewer 跨 PoP 收流: ${FRAMES}, ${DECODED}"

echo "== 断言"
# bridge 转发计数（stats 每 5s 一行；等它出现，关键帧数用即时日志兜底）
ok=0
for _ in $(seq 1 30); do
  if grep -q "bridge stats:" /tmp/bridge.log 2>/dev/null; then ok=1; break; fi
  sleep 0.5
done
[ "$ok" = "1" ] || fail "bridge 未输出 stats（转发异常）"
FWD=$(grep -oE "forwarded=[0-9]+" /tmp/bridge.log | tail -1 | cut -d= -f2)
KF_STATS=$(grep -oE "forwarded_kf=[0-9]+" /tmp/bridge.log | tail -1 | cut -d= -f2)
KF_CNT=$(grep -c "forwarded keyframe" /tmp/bridge.log 2>/dev/null || echo 0)
KF=$((KF_STATS > KF_CNT ? KF_STATS : KF_CNT))
echo "  bridge: forwarded=${FWD} forwarded_kf=${KF}（stats=${KF_STATS} log=${KF_CNT}）"
[ "${FWD:-0}" -gt 0 ] || fail "bridge 未转发任何媒体"
[ "${KF:-0}" -ge 1 ] || fail "bridge 未转发关键帧（远端 viewer 无法起流）"

# 双 SFU 客户端数
clients_of() { curl -s --max-time 2 "http://127.0.0.1:$1/metrics/prometheus" | awk '/^aerodesk_sfu_clients [0-9]+$/{v=$2} END{print v+0}'; }
for _ in $(seq 1 20); do
  CA=$(clients_of "$INT_A"); CB=$(clients_of "$INT_B")
  [ "$CA" -ge 2 ] && [ "$CB" -ge 2 ] && break
  sleep 0.5
done
echo "  PoP-A clients=${CA}（预期 2: publisher+bridge-view） PoP-B clients=${CB}（预期 2: bridge-pub+viewer）"
[ "${CA:-0}" -ge 2 ] || fail "PoP-A clients=${CA}"
[ "${CB:-0}" -ge 2 ] || fail "PoP-B clients=${CB}"

echo "== M2：data channel 桥（input 至少）"
# PoP-B viewer 默认周期性发合成输入 → SFU-B → bridge → SFU-A → PoP-A publisher
ok=0
for _ in $(seq 1 60); do
  if grep -q "inject:" /tmp/bridge-pub-a.log 2>/dev/null; then ok=1; break; fi
  sleep 0.5
done
[ "$ok" = "1" ] || fail "PoP-A publisher 未收到跨 PoP input（data channel 桥未通）"
echo "  PoP-A publisher 收到 input: $(grep -m1 'inject:' /tmp/bridge-pub-a.log | sed 's/.*inject:/inject:/')"
DF=$(grep -oE "data_forwarded=[0-9]+" /tmp/bridge.log | tail -1 | cut -d= -f2)
echo "  bridge data_forwarded=${DF}"
[ "${DF:-0}" -gt 0 ] || fail "bridge 未转发 data channel"

echo "== M3（文件）：跨 PoP 文件传输（sha256 一致性）"
kill "$PUB_A" "$VIEW_B" 2>/dev/null || true
sleep 1
FILESIZE_KB=512
SRC_FILE="$(mktemp -d)/send-${FILESIZE_KB}kb.bin"
OUT_DIR="$(mktemp -d)/out"; mkdir -p "$OUT_DIR"
dd if=/dev/urandom of="$SRC_FILE" bs=1024 count="$FILESIZE_KB" 2>/dev/null
SRC_HASH=$(shasum -a 256 "$SRC_FILE" | awk '{print $1}')
# PoP-A publisher 收（--recv-dir），PoP-B viewer 发（--send-file）
"$TARGET_DIR/aerodesk-cli" --role publisher --recv-dir "$OUT_DIR" \
  --signal "ws://127.0.0.1:${PLAIN_A}" --room "$ROOM" \
  --encoder vt --width 1280 --height 720 --fps 30 --bitrate 2000000 --noisy \
  >/tmp/bridge-pub-a.log 2>&1 &
PUB_A=$!
"$TARGET_DIR/aerodesk-cli" --role viewer --send-file "$SRC_FILE" \
  --signal "ws://127.0.0.1:${PLAIN_B}" --room "$ROOM" \
  >/tmp/bridge-view-b.log 2>&1 &
VIEW_B=$!
OUT_FILE="$OUT_DIR/$(basename "$SRC_FILE")"
ok=0
for _ in $(seq 1 600); do
  if grep -q "file receive complete" /tmp/bridge-pub-a.log 2>/dev/null && [ -f "$OUT_FILE" ]; then ok=1; break; fi
  if ! kill -0 "$PUB_A" 2>/dev/null || ! kill -0 "$VIEW_B" 2>/dev/null; then break; fi
  sleep 0.5
done
[ "$ok" = "1" ] || fail "跨 PoP 文件接收未完成（见 /tmp/bridge-pub-a.log /tmp/bridge-view-b.log）"
OUT_HASH=$(shasum -a 256 "$OUT_FILE" | awk '{print $1}')
[ "$OUT_HASH" = "$SRC_HASH" ] || fail "sha256 不一致 src=${SRC_HASH} out=${OUT_HASH}"
echo "  跨 PoP 文件传输 ${FILESIZE_KB}KB: sha256 一致（${SRC_HASH:0:16}…）"
DF2=$(grep -oE "data_forwarded=[0-9]+" /tmp/bridge.log | tail -1 | cut -d= -f2)
echo "  bridge data_forwarded 累计=${DF2}（含 file 块）"

# 无 panic/abort
grep -qiE "panic|abort" /tmp/bridge.log /tmp/bridge-sfu-a.log /tmp/bridge-sfu-b.log \
  /tmp/bridge-pub-a.log /tmp/bridge-view-b.log && fail "发现 panic/abort"

kill "$VIEW_B" "$BRIDGE" "$PUB_A" 2>/dev/null || true
sleep 1
echo "== 跨 PoP 桥接 M1+M2+M3 PASS（媒体 + input + 文件 sha256 一致；bridge 原样转发 keyframe=${KF}）=="
