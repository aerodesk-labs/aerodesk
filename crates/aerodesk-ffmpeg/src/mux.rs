//! ADREC2 → MP4（#234）：SFU 录制文件 → 可播放 MP4。
//!
//! 读取 `ADREC2`（magic + 每包 `[kind][codec][flags][rsv][wall_us][rtp_ts][len][payload]`），
//! 取 H.264 视频流（Annex-B 载荷），从流中提取 SPS/PPS 构造 AVCC extradata，
//! 用 ffmpeg-next 以 90kHz 时间基 mux 成 MP4（不重编码）。

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::packet::Packet;
use ffmpeg_next::{Rational, codec, format};

pub const MAGIC: &[u8] = b"ADREC2\n";
pub const PACKET_HEADER_LEN: usize = 24;
pub const KIND_VIDEO: u8 = 0;
pub const CODEC_H264: u8 = 1;

/// 一个 ADREC2 包（仅保留转封装所需字段）。
#[derive(Debug)]
pub struct AdrecPacket {
    pub kind: u8,
    pub codec: u8,
    pub rtp_ts: u64,
    pub keyframe: bool,
    pub payload: Vec<u8>,
}

/// 读取 ADREC2 文件全部包。文件结尾（EOF 落在包边界）视为正常结束。
pub fn read_adrec(path: &Path) -> Result<Vec<AdrecPacket>, String> {
    let f = File::open(path).map_err(|e| format!("open {path:?}: {e}"))?;
    let mut r = BufReader::new(f);
    let mut magic = [0u8; MAGIC.len()];
    r.read_exact(&mut magic)
        .map_err(|e| format!("read magic: {e}"))?;
    if &magic != MAGIC {
        return Err(format!("not ADREC2 (magic mismatch)"));
    }
    let mut out = Vec::new();
    loop {
        let mut header = [0u8; PACKET_HEADER_LEN];
        match r.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("read header: {e}")),
        }
        let len = u32::from_le_bytes(header[20..24].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload)
            .map_err(|e| format!("read payload: {e}"))?;
        out.push(AdrecPacket {
            kind: header[0],
            codec: header[1],
            rtp_ts: u64::from_le_bytes(header[12..20].try_into().unwrap()),
            keyframe: header[2] & 1 == 1,
            payload,
        });
    }
    Ok(out)
}

/// 把 Annex-B 载荷按 start code（3/4 字节）边界切成 NAL（不含 start code）。
fn annexb_nalus(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 3 < data.len() {
        let mut sc = None;
        for i in pos..data.len().saturating_sub(2) {
            let is3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
            let is4 = i + 3 < data.len()
                && data[i] == 0
                && data[i + 1] == 0
                && data[i + 2] == 0
                && data[i + 3] == 1;
            if is3 || is4 {
                sc = Some(i);
                break;
            }
        }
        let Some(sc) = sc else { break };
        let sc_len = if sc + 3 < data.len()
            && data[sc] == 0
            && data[sc + 1] == 0
            && data[sc + 2] == 0
            && data[sc + 3] == 1
        {
            4
        } else {
            3
        };
        let nal_start = sc + sc_len;
        if nal_start >= data.len() {
            break;
        }
        let mut nal_end = data.len();
        for i in (nal_start + 1)..data.len().saturating_sub(2) {
            let is3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
            let is4 = i + 3 < data.len()
                && data[i] == 0
                && data[i + 1] == 0
                && data[i + 2] == 0
                && data[i + 3] == 1;
            if is3 || is4 {
                nal_end = i;
                break;
            }
        }
        if nal_end > nal_start {
            out.push(data[nal_start..nal_end].to_vec());
        }
        pos = nal_end;
    }
    out
}

/// Annex-B → AVCC（lengthSizeMinusOne=3）：4 字节大端长度 + NAL。
fn annexb_to_avcc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for nal in annexb_nalus(data) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(&nal);
    }
    out
}

/// 从 Annex-B 载荷提取首个 SPS（type 7）/PPS（type 8）NAL（不含 start code）。
fn find_sps_pps(pkts: &[&AdrecPacket]) -> (Vec<u8>, Vec<u8>) {
    let mut sps = None;
    let mut pps = None;
    'outer: for p in pkts {
        for nal in annexb_nalus(&p.payload) {
            if nal.is_empty() {
                continue;
            }
            let nal_type = nal[0] & 0x1f;
            if nal_type == 7 && sps.is_none() {
                sps = Some(nal);
            } else if nal_type == 8 && pps.is_none() {
                pps = Some(nal);
            }
            if sps.is_some() && pps.is_some() {
                break 'outer;
            }
        }
    }
    (sps.unwrap_or_default(), pps.unwrap_or_default())
}

/// 构造 H.264 AVCC extradata（ISO/IEC 14496-15）。
fn build_avcc(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>, String> {
    if sps.len() < 4 {
        return Err(format!("SPS 太短（{}B），无法构造 extradata", sps.len()));
    }
    let mut out = Vec::with_capacity(8 + sps.len() + pps.len());
    out.push(1); // version
    out.push(sps[1]); // profile
    out.push(sps[2]); // compat
    out.push(sps[3]); // level
    out.push(0xff); // lengthSizeMinusOne=3, reserved
    out.push(0xe1); // numOfSPS=1
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1); // numOfPPS
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    Ok(out)
}

