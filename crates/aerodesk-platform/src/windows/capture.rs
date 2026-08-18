//! 屏幕采集：DXGI Desktop Duplication（Win8+，Win10 回退路径）。
//!
//! D3D11 设备 → 输出复制 → 纹理读回 BGRA。Graphics Capture（Win10 1903+）
//! 为后续可选升级路径（同接口，输出帧格式一致）。

use crate::windows::CapturedFrame;

/// DXGI Desktop Duplication 采集器（被控端，Windows）。
#[cfg(windows)]
pub struct DxgiCapturer {
    // Option 以便运行中切换显示器时先释放旧输出复制（同一 output 同时只允许一个
    // duplication，否则 DuplicateOutput 返回 E_INVALIDARG，#58）。
    context: Option<windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext>,
    duplication: Option<windows::Win32::Graphics::Dxgi::IDXGIOutputDuplication>,
    staging: Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
    /// 原生桌面宽高。
    width: u32,
    height: u32,
    /// 输出（缩放后）宽高；与原生不同时 capture_frame 做 CPU 双线性缩放。
    out_width: u32,
    out_height: u32,
    /// 被控显示器在虚拟屏幕中的区域（像素；多显示器坐标映射用，#75）。
    display_rect: (i32, i32, u32, u32),
    /// 当前采集的显示器索引（#58 运行中切换用）。
    display: u32,
    /// 缩放目标宽高（0 = 原生；切换显示器后保持同一输出分辨率，编码器无需重建）。
    target_w: u32,
    target_h: u32,
    /// #477 机制 B：DXGI 纯变化驱动，屏幕完全静止（合成器无呈现）时首帧永不到来
    /// （服务器端录制零字节实证）。用 GDI BitBlt 主动引导一次首帧。
    bootstrapped: bool,
}

#[cfg(windows)]
impl DxgiCapturer {
    /// 原生分辨率采集。
    pub fn new() -> Result<Self, String> {
        Self::new_with_scale(0, 0)
    }

    /// 按目标分辨率采集（0/0 = 原生；目标必须为偶数，适配 OpenH264 I420）。
    /// 软编路径在 4K 显示器下性能不足，缩放到 1080p/720p 后可用（#3）。
    pub fn new_with_scale(target_w: u32, target_h: u32) -> Result<Self, String> {
        Self::new_with_display(0, target_w, target_h)
    }

