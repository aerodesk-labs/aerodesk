# AeroDesk 压测方法学与基线（#8）

## 工具

| 脚本 | 作用 |
|---|---|
| `scripts/loadtest.sh` | 施压：N 房间 × P 对（发布端 + 观看端），参数化分辨率/fps/码率；`NOISY=1` 用高熵合成源（码率贴近目标档位，避免彩条源吞吐失真） |
| `scripts/highrate-e2e.sh` | 高码率（大帧）回归：VT 1080p60 `--noisy` ~5k pps → viewer 关键帧/解码正常（防 CLI viewer 单包读取丢关键帧回归） |
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
- **端到端延迟**（#8）：发布端 cursor 通道带发送墙钟（`CursorPos.sent_ms`），观看端 `LATENCY: N ms`；bench 报告 avg/max/p99（同机测量=真实 one-way 延迟）
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

## 基线（2026-08-07 复测：低负载 2.5 + CLI viewer 高码率修复后）

> 环境：Apple Silicon M4 10 核 / macOS 26.5.2；1-min loadavg ≈ 2.5（较 08-04 的 8.5 干净）。
> **关键修复**：CLI viewer 每轮只读 1 包 + `sleep(2ms)`，高 pps（~5k）时内核丢包、
> 关键帧永远不完整（0 keyframes / DECODED 0）；改为排空式读取（每轮 ≤512 包、有数据不 sleep）。
> 修复前 noisy 1080p60 仅 15 帧/0 关键帧/0 解码，修复后 ~57fps/2-4 关键帧/~80% 解码。

| 配置 | 连接 | 实际帧率 | 吞吐 | 收/发包 | 说明 |
|---|---|---|---|---|---|
| 1×1 @ 4K60 10Mbps（彩条源）20s | 2/2 | 25.58 fps | 0.33 Mbps | - | 彩条过度可压缩，吞吐失真；帧率仍受编码/共享机限制 |
| 1×1 @ 4K60 10Mbps `NOISY=1` 20s | 2/2 | 25.06 fps | 86.9 Mbps avg / 97.7 peak | 93520 / 90346（≈3.4% 丢包） | 真实高码率：SFU CPU 5.0%、内存 31.5MB；发布/观看端各 ~58% CPU（本机瓶颈） |
| 1×1 @ 1080p60 10Mbps `--noisy`（探针）8s | 2/2 | ~57 fps | ~20 MB/8s | - | 修复后关键帧 3、DECODED 281/381 |

要点：
- **瓶颈在发布端 VT 编码 + 观看端 FFmpeg 软解**（各 ~58% CPU），SFU 在 87Mbps 时仅 5% CPU；4K60@60fps 需干净/专用机或硬解；
- 高码率丢包从修复前 ~30% 降至 ~3.4%（剩余为事件处理窗口的短暂溢出，不影响关键帧送达）；
- `scripts/highrate-e2e.sh` 已入 CI 防止回归（VT 不可用时 SKIP，与本仓库 VT 单测策略一致）。

## 验收口径映射

| 指标 | 证据 |
|---|---|
| p99 抖动 <2ms | netem 回归（`crates/aerodesk-sfu/tests/sim.rs`：抖动/丢包/拥塞 7 测试，CI 三平台绿） |
| 端到端 40-80ms@4K60 | 需干净环境 + 真机矩阵实测（本机基线受共享负载污染） |
| 真机冒烟矩阵 | 待硬件（Windows/Linux/Android/iOS/鸿蒙） |

## 待办
- [ ] 干净环境（专用机/低负载）复测 4K60 **达到 60fps**（本机 08-07 复测 ~25fps，瓶颈为 VT 编码 + FFmpeg 软解 CPU）
- [ ] 观看端 AV1/HEVC 硬解接入后可显著降低 4K 解码 CPU（#74 剩余项）
- [ ] 真机矩阵补齐后更新本表
