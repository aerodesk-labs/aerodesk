# ADR-0005：客户端 TURN relay 接入设计（#157）

- 状态：已采纳（2026-08-08）；里程碑 1 已实现，里程碑 2/3 待立项与部署环境
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

## 方案对比

| 方案 | 能力 | 工作量 | 结论 |
|---|---|---|---|
| A. 补 str0m `is` TURN socket（Allocate 状态机 + Permission + 数据通路接入 ICE） | 完整 TURN | 大（fork 补丁 500+ 行） | **里程碑 2** |
| B. 最小外部 TURN Allocate 客户端（本批） | 拿到 relayed 地址 + 凭证流程 | 中（已实现） | **里程碑 1（已交付）** |
| C. 换完整 webrtc crate | 完整 | 与 str0m 栈冲突 | 不选 |

## 里程碑

- **M1（已完成，#157 本批）**：`aerodesk-core::turn_client`——RFC 5766 Allocate（UDP）：
  无凭证→401（REALM/NONCE）→ 带 USERNAME/REALM/NONCE/MESSAGE-INTEGRITY（HMAC-SHA1，
  key=MD5(user:realm:pass)）重试 → 200（XOR-RELAYED-ADDRESS）；超时/事务 id 校验；
  单测含 mock TURN 服务器（401/200/坏密码）+ RFC 2202 HMAC 向量 + Python 参考值。
- **M2（待立项）**：str0m `is` 增加 TURN socket（Allocate 状态机 + CreatePermission +
  Send/Data 通路），把 relay 作为本地候选的可用中继路径；需真实 coturn（容器/brew）联调
  与 Android 模拟器 NAT 验证。
- **M3（待 M2）**：`connect_live_role` 把 `join` 返回的 `TurnConfig` 注入（M2 TURN socket），
  CLI/iOS/Android 自动使用；无 TURN 时直连优先、TURN 兜底（行为不变）。

## 风险与决策点

- M2 工作量大且需真实 TURN 环境验证：**是否立项由产品决定**（NAT/弱网是否核心场景）。
- 在 M2 落地前，客户端**不注入** relayed candidate（避免不可用候选破坏 ICE），
  只提供可测的 Allocate 原语。
- 部署侧 coturn 已就绪（TURN_SECRET/TURN_URLS），等 M2 即可端到端。

## 验收

- [x] M1：`cargo test -p aerodesk-core turn_client` 4 项全过
- [ ] M2：双端（客户端+SFU）经 coturn 中继媒体互通（真实环境）
- [ ] M3：Android 模拟器 NAT 下完整观看（媒体解码）；无 TURN 行为不变
