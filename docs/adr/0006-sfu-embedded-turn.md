# ADR-0006：SFU 内嵌 TURN+STUN server（替代 coturn 侧车）

- 状态：已采纳（2026-08-09）
- 关联 Issue：#191（实现批次）、#182（求证：是）、#185（端口冲突）、#157（客户端 TURN）
- 上游：ADR-0005（客户端 TURN relay）、docs/TURN.md

## 问题

- TURN relay 依赖独立 coturn 侧车（每 PoP 多一个进程/运维面）。
- #185：SFU 默认媒体端口 3478 与 coturn 默认 3478 同机冲突，TURN_URLS 默认指向
  媒体端口导致 relay 失效。
- 客户端 TURN 已就绪（#157 M2，应用层 TurnTransport + MediaSocket），缺服务端闭环。

## 决策

**SFU 进程内实现 TURN+STUN server（RFC 5389 Binding + RFC 5766 Allocate/
CreatePermission/ChannelBind/Send/Data/Refresh），默认替代 coturn**：

- `TURN_SECRET` 设置且未显式 `TURN_URLS` 时启动内嵌 server（`SFU_TURN_PORT`，默认 **3479**，
  与媒体 3478 不冲突）；TURN_URLS 默认指向内嵌地址。
- 认证与 coturn REST 兼容（`username=expiry:userid`，`credential=base64(HMAC-SHA1(secret, username))`），
  已下发的 TurnConfig 直接可用；STUN/TURN codec 上移到 `aerodesk-protocol::turn::codec`
  供客户端/服务端共用。
- 显式 `TURN_URLS` 仍走外部 coturn（向后兼容，老部署不变）。

## 架构

- 控制面：单线程 UDP 事件循环（Allocate/Permission/ChannelBind/Send/ChannelData/Refresh/
  Binding），每 allocation 一个 relay 线程（peer → 客户端：ChannelData 或 Data indication）。
- 数据面：客户端 → peer 走 Send indication / ChannelData；peer → 客户端走 ChannelData
  （已绑 channel）或 Data indication（仅有 permission）。
- 生命周期：allocation lifetime 600s（Refresh 续期），30s 清扫过期。

## 互操作验证

- 自研客户端 ↔ 内嵌 server：`scripts/turn-e2e.sh` 中继回环 PASS。
- **webrtc-rs turn client ↔ 内嵌 server**：Allocate + CreatePermission + Channel + relay
  ping PASS（独立实现交叉验证）。
- 外部 coturn 模式回归 PASS（TURN_MODE=coturn）。
- 踩坑：webrtc-rs 请求在 MI 后追加 FINGERPRINT，MI 校验须按 MI 偏移并回退 length 字段
  （pion/stun 一致）；已修 `codec::verify_message_integrity`。

## 影响

- 部署：每 PoP 从 `signal + sfu + coturn` 简化为 `signal + sfu`（#185 解决）。
- 端口：开放 UDP/TCP 3479（TURN）+ TCP 5349（TURN TLS，#196）+ 49152-49200（relay 段）+ 原媒体端口。
- 更新（#196）：TURN over TCP/TLS 已实现（`?transport=tcp` 与 `turns:`，浏览器原生可用）；
  native 客户端 TCP/TLS 已实现（#199）。
- 更新（#204）：allocation 配额（per-IP/全局，486）与 peer 拒绝策略（403/Send 丢弃）已实现；
  IPv6 双栈（SFU_TURN_IPV6=1）已实现。剩余：IPv6 生产验证、跨 PoP 级联（v3）。
