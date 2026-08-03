package io.aerodesk.viewer

/**
 * Rust 侧 JNI 桥（aerodesk-android）。
 * 对应 crates/aerodesk-android/src/jni.rs。
 */
object NativeBridge {
    init {
        System.loadLibrary("aerodesk_android")
    }

    external fun version(): String
    external fun connect(server: String, room: String): String

    // 观看会话（Rust 收流 → AnnexB 帧）
    external fun viewerCreate(server: String, room: String): Long
    external fun viewerDestroy(ptr: Long)
    external fun viewerTakeAnnexB(ptr: Long): ByteArray
}
