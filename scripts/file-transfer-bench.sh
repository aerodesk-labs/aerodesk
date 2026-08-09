#!/usr/bin/env bash
# #72/#210 大文件传输基准：release 构建 + 多尺寸矩阵（sha256 + 耗时 + 吞吐）。
# 用法: scripts/file-transfer-bench.sh [尺寸KB...]   # 默认 2048 102400 262144
set -uo pipefail
cd "$(dirname "$0")/.."
export RUST_LOG="${RUST_LOG:-info}"

SIZES=("${@:-2048 102400 262144}")
echo "== 构建 release"
PROFILE=release cargo build -q --release -p aerodesk-sfu -p aerodesk-signal -p aerodesk-cli

echo "== 基准矩阵"
printf "%-12s %-10s %-10s %-10s %s\n" "size" "elapsed" "MB/s" "sha256" "result"
for KB in "${SIZES[@]}"; do
    START=$(date +%s)
    OUT=/tmp/ftx-bench-${KB}.log
    PROFILE=release ./scripts/file-transfer-e2e.sh "ftx-bench-${KB}-$(date +%s)" "$KB" >"$OUT" 2>&1
    RC=$?
    END=$(date +%s)
    ELAPSED=$((END-START))
    MBPS=$(python3 -c "print(f'{(int('$KB')/1024.0)/max($ELAPSED,1):.2f}')")
    SHA=$(grep -c "PASS sha256" "$OUT")
    RES="FAIL"
    [ "$RC" -eq 0 ] && [ "$SHA" -ge 1 ] && RES="PASS"
    printf "%-12s %-10s %-10s %-10s %s\n" "${KB}KB" "${ELAPSED}s" "${MBPS}MB/s" "($SHA/1)" "$RES"
done
