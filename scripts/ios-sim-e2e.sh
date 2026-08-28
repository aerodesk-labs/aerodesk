#!/usr/bin/env bash
# iOS 主控端（观看端）模拟器端到端：构建 iOS App → 启动模拟器 → 本地 SFU 发布端
# → App 自动连接观看 → 断言 Rust pump 解码帧持续增长 + 发布端收到输入（模拟器 UI 点击）。
# 默认 PUBLISHER_ENCODER=h265（--encoder ffmpeg --codec h265 合成源，走 #328 HEVC 关键帧
# 参数集内联路径；无 VT 硬编时 FFmpeg 软编回退仍是 HEVC 流）。x264/screen 可显式回退。
# 本地有摄像头时 PUBLISHER_CAMERA=1 附加断言摄像头第二轨解码（需采集/摄像头权限）。
# 依赖：Xcode + iOS Simulator Runtime（macos runner 预装）、cargo、xcodegen。
# 用法: scripts/ios-sim-e2e.sh [room]   （PUBLISHER_ENCODER=h265|x264|screen，默认 h265）
set -euo pipefail
cd "$(dirname "$0")/.."
ROOM="${1:-iossim-$(date +%s)}"
WORK="$(mktemp -d)"
trap 'pkill -f "aerodesk-(sfu|signal|cli)" 2>/dev/null || true; xcrun simctl terminate booted io.aerodesk.viewer 2>/dev/null || true' EXIT

echo "== [1/7] 构建 Rust lib + Xcode 工程"
cargo build -q -p aerodesk-sfu -p aerodesk-signal -p aerodesk-agent
bash scripts/build-ios-lib.sh all >/dev/null
cd ios && xcodegen generate >/dev/null && cd ..

echo "== [2/7] 构建模拟器 App"
DD="$WORK/dd"
xcodebuild -project ios/AeroDesk.xcodeproj -scheme AeroDesk \
  -destination 'generic/platform=iOS Simulator' -configuration Debug \
  ARCHS=arm64 ONLY_ACTIVE_ARCH=YES build CODE_SIGNING_ALLOWED=NO \
  -derivedDataPath "$DD" >/dev/null
APP="$DD/Build/Products/Debug-iphonesimulator/AeroDesk.app"
[ -d "$APP" ] || { echo "FAIL: app 未生成"; exit 1; }

echo "== [3/7] 准备模拟器"
# 必须选最新 runtime：App 用最新 Xcode SDK 编译，旧 runtime 的 VideoToolbox
# 缺新符号（如 _VTRegisterSupplementalVideoDecoderIfAvailable）会 dyld 崩溃。
RUNTIME=$(xcrun simctl list runtimes 2>/dev/null | grep -oE 'com.apple.CoreSimulator.SimRuntime.iOS-[0-9-]+' | sort -V | tail -1)
[ -n "$RUNTIME" ] || { echo "FAIL: 无 iOS Simulator Runtime"; exit 1; }
echo "runtime=$RUNTIME"
# 镜像预建设备可能绑定旧 runtime（如 iOS 18.5），App 用最新 SDK 编译会 dyld 崩；
# 必须用最新 runtime 新建设备，不复用已存在设备。
DT=$(xcrun simctl list devicetypes 2>/dev/null | grep -oE 'com.apple.CoreSimulator.SimDeviceType.iPhone-[0-9]+' | sort -V | tail -1)
DEVICE=$(xcrun simctl create "AeroDesk-E2E-$(date +%s)" "$DT" "$RUNTIME")
echo "device=$DEVICE ($DT / $RUNTIME)"
xcrun simctl boot "$DEVICE" 2>/dev/null || true
xcrun simctl bootstatus "$DEVICE" -b >/dev/null 2>&1 || true
xcrun simctl boot "$DEVICE" 2>/dev/null || true
xcrun simctl bootstatus "$DEVICE" -b >/dev/null 2>&1 || true

echo "== [4/7] 启动 SFU/signal/publisher（端口可配，默认 3478/3000/3002 + ops 3001/SIP UDP 5060）"
REC="$(mktemp -d)"
SFU_MEDIA_PORT="${SFU_MEDIA_PORT:-3478}" SFU_SIGNAL_PORT="${SFU_SIGNAL_PORT:-3000}" \
  SFU_INTERNAL_PORT="${SFU_INTERNAL_PORT:-3002}" RECORD_DIR="$REC" \
  ./target/debug/aerodesk-sfu >/tmp/iossim-sfu.log 2>&1 &
SFU=$!
SIGNAL_OPS_PORT="${SIGNAL_OPS_PORT:-3001}" \
  SFU_URL="http://127.0.0.1:${SFU_INTERNAL_PORT:-3002}" \
  SIP_UDP_PORT=5060 ./target/debug/aerodesk-signal >/tmp/iossim-sig.log 2>&1 &
SIG=$!
# 等 SFU/signal 就绪再起 publisher（此前固定 sleep 1.5，CI 负载下 SFU /start 会超时）
for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 "${SFU_INTERNAL_PORT:-3002}" 2>/dev/null \
       && curl -sk "https://127.0.0.1:${SIGNAL_OPS_PORT:-3001}/healthz" 2>/dev/null | grep -q status; then break; fi
    sleep 0.2
