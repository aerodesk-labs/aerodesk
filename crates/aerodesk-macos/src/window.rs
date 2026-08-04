//! macOS 窗口配置：保留原生红绿灯控制按钮，内容铺满窗口。
//!
//! 通过 NSWindow 隐藏标题栏文字、标题栏透明，并把窗口设为
//! full-size content view（内容延伸到标题栏区域），红绿灯按钮保留。

/// 隐藏标题栏文字 + 透明 + 内容铺满，保留红绿灯按钮。
///
/// 遍历 NSApp 的 windows，找到标题匹配的窗口设置。
/// 应在窗口显示后调用；`window_title` 为 Slint 的 `title`。
pub fn configure_fullsize_titlebar(window_title: &str) {
    use objc::runtime::{Object, YES};
    use objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let windows: *mut Object = msg_send![app, windows];
        let count: usize = msg_send![windows, count];
        for i in 0..count {
            let w: *mut Object = msg_send![windows, objectAtIndex: i];
            let title_obj: *mut Object = msg_send![w, title];
            if title_obj.is_null() {
                continue;
            }
            let s: *const i8 = msg_send![title_obj, UTF8String];
            if s.is_null() {
                continue;
            }
            let title = std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned();
            if title != window_title {
                continue;
            }
            // NSWindowTitleVisibility: NSWindowTitleHidden = 1
            let _: () = msg_send![w, setTitleVisibility: 1u64];
            let _: () = msg_send![w, setTitlebarAppearsTransparent: YES];
            // NSWindowStyleMask: NSFullSizeContentViewWindowMask = 1 << 15
            let mask: u64 = msg_send![w, styleMask];
            let _: () = msg_send![w, setStyleMask: mask | (1u64 << 15)];
        }
    }
}
