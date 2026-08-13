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
use aerodesk_sfu::recorder::{
    CODEC_H264, CODEC_H265, CODEC_OPUS, CODEC_PCMU, KIND_AUDIO, KIND_VIDEO, MAGIC,
    PACKET_HEADER_LEN,
};

/// ADREC2 包：解析后的一帧/NAL 片段。
struct Packet {
    codec: u8,
    keyframe: bool,
    rtp_ts: u64,
    payload: Vec<u8>,
}

/// ADREC2 原始记录（未按 kind/codec 过滤）。
struct RawRecord {
    kind: u8,
    codec: u8,
    keyframe: bool,
    rtp_ts: u64,
    payload: Vec<u8>,
}

fn read_adrec2(path: &Path) -> Result<Vec<RawRecord>, String> {
    let file = File::open(path).map_err(|e| format!("open {path:?}: {e}"))?;
    let mut r = BufReader::new(file);
    let mut magic = [0u8; 7];
    r.read_exact(&mut magic)
        .map_err(|e| format!("read magic: {e}"))?;
    if &magic != MAGIC {
        return Err(format!("不是 ADREC2 文件（magic 不符）"));
    }
    let mut records = Vec::new();
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
        let keyframe = header[2] & 1 != 0;
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
        records.push(RawRecord {
            kind,
            codec,
            keyframe,
            rtp_ts,
            payload,
        });
    }
    Ok(records)
}

/// 提取视频 H264/H265 包（供访问单元聚合）。
fn parse_adrec2(path: &Path) -> Result<Vec<Packet>, String> {
    Ok(read_adrec2(path)?
        .into_iter()
        .filter(|r| r.kind == KIND_VIDEO && (r.codec == CODEC_H264 || r.codec == CODEC_H265))
        .map(|r| Packet {
            codec: r.codec,
            keyframe: r.keyframe,
            rtp_ts: r.rtp_ts,
            payload: r.payload,
        })
        .collect())
}

/// 提取 PCMU（G.711 μ-law）音频包，按 RTP 时间戳排序（8kHz 时钟）。
fn parse_pcmu_audio(path: &Path) -> Result<Vec<(u64, Vec<u8>)>, String> {
    let mut records: Vec<(u64, Vec<u8>)> = read_adrec2(path)?
        .into_iter()
        .filter(|r| r.kind == KIND_AUDIO && r.codec == CODEC_PCMU)
        .map(|r| (r.rtp_ts, r.payload))
        .collect();
    records.sort_by_key(|(ts, _)| *ts);
    Ok(records)
}