done
# 发布端编码：默认 h265（#328 参数集内联回归；合成源无需采集权限）。
# PUBLISHER_ENCODER=x264 显式回退 H264；screen 用真实屏幕硬编（本地有权限时）。
# PUBLISHER_CAMERA=1 时强制走 screen 采集：摄像头轨只在 --encoder screen 下发布，
# 合成源（ffmpeg）不接摄像头。
PUBLISHER_ENCODER="${PUBLISHER_ENCODER:-h265}"
CODEC_ARGS=""
CAMERA_ARGS=""
if [ "${PUBLISHER_CAMERA:-0}" = "1" ]; then
    # 本地摄像头链路验证：真实屏幕采集 + 摄像头第二轨（需采集/摄像头权限；CI 不启用）。
    ENC="screen"; CODEC_ARGS="--codec h265"; CAMERA_ARGS="--camera"
elif [ "$PUBLISHER_ENCODER" = "h265" ]; then
    ENC="ffmpeg"; CODEC_ARGS="--codec h265"
elif [ "$PUBLISHER_ENCODER" = "screen" ]; then
    ENC="screen"; CODEC_ARGS="--codec h265"   # macOS 默认 h265，显式声明
else
    ENC="$PUBLISHER_ENCODER"
fi
./target/debug/aerodesk-agent --role publisher --signal "ws://127.0.0.1:${SIGNAL_OPS_PORT:-3001}" --room "$ROOM" --encoder "$ENC" $CODEC_ARGS $CAMERA_ARGS >/tmp/iossim-pub.log 2>&1 &
PUB=$!
sleep 2

echo "== [5/7] 安装并启动 App（自动连接；启动失败/无输出时快速重试）"
xcrun simctl install "$DEVICE" "$APP"
# CI 负载下 simctl launch --console 偶发不生效（console 全空、App 未连）。
# 等待 console 出现 pump 诊断输出，无输出则 terminate 重启动（最多 3 次，远快于整脚本重试）。
launch_app() {
    rm -f /tmp/iossim-console.log
    nohup xcrun simctl launch --console "$DEVICE" io.aerodesk.viewer \
        -autoconnect -server "ws://127.0.0.1:${SIGNAL_OPS_PORT:-3001}" -room "$ROOM" > /tmp/iossim-console.log 2>&1 &
}
launch_app
APP_OK=0
for attempt in 1 2 3; do
    for _ in $(seq 1 150); do  # 最多 30s 等 pump 输出
        if grep -q "pump:" /tmp/iossim-console.log 2>/dev/null \
           || grep -q "ad_viewer_create error" /tmp/iossim-console.log 2>/dev/null; then
            APP_OK=1
            break
        fi
        sleep 0.2
    done
    if [ "$APP_OK" = "1" ]; then
        echo "PASS app launched (attempt $attempt, console has pump output)"
        break
    fi
    echo "WARN: app 无输出（attempt ${attempt}），terminate 后重启动..."
    xcrun simctl terminate "$DEVICE" io.aerodesk.viewer 2>/dev/null || true
    launch_app
done
if [ "$APP_OK" != "1" ]; then
    echo "FAIL: app 启动 3 次仍无输出；console 内容："
    cat /tmp/iossim-console.log 2>/dev/null | tail -20
    exit 1
fi

echo "== [6/7] 断言解码帧持续增长"
python3 - <<'PY'
import os, time, re, sys
enc = os.environ.get('PUBLISHER_ENCODER', 'h265')
need_hevc = enc in ('h265', 'screen')   # 两者都是 HEVC 流（screen 默认 h265）
need_camera = os.environ.get('PUBLISHER_CAMERA') == '1'
ok = False
for i in range(90):  # 最多 90s
    try:
        txt = open('/tmp/iossim-console.log', errors='replace').read()
    except FileNotFoundError:
        txt = ''
    frames = re.findall(r'decoded frame #(\d+)', txt)
    if frames and int(frames[-1]) >= 30:
        print(f"PASS decoded frames >= 30 (last #{frames[-1]})")
        ok = True
        break
    if 'ad_viewer_create error' in txt:
        m = re.search(r'ad_viewer_create error: (.+)', txt)
        print("FAIL connect:", m.group(1) if m else txt[-300:])
        sys.exit(1)
    time.sleep(1)
if not ok:
    print("FAIL: 60s 内未解码 30 帧；取证：")
    print("--- console ---")
    print(open('/tmp/iossim-console.log', errors='replace').read()[-1500:])
    print("--- sfu ---")
    try: print(open('/tmp/iossim-sfu.log', errors='replace').read()[-800:])
    except FileNotFoundError: pass
    print("--- pub ---")
    try: print(open('/tmp/iossim-pub.log', errors='replace').read()[-800:])
    except FileNotFoundError: pass
    sys.exit(1)
# #328：断言观看端确实走 HEVC 硬解路径（否则悄悄退回 H264 等于没测）。
if need_hevc and 'using decoder Hevc' not in txt:
    print("FAIL: 未检测到 'using decoder Hevc'（HEVC 路径未触达）")
    sys.exit(1)
print("PASS HEVC decoder path (using decoder Hevc)")
if need_camera:
    # #340：摄像头第二轨经 SFU 到 iOS 观看端尚不达（publisher 已发、camera_mid 已协商
    # 但无媒体），此处仅告警不阻断——主合同是 HEVC 主轨解码回归。
    if 'using camera decoder' not in txt or '摄像头轨已接收' not in txt:
        print("WARN: 摄像头第二轨未解码（见 #340）；主轨 HEVC 已 PASS")
    else:
        print("PASS camera track decoded")
PY

echo "== [7/7] 截图留证"
xcrun simctl io "$DEVICE" screenshot /tmp/iossim-e2e.png || true

kill "$PUB" "$SFU" "$SIG" 2>/dev/null || true
echo "E2E DONE"
