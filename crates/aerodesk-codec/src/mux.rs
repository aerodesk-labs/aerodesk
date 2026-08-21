//! ADREC2 → 可播放容器（#234/#236）：SFU 录制文件转 MP4/WebM，不重编码。
//!
//! 读取 `ADREC2`（magic + 每包 `[kind][codec][flags][rsv][wall_us][rtp_ts][len][payload]`）：
//! - H.264/H.265（Annex-B）：提取 SPS/PPS（+VPS）→ AVCC/hvcC extradata → MP4
//! - VP9/AV1（完整帧/OBU 流）：→ WebM（VP9 无 extradata；AV1 附 av1c）
//! 载荷按 90kHz RTP 时间戳 mux；宽高由解码首关键帧探测。

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
pub const CODEC_H265: u8 = 2;
pub const CODEC_VP9: u8 = 4;
pub const CODEC_AV1: u8 = 5;

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
        match r.read_exact(&mut payload) {
            Ok(()) => {}
            // 录制可能在崩溃/强杀时尾部截断：忽略不完整尾包（#236）。
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                tracing::warn!("read_adrec: 尾部截断（忽略 {}B 不完整包）", len);
                break;
            }
            Err(e) => return Err(format!("read payload: {e}")),
        }
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

fn video_packets<'a>(pkts: &'a [AdrecPacket], codec: u8) -> Vec<&'a AdrecPacket> {
    pkts.iter()
        .filter(|p| p.kind == KIND_VIDEO && p.codec == codec)
        .collect()
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

/// Annex-B → AVCC/hvcC 包（lengthSizeMinusOne=3）：4 字节大端长度 + NAL。
fn annexb_to_avcc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for nal in annexb_nalus(data) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(&nal);
    }
    out
}

/// H.264 参数集：SPS(7)/PPS(8)。
fn find_h264_params(pkts: &[&AdrecPacket]) -> (Vec<u8>, Vec<u8>) {
    let mut sps = None;
    let mut pps = None;
    'outer: for p in pkts {
        for nal in annexb_nalus(&p.payload) {
            if nal.is_empty() {
                continue;
            }
            let t = nal[0] & 0x1f;
            if t == 7 && sps.is_none() {
                sps = Some(nal);
            } else if t == 8 && pps.is_none() {
                pps = Some(nal);
            }
            if sps.is_some() && pps.is_some() {
                break 'outer;
            }
        }
    }
    (sps.unwrap_or_default(), pps.unwrap_or_default())
}

/// H.265 参数集：VPS(32)/SPS(33)/PPS(34)（HEVC NAL type = (byte>>1)&0x3f）。
fn find_h265_params(pkts: &[&AdrecPacket]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (mut vps, mut sps, mut pps) = (None, None, None);
    'outer: for p in pkts {
        for nal in annexb_nalus(&p.payload) {
            if nal.is_empty() {
                continue;
            }
            let t = (nal[0] >> 1) & 0x3f;
            if t == 32 && vps.is_none() {
                vps = Some(nal);
            } else if t == 33 && sps.is_none() {
                sps = Some(nal);
            } else if t == 34 && pps.is_none() {
                pps = Some(nal);
            }
            if vps.is_some() && sps.is_some() && pps.is_some() {
                break 'outer;
            }
        }
    }
    (
        vps.unwrap_or_default(),
        sps.unwrap_or_default(),
        pps.unwrap_or_default(),
    )
}

/// 构造 H.264 AVCC extradata（ISO/IEC 14496-15）。
fn build_avcc(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>, String> {
    if sps.len() < 4 {
        return Err(format!("H264 SPS 太短（{}B）", sps.len()));
    }
    let mut out = Vec::with_capacity(8 + sps.len() + pps.len());
    out.push(1);
    out.push(sps[1]);
    out.push(sps[2]);
    out.push(sps[3]);
    out.push(0xff);
    out.push(0xe1);
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1);
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    Ok(out)
}

/// 构造 H.265 hvcC extradata（ISO/IEC 14496-15）。
/// SPS 载荷（跳过 2 字节 NAL 头）：byte0=sps_vps_id+max_sub_layers+nesting；
/// byte1=profile_space/tier/profile_idc；byte2-5=compat；byte6-11=constraint；byte12=level。
fn build_hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Result<Vec<u8>, String> {
    if vps.is_empty() || sps.is_empty() || pps.is_empty() {
        return Err("H265 缺 VPS/SPS/PPS".into());
    }
    if sps.len() < 14 {
        return Err(format!("H265 SPS 太短（{}B）", sps.len()));
    }
    let sp = &sps[2..]; // 跳过 2 字节 NAL 头
    let mut out = Vec::new();
    out.push(1); // 0 configurationVersion
    out.push(sp[1]); // 1 profile_space/tier/profile_idc
    out.extend_from_slice(&sp[2..6]); // 2-5 profile_compatibility_flags
    out.extend_from_slice(&sp[6..12]); // 6-11 constraint_indicator_flags
    out.push(sp[12]); // 12 level_idc
    out.push(0xf0); // 13 reserved + min_spatial_segmentation_idc 高位
    out.push(0x00); // 14 min_spatial_segmentation_idc 低位（无分割）
    out.push(0xfc); // 15 parallelismType（未知）
    out.push(0xfc); // 16 chromaFormat（未知，默认 4:2:0）
    out.push(0xf8); // 17 bitDepthLumaMinus8（未知）
    out.push(0xf8); // 18 bitDepthChromaMinus8（未知）
    out.push(0x00); // 19 avgFrameRate 高位
    out.push(0x00); // 20 avgFrameRate 低位
    out.push(0x0f); // 21 cf=0, numTemporalLayers=1, nested=1, lengthSizeMinusOne=3
    out.push(3); // 22 numOfArrays: VPS/SPS/PPS
    for nal in [&vps[..], &sps[..], &pps[..]] {
        let t = (nal[0] >> 1) & 0x3f;
        out.push(0x80 | (t & 0x3f)); // array_completeness=1 + nal_unit_type
        out.extend_from_slice(&1u16.to_be_bytes()); // numNalus
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(nal);
    }
    Ok(out)
}

