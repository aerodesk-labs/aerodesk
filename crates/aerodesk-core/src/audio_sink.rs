//! #73 观看端音频播放（cpal 全平台：macOS CoreAudio / Windows WASAPI / Linux ALSA）。
//! PCMU/Opus 解码后的 i16 → 线性重采样 → 默认输出设备。
//!
//! cpal 拉模型回调从内部缓冲取数；无输出设备/权限时 `AudioSink::new()` 返回
//! Err，调用方降级为「仅统计不播放」。静音在 sink 层生效（丢弃缓冲）。

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// 8kHz 输入 → 输出采样率线性重采样器（内部缓冲 + 读取位置）。
#[derive(Debug)]
pub struct Resampler {
    /// 未消费的输入样本（8kHz i16）。
    buf: Vec<i16>,
    /// 绝对读取位置（输入样本单位）。
    read_pos: f64,
    /// 输出 1 个采样消耗的输入样本数（in_rate/out_rate）。
    ratio: f64,
    out_channels: u16,
    /// f32 输出中转。
    scratch: Vec<i16>,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, out_channels: u16) -> Self {
        Self {
            buf: Vec::new(),
            read_pos: 0.0,
            ratio: in_rate as f64 / out_rate.max(1) as f64,
            out_channels: out_channels.max(1),
            scratch: Vec::new(),
        }
    }

    /// 追加输入样本（8kHz）。
    pub fn push(&mut self, samples: &[i16]) {
        self.buf.extend_from_slice(samples);
        // 只保留 0.5s 历史，防止读取位置落后导致缓冲无限增长。
        const MAX: usize = 4000;
        if self.buf.len() > MAX {
            let cut = self.buf.len() - MAX;
            self.buf.drain(..cut);
            self.read_pos = (self.read_pos - cut as f64).max(0.0);
        }
    }

    /// 丢弃全部缓冲（静音切换时）。
    pub fn reset(&mut self) {
        self.buf.clear();
        self.read_pos = 0.0;
    }

    /// 输出 i16 采样（按声道复制单声道）。
    pub fn fill_i16(&mut self, out: &mut [i16]) {
        let frames = out.len() / self.out_channels as usize;
        let mut wrote = 0usize;
        for _ in 0..frames {
            let s = self.next_sample();
            for _ in 0..self.out_channels as usize {
                if wrote < out.len() {
                    out[wrote] = s;
                    wrote += 1;
                }
            }
        }
        for slot in out.iter_mut().skip(wrote) {
            *slot = 0;
        }
    }

    /// 输出 f32 采样（-1..1）。
    pub fn fill_f32(&mut self, out: &mut [f32]) {
        let mut tmp = std::mem::take(&mut self.scratch);
        tmp.resize(out.len(), 0);
        self.fill_i16(&mut tmp);
        for (o, s) in out.iter_mut().zip(tmp.iter()) {
            *o = *s as f32 / 32768.0;
        }
        self.scratch = tmp;
    }

    fn next_sample(&mut self) -> i16 {
        if self.read_pos + 1.0 >= self.buf.len() as f64 {
            return 0; // 数据不足：静音且不推进，数据到达后从原处继续
        }
        let i = self.read_pos.floor() as usize;
        let frac = self.read_pos - i as f64;
        let s0 = self.buf[i] as f32;
        let s1 = self.buf[i + 1] as f32;
        self.read_pos += self.ratio;
        (s0 + (s1 - s0) * frac as f32).clamp(-32768.0, 32767.0) as i16
    }
}

/// 输出设备音频播放器（macOS cpal）。
pub struct AudioSink {
    _stream: cpal::Stream,
    state: Arc<Mutex<Resampler>>,
    muted: Arc<AtomicBool>,
    /// 音量 0..=100（#73 观看端音量滑块；100=原音量）。
    volume: Arc<AtomicU16>,
}

impl AudioSink {
    /// 打开默认输出设备并启动播放（PCMU 8kHz 输入）。无设备/格式不支持时返回 Err。
    pub fn new() -> Result<Self, String> {
        Self::new_with_rate(8000)
    }

