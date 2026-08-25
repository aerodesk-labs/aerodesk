# Web viewer ↔ 原生被控端互通：Web SIP-WSS 客户端面设计（#583）

状态：设计定稿（2026-08-25）。本文件回答「浏览器观看端如何以标准 SIP 直连原生被控端」，
是 #553 验收 §10「原生端（SIP/TLS）与 Web 端（SIP-WSS）互呼互通」的实施方案，
对应 #553 报告 §6 问题 5（P1 后交接项）。

## 1. 现状调研结论

### 1.1 web/index.html：遗留 WSS JSON 观看端（不可呼叫原生端）

- `web/index.html` 是 #552 迁移前的 WSS JSON 观看端/发布端页面：`join{room,role,auth_token}` →
  SFU 房间模型（`ws://host:3003/ws`、`wss://host:3001/ws`），SDP 经 JSON `description` 消息交换，
  ICE 非 trickle（#17 等 gathering 完成后整体发送），输入事件经 `input` data channel 回传。
- #552 后原生端（agent/desktop/mobile）已全部迁移到 SIP（REGISTER/INVITE），**WSS JSON 面
  与原生端无信令交集**：web-e2e.sh 只能在「浏览器↔浏览器 + SFU」的 WSS 房间内闭环，
  无法对 CLI 被控端发起呼叫（web-e2e.sh 头注释明示「互通缺口待 Web SIP-WSS」）。
- 结论：**index.html 保留不动**（web-e2e.sh 依赖其 WSS 房间闭环），新增独立页面
  `web/sip-viewer.html` 承载 SIP-WSS 观看端；后续可再评估 index.html 的退役。

### 1.2 signal 3061：标准 SIP 协议面（已具备 Web 接入所需全部能力）

`crates/aerodesk-signal/src/sip_server.rs` 已实现（#549/#550/#552，P0 已验收）：

- **传输**：`SIP_WSS_PORT=3061` 开启 WSS（RFC 7118）监听；rsipstack 接受不带 subprotocol 的
  连接（对 `sec-websocket-protocol: sip` 回显确认）。另有 TLS 5061（原生默认）/ UDP 5060。
- **认证**：REGISTER → `401 Unauthorized`（WWW-Authenticate: Digest, realm 默认 `aerodesk`）→
  REGISTER+Authorization → 200。口令源：`SIP_DIGEST_USERS=user=token,...` 或
  `AUTH_TOKENS` 首个 token（`token_password` 兜底）；两者皆空时 `open_register=true`
  （跳过 Digest 直接注册——本地 e2e 即此模式）。
- **呼叫**：透明 INVITE 代理——A 腿（本端连接）→ B 腿（被叫注册 flow），100/180/200 中继、
  SDP body 字节透传、INFO（trickle-ice-sdpfrag）双向透传、CANCEL/BYE 级联。
- **严格子集**：REGISTER/INVITE/ACK/BYE/CANCEL/INFO/OPTIONS 实现，其余 501。
- **跨传输互通**：代理按注册 flow 路由，A/B 腿传输类型互不要求一致（WSS↔UDP 混合即为
  #583 场景）；`scripts/sip-accept-wss.py`（P0 验收通过）已证明 WSS 客户端可走完整呼叫闭环。

### 1.3 SDP 线上格式（本设计的关键发现）：原生端 = `{"type":"offer","sdp":"<标准 SDP 文本>"}`

- 原生端（agent/core）经 `str0m` 收发 SDP：`str0m::sdp::SdpOffer` 的 serde 信封即
  `{"type":"offer","sdp":"v=0\r\n…"}`（str0m src/sdp/mod.rs `sdp_ser!` 宏，
  答案同型 `{"type":"answer",…}`），INVITE/200 的 Content-Type 声明为 `application/sdp`。
