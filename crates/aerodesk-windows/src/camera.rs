//! Windows 摄像头采集（Media Foundation SourceReader，CameraSource 实现）。
//!
//! 枚举视频采集设备（MFEnumDeviceSources + VIDCAP_GUID），SourceReader 输出
//! RGB32（内存顺序 BGRA），与 Linux/macOS `CameraFrame.raw` 的 BGRA 约定对齐。
//! 无摄像头环境 `new()`/`start()` 返回明确错误，调用方（CLI）仅告警并继续视频轨。

use aerodesk_core::platform::{CameraFrame, CameraSource};

/// 枚举本机摄像头（索引, 友好名称；配合 `--camera-device <索引>` 选择）。
#[cfg(windows)]
pub fn list_cameras() -> Vec<(String, String)> {
    let Ok(devices) = enumerate_devices() else {
        return Vec::new();
    };
    devices
        .iter()
        .enumerate()
        .map(|(i, act)| {
            let name = friendly_name(act).unwrap_or_else(|| format!("camera {i}"));
            (i.to_string(), name)
        })
        .collect()
}

/// MF 枚举视频采集设备（返回 IMFActivate 列表，调用方负责 CoTaskMemFree 数组）。
#[cfg(windows)]
fn enumerate_devices() -> Result<Vec<windows::Win32::Media::MediaFoundation::IMFActivate>, String> {
    use windows::Win32::Media::MediaFoundation::{IMFAttributes, MFCreateAttributes};
    use windows::Win32::Media::MediaFoundation::{
        MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        MFEnumDeviceSources,
    };
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};

    // 已在 MTA 时返回 RPC_E_CHANGED_MODE，可忽略（MF 仍可用）。
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(|e| format!("MFCreateAttributes: {e}"))?;
        let attrs = attrs.ok_or("no IMFAttributes")?;
        attrs
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|e| format!("SetGUID(VIDCAP): {e}"))?;

        let mut devices: *mut Option<windows::Win32::Media::MediaFoundation::IMFActivate> =
            std::ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attrs, &mut devices, &mut count)
            .map_err(|e| format!("MFEnumDeviceSources: {e}"))?;
        if count == 0 || devices.is_null() {
            return Ok(Vec::new());
        }
        // SAFETY: MFEnumDeviceSources 成功后 devices 指向 count 个 IMFActivate。
        let list = std::slice::from_raw_parts(devices, count as usize)
            .iter()
            .filter_map(|a| a.as_ref().cloned())
            .collect::<Vec<_>>();
        // SAFETY: 释放 MFEnumDeviceSources 分配的数组（元素已克隆，引用计数+1）。
        CoTaskMemFree(Some(devices as *const core::ffi::c_void));
        Ok(list)
    }
}

/// 设备友好名称（MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME）。
#[cfg(windows)]
fn friendly_name(act: &windows::Win32::Media::MediaFoundation::IMFActivate) -> Option<String> {
    use windows::Win32::Media::MediaFoundation::MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME;
    let mut buf = vec![0u16; 256];
    unsafe { act.GetString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut buf, None) }.ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Windows MF 摄像头采集器（SourceReader → RGB32/BGRA）。
#[cfg(windows)]
pub struct MfCamera {
    reader: windows::Win32::Media::MediaFoundation::IMFSourceReader,
    width: u32,
    height: u32,
}

