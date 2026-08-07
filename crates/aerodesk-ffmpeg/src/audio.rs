//! FFmpeg Opus 音频编解码（#73）：48kHz 立体声 → Opus 包（20ms/帧）。
//!
//! 远程桌面音频从 PCMU（8kHz 电话级）升级为 Opus（48kHz 更高音质）：
//! - 编码：libopus（ffmpeg），S16 交错输入，application=voip 低延迟参数
//! - 解码：libopus，请求 S16 输出，双声道平均为单声道 i16（与既有 jitter
//!   buffer / AudioSink 单声道管线兼容）
//! - str0m Opus 协商参数：PT 111 / 48kHz / 2 声道（见 str0m codec_config）

use ffmpeg_next as ffmpeg;
use ffmpeg_next::ChannelLayout;
use ffmpeg_next::codec::packet::Packet;
use ffmpeg_next::format::Sample;
use ffmpeg_next::format::sample::Type;
use ffmpeg_next::frame::Audio as AudioFrame;

/// Opus 默认参数：48kHz / 2 声道 / 20ms 帧（960 样本）。
pub const OPUS_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_CHANNELS: u16 = 2;
/// 20ms @ 48kHz 的每帧样本数。
pub const OPUS_FRAME_SAMPLES: usize = 960;

/// Opus 编码器（libopus，48kHz 立体声）。
pub struct OpusEncoder {
    encoder: ffmpeg_next::encoder::Audio,
    frame_samples: usize,
    pts: i64,
}

impl OpusEncoder {
    pub fn new(bitrate_bps: u64) -> Result<Self, String> {
        crate::encode::init();
        let codec = ffmpeg::encoder::find_by_name("libopus")
            .ok_or_else(|| "libopus encoder not found (ffmpeg 未编译 libopus)".to_string())?;
        let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .map_err(|e| format!("opus encoder context: {e}"))?;
        ctx.set_rate(OPUS_SAMPLE_RATE as i32);
        ctx.set_format(Sample::I16(Type::Packed));
        ctx.set_channel_layout(ChannelLayout::STEREO);
        ctx.set_bit_rate(bitrate_bps as usize);
        ctx.set_time_base(ffmpeg::Rational(1, OPUS_SAMPLE_RATE as i32));
        let mut dict = ffmpeg::Dictionary::new();
        // voip：低延迟、语音优先，适合远程桌面音频。
        dict.set("application", "voip");
        let encoder = ctx
            .open_with(dict)
            .map_err(|e| format!("opus encoder open: {e}"))?;
        let frame_samples = encoder.frame_size().max(OPUS_FRAME_SAMPLES as u32) as usize;
        tracing::info!(
            "opus encoder opened: {} Hz, {} ch, {} samples/frame, {bitrate_bps} bps",
            OPUS_SAMPLE_RATE,
            OPUS_CHANNELS,
            frame_samples
        );
        Ok(Self {
            encoder,
            frame_samples,
            pts: 0,
        })
    }

    /// 编码一帧单声道 PCM（48kHz），内部复制为立体声；返回 Opus 包。
    pub fn encode(&mut self, pcm_mono: &[i16]) -> Result<Option<Vec<u8>>, String> {
        let samples = self.frame_samples.min(pcm_mono.len());
        let mut frame = AudioFrame::new(Sample::I16(Type::Packed), samples, ChannelLayout::STEREO);
        frame.set_rate(OPUS_SAMPLE_RATE);
        frame.set_channel_layout(ChannelLayout::STEREO);
        frame.set_pts(Some(self.pts));
        self.pts += samples as i64;
        // S16 交错双声道：左=右=单声道样本（写入字节序无关，本机为 LE）。
        let dst = frame.data_mut(0);
        for (i, chunk) in dst.chunks_exact_mut(4).enumerate() {
            let s: i16 = pcm_mono.get(i).copied().unwrap_or(0);
            let bytes = s.to_le_bytes();
            chunk[0] = bytes[0];
            chunk[1] = bytes[1];
            chunk[2] = bytes[0];
            chunk[3] = bytes[1];
        }
        self.encoder
            .send_frame(&frame)
            .map_err(|e| format!("opus send_frame: {e}"))?;
        let mut packet = Packet::new(4096);
        match self.encoder.receive_packet(&mut packet) {
            Ok(()) => Ok(Some(packet.data().unwrap_or(&[]).to_vec())),
            Err(ffmpeg::Error::Eof) => Ok(None),
            Err(ffmpeg::Error::Other { errno }) if errno.abs() == 11 => Ok(None), // EAGAIN
            Err(e) => Err(format!("opus receive_packet: {e:?}")),
        }
    }
}

