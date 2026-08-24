//! macOS 观看端：真实 H.264 解码渲染（#29）。
//!
//! 连接 `LiveSession` → `AccessUnitAssembler` 聚合成完整访问单元 →
//! VideoToolbox 硬解 → CVPixelBuffer → RGBA → 帧回调（desktop 为 Slint `Image`）。
//! 替换演示帧源；其余平台仍走演示帧（等各自解码管线接入）。
//!
//! #508 B1：UI 副作用经 [`crate::SessionUi`] 缝 + `on_frame` 帧回调回传；
//! `FILE_TRANSFER_ENABLED` 静态以引用注入，本模块不再引用 Slint/UI 类型。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::{Duration, Instant};

use aerodesk_core::access_unit::AccessUnitAssembler;
use aerodesk_core::connect::connect_live_role_with_camera;
use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::platform::Codec;
use aerodesk_core::protocol::cmd::CmdRequest;
use aerodesk_core::protocol::signal::Role;
use aerodesk_platform::macos::decode::{H264Decoder, HevcDecoder, to_rgba};
use str0m::net::Protocol;

use crate::{FileCmd, SessionUi};

/// 默认接收目录：~/Downloads/AeroDesk（不存在则创建）。
fn default_recv_dir() -> std::path::PathBuf {
    let dir = std::env::var("HOME")
        .map(|h| {
            std::path::PathBuf::from(h)
                .join("Downloads")
                .join("AeroDesk")
        })
        .unwrap_or_else(|_| std::env::temp_dir().join("AeroDesk"));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// #74 观看端多 codec 解码器：H264/H265 走 VideoToolbox 硬解（H265 无硬解
/// 时回退 FFmpeg），VP9/AV1 走 FFmpeg 软解。统一输出 RGBA。
enum UiDecoder {
    H264(H264Decoder),
    Hevc(HevcDecoder),
    Ffmpeg(aerodesk_codec::decode::FfmpegDecoder),
}

impl UiDecoder {
    fn for_codec(codec: Codec) -> Option<Self> {
        match codec {
            Codec::H264 => Some(UiDecoder::H264(H264Decoder::new())),
            Codec::Hevc if HevcDecoder::is_hardware_supported() => {
                Some(UiDecoder::Hevc(HevcDecoder::new()))
            }
            Codec::Hevc | Codec::Vp9 | Codec::Av1 => {
                aerodesk_codec::decode::FfmpegDecoder::new(codec)
                    .ok()
                    .map(UiDecoder::Ffmpeg)
            }
            _ => None,
        }
    }

    fn matches(&self, codec: Codec) -> bool {
        match self {
            UiDecoder::H264(_) => codec == Codec::H264,
            UiDecoder::Hevc(_) => codec == Codec::Hevc,
            // FfmpegDecoder 按单一 codec 打开：仅 codec 一致才可复用，
            // 否则 VP9↔AV1↔H265 切换会误用旧解码器解出新流（花屏/失败）。
            UiDecoder::Ffmpeg(d) => d.codec() == codec,
        }
    }

    fn decode_rgba(
        &mut self,
        codec: Codec,
        data: &[u8],
        pts: i64,
    ) -> Result<Option<(Vec<u8>, u32, u32)>, String> {
        match self {
            UiDecoder::H264(d) => d
                .decode_annexb(data, pts)
                .map(|pb| pb.and_then(|pb| to_rgba(&pb).map(|(r, w, h)| (r, w as u32, h as u32)))),
            UiDecoder::Hevc(d) => d
                .decode_annexb(data, pts)
                .map(|pb| pb.and_then(|pb| to_rgba(&pb).map(|(r, w, h)| (r, w as u32, h as u32)))),
            UiDecoder::Ffmpeg(d) => {
                let unit = aerodesk_core::platform::EncodedUnit {
                    data: data.to_vec(),
                    keyframe: false,
                    pts_ms: pts.max(0) as u64 / 1000,
                    rtp_timestamp: 0,
                };
                d.decode_unit(&unit)
                    .map(|f| f.and_then(|f| f.raw.map(|raw| (raw, f.width, f.height))))
            }
        }
    }
}

/// 核心 `Decoder` trait 实现：`UiDecoder` 已按 codec 收敛 H264/HEVC 硬解 +
/// FFmpeg 回退，直接对接 `EncodedUnit`（跨平台观看管线可泛型调用）。
impl aerodesk_core::platform::Decoder for UiDecoder {
    type Error = String;

    fn configure(&mut self, _codec: Codec, _width: u32, _height: u32) -> Result<(), Self::Error> {
        Ok(())
    }

    fn decode(
        &mut self,
        unit: &aerodesk_core::platform::EncodedUnit,
    ) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        let pts_us = unit.pts_ms.saturating_mul(1000) as i64;
        match self {
            UiDecoder::H264(d) => d
                .decode_annexb(&unit.data, pts_us)
                .map_err(|e| e.to_string())
                .map(|pb| {
                    pb.and_then(|pb| {
                        to_rgba(&pb).map(|(raw, w, h)| aerodesk_core::platform::VideoFrame {
                            platform: None,
                            handle: None,
                            raw: Some(raw),
                            width: w as u32,
                            height: h as u32,
                            pts_ms: unit.pts_ms,
                        })
                    })
                }),
            UiDecoder::Hevc(d) => d
                .decode_annexb(&unit.data, pts_us)
                .map_err(|e| e.to_string())
                .map(|pb| {
                    pb.and_then(|pb| {
                        to_rgba(&pb).map(|(raw, w, h)| aerodesk_core::platform::VideoFrame {
                            platform: None,
                            handle: None,
                            raw: Some(raw),
                            width: w as u32,
                            height: h as u32,
                            pts_ms: unit.pts_ms,
                        })
                    })
                }),
            UiDecoder::Ffmpeg(d) => d.decode_unit(unit).map_err(|e| e.to_string()),
        }
    }
}

