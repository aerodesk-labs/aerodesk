# ADR-0007：SFU data channel 转发约定（多通道 / in-band / file，#192）

- 状态：已采纳（2026-08-09）
- 关联 Issue：#192（Web file 通道接入确认）、#72（文件传输）、#162（Web 重协商教训）

## 问题

Web 端文件传输（file data channel）两种尝试失败：
- 初始 offer 前创建 fileChannel → Web 观看/重连异常；
- 连接后创建 fileChannel → 数据到不了被控端（SFU 不转发）。

需要确认 SFU 对多通道 offer / 连接后新增通道 / file 标签的转发行为，并给出正确接入模式。

## 结论（已用 str0m 双端复现验证，`crates/aerodesk-sfu/tests/dc_multi_channel.rs`）

1. **多通道初始 offer（含 file）**：✅ 支持——6 个通道全部打开，file 双向数据转发正常
   （与 CLI 一致；`scripts/file-transfer-e2e.sh` 端到端字节一致）。
2. **连接后新增通道（in-band DCEP）**：✅ 支持——str0m 连接后 `add_channel().apply()`
   返回 None = 直接走 in-band DCEP（与浏览器 `createDataChannel` 一致），对端
   `ChannelOpen("file")` + 数据转发正常。
3. **`negotiated: true`（out-of-band，无 DCEP）**：❌ 不支持——str0m 只建无标签通道、
   不产生 ChannelOpen，SFU 找不到 label 转发，数据静默丢弃（现改为 warn 日志）。
   原因：SFU 侧无法仅凭 SCTP 流号得知通道 label（标准 WebRTC SDP 不携带 negotiated
   通道配置），必须双方应用层同 id 预配置。

## 约定（客户端接入 SFU 的 data channel 规则）

- **通道 label 固定**：`offer/answer`、`input`、`control`、`file`、`cursor`、`cmd`。
- **面向 SFU 的通道一律 in-band**：默认 `createDataChannel(label)`（不设
  `negotiated: true`）；连接后创建即可，无需 SDP 重协商（DCEP 自动）。
- **file 通道**：Web 端在 `signalChannel.onopen` 后 `rtc.createDataChannel('file')`
  （与 input 完全一致），SFU 按 label 转发到被控端同名通道。
- SFU 转发语义：除 `offer/answer`（SDP 交换）与 `control`（选层/显示器切换，SFU 消费）
  外，其余 label 的消息按 label 广播到房间内其它客户端同名通道。

## 验证

- [x] `dc_multi_channel`：多通道 offer 双向 file 转发 + 连接后 in-band 通道转发（2 测试）
- [x] `scripts/file-transfer-e2e.sh`：CLI publisher→SFU→viewer 文件 SHA-256 一致
- [ ] Web 端恢复实现（#189）后：Web→SFU→被控端文件落盘 sha256 一致

## 影响

- SFU 无转发逻辑改动（仅增加未注册通道告警）；#189 按约定恢复 file 通道即可。
