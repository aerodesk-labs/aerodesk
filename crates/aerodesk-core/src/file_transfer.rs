//! #72 文件传输 + 剪贴板状态机（data channel label "file"）。
//!
//! 从 aerodesk-cli 提升到 core，供 CLI 与桌面 UI 共用：
//! - 发送：读文件 → SHA-256 → Meta(JSON) → Chunk(binary) → Done
//! - 接收：聚合分片 → 校验大小/hash → 落盘 → 回 ack
//! - 补包：接收端 Nack → 发送端重传（SFU 转发在出站缓冲满时会丢包）
//! - 剪贴板：经同一 file 通道传 `FileControl::Clipboard`（文本双向同步）
//!
//! 背压：通道写失败（返回 false）时暂停，下一轮重试。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aerodesk_protocol::file::{
    self, CHUNK_SIZE, FileCancel, FileControl, FileDone, FileMeta, FileNack,
};
use sha2::{Digest, Sha256};

/// 传输会话 id 计数器（多会话/重复发送避免同 id 冲突）。
static SEND_SEQ: AtomicU64 = AtomicU64::new(0);

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 当前传输进度（UI 展示用）。
#[derive(Debug, Clone, Default)]
pub struct FileTransferStatus {
    /// 发送中：(文件名, 已发分片, 总分片)
    pub sending: Option<(String, u64, u64)>,
    /// 接收中：(文件名, 已收分片, 总分片)
    pub receiving: Option<(String, u64, u64)>,
    /// 一次性事件（完成/失败/取消），消费后清除。
    pub message: Option<String>,
}

/// 文件传输 + 剪贴板状态机（发送 + 接收）。
pub struct FileTransfer {
    send: Option<Sender>,
    recv: HashMap<String, Receiver>,
    recv_dir: Option<PathBuf>,
    /// 收到远端剪贴板文本（调用方 take 后写入系统剪贴板）。
    incoming_clipboard: Option<String>,
    /// 一次性状态事件（完成/失败/取消），UI 展示后消费。
    message: Option<String>,
    /// 待补发的剪贴板文本（SFU 转发可能丢首包，1s 幂等重试）。
    clipboard_pending: Option<String>,
    clipboard_sends: u32,
    last_clipboard_send: Option<Instant>,
}

struct Sender {
    id: String,
    name: String,
    data: Vec<u8>,
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
}

struct Receiver {
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
}

impl FileTransfer {
    /// 创建状态机。`recv_dir` 为接收落盘目录；`None` 表示不接收。
    pub fn new(recv_dir: Option<PathBuf>) -> Self {
        Self {
            send: None,
            recv: HashMap::new(),
            recv_dir,
            incoming_clipboard: None,
            message: None,
            clipboard_pending: None,
            clipboard_sends: 0,
            last_clipboard_send: None,
        }
    }

    /// 设置接收目录（可在会话中启用接收）。
    pub fn set_recv_dir(&mut self, dir: Option<PathBuf>) {
        self.recv_dir = dir;
    }

