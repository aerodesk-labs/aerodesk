# SIP 信令映射规范（SignalMessage ↔ SIP，#549/#550）

信令从自研 JSON 改为**标准 SIP**（动机：标准化本身，开源互联互通；与 PSTN/PBX 无关）。
媒体 P2P 优先 → TURN 中继（SFU 内嵌）→ SFU 兜底（≥3 人全员 SFU，见 §4.1）。本规范是 #550 的入口交付物与验收对照表：
评审稿（v0.1/v0.2）见 #550 评论，本文件为评审通过后的定稿落盘（v0.3：多方拓扑口径定稿）。

- **传输矩阵（v0.2 定调）**：仅 **Web 端走 SIP-WSS**（RFC 7118，浏览器唯一可用传输）；
  **桌面/CLI 原生端直连标准 SIP**——TLS(5061) 默认（信令含 Digest 凭据与 SDP，公网必须加密），
  TCP/UDP(5060) 内网/调试可选。UDP 受 RFC 3261 §18.1.1 MTU 约束：含 data channel m-line +
  DTLS fingerprint 的初始 SDP 即使 trickle 瘦身仍可能超 1300B，**超 MTU 必须切 TCP**。
  SIP 语义与传输解耦，本规范映射表与传输无关；signal 多传输监听（TLS + WSS），
  Contact 按传输分别绑定（RFC 5626 flow）。rsipstack 原生支持 UDP/TCP/TLS/WS/WSS（S1 已确认）。

## 1. 身份与寻址模型

| 现有概念 | SIP 模型 |
|---|---|
| 设备 ID / 房间名（如 AD-01AB3C） | AoR：`sip:<device-id>@<domain>` |
| presence 常驻房间 | 注册状态（REGISTER 绑定 AoR→Contact） |
| peer_id（连接级） | Contact / 传输连接；对话级标识 = Call-ID + From/To tag |
| Role::Publisher / Viewer | 不再是信令角色：UAS（被叫）≈ 被控端、UAC（主叫）≈ 观看端；publisher/viewer 语义留在媒体层（SDP direction / SFU mid） |
| auth_token | Digest 口令：username = device-id，realm 可配；服务端仅存 HA1 = H(user:realm:token) |

## 2. 变体全量映射表（13/13 覆盖）

| # | SignalMessage | SIP 承载 | 语义说明 |
|---|---|---|---|
| 1 | `Ping` | **（消失）** | 现 Ping 是服务端发送队列 drain 的实现工件；rsipstack 传输层常活后无此需求。连接保活 = 传输层 keepalive（WSS ping/pong、RFC 5626 flow）；会话保活 = Session-Timer（§5） |
| 2 | `Join{room,role,auth_token,dc_ready}` | `REGISTER` → `401` → `REGISTER`+Authorization → `200` | room→AoR；auth_token→Digest 口令；dc_ready 见本节末注 |
| 3 | `Joined{peer_id,peers,turn}` | `200 OK`(REGISTER) | peers：P0 不下发 roster（在线 = 注册存在）；turn：**不进 SIP 面**，沿用 `/config` HTTP 签发（#549 已定） |
| 4 | `Redirect{pop,url,reason}` | `302 Moved Temporarily`（Contact = 目标 PoP） | 多 PoP，P0 可后置；亦用于 P2P→SFU 升级重定向（§4.1） |
| 5 | `Description{from,to,description}` | `INVITE` / `200 OK` 的 SDP body；重协商 = re-INVITE | signal 透传不解析；SFU 模式 = 客户端与 SFU UAS 的对话（见 §4 注） |
| 6 | `IceCandidate{from,to,candidate}` | `INFO`，Content-Type: `application/trickle-ice-sdpfrag`（RFC 8840） | 字段对齐 candidate / sdpMid / sdpMLineIndex |
| 7 | `PeerLeft{peer_id}` | **语义拆分**：对话内对端离开 = `BYE`；presence 离线 = `REGISTER` expires=0 / 注册过期 | 现 PeerLeft 混淆「媒体会话结束」与「在线消失」两类语义；SIP 内建分开——此类翻译丢失 bug 由标准消除 |
| 8 | `Call{from,target,call_id,timeout_ms}` | `INVITE`（含 SDP offer）；Call-ID = call_id；timeout_ms → `Expires` 头 + 主叫 CANCEL 定时 | target → Request-URI AoR |
| 9 | `CallRinging` | `180 Ringing` | |
| 10 | `CallAccepted` | `200 OK`（含 SDP answer），UAC 回 `ACK` | 免授权静默接听 = UAS 直接 200（可省 180） |
| 11 | `CallRejected{reason,error_code}` | 4xx/6xx 响应码（§3）+ 正文携带 error_code | SIP 响应码为权威语义；机器码放 `application/json` 正文，双栈期客户端直接消费 |
| 12 | `Hangup{reason}` | `BYE`（Reason 头，RFC 3326） | 对话内任一方 |
| 13 | `Error{message}` | `400` / 4xx / 5xx + Warning；畸形报文 400 | 延续 #542 口径（400 不 panic） |

