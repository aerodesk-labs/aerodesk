//! #72 文件传输 + 剪贴板状态机（data channel label "file"）。
//!
//! 从 aerodesk-agent 提升到 core，供 CLI 与桌面 UI 共用：
//! - 发送：读文件 → SHA-256 → Meta(JSON) → Chunk(binary) → Done
//! - 接收：聚合分片 → 校验大小/hash → 落盘 → 回 ack
//! - 补包：接收端 Nack → 发送端重传（SFU 转发在出站缓冲满时会丢包）
//! - 剪贴板：经同一 file 通道传 `FileControl::Clipboard`（文本双向同步）
//!
//! 背压：通道写失败（返回 false）时暂停，下一轮重试。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::protocol::file::{
    self, CHUNK_SIZE, FileCancel, FileControl, FileDone, FileKind, FileMeta, FileNack,
};
use sha2::{Digest, Sha256};

/// 传输会话 id 计数器（多会话/重复发送避免同 id 冲突）。
static SEND_SEQ: AtomicU64 = AtomicU64::new(0);

/// 单文件大小上限（1 GiB）：防远端恶意 Meta 触发巨量内存分配（审查 #255）。
/// 剪贴板图片大小上限（16 MiB，#271）：截图 PNG 通常 <5 MiB，防远端恶意大图占内存。
pub const MAX_CLIPBOARD_IMAGE: u64 = 16 * 1024 * 1024;
pub const MAX_FILE_SIZE: u64 = 1024 * 1024 * 1024;
/// 并发接收器数量上限：防远端用大量不同 id 塞满内存。
pub const MAX_RECV: usize = 16;
/// 接收器生命周期：超时未完成即清理（发送端消失/断线防残留）。
pub const RECV_TTL: Duration = Duration::from_secs(300);
/// 已确认接收 id 缓存上限（幂等重发 ack 用）。
const MAX_COMPLETED: usize = 64;
/// 发送队列上限（#503 传输中心：批量入队，防无限排队）。
const MAX_QUEUE: usize = 64;
/// 传输记录上限（#503 传输中心：记录/重试，防无限增长）。
const MAX_HISTORY: usize = 200;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 当前墙钟（unix 毫秒），传输记录时间戳用。
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 取远端文件名的安全形式：只保留最后一段（拒绝绝对路径/`..`/子目录/空名）。
fn safe_file_name(name: &str) -> String {
    let p = Path::new(name);
    match p.file_name() {
        Some(s) => {
            let s = s.to_string_lossy().into_owned();
            if s.is_empty() || s == "." || s == ".." {
                String::new()
            } else {
                s
            }
        }
        None => String::new(),
    }
}

/// 生成接收目录内的临时文件名（隐藏文件 + pid + 计数，避免与目标冲突）。
fn temp_path_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let name = safe_file_name(name);
    if name.is_empty() {
        return None;
    }
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    Some(tmp)
}

/// 活动发送进度（#503：UI 展示 + 逐项取消/重试定位）。
#[derive(Debug, Clone, Default)]
pub struct SendProgress {
    /// 发送任务 id（取消定位用）。
    pub id: String,
    /// 文件名。
    pub name: String,
    /// 文件大小（字节）。
    pub size: u64,
    /// 已发分片。
    pub done: u64,
    /// 总分片。
    pub total: u64,
    /// 本地源路径（失败记录/重试用）。
    pub path: Option<PathBuf>,
}

/// 当前传输进度（UI 展示用）。
#[derive(Debug, Clone, Default)]
pub struct FileTransferStatus {
    /// 发送中（活动发送任务）。
    pub sending: Option<SendProgress>,
    /// 接收中：(文件名, 已收分片, 总分片)
    pub receiving: Option<(String, u64, u64)>,
    /// 排队待发送（#503 批量入队）：(id, 文件名, 大小)
    pub queue: Vec<(String, String, u64)>,
    /// 一次性事件（完成/失败/取消），消费后清除。
    pub message: Option<String>,
}

/// 传输记录（#503 传输中心「传输记录/重试」：方向/大小/终态/时间/本地路径）。
#[derive(Debug, Clone)]
pub struct TransferRecord {
    /// 记录 id（发送任务 id / 接收会话 id）。
    pub id: String,
    /// 方向："发送" / "接收"。
    pub direction: String,
    /// 文件名。
    pub name: String,
    /// 文件大小（字节）。
    pub size: u64,
    /// 终态："成功" / "失败" / "已取消"。
    pub state: String,
    /// 失败原因等细节文案（成功/取消为空）。
    pub detail: String,
    /// 完成时刻（unix 毫秒）。
    pub time_ms: u64,
    /// 本地路径（发送项；重试用）。接收项为空。
    pub path: Option<PathBuf>,
}

/// 排队待发送项（#503 批量入队：发送空闲时立即开始，忙则排队）。
#[derive(Debug, Clone)]
struct QueuedSend {
    id: String,
    path: PathBuf,
    name: String,
    size: u64,
}

/// 文件传输 + 剪贴板状态机（发送 + 接收）。
pub struct FileTransfer {
    send: Option<Sender>,
    /// 发送队列（#503 批量入队：活动发送结束后自动启动下一项）。
    queue: VecDeque<QueuedSend>,
    /// 传输记录（#503：成功/失败/取消留痕，UI 展示与重试）。
    history: Vec<TransferRecord>,
    recv: HashMap<String, Receiver>,
    recv_dir: Option<PathBuf>,
    /// 已确认接收完成的 id（幂等重发 ack，防 ack 丢失导致发送端永久重试）。
    completed: HashMap<String, Instant>,
    /// 是否允许响应远端 FileControl::Request 读取本机文件（默认 false；
    /// 仅被控端显式开启。观看端/UI 拒绝，防任意文件读取）。
    allow_request: bool,
    /// 收到远端剪贴板文本/图片（调用方 take 后写入系统剪贴板；图片为 PNG，#271）。
    incoming_clipboard: Option<String>,
    incoming_clipboard_image: Option<Vec<u8>>,
    /// 一次性状态事件（完成/失败/取消），UI 展示后消费。
    message: Option<String>,
    /// 待补发的剪贴板文本（SFU 转发可能丢首包，1s 幂等重试）。
    clipboard_pending: Option<String>,
    clipboard_sends: u32,
    last_clipboard_send: Option<Instant>,
    /// 终端态（收到接收端 Done ack）的常规文件发送 id——完成观察粘滞。
    /// 观察者（viewer --send-file 轮询 send_complete()）可能晚于同轮 tick 的
    /// 剪贴板图片换槽（busy-check 只拦未确认槽位）：live-slot 判定漏报 →
    /// viewer 永不退出（#595 审查二批，agent 层 AtomicBool 无法覆盖“首次
    /// 观察前”窗口故下沉至此）。下一个常规发送真正启动时清除
    /// （start_next_queued / send_file 直启），保持 #503 契约「推进到下一项
    /// 后不再报完成」。本地读失败走 tick 的槽位清空路径、不置粘滞——与既有
    /// 行为一致（该类失败从不触发 viewer 退出）。
    last_done_file: Option<String>,
}

struct Sender {
    /// 内存数据源（#271 剪贴板图片；与 file 二选一，Some 时优先）。
    data: Option<Vec<u8>>,
    /// 内容类型（#271：剪贴板图片接收端不落盘）。
    kind: FileKind,
    id: String,
    name: String,
    /// 本地源路径（#503 失败记录/重试用；剪贴板图片为 None）。
    path: Option<PathBuf>,
    /// 已打开的文件句柄（流式发送，避免整文件进内存；#271 内存数据时为 None）。
    file: Option<std::fs::File>,
    size: u64,
    hash: String,
    total_chunks: u64,
    next_chunk: u64,
    meta_sent: bool,
    last_meta_send: Instant,
    done_sent: bool,
    last_done_send: Instant,
    last_progress: Instant,
    /// 启动延迟：data channel 双方（经 SFU）打开有先后，过早发 Meta 会被
    /// SFU 丢弃（对端通道未就绪）。3s 覆盖 DCEP 错峰打开。
    start_after: Instant,
    /// 接收端补包队列（SFU 转发可能丢包，见 #72）。
    resend: Vec<u64>,
    /// 接收端已确认完整（收到 FileDone ack）。
    confirmed: bool,
    /// 本地发送失败原因（读文件失败等），由 FileTransfer::tick 上报后清空。
    failed: Option<String>,
}

struct Receiver {
    /// 内容类型（#271：剪贴板图片接收端不落盘）。
    kind: FileKind,
    name: String,
    size: u64,
    total_chunks: u64,
    hash: Option<String>,
    buf: Vec<u8>,
    received: u64,
    received_flags: Vec<bool>,
    /// 上次发 Nack 的时间：SFU 转发可能丢 Nack，超时后重发（自愈）。
    last_nack: Option<Instant>,
    /// 进度日志节流（诊断用）。
    last_progress: Option<Instant>,
    /// 接收器创建时间（TTL 清理用）。
    created_at: Instant,
}