/// 状态栏 codec 显示名（首包前未知 → H.264 兼容占位，收到首包后更新）。
/// 系统通知（#277 `Notifier` trait 的 macOS 消费入口；非 macOS no-op）。
fn notify_user(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        use aerodesk_core::platform::Notifier;
        aerodesk_platform::macos::notifier::MacNotifier.notify(title, body);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, body);
    }
}

fn codec_label(codec: Option<Codec>) -> &'static str {
    match codec {
        Some(Codec::Hevc) => "H.265",
        Some(Codec::Vp9) => "VP9",
        Some(Codec::Av1) => "AV1",
        _ => "H.264",
    }
}

/// 运行 macOS 观看会话（阻塞直到断开/代际失效）。
///
/// - `ui`：会话事件缝（desktop 为 Slint 适配器；槽位语义含在实现内）。
/// - `file_transfer_enabled`：#72 文件传输总开关（desktop 静态的引用注入）。
/// - `on_frame`：解码出 RGBA 帧后的呈现回调（desktop 映射到会话帧槽）。
#[cfg(test)]
mod tests {
    use super::*;
    use aerodesk_core::synthetic::SyntheticSource;
    use aerodesk_platform::macos::decode::to_rgba;
    use aerodesk_platform::macos::vt_encoder::VtEncoder;

    /// 按 AnnexB 起始码拆分 NAL（保留起始码，模拟 str0m 的 `Output::Media` AnnexB 输出）。
    fn split_annexb_nalus(annexb: &[u8]) -> Vec<&[u8]> {
        // 记录所有起始码（位置 + 长度；4 字节优先，避免被误判为 3 字节）。
        let mut codes: Vec<(usize, usize)> = Vec::new();
        let mut i = 0usize;
        while i + 3 <= annexb.len() {
            if i + 4 <= annexb.len()
                && annexb[i] == 0
                && annexb[i + 1] == 0
                && annexb[i + 2] == 0
                && annexb[i + 3] == 1
            {
                codes.push((i, 4));
                i += 4;
            } else if annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 1 {
                codes.push((i, 3));
                i += 3;
            } else {
                i += 1;
            }
        }
        codes
            .iter()
            .enumerate()
            .map(|(k, &(code, _))| {
                // 返回含起始码的完整 NAL（str0m AnnexB 输出格式），供 assembler 拼接。
                let payload_end = codes.get(k + 1).map(|&(c, _)| c).unwrap_or(annexb.len());
                &annexb[code..payload_end]
            })
            .collect()
    }

