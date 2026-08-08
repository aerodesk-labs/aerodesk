# ADR-0004：跨 PoP 实时媒体桥接设计决策

- 状态：已采纳（设计稿，2026-08-08；实现需真实多 PoP 部署）
- 关联 Issue：#146/#150（多 PoP v1：房间→PoP 映射 + 重定向）、#5（服务端收口）
- 上游决策：ADR-0001 系列；DEPLOYMENT.md 多 PoP 章节

## 问题与约束

现状（#146/#150 v1）：每 PoP 独立 signal+SFU+coturn；信令用 `ROOM_POP_MAP` 把房间钉到固定 PoP，
客户端连错 PoP 时信令返回 `Redirect`，客户端自动重连到目标 PoP。**房间内成员必须落在同一 PoP**，
跨 PoP 实时媒体桥接未支持。

约束：
- 无重编码转发（SFU 是 selective forwarding；跨 PoP 中继应保持 RTP 原样，避免转码成本）
- 输入/剪贴板/文件走 data channel（SCTP）——跨 PoP 中继若只转媒体、不转 data channel，远程控制断链
- 延迟预算：端到端 40-80ms@4K60（#8）；跨 PoP 中继每跳增加 ~10-30ms（现实网络）
- 无真实多 PoP 部署环境：直接实现桥接无法端到端验证

## 候选方案对比

| 方案 | 跨区成员 | 媒体 | data channel | 复杂度 | 延迟 | 结论 |
|---|---|---|---|---|---|---|
| **A. 同 PoP 就近 + GeoDNS/Anycast（现状 v1）** | ❌ 必须同区 | 直连 | 直连 | 低（已实现） | 最低 | **v2 主线** |
| B. SFU 级联 relay（publisher PoP SFU ↔ viewer PoP SFU 中继 RTP） | ✅ | RTP 中继（无重编码） | 需同步中继 SCTP（跨 SFU 建 data channel 桥） | 高 | +1 跳 | v3 候选 |
| C. 全局大房间（不分区，单 SFU 集群） | ✅ | 直连 | 直连 | 中（但失去 PoP 就近/故障域） | 视拓扑 | 不选（与 PoP 目标冲突） |

## 建议路线

**v2（可立即实施，无需桥接）**：保持方案 A 为默认——GeoDNS/Anycast 把用户导向最近 PoP，
`ROOM_POP_MAP` 保证同房间成员钉同一 PoP；跨 PoP 只作为**容灾/迁移**场景：
- 房间创建时按主叫方就近 PoP 动态注册（v2 增强：把静态 `ROOM_POP_MAP` 升级为运行时注册表，
  `room → pop` 由首个加入者所在 PoP 登记，见后续动态注册表批次）
- 无需实时媒体桥接；投入放在监控/告警/容灾切换

**v3（需真实多 PoP + 压测后实施）**：SFU 级联 relay（方案 B）设计要点（作为未来实现基线）：
1. **信令面**：viewer 的 PoP 信令在 SDP 协商时检测 room 主 PoP ≠ 本 PoP → 向主 PoP SFU 发起
   `POST /relay`（内部 API + token）：主 PoP SFU 建一个"relay peer"（Rtc 只收不发/只发不收）
2. **媒体面**：relay 两端用 RTP 中继（SRTP 会话，无重编码）；SFU 现有订阅驱动转发（#132）天然可复用：
   relay peer 作为虚拟 viewer 订阅 publisher 的层
3. **data channel**：输入/剪贴板必须跨 relay——需要跨 SFU 的 SCTP 桥（relay peer 与真实 viewer 之间
   建 data channel，主 PoP SFU 把 `input` 标签消息转发到 publisher）。SCTP 桥是 v3 主要复杂度
4. **失败/回退**：relay 建失败 → 返回 `Redirect` 让 viewer 重连到主 PoP（保留 v1 兜底）
5. **安全**：relay 会话用内部 token + 每会话随机凭据，防止跨 PoP 伪造

## 风险与决策点

| 风险/决策 | 说明 |
|---|---|
| SCTP 跨 SFU 桥复杂度 | v3 主要工作项；先做媒体中继（RTP）验证，再做 data channel 桥 |
| 延迟叠加 | 中继 +1 跳；验收需真实跨区链路测 p99（#8 方法论） |
| 是否需要 v3 | 若产品场景是"单人多设备同区"，v1/v2 足够；跨区协作才需要 v3——**由产品决策** |
| 动态注册表后端 | v2 增强需选型（etcd/Redis/DB）——由部署团队决策 |

## 验收（未来）

- [ ] v2：动态 `room→PoP` 注册表 + 容灾切换（无需桥接）
- [ ] v3（若立项）：双 PoP 真实部署 → viewer 跨区加入 → 媒体 + input 均通 → 延迟 p99 达标 → 失败回退重定向

## 结论

**当前推荐：v2 = 动态注册表 + 保持同 PoP（不建实时媒体桥接）；v3（SFU 级联 relay）立项后再实施。**

> 更新：v2 动态注册表已实现（#154，`POP_REGISTRY_FILE`/`POP_REGISTRY_TTL_SECS`，文件共享版）。