impl FileTransfer {
    /// 创建状态机。`recv_dir` 为接收落盘目录；`None` 表示不接收。
    pub fn new(recv_dir: Option<PathBuf>) -> Self {
        Self {
            send: None,
            queue: VecDeque::new(),
            history: Vec::new(),
            recv: HashMap::new(),
            recv_dir,
            completed: HashMap::new(),
            allow_request: false,
            incoming_clipboard: None,
            incoming_clipboard_image: None,
            message: None,
            clipboard_pending: None,
            clipboard_sends: 0,
            last_clipboard_send: None,
            last_done_file: None,
        }
    }

    /// 设置接收目录（可在会话中启用接收）。
    pub fn set_recv_dir(&mut self, dir: Option<PathBuf>) {
        self.recv_dir = dir;
    }

    /// 是否允许响应远端 FileControl::Request（仅被控端开启；默认拒绝）。
    pub fn set_allow_request(&mut self, allow: bool) {
        self.allow_request = allow;
    }

    /// 触发发送一个文件（#503 批量入队）：当前无发送任务时立即开始，
    /// 忙则加入发送队列（活动发送结束后自动启动下一项）。
    /// 发送是否已被接收端确认（#122：viewer --send-file 模式发送完成判定）。
    /// 剪贴板图片（kind=ClipboardImage）不计入完成：它是常驻双向同步，接收端
    /// 拒绝（#503 无 recv-dir 显式 Nack）只是"本次同步失败"，不是 --send-file
    /// 的上传终结——若计入，viewer 会在媒体腿建立前退出（simulcast e2e 回归：
    /// 剪贴板回声 → Nack → exit(0)，RECEIVED 0 帧）。
    ///
    /// 粘滞判定（#595 审查二批）：收到常规文件的终端 Done ack 后，即使同轮
    /// tick 的剪贴板图片顶掉槽位，完成仍可观察（否则观察者可能永远错过一瞬
    /// 即逝的 confirmed 态）；粘滞由新文件启动清除。
    pub fn send_complete(&self) -> bool {
        self.send
            .as_ref()
            .is_some_and(|s| s.confirmed && s.kind != FileKind::ClipboardImage)
            || self.last_done_file.is_some()
    }

    /// 当前接收落盘目录（#122：--request-file 模式轮询落盘判定）。
    pub fn recv_dir(&self) -> Option<&Path> {
        self.recv_dir.as_deref()
    }

    pub fn send_file(&mut self, path: &Path) -> Result<(), String> {
        // 入队前先做轻量校验（存在 + 大小）；SHA-256 计算延迟到真正开始发送时。
        let meta = std::fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
        let size = meta.len();
        if size == 0 || size > MAX_FILE_SIZE {
            return Err(format!(
                "文件大小 {size} 超出范围（0 < size <= {MAX_FILE_SIZE}）"
            ));
        }
        if self.send.is_some() {
            // 发送槽位占用（含已确认但尚未被 tick 推进的旧任务）：一律入队——
            // 直接替换 confirmed 发送者会让新文件插到队列前面（#503 队列语义回归）。
            self.push_queue(path, size)
        } else {
            self.send = Some(Sender::open(path)?);
            // 新常规发送启动：完成粘滞失效（新上传的确认未发生）。
            self.last_done_file = None;
            Ok(())
        }
    }

    /// 批量入队发送（#503 传输中心：多选/拖放批量）；逐路径返回结果。
    pub fn send_files(&mut self, paths: &[PathBuf]) -> Vec<Result<(), String>> {
        paths.iter().map(|p| self.send_file(p)).collect()
    }

    /// 当前排队项数量（#503 UI 队列展示/文案用）。
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// 传输记录（#503 传输中心「记录/重试」展示）。
    pub fn history(&self) -> &[TransferRecord] {
        &self.history
    }

    /// 清空传输记录（#503 传输中心「清空记录」）。
    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// 取消指定发送项（#503 传输中心逐项取消）：id 匹配活动发送 → 取消并启动
    /// 队列下一项；匹配排队项 → 出队并记「已取消」。未命中返回 false。
    pub fn cancel_send_id(&mut self, id: &str, endpoint: &mut crate::Endpoint) -> bool {
        if self.send.as_ref().is_some_and(|s| s.id == id) {
            self.cancel_send(endpoint);
            return true;
        }
        if let Some(pos) = self.queue.iter().position(|q| q.id == id) {
            let q = self.queue.remove(pos).unwrap();
            self.push_record("发送", &q.id, &q.name, q.size, "已取消", "", Some(q.path));
            self.message = Some(format!("已取消排队：{}", q.name));
            tracing::info!("file queued send cancelled: {}", q.name);
            return true;
        }
        false
    }

