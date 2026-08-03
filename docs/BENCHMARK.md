# AeroDesk 压测方法学与基线（#8）

## 工具

| 脚本 | 作用 |
|---|---|
| `scripts/loadtest.sh` | 施压：N 房间 × P 对（发布端 + 观看端），参数化分辨率/fps/码率 |
| `scripts/bench.sh` | 编排：起 sfu/signal → 跑 loadtest → 采样 /metrics + 进程 CPU/RSS → 产出报告 |
| `scripts/bench_report.py` | 汇总：连接数、实际流帧率、吞吐、SFU/signal CPU 与内存，输出 `report.json` + Markdown |

用法：

```sh
scripts/bench.sh [rooms] [pairs] [seconds] [width] [height] [fps]
BITRATE=10000000 REPORT_DIR=/tmp/bench-4k60 scripts/bench.sh 1 1 30 3840 2160 60
```

## 指标定义

- **吞吐**：SFU `/metrics` 各分片 rx/tx 字节差分 × 8 / 采样间隔（Mbps）
- **实际流帧率**：观看端 CLI 日志 `RECEIVED: N frames` 差分 / 时长
- **CPU/RSS**：`ps` 每秒采样 sfu/signal 进程
- **连接成功率**：loadtest 结束后各端 "ICE connected" 计数

## 基线（2026-08-04 本机实测）

> 环境：Apple Silicon arm64 10 核 / macOS 26.5.2；**共享开发机，负载 8.5/10**
> （同机存在其他项目 esbuild 2×200% + Spotlight 索引 ~8×15% 持续占核），
> 结果受环境干扰，仅作工具链验证基线；4K60 验收需在干净/生产环境复测。

| 配置 | 连接 | 实际帧率 | 吞吐 | SFU CPU | SFU 内存 | signal CPU |
|---|---|---|---|---|---|---|
| 1×1 @ 1080p60 10Mbps 15s | 2/2 | ~11.6 fps | 0.15 Mbps | 0.6% | 19 MB | 0.2% |
| 1×1 @ 4K60 10Mbps 20s | 2/2 | ~2.3 fps | 0.10 Mbps | 0.3% | 20 MB | 0.1% |

要点：
- **SFU/signal 占用极低**（<1% CPU），瓶颈在发布端编码（VT 同步阻塞 + 共享机器负载），不在服务端；
- 1080p60 帧率约 4.9× 于 4K60，与 IOSurface 逐帧拷贝/编码量（33MB vs 8MB）量级一致；
- 本批次附带修复 `VtEncoder` 时间戳步进 bug（固定 3000 = 30fps，60fps 时接收端被压到 30fps；现按 `90_000/fps` 步进）。

## 验收口径映射

| 指标 | 证据 |
|---|---|
| p99 抖动 <2ms | netem 回归（`crates/aerodesk-sfu/tests/sim.rs`：抖动/丢包/拥塞 7 测试，CI 三平台绿） |
| 端到端 40-80ms@4K60 | 需干净环境 + 真机矩阵实测（本机基线受共享负载污染） |
| 真机冒烟矩阵 | 待硬件（Windows/Linux/Android/iOS/鸿蒙） |

## 待办
- [ ] 干净环境（专用机/低负载）复测 4K60，产出正式验收报告
- [ ] 真机矩阵补齐后更新本表
