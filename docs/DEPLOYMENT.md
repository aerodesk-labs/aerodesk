# 服务端部署（生产化）

覆盖信令（aerodesk-signal）、SFU（aerodesk-sfu，含内嵌 TURN+STUN，#191）、
录制/审计与 TLS 自动化。对应 Issue #5。

## 组件拓扑

```
                        ┌────────────────────────────┐
 客户端 ──SIP/UDP:5060─▶│ aerodesk-signal（SIP 单栈） │
   │   SIP/TLS:5061     │  REGISTER/INVITE + ops HTTP │
   │   SIP/WSS:3061     │  （房间归属 / SFU 会议桥）    │
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
| signal | `SIGNAL_OPS_PORT` | HTTP 运维面端口（默认 3001；兼容别名 `SIGNAL_PORT`）：/healthz /devices /metrics/prometheus /admin/temp-password |
| signal | `SIP_TLS_PORT` / `SIP_WSS_PORT` / `SIP_UDP_PORT` | SIP 三传输端口（P3 默认全开：5061 / 3061 / 5060）；`off`/`disabled`/`none` 显式关闭对应传输 |
| signal | `SIP_REALM` | SIP Digest 域（默认 `aerodesk`） |
| signal | `SIP_DIGEST_USERS` | 设备固定密码表（逗号分隔 `user=password`；#503-4） |
| signal | `SIP_ADMIN_TOKEN` | /admin/temp-password 管理 token（缺省回退首个 `AUTH_TOKEN`） |
| signal | `AUTH_TOKENS` | 静态 token：即 SIP Digest 回退口令（规范 §8 迁移期同一凭据）+ /admin 鉴权回退 |
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
| signal | `SFU_URLS` / `SFU_URL` / `SFU_TOKEN` | SFU 池（逗号分隔，可选）+ 单值回退 + 内部 token。设 `SFU_URLS` 时按房间名无状态哈希选路到池中某个 SFU（同房间恒同 SFU）；未设回退 `SFU_URL`（单 SFU，向后兼容） |
| signal | `SFU_POLL_INTERVAL_SECS` / `SFU_FAIL_COOLDOWN_SECS` | SFU 池负载轮询间隔（默认 5s）与探测失败冷却期（默认 30s，期间不参与新房间分配）。仅 `SFU_URLS` 池 >1 时生效；新房间选最闲 SFU 并避开下线节点；同房间粘性（已分配房间不重映射，SFU 下线由客户端 --reconnect 恢复）。轮询携带 `SFU_TOKEN`（SFU 设 `INTERNAL_TOKEN` 时必须配置）；401/403 视为配置错误告警，不标记节点下线 |
| signal | `SFU_STICKY_TTL_SECS` | 房间→SFU 粘性映射空闲淘汰阈值（秒，默认 21600=6h，0 值无效按默认；仅池 >1 时生效）：last_used 由 SIP INVITE 会议分支刷新（池唯一消费点），零 INVITE 超过 TTL 视为死房间淘汰，防无界增长 |
| sfu | `RECORD_DIR` | 录制/审计目录（可选） |
| sfu | `RECORD_ON_DEMAND` | `1` 时只录显式 start() 的房间（配合内部 API 按需录制，#160） |
| sfu | `MAX_ROOM_CLIENTS` / `MAX_TOTAL_CLIENTS` | `/start` 准入配额（0=不限，#180，信令层 #163/#171 之外 SFU 侧纵深防御）；超限 503 `room full`/`server full` |
| sfu | `RECORD_MAX_BYTES` / `RECORD_MAX_SECS` | 录制轮转（0=不限，#180）：达阈值自动开新段 `{room}.adrec.{N}`，meta.json 汇总 segments |
| sfu | `AUDIT_MAX_BYTES` | 录制审计日志 `audit.log` 轮转上限（0=不限）：超限归档为 `audit.log.1` 后重开 |
| signal | `POP_REGISTRY_FILE` | 动态 room→PoP 注册表文件（可选，#154）：多 PoP 共享同一文件即互见；房间归属由首个 INVITE 登记（P3 写入点在 INVITE 会议分支） |
| signal | `POP_REGISTRY_TTL_SECS` | 注册条目 TTL（默认 3600，过期后可被重新登记） |
| signal | `POP_SIP_URLS` | PoP=SIP 目标 host:port（逗号分隔；跨 PoP 房间 INVITE 的 302 Contact 载体） |
| （P3.1 退役） | `MAX_ROOM_CLIENTS` / `MAX_TOTAL_CLIENTS` / `SIGNAL_MAX_PREJOIN_CLIENTS` / `SIGNAL_ALLOWED_ORIGINS` / `SIGNAL_PLAIN_PORT` / `JWT_SECRET` / `JWT_SECRET_OLD` / `TURN_SECRET`(signal) / `TURN_URLS`(signal) / `ROOM_POP_MAP` / `POP_URLS` / `BRIDGE_*` | 随 JSON 信令面退役（配额/白名单/JWT 认证/TURN 下发/明文 WS/桥编排）；SFU 侧同名配额不受影响 |

## 2. TLS 证书自动化

P3 SIP 单栈下证书同时服务 SIP/TLS(:5061)、SIP/WSS(:3061) 与 ops HTTPS(:3001)；
生产建议 443 反代终止 TLS，SIP/TLS 直连端口配公网证书，自动续期：

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
Environment=AUTH_TOKENS=CHANGE_ME
Restart=on-failure
```
完整可用的 systemd 单元模板见 `deploy/systemd/aerodesk-signal.service` /
`aerodesk-sfu.service`（P3 SIP 单栈：SIGNAL_OPS_PORT + SIP 三传输 + POP_SIP_URLS 示例，
#246）。Prometheus 双 PoP 抓取示例见 `deploy/prometheus/prometheus.yml`。

