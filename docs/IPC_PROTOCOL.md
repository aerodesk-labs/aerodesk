# AeroDesk 服务化 IPC 协议（#516 / ADR-0009 B2）

> 状态：v1 定稿（控制面消息集 + 帧面 F1 选型，基准见 IPC_FRAME_BENCHMARK.md）
> 范围：desktop（UI 壳）↔ host（用户态 agent / SYSTEM 服务）之间的 loopback IPC。
> 本批只定协议与基准，不做会话迁移（B3/B4）。

## 1. 传输与帧格式

- **传输**：命名管道 loopback（Windows `\\.\pipe\aerodesk-*`；macOS/Linux 用 Unix domain socket，路径差异仅落地点不同，协议一致）。Windows 服务侧管道一律拒绝远端（loopback-only，SDDL 限定本机 +  rejecting network 面）。
- **帧格式**：`u32 LE 长度` + JSON 载荷（UTF-8，无内嵌二进制；帧面见 §4）。
- **线程模型**：遵循 ADR-0008——收发各一 std::thread 同步阻塞泵，不引入 async 运行时进数据面。
- **背压**：控制面消息必须小（<64KB）；超阈值直接报错帧。剪贴板图片等大载荷走 file 通道既有分片语义（不经控制面内联）。

## 2. 信封与版本化

```json
{ "v": 1, "kind": "hello", ... }
```

- `v`：协议主版本（本文件 = 1）。**不兼容变更 → v+1**；新增可选字段/新 kind = 同版本演进。
- 兼容规则：
  1. 接收方**必须容忍未知字段**（serde `default`/忽略）；
  2. 收到未知 `kind` → 回 `error{code:"unknown_kind"}`，不断连；
  3. 握手 `hello{min_v, max_v}` 与对端可取版本取交集；`welcome` 帧信封的 `v` 即协商结果，无交集 → `error{code:"version_unsupported"}` 后关闭；
  4. 新增必填字段 = 主版本 +1。

## 3. 控制面消息集 v1

方向：C2S = desktop→host；S2C = host→desktop。`session` 为 host 分配的会话 ID（u64，贯穿日志）。

### 3.1 握手与保活

| kind | 方向 | 字段 | 说明 |
|---|---|---|---|
| `hello` | C2S | `client`("desktop"/"cli"), `client_version`, `min_v`, `max_v` | 连接建立后首帧 |
| `welcome` | S2C | `server_version`, `sessions:[{session,room,state}]` | 协商结果即本帧信封的 `v`；携带现存会话清单（UI 重开挂回的入口，B4 用） |
| `ping` / `pong` | 双向 | `nonce`, `sent_ms` | 默认 5s 节拍，3 拍未回判死 |
| `error` | 双向 | `code`, `message`, `session?` | 协议级错误；致命错误后跟关闭 |

### 3.2 会话控制（C2S）

| kind | 字段 | 对应 desktop 现状 |
|---|---|---|
| `connect` | `room`, `server`, `token`, `mode`("control"/"view"/"camera") | start_viewer_session 入参；S2C 回 `session_opened{session}` |
| `disconnect` | `session` | 会话 stop 置位 |
| `input` | `session`, `payload`(string) | input data channel 原样转发 |
| `control` | `session`, `payload`(string) | control 通道（选层/切显示器） |
| `cmd` | `session`, `req`(CmdRequest) | 终端命令通道 |
| `file_cmd` | `session`, `cmd`("send_file"/"send_clipboard"/"send_clipboard_image"/"cancel", 参数) | FileCmd 枚举映射 |
| `chat_send` | `session`, `text` | ChatCmd::Send |
| `tune` | `session`, `muted?`, `volume?`, `show_camera?`, `view_only?` | 会话句柄上的四个 Atomic 开关 |

### 3.3 被控控制（C2S）

| kind | 字段 | 对应 |
|---|---|---|
| `publisher_start` | `server`, `room`, `token`, `audio`, `mouse`, `view_only` | PublisherConfig 快照 |
| `publisher_stop` | — | 置 stop |
| `presence_set` | `enabled`(bool) | #456 授权开关（presence 接听策略） |

### 3.4 会话事件（S2C）——与 `SessionUi`（aerodesk-session）逐法对应

| kind | 字段 | SessionUi 方法 |
|---|---|---|
| `main_status` | `msg` | `set_status` |
| `conn_state` | `state`(i32) | `set_conn_state` |
| `log` | `msg` | `set_log` |
| `session_status` | `session`, `msg` | `session_status` |
| `joined` | `session` | `joined` |
| `cleanup` | `session`, `terminal?` | `cleanup` |
| `remote_cursor` | `session`, `x`, `y`(f32) | `set_remote_cursor` |
| `recent_add` | `room`, `server` | `add_recent` |
| `terminal_output` | `session`, `text` | `append_terminal_output` |
| `chat_message` | `session`, `sender`, `text`, `own` | `append_chat_message` |
| `message_window_status` | `session`, `status` | `set_message_window_status` |
| `file_window_progress` | `session`, `progress`(f32), `label`, `status` | `update_file_window_progress` |
| `file_window_clear` | `session`, `status?` | `clear_file_window_progress` |
| `main_session_status` | `msg` | `main_session_status`（macOS 文案面） |
| `file_progress` | `session`, `progress`, `label` | `set_file_progress` |
| `camera_available` | `session`, `available` | `set_camera_available` |