/// 提取 Opus 音频包，按 RTP 时间戳排序（48kHz 时钟）。
fn parse_opus_audio(path: &Path) -> Result<Vec<(u64, Vec<u8>)>, String> {
    let mut records: Vec<(u64, Vec<u8>)> = read_adrec2(path)?
        .into_iter()
        .filter(|r| r.kind == KIND_AUDIO && r.codec == CODEC_OPUS)
        .map(|r| (r.rtp_ts, r.payload))
        .collect();
    records.sort_by_key(|(ts, _)| *ts);
    Ok(records)
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

/// 把 PCMU（μ-law）音频包按时间戳顺序拼成原始 μ-law 基本流（8kHz 单声道）。
fn write_pcmu_es(records: &[(u64, Vec<u8>)], work_dir: &Path) -> Result<PathBuf, String> {
    let path = work_dir.join("audio.ulaw");
    let mut es = File::create(&path).map_err(|e| format!("create ulaw: {e}"))?;
    for (_, payload) in records {
        es.write_all(payload)
            .map_err(|e| format!("write ulaw: {e}"))?;
    }
    Ok(path)
}

/// 音频轨输入（决定 ffmpeg 的输入 demuxer 与输出 codec）。
enum AudioTrack {
    /// 原始 μ-law → `-f mulaw` + 转 AAC（MP4 不原生支持 μ-law）。
    Pcmu(PathBuf),
    /// Ogg/Opus → `-f ogg` + `-c:a copy`。
    Opus(PathBuf),
}

impl AudioTrack {
    fn path(&self) -> &Path {
        match self {
            AudioTrack::Pcmu(p) | AudioTrack::Opus(p) => p,
        }
    }
}

/// Ogg CRC-32（多项式 0x04C11DB7，无反射，初值 0，无最终异或）。
fn ogg_crc(data: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// 构造一个 Ogg 页（单包；lacing 按 255 字节分段）。
fn ogg_page(serial: u32, seq: u32, header_type: u8, granule: i64, packet: &[u8]) -> Vec<u8> {
    let mut lacing = Vec::new();
    let mut n = packet.len();
    while n >= 255 {
        lacing.push(255u8);
        n -= 255;
    }
    lacing.push(n as u8);

    let mut page = Vec::with_capacity(27 + lacing.len() + packet.len());
    page.extend_from_slice(b"OggS");
    page.push(0); // version
    page.push(header_type);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&seq.to_le_bytes());
    page.extend_from_slice(&[0u8; 4]); // CRC 占位
    page.push(lacing.len() as u8);
    page.extend_from_slice(&lacing);
    page.extend_from_slice(packet);

    // CRC 覆盖整页（含 "OggS"），CRC 字段置 0 后计算。
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

/// OpusHead：48kHz 双声道、pre-skip 312、mapping 0。
fn opus_head() -> Vec<u8> {
    let mut h = Vec::with_capacity(19);
    h.extend_from_slice(b"OpusHead");
    h.push(1); // version
    h.push(2); // channels（立体声）
    h.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
    h.extend_from_slice(&48_000u32.to_le_bytes()); // input sample rate
    h.extend_from_slice(&0u16.to_le_bytes()); // output gain
    h.push(0); // mapping family
    h
}

/// OpusTags：空 vendor + 空 comment 列表。
fn opus_tags() -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(b"OpusTags");
    t.extend_from_slice(&0u32.to_le_bytes()); // vendor 长度 0
    t.extend_from_slice(&0u32.to_le_bytes()); // comment 数 0
    t
}

/// 把 Opus 音频包按时间戳顺序写成 Ogg/Opus（每包一页，20ms@48kHz = 960 样本）。
fn write_opus_ogg(records: &[(u64, Vec<u8>)], work_dir: &Path) -> Result<PathBuf, String> {
    let path = work_dir.join("audio.opus");
    let mut f = File::create(&path).map_err(|e| format!("create opus: {e}"))?;
    let serial: u32 = 0xAD_DEC0DE;
    let mut seq = 0u32;
    f.write_all(&ogg_page(serial, seq, 0x02, 0, &opus_head())) // BOS
        .map_err(|e| format!("write OpusHead: {e}"))?;
    seq += 1;
    f.write_all(&ogg_page(serial, seq, 0x00, 0, &opus_tags()))
        .map_err(|e| format!("write OpusTags: {e}"))?;
    seq += 1;

    let mut granule: i64 = 0;
    let n = records.len();
    for (i, (_, payload)) in records.iter().enumerate() {
        granule += 960;
        // 最后一页置 EOS。
        let htype = if i + 1 == n { 0x04 } else { 0x00 };
        f.write_all(&ogg_page(serial, seq, htype, granule, payload))
            .map_err(|e| format!("write Opus page: {e}"))?;
        seq += 1;
    }
    Ok(path)
}

/// 调 ffmpeg 把 AnnexB 基本流复用为 MP4（-c copy，不重编码）。
/// `fmt` 为 `-f` 输入格式：h264 / hevc（不指定时 ffmpeg 可能把 HEVC 误判成 H264，
/// stsd 写 avc1 导致播放器/ffprobe 报错）。
fn mux_mp4(
    es: &Path,
    out: &Path,
    fps: u32,
    fmt: &str,
    audio: Option<&AudioTrack>,
) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    // 输入侧 `-r fps` 强制裸流帧率（比 -framerate 更彻底）：ffmpeg 的裸
    // HEVC demuxer 在 -framerate 下会在 ~51 帧后失步（帧时长变 1 tick），
    // 导致 MP4 时长只有 1.7s；-r 输入则正确分配 1/fps 每帧。
    cmd.args(["-y", "-f", fmt, "-r", &fps.to_string(), "-i"])
        .arg(es);
    if let Some(track) = audio {
        match track {
            // 原始 μ-law → MP4：转 AAC（MP4 不原生支持 μ-law），8kHz 单声道。
            AudioTrack::Pcmu(p) => {
                cmd.args(["-f", "mulaw", "-ar", "8000", "-ac", "1", "-i"])
                    .arg(p);
            }
            // Ogg/Opus → MP4：-c:a copy（Opus 原生支持）。
            AudioTrack::Opus(p) => {
                cmd.args(["-f", "ogg", "-i"]).arg(p);
            }
        }
    }
    cmd.args(["-map", "0:v:0", "-c:v", "copy"]);
    if let Some(track) = audio {
        let acodec = match track {
            AudioTrack::Pcmu(_) => "aac",
            AudioTrack::Opus(_) => "copy",
        };
        cmd.args(["-map", "1:a:0", "-c:a", acodec]);
    }
    let status = cmd
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
    let pcmu = parse_pcmu_audio(input)?;
    let opus = parse_opus_audio(input)?;
    let work_dir = std::env::temp_dir().join(format!("rec2mp4-{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("tmp dir: {e}"))?;
    let (es_path, est_fps, au_count, duration_secs, codec) =
        aggregate_and_write_es(&packets, &work_dir)?;
    // 音频轨优先 Opus（原生 copy）；否则 PCMU（转 AAC）。
    let audio = if !opus.is_empty() {
        Some(AudioTrack::Opus(write_opus_ogg(&opus, &work_dir)?))
    } else if !pcmu.is_empty() {
        Some(AudioTrack::Pcmu(write_pcmu_es(&pcmu, &work_dir)?))
    } else {
        None
    };
    let fps = fps_override.unwrap_or(est_fps);
    let fmt = if codec == CODEC_H264 { "h264" } else { "hevc" };
    mux_mp4(&es_path, output, fps, fmt, audio.as_ref())?;
    if std::env::var("REC2MP4_KEEP_ES").is_err() {
        let _ = std::fs::remove_file(&es_path);
        if let Some(track) = &audio {
            let _ = std::fs::remove_file(track.path());
        }
    } else {
        eprintln!("rec2mp4: 保留 ES {}", es_path.display());
        if let Some(track) = &audio {
            eprintln!("rec2mp4: 保留音频 ES {}", track.path().display());
        }
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

    /// 追加一条 ADREC2 记录（kind/codec/rtp_ts/payload）。
    fn push_record(buf: &mut Vec<u8>, kind: u8, codec: u8, rtp_ts: u64, payload: &[u8]) {
        buf.push(kind);
        buf.push(codec);
        buf.push(0); // flags（音频无关键帧）
        buf.push(0);
        buf.extend_from_slice(&0u64.to_le_bytes()); // wall ts
        buf.extend_from_slice(&rtp_ts.to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(payload);
    }

    #[test]
    fn parses_opus_audio_sorted_by_rtp_ts() {
        let work = std::env::temp_dir().join(format!("rec2mp4-opus-aud-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        let mut data = MAGIC.to_vec();
        push_record(&mut data, KIND_AUDIO, CODEC_OPUS, 960, &[0x01, 0x02]);
        push_record(&mut data, KIND_VIDEO, CODEC_H264, 3000, &[0, 0, 0, 1, 0x65]);
        push_record(&mut data, KIND_AUDIO, CODEC_OPUS, 480, &[0x03, 0x04]);
        let path = work.join("opus.adrec");
        std::fs::write(&path, &data).unwrap();

        let opus = parse_opus_audio(&path).unwrap();
        assert_eq!(opus.len(), 2);
        assert_eq!(opus[0].0, 480, "按 rtp_ts 升序");
        assert_eq!(opus[0].1, vec![0x03, 0x04]);
        assert_eq!(opus[1].0, 960);
        assert_eq!(opus[1].1, vec![0x01, 0x02]);
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn ogg_page_basic_structure_and_lacing() {
        // 单包 600 字节 → lacing 255+255+90；验证 magic/version/granule/serial/seq。
        let payload = vec![0xABu8; 600];
        let page = ogg_page(0xADDEC0DE, 3, 0x02, 960, &payload);
        assert_eq!(&page[0..4], b"OggS");
        assert_eq!(page[4], 0); // version
        assert_eq!(page[5], 0x02); // BOS
        assert_eq!(i64::from_le_bytes(page[6..14].try_into().unwrap()), 960);
        assert_eq!(
            u32::from_le_bytes(page[14..18].try_into().unwrap()),
            0xADDEC0DE
        );
        assert_eq!(u32::from_le_bytes(page[18..22].try_into().unwrap()), 3);
        assert_eq!(page[26], 3, "3 个 lacing 段");
        assert_eq!(&page[27..30], &[255, 255, 90], "255/255/90 lacing");
        assert_eq!(page.len(), 27 + 3 + 600);
        // CRC 字段非零。
        let crc = u32::from_le_bytes(page[22..26].try_into().unwrap());
        assert_ne!(crc, 0);
    }

    #[test]
    fn parses_pcmu_audio_sorted_by_rtp_ts() {
        let work = std::env::temp_dir().join(format!("rec2mp4-aud-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        let mut data = MAGIC.to_vec();
        // 乱序写入：先 rtp_ts=320，再视频，再 rtp_ts=160。
        push_record(&mut data, KIND_AUDIO, CODEC_PCMU, 320, &[0x11, 0x22]);
        push_record(&mut data, KIND_VIDEO, CODEC_H264, 3000, &[0, 0, 0, 1, 0x65]);
        push_record(&mut data, KIND_AUDIO, CODEC_PCMU, 160, &[0x33, 0x44]);
        let path = work.join("mix.adrec");
        std::fs::write(&path, &data).unwrap();

        let pcmu = parse_pcmu_audio(&path).unwrap();
        assert_eq!(pcmu.len(), 2);
        assert_eq!(pcmu[0].0, 160, "按 rtp_ts 升序");
        assert_eq!(pcmu[0].1, vec![0x33, 0x44]);
        assert_eq!(pcmu[1].0, 320);
        assert_eq!(pcmu[1].1, vec![0x11, 0x22]);

        // 视频仍正确解析，音频被过滤。
        let video = parse_adrec2(&path).unwrap();
        assert_eq!(video.len(), 1);
        assert_eq!(video[0].codec, CODEC_H264);
        let _ = std::fs::remove_dir_all(&work);
    }
}