Caddy 示例：

```caddyfile
signal.aerodesk.io {
    reverse_proxy 127.0.0.1:3001
}
```

**nginx 反代 + 限流示例**（生产推荐：TLS 终止在 nginx，signal ops 面只绑内网；
限流覆盖连接/请求速率，防未授权客户端拉取运维面 DoS。注意 P3 单栈下 SIP/UDP
5060、SIP/TLS 5061 直连不经反代——反代只服务 ops HTTPS 面）：

```nginx
# 每 IP 并发连接 + 请求速率
limit_conn_zone $binary_remote_addr zone=ops_conn:10m;
limit_req_zone  $binary_remote_addr zone=ops_req:10m rate=10r/s;

server {
    listen 443 ssl;
    server_name signal.aerodesk.io;
    ssl_certificate     /etc/aerodesk/tls/cer.pem;
    ssl_certificate_key /etc/aerodesk/tls/key.pem;

    client_max_body_size 1m;

    location / {
        limit_conn ops_conn 20;
        limit_req  zone=ops_req burst=40 nodelay;
        # ops HTTPS 上游（/healthz /devices /metrics/prometheus /admin/*）。
        # signal 自带 TLS（缺省开发证书），内网回环跳过校验即可。
        proxy_pass https://127.0.0.1:3001;
        proxy_ssl_verify off;
        proxy_read_timeout 30s;
    }
}
```

> 旧 JSON 明文 WS（3003/14703 端口、`Upgrade` 升级头、#361 WS 帧级上限）随
> P3.1 JSON 面一并退役——SIP 单栈无 WS 长连接反代需求。

**内部 CA（企业内网）**：用 `step-ca` 或 vault PKI 签发，客户端信任链预置；
证书轮换走 **SIGHUP 热重载**（signal/SFU 均已实现：`kill -HUP <pid>` 重读
`CERT_FILE`/`KEY_FILE` 并重建 TLS server，无需重启进程、旧连接不受影响）。

## 3. 多 PoP 部署

- **就近接入**：每个 PoP 一组 `signal + sfu`（SFU 内嵌 TURN+STUN，#191）；客户端经 DNS（GeoDNS/Anycast）
  选最近 PoP，信令返回该 PoP 的 TURN/SFU 地址。
- **房间跨 PoP（P3 SIP 302，#146/#154）**：每 PoP 设置 `POP_ID`；房间归属由动态注册表
  登记（首个 INVITE 所在 PoP 成为 owner），其它 PoP 收到同房间 INVITE 时查注册表命中 →
  **302 + Contact**（`POP_SIP_URLS`，`PoP=host:port`）把主叫引导到 owner PoP 重发 INVITE。
  无 `POP_REGISTRY_FILE` 时单 PoP 行为不变；命中他 PoP 但无 302 目标时回 486 即刻失败。
  静态前缀钉住（旧 `ROOM_POP_MAP`/`POP_URLS`）随 JSON 面退役，如需预登记可预先写入共享
  注册表文件（见 `scripts/multipop-e2e.sh`）。客户端 302 跟随（会话层换拨）尚未
  实现（#600 仅落地 core 层 RedirectedTo 事件透传）——跟随落地前跨 PoP 主叫无法
  自动跟随 302，会以呼叫失败收场。
  单机多 PoP 测试可用 `SFU_MEDIA_PORT`/`SFU_SIGNAL_PORT`/`SFU_INTERNAL_PORT` 覆盖 SFU 端口，
  示例见 `scripts/multipop-e2e.sh` / `scripts/popreg-e2e.sh`。