    /// 打开默认输出设备并启动播放，输入 PCM 采样率可指定（#73：
    /// PCMU=8000，Opus=48000）。无设备/格式不支持时返回 Err。
    pub fn new_with_rate(in_rate: u32) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string())?;
        let config = device
            .default_output_config()
            .map_err(|e| format!("default output config: {e}"))?;
        let out_rate = config.sample_rate().0;
        let out_channels = config.channels();
        let state = Arc::new(Mutex::new(Resampler::new(
            in_rate.max(1),
            out_rate,
            out_channels,
        )));
        let muted = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU16::new(100));
        let stream_config: cpal::StreamConfig = config.clone().into();

        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => {
                let st = state.clone();
                let mu = muted.clone();
                let vo = volume.clone();
                device
                    .build_output_stream(
                        &stream_config,
                        move |data: &mut [f32], _| fill_f32(data, &st, &mu, &vo),
                        |e| tracing::warn!("audio stream error: {e}"),
                        None,
                    )
                    .map_err(|e| format!("build f32 stream: {e}"))?
            }
            cpal::SampleFormat::I16 => {
                let st = state.clone();
                let mu = muted.clone();
                let vo = volume.clone();
                device
                    .build_output_stream(
                        &stream_config,
                        move |data: &mut [i16], _| fill_i16(data, &st, &mu, &vo),
                        |e| tracing::warn!("audio stream error: {e}"),
                        None,
                    )
                    .map_err(|e| format!("build i16 stream: {e}"))?
            }
            other => return Err(format!("unsupported output sample format {other:?}")),
        };
        stream
            .play()
            .map_err(|e| format!("audio stream play: {e}"))?;
        tracing::info!(
            "audio sink started: {} Hz, {} ch, format {:?}",
            out_rate,
            out_channels,
            config.sample_format()
        );
        Ok(Self {
            _stream: stream,
            state,
            muted,
            volume,
        })
    }

    /// 写入一段 PCM（PCMU 8kHz / Opus 48kHz，按创建时输入采样率）。
    pub fn push_pcm(&self, pcm: &[i16]) {
        if let Ok(mut st) = self.state.lock() {
            st.push(pcm);
        }
    }

    /// 静音/取消静音（丢弃已缓冲音频）。
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
        if muted {
            if let Ok(mut st) = self.state.lock() {
                st.reset();
            }
        }
    }

    /// 设置音量 0..=100（#73 观看端音量滑块）。
    pub fn set_volume(&self, volume: u16) {
        self.volume.store(volume.min(100), Ordering::SeqCst);
    }
}

/// 应用音量增益（i16，0..=100；100=原样）。
pub fn apply_gain_i16(sample: i16, volume: u16) -> i16 {
    if volume >= 100 {
        return sample;
    }
    ((sample as f32 * volume as f32 / 100.0).round()).clamp(-32768.0, 32767.0) as i16
}

/// 应用音量增益（f32，0..=100；100=原样）。
pub fn apply_gain_f32(sample: f32, volume: u16) -> f32 {
    if volume >= 100 {
        return sample;
    }
    (sample * volume as f32 / 100.0).clamp(-1.0, 1.0)
}

fn fill_f32(data: &mut [f32], state: &Mutex<Resampler>, muted: &AtomicBool, volume: &AtomicU16) {
    if muted.load(Ordering::SeqCst) {
        data.fill(0.0);
        return;
    }
    if let Ok(mut st) = state.lock() {
        st.fill_f32(data);
    } else {
        data.fill(0.0);
    }
    let vol = volume.load(Ordering::SeqCst);
    for s in data.iter_mut() {
        *s = apply_gain_f32(*s, vol);
    }
}

