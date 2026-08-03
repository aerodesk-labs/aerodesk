//! SFU 侧录制/审计模块（可选）。
//!
//! 通过环境变量 `RECORD_DIR` 开启：将每个房间收到的媒体载荷落盘，
//! 并输出 JSON 审计日志（会话起止/包数/字节数）。
//!
//! 文件格式（`ADREC1`）：
//! ```text
//! magic "ADREC1\n"
//! 每包: [u64 timestamp_us][u32 len][payload bytes]
//! ```
//! 媒体文件：`{RECORD_DIR}/{room}.adrec`；元数据：`{room}.meta.json`；
//! 审计日志：`{RECORD_DIR}/audit.log`（JSON Lines）。

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAGIC: &[u8] = b"ADREC1\n";

/// 每房间录制状态。
struct Recording {
    room: String,
    path: PathBuf,
    writer: BufWriter<File>,
    started_at: u64,
    packets: u64,
    bytes: u64,
}

/// 录制器（进程级单例，跨分片共享）。
pub struct Recorder {
    root: PathBuf,
    audit: Mutex<File>,
    recordings: Mutex<HashMap<String, Recording>>,
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
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<Recorder> {
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
        })
    }

    fn audit(&self, line: serde_json::Value) {
        let mut f = self.audit.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }

    /// 记录一条媒体载荷（发布端 → SFU 的入口）。
    pub fn record(&self, room: &str, payload: &[u8]) {
        let ts = now_micros();
        let mut recs = self.recordings.lock().unwrap_or_else(|e| e.into_inner());
        let entry = recs.entry(room.to_string()).or_insert_with(|| {
            let safe = safe_name(room);
            let path = self.root.join(format!("{safe}.adrec"));
            let mut writer = BufWriter::new(File::create(&path).expect("create recording"));
            let _ = writer.write_all(MAGIC);
            let r = Recording {
                room: room.to_string(),
                path,
                writer,
                started_at: ts,
                packets: 0,
                bytes: 0,
            };
            self.audit(serde_json::json!({
                "ts": ts,
                "event": "room_start",
                "room": room,
                "path": r.path.display().to_string(),
            }));
            r
        });

        let len = payload.len() as u32;
        let mut header = [0u8; 12];
        header[0..8].copy_from_slice(&ts.to_le_bytes());
        header[8..12].copy_from_slice(&len.to_le_bytes());
        if entry.writer.write_all(&header).is_ok() && entry.writer.write_all(payload).is_ok() {
            entry.packets += 1;
            entry.bytes += len as u64;
            // 周期性落盘，避免崩溃丢太多。
            if entry.packets % 128 == 0 {
                let _ = entry.writer.flush();
            }
        }
    }

    /// 结束所有录制并写元数据（进程退出/手动调用时）。
    pub fn finalize_all(&self) {
        let mut recs = self.recordings.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_micros();
        for (_, mut rec) in recs.drain() {
            let _ = rec.writer.flush();
            let meta = serde_json::json!({
                "room": rec.room,
                "path": rec.path.display().to_string(),
                "started_at": rec.started_at,
                "ended_at": now,
                "packets": rec.packets,
                "bytes": rec.bytes,
            });
            let meta_path = rec.path.with_extension("meta.json");
            let _ = fs::write(
                &meta_path,
                serde_json::to_string_pretty(&meta).unwrap_or_default(),
            );
            self.audit(serde_json::json!({
                "ts": now,
                "event": "room_end",
                "room": rec.room,
                "packets": rec.packets,
                "bytes": rec.bytes,
            }));
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
    fn records_and_finalizes() {
        let dir = tmpdir("t1");
        let rec = Recorder::new(&dir).unwrap();
        rec.record("room-a", b"hello world");
        rec.record("room-a", b"hello world");
        rec.finalize_all();

        // 媒体文件：magic + 2 条记录。
        let raw = fs::read(dir.join("room-a.adrec")).unwrap();
        assert_eq!(&raw[..7], MAGIC);
        assert_eq!(raw.len(), 7 + 2 * (8 + 4 + 11));

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
    fn unsafe_room_name_sanitized() {
        let dir = tmpdir("t2");
        let rec = Recorder::new(&dir).unwrap();
        rec.record("../bad/room", b"x");
        rec.finalize_all();
        // 路径分隔符被替换为 _；点号保留（仍是安全文件名）。
        assert!(dir.join(".._bad_room.adrec").exists());
        assert!(!dir.join("bad").join("room.adrec").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