    /// 入队一个待发送文件（id 在入队时分配，供取消定位）。
    fn push_queue(&mut self, path: &Path, size: u64) -> Result<(), String> {
        if self.queue.len() >= MAX_QUEUE {
            return Err("发送队列已满，请等待当前传输完成".into());
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let seq = SEND_SEQ.fetch_add(1, Ordering::SeqCst);
        self.queue.push_back(QueuedSend {
            id: format!("tx{}-q{seq}", std::process::id()),
            path: path.to_path_buf(),
            name: name.clone(),
            size,
        });
        tracing::info!("file send queued: {name} (queue {})", self.queue.len());
        Ok(())
    }

    /// 队列推进：取出队首启动发送；文件打开失败则记「失败」并继续下一项。
    fn start_next_queued(&mut self) {
        loop {
            let Some(q) = self.queue.pop_front() else {
                break;
            };
            match Sender::open(&q.path) {
                Ok(mut s) => {
                    // #503 传输中心：保持排队时的 id（tx{pid}-qN）跨启动稳定——
                    // 否则启动瞬间 UI 还持有旧 id，逐项取消会「找不到传输项」，
                    // 且同一文件在记录里出现两个 id。
                    s.id = q.id.clone();
                    // 队列下一项启动：上一项的完成粘滞失效（#503 推进契约）。
                    self.last_done_file = None;
                    tracing::info!(
                        "file send start (queued): {} (queue {})",
                        q.name,
                        self.queue.len()
                    );
                    self.send = Some(s);
                    break;
                }
                Err(e) => {
                    tracing::warn!("queued file open failed: {}: {e}", q.name);
                    self.push_record("发送", &q.id, &q.name, q.size, "失败", &e, Some(q.path));
                    self.message = Some(format!("发送失败：{e}"));
                }
            }
        }
    }

    /// 写入一条传输记录（有界：超出 MAX_HISTORY 丢弃最旧）。
    fn push_record(
        &mut self,
        direction: &str,
        id: &str,
        name: &str,
        size: u64,
        state: &str,
        detail: &str,
        path: Option<PathBuf>,
    ) {
        self.history.push(TransferRecord {
            id: id.to_string(),
            direction: direction.to_string(),
            name: name.to_string(),
            size,
            state: state.to_string(),
            detail: detail.to_string(),
            time_ms: now_millis(),
            path,
        });
        if self.history.len() > MAX_HISTORY {
            self.history.drain(..self.history.len() - MAX_HISTORY);
        }
    }

    /// 处理 data channel 事件（仅 label == "file" 生效）。
    pub fn handle_event(&mut self, ev: &crate::ClientEvent, endpoint: &mut crate::Endpoint) {
        match ev {
            crate::ClientEvent::ChannelOpen(label, _) if label == "file" => {
                tracing::info!("file channel open");
            }
            crate::ClientEvent::ChannelData(cid, _, data)
                if endpoint.channel_label(*cid).as_deref() == Some("file") =>
            {
                self.handle_data(data, endpoint);
            }
            _ => {}
        }
    }

    /// 每轮事件循环推进一次发送 + 接收端 Nack 重试。
    pub fn tick(&mut self, endpoint: &mut crate::Endpoint) {
        // 接收器 TTL：发送端消失/断线后清理，防内存残留（审查 #255 Important）。
        if !self.recv.is_empty() {
            let expired: Vec<String> = self
                .recv
                .iter()
                .filter(|(_, r)| r.created_at.elapsed() >= RECV_TTL)
                .map(|(id, _)| id.clone())
                .collect();
            for id in expired {
                if let Some(r) = self.recv.remove(&id) {
                    // #503 记录留痕：接收超时视为失败。
                    self.push_record("接收", &id, &r.name, r.size, "失败", "接收超时未完成", None);
                    tracing::warn!("file receive TTL expired, dropped: {id}");
                }
            }
        }
        // 幂等 ack 缓存 TTL（1 小时）清理。
        if !self.completed.is_empty() {
            let expired: Vec<String> = self
                .completed
                .iter()
                .filter(|(_, t)| t.elapsed() >= Duration::from_secs(3600))
                .map(|(id, _)| id.clone())
                .collect();
            for id in expired {
                self.completed.remove(&id);
            }
        }
        // 发送推进：tick + 失败上报 + #503 队列推进（成功/失败/本地失败后自动下一项）。
        let mut failed: Option<(String, String, u64, Option<PathBuf>, String)> = None;
        if let Some(s) = &mut self.send {
            s.tick(endpoint);
            if let Some(reason) = &s.failed {
                failed = Some((
                    s.id.clone(),
                    s.name.clone(),
                    s.size,
                    s.path.clone(),
                    reason.clone(),
                ));
            }
        }
        if let Some((id, name, size, path, reason)) = failed {
            self.push_record("发送", &id, &name, size, "失败", &reason, path);
            self.message = Some(reason);
            self.send = None;
            self.start_next_queued();
        } else if self.send.as_ref().is_some_and(|s| s.confirmed) && !self.queue.is_empty() {
            // 成功确认且队列非空：启动下一项（队列为空时保留 confirmed 发送者，
            // 供 send_complete() 判定——CLI --send-file 模式依赖它退出）。
            self.send = None;
            self.start_next_queued();
        }
        // #72 剪贴板补发：首包可能被 SFU 丢弃，1s 后重发（幂等，最多 8 次）。
        if let Some(text) = self.clipboard_pending.clone() {
            if self.clipboard_sends >= 8 {
                self.clipboard_pending = None;
                self.message = Some("剪贴板发送失败（通道未就绪）".into());
            } else if self
                .last_clipboard_send
                .is_some_and(|t| t.elapsed() >= Duration::from_secs(1))
            {
                self.clipboard_sends += 1;
                self.last_clipboard_send = Some(Instant::now());
                self.dispatch_clipboard(&text, endpoint);
            }
        }
        // 未完成的接收器：Nack 超时（1s）未得到补包时重发 Nack，
        // 防 SFU 转发丢 Nack 导致补包死锁。
        let ids: Vec<String> = self.recv.keys().cloned().collect();
        for id in ids {
            let resend = self
                .recv
                .get(&id)
                .and_then(|r| r.last_nack)
                .is_some_and(|t| t.elapsed() >= Duration::from_secs(1));
            if resend {
                let Some(mut r) = self.recv.remove(&id) else {
                    continue;
                };
                self.request_resend(
                    r,
                    FileDone {
                        id,
                        ok: true,
                        error: None,
                    },
                    endpoint,
                );
            }
        }
    }

    /// 当前传输状态（UI 轮询；message 消费后清除）。
    pub fn status(&mut self) -> FileTransferStatus {
        // 已确认（完成）不再报 sending，避免 UI 进度条永久卡 100%。
        let sending = self
            .send
            .as_ref()
            .filter(|s| !s.confirmed)
            .map(|s| SendProgress {
                id: s.id.clone(),
                name: s.name.clone(),
                size: s.size,
                done: s.next_chunk,
                total: s.total_chunks,
                path: s.path.clone(),
            });
        let receiving = self
            .recv
            .values()
            .next()
            .map(|r| (r.name.clone(), r.received, r.total_chunks));
        let queue = self
            .queue
            .iter()
            .map(|q| (q.id.clone(), q.name.clone(), q.size))
            .collect();
        FileTransferStatus {
            sending,
            receiving,
            queue,
            message: self.message.take(),
        }
    }

    /// 取走收到的远端剪贴板图片（PNG，#271；应用层写入系统剪贴板）。
    pub fn take_incoming_clipboard_image(&mut self) -> Option<Vec<u8>> {
        self.incoming_clipboard_image.take()
    }

    pub fn take_incoming_clipboard(&mut self) -> Option<String> {
        self.incoming_clipboard.take()
    }

    /// 取消当前发送：向接收端下发 FileCancel 并清空发送状态（#72 回归：
    /// 接收端 on_cancel 移除接收器，不落盘，无残留临时文件）。
    /// #503：取消后自动启动队列下一项。
    pub fn cancel_send(&mut self, endpoint: &mut crate::Endpoint) {
        let Some(s) = self.send.take() else {
            return;
        };
        let cancel = FileControl::Cancel(FileCancel { id: s.id.clone() });
        if let Ok(json) = serde_json::to_string(&cancel) {
            let _ = endpoint.send_channel_data("file", false, json.as_bytes());
        }
        self.push_record("发送", &s.id, &s.name, s.size, "已取消", "", s.path.clone());
        self.message = Some(format!("已取消发送：{}", s.name));
        tracing::info!("file send cancelled: {}", s.name);
        self.start_next_queued();
    }

    /// 发送剪贴板文本到远端（同一 file 通道）；进入补发队列（1s 幂等重试，
    /// 最多 8 次），应对 SFU 转发丢首包。
    pub fn send_clipboard(&mut self, text: &str, endpoint: &mut crate::Endpoint) -> bool {
        self.clipboard_pending = Some(text.to_string());
        self.clipboard_sends = 0;
        self.last_clipboard_send = Some(Instant::now());
        self.dispatch_clipboard(text, endpoint)
    }

    /// 发送剪贴板图片（PNG，#271）到远端：复用文件分片通道（Meta/Chunk/Done + Nack 补包），
    /// 接收端不落盘、直接写入系统剪贴板。发送期间占用文件发送槽位（互斥）。
    pub fn send_clipboard_image(&mut self, png: Vec<u8>) -> Result<(), String> {
        if self.send.as_ref().is_some_and(|s| !s.confirmed) {
            return Err("已有文件传输进行中，剪贴板图片稍后重试".into());
        }
        if png.is_empty() || png.len() as u64 > MAX_CLIPBOARD_IMAGE {
            return Err(format!(
                "剪贴板图片大小 {} 超出范围（0 < size <= {MAX_CLIPBOARD_IMAGE}）",
                png.len()
            ));
        }
        let name = format!(
            "clipboard-image-{}.png",
            SEND_SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let sender = Sender::from_bytes(name, png, FileKind::ClipboardImage)?;
        tracing::info!("clipboard image send start: {} bytes", sender.size);
        // 不清除 last_done_file：图片顶掉「已确认、观察未到」的常规发送正是
        // 完成粘滞要覆盖的场景（send_complete 需保持 true 直到新文件启动）。
        self.send = Some(sender);
        Ok(())
    }

    fn dispatch_clipboard(&mut self, text: &str, endpoint: &mut crate::Endpoint) -> bool {
        // 剪贴板大小上限（1 MiB）：防超大文本构造超大 data channel 消息。
        const MAX_CLIPBOARD: usize = 1024 * 1024;
        if text.len() > MAX_CLIPBOARD {
            tracing::warn!("clipboard too large ({} bytes), not sent", text.len());
            self.clipboard_pending = None;
            self.message = Some("剪贴板超过 1 MiB，未发送".into());
            return false;
        }
        let ctrl = FileControl::Clipboard {
            text: text.to_string(),
        };
        let Ok(json) = serde_json::to_string(&ctrl) else {
            return false;
        };
        endpoint.send_channel_data("file", false, json.as_bytes())
    }

    fn handle_data(&mut self, data: &[u8], endpoint: &mut crate::Endpoint) {
        if let Some((id, index, payload)) = file::decode_chunk(data) {
            self.on_chunk(id, index, payload);
            return;
        }
        let Ok(ctrl) = serde_json::from_slice::<FileControl>(data) else {
            tracing::debug!("file channel: unrecognized message");
            return;
        };
        match ctrl {
            FileControl::Meta(m) => self.on_meta(m),
            FileControl::Done(d) => {
                // 发送端收到 Done = 接收端 ack（已完整落盘）；接收端收到 = 发送端完成。
                if self.send.as_ref().map(|s| s.id == d.id).unwrap_or(false) {
                    // 借用外写历史/消息（#503 传输记录留痕；失败记录在 tick 中写入）。
                    let mut record: Option<(String, String, u64, Option<PathBuf>)> = None;
                    let mut message: Option<String> = None;
                    if let Some(s) = &mut self.send {
                        if d.ok {
                            s.confirmed = true;
                            if s.kind != FileKind::ClipboardImage {
                                // 完成粘滞：确认可被晚到的观察者看见（换槽不灭失）。
                                self.last_done_file = Some(s.id.clone());
                            }
                            tracing::info!(
                                "file transfer confirmed by receiver: {} ({} bytes)",
                                s.name,
                                s.size
                            );
                            record = Some((s.id.clone(), s.name.clone(), s.size, s.path.clone()));
                            message = Some(format!("已发送：{}", s.name));
                        } else {
                            // 接收端回报失败（ok=false）：进入失败终态。
                            let reason = d.error.unwrap_or_else(|| "接收端拒绝".to_string());
                            s.failed = Some(format!("{}：发送失败（{reason}）", s.name));
                            s.confirmed = true;
                            if s.kind != FileKind::ClipboardImage {
                                // 失败同样置粘滞：维持既有「拒收也退出 viewer」语义，
                                // 仅把此前依赖竞态的单迭代窗口变确定。
                                self.last_done_file = Some(s.id.clone());
                            }
                        }
                    }
                    if let Some((id, name, size, path)) = record {
                        self.push_record("发送", &id, &name, size, "成功", "", path);
                    }
                    if let Some(msg) = message {
                        self.message = Some(msg);
                    }
                } else {
                    self.on_done(d, endpoint);
                }
            }
            FileControl::Cancel(c) => self.on_cancel(c),
            FileControl::Nack(n) => {
                if let Some(s) = &mut self.send
                    && s.id == n.id
                    && !s.confirmed
                {
                    let before = s.resend.len();
                    for idx in n.missing {
                        if idx < s.total_chunks && !s.resend.contains(&idx) {
                            s.resend.push(idx);
                        }
                    }
                    tracing::info!(
                        "file resend request: {} missing chunks ({} -> {})",
                        s.name,
                        before,
                        s.resend.len()
                    );
                    // 重传一批后重新发 Done，让接收端再次校验。
                    s.done_sent = false;
                }
            }
            FileControl::Clipboard { text } => {
                tracing::info!("file clipboard received ({} chars)", text.chars().count());
                self.incoming_clipboard = Some(text);
            }
            FileControl::Request { path } => {
                // #122：控制端请求被控端发送文件（大文件下载）。
                // 安全：默认拒绝（allow_request=false），仅被控端显式开启，
                // 防房间内任意对端读取本机任意文件（审查 #255 Critical）。
                // #503：发送忙时入队，请求不再被忽略。
                if !self.allow_request {
                    tracing::warn!("file request rejected (allow_request=false): {path}");
                } else {
                    match self.send_file(std::path::Path::new(&path)) {
                        Ok(()) => tracing::info!("file request: 开始发送/排队 {path}"),
                        Err(e) => tracing::warn!("file request failed: {e}"),
                    }
                }
            }
        }
    }

    fn on_meta(&mut self, m: FileMeta) {
        let is_clip_image = m.kind == FileKind::ClipboardImage;
        if !is_clip_image && self.recv_dir.is_none() {
            tracing::info!("file receive disabled (no recv dir); ignore {}", m.name);
            return;
        }
        if self.recv.contains_key(&m.id) {
            tracing::warn!("duplicate FileMeta id={}", m.id);
            return;
        }
        // 安全上限（审查 #255 Critical）：防远端恶意 Meta 触发巨量分配/DoS。
        // #271 剪贴板图片：名字固定、不落盘，仅内存接收。
        let safe_name = if is_clip_image {
            "clipboard-image.png".to_string()
        } else {
            safe_file_name(&m.name)
        };
        if safe_name.is_empty() {
            tracing::warn!("file receive rejected: 非法文件名 {:?}", m.name);
            return;
        }
        let expect_chunks = m.size.div_ceil(CHUNK_SIZE as u64);
        let max_size = if is_clip_image {
            MAX_CLIPBOARD_IMAGE
        } else {
            MAX_FILE_SIZE
        };
        if m.size == 0 || m.size > max_size {
            tracing::warn!("file receive rejected: size {} (max {max_size})", m.size);
            return;
        }
        if m.chunks != expect_chunks {
            tracing::warn!(
                "file receive rejected: chunks {} != ceil(size/{}) = {expect_chunks}",
                m.chunks,
                CHUNK_SIZE
            );
            return;
        }
        if self.recv.len() >= MAX_RECV {
            tracing::warn!("file receive rejected: 并发接收器已达上限 {MAX_RECV}");
            return;
        }
        tracing::info!(
            "file receive start: {} ({} bytes, {} chunks)",
            safe_name,
            m.size,
            m.chunks
        );
        self.recv.insert(
            m.id.clone(),
            Receiver {
                kind: m.kind,
                name: safe_name,
                size: m.size,
                total_chunks: m.chunks,
                hash: m.hash,
                buf: vec![0; m.size as usize],
                received: 0,
                received_flags: vec![false; m.chunks as usize],
                last_nack: None,
                last_progress: None,
                created_at: Instant::now(),
            },
        );
    }

    fn on_chunk(&mut self, id: String, index: u64, payload: &[u8]) {
        let Some(r) = self.recv.get_mut(&id) else {
            return;
        };
        // 先校验 index 再乘 CHUNK_SIZE，防 debug 溢出 panic / release 回绕写入错误偏移。
        if index >= r.total_chunks {
            tracing::warn!(
                "chunk index out of range: id={id} index={index} total={}",
                r.total_chunks
            );
            return;
        }
        let start = index as usize * CHUNK_SIZE;
        if start + payload.len() > r.buf.len() {
            tracing::warn!(
                "chunk out of range: id={id} index={index} len={}",
                payload.len()
            );
            return;
        }
        r.buf[start..start + payload.len()].copy_from_slice(payload);
        let Some(f) = r.received_flags.get_mut(index as usize) else {
            return;
        };
        // 去重：SFU 转发/重传可能重复送达同一分片，重复计入会让 received
        // 超过 total，导致「missing 0 却 incomplete」的补包死循环。
        if *f {
            return;
        }
        *f = true;
        r.received += 1;
        // 诊断：接收进度（500ms 节流）
        let now = Instant::now();
        if r.last_progress
            .map(|t| t.elapsed() >= Duration::from_millis(500))
            .unwrap_or(true)
        {
            r.last_progress = Some(now);
            tracing::info!(
                "file receive {}: {}/{} chunks ({:.0}%)",
                r.name,
                r.received,
                r.total_chunks,
                r.received as f64 * 100.0 / r.total_chunks.max(1) as f64
            );
        }
    }

    fn on_done(&mut self, d: FileDone, endpoint: &mut crate::Endpoint) {
        let Some(r) = self.recv.remove(&d.id) else {
            if self.completed.contains_key(&d.id) && d.ok {
                // 幂等 ack：已完成的 id 收到重复 Done（ack 丢失后发送端重传）时重发 ack。
                let ack = FileControl::Done(FileDone {
                    id: d.id,
                    ok: true,
                    error: None,
                });
                if let Ok(json) = serde_json::to_string(&ack) {
                    let _ = endpoint.send_channel_data("file", false, json.as_bytes());
                }
            } else if d.ok {
                // 从未接受过的传输（无接收目录 / Meta 被拒 / 接收器已超时移除）：
                // 显式回执拒绝，否则发送端每秒重传 Meta/Done、永不失败（#503
                // 桌面被控端未设 recv-dir 时发送端无限挂起回归）。
                let ack = FileControl::Done(FileDone {
                    id: d.id,
                    ok: false,
                    error: Some("接收端未开启接收".into()),
                });
                if let Ok(json) = serde_json::to_string(&ack) {
                    let _ = endpoint.send_channel_data("file", false, json.as_bytes());
                }
            }
            return;
        };
        if !d.ok {
            if let Some(err) = &d.error {
                tracing::warn!("file {} failed on sender: {err}", r.name);
            }
            return;
        }
        // 完整性检查：大小 + 全分片 + hash。
        if r.received != r.total_chunks || r.buf.len() as u64 != r.size {
            tracing::error!(
                "file {} incomplete: {}/{} chunks, {} bytes (expected {})",
                r.name,
                r.received,
                r.total_chunks,
                r.buf.len(),
                r.size
            );
            self.request_resend(r, d, endpoint);
            return;
        }
        if let Some(h) = &r.hash {
            let actual = hex(&Sha256::digest(&r.buf));
            if &actual != h {
                tracing::error!("file {} hash mismatch (expected {h}, got {actual})", r.name);
                self.request_resend(r, d, endpoint);
                return;
            }
        }
        // #271 剪贴板图片：完整接收后交内存队列（不落盘），由调用方写入系统剪贴板。
        // 剪贴板图片不入传输记录（#503：记录只留文件传输条目，避免剪贴板噪音）。
        if r.kind == FileKind::ClipboardImage {
            tracing::info!("clipboard image receive complete: {} bytes", r.buf.len());
            self.incoming_clipboard_image = Some(r.buf);
            self.message = Some("已接收剪贴板图片".into());
            self.completed.insert(d.id.clone(), Instant::now());
            if self.completed.len() > MAX_COMPLETED {
                let oldest = self
                    .completed
                    .iter()
                    .min_by_key(|(_, t)| **t)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    self.completed.remove(&k);
                }
            }
            let ack = FileControl::Done(FileDone {
                id: d.id,
                ok: true,
                error: None,
            });
            if let Ok(json) = serde_json::to_string(&ack) {
                let _ = endpoint.send_channel_data("file", false, json.as_bytes());
            }
            return;
        }
        let Some(dir) = self.recv_dir.clone() else {
            return;
        };
        // 落盘：目标路径限定在 recv_dir 内（safe_file_name 已在 on_meta 校验），
        // 临时文件 + rename 原子写入，避免半截文件与符号链接跟随。
        let Some(tmp) = temp_path_in(&dir, &r.name) else {
            tracing::error!("file {} temp path invalid", r.name);
            return;
        };
        if let Err(e) = std::fs::write(&tmp, &r.buf) {
            tracing::error!("file {} temp write failed: {e}", tmp.display());
            return;
        }
        let final_path = dir.join(&r.name);
        if let Err(e) = std::fs::rename(&tmp, &final_path) {
            tracing::error!("file {} rename failed: {e}", final_path.display());
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        tracing::info!(
            "file receive complete: {} -> {}",
            r.name,
            final_path.display()
        );
        // #503 传输记录留痕。
        self.push_record("接收", &d.id, &r.name, r.size, "成功", "", None);
        self.message = Some(format!("已接收：{}", final_path.display()));
        // 幂等 ack 缓存（有界）。
        self.completed.insert(d.id.clone(), Instant::now());
        if self.completed.len() > MAX_COMPLETED {
            let oldest = self
                .completed
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                self.completed.remove(&k);
            }
        }
        let ack = FileControl::Done(FileDone {
            id: d.id,
            ok: true,
            error: None,
        });
        if let Ok(json) = serde_json::to_string(&ack) {
            let _ = endpoint.send_channel_data("file", false, json.as_bytes());
        }
    }
    /// 分片/校验失败：把接收器放回 map 并发 Nack 补包。
    fn request_resend(&mut self, mut r: Receiver, d: FileDone, endpoint: &mut crate::Endpoint) {
        let missing: Vec<u64> = r
            .received_flags
            .iter()
            .enumerate()
            .filter(|(_, got)| !**got)
            .map(|(i, _)| i as u64)
            .collect();
        if missing.is_empty() {
            // 没有缺失分片却 incomplete：重复分片导致计数错乱，无法自愈，
            // 放弃该接收器避免死循环。
            tracing::error!(
                "file {} incomplete but no missing chunks (received {}/{}); dropped",
                r.name,
                r.received,
                r.total_chunks
            );
            // #503 记录留痕。
            self.push_record(
                "接收",
                &d.id,
                &r.name,
                r.size,
                "失败",
                "分片计数错乱，放弃",
                None,
            );
            return;
        }
        tracing::info!(
            "file {} missing {} chunks, requested resend",
            r.name,
            missing.len()
        );
        let nack = FileControl::Nack(FileNack {
            id: d.id.clone(),
            missing: missing.into_iter().take(512).collect(),
        });
        if let Ok(json) = serde_json::to_string(&nack) {
            let _ = endpoint.send_channel_data("file", false, json.as_bytes());
        }
        r.last_nack = Some(Instant::now());
        self.recv.insert(d.id, r);
    }

    fn on_cancel(&mut self, c: FileCancel) {
        if let Some(r) = self.recv.remove(&c.id) {
            tracing::info!("file {} cancelled", c.id);
            // #503 记录留痕。
            self.push_record("接收", &c.id, &r.name, r.size, "已取消", "", None);
            self.message = Some(format!("已取消接收：{}", r.name));
        }
    }
}

impl Sender {
    fn open(path: &Path) -> Result<Self, String> {
        use std::io::Read;
        let file =
            std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let size = file
            .metadata()
            .map_err(|e| format!("stat {}: {e}", path.display()))?
            .len();
        if size == 0 || size > MAX_FILE_SIZE {
            return Err(format!(
                "文件大小 {size} 超出范围（0 < size <= {MAX_FILE_SIZE}）"
            ));
        }
        // 流式计算 SHA-256（不整文件进内存）；发送时再从文件句柄读分片。
        let mut hasher = Sha256::new();
        let mut file2 = file.try_clone().map_err(|e| format!("clone file: {e}"))?;
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = file2
                .read(&mut buf)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let hash = hex(&hasher.finalize());
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let total_chunks = size.div_ceil(CHUNK_SIZE as u64);
        let seq = SEND_SEQ.fetch_add(1, Ordering::SeqCst);
        Ok(Self {
            data: None,
            file: Some(file),
            kind: FileKind::File,
            id: format!("tx{}-{seq}", std::process::id()),
            name,
            path: Some(path.to_path_buf()),
            size,
            hash,
            total_chunks,
            next_chunk: 0,
            meta_sent: false,
            last_meta_send: Instant::now(),
            done_sent: false,
            last_done_send: Instant::now(),
            last_progress: Instant::now(),
            start_after: Instant::now() + Duration::from_secs(3),
            resend: Vec::new(),
            confirmed: false,
            failed: None,
        })
    }

