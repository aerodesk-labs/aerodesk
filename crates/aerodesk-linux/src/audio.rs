//! Linux 系统音频采集（被控端，feature `pipewire`）。
//!
//! 经 PipeWire `stream.capture.sink` 捕获默认 sink 的系统输出（播放声音），
//! 无需 xdg-desktop-portal。实现 core `AudioCapturer`（单声道 f32，48kHz）。
//! 采集线程由 lamco-pipewire `spawn_audio_capture` 管理；`next_samples` 排空
//! channel 并做交错多声道 → 单声道降混。

use aerodesk_core::platform::AudioCapturer;

/// PipeWire 系统音频采集器（系统输出监听）。
pub struct SystemAudioCapture {
    handle: Option<lamco_pipewire::audio::AudioCaptureHandle>,
    channels: u32,
    /// 单声道 f32 待消费缓冲（上次未取完的样本）。
    mono: Vec<f32>,
}

impl SystemAudioCapture {
    /// 启动 PipeWire 系统音频捕获（默认 sink 输出；失败时返回 Err 由上层回退合成音）。
    pub fn new() -> Result<Self, String> {
        use lamco_pipewire::audio::{AudioFormat, CaptureConfig};
        let config = CaptureConfig {
            sample_rate: 48_000,
            channels: 2,
            format: AudioFormat::F32,
            buffer_frames: 1024,
        };
        let handle = lamco_pipewire::audio::spawn_audio_capture(config, None, 16)
            .map_err(|e| format!("pipewire audio capture: {e}"))?;
        Ok(Self {
            handle: Some(handle),
            channels: 2,
            mono: Vec::new(),
        })
    }
}

impl AudioCapturer for SystemAudioCapture {
    type Error = String;

    fn next_samples(&mut self, max: usize) -> Result<Vec<f32>, String> {
        // 排空 PipeWire channel → 交错 f32 → 单声道降混。
        if self.mono.is_empty() {
            let mut mono = Vec::new();
            if let Some(handle) = &mut self.handle {
                while let Ok(samples) = handle.receiver.try_recv() {
                    let f = samples.to_f32();
                    let ch = self.channels.max(1) as usize;
                    for frame in f.chunks(ch) {
                        mono.push(frame.iter().sum::<f32>() / ch as f32);
                    }
                }
            }
            self.mono = mono;
        }
        let take = self.mono.len().min(max);
        Ok(self.mono.drain(..take).collect())
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.stop();
        }
    }
}
