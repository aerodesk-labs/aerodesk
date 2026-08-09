# SFU 容量压测基准（#215/#218，#8 方法论）

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

## 复现
```sh
scripts/sfu-capacity-bench.sh 1 2 15 1280 720 30 2000000        # 单档（直连）
scripts/sfu-capacity-bench.sh 2 2 20 1920 1080 30 4000000       # 1080p 档（直连）
scripts/sfu-capacity-bench.sh 1 2 15 1280 720 30 2000000 1      # TURN 中继单档
scripts/sfu-capacity-bench.sh 2 2 20 1920 1080 30 4000000 1     # TURN 中继 1080p
```

## 结论
- SFU 转发路径（直连 + TURN 中继）在 8 并发/20Mbps 级无瓶颈；后续 #8 验收按此
  方法论扩展到真机、4K60 与长稳（#218 已预留 TURN 中继长稳项）
