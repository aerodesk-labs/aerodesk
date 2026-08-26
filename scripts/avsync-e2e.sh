#!/usr/bin/env bash
# #73 A/V 同步链路：publisher（视频+音频）→ SFU → viewer，AVSYNC 统计（漂移/jitter）。
# 真实播放需 macOS 音频设备，CI 验证同步机制与统计。
# 用法: scripts/avsync-e2e.sh [房间] [观察秒数]
set -euo pipefail
cd "$(dirname "$0")/.."

ROOM="${1:-avsync-$(date +%s)}"
OBS="${2:-6}"
export RUST_LOG="${RUST_LOG:-info}"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent

REC="$(mktemp -d)"
echo "== 启动 sfu/signal"
RECORD_DIR="$REC" ./target/debug/aerodesk-sfu >/tmp/av-sfu.log 2>&1 &
SFU_PID=$!
SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/av-sig.log 2>&1 &
SIG_PID=$!
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 3003 2>/dev/null && nc -z 127.0.0.1 3002 2>/dev/null; then break; fi
    if ! kill -0 "$SFU_PID" 2>/dev/null || ! kill -0 "$SIG_PID" 2>/dev/null; then
        echo "sfu/signal 启动失败"; tail -5 /tmp/av-sfu.log; tail -5 /tmp/av-sig.log; exit 1
    fi
    sleep 0.2
done
sleep 0.3

echo "== 启动 publisher（视频 + --audio），等 SIP 注册就绪后 viewer（--audio）"
# 连续视频源（x264 合成，避免 pcap 48 帧发完导致漂移统计假象）
./target/debug/aerodesk-agent --role publisher --encoder x264 --audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/av-pub.log 2>&1 &
PUB_PID=$!
# #552 SIP 1:1：viewer 须在 publisher 注册完成后才 INVITE（否则 lookup 未命中
# 走会议桥 SFU——同 linux-native 竞态）；同时避免注册期音频 0fps 饥饿被
# drift 判为"真失同步"（#523 v3 健康窗逻辑对饥饿-突发恢复模式误判，实测
# 71fps 追赶窗 FAIL）——轮询注册就绪（≤15s）。
OK=0
for _ in $(seq 1 30); do
    if grep -q "SIP registered" /tmp/av-pub.log 2>/dev/null; then OK=1; break; fi
    sleep 0.5
done
if [ "$OK" != "1" ]; then
    echo "FAIL: publisher 未完成 SIP 注册"; tail -8 /tmp/av-pub.log
    kill "$PUB_PID" 2>/dev/null || true
    exit 1
fi
./target/debug/aerodesk-agent --role viewer --audio \
    --signal ws://127.0.0.1:3003 --room "$ROOM" >/tmp/av-view.log 2>&1 &
VIEW_PID=$!
# 先等媒体到达（CI 慢启动时固定 sleep 会误判 0 帧；最多 ~30s）
MEDIA_OK=0
for _ in $(seq 1 60); do
    if grep -qE "RECEIVED: [1-9]" /tmp/av-view.log 2>/dev/null; then MEDIA_OK=1; break; fi
    if ! kill -0 "$VIEW_PID" 2>/dev/null; then break; fi
    sleep 0.5
