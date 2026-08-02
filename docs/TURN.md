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
native 客户端（aerodesk-core，P2）：信令 `Joined` 消息携带
`TurnConfig`（aerodesk-protocol::signal）。

## 验证

```sh
# 1. STUN 可达
python3 -c "import socket,struct,os; s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.settimeout(3); s.sendto(struct.pack('!HHI12s',1,0,0x2112A442,os.urandom(12)),('127.0.0.1',3478)); print('OK' if s.recvfrom(2048) else 'FAIL')"

# 2. 凭证 + 分配（coturn 自带工具）
turnutils_uclient -W <SECRET> -u aerodesk <TURN_IP>
```
