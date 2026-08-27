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

    // 观看会话（Rust 收流 → AnnexB 帧）；token=Digest 口令（#598 P1d 补通道）
    external fun viewerCreate(server: String, room: String, token: String?, forceRelay: Boolean): Long
    external fun viewerDestroy(ptr: Long)
    external fun viewerTakeAnnexB(ptr: Long): ByteArray
    external fun viewerSendInput(ptr: Long, json: String): Boolean

    // 发布会话（被控端：Kotlin 采集编码 → Rust 发送）；token=Digest 口令
    external fun publisherCreate(server: String, room: String, token: String?): Long
    external fun publisherDestroy(ptr: Long)
    external fun publisherFeedAnnexB(ptr: Long, frame: ByteArray, ptsUs: Long): Boolean
    external fun publisherTakeInput(ptr: Long): String
}
