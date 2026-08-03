# 服务端部署（生产化）

覆盖信令（aerodesk-signal）、SFU（aerodesk-sfu）、TURN（coturn）、
录制/审计与 TLS 自动化。对应 Issue #5。

## 组件拓扑

```
                        ┌────────────────────────────┐
 客户端 ──WSS(:443)────▶│ aerodesk-signal           │
   │                    │  JWT 认证 / 房间 / TURN 凭证 │
   │  WebRTC            └──────────┬─────────────────┘
   │   UDP:3478 / TCP:443          │ 内部 HTTP(S) :3002（SFU_TOKEN 保护）
   │        │                      ▼
   │        ├────────────▶ aerodesk-sfu（多核分片 /metrics）
   │        └────────────▶ coturn（ICE 中继兜底）
   └── 录制目录 RECORD_DIR（可选）
```

## 1. 环境变量总览

| 组件 | 变量 | 说明 |
|---|---|---|
| signal | `JWT_SECRET` | HS256 共享密钥；**生产必设**，开启 JWT 认证 |
| signal | `AUTH_TOKENS` | 静态 token（兼容模式；JWT 开启时忽略） |
| signal | `TURN_SECRET` / `TURN_URLS` | coturn REST 凭证下发 |
| signal | `SFU_URL` / `SFU_TOKEN` | SFU 内部接口 + 内部 token |
| sfu | `RECORD_DIR` | 录制/审计目录（可选） |

## 2. TLS 证书自动化

生产只开 WSS/443 + SSL-TCP 443，证书自动续期：

**Let's Encrypt（推荐，公网）**：Caddy 或 nginx 反代终止 TLS；
也可以让 signal/SFU 直接终止 —— 已支持 `CERT_FILE`/`KEY_FILE` 从文件读取
（未设置时回退到仓库内嵌开发证书），配合 certbot deploy-hook 自动安装+重启：

```sh
certbot certonly --standalone -d signal.aerodesk.io   --deploy-hook scripts/cert-renew-hook.sh
# 之后：
certbot renew --deploy-hook scripts/cert-renew-hook.sh
```

`scripts/cert-renew-hook.sh` 会把新证书**原子安装**到 `CERT_DEST`（默认
`/etc/aerodesk/tls`）并重启 signal/sfu 服务：每个 lineage 装到独立子目录
`$CERT_DEST/<lineage>/`，顶层 `cer.pem`/`key.pem` 是指向最新 lineage 的符号链接
（证书与私钥永远成对；多 lineage 互不覆盖）。systemd 服务示例：

```ini
# /etc/systemd/system/aerodesk-signal.service
[Service]
ExecStart=/opt/aerodesk/aerodesk-signal
Environment=CERT_FILE=/etc/aerodesk/tls/cer.pem
Environment=KEY_FILE=/etc/aerodesk/tls/key.pem
Environment=JWT_SECRET=...
Restart=on-failure
```

Caddy 示例：

```caddyfile
signal.aerodesk.io {
    reverse_proxy 127.0.0.1:3001
}
```

**内部 CA（企业内网）**：用 `step-ca` 或 vault PKI 签发，客户端信任链预置；
证书轮换走 SIGHUP 重载（待实现：signal/SFU 监听 SIGHUP 重读证书）。

## 3. 多 PoP 部署

- **就近接入**：每个 PoP 一组 `signal + sfu + coturn`；客户端经 DNS（GeoDNS/Anycast）
  选最近 PoP，信令返回该 PoP 的 TURN/SFU 地址。
- **房间跨 PoP**：默认房间内成员落在同一 PoP（分片哈希路由同房间同分片）。
  跨 PoP 房间需信令层把房间钉到固定 PoP（房间 → PoP 映射表），暂不支持实时跨区媒体桥接。
- **TURN 就近**：每 PoP 部署 coturn，`TURN_URLS` 指向本 PoP；`RELAY 端口段` 开放 UDP 49152-49200。
- **监控告警**：SFU 暴露 `GET /metrics`（每分片 client/包数等 JSON），
  接入 Prometheus（用 `prometheus_exporter` 或 textfile collector）+ Alertmanager：
  - 告警项：客户端掉线率、分片 CPU 突增、UDP 端口占用、录制目录磁盘水位 >80%
- **录制审计**：`RECORD_DIR` 落在独立数据盘（只读权限仅运维），
  `audit.log` 按天轮转，接入 SIEM/对象存储归档。

## 4. JWT 密钥管理

- `JWT_SECRET` 用强随机值：`openssl rand -base64 48`
- 每 PoP 独立或共享按需；密钥轮换：旧 secret 保留宽限期（待实现双 secret 支持），
  再统一切换。
- 签发 token（运维/测试）：

```sh
JWT_SECRET=<secret> cargo run -p aerodesk-cli -- --issue-token \
  --user u1 --device mac-1 --room demo --role publisher --ttl 3600
```

## 5. 录制文件格式

```
{RECORD_DIR}/{room}.adrec       # magic "ADREC1\n" + [u64 ts_us][u32 len][payload]...
{RECORD_DIR}/{room}.meta.json   # 起止时间/包数/字节数
{RECORD_DIR}/audit.log          # JSON Lines：room_start / room_end
```

## 6. 快速起一套（开发）

```sh
export JWT_SECRET=$(openssl rand -base64 48)
export RECORD_DIR=/tmp/aerodesk-rec
export TURN_SECRET=<coturn-secret>

cargo run -p aerodesk-sfu &        # 媒体 3478 + HTTPS 信令 3000 + /metrics
cargo run -p aerodesk-signal &     # WSS 3001 / 明文 3003（开发）
```

冒烟：

```sh
# 签发并连接
JWT_SECRET=$JWT_SECRET cargo run -p aerodesk-cli -- --issue-token \
  --user u1 --room demo --role publisher --ttl 600 | xargs -I{} \
  cargo run -p aerodesk-cli -- --role publisher --signal ws://127.0.0.1:3003 \
  --room demo --token {}
```

## 7. 验收清单（对应 Issue #5）

- [x] 信令 JWT 认证（用户/设备/房间/角色）
- [x] 房间录制/审计（SFU 侧，RECORD_DIR）
- [ ] 多 PoP 部署文档（本节即文档；跨 PoP 媒体桥待评估）
- [x] TLS 自动化落地（CERT_FILE/KEY_FILE + certbot deploy-hook；建议生产走 Caddy 边终止）
- [ ] 压测：4K60 × N 房间（见压测工具章节）
