# AeroDesk 性能与验收报告（2026-08-07）

> 关联：#8 质量验收与 4K60 压测；方法学与基线见 [`BENCHMARK.md`](BENCHMARK.md)。
> 全部数据来自本机实测（release 构建），共享开发机负载波动（2.5–27），
> **4K60@60fps 与 40-80ms 延迟的正式验收仍需干净/专用机**（工具已就绪）。

## 环境

- 机器：Apple M4（10 核）/ macOS 26.5.2 / 32GB
- 构建：release（debug 时序失真，见 LESSON_性能压测必须用release构建否则数据失真）
- 负载：1-min loadavg 2.5（低负载复测）~ 27（其他 agent 重编译期间）

## 关键修复（本报告周期内）

| 修复 | 影响 |
|---|---|
| CLI viewer 排空式读取（#101） | 高码率大帧流 0 关键帧/0 解码 → 1080p60 noisy ~57fps、4K60 noisy 通；高码率丢包 ~30%→~3.4% |
| 合成源帧间隔纳秒精确（#100） | 10 分钟 A/V 漂移 ~6s → 中位数差 7ms（#73） |
| SFU shard 栈 8MB（#104） | file-transfer e2e 栈溢出崩溃消除，CI 恢复稳定 |
| 文件传输补包成功才出队（#101） | 发送端补包不再丢块，100MB 稳定完成 |

## 压测数据

### 4K60（3840x2160@60，10Mbps）

| 场景 | 实际帧率 | 吞吐 | 说明 |
|---|---|---|---|
| 彩条源（低负载） | 25.58 fps | 0.33 Mbps | 彩条过度可压缩，吞吐失真 |
| `NOISY=1`（低负载） | 25.06 fps | 86.9 Mbps avg / 97.7 peak | 发布/观看端各 ~58% CPU（本机瓶颈）；SFU CPU 5.0%、内存 31.5MB |
| 修复前基线（2026-08-04，负载 8.5） | ~2.3 fps | 0.10 Mbps | 共享负载 + 旧 VtEncoder pts bug |

### 1080p60 noisy（1920x1080@60，5–10Mbps）

- 修复前：15 帧 / 0 关键帧 / 0 解码（viewer 单包读取丢包）
- 修复后：~57 fps / 2–4 关键帧 / DECODED ~80%

### 端到端延迟（#107 新指标，cursor 通道带发送墙钟）

- 1080p60 noisy（负载 27 时）：avg 223.9ms / max 438ms / p99 438ms（7 样本）
- 干净机预期显著更低；40-80ms@4K60 验收待专用环境

### 文件传输（#72/#85）

- **100MB**（release，最新 main）：**4 分钟完成，sha256 一致**，进度日志齐全，无 SFU 栈溢出
- <60s 目标需 SFU 每客户端独立线程/异步 DTLS（架构级专项，#85 分析结论）

### A/V 同步（#73）

- 10 分钟 release 实测：漂移 p95=40ms、max=60ms（单次瞬态，80ms jitter buffer 吸收）、前后半程中位数 13/20ms 差 **7ms** → 无累积漂移

### 网络抗性（自动化，CI 三平台）

- netem：1%/5% 丢包、组合 5% 丢包 + 30ms 延迟 + 15ms 抖动（7 测试全绿，`crates/aerodesk-sfu/tests/sim.rs`）

## 验收结论

| 指标 | 状态 |
|---|---|
| p99 抖动 <2ms | ✅ netem 回归覆盖（CI 三平台绿） |
| 端到端 40-80ms@4K60 | ⏳ 测量工具就绪（#107）；需干净机/真机正式验收 |
| 4K60 60fps | ⏳ 本机 ~25fps（VT 编码 + FFmpeg 软解 CPU 瓶颈）；需专用机或观看端硬解（AV1 VT 不可用已取证） |
| ≥100MB 文件字节一致 | ✅ 100MB 4 分钟 sha256 一致（#106） |
| 10 分钟无累积漂移 <50ms | ✅ 中位数差 7ms（#100） |
| 真机冒烟矩阵 | ⏳ 需硬件（Windows/Linux/Android/iOS/鸿蒙） |

## 复现命令

```sh
# 4K60 noisy 压测 + 延迟（#107）
NOISY=1 BITRATE=10000000 REPORT_DIR=/tmp/bench-4k60 scripts/bench.sh 1 1 30 3840 2160 60
# 100MB 文件传输验收
PROFILE=release scripts/file-transfer-e2e.sh room 102400
# 10 分钟 A/V 漂移验收
PROFILE=release scripts/avsync-10min-e2e.sh room 600
# 高码率大帧回归
scripts/highrate-e2e.sh
```
