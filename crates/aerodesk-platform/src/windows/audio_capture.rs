//! WASAPI loopback 系统音频采集（被控端：采集系统正在播放的声音）。
//!
//! 输出单声道 f32 样本，**统一重采样到 48 kHz**（与 core `AudioCapturer` 约定一致；
//! 设备混音器速率随端点变化——44.1k USB DAC / 96k 设备并不少见，发送侧按 48k
//! 模型硬编码，采集侧负责把任意设备速率重采样到 48k）。
//! 无音频输出设备/无交互会话时 start() 返回 Err，调用方回退合成音。

use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::core::GUID;

/// windows 0.58 未生成 CLSID_MMDeviceEnumerator / KSDATAFORMAT_SUBTYPE_IEEE_FLOAT，手动声明。
const CLSID_MMDEVICE_ENUMERATOR: GUID = GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// 设备速率 → 48 kHz 线性插值重采样器（f32 单声道）。
///
/// 跨调用保持相位（`pos` 为绝对输入位置）；缓冲上限防"采集远快于消费"时
/// 无界增长（超出丢弃最旧段，代价是短暂不连续，仅异常场景触发）。
struct LinearResampler48 {
    rate: u32,
    buf: Vec<f32>,
    pos: f64,
}

impl LinearResampler48 {
    /// 缓冲上限：96k 速率下 1s 的输入样本量。
    const MAX_BUF: usize = 96_000;

    /// `rate` 为 0 时按 48k 直通（防御：解析失败不应导致除零）。
    fn new(rate: u32) -> Self {
        Self {
            rate: if rate == 0 { 48_000 } else { rate },
            buf: Vec::new(),
            pos: 0.0,
        }
    }

    /// 追加设备速率样本（含上限裁切）。
    fn push(&mut self, samples: &[f32]) {
        self.buf.extend_from_slice(samples);
        if self.buf.len() > Self::MAX_BUF {
            let cut = self.buf.len() - Self::MAX_BUF;
            self.buf.drain(..cut);
            self.pos = (self.pos - cut as f64).max(0.0);
        }
    }

    /// 拉取最多 `max` 个 48 kHz 样本；数据不足时返回现有可产出的量。
    /// 缺插值尾样本（i+1）即停、不推进——分段拉取与一次性拉取结果逐样本一致，
    /// 且流末尾只保留最后一个输入样本不单独输出（下一包到达即被使用）。
    fn pull(&mut self, max: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(max.min(48_000));
        let ratio = self.rate as f64 / 48_000.0;
        while out.len() < max && self.pos + 1.0 < self.buf.len() as f64 {
            let i = self.pos.floor() as usize;
            let frac = (self.pos - i as f64) as f32;
            let s0 = self.buf[i];
            let s1 = self.buf[i + 1];
            out.push(s0 + (s1 - s0) * frac);
            self.pos += ratio;
        }
        out
    }
}

/// WASAPI 回环采集器（渲染端点 loopback：采集系统播放的音频）。
pub struct WasapiLoopbackCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    channels: u16,
    bits_per_sample: u16,
    block_align: u16,
    float_samples: bool,
    resampler: LinearResampler48,
}

impl WasapiLoopbackCapture {
    /// 启动回环采集（失败返回 Err，调用方回退合成音）。
    pub fn start() -> Result<Self, String> {
        unsafe {
            // 已在 MTA 时返回 RPC_E_CHANGED_MODE，可忽略（WASAPI 仍可用）。
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&CLSID_MMDEVICE_ENUMERATOR, None, CLSCTX_ALL)
                    .map_err(|e| format!("CoCreateInstance enumerator: {e}"))?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| format!("GetDefaultAudioEndpoint: {e}"))?;
            let client: IAudioClient = device
                .Activate::<IAudioClient>(CLSCTX_ALL, None)
                .map_err(|e| format!("Activate IAudioClient: {e}"))?;
            let mix = client
                .GetMixFormat()
                .map_err(|e| format!("GetMixFormat: {e}"))?;
            let (channels, bits, block_align, float_samples, sample_rate) = parse_mix_format(mix);
            // 100ms 缓冲；共享模式回环流。pFormat 在 Initialize 后即可释放。
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    100 * 10_000,
                    0,
                    mix,
                    None,
                )
                .map_err(|e| format!("Initialize: {e}"))?;
            CoTaskMemFree(Some(mix as *const _));
            let capture: IAudioCaptureClient = client
                .GetService()
                .map_err(|e| format!("GetService IAudioCaptureClient: {e}"))?;
            client.Start().map_err(|e| format!("Start: {e}"))?;
            Ok(Self {
                client,
                capture,
                channels,
                bits_per_sample: bits,
                block_align,
                float_samples,
                resampler: LinearResampler48::new(sample_rate),
            })
        }
    }

    /// 排空当前所有可用包并重采样，返回最多 `max` 个 48 kHz 单声道 f32 样本。
    pub fn next_samples(&mut self, max: usize) -> Vec<f32> {
        unsafe {
            loop {
                let packet = match self.capture.GetNextPacketSize() {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if packet == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if self
                    .capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .is_err()
                {
                    break;
                }
                let bytes = frames as usize * self.block_align as usize;
                let src = std::slice::from_raw_parts(data, bytes);
                let ch = self.channels.max(1) as usize;
                let frame_bytes = self.block_align.max(2) as usize;
                let frame_count = bytes / frame_bytes;
                let sample_bytes = (self.bits_per_sample as usize / 8).max(1);
                let mut mono = Vec::with_capacity(frame_count);
                for f in 0..frame_count {
                    let base = f * frame_bytes;
                    let mut acc = 0.0f64;
                    for c in 0..ch {
                        let off = base + c * sample_bytes;
                        if off + sample_bytes > src.len() {
                            continue;
                        }
                        let v = if self.float_samples && self.bits_per_sample == 32 {
                            f32::from_le_bytes(src[off..off + 4].try_into().unwrap()) as f64
                        } else if self.bits_per_sample == 16 {
                            i16::from_le_bytes(src[off..off + 2].try_into().unwrap()) as f64
                                / 32768.0
                        } else if self.bits_per_sample == 24 {
                            let raw = (src[off] as i32)
                                | ((src[off + 1] as i32) << 8)
                                | ((src[off + 2] as i32) << 16);
                            (raw << 8 >> 8) as f64 / 8_388_608.0
                        } else if self.bits_per_sample == 32 && !self.float_samples {
                            i32::from_le_bytes(src[off..off + 4].try_into().unwrap()) as f64
                                / 2_147_483_648.0
                        } else {
                            0.0
                        };
                        acc += v;
                    }
                    mono.push((acc / ch as f64) as f32);
                }
                let _ = self.capture.ReleaseBuffer(frames);
                self.resampler.push(&mono);
            }
        }
        self.resampler.pull(max)
    }
}

