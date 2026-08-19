//! 长度前缀帧：`u32 LE 长度` + JSON 载荷（UTF-8）。
//!
//! 帧边界由长度前缀保证（管道为字节流，无消息边界）。读端对长度做上限
//! 校验，防畸形对端撑爆内存。

use std::io::{self, Read, Write};

/// 单帧上限 64MB（`file_cmd` 的剪贴板图片 base64 载荷可能达数 MB；
/// 常规控制消息应 <64KB，超出的 kind 在协议文档另行声明）。
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// 写一帧（长度前缀 + 载荷）。`payload` 超上限返回 InvalidInput。
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame too large: {} > {MAX_FRAME}", payload.len()),
        ));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// 读一帧；干净 EOF（对端关闭且无一字节残留）返回 `Ok(None)`，
/// 半帧 EOF 返回 UnexpectedEof（协议错误）。
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds cap {MAX_FRAME}"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip_and_boundaries() {
        let mut wire = Vec::new();
        write_frame(&mut wire, b"hello").unwrap();
        write_frame(&mut wire, b"").unwrap();
        write_frame(&mut wire, &[7u8; 1000]).unwrap();
        let mut cur = Cursor::new(&wire);
        assert_eq!(
            read_frame(&mut cur).unwrap().as_deref(),
            Some(&b"hello"[..])
        );
        assert_eq!(read_frame(&mut cur).unwrap().as_deref(), Some(&b""[..]));
        assert_eq!(read_frame(&mut cur).unwrap().unwrap().len(), 1000);
        // 干净 EOF → None。
        assert_eq!(read_frame(&mut cur).unwrap(), None);
    }

    #[test]
    fn frame_rejects_oversize_and_truncated() {
        let mut wire = Vec::new();
        // 超长声明被拒。
        wire.extend_from_slice(&(MAX_FRAME as u32 + 1).to_le_bytes());
        let mut cur = Cursor::new(&wire);
        let err = read_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // 半帧 EOF → UnexpectedEof。
        let mut wire = Vec::new();
        wire.extend_from_slice(&10u32.to_le_bytes());
        wire.extend_from_slice(b"abc");
        let mut cur = Cursor::new(&wire);
        let err = read_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        // 写入超上限被拒。
        let mut sink = Vec::new();
        let err = write_frame(&mut sink, &vec![0u8; MAX_FRAME + 1]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
