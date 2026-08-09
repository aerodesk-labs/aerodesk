# ADR-0005：客户端 TURN relay 接入设计（#157）

- 状态：已采纳（2026-08-08）；里程碑 1/2 已实现（2026-08-09），里程碑 3 部分实现（CLI/iOS/Android/UI 壳层复用 MediaSocket）
- 关联 Issue：#157（客户端 TURN relay 接入）、#8（网络抗性）、#2/#1（Android/iOS NAT）
- 上游：DEPLOYMENT.md（coturn TURN 中继兜底）

## 问题

`signal.join()` 返回的 `TurnConfig` 一直被客户端忽略（`connect_live_role` 里 `_turn`），
str0m 无现成 relay 接入：真机跨网络（非同一 LAN）无 TURN relay 时媒体必然连不通；
Android 模拟器 NAT 下宿主 SFU 无法回传 UDP。

## 调研结论（已核对 aerodesk-labs/str0m fork acd7e77）

- str0m `is` 子 crate **支持 STUN/TURN 消息编解码**（`stun::Builder` 含 Allocate/CreatePermission）
  与 **Relayed candidate 表示**（`CandidateKind::Relayed`、`Candidate::relayed(addr, local)`），
  可解析远端 SDP 里的 relay candidate。
- str0m **没有 TURN 客户端数据通路**：不执行 Allocate 事务、不建 Permission、
  不把媒体经 TURN 服务器中继——本地即使构造 relayed candidate 也无数据通路可用。
- Web 端浏览器原生 RTC 已通过 `iceServers` 使用 joined.turn（已有实现，核对一致）。

## 方案对比（M2 已按 D 落地）

| 方案 | 能力 | 工作量 | 结论 |
|---|---|---|---|
| A. 补 str0m `is` TURN socket（fork 补丁） | 完整 TURN | 大（500+ 行） | 不选 |
| B. 最小外部 TURN Allocate 客户端 | 拿 relayed 地址 + 凭证流程 | 中（已实现） | M1（已交付） |
| C. 换完整 webrtc crate | 完整 | 与 str0m 栈冲突 | 不选 |
| **D. 应用层 TURN 传输（M2 落地）** | 完整数据通路 | 中（core 内实现） | **已采用** |

**D 方案依据**：str0m 不绑 socket、由应用驱动收发包循环（`recv_from → handle_input(Receive)
→ poll_output → send_to`），因此 TURN 封装完全放在 `aerodesk-core` 传输层：
- `TurnTransport`（RFC 5766）：Allocate（保留 REALM/NONCE）+ CreatePermission + ChannelBind +
  ChannelData/Data indication + 定期 Refresh
- `MediaSocket`：与 `UdpSocket` 同接口（recv_from/send_to/local_addr/set_read_timeout），
  内部双路收发 + ICE 锁定（首个非 STUN Binding 包决定直连/TURN，直连优先、TURN 兜底）
- `Endpoint::add_relay_candidate`：relayed 候选进 offer（`typ relay`），SFU 无需改动
- CLI/iOS/Android/UI 壳层零改动（LiveSession.socket 类型替换为 MediaSocket）

**webrtc-rs turn crate 已评估（未采用）**：功能完整（异步、含 refresh/permission/channel），
但强依赖 tokio + ring + webrtc-util，与 aerodesk-core 同步泵架构不符；其 `stun` crate 的
属性/密钥定义（XOR-RELAYED-ADDRESS=0x0016）已作为互操作参考。

## 里程碑

- **M1（已完成，#157 本批）**：`aerodesk-core::turn_client`——RFC 5766 Allocate（UDP）：
  无凭证→401（REALM/NONCE）→ 带 USERNAME/REALM/NONCE/MESSAGE-INTEGRITY（HMAC-SHA1，
  key=MD5(user:realm:pass)）重试 → 200（XOR-RELAYED-ADDRESS）；超时/事务 id 校验；
  单测含 mock TURN 服务器（401/200/坏密码）+ RFC 2202 HMAC 向量 + Python 参考值。
- **M2（已完成）**：应用层 `TurnTransport` + `MediaSocket` 数据通路；真实 coturn
  （brew 4.17.2）联调通过（Allocate/Permission/Channel/Data 双向中继，`scripts/turn-e2e.sh`）。
- **M3（部分完成）**：`connect_live_role`/CLI 注入 join 返回的 TurnConfig；壳层复用
  MediaSocket 自动生效；无 TURN 时直连优先、TURN 兜底（行为不变）。待办：Android 模拟器
  NAT 完整观看验证（真机/模拟器环境）。

## 风险与决策点

- M2 工作量大且需真实 TURN 环境验证：**是否立项由产品决定**（NAT/弱网是否核心场景）。
- 在 M2 落地前，客户端**不注入** relayed candidate（避免不可用候选破坏 ICE），
  只提供可测的 Allocate 原语。
- 部署侧 coturn 已就绪（TURN_SECRET/TURN_URLS），等 M2 即可端到端。

## 验收

- [x] M1：`cargo test -p aerodesk-core turn_client` 全过（含 401/200/HMAC/RFC 向量）
- [x] M2：真实 coturn 中继回环 + SFU 下发 TURN 配置 → CLI 发布/观看端 allocate +
      relayed 候选 + ICE 连通（`scripts/turn-e2e.sh` 全 PASS）
- [ ] M3：Android 模拟器 NAT 下完整观看（媒体解码）；无 TURN 行为不变（现有 e2e 覆盖）

## 踩坑记录（M2）

- coturn `use-auth-secret`：brew 4.16.0 有已知回归（#1534），升级 4.17.2 后 REST 凭证可用
- **XOR-RELAYED-ADDRESS = 0x0016**（0x0022 是 RESERVATION-TOKEN）；XOR 地址 family 在
  byte1（byte0 保留）；MESSAGE-INTEGRITY 的 HMAC 输入不含 MI 属性头+值（`msg.len()-24`）
  ——这三处 mock 自洽掩盖了互操作 bug，最终以真实 coturn 联调暴露
