//! A/V 同步（#73）：RTP 时间戳 → 统一时间轴 + 漂移跟踪 + 音频 jitter buffer。
//!
//! 时间轴：以首个媒体帧为基准（audio/video 各自归零），漂移 = audio - video。
//! jitter buffer：吸收网络抖动，目标播放延迟后弹出；迟到帧丢弃。

use std::collections::VecDeque;

/// RTP 时间戳 → 秒。
pub fn rtp_secs(rtp_ts: u64, clock_rate: u32) -> f64 {
    rtp_ts as f64 / clock_rate.max(1) as f64
}

/// A/V 时间轴与漂移跟踪。
#[derive(Debug, Default)]
pub struct AvSync {
    audio_base: Option<f64>,
    video_base: Option<f64>,
    audio_time: f64,
    video_time: f64,
}

impl AvSync {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一帧音频（PCMU 8kHz / Opus 48kHz 等）。
    pub fn on_audio(&mut self, rtp_ts: u64, clock_rate: u32) {
        let t = rtp_secs(rtp_ts, clock_rate);
        match self.audio_base {
            None => {
                self.audio_base = Some(t);
                self.audio_time = 0.0;
            }
            Some(base) => self.audio_time = t - base,
        }
    }

    /// 记录一帧视频（90kHz）。
    pub fn on_video(&mut self, rtp_ts: u64, clock_rate: u32) {
        let t = rtp_secs(rtp_ts, clock_rate);
        match self.video_base {
            None => {
                self.video_base = Some(t);
                self.video_time = 0.0;
            }
            Some(base) => self.video_time = t - base,
        }
    }

    /// 漂移（秒）：正 = 音频领先视频。
    pub fn drift_secs(&self) -> f64 {
        if self.audio_base.is_none() || self.video_base.is_none() {
            return 0.0;
        }
        self.audio_time - self.video_time
    }

    /// 漂移（毫秒）。
    pub fn drift_ms(&self) -> f64 {
        self.drift_secs() * 1000.0
    }

    pub fn audio_time_secs(&self) -> f64 {
        self.audio_time
    }

    pub fn video_time_secs(&self) -> f64 {
        self.video_time
    }
}

/// 音频 jitter buffer（时间轴秒，20ms 帧）。
#[derive(Debug)]
pub struct AudioJitterBuffer {
    frames: VecDeque<(f64, Vec<i16>)>,
    /// 目标播放延迟（秒），吸收网络抖动。
    target_delay: f64,
    dropped: u64,
    last_play: Option<f64>,
}

impl AudioJitterBuffer {
    pub fn new(target_delay: f64) -> Self {
        Self {
            frames: VecDeque::new(),
            target_delay,
            dropped: 0,
            last_play: None,
        }
    }

    /// 入队一帧 PCM；若已远迟于播放位置（迟到帧）则丢弃。
    pub fn push(&mut self, t: f64, pcm: Vec<i16>) {
        if let Some(last) = self.last_play {
            if t + 0.02 < last {
                self.dropped += 1;
                return;
            }
        }
        self.frames.push_back((t, pcm));
    }

    /// 到播放时刻（首帧 + target_delay）弹出；未到点返回 None。
    pub fn pop(&mut self, now: f64) -> Option<Vec<i16>> {
        let (t, _) = self.frames.front()?;
        if now < t + self.target_delay {
            return None;
        }
        let (t, pcm) = self.frames.pop_front()?;
        self.last_play = Some(t);
        Some(pcm)
    }

    pub fn buffered(&self) -> usize {
        self.frames.len()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_timeline_seconds() {
        assert!((rtp_secs(90_000, 90_000) - 1.0).abs() < 1e-9);
        assert!((rtp_secs(160, 8_000) - 0.02).abs() < 1e-9);
    }

    #[test]
    fn drift_tracks_audio_vs_video() {
        let mut s = AvSync::new();
        // 首帧各自归零（首帧对齐），漂移为 0
        s.on_audio(800, 8_000);
        s.on_video(90_000, 90_000);
        assert!((s.drift_secs() - 0.0).abs() < 1e-9);
        // 音频推进 2 帧（+0.04s），视频未动 → drift = +0.04（音频领先）
        s.on_audio(800 + 160, 8_000);
        s.on_audio(800 + 320, 8_000);
        assert!((s.drift_secs() - 0.04).abs() < 1e-9);
        // 视频追上一帧（+1/30 ≈ 0.0333s）→ drift ≈ +0.0067
        s.on_video(90_000 + 3_000, 90_000);
        assert!((s.drift_secs() - (0.04 - 0.0333333)).abs() < 1e-6);
    }

    #[test]
    fn jitter_buffer_pops_after_target_delay() {
        let mut jb = AudioJitterBuffer::new(0.08);
        jb.push(0.0, vec![0i16; 160]);
        jb.push(0.02, vec![1i16; 160]);
        assert_eq!(jb.pop(0.0), None, "not yet");
        assert_eq!(jb.pop(0.05), None, "still before target");
        let f = jb.pop(0.09).expect("first frame at 0+0.08");
        assert_eq!(f[0], 0);
        let f = jb.pop(0.11).expect("second frame at 0.02+0.08");
        assert_eq!(f[0], 1);
        assert_eq!(jb.buffered(), 0);
    }

    #[test]
    fn jitter_buffer_drops_late_frames() {
        let mut jb = AudioJitterBuffer::new(0.08);
        jb.push(0.0, vec![0i16; 160]);
        jb.push(0.02, vec![1i16; 160]);
        jb.push(0.05, vec![2i16; 160]);
        assert_eq!(jb.pop(0.20).map(|f| f[0]), Some(0));
        assert_eq!(jb.pop(0.20).map(|f| f[0]), Some(1));
        assert_eq!(jb.pop(0.20).map(|f| f[0]), Some(2)); // last_play=0.05
        // 迟到帧：时间轴 0.02 早于播放位置 0.05
        jb.push(0.02, vec![9i16; 160]);
        assert_eq!(jb.dropped(), 1);
        assert_eq!(jb.buffered(), 0);
    }
}
