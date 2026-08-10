//! SFU 侧录制/审计模块（可选）。
//!
//! 通过环境变量 `RECORD_DIR` 开启：将每个房间收到的媒体载荷落盘，
//! 并输出 JSON 审计日志（会话起止/包数/字节数）。
//!
//! 录制模式：
//! - 自动模式（默认）：任何房间首个媒体包即自动录制；
//! - 按需模式（`RECORD_ON_DEMAND=1`）：仅 `start(room)` 显式开启的房间录制，
//!   支持内部 API 按房间 start/stop/status（#160）。
//!
//! 文件格式（`ADREC2`，#234 容器化）：
//! ```text
//! magic "ADREC2\n"
//! 每包: [u8 kind(0=video,1=audio)][u8 codec][u8 flags(bit0=keyframe)][u8 rsv]
//!       [u64 wall_us][u64 rtp_ts][u32 len][payload bytes]
//! ```
//! codec id：0=none 1=H264 2=H265 3=VP8 4=VP9 5=AV1 6=Opus 7=PCMU。
//! 媒体文件：`{RECORD_DIR}/{room}.adrec`；元数据：`{room}.meta.json`；
//! 审计日志：`{RECORD_DIR}/audit.log`（JSON Lines）。

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write, sink};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAGIC: &[u8] = b"ADREC2\n";

/// ADREC2 包 kind 值。
pub const KIND_VIDEO: u8 = 0;
pub const KIND_AUDIO: u8 = 1;
/// ADREC2 包 codec id（#234：与 str0m Codec 映射，供 rec2mp4 识别）。
pub const CODEC_NONE: u8 = 0;
pub const CODEC_H264: u8 = 1;
pub const CODEC_H265: u8 = 2;
pub const CODEC_VP8: u8 = 3;
pub const CODEC_VP9: u8 = 4;
pub const CODEC_AV1: u8 = 5;
pub const CODEC_OPUS: u8 = 6;
pub const CODEC_PCMU: u8 = 7;
/// ADREC2 包固定头长：kind+codec+flags+rsv+wall_us+rtp_ts+len。
pub const PACKET_HEADER_LEN: usize = 24;

pub fn codec_id(codec: str0m::format::Codec) -> u8 {
    use str0m::format::Codec;
    match codec {
        Codec::H264 => CODEC_H264,
        Codec::H265 => CODEC_H265,
        Codec::Vp8 => CODEC_VP8,
        Codec::Vp9 => CODEC_VP9,
        Codec::Av1 => CODEC_AV1,
        Codec::Opus => CODEC_OPUS,
        Codec::PCMU => CODEC_PCMU,
        _ => CODEC_NONE,
    }
}

/// 每房间录制状态。
struct Recording {
    room: String,
    path: PathBuf,
    // Box<dyn Write + Send>：失败哨兵用内存 sink，避免依赖 /dev/null（#15）。
    writer: BufWriter<Box<dyn Write + Send>>,
    started_at: u64,
    packets: u64,
    bytes: u64,
    /// #180 轮转：当前段编号（0 = `{room}.adrec`，N>0 = `{room}.adrec.{N}`）。
    segment: u32,
    segment_bytes: u64,
    segment_started_at: u64,
    /// 已关闭段（path, packets, bytes）。
    segments: Vec<(String, u64, u64)>,
    /// #15：创建/写入失败后标记为失败，本次会话跳过该房间录制（不 panic）。
    failed: bool,
}

impl Recording {
    /// 打开指定段录制文件并写入 magic（#180 轮转：segment 0 = `{room}.adrec`）。
    fn open_segment_writer(
        root: &Path,
        room: &str,
        segment: u32,
    ) -> std::io::Result<(PathBuf, BufWriter<Box<dyn Write + Send>>)> {
        let safe = safe_name(room);
        let path = if segment == 0 {
            root.join(format!("{safe}.adrec"))
        } else {
            root.join(format!("{safe}.adrec.{segment}"))
        };
        let file = File::create(&path)?;
        let mut writer: BufWriter<Box<dyn Write + Send>> = BufWriter::new(Box::new(file));
        writer.write_all(MAGIC)?;
        Ok((path, writer))
    }

    /// 打开首个录制文件并写入 magic。失败返回 Err（调用方决定降级策略）。
    fn open(root: &Path, room: &str, ts: u64) -> std::io::Result<Recording> {
        let (path, writer) = Recording::open_segment_writer(root, room, 0)?;
        Ok(Recording {
            room: room.to_string(),
            path,
            writer,
            started_at: ts,
            packets: 0,
            bytes: 0,
            segment: 0,
            segment_bytes: 0,
            segment_started_at: ts,
            segments: Vec::new(),
            failed: false,
        })
    }

