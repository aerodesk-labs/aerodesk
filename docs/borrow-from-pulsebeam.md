# PulseBeam 调研与借鉴清单

调研对象：
- [PulseBeam](https://github.com/PulseBeamDev/pulsebeam)（v0.3.2，AGPL-3.0）— 基于 str0m 的生产级 WebRTC SFU
- PulseBeamDev/str0m fork `feat/bwe-fixes`（MIT/Apache，与上游同 License）

结论：**PulseBeam 是"str0m 商业化 SFU"的先行者，架构值得学，代码（AGPL）不抄。**

## 一、PulseBeam 架构要点

```
Control Plane（Thread 0）：axum 信令 API + 全局状态 + ShardRouter
Data Plane（每核一个 Shard）：SO_REUSEPORT UDP:443 + TCP:443 复用
  ShardWorker ← ShardCommand（AddParticipant/AddTcpConnection/Cluster）
  UnifiedSocket：批量接收 RecvPacketBatch（GSO）
  Demuxer：addr_map 快路径 + STUN ufrag 解码慢路径（带内存上限防伪造）
  ParticipantRegistry + DirtyTracker（脏标记批量发送）
  TimerWheel（定时器轮）
```

关键设计：
- **房间→分片路由**（control/router.rs）：ahash(room_id, shard_idx) 取最高哈希，
  load < 0.8 时保持房间 locality；超载级联到次高分片（房间跨核 → 跨分片中继）。
  Load 用不对称 EWMA 平滑（升 0.8 / 降 0.1）。
- **跨分片订阅驱动转发**（shard/worker.rs）：SubscribeTrack/UnsubscribeTrack 命令 +
  VideoRtpPublished/AudioRtpPublished/KeyframeRequested/UdpPacket 事件；
  订阅者所在分片 → 发布者分片按需拉流，避免全量广播。
- **目标**：CPU 引起的 p99 抖动 < 2ms；用 turmoil 模拟器 + debug_assert + 固定种子
  做确定性验证（AGENTS.md 强制规范）。

## 二、str0m fork（feat/bwe-fixes）改了什么

基线 = 上游 0.21.0（d7368c8），+536/-114，14 个文件，17 个 commit，全部围绕 BWE：

| 改动 | 动机（对远程桌面的价值） |
|---|---|
| padding 目标固定 50kbps，与 BWE 估计解耦 | 上游 padding 随估计膨胀 → 产生虚假 RTX 流量 = **白花带宽钱** |
| `set_current_bitrate`：显式分配码率作为 pacing floor | SFU 需要按端分配码率（PulseBeam BitrateController 的接口） |
| pacing_rate = max(allocated, estimate) × 1.1 | 对齐 libWebRTC，媒体平滑、不排队积压 |
| overuse/无媒体时禁止 padding | 避免拥塞中添乱 |
| probe 结果过滤：recv_interval >> send_interval 拒绝、pacer 未按时投递拒绝 | 防止错误探测压低带宽估计 |
| LargeDrop 后 30s 冷却、ALR probe 短重调度链式探测 | 探测稳健性 |
| 低 probe 估计受 acked throughput 限制 | 防止探测低估 |
| ALR 检测计入全部发送包、跨 ALR 保留丢包观测 | 状态机正确性 |
| 恢复 libWebRTC 默认 loss controller 参数 | 与浏览器行为对齐 |
| BWE 更新加 trace 日志 | 可观测性 |

## 三、借鉴清单

### ✅ 已落地（v0.1）
1. **str0m 依赖切到 bwe-fixes fork（pin rev 7db621f）**
   - 直接获得全部 17 个 BWE 修复；License 不变（MIT/Apache）
   - 验证：构建通过、`POST /start` 应答正常、UDP/TCP/SSL-TCP 三候选正常
2. **UnifiedSocket：UDP+TCP+SSL-TCP 同端口复用（3478，生产 443）**
   - libwebrtc fake-SSL hello exchange 完整实现（72B client hello → 79B server hello，
     字节级精确匹配，握手后明文，无真实 TLS）
   - 验证：fake-SSL 握手精确匹配 ✔、普通 TCP 路径不受影响 ✔、SDP 输出
     udp/tcp/ssltcp 三候选 ✔

### 🔜 v2 候选（按优先级）
| # | 借鉴项 | 理由 | 实施方式 |
|---|---|---|---|
| 1 | ~~UnifiedSocket：UDP+TCP 同端口复用（3478/443）~~ ✅ **已落地** | 防火墙友好、443 免 root | 同端口 3478；批量接收留待 v2 |
| 2 | Demuxer：addr_map 快路径 + STUN ufrag 解码 + 有界缓存 | 多参与者下 O(1) 路由 + 防伪造内存攻击 | 借鉴思路自研 |
| 3 | thread-per-core + SO_REUSEPORT | 单核 run loop → N 核线性扩展；p99 抖动目标的前提 | 大重构，v2 主线 |
| 4 | 房间哈希路由（load<0.8 locality + 超载级联） | 远程桌面"房间=1对1"，locality 天然适配 | 控制面组件 |
| 5 | 订阅驱动跨分片转发 | 房间跨核时只转发被订阅的流 | 与 3 配套 |
| 6 | BitrateController + `set_current_bitrate` | 每端码率分配 = 省带宽成本的核心 | fork 已给接口，自研控制器 |
| 7 | simulcast rid→层质量映射（q/h/f） | 按端选层，多观看端省出口 | 已有选择点，补映射 |
| 8 | turmoil 确定性模拟测试 | 延迟/丢包可复现，p99 抖动可证明 | 方法论采纳 |

### ❌ 不借鉴
- **AGPL 代码**（pulsebeam 主 crate）：只学架构，一行不抄
- **H.264 Baseline only**：远程桌面 4K60 需要 High/HEVC/AV1
- **无 DataChannel 现状**：输入通道是远程桌面硬需求（aerodesk-sfu 已有）
- 他们的信令 API（axum）：我们按远程桌面场景自定

## 四、行动建议

1. 短期：保持 fork 依赖，观察上游是否合并这些 BWE 修复（合并后切回上游）
2. 中期：v2 先做 UnifiedSocket 同端口复用 + 多核分片骨架（#1/#3/#4）
3. 持续：跟踪 PulseBeamDev/str0m fork 新 commit，逐批评估
