//! 屏幕采集主路径：Windows Graphics Capture（Win10 1903+，经 windows-capture crate）。
//!
//! 为什么上位（#514）：WGC 由系统合成器（DWM）直接出帧，不经显卡适配器/输出枚举——
//! 手写 DXGI 路径硬编码 EnumAdapters1(0)，曾被虚拟显示驱动顶掉输出序导致采集失效；
//! WGC 的 Monitor 枚举（EnumDisplayMonitors）天然覆盖全部适配器的输出。变化驱动
//! 特性与 DXGI 相同，首帧仍由 GDI 引导兜底（#477，capture::gdi_bootstrap_frame）。
//!
//! Win10 19045 实测配方（#514 评估）：CursorCaptureSettings::WithoutCursor + 其余
//! Default——MinimumUpdateIntervalSettings::Custom 与 DrawBorderSettings::WithoutBorder
//! 是 Win11-only，在 Win10 上启动即报错（crate 有 ApiInformation 守卫，错误清晰）。
//! 帧内不含光标：与 DXGI 路径既有行为一致（光标合成是独立特性）。
//!
//! 线程模型：crate 自管采集线程（start_free_threaded），回调把 BGRA 帧推入帧槽
//! （只留最新，编码背压时丢旧不排队）；`capture_frame` 以 ≤16ms 等待新帧，
//! 与 DxgiCapturer 的 AcquireNextFrame(16) 节奏一致。

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::windows::CapturedFrame;
use crate::windows::capture::{gdi_bootstrap_frame, scale_bgra};

/// 帧槽：采集回调线程推入最新帧，`capture_frame` 取走；seq 为新帧序号。
#[derive(Default)]
struct FrameSlot {
    inner: Mutex<SlotInner>,
    cond: Condvar,
}

#[derive(Default)]
struct SlotInner {
    seq: u64,
    frame: Option<CapturedFrame>,
}

/// 传给采集回调的上下文（构造时确定输出分辨率，回调内完成缩放）。
struct WgcFlags {
    slot: Arc<FrameSlot>,
    out_w: u32,
    out_h: u32,
}

struct WgcHandler {
    slot: Arc<FrameSlot>,
    out_w: u32,
    out_h: u32,
    /// as_nopadding_buffer 的复用暂存（帧缓冲有行填充时拷贝去填充）。
    scratch: Vec<u8>,
}

impl GraphicsCaptureApiHandler for WgcHandler {
    type Flags = WgcFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            slot: ctx.flags.slot,
            out_w: ctx.flags.out_w,
            out_h: ctx.flags.out_h,
            scratch: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let w = frame.width();
        let h = frame.height();
        let buf = frame
            .buffer()
            .map_err(|e| -> Self::Error { Box::new(e) })?
            .as_nopadding_buffer(&mut self.scratch)
            .to_vec();
        // 与 DXGI 路径同一输出约定：目标分辨率（默认 1080p 缩放喂软编/硬编）。
        let bgra = if (w, h) != (self.out_w, self.out_h) {
            scale_bgra(&buf, w, h, self.out_w, self.out_h)
        } else {
            buf
        };
        let pts_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let mut g = self.slot.inner.lock().unwrap();
        g.frame = Some(CapturedFrame {
            bgra,
            width: self.out_w,
            height: self.out_h,
            pts_us,
        });
        g.seq = g.seq.wrapping_add(1);
        drop(g);
        self.slot.cond.notify_one();
        Ok(())
    }
}

/// WGC 采集器（被控端，Windows）。API 面与 DxgiCapturer 对齐，供 ScreenCapturer 组合。
pub struct WgcCapturer {
    control: Option<CaptureControl<WgcHandler, Box<dyn std::error::Error + Send + Sync>>>,
    slot: Arc<FrameSlot>,
    /// 输出（缩放后）宽高；与原生不同时回调内做 CPU 双线性缩放。
    out_width: u32,
    out_height: u32,
    /// 被控显示器在虚拟屏幕中的区域（像素；多显示器坐标映射用，#75）。
    display_rect: (i32, i32, u32, u32),
    /// 当前采集的显示器索引（#58 运行中切换用；0 起，EnumDisplayMonitors 序）。
    display: u32,
    target_w: u32,
    target_h: u32,
    /// 已消费的最新帧序号。
    last_seq: u64,
    /// #477：首帧 GDI 引导只做一次。
    bootstrapped: bool,
}

