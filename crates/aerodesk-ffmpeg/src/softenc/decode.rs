//! 共享软解：OpenH264 H.264 解码器（Cisco BSD 许可，全平台可编译）。
//!
//! 供 Windows/Linux 适配器作为硬件解码（DXVA2/VAAPI）的回退路径（#3/#4），
//! 也作为 macOS/iOS/Android 硬解不可用时的兜底。

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

/// H.264 软件解码器（AnnexB 输入 → RGBA 输出）。
pub struct SoftDecoder {
    decoder: Decoder,
}

impl SoftDecoder {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            decoder: Decoder::new().map_err(|e| format!("openh264 init: {e}"))?,
        })
    }

    /// 解码一帧/多帧 AnnexB H.264，返回最新帧 RGBA + 尺寸。
    pub fn decode_rgba(
        &mut self,
        annexb: &[u8],
    ) -> Result<Option<(Vec<u8>, usize, usize)>, String> {
        match self.decoder.decode(annexb) {
            Ok(Some(yuv)) => {
                let (w, h) = yuv.dimensions();
                if w == 0 || h == 0 {
                    return Ok(None);
                }
                let mut rgba = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut rgba);
                Ok(Some((rgba, w, h)))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(format!("openh264 decode: {e}")),
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use crate::softenc::encode::X264Encoder;

    /// x264 编码 → OpenH264 解码 → RGBA 回环（#3/#4 软解验证）。
    #[test]
    fn x264_encode_openh264_decode_roundtrip() {
        let (w, h) = (320u32, 180u32);
        let mut enc = X264Encoder::new(w, h, 30, 500).expect("x264");
        let mut dec = SoftDecoder::new().expect("openh264");

        let mut frame = vec![0u8; (w * h * 3) as usize];
        let mut decoded = None;
        for i in 0..12u8 {
            for (j, px) in frame.iter_mut().enumerate() {
                *px = (i * 25 + (j / 100) as u8).wrapping_add(0);
            }
            if let Some(out) = enc.encode(&frame).expect("encode") {
                if let Some((rgba, dw, dh)) = dec.decode_rgba(&out.data).expect("decode") {
                    decoded = Some((rgba, dw, dh));
                    break;
                }
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
}

/// 核心 `Decoder` 实现（OpenH264 软解，H.264；全平台回退）。
impl aerodesk_core::platform::Decoder for SoftDecoder {
    type Error = String;

    fn configure(
        &mut self,
        codec: aerodesk_core::media_pipeline::Codec,
        _width: u32,
        _height: u32,
    ) -> Result<(), Self::Error> {
        if codec != aerodesk_core::media_pipeline::Codec::H264 {
            return Err(format!("OpenH264 仅支持 H.264，收到 {codec:?}"));
        }
        Ok(())
    }

    fn decode(
        &mut self,
        unit: &aerodesk_core::media_pipeline::EncodedUnit,
    ) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        let Some((rgba, w, h)) = self.decode_rgba(&unit.data)? else {
            return Ok(None);
        };
        Ok(Some(aerodesk_core::platform::VideoFrame {
            platform: None,
            handle: None,
            raw: Some(rgba),
            width: w as u32,
            height: h as u32,
            pts_ms: unit.pts_ms,
        }))
    }
}
