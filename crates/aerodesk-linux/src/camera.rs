//! V4L2 摄像头采集（Linux 被控端，CameraSource 实现）。
//!
//! 经 `v4l` crate 打开 /dev/videoN，请求 YUYV 格式，mmap 流式捕获，
//! YUYV → BGRA32（与 macOS MacCamera 输出格式对齐，core CameraFrame.raw 约定）。
//! 无摄像头环境（CI）`start()` 返回明确错误。

use std::path::Path;

use aerodesk_core::platform::{CameraFrame, CameraSource};
use v4l::buffer::Type;
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;
use v4l::{Device, FourCC};

/// YUV（BT.601 有限范围）→ BGR 单像素。
fn yuv_to_bgr(y: i32, u: i32, v: i32) -> [u8; 3] {
    let r = (y + ((91_881 * v) >> 16)).clamp(0, 255) as u8;
    let g = (y - ((22_554 * u + 46_802 * v) >> 16)).clamp(0, 255) as u8;
    let b = (y + ((116_130 * u) >> 16)).clamp(0, 255) as u8;
    [b, g, r]
}

/// YUYV（4 字节 2 像素：Y0 U Y1 V）→ 紧凑 BGRA32。
fn yuyv_to_bgra(data: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in (0..width).step_by(2) {
            let i = (y * width + x) * 2;
            let (y0, u, y1, v) = (
                data[i] as i32,
                data[i + 1] as i32 - 128,
                data[i + 2] as i32,
                data[i + 3] as i32 - 128,
            );
            let o = (y * width + x) * 4;
            let [b0, g0, r0] = yuv_to_bgr(y0, u, v);
            out[o..o + 4].copy_from_slice(&[b0, g0, r0, 255]);
            if x + 1 < width {
                let [b1, g1, r1] = yuv_to_bgr(y1, u, v);
                let o1 = o + 4;
                out[o1..o1 + 4].copy_from_slice(&[b1, g1, r1, 255]);
            }
        }
    }
    out
}

/// 枚举本机 V4L2 摄像头设备（/dev/video* 中可打开且可读格式的）。
pub fn list_cameras() -> Vec<String> {
    let mut out = Vec::new();
    for idx in 0..16u32 {
        let path = format!("/dev/video{idx}");
        if !Path::new(&path).exists() {
            continue;
        }
        if let Ok(dev) = Device::with_path(&path)
            && let Ok(fmt) = dev.format()
            && fmt.fourcc != FourCC::new(b"    ")
        {
            out.push(path);
        }
    }
    out
}

/// V4L2 摄像头采集器（YUYV → BGRA32）。
pub struct V4l2Camera {
    device: Option<Device>,
    // Stream<'a> 的 'a 只出现在内部 mmap 切片上（mmap 区域由 Arena 持有到 Drop，
    // handle 为 Arc 克隆，不真正借用 Device），故用 'static 便于自持有。
    stream: Option<Stream<'static>>,
    width: u32,
    height: u32,
}

impl V4l2Camera {
    /// 打开摄像头设备（默认 /dev/video0）。
    pub fn new(device: &str) -> Result<Self, String> {
        let dev = Device::with_path(device).map_err(|e| format!("v4l open {device}: {e}"))?;
        Ok(Self {
            device: Some(dev),
            stream: None,
            width: 0,
            height: 0,
        })
    }
}

impl CameraSource for V4l2Camera {
    type Error = String;

    fn start(&mut self, width: u32, height: u32, _fps: u32) -> Result<(), Self::Error> {
        let dev = self.device.take().ok_or("camera already stopped")?;
        // 请求 YUYV；驱动可能回退到最接近的分辨率/格式。
        let mut fmt = dev.format().map_err(|e| format!("v4l get format: {e}"))?;
        fmt.width = width;
        fmt.height = height;
        fmt.fourcc = FourCC::new(b"YUYV");
        let fmt = dev
            .set_format(&fmt)
            .map_err(|e| format!("v4l set format: {e}"))?;
        self.width = fmt.width;
        self.height = fmt.height;
        let stream: Stream<'static> = Stream::with_buffers(&dev, Type::VideoCapture, 4)
            .map_err(|e| format!("v4l stream: {e}"))?;
        self.device = Some(dev);
        self.stream = Some(stream);
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<CameraFrame>, Self::Error> {
        let Some(stream) = &mut self.stream else {
            return Ok(None);
        };
        let (buf, meta) = stream.next().map_err(|e| format!("v4l next: {e}"))?;
        let used = meta.bytesused as usize;
        let raw = yuyv_to_bgra(
            &buf[..used.min(buf.len())],
            self.width as usize,
            self.height as usize,
        );
        Ok(Some(CameraFrame {
            raw,
            width: self.width,
            height: self.height,
            pts_ms: 0,
        }))
    }

    fn stop(&mut self) {
        self.stream = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuyv_to_bgra_converts() {
        // 纯色灰帧：Y=128 U=128 V=128 → BGR=(128,128,128)。
        let (w, h) = (4usize, 2usize);
        let mut data = vec![0u8; w * h * 2];
        for px in data.chunks_exact_mut(4) {
            px.copy_from_slice(&[128, 128, 128, 128]);
        }
        let bgra = yuyv_to_bgra(&data, w, h);
        assert_eq!(bgra.len(), w * h * 4);
        for px in bgra.chunks_exact(4) {
            assert_eq!(px, &[128, 128, 128, 255]);
        }
    }

    #[test]
    fn yuyv_to_bgra_red() {
        // YUYV 中 Y=76 U=84 V=255 约等于 BT.601 红（有限范围近似）。
        let (w, h) = (2usize, 1usize);
        let data = [76u8, 84, 76, 255];
        let bgra = yuyv_to_bgra(&data, w, h);
        // BT.601 红：R≈254、G≈1、B≈0 → BGRA 序 R(out[2]) 显著高于 B(out[0])。
        assert!(bgra[2] > 200, "红帧 R 通道应显著高（BGRA 序）: {bgra:?}");
        assert!(bgra[0] < 100, "红帧 B 通道应低: {bgra:?}");
        assert_eq!(bgra[3], 255);
    }
}
