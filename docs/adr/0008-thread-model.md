# ADR-0008：SFU/signal 采用 std::thread 同步模型（#476）

- 状态：已采纳（记录既有设计，2026-08-17）
- 关联 Issue：#476（盘点）、#191/#196（内嵌 TURN 线程模型）、#85/#134（背压队列）、#104（栈溢出治理）
- 决策：**SFU / signal 保持 std::thread 同步事件循环模型，不引入 tokio**；tokio 继续作为 optional 依赖（仅 `aerodesk-linux` 的 `pipewire` feature 开启），不进入 SFU/signal 数据面。

## 背景

盘点 tokio 任务治理时确认：aerodesk-sfu / aerodesk-signal 共 43 处 `std::thread`（Shard 线程、manager/monitor、session_loop、TCP listener、5ms 轮询），全部同步模型。本 ADR 记录该设计的选择依据，避免后续重构走回头路。

关键事实：

1. **str0m 是同步 poll 模型**：`poll_output` / `poll_timeout` / `poll_event` 非 async，事件循环天然适配"每连接/每分片一个 std::thread + 排空式轮询"。历史上在 poll 循环里加 async 运行时边界（如 `tokio::spawn` 驱动 str0m）曾造成 100% CPU 死循环与背压失控（#85/#134 系列），最终全部回归同步排空。
2. **分片线程 = CPU 亲和性**：SFU 按核数开分片线程（SO_REUSEPORT socket 每线程独立收包），media demux / SCTP / DTLS 全在单线程内完成，无锁竞争；跨分片走 mpsc 命令 + CrossShardEvent。tokio 的 work-stealing 会破坏这种确定性与 cache locality。
3. **栈深度可控**：str0m DTLS/SCTP 解包调用链深，`thread::Builder::stack_size`（8-32MB）显式治理（#104）；tokio 的默认栈与调度时机更难做同等级别的确定性治理。
4. **实时性依赖节拍而非事件驱动**：媒体泵用 `recv_from` 超时 + `Instant` 节拍（5-50ms），背压用有界队列（64MB）+ 写失败计数，模型简单且可观测。

## 候选方案对比

| 方案 | 优点 | 代价 | 结论 |
|---|---|---|---|
| **保持 std::thread 同步模型**（现状） | 确定性、无锁分片、栈可控、与 str0m 天然匹配 | 无 async 生态（HTTP 用 rouille 同步）；每线程栈开销 | **选定** |
| 全面迁移 tokio | async 生态、I/O 复用 | str0m 非 async 需要适配层；work-stealing 破坏分片亲和；历史上引入过死循环/背压事故；迁移面 = 全部数据面 | 不选 |
| 混合（数据面同步 + 控制面 tokio） | 局部收益 | 两套模型并行增加心智负担；控制面（HTTP /start、/metrics）目前 rouille 同步已够用（短事务） | 不选 |

## 决策

- SFU/signal 数据面与内嵌 TURN 保持 std::thread 同步事件循环，不引入 tokio。
- 控制面 HTTP 继续用同步 rouille（短事务、低 QPS）；如未来需要高并发长连接控制面（如大规模 WebSocket 推送），在**控制面单独**评估异步运行时，不进入媒体数据面。
- tokio 维持 optional（linux pipewire feature），不影响其他平台构建。

## 后果

正向：

- 分片线程模型确定性强：CPU 亲和、无锁、排空节奏可预测；压测与生产行为一致。
- 线程栈显式治理（8-32MB），str0m 深调用链不依赖运行时默认栈。
- 依赖面窄：SFU/signal 不引入运行时，二进制更小、启动更快。

代价：

- 每连接/每分片线程的栈内存开销（~10MB×分片数，可接受）。
- 无法直接使用 async 生态库；需要时以短同步事务或独立进程桥接。
- 轮询节拍带来固定延迟下界（5ms 级，对 RTC 场景无感）。

## 相关 ADR

- ADR-0006（内嵌 TURN）：同一线程模型（控制面多线程共享 + 每 allocation 一个 relay 线程）。
- ADR-0007（data channel 转发）：SFU 分片内直转 + 背压队列，均基于同步模型实现。
