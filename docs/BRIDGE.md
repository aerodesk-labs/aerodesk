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
- M3 编排 ✅（`BRIDGE_CMD` 桥优先自动接入 + 失败回退 v1 Redirect，本地双 SFU e2e 全 PASS）
- M3 延迟 p99 ✅（本地方法学 + 直连基线对比；真实多 PoP 部署按下方 runbook 验收）

## M3：桥接编排（#216，`BRIDGE_CMD`）

PoP-B 信令配置 `BRIDGE_CMD` 后，跨 PoP viewer **无需人工起桥**：

```
PoP-B signal（BRIDGE_CMD + ROOM_POP_MAP + POP_URLS）
  viewer Join room(R 钉在 pop-a)
   ├─ 已有/可起桥？ ── 是 → 等待就绪（stdout "publisher leg:"）→ 本 PoP 接入（不 Redirect）
   └─ 桥失败/超时/冷却 ── → 回退 v1 Redirect → viewer 自动跟随到 pop-a
```

配置（PoP-B 信令环境变量）：
| 变量 | 说明 |
|---|---|
| `BRIDGE_CMD` | 房间桥命令模板，建议含 `{room}` 占位符（如 `aerodesk-bridge --remote-signal ws://... --local-signal ws://... --room {room} --auth-token \"$BRIDGE_AUTH_TOKEN\" --codec h264`）；缺 `{room}` 时自动追加 `--room {room}`。未设置 = 纯 v1 Redirect |
| `BRIDGE_AUTH_TOKEN` | 注入桥子进程环境的认证 token（`BRIDGE_CMD` 内以 `$BRIDGE_AUTH_TOKEN` 引用；配合 `--auth-token`，生产开启 JWT/静态 token 时必填） |
| `BRIDGE_READY_TIMEOUT_SECS` | 桥就绪等待上限（默认 15） |
| `BRIDGE_FAIL_COOLDOWN_SECS` | 桥失败冷却（默认 30；期间直接 Redirect 不反复 spawn） |
| `BRIDGE_MAX_RUNNING` | 并发桥上限（默认 8；防房间名轮换绕过冷却的进程滥用） |
| `BRIDGE_IDLE_SECS` | 桥空闲回收阈值（默认 300）：房间内无真实客户端（桥自身
  publisher 腿不计）超过该时长 → 后台 monitor 停桥并释放进程 |
| `BRIDGE_MONITOR_INTERVAL_SECS` | 桥 monitor 轮询间隔（默认 15，下限 2）：死亡
  检测/空闲回收粒度；建议 < 客户端 no-media watchdog（CLI 默认 8s）以便 kick
  先于 watchdog 生效（e2e 用 2） |

语义/边界：
- **认证/配额先行**：桥决策在 `auth_result` 与房间/全局配额通过后执行（未授权
  客户端无法触发进程 spawn）；桥自身 publisher 腿豁免配额（内部基础设施）；
- 同房间并发 viewer 统一走 `ensure_ready` 单飞（只 spawn 一次桥，失败一致回退
  Redirect）；桥自身 publisher 腿以 `is_running` 快路径放行（等自身就绪会死锁）；
- 真实 publisher 在桥模式下一律回退 Redirect（桥只支持主 PoP→本 PoP 媒体方向）；
- 房间名仅 `[A-Za-z0-9._-]` 且**不以 `-` 开头**才允许进命令模板（防 `sh -c`
  注入/选项注入），非法 → 直接 Redirect；
- 桥就绪后保持运行直到进程退出（主 PoP 媒体消失自然退出）；信令进程被 SIGTERM
  强杀时桥会短暂孤儿化，随后因 WS 腿断开自行退出（不建议依赖 Drop）；
