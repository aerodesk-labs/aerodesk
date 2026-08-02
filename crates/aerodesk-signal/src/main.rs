//! AeroDesk 信令服务（WSS）。
//!
//! 职责：认证、房间管理、TURN 凭证下发、WebRTC offer/answer 代理到 SFU。
//! 协议见 `aerodesk-protocol::signal`。
//!
//! 环境变量：
//!   SIGNAL_PORT   WSS 端口（默认 3001）
//!   AUTH_TOKENS   逗号分隔合法 token（空则不认证）
//!   TURN_SECRET   coturn REST secret（空则不下发 TURN）
//!   TURN_URLS     逗号分隔 TURN URL（默认 127.0.0.1:3478）
//!   SFU_URL       SFU 内部接口（默认 http://127.0.0.1:3002）
//!   SFU_TOKEN     SFU 内部接口 token（可选）

#[macro_use]
extern crate tracing;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aerodesk_protocol::signal::{PeerInfo, Role, SignalMessage, TurnConfig};
use rouille::websocket::{self, Message, Websocket};
use rouille::{Request, Response};

struct Config {
    auth_tokens: Vec<String>,
    turn: Option<TurnConfig>,
    sfu_url: String,
    sfu_token: Option<String>,
}

struct Peer {
    id: String,
    role: Role,
    ws: Arc<Mutex<Websocket>>,
}

type Rooms = Arc<Mutex<HashMap<String, Vec<Peer>>>>;