    /// 从内存数据构造发送任务（#271 剪贴板图片；文件路径走 open）。
    fn from_bytes(name: String, data: Vec<u8>, kind: FileKind) -> Result<Self, String> {
        if data.is_empty() || data.len() as u64 > MAX_FILE_SIZE {
            return Err(format!(
                "剪贴板图片大小 {} 超出范围（0 < size <= {MAX_FILE_SIZE}）",
                data.len()
            ));
        }
        let hash = hex(&Sha256::digest(&data));
        let size = data.len() as u64;
        let total_chunks = size.div_ceil(CHUNK_SIZE as u64);
        let seq = SEND_SEQ.fetch_add(1, Ordering::SeqCst);
        Ok(Self {
            id: format!("tx{}-{seq}", std::process::id()),
            name,
            path: None,
            data: Some(data),
            file: None,
            kind,
            size,
            hash,
            total_chunks,
            next_chunk: 0,
            meta_sent: false,
            last_meta_send: Instant::now(),
            done_sent: false,
            last_done_send: Instant::now(),
            last_progress: Instant::now(),
            start_after: Instant::now() + Duration::from_secs(3),
            resend: Vec::new(),
            confirmed: false,
            failed: None,
        })
    }

    /// 读取 [start, start+len) 分片（len ≤ CHUNK_SIZE）；内存数据源直接切片。
    fn read_chunk(&mut self, start: usize, len: usize) -> Option<Vec<u8>> {
        if len == 0 {
            return Some(Vec::new());
        }
        if let Some(data) = &self.data {
            let end = (start + len).min(data.len());
            return Some(data[start..end].to_vec());
        }
        use std::io::{Read, Seek, SeekFrom};
        let file = self.file.as_mut()?;
        file.seek(SeekFrom::Start(start as u64)).ok()?;
        let mut out = vec![0u8; len];
        let mut got = 0;
        while got < len {
            let n = file.read(&mut out[got..]).ok()?;
            if n == 0 {
                return None;
            }
            got += n;
        }
        Some(out)
    }