    /// 按显示器索引 + 目标分辨率采集（#75 多显示器；display=0 主显示器）。
    pub fn new_with_display(display: u32, target_w: u32, target_h: u32) -> Result<Self, String> {
        if (target_w != 0 || target_h != 0) && (target_w == 0 || target_h == 0) {
            return Err("scale target must be both set or both 0".into());
        }
        if target_w % 2 != 0 || target_h % 2 != 0 {
            return Err(format!("scale target must be even: {target_w}x{target_h}"));
        }
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
            D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
            ID3D11DeviceContext, ID3D11Texture2D,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
        };
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIOutput1};
        use windows::core::Interface;

        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
            let device = device.ok_or("no D3D11 device")?;
            let context = context.ok_or("no D3D11 context")?;

            let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapters1(0)
                .map_err(|e| format!("EnumAdapters1: {e}"))?;
            let output = adapter
                .EnumOutputs(display)
                .map_err(|e| format!("EnumOutputs({display}): {e}"))?;
            let output1: IDXGIOutput1 = output.cast().map_err(|e| format!("cast output1: {e}"))?;
            let desc = output.GetDesc().map_err(|e| format!("GetDesc: {e}"))?;
            let width =
                (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).max(0) as u32;
            let height =
                (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).max(0) as u32;
            if width == 0 || height == 0 {
                return Err("invalid desktop size".into());
            }
            let duplication = output1
                .DuplicateOutput(&device)
                .map_err(|e| format!("DuplicateOutput: {e}"))?;

            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                // STAGING 纹理不允许绑定标志（BindFlags 必须为 0）；
                // 此前误设 RENDER_TARGET 导致 CreateTexture2D 返回 E_INVALIDARG（0x80070057）。
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .map_err(|e| format!("CreateTexture2D: {e}"))?;
            let staging = staging.ok_or("no staging texture")?;

            let (out_width, out_height) = if target_w == 0 {
                (width, height)
            } else {
                (target_w, target_h)
            };
            let display_rect = (
                desc.DesktopCoordinates.left,
                desc.DesktopCoordinates.top,
                (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).max(0) as u32,
                (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).max(0) as u32,
            );
            Ok(Self {
                context: Some(context),
                duplication: Some(duplication),
                staging: Some(staging),
                width,
                height,
                out_width,
                out_height,
                display_rect,
                display,
                target_w,
                target_h,
                bootstrapped: false,
            })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.out_width, self.out_height)
    }

    /// 被控显示器在虚拟屏幕中的区域（像素；#75 注入坐标映射用）。
    pub fn display_rect(&self) -> (i32, i32, u32, u32) {
        self.display_rect
    }

    /// 运行中切换采集显示器（#58）：按新索引重建输出复制/暂存纹理，
    /// 保持目标输出分辨率（编码器无需重建）。失败时保持原采集不变。
    pub fn switch_display(&mut self, display: u32) -> Result<(), String> {
        let old_display = self.display;
        let target_w = self.target_w;
        let target_h = self.target_h;
        // 同一 output 同时只允许一个 duplication，且旧 D3D11 device 存活也会让
        // DuplicateOutput 返回 E_INVALIDARG：先彻底释放旧资源再重建。
        self.context = None;
        self.duplication = None;
        self.staging = None;
        match Self::new_with_display(display, target_w, target_h) {
            Ok(next) => {
                *self = next;
                Ok(())
            }
            Err(e) => {
                // 回退重建原显示器，尽量恢复会话（旧资源已释放，原显示器可重建）。
                if let Ok(prev) = Self::new_with_display(old_display, target_w, target_h) {
                    *self = prev;
                }
                Err(e)
            }
        }
    }

    /// #477 机制 B：GDI BitBlt 主动截取当前桌面作为首帧引导。
    /// DXGI duplication 纯变化驱动，桌面完全静止（无窗口/光标/时钟跳动触发
    /// 合成器呈现）时 AcquireNextFrame 永远超时——首帧缺失会级联成：发布循环
    /// 末帧缓存为空 → 心跳无从重发 → viewer 永远等不到首帧。仅引导一次；
    /// 失败返回 None（保持旧行为，首帧仍由首个真实变化帧提供）。
    fn gdi_bootstrap_frame(&mut self) -> Option<CapturedFrame> {
        use std::time::{SystemTime, UNIX_EPOCH};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap,
            CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits,
            ReleaseDC, SRCCOPY, SelectObject,
        };
        let (dx, dy, w, h) = self.display_rect;
        unsafe {
            let hdc_screen = GetDC(HWND::default());
            if hdc_screen.is_invalid() {
                return None;
            }
            let hdc_mem = CreateCompatibleDC(hdc_screen);
            let hbmp = CreateCompatibleBitmap(hdc_screen, w as i32, h as i32);
            if hbmp.is_invalid() || hdc_mem.is_invalid() {
                ReleaseDC(HWND::default(), hdc_screen);
                return None;
            }
            let old = SelectObject(hdc_mem, windows::Win32::Graphics::Gdi::HGDIOBJ(hbmp.0));
            let blit = BitBlt(
                hdc_mem, 0, 0, w as i32, h as i32, hdc_screen, dx, dy, SRCCOPY,
            );
            let mut bgra: Option<Vec<u8>> = None;
            if blit.is_ok() {
                // biHeight 取负 = top-down（DXGI 路径同为 top-down，消费方一致）。
                let mut bi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w as i32,
                        biHeight: -(h as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut buf = vec![0u8; w as usize * h as usize * 4];
                let got = GetDIBits(
                    hdc_mem,
                    hbmp,
                    0,
                    h,
                    Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                    &mut bi,
                    DIB_RGB_COLORS,
                );
                if got == h as i32 {
                    bgra = Some(buf);
                } else {
                    tracing::warn!("#477 GDI 引导 GetDIBits 失败：got={got} h={h}");
                }
            } else {
                let e = blit.err();
                tracing::warn!("#477 GDI 引导 BitBlt 失败：{e:?}");
            }
            SelectObject(hdc_mem, old);
            let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(hbmp.0));
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(HWND::default(), hdc_screen);

            let mut bgra = bgra?;
            // 32bpp DIB 为 BGRA（alpha 未定义，编码走 RGB 三分量不受影响）。
            if self.out_width != w || self.out_height != h {
                bgra = scale_bgra(&bgra, w, h, self.out_width, self.out_height);
            }
            let pts_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_micros() as i64)
                .unwrap_or(0);
            Some(CapturedFrame {
                bgra,
                width: self.out_width,
                height: self.out_height,
                pts_us,
            })
        }
    }

    /// 取下一帧（阻塞最多 16ms）。无新帧/错误返回 None。
    pub fn capture_frame(&mut self) -> Option<CapturedFrame> {
        use std::time::{SystemTime, UNIX_EPOCH};
        use windows::Win32::Graphics::Direct3D11::{D3D11_MAP_READ, ID3D11Texture2D};
        use windows::Win32::Graphics::Dxgi::IDXGIResource;
        use windows::core::Interface;

        unsafe {
            let Some(duplication) = &self.duplication else {
                return None;
            };
            let Some(staging) = &self.staging else {
                return None;
            };
            let Some(context) = &self.context else {
                return None;
            };
            let mut info = windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            if let Err(e) = duplication.AcquireNextFrame(16, &mut info, &mut resource) {
                // WAIT_TIMEOUT（静止屏）是常态；其余错误（ACCESS_LOST 等）首现告警——
                // 全错误静默曾让「发布端零帧」排查无门（虚拟显示驱动致盲实例）。
                if e.code().0 as u32 != 0x887A0027 {
                    static ACQ_WARNED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !ACQ_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        tracing::warn!("DXGI AcquireNextFrame 非超时错误: code={:?} {e}", e.code());
                    }
                }
                // #477 机制 B：首个变化帧到来之前用 GDI 引导当前桌面内容，
                // 让发布循环拿到首帧（并进入心跳维护）；仅此一次。
                if !self.bootstrapped {
                    self.bootstrapped = true;
                    let f = self.gdi_bootstrap_frame();
                    if f.is_some() {
                        tracing::info!("#477 GDI 首帧引导成功");
                    } else {
                        tracing::warn!("#477 GDI 首帧引导失败（回退等待首个变化帧）");
                    }
                    return f;
                }
                return None;
            }
            self.bootstrapped = true;
            let Some(res) = resource else {
                let _ = duplication.ReleaseFrame();
                return None;
            };
            let tex: ID3D11Texture2D = match res.cast() {
                Ok(t) => t,
                Err(_) => {
                    let _ = duplication.ReleaseFrame();
                    return None;
                }
            };
            context.CopyResource(staging, &tex);

            let mut mapped =
                windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
            if context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .is_err()
            {
                let _ = duplication.ReleaseFrame();
                return None;
            }
            let row_pitch = mapped.RowPitch as usize;
            let src = std::slice::from_raw_parts(
                mapped.pData as *const u8,
                self.height as usize * row_pitch,
            );
            let mut bgra = Vec::with_capacity(self.width as usize * self.height as usize * 4);
            for y in 0..self.height as usize {
                let row = &src[y * row_pitch..y * row_pitch + self.width as usize * 4];
                bgra.extend_from_slice(row);
            }
            context.Unmap(staging, 0);
            let _ = duplication.ReleaseFrame();

            let pts_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_micros() as i64)
                .unwrap_or(0);
            let bgra = if self.out_width == self.width && self.out_height == self.height {
                bgra
            } else {
                scale_bgra(
                    &bgra,
                    self.width,
                    self.height,
                    self.out_width,
                    self.out_height,
                )
            };
            Some(CapturedFrame {
                bgra,
                width: self.out_width,
                height: self.out_height,
                pts_us,
            })
        }
    }
}

