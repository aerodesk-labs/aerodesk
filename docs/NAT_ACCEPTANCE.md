# NAT 公网实测手册（#582：srflx/relay 路径 + 回退黑屏时长）

> 承接 P0 验收报告（docs/P0_ACCEPTANCE_REPORT_20260824.md §4/§6-6）的 P1 交接项：
> 「NAT srflx/relay 公网实测（含回退黑屏时长上限）」。
> 本机无公网 NAT 环境时，先用 `scripts/nat-e2e.sh`（netns 模拟双 NAT）自动跑通
> 三条断言路径；有公网条件后按 §4 部署 VPS、按 §5 采集证据，补齐 Cone/对称 NAT
> 真机数据。脚本模式与公网实测对应关系见 §6.4。

## 1. 目标与范围

- **S1 直连路径（"srflx"）**：双端 NAT 打洞成功时，媒体直连不经过服务器/TURN；
  证据=媒体收包源地址=对端公网映射地址、SFU TURN allocation 为 0、媒体路径锁定 Direct。
- **S2 打洞失败**：对称 NAT/双端 NAT 下直连失败——无 TURN 时断言失败语义
  （ICE 超时、黑屏、不误报成功）；有 TURN 时断言自动回退中继（媒体经 TURN 恢复）。
- **S3 relay 路径**：`AERO_TURN_*` + `AERODESK_FORCE_RELAY=1` 强制媒体经 TURN
  （#201/#218），证据=TURN allocation 出现在 SFU 侧、媒体源地址=中继地址。
- **S4 回退黑屏时长上限**：会话中断 UDP→恢复（TURN/重连）的黑屏间隙测量方法
  （SIP_SIGNALING.md §10「断 UDP→TURN→强制 SFU 回退演练」的可测部分）。

**范围边界（诚实声明，见 §9）**：native 客户端当前**只通告 host + relayed 两类候选**
（无 STUN Binding srflx 发现）；「srflx 路径」指经 NAT 映射后直连媒体包可通的场景
（LAN/路由可达私网/端口预映射等），非标准 STUN srflx 候选。公网双普通 NAT 下
直连能否成功是本实测要回答的开放问题。

## 2. 现状盘点（客户端 ICE/媒体路径事实，代码位置）

| 事实 | 说明 | 位置 |
|---|---|---|
| 候选类型 | 仅 host（出口接口私网 IP，`discover_egress_ip` 探测）+ relayed（`add_relay_candidate`，`typ relay`）；**无 srflx 发现** | `crates/aerodesk-core/src/connect.rs`、`crates/aerodesk-agent/src/main.rs` connect_h264 |
| 双路收发 | `MediaSocket`：收=TURN 非阻塞探测优先 + 直连 UDP；发=未锁定前双路（直连+TURN），**首个非 STUN Binding 包锁定单路**——直连优先、TURN 兜底 | `crates/aerodesk-core/src/media_socket.rs` |
| 锁定日志 | `debug!("media path locked: {path:?}")`（Direct/Turn）——**媒体路径证据** | `media_socket.rs:133` |
| ICE 超时 | 连接阶段：无 TURN 5s / 有 TURN 15s；失败报 `ICE 连接超时（直连 5s / TURN 15s 未建立）` | `crates/aerodesk-agent/src/main.rs`（connect 阶段） |
| 会话死亡 | str0m `is_alive()` 失效 → `ICE session ended` → `--reconnect` 退避重连（1s/2s/4s/8s/10s 封顶，#173） | `main.rs` run_with_reconnect |
| force-relay | `AERODESK_FORCE_RELAY=1\|true`：跳过 host 候选、只通告 relayed（#201/#218） | `connect.rs:236` force_relay_env |
| TURN 凭证 | SIP 无 join 下发一环 → **须本地配置** `AERO_TURN_URLS/USERNAME/CREDENTIAL`（#570）；coturn REST 规范 `username=<expiry>:<userid>`、`credential=base64(HMAC-SHA1(secret, username))` | `main.rs` connect_h264、`aerodesk-core::turn_client::p2p_turn_transport` |
| TURN 传输 | RFC 5766 Allocate/CreatePermission/ChannelBind/Send/Data/Refresh；`TURN allocation ok` 日志 | `crates/aerodesk-core/src/turn_client.rs:163` |
| 内嵌 TURN server | `TURN_SECRET` 设置且无显式 `TURN_URLS` 时启动：`SFU_TURN_PORT`（默认 3479，UDP+TCP）+ `SFU_TURN_TLS_PORT`（5349）；relayed 地址=**`SFU_HOST_ADDRESS`:relay_port**（#216 通告地址，公网 VPS 必须显式设公网 IP） | `crates/aerodesk-sfu/src/turn_server.rs`、docs/TURN.md |
| 信令 | CLI 走标准 SIP：`ws://`→SIP/UDP 5060，`wss://`→SIP/TLS 5061（`AERO_SIP_TRANSPORT`/`AERO_SIP_PORT`/`AERO_SIP_DOMAIN`/`AERO_SIP_CA_PEM`） | `crates/aerodesk-core/src/sip_link.rs` SipLinkConfig::from_parts |