impl WgcCapturer {
    /// 原生分辨率采集（主显示器）。
    #[allow(dead_code)] // 与 DxgiCapturer 对齐的完整 API 面；调用点一律带缩放
    pub fn new() -> Result<Self, String> {
        Self::new_with_display(0, 0, 0)
    }

    /// 按目标分辨率采集（0/0 = 原生；目标必须为偶数，适配 I420 编码）。
    #[allow(dead_code)] // 同上：API 面对齐
    pub fn new_with_scale(target_w: u32, target_h: u32) -> Result<Self, String> {
        Self::new_with_display(0, target_w, target_h)
    }

    /// 按显示器索引 + 目标分辨率采集（display=0 = 枚举序首台，通常即主显示器）。
    pub fn new_with_display(display: u32, target_w: u32, target_h: u32) -> Result<Self, String> {
        if (target_w != 0 || target_h != 0) && (target_w == 0 || target_h == 0) {
            return Err("scale target must be both set or both 0".into());
        }
        if target_w % 2 != 0 || target_h % 2 != 0 {
            return Err(format!("scale target must be even: {target_w}x{target_h}"));
        }
        let monitors = Monitor::enumerate().map_err(|e| format!("WGC 显示器枚举失败: {e}"))?;
        let Some(monitor) = monitors.get(display as usize).copied() else {
            return Err(format!(
                "WGC 无显示器 #{display}（共 {} 台）",
                monitors.len()
            ));
        };
        let width = monitor
            .width()
            .map_err(|e| format!("WGC 显示器宽度: {e}"))?;
        let height = monitor
            .height()
            .map_err(|e| format!("WGC 显示器高度: {e}"))?;
        if width == 0 || height == 0 {
            return Err("invalid desktop size".into());
        }
        let display_rect = monitor_rect(&monitor)?;
        let (out_width, out_height) = if target_w == 0 {
            (width, height)
        } else {
            (target_w, target_h)
        };
        let slot = Arc::new(FrameSlot::default());
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithoutCursor,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            WgcFlags {
                slot: slot.clone(),
                out_w: out_width,
                out_h: out_height,
            },
        );
        // 启动错误（旧系统无 WGC/会话受限等）在此同步返回，交由 ScreenCapturer 回退。
        let control = WgcHandler::start_free_threaded(settings)
            .map_err(|e| format!("WGC 会话启动失败: {e}"))?;
        Ok(Self {
            control: Some(control),
            slot,
            out_width,
            out_height,
            display_rect,
            display,
            target_w,
            target_h,
            last_seq: 0,
            bootstrapped: false,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.out_width, self.out_height)
    }

    /// 被控显示器在虚拟屏幕中的区域（像素；#75 注入坐标映射用）。
    pub fn display_rect(&self) -> (i32, i32, u32, u32) {
        self.display_rect
    }

    /// 运行中切换采集显示器（#58）：停会话按新索引重建，保持目标输出分辨率
    /// （编码器无需重建）。失败时保持原采集不变。
    pub fn switch_display(&mut self, display: u32) -> Result<(), String> {
        let old_display = self.display;
        let (target_w, target_h) = (self.target_w, self.target_h);
        self.shutdown_session();
        match Self::new_with_display(display, target_w, target_h) {
            Ok(next) => {
                *self = next;
                Ok(())
            }
            Err(e) => {
                // 回退重建原显示器，尽量恢复会话。
                if let Ok(prev) = Self::new_with_display(old_display, target_w, target_h) {
                    *self = prev;
                }
                Err(e)
            }
        }
    }