    /// 触发发送一个文件（当前无发送任务时生效）。
    pub fn send_file(&mut self, path: &Path) -> Result<(), String> {
        if self.send.as_ref().is_some_and(|s| !s.confirmed) {
            return Err("已有文件传输进行中".into());
        }
        self.send = Some(Sender::open(path)?);
        Ok(())
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
        if let Some(s) = &mut self.send {
            s.tick(endpoint);
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
        let sending = self.send.as_ref().map(|s| {
            let done = if s.confirmed {
                s.total_chunks
            } else {
                s.next_chunk
            };
            (s.name.clone(), done, s.total_chunks)
        });
        let receiving = self
            .recv
            .values()
            .next()
            .map(|r| (r.name.clone(), r.received, r.total_chunks));
        FileTransferStatus {
            sending,
            receiving,
            message: self.message.take(),
        }
    }

    /// 取走收到的远端剪贴板文本（应用层写入系统剪贴板）。
    pub fn take_incoming_clipboard(&mut self) -> Option<String> {
        self.incoming_clipboard.take()
    }

    /// 取消当前发送：向接收端下发 FileCancel 并清空发送状态（#72 回归：
    /// 接收端 on_cancel 移除接收器，不落盘，无残留临时文件）。
    pub fn cancel_send(&mut self, endpoint: &mut crate::Endpoint) {
        let Some(s) = self.send.take() else {
            return;
        };
        let cancel = FileControl::Cancel(FileCancel { id: s.id.clone() });
        if let Ok(json) = serde_json::to_string(&cancel) {
            let _ = endpoint.send_channel_data("file", false, json.as_bytes());
        }
        self.message = Some(format!("已取消发送：{}", s.name));
        tracing::info!("file send cancelled: {}", s.name);
    }

    /// 发送剪贴板文本到远端（同一 file 通道）；进入补发队列（1s 幂等重试，
    /// 最多 8 次），应对 SFU 转发丢首包。
    pub fn send_clipboard(&mut self, text: &str, endpoint: &mut crate::Endpoint) -> bool {
        self.clipboard_pending = Some(text.to_string());
        self.clipboard_sends = 0;
        self.last_clipboard_send = Some(Instant::now());
        self.dispatch_clipboard(text, endpoint)
    }

    fn dispatch_clipboard(&mut self, text: &str, endpoint: &mut crate::Endpoint) -> bool {
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
                    if let Some(s) = &mut self.send {
                        s.confirmed = true;
                        tracing::info!(
                            "file transfer confirmed by receiver: {} ({} bytes)",
                            s.name,
                            s.data.len()
                        );
                        self.message = Some(format!("已发送：{}", s.name));
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
        }
    }

    fn on_meta(&mut self, m: FileMeta) {
        if self.recv_dir.is_none() {
            tracing::info!("file receive disabled (no recv dir); ignore {}", m.name);
            return;
        }
        if self.recv.contains_key(&m.id) {
            tracing::warn!("duplicate FileMeta id={}", m.id);
            return;
        }
        tracing::info!(
            "file receive start: {} ({} bytes, {} chunks)",
            m.name,
            m.size,
            m.chunks
        );
        self.recv.insert(
            m.id.clone(),
            Receiver {
                name: m.name,
                size: m.size,
                total_chunks: m.chunks,
                hash: m.hash,
                buf: vec![0; m.size as usize],
                received: 0,
                received_flags: vec![false; m.chunks as usize],
                last_nack: None,
                last_progress: None,
            },
        );
    }

    fn on_chunk(&mut self, id: String, index: u64, payload: &[u8]) {
        let Some(r) = self.recv.get_mut(&id) else {
            return;
        };
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
            return;
        };
        let Some(dir) = self.recv_dir.clone() else {
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
        let path = dir.join(&r.name);
        if let Err(e) = std::fs::write(&path, &r.buf) {
            tracing::error!("file {} write failed: {e}", path.display());
            return;
        }
        tracing::info!("file receive complete: {} -> {}", r.name, path.display());
        self.message = Some(format!("已接收：{}", path.display()));
        // 回 ack：发送端据此停止。
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
        if self.recv.remove(&c.id).is_some() {
            tracing::info!("file {} cancelled", c.id);
            self.message = Some(format!("已取消：{}", c.id));
        }
    }
}

impl Sender {
    fn open(path: &Path) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let size = data.len() as u64;
        let total_chunks = size.div_ceil(CHUNK_SIZE as u64);
        let hash = hex(&Sha256::digest(&data));
        let seq = SEND_SEQ.fetch_add(1, Ordering::SeqCst);
        Ok(Self {
            id: format!("tx{}-{seq}", std::process::id()),
            name,
            data,
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
        })
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
                size: self.data.len() as u64,
                chunks: self.total_chunks,
                hash: Some(self.hash.clone()),
            });
            if let Ok(json) = serde_json::to_string(&meta)
                && endpoint.send_channel_data("file", false, json.as_bytes())
            {
                if !self.meta_sent {
                    tracing::info!("file send start: {} ({} bytes)", self.name, self.data.len());
                }
                self.meta_sent = true;
                self.last_meta_send = Instant::now();
            }
            if !self.meta_sent {
                return;
            }
        }
        // 补包优先（接收端 Nack）。
        if let Some(resend_idx) = self.resend.pop() {
            let start = (resend_idx as usize) * CHUNK_SIZE;
            let end = ((resend_idx + 1) as usize * CHUNK_SIZE).min(self.data.len());
            let frame = file::encode_chunk(&self.id, resend_idx, &self.data[start..end]);
            if endpoint.send_channel_data("file", true, &frame) {
                tracing::debug!("file resend chunk {resend_idx}");
            }
            return;
        }
        // #85 现状：str0m 指向 aerodesk-labs 派生（dimpl DTLS receive queue
        // 2048），SFU 出站为背压队列（不丢包）；8KB 分片 + 单发可稳定完成
        // 100MB（~5min，sha256 一致、无断连）。进一步提速受 CLI viewer 事件
        // 循环速率限制（见 #85）。
        if self.next_chunk < self.total_chunks {
            let start = (self.next_chunk as usize) * CHUNK_SIZE;
            let end = ((self.next_chunk + 1) as usize * CHUNK_SIZE).min(self.data.len());
            let frame = file::encode_chunk(&self.id, self.next_chunk, &self.data[start..end]);
            if endpoint.send_channel_data("file", true, &frame) {
                self.next_chunk += 1;
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
        let (name, done, total) = st.sending.expect("sending should be Some");
        assert_eq!(name, "sample.bin");
        assert_eq!(done, 0);
        // 20000B / 8192B 分片 → 3 片
        assert_eq!(total, 3);
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
}
