//! iOS/iPad Slint UI 宿主（统一 Slint 的端侧入口）。
//!
//! 平台实现位于 [`aerodesk_platform::ios`]；本文件只负责启动 Slint UI。
//! Swift 壳层最终退化为生命周期宿主 + 系统桥，UI 由本模块渲染。

use slint::ComponentHandle;

slint::slint! {
    export component IosAppWindow inherits Window {
        title: "AeroDesk";
        Text { text: "AeroDesk"; }
    }
}

/// 供 Swift 调用的 Slint 启动入口（阻塞运行事件循环）。
#[unsafe(no_mangle)]
pub extern "C" fn ad_slint_run() {
    let app = IosAppWindow::new().unwrap();
    let _ = app.run();
}
