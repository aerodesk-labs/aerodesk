//! AeroDesk UI 壳（Slint）：连接/房间/状态。
//!
//! 7 平台一套 UI（桌面/移动/Web）；鸿蒙走 ArkTS 壳 + Rust NAPI，UI 组件保持可迁移。
//! 连接逻辑复用 aerodesk-core（WsSignalClient + Endpoint），与 CLI/App 共用。

slint::include_modules!();

use std::net::UdpSocket;
use std::time::Duration;

use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::{Endpoint, signaling::WsSignalClient};
use aerodesk_protocol::signal::Role;
use str0m::net::Protocol;

fn main() -> Result<(), slint::PlatformError> {
    init_log();
    let ui = AppWindow::new()?;

    ui.on_connect({
        let weak = ui.as_weak();
        move || {
            let ui = weak.unwrap();
            let server = ui.get_server_input().to_string();
            let room = ui.get_room_input().to_string();
            ui.set_connecting(true);
            ui.set_status(format!("连接 {} @ {} …", server, room).into());
            let weak2 = weak.clone();
            std::thread::spawn(move || {
                let out = connect_viewer(&server, &room);
                let ui = weak2.unwrap();
                match out {
                    Ok((log, peer_id)) => {
                        ui.set_status(format!("已加入房间，peer={peer_id}").into());
                        ui.set_log(
                            format!(
                                "{log}\n\n已建立 WebRTC 会话（媒体收发循环由后续里程碑接入）。"
                            )
                            .into(),
                        );
                    }
                    Err(e) => {
                        ui.set_status(format!("连接失败：{e}").into());
                        ui.set_log(String::new().into());
                    }
                }
                ui.set_connecting(false);
            });
        }
    });

    ui.on_disconnect({
        let weak = ui.as_weak();
        move || {
            let ui = weak.unwrap();
            ui.set_status("已断开".into());
            ui.set_connecting(false);
        }
    });

    ui.run()
}

/// 观看端连接流程：WSS 信令 → join 房间 → SDP 交换（与 CLI viewer 相同的最小路径）。
fn connect_viewer(server: &str, room: &str) -> Result<(String, String), String> {
    let mut signal = WsSignalClient::connect(server).map_err(|e| format!("signal connect: {e}"))?;
    let (peer_id, turn) = signal
        .join(room, Role::Viewer, None)
        .map_err(|e| format!("join: {e}"))?;

    let socket = UdpSocket::bind("127.0.0.1:0").map_err(|e| format!("udp bind: {e}"))?;
    let addr = socket.local_addr().map_err(|e| e.to_string())?;
    let mut endpoint = Endpoint::new();
    endpoint
        .add_local_candidate(addr, Protocol::Udp)
        .map_err(|e| format!("candidate: {e:?}"))?;
    endpoint.add_video();
    let (offer, pending, video_mid) = endpoint
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

    // 泵 ICE 一小段时间，确认连接建立（媒体循环后续里程碑接入）。
    let mut connected = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok();
        let mut buf = [0u8; 2048];
        if let Ok((n, source)) = socket.recv_from(&mut buf) {
            let contents: &[u8] = &buf[..n];
            if let Ok(contents) = contents.try_into() {
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
    let _ = turn;
    let _ = video_mid;

    Ok((
        format!(
            "信令: {server}\n房间: {room}\n角色: viewer\nSDP 交换: OK\nICE: {}",
            if connected {
                "connected"
            } else {
                "pending(5s 超时)"
            }
        ),
        peer_id,
    ))
}

fn init_log() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aerodesk_ui=info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
}
