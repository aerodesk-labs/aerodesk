//! macOS 摄像头采集（AVFoundation，objc2-av-foundation 0.3）。
//!
//! 真实实现 `CameraSource`：AVCaptureSession + AVCaptureVideoDataOutput，
//! 输出 BGRA32 原始帧（`CameraFrame`）。摄像头帧没有 IOSurface 零拷贝通道，
//! 编码走 FFmpeg 软编（或 VideoToolbox raw-BGRA 输入路径）。
//!
//! 权限：macOS TCC「相机」；未授权时 `start` 返回明确错误并提示系统设置路径。

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::time::{SystemTime, UNIX_EPOCH};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput,
    AVCaptureOutput, AVCaptureSession, AVCaptureSessionPresetHigh, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};

use aerodesk_core::platform::{CameraFrame, CameraSource};

/// 摄像头设备信息。
#[derive(Debug, Clone)]
pub struct CameraDeviceInfo {
    pub id: String,
    pub name: String,
}

/// 枚举系统摄像头（AVMediaTypeVideo）。
#[allow(deprecated)] // devicesWithMediaType（DiscoverySession 后续批次）
pub fn list_cameras() -> Vec<CameraDeviceInfo> {
    let Some(media_type) = (unsafe { AVMediaTypeVideo }) else {
        return Vec::new();
    };
    let devices = unsafe { AVCaptureDevice::devicesWithMediaType(media_type) };
    devices
        .iter()
        .map(|d| CameraDeviceInfo {
            id: unsafe { d.uniqueID() }.to_string(),
            name: unsafe { d.localizedName() }.to_string(),
        })
        .collect()
}

/// 相机权限是否已授权（TCC）。
pub fn camera_authorized() -> bool {
    let Some(media_type) = (unsafe { AVMediaTypeVideo }) else {
        return false;
    };
    (unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) })
        == AVAuthorizationStatus::Authorized
}

/// 触发相机权限请求（未决定时弹系统授权框）。返回是否已授权。
pub fn request_camera_access() -> bool {
    let Some(media_type) = (unsafe { AVMediaTypeVideo }) else {
        return false;
    };
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
    if status == AVAuthorizationStatus::Authorized {
        return true;
    }
    if status != AVAuthorizationStatus::NotDetermined {
        return false;
    }
    // 同步等用户应答：block 在任意队列回调，用 channel 等结果。
    let (tx, rx) = std::sync::mpsc::sync_channel::<bool>(1);
    let handler = block2::RcBlock::new(move |granted: Bool| {
        let _ = tx.try_send(granted.as_bool());
    });
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &handler);
    }
    rx.recv_timeout(std::time::Duration::from_secs(30))
        .unwrap_or(false)
}

/// 从系统设置打开「相机」隐私面板。
pub fn open_camera_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Camera")
        .spawn();
}

// ---------- AVCaptureVideoDataOutput 采样委托 ----------

struct CameraDelegateIvars {
    /// 发送 BGRA 帧到采集循环；`SyncSender` 限深 4，满则丢帧。
    sender: Mutex<SyncSender<CameraFrame>>,
}

define_class!(
    // SAFETY:
    // - NSObject 无子类化要求。
    // - ivars 全部 Send+Sync（Mutex<SyncSender>），类自动线程安全。
    #[unsafe(super(NSObject))]
    #[ivars = CameraDelegateIvars]
    struct CameraDelegate;

    unsafe impl NSObjectProtocol for CameraDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CameraDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn capture_output_did_output_sample_buffer_from_connection(
            &self,
            _output: &AVCaptureOutput,
            sample_buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            let Some(image) = (unsafe { sample_buffer.image_buffer() }) else {
                return;
            };
            let width = CVPixelBufferGetWidth(&image);
            let height = CVPixelBufferGetHeight(&image);
            if width == 0 || height == 0 {
                return;
            }
            let row_bytes = CVPixelBufferGetBytesPerRow(&image);
            if unsafe { CVPixelBufferLockBaseAddress(&image, CVPixelBufferLockFlags(0)) } != 0 {
                return;
            }
            let base = CVPixelBufferGetBaseAddress(&image);
            if base.is_null() {
                unsafe { CVPixelBufferUnlockBaseAddress(&image, CVPixelBufferLockFlags(0)) };
                return;
            }
            let mut raw = vec![0u8; width * height * 4];
            let src = base as *const u8;
            // 行可能带 padding，逐行拷成紧凑 BGRA。
            for y in 0..height {
                let line = unsafe { std::slice::from_raw_parts(src.add(y * row_bytes), width * 4) };
                raw[y * width * 4..(y + 1) * width * 4].copy_from_slice(line);
            }
            unsafe { CVPixelBufferUnlockBaseAddress(&image, CVPixelBufferLockFlags(0)) };
            let pts_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let frame = CameraFrame {
                raw,
                width: width as u32,
                height: height as u32,
                pts_ms,
            };
            if let Ok(sender) = self.ivars().sender.lock() {
                // 满则丢帧（try_send），避免阻塞采集回调。
                let _ = sender.try_send(frame);
            }
        }
    }
);

