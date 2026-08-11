//! 系统音频采集（ScreenCaptureKit SCStream audio-only）。
//!
//! 视频路径用 SCScreenshotManager 轮询（macOS 26 上 SCStream 视频会返回空白帧），
//! 音频单独开一条 audio-only SCStream——没有视频帧输出，不受该问题影响。
//!
//! 输出 48kHz 单声道 f32（SCK 默认 Float32 48k 立体声，这里做下混）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use screencapturekit::prelude::*;
use screencapturekit::stream::output_type::SCStreamOutputType;

/// 系统音频采集器（被控端：把机器正在播放的声音送出去）。
pub struct SystemAudioCapture {
    stream: SCStream,
    /// 单声道 f32 样本缓冲（handler 追加，主线程排空）。
    buf: Arc<Mutex<Vec<f32>>>,
    /// 已采集样本总数（诊断）。
    total: Arc<std::sync::atomic::AtomicU64>,
    stop: Arc<AtomicBool>,
}

impl SystemAudioCapture {
    /// 启动音频采集（失败返回 Err，调用方回退合成音）。
    pub fn start() -> Result<Self, String> {
        let content = SCShareableContent::get().map_err(|e| format!("SCK content: {e}"))?;
        let displays = content.displays();
        let display = displays.first().ok_or("no display")?;
        let filter = SCContentFilter::create().with_display(display).build();

        let mut config = SCStreamConfiguration::default();
        config.set_captures_audio(true);
        // SCK 默认 48kHz 立体声 Float32；保持默认，提取时下混。

        let buf: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let total = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let buf2 = buf.clone();
        let total2 = total.clone();

        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(
            move |sample: screencapturekit::cm::CMSampleBuffer, of_type: SCStreamOutputType| {
                if of_type != SCStreamOutputType::Audio {
                    return;
                }
                let Some(fd) = sample.format_description() else {
                    return;
                };
                let bytes_per_frame = fd.audio_bytes_per_frame().unwrap_or(4).max(2) as usize;
                let Some(abl) = sample.audio_buffer_list() else {
                    return;
                };
                // 兼容交错（1 buffer × N 声道）与非交错（N buffer × 1 声道）。
                let mut mono = Vec::new();
                for ab in abl.iter() {
                    let data = ab.data();
                    let ch = ab.number_channels.max(1) as usize;
                    let frames = data.len() / bytes_per_frame;
                    if ch > 1 {
                        // 交错：每帧取各声道平均。
                        let frame_count = frames / ch;
                        for i in 0..frame_count {
                            let mut acc = 0.0f32;
                            for c in 0..ch {
                                let off = (i * ch + c) * bytes_per_frame;
                                acc += f32::from_le_bytes([
                                    data[off],
                                    data[off + 1],
                                    data[off + 2],
                                    data[off + 3],
                                ]);
                            }
                            mono.push(acc / ch as f32);
                        }
                    } else {
                        // 非交错：直接追加本声道样本。
                        for i in 0..frames {
                            let off = i * bytes_per_frame;
                            mono.push(f32::from_le_bytes([
                                data[off],
                                data[off + 1],
                                data[off + 2],
                                data[off + 3],
                            ]));
                        }
                    }
                }
                if !mono.is_empty() {
                    total2.fetch_add(mono.len() as u64, Ordering::Relaxed);
                    if let Ok(mut b) = buf2.lock() {
                        b.extend_from_slice(&mono);
                        // 上限 2s（48k×2），防 handler 比消费快导致无限增长。
                        const MAX: usize = 96_000;
                        if b.len() > MAX {
                            let cut = b.len() - MAX;
                            b.drain(..cut);
                        }
                    }
                }
            },
            SCStreamOutputType::Audio,
        );
        stream
            .start_capture()
            .map_err(|e| format!("sc stream start: {e}"))?;
        Ok(Self {
            stream,
            buf,
            total,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 排空已采集样本（单声道 f32）。
    pub fn take_samples(&self, max: usize) -> Vec<f32> {
        let mut b = match self.buf.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let n = b.len().min(max);
        b.drain(..n).collect()
    }

    /// 已采集样本总数（诊断/自愈判断）。
    pub fn total_samples(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.stream.stop_capture();
    }
}

/// 核心 `AudioCapturer` 实现（被控端系统音频采集，单声道 f32）。
impl aerodesk_core::platform::AudioCapturer for SystemAudioCapture {
    type Error = String;

    fn next_samples(&mut self, max: usize) -> Result<Vec<f32>, Self::Error> {
        Ok(self.take_samples(max))
    }
}
