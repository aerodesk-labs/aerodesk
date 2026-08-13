//! 观看端音频播放（#73）：实现已下沉 aerodesk-core（cpal 全平台），此处 re-export 保持兼容。

pub use aerodesk_core::audio_sink::{AudioSink, Resampler, apply_gain_f32, apply_gain_i16};