#[cfg(windows)]
impl MfCamera {
    /// 打开摄像头设备（`device` 为 `--list-cameras` 输出的索引，默认 0）。
    pub fn new(device: Option<&str>) -> Result<Self, String> {
        use windows::Win32::Media::MediaFoundation::{
            MFCreateDeviceSource, MFCreateSourceReaderFromMediaSource,
        };
        let acts = enumerate_devices()?;
        if acts.is_empty() {
            return Err("no camera device".into());
        }
        let idx: usize = device.and_then(|d| d.parse().ok()).unwrap_or(0);
        let act = acts
            .get(idx)
            .cloned()
            .ok_or_else(|| format!("camera index {idx} out of range"))?;
        let source = unsafe { MFCreateDeviceSource(&act) }
            .map_err(|e| format!("MFCreateDeviceSource: {e}"))?;
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, None) }
            .map_err(|e| format!("MFCreateSourceReaderFromMediaSource: {e}"))?;
        Ok(Self {
            reader,
            width: 0,
            height: 0,
        })
    }

    /// 启动采集：SourceReader 输出 RGB32（BGRA），分辨率/帧率为尽力设置。
    pub fn start(&mut self, width: u32, height: u32, fps: u32) -> Result<(), String> {
        use windows::Win32::Media::MediaFoundation::{
            MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
            MF_SOURCE_READER_FIRST_VIDEO_STREAM, MFCreateMediaType, MFMediaType_Video,
            MFVideoFormat_RGB32,
        };
        let mt = unsafe { MFCreateMediaType() }.map_err(|e| format!("MFCreateMediaType: {e}"))?;
        unsafe {
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| format!("SetGUID(MAJOR): {e}"))?;
            mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(|e| format!("SetGUID(SUBTYPE): {e}"))?;
            // 尽力设置分辨率/帧率；设备不支持时 SourceReader 用最近模式并转换。
            let _ = mt.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64);
            let _ = mt.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1);
        }
        let first = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        unsafe { self.reader.SetCurrentMediaType(first, None, &mt) }
            .map_err(|e| format!("SetCurrentMediaType: {e}"))?;
        let cur = unsafe { self.reader.GetCurrentMediaType(first) }
            .map_err(|e| format!("GetCurrentMediaType: {e}"))?;
        // 实际输出分辨率以当前媒体类型为准（设备模式可能不同于请求值）。
        let size = unsafe { cur.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
        let w = (size >> 32) as u32;
        let h = (size & 0xFFFF_FFFF) as u32;
        self.width = if w > 0 && h > 0 { w } else { width };
        self.height = if w > 0 && h > 0 { h } else { height };
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<CameraFrame>, String> {
        use windows::Win32::Media::MediaFoundation::{
            IMFSample, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
        };
        let first = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let mut flags = 0u32;
        let mut ts: i64 = 0;
        let mut sample: Option<IMFSample> = None;
        unsafe {
            self.reader.ReadSample(
                first,
                0,
                None,
                Some(&mut flags as *mut u32),
                Some(&mut ts as *mut i64),
                Some(&mut sample as *mut Option<IMFSample>),
            )
        }
        .map_err(|e| format!("ReadSample: {e}"))?;
        let Some(sample) = sample else {
            return Ok(None);
        };
        let buf = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len = 0u32;
        let mut len = 0u32;
        unsafe {
            buf.Lock(
                &mut ptr,
                Some(&mut max_len as *mut u32),
                Some(&mut len as *mut u32),
            )
        }
        .map_err(|e| format!("Lock: {e}"))?;
        let raw = if ptr.is_null() {
            Vec::new()
        } else {
            // SAFETY: Lock 成功后 ptr 指向 len 字节有效缓冲区（ReadSample 样本生命周期内）。
            unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec()
        };
        unsafe { buf.Unlock() }.ok();
        Ok(Some(CameraFrame {
            raw,
            width: self.width,
            height: self.height,
            pts_ms: if ts > 0 { (ts / 10_000) as u64 } else { 0 },
        }))
    }

    fn stop(&mut self) {
        use windows::Win32::Media::MediaFoundation::MF_SOURCE_READER_FIRST_VIDEO_STREAM;
        let first = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let _ = unsafe { self.reader.SetStreamSelection(first, false) };
    }
}

#[cfg(windows)]
impl CameraSource for MfCamera {
    type Error = String;

    fn start(&mut self, width: u32, height: u32, fps: u32) -> Result<(), Self::Error> {
        self.start(width, height, fps)
    }

    fn next_frame(&mut self) -> Result<Option<CameraFrame>, Self::Error> {
        self.next_frame()
    }

    fn stop(&mut self) {
        self.stop();
    }
}

/// 非 Windows 主机上的编译期骨架（保证 workspace 全平台可编译）。
#[cfg(not(windows))]
pub struct MfCamera;

#[cfg(not(windows))]
impl MfCamera {
    pub fn new(_device: Option<&str>) -> Result<Self, String> {
        Err("windows: MF camera only available on Windows".into())
    }
}

#[cfg(not(windows))]
impl CameraSource for MfCamera {
    type Error = String;

    fn start(&mut self, _width: u32, _height: u32, _fps: u32) -> Result<(), Self::Error> {
        Err("windows: MF camera only available on Windows".into())
    }

    fn next_frame(&mut self) -> Result<Option<CameraFrame>, Self::Error> {
        Ok(None)
    }

    fn stop(&mut self) {}
}