    /// 失败哨兵：该房间本次会话不再尝试录制。
    fn failed(room: &str, ts: u64) -> Recording {
        Recording {
            room: room.to_string(),
            path: PathBuf::new(),
            // 内存 sink：任何环境都不依赖 /dev/null，绝不 panic。
            writer: BufWriter::new(Box::new(sink())),
            started_at: ts,
            packets: 0,
            bytes: 0,
            segment: 0,
            segment_bytes: 0,
            segment_started_at: ts,
            segments: Vec::new(),
            failed: true,
        }
    }
}

/// 录制器（进程级单例，跨分片共享）。
pub struct Recorder {
    root: PathBuf,
    audit: Mutex<File>,
    recordings: Mutex<HashMap<String, Recording>>,
    /// 按需模式（RECORD_ON_DEMAND=1）：只录显式 start() 的房间（#160）。
    on_demand: bool,
    /// 单段字节上限（0=不限，#180 轮转）。
    max_bytes: u64,
    /// 单段时间上限（微秒，0=不限，#180 轮转）。
    max_secs: u64,
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn safe_name(room: &str) -> String {
    let mut s = String::with_capacity(room.len());
    for ch in room.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            s.push(ch);
        } else {
            s.push('_');
        }
    }
    if s.is_empty() {
        s.push_str("room");
    }
    s
}