/// Opus 解码器（libopus → 单声道 i16，48kHz）。
pub struct OpusDecoder {
    decoder: ffmpeg_next::decoder::Audio,
    sample_rate: u32,
    channels: u16,
}

impl OpusDecoder {
    pub fn new() -> Result<Self, String> {
        crate::encode::init();
        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::OPUS)
            .ok_or_else(|| "opus decoder not found".to_string())?;
        let mut decoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .decoder()
            .audio()
            .map_err(|e| format!("opus decoder context: {e}"))?;
        // 统一输出 S16 交错，避免按解码器默认格式（FLTP）分平面处理。
        decoder.request_format(Sample::I16(Type::Packed));
        let sample_rate = decoder.rate();
        let channels = decoder.channels();
        tracing::info!("opus decoder opened: {sample_rate} Hz, {channels} ch");
        Ok(Self {
            decoder,
            sample_rate,
            channels,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// 解码一个 Opus 包 → 单声道 i16 PCM（双声道平均）。
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Vec<i16>>, String> {
        let mut packet = Packet::new(data.len());
        if let Some(d) = packet.data_mut() {
            d.copy_from_slice(data);
        }
        self.decoder
            .send_packet(&packet)
            .map_err(|e| format!("opus send_packet: {e}"))?;
        let mut frame = AudioFrame::empty();
        match self.decoder.receive_frame(&mut frame) {
            Ok(()) => {
                let samples = frame.samples();
                let ch = frame.channels().max(1) as usize;
                let bytes = frame.data(0);
                let mut out = Vec::with_capacity(samples);
                for i in 0..samples {
                    let mut sum = 0i32;
                    let mut cnt = 0usize;
                    for c in 0..ch {
                        let off = (i * ch + c) * 2;
                        if off + 2 <= bytes.len() {
                            sum += i16::from_le_bytes([bytes[off], bytes[off + 1]]) as i32;
                            cnt += 1;
                        }
                    }
                    out.push((sum / cnt.max(1) as i32) as i16);
                }
                Ok(Some(out))
            }
            Err(ffmpeg::Error::Eof) => Ok(None),
            Err(ffmpeg::Error::Other { errno }) if errno.abs() == 11 => Ok(None), // EAGAIN
            Err(e) => Err(format!("opus receive_frame: {e:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_encode_decode_roundtrip() {
        // 无 libopus 环境（未装 ffmpeg）时跳过。
        let Ok(mut enc) = OpusEncoder::new(64_000) else {
            eprintln!("skip: libopus encoder unavailable");
            return;
        };
        let Ok(mut dec) = OpusDecoder::new() else {
            eprintln!("skip: libopus decoder unavailable");
            return;
        };
        let mut pcm = vec![0i16; OPUS_FRAME_SAMPLES];
        for (i, s) in pcm.iter_mut().enumerate() {
            // 440Hz 正弦，避免静音被 Opus 压缩为 DTX 空包。
            let t = i as f64 / OPUS_SAMPLE_RATE as f64;
            *s = ((t * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16;
        }
        let mut packets = Vec::new();
        for _ in 0..4 {
            if let Some(p) = enc.encode(&pcm).expect("encode") {
                packets.push(p);
            }
        }
        assert!(!packets.is_empty(), "应产出 Opus 包");
        let mut decoded_samples = 0usize;
        let mut non_silent = false;
        for p in &packets {
            if let Some(out) = dec.decode(p).expect("decode") {
                decoded_samples += out.len();
                non_silent |= out.iter().any(|s| s.abs() > 100);
            }
        }
        // 4 帧 × 960 样本（编码器可能有少量内部延迟/裁剪，允许偏差）。
        assert!(
            decoded_samples >= 960 * 2,
            "解码样本数 {decoded_samples} 过少"
        );
        assert!(non_silent, "解码 PCM 不应全静音");
    }
}