impl Drop for WasapiLoopbackCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

impl aerodesk_core::platform::AudioCapturer for WasapiLoopbackCapture {
    type Error = String;

    fn next_samples(&mut self, max: usize) -> Result<Vec<f32>, Self::Error> {
        Ok(WasapiLoopbackCapture::next_samples(self, max))
    }
}

/// 解析 GetMixFormat 的 WAVEFORMATEX / WAVEFORMATEXTENSIBLE，返回
/// (声道, 位深, 块对齐, 是否浮点, 采样率)。采样率是设备端点决定的——
/// 44.1k/96k 设备常见，采集侧据此重采样到 48k（见 LinearResampler48）。
unsafe fn parse_mix_format(mix: *const WAVEFORMATEX) -> (u16, u16, u16, bool, u32) {
    // SAFETY: mix 来自 GetMixFormat，有效期至 CoTaskMemFree；调用点保证指针有效。
    let wfx = unsafe { &*mix };
    let mut channels = wfx.nChannels;
    let mut bits = wfx.wBitsPerSample;
    let mut block_align = wfx.nBlockAlign;
    let mut sample_rate = wfx.nSamplesPerSec;
    let mut float_samples = false;
    if wfx.wFormatTag == 0xFFFE {
        // WAVEFORMATEXTENSIBLE：SubFormat 决定采样类型。
        // packed struct：addr_of! 取地址 + read_unaligned（避免未对齐引用）。
        let ext = unsafe { &*(mix as *const windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE) };
        let subformat = unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(ext.SubFormat)) };
        channels = unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(ext.Format.nChannels)) };
        bits = unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(ext.Format.wBitsPerSample)) };
        block_align =
            unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(ext.Format.nBlockAlign)) };
        sample_rate =
            unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(ext.Format.nSamplesPerSec)) };
        if subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            float_samples = true;
        } else if subformat == KSDATAFORMAT_SUBTYPE_PCM {
            float_samples = false;
        }
    } else if wfx.wFormatTag == 3 {
        // WAVE_FORMAT_IEEE_FLOAT
        float_samples = true;
    }
    (channels, bits, block_align, float_samples, sample_rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WASAPI 回环采集运行级验证：启动 + 排空样本（无音频设备/无交互会话时 SKIP）。
    #[test]
    fn wasapi_loopback_starts_and_drains() {
        let mut cap = match WasapiLoopbackCapture::start() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: WASAPI init failed: {e}");
                return;
            }
        };
        let samples = cap.next_samples(48_000);
        eprintln!("wasapi loopback drained {} samples", samples.len());
        assert!(samples.len() <= 48_000, "最多排空 max 个样本");
    }

    /// 440Hz 单音（rate 采样率、seconds 秒）。
    fn tone(rate: u32, freq: f32, seconds: f32) -> Vec<f32> {
        (0..(rate as f32 * seconds) as usize)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    /// 上升过零计数估算频率：freq = rising / duration（不依赖 FFT，误差 ~1 周期）。
    fn estimate_freq(samples: &[f32], rate: u32) -> f32 {
        let rising = samples
            .windows(2)
            .filter(|w| w[0] <= 0.0 && w[1] > 0.0)
            .count();
        rising as f32 * rate as f32 / samples.len() as f32
    }

    /// 普通 WAVEFORMATEX（PCM16）的 nSamplesPerSec 应被读出。
    #[test]
    fn parse_mix_format_reads_rate_pcm16_44100() {
        let wfx = WAVEFORMATEX {
            wFormatTag: 1, // WAVE_FORMAT_PCM
            nChannels: 2,
            nSamplesPerSec: 44_100,
            nAvgBytesPerSec: 176_400,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let (ch, bits, align, float, rate) = unsafe { parse_mix_format(&wfx as *const _) };
        assert_eq!((ch, bits, align, float, rate), (2, 16, 4, false, 44_100));
    }

    /// WAVEFORMATEXTENSIBLE（IEEE float）的 Format.nSamplesPerSec 应被读出。
    #[test]
    fn parse_mix_format_reads_rate_extensible_float_96000() {
        #[repr(C, packed(1))]
        struct Ext {
            format: WAVEFORMATEX,
            samples: u16, // union WORD
            channel_mask: u32,
            sub_format: GUID,
        }
        let ext = Ext {
            format: WAVEFORMATEX {
                wFormatTag: 0xFFFE, // WAVE_FORMAT_EXTENSIBLE
                nChannels: 2,
                nSamplesPerSec: 96_000,
                nAvgBytesPerSec: 768_000,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            samples: 32,
            channel_mask: 3,
            sub_format: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };
        let (ch, bits, align, float, rate) =
            unsafe { parse_mix_format(&ext as *const _ as *const WAVEFORMATEX) };
        assert_eq!((ch, bits, align, float, rate), (2, 32, 8, true, 96_000));
    }

    /// A1 验收：44.1 kHz 设备的 440Hz 单音重采样到 48k 后频率不变（±2%）。
    #[test]
    fn resampler_44100_tone_keeps_frequency() {
        let mut r = LinearResampler48::new(44_100);
        r.push(&tone(44_100, 440.0, 1.0));
        let out = r.pull(48_000);
        let freq = estimate_freq(&out, 48_000);
        assert!(
            (freq - 440.0).abs() < 440.0 * 0.02,
            "44.1k→48k 重采样后频率 {freq} 偏离 440Hz"
        );
    }

    /// A1 验收：96 kHz 设备重采样到 48k 后频率不变、样本量约减半。
    #[test]
    fn resampler_96000_halves_and_keeps_frequency() {
        let mut r = LinearResampler48::new(96_000);
        r.push(&tone(96_000, 440.0, 1.0));
        let out = r.pull(48_000);
        assert!(
            (out.len() as i64 - 48_000).abs() <= 1,
            "96k 1s 应产出约 48k 样本"
        );
        let freq = estimate_freq(&out, 48_000);
        assert!(
            (freq - 440.0).abs() < 440.0 * 0.02,
            "96k→48k 重采样后频率 {freq} 偏离 440Hz"
        );
    }

    /// 相位连续性：分段 push/pull 与一次性 push/pull 结果逐样本一致
    /// （流式重采样必须可精确续接，否则每次取包边界都有咔哒声）。
    #[test]
    fn resampler_phase_continuity_across_pulls() {
        let src = tone(44_100, 440.0, 0.5);
        let mut one_shot = LinearResampler48::new(44_100);
        one_shot.push(&src);
        let full = one_shot.pull(usize::MAX);

        let mut split = LinearResampler48::new(44_100);
        let mid = src.len() / 2;
        split.push(&src[..mid]);
        let p1 = split.pull(usize::MAX);
        split.push(&src[mid..]);
        let p2 = split.pull(usize::MAX);
        let concat: Vec<f32> = p1.iter().chain(p2.iter()).copied().collect();

        assert_eq!(full.len(), concat.len(), "分段与一次性产出长度一致");
        for (i, (a, b)) in full.iter().zip(concat.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "样本 {i} 不一致：{a} vs {b}");
        }
    }

    /// 缓冲上限：消费停滞时输入缓冲不无界增长（96k 下最多 1s 量）。
    #[test]
    fn resampler_buffer_capped() {
        let mut r = LinearResampler48::new(96_000);
        r.push(&vec![0.5f32; 200_000]);
        assert!(
            r.buf.len() <= LinearResampler48::MAX_BUF,
            "缓冲应被裁切到上限"
        );
        let out = r.pull(usize::MAX);
        assert!(!out.is_empty(), "裁切后仍可产出样本");
    }

    /// 防御：rate=0（解析异常）按 48k 直通，不除零。
    #[test]
    fn resampler_zero_rate_passthrough() {
        let mut r = LinearResampler48::new(0);
        let src: Vec<f32> = (0..100).map(|i| i as f32 * 0.01).collect();
        r.push(&src);
        let out = r.pull(usize::MAX);
        assert_eq!(out.len(), src.len() - 1, "直通保留最后一个尾样本");
        for (a, b) in out.iter().zip(src.iter()) {
            assert_eq!(a, b);
        }
    }
}