- **这与浏览器 `RTCSessionDescription` 的 JSON 形状逐字段一致**——浏览器无需任何 SDP 转换：
  `JSON.stringify(pc.localDescription)` 直接作为 INVITE body，对端 200 的 body `JSON.parse`
  后喂 `setRemoteDescription`。
- str0m 的 SDP 解析器覆盖浏览器 offer 的典型属性（a=group:BUNDLE、a=extmap-allow-mixed、
  a=rtcp-mux、a=rtcp-fb、a=end-of-candidates、a=setup、m=application + a=sctp-port /
  a=max-message-size、a=ssrc/ssrc-group），未知属性落入 Unused 宽容处理——可解析浏览器 SDP。
- **候选内联（非 trickle）**：原生端在 offer/answer 构造前已加入候选（host + relay），
  候选随 SDP 内联；且 agent 对后到的 INFO trickle 候选**直接忽略**（main.rs 注释「候选内联」）。
  → 浏览器端必须等 ICE gathering 完成后整体发 offer（旧 index.html #17 同款策略）；
  trickle 互通需要原生端消费 INFO 候选，列为后续阶段（§5 任务分解）。

### 1.4 认证与 TLS（浏览器侧的现实约束）

- **Digest**：MD5（RFC 2617）：`HA1=md5(user:realm:password)`，`HA2=md5(method:uri)`，
  `response=md5(HA1:nonce:HA2)`——JS 内联 MD5（约 70 行）即可，无依赖。
- **TLS**：浏览器 WSS 无法绕过证书校验（无 `ssl.CERT_NONE`）。仓库内嵌开发证书
  （`certs/cer.pem`，自签 CN=str0m.test，仅 SAN str0m.test）**不能**用于 `wss://127.0.0.1:3061`。
  - 本地演示/CI：headless Chrome/Edge 加 `--ignore-certificate-errors`（web-e2e 同法），
    或把开发证书导入系统信任库；
  - 生产：`signal.aerodesk.io` 的正式证书（cert-renew-hook.sh 已铺 fullchain），SAN 覆盖
    Web 访问域名即可。

### 1.5 认证口令语义

被控端 AoR = `sip:<设备ID>@<domain>`（domain 默认 `aerodesk.test`，原生端一致）。
浏览器观看端以自己的设备 ID REGISTER（任意名即可，P0 无 roster 校验），
以「被控端设备 ID」为 INVITE 目标；Digest 口令 = 设备 token（`SIP_DIGEST_USERS` 或
`AUTH_TOKENS` 首项；本地 open_register 模式下任意口令/空口令均放行）。

## 2. 方案取舍：手写 SIP 子集 vs 引入 SIP.js/JsSIP

| 维度 | 手写子集（采纳） | SIP.js / JsSIP |
|---|---|---|
| 体积/依赖 | 0 依赖，页面单文件内联（MD5 ~70 行 + 报文构造 ~120 行） | ~100KB+ 第三方库，需 npm/CDN 引入 |
| 协议匹配 | 严格贴合本仓库窄子集（REGISTER/INVITE/ACK/BYE/INFO + 401/100/180/200），与 sip-accept-wss.py 完全同构（P0 已验证的报文形态） | 通用 UA：注册刷新/事务细节/Require 协商与本仓库子集有摩擦面，反而需要适配层 |
| 维护 | 报文模板单一、可对照 scripts/sip-accept-wss.py 双端自证 | 上游升级风险 + 仓库「不引入新依赖除非必要」铁律冲突 |
| 扩展 | trickle INFO、302 升级、多呼叫状态机可按需添加（模板化） | 相同能力但引入面大 |

**结论**：P0 严格子集 + 透明代理下，手写报文是更小更稳的切片；SIP.js/JsSIP 作为
「未来若需完整 UA 行为（SUBSCRIBE/多线/定时刷新管理）」的备选在 §5 任务 7 中评估。

## 3. 目标架构与呼叫时序

