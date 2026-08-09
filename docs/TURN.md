# TURN 中继部署（coturn）

企业网 UDP 被封时，浏览器/客户端通过 TURN relay 兜底连接 SFU。

## 架构

```
客户端 ──ICE 失败──▶ coturn (UDP/TCP/TLS 443) ──▶ aerodesk-sfu
        ◀──信令下发临时凭证（/config）──────────┘
```

## 1. 部署 coturn（REST secret 模式）

Docker：

```sh
docker run -d --name aerodesk-turn --network host \
  instrumentisto/coturn \
  -n --use-auth-secret --static-auth-secret=<SECRET> \
  --realm=aerodesk.io --fingerprint \
  --listening-port=3478 --tls-listening-port=5349 \
  --min-port=49152 --max-port=49200
```

Homebrew（开发机）：

```sh
turnserver -n --use-auth-secret --static-auth-secret=<SECRET> \
  --realm=aerodesk.io --no-tls --no-dtls --fingerprint \
  --listening-port=3478 --listening-ip=0.0.0.0 \
  --external-ip=<PUBLIC_IP> --min-port=49152 --max-port=49200
```

生产注意：
- 开放 UDP 3478、TCP 3478、TCP/TLS 5349（或 443）
- `--external-ip` 必须填公网 IP（或 NAT 映射）
- 生产建议开 TLS/DTLS（`--cert --pkey`），或把 443 映射到 5349
- 多实例横向扩展：相同 secret，任意实例可验证凭证

## 1.5 SFU 与 coturn 端口组合（#185）

**端口冲突提醒**：`aerodesk-sfu` 默认媒体端口 **3478**（`MEDIA_PORT`，UDP/TCP/SSL-TCP），
coturn 默认也监听 **3478/5349**——**同机部署时端口冲突**，客户端拿到的 TURN 地址
（`turn:{host}:3478`）会打到 SFU 媒体端口而非 coturn，relay 不生效。

推荐组合（二选一）：

**方案 A（推荐）：SFU 媒体端口让给 coturn，SFU 用 443**
```sh
# coturn 保持默认 3478/5349（--network host）
# SFU 媒体端口改为 443（生产注释亦建议 443）：
SFU_MEDIA_PORT=443 TURN_SECRET=<SECRET> cargo run -p aerodesk-sfu
# TURN_URLS 默认仍指向 :3478/:5349（coturn 占用），无需覆盖
```

**方案 B：coturn 换端口，SFU 保持 3478**
```sh
# coturn：--listening-port=3479 --tls-listening-port=5350
# SFU 下发 TURN 时用 TURN_URLS 覆盖为实际端口：
TURN_URLS="turn:sfu.example.com:3479?transport=udp,turn:sfu.example.com:3479?transport=tcp,turns:sfu.example.com:5350?transport=tcp" \
TURN_SECRET=<SECRET> cargo run -p aerodesk-sfu
```

> 单机多 PoP 测试（multipop-e2e）已用 `SFU_MEDIA_PORT` 覆盖端口；
> 生产部署请按上表明确组合，避免 TURN relay 静默失效。

## 2. 配置 aerodesk-sfu

```sh
TURN_SECRET=<与 coturn 相同的 SECRET> \
TURN_URLS="turn:sfu.example.com:3478?transport=udp,turn:sfu.example.com:3478?transport=tcp,turns:sfu.example.com:5349?transport=tcp" \
cargo run -p aerodesk-sfu
```

- 未设置 `TURN_SECRET`：不下发 TURN 配置（纯直连模式）
- 凭证：coturn REST 规范（`username=<expiry>:<userid>`，
  `credential=base64(HMAC-SHA1(secret, username))`），1 小时有效，由
  `turn::generate_turn_credentials` 生成（见 crates/aerodesk-sfu/src/turn.rs）

## 3. 客户端

浏览器端自动生效：`GET /config` → `RTCPeerConnection({ iceServers })`。
native 客户端（aerodesk-core，#157 M2 已实现）：信令 `Joined` 消息携带
`TurnConfig`（aerodesk-protocol::signal），客户端在
`connect_live_role`/CLI `connect_inner` 中建立 TURN 传输（`TurnTransport`）并把
relayed 候选加入 offer（`typ relay`）；`MediaSocket` 双路收发——ICE 直连优先、
TURN 兜底，无 TURN 配置时行为不变。

- 信令与 SFU 均支持 `TURN_SECRET` + `TURN_URLS`（逗号分隔，覆盖默认 URL）
- 本地联调：`scripts/turn-e2e.sh`（需 `turnserver`，coturn ≥ 4.17.2；
  coturn 4.16.0 的 REST auth 有回归 #1534）
- 本地 e2e 因 peer 是 127.0.0.1，coturn 需加 `--allow-loopback-peers`（生产真机不需要）

## 验证

```sh
# 1. STUN 可达
python3 -c "import socket,struct,os; s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.settimeout(3); s.sendto(struct.pack('!HHI12s',1,0,0x2112A442,os.urandom(12)),('127.0.0.1',3478)); print('OK' if s.recvfrom(2048) else 'FAIL')"

# 2. 凭证 + 分配（coturn 自带工具）
turnutils_uclient -W <SECRET> -u aerodesk <TURN_IP>
```
