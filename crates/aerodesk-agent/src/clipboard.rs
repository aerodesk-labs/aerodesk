//! 剪贴板读写（thin wrapper，实现见 aerodesk-core::clipboard）。
//!
//! #503-2：图片剪贴板（PNG）与文本同源——read_image/write_image 透传 core
//! （Windows 原生 DIB / macOS NSPasteboard PNGf / Linux xclip/wl-copy）。
pub use aerodesk_core::clipboard::{cached, read, read_image, set_cache, write, write_image};