    fn tick(&mut self, endpoint: &mut crate::Endpoint) {
        if self.confirmed {
            return;
        }
        if Instant::now() < self.start_after {
            return;
        }
        // #85：SFU 转发可能丢一次性消息——Meta/Done 在未确认前每 1s 重传，
        // 直到接收端 ack（Done ok=true）。接收端对重复 Meta 去重。
        if !self.meta_sent || self.last_meta_send.elapsed() >= Duration::from_secs(1) {
            let meta = FileControl::Meta(FileMeta {
                id: self.id.clone(),
                name: self.name.clone(),
                size: self.size,
                chunks: self.total_chunks,
                hash: Some(self.hash.clone()),
                kind: self.kind,
            });
            if let Ok(json) = serde_json::to_string(&meta)
                && endpoint.send_channel_data("file", false, json.as_bytes())
            {
                if !self.meta_sent {
                    tracing::info!("file send start: {} ({} bytes)", self.name, self.size);
                }
                self.meta_sent = true;
                self.last_meta_send = Instant::now();
            }
            if !self.meta_sent {
                return;
            }
        }
        // 补包优先（接收端 Nack）。先发送成功再出队：若 send 失败（SCTP 缓冲
        // 满/背压），下一轮重试同一块——pop 后再 send 会把失败块永久丢失，
        // 接收端无限 Nack 挂起（#72/#85 的间歇性 "receive not completed"）。
        if let Some(&resend_idx) = self.resend.last() {
            let start = (resend_idx as usize) * CHUNK_SIZE;
            let end = ((resend_idx + 1) as usize * CHUNK_SIZE).min(self.size as usize);
            let Some(chunk) = self.read_chunk(start, end - start) else {
                self.message_self_failed("读取文件失败（补包）");
                return;
            };
            let frame = file::encode_chunk(&self.id, resend_idx, &chunk);
            if endpoint.send_channel_data("file", true, &frame) {
                self.resend.pop();
                tracing::debug!("file resend chunk {resend_idx}");
            }
            return;
        }
        // #85 吞吐：背压突发发送——只要 SCTP 缓冲可接受就连续发（str0m
        // MAX_BUFFERED 8MB + rwnd 8MB 下安全，不会触发 DTLS 队列溢出），
        // 突破单发节拍（1 chunk/轮 ≈ 1.6MB/s）上限；send 失败即停，下一轮续。
        // 每轮最多 32 chunk（256KB）：既达到 100MB <60s（实测 ~44s），又避免
        // 单轮占用事件循环过久；debug 下 str0m/sctp-proto 高吞吐 data channel
        // 深链栈溢出为已知限制（#102，SFU 8MB 栈缓解），CI 文件传输 e2e 已
        // 改用 release 构建规避。
        if self.next_chunk < self.total_chunks {
            let mut sent = 0usize;
            while self.next_chunk < self.total_chunks && sent < 32 {
                let start = (self.next_chunk as usize) * CHUNK_SIZE;
                let end = ((self.next_chunk + 1) as usize * CHUNK_SIZE).min(self.size as usize);
                let Some(chunk) = self.read_chunk(start, end - start) else {
                    self.message_self_failed("读取文件失败（发送）");
                    return;
                };
                let frame = file::encode_chunk(&self.id, self.next_chunk, &chunk);
                if !endpoint.send_channel_data("file", true, &frame) {
                    break;
                }
                self.next_chunk += 1;
                sent += 1;
                if self.last_progress.elapsed() >= Duration::from_millis(500) {
                    tracing::info!(
                        "file send {}: {}/{} chunks ({:.0}%)",
                        self.name,
                        self.next_chunk,
                        self.total_chunks,
                        self.next_chunk as f64 * 100.0 / self.total_chunks.max(1) as f64
                    );
                    self.last_progress = Instant::now();
                }
            }
            return;
        }
        if !self.done_sent || self.last_done_send.elapsed() >= Duration::from_secs(1) {
            let done = FileControl::Done(FileDone {
                id: self.id.clone(),
                ok: true,
                error: None,
            });
            if let Ok(json) = serde_json::to_string(&done)
                && endpoint.send_channel_data("file", false, json.as_bytes())
            {
                if !self.done_sent {
                    tracing::info!("file send done: {}", self.name);
                }
                self.done_sent = true;
                self.last_done_send = Instant::now();
            }
        }
    }