/// ADREC2（H.264 视频流）→ MP4。返回写入的视频包数。
pub fn adrec_to_mp4(input: &Path, output: &Path) -> Result<u64, String> {
    crate::encode::init();
    let pkts = read_adrec(input)?;
    let video: Vec<&AdrecPacket> = pkts
        .iter()
        .filter(|p| p.kind == KIND_VIDEO && p.codec == CODEC_H264)
        .collect();
    if video.is_empty() {
        return Err(format!(
            "{input:?} 无 H.264 视频包（共 {} 包；H.265/VP9/AV1 容器化未支持，见 #234 M2）",
            pkts.len()
        ));
    }
    let (sps, pps) = find_sps_pps(&video);
    if sps.is_empty() || pps.is_empty() {
        return Err("流中未找到 SPS/PPS（无关键帧？）".into());
    }
    let extradata = build_avcc(&sps, &pps)?;

    // 宽高：解码首个关键帧（含 SPS/PPS/IDR）探测，MP4 muxer 需要 dimensions。
    let mut dec = crate::decode::FfmpegDecoder::new(aerodesk_core::media_pipeline::Codec::H264)
        .map_err(|e| format!("probe decoder: {e}"))?;
    let mut probed = false;
    for p in &video {
        if p.keyframe {
            let unit = aerodesk_core::media_pipeline::EncodedUnit {
                data: p.payload.clone(),
                keyframe: true,
                pts_ms: 0,
                rtp_timestamp: 0,
            };
            if let Ok(Some(_)) = dec.decode_unit(&unit) {
                probed = true;
                break;
            }
        }
    }
    if !probed {
        return Err("无法探测宽高（首关键帧解码失败）".into());
    }
    let (w, h) = (dec.width() as i32, dec.height() as i32);

    let mut params = codec::Parameters::new();
    unsafe {
        let p = params.as_mut_ptr();
        (*p).codec_type = ffmpeg::ffi::AVMediaType::AVMEDIA_TYPE_VIDEO;
        (*p).codec_id = ffmpeg::ffi::AVCodecID::AV_CODEC_ID_H264;
        (*p).codec_tag = 0;
        (*p).width = w;
        (*p).height = h;
        let len = extradata.len();
        let buf = ffmpeg::ffi::av_mallocz(len + 64) as *mut u8;
        if buf.is_null() {
            return Err("av_mallocz extradata failed".into());
        }
        std::ptr::copy_nonoverlapping(extradata.as_ptr(), buf, len);
        (*p).extradata = buf;
        (*p).extradata_size = len as i32;
    }
    let mut ctx = codec::context::Context::new();
    ctx.set_parameters(params)
        .map_err(|e| format!("set_parameters: {e}"))?;

    let mut out = format::output(output).map_err(|e| format!("open {output:?}: {e}"))?;
    let mut stream = out
        .add_stream_with(&ctx)
        .map_err(|e| format!("add_stream: {e}"))?;
    stream.set_time_base(Rational(1, 90_000));
    out.write_header()
        .map_err(|e| format!("write_header: {e}"))?;

    let mut written = 0u64;
    for p in &video {
        // MP4（extradata=AVCC）期望 length-prefixed 包：Annex-B → AVCC。
        let avcc = annexb_to_avcc(&p.payload);
        let mut packet = Packet::new(avcc.len());
        if let Some(d) = packet.data_mut() {
            d.copy_from_slice(&avcc);
        }
        packet.set_stream(0);
        let ts = p.rtp_ts as i64;
        packet.set_pts(Some(ts));
        packet.set_dts(Some(ts));
        packet.set_time_base(Rational(1, 90_000));
        if p.keyframe {
            packet.set_flags(ffmpeg::codec::packet::Flags::KEY);
        }
        packet
            .write_interleaved(&mut out)
            .map_err(|e| format!("write packet #{written}: {e}"))?;
        written += 1;
    }
    out.write_trailer()
        .map_err(|e| format!("write_trailer: {e}"))?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_extradata_layout() {
        let sps = [0x67, 0x42, 0xc0, 0x1e, 0xda, 0x02];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        let ex = build_avcc(&sps, &pps).unwrap();
        assert_eq!(ex[0], 1);
        assert_eq!(ex[1], 0x42);
        assert_eq!(ex[2], 0xc0);
        assert_eq!(ex[3], 0x1e);
        assert_eq!(ex[5], 0xe1);
        assert_eq!(u16::from_be_bytes([ex[6], ex[7]]) as usize, sps.len());
        assert_eq!(&ex[8..8 + sps.len()], &sps);
        assert_eq!(ex[8 + sps.len()], 1); // pps count
    }

    #[test]
    fn read_adrec_roundtrip() {
        let dir = std::env::temp_dir().join(format!("adrec-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.adrec");
        {
            let mut w = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
            use std::io::Write;
            w.write_all(MAGIC).unwrap();
            let mut h = [0u8; PACKET_HEADER_LEN];
            h[0] = 0; // video
            h[1] = 1; // h264
            h[2] = 1; // keyframe
            h[12..20].copy_from_slice(&123456u64.to_le_bytes());
            h[20..24].copy_from_slice(&5u32.to_le_bytes());
            w.write_all(&h).unwrap();
            w.write_all(b"hello").unwrap();
            w.flush().unwrap();
        }
        let pkts = read_adrec(&path).unwrap();
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].codec, CODEC_H264);
        assert!(pkts[0].keyframe);
        assert_eq!(pkts[0].rtp_ts, 123456);
        assert_eq!(&pkts[0].payload, b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
