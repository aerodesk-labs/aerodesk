package io.aerodesk.viewer

import android.app.NativeActivity
import android.os.Bundle

/**
 * Slint UI 的 NativeActivity 宿主。
 *
 * Rust 侧 `android_main`（aerodesk-android/src/ui.rs）由 NativeActivity 加载
 * `libaerodesk_android.so` 后触发；Kotlin 只保留系统垫片职责。
 */
class SlintActivity : NativeActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
    }
}