**注（signal_ready / dc_ready，#467）**：P2P 模式下 DCEP 在 SDP `m=application` 数据通道
协商内完成，就绪 = data channel onopen 事件，**不再占用信令消息**；SFU 兼容路径在双栈期
保留现有 JSON 字段。

## 3. 拒绝/错误码映射

| error_code | SIP | 说明 |
|---|---|---|
| `user_rejected` | `603 Decline` | 用户主动拒接 |
| `busy` | `486 Busy Here` | |
| `offline` | `480 Temporarily Unavailable` / `404 Not Found` | 注册过期不可达 / AoR 不存在 |
| `timeout` | `408 Request Timeout`（proxy 生成）；被叫侧 `487 Request Terminated` | 主叫 CANCEL → UAS 回 487 |
| `control_disabled`（未开启被控） | `403 Forbidden` + 正文 error_code | 策略拒绝（#545 语义） |
| 未知能力 | `420 Bad Extension`（Unsupported 头） | 版本/能力协商 |
| 未实现方法 | `501 Not Implemented` | 严格子集纪律（§6） |

## 4. 呼叫状态机（1:1 P2P 基准时序）

```
观看端(UAC)            signal(Registrar+Proxy)              被控端(UAS)
   | REGISTER → 401 → REGISTER+Authorization → 200 OK          |
   |                        ← REGISTER+Authorization → 200 OK  |
   | INVITE(sip:<device>@dom, SDP offer, Call-ID, Expires)     |
   |---------------------->|----------- INVITE --------------->|
   |<------ 100 Trying ----|                                   |
   |                       |<--- 180 Ringing（弹出确认窗） -----|
   |<------ 180 -----------|                                   |
   |                       |<--- 200 OK（SDP answer） ----------| ← 用户确认/免授权直答
   |<----------------------|                                   |
   |---------------------------- ACK ------------------------->|
   |<=========== ICE(host→srflx→relay) + DTLS + SCTP =========>|
   |<=========== 视频 + data channel（键鼠/文件/bash） ========>|
   | INFO(trickle-ice-sdpfrag) ←------------------------------>| （双向，随候选）
   |---------------------------- BYE（Reason） ---------------->|
   |<--------------------------- 200 OK(BYE) ------------------|
```

异常分支（均为标准语义，无需自研状态机）：

- 用户拒接 → `603`；未开启被控 → `403`；忙 → `486`
- 确认超时 → 主叫 `CANCEL` → UAS `487`（或 proxy `408`）
- 被叫离线 → proxy `480`/`404`
- **确认期间关闭被控**（#539 竞态）→ early dialog 内 UAS 状态变化，直接改回 `603`/`403`，语义明确无竞态

**注（SFU 模式）**：SFU 以 UAS 身份持有 AoR（如 `sip:sfu@<domain>`）。P2P ICE 失败回退 =
向 SFU 发新 INVITE（或 re-INVITE 升级），客户端始终只有 SIP 一条信令路径；回退黑屏时长
上限入 #553 验收。多方（≥3 人）拓扑与升级时序见 §4.1。

## 4.1 多方拓扑：≥3 人全员 SFU（无混合，2026-08-22 定调）

口径：

- **1:1（1 被控 + 1 观看）= P2P**，媒体不经服务器（§4 基准时序）。
- **≥3 人（1 被控 + ≥2 观看）= 全员 SFU**：被控端与每个观看端各自与 SFU 建对话，
  媒体全部经 SFU 转发。**无混合态**——同一呼叫内不允许部分观看 P2P、部分观看 SFU 并存
  （避免被控端双发吃带宽、两套媒体路径并存的复杂度）。
