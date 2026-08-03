//! JNI 桥（aerodesk-core ↔ Kotlin/Java 壳）。
//!
//! 里程碑 1：版本 + 观看端连接（WSS 信令 + SDP 交换），供 Kotlin 壳在后台线程调用。
//! 后续：媒体收发循环（解码帧回调）、采集、输入注入。

use std::net::UdpSocket;
use std::time::Duration;

use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::jstring;

use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::{Endpoint, signaling::WsSignalClient};
use aerodesk_protocol::signal::Role;
use str0m::net::Protocol;

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
        .map(|s| s)
        .unwrap_or_else(|e| format!("连接失败: {e}"));
    env.new_string(status).expect("jstring alloc").into_raw()
}

/// 观看端最小连接流程：WSS join → SDP 交换 → ICE 泵（5s 超时）。
fn connect_viewer(server: &str, room: &str) -> Result<String, String> {
    let mut signal = WsSignalClient::connect(server).map_err(|e| format!("signal connect: {e}"))?;
    let (peer_id, _turn) = signal
        .join(room, Role::Viewer, None)
        .map_err(|e| format!("join: {e}"))?;

    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("udp bind: {e}"))?;
    let addr = socket.local_addr().map_err(|e| e.to_string())?;
    let mut endpoint = Endpoint::new();
    endpoint
        .add_local_candidate(addr, Protocol::Udp)
        .map_err(|e| format!("candidate: {e:?}"))?;
    endpoint.add_video();
    let (offer, pending, _video_mid) = endpoint
        .create_offer()
        .map_err(|e| format!("offer: {e:?}"))?;
    let offer_json = serde_json::to_string(&offer).map_err(|e| e.to_string())?;
    let answer_json = signal
        .exchange_description(&offer_json)
        .map_err(|e| format!("answer: {e}"))?;
    let answer: str0m::change::SdpAnswer =
        serde_json::from_str(&answer_json).map_err(|e| format!("answer parse: {e}"))?;
    endpoint
        .accept_answer(pending, answer)
        .map_err(|e| format!("accept: {e:?}"))?;

    let mut connected = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok();
        let mut buf = [0u8; 2048];
        if let Ok((n, source)) = socket.recv_from(&mut buf) {
            if let Ok(contents) = buf[..n].try_into() {
                let _ = endpoint.handle_input(str0m::Input::Receive(
                    std::time::Instant::now(),
                    str0m::net::Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: socket.local_addr().unwrap(),
                        contents,
                    },
                ));
            }
        }
        let _ = endpoint.handle_timeout(std::time::Instant::now());
        while let Some(output) = endpoint.poll_output() {
            if let str0m::Output::Transmit(t) = output {
                let _ = socket.send_to(&t.contents, t.destination);
            }
        }
        while let Some(ev) = endpoint.poll_event() {
            if let ClientEvent::IceConnected = ev {
                connected = true;
                break;
            }
        }
        if connected {
            break;
        }
    }

    Ok(format!(
        "peer={peer_id} room={room} sdp=ok ice={}",
        if connected {
            "connected"
        } else {
            "pending(5s 超时)"
        }
    ))
}