**已知边界（决定本实测口径）**：

1. **无 srflx 候选发现**：客户端不向 STUN 服务器发 Binding 取公网映射地址。因此
   「标准双 NAT（双方私网地址互不可达）+ 无 TURN」场景下 ICE 必然失败——这不是 bug，
   是当前能力边界；S2 要验证的正是该失败语义是否干净（超时、无假通、可回退）。
2. **发送路径锁定后不自动切 TURN**：`send_path` 被首个非 STUN 包锁定后，直连
   死亡时发送仍走直连 → 会话死亡 → 靠 `--reconnect` 重连（此时 ICE 直连失败、
   TURN 兜底成功）。因此 S4 的黑屏时长是**会话级恢复时间**（含死亡检测+重连+ICE），
   文档化该口径；会话内无缝切换列为 §9 后续改进。

## 3. 场景矩阵

| 场景 | 网络形态 | 预期 | 自动脚本模式 |
|---|---|---|---|
| S0 基线（LAN/路由直连） | 双端私网互可达 | ICE 直连、媒体源=对端私网地址、TURN 闲置 | `host`（本机） |
| S1 Cone NAT / 端口受限 NAT 打洞成功 | 双端经 NAT，映射后互可达 | 同 S0，媒体源=对端**公网映射**地址（=srflx 路径） | `netns` 双命名空间 + 预映射直连（见 §6.3 说明） |
| S2a 双 NAT 打洞失败（无 TURN） | 对称/双 NAT，直连不可达 | ICE 超时干净失败，无假通 | `netns` 双命名空间 FORWARD 阻断 + 无 AERO_TURN_* |
| S2b 双 NAT 打洞失败（有 TURN） | 同上 + `AERO_TURN_*` | ICE 直连失败 → TURN 兜底 → 媒体恢复 | `netns` 双命名空间 FORWARD 阻断 + TURN |
| S3 relay 强制路径 | 任意 + `AERODESK_FORCE_RELAY=1` | 只通告 relayed、媒体经 TURN、SFU allocation>0 | `netns` / `host` |
| S4 回退黑屏时长 | 会话中切断直连 | 恢复间隙 ≤ 上限（默认 15s，见 §7），记录实测值 | `netns` 会话中注入 DROP |

## 4. 环境搭建（公网 VPS 实测）

### 4.1 服务器（一台公网 VPS，建议 Ubuntu 22.04+，`cargo build --release` 三件套）

```sh
# 必设环境变量（release 运行）
export SFU_HOST_ADDRESS=<VPS 公网 IP>        # #216：通告地址，不设则外部客户端连不上
export TURN_SECRET=<随机 32 字符>             # 内嵌 TURN 密钥（SFU 与 signal 一致）
export SFU_MEDIA_PORT=3478
export SFU_TURN_PORT=3479                    # UDP+TCP
export SFU_TURN_TLS_PORT=5349                # 可选（turns:），需证书
export RECORD_DIR=/data/rec
./target/release/aerodesk-sfu &              # 内嵌 TURN+STUN 自动启动
export SIGNAL_OPS_PORT=3001                  # ops HTTPS（/healthz /metrics /admin/*）
export SIP_TLS_PORT=5061                     # SIP/TLS；SIP/WSS 3061 默认同证书开启
export SIP_UDP_PORT=5060                     # SIP/UDP（P3 单栈；明文 WS 已退役）
export TURN_URLS="turn:<VPS 公网 IP>:3479?transport=udp,turn:<VPS 公网 IP>:3479?transport=tcp,turns:<VPS 公网 IP>:5349?transport=tcp"
./target/release/aerodesk-signal &
```

