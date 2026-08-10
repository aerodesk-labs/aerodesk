# 跨 PoP 媒体桥接（#216 M1/M2）

## 目标
房间成员跨 PoP 实时互通（媒体 + data channel）：viewer 在其 PoP（PoP-B）加入钉在另一
PoP（PoP-A）的房间时，**不经 Redirect** 经桥接客户端收到 PoP-A 媒体（**不重编码**，
RTP 载荷直通），且输入/剪贴板等 data channel 消息双向跨 PoP。

## 架构（M1，本地双 SFU 模拟双 PoP）

```
PoP-A (14600 系)                        PoP-B (14700 系)
  publisher ──RTP──▶ SFU-A ◀──view── bridge ──publish──▶ SFU-B ◀──RTP── viewer
                          (aerodesk-bridge: view PoP-A + publish PoP-B)
```

- **bridge（`crates/aerodesk-bridge`）**：以 Viewer 身份连主 PoP 收流，以 Publisher
  身份连本 PoP 转发。`ClientEvent::Media` 拿到 str0m 去包化的**编码载荷**
  （`MediaData.data`，如 H.264 NAL），经本 PoP 端点 `Writer::write` **原样重打包**
  （新 RTP 头/SSRC，载荷不重编码；bridge 不链接任何编码器）。
- **关键帧**：bridge 初次加入会错过首帧 IDR，主动向主 PoP publisher 连发 3 次 PLI
  （0/1/2s）；本 PoP viewer 的 KeyframeRequest 也会实时回传到主 PoP publisher
  （`Writer::request_keyframe`）。
- **M2（data channel 桥）**：按 label 白名单（input/file/cursor/cmd，跳过
  offer/answer 与 control）双向转发 `ChannelData`——本 PoP viewer 的 input 经 bridge
  到主 PoP publisher（`inject` 生效），主 PoP 的剪贴板/文件/光标反向。
- **M3（文件，#230）**：跨 PoP 文件传输——PoP-B viewer `--send-file` → SFU-B →
  bridge（file 白名单）→ SFU-A → PoP-A publisher `--recv-dir`，落盘 sha256 与源一致。
  bridge 线程 16MB 栈（数据通道大块发送在默认 2MB 栈会溢出，见 RULE）。
- **失败回退**：任一条腿连不上则 bridge 非零退出（v1 Redirect 兜底保留给上层编排）。

## 运行

```sh
# 1) 起双 PoP（脚本内置端口：PoP-A 14600 系 / PoP-B 14700 系）
scripts/bridge-e2e.sh          # 全自动：起双 SFU+signal → PoP-A publisher → bridge → PoP-B viewer → 断言

# 手动
aerodesk-bridge --remote-signal ws://127.0.0.1:14603 --local-signal ws://127.0.0.1:14703 \
  --room bridge-demo [--codec h264|hevc|vp9|av1|default]
```

## 验证（macOS M4，debug 混合，2026-08-10，连跑 3 次全 PASS）

| 项 | 结果 |
|---|---|
| PoP-B viewer 跨 PoP 收流 | RECEIVED 31-75 帧 / DECODED 6-32 |
| bridge 转发 | ~32-73 包/次，关键帧 1-2（初始 PLI 生效） |
| M2 input 跨 PoP | PoP-A publisher 收到 `inject: seq=0 MouseMove`，data_forwarded=32/次 |
| M3 文件跨 PoP | 512KB sha256 一致（连跑 2 次） |
| 双 SFU 客户端 | PoP-A=2（publisher+bridge-view），PoP-B=2（bridge-pub+viewer） |
| 无重编码 | bridge 无编码器依赖，载荷原样重打包 |
| panic/abort | 0 |

## 状态
- M1 ✅（本地双 SFU 端到端媒体互通）
- M2 ✅（data channel 桥：input 已验证；clipboard/file/cursor 同机制按白名单转发）
- M3 ✅（本地：跨 PoP 文件传输 sha256 一致；真实多 PoP 部署验收：延迟 p99、失败回退 ⏳）

## 关联
- #216（立项）、ADR-0004（v3 设计）、#146/#150/#154（v1/v2）、#8（延迟验收）