- **决策点在被控端**：它是媒体源且掌握当前观看数；signal proxy 保持透明（§4），不承载呼叫策略。
- **会议 AoR 由设备 AoR 确定性推导**：`sip:view-<device-id>@<domain>`，由 SFU 以 UAS 身份应答；
  参与方无需额外带路信息即可自行构造。
- **不做降级（v1）**：观看数从 ≥2 回落到 1 仍留在 SFU 直至呼叫结束，避免 P2P↔SFU 振荡。

升级时序（P2P → SFU，第 2 个观看者入呼触发）：

```
观看1(V1)          被控(C)            signal(proxy)        观看2(V2)         SFU
   |<=========== P2P 已建立（ICE/DTLS/SCTP 直连） ==========>|                  |
   |                |<--------- INVITE(sip:<C>@dom) --------|                  |
   |                | 已有 1 路 P2P → 升级全员 SFU           |                  |
   |                |-- 302（Contact: sip:view-<C>@dom） --->|                  |
   |                |                     (ACK)             |                  |
   |                |                                       |-- INVITE(view-C)->|
   |<- BYE（Reason: SIP;cause=302）                         |                  |
   |--- 200(BYE) -->|                                       |                  |
   |-- INVITE(sip:view-<C>@dom) --------------------------------------------->|
   |                |-- INVITE(sip:view-<C>@dom, 发布方向） -------------------->|
   |                |   <==== 各方与 SFU 完成 200/ACK，ICE 指向 SFU ====>        |
   |<==================== 媒体全员经 SFU 转发 ================================>|
```

要点：

- V1 从 BYE 的 `Reason: SIP;cause=302`（RFC 3326）得知「呼叫已转移至会议 AoR」，
  自行推导 view AoR 并重新 INVITE；无需额外信令字段。
- V2 走标准 302 重定向：收到 Contact 后向会议 AoR 重新 INVITE。
- 升级期间媒体短暂中断，黑屏时长上限与 P2P→SFU ICE 回退一并入 #553 验收。
- 被控端已在 SFU 态时，后续观看 INVITE 一律直接 302。
- 竞态收敛：被控端串行处理入呼——空闲→P2P；已有任何活动对话→302。
  多观看同时入呼各自收 302，结果一致。

## 5. 近期 bug 类 → SIP 内建机制（标准红利对照）

| 事故 | 根因 | SIP 机制 |
|---|---|---|
| PeerLeft 语义转发丢失（stuck publisher） | 自研消息一语多义 | BYE / 注册注销语义内建 |
| 30s 无条件挂断造孤儿会话 | 服务端硬编码定时器 | Session-Timer（RFC 4028），re-INVITE 刷新 |
| 确认对话框竞态（#539） | 自研状态机边界模糊 | early/confirmed dialog + CANCEL/603 |
| 双端版本漂移（Mac 缺 PeerLeft 处理） | 无能力协商 | Supported/Require/User-Agent；`Require: aerodesk.p2p` → 420 |
| 呼叫/挂断时 presence 重连闪烁 | 连接与会话生命周期耦合 | RFC 5626 flow 保活 + 注册刷新；对话独立于注册抖动 |

## 6. 严格子集（实现清单 vs 501 清单）

- **实现**：`REGISTER` / `INVITE` / `ACK` / `BYE` / `CANCEL` / `INFO` / `OPTIONS`（保活+能力探测）
- **一律 501**：`SUBSCRIBE` / `NOTIFY` / `PRACK`（100rel 不要求）/ `UPDATE`（Session-Timer
  刷新走 re-INVITE）/ `REFER` / `PUBLISH` / 其余全部
- **响应子集**：100 / 180 / 200 / 302 / 400 / 401 / 403 / 404 / 407 / 408 / 420 / 480 /
  486 / 487 / 501 / 603 + Reason 头
- **forking**：P0 单 Contact，重复 REGISTER 覆盖（不并行响铃）

## 7. P0 非目标

SUBSCRIBE/NOTIFY presence roster、forking、GRUU、MESSAGE（呼叫提示，可后置）、
302 多 PoP（可后置）、TLS 客户端证书

## 8. 迁移与兼容约束（#549 已定口径）

