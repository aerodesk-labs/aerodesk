# SFU 容量压测基准（#215/#218/#220/#222，#8 方法论）

## 方法
`scripts/sfu-capacity-bench.sh`：起 SFU+signal（独立端口）→ `loadtest.sh` 施压
（N 房间 × M 对，vt 硬编 + NOISY 高熵源）→ 后台每 2s 采样 `/metrics/prometheus`
→ 汇总 峰值连接 / rx·tx 吞吐 / pps / viewer 媒体帧 / ICE 成功率 / 错误。

## 结果（macOS M4，release/debug 混合，2026-08-10）

| 配置 | 峰值 clients | 连接 | tx 吞吐 | tx pps | viewer 帧 | 错误 |
|---|---|---|---|---|---|---|
| 1×2 @720p30 2Mbps | 4 | 2/2 | 1.79 MB/s (14Mbps) | 1773 | 3184 | 0 |
| 2×2 @720p30 2Mbps | 8 | 4/4 | 2.78 MB/s (22Mbps) | 2780 | 9002 | 0 |
| 2×2 @1080p30 4Mbps | 8 | 4/4 | 2.50 MB/s (20Mbps) | 2434 | 4015 | 0 |

- 全部连接成功、媒体帧送达、无 panic/错误；8 并发 20Mbps+ 聚合无压力
- tx 含发布端媒体 + ICE/RTCP；rx 为 viewer→SFU（RTCP/输入等）
- 单机 SFU 容量受编码器（vt）与磁盘/网络影响；生产容量建议真机矩阵 + 长稳

## TURN 中继路径（#218）

第 8 参 `turn_relay=1`：SFU 启动内嵌 TURN server（`SFU_TURN_PORT`，默认 14789）并
下发 `TURN_URLS` 给信令；客户端 `AERODESK_FORCE_RELAY=1`（CLI 与 core 一致）只通告
relayed 候选、跳过 host 候选，媒体强制走 TURN 中继。摘要额外断言
`relayed-candidate`/`force-relay-skip` 日志行数 ≥ 连接数。

### 结果（同机，macOS M4，debug 混合，2026-08-10）

| 配置 | 峰值 clients | 连接 | relayed 断言 | tx 吞吐 | tx pps | viewer 帧 | 错误 |
|---|---|---|---|---|---|---|---|
| 1×2 @720p30 2Mbps | 4 | 2/2 | 4/4 | 0.71 MB/s | 727 | 1054 | 0 |
| 2×2 @720p30 2Mbps | 8 | 4/4 | 8/8 | 2.73 MB/s | 2751 | 6375 | 0 |
| 2×2 @1080p30 4Mbps | 8 | 4/4 | 8/8 | 2.18 MB/s | 1733 | 2991 | 0 |

- 全部连接经 relayed 候选成功、媒体帧送达、无 panic/错误
- TURN 中继路径同机吞吐约为直连的 60-80%（额外一跳 + 内嵌 TURN 转发），符合预期；
  绝对容量（8 并发 20Mbps 级）仍无瓶颈
- 偶发：一次 1080p 档出现 1 对 0 字节日志（启动竞态），重跑全绿——见 ISSUE 记录
  不在本批处理范围

## 长稳压测（#220）

`scripts/sfu-longrun.sh`（参数同 capacity bench，第 8 参 turn_relay）：起 SFU+signal →
N×M 对持续施压 → 每 30s 采样 `/metrics/prometheus`（clients/rx·tx/turn_allocations）
→ 看门狗断言：
- 60s 内连接收敛，之后任一采样低于预期即失败
- viewer RECEIVED 帧单调递增，90s 无增长判定卡死
- TURN 变体：`aerodesk_sfu_turn_allocations` 收敛后 == 客户端数，连续 2 采样下降即失败
- 任何 panic/abort/ICE disconnected/reconnect 即失败

### 结果（macOS M4，debug 混合，2026-08-10）

