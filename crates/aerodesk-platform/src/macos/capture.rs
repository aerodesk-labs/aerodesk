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
    /// 已重建次数（#503：会话重建同样退避+封顶，防重建本身成为 SCStream churn 源）。
    rebuilds: u32,
    /// 采集已降级（连续重建/重试仍无有效帧）：停止一切重建与重试，视频静默下线。
    degraded: bool,
}

/// 显示器原生像素尺寸按上限等比缩放（默认高 ≤1080、宽 ≤1920）。
/// 关键：必须保持与显示器相同的宽高比，否则画面被拉伸、输入坐标错位。
pub const MAX_CAPTURE_W: u32 = 1920;
pub const MAX_CAPTURE_H: u32 = 1080;

/// 显示器休眠时 SCK 的 `displays()` 为空（#315），采集拿不到画面。
/// 用 `caffeinate -u`（断言用户活跃）唤醒后重查，最多重试 6 次（约 9s）。
fn ensure_displays() -> Result<SCShareableContent, String> {
    let mut last = SCShareableContent::get().map_err(|e| format!("SCK content: {e}"))?;
    if !last.displays().is_empty() {
        return Ok(last);
    }
    tracing::warn!("显示器休眠中（SCK 无可用显示器），尝试唤醒…");
    for _ in 0..6 {
        let _ = std::process::Command::new("caffeinate")
            .args(["-u", "-t", "3"])
            .spawn();
        std::thread::sleep(Duration::from_millis(1500));
        last = SCShareableContent::get().map_err(|e| format!("SCK content: {e}"))?;
        if !last.displays().is_empty() {
            tracing::info!("显示器已唤醒，恢复屏幕采集");
            return Ok(last);
        }
    }
    tracing::error!("显示器唤醒失败（可能已锁屏/无物理显示器）");
    Ok(last)
}