**防火墙/安全组放行**：UDP+TCP `5060`（SIP）、TCP `3061`（WSS）、UDP/TCP `3478`
（媒体）、UDP/TCP `3479`（TURN）、TCP `5349`（TURN TLS）、UDP `49152-49200`
（TURN relay 段）、SFU 内部端口（`SFU_INTERNAL_PORT`，仅本机/metrics 采集）。

启动自检：

```sh
# 1. 内嵌 TURN 就绪（日志出现 embedded TURN+STUN server UDP on <公网 IP>:3479）
grep "embedded TURN" /data/rec/sfu.log
# 2. STUN Binding 可达性（公网任意机器）
python3 - <<'PY'
import socket,struct,os
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.settimeout(3)
s.sendto(struct.pack('!HHI12s',1,0,0x2112A442,os.urandom(12)),('<VPS IP>',3479))
print('STUN OK' if s.recvfrom(2048) else 'FAIL')
PY
# 3. /metrics 暴露 TURN 指标（内网 curl 内部端口；公网勿暴露）
curl -s http://127.0.0.1:<SFU_INTERNAL_PORT>/metrics/prometheus | grep turn_allocations
```

### 4.2 客户端环境变量（publisher/viewer 各自按需）

| 变量 | 值 | 作用 |
|---|---|---|
| `AERO_TURN_URLS` | `turn:<VPS IP>:3479?transport=udp,...` | TURN server 地址（#570） |
| `AERO_TURN_USERNAME` | `<expiry>:<userid>`（见下） | TURN REST 用户名 |
| `AERO_TURN_CREDENTIAL` | `base64(HMAC-SHA1(...))`（见下） | TURN REST 凭证 |
| `AERODESK_FORCE_RELAY` | `1`（S3 用） | 只通告 relayed 候选 |
| `AERO_SIP_TRANSPORT` | `udp`（默认）/`tls` | 信令传输；公网建议 `tls` |
| `AERO_SIP_PORT` | 0=按传输默认（5060/5061） | 覆盖端口 |
| `AERO_SIP_DOMAIN` | `aerodesk.test`（代码缺省；AoR 报文域。Digest realm 由服务端 401 质询下发——与 domain 无关，误填不影响认证只影响路由域一致性） | AoR 域 |
| `AERO_SIP_CA_PEM` | CA 路径（自签时） | TLS 校验 |
| `RUST_LOG` | `aerodesk_agent=info`（证据采集时 `=debug`） | 日志级别；**媒体源地址与路径锁定证据需 debug** |

TURN 凭证生成（coturn REST 规范，与 `TURN_SECRET` 一致）：

```sh
TURN_USER="$(($(date +%s) + 3600)):nat-test"
TURN_CRED="$(python3 -c "import hmac,hashlib,base64;print(base64.b64encode(hmac.new(b'$TURN_SECRET', b'$TURN_USER', hashlib.sha1).digest()).decode())")"
export AERO_TURN_USERNAME="$TURN_USER" AERO_TURN_CREDENTIAL="$TURN_CRED"
```

### 4.3 客户端起呼命令（SIP 1:1，参考 turn-e2e.sh）

```sh
# 被控端（等 INVITE；合成源 --noisy 便于无桌面服务器/CI）
./target/release/aerodesk-agent --role publisher --encoder x264 --noisy \
  --signal ws://<VPS IP>:3061/ws --room <room> [--reconnect]

# 观看端（呼入并显示统计）
AERO_TURN_URLS=... AERO_TURN_USERNAME=... AERO_TURN_CREDENTIAL=... \
./target/release/aerodesk-agent --role viewer --signal ws://<VPS IP>:3061/ws \
  --room <room> --reconnect --reconnect-max 5
```