#[cfg(windows)]
impl Drop for DxgiCapturer {
    fn drop(&mut self) {
        unsafe {
            if let Some(d) = &self.duplication {
                let _ = d.ReleaseFrame();
            }
        }
    }
}

#[cfg(windows)]
impl aerodesk_core::platform::MediaSource for DxgiCapturer {
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

    fn stop(&mut self) {}

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

/// 非 Windows 主机上的编译期骨架（保证 workspace 全平台可编译）。
#[cfg(not(windows))]
pub struct DxgiCapturer;

#[cfg(not(windows))]
impl DxgiCapturer {
    pub fn new() -> Result<Self, String> {
        Err("windows: DXGI capture only available on Windows".into())
    }

    /// 非 Windows 骨架：返回 Err（编译期占位）。
    pub fn new_with_scale(_target_w: u32, _target_h: u32) -> Result<Self, String> {
        Err("windows: DXGI capture only available on Windows".into())
    }

    /// 非 Windows 骨架：返回 Err（编译期占位）。
    pub fn new_with_display(_display: u32, _target_w: u32, _target_h: u32) -> Result<Self, String> {
        Err("windows: DXGI capture only available on Windows".into())
    }

    pub fn size(&self) -> (u32, u32) {
        (0, 0)
    }
}

#[cfg(not(windows))]
impl aerodesk_core::platform::MediaSource for DxgiCapturer {
    type Error = String;