```
浏览器观看端 (UAC)              signal (WSS 3061)                原生被控端 agent (UDP 5060)
   | REGISTER → 401 → REGISTER+Digest → 200 OK                       |
   |----------------------------- INVITE (sip:<设备>@dom) ---------->|   SDP offer 内联候选
   |<----- 100 Trying / 180 Ringing -------------------------------- |
   |<----------------- 200 OK ({"type":"answer","sdp":…}) ---------- |   SDP answer 内联候选
   |----------------------------- ACK ----------------------------->|
   |<===================== ICE(host→relay) + DTLS + SRTP ===========>|
   |<============================= 视频流 ===========================|
   |----------------------------- BYE (Reason) --------------------->|
   |<----------------------------- 200 OK -------------------------- |
```

- 浏览器为 UAC：`recvonly` 视频 transceiver + `input` data channel（m=application 随 offer
  协商，为后续键鼠回传铺路；本阶段只建通道不传事件）。
- 非 trickle：等 ICE gathering 完成后 `JSON.stringify(pc.localDescription)` 作 INVITE body。
- 被控端免授权静默接听（UAS 直接 200，agent 行为），无需人工确认。
- 信令失败/超时：按阶段 timeout（REGISTER 10s、INVITE 30s），状态栏明示。

## 4. 最小骨架交付（本 PR）

`web/sip-viewer.html`（单文件，与 index.html 同风格）：

1. WSS 连接 `wss://<host>:3061`（subprotocol `sip`，可选）；
2. REGISTER → 401 → Digest（内联 MD5，RFC 1321 实现已过向量/差分测试）→ 200；`Expires: 120` + 60s 刷新定时器；
3. `RTCPeerConnection`：`addTransceiver('video',{direction:'recvonly'})` +
   `createDataChannel('input')`；等 gathering 完成（非 trickle，候选内联）；
4. INVITE（body = 本地 SDP JSON `{"type":"offer","sdp":…}`，与原生端 str0m 信封同构，
   Content-Type: application/sdp）→ 100/180 日志 → 200 解析 answer → `setRemoteDescription` → ACK
   （ACK 与 INVITE 同 CSeq 号、To 带 to-tag，RFC 3261 §13.2.2.4）；
5. `ontrack` → 画面显示；data channel onopen 日志；ICE 状态展示；
6. 断开 → BYE（Reason 头，对话内带 to-tag）→ 清理；对端 BYE → 状态复位。

页面参数（URL query 预填、输入框可改）：`target`（被控端设备 ID，必填）、`device`（本端 AoR，
默认 `web-viewer-<随机>`）、`signal`（默认 `wss://<host>:3061`）、`token`（Digest 口令，可选）。

### 4.1 实测结果（本 PR 验证）

环境：本机 Windows（signal `SIP_WSS_PORT=3061` + `SIP_UDP_PORT=5060`；
`aerodesk-agent --role publisher --encoder screen`；headless Edge + Playwright；
页面经 `python http.server 3004` 提供，浏览器 `wss://127.0.0.1:3061`）。

| 场景 | 结果 |
|---|---|
| 开放模式（open_register）完整呼叫：REGISTER→200、INVITE→100/200+ACK、ICE connected、视频播放（readyState=4）、input channel open | ✅ 多次通过 |
| Digest 关闭模式（`SIP_DIGEST_USERS` 真实 401 质询）：同上 + 断开 BYE 后被控端干净退出 | ✅ 通过 |
| 错误口令 | ✅ 被拒并明示（403 Forbidden） |
| 原生端对照（agent viewer → agent publisher） | ✅ 与 Web 端同路径 |

**实测发现（候选面互通，后续任务依据）**：

1. **SDP 线格式零转换成立**：str0m 信封 `{"type":"offer","sdp":"…"}` 与浏览器
   `RTCSessionDescription` JSON 逐字段一致，浏览器原生 SDP 可直接被被控端 str0m 解析
   （offer 7498B+ 实测解析通过，video mid 反演正确）。
