//! macOS：点击 Dock 图标恢复隐藏的主窗口。
//!
//! Slint/winit 的窗口隐藏（orderOut）后，Dock 图标点击默认不会重新显示
//! 窗口，除非 `NSApplicationDelegate` 实现
//! `applicationShouldHandleReopen:hasVisibleWindows:` 返回 YES。
//! winit 已注册 `WinitApplicationDelegate`，这里直接在该类上补充该方法
//! （不替换 delegate，避免破坏 winit）。

use objc::runtime::{Class, Imp, Object, Sel};
use objc::{msg_send, sel, sel_impl};
use std::ffi::c_void;

/// 在 winit 的 `WinitApplicationDelegate` 类上添加 reopen 处理方法。
pub fn install_reopen_handler() {
    unsafe {
        let Some(cls) = Class::get("WinitApplicationDelegate") else {
            eprintln!("[dock] WinitApplicationDelegate not found; reopen skipped");
            return;
        };
        let method_sel = sel!(applicationShouldHandleReopen:hasVisibleWindows:);
        if cls.instance_method(method_sel).is_some() {
            return; // 已添加过
        }
        objc::runtime::class_addMethod(
            cls as *const Class as *mut Class,
            method_sel,
            std::mem::transmute::<*const (), Imp>(should_handle_reopen as *const ()),
            b"c@:c\0".as_ptr() as *const i8,
        );
        eprintln!("[dock] added reopen handler to WinitApplicationDelegate");
    }
}

unsafe extern "C" fn should_handle_reopen(
    _: &Object,
    _: Sel,
    _: &Object,
    _has_visible_windows: bool,
) -> bool {
    // 点击 Dock 图标：激活 App，让已打开的窗口回到最前（最小化窗口由
    // AppKit reopen 语义还原）。
    activate_app();
    true
}

/// 激活应用并忽略其它 App（配合托盘“显示主窗口”/Dock reopen 把窗口带到最前）。
pub fn activate_app() {
    unsafe {
        let Some(cls) = Class::get("NSRunningApplication") else {
            return;
        };
        let current: *mut Object = msg_send![cls, currentApplication];
        // NSApplicationActivateIgnoringOtherApps = 1 << 1
        let _: bool = msg_send![current, activateWithOptions: 2u64];
    }
}

/// 把 Slint/winit 的 NSView 所在窗口置前 + 还原最小化（“显示主窗口”菜单用）。
pub fn focus_ns_view(ns_view: *mut c_void) {
    unsafe {
        let view = ns_view as *mut Object;
        let window: *mut Object = msg_send![view, window];
        if !window.is_null() {
            let nil: *mut c_void = std::ptr::null_mut();
            let _: () = msg_send![window, makeKeyAndOrderFront: nil];
            let _: () = msg_send![window, deminiaturize: nil];
        }
        activate_app();
    }
}
