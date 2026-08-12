//! 屏幕采集（被控端）。
//!
//! X11 回退：x11rb `GetImage`（纯 Rust，无 C 依赖）读回 BGRA → RGBA。
//! Wayland：xdg-desktop-portal ScreenCast + PipeWire（真机阶段）。

use std::time::{SystemTime, UNIX_EPOCH};

use crate::CapturedFrame;

/// X11 采集器（X11 桌面）。
#[cfg(target_os = "linux")]
pub struct X11Capturer {
    conn: x11rb::rust_connection::RustConnection,
    root: x11rb::protocol::xproto::Window,
    width: u32,
    height: u32,
    depth: u8,
}

#[cfg(target_os = "linux")]
impl X11Capturer {
    pub fn new() -> Result<Self, String> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt;

        let (conn, screen_num) = x11rb::rust_connection::RustConnection::connect(None)
            .map_err(|e| format!("x11 connect: {e}"))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let depth = screen.root_depth;
        let geo = conn
            .get_geometry(root)
            .map_err(|e| format!("get_geometry: {e:?}"))?
            .reply()
            .map_err(|e| format!("get_geometry reply: {e}"))?;
        let (width, height) = (geo.width.max(1) as u32, geo.height.max(1) as u32);
        Ok(Self {
            conn,
            root,
            width,
            height,
            depth,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 取下一帧（X11 GetImage，BGRA → RGBA）。
    pub fn capture_frame(&mut self) -> Option<CapturedFrame> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};

        let geo = self.conn.get_geometry(self.root).ok()?.reply().ok()?;
        let (w, h) = (geo.width as u32, geo.height as u32);
        let img = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                geo.width,
                geo.height,
                !0,
            )
            .ok()?
            .reply()
            .ok()?;
        let src = img.data.as_slice();
        // X11 24/32bpp little-endian：内存为 BGRX / BGRA → 转 RGBA。
        let bpp = (self.depth / 8).max(3) as usize;
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
        for y in 0..h as usize {
            let row = y * w as usize * bpp;
            for x in 0..w as usize {
                let i = row + x * bpp;
                let (b, g, r) = (src[i], src[i + 1], src[i + 2]);
                rgba.extend_from_slice(&[r, g, b, 255]);
            }
        }
        let pts_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        Some(CapturedFrame {
            rgba,
            width: w,
            height: h,
            pts_us,
        })
    }
}

#[cfg(target_os = "linux")]
impl aerodesk_core::platform::MediaSource for X11Capturer {
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
                raw: Some(f.rgba),
                width: f.width,
                height: f.height,
                pts_ms: f.pts_us.max(0) as u64 / 1000,
            }))
    }

    fn stop(&mut self) {}
}

/// 非 Linux 主机上的编译期骨架（保证 workspace 全平台可编译）。
#[cfg(not(target_os = "linux"))]
pub struct X11Capturer;

#[cfg(not(target_os = "linux"))]
impl X11Capturer {
    pub fn new() -> Result<Self, String> {
        Err("linux: X11 capture only available on Linux".into())
    }

    pub fn size(&self) -> (u32, u32) {
        (0, 0)
    }
}

#[cfg(not(target_os = "linux"))]
impl aerodesk_core::platform::MediaSource for X11Capturer {
    type Error = String;

    fn start(&mut self, _fps: u32, _with_cursor: bool) -> Result<(), Self::Error> {
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        Ok(None)
    }

    fn stop(&mut self) {}
}

/// Wayland（xdg-desktop-portal ScreenCast + PipeWire）采集器。
///
/// 流程：portal 会话（触发用户授权）→ 拿 PipeWire fd → 建流 → 收 BGRA 帧。
/// 后台线程持有 tokio runtime + lamco 封装（portal/pipewire 均为 !Send 类型，
/// 由 lamco 线程架构解决）；`next_frame()` 从同步 channel 取帧。
///
/// 依赖：Wayland 桌面 + `xdg-desktop-portal` + 对应后端 + PipeWire 运行；
/// 无这些环境时 `start()` 返回明确错误（CI/无头环境跳过，不 panic）。
/// 线程安全采集帧（core `VideoFrame` 含 `Arc<dyn Any + Send>` 非 Send，
/// 不能直接跨线程走 channel；这里只传纯数据，消费侧再包成 core 帧）。
#[cfg(all(target_os = "linux", feature = "pipewire"))]
struct RawCaptureFrame {
    raw: Vec<u8>,
    width: u32,
    height: u32,
    pts_ms: u64,
}

