import SwiftUI
import Dispatch

@main
struct AeroDeskApp: App {
    init() {
        // 实验性 Slint 宿主：`-slint` 启动参数切到 Rust Slint UI。
        // 默认仍走 SwiftUI 观看端，保持现有 ios-sim-e2e/真机链路不回归；
        // Slint winit 后端在主线程运行事件循环，这里异步调度避免阻塞 App 启动。
        if CommandLine.arguments.contains("-slint") {
            DispatchQueue.main.async {
                ad_slint_run()
            }
        }
    }

    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}
