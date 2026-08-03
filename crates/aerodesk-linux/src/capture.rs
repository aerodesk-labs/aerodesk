//! 屏幕采集（被控端）。
//!
//! X11 回退：x11rb `GetImage`（纯 Rust，无 C 依赖）读回 BGRA → RGBA。
//! Wayland：xdg-desktop-portal ScreenCast + PipeWire（真机阶段）。

use std::time::{SystemTime, UNIX_EPOCH};

use crate::CapturedFrame;

/// 采集器抽象（被控端）。
pub trait ScreenCapturer {
    fn next_frame(&mut self) -> Option<CapturedFrame>;
}

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
    pub fn next_frame(&mut self) -> Option<CapturedFrame> {
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
impl ScreenCapturer for X11Capturer {
    fn next_frame(&mut self) -> Option<CapturedFrame> {
        self.next_frame()
    }
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
impl ScreenCapturer for X11Capturer {
    fn next_frame(&mut self) -> Option<CapturedFrame> {
        None
    }
}

/// PipeWire 采集器占位（Wayland 真机阶段实现）。
pub struct PipeWireCapturer;

impl PipeWireCapturer {
    pub fn new() -> Result<Self, String> {
        Err("linux: PipeWire capture not implemented yet (P4)".into())
    }
}

impl ScreenCapturer for PipeWireCapturer {
    fn next_frame(&mut self) -> Option<CapturedFrame> {
        None
    }
}