    /// 桌面 UI 观看链路（无网络）：VT 编码 → to_annexb（完整 AU）→ 拆成
    /// NAL 事件 → AccessUnitAssembler 重组 → H264Decoder 解码 → RGBA。
    /// 与 `run_viewer` 的媒体路径完全一致（#29 真实解码）。
    #[test]
    fn desktop_ui_decode_chain_roundtrip() {
        let (w, h) = (320u32, 180u32);
        let mut enc = VtEncoder::new(w, h, 30, 1_000_000).expect("vt encoder");
        let mut src = SyntheticSource::new(w, h);
        let mut assembler = AccessUnitAssembler::new();
        let mut decoder = H264Decoder::new();

        let mut decoded = None;
        for i in 0..12u32 {
            let frame = enc
                .encode_bgra(&src.next_frame_bgra())
                .expect("encode")
                .expect("frame");
            let au = enc.to_annexb(&frame);
            let pts_us = i as u64 * 33_333; // ~30fps
            // 模拟 str0m 逐条 NAL 事件：同 pts 聚合为完整访问单元后整帧解码。
            for nal in split_annexb_nalus(&au) {
                if let Some(complete) = assembler.push(nal, pts_us, false)
                    && let Ok(Some(buf)) =
                        decoder.decode_annexb(&complete.data, complete.pts_us as i64)
                    && let Some((rgba, dw, dh)) = to_rgba(&buf)
                {
                    decoded = Some((rgba, dw, dh));
                }
            }
            if decoded.is_some() {
                break;
            }
        }
        let (rgba, dw, dh) = decoded.expect("应在若干帧内解码出 RGBA");
        assert_eq!((dw, dh), (w as usize, h as usize));
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert!(
            rgba.chunks_exact(4).any(|p| p[3] == 255),
            "alpha 应全不透明"
        );
    }

    /// #277 跨平台抽象：泛型消费者只依赖 core `Decoder` trait 即可解码。
    #[test]
    fn generic_decoder_trait_drives_macos_decoder() {
        fn count_frames<D: aerodesk_core::platform::Decoder>(
            dec: &mut D,
            units: &[aerodesk_core::platform::EncodedUnit],
        ) -> usize {
            let mut n = 0;
            for u in units {
                if let Ok(Some(_)) = dec.decode(u) {
                    n += 1;
                }
            }
            n
        }

        use aerodesk_codec::encode::FfmpegEncoder;
        for codec in [Codec::H264, Codec::Hevc] {
            let mut enc = FfmpegEncoder::new(320, 180, 30, 1_000_000, codec).expect("encoder");
            enc.request_keyframe();
            let mut dec = UiDecoder::for_codec(codec).expect("decoder");
            let mut frame = vec![0u8; 320 * 180 * 4];
            let mut units = Vec::new();
            for i in 0..8u32 {
                for (j, px) in frame.iter_mut().enumerate() {
                    *px = (i * 30 + j as u32 / 100) as u8;
                }
                if let Some(u) = enc.encode_bgra(&frame).expect("encode") {
                    units.push(u);
                }
            }
            let n = count_frames(&mut dec, &units);
            assert!(n >= 1, "{codec:?} 泛型 Decoder 应解出帧，got {n}");
        }
    }

