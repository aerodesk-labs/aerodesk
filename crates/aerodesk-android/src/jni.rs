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

use crate::viewer::ViewerSession;
use jni::objects::JByteArray;
use jni::sys::{jbyteArray, jlong};

/// 创建观看会话（连接 + 后台收流）。返回指针（jlong），失败为 0。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_aerodesk_viewer_NativeBridge_viewerCreate<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    server: JString<'local>,
    room: JString<'local>,
) -> jlong {
    let server: String = env
        .get_string(&server)
        .map(|s| s.into())
        .unwrap_or_default();
    let room: String = env.get_string(&room).map(|s| s.into()).unwrap_or_default();
    match ViewerSession::connect(&server, &room) {
        Ok(v) => Box::into_raw(Box::new(v)) as jlong,
        Err(_) => 0,
    }
}

/// 销毁观看会话。
///
/// # Safety
/// `ptr` 必须来自 viewerCreate 且未被销毁过。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_io_aerodesk_viewer_NativeBridge_viewerDestroy<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    ptr: jlong,
) {
    if ptr != 0 {
        drop(unsafe { Box::from_raw(ptr as *mut ViewerSession) });
    }
}

/// 取最新 AnnexB H.264 帧（空数组表示暂无）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_aerodesk_viewer_NativeBridge_viewerTakeAnnexB<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    ptr: jlong,
) -> jbyteArray {
    if ptr == 0 {
        return env.new_byte_array(0).expect("array").into_raw();
    }
    let v = unsafe { &*(ptr as *mut ViewerSession) };
    match v.take_annexb() {
        Some(frame) => {
            let arr = env.new_byte_array(frame.len() as i32).expect("byte array");
            let i8slice =
                unsafe { std::slice::from_raw_parts(frame.as_ptr() as *const i8, frame.len()) };
            let _ = env.set_byte_array_region(&arr, 0, i8slice);
            arr.into_raw()
        }
        None => env.new_byte_array(0).expect("byte array").into_raw(),
    }
}
