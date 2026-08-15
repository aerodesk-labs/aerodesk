//! Android Slint UI 宿主（统一 Slint 的端侧入口）。
//!
//! 平台实现位于 [`aerodesk_platform::android`]；本文件只负责启动 Slint UI。
//! Kotlin 壳层退化为系统垫片（权限、前台服务、MediaCodec 采集/解码），
//! UI 由本模块通过 `android-activity`/NativeActivity 渲染。

use slint::ComponentHandle;

slint::slint! {
    export component AndroidAppWindow inherits Window {
        title: "AeroDesk";
        Text { text: "AeroDesk Android Slint"; }
    }
}

/// NativeActivity 的 Rust 入口：初始化 Slint Android 后端并运行事件循环。
///
/// 由 `android.app.NativeActivity`（或派生类）在加载
/// `libaerodesk_android.so` 后调用，对应 `ANativeActivity_onCreate` 入口。
#[unsafe(no_mangle)]
pub extern "C" fn android_main(app: slint::android::AndroidApp) {
    slint::android::init(app).expect("failed to initialize Slint Android backend");
    let window = AndroidAppWindow::new().expect("failed to create Slint window");
    let _ = window.run();
}
