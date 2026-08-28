# P0 验收报告（#553）——SIP-WSS 标准性 + 1:1 P2P 音视频

- 日期：2026-08-24
- 环境：Windows 10 本机（aerodesk main @ a14499f + PR #581 修复集）
- 验收对象：#551（signal SIP-WSS）+ #552（客户端 SIP + P2P）+ #553（P0 验收）
- 拓扑口径（#561 定稿）：1:1 = P2P 直连（SFU 不在环）；≥3 人 = 全员 SFU

## 1. 本机 1:1 P2P 音视频打通 ✅

两客户端经 signal（SIP）完成 REGISTER/INVITE/ICE 协商后 P2P 直连：

```
aerodesk-agent --role publisher --encoder screen --audio --room accept-p2p-2
aerodesk-agent --role viewer    --audio --room accept-p2p-2
```

| 项 | 结果 | 证据 |
|---|---|---|
| SIP REGISTER（Digest） | ✅ | publisher `SIP registered: accept-p2p-2`；viewer 同 |
| INVITE → P2P | ✅ | viewer `SDP negotiated, awaiting ICE... → ICE connected` |
| 视频端到端 | ✅ | viewer `RECEIVED: 786 frames, 4.6MB, 12 keyframes, DECODED 持续增长` |
| 音频轨 | ✅ | publisher `Windows system audio capture started (WASAPI)`；callee mids 含 `audio=Some(Mid)`；viewer 侧 PCMU 收流 |
| 输入回传（data channel） | ✅ | publisher `inject: seq=15 MouseMove`（viewer 经 P2P data channel 注入） |

## 2. 媒体路径证据：目的地址 = 对端（SFU 不在环）✅

本机无 tshark/Wireshark，以运行时日志证据替代抓包 pcap（等价性：ICE 协商后的
实际收包源地址 + SFU 全程零参与）：

| 证据 | 值 |
|---|---|
| publisher 本地媒体端口 | `127.0.0.1:51731` |
| viewer 本地媒体端口 | `127.0.0.1:51730` |
| viewer 收包源地址分布 | **5249 包 100% 来自 127.0.0.1:51731（对端媒体端口）**；0 包来自 SFU 3478 |
| SFU 日志（accept-p2p-2 房间） | **0 条**（无客户端加入/无转发记录） |
| SFU /healthz | `{"clients":0,"shards":8,"status":"ok"}`（会话期间无 SFU 客户端） |
| 包类型 | STUN(0x01)/DTLS(0x16)/SCTP(0x14)/RTP(0x17)——P2P 直连媒体栈 |

结论：**媒体包目的/源地址均为对端，SFU 全程不在环**（#552 验收标准第 1 条达成）。

## 3. 标准 SIP 客户端验证（协议无私有依赖）✅

手写 RFC 3261 报文（Node/Python 实现，无 aerodesk 私有扩展）双传输验证：

| 传输 | REGISTER(Digest 401→200) | INVITE→100/180→200(SDP 透传)→ACK |
|---|---|---|
| **UDP 5060**（RFC 3261） | ✅ UA-A/UA-B 双端 | ✅ SDP answer 端到端透传（m=application 行确认） |
| **WSS 3061**（RFC 7118） | ✅ UA-A/UA-B 双端 | ✅ 同上 |

- realm= aerodesk、Digest 算法 MD5、CSeq 认证重发递增（RFC 3261 §22.2）
- 标准语义响应：401（质询）/403（口令错）/200（成功）
- **验收发现并修复**：rsipstack 0.6.4 secure WebSocket listener 未做 TLS
  （`MaybeTlsStream::Plain` 写死）——"SIP/WSS" 实为明文 WS，Digest 凭据与 SDP
  明文暴露，标准客户端 wss:// 必然失败。已 vendor 修复（PR #581）：TLS
  accept（与 TlsListenerConnection 同源 TlsAcceptor）+ 手动 RFC 6455 握手
  （钨钢 accept_hdr_async 与 tokio_rustls server 流组合解析异常）。修复后
  WSS 全链路 PASS，明文 WS 连 3061 被正确拒绝。

## 4. NAT 场景（srflx/relay）⏳ 留 P1

- 本机无 NAT 环境，未做公网 srflx（STUN）实测；TURN relayed 候选已接线
  （#570：AERO_TURN_URLS/USERNAME/CREDENTIAL + add_relay_candidate）
- 按 #553 任务清单原文：relay（TURN）兜底确认留 P1——**交接项**
- SIP_SIGNALING.md §10 的「断 UDP→TURN→强制 SFU 回退演练」同样留 P1

## 5. 回归：/healthz、/metrics、/config ✅

| 端点 | 结果 |
|---|---|
| SFU /healthz | `{"clients":0,"shards":8,"status":"ok"}` ✅ |
| signal /healthz | `{"clients":0,"pop":"local","rooms":0,"status":"ok"}` ✅（P3.1 起字段变为 `status/pop/sip`，历史断言留档） |
| signal /metrics/prometheus | `sip_registrations 4`、`sip_calls_established 2`、`sip_calls_terminated 0` ✅（#551 新指标） |
| signal /metrics/prometheus | `aerodesk_signal_clients/rooms/bridges` 保留 ✅（P3.1 起改 `sip_*` 指标，历史断言留档） |
| /config | `{"turn":null}`（本机未配 TURN，格式正确）✅ |

## 6. 问题清单与 P1 交接项

| # | 问题 | 状态 |
|---|---|---|
| 1 | rsipstack WSS 无 TLS（验收发现） | **已修复**（PR #581 vendor patch） |
| 2 | CI 大面积红（#552 迁移后脚本/门控/环境漂移） | PR #581 修复中（macos/windows test 待绿） |
| 3 | 上游 str0m master 漂移（netem 0.4）致 Cargo.lock 双版本 | 已修复（PR #581 固定 rev） |
| 4 | simulcast 三层 e2e（q/h/f + f>q）需原生端会议发布（agent 302 升级 + SFU 会议发布 = P2 完整实现） | P2 恢复 |
| 5 | Web viewer ↔ 原生被控端互通需 Web SIP-WSS（方案 A，§10 验收增补） | P1 后 |
| 6 | NAT srflx/relay 公网实测（含回退黑屏时长上限） | P1 交接 |
| 7 | ubuntu CI codec SVT-AV1 死锁防护（watchdog 全覆盖 + 锁超时） | 已修复（PR #581） |

## 7. 验收标准对照

- [x] 媒体路径证据齐全（对端直连、SFU 不在环）——§2
- [x] 标准 SIP 客户端可接入（协议无私有依赖）——§3（UDP + WSS 双传输）
- [x] P0 验收报告含问题清单与 P1 交接项——本报告 §4/§6

**结论**：P0 验收项（#553 核心三标准）全部达成；CI 恢复（PR #581）绿后即可关闭
#551 → #552 → #553。