#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub struct WaylandPortalCapturer {
    thread: Option<std::thread::JoinHandle<()>>,
    rx: Option<std::sync::mpsc::Receiver<RawCaptureFrame>>,
    stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

/// 按行剥离 stride padding，输出紧凑 BGRA（width*height*4）。
#[cfg(all(target_os = "linux", feature = "pipewire"))]
fn strip_stride(data: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
    let row = (width as usize) * 4;
    if stride as usize == row || stride == 0 {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = y * stride as usize;
        let end = (start + row).min(data.len());
        if start >= data.len() {
            break;
        }
        out.extend_from_slice(&data[start..end]);
    }
    out
}

/// lamco `VideoFrame`（BGRA）→ 线程安全原始帧（raw=紧凑 BGRA32）。
#[cfg(all(target_os = "linux", feature = "pipewire"))]
fn lamco_frame_to_raw(frame: lamco_pipewire::VideoFrame) -> Result<RawCaptureFrame, String> {
    use lamco_pipewire::FrameBuffer;
    let lamco_pipewire::VideoFrame {
        width,
        height,
        stride,
        format,
        buffer,
        pts,
        ..
    } = frame;
    let data = match buffer {
        FrameBuffer::Memory(bytes) => bytes.as_ref().clone(),
        FrameBuffer::DmaBuf(_) => {
            return Err("unexpected dma-buf frame (capture configured use_dmabuf=false)".into());
        }
    };
    let raw = if format == lamco_pipewire::PixelFormat::BGRA {
        strip_stride(&data, width, height, stride)
    } else {
        // 兜底转换（正常配置下 portal 按 BGRA 协商，很少走到这里）。
        let mut bgra = vec![0u8; (width * height * 4) as usize];
        lamco_pipewire::convert_format(
            &data,
            &mut bgra,
            format,
            lamco_pipewire::PixelFormat::BGRA,
            width,
            height,
            stride,
            width * 4,
        )
        .map_err(|e| format!("pipewire format convert: {e}"))?;
        bgra
    };
    Ok(RawCaptureFrame {
        raw,
        width,
        height,
        pts_ms: pts / 1_000_000,
    })
}

#[cfg(all(target_os = "linux", feature = "pipewire"))]
async fn wayland_capture_loop(
    _fps: u32,
    frame_tx: std::sync::mpsc::SyncSender<RawCaptureFrame>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::sync::atomic::Ordering;

    use lamco_pipewire::{PipeWireConfig, PipeWireManager, PixelFormat, SourceType, StreamInfo};
    use lamco_portal::PortalManager;

    let init = async {
        let portal = PortalManager::with_default()
            .await
            .map_err(|e| format!("portal: {e}"))?;
        let (session, _token) = portal
            .create_session("aerodesk".to_string(), None)
            .await
            .map_err(|e| format!("portal session: {e}"))?;
        let streams = session.streams().to_vec();
        if streams.is_empty() {
            return Err("portal: 没有可采集的流（用户未授权/无显示器）".to_string());
        }
        let config = PipeWireConfig {
            buffer_count: 4,
            preferred_format: Some(PixelFormat::BGRA),
            use_dmabuf: false,
            dmabuf_passthrough: false,
            frame_buffer_size: 8,
            stream_name_prefix: "aerodesk".to_string(),
            ..Default::default()
        };
        let mut pw = PipeWireManager::new(config).map_err(|e| format!("pipewire manager: {e}"))?;
        // SAFETY: lamco-portal 已转移 fd 所有权（内部 mem::forget），这里把它包装成
        // OwnedFd 交给 PipeWire（connect 失败时自动 close，避免泄漏）。
        let owned = unsafe { OwnedFd::from_raw_fd(session.pipewire_fd()) };
        pw.connect(owned)
            .await
            .map_err(|e| format!("pipewire connect: {e}"))?;
        let s = &streams[0];
        let info = StreamInfo {
            node_id: s.node_id,
            position: s.position,
            size: s.size,
            source_type: SourceType::Monitor,
        };
        let handle = pw
            .create_stream(&info)
            .await
            .map_err(|e| format!("create stream: {e}"))?;
        let rx = pw
            .frame_receiver(handle.id)
            .await
            .ok_or_else(|| "pipewire: 无帧接收器".to_string())?;
        Ok::<_, String>((portal, session, pw, rx))
    }
    .await;

    let (portal, session, mut pw, mut rx) = match init {
        Ok(x) => x,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let _ = ready_tx.send(Ok(()));

    while !stop.load(Ordering::Relaxed) {
        match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
            Ok(Some(frame)) => {
                if let Ok(raw) = lamco_frame_to_raw(frame) {
                    if frame_tx.send(raw).is_err() {
                        break; // 消费端已 drop（stop）
                    }
                }
            }
            Ok(None) => break,
            Err(_) => {} // 超时：继续检查 stop
        }
    }
    let _ = pw.shutdown().await;
    drop(session); // 关闭 portal 会话
    drop(portal);
    drop(frame_tx);
}