/// 构建 SCContentFilter + SCStreamConfiguration（重建会话时复用）。
/// width/height 传 0 表示按显示器原生尺寸等比缩放（保持宽高比）。
fn build_capture(
    display_idx: usize,
    fps: u32,
    width: u32,
    height: u32,
) -> Result<(SCContentFilter, SCStreamConfiguration, u32, u32, u32), String> {
    let content = ensure_displays()?;
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
            // #503 修复（系统重启事故直接死因）：采集失败必须退避重试 + 次数上限。
            // 每次 capture_sample_buffer 内部都会建/销一个 SCStream；原先失败只睡
            // 16ms 紧重试（~60 次/秒），会把 WindowServer 的 sharing context 刷爆
            // （86GB），16GB 机器被 Jetsam + watchdog 强制重启。改为：连续失败按
            // 指数退避（40ms→…→1s 封顶），达到上限后发送终态错误并退出线程（降级）。
            const MAX_ERR_STREAK: u32 = 15;
            let mut err_streak: u32 = 0;
            loop {
                let res = match SCScreenshotManager::capture_sample_buffer(&filter, &config) {
                    Ok(sample) => match sample.image_buffer().and_then(|pb| pb.io_surface()) {
                        Some(surface) => Ok(surface),
                        None => Err("sample without surface".to_string()),
                    },
                    Err(e) => Err(format!("SCK error: {e:?}")),
                };
                match res {
                    Ok(surface) => {
                        err_streak = 0;
                        if tx.send(Ok(surface)).is_err() {
                            return;
                        }
                        // 节流：无间隔狂轮询会压垮 replayd 导致 SCK 挂起
                        // （macOS 26 实测），按 ~30fps 节奏采集。
                        std::thread::sleep(Duration::from_millis(33));
                    }
                    Err(msg) => {
                        err_streak += 1;
                        if err_streak >= MAX_ERR_STREAK {
                            // 终态：通知主线程后退出，不再 churn（capture_frame 据此降级）。
                            let _ = tx.send(Err(format!(
                                "SCK 采集连续失败 {err_streak} 次，降级停止视频采集（最后错误: {msg}）"
                            )));
                            return;
                        }
                        if tx.send(Err(msg)).is_err() {
                            return;
                        }
                        // 指数退避：40ms << min(streak,5)，封顶 1s。
                        let backoff = Duration::from_millis(40)
                            .saturating_mul(1u32 << err_streak.min(5))
                            .min(Duration::from_secs(1));
                        std::thread::sleep(backoff);
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
            rebuilds: 0,
            degraded: false,
        })
    }

    /// 被采集显示器的 CGDirectDisplayID（输入注入坐标换算用）。
    pub fn display_id(&self) -> u32 {
        self.display_id
    }

    /// 采集一帧，返回 IOSurface（零拷贝，可直接进 VideoToolbox）。
    /// 失败（如无权限）返回 None；SCK 挂起或持续报错时连续无有效帧
    /// 达阈值自动重建会话（#274 自愈，对超时与错误路径同等生效）。
    /// 命名为 capture_frame 以区别于 core `MediaSource::next_frame`（#277）。
    pub fn capture_frame(&mut self, timeout: Duration) -> Option<IOSurface> {
        // #503：已降级 → 不再重试/重建。排空并丢弃 channel，防止采集线程退出前
        // 残留的帧/错误无人消费而堆积（IOSurface 是大对象）。
        if self.degraded {
            while self.rx.try_recv().is_ok() {}
            return None;
        }
        match self.rx.recv_timeout(timeout) {
            Ok(Ok(surface)) => {
                self.seq += 1;
                self.stale = 0;
                Some(surface)
            }
            // 错误与超时同样计入 stale：持续报错（无权限/设备移除）若不计数，
            // 永远到不了重建阈值，错误还会被静默吞掉。
            Ok(Err(msg)) => {
                // 采集线程连续失败达上限后的终态信号 → 直接降级（不再等重建阈值）。
                if msg.contains("降级停止视频采集") {
                    self.degraded = true;
                    tracing::error!("{msg}");
                }
                self.stale += 1;
                self.rebuild_if_stale();
                None
            }
            Err(_) => {
                self.stale += 1;
                self.rebuild_if_stale();
                None
            }
        }
    }

    /// 连续无有效帧达到阈值（60 次调用，按调用方超时 33~50ms 约 2~3s）时
    /// 重建采集会话（换新线程 + 新 filter/config）。重建失败记日志，
    /// stale 已清零，下一轮窗口后自动重试。
    fn rebuild_if_stale(&mut self) {
        if self.stale < 60 || self.degraded {
            return;
        }
        self.stale = 0;
        self.rebuilds += 1;
        // #503：重建次数封顶——连续重建仍无有效帧说明采集在本机当前不可用，
        // 降级停止（不再重建/重试），避免会话重建本身成为 SCStream churn 源。
        const MAX_REBUILDS: u32 = 3;
        if self.rebuilds > MAX_REBUILDS {
            self.degraded = true;
            tracing::error!(
                "SCK 采集重建 {MAX_REBUILDS} 次仍无有效帧，降级停止视频采集（音频/连接不受影响）"
            );
            return;
        }
        tracing::warn!(
            "SCK capture stalled, recreating session ({}/{MAX_REBUILDS})",
            self.rebuilds
        );
        match build_capture(self.display_idx, self.fps, self.width, self.height) {
            Ok((f, c, id, w, h)) => {
                self.display_id = id;
                self.width = w;
                self.height = h;
                self.rx = spawn_capture_thread(f, c);
            }
            Err(e) => tracing::error!("SCK capture session rebuild failed: {e}"),
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

/// 核心 `MediaSource` 实现：零拷贝帧以 `Arc<dyn Any + Send>` 承载 IOSurface，
/// 交给实现了核心 `Encoder` 的 VideoToolbox 编码器直接消费（不拷贝像素）。
impl aerodesk_core::platform::MediaSource for ScreenCapture {
    type Error = String;

    fn start(&mut self, _fps: u32, _with_cursor: bool) -> Result<(), Self::Error> {
        // 采集器在构造时已按 ScreenCapture::start 参数建好；trait 的 start 为
        // 泛化入口预留，重复调用视为已就绪。
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        match self.capture_frame(Duration::from_millis(33)) {
            Some(surface) => {
                let seq = self.seq();
                Ok(Some(aerodesk_core::platform::VideoFrame {
                    platform: Some(std::sync::Arc::new(surface)),
                    handle: None,
                    raw: None,
                    width: self.width(),
                    height: self.height(),
                    pts_ms: (seq * 33) as u64,
                }))
            }
            None => Ok(None),
        }
    }

    fn stop(&mut self) {}

    fn display_id(&self) -> Option<u32> {
        Some(ScreenCapture::display_id(self))
    }
}
