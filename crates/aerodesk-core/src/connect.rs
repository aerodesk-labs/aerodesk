//! 客户端连接流程（观看端/被控端共用）。
//!
//! WSS 信令 join → SDP 交换 → ICE 泵。供 CLI/UI/移动端壳层复用。

use std::net::UdpSocket;
use std::time::Duration;

use crate::endpoint::ClientEvent;
use crate::signaling::WsSignalClient;
use aerodesk_protocol::signal::Role;
use str0m::net::Protocol;

/// 连接结果摘要。
#[derive(Debug, Clone)]
pub struct ConnectResult {
    pub room: String,
    pub peer_id: String,
    pub ice_connected: bool,
}

impl ConnectResult {
    pub fn summary(&self) -> String {
        format!(
            "peer={} room={} sdp=ok ice={}",
            self.peer_id,
            self.room,
            if self.ice_connected {
                "connected"
            } else {
                "pending(5s 超时)"
            }
        )
    }
}

/// 观看端连接：WSS join → SDP 交换 → ICE 泵（5s 超时）。
pub fn connect_viewer(server: &str, room: &str) -> Result<ConnectResult, String> {
    connect(server, room, Role::Viewer)
}

/// 发布端连接（参数化角色）。
pub fn connect(server: &str, room: &str, role: Role) -> Result<ConnectResult, String> {
    let mut signal = WsSignalClient::connect(server).map_err(|e| format!("signal connect: {e}"))?;
    let (peer_id, _turn) = signal
        .join(room, role, None)
        .map_err(|e| format!("join: {e}"))?;

    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("udp bind: {e}"))?;
    let addr = socket.local_addr().map_err(|e| e.to_string())?;
    let mut endpoint = crate::Endpoint::new();
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

    let mut ice_connected = false;
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
                ice_connected = true;
                break;
            }
        }
        if ice_connected {
            break;
        }
    }

    Ok(ConnectResult {
        room: room.to_string(),
        peer_id,
        ice_connected,
    })
}