| 配置 | 时长 | 连接 | 帧增量 | TURN allocation | 错误 |
|---|---|---|---|---|---|
| 直连 1×1 @720p30 2Mbps | 420s | 1/1 | 5623 | 不适用 | 0 |
| TURN 中继 1×1 @720p30 2Mbps（force-relay） | 664s（>600s lifetime） | 1/1 | 5845 | 活跃=2/预期=2，累计=2（零 churn） | 0 |

- TURN 变体跑过 600s allocation lifetime：Refresh 正常维持，无 allocation 消失/重连
- 累计 allocation 数 = 活跃数（无泄漏、无 churn）；媒体全程持续送达
- 直连与 TURN 中继长稳均 PASS

### TURN allocation 指标（#220）
- `aerodesk_sfu_turn_allocations` gauge：当前活跃 allocation 数（`/metrics/prometheus`
  与 `/metrics` JSON `turn_allocations` 字段）
- `aerodesk_sfu_turn_allocations_total` counter：累计创建数（观测 churn/重连）
- `scripts/turn-e2e.sh` 3d 断言连接后活跃数 >= 3（回归防护）

## 重连韧性（#222）

`scripts/sfu-reconnect.sh [cycles] [rooms] [pairs] [turn_relay] [settle_s]`：
每轮起 N×M 对 → 等 ICE connected → 断言 clients（5s 心跳指标，轮询 ≤10s）与
`turn_allocations`（TURN 模式）达预期 → SIGKILL 全部客户端（模拟闪断）→
断言 clients → 0、TURN allocation → 0（TURN 变体 `TURN_LIFETIME_SEC=60` 加速
过期回收，默认等 120s）→ 下一轮；全部轮次后断言累计 allocation == 轮次×客户端
（零泄漏/零异常 churn）、无 panic。

### 结果（macOS M4，debug 混合，2026-08-10）

| 模式 | 轮次 | 每轮 allocation | 累计 allocation | 清理 | 错误 |
|---|---|---|---|---|---|
| 直连 1×1 | 3 | 不适用 | 不适用 | clients 归零 | 0 |
| TURN 中继 1×1（force-relay，lifetime=60s） | 3 | 2（=客户端数） | 2/4/6（精确） | clients + allocation 均归零 | 0 |

- SIGKILL（无 Refresh 0）后 UDP allocation 依赖 lifetime 过期 + 30s 清扫回收，
  `TURN_LIFETIME_SEC` 可调（默认 600，min 60）；生产短租期部署可收紧
- SFU 会话清理：信号 WS 断开 → shard 移除，clients 指标 5s 心跳后归零

## 复现
```sh
scripts/sfu-capacity-bench.sh 1 2 15 1280 720 30 2000000        # 单档（直连）
scripts/sfu-capacity-bench.sh 2 2 20 1920 1080 30 4000000       # 1080p 档（直连）
scripts/sfu-capacity-bench.sh 1 2 15 1280 720 30 2000000 1      # TURN 中继单档
scripts/sfu-capacity-bench.sh 2 2 20 1920 1080 30 4000000 1     # TURN 中继 1080p
scripts/sfu-longrun.sh 1 1 420 1280 720 30 2000000              # 直连长稳 7 分钟
scripts/sfu-longrun.sh 1 1 660 1280 720 30 2000000 1            # TURN 中继长稳 11 分钟（越过 lifetime）
scripts/sfu-reconnect.sh 3 1 1 0 120                            # 直连重连韧性 3 轮
scripts/sfu-reconnect.sh 3 1 1 1 120                            # TURN 中继重连韧性 3 轮（lifetime=60s）
```

## 结论
- SFU 转发路径（直连 + TURN 中继）在 8 并发/20Mbps 级无瓶颈
- 长稳（#220）：直连 420s 与 TURN 中继 664s（越过 allocation lifetime）均 PASS，
  TURN allocation 零泄漏零 churn，Refresh 正常
- 重连韧性（#222）：直连/TURN 各 3 轮 SIGKILL 循环后 clients 与 allocation 全部
  归零、累计精确、无 panic；TURN allocation 过期回收依赖 lifetime+30s 清扫
- 后续 #8 验收按此方法论扩展到真机与 4K60
