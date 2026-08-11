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
///
/// macOS 26 上 `SCScreenshotManager::capture_sample_buffer` 的同步等待可能
/// 永久挂起（replayd 回调不再返回，实测 ~600 帧后出现）。因此采集放到
/// 专用线程 + mpsc：主线程 `recv_timeout` 等待，挂起时超时返回；连续超时
/// 3s 判定会话卡死，自动重建 filter/config 并重启采集线程（自愈）。
pub struct ScreenCapture {
    rx: std::sync::mpsc::Receiver<Result<IOSurface, String>>,
    display_idx: usize,
    fps: u32,
    width: u32,
    height: u32,
    seq: i64,
    /// CGDirectDisplayID（#75：输入注入按被控显示器换算坐标，不只主屏）。
    display_id: u32,
    /// 连续无帧计数（达到阈值重建采集会话）。
    stale: u32,
}

/// 显示器原生像素尺寸按上限等比缩放（默认高 ≤1080、宽 ≤1920）。
/// 关键：必须保持与显示器相同的宽高比，否则画面被拉伸、输入坐标错位。
pub const MAX_CAPTURE_W: u32 = 1920;
pub const MAX_CAPTURE_H: u32 = 1080;

/// 构建 SCContentFilter + SCStreamConfiguration（重建会话时复用）。
/// width/height 传 0 表示按显示器原生尺寸等比缩放（保持宽高比）。
fn build_capture(
    display_idx: usize,
    fps: u32,
    width: u32,
    height: u32,
) -> Result<(SCContentFilter, SCStreamConfiguration, u32, u32, u32), String> {
    let content = SCShareableContent::get().map_err(|e| format!("SCK content: {e}"))?;
    let displays = content.displays();
    let display = displays.get(display_idx).ok_or("display not found")?;
    let display_id = display.display_id();
    let (dw, dh) = (display.width().max(1), display.height().max(1));
    let (w, h) = if width == 0 || height == 0 {
        let scale = (MAX_CAPTURE_W as f32 / dw as f32)
            .min(MAX_CAPTURE_H as f32 / dh as f32)
            .min(1.0);
        (
            ((dw as f32 * scale) as u32).max(2),
            ((dh as f32 * scale) as u32).max(2),
        )
    } else {
        (width, height)
    };

    let filter = SCContentFilter::create().with_display(display).build();

    let mut config = SCStreamConfiguration::default();
    config.set_width(w).set_height(h);
    config.set_pixel_format(PixelFormat::BGRA);
    // 光标不进画面：观看端用 cursor 通道 + 叠加层渲染（#75），
    // 避免视频里烤进光标与叠加层重影/错位。
    config.set_shows_cursor(false);
    config.set_minimum_frame_interval(&CMTime::new(1, fps as i32));
    Ok((filter, config, display_id, w, h))
}

/// 采集线程：循环调用 capture_sample_buffer，成功把 IOSurface 发回主线程。
/// SCK 挂起时线程阻塞在等待回调，主线程 recv_timeout 超时后重建会话。
fn spawn_capture_thread(
    filter: SCContentFilter,
    config: SCStreamConfiguration,
) -> std::sync::mpsc::Receiver<Result<IOSurface, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            loop {
                match SCScreenshotManager::capture_sample_buffer(&filter, &config) {
                    Ok(sample) => match sample.image_buffer().and_then(|pb| pb.io_surface()) {
                        Some(surface) => {
                            if tx.send(Ok(surface)).is_err() {
                                return;
                            }
                            // 节流：无间隔狂轮询会压垮 replayd 导致 SCK 挂起
                            // （macOS 26 实测），按 ~30fps 节奏采集。
                            std::thread::sleep(Duration::from_millis(33));
                        }
                        None => {
                            let _ = tx.send(Err("sample without surface".into()));
                            std::thread::sleep(Duration::from_millis(33));
                        }
                    },
                    Err(e) => {
                        let _ = tx.send(Err(format!("SCK error: {e:?}")));
                        std::thread::sleep(Duration::from_millis(16));
                    }
                }
            }
        })
        .expect("spawn capture thread");
    rx
}

impl ScreenCapture {
    /// 启动采集（display_idx：0 = 主显示器；width/height=0 按显示器原生等比缩放）。
    pub fn start(display_idx: usize, fps: u32, width: u32, height: u32) -> Result<Self, String> {
        let (filter, config, display_id, w, h) = build_capture(display_idx, fps, width, height)?;
        let rx = spawn_capture_thread(filter, config);
        Ok(Self {
            rx,
            display_idx,
            fps,
            width: w,
            height: h,
            seq: 0,
            display_id,
            stale: 0,
        })
    }

    /// 被采集显示器的 CGDirectDisplayID（输入注入坐标换算用）。
    pub fn display_id(&self) -> u32 {
        self.display_id
    }

    /// 采集一帧，返回 IOSurface（零拷贝，可直接进 VideoToolbox）。
    /// 失败（如无权限）返回 None；SCK 挂起时连续超时自动重建会话。
    pub fn next_frame(&mut self, timeout: Duration) -> Option<IOSurface> {
        match self.rx.recv_timeout(timeout) {
            Ok(Ok(surface)) => {
                self.seq += 1;
                self.stale = 0;
                Some(surface)
            }
            Ok(Err(_)) => None,
            Err(_) => {
                // 超时：SCK 可能挂起。连续 ~3s（60×50ms）无帧则重建会话自愈。
                self.stale += 1;
                if self.stale >= 60 {
                    self.stale = 0;
                    eprintln!("SCK capture stalled, recreating session");
                    if let Ok((f, c, id, w, h)) =
                        build_capture(self.display_idx, self.fps, self.width, self.height)
                    {
                        self.display_id = id;
                        self.width = w;
                        self.height = h;
                        self.rx = spawn_capture_thread(f, c);
                    }
                }
                None
            }
        }
    }

    pub fn seq(&self) -> i64 {
        self.seq
    }

    /// 采集分辨率宽。
    pub fn width(&self) -> u32 {
        self.width
    }

    /// 采集分辨率高。
    pub fn height(&self) -> u32 {
        self.height
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
