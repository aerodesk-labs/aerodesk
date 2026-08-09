# SFU 容量压测基准（#215，#8 方法论）

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

## 复现
```sh
scripts/sfu-capacity-bench.sh 1 2 15 1280 720 30 2000000   # 单档
scripts/sfu-capacity-bench.sh 2 2 20 1920 1080 30 4000000  # 1080p 档
```

## 结论
- SFU 转发路径在 8 并发/20Mbps 级无瓶颈；后续 #8 验收按此方法论扩展到真机与 4K60
