//! ScreenCaptureKit 屏幕采集（轮询式，无 delegate）。
//!
//! 输出 IOSurface（与 VideoToolbox 硬编零拷贝直连）。
//! 运行时需要屏幕录制权限（TCC：系统设置 → 隐私与安全性 → 屏幕录制）。
//!
//! 采用 screencapturekit crate 的 SCScreenshotManager 轮询路径
//! （macOS 26 上 SCStream 对部分原生窗口返回空白帧，轮询路径无此问题，
//! 参考 mac-screen-cast 项目）。

use std::time::Duration;

use screencapturekit::IOSurface;
use screencapturekit::prelude::*;
use screencapturekit::screenshot_manager::SCScreenshotManager;

/// ScreenCaptureKit 屏幕采集器。
pub struct ScreenCapture {
    filter: SCContentFilter,
    config: SCStreamConfiguration,
    seq: i64,
}

impl ScreenCapture {
    /// 启动采集（display_idx：0 = 主显示器）。
    pub fn start(display_idx: usize, fps: u32, width: u32, height: u32) -> Result<Self, String> {
        let content = SCShareableContent::get().map_err(|e| format!("SCK content: {e}"))?;
        let displays = content.displays();
        let display = displays.get(display_idx).ok_or("display not found")?;

        let filter = SCContentFilter::create().with_display(display).build();

        let mut config = SCStreamConfiguration::default();
        config.set_width(width).set_height(height);
        config.set_pixel_format(PixelFormat::BGRA);
        config.set_shows_cursor(true);
        config.set_minimum_frame_interval(&CMTime::new(1, fps as i32));

        Ok(Self {
            filter,
            config,
            seq: 0,
        })
    }

    /// 采集一帧，返回 IOSurface（零拷贝，可直接进 VideoToolbox）。
    /// 失败（如无权限）返回 None。
    pub fn next_frame(&mut self, timeout: Duration) -> Option<IOSurface> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match SCScreenshotManager::capture_sample_buffer(&self.filter, &self.config) {
                Ok(sample) => {
                    let pb = sample.image_buffer()?;
                    let surface = pb.io_surface()?;
                    self.seq += 1;
                    return Some(surface);
                }
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(16));
                    let _ = e;
                }
            }
        }
    }

    pub fn seq(&self) -> i64 {
        self.seq
    }

    /// 采集分辨率宽。
    pub fn width(&self) -> u32 {
        self.config.width()
    }

    /// 采集分辨率高。
    pub fn height(&self) -> u32 {
        self.config.height()
    }
}

/// 从 IOSurface 读取 BGRA 像素（按 bytes_per_row 行复制），供软件编码路径使用。
pub fn surface_to_bgra(surface: &IOSurface, w: u32, h: u32) -> Result<Vec<u8>, String> {
    use screencapturekit::cm::IOSurfaceLockOptions;
    let guard = surface
        .lock(IOSurfaceLockOptions::READ_ONLY)
        .map_err(|e| format!("iosurface lock: {e}"))?;
    let bpr = guard.bytes_per_row();
    let base = guard.base_address();
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        let src = unsafe { base.add(y * bpr) };
        let dst = &mut bgra[y * (w as usize) * 4..(y + 1) * (w as usize) * 4];
        unsafe {
            std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len());
        }
    }
    Ok(bgra)
}
