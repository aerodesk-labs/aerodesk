# TURN 中继（SFU 内嵌，coturn 可选）

企业网 UDP 被封时，浏览器/客户端通过 TURN relay 兜底连接 SFU。

## 架构（#191：SFU 内嵌 TURN+STUN）

```
客户端 ──ICE 失败──▶ SFU 内嵌 TURN (UDP :3479) ──▶ 同进程媒体
        ◀──信令下发临时凭证（/config）──────────┘
```

- **默认**：`TURN_SECRET` 设置且未显式 `TURN_URLS` 时，SFU 启动**内嵌 TURN+STUN server**
  （RFC 5389 Binding + RFC 5766 Allocate/CreatePermission/ChannelBind/Send/Data/Refresh，
  单进程，无 coturn 侧车），监听 `SFU_TURN_PORT`（默认 **3479**，与媒体 3478 不冲突，#185）。
- **兼容**：显式 `TURN_URLS`（逗号分隔）时走外部 coturn（老部署不变）。
- **凭证**：coturn REST 规范（`username=<expiry>:<userid>`，
  `credential=base64(HMAC-SHA1(secret, username))`），1 小时有效；SFU 与内嵌 server
  共享 `TURN_SECRET`，signals 服务照常通过 `TURN_SECRET`/`TURN_URLS` 下发。

## 1. 配置

### 1.1 内嵌模式（默认，推荐）

```sh
# SFU
TURN_SECRET=<secret> \
SFU_TURN_PORT=3479 \          # 默认 3479
cargo run -p aerodesk-sfu

# signal（照常下发）
TURN_SECRET=<secret> \
TURN_URLS="turn:sfu.example.com:3479?transport=udp" \
cargo run -p aerodesk-signal
```

- 开放 UDP 3479（TURN）与 UDP 49152-49200（relay 端口段）
- 生产建议媒体走 `SFU_MEDIA_PORT=443`（TCP/SSL-TCP 443），TURN 保持 3479

### 1.2 外部 coturn（可选，向后兼容）

```sh
turnserver -n --use-auth-secret --static-auth-secret=<SECRET> \
  --realm=aerodesk.io --no-tls --no-dtls --fingerprint \
  --listening-port=3478 --listening-ip=0.0.0.0 \
  --external-ip=<PUBLIC_IP> --min-port=49152 --max-port=49200
```

SFU/signal 设 `TURN_URLS="turn:sfu.example.com:3478?transport=udp,..."`。
生产注意：coturn 与 SFU 媒体端口同机冲突时用 `SFU_MEDIA_PORT` 错开（#185）。

## 2. 客户端

浏览器端自动生效：`GET /config` → `RTCPeerConnection({ iceServers })`。
native 客户端（aerodesk-core，#157 M2 已实现）：信令 `Joined` 消息携带
`TurnConfig`，客户端在 `connect_live_role`/CLI `connect_inner` 中建立 TURN 传输
（`TurnTransport`）并把 relayed 候选加入 offer（`typ relay`）；`MediaSocket` 双路
收发——ICE 直连优先、TURN 兜底，无 TURN 配置时行为不变。

## 3. 验证

```sh
# 1. STUN 可达（内嵌 server）
python3 -c "import socket,struct,os; s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.settimeout(3); s.sendto(struct.pack('!HHI12s',1,0,0x2112A442,os.urandom(12)),('127.0.0.1',3479)); print('OK' if s.recvfrom(2048) else 'FAIL')"

# 2. 全链路 e2e（内嵌模式默认；TURN_MODE=coturn 走外部）
./scripts/turn-e2e.sh
```