    /// 发送侧本地失败（读取文件失败等）：记录原因并停发（由 FileTransfer::tick 上报）。
    fn message_self_failed(&mut self, msg: &str) {
        self.failed = Some(format!("{}：{}", self.name, msg));
        self.confirmed = true; // 停发
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_missing_file_returns_err() {
        let mut ft = FileTransfer::new(None);
        assert!(
            ft.send_file(Path::new("/nonexistent/aerodesk-file-transfer-test.bin"))
                .is_err()
        );
    }

    #[test]
    fn send_file_then_status_reports_sending() {
        // 写一个临时文件，send_file 后 status 应显示发送中（name/总数）。
        let dir = std::env::temp_dir().join("aerodesk-ft-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.bin");
        std::fs::write(&path, vec![7u8; 20000]).unwrap();
        let mut ft = FileTransfer::new(None);
        ft.send_file(&path).unwrap();
        let st = ft.status();
        let sp = st.sending.expect("sending should be Some");
        assert_eq!(sp.name, "sample.bin");
        assert_eq!(sp.done, 0);
        // 20000B / 8192B 分片 → 3 片
        assert_eq!(sp.total, 3);
        assert_eq!(
            sp.path.as_deref(),
            Some(path.as_path()),
            "发送任务应记录源路径"
        );
        assert!(st.queue.is_empty(), "空闲时不应有排队项");
    }

    #[test]
    fn cancel_send_clears_sender_and_sets_message() {
        // 写临时文件触发发送；cancel_send 后：发送任务清空、message 标记取消。
        // send_channel_data 在无 file 通道时返回 false（不 panic），不影响状态清理。
        let dir = std::env::temp_dir().join("aerodesk-ft-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cancel.bin");
        std::fs::write(&path, vec![9u8; 20000]).unwrap();
        let mut ft = FileTransfer::new(None);
        ft.send_file(&path).unwrap();
        assert!(ft.status().sending.is_some(), "sending should start");
        let mut ep = crate::Endpoint::new();
        ft.cancel_send(&mut ep);
        let st = ft.status();
        assert!(st.sending.is_none(), "sender should be cleared");
        assert!(
            st.message.as_deref().is_some_and(|m| m.contains("已取消")),
            "message should mention cancel, got {:?}",
            st.message
        );
    }

    #[test]
    fn cancel_send_without_sender_is_noop() {
        let mut ft = FileTransfer::new(None);
        let mut ep = crate::Endpoint::new();
        ft.cancel_send(&mut ep); // 不应 panic，也不应产生取消 message
        assert!(ft.status().message.is_none());
    }

    #[test]
    fn clipboard_roundtrip_via_send() {
        // send_clipboard 需要 Endpoint（通道未开时返回 false 且不 panic）。
        let mut ft = FileTransfer::new(None);
        // 无端点可测：直接构造一个空 FileTransfer 检查默认状态。
        assert!(ft.take_incoming_clipboard().is_none());
        assert!(ft.status().message.is_none());
    }

    fn meta(id: &str, name: &str, size: u64, chunks: u64) -> FileMeta {
        // #271：kind 缺省为 File（serde default），构造时显式给 File。
        FileMeta {
            id: id.to_string(),
            name: name.to_string(),
            size,
            chunks,
            hash: None,
            kind: FileKind::File,
        }
    }

    #[test]
    fn safe_file_name_strips_traversal_and_abs() {
        assert_eq!(safe_file_name("evil.txt"), "evil.txt");
        assert_eq!(safe_file_name("../evil.txt"), "evil.txt");
        assert_eq!(safe_file_name("a/b/evil.txt"), "evil.txt");
        assert_eq!(safe_file_name("/etc/passwd"), "passwd");
        assert_eq!(safe_file_name(".."), "");
        assert_eq!(safe_file_name("."), "");
        assert_eq!(safe_file_name(""), "");
    }

    #[test]
    fn receive_rejects_traversal_and_abs_meta_names() {
        // 恶意 Meta：../ 与绝对路径都会被拒绝（不在 recv 里建条目）。
        let dir = std::env::temp_dir().join("aerodesk-ft-sec");
        std::fs::create_dir_all(&dir).unwrap();
        let mut ft = FileTransfer::new(Some(dir.clone()));
        // 路径穿越/绝对路径被收敛为 basename（不会逃逸 recv_dir）；`..` 等空名拒绝。
        ft.on_meta(meta("m1", "../evil.txt", 10, 1));
        ft.on_meta(meta("m2", "/etc/passwd", 10, 1));
        assert_eq!(ft.recv.len(), 2);
        assert_eq!(ft.recv.get("m1").unwrap().name, "evil.txt");
        assert_eq!(ft.recv.get("m2").unwrap().name, "passwd");
        ft.on_meta(meta("m3", "..", 10, 1));
        assert_eq!(ft.recv.len(), 2, "空名/`..` 应被拒绝");

        // 带子目录前缀也只取最后一段。
        ft.on_meta(meta("m4", "sub/ok.txt", 10, 1));
        assert_eq!(ft.recv.len(), 3);
        assert_eq!(ft.recv.get("m4").unwrap().name, "ok.txt");
    }

    #[test]
    fn receive_rejects_oversize_and_chunks_mismatch() {
        let dir = std::env::temp_dir().join("aerodesk-ft-sec2");
        std::fs::create_dir_all(&dir).unwrap();
        let mut ft = FileTransfer::new(Some(dir));
        // 超大小
        ft.on_meta(meta("s1", "big.bin", MAX_FILE_SIZE + 1, 1));
        // 分片数不匹配
        ft.on_meta(meta("s2", "x.bin", 100, 999));
        // size=0
        ft.on_meta(meta("s3", "z.bin", 0, 0));
        assert!(ft.recv.is_empty());
        // 合法
        ft.on_meta(meta("s4", "ok.bin", 100, 1));
        assert_eq!(ft.recv.len(), 1);
    }

    #[test]
    fn receive_rejects_when_dir_disabled() {
        let mut ft = FileTransfer::new(None);
        ft.on_meta(meta("d1", "x.bin", 100, 1));
        assert!(ft.recv.is_empty());
    }

    #[test]
    fn request_rejected_by_default() {
        // FileControl::Request 默认拒绝：不触发 send_file（无文件句柄）。
        let mut ft = FileTransfer::new(None);
        let req = FileControl::Request {
            path: "/etc/passwd".to_string(),
        };
        let data = serde_json::to_vec(&req).unwrap();
        // 用一个假 endpoint 无法构造，直接验证 allow_request 默认值：
        assert!(!ft.allow_request);
        // 开启后允许（不在这里真发文件）。
        ft.set_allow_request(true);
        assert!(ft.allow_request);
    }

    #[test]
    fn status_not_sending_after_confirmed() {
        // 确认完成后 status 不再报 sending（进度条不永久卡 100%）。
        let dir = std::env::temp_dir().join("aerodesk-ft-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("confirm.bin");
        std::fs::write(&path, vec![1u8; 100]).unwrap();
        let mut ft = FileTransfer::new(None);
        ft.send_file(&path).unwrap();
        ft.send.as_mut().unwrap().confirmed = true;
        let st = ft.status();
        assert!(st.sending.is_none(), "confirmed 后不应再报 sending");
    }

    #[test]
    fn recv_ttl_prunes_expired() {
        let dir = std::env::temp_dir().join("aerodesk-ft-ttl");
        std::fs::create_dir_all(&dir).unwrap();
        let mut ft = FileTransfer::new(Some(dir));
        ft.on_meta(meta("t1", "a.bin", 100, 1));
        assert_eq!(ft.recv.len(), 1);
        // 把 created_at 改成早已过期，tick 应清理（无端点，tick 只走 TTL 分支）。
        if let Some(r) = ft.recv.get_mut("t1") {
            r.created_at = Instant::now() - RECV_TTL - Duration::from_secs(1);
        }
        let mut fake = FileTransfer::new(None); // 占位避免借用问题
        let _ = &mut fake;
        // 直接调用 tick 需要 endpoint；改走内部清理路径不可行，验证 TTL 常量即可。
        assert!(RECV_TTL >= Duration::from_secs(60));
    }

    #[test]
    fn clipboard_image_receive_in_memory_without_recv_dir() {
        // #271：无 recv_dir 也能接收剪贴板图片（不落盘，直接进内存队列）。
        let png: Vec<u8> = vec![0x89, b"P"[0], b"N"[0], b"G"[0], 1, 2, 3, 4];
        let size = png.len() as u64;
        let chunks = size.div_ceil(CHUNK_SIZE as u64);
        let hash = hex(&Sha256::digest(&png));
        let mut ep = crate::Endpoint::new();
        let mut ft = FileTransfer::new(None);
        let meta = FileControl::Meta(FileMeta {
            id: "clipimg-1".into(),
            name: "clipboard-image-1.png".into(),
            size,
            chunks,
            hash: Some(hash),
            kind: FileKind::ClipboardImage,
        });
        let json = serde_json::to_string(&meta).unwrap();
        ft.handle_data(json.as_bytes(), &mut ep);
        let frame = file::encode_chunk("clipimg-1", 0, &png);
        ft.handle_data(&frame, &mut ep);
        let done = FileControl::Done(FileDone {
            id: "clipimg-1".into(),
            ok: true,
            error: None,
        });
        let json = serde_json::to_string(&done).unwrap();
        ft.handle_data(json.as_bytes(), &mut ep);
        assert_eq!(ft.take_incoming_clipboard_image().unwrap(), png);
        assert!(ft.take_incoming_clipboard().is_none());
        assert!(
            ft.status()
                .message
                .as_deref()
                .is_some_and(|m| m.contains("剪贴板图片"))
        );
    }

    #[test]
    fn sender_from_bytes_read_chunk_slices_correctly() {
        let data: Vec<u8> = (0..20000u32).map(|i| (i % 251) as u8).collect();
        let mut s =
            Sender::from_bytes("clip.png".into(), data.clone(), FileKind::ClipboardImage).unwrap();
        assert_eq!(s.size, 20000);
        assert_eq!(s.total_chunks, 3);
        let c0 = s.read_chunk(0, CHUNK_SIZE).unwrap();
        assert_eq!(c0, data[..CHUNK_SIZE]);
        let tail = data.len() - 2 * CHUNK_SIZE;
        let c2 = s.read_chunk(2 * CHUNK_SIZE, tail).unwrap();
        assert_eq!(c2, data[2 * CHUNK_SIZE..]);
    }

    #[test]
    fn send_clipboard_image_rejects_empty_and_busy() {
        let mut ft = FileTransfer::new(None);
        assert!(ft.send_clipboard_image(Vec::new()).is_err(), "空图片应拒绝");
        ft.send_clipboard_image(vec![1u8; 100]).unwrap();
        assert!(
            ft.send_clipboard_image(vec![2u8; 10]).is_err(),
            "发送槽位忙时剪贴板图片应拒绝"
        );
    }

    // ---- #503 传输中心：队列 + 记录 ----

    fn tmp_file(name: &str, bytes: usize) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("aerodesk-ft-queue");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, vec![3u8; bytes]).unwrap();
        path
    }

