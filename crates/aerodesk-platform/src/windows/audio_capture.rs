//! WASAPI loopback 系统音频采集（被控端：采集系统正在播放的声音）。
//!
//! 输出单声道 f32 样本（与 core `AudioCapturer` 约定一致；48kHz 由
//! 系统混音器决定，CLI 侧按 RealAudioSender 的 48k→8k 降采样逻辑处理）。
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

/// WASAPI 回环采集器（渲染端点 loopback：采集系统播放的音频）。
pub struct WasapiLoopbackCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    channels: u16,
    bits_per_sample: u16,
    block_align: u16,
    float_samples: bool,
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
            let (channels, bits, block_align, float_samples) = parse_mix_format(mix);
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
            })
        }
    }

    /// 排空当前所有可用包，返回最多 `max` 个单声道 f32 样本。
    pub fn next_samples(&mut self, max: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(max.min(48_000));
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
                    out.push((acc / ch as f64) as f32);
                    if out.len() >= max {
                        break;
                    }
                }
                let _ = self.capture.ReleaseBuffer(frames);
                if out.len() >= max {
                    break;
                }
            }
        }
        out
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

/// 解析 GetMixFormat 的 WAVEFORMATEX / WAVEFORMATEXTENSIBLE。
unsafe fn parse_mix_format(mix: *const WAVEFORMATEX) -> (u16, u16, u16, bool) {
    // SAFETY: mix 来自 GetMixFormat，有效期至 CoTaskMemFree；调用点保证指针有效。
    let wfx = unsafe { &*mix };
    let mut channels = wfx.nChannels;
    let mut bits = wfx.wBitsPerSample;
    let mut block_align = wfx.nBlockAlign;
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
        if subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            float_samples = true;
        } else if subformat == KSDATAFORMAT_SUBTYPE_PCM {
            float_samples = false;
        }
    } else if wfx.wFormatTag == 3 {
        // WAVE_FORMAT_IEEE_FLOAT
        float_samples = true;
    }
    (channels, bits, block_align, float_samples)
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
}