    fn start(&mut self, _fps: u32, _with_cursor: bool) -> Result<(), Self::Error> {
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        Ok(None)
    }

    fn stop(&mut self) {}
}

/// CPU 双线性缩放 BGRA32（DXGI 原生 → 目标分辨率，适配 OpenH264 软编；#3）。
fn scale_bgra(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let mut out = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        let sy = (y as f64 * sh as f64 / dh as f64).min(sh as f64 - 1.0);
        let y0 = sy.floor() as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let fy = sy - y0 as f64;
        let row0 = y0 * sw * 4;
        let row1 = y1 * sw * 4;
        for x in 0..dw {
            let sx = (x as f64 * sw as f64 / dw as f64).min(sw as f64 - 1.0);
            let x0 = sx.floor() as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let fx = sx - x0 as f64;
            let di = (y * dw + x) * 4;
            let (i00, i10, i01, i11) = (row0 + x0 * 4, row0 + x1 * 4, row1 + x0 * 4, row1 + x1 * 4);
            for c in 0..4 {
                let top = src[i00 + c] as f64 * (1.0 - fx) + src[i10 + c] as f64 * fx;
                let bot = src[i01 + c] as f64 * (1.0 - fx) + src[i11 + c] as f64 * fx;
                let v = top * (1.0 - fy) + bot * fy;
                out[di + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_bgra_halves_size_and_preserves_color() {
        // 4x4 纯红（BGRA: B=0,G=0,R=255,A=255）→ 2x2。
        let src = [255u8, 0, 0, 255].repeat(16);
        let out = scale_bgra(&src, 4, 4, 2, 2);
        assert_eq!(out.len(), 2 * 2 * 4);
        for px in out.chunks(4) {
            assert_eq!(px, [255, 0, 0, 255], "纯色缩放应保持颜色");
        }
    }

    #[cfg(windows)]
    #[test]
    fn new_with_scale_odd_target_rejected() {
        // 奇数目标拒绝（OpenH264 I420 需要偶数）；真机无桌面时 0/0=原生可能失败。
        assert!(
            DxgiCapturer::new_with_scale(1, 1).is_err(),
            "奇数目标应拒绝"
        );
    }
}