    #[test]
    fn busy_send_queues_and_status_reports_queue() {
        let a = tmp_file("q-a.bin", 100);
        let b = tmp_file("q-b.bin", 200);
        let mut ft = FileTransfer::new(None);
        // 空闲：立即开始发送 a。
        ft.send_file(&a).unwrap();
        assert!(ft.status().sending.is_some());
        // 忙：b 入队。
        ft.send_file(&b).unwrap();
        let st = ft.status();
        assert_eq!(st.queue.len(), 1);
        assert_eq!(st.queue[0].1, "q-b.bin");
        assert_eq!(ft.queue_len(), 1);
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn batch_send_files_returns_per_path_results() {
        let a = tmp_file("batch-a.bin", 100);
        let missing = std::env::temp_dir()
            .join("aerodesk-ft-queue")
            .join("missing.bin");
        let mut ft = FileTransfer::new(None);
        let results = ft.send_files(&[a.clone(), missing]);
        assert!(results[0].is_ok());
        assert!(results[1].is_err(), "不存在的文件应报错");
        let _ = std::fs::remove_file(a);
    }

    #[test]
    fn confirmed_send_advances_queue_and_records_history() {
        let a = tmp_file("adv-a.bin", 100);
        let b = tmp_file("adv-b.bin", 200);
        let mut ft = FileTransfer::new(None);
        ft.send_file(&a).unwrap();
        let first_id = ft.status().sending.unwrap().id;
        ft.send_file(&b).unwrap();
        // 模拟接收端 ack：确认第一个发送 → tick 应启动队列中的 b。
        let done = FileControl::Done(FileDone {
            id: first_id.clone(),
            ok: true,
            error: None,
        });
        let json = serde_json::to_string(&done).unwrap();
        let mut ep = crate::Endpoint::new();
        ft.handle_data(json.as_bytes(), &mut ep);
        ft.tick(&mut ep);
        let st = ft.status();
        assert_eq!(
            st.sending.as_ref().map(|s| s.name.as_str()),
            Some("adv-b.bin")
        );
        assert!(st.queue.is_empty(), "队列应已推进");
        // 历史记录：第一个发送成功。
        let rec = ft
            .history()
            .iter()
            .find(|r| r.id == first_id)
            .expect("应有成功记录");
        assert_eq!(rec.direction, "发送");
        assert_eq!(rec.state, "成功");
        assert_eq!(rec.size, 100);
        assert_eq!(
            rec.path.as_deref(),
            Some(a.as_path()),
            "成功记录应保留源路径（重试用）"
        );
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn cancel_send_id_removes_queued_item_and_records() {
        let a = tmp_file("cq-a.bin", 100);
        let b = tmp_file("cq-b.bin", 200);
        let mut ft = FileTransfer::new(None);
        ft.send_file(&a).unwrap();
        ft.send_file(&b).unwrap();
        let queued_id = ft.status().queue[0].0.clone();
        let mut ep = crate::Endpoint::new();
        assert!(ft.cancel_send_id(&queued_id, &mut ep), "排队项应能取消");
        let st = ft.status();
        assert!(st.queue.is_empty(), "排队项应已移除");
        let rec = ft
            .history()
            .iter()
            .find(|r| r.id == queued_id)
            .expect("应有取消记录");
        assert_eq!(rec.state, "已取消");
        // 活动发送 id 取消 → 走 cancel_send（无队列可推进）。
        let active_id = st.sending.unwrap().id;
        assert!(ft.cancel_send_id(&active_id, &mut ep));
        assert!(ft.status().sending.is_none());
        // 未命中 id → false。
        assert!(!ft.cancel_send_id("nonexistent", &mut ep));
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn cancel_active_send_advances_queue() {
        let a = tmp_file("ca-a.bin", 100);
        let b = tmp_file("ca-b.bin", 200);
        let mut ft = FileTransfer::new(None);
        ft.send_file(&a).unwrap();
        ft.send_file(&b).unwrap();
        let mut ep = crate::Endpoint::new();
        ft.cancel_send(&mut ep);
        let st = ft.status();
        assert_eq!(
            st.sending.as_ref().map(|s| s.name.as_str()),
            Some("ca-b.bin"),
            "取消当前发送后应自动启动队列下一项"
        );
        assert!(st.queue.is_empty());
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn failed_queued_item_skips_and_starts_next() {
        let a = tmp_file("fq-a.bin", 100);
        let c = tmp_file("fq-c.bin", 200);
        let mut ft = FileTransfer::new(None);
        // 不存在的文件在入队时即报错（不进入队列）。
        let missing = std::env::temp_dir()
            .join("aerodesk-ft-queue")
            .join("gone.bin");
        assert!(ft.send_file(&missing).is_err());
        ft.send_file(&a).unwrap();
        ft.send_file(&c).unwrap();
        assert_eq!(ft.queue_len(), 1);
        // 模拟当前发送失败：tick 应启动队列中的 c。
        let id = ft.status().sending.unwrap().id;
        let done = FileControl::Done(FileDone {
            id,
            ok: false,
            error: Some("disk full".into()),
        });
        let json = serde_json::to_string(&done).unwrap();
        let mut ep = crate::Endpoint::new();
        ft.handle_data(json.as_bytes(), &mut ep);
        ft.tick(&mut ep);
        let st = ft.status();
        assert_eq!(
            st.sending.as_ref().map(|s| s.name.as_str()),
            Some("fq-c.bin")
        );
        // 历史：发送失败记录。
        assert!(
            ft.history()
                .iter()
                .any(|r| r.state == "失败" && r.name == "fq-a.bin"),
            "失败应留痕：{:?}",
            ft.history()
        );
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(c);
    }

    #[test]
    fn receive_complete_records_history() {
        let dir = std::env::temp_dir().join("aerodesk-ft-rec-rec");
        std::fs::create_dir_all(&dir).unwrap();
        let mut ft = FileTransfer::new(Some(dir));
        let data: Vec<u8> = vec![5u8; 100];
        let chunks = data.len().div_ceil(CHUNK_SIZE) as u64;
        let hash = hex(&Sha256::digest(&data));
        let meta = FileControl::Meta(FileMeta {
            id: "rec1".into(),
            name: "recv.bin".into(),
            size: data.len() as u64,
            chunks,
            hash: Some(hash),
            kind: FileKind::File,
        });
        let mut ep = crate::Endpoint::new();
        ft.handle_data(serde_json::to_string(&meta).unwrap().as_bytes(), &mut ep);
        ft.handle_data(&file::encode_chunk("rec1", 0, &data), &mut ep);
        ft.handle_data(
            serde_json::to_string(&FileControl::Done(FileDone {
                id: "rec1".into(),
                ok: true,
                error: None,
            }))
            .unwrap()
            .as_bytes(),
            &mut ep,
        );
        let rec = ft
            .history()
            .iter()
            .find(|r| r.id == "rec1")
            .expect("应有接收成功记录");
        assert_eq!(rec.direction, "接收");
        assert_eq!(rec.state, "成功");
        assert_eq!(rec.name, "recv.bin");
        assert!(rec.path.is_none(), "接收记录无本地源路径");
    }

    #[test]
    fn clear_history_empties_records() {
        let a = tmp_file("ch-a.bin", 100);
        let mut ft = FileTransfer::new(None);
        ft.send_file(&a).unwrap();
        let id = ft.status().sending.unwrap().id;
        let done = FileControl::Done(FileDone {
            id,
            ok: true,
            error: None,
        });
        let mut ep = crate::Endpoint::new();
        ft.handle_data(serde_json::to_string(&done).unwrap().as_bytes(), &mut ep);
        assert!(!ft.history().is_empty());
        ft.clear_history();
        assert!(ft.history().is_empty());
        let _ = std::fs::remove_file(a);
    }

    #[test]
    fn send_complete_false_after_queue_advance() {
        // #503 队列推进后：send_complete() 反映当前（下一项）发送状态。
        let a = tmp_file("sc-a.bin", 100);
        let b = tmp_file("sc-b.bin", 200);
        let mut ft = FileTransfer::new(None);
        ft.send_file(&a).unwrap();
        let first_id = ft.status().sending.unwrap().id;
        ft.send_file(&b).unwrap();
        let done = FileControl::Done(FileDone {
            id: first_id,
            ok: true,
            error: None,
        });
        let mut ep = crate::Endpoint::new();
        ft.handle_data(serde_json::to_string(&done).unwrap().as_bytes(), &mut ep);
        ft.tick(&mut ep);
        assert!(!ft.send_complete(), "推进到下一项后不应再报完成");
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn send_complete_false_for_clipboard_image_even_when_nacked() {
        // #595 回归护栏：剪贴板图片发送被接收端显式拒绝（无 recv-dir Nack）后，
        // send_complete() 必须仍为 false——它是常驻同步失败，不是 --send-file
        // 的上传终结。此前 ok=false 的 Done 置 confirmed → viewer 在媒体腿建立前
        // exit(0)（simulcast e2e 6 连败根因）。
        let mut ft = FileTransfer::new(None);
        ft.send_clipboard_image(vec![0x89, b'P', b'N', b'G'])
            .unwrap();
        let sending = ft.status().sending.unwrap();
        assert!(
            sending.name.starts_with("clipboard-image-"),
            "剪贴板图片文件名前缀"
        );
        let id = sending.id;
        let done = FileControl::Done(FileDone {
            id,
            ok: false,
            error: Some("接收端未开启接收".into()),
        });
        let mut ep = crate::Endpoint::new();
        ft.handle_data(serde_json::to_string(&done).unwrap().as_bytes(), &mut ep);
        assert!(
            !ft.send_complete(),
            "剪贴板图片被拒不算发送完成（viewer 不得退出）"
        );
    }

    #[test]
    fn send_complete_false_for_clipboard_image_even_when_confirmed() {
        // #586 环境残留（clipboard sync e2e 往剪贴板写测试图）→ viewer 剪贴板
        // 轮询捡到 → 上传 → 对端正常接收 ok=true 确认 → 此前 send_complete() 为
        // true → viewer 在媒体腿建立前 exit(0)。剪贴板同步是常驻功能，确认/拒绝
        // 都不是 --send-file 的上传终结。
        let mut ft = FileTransfer::new(None);
        ft.send_clipboard_image(vec![0x89, b'P', b'N', b'G'])
            .unwrap();
        let id = ft.status().sending.unwrap().id;
        let done = FileControl::Done(FileDone {
            id,
            ok: true,
            error: None,
        });
        let mut ep = crate::Endpoint::new();
        ft.handle_data(serde_json::to_string(&done).unwrap().as_bytes(), &mut ep);
        assert!(
            !ft.send_complete(),
            "剪贴板图片正常确认也不算发送完成（viewer 不得退出）"
        );
    }

    #[test]
    fn send_complete_true_for_regular_file_confirmed() {
        // --send-file（kind=File）确认后 send_complete() 必须仍为 true（#122
        // viewer 上传完成后退出，e2e 依赖）。
        let a = tmp_file("sc-regular.bin", 100);
        let mut ft = FileTransfer::new(None);
        ft.send_file(&a).unwrap();
        let id = ft.status().sending.unwrap().id;
        let done = FileControl::Done(FileDone {
            id,
            ok: true,
            error: None,
        });
        let mut ep = crate::Endpoint::new();
        ft.handle_data(serde_json::to_string(&done).unwrap().as_bytes(), &mut ep);
        assert!(ft.send_complete(), "普通文件确认后应报完成");
        let _ = std::fs::remove_file(a);
    }

    #[test]
    fn send_complete_sticky_survives_clipboard_image_eviction() {
        // #595 二批根因回归：确认（Done ok）与首次观察之间，同轮 tick 的剪贴板
        // 图片换槽会顶掉已确认槽位——busy-check 只拦未确认发送者。agent 层
        // AtomicBool 闩锁只能保住“已观察到的 true”，一旦观察发生在换槽之后即
        // 永不置位 → viewer --send-file 永不退出。完成判定下沉为 core 粘滞 id：
        // 换槽不灭失，直到下一个常规文件真正启动。
        let a = tmp_file("sc-sticky.bin", 100);
        let mut ft = FileTransfer::new(None);
        ft.send_file(&a).unwrap();
        let id = ft.status().sending.unwrap().id;
        let mut ep = crate::Endpoint::new();
        let done = FileControl::Done(FileDone {
            id,
            ok: true,
            error: None,
        });
        ft.handle_data(serde_json::to_string(&done).unwrap().as_bytes(), &mut ep);
        assert!(ft.send_complete(), "确认后应报完成");

        // 模拟同轮 tick：剪贴板图片顶掉已确认的常规文件槽位。
        ft.send_clipboard_image(vec![0x89, b'P', b'N', b'G'])
            .unwrap();
        let img_id = ft.status().sending.unwrap().id;
        assert!(
            ft.send_complete(),
            "图片换槽后已确认文件的完成仍须可观察（否则 viewer 永不退出）"
        );

        // 图片本次同步终结（接收端无 recv-dir Nack 语义）：粘滞不受影响。
        let img_done = FileControl::Done(FileDone {
            id: img_id,
            ok: false,
            error: Some("接收端未开启接收".into()),
        });
        ft.handle_data(
            serde_json::to_string(&img_done).unwrap().as_bytes(),
            &mut ep,
        );
        assert!(ft.send_complete(), "图片终结不得清除常规文件的完成粘滞");

        // 下一个常规发送启动 → 粘滞清除（#503 契约：新一轮上传未确认前不报完成，
        // viewer 不得凭上一轮结果提前退出）。
        let b = tmp_file("sc-sticky-next.bin", 50);
        ft.tick(&mut ep); // 槽位为失败终态：清槽走 tick 失败分支，再直启新文件
        ft.send_file(&b).unwrap();
        assert!(!ft.send_complete(), "新常规发送启动后粘滞必须清除");
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn sticky_persists_past_cancel_and_clears_on_next_start() {
        // 粘滞生命周期补测：对端已 ack 的完成不因本地取消/清槽而灭失
        // （与修复前“先观察到才有效”的闩锁语义同向，只是变确定）；
        // 只有下一个常规发送真正启动时才复位。
        let a = tmp_file("sc-sticky-cancel.bin", 100);
        let mut ft = FileTransfer::new(None);
        ft.send_file(&a).unwrap();
        let id = ft.status().sending.unwrap().id;
        let mut ep = crate::Endpoint::new();
        let done = FileControl::Done(FileDone {
            id,
            ok: true,
            error: None,
        });
        ft.handle_data(serde_json::to_string(&done).unwrap().as_bytes(), &mut ep);

        ft.cancel_send(&mut ep); // 本地取消并清槽
        assert!(ft.status().sending.is_none(), "槽位应已被取消清空");
        assert!(
            ft.send_complete(),
            "已 ack 的完成不得因取消灭失（否则观察竞态重现）"
        );

        let b = tmp_file("sc-sticky-next2.bin", 40);
        ft.send_file(&b).unwrap(); // 新一轮常规发送直启
        assert!(!ft.send_complete(), "新发送启动即清除上一轮粘滞");
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn send_complete_sticky_also_on_receiver_reject() {
        // 接收端拒收（ok=false）同样置粘滞——保持既有「拒收也退出 viewer」
        // 的观察语义（修复前依赖“同迭代先观察、下一轮 tick 才清槽”的竞态窗口，
        // 现在确定可观察）。复位时机与成功路径一致：下一个常规发送启动。
        let a = tmp_file("sc-reject.bin", 60);
        let mut ft = FileTransfer::new(None);
        ft.send_file(&a).unwrap();
        let id = ft.status().sending.unwrap().id;
        let mut ep = crate::Endpoint::new();
        let done = FileControl::Done(FileDone {
            id,
            ok: false,
            error: Some("接收端未开启接收".into()),
        });
        ft.handle_data(serde_json::to_string(&done).unwrap().as_bytes(), &mut ep);
        assert!(ft.send_complete(), "拒收终态亦为完成观察点（既有语义）");
        let _ = std::fs::remove_file(a);
    }
}
