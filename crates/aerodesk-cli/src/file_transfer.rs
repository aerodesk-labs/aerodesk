//! #72 file transfer over WebRTC data channel (label "file").
//!
//! Send: read file -> SHA-256 -> Meta(JSON) -> Chunk(binary) -> Done.
//! Receive: aggregate chunks -> verify size/hash on Done -> write to --recv-dir.
//! Backpressure: if channel write returns false, pause and retry next tick.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aerodesk_core::Endpoint;
use aerodesk_protocol::file::{
    self, CHUNK_SIZE, FileCancel, FileControl, FileDone, FileMeta, FileNack,
};
use sha2::{Digest, Sha256};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Process-wide file transfer state machine.
static FILE_TX: std::sync::Mutex<Option<FileTransfer>> = std::sync::Mutex::new(None);

/// Initialize the process-wide state machine (call once from main).
pub fn init(send_file: Option<PathBuf>, recv_dir: Option<PathBuf>) {
    let ft = FileTransfer::new(send_file, recv_dir).unwrap_or_else(|e| {
        eprintln!("file transfer init failed: {e}");
        std::process::exit(1);
    });
    *FILE_TX.lock().unwrap() = Some(ft);
}

/// Route a data-channel event to the state machine (no-op unless label == "file").
pub fn handle_event(ev: &aerodesk_core::ClientEvent, endpoint: &mut Endpoint) {
    if let Ok(mut ft) = FILE_TX.lock()
        && let Some(ft) = ft.as_mut()
    {
        ft.handle_event(ev, endpoint);
    }
}

/// Advance file sending once per event loop iteration.
pub fn tick(endpoint: &mut Endpoint) {
    if let Ok(mut ft) = FILE_TX.lock()
        && let Some(ft) = ft.as_mut()
    {
        ft.tick(endpoint);
    }
}

/// File transfer state machine (send + receive).
pub struct FileTransfer {
    send: Option<Sender>,
    recv: HashMap<String, Receiver>,
    recv_dir: Option<PathBuf>,
}

struct Sender {
    id: String,
    name: String,
    data: Vec<u8>,
    hash: String,
    total_chunks: u64,
    next_chunk: u64,
    meta_sent: bool,
    done_sent: bool,
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
}

impl FileTransfer {
    /// Create the state machine. `send_file` is the file to send; `recv_dir` is
    /// where received files are written.
    pub fn new(send_file: Option<PathBuf>, recv_dir: Option<PathBuf>) -> Result<Self, String> {
        let send = match send_file {
            Some(p) => Some(Sender::open(&p)?),
            None => None,
        };
        Ok(Self {
            send,
            recv: HashMap::new(),
            recv_dir,
        })
    }

    /// Handle a data-channel event (only acts on label == "file").
    pub fn handle_event(&mut self, ev: &aerodesk_core::ClientEvent, endpoint: &mut Endpoint) {
        match ev {
            aerodesk_core::ClientEvent::ChannelOpen(label, _) if label == "file" => {
                tracing::info!("file channel open");
            }
            aerodesk_core::ClientEvent::ChannelData(cid, _, data)
                if endpoint.channel_label(*cid).as_deref() == Some("file") =>
            {
                self.handle_data(data, endpoint);
            }
            _ => {}
        }
    }

    /// Advance sending one step per event loop iteration.
    pub fn tick(&mut self, endpoint: &mut Endpoint) {
        if let Some(s) = &mut self.send {
            s.tick(endpoint);
        }
    }

    fn handle_data(&mut self, data: &[u8], endpoint: &mut Endpoint) {
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
        }
    }

    fn on_meta(&mut self, m: FileMeta) {
        if self.recv_dir.is_none() {
            tracing::info!("file receive disabled (no --recv-dir); ignore {}", m.name);
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
        if let Some(f) = r.received_flags.get_mut(index as usize) {
            *f = true;
        }
        r.received += 1;
    }

    fn on_done(&mut self, d: FileDone, endpoint: &mut Endpoint) {
        let Some(r) = self.recv.remove(&d.id) else {
            return;
        };
        let Some(dir) = self.recv_dir.clone() else {
            return;
        };
        if !d.ok {
            tracing::warn!("file {} failed: {:?}", r.name, d.error);
            return;
        }
        // 计算缺失分片：SFU 转发可能丢包，缺了要回 Nack 让发送端补。
        let missing: Vec<u64> = r
            .received_flags
            .iter()
            .enumerate()
            .filter(|(_, got)| !**got)
            .map(|(i, _)| i as u64)
            .collect();
        if !missing.is_empty() {
            let nack = FileControl::Nack(FileNack {
                id: d.id.clone(),
                missing: missing.iter().take(512).copied().collect(),
            });
            if let Ok(json) = serde_json::to_string(&nack) {
                let _ = endpoint.send_channel_data("file", false, json.as_bytes());
            }
            tracing::info!(
                "file {} missing {} chunks, requested resend",
                r.name,
                missing.len()
            );
            self.recv.insert(d.id, r);
            return;
        }
        if r.received != r.total_chunks || r.buf.len() as u64 != r.size {
            tracing::error!(
                "file {} incomplete: {}/{} chunks, {} bytes (expected {})",
                r.name,
                r.received,
                r.total_chunks,
                r.buf.len(),
                r.size
            );
            return;
        }
        if let Some(h) = &r.hash {
            let actual = hex(&Sha256::digest(&r.buf));
            if &actual != h {
                tracing::error!("file {} hash mismatch (expected {h}, got {actual})", r.name);
                return;
            }
        }
        let path = dir.join(&r.name);
        if let Err(e) = std::fs::write(&path, &r.buf) {
            tracing::error!("file {} write failed: {e}", path.display());
            return;
        }
        tracing::info!("file receive complete: {} -> {}", r.name, path.display());
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

    fn on_cancel(&mut self, c: FileCancel) {
        if self.recv.remove(&c.id).is_some() {
            tracing::info!("file {} cancelled", c.id);
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
        Ok(Self {
            id: format!("tx{}", std::process::id()),
            name,
            data,
            hash,
            total_chunks,
            next_chunk: 0,
            meta_sent: false,
            done_sent: false,
            last_progress: Instant::now(),
            start_after: Instant::now() + Duration::from_secs(3),
            resend: Vec::new(),
            confirmed: false,
        })
    }

    fn tick(&mut self, endpoint: &mut Endpoint) {
        if self.confirmed {
            return;
        }
        if Instant::now() < self.start_after {
            return;
        }
        if !self.meta_sent {
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
                tracing::info!("file send start: {} ({} bytes)", self.name, self.data.len());
                self.meta_sent = true;
            }
            return;
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
        if !self.done_sent {
            let done = FileControl::Done(FileDone {
                id: self.id.clone(),
                ok: true,
                error: None,
            });
            if let Ok(json) = serde_json::to_string(&done)
                && endpoint.send_channel_data("file", false, json.as_bytes())
            {
                self.done_sent = true;
                tracing::info!(
                    "file send complete: {} ({} bytes)",
                    self.name,
                    self.data.len()
                );
            }
        }
    }
}
