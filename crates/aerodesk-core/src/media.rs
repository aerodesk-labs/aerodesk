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
        let Some(header_len) = rtp_payload_offset(rtp) else {
            continue;
        };
        let ts = u32::from_be_bytes([rtp[4], rtp[5], rtp[6], rtp[7]]);
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

/// 计算 RTP 负载起始偏移：跳过 12 字节固定头、CSRC 列表（CC）与 RFC 8285
/// 头扩展（X 标志：4 字节扩展头 + 以 32bit 字为单位的扩展长度）。
///
/// 返回 `None` 表示不是合法/完整的 RTP 包，或跳过头部后已无负载，调用方应跳过该包。
///
/// 旧实现误把 `rtp[12]` 当扩展长度用——那正是 0xBEDE 扩展 profile 的高字节
/// （0xBE = 190），于是负载从真实 payload 中间切开，产出无法解码的 VP8 帧；
/// 见 `parses_real_vp8_stream` 对 vp8.pcap 的回归断言。
fn rtp_payload_offset(rtp: &[u8]) -> Option<usize> {
    if rtp.len() < 12 || rtp[0] >> 6 != 2 {
        return None;
    }
    let mut off = 12 + ((rtp[0] & 0x0F) as usize) * 4;
    if rtp[0] & 0x10 != 0 {
        // 扩展头：profile(2) + length-in-words(2)，长度不含这 4 字节。
        if rtp.len() < off + 4 {
            return None;
        }
        let ext_words = u16::from_be_bytes([rtp[off + 2], rtp[off + 3]]) as usize;
        off += 4 + ext_words * 4;
    }
    (rtp.len() > off).then_some(off)
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
        // 该 pcap 每个 RTP 包都带 0xBEDE 头扩展（2–4 词）。必须按扩展长度跳过，
        // 否则负载从帧中间切开：旧实现得 48 帧、首帧非关键帧、ffmpeg 解 0 帧。
        assert_eq!(
            frames.len(),
            101,
            "RTP 头扩展必须被正确跳过（旧实现误得 48 帧）"
        );
        assert!(
            frames.iter().any(|f| f.keyframe),
            "stream should contain a keyframe"
        );
        assert!(frames[0].keyframe, "首帧必须解码为关键帧");
        // VP8 关键帧起始码：帧头 3 字节 + 0x9d 0x01 0x2a。
        assert_eq!(
            &frames[0].data[3..6],
            &[0x9d, 0x01, 0x2a],
            "首帧必须是带合法起始码的 VP8 关键帧"
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
    fn skips_rtp_header_extension() {
        // 12 字节固定头 + 0xBEDE 扩展（长度 4 词 = 16 字节）+ VP8 描述符 + 帧体。
        let mut rtp = vec![0x90, 0x60, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0];
        rtp.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x04]);
        rtp.extend_from_slice(&[0u8; 16]);
        rtp.push(0x10); // VP8 payload descriptor：S=1
        rtp.push(0xAA); // 帧体首字节
        assert_eq!(rtp_payload_offset(&rtp), Some(12 + 4 + 16));
        // 无扩展（X=0）：偏移恰为 12。
        let plain = [0x80u8, 0x60, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0xAA];
        assert_eq!(rtp_payload_offset(&plain), Some(12));
        // 截断的扩展头（声明 X 但不足 4 字节）/ 扩展后负载为空：不是合法包。
        assert_eq!(rtp_payload_offset(&rtp[..13]), None);
        assert_eq!(rtp_payload_offset(&rtp[..32]), None);
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