### 3.5 被控/presence 事件（S2C）

| kind | 字段 | 对应 |
|---|---|---|
| `publisher_event` | `state`("starting"/"status"/"start_failed"/"stopped"), `msg?` | PublisherEvent 四态 |
| `presence_status` | `text`, `online`, `active` | signal_status/signal_online/presence_active 三件套 |
| `incoming_call` | `from`, `call_id` | #456 呼叫；desktop 回 `call_answer{call_id, accept, reason?}` |

## 4. 帧面（已定稿：F1 共享内存）

基准数据见 `IPC_FRAME_BENCHMARK.md`：F1 p95 附加延迟 1080p **0.75ms** / 4K **3.09ms**，满足 <5ms 预算（4K 余量 38%），与进程内 memcpy 下限持平；命名管道载体在 4K 越预算（p95 6.12ms），仅留作控制面。

- **F1 选定形态**：host 解码 → RGBA 写入环形共享内存（`CreateFileMapping`，`Local\aerodesk-frame-<session>` 命名，双槽、4K 上限 64MB）+ ready/taken 双事件换手；desktop 从映射视图拷入 Slint 图像呈现，**不链接解码栈**——「UI 壳瘦身」目标完整成立。
- **失效处理**：desktop 崩溃/挂起 → host 写侧 taken 事件等待超时即回收该会话帧面，不阻塞引擎。
- **F2（编码流直连）为备案**：传输成本可忽略（1MB 编码帧 p95 0.17ms），但 desktop 须保留解码栈、瘦身缩水且每帧多一次完整解码；仅当 F1 在某平台受阻时启用，启用即在本文件补记 ADR 备注。
- **平台差异**：本结论覆盖 Windows 先行范围；macOS/Linux 帧面载体（`shm_open`+`mmap` / XPC 等）在对应平台批次另测，口径与本报告一致（p95 < 5ms）。

## 5. 安全边界

- 管道仅 loopback + 本机用户 SID 限定（服务侧管道另限 SYSTEM/Administrators + 登录用户会话）；
- 无鉴权 token（同机信任边界与现状 desktop 直连引擎一致）；token 等敏感字段只在 C2S 单向流动，不进日志（沿用 #470 日志脱敏约定）；
- 协议错误一律闭合当前连接，不做静默恢复。

## 6. 与既有通道的关系

盘点结论（#516 全仓 grep + 模块走查）：**host↔desktop 当前没有任何跨进程消息通道**——#470/#471 的 presence 仲裁与让位全部走 WTS/SCM API（`platform::windows::session` 枚举会话、`service_run.rs` 让位状态机）加 `CreateProcessAsUserW` 拉起 desktop，host 与 desktop 各自独立连信令服务器、互不通信；全仓无命名管道/共享内存/mailslot/Unix socket 命中，`windows` crate 既有的 platform features 也未开 `Win32_System_Pipes`/`Win32_System_Memory`。

因此传输层**新建**（不存在复用对象），模式层复用：

- **消息形态**：沿用 `aerodesk-protocol` 的 serde tagged-JSON 枚举先例（`SignalMessage`/`InputFrame`）；信封 `v` 字段沿用 `InputFrame.version` 的先例，但 IPC 协议独立演化、不与网络面共享版本号。
- **消息映射源**：§3.2/§3.4 逐法对应 B1 抽出的 `aerodesk-session` 接口形状（`SessionUi` 16 法、`PublisherEvent` 四态、`FileCmd`/`ChatCmd` 命令枚举），B4 迁移时 desktop 侧适配器即「SlintSessionUi ↔ IPC 帧」的翻译层。
- **依赖边界**：`cmd.req` 的 schema 引用 `aerodesk-protocol::CmdRequest`，但 aerodesk-ipc **不建依赖边**（`serde_json::Value` 透传）——protocol crate 携带 jsonwebtoken/hmac/sha1 等 crypto 依赖，IPC 层保持 serde/serde_json/tracing + windows 最小集；desktop/host 两侧各自 typed 编解码。
- **基准管线**：1080p/4K 帧面基准无现成 harness（`scripts/bench*.sh` 全部面向网络栈），新建 `aerodesk-ipc` 基准二进制；指标口径（p50/p95/p99、吞吐）沿用 `docs/BENCHMARK.md` 方法学。