2. **mDNS 候选被 str0m 丢弃**：str0m 候选解析只认 `IpAddr`（sdp/parser.rs），浏览器
   `a=candidate:…local`（mDNS 混淆）解析失败落入 Unused；ICE 依赖浏览器主动发起
   check 后在 str0m 侧生成 prflx 候选收敛——可用但有竞态（agent 直连 5s ICE 时限）。
3. **127.0.0.1 候选缺失竞态**：agent 在回环信令 URL 下绑定 127.0.0.1 并仅通告该 host 候选；
   本机 Chrome 偶发只通告局域网 IP（172.19.44.184，无回环）→ 候选对无法形成 → ICE 超时。
   **规避配方（本 PR 实测确定性通过）**：publisher 用非回环信令 URL
   （`--signal ws://<LAN-IP>:3003`）→ agent 绑 0.0.0.0 + 出接口 IP 候选 → 与浏览器
   同网段候选配对，4/4 全绿。回环-单候选场景的收敛属于后续任务（§5 任务 4/5）。

验证方式（可复现）：`scripts/sip-accept-wss.py` 已验证协议面；Web 面按上表场景
（signal `SIP_WSS_PORT=3061` + publisher `--signal ws://<LAN-IP>:3003`，headless Edge
`--ignore-certificate-errors` 打开页面 → 连接 → 断言视频 readyState≥2 + 被控端日志
`ICE connected, starting generic stream (codec=H264)`）。

## 5. 分阶段任务分解（#583 后续）

| 阶段 | 内容 | 依赖/说明 |
|---|---|---|
| 1（本 PR） | Web SIP-WSS 最小骨架：REGISTER+Digest+INVITE+媒体接通 | 无（已完成并实测） |
| 2 | 键鼠回传：`input` channel 事件编码（沿用 web/index.html 的 InputFrame JSON + 归一化坐标）→ 原生端注入链路（agent 已消费 `input` label） | 骨架已建通道，仅需事件发送与断线处理 |
| 3 | 剪贴板双向 + 文件传输（file channel，#72 协议：meta/分片/Nack） | 复用 web/index.html 的 file 实现移植 |
| 4 | 候选面互通（实测发现 1-3 的收敛）：agent 侧消费 INFO trickle + 候选多接口通告（或 0.0.0.0 绑定策略），浏览器 `onicecandidate` → INFO sdpfrag（RFC 8840） | 涉及 agent/desktop 改动，独立 PR；收敛后回环 URL 场景不再有竞态 |
| 5 | TURN：浏览器接 `/config` 或 `AERO_TURN_*` 下发 relay 候选（非回环/公网场景） | 参照 index.html joined.turn 的 iceServers 注入 |
| 6 | 被控端拒绝/离线/忙的 SIP 语义呈现（403/480/486/603 + 正文 error_code） | 骨架已能区分 4xx/6xx，仅 UI 文案 |
| 7 | 评估 SIP.js/JsSIP 或浏览器发布端（屏幕共享 → INVITE 发布，SIP 语义为 UAC 带 sendonly） | 与任务 2-6 无强依赖；浏览器发布端为独立能力线 |
| 8 | index.html 退役决策：WSS JSON 面下线条件 = Web 端 SIP 能力 parity + web-e2e.sh 迁移到 sip-viewer.html | 与 #552 迁移口径对齐（JSON 在 SIP parity 前不下线） |

## 6. 验收对照（#553 §10 增补）

- [x] 原生端与 Web 端互呼互通：同一 AoR 体系、跨传输（WSS↔UDP）呼叫时序一致（本 PR 实测）；
- [x] Digest 认证（SIP_DIGEST_USERS 校验模式）端到端通过；错误口令 403 明示；
- [ ] P2P 抓包证据（媒体不经服务器）→ 阶段 2 完成后补 wireshark/ICE 日志证据；
- [ ] 断 UDP→TURN→SFU 回退演练（Web 侧）→ 阶段 4/5 完成后补。