/// 解码首关键帧探测宽高（复用 FfmpegDecoder）。
fn probe_size(
    video: &[&AdrecPacket],
    codec: aerodesk_core::platform::Codec,
) -> Result<(i32, i32), String> {
    let mut dec =
        crate::decode::FfmpegDecoder::new(codec).map_err(|e| format!("probe decoder: {e}"))?;
    for p in video {
        if p.keyframe {
            let unit = aerodesk_core::platform::EncodedUnit {
                data: p.payload.clone(),
                keyframe: true,
                pts_ms: 0,
                rtp_timestamp: 0,
            };
            if let Ok(Some(_)) = dec.decode_unit(&unit) {
                return Ok((dec.width() as i32, dec.height() as i32));
            }
        }
    }
    Err("无法探测宽高（首关键帧解码失败）".into())
}

/// 通用 mux 入口：按首个视频包 codec 选容器/参数集。
fn mux(
    input: &Path,
    output: &Path,
    pkts: &[AdrecPacket],
    codec: u8,
    extradata: Option<Vec<u8>>,
    annexb: bool,
) -> Result<u64, String> {
    crate::encode::init();
    let video = video_packets(pkts, codec);
    if video.is_empty() {
        return Err(format!("{input:?} 无 codec={codec} 视频包"));
    }
    let core_codec = match codec {
        CODEC_H264 => aerodesk_core::platform::Codec::H264,
        CODEC_H265 => aerodesk_core::platform::Codec::Hevc,
        CODEC_VP9 => aerodesk_core::platform::Codec::Vp9,
        CODEC_AV1 => aerodesk_core::platform::Codec::Av1,
        other => return Err(format!("不支持 codec={other}")),
    };
    let (w, h) = probe_size(&video, core_codec)?;

    let mut params = codec::Parameters::new();
    unsafe {
        let p = params.as_mut_ptr();
        (*p).codec_type = ffmpeg::ffi::AVMediaType::AVMEDIA_TYPE_VIDEO;
        (*p).codec_id = match codec {
            CODEC_H264 => ffmpeg::ffi::AVCodecID::AV_CODEC_ID_H264,
            CODEC_H265 => ffmpeg::ffi::AVCodecID::AV_CODEC_ID_HEVC,
            CODEC_VP9 => ffmpeg::ffi::AVCodecID::AV_CODEC_ID_VP9,
            _ => ffmpeg::ffi::AVCodecID::AV_CODEC_ID_AV1,
        };
        (*p).codec_tag = 0;
        (*p).width = w;
        (*p).height = h;
        if let Some(ex) = &extradata {
            let len = ex.len();
            let buf = ffmpeg::ffi::av_mallocz(len + 64) as *mut u8;
            if buf.is_null() {
                return Err("av_mallocz extradata failed".into());
            }
            std::ptr::copy_nonoverlapping(ex.as_ptr(), buf, len);
            (*p).extradata = buf;
            (*p).extradata_size = len as i32;
        }
    }
    let mut ctx = codec::context::Context::new();
    ctx.set_parameters(params)
        .map_err(|e| format!("set_parameters: {e}"))?;

    let mut out = format::output(output).map_err(|e| format!("open {output:?}: {e}"))?;
    let mut stream = out
        .add_stream_with(&ctx)
        .map_err(|e| format!("add_stream: {e}"))?;
    // WebM/Matroska muxer 以 1/1000 时间基写时间戳；MP4 用 90kHz。
    let webm = matches!(codec, CODEC_VP9 | CODEC_AV1);
    let tb = if webm {
        Rational(1, 1000)
    } else {
        Rational(1, 90_000)
    };
    stream.set_time_base(tb);
    out.write_header()
        .map_err(|e| format!("write_header: {e}"))?;

    let mut written = 0u64;
    for p in &video {
        let data = if annexb {
            annexb_to_avcc(&p.payload)
        } else {
            p.payload.clone()
        };
        let mut packet = Packet::new(data.len());
        if let Some(d) = packet.data_mut() {
            d.copy_from_slice(&data);
        }
        packet.set_stream(0);
        let ts = if webm {
            (p.rtp_ts / 90) as i64 // 90kHz → ms
        } else {
            p.rtp_ts as i64
        };
        packet.set_pts(Some(ts));
        packet.set_dts(Some(ts));
        packet.set_time_base(tb);
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

/// ADREC2 → 可播放容器（按 codec 自动选：H.264/H.265→MP4；VP9/AV1→WebM）。
pub fn adrec_to_container(input: &Path, output: &Path) -> Result<u64, String> {
    let pkts = read_adrec(input)?;
    let Some(first_video) = pkts.iter().find(|p| p.kind == KIND_VIDEO) else {
        return Err(format!("{input:?} 无视频包"));
    };
    match first_video.codec {
        CODEC_H264 => {
            let video = video_packets(&pkts, CODEC_H264);
            let (sps, pps) = find_h264_params(&video);
            if sps.is_empty() || pps.is_empty() {
                return Err("H264 流中未找到 SPS/PPS".into());
            }
            let ex = build_avcc(&sps, &pps)?;
            mux(input, output, &pkts, CODEC_H264, Some(ex), true)
        }
        CODEC_H265 => {
            let video = video_packets(&pkts, CODEC_H265);
            let (vps, sps, pps) = find_h265_params(&video);
            let ex = build_hvcc(&vps, &sps, &pps)?;
            mux(input, output, &pkts, CODEC_H265, Some(ex), true)
        }
        CODEC_VP9 => mux(input, output, &pkts, CODEC_VP9, None, false),
        CODEC_AV1 => {
            // av1c：0x81 + (seq_profile<<5|level=0) + 首个 sequence header OBU（去 size）。
            let video = video_packets(&pkts, CODEC_AV1);
            let mut ex = vec![0x81u8, 0x00];
            'outer: for p in &video {
                if !p.keyframe {
                    continue;
                }
                let mut off = 0usize;
                while off < p.payload.len() {
                    let h = p.payload[off];
                    let typ = (h >> 3) & 0xf;
                    let has = h & 0x02 != 0;
                    let mut pos = off + 1;
                    let sz = if has {
                        let mut v = 0u64;
                        let mut shift = 0;
                        loop {
                            let b = p.payload[pos];
                            pos += 1;
                            v |= ((b & 0x7f) as u64) << shift;
                            shift += 7;
                            if b & 0x80 == 0 {
                                break;
                            }
                        }
                        v as usize
                    } else {
                        p.payload.len() - pos
                    };
                    if typ == 1 && pos + sz <= p.payload.len() {
                        // sequence header OBU：去 size 标志 + 去 size 字段
                        ex.push(h & !0x02);
                        ex.extend_from_slice(&p.payload[pos..pos + sz]);
                        break 'outer;
                    }
                    off = pos + sz;
                }
            }
            if ex.len() == 2 {
                return Err("AV1 流中未找到 sequence header OBU".into());
            }
            mux(input, output, &pkts, CODEC_AV1, Some(ex), false)
        }
        other => Err(format!("不支持的 codec={other}（支持 h264/h265/vp9/av1）")),
    }
}

/// 兼容旧入口：ADREC2 → MP4（H.264）。
pub fn adrec_to_mp4(input: &Path, output: &Path) -> Result<u64, String> {
    adrec_to_container(input, output)
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
        assert_eq!(ex[8 + sps.len()], 1);
    }

    #[test]
    fn hvcc_extradata_layout() {
        // 2 字节 NAL 头（type 33）+ SPS 载荷（≥13 字节）
        let mut sps = vec![0x42, 0x01];
        sps.push(0x21); // sps_vps_id/max_sub_layers/nesting
        sps.push(0x00); // profile_space/tier/profile_idc(0=?)  -> 用 1 (Main)
        sps[3] = 0x01;
        sps.extend_from_slice(&[0x60, 0x00, 0x00, 0x00]); // compat
        sps.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // constraint
        sps.push(93); // level
        let vps = vec![0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60];
        let pps = vec![0x44, 0x01, 0xc1, 0x72, 0xb4, 0x62, 0x40];
        let ex = build_hvcc(&vps, &sps, &pps).unwrap();
        assert_eq!(ex[0], 1);
        assert_eq!(ex[1], 0x01); // Main profile
        assert_eq!(ex[12], 93); // level
        assert_eq!(ex[21] & 3, 3, "lengthSizeMinusOne=3");
        assert_eq!(ex[22], 3); // 3 arrays
        assert_eq!(ex[23], 0x80 | 32); // VPS
        let vps_len = u16::from_be_bytes([ex[26], ex[27]]) as usize;
        assert_eq!(vps_len, vps.len());
        let sps_hdr = 28 + vps_len;
        assert_eq!(ex[sps_hdr], 0x80 | 33); // SPS
        let sps_len = u16::from_be_bytes([ex[sps_hdr + 3], ex[sps_hdr + 4]]) as usize;
        assert_eq!(sps_len, sps.len());
        assert_eq!(ex[sps_hdr + 5 + sps_len], 0x80 | 34); // PPS
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
            h[0] = 0;
            h[1] = 1;
            h[2] = 1;
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
