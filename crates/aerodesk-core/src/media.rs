//! 媒体管线：VP8 测试媒体源（pcap → 帧）与帧重组。
//!
//! 演示/测试用：把真实 VP8 抓包重组为可发送的帧（发布端媒体源）。

/// 一帧 VP8 视频。
#[derive(Debug, Clone)]
pub struct Vp8Frame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// RTP 时间戳（90kHz）。
    pub rtp_timestamp: u32,
}

/// 从 pcap 字节解析 VP8 帧序列。
///
/// 输入为 str0m 测试用的 pcap 格式（Ethernet/IP/UDP 42 字节头 + RTP）。
pub fn parse_vp8_pcap(pcap: &[u8]) -> Vec<Vp8Frame> {
    let reader = std::io::Cursor::new(pcap);
    let mut pcap_reader = pcap_file::pcap::PcapReader::new(reader).expect("pcap reader");
    let mut frames: Vec<Vp8Frame> = Vec::new();
    let mut current: Option<(Vec<u8>, u32, bool)> = None; // (payload, ts, started)

    while let Some(pkt) = pcap_reader.next_packet() {
        let pkt = pkt.expect("packet");
        if pkt.data.len() <= 42 {
            continue;
        }
        let rtp = &pkt.data[42..];
        if rtp.len() < 12 || rtp[0] >> 6 != 2 {
            continue;
        }
        let ts = u32::from_be_bytes([rtp[4], rtp[5], rtp[6], rtp[7]]);
        let header_len = 12 + ((rtp[0] & 0x0F) as usize) * 4 + rtp[12] as usize;
        if rtp.len() <= header_len {
            continue;
        }
        let payload = &rtp[header_len..];

        // VP8 payload descriptor（RFC 7741）
        let (desc_len, start_of_frame) = parse_vp8_descriptor(payload);
        let body = &payload[desc_len..];
        if body.is_empty() {
            continue;
        }

        if start_of_frame {
            // 上一帧收尾
            if let Some((data, t, _)) = current.take()
                && !data.is_empty()
            {
                frames.push(Vp8Frame {
                    keyframe: is_vp8_keyframe(&data),
                    rtp_timestamp: t,
                    data,
                });
            }
            current = Some((body.to_vec(), ts, true));
        } else if let Some((data, _, _)) = &mut current {
            data.extend_from_slice(body);
        }
    }
    if let Some((data, t, _)) = current
        && !data.is_empty()
    {
        frames.push(Vp8Frame {
            keyframe: is_vp8_keyframe(&data),
            rtp_timestamp: t,
            data,
        });
    }
    frames
}

/// 解析 VP8 payload descriptor，返回 (描述头长度, 是否帧起始 S)。
fn parse_vp8_descriptor(payload: &[u8]) -> (usize, bool) {
    let b0 = payload[0];
    let start = b0 & 0x10 != 0;
    let x = b0 & 0x80 != 0;
    if !x {
        return (1, start);
    }
    // 扩展控制字节
    if payload.len() < 2 {
        return (1, start);
    }
    let b1 = payload[1];
    let mut len = 2;
    if b1 & 0x80 != 0 {
        // I：扩展 picture id（1-2 字节）
        if payload.len() > len {
            if payload[len] & 0x80 != 0 {
                len += 2;
            } else {
                len += 1;
            }
        }
    }
    if b1 & 0x40 != 0 && payload.len() > len {
        len += 1; // L
    }
    if b1 & 0x20 != 0 && payload.len() > len {
        len += 1; // T
    }
    if b1 & 0x10 != 0 && payload.len() > len {
        len += 1; // K
    }
    (len, start)
}

/// VP8 关键帧判断：帧头 P 位（bit0）= 0。
fn is_vp8_keyframe(data: &[u8]) -> bool {
    data.first().is_some_and(|b| b & 0x01 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VP8_PCAP: &[u8] = include_bytes!("../tests/data/vp8.pcap");

    #[test]
    fn parses_real_vp8_stream() {
        let frames = parse_vp8_pcap(VP8_PCAP);
        assert!(
            frames.len() >= 2,
            "expected multiple frames, got {}",
            frames.len()
        );
        assert!(
            frames.iter().any(|f| f.keyframe),
            "stream should contain a keyframe"
        );
        let total: usize = frames.iter().map(|f| f.data.len()).sum();
        assert!(total > 10_000, "payload should be substantial: {total}");
        // 时间戳单调（同帧聚合）
        for w in frames.windows(2) {
            assert!(
                w[0].rtp_timestamp <= w[1].rtp_timestamp,
                "timestamps should not go backwards"
            );
        }
    }

    #[test]
    fn descriptor_parsing() {
        // S=1, PID=0（无扩展）
        assert_eq!(parse_vp8_descriptor(&[0x10]), (1, true));
        // S=0, PID=1（无扩展）
        assert_eq!(parse_vp8_descriptor(&[0x01]), (1, false));
        // X=1 + S=1 + I 扩展（1 字节 picture id）
        assert_eq!(parse_vp8_descriptor(&[0x90, 0x80, 0x05, 0xAA]), (3, true));
        // X=1 + S=0 + I 扩展
        assert_eq!(parse_vp8_descriptor(&[0x80, 0x80, 0x05, 0xAA]), (3, false));
    }

    #[test]
    fn keyframe_detection() {
        // VP8 payload header: P bit(bit0)=0 -> keyframe
        assert!(is_vp8_keyframe(&[0x00, 0x01, 0x2A]));
        assert!(!is_vp8_keyframe(&[0x01, 0x00, 0x00]));
    }
}
