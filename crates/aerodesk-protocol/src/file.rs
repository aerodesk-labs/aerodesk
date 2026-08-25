//! 文件传输协议（#72）：经 WebRTC data channel（label "file"）双向传输。
//!
//! 帧格式：
//! - 控制消息（Meta/Done/Cancel）：JSON 文本，UTF-8
//! - 数据分片（Chunk）：二进制，首字节 0x01 + id 长度(u32 LE) + id(UTF-8)
//!   + 分片序号(u64 LE) + 载荷
//!
//! 大文件分片（默认 64KB）避免单条 data channel 消息过大；接收端按 id
//! 聚合分片，Done 后校验大小/hash 落盘。
//!
//! 断点续传（#503 传输中心）：当前协议不支持——分片序号从 0 开始、接收端内存
//! 聚合后整写落盘，断线/取消即重传全量。续传需扩展 FileMeta（起始分片）与
//! 接收端分片增量落盘（避免内存聚合），列为后续版本扩展点。

use serde::{Deserialize, Serialize};

/// 默认分片大小。
///
/// 注意：SCTP data channel 远端默认 max message size 为 64KB（str0m
/// `DEFAULT_REMOTE_MAX_MESSAGE_SIZE`），且 str0m 跨流缓冲上限 128KB；
/// 实测经 SFU 转发大分片会卡在缓冲排空，取 8KB 在吞吐与可靠性间平衡。
pub const CHUNK_SIZE: usize = 8192;

/// 分片二进制帧类型标记。
pub const CHUNK_MAGIC: u8 = 0x01;

/// 传输内容类型（#271 图片剪贴板：复用 file 分片通道，接收端不落盘）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// 普通文件（接收端落盘）。
    File,
    /// 剪贴板图片（PNG 编码；接收端写入系统剪贴板，不落盘）。
    ClipboardImage,
}

impl Default for FileKind {
    fn default() -> Self {
        Self::File
    }
}

/// 文件元信息（发送端先发）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    /// 传输会话 id（发送端生成，如 "tx1"）。
    pub id: String,
    /// 文件名（不含路径）。
    pub name: String,
    /// 文件总字节数。
    pub size: u64,
    /// 分片总数。
    pub chunks: u64,
    /// 内容类型（#271；缺省为普通文件，兼容旧消息）。
    #[serde(default)]
    pub kind: FileKind,

    /// 可选 SHA-256（十六进制小写），接收端校验。
    pub hash: Option<String>,
}

/// 传输完成/失败。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDone {
    pub id: String,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

/// 取消传输。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCancel {
    pub id: String,
}

/// 接收端补包请求（#72：SFU 转发在出站缓冲满时会丢包，需应用层重传）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileNack {
    pub id: String,
    /// 缺失分片序号（最多一批 512 个，避免单条消息过大）。
    pub missing: Vec<u64>,
}

/// 控制消息（JSON 文本）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileControl {
    Meta(FileMeta),
    Done(FileDone),
    Cancel(FileCancel),
    Nack(FileNack),
    /// 剪贴板文本同步（复用 file 通道，双向）。
    Clipboard {
        text: String,
    },
    /// #122：控制端请求被控端发送指定路径文件（大文件下载，走 file 通道）。
    Request {
        path: String,
    },
}

/// 编码一个分片为二进制帧。
pub fn encode_chunk(id: &str, index: u64, data: &[u8]) -> Vec<u8> {
    let id_bytes = id.as_bytes();
    let mut out = Vec::with_capacity(1 + 4 + id_bytes.len() + 8 + data.len());
    out.push(CHUNK_MAGIC);
    out.extend_from_slice(&(id_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(id_bytes);
    out.extend_from_slice(&index.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// 解析分片二进制帧；不是分片帧时返回 None。
pub fn decode_chunk(buf: &[u8]) -> Option<(String, u64, &[u8])> {
    if buf.first() != Some(&CHUNK_MAGIC) {
        return None;
    }
    if buf.len() < 1 + 4 + 8 {
        return None;
    }
    let id_len = u32::from_le_bytes(buf[1..5].try_into().ok()?) as usize;
    let body = buf.get(5..)?;
    if body.len() < id_len + 8 {
        return None;
    }
    let id = std::str::from_utf8(&body[..id_len]).ok()?.to_string();
    let index = u64::from_le_bytes(body[id_len..id_len + 8].try_into().ok()?);
    let payload = &body[id_len + 8..];
    Some((id, index, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_roundtrip() {
        let data = vec![7u8; 12345];
        let frame = encode_chunk("tx1", 42, &data);
        let (id, index, payload) = decode_chunk(&frame).expect("decode");
        assert_eq!(id, "tx1");
        assert_eq!(index, 42);
        assert_eq!(payload, data.as_slice());
    }

    #[test]
    fn non_chunk_returns_none() {
        assert!(decode_chunk(b"{\"type\":\"meta\"}").is_none());
        assert!(decode_chunk(&[]).is_none());
    }

    #[test]
    fn control_json_roundtrip() {
        let meta = FileControl::Meta(FileMeta {
            id: "tx1".into(),
            name: "a.bin".into(),
            size: 1024,
            chunks: 1,
            hash: Some("abc".into()),
            kind: FileKind::File,
        });
        let json = serde_json::to_string(&meta).unwrap();
        let back: FileControl = serde_json::from_str(&json).unwrap();
        match back {
            FileControl::Meta(m) => {
                assert_eq!(m.name, "a.bin");
                assert_eq!(m.size, 1024);
                assert_eq!(m.kind, FileKind::File);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn meta_without_kind_defaults_to_file() {
        // #271 兼容：旧版本消息无 kind 字段，应反序列化为 File。
        let json = r#"{"type":"meta","id":"tx1","name":"a.bin","size":1,"chunks":1,"hash":null}"#;
        let ctrl: FileControl = serde_json::from_str(json).unwrap();
        match ctrl {
            FileControl::Meta(m) => assert_eq!(m.kind, FileKind::File),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn request_json_roundtrip() {
        // #122：下载请求消息序列化/反序列化。
        let ctrl = FileControl::Request {
            path: "/var/log/system.log".into(),
        };
        let json = serde_json::to_string(&ctrl).unwrap();
        assert!(json.contains("\"type\":\"request\""));
        let back: FileControl = serde_json::from_str(&json).unwrap();
        match back {
            FileControl::Request { path } => assert_eq!(path, "/var/log/system.log"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn clipboard_json_roundtrip() {
        // #72 剪贴板复用 file 通道：FileControl::Clipboard 序列化/反序列化。
        let ctrl = FileControl::Clipboard {
            text: "hello 你好".into(),
        };
        let json = serde_json::to_string(&ctrl).unwrap();
        assert!(json.contains("\"type\":\"clipboard\""));
        let back: FileControl = serde_json::from_str(&json).unwrap();
        match back {
            FileControl::Clipboard { text } => assert_eq!(text, "hello 你好"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