fn fill_i16(data: &mut [i16], state: &Mutex<Resampler>, muted: &AtomicBool, volume: &AtomicU16) {
    if muted.load(Ordering::SeqCst) {
        data.fill(0);
        return;
    }
    if let Ok(mut st) = state.lock() {
        st.fill_i16(data);
    } else {
        data.fill(0);
    }
    let vol = volume.load(Ordering::SeqCst);
    for s in data.iter_mut() {
        *s = apply_gain_i16(*s, vol);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_constant_8k_to_48k() {
        let mut r = Resampler::new(8000, 48000, 1);
        r.push(&[1000i16; 8000]);
        let mut out = vec![0i16; 4800];
        r.fill_i16(&mut out);
        assert!(out.iter().all(|&s| s == 1000), "常量信号应保持常量");
    }

    #[test]
    fn underrun_outputs_silence() {
        let mut r = Resampler::new(8000, 48000, 1);
        let mut out = vec![0i16; 480];
        r.fill_i16(&mut out);
        assert!(out.iter().all(|&s| s == 0));
    }

    #[test]
    fn mono_replicated_to_stereo() {
        let mut r = Resampler::new(8000, 48000, 2);
        r.push(&[500i16; 8000]);
        let mut out = vec![0i16; 960];
        r.fill_i16(&mut out);
        for pair in out.chunks_exact(2) {
            assert_eq!(pair[0], pair[1]);
            assert_eq!(pair[0], 500);
        }
    }

    #[test]
    fn reset_discards_buffered() {
        let mut r = Resampler::new(8000, 48000, 1);
        r.push(&[1000i16; 8000]);
        r.reset();
        let mut out = vec![0i16; 480];
        r.fill_i16(&mut out);
        assert!(out.iter().all(|&s| s == 0));
    }

    #[test]
    fn gain_i16_scales_and_clamps() {
        assert_eq!(apply_gain_i16(1000, 100), 1000);
        assert_eq!(apply_gain_i16(1000, 50), 500);
        assert_eq!(apply_gain_i16(-1000, 50), -500);
        assert_eq!(apply_gain_i16(0, 0), 0);
        assert_eq!(apply_gain_i16(i16::MAX, 100), i16::MAX);
        // 0 音量 → 静音
        assert_eq!(apply_gain_i16(12345, 0), 0);
        // 超过 100 的输入按 100 处理
        assert_eq!(apply_gain_i16(1000, 200), 1000);
    }

    #[test]
    fn gain_f32_scales_and_clamps() {
        assert_eq!(apply_gain_f32(0.5, 100), 0.5);
        assert!((apply_gain_f32(0.5, 50) - 0.25).abs() < 1e-6);
        assert!((apply_gain_f32(-0.5, 50) + 0.25).abs() < 1e-6);
        assert_eq!(apply_gain_f32(0.0, 0), 0.0);
        assert_eq!(apply_gain_f32(0.9, 0), 0.0);
        assert_eq!(apply_gain_f32(0.9, 200), 0.9);
    }

    /// Windows/Linux 观看端音频播放：默认输出设备可创建并 push 不 panic（无设备 SKIP）。
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn sink_creates_and_accepts_pcm() {
        let mut sink = match AudioSink::new_with_rate(8000) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("SKIP: audio sink init failed: {e}");
                return;
            }
        };
        sink.push_pcm(&[0i16; 800]);
        sink.set_muted(false);
        sink.set_volume(100);
        eprintln!("audio sink ok (Windows/Linux 输出设备)");
    }
}

/// 核心 `AudioSink` 实现（观看端 PCM 播放）。
///
/// 注意：`self.push_pcm(...)` 在这里会解析到 **trait 方法自身**（固有方法接收者
/// 是 `&self`、trait 是 `&mut self`，解析优先 trait）→ 无限递归告警。用 UFCS
/// `<AudioSink>::` 显式调固有实现。
impl crate::platform::AudioSink for AudioSink {
    fn push_pcm(&mut self, samples: &[i16]) {
        <AudioSink>::push_pcm(self, samples);
    }

    fn set_muted(&mut self, muted: bool) {
        <AudioSink>::set_muted(self, muted);
    }

    fn set_volume(&mut self, volume: u16) {
        <AudioSink>::set_volume(self, volume);
    }
}