impl CameraDelegate {
    fn new(sender: SyncSender<CameraFrame>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(CameraDelegateIvars {
            sender: Mutex::new(sender),
        });
        // SAFETY: NSObject init 无额外要求。
        unsafe { msg_send![super(this), init] }
    }
}

// ---------- MacCamera（CameraSource trait 实现） ----------

/// macOS AVFoundation 摄像头源。
pub struct MacCamera {
    device_id: Option<String>,
    session: Option<Retained<AVCaptureSession>>,
    _delegate: Option<Retained<CameraDelegate>>,
    _output: Option<Retained<AVCaptureVideoDataOutput>>,
    receiver: Option<Receiver<CameraFrame>>,
    running: bool,
}

impl MacCamera {
    pub fn new() -> Self {
        Self {
            device_id: None,
            session: None,
            _delegate: None,
            _output: None,
            receiver: None,
            running: false,
        }
    }

    /// 指定摄像头设备（`list_cameras()` 的 id；默认第一个可用摄像头）。
    pub fn with_device(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = Some(device_id.into());
        self
    }
}

impl Default for MacCamera {
    fn default() -> Self {
        Self::new()
    }
}

impl CameraSource for MacCamera {
    type Error = String;

    fn start(&mut self, _width: u32, _height: u32, _fps: u32) -> Result<(), Self::Error> {
        if self.running {
            return Ok(());
        }
        if !camera_authorized() {
            return Err(
                "macos: 相机权限未授权（System Settings > Privacy & Security > Camera），\
                 请先授权后重试"
                    .into(),
            );
        }
        let cameras = list_cameras();
        let device = if let Some(id) = &self.device_id {
            cameras
                .iter()
                .find(|c| &c.id == id)
                .ok_or_else(|| format!("macos: 找不到摄像头设备 {id}"))?
        } else {
            cameras
                .first()
                .ok_or_else(|| "macos: 未发现可用摄像头".to_string())?
        };
        let device_obj =
            unsafe { AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(&device.id)) }
                .ok_or_else(|| format!("macos: 摄像头设备 {} 已断开", device.id))?;
        let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device_obj) }
            .map_err(|e| format!("macos: 打开摄像头失败: {e:?}"))?;

        let session = unsafe { AVCaptureSession::new() };
        if unsafe { session.canSetSessionPreset(&AVCaptureSessionPresetHigh) } {
            unsafe { session.setSessionPreset(&AVCaptureSessionPresetHigh) };
        }
        if !unsafe { session.canAddInput(&input) } {
            return Err("macos: 会话无法添加摄像头输入".into());
        }
        unsafe { session.addInput(&input) };

        let output = unsafe { AVCaptureVideoDataOutput::new() };
        // 请求 BGRA32 输出（key 为 CFString，与 NSString toll-free bridge）。
        let key_ns: &NSString = unsafe {
            &*(kCVPixelBufferPixelFormatTypeKey as *const objc2_core_foundation::CFString
                as *const NSString)
        };
        let value = NSNumber::numberWithUnsignedInt(kCVPixelFormatType_32BGRA);
        let settings: Retained<NSDictionary<NSString, objc2::runtime::AnyObject>> = unsafe {
            NSDictionary::<NSString, objc2::runtime::AnyObject>::dictionaryWithObject_forKey(
                &*value,
                ProtocolObject::from_ref(key_ns),
            )
        };
        unsafe { output.setVideoSettings(Some(&settings)) };

        let (tx, rx) = sync_channel(4);
        let delegate = CameraDelegate::new(tx);
        let queue = DispatchQueue::new("aerodesk.camera", None);
        unsafe {
            output.setSampleBufferDelegate_queue(
                Some(ProtocolObject::from_ref(&*delegate)),
                Some(&queue),
            )
        };
        if !unsafe { session.canAddOutput(&output) } {
            return Err("macos: 会话无法添加摄像头输出".into());
        }
        unsafe { session.addOutput(&output) };

        unsafe { session.startRunning() };
        if !unsafe { session.isRunning() } {
            unsafe { session.stopRunning() };
            return Err("macos: 摄像头会话启动失败（可能被其它应用占用）".into());
        }

        self.session = Some(session);
        self._delegate = Some(delegate);
        self._output = Some(output);
        self.receiver = Some(rx);
        self.running = true;
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<CameraFrame>, Self::Error> {
        let Some(rx) = &self.receiver else {
            return Err("macos: camera not started".into());
        };
        match rx.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.running = false;
                Err("macos: camera delegate disconnected".into())
            }
        }
    }

    fn stop(&mut self) {
        if let Some(session) = &self.session {
            unsafe { session.stopRunning() };
        }
        self.session = None;
        self._delegate = None;
        self._output = None;
        self.receiver = None;
        self.running = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_device_list_shape() {
        // 只验证接口可调用（无摄像头/无权限时不崩溃）。
        let _ = list_cameras();
    }
}