## 5. 证据采集（每场景断言表）

证据来源三类：**客户端日志**（stderr，`RUST_LOG=debug` 打开路径证据）、
**SFU/signal 日志与 /metrics**、**抓包**（可选，`tcpdump -i <iface> udp` 在 VPS 与客户端网卡）。

### 5.1 客户端关键日志行（grep 用）

| 含义 | 日志行 | 级别 |
|---|---|---|
| 本地 UDP 绑定 | `local UDP addr: 0.0.0.0:xxxxx` | info |
| TURN allocation 成功 | `TURN allocation ok (turn:...): relayed=<host>:<port>` | info |
| relayed 候选进 offer | `relayed candidate <host>:<port> (local <ip>:<port>) force_relay=false` | info |
| force-relay 生效 | `force-relay: skip host candidate <ip>:<port>` | info |
| SIP 注册完成 | `SIP registered: <device>` | info |
| SDP 交换完成 | `SDP negotiated, awaiting ICE...` | info |
| ICE 连通 | `ICE connected`（或 `ICE connected (connect 阶段)`） | info |
| 媒体统计（每 2s） | `RECEIVED: <frames> frames, <bytes> bytes, ... DECODED: <n> ...` | info |
| **媒体收包源地址**（逐包） | `recv <n> bytes from <src:port> type=<0x..>` | **debug** |
| **发送路径锁定** | `media path locked: Direct\|Turn` | **debug** |
| 直连失败 | `ICE 连接超时（直连 5s / TURN 15s 未建立）` | error |
| 会话死亡 | `ICE session ended, exiting session for reconnect` / `session ended: ...; reconnecting in ...` | info |

### 5.2 SFU 侧证据

| 含义 | 来源 |
|---|---|
| TURN server 就绪 | sfu 日志 `embedded TURN+STUN server UDP on <ip>:3479` |
| **TURN allocation 事件** | sfu 日志（debug）`TURN allocation: <key> user=<user> relayed=<host:port>` |
| allocation 活跃/累计数 | `curl -s http://127.0.0.1:<内部端口>/metrics/prometheus`：`aerodesk_sfu_turn_allocations`（gauge）/`_total`（counter） |
| 媒体是否经服务器 | 抓包：VPS 网卡上**没有**两客户端间的媒体流（P2P 时只有信令），relay 时有 relay 段流量 |

### 5.3 场景断言表（公网实测填写模板）

| # | 场景 | 通过条件 | 证据 |
|---|---|---|---|
| S0 | LAN/路由直连 | `ICE connected` + `RECEIVED` 增长 + 媒体源=对端地址 + SFU `turn_allocations` 0 | 客户端 debug 日志、sfu 指标 |
| S1 | Cone NAT 打洞成功 | 同 S0，媒体源=对端**公网映射地址**（≠ 私网地址）+ TURN 闲置 | debug 日志 `recv ... from <公网IP:端口>` |
| S2a | 双 NAT 失败（无 TURN） | 观看端 ~5s 内 `ICE 连接超时`，**无** `RECEIVED`，无假通 | 客户端日志 |
| S2b | 双 NAT 失败（有 TURN） | `ICE connected` + `RECEIVED` 增长 + SFU `turn_allocations ≥2` + 媒体源=中继地址 | 客户端 debug 日志 + sfu 指标 |
| S3 | relay 强制 | viewer 日志 `force-relay: skip host candidate` + `relayed candidate` + `RECEIVED` 增长 + SFU allocation>0 | 客户端日志 + sfu 指标 |
| S4 | 回退黑屏时长 | 断直连后恢复间隙 ≤ 上限（默认 15s，见 §7） | RECEIVED 时间戳间隙 |

## 6. 自动化脚本（scripts/nat-e2e.sh）

在无公网 NAT 的 CI/本机用 **Linux 网络命名空间 + iptables** 模拟双端 NAT
（同一台 Linux 主机即可，需 root）：

- `netns natA`（被控端侧私网 10.200.0.0/24）+ `netns natB`（观看端侧私网 10.201.0.0/24）；
  两台客户端在各自命名空间内以 veth 出网，默认路由指向宿主。
