//! AeroDesk 信令服务（WSS）。
//!
//! 职责：认证、房间管理、TURN 凭证下发、WebRTC offer/answer 代理到 SFU。
//! 协议见 `aerodesk-protocol::signal`。
//!
//! 环境变量：
//!   SIGNAL_PORT   WSS 端口（默认 3001）
//!   AUTH_TOKENS   逗号分隔合法 token（JWT_SECRET 未设置时使用；空则不认证）
//!   JWT_SECRET     HS256 共享密钥；设置后 Join 必须携带合法 JWT（用户/设备/房间/角色授权）
//!   JWT_SECRET_OLD 旧密钥（轮换宽限期）：新密钥验证失败时回退旧密钥，轮换不中断
//!
//! SIGHUP：重读 TLS 证书并重建 WSS server（无需重启进程）。
//!   TURN_SECRET   coturn REST secret（空则不下发 TURN）
//!   TURN_URLS     逗号分隔 TURN URL（默认 127.0.0.1:3478）
//!   SFU_URL       SFU 内部接口（默认 http://127.0.0.1:3002）
//!   SFU_TOKEN     SFU 内部接口 token（可选）

#[macro_use]
extern crate tracing;

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aerodesk_protocol::signal::{PeerInfo, Role, SignalMessage, TurnConfig};
use rouille::websocket::{self, Message, Websocket};
use rouille::{Request, Response};

struct Config {
    auth_tokens: Vec<String>,
    jwt_secret: Option<String>,
    /// 旧密钥（轮换宽限期）：`jwt_secret` 验证失败时回退（#143）。
    jwt_secret_old: Option<String>,
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

/// SIGHUP 触发：重读 TLS 证书并重建 WSS server。
static RELOAD_TLS: AtomicBool = AtomicBool::new(false);
static CONFIG: OnceLock<Arc<Config>> = OnceLock::new();
static ROOMS: OnceLock<Rooms> = OnceLock::new();

#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    if sig == libc::SIGHUP {
        RELOAD_TLS.store(true, Ordering::Relaxed);
    }
}

#[cfg(unix)]
fn install_signal_handlers() {
    // Safety: 信号处理器只做原子写（async-signal-safe）。
    unsafe {
        libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
    }
}

/// WSS 处理器（fn 指针，便于 SIGHUP 重建 Server）。
fn wss_handler(request: &Request) -> Response {
    let config = CONFIG.get().expect("config initialized").clone();
    let rooms = ROOMS.get().expect("rooms initialized").clone();
    handle(request, config, rooms)
}

