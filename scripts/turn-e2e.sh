#!/usr/bin/env bash
# turn-e2e.sh —— TURN relay 端到端（#191 SFU 内嵌 TURN server）：
#   默认内嵌模式：SFU 单进程提供 TURN+STUN（SFU_TURN_PORT），无 coturn。
#   可选 coturn 模式（TURN_MODE=coturn）：外部 coturn + SFU TURN_URLS 覆盖（兼容老部署）。
# 断言：
#   1) 自研客户端 ↔ TURN server 中继回环（core #[ignore] 测试）
#   2) webrtc-rs turn client 互操作（独立实现）
#   3) CLI 发布/观看端：allocate + relayed 候选 + ICE 连通
# 独立端口避免与本机其它 agent 冲突。
set -euo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"

TURN_MODE="${TURN_MODE:-embedded}"
# TURN_PROTO=udp|tcp|turns：信令只下发对应传输的 URL（native 客户端走对应 TURN 传输）
TURN_PROTO="${TURN_PROTO:-udp}"
TURN_PORT="${TURN_PORT:-14789}"
TURN_TLS_PORT="${TURN_TLS_PORT:-15349}"
TURN_SECRET="${TURN_SECRET:-testsecret}"
ROOM="turn-$(date +%s)"

echo "== 构建"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent -p aerodesk-core
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/debug"

TURN_PID=""
if [ "$TURN_MODE" = "coturn" ]; then
    echo "== coturn 模式：启动 coturn (127.0.0.1:$TURN_PORT)"
    command -v turnserver >/dev/null || { echo "FAIL: turnserver not found (brew install coturn)"; exit 1; }
    turnserver -n --use-auth-secret --static-auth-secret="$TURN_SECRET" \
      --realm=aerodesk.io --no-tls --no-dtls --fingerprint --allow-loopback-peers \
      --listening-port="$TURN_PORT" --listening-ip=127.0.0.1 \
      --min-port=49152 --max-port=49200 >/tmp/turn-e2e-turn.log 2>&1 &
    TURN_PID=$!
    for _ in $(seq 1 50); do
        if nc -z 127.0.0.1 "$TURN_PORT" 2>/dev/null; then break; fi
        sleep 0.2
    done
    sleep 0.3
    echo "PASS coturn 已启动"
    TURN_URLS_OVERRIDE="turn:127.0.0.1:$TURN_PORT?transport=udp"
else
    echo "== 内嵌模式：SFU 单进程提供 TURN+STUN（无 coturn）"
    TURN_URLS_OVERRIDE=""
fi

# turns 变体：生成带 IP SAN 的测试证书链（SFU 用 leaf 链，客户端用 CA 根）
TLS_CERT_FILE=""
TLS_KEY_FILE=""
if [ "$TURN_PROTO" = "turns" ]; then
    TMPTLS="$(mktemp -d)"
    openssl req -x509 -newkey rsa:2048 -nodes -keyout "$TMPTLS/ca-key.pem" -out "$TMPTLS/ca-cert.pem" -days 1 \
      -subj "/CN=Turn Test CA" -addext "basicConstraints=critical,CA:TRUE" 2>/dev/null
    openssl req -newkey rsa:2048 -nodes -keyout "$TMPTLS/leaf-key.pem" -out "$TMPTLS/leaf-csr.pem" -subj "/CN=127.0.0.1" 2>/dev/null
    printf "subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\n" > "$TMPTLS/leaf-ext.cnf"
    openssl x509 -req -in "$TMPTLS/leaf-csr.pem" -CA "$TMPTLS/ca-cert.pem" -CAkey "$TMPTLS/ca-key.pem" \
      -CAcreateserial -out "$TMPTLS/leaf-cert.pem" -days 1 -extfile "$TMPTLS/leaf-ext.cnf" 2>/dev/null
    cat "$TMPTLS/leaf-cert.pem" "$TMPTLS/ca-cert.pem" > "$TMPTLS/chain.pem"
    TLS_CERT_FILE="$TMPTLS/chain.pem"
    TLS_KEY_FILE="$TMPTLS/leaf-key.pem"
    export TURN_TLS_CA="$TMPTLS/ca-cert.pem"
    echo "PASS 生成 turns 测试证书链（CA + leaf IP:127.0.0.1）"
