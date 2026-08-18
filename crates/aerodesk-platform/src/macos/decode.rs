//! VideoToolbox 硬解（H.264/HEVC）：实现已收敛至 `crate::apple::vt_decode`
//! （#506，macOS/iOS 共享单实现）；本模块 re-export 保持既有调用路径稳定。

pub use crate::apple::vt_decode::{H264Decoder, HevcDecoder, to_rgba, yuv_to_rgb};

#[cfg(test)]
mod tests {
    use super::{H264Decoder, HevcDecoder, to_rgba};

    /// H.265 硬解回环：#74 FFmpeg(x265) 软编 → HevcDecoder(VT 硬解) → RGBA。
    #[test]
    fn hevc_vt_decodes_ffmpeg_annexb() {
        use crate::macos::synthetic::SyntheticSource;
        use aerodesk_codec::encode::FfmpegEncoder;
        use aerodesk_core::media_pipeline::Codec;

        if !HevcDecoder::is_hardware_supported() {
            eprintln!("skip: 本机无 HEVC 硬解");
            return;
        }
        let (w, h) = (320u32, 180u32);
        let mut enc = FfmpegEncoder::new(w, h, 30, 1_000_000, Codec::Hevc).expect("x265 encoder");
        enc.request_keyframe();
        let mut src = SyntheticSource::new(w, h);
        let mut decoder = HevcDecoder::new();
        let mut decoded = None;
        for _ in 0..40 {
            let Some(unit) = enc.encode_bgra(&src.next_frame_bgra()).expect("encode") else {
                continue; // x265 内部缓冲，等包产出
            };
            if let Ok(Some(buf)) = decoder.decode_annexb(&unit.data, 0) {
                decoded = Some(buf);
                break;
            }
        }
        let buf = decoded.expect("应在若干帧内解出 HEVC 像素缓冲");
        let (rgba, dw, dh) = to_rgba(&buf).expect("rgba 转换");
        assert_eq!((dw, dh), (w as usize, h as usize));
        assert!(
            rgba.chunks_exact(4).any(|p| p[3] == 255),
            "alpha 应全不透明"
        );
    }

    /// 编码→解码→RGBA 回环：验证 #29 macOS 真实解码渲染链。
    #[test]
    fn vt_encode_decode_roundtrip_to_rgba() {
        use crate::macos::synthetic::SyntheticSource;
        use crate::macos::vt_encoder::{VtEncoder, avcc_to_annexb};

        let (w, h) = (320u32, 180u32);
        let mut enc = VtEncoder::new(w, h, 30, 1_000_000).expect("vt encoder");
        let mut src = SyntheticSource::new(w, h);

        let mut decoded = None;
        for _ in 0..12 {
            let frame = enc
                .encode_bgra(&src.next_frame_bgra())
                .expect("encode")
                .expect("frame");
            let annexb = enc.to_annexb(&frame);
            if let Ok(Some(buf)) = H264Decoder::new().decode_annexb(&annexb, 0) {
                decoded = Some(buf);
                break;
            }
        }
        let buf = decoded.expect("应在若干帧内解码出像素缓冲");
        let (rgba, dw, dh) = to_rgba(&buf).expect("rgba 转换");
        assert_eq!(dw, w as usize);
        assert_eq!(dh, h as usize);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert!(
            rgba.chunks_exact(4).any(|p| p[3] == 255),
            "alpha 应全不透明"
        );
    }
}