#[cfg(all(target_os = "linux", feature = "pipewire"))]
impl WaylandPortalCapturer {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            thread: None,
            rx: None,
            stop: None,
        })
    }
}

#[cfg(all(target_os = "linux", feature = "pipewire"))]
impl aerodesk_core::platform::MediaSource for WaylandPortalCapturer {
    type Error = String;

    fn start(&mut self, fps: u32, _with_cursor: bool) -> Result<(), Self::Error> {
        if self.thread.is_some() {
            return Ok(());
        }
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(8);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("aerodesk-wayland-capture".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("tokio runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(wayland_capture_loop(fps, frame_tx, ready_tx, stop2));
            })
            .map_err(|e| format!("spawn capture thread: {e}"))?;
        self.thread = Some(thread);
        self.stop = Some(stop);
        self.rx = Some(frame_rx);

        match ready_rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                self.stop();
                Err(e)
            }
            Err(_) => {
                self.stop();
                Err("wayland capture init timeout（portal 未响应）".into())
            }
        }
    }

    fn next_frame(&mut self) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        let Some(rx) = &self.rx else {
            return Ok(None);
        };
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(frame) => Ok(Some(aerodesk_core::platform::VideoFrame {
                platform: None,
                handle: None,
                raw: Some(frame.raw),
                width: frame.width,
                height: frame.height,
                pts_ms: frame.pts_ms,
            })),
            Err(_) => Ok(None), // 超时/断开 → 无新帧
        }
    }

    fn stop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // 不 join：portal 授权对话框未决时 join 会阻塞调用方；线程在
        // 下一轮 stop 检查或 portal 返回后自行退出（frame 循环每 200ms 检查）。
        self.thread.take();
        self.rx = None;
        self.stop = None;
    }
}

#[cfg(all(target_os = "linux", feature = "pipewire"))]
impl Drop for WaylandPortalCapturer {
    fn drop(&mut self) {
        <Self as aerodesk_core::platform::MediaSource>::stop(self);
    }
}

/// 非 pipewire 构建（macOS/Windows/Linux 默认）的编译期骨架。
#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
pub struct WaylandPortalCapturer;

#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
impl WaylandPortalCapturer {
    pub fn new() -> Result<Self, String> {
        Err(if cfg!(target_os = "linux") {
            "linux: PipeWire capture disabled (build with feature `pipewire`)".into()
        } else {
            "linux: PipeWire capture only available on Linux".into()
        })
    }
}

#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
impl aerodesk_core::platform::MediaSource for WaylandPortalCapturer {
    type Error = String;

    fn start(&mut self, _fps: u32, _with_cursor: bool) -> Result<(), Self::Error> {
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        Ok(None)
    }

    fn stop(&mut self) {}
}

/// 兼容别名：PipeWireCapturer = WaylandPortalCapturer。
pub type PipeWireCapturer = WaylandPortalCapturer;

#[cfg(all(test, target_os = "linux", feature = "pipewire"))]
mod linux_tests {
    use super::strip_stride;

    #[test]
    fn strip_stride_removes_padding() {
        let (w, h, stride) = (4u32, 2u32, 20u32);
        let mut data = vec![0u8; (h * stride) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = (y * stride + x * 4) as usize;
                data[i..i + 4].copy_from_slice(&[x as u8, y as u8, 7, 255]);
            }
        }
        let out = strip_stride(&data, w, h, stride);
        assert_eq!(out.len(), (w * h * 4) as usize);
        // 第一行像素 0..3
        assert_eq!(&out[0..4], &[0, 0, 7, 255]);
        assert_eq!(&out[12..16], &[3, 0, 7, 255]);
        // 第二行（stride 偏移 20）
        assert_eq!(&out[16..20], &[0, 1, 7, 255]);
        assert_eq!(&out[28..32], &[3, 1, 7, 255]);
    }

    #[test]
    fn strip_stride_noop_when_tight() {
        let data = vec![9u8; 32];
        assert_eq!(strip_stride(&data, 4, 2, 16), data);
        assert_eq!(strip_stride(&data, 4, 2, 0), data);
    }
}