    /// 取下一帧（阻塞最多 16ms）。无新帧/错误返回 None。
    pub fn capture_frame(&mut self) -> Option<CapturedFrame> {
        // 会话线程已结束（安全桌面切换/系统收回会话等）：取回错误日志一次，
        // 之后恒 None。与 DXGI ACCESS_LOST 后恒 None 的行为对齐。
        if self.control.as_ref().is_none_or(|c| c.is_finished()) {
            if let Some(control) = self.control.take() {
                match control.into_thread_handle().join() {
                    Ok(Err(e)) => tracing::warn!("WGC 采集会话结束（错误）: {e}"),
                    Ok(Ok(())) => tracing::warn!("WGC 采集会话已结束"),
                    Err(_) => tracing::warn!("WGC 采集线程 panic"),
                }
            }
            // 保持与 AcquireNextFrame(16) 相同的节奏，避免发布循环热自旋。
            std::thread::sleep(Duration::from_millis(16));
            return None;
        }

        let last = self.last_seq;
        let guard = self.slot.inner.lock().unwrap();
        let (mut guard, _timeout) = self
            .slot
            .cond
            .wait_timeout_while(guard, Duration::from_millis(16), |g| g.seq == last)
            .unwrap();
        if guard.seq != last {
            self.last_seq = guard.seq;
            self.bootstrapped = true;
            return guard.frame.take();
        }
        drop(guard);

        // #477 机制 B：WGC 同为变化驱动，首个变化帧到来之前用 GDI 引导当前桌面
        // 内容（实测 WGC 首帧 35-47ms，也必然晚于 16ms 等待——首帧基本都由引导
        // 提供，随后 WGC 帧接管）。仅此一次。
        if !self.bootstrapped {
            self.bootstrapped = true;
            let f = gdi_bootstrap_frame(self.display_rect, self.out_width, self.out_height);
            if f.is_some() {
                tracing::info!("#477 GDI 首帧引导成功（WGC 路径）");
            } else {
                tracing::warn!("#477 GDI 首帧引导失败（WGC 路径，回退等待首个变化帧）");
            }
            return f;
        }
        None
    }

    fn shutdown_session(&mut self) {
        if let Some(control) = self.control.take() {
            if let Err(e) = control.stop() {
                tracing::warn!("WGC 会话停止异常: {e:?}");
            }
        }
    }
}

impl Drop for WgcCapturer {
    fn drop(&mut self) {
        self.shutdown_session();
    }
}

impl aerodesk_core::platform::MediaSource for WgcCapturer {
    type Error = String;

    fn start(&mut self, _fps: u32, _with_cursor: bool) -> Result<(), Self::Error> {
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        Ok(self
            .capture_frame()
            .map(|f| aerodesk_core::platform::VideoFrame {
                platform: None,
                handle: None,
                raw: Some(f.bgra),
                width: f.width,
                height: f.height,
                pts_ms: f.pts_us.max(0) as u64 / 1000,
            }))
    }

    fn stop(&mut self) {
        self.shutdown_session();
    }

    fn display_id(&self) -> Option<u32> {
        Some(self.display)
    }

    fn switch_display(&mut self, display: u32) -> Result<(), String> {
        self.switch_display(display)
    }

    fn display_rect(&self) -> Option<(i32, i32, u32, u32)> {
        Some(self.display_rect)
    }
}

/// 由 HMONITOR 取虚拟屏幕区域（GetMonitorInfoW 的 rcMonitor）。
fn monitor_rect(monitor: &Monitor) -> Result<(i32, i32, u32, u32), String> {
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
    let hmon = HMONITOR(monitor.as_raw_hmonitor());
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(hmon, &mut info) }
        .ok()
        .map_err(|e| format!("GetMonitorInfoW: {e}"))?;
    let r = info.rcMonitor;
    Ok((
        r.left,
        r.top,
        (r.right - r.left).max(0) as u32,
        (r.bottom - r.top).max(0) as u32,
    ))
}