- 宿主 `iptables FORWARD`：默认 DROP + `ESTABLISHED,RELATED` ACCEPT——**阻断两私网
  间一切"新"直连流量**（模拟双方 host 候选互不可达），同时保留经宿主上
  signal/SFU/TURN 进程的流量（INPUT/OUTPUT 不受影响）。
- 双端各自配 `AERO_TURN_URLS`（指向宿主 IP）→ 模拟 S2b；不配 → 模拟 S2a；
  阻断规则撤掉 → 模拟 S0/S1 直连。

脚本模式（`NAT_MODE=auto|netns|host|skip`，默认 auto 自动探测）：

| 模式 | 前提 | 执行 |
|---|---|---|
| `netns` | Linux + root + iproute2 + iptables | 起 netns 双端，跑 S0/S2a/S2b/S3/S4 全断言 |
| `host` | 本机构建产物 | 本机直连基线（S0）+ relay 路径（S3）冒烟 |
| `skip`/不可用 | 无 netns 能力（如本机 Windows/macOS） | 打印 SKIP 说明 + 公网实测步骤指引（即本文档 §4/§5） |

### 6.1 脚本断言一览

| 断言 | 判据 |
|---|---|
| A1 直连路径媒体直连 | `ICE connected` + `RECEIVED` 增长 + sfu `turn_allocations` 保持 0 |
| A2 打洞失败语义（无 TURN） | viewer 日志出现 `ICE 连接超时` 且无 `RECEIVED` |
| A3 relay 兜底 | sfu `turn_allocations ≥ 2` + viewer `RECEIVED` 增长 + `media path locked: Turn`（debug） |
| A4 relay 强制 | viewer `force-relay: skip host candidate` + `relayed candidate` + `RECEIVED` 增长 |
| A5 回退黑屏上限 | 断直连后 RECEIVED 时间戳间隙 ≤ `NAT_BLACKSCREEN_BOUND`（默认 15000ms） |

### 6.2 运行

```sh
# netns 全量（Linux root；端口 167xx 避免与其它 e2e 冲突）
sudo NAT_MODE=netns ./scripts/nat-e2e.sh

# 本机冒烟（无 NAT；Windows/macOS/Linux 均可，需已构建三件套；
# 二进制被占用/快速迭代时 NAT_SKIP_BUILD=1 跳过 cargo build）
NAT_MODE=host ./scripts/nat-e2e.sh
NAT_SKIP_BUILD=1 NAT_MODE=host ./scripts/nat-e2e.sh

# 无 netns 环境自动降级：打印 SKIP + 公网实测步骤
./scripts/nat-e2e.sh        # 或 NAT_MODE=skip
```

要点：脚本自起 signal（`SIP_UDP_PORT=16703`）+ SFU（内嵌 TURN），双端 agent 以
`AERO_SIP_PORT=16703` + `AERO_TURN_*` 连接（SIP 无 join 下发一环，须本地配置）；
先等被控端 `SIP registered` 再起观看端，避免 INVITE 抢跑在 REGISTER 之前被
signal 当会议 INVITE 转 SFU 桥（无 SFU_URL → 503）。

### 6.3 netns 模拟的"srflx 直连"口径

netns 双端 + FORWARD 全阻断时，两客户端间**不存在**可达的直连候选——这正是
S2a/S2b 的语义。S1（打洞成功直连）在 netns 下用「撤掉阻断规则」近似（私网
可路由 = 直连路径成立），公网真实 Cone NAT 的映射地址证据（媒体源=公网 IP）
必须在 VPS 实测中补齐（§4/§5.3 S1 行）。脚本在 netns 模式输出
`S1 近似断言通过，公网映射地址证据待 VPS 实测`。

### 6.4 脚本与公网实测的衔接

`NAT_MODE=host` / `netns` 全部 PASS 后，公网实测只需按 §4 部署并把 §5.3 模板
中的 S1/S4 两行用真机数据填上（本机无 NAT 时脚本会打印对应指引）。PR 评审
以脚本断言 + 本文档模板为验收面。

## 7. 回退黑屏时长上限测量方法（S4）