fi

echo "== 启动 SFU + signal"
REC="$(mktemp -d)"
RECORD_DIR="$REC" TURN_SECRET="$TURN_SECRET" \
  CERT_FILE="$TLS_CERT_FILE" KEY_FILE="$TLS_KEY_FILE" \
  TURN_URLS="$TURN_URLS_OVERRIDE" SFU_TURN_PORT="$TURN_PORT" \
  SFU_TURN_TLS_PORT="$TURN_TLS_PORT" \
  SFU_MEDIA_PORT=14578 SFU_SIGNAL_PORT=14500 SFU_INTERNAL_PORT=14502 \
  "$TARGET_DIR"/aerodesk-sfu >/tmp/turn-e2e-sfu.log 2>&1 &
echo $! > /tmp/turn-e2e-sfu.pid
case "$TURN_PROTO" in
  tcp)  SIG_TURN_URLS="turn:127.0.0.1:$TURN_PORT?transport=tcp" ;;
  turns) SIG_TURN_URLS="turns:127.0.0.1:$TURN_TLS_PORT?transport=tcp" ;;
  *)    SIG_TURN_URLS="turn:127.0.0.1:$TURN_PORT?transport=udp,turn:127.0.0.1:$TURN_PORT?transport=tcp,turns:127.0.0.1:$TURN_TLS_PORT?transport=tcp" ;;
esac
SIGNAL_PORT=14501 SFU_URL=http://127.0.0.1:14502 \
  TURN_SECRET="$TURN_SECRET" TURN_URLS="$SIG_TURN_URLS" \
  "$TARGET_DIR"/aerodesk-signal >/tmp/turn-e2e-sig.log 2>&1 &
echo $! > /tmp/turn-e2e-sig.pid
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 14502 2>/dev/null && nc -z 127.0.0.1 14503 2>/dev/null; then break; fi
    sleep 0.2
done
sleep 0.3
if [ "$TURN_MODE" = "embedded" ]; then
    grep -q 'TURN+STUN server UDP on' /tmp/turn-e2e-sfu.log || { echo "FAIL 内嵌 TURN(UDP) 未启动"; tail -5 /tmp/turn-e2e-sfu.log; exit 1; }
    grep -q 'TURN+STUN server TCP on' /tmp/turn-e2e-sfu.log || { echo "FAIL 内嵌 TURN(TCP) 未启动"; tail -5 /tmp/turn-e2e-sfu.log; exit 1; }
    grep -q 'TURN+STUN server TLS on' /tmp/turn-e2e-sfu.log || { echo "FAIL 内嵌 TURN(TLS) 未启动"; tail -5 /tmp/turn-e2e-sfu.log; exit 1; }
    echo "PASS SFU 内嵌 TURN server 已启动（UDP+TCP+TLS）"
else
    grep -q 'TURN relay configured' /tmp/turn-e2e-sfu.log || { echo "FAIL SFU 未下发 TURN 配置"; tail -5 /tmp/turn-e2e-sfu.log; exit 1; }
fi

echo "== 1) TURN 中继回环（core ignored test ↔ $TURN_MODE server）"
if TURN_E2E_SERVER="127.0.0.1:$TURN_PORT" TURN_E2E_SECRET="$TURN_SECRET" \
   cargo test -q -p aerodesk-core --lib -- --ignored real_coturn_relay_roundtrip >/tmp/turn-e2e-core.log 2>&1; then
    echo "PASS 中继回环（自研 client）"
else
    echo "FAIL 中继回环"; tail -20 /tmp/turn-e2e-core.log
    kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
    [ -n "$TURN_PID" ] && kill "$TURN_PID" 2>/dev/null || true
    exit 1
fi

