//! FFmpeg 编解码器（#74）：H.265/HEVC + AV1 + VP9（含 H.264），
//! 有硬件编码器时优先硬编（macOS VideoToolbox），否则 FFmpeg 软编回退。

pub mod decode;
pub mod encode;

/// FFmpeg 版本探测（最小可用性检查）。
pub fn version() -> u32 {
    ffmpeg_next::util::version()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerodesk_core::media_pipeline::{Codec, EncodedUnit};
    use decode::FfmpegDecoder;
    use encode::FfmpegEncoder;

    fn rgb_frame(w: u32, h: u32, t: u64) -> Vec<u8> {
        let mut buf = vec![0u8; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 3) as usize;
                buf[i] = ((x * 255 / w) as u8).wrapping_add((t * 7) as u8);
                buf[i + 1] = ((y * 255 / h) as u8).wrapping_add((t * 11) as u8);
                buf[i + 2] = 128;
            }
        }
        buf
    }

    #[test]
    fn loopback_all_codecs() {
        for codec in [Codec::H264, Codec::Hevc, Codec::Vp9, Codec::Av1] {
            let mut enc = FfmpegEncoder::new(320, 180, 30, 800_000, codec).expect("encoder");
            let mut dec = FfmpegDecoder::new(codec).expect("decoder");
            let mut encoded: Vec<EncodedUnit> = Vec::new();
            for t in 0..120u64 {
                let rgb = rgb_frame(320, 180, t);
                if t == 6 {
                    enc.request_keyframe();
                }
                if let Some(unit) = enc.encode_rgb(&rgb).expect("encode") {
                    encoded.push(unit);
                }
            }
            let mut decoded = 0u32;
            for unit in &encoded {
                if let Some(f) = dec.decode_unit(unit).expect("decode") {
                    assert_eq!(f.width, 320, "codec {codec:?} width");
                    assert_eq!(f.height, 180, "codec {codec:?} height");
                    assert_eq!(
                        f.raw.as_ref().map(|r| r.len()).unwrap_or(0),
                        (320 * 180 * 4) as usize,
                        "codec {codec:?} raw size"
                    );
                    decoded += 1;
                }
            }
            eprintln!(
                "loopback {codec:?}: encoded={} decoded={decoded}",
                encoded.len()
            );
            assert!(decoded >= 1, "codec {codec:?} decoded 0 frames");
        }
    }
}