**测量口径**：观看端「最后一帧到恢复首帧」的间隙 = 黑屏时长上限。客户端日志
`RECEIVED:` 每 2s 一行且 tracing 默认带时间戳，直接取相邻 RECEIVED 行的
时间戳间隙即可（脚本用 `grep -n RECEIVED` + `date` 差值实现，见
`scripts/nat-e2e.sh` 的 `blackscreen_gap` 函数）。

**演练步骤（断 UDP→TURN 回退）**：

1. 基线：S0/S1 直连会话建立，`RECEIVED` 帧数持续增长，记录最后一帧时间 T0。
2. 切断直连：netns 中 `ip netns exec natB iptables -A OUTPUT -p udp -j DROP`
   （或 VPS 实测时在客户端网卡上临时 `iptables -A OUTPUT -p udp -d <对端> -j DROP`）。
3. 观察恢复：`--reconnect` 下客户端 ICE 直连失败 → TURN 兜底 → `RECEIVED` 恢复，
   记录恢复首帧时间 T1。
4. 黑屏时长 = T1 − T0；断言 ≤ `NAT_BLACKSCREEN_BOUND`（默认 15s = TURN ICE
   15s 超时上限，可按部署网络收紧）。
5. **强制 SFU 回退腿（SIP_SIGNALING.md §10 后半）**：P2P ICE 失败后向
   `sip:view-<device>@<domain>` 会议 AoR 重发 INVITE（§4.1 升级时序）——客户端
   该自动回退链路为后续批次实现（当前会话死亡 → 重连仍是 P2P），本手册先固化
   测量方法；实现后同一套 T1−T0 口径直接复用。

**注意**：发送路径锁定（§2 已知边界 2）后，会话内直连死亡不会无缝切 TURN，
黑屏时长=会话级恢复时间（含死亡检测 + 重连退避 + ICE）。这是当前能力下的
**诚实上界**；若验收需要 <2s 无缝切换，见 §9 改进项。

## 8. 结果记录模板（PR/验收附件）

```markdown
## NAT 实测结果（#582）
- 环境：VPS=<ip>（<云商>/<region>）、客户端 A=<网络形态>、客户端 B=<网络形态>
- 构建：<commit>，cargo build --release 三件套
| 场景 | 结果 | 黑屏时长 | 证据文件 |
|---|---|---|---|
| S0 直连基线 | PASS/FAIL | - | pub.log/view.log/sfu.log |
| S1 Cone NAT 直连 | PASS/FAIL | - | debug 日志（媒体源=公网映射地址）|
| S2a 双 NAT 无 TURN | PASS/FAIL | - | ICE 超时日志 |
| S2b 双 NAT 有 TURN | PASS/FAIL | - | sfu metrics + 日志 |
| S3 relay 强制 | PASS/FAIL | - | force-relay 日志 |
| S4 回退黑屏 | PASS/FAIL | <实测 ms> | RECEIVED 时间戳 |
- 脚本输出：`NAT_MODE=netns ./scripts/nat-e2e.sh` 附尾
```

## 9. 已知限制与后续建议

1. **无 STUN srflx 发现**：客户端不向 STUN 取公网映射地址，标准双 NAT（私网互
   不可达）下无 TURN 必失败。建议后续（P2）：ICE 候选前先做 STUN Binding
   （可复用内嵌 TURN server 的 Binding 响应），生成 srflx 候选——S1 公网实测
   结果将直接支撑该决策。
2. **发送路径锁定后不自动切 TURN**（`media_socket.rs` `send_path`）：建议后续：
   直连 socket 连续 N 秒无对端包（或收到 ICMP 不可达）时解锁，回落双路发送，
   让 TURN 兜底接管——把 S4 从「会话级恢复」降到「包级切换」。
3. **SIP 1:1 下 TURN 凭证无信令下发**（#570 已定 AERO_TURN_* 环境变量）：
   桌面 UI 的用户配置入口待补（当前仅 CLI 可配）。
4. **多 PoP/跨运营商**：对称 NAT 需真实运营商网络，netns 模拟覆盖不到
   （iptables 无法做按目标端口变化的映射）；公网实测须包含一对称 NAT 客户端。
