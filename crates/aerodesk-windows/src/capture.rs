//! 屏幕采集：DXGI Desktop Duplication（Win8+，Win10 回退路径）。
//!
//! D3D11 设备 → 输出复制 → 纹理读回 BGRA。Graphics Capture（Win10 1903+）
//! 为后续可选升级路径（同接口，输出帧格式一致）。

use crate::CapturedFrame;

/// DXGI Desktop Duplication 采集器（被控端，Windows）。
#[cfg(windows)]
pub struct DxgiCapturer {
    device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    duplication: windows::Win32::Graphics::Dxgi::IDXGIOutputDuplication,
    staging: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    width: u32,
    height: u32,
}

#[cfg(windows)]
impl DxgiCapturer {
    pub fn new() -> Result<Self, String> {
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
            D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
            ID3D11Texture2D,
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
                .EnumOutputs(0)
                .map_err(|e| format!("EnumOutputs: {e}"))?;
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

            Ok(Self {
                device,
                context,
                duplication,
                staging,
                width,
                height,
            })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 取下一帧（阻塞最多 16ms）。无新帧/错误返回 None。
    pub fn capture_frame(&mut self) -> Option<CapturedFrame> {
        use std::time::{SystemTime, UNIX_EPOCH};
        use windows::Win32::Graphics::Direct3D11::{D3D11_MAP_READ, ID3D11Texture2D};
        use windows::Win32::Graphics::Dxgi::IDXGIResource;
        use windows::core::Interface;

        unsafe {
            let mut info = windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            if self
                .duplication
                .AcquireNextFrame(16, &mut info, &mut resource)
                .is_err()
            {
                return None;
            }
            let Some(res) = resource else {
                let _ = self.duplication.ReleaseFrame();
                return None;
            };
            let tex: ID3D11Texture2D = match res.cast() {
                Ok(t) => t,
                Err(_) => {
                    let _ = self.duplication.ReleaseFrame();
                    return None;
                }
            };
            self.context.CopyResource(&self.staging, &tex);

            let mut mapped =
                windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
            if self
                .context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .is_err()
            {
                let _ = self.duplication.ReleaseFrame();
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
            self.context.Unmap(&self.staging, 0);
            let _ = self.duplication.ReleaseFrame();

            let pts_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_micros() as i64)
                .unwrap_or(0);
            Some(CapturedFrame {
                bgra,
                width: self.width,
                height: self.height,
                pts_us,
            })
        }
    }
}

#[cfg(windows)]
impl Drop for DxgiCapturer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.duplication.ReleaseFrame();
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
}

/// 非 Windows 主机上的编译期骨架（保证 workspace 全平台可编译）。
#[cfg(not(windows))]
pub struct DxgiCapturer;

#[cfg(not(windows))]
impl DxgiCapturer {
    pub fn new() -> Result<Self, String> {
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