echo "== 2) webrtc-rs turn client 互操作（独立实现 ↔ TURN server）"
if [ "$TURN_MODE" = "embedded" ]; then
    WEBRTC_RS="/tmp/turn-rs/target/debug/examples/turn_client_udp"
    if [ ! -x "$WEBRTC_RS" ]; then
        echo "SKIP: webrtc-rs example 未构建（cd /tmp/turn-rs && cargo build --example turn_client_udp）"
    else
        CRED=$(python3 -c "
import hmac,hashlib,base64,time
now=int(time.time())+3600
u=f'{now}:e2e'
p=base64.b64encode(hmac.new(b'$TURN_SECRET',u.encode(),hashlib.sha1).digest()).decode()
print(f'{u}={p}')
")
        if RUST_LOG=info "$WEBRTC_RS" --host 127.0.0.1 --port "$TURN_PORT" --user "$CRED" \
            --realm aerodesk.io --ping >/tmp/turn-e2e-webrtc.log 2>&1 \
            && grep -q 'relayed-address=' /tmp/turn-e2e-webrtc.log; then
            echo "PASS webrtc-rs 互操作（allocate + ping）"
        else
            echo "FAIL webrtc-rs 互操作"; tail -8 /tmp/turn-e2e-webrtc.log
            kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
            [ -n "$TURN_PID" ] && kill "$TURN_PID" 2>/dev/null || true
            exit 1
        fi
    fi
else
    echo "SKIP webrtc-rs（coturn 模式不要求）"
fi

echo "== 2b) TCP/TLS 互操作（Python 独立探针）"
if [ "$TURN_MODE" = "embedded" ]; then
    if python3 scripts/turn_tcp_probe.py 127.0.0.1 "$TURN_PORT" "$TURN_SECRET" >/tmp/turn-e2e-probe-tcp.log 2>&1        && grep -q 'RESULT: OK' /tmp/turn-e2e-probe-tcp.log; then
        echo "PASS TCP 互操作（allocate + relay 回环）"
    else
        echo "FAIL TCP 互操作"; tail -8 /tmp/turn-e2e-probe-tcp.log
        kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
        exit 1
    fi
    if [ "$TURN_PROTO" = "turns" ]; then
        echo "SKIP TLS 探针（turns 变体由 CLI 全链路覆盖）"
    elif python3 scripts/turn_tcp_probe.py 127.0.0.1 "$TURN_TLS_PORT" "$TURN_SECRET" --tls --tls-cert certs/cer.pem >/tmp/turn-e2e-probe-tls.log 2>&1        && grep -q 'RESULT: OK' /tmp/turn-e2e-probe-tls.log; then
        echo "PASS TLS 互操作（TLSv1.3 allocate + relay 回环）"
    else
        echo "FAIL TLS 互操作"; tail -8 /tmp/turn-e2e-probe-tls.log
        kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
        exit 1
    fi
else
    echo "SKIP TCP/TLS 探针（coturn 模式不要求）"
fi

# native 客户端 TLS（turns:）校验用 CA 根（turns 变体已 export 测试 CA）
if [ -z "${TURN_TLS_CA:-}" ]; then
    export TURN_TLS_CA="$PWD/certs/cer.pem"
fi
# #552 SIP：TURN 须本地配置（AERO_TURN_*）——REST 凭证（TURN_SECRET）与
# SFU 内嵌 TURN 一致（username=`<expiry>:<user>`，credential=base64(HMAC-SHA1)）。
TURN_USER="$(($(date +%s) + 3600)):turn-e2e"
TURN_CRED="$(python3 -c "import hmac,hashlib,base64; print(base64.b64encode(hmac.new(b'$TURN_SECRET', b'$TURN_USER', hashlib.sha1).digest()).decode())")"
echo "== 3a) 发布端（TURN_PROTO=${TURN_PROTO}）：allocate + relayed 候选 + ICE"
# #584：发布端同样需本地 AERO_TURN_* 配置（#552 SIP 化后 TURN 不随信令自动下发），
# 否则不发起 allocation，3b 的「发布端 TURN 接入」断言必失败（与 viewer 对称）。
AERO_TURN_URLS="$SIG_TURN_URLS" AERO_TURN_USERNAME="$TURN_USER" AERO_TURN_CREDENTIAL="$TURN_CRED" \
  "$TARGET_DIR"/aerodesk-agent --role publisher --encoder x264 --noisy \
  --signal ws://127.0.0.1:14503 --room "$ROOM" >/tmp/turn-e2e-pub.log 2>&1 &
PUB_PID=$!
# #552 SIP 1:1：publisher 等 IncomingCall——先等注册就绪，viewer（3b）起后再等 ICE。
ok=1
for _ in $(seq 1 30); do grep -q 'SIP registered' /tmp/turn-e2e-pub.log 2>/dev/null && ok=0 && break; sleep 0.3; done
if [ "$ok" -ne 0 ]; then
    echo "FAIL 发布端未完成 SIP 注册"; tail -8 /tmp/turn-e2e-pub.log
    kill "$PUB_PID" 2>/dev/null || true
    kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
    [ -n "$TURN_PID" ] && kill "$TURN_PID" 2>/dev/null || true
    exit 1
fi

echo "== 3b) 观看端（TURN_PROTO=${TURN_PROTO}）：allocate + relayed 候选 + ICE"
AERO_TURN_URLS="$SIG_TURN_URLS" AERO_TURN_USERNAME="$TURN_USER" AERO_TURN_CREDENTIAL="$TURN_CRED"   "$TARGET_DIR"/aerodesk-agent --role viewer --signal ws://127.0.0.1:14503 --room "$ROOM" >/tmp/turn-e2e-view.log 2>&1 &
VIEW_PID=$!
# 发布端 ICE（viewer 呼入后建链）——TURN allocation/relayed/SDP/ICE 全断言。
ok=1
for _ in $(seq 1 60); do
    if grep -q 'TURN allocation ok' /tmp/turn-e2e-pub.log 2>/dev/null \
       && grep -q 'relayed candidate' /tmp/turn-e2e-pub.log 2>/dev/null \
       && grep -q 'SDP negotiated' /tmp/turn-e2e-pub.log 2>/dev/null \
       && grep -q 'ICE connected' /tmp/turn-e2e-pub.log 2>/dev/null; then ok=0; break; fi
    sleep 0.3
done
if [ "$ok" -eq 0 ]; then
    echo "PASS 发布端 TURN 接入 + ICE 连通"
else
    # #584 恢复 FAIL（f3ff07e 曾降级 WARN）：TURN 链路回归保护。
    echo "FAIL 发布端未完成 TURN 接入"; tail -8 /tmp/turn-e2e-pub.log
    kill "$PUB_PID" 2>/dev/null || true
    kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
    [ -n "$TURN_PID" ] && kill "$TURN_PID" 2>/dev/null || true
    exit 1
fi

ok=1
for _ in $(seq 1 60); do
    if grep -q 'TURN allocation ok' /tmp/turn-e2e-view.log 2>/dev/null \
       && grep -q 'relayed candidate' /tmp/turn-e2e-view.log 2>/dev/null \
       && grep -q 'SDP negotiated' /tmp/turn-e2e-view.log 2>/dev/null \
       && grep -q 'ICE connected' /tmp/turn-e2e-view.log 2>/dev/null; then ok=0; break; fi
    sleep 0.3
done
if [ "$ok" -ne 0 ]; then
    # #584 恢复 FAIL（f3ff07e 曾降级 WARN）：TURN 链路回归保护。
    echo "FAIL 观看端未完成 TURN 接入"; tail -8 /tmp/turn-e2e-view.log
else
    echo "PASS 观看端 TURN 接入 + ICE 连通"
fi
# #552 SIP 1:1：publisher 单呼叫——3b 结束释放 view，3c 用独立房间 + 新 publisher。
kill "$VIEW_PID" 2>/dev/null || true
wait "$VIEW_PID" 2>/dev/null || true
ROOM_FR="${ROOM}-fr"

echo "== 3c) force-relay 观看端：只走 relayed 候选 + 媒体到达（#201）"
AERO_TURN_URLS="$SIG_TURN_URLS" AERO_TURN_USERNAME="$TURN_USER" AERO_TURN_CREDENTIAL="$TURN_CRED" \
  "$TARGET_DIR"/aerodesk-agent --role publisher --encoder x264 --noisy \
  --signal ws://127.0.0.1:14503 --room "$ROOM_FR" >/tmp/turn-e2e-pub-fr.log 2>&1 &
PUB_FR_PID=$!
sleep 2
AERO_TURN_URLS="$SIG_TURN_URLS" AERO_TURN_USERNAME="$TURN_USER" AERO_TURN_CREDENTIAL="$TURN_CRED" AERODESK_FORCE_RELAY=1 \
  "$TARGET_DIR"/aerodesk-agent --role viewer \
  --signal ws://127.0.0.1:14503 --room "$ROOM_FR" >/tmp/turn-e2e-view-fr.log 2>&1 &
VIEW_FR_PID=$!
ok=1
for _ in $(seq 1 90); do
    if grep -q 'relayed candidate' /tmp/turn-e2e-view-fr.log 2>/dev/null \
       && grep -q 'force-relay: skip host candidate' /tmp/turn-e2e-view-fr.log 2>/dev/null \
       && grep -q 'RECEIVED: [1-9]' /tmp/turn-e2e-view-fr.log 2>/dev/null; then ok=0; break; fi
    sleep 0.3
done
kill "$VIEW_FR_PID" 2>/dev/null || true
# #553：pub-fr（3c 发布端）等 IncomingCall 300s——不 kill 会让末尾 wait 拖满
# step 超时（15 分钟）——3c 结束即清理。
kill "$PUB_FR_PID" 2>/dev/null || true
if [ "$ok" -eq 0 ]; then
    echo "PASS force-relay 观看端媒体经 relay 到达"; grep -m1 'RECEIVED:' /tmp/turn-e2e-view-fr.log
else
    # #584 恢复 FAIL（f3ff07e 曾降级 WARN）：force-relay 回归保护。
    echo "FAIL force-relay 观看端媒体未到达"; tail -8 /tmp/turn-e2e-view-fr.log
    kill "$PUB_PID" 2>/dev/null || true
    kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
    [ -n "$TURN_PID" ] && kill "$TURN_PID" 2>/dev/null || true
    exit 1
fi

echo "== 3d) TURN allocation 指标（#220）：/metrics/prometheus 暴露活跃数"
BODY=$(curl -s --max-time 2 "http://127.0.0.1:14502/metrics/prometheus")
TA=$(echo "$BODY" | awk '/^aerodesk_sfu_turn_allocations [0-9]+$/{v=$2} END{print v+0}')
TAT=$(echo "$BODY" | awk '/^aerodesk_sfu_turn_allocations_total [0-9]+$/{v=$2} END{print v+0}')
if [ "$TA" -ge 3 ] && [ "$TAT" -ge 3 ]; then
    echo "PASS TURN allocation 指标 active=${TA} total=${TAT}（3 客户端均已分配）"
else
    echo "FAIL TURN allocation 指标 active=${TA} total=${TAT}（预期 >=3）"
    kill "$PUB_PID" "$VIEW_PID" 2>/dev/null || true
    kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
    [ -n "$TURN_PID" ] && kill "$TURN_PID" 2>/dev/null || true
    exit 1
fi

kill "$PUB_PID" "$VIEW_PID" 2>/dev/null || true
kill "$(cat /tmp/turn-e2e-sfu.pid)" "$(cat /tmp/turn-e2e-sig.pid)" 2>/dev/null || true
[ -n "$TURN_PID" ] && kill "$TURN_PID" 2>/dev/null || true
wait "$VIEW_FR_PID" "$PUB_FR_PID" 2>/dev/null || true
echo "E2E DONE"
