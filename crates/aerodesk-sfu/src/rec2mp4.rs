//! rec2mp4 —— 把 ADREC2 录制转封装为 MP4（#266）。
//!
//! 核心：按 RTP 时间戳用 `AccessUnitAssembler` 把 NAL **聚合为完整访问单元
//! （一帧）**，每个访问单元写入一个 MP4 sample 对应的 AnnexB 数据——
//! 修复「按 NAL 写 sample 导致帧数放大/播放器兼容」问题。
//!
//! 用法：`rec2mp4 <in.adrec> <out.mp4> [--fps N]`
//! 输出：AnnexB 基本流（.h264/.h265）→ 调系统 ffmpeg `-c copy` 复用为 MP4
//! （MP4 盒子由 ffmpeg 生成，本工具专注访问单元聚合的正确性）。

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use aerodesk_core::access_unit::AccessUnitAssembler;
use aerodesk_sfu::recorder::{CODEC_H264, CODEC_H265, KIND_VIDEO, MAGIC, PACKET_HEADER_LEN};

/// ADREC2 包：解析后的一帧/NAL 片段。
struct Packet {
    codec: u8,
    keyframe: bool,
    rtp_ts: u64,
    payload: Vec<u8>,
}

fn parse_adrec2(path: &Path) -> Result<Vec<Packet>, String> {
    let file = File::open(path).map_err(|e| format!("open {path:?}: {e}"))?;
    let mut r = BufReader::new(file);
    let mut magic = [0u8; 7];
    r.read_exact(&mut magic)
        .map_err(|e| format!("read magic: {e}"))?;
    if &magic != MAGIC {
        return Err(format!("不是 ADREC2 文件（magic 不符）"));
    }
    let mut packets = Vec::new();
    let mut header = [0u8; PACKET_HEADER_LEN];
    loop {
        let mut n = 0usize;
        while n < PACKET_HEADER_LEN {
            match r.read(&mut header[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) => return Err(format!("read header: {e}")),
            }
        }
        if n == 0 {
            break; // EOF
        }
        if n < PACKET_HEADER_LEN {
            return Err(format!("截断的包头（{n}/{PACKET_HEADER_LEN} 字节）"));
        }
        let kind = header[0];
        let codec = header[1];
        let flags = header[2];
        let rtp_ts = u64::from_le_bytes(header[12..20].try_into().unwrap());
        let len = u32::from_le_bytes(header[20..24].try_into().unwrap()) as usize;
        if len > 64 << 20 {
            return Err(format!("包长异常：{len}"));
        }
        let mut payload = vec![0u8; len];
        // 进程被杀时尾部可能只有半截包：容忍截断，跳过该包（不再读入错误）。
        let mut got = 0usize;
        while got < len {
            match r.read(&mut payload[got..]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(e) => return Err(format!("read payload ({len}B): {e}")),
            }
        }
        if got < len {
            eprintln!("rec2mp4: 尾部截断包跳过（期望 {len}B，实得 {got}B）");
            break;
        }
        // 只要视频 H264/H265；音频暂不转封装（#266 聚焦视频帧正确性）。
        if kind == KIND_VIDEO && (codec == CODEC_H264 || codec == CODEC_H265) {
            packets.push(Packet {
                codec,
                keyframe: flags & 1 != 0,
                rtp_ts,
                payload,
            });
        }
    }
    Ok(packets)
}

/// 估算帧率：取相邻访问单元 pts 差的众数区间（90kHz RTP 时间戳），默认 30。
fn estimate_fps(pts_us: &[u64]) -> u32 {
    if pts_us.len() < 2 {
        return 30;
    }
    let mut deltas: Vec<u64> = pts_us
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .collect();
    deltas.retain(|d| *d > 0 && *d < 1_000_000); // 0.1..1000ms 之间
    if deltas.is_empty() {
        return 30;
    }
    deltas.sort_unstable();
    let median_us = deltas[deltas.len() / 2];
    let fps = (1_000_000.0 / median_us as f64).round() as u32;
    fps.clamp(1, 120)
}