done
# 漂移采样:last 3 次 AVSYNC 打印的 drift 值（毫秒，换行分隔）
sample_drift() {
    grep -oE 'drift=[-0-9.]+ms' /tmp/av-view.log 2>/dev/null | sed 's/drift=//; s/ms//' | tail -3
}
# 音频到达计数采样（AUDIO: N frames 的末次值）
sample_audio_frames() {
    # 音频帧未打印（慢启动/饥饿）时首个 grep 无匹配返回 1——顶层调用处（audio_prev/
    # audio_now 赋值）会被 pipefail+set -e 误杀；|| true 兜底，调用方再按 :-0 归一。
    grep -oE 'AUDIO: [0-9]+ frames' /tmp/av-view.log 2>/dev/null | tail -1 | grep -oE '[0-9]+' | head -1 || true
}
# 漂移判定：有界（±3000ms）且相邻变化 ≤500ms。0=稳定，1=超差/样本不足
drift_stable() {
    local drifts last prev
    drifts=$(sample_drift)
    last=$(echo "$drifts" | tail -1)
    prev=$(echo "$drifts" | tail -2 | head -1)
    [ -n "$last" ] && [ -n "$prev" ] && \
        awk -v a="$last" -v b="$prev" 'BEGIN { exit !(a >= -3000 && a <= 3000 && (a-b) >= -500 && (a-b) <= 500) }'
}
# #523 v3：按「音频到达速率」区分断症——drift 用的是接收侧 RTP 时间戳，
# 共享 runner 负载下音频到达被持续饥饿时 audio_time 停摆、drift 单调恶化
# （buffered 近空/played 低于实时速率），判漂移必然误杀；映射类真 bug 则
# 到达健康（≈50fps，20ms 帧）而漂移照样行进。判据：到达 <40fps = 饥饿环境，
# 容忍再加窗（封顶 3 窗）；到达健康而漂移超差 = 真失同步，立即 FAIL。
audio_prev=$(sample_audio_frames)
audio_prev=${audio_prev:-0}
window=0
max_windows=3
starved_all=1   # 全部观察窗均处饥饿（到达 <40fps）= 1；任一健康窗即清 0
while true; do
    sleep "$OBS"
    window=$((window + 1))
    if drift_stable; then break; fi
    audio_now=$(sample_audio_frames)
    audio_now=${audio_now:-0}
    rate=$(( (audio_now - audio_prev) / OBS ))
    audio_prev=$audio_now
    if [ "$rate" -ge 40 ]; then starved_all=0; fi
    if [ "$window" -ge "$max_windows" ]; then
        echo "== drift 第 ${window} 窗仍超差（$(sample_drift | tr '\n' ' ')），到达 ${rate}fps，窗口用尽"
        break
    fi
    if [ "$rate" -lt 40 ]; then
        echo "== drift 第 ${window} 窗超差（$(sample_drift | tr '\n' ' ')），音频到达 ${rate}fps <40 = 负载饥饿，加时 ${OBS}s 复测"
    else
        echo "== drift 第 ${window} 窗超差（$(sample_drift | tr '\n' ' ')），音频到达 ${rate}fps 健康 = 真失同步嫌疑，不再加窗"
        break
    fi
done
kill "$PUB_PID" "$VIEW_PID" "$SFU_PID" "$SIG_PID" 2>/dev/null || true
wait 2>/dev/null || true

echo "== 断言"
fail=0
# 1) 视频与音频都收到（媒体等待轮询保证到达，避免 CI 慢启动误判）
if [ "$MEDIA_OK" = "1" ] && grep -qE "AUDIO: [1-9]" /tmp/av-view.log; then
    echo "PASS video+audio received"
else
    echo "FAIL media receive"; tail -3 /tmp/av-view.log; fail=1
fi
# 2) AVSYNC 统计出现（音频/视频时间轴 + 漂移）
if grep -q "AVSYNC:" /tmp/av-view.log; then
    echo "PASS AVSYNC stats"
else
    echo "FAIL AVSYNC"; tail -3 /tmp/av-view.log; fail=1
fi
# 3) 漂移稳定（相邻两次变化 < 500ms）且有界（±3000ms）。
# 首帧到达时差会造成固定偏移（编码启动/转发延迟），但不应持续漂移。
# 采样在观察窗循环之后进行（#523 v3），判定逻辑与 drift_stable 单一来源。
# v3.1：若所有观察窗音频到达均饥饿（runner 持续满载，实测 33-39fps×18s），
# drift（接收侧时间戳）必然恶化——此时断言降级为 SKIP 并明示未验证；
# 任一健康窗漂移超差才 FAIL（映射类真 bug 在健康窗照样显形）。
if drift_stable; then
    DRIFTS=$(sample_drift)
    LAST=$(echo "$DRIFTS" | tail -1)
    PREV=$(echo "$DRIFTS" | tail -2 | head -1)
    echo "PASS drift stable (${PREV} -> ${LAST}ms)"
elif [ "$starved_all" = "1" ]; then
    echo "SKIP drift（全部 $max_windows 窗音频到达饥饿，runner 满载，本轮未验证漂移；媒体接收/jitter/panic 断言仍生效）"
else
    echo "FAIL drift unstable: $(sample_drift | tr '\n' ' ')"; tail -3 /tmp/av-view.log; fail=1
fi
# 4) jitter buffer 工作（播放计数 > 0）
if grep -qE "played=[1-9]" /tmp/av-view.log; then
    echo "PASS jitter buffer played"
else
    echo "FAIL jitter played"; tail -3 /tmp/av-view.log; fail=1
fi
# 5) 无 panic
if grep -qiE "panic" /tmp/av-pub.log /tmp/av-view.log /tmp/av-sfu.log; then
    echo "FAIL panic in logs"; fail=1
else
    echo "PASS no panics"
fi
exit $fail