impl Recorder {
    /// 创建录制器并打开审计日志（追加模式）。目录不存在会自动创建。
    pub fn new(
        root: impl AsRef<Path>,
        on_demand: bool,
        max_bytes: u64,
        max_secs: u64,
    ) -> std::io::Result<Recorder> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        let audit = OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("audit.log"))?;
        Ok(Recorder {
            root,
            audit: Mutex::new(audit),
            recordings: Mutex::new(HashMap::new()),
            on_demand,
            max_bytes,
            max_secs,
        })
    }

    /// #180 轮转检查：段超限则关段、开新段（保留总计数与 segments 历史）。
    fn maybe_rotate(&self, entry: &mut Recording, ts: u64) {
        let over_bytes = self.max_bytes > 0 && entry.segment_bytes >= self.max_bytes;
        let over_time =
            self.max_secs > 0 && ts.saturating_sub(entry.segment_started_at) >= self.max_secs;
        if !over_bytes && !over_time {
            return;
        }
        let _ = entry.writer.flush();
        entry
            .segments
            .push((entry.path.display().to_string(), entry.packets, entry.bytes));
        let next = entry.segment + 1;
        match Recording::open_segment_writer(&self.root, &entry.room, next) {
            Ok((path, writer)) => {
                entry.path = path;
                entry.writer = writer;
                entry.segment = next;
                entry.segment_bytes = 0;
                entry.segment_started_at = ts;
                debug!("recorder: room={} rotated to segment {next}", entry.room);
            }
            Err(e) => {
                warn!(
                    "recorder: room={} 轮转打开段 {next} 失败（{e}），保持当前段",
                    entry.room
                );
            }
        }
    }

    /// 关段 + 写 meta（stop/finalize 共用；segments 汇总每段 path/packets/bytes）。
    fn finalize_recording(&self, rec: &mut Recording, now: u64) {
        let _ = rec.writer.flush();
        rec.segments
            .push((rec.path.display().to_string(), rec.packets, rec.bytes));
        let meta = serde_json::json!({
            "room": rec.room,
            "started_at": rec.started_at,
            "ended_at": now,
            "packets": rec.packets,
            "bytes": rec.bytes,
            "segments": rec
                .segments
                .iter()
                .map(|(path, packets, bytes)| serde_json::json!({
                    "path": path,
                    "packets": packets,
                    "bytes": bytes,
                }))
                .collect::<Vec<_>>(),
        });
        // meta 始终写房间基名 `{room}.meta.json`（轮转后 rec.path 是段文件，不能用 with_extension）。
        let meta_path = self
            .root
            .join(format!("{}.meta.json", safe_name(&rec.room)));
        let _ = fs::write(
            &meta_path,
            serde_json::to_string_pretty(&meta).unwrap_or_default(),
        );
    }

    fn audit(&self, line: serde_json::Value) {
        let mut f = self.audit.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }

    /// 写一条审计事件（含按需录制 API 调用追责，#240）。
    /// `payload` 与 `event`/`ts` 合并后以 JSON Lines 追加到 audit.log。
    pub fn audit_event(&self, event: &str, payload: serde_json::Value) {
        let mut line = payload;
        line["ts"] = serde_json::json!(now_micros());
        line["event"] = serde_json::json!(event);
        self.audit(line);
    }

    /// 记录一条媒体载荷（发布端 → SFU 的入口）。
    ///
    /// #15：录制文件创建失败（磁盘满/权限错误）时 warn 并跳过该房间，
    /// 绝不 panic——否则会杀死分片线程、断开该分片全部客户端。
    /// #234：ADREC2 每包携带 kind/codec/RTP 时间戳/keyframe，供 rec2mp4 精确转封装。
    pub fn record(
        &self,
        room: &str,
        codec: str0m::format::Codec,
        rtp_ts: Option<u64>,
        keyframe: bool,
        payload: &[u8],
    ) {
        let ts = now_micros();
        let mut recs = self.recordings.lock().unwrap_or_else(|e| e.into_inner());

        // 按需模式：未显式 start 的房间不录制。
        if self.on_demand && !recs.contains_key(room) {
            return;
        }

        // 首次见到该房间：尝试创建录制文件。
        if !recs.contains_key(room) {
            match Recording::open(&self.root, room, ts) {
                Ok(rec) => {
                    self.audit_event(
                        "room_start",
                        serde_json::json!({
                            "room": room,
                            "path": rec.path.display().to_string(),
                            "source": "auto",
                        }),
                    );
                    recs.insert(room.to_string(), rec);
                }
                Err(e) => {
                    warn!("recorder: room={room} 录制文件创建失败（{e}），本次会话跳过该房间录制");
                    recs.insert(room.to_string(), Recording::failed(room, ts));
                    return;
                }
            }
        }

        let entry = recs.get_mut(room).expect("recording just ensured");
        if entry.failed {
            return;
        }
        let len = payload.len() as u32;
        let mut header = [0u8; PACKET_HEADER_LEN];
        header[0] = if codec.is_audio() {
            KIND_AUDIO
        } else {
            KIND_VIDEO
        };
        header[1] = codec_id(codec);
        header[2] = if keyframe { 1 } else { 0 };
        header[3] = 0;
        header[4..12].copy_from_slice(&ts.to_le_bytes());
        header[12..20].copy_from_slice(&rtp_ts.unwrap_or(0).to_le_bytes());
        header[20..24].copy_from_slice(&len.to_le_bytes());
        if entry.writer.write_all(&header).is_ok() && entry.writer.write_all(payload).is_ok() {
            entry.packets += 1;
            entry.bytes += len as u64;
            entry.segment_bytes += len as u64;
            // 周期性落盘，避免崩溃丢太多。
            if entry.packets & 127 == 0 {
                let _ = entry.writer.flush();
            }
            self.maybe_rotate(entry, ts);
        }
    }

    /// 测试便捷入口：以 H264/keyframe 默认元数据落盘（等价 ADREC2 video/H264）。
    #[cfg(test)]
    pub fn record_payload(&self, room: &str, payload: &[u8]) {
        self.record(room, str0m::format::Codec::H264, None, true, payload);
    }

    /// 显式开始录制一个房间（幂等）。创建文件失败返回 Err（调用方按 503 处理）。
    /// 按需模式与自动模式都可用（自动模式下也可提前 start 记录空房间）。
    pub fn start(&self, room: &str) -> Result<(), String> {
        let ts = now_micros();
        let mut recs = self.recordings.lock().unwrap_or_else(|e| e.into_inner());
        if recs.contains_key(room) {
            return Ok(()); // 幂等
        }
        match Recording::open(&self.root, room, ts) {
            Ok(rec) => {
                self.audit_event(
                    "room_start",
                    serde_json::json!({
                        "room": room,
                        "path": rec.path.display().to_string(),
                        "source": "api",
                    }),
                );
                recs.insert(room.to_string(), rec);
                Ok(())
            }
            Err(e) => {
                warn!("recorder: room={room} 录制文件创建失败（{e}），标记失败哨兵");
                recs.insert(room.to_string(), Recording::failed(room, ts));
                Err(format!("open recording for {room}: {e}"))
            }
        }
    }

    /// 显式停止录制一个房间：立即 finalize（写 meta + 审计）。幂等（未在录返回 false）。
    pub fn stop(&self, room: &str) -> bool {
        let now = now_micros();
        let mut recs = self.recordings.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut rec) = recs.remove(room) else {
            return false;
        };
        if rec.failed {
            return true;
        }
        self.finalize_recording(&mut rec, now);
        self.audit_event(
            "room_end",
            serde_json::json!({
                "room": rec.room,
                "packets": rec.packets,
                "bytes": rec.bytes,
                "segments": rec.segments.len(),
                "duration_us": now.saturating_sub(rec.started_at),
            }),
        );
        true
    }

    /// 当前录制状态（供 GET /record/status）。
    pub fn status(&self) -> Vec<serde_json::Value> {
        let recs = self.recordings.lock().unwrap_or_else(|e| e.into_inner());
        recs.values()
            .filter(|r| !r.failed)
            .map(|r| {
                serde_json::json!({
                    "room": r.room,
                    "path": r.path.display().to_string(),
                    "started_at": r.started_at,
                    "packets": r.packets,
                    "bytes": r.bytes,
                })
            })
            .collect()
    }

    /// 当前在录房间数（供 /metrics/prometheus gauge，不含失败哨兵，#240）。
    pub fn active_count(&self) -> usize {
        let recs = self.recordings.lock().unwrap_or_else(|e| e.into_inner());
        recs.values().filter(|r| !r.failed).count()
    }

    /// 结束所有录制并写元数据（进程退出/手动调用时）。
    pub fn finalize_all(&self) {
        let mut recs = self.recordings.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_micros();
        for (_, mut rec) in recs.drain() {
            if rec.failed {
                continue;
            }
            self.finalize_recording(&mut rec, now);
            self.audit_event(
                "room_end",
                serde_json::json!({
                    "room": rec.room,
                    "packets": rec.packets,
                    "bytes": rec.bytes,
                    "segments": rec.segments.len(),
                    "duration_us": now.saturating_sub(rec.started_at),
                }),
            );
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.finalize_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aerodesk-rec-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn auto_mode_start_is_idempotent_and_stop_finalizes() {
        let dir = tmpdir("auto");
        let rec = Recorder::new(&dir, false, 0, 0).unwrap();
        rec.start("room-a").unwrap(); // 自动模式也可显式 start（空房间）
        rec.start("room-a").unwrap(); // 幂等
        rec.record_payload("room-a", b"x");
        assert!(rec.stop("room-a"));
        assert_eq!(rec.status().len(), 0);
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("room-a.meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["packets"], 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn on_demand_records_only_started_rooms() {
        let dir = tmpdir("ondemand");
        let rec = Recorder::new(&dir, true, 0, 0).unwrap();
        // 未 start 的房间不录制
        rec.record_payload("room-x", b"ignored");
        assert_eq!(rec.status().len(), 0);
        // start 后开始录制
        rec.start("room-x").unwrap();
        rec.record_payload("room-x", b"hello");
        rec.record_payload("room-x", b"world");
        let st = rec.status();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0]["room"], "room-x");
        assert_eq!(st[0]["packets"], 2);
        // stop 后立即出 meta，且不再自动录制
        assert!(rec.stop("room-x"));
        assert!(
            !rec.stop("room-x"),
            "stop must be idempotent (false when absent)"
        );
        rec.record_payload("room-x", b"after-stop");
        assert_eq!(
            rec.status().len(),
            0,
            "stopped room must not auto-record in on-demand"
        );
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("room-x.meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["packets"], 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_and_finalizes() {
        let dir = tmpdir("t1");
        let rec = Recorder::new(&dir, false, 0, 0).unwrap();
        rec.record_payload("room-a", b"hello world");
        rec.record_payload("room-a", b"hello world");
        rec.finalize_all();

        // 媒体文件：magic + 2 条记录。
        let raw = fs::read(dir.join("room-a.adrec")).unwrap();
        assert_eq!(&raw[..MAGIC.len()], MAGIC);
        assert_eq!(
            raw.len(),
            MAGIC.len() + 2 * (PACKET_HEADER_LEN + 11),
            "ADREC2 每包 24B 头 + 载荷"
        );
        // 首包元数据：video/h264/keyframe 标记 + rtp_ts 落盘（供 rec2mp4）。
        let h = &raw[MAGIC.len()..MAGIC.len() + PACKET_HEADER_LEN];
        assert_eq!(h[0], KIND_VIDEO);
        assert_eq!(h[1], CODEC_H264);
        assert_eq!(h[2], 1, "keyframe 标记");

        // 元数据。
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("room-a.meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["room"], "room-a");
        assert_eq!(meta["packets"], 2);
        assert_eq!(meta["bytes"], 22);

        // 审计日志有 start/end。
        let audit = fs::read_to_string(dir.join("audit.log")).unwrap();
        assert!(audit.contains("room_start"));
        assert!(audit.contains("room_end"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_tracks_api_calls_and_source() {
        let dir = tmpdir("audit");
        let rec = Recorder::new(&dir, false, 0, 0).unwrap();

        // API start → room_start source=api
        rec.start("room-api").unwrap();
        // 自动首包 → room_start source=auto
        rec.record_payload("room-auto", b"x");
        // 按需录制 API 调用留痕（start/stop/403 语义）
        rec.audit_event(
            "record_api",
            serde_json::json!({ "action": "record/start", "room": "room-api", "status": 200, "ok": true }),
        );
        rec.audit_event(
            "record_api",
            serde_json::json!({ "action": "record/start", "room": "", "status": 403, "ok": false, "detail": "forbidden" }),
        );
        rec.stop("room-api");

        let audit = fs::read_to_string(dir.join("audit.log")).unwrap();
        assert!(audit.contains("\"source\":\"api\""), "{audit}");
        assert!(audit.contains("\"source\":\"auto\""), "{audit}");
        assert!(audit.contains("\"event\":\"record_api\""), "{audit}");
        assert!(audit.contains("\"action\":\"record/start\""), "{audit}");
        assert!(audit.contains("\"status\":403"), "{audit}");
        assert!(audit.contains("\"event\":\"room_end\""), "{audit}");
        assert!(audit.contains("\"duration_us\""), "{audit}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn create_failure_skips_room_without_panic() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmpdir("t3");
        let rec = Recorder::new(&dir, false, 0, 0).unwrap();
        rec.record_payload("ok-room", b"first");

        // 目录改为只读：新房间创建录制文件必失败。
        let orig = fs::metadata(&dir).unwrap().permissions().mode();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

        // #15：失败不得 panic，且失败房间不再重试。
        rec.record_payload("blocked-room", b"x");
        rec.record_payload("blocked-room", b"y");

        fs::set_permissions(&dir, fs::Permissions::from_mode(orig)).unwrap();
        rec.finalize_all();

        assert!(
            !dir.join("blocked-room.adrec").exists(),
            "失败房间不应有录制文件"
        );
        assert!(dir.join("ok-room.adrec").exists(), "正常房间不受影响");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_sink_never_panics() {
        // #15：失败哨兵不依赖 /dev/null，写入全部丢弃且不 panic。
        let mut rec = Recording::failed("room-x", 1);
        assert!(rec.failed);
        assert!(rec.writer.write_all(b"anything").is_ok());
        assert!(rec.writer.flush().is_ok());
        rec.packets += 1;
        assert_eq!(rec.packets, 1);

        #[test]
        fn unsafe_room_name_sanitized() {
            let dir = tmpdir("t2");
            let rec = Recorder::new(&dir, false, 0, 0).unwrap();
            rec.record_payload("../bad/room", b"x");
            rec.finalize_all();
            // 路径分隔符被替换为 _；点号保留（仍是安全文件名）。
            assert!(dir.join(".._bad_room.adrec").exists());
            assert!(!dir.join("bad").join("room.adrec").exists());
            let _ = fs::remove_dir_all(&dir);
        }
    }
    #[test]
    fn rotates_by_bytes() {
        let dir = tmpdir("rotate-bytes");
        let rec = Recorder::new(&dir, false, 40, 0).unwrap();
        rec.record_payload("room-r", b"12345678901234567890");
        rec.record_payload("room-r", b"12345678901234567890");
        rec.record_payload("room-r", b"12345678901234567890");
        rec.stop("room-r");

        let files: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|f| f.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.ends_with(".adrec") || n.contains(".adrec."))
            .collect();
        assert!(files.iter().any(|n| n == "room-r.adrec"), "{files:?}");
        assert!(files.iter().any(|n| n == "room-r.adrec.1"), "{files:?}");

        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("room-r.meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["packets"], 3);
        assert_eq!(meta["segments"].as_array().map(|a| a.len()), Some(2));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotates_by_time() {
        let dir = tmpdir("rotate-time");
        let rec = Recorder::new(&dir, false, 0, 1_000_000).unwrap(); // 1s 段
        rec.record_payload("room-t", b"x");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        rec.record_payload("room-t", b"y");
        rec.stop("room-t");
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("room-t.meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["segments"].as_array().map(|a| a.len()), Some(2));
        let _ = fs::remove_dir_all(&dir);
    }
}
