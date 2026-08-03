//! JNI 桥（aerodesk-core ↔ Kotlin/Java 壳）。
//!
//! 里程碑 1：版本 + 观看端连接（WSS 信令 + SDP 交换），供 Kotlin 壳在后台线程调用。
//! 后续：媒体收发循环（解码帧回调）、采集、输入注入。

use std::net::UdpSocket;
use std::time::Duration;

use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::jstring;

use aerodesk_core::connect::connect_viewer;

const VERSION: &str = concat!("aerodesk-android ", env!("CARGO_PKG_VERSION"));

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_aerodesk_viewer_NativeBridge_version<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    env.new_string(VERSION).expect("jstring alloc").into_raw()
}

/// 观看端连接（阻塞调用，请在 Kotlin 后台线程执行）。
/// 返回状态文本（含 peer_id / ICE 状态）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_aerodesk_viewer_NativeBridge_connect<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    server: JString<'local>,
    room: JString<'local>,
) -> jstring {
    let server: String = env
        .get_string(&server)
        .map(|s| s.into())
        .unwrap_or_default();
    let room: String = env.get_string(&room).map(|s| s.into()).unwrap_or_default();
    let status = connect_viewer(&server, &room)
        .map(|r| r.summary())
        .unwrap_or_else(|e| format!("连接失败: {e}"));
    env.new_string(status).expect("jstring alloc").into_raw()
}