    /// #277 观看端泛型链路：`Decoder + Renderer` trait 驱动解码并渲染。
    #[test]
    fn generic_decoder_renderer_chain() {
        struct CountingRenderer {
            frames: usize,
        }
        impl aerodesk_core::platform::Renderer for CountingRenderer {
            type Error = String;
            fn render(
                &mut self,
                frame: &aerodesk_core::platform::VideoFrame,
            ) -> Result<(), Self::Error> {
                assert!(!frame.raw.as_deref().unwrap_or_default().is_empty());
                self.frames += 1;
                Ok(())
            }
        }

        fn pump<D: aerodesk_core::platform::Decoder, R: aerodesk_core::platform::Renderer>(
            dec: &mut D,
            ren: &mut R,
            units: &[aerodesk_core::platform::EncodedUnit],
        ) -> usize {
            let mut rendered = 0;
            for u in units {
                if let Ok(Some(frame)) = dec.decode(u) {
                    if ren.render(&frame).is_ok() {
                        rendered += 1;
                    }
                }
            }
            rendered
        }

        use aerodesk_codec::encode::FfmpegEncoder;
        for codec in [Codec::H264, Codec::Hevc] {
            let mut enc = FfmpegEncoder::new(320, 180, 30, 1_000_000, codec).expect("encoder");
            enc.request_keyframe();
            let mut dec = UiDecoder::for_codec(codec).expect("decoder");
            let mut ren = CountingRenderer { frames: 0 };
            let mut frame = vec![0u8; 320 * 180 * 4];
            let mut units = Vec::new();
            for i in 0..8u32 {
                for (j, px) in frame.iter_mut().enumerate() {
                    *px = (i * 30 + (j as u32 / 100)) as u8;
                }
                if let Some(u) = enc.encode_bgra(&frame).expect("encode") {
                    units.push(u);
                }
            }
            let n = pump(&mut dec, &mut ren, &units);
            assert!(n >= 1, "{codec:?} 泛型 Decoder+Renderer 应渲染，got {n}");
        }
    }

    /// 状态栏 codec 显示名与协商 codec 一致（H.265 不再误显示 H.264）。
    #[test]
    fn codec_label_matches_negotiated() {
        assert_eq!(codec_label(None), "H.264");
        assert_eq!(codec_label(Some(Codec::H264)), "H.264");
        assert_eq!(codec_label(Some(Codec::Hevc)), "H.265");
        assert_eq!(codec_label(Some(Codec::Vp9)), "VP9");
        assert_eq!(codec_label(Some(Codec::Av1)), "AV1");
    }

    /// #74 UI 解码器（硬解优先 + FFmpeg 回退）对全部 codec 回环出 RGBA。
    #[test]
    fn ui_decoder_decodes_all_codecs() {
        use aerodesk_codec::encode::FfmpegEncoder;

        let (w, h) = (320u32, 180u32);
        for codec in [Codec::H264, Codec::Hevc, Codec::Vp9, Codec::Av1] {
            let mut enc = FfmpegEncoder::new(w, h, 30, 1_000_000, codec).expect("encoder");
            enc.request_keyframe();
            let mut dec = UiDecoder::for_codec(codec).expect("decoder");
            let mut ok = false;
            for i in 0..80u32 {
                let bgra: Vec<u8> = (0..(w * h * 4) as usize)
                    .map(|j| ((i * 7 + (j as u32) / 4) & 0xff) as u8)
                    .collect();
                let Some(unit) = enc.encode_bgra(&bgra).expect("encode") else {
                    continue;
                };
                if let Ok(Some((rgba, dw, dh))) = dec.decode_rgba(codec, &unit.data, 0) {
                    assert_eq!((dw, dh), (w, h));
                    assert_eq!(rgba.len(), (w * h * 4) as usize);
                    ok = true;
                    break;
                }
            }
            assert!(ok, "{codec:?} 应解出 RGBA");
        }
    }

    #[test]
    fn split_annexb_nalus_works() {
        // 3 字节与 4 字节起始码都要识别。
        let data: Vec<u8> = [
            &[0u8, 0, 0, 1, 0x67, 0x01][..],
            &[0, 0, 0, 1, 0x65, 0x02][..],
            &[0, 0, 1, 0x41, 0x03][..],
        ]
        .concat();
        let nals = split_annexb_nalus(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0, 0, 0, 1, 0x67, 0x01]);
        assert_eq!(nals[1], &[0, 0, 0, 1, 0x65, 0x02]);
        assert_eq!(nals[2], &[0, 0, 1, 0x41, 0x03]);
    }
}
