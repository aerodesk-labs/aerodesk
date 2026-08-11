//! C ABI（供 iOS Swift / 其他语言壳层调用）。
//!
//! 约定：
//! - `ad_decoder_decode` 返回的 CVPixelBufferRef 为 +1 保留，调用方负责
//!   `CVBufferRelease`（Swift 侧用 `CVPixelBuffer` 包装或 CIImage 渲染）。

use std::ffi::{c_char, c_int, c_void};

use crate::decode::H264Decoder;

const VERSION_C: &[u8] = concat!("aerodesk-ios ", env!("CARGO_PKG_VERSION"), "\0").as_bytes();

unsafe extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
}

/// 返回 SDK 版本字符串（nul 结尾，静态存储）。
#[unsafe(no_mangle)]
pub extern "C" fn ad_version() -> *const c_char {
    VERSION_C.as_ptr().cast()
}

/// 创建解码器实例，返回不透明句柄（`ad_decoder_free` 释放）。
#[unsafe(no_mangle)]
pub extern "C" fn ad_decoder_create() -> *mut H264Decoder {
    Box::into_raw(Box::new(H264Decoder::new()))
}

/// 释放解码器实例。
///
/// # Safety
/// `d` 必须来自 `ad_decoder_create` 且未被释放过。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ad_decoder_free(d: *mut H264Decoder) {
    if !d.is_null() {
        drop(unsafe { Box::from_raw(d) });
    }
}

/// 是否支持硬件解码。
#[unsafe(no_mangle)]
pub extern "C" fn ad_decoder_hardware() -> c_int {
    H264Decoder::is_hardware_supported() as c_int
}

/// 解码一帧 AnnexB H.264。
///
/// 返回值：
/// - `0`：有输出帧，`*out` 写入新的 +1 CVPixelBufferRef（调用方负责 CVBufferRelease）
/// - `1`：暂无输出（等待关键帧 / 空输入）
/// - `-1`：参数错误
/// - `-2`：解码失败
///
/// # Safety
/// `d` 必须来自 `ad_decoder_create`；`data`/`out` 必须有效；`len` 必须与 `data` 指向的缓冲区匹配。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ad_decoder_decode(
    d: *mut H264Decoder,
    data: *const u8,
    len: usize,
    pts: i64,
    out: *mut *mut c_void,
) -> c_int {
    if d.is_null() || data.is_null() || out.is_null() {
        return -1;
    }
    let decoder = unsafe { &mut *d };
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    match decoder.decode_annexb(slice, pts) {
        Ok(Some(buf)) => {
            let raw = buf.as_ptr();
            // +1 保留后返回给调用方；本地 wrapper drop 时 -1，净 +1 归调用方。
            unsafe { CFRetain(raw) };
            unsafe { *out = raw };
            0
        }
        Ok(None) => 1,
        Err(_) => -2,
    }
}

/// 观看端连接（阻塞调用，请在后台线程执行）。
/// 返回 malloc 分配的 C 字符串（用 `ad_free_string` 释放）。
///
/// # Safety
/// `server`/`room` 必须是以 NUL 结尾的有效 C 字符串。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ad_connect(server: *const c_char, room: *const c_char) -> *mut c_char {
    let server = if server.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(server) }
            .to_string_lossy()
            .into_owned()
    };
    let room = if room.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(room) }
            .to_string_lossy()
            .into_owned()
    };
    let status = aerodesk_core::connect::connect_viewer(&server, &room)
        .map(|r| r.summary())
        .unwrap_or_else(|e| format!("连接失败: {e}"));
    std::ffi::CString::new(status).unwrap().into_raw()
}

/// 释放 `ad_connect` 返回的字符串。
///
/// # Safety
/// `s` 必须来自 `ad_connect` 且未被释放过。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ad_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { std::ffi::CString::from_raw(s) });
    }
}

use crate::viewer::ViewerSession;

/// 创建观看会话：连接信令 + 启动后台收流解码线程。失败返回 null。
///
/// # Safety
/// `server`/`room` 必须是以 NUL 结尾的有效 C 字符串。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ad_viewer_create(
    server: *const c_char,
    room: *const c_char,
) -> *mut ViewerSession {
    let server = if server.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(server) }
            .to_string_lossy()
            .into_owned()
    };
    let room = if room.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(room) }
            .to_string_lossy()
            .into_owned()
    };
    match ViewerSession::connect(&server, &room) {
        Ok(s) => Box::into_raw(Box::new(s)),
        Err(e) => {
            // 模拟器/CI 冒烟诊断：连接失败原因直接打到 stderr（simctl launch --console 可见）。
            eprintln!("ad_viewer_create error: {e}");
            std::ptr::null_mut()
        }
    }
}

/// 销毁观看会话（停止收流线程）。
///
/// # Safety
/// `v` 必须来自 `ad_viewer_create` 且未被销毁过。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ad_viewer_destroy(v: *mut ViewerSession) {
    if !v.is_null() {
        drop(unsafe { Box::from_raw(v) });
    }
}

/// 取最新解码帧。
/// 返回 0=有新帧（`*out` 为 +1 CVPixelBufferRef，调用方负责 CVBufferRelease），
///      1=暂无新帧，<0=参数错误。
///
/// # Safety
/// `v` 必须来自 `ad_viewer_create`；`out` 必须有效。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ad_viewer_take_frame(
    v: *mut ViewerSession,
    out: *mut *mut c_void,
) -> c_int {
    if v.is_null() || out.is_null() {
        return -1;
    }
    unsafe { crate::viewer::take_frame(&*v, out) }
}

/// 取解码后的 PCM i16 音频样本（8kHz 单声道）。返回拷贝样本数（0=暂无）。
///
/// # Safety
/// `v` 必须来自 `ad_viewer_create`；`dst` 必须指向至少 `max` 个 i16 的有效空间。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ad_viewer_take_audio(
    v: *mut ViewerSession,
    dst: *mut i16,
    max: usize,
) -> c_int {
    if v.is_null() || dst.is_null() {
        return -1;
    }
    unsafe { crate::viewer::take_audio(&*v, dst, max) }
}

/// 发送输入事件（JSON InputFrame）到 input 数据通道。
/// 返回 0=已入队，<0=参数错误/会话无效。
///
/// # Safety
/// `v` 必须来自 `ad_viewer_create`；`json` 必须是以 NUL 结尾的 C 字符串。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ad_viewer_send_input(v: *mut ViewerSession, json: *const c_char) -> c_int {
    if v.is_null() || json.is_null() {
        return -1;
    }
    let json = unsafe { std::ffi::CStr::from_ptr(json) }.to_bytes();
    if unsafe { &*v }.send_input(json) {
        0
    } else {
        -2
    }
}