fn main() {
    init_log();
    let config = Arc::new(load_config());
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));
    let port: u16 = std::env::var("SIGNAL_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);
    let cert = include_bytes!("../../../certs/cer.pem").to_vec();
    let key = include_bytes!("../../../certs/key.pem").to_vec();

    // 明文 WS（开发用；生产只开 WSS 端口）
    let plain_port: u16 = std::env::var("SIGNAL_PLAIN_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3003);
    let plain_config = config.clone();
    let plain_rooms = rooms.clone();
    let plain = rouille::Server::new(format!("0.0.0.0:{plain_port}"), move |request| {
        handle(request, plain_config.clone(), plain_rooms.clone())
    })
    .expect("start plain signaling server");
    std::thread::spawn(move || plain.run());
    info!("Signaling (WS plain) listening on :{plain_port}");

    let server = rouille::Server::new_ssl(
        format!("0.0.0.0:{port}"),
        move |request| handle(request, config.clone(), rooms.clone()),
        cert,
        key,
    )
    .expect("start signaling server");
    info!("Signaling (WSS) listening on :{port}");
    server.run();
}

fn init_log() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("aerodesk_signal=info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
}

fn load_config() -> Config {
    let auth_tokens = std::env::var("AUTH_TOKENS")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(|t| t.to_string()).collect())
        .unwrap_or_default();

    let turn = std::env::var("TURN_SECRET").ok().map(|secret| {
        let urls = std::env::var("TURN_URLS").unwrap_or_default();
        let urls = if urls.is_empty() {
            vec![
                "turn:127.0.0.1:3478?transport=udp".into(),
                "turn:127.0.0.1:3478?transport=tcp".into(),
                "turns:127.0.0.1:5349?transport=tcp".into(),
            ]
        } else {
            urls.split(',').map(|u| u.to_string()).collect()
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_secs();
        let creds =
            aerodesk_protocol::turn::generate_turn_credentials(&secret, "aerodesk", 3600, now);
        TurnConfig {
            urls,
            username: creds.username,
            credential: creds.credential,
        }
    });

    Config {
        auth_tokens,
        turn,
        sfu_url: std::env::var("SFU_URL").unwrap_or_else(|_| "http://127.0.0.1:3002".into()),
        sfu_token: std::env::var("SFU_TOKEN").ok(),
    }
}

fn handle(request: &Request, config: Arc<Config>, rooms: Rooms) -> Response {
    if request.method() == "GET" && request.url() == "/ws" {
        return match websocket::start(request, None::<&str>) {
            Ok((response, rx)) => {
                std::thread::spawn(move || session_loop(rx, config, rooms));
                response
            }
            Err(_) => Response::text("websocket upgrade required").with_status_code(400),
        };
    }
    if request.method() == "GET" {
        return Response::text("aerodesk-signal: connect to /ws");
    }
    Response::text("method not allowed").with_status_code(405)
}

fn session_loop(rx: std::sync::mpsc::Receiver<Websocket>, config: Arc<Config>, rooms: Rooms) {
    let ws = Arc::new(Mutex::new(rx.recv().expect("websocket accepted")));
    info!("session open");

    loop {
        let msg = match ws.lock().unwrap().next() {
            Some(Message::Text(t)) => t,
            Some(_) => continue,
            None => break,
        };

        let parsed: Result<SignalMessage, _> = serde_json::from_str(&msg);
        let Ok(msg) = parsed else {
            send(
                ws.clone(),
                SignalMessage::Error {
                    message: "invalid message".into(),
                },
            );
            continue;
        };

        match msg {
            SignalMessage::Join {
                room,
                role,
                auth_token,
            } => {
                if !config.auth_tokens.is_empty()
                    && !config
                        .auth_tokens
                        .iter()
                        .any(|t| Some(t.as_str()) == auth_token.as_deref())
                {
                    send(
                        ws.clone(),
                        SignalMessage::Error {
                            message: "auth failed".into(),
                        },
                    );
                    break;
                }
                let peer_id = format!("{}-{}", room, fastrand_id());
                let peers: Vec<PeerInfo> = rooms
                    .lock()
                    .unwrap()
                    .get(&room)
                    .map(|peers| {
                        peers
                            .iter()
                            .map(|p| PeerInfo {
                                peer_id: p.id.clone(),
                                role: p.role,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                rooms
                    .lock()
                    .unwrap()
                    .entry(room.clone())
                    .or_default()
                    .push(Peer {
                        id: peer_id.clone(),
                        role,
                        ws: ws.clone(),
                    });
                info!("peer {peer_id} joined room {room} as {role:?}");
                send(
                    ws.clone(),
                    SignalMessage::Joined {
                        peer_id,
                        peers,
                        turn: config.turn.clone(),
                    },
                );
            }
            SignalMessage::Description {
                from, description, ..
            } => {
                let room = find_room(&rooms, &from);
                let Some(room) = room else {
                    send(
                        ws.clone(),
                        SignalMessage::Error {
                            message: "not in a room".into(),
                        },
                    );
                    continue;
                };
                match proxy_to_sfu(&config, &room, &description) {
                    Ok(answer) => send(
                        ws.clone(),
                        SignalMessage::Description {
                            from: "sfu".into(),
                            to: from,
                            description: answer,
                        },
                    ),
                    Err(e) => send(
                        ws.clone(),
                        SignalMessage::Error {
                            message: format!("sfu error: {e}"),
                        },
                    ),
                }
            }
            SignalMessage::IceCandidate { .. } => {
                // v1 非 trickle：candidate 内嵌在 offer/answer 中
            }
            SignalMessage::Joined { .. }
            | SignalMessage::PeerLeft { .. }
            | SignalMessage::Error { .. } => {}
        }
    }

    // 断开：移除并广播 PeerLeft
    let found = {
        let mut rooms = rooms.lock().unwrap();
        let mut found = None;
        for (room, peers) in rooms.iter_mut() {
            if let Some(idx) = peers.iter().position(|p| Arc::ptr_eq(&p.ws, &ws)) {
                found = Some((room.clone(), peers[idx].id.clone()));
                peers.remove(idx);
                break;
            }
        }
        if let Some((room, _)) = &found
            && rooms.get(room).map(|p| p.is_empty()).unwrap_or(true)
        {
            rooms.remove(room);
        }
        found
    };
    let Some((room, peer_id)) = found else {
        info!("session closed");
        return;
    };
    info!("peer {peer_id} left room {room}");
    let peers = rooms.lock().unwrap();
    if let Some(peers) = peers.get(&room) {
        for p in peers {
            send(
                p.ws.clone(),
                SignalMessage::PeerLeft {
                    peer_id: peer_id.clone(),
                },
            );
        }
    }
    info!("session closed");
}

fn send(ws: Arc<Mutex<Websocket>>, msg: SignalMessage) {
    if let Ok(text) = serde_json::to_string(&msg)
        && let Ok(mut ws) = ws.lock()
    {
        let _ = ws.send_text(&text);
    }
}

fn find_room(rooms: &Rooms, peer_id: &str) -> Option<String> {
    let rooms = rooms.lock().unwrap();
    rooms
        .iter()
        .find(|(_, peers)| peers.iter().any(|p| p.id == peer_id))
        .map(|(room, _)| room.clone())
}

/// 调用 SFU 内部接口：POST /start?room=xxx（body = SDP offer JSON）
fn proxy_to_sfu(config: &Config, room: &str, description: &str) -> Result<String, String> {
    let url = format!("{}/start?room={}", config.sfu_url, room);
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let mut req = agent.post(&url).set("Content-Type", "application/json");
    if let Some(token) = &config.sfu_token {
        req = req.set("X-Internal-Token", token);
    }
    let resp = req.send_string(description).map_err(|e| e.to_string())?;
    resp.into_string().map_err(|e| e.to_string())
}

fn fastrand_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}