/// 带重试的 TLS server 绑定：旧 listener 释放后端口可能短暂 EADDRINUSE（macOS 实测），
/// 重试可自愈；失败返回最后一次错误。
fn bind_wss_with_retry(
    port: u16,
    cert: &[u8],
    key: &[u8],
    attempts: usize,
) -> Result<rouille::Server<fn(&Request) -> Response>, String> {
    let mut last_err = String::new();
    for i in 0..attempts {
        match rouille::Server::new_ssl(
            format!("0.0.0.0:{port}"),
            wss_handler as fn(&Request) -> Response,
            cert.to_vec(),
            key.to_vec(),
        ) {
            Ok(srv) => return Ok(srv),
            Err(e) => {
                last_err = e.to_string();
                if i + 1 < attempts {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    Err(last_err)
}

/// SIGHUP：重读 TLS 身份并重建 WSS server（旧连接不受影响；同证书 no-op）。
fn reload_tls(
    server: &mut Option<rouille::Server<fn(&Request) -> Response>>,
    tls: &mut aerodesk_protocol::tls::TlsIdentity,
) {
    match aerodesk_protocol::tls::TlsIdentity::load() {
        Ok(new_tls) => {
            if new_tls.cert == tls.cert && new_tls.key == tls.key {
                info!(
                    "SIGHUP: TLS identity unchanged ({}), keep serving",
                    new_tls.source
                );
                return;
            }
            let port: u16 = std::env::var("SIGNAL_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(3001);
            info!("SIGHUP: reloading TLS identity from {}", new_tls.source);
            server.take();
            match bind_wss_with_retry(port, &new_tls.cert, &new_tls.key, 20) {
                Ok(srv) => {
                    *server = Some(srv);
                    *tls = new_tls;
                    info!("TLS reloaded (new connections use updated certificate)");
                }
                Err(e) => {
                    error!("SIGHUP: TLS reload bind failed: {e}; restoring previous identity");
                    match bind_wss_with_retry(port, &tls.cert, &tls.key, 20) {
                        Ok(srv) => *server = Some(srv),
                        Err(e2) => error!("restore failed: {e2}; WSS down until restart"),
                    }
                }
            }
        }
        Err(e) => error!("SIGHUP: TLS identity reload failed: {e}"),
    }
}

fn main() {
    init_log();
    let config = Arc::new(load_config());
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));
    let port: u16 = std::env::var("SIGNAL_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);
    let tls = aerodesk_protocol::tls::TlsIdentity::load().unwrap_or_else(|e| {
        eprintln!("fatal: TLS identity load failed: {e}");
        std::process::exit(1);
    });
    info!("TLS identity source: {}", tls.source);
    let _ = CONFIG.set(config.clone());
    let _ = ROOMS.set(rooms.clone());

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

    #[cfg(unix)]
    install_signal_handlers();

    let mut tls = tls;
    let mut server = Some(
        rouille::Server::new_ssl(
            format!("0.0.0.0:{port}"),
            wss_handler as fn(&Request) -> Response,
            tls.cert.clone(),
            tls.key.clone(),
        )
        .expect("start signaling server"),
    );
    info!("Signaling (WSS) listening on :{port}");
    // 轮询 + SIGHUP 证书热重载（#143，复用 #128 模式）。
    loop {
        if let Some(srv) = &server {
            srv.poll_timeout(Duration::from_millis(10));
        }
        if RELOAD_TLS.swap(false, Ordering::Relaxed) {
            reload_tls(&mut server, &mut tls);
        }
    }
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
        jwt_secret: std::env::var("JWT_SECRET").ok().filter(|s| !s.is_empty()),
        jwt_secret_old: std::env::var("JWT_SECRET_OLD")
            .ok()
            .filter(|s| !s.is_empty()),
        turn,
        sfu_url: std::env::var("SFU_URL").unwrap_or_else(|_| "http://127.0.0.1:3002".into()),
        sfu_token: std::env::var("SFU_TOKEN").ok(),
    }
}

/// 认证：JWT（新密钥优先，失败回退 JWT_SECRET_OLD 宽限期）→ 静态 token → 开发模式放行。
fn auth_ok(config: &Config, token: Option<&str>, room: &str, role: Role) -> bool {
    let token = token.unwrap_or_default();
    if let Some(secret) = &config.jwt_secret {
        // JWT 认证：校验签名/过期/房间/角色。
        match aerodesk_protocol::jwt::validate_token(secret, token, room, role) {
            Ok(claims) => {
                info!(
                    "jwt auth ok: user={} dev={:?} room={} role={:?}",
                    claims.sub, claims.dev, room, role
                );
                true
            }
            Err(new_err) => {
                if let Some(old) = &config.jwt_secret_old {
                    match aerodesk_protocol::jwt::validate_token(old, token, room, role) {
                        Ok(claims) => {
                            info!(
                                "jwt auth ok (legacy secret): user={} dev={:?} room={} role={:?}",
                                claims.sub, claims.dev, room, role
                            );
                            return true;
                        }
                        Err(e) => {
                            warn!("jwt auth failed (new: {new_err}; legacy: {e})");
                        }
                    }
                } else {
                    warn!("jwt auth failed: {new_err}");
                }
                false
            }
        }
    } else if !config.auth_tokens.is_empty() {
        // 静态 token 认证（兼容模式）。
        config
            .auth_tokens
            .iter()
            .any(|t| Some(t.as_str()) == token.into())
    } else {
        // 开发模式：不认证。
        true
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
                let auth_ok = auth_ok(&config, auth_token.as_deref(), &room, role);
                if !auth_ok {
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
                // #12：把 peer 角色传给 SFU，SFU 据此拒绝 viewer 发布媒体。
                let role = rooms
                    .lock()
                    .unwrap()
                    .get(&room)
                    .and_then(|peers| peers.iter().find(|p| p.id == from))
                    .map(|p| p.role)
                    .unwrap_or(Role::Viewer);
                match proxy_to_sfu(&config, &room, role, &description) {
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
    // #66：先快照同房间其它连接、释放 rooms 锁，再尽力广播 PeerLeft。
    // 不能在持有 rooms 锁时去锁其它连接的 ws（锁序反转 → 死锁）。
    let peers: Vec<Arc<Mutex<Websocket>>> = {
        let rooms = rooms.lock().unwrap();
        rooms
            .get(&room)
            .map(|peers| peers.iter().map(|p| p.ws.clone()).collect())
            .unwrap_or_default()
    };
    for p in peers {
        send(
            p,
            SignalMessage::PeerLeft {
                peer_id: peer_id.clone(),
            },
        );
    }
    info!("session closed");
}

fn send(ws: Arc<Mutex<Websocket>>, msg: SignalMessage) {
    if let Ok(text) = serde_json::to_string(&msg) {
        // #66：非阻塞发送。每个连接自己的 session_loop 在阻塞读 `next()` 时
        // 持有本连接的 ws Mutex；若这里用 `lock()` 等待，会与「清理广播持有
        // rooms 锁再去锁其它连接 ws」形成锁序反转死锁——kill 一个连接后，
        // 新连接的 Join 就永远拿不到 rooms 锁（Join 卡死）。
        // 对本连接（Join/Description 应答）try_lock 必然成功；对其它连接的
        // PeerLeft 广播为尽力而为（协议保留该消息，当前无消费方，见 #66）。
        let Ok(mut ws) = ws.try_lock() else {
            tracing::debug!("send skipped: peer websocket busy (non-blocking, #66)");
            return;
        };
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

/// 调用 SFU 内部接口：POST /start?room=xxx&role=xxx（body = SDP offer JSON）
fn proxy_to_sfu(
    config: &Config,
    room: &str,
    role: Role,
    description: &str,
) -> Result<String, String> {
    let role_name = match role {
        Role::Publisher => "publisher",
        Role::Viewer => "viewer",
    };
    let url = format!("{}/start?room={}&role={}", config.sfu_url, room, role_name);
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

/// 进程内单调计数器：与纳秒时间戳组合，保证 peer_id 唯一。
///
/// 仅用纳秒时间戳在粗时钟（如 macOS ~1ms 分辨率）下可能同 tick 碰撞：
/// publisher 与 viewer 几乎同时 Join 得到相同 peer_id，Description 按 id
/// 查角色时会命中错误条目（如 publisher 被当成 viewer）→ SFU #12 误拒。
static PEER_COUNTER: AtomicU64 = AtomicU64::new(0);

fn fastrand_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = PEER_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{n}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerodesk_protocol::jwt::mint_token;

    fn cfg(new: &str, old: Option<&str>) -> Config {
        Config {
            auth_tokens: vec![],
            jwt_secret: Some(new.into()),
            jwt_secret_old: old.map(|s| s.to_string()),
            turn: None,
            sfu_url: "http://127.0.0.1:3002".into(),
            sfu_token: None,
        }
    }

    #[test]
    fn dual_secret_accepts_new_and_legacy() {
        let token_new =
            mint_token("new-secret", "u1", None, Some("r1"), Some(Role::Viewer), 60).unwrap();
        let token_old =
            mint_token("old-secret", "u2", None, Some("r1"), Some(Role::Viewer), 60).unwrap();
        let config = cfg("new-secret", Some("old-secret"));
        assert!(
            auth_ok(&config, Some(&token_new), "r1", Role::Viewer),
            "new secret token must pass"
        );
        assert!(
            auth_ok(&config, Some(&token_old), "r1", Role::Viewer),
            "legacy secret token must pass during grace"
        );
    }

    #[test]
    fn dual_secret_rejects_wrong_secret_and_wrong_room() {
        let token_wrong =
            mint_token("attacker", "u3", None, Some("r1"), Some(Role::Viewer), 60).unwrap();
        let config = cfg("new-secret", Some("old-secret"));
        assert!(
            !auth_ok(&config, Some(&token_wrong), "r1", Role::Viewer),
            "wrong secret must fail"
        );

        let token_room =
            mint_token("new-secret", "u4", None, Some("r2"), Some(Role::Viewer), 60).unwrap();
        assert!(
            !auth_ok(&config, Some(&token_room), "r1", Role::Viewer),
            "room mismatch must fail"
        );
    }

    #[test]
    fn old_secret_only_valid_before_rotation_completes() {
        // 轮换完成（JWT_SECRET_OLD 移除）后，旧密钥 token 必须拒绝
        let token_old =
            mint_token("old-secret", "u5", None, Some("r1"), Some(Role::Viewer), 60).unwrap();
        let config = cfg("new-secret", None);
        assert!(
            !auth_ok(&config, Some(&token_old), "r1", Role::Viewer),
            "old token must fail after grace ends"
        );
    }

    use super::*;

    /// 回归：peer_id 必须在快速连续 Join 下保持唯一（粗时钟下纳秒时间戳会碰撞，
    /// 碰撞会让 Description 按 id 查角色时命中错误条目，导致 SFU #12 误拒）。
    #[test]
    fn peer_ids_unique_across_rapid_calls() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            let id = fastrand_id();
            let dup = !seen.insert(id.clone());
            assert!(!dup, "duplicate peer_id: {id}");
        }
    }
}