/// 把 3 字节起始码 `00 00 01` 统一为 4 字节 `00 00 00 01`（不处理 4 字节已存在）。
fn normalize_start_codes(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut i = 0;
    while i < data.len() {
        if i + 3 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 1
            && (i + 3 == data.len() || data[i + 3] != 0)
        {
            out.extend_from_slice(&[0, 0, 0, 1]);
            i += 3;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// 聚合访问单元并写 AnnexB 基本流，返回 (es 临时文件, fps, au 数, 时长秒)。
fn aggregate_and_write_es(
    packets: &[Packet],
    work_dir: &Path,
) -> Result<(PathBuf, u32, usize, f64, u8), String> {
    let mut assembler = AccessUnitAssembler::new();
    let mut aus: Vec<(Vec<u8>, bool, u64)> = Vec::new(); // (annexb, keyframe, pts_us)
    for p in packets {
        // RTP 90kHz → 微秒（AccessUnitAssembler 的时间单位）。
        let pts_us = p.rtp_ts.saturating_mul(1_000_000) / 90_000;
        if let Some(au) = assembler.push(&p.payload, pts_us, p.keyframe) {
            aus.push((au.data, au.keyframe, au.pts_us));
        }
    }
    if let Some(au) = assembler.flush() {
        aus.push((au.data, au.keyframe, au.pts_us));
    }
    if aus.is_empty() {
        return Err("没有可转封装的视频访问单元（需要 H264/H265 视频包）".into());
    }
    let codec = packets
        .iter()
        .find(|p| p.codec == CODEC_H264 || p.codec == CODEC_H265)
        .map(|p| p.codec)
        .unwrap_or(CODEC_H264);
    let ext = if codec == CODEC_H264 { "h264" } else { "h265" };
    let es_path = work_dir.join(format!("out.{ext}"));
    let codec_out = codec;
    let mut es = File::create(&es_path).map_err(|e| format!("create es: {e}"))?;
    let mut pts = Vec::with_capacity(aus.len());
    // 每帧前插 AUD（H.264 type9 / HEVC type35）：ffmpeg 裸流解析器没有 AUD 时
    // 对 HEVC 会在 ~52 帧后误解析成微型帧（时长/帧率失真）。
    let aud: &[u8] = if codec == CODEC_H264 {
        &[0, 0, 0, 1, 0x09, 0xF0]
    } else {
        &[0, 0, 0, 1, 0x46, 0x01]
    };
    for (data, _, pts_us) in &aus {
        es.write_all(aud)
            .map_err(|e| format!("write es aud: {e}"))?;
        // 统一 4 字节起始码（00 00 00 01）：ffmpeg 裸 HEVC 解析器对 3/4 字节
        // 混用流会在 ~51 帧后失步（帧时长变为 1 tick）。
        es.write_all(&normalize_start_codes(data))
            .map_err(|e| format!("write es: {e}"))?;
        pts.push(*pts_us);
    }
    let fps = estimate_fps(&pts);
    let duration_secs = if aus.len() > 1 {
        (pts[pts.len() - 1] - pts[0]) as f64 / 1_000_000.0
    } else {
        0.0
    };
    Ok((es_path, fps, aus.len(), duration_secs, codec_out))
}

/// 调 ffmpeg 把 AnnexB 基本流复用为 MP4（-c copy，不重编码）。
/// `fmt` 为 `-f` 输入格式：h264 / hevc（不指定时 ffmpeg 可能把 HEVC 误判成 H264，
/// stsd 写 avc1 导致播放器/ffprobe 报错）。
fn mux_mp4(es: &Path, out: &Path, fps: u32, fmt: &str) -> Result<(), String> {
    let status = Command::new("ffmpeg")
        // 输入侧 `-r fps` 强制裸流帧率（比 -framerate 更彻底）：ffmpeg 的裸
        // HEVC demuxer 在 -framerate 下会在 ~51 帧后失步（帧时长变 1 tick），
        // 导致 MP4 时长只有 1.7s；-r 输入则正确分配 1/fps 每帧。
        .args(["-y", "-f", fmt, "-r", &fps.to_string(), "-i"])
        .arg(es)
        .args(["-c", "copy"])
        .arg(out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|e| format!("ffmpeg 启动失败（需安装 ffmpeg）: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg 复用失败（exit={status}）"));
    }
    Ok(())
}

/// 转封装入口：ADREC2 → MP4。
pub fn convert(
    input: &Path,
    output: &Path,
    fps_override: Option<u32>,
) -> Result<ConvertStats, String> {
    let packets = parse_adrec2(input)?;
    let work_dir = std::env::temp_dir().join(format!("rec2mp4-{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("tmp dir: {e}"))?;
    let (es_path, est_fps, au_count, duration_secs, codec) =
        aggregate_and_write_es(&packets, &work_dir)?;
    let fps = fps_override.unwrap_or(est_fps);
    let fmt = if codec == CODEC_H264 { "h264" } else { "hevc" };
    mux_mp4(&es_path, output, fps, fmt)?;
    if std::env::var("REC2MP4_KEEP_ES").is_err() {
        let _ = std::fs::remove_file(&es_path);
    } else {
        eprintln!("rec2mp4: 保留 ES {}", es_path.display());
    }
    Ok(ConvertStats {
        frames: au_count,
        fps,
        duration_secs,
    })
}

/// 转换统计（e2e/日志用）。
#[derive(Debug, Clone, Copy)]
pub struct ConvertStats {
    pub frames: usize,
    pub fps: u32,
    pub duration_secs: f64,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("用法: rec2mp4 <in.adrec> <out.mp4> [--fps N]");
        std::process::exit(2);
    }
    let input = PathBuf::from(&args[1]);
    let output = PathBuf::from(&args[2]);
    let fps_override = args
        .iter()
        .position(|a| a == "--fps")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok());
    match convert(&input, &output, fps_override) {
        Ok(st) => {
            eprintln!(
                "rec2mp4: {} 帧, fps={}, 时长={:.2}s → {}",
                st.frames,
                st.fps,
                st.duration_secs,
                output.display()
            );
        }
        Err(e) => {
            eprintln!("rec2mp4: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h264_annexb(nal: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1];
        v.extend_from_slice(nal);
        v
    }

    /// 合成 ADREC2：SPS/PPS + IDR 同 rtp_ts，随后多个 P 帧。
    fn synthetic_packets() -> Vec<Packet> {
        // SPS/PPS/IDR（rtp_ts=3000），P 帧（30fps → 90kHz 增量 3000）。
        let sps = h264_annexb(&[0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40]);
        let pps = h264_annexb(&[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0]);
        let idr = h264_annexb(&[0x65, 0x88, 0x84, 0x00]);
        let mut pkts = vec![
            Packet {
                codec: CODEC_H264,
                keyframe: true,
                rtp_ts: 3000,
                payload: sps,
            },
            Packet {
                codec: CODEC_H264,
                keyframe: true,
                rtp_ts: 3000,
                payload: pps,
            },
            Packet {
                codec: CODEC_H264,
                keyframe: true,
                rtp_ts: 3000,
                payload: idr,
            },
        ];
        for (i, ts) in [6000u64, 9000, 12000, 15000].iter().enumerate() {
            pkts.push(Packet {
                codec: CODEC_H264,
                keyframe: false,
                rtp_ts: *ts,
                payload: h264_annexb(&[0x41, 0x9a, 0x00 + i as u8, 0x01]),
            });
        }
        pkts
    }

    #[test]
    fn aggregates_by_rtp_ts_into_access_units() {
        let work = std::env::temp_dir().join(format!("rec2mp4-test-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        let (es, fps, frames, _dur, _codec) =
            aggregate_and_write_es(&synthetic_packets(), &work).unwrap();
        // 1 关键帧 AU + 4 P 帧 AU = 5 帧（不再是 7 个 NAL sample）。
        assert_eq!(frames, 5, "应按访问单元聚合为 5 帧");
        assert_eq!(fps, 30);
        let data = std::fs::read(&es).unwrap();
        assert!(data.windows(4).any(|w| w == [0, 0, 0, 1]), "含起始码");
        // 关键帧 AU 应包含 SPS+PPS+IDR（聚合验证）。
        let sps_pos = data
            .windows(7)
            .position(|w| w == [0, 0, 0, 1, 0x67, 0x64, 0x00]);
        let idr_pos = data.windows(6).position(|w| w == [0, 0, 0, 1, 0x65, 0x88]);
        assert!(
            sps_pos.is_some() && idr_pos.is_some(),
            "关键帧 AU 含 SPS+IDR"
        );
        assert!(sps_pos.unwrap() < idr_pos.unwrap(), "SPS 在 IDR 前");
        let _ = std::fs::remove_dir_all(&work);
    }
}
