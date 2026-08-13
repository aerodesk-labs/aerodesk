# 服务端部署（生产化）

覆盖信令（aerodesk-signal）、SFU（aerodesk-sfu，含内嵌 TURN+STUN，#191）、
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
   │        │               └─ 内嵌 TURN+STUN server（UDP :3479，#191）
   │        └────────────▶ coturn（可选：显式 TURN_URLS 时走外部中继）
   └── 录制目录 RECORD_DIR（可选）
```
> 端口组合（#185 已解决）：SFU 媒体默认 `3478`，内嵌 TURN 默认 `SFU_TURN_PORT=3479`，
> 互不冲突；显式设置 `TURN_URLS`（外部 coturn）时按外部地址下发。详见 docs/TURN.md。

## 1. 环境变量总览

| 组件 | 变量 | 说明 |
|---|---|---|
| signal | `JWT_SECRET` | HS256 共享密钥；**生产必设**，开启 JWT 认证 |
| signal | `JWT_SECRET_OLD` | 旧密钥（轮换宽限期，可选）：新密钥验证失败时回退；轮换完成后移除 |
| signal | `AUTH_TOKENS` | 静态 token（兼容模式；JWT 开启时忽略） |
| signal | `TURN_SECRET` / `TURN_URLS` | TURN REST 凭证下发（内嵌或 coturn） |
| sfu | `TURN_SECRET` | 启用 TURN：未设 `TURN_URLS` 时启动内嵌 TURN server（#191） |
| sfu | `SFU_TURN_PORT` | 内嵌 TURN UDP+TCP 端口（默认 3479，#196） |
| sfu | `SFU_TURN_TLS_PORT` | 内嵌 TURN TLS 端口（默认 5349；证书加载失败降级） |
| sfu | `MAX_TURN_ALLOCS_PER_IP` | TURN allocation 每 IP 配额（默认 16，0=不限，#204） |
| sfu | `MAX_TURN_ALLOCS_TOTAL` | TURN allocation 全局配额（默认 256，0=不限，#204） |
| sfu | `TURN_DENIED_PEER_CIDRS` | 拒绝中继的 peer CIDR 列表（逗号分隔，默认空，#204） |
| sfu | `SFU_TURN_IPV6` | `1` 时 TURN 双栈绑定（IPv6 中继，#204） |
| sfu | `TURN_URLS` | 显式设置时走外部 coturn（向后兼容），空/未设走内嵌 |
| sfu | `SFU_HOST_ADDRESS` | 对外通告地址（ICE 候选/TURN/web 地址；默认自动选择首个非回环 IPv4，#216）。NAT/带 docker0 等虚拟网卡的服务器必须设公网 IP，否则外部客户端连不上媒体 |
| sfu | `SFU_BIND_ADDRESS` | 媒体 socket 绑定地址（未设时：显式 `SFU_HOST_ADDRESS` 则默认 `0.0.0.0`，否则跟随通告地址，#216） |
| sfu | `SFU_SHARD_COUNT` | 媒体分片数（默认 CPU 核数，上限 8；1..=64 可覆盖）。大规格机器可上调利用更多核，小容器可下调到 1 |
| signal | `SFU_URL` / `SFU_TOKEN` | SFU 内部接口 + 内部 token |
| sfu | `RECORD_DIR` | 录制/审计目录（可选） |
| sfu | `RECORD_ON_DEMAND` | `1` 时只录显式 start() 的房间（配合内部 API 按需录制，#160） |
| sfu | `MAX_ROOM_CLIENTS` / `MAX_TOTAL_CLIENTS` | `/start` 准入配额（0=不限，#180，信令层 #163/#171 之外 SFU 侧纵深防御）；超限 503 `room full`/`server full` |
| sfu | `RECORD_MAX_BYTES` / `RECORD_MAX_SECS` | 录制轮转（0=不限，#180）：达阈值自动开新段 `{room}.adrec.{N}`，meta.json 汇总 segments |
| sfu | `AUDIT_MAX_BYTES` | 录制审计日志 `audit.log` 轮转上限（0=不限）：超限归档为 `audit.log.1` 后重开 |
| signal | `POP_REGISTRY_FILE` | 动态 room→PoP 注册表文件（可选，#154）：多 PoP 共享同一文件即互见；首个加入者登记房间归属 |
| signal | `POP_REGISTRY_TTL_SECS` | 注册条目 TTL（默认 3600，过期后可被重新登记） |
| signal | `MAX_ROOM_CLIENTS` | 每房间人数上限（0=不限，#163）；超限 Join 返回 `Error("room full")` |
| signal | `MAX_TOTAL_CLIENTS` | 单实例全局连接上限（0=不限，#163）；超限返回 `Error("server full")` |
| signal | `BRIDGE_CMD` | 跨 PoP 桥接编排（#216 M3，可选）：房间桥命令模板（含 `{room}`）。设置后跨 PoP viewer 先经桥在本 PoP 接入，失败/超时回退 v1 Redirect（详见 docs/BRIDGE.md） |
| signal | `BRIDGE_READY_TIMEOUT_SECS` | 桥就绪等待上限（默认 15） |
| signal | `BRIDGE_FAIL_COOLDOWN_SECS` | 桥失败冷却（默认 30；期间直接 Redirect 不反复 spawn） |
| signal | `BRIDGE_MAX_RUNNING` | 并发桥上限（默认 8；防房间名轮换绕过冷却的进程滥用） |
| signal | `BRIDGE_AUTH_TOKEN` | 注入桥子进程的认证 token（`BRIDGE_CMD` 内 `$BRIDGE_AUTH_TOKEN` 引用，配合 aerodesk-bridge `--auth-token`） |
| signal | `BRIDGE_IDLE_SECS` | 桥空闲回收阈值（默认 300；房间无真实客户端超时停桥，#246） |

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
完整可用的 systemd 单元模板见 `deploy/systemd/aerodesk-signal.service` /
`aerodesk-sfu.service`（含 BRIDGE_CMD/BRIDGE_AUTH_TOKEN 等跨 PoP 桥接示例，
#246）。Prometheus 双 PoP 抓取示例见 `deploy/prometheus/prometheus.yml`。

Caddy 示例：

```caddyfile
signal.aerodesk.io {
    reverse_proxy 127.0.0.1:3001
}
```

**内部 CA（企业内网）**：用 `step-ca` 或 vault PKI 签发，客户端信任链预置；
证书轮换走 **SIGHUP 热重载**（signal/SFU 均已实现：`kill -HUP <pid>` 重读
`CERT_FILE`/`KEY_FILE` 并重建 TLS server，无需重启进程、旧连接不受影响）。

## 3. 多 PoP 部署

- **就近接入**：每个 PoP 一组 `signal + sfu`（SFU 内嵌 TURN+STUN，#191）；客户端经 DNS（GeoDNS/Anycast）
  选最近 PoP，信令返回该 PoP 的 TURN/SFU 地址。
- **房间跨 PoP（v1 已支持，#146）**：信令层把房间钉到固定 PoP——每 PoP 设置
  `POP_ID`，并用 `ROOM_POP_MAP`（`房间前缀=PoP`，逗号分隔，最长前缀优先）声明钉 PoP 的房间，
  `POP_URLS`（`PoP=客户端信令URL`）给出各 PoP 的信令地址。客户端连错 PoP 时信令返回
  `Redirect`，原生客户端（aerodesk-core）自动重连到目标 PoP 的 signal/SFU
  （最多 3 跳防环）；Web 端重定向支持为后续。
  单机多 PoP 测试可用 `SFU_MEDIA_PORT`/`SFU_SIGNAL_PORT`/`SFU_INTERNAL_PORT` 覆盖 SFU 端口，
  示例见 `scripts/multipop-e2e.sh`。
- **跨 PoP 实时桥接（v3，#216）**：副 PoP 信令设置 `BRIDGE_CMD` 后，跨 PoP viewer
  不再直接 Redirect，而是由信令自动拉起 `aerodesk-bridge`（view 主 PoP + publish
  本 PoP，RTP 载荷直通不重编码 + data channel 白名单桥）并在就绪后本 PoP 接入；
  桥失败/超时回退 v1 Redirect（`BRIDGE_READY_TIMEOUT_SECS`/`BRIDGE_FAIL_COOLDOWN_SECS`
  可调）。设计/验收见 [ADR-0004](adr/0004-multipop-bridging.md) 与 `docs/BRIDGE.md`；
  真实部署验收直接跑 `POP_A_SIGNAL=... POP_B_SIGNAL=... scripts/bridge-fallback-e2e.sh`
  （远程模式：直连基线 + 桥优先 + 延迟 p50/p90/p99；可选 `BRIDGE_KILL_CMD`
  桥死亡自动恢复，见 BRIDGE.md）。
- **动态注册表（v2，#154）**：不配 `ROOM_POP_MAP` 时，房间归属由**首个加入者所在 PoP 登记**
  （`POP_REGISTRY_FILE` 共享文件 + `POP_REGISTRY_TTL_SECS` 过期）；其它 PoP 加入同房间时
  查注册表命中 → 返回 `Redirect`。文件后端为 last-writer-wins（低变更场景可接受）；
  生产多写并发可换 Redis 后端。示例见 `scripts/popreg-e2e.sh`。
- **TURN 就近**：默认每 PoP 由 SFU 内嵌 TURN server 提供中继（`SFU_TURN_PORT=3479`
  UDP+TCP，`SFU_TURN_TLS_PORT=5349` TLS，#196）；外部 coturn 部署可设 `TURN_URLS`
  指向本 PoP（向后兼容）。开放 UDP/TCP 3479、TCP 5349 与 `RELAY 端口段` 49152-49200。
- **监控告警**：SFU 暴露 `GET /metrics/prometheus`（Prometheus 文本格式：每分片
  clients/rx·tx packets/bytes + 合计 + `aerodesk_sfu_draining` gauge，以及 #238
  rtt_us/egress·ingress_loss/bwe_tx_bps/qos_clients、#220 turn_allocations[_total]、
  #240 recordings_active），可直接被 Prometheus 抓取；`GET /metrics`（JSON）保留
  兼容 bench 工具。**告警规则模板**：`deploy/prometheus/sfu-alerts.yml`（#240，
  含 draining/抓取失败/客户端掉线/媒体停摆/丢包/RTT/质量样本缺失/TURN 容量/
  录制磁盘水位/无在录房间），用 `promtool check rules` 校验后挂载到 Prometheus
  `rule_files`，Alertmanager 按 `severity` 路由（page/warning）。丢包/RTT/质量
  样本/媒体停摆规则按分片（`{shard=~".+"}`）告警——总量是跨分片加权平均，单一坏
  分片会被摊薄，不宜直接设阈值；`AeroDeskSFUNoActiveRecordings` 默认注释关闭（仅按需录制且期望持续录制
  时启用）。磁盘水位依赖 node_exporter 暴露 RECORD_DIR 所在挂载点；分片线程 CPU
  已导出为 `aerodesk_sfu_shard_cpu`（Linux，%/100），另有 pps 近似注释示例
  （配合容量基线 scripts/sfu-capacity-bench.sh）。
- **健康检查**：`GET /healthz` 返回 JSON（`status: ok|draining` + shards/clients）；
  正常 200，**draining 中 503**，供 LB/探活与滚动发布判断。
- **信号服务器探活/指标**：`GET /healthz`（JSON `status/clients/rooms/pop`）与
  `GET /metrics/prometheus`（`aerodesk_signal_clients/rooms/bridges`）暴露于
  信令端口（plain 与 WSS 同路径），供探活与 Prometheus 抓取。
- **优雅关闭**：`SIGTERM`/`SIGINT` → 拒绝新房间（`/start` 503）→ 限时 3s drain
  现有客户端 → finalize 录制 → 退出；systemd `KillSignal=SIGTERM` 可安全停服。
- **录制审计**：`RECORD_DIR` 落在独立数据盘（只读权限仅运维），
  `audit.log` 按天轮转，接入 SIEM/对象存储归档。事件含 `room_start`/`room_end`
  （#240 起带 `source`：`auto`=首包自动开启、`api`=按需 API 开启；`room_end`
  带 `duration_us`）、`record_api`（#240：按需录制 API 调用留痕，action/room/status/
  ok/detail，含 400/403/404/500/503 失败路径；只读 status 成功不写 audit.log 防
  轮询刷爆，503 例外会写）与 `session_api`（#240：会话管理 API 调用留痕，含
  踢人 400/403/404/500/200 与 query 中的 room/client）。
- **按需录制 API（#160）**：`RECORD_ON_DEMAND=1` 时通过内部接口（127.0.0.1:3002，
  `X-Internal-Token` 保护）按房间 start/stop/查询：
  ```sh
  curl -X POST -H "X-Internal-Token: $SFU_TOKEN" \
    'http://127.0.0.1:3002/record/start?room=demo'
  curl -H "X-Internal-Token: $SFU_TOKEN" 'http://127.0.0.1:3002/record/status'
  curl -X POST -H "X-Internal-Token: $SFU_TOKEN" \
    'http://127.0.0.1:3002/record/stop?room=demo'   # 立即 finalize + meta.json
  ```
  未设置 `RECORD_DIR` 时返回 503；无 token 返回 403；`stop` 幂等。
- **会话管理 API（#240）**：同一内部接口提供房间/客户端列表与踢人（运维/客服排障）：
  ```sh
  curl -H "X-Internal-Token: $SFU_TOKEN" 'http://127.0.0.1:3002/session/rooms'
  curl -H "X-Internal-Token: $SFU_TOKEN" 'http://127.0.0.1:3002/session/clients?room=demo'
  curl -X POST -H "X-Internal-Token: $SFU_TOKEN"     'http://127.0.0.1:3002/session/kick?room=demo&client=<id>'   # 踢单客户端，幂等
  curl -X POST -H "X-Internal-Token: $SFU_TOKEN"     'http://127.0.0.1:3002/session/kick?room=demo'                # 踢整个房间（#249）
  ```
  `session/rooms` 返回房间 + 客户端数 + 分片分布；`session/clients` 返回
  id/room/role/shard/joined_at/uptime；单客户端 kick 对不存在/房间不匹配返回 404、
  参数缺失 400、分片通道不可用 500；省略 `client` = 踢掉房间全部客户端
  （返回 `{room, kicked}`，幂等；kicked 是命令投递数而非确认断开数；room 级
  对分片投递失败仅审计 detail 不返回 500；也会踢掉该房间的 publisher——跨 PoP
  桥拓扑下真实 publisher 已被 Redirect 走，安全）。踢人/未授权调用写入 audit.log
  （`session_api` 事件，action=`session/kick`），供追责。

## 3.5 内部接口安全（#240）

`INTERNAL_TOKEN` 保护 127.0.0.1:3002 上全部内部接口（`/record/*`、`/session/*`、
`/metrics` 等）。**生产必须设置**；未设置时 SFU 启动会打印醒目警告，管理接口
（含踢人）处于无认证状态（仅限开发回环使用）。`X-Internal-Token` 缺失/错误返回
403 并写入审计日志（record/session 路径）。

## 4. JWT 密钥管理

- `JWT_SECRET` 用强随机值：`openssl rand -base64 48`
- 每 PoP 独立或共享按需；密钥轮换（**双 secret 宽限期已支持**）：
  1. 签发新 token 用新 `JWT_SECRET`；同时设置 `JWT_SECRET_OLD` 保留旧密钥（无需重启）
  2. 观察宽限期（按旧 token TTL）内认证无异常后，移除 `JWT_SECRET_OLD` 并重启（或滚动）
- **按用户连接配额（#171）**：JWT claims 支持可选 `max_conns`（该用户最大并发连接数，
  0/缺省=不限）；信令按 `sub` 计数，超限返回 `Error("user quota exceeded")`。
  签发示例：`JWT_SECRET=... aerodesk-cli --issue-token --user u1 --room demo --role viewer --max-conns 4`
- 签发 token（运维/测试）：

```sh
JWT_SECRET=<secret> cargo run -p aerodesk-cli -- --issue-token \
  --user u1 --device mac-1 --room demo --role publisher --ttl 3600
```

## 5. 录制文件格式

```
{RECORD_DIR}/{room}.adrec       # magic "ADREC1\n" + [u64 ts_us][u32 len][payload]...
{RECORD_DIR}/{room}.meta.json   # 起止时间/包数/字节数
{RECORD_DIR}/audit.log          # JSON Lines：room_start / room_end / record_api / session_api
```

## 6. 快速起一套（开发）

```sh
export JWT_SECRET=$(openssl rand -base64 48)
export RECORD_DIR=/tmp/aerodesk-rec
export TURN_SECRET=<共享 secret>  # 未设 TURN_URLS 时 SFU 内嵌 TURN（#191）

cargo run -p aerodesk-sfu &        # 媒体 3478 + HTTPS 3000 + /healthz + /metrics[/prometheus]
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
