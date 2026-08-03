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
