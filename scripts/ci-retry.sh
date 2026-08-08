#!/usr/bin/env bash
# ci-retry.sh —— 对已知瞬时 flaky 的 CI 步骤做有限重试（不掩盖真实回归）。
#
# 用法：bash scripts/ci-retry.sh <N> <cmd...>
#   最多执行 <N> 次（含首次），任意一次成功即退出 0；全部失败退出 1。
# 适用范围：仅用于已被多次观察为瞬时失败（时序/环境，非代码）的 e2e 步骤；
#           稳定步骤（cargo test/fmt/clippy）绝不套用。
set -euo pipefail

if [ "$#" -lt 2 ]; then
    echo "usage: bash scripts/ci-retry.sh <N> <cmd...>" >&2
    exit 2
fi
N="$1"
shift
if ! [[ "$N" =~ ^[0-9]+$ ]] || [ "$N" -lt 1 ]; then
    echo "N must be a positive integer" >&2
    exit 2
fi

for i in $(seq 1 "$N"); do
    if "$@"; then
        exit 0
    fi
    echo "[ci-retry] attempt $i/$N failed, retrying in 5s..." >&2
    sleep 5
done
exit 1
