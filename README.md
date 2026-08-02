# AeroDesk — Remote Desktop（workspace）

全平台远程桌面的 Rust workspace：**WebRTC SFU 服务端 + 共享协议 + 跨平台客户端核心**。

> 仓库：<https://github.com/aerodesk-labs/aerodesk>
服务端与客户端共用 str0m（含 PulseBeam bwe-fixes fork）作为协议底座。

> 平台矩阵、技术选型与路线图见 [`docs/PRODUCT-PLAN.md`](docs/PRODUCT-PLAN.md)。
> PulseBeam 架构借鉴见 [`docs/borrow-from-pulsebeam.md`](docs/borrow-from-pulsebeam.md)。
> TURN 中继部署见 [`docs/TURN.md`](docs/TURN.md)。

## Workspace 结构

```
aerodesk/
├── crates/
│   ├── aerodesk-sfu/        # SFU 服务端（当前唯一可运行组件）
│   │   ├── src/main.rs   # run loop + 媒体/输入事件转发 + HTTPS 信令
│   │   └── src/tcp.rs    # UnifiedSocket：TCP + SSL-TCP（fake-SSL）+ RFC 4571
│   ├── aerodesk-signal/     # 独立信令服务：房间/认证/TURN 凭证/SFU 代理
│   ├── aerodesk-protocol/   # 共享协议：输入事件（input）+ 信令消息（signal）+ TURN 配置 ✅ 已定义
│   └── aerodesk-core/       # 客户端核心骨架：端点/媒体管线/信令 trait（P2 填充）
├── web/index.html     # 浏览器端（publisher=屏幕采集 / viewer=观看+输入）
├── certs/             # str0m.test 自签证书（开发用）
└── docs/              # 规划与调研

## TURN 配置

```sh
TURN_SECRET=<coturn static-auth-secret> cargo run -p aerodesk-sfu      # SFU（UDP/TCP/SSL-TCP 3478 + 内部 API 3002）
cargo run -p aerodesk-signal   # 信令（WSS 3001）
# 客户端 GET /config 自动获取 iceServers
```

## 架构

```
browser (publisher) ─┐                     ┌─ browser (viewer)
native   (publisher) ─┼─ WebRTC ─▶ aerodesk-sfu ─┼─ native   (viewer)
                      │  UDP/TCP/SSL-TCP   │
                      │  同端口 3478(dev)/443(prod)
                      └── input 数据通道（观看端→被控端）──┘
```

- 信令：浏览器连 WSS（aerodesk-signal :3001）→ Join → offer/answer 代理到 SFU
  内部接口（127.0.0.1:3002）；轨道增删走 `offer/answer` 数据通道
- 媒体：`MediaData` 选择性转发（不重编码）；simulcast 选层点已标注
- 输入：`input` 通道 JSON 事件（协议类型在 `aerodesk-protocol::input`）

## 运行

```sh
cargo run -p aerodesk-sfu
# 发布端: https://<host>:3000/?role=publisher   （浏览器采集屏幕）
# 观看端: https://<host>:3000/?role=viewer      （接收 + 发送输入事件）
```

## 验证

```sh
cargo test --workspace
cargo clippy --workspace
cargo fmt --check
```

已验证：UDP/TCP/SSL-TCP 同端口三候选、fake-SSL 握手字节级匹配、信令应答、
ICE-TCP candidate 输出（tcptype passive）。

## 路线图摘要

- P0 原型：✅ 完成（SFU + Web 双端）
- P1 服务端生产化：多核分片、coturn、独立信令、BitrateController、SVC 选层、模拟器测试
- P2 桌面客户端：Windows/macOS/Linux（aerodesk-core + 平台适配器）
- P3 移动端：Android 双角色、iOS 观看端
- P4 鸿蒙 + Web 收口