- 双栈并存 + feature gate；JSON 协议在 SIP parity 前不下线
- `User-Agent` 携带协议版本；option-tag `Require: aerodesk.p2p` 能力协商
- Digest 迁移：现有 token 即口令，服务端仅存 HA1（迁移期旧 token 一次性登记）
- TURN 凭证签发留在 `/config` HTTP
- **媒体核心不 import SIP 类型**（#552 约束）：SIP UA 收敛在 protocol/core 信令层，
  对媒体层只暴露 SDP/ICE 参数

## 9. 对 #551/#552 的接口形态建议

protocol crate 暴露与 SignalMessage 同构的**语义事件**（如
`IncomingCall{dialog_id, offer_sdp, from_aor}`），core 状态机按事件迁移而非按报文迁移——
本映射表同时是报文对照与事件 API 对照。客户端须处理两类多方拓扑事件（§4.1）：
**跟随 302**（向 Contact 会议 AoR 重新 INVITE）与**识别 BYE 的 `Reason: SIP;cause=302`**
（推导 view AoR 并重新 INVITE）。

**遗留开口（已收口，v0.3）**：多方拓扑时序图已定稿（§4.1）——口径简化为
「1:1 = P2P；≥3 人 = 全员 SFU，无混合态」，不采用「2 人 P2P + 第 3 人 SFU」混合方案。

## 10. 验收增补（#551/#553，v0.2）

- 原生端（SIP/TLS）与 Web 端（SIP-WSS）互呼互通：同一 AoR 体系、跨传输呼叫时序一致
- P2P 抓包证据：媒体不经服务器；断 UDP→TURN→强制 SFU 回退演练；回退黑屏时长上限

## 11. 无人值守密码（#503-4，v0.4）

**固定密码（无人值守）**：每台设备一个常驻口令，存在 signal 设备表
（`SIP_DIGEST_USERS`，未逐设备配置时回退首个 `AUTH_TOKEN`）。**INVITE 授权**——
呼叫设备必须证明知道该设备口令：

```
观看端(UAC)                    signal(proxy)                      被控端(UAS)
   | INVITE（无 Proxy-Authorization）|
   |----------------------------->|
   |<- 407 Proxy Authentication Required（Digest 质询，同 REGISTER）|
   | INVITE + Proxy-Authorization（Digest 以「被叫设备 ID + 被叫口令」应答）|
   |----------------------------->|------- INVITE（透传）---------->|
   |                              |<---- 200 OK（SDP answer）--------|
   |<---- 200 OK ----------------|                                  |
```

- 口令错 / 未知设备 → `403`（不泄露存在性）；目标设备无任何口令配置（开放部署/
  未配置设备）→ 不设卡，与旧行为一致
- 未带凭据（旧客户端/无 call_password）→ `407` 终局，客户端映射
  error_code=`auth_required`——不静默放行
- 设备侧口令配置入口：agent `--token` / 桌面端「默认访问凭证」（= Digest 口令，
  与 REGISTER 同凭据，§8 迁移期同一凭据）；主叫侧 agent `--call-password`
  （或 `AERO_CALL_PASSWORD`，缺省回退自身 `--token`——单 token 部署即被叫口令）

**临时密码（主控端发起、带有效期）**：signal 管理端点签发，有效期等效固定口令：

```
POST   /admin/temp-password {"device_id":"AD-XX","ttl_secs":300}  → {"device_id","password","ttl_secs","expires_at_secs"}
DELETE /admin/temp-password/<device>                              → {"device_id","revoked":bool}
```

- 鉴权：`Authorization: Bearer <SIP_ADMIN_TOKEN>`（缺省回退首个 `AUTH_TOKEN`）；
  未配置管理 token → 503
- ttl 钳制 60..86400s（缺省 300s）；签发覆盖同设备旧临时密码；到期自动失效
- 主控端用法：`aerodesk-agent --temp-password AD-XX --ttl 300 --token <admin>`
  拿临时密码 → `--call-password <临时密码>` 呼叫（或填到桌面端连接密码）
- 口令为 8 位 CSPRNG 随机（getrandom 拒绝采样，去易混淆字符），与桌面端
  「一次性密码」同构

**已覆盖/未覆盖**：临时密码生效域 = INVITE 授权（与固定口令并列校验）；REGISTER
仍按设备表口令。Web 端（JSON WSS 面）与 SIP 原生端互通为既有 P1 缺口，Web 端呼叫
配置了口令的设备前需先支持 Proxy-Authorization。桌面端「逐设备连接密码」输入框为
后续项（当前以本机访问凭证为呼叫口令）。