- **跨 PoP 实时桥接（v3，#216，P3 退役待重建）**：旧 `BRIDGE_CMD` 进程编排随 JSON 面
  退役；桥双腿 SIP 化重建见 #601（设计与验收输入见 [ADR-0004](adr/0004-multipop-bridging.md)
  与 `docs/BRIDGE.md`）。重建前跨 PoP 房间一律走 302 引导。
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
- **信号服务器探活/指标**（P3 ops HTTPS 面，默认 :3001）：`GET /healthz`
  （JSON `status`/`pop`/`sip`——`sip` 为三传输监听状态对象，SIP 端点关闭时为
  `null`）与 `GET /metrics/prometheus`（`sip_registrations` gauge、
  `sip_calls_established`/`sip_calls_terminated` counter）供探活与 Prometheus 抓取。
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

## 4. 认证（P3：JWT 信令认证已退役）

- P3.1 起 signal 不再支持 JWT 信令认证（`JWT_SECRET`/`JWT_SECRET_OLD`、
  `aerodesk-agent --issue-token` 同批退役）；认证走 **SIP Digest**：
  - 设备固定口令：`SIP_DIGEST_USERS`（`user=password` 逗号分隔）；
  - 存量静态 token 直接作为 Digest 口令（`AUTH_TOKENS` 首个为未列设备回退，
    规范 §8 迁移期同一凭据）；口令表与 token 全空时为开放注册（开发/e2e）。
- 临时口令（无人值守访问）经 `/admin/temp-password` 签发（#503-4），
  `Authorization: Bearer <SIP_ADMIN_TOKEN 或首个 AUTH_TOKEN>`。

## 5. 录制文件格式

```
{RECORD_DIR}/{room}.adrec       # magic "ADREC1\n" + [u64 ts_us][u32 len][payload]...
{RECORD_DIR}/{room}.meta.json   # 起止时间/包数/字节数
{RECORD_DIR}/audit.log          # JSON Lines：room_start / room_end / record_api / session_api
```

## 6. 快速起一套（开发，P3 SIP 流）

```sh
export RECORD_DIR=/tmp/aerodesk-rec
export TURN_SECRET=<共享 secret>  # 未设 TURN_URLS 时 SFU 内嵌 TURN（#191）

cargo run -p aerodesk-sfu &        # 媒体 3478 + HTTPS 3000 + /healthz + /metrics[/prometheus]
cargo run -p aerodesk-signal &     # SIP/UDP 5060 + SIP/TLS 5061 + SIP/WSS 3061（默认全开）
                                   # ops HTTPS 3001（/healthz /devices /metrics /admin/*）
```

冒烟（SIP REGISTER → 被叫 INVITE）：

```sh
# 被控端（publisher 以 --room 值为设备 AoR 注册；AUTH_TOKENS 口令经 --token 传入）
cargo run -p aerodesk-agent -- --role publisher --signal ws://127.0.0.1:5060   --room demo --token <AUTH_TOKENS 值>

# 主控端观看（viewer INVITE demo 房间）
cargo run -p aerodesk-agent -- --role viewer --signal ws://127.0.0.1:5060   --room demo --token <AUTH_TOKENS 值>
```

## 6.1 公共测试服务器（129.226.150.174）

> 开发/跨机联调用公共节点（腾讯云轻量），signal + SFU（内嵌 TURN）单机部署；
> 认证用静态 token（`AUTH_TOKENS`，P3 起即 SIP Digest 口令）直连即可；
> token 值向维护者索取（不写进公开仓库）。

| 端口 | 协议 | 用途 |
|---|---|---|
| 15060 | UDP | signal SIP/UDP（客户端连接口，最关键；P3 单栈） |
| 15061 | TCP | signal SIP/TLS（可选；公网证书就绪后启用） |
| 14701 | TCP | signal ops HTTPS（/healthz /devices /metrics/prometheus /admin/*） |
| 14778 | UDP + TCP | SFU 媒体（WebRTC RTP/RTCP，**UDP 必须放行**） |
| 14779 | UDP + TCP | TURN 中继 |
| 15449 | TCP | TURN TLS（可选） |

连接示例（信令地址 = SIP 形态 `ws://host:sip-udp-port`）：

```sh
# 信令地址：ws://129.226.150.174:15060（agent 解析为 SIP/UDP 到该 host）
# token：从服务器 AUTH_TOKENS 获取

cargo run -p aerodesk-agent -- --role publisher --signal ws://129.226.150.174:15060   --room accept --token <AUTH_TOKENS 值>
```

> 注意：14703 明文 WS 已随 P3 JSON 面退役；14701（ops HTTPS）当前为开发 CA 证书，
> 浏览器访问需手动信任。节点重部署到 P3 单栈后以本表为准。

## 7. 验收清单（对应 Issue #5）

- [x] 信令认证（P3：SIP Digest + 临时口令；JWT 面随 P3.1 退役）
- [x] 房间录制/审计（SFU 侧，RECORD_DIR）
- [ ] 多 PoP 部署文档（本节即文档；跨 PoP 媒体桥待评估）
- [x] TLS 自动化落地（CERT_FILE/KEY_FILE + certbot deploy-hook；建议生产走 Caddy 边终止）
- [ ] 压测：4K60 × N 房间（见压测工具章节）