- **桥死亡恢复（#246/#249，e2e 场景 3/4）**：桥进程被杀/退出后，下一位 viewer
  Join 会自动重建桥（自然死亡不触发失败冷却），无需人工干预；**已连接的 viewer
  由 signal monitor 检测桥死亡（died_rooms 差集）后自动调 SFU room-kick**
  （`POST /session/kick?room=R`，#249）断开其 WebRTC，客户端 `--reconnect`
  重连后走同一条重建/回退逻辑——全程无需人工；
- **空闲回收（#246）**：房间内无真实客户端超过 `BRIDGE_IDLE_SECS` 时，signal
  后台 monitor 停桥（释放进程与主 PoP 连接）；桥自身 publisher 腿通过 Peer
  `bridge_leg` 标记排除，不误判为真实客户端；
- 生产监控用 SFU 会话 API（#240）巡检桥两端客户端数。

## 部署模板（#246）

- `deploy/systemd/aerodesk-signal.service` / `aerodesk-sfu.service`：systemd 单元
  模板，含 `BRIDGE_CMD`/`BRIDGE_AUTH_TOKEN`/`BRIDGE_IDLE_SECS` 等完整示例；
- `deploy/prometheus/prometheus.yml`：双 PoP 抓取示例（配合 `sfu-alerts.yml`）。

### 延迟 p99 验收（本地方法学，`scripts/bridge-fallback-e2e.sh`）

`#8` 光标墙钟法：publisher 每 30Hz 经 cursor 通道带发送时间戳，viewer 计算
`LATENCY: N ms`（节流 1s）。脚本先测同 PoP 直连基线 p99，再测桥路径 p99，
断言 `桥 p99 < 直连 p99 × 4 + 500ms`（SCTP 每跳 ~150ms；桥比直连多 2 跳）。
本地 debug/loopback 实测（2026-08-10）：直连 p99 ≈ 0.5–1.5s、桥 p99 ≈ 0.9–1.6s
（负载敏感），桥相对直连增加 ~100–400ms。

### 真实多 PoP 部署验收 runbook（M3 剩余项）

1. **部署**：每 PoP 一组 signal+SFU（+可选 coturn），见 `DEPLOYMENT.md`；
   PoP-A 用 `ROOM_POP_MAP`/`POP_REGISTRY_FILE` 钉房间；PoP-B 设置
   `POP_ID=pop-b`、`ROOM_POP_MAP="<前缀>=pop-a"`、`POP_URLS="pop-a=wss://<pop-a>:443/ws"`、
   `BRIDGE_CMD="aerodesk-bridge --remote-signal wss://<pop-a>:443/ws --local-signal wss://<pop-b>:443/ws --room {room}"`
   （桥凭证走信令 JWT/静态 token；生产建议 `BRIDGE_READY_TIMEOUT_SECS=30`）。
2. **验收**：跨 PoP viewer 加入 → 本 PoP 接入且无 Redirect；`signal` 日志出现
   `bridge ready`；`/session/clients` 双 PoP 各 +2（publisher+bridge-view / bridge-pub+viewer）；
   本地自动化验证 = `scripts/bridge-fallback-e2e.sh`（直连基线 → 桥优先 → 桥死亡
   重建 → 失败回退 Redirect 四场景）。
3. **延迟 p99**：按 `#8` 方法在真实链路采集 ≥30 个 `LATENCY` 样本，p99 ≤ 验收阈值
   （预算：直连 p99 + 2×10–30ms 中继预算，按业务 SLA 定）；本地对比
   `scripts/bridge-fallback-e2e.sh`。
4. **失败回退**：停掉 PoP-A 信令（或令桥必失败）→ viewer 收到 Redirect 并自动
   跟随到 PoP-A；恢复后冷却期结束自动重新桥接。
5. **监控**：SFU `/metrics/prometheus`（#238 质量指标）+ 会话 API（#240）巡检
   桥两端客户端数。

## 关联
- #216（立项）、ADR-0004（v3 设计）、#146/#150/#154（v1/v2）、#8（延迟验收）
