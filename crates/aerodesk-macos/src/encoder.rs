//! H.264 编码器（x264 软编，AnnexB 输出）。

use x264::{Colorspace, Encoder, Image};

/// 一帧编码输出。
#[derive(Debug)]
pub struct EncodedFrame {
    /// AnnexB H.264 NAL（SPS/PPS 由 headers() 提供，帧数据含关键帧）。
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts: i64,
}

/// H.264 编码器。
pub struct X264Encoder {
    encoder: Encoder,
    width: i32,
    height: i32,
    pts: i64,
}

impl X264Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self, String> {
        let encoder = x264::Setup::preset(x264::Preset::Ultrafast, x264::Tune::None, false, true)
            .fps(fps, 1)
            .bitrate(bitrate_kbps as i32)
            .annexb(true)
            .max_keyframe_interval((fps * 2) as i32) // 每 2 秒一个关键帧
            .build(Colorspace::RGB, width as i32, height as i32)
            .map_err(|e| format!("x264 init: {e:?}"))?;
        Ok(Self {
            encoder,
            width: width as i32,
            height: height as i32,
            pts: 0,
        })
    }

    /// SPS/PPS 参数集（连接建立后发送一次）。
    pub fn headers(&mut self) -> Vec<u8> {
        self.encoder
            .headers()
            .map(|h| h.entirety().to_vec())
            .unwrap_or_default()
    }

    /// 编码一帧 RGB24 图像。
    pub fn encode(&mut self, rgb: &[u8]) -> Result<Option<EncodedFrame>, String> {
        if rgb.len() != (self.width * self.height * 3) as usize {
            return Err("frame size mismatch".into());
        }
        let image = Image::rgb(self.width, self.height, rgb);
        let (data, picture) = self
            .encoder
            .encode(self.pts, image)
            .map_err(|e| format!("x264 encode: {e:?}"))?;
        let pts = self.pts;
        self.pts += 1;
        let bytes = data.entirety();
        if bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame {
            data: bytes.to_vec(),
            keyframe: picture.keyframe(),
            pts,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_synthetic_frames() {
        let mut enc = X264Encoder::new(320, 180, 30, 500).expect("encoder");
        let headers = enc.headers();
        assert!(!headers.is_empty(), "SPS/PPS headers expected");

        // 合成帧：渐变
        let mut frame = vec![0u8; 320 * 180 * 3];
        let mut keyframes = 0;
        for i in 0..10 {
            for (j, px) in frame.iter_mut().enumerate() {
                *px = (i * 25 + j as usize / 100) as u8;
            }
            if let Some(out) = enc.encode(&frame).expect("encode") {
                if out.keyframe {
                    keyframes += 1;
                }
                assert!(!out.data.is_empty());
                // AnnexB 起始码
                assert!(out.data.starts_with(&[0, 0, 0, 1]) || out.data.starts_with(&[0, 0, 1]));
            }
        }
        // 第一帧应为关键帧
        assert!(keyframes >= 1, "expected at least one keyframe");
    }
}
