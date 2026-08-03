//! H.264 软编（x264，AnnexB 输出，4:2:0 兼容 WebRTC/VideoToolbox）。
//! 非 Windows 平台（Windows 无系统 x264，见 lib.rs 说明）。

use x264::{Colorspace, Encoder, Image, Plane};

/// 一帧编码输出。
#[derive(Debug)]
pub struct EncodedFrame {
    /// AnnexB H.264 NAL（SPS/PPS 由 headers() 提供，帧数据含关键帧）。
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts: i64,
}

/// RGB24 → I420 转换（BT.601 有限范围，整数运算）。
pub fn rgb_to_i420(rgb: &[u8], width: u32, height: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    assert_eq!(
        rgb.len(),
        (width * height * 3) as usize,
        "rgb size mismatch"
    );
    let w = width as usize;
    let h = height as usize;
    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; (w / 2) * (h / 2)];
    let mut v = vec![0u8; (w / 2) * (h / 2)];
    for row in 0..h {
        for col in 0..w {
            let i = (row * w + col) * 3;
            let (r, g, b) = (rgb[i] as i32, rgb[i + 1] as i32, rgb[i + 2] as i32);
            let yv = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y[row * w + col] = yv.clamp(0, 255) as u8;
            if row % 2 == 0 && col % 2 == 0 {
                let uv = ((row / 2) * (w / 2) + col / 2) as usize;
                let uv_r = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let vv = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                u[uv] = uv_r.clamp(0, 255) as u8;
                v[uv] = vv.clamp(0, 255) as u8;
            }
        }
    }
    (y, u, v)
}

/// H.264 编码器（x264 软编，输入 RGB24，输出 4:2:0 AnnexB）。
pub struct X264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    pts: i64,
}

impl X264Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self, String> {
        assert!(
            width % 2 == 0 && height % 2 == 0,
            "I420 needs even dimensions"
        );
        let encoder = x264::Setup::preset(x264::Preset::Veryfast, x264::Tune::None, false, true)
            .fps(fps, 1)
            .bitrate(bitrate_kbps as i32)
            .annexb(true)
            .sliced_threads(false)
            .threads(1)
            .max_keyframe_interval((fps * 2) as i32)
            .build(Colorspace::I420, width as i32, height as i32)
            .map_err(|e| format!("x264 init: {e:?}"))?;
        Ok(Self {
            encoder,
            width,
            height,
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
        let (y, u, v) = rgb_to_i420(rgb, self.width, self.height);
        let y_plane = Plane {
            stride: self.width as i32,
            data: &y,
        };
        let uv_plane = Plane {
            stride: (self.width / 2) as i32,
            data: &u,
        };
        let v_plane = Plane {
            stride: (self.width / 2) as i32,
            data: &v,
        };
        let image = Image::new(
            Colorspace::I420,
            self.width as i32,
            self.height as i32,
            &[y_plane, uv_plane, v_plane],
        );
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
                assert!(out.data.starts_with(&[0, 0, 0, 1]) || out.data.starts_with(&[0, 0, 1]));
            }
        }
        assert!(keyframes >= 1, "expected at least one keyframe");
    }

    #[test]
    fn converts_rgb_to_i420() {
        let mut frame = vec![0u8; 320 * 180 * 3];
        frame[0] = 255;
        let (y, u, v) = rgb_to_i420(&frame, 320, 180);
        assert_eq!(y.len(), 320 * 180);
        assert_eq!(u.len(), 160 * 90);
        assert_eq!(v.len(), 160 * 90);
        assert!(y[0] < 128);
        assert!(v[0] > 128);
        assert!(u[0] < 128);
    }
}
