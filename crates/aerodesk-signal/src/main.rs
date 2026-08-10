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
//!
//! 多 PoP（#146）：
//! 连接配额（#163）：
//!   MAX_ROOM_CLIENTS  每房间人数上限（0=不限）；超限 Join 返回 Error("room full")
//!   MAX_TOTAL_CLIENTS 单实例全局连接上限（0=不限）；超限返回 Error("server full")
//!
//!   POP_ID        本 PoP 标识（默认 local）
//!   ROOM_POP_MAP  房间前缀=PoP，逗号分隔（如 eu-=pop-eu,us-=pop-us）；最长前缀优先
//!   POP_URLS      PoP=客户端信令 URL，逗号分隔（如 pop-eu=wss://eu.example.com:443/ws）
//!
//! 跨 PoP 桥接（#216 M3，可选）：
//!   BRIDGE_CMD                 房间桥命令模板（含 {room} 占位符）。设置后，加入
//!                              「钉在其它 PoP」房间的 viewer 先经桥在本 PoP 接入，
//!                              桥失败/超时回退 v1 Redirect
//!   BRIDGE_READY_TIMEOUT_SECS  桥就绪等待上限（默认 15）
//!   BRIDGE_FAIL_COOLDOWN_SECS  桥失败冷却（默认 30，期间直接 Redirect 不重试）
//!   BRIDGE_MAX_RUNNING         并发桥上限（默认 8；防房间名轮换绕过冷却的进程滥用）
//!   BRIDGE_IDLE_SECS           桥空闲回收阈值（默认 300；房间无真实客户端超时停桥）

#[macro_use]
extern crate tracing;

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aerodesk_protocol::jwt::Claims;
use aerodesk_protocol::signal::{PeerInfo, Role, SignalMessage, TurnConfig};
use bridge::{BridgeManager, BridgeOutcome};
use pop_registry::PopRegistry;
use rouille::websocket::{self, Message, Websocket};
use rouille::{Request, Response};

mod bridge;
mod pop_registry;

struct Config {
    auth_tokens: Vec<String>,
    jwt_secret: Option<String>,
    /// 旧密钥（轮换宽限期）：`jwt_secret` 验证失败时回退（#143）。
    jwt_secret_old: Option<String>,
    /// 本 PoP 标识（默认 local）。
    pop_id: String,
    /// 房间前缀 → PoP（最长前缀优先，静态钉住）。
    room_pop_map: Vec<(String, String)>,
    /// 动态注册表（POP_REGISTRY_FILE 开启时存在）：首个加入者登记房间归属（#154）。
    pop_registry: Option<Arc<PopRegistry>>,
    /// 每房间人数上限（0=不限，#163）。
    max_room_clients: usize,
    /// 全局连接上限（0=不限，#163）。
    max_total_clients: usize,
    /// PoP → 客户端信令 URL（重定向目标）。
    pop_urls: HashMap<String, String>,
    turn: Option<TurnConfig>,
    sfu_url: String,
    sfu_token: Option<String>,
    /// 跨 PoP 桥接编排（#216 M3）：BRIDGE_CMD 设置时启用；桥失败回退 Redirect。
    bridge: Option<Arc<BridgeManager>>,
    /// 桥空闲回收阈值（#246）：房间内无真实客户端超过该时长 → 停止桥。
    bridge_idle_secs: Duration,
}

struct Peer {
    id: String,
    role: Role,
    ws: Arc<Mutex<Websocket>>,
    /// JWT sub（#171 per-user 配额计数用；静态/开发模式为 None）。
    user: Option<String>,
    /// 桥自身 publisher 腿（#246）：空闲回收/配额豁免按此区分，不计真实客户端。
    bridge_leg: bool,
}

type Rooms = Arc<Mutex<HashMap<String, Vec<Peer>>>>;

/// SIGHUP 触发：重读 TLS 证书并重建 WSS server。
static RELOAD_TLS: AtomicBool = AtomicBool::new(false);
static CONFIG: OnceLock<Arc<Config>> = OnceLock::new();
static ROOMS: OnceLock<Rooms> = OnceLock::new();
/// 全局在线连接数（#163）。
static TOTAL_CLIENTS: OnceLock<Arc<AtomicUsize>> = OnceLock::new();
/// 按用户（JWT sub）在线连接数（#171）。
static USER_CONNS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();

/// 用户配额检查（纯函数）：max=0 不限；已达上限拒绝；通过则计数 +1。
fn user_quota_take(
    conns: &mut HashMap<String, usize>,
    user: &str,
    max_conns: u32,
) -> Result<(), &'static str> {
    if max_conns == 0 {
        return Ok(());
    }
    let n = conns.get(user).copied().unwrap_or(0);
    if n as u32 >= max_conns {
        return Err("user quota exceeded");
    }
    conns.insert(user.to_string(), n + 1);
    Ok(())
}

/// 用户连接释放（断开时 -1）。
fn user_quota_release(conns: &mut HashMap<String, usize>, user: &str) {
    if let Some(n) = conns.get_mut(user) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            conns.remove(user);
        }
    }
}

/// 配额检查（纯函数）：房间/全局任一超限返回原因。
fn quota_ok(
    room_len: usize,
    total: usize,
    room_cap: usize,
    total_cap: usize,
) -> Result<(), &'static str> {
    if room_cap > 0 && room_len >= room_cap {
        return Err("room full");
    }
    if total_cap > 0 && total >= total_cap {
        return Err("server full");
    }
    Ok(())
}

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
    let _ = TOTAL_CLIENTS.set(Arc::new(AtomicUsize::new(0)));
    let _ = USER_CONNS.set(Mutex::new(HashMap::new()));

    // #246 桥生命周期 monitor：房间无真实客户端超过 BRIDGE_IDLE_SECS → 停桥。
    // 空闲判定用纯函数 idle_rooms_to_stop（按 spawn 代数防旧时间戳误杀新桥）；
    // 停桥前持 rooms 锁二次确认并执行 stop，避免与并发 Join 的 TOCTOU。
    if let Some(bridge) = &config.bridge {
        let bridge = bridge.clone();
        let rooms = rooms.clone();
        let idle = config.bridge_idle_secs;
        std::thread::Builder::new()
            .name("bridge-monitor".into())
            .spawn(move || {
                let mut idle_since: HashMap<String, Instant> = HashMap::new();
                let mut last_epoch: HashMap<String, u64> = HashMap::new();
                loop {
                    std::thread::sleep(Duration::from_secs(15));
                    let now = Instant::now();
                    let running = bridge.running_rooms();
                    let real_peers: HashMap<String, usize> = {
                        let rooms = rooms.lock().unwrap();
                        running
                            .iter()
                            .map(|(room, _)| {
                                let n = rooms
                                    .get(room)
                                    .map(|peers| peers.iter().filter(|p| !p.bridge_leg).count())
                                    .unwrap_or(0);
                                (room.clone(), n)
                            })
                            .collect()
                    };
                    for room in bridge::idle_rooms_to_stop(
                        &running,
                        &real_peers,
                        &mut idle_since,
                        &mut last_epoch,
                        idle,
                        now,
                    ) {
                        // 二次确认 + 停桥：持 rooms 锁（无锁序环：running 锁不反向取 rooms）。
                        let stop = {
                            let rooms = rooms.lock().unwrap();
                            let real = rooms
                                .get(&room)
                                .map(|peers| peers.iter().filter(|p| !p.bridge_leg).count())
                                .unwrap_or(0);
                            if real == 0 && bridge.is_running(&room) {
                                info!(
                                    "bridge monitor: room {room} idle ({idle:?} no real peers), stopping bridge"
                                );
                                bridge.stop(&room);
                                true
                            } else {
                                false
                            }
                        };
                        if stop {
                            idle_since.remove(&room);
                        }
                    }
                }
            })
            .expect("spawn bridge monitor");
    }

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

    let room_pop_map = std::env::var("ROOM_POP_MAP")
        .unwrap_or_default()
        .split(',')
        .filter(|kv| kv.contains('='))
        .map(|kv| {
            let (prefix, pop) = kv.split_once('=').expect("checked contains '='");
            (prefix.trim().to_string(), pop.trim().to_string())
        })
        .collect();
    let pop_urls = std::env::var("POP_URLS")
        .unwrap_or_default()
        .split(',')
        .filter(|kv| kv.contains('='))
        .map(|kv| {
            let (pop, url) = kv.split_once('=').expect("checked contains '='");
            (pop.trim().to_string(), url.trim().to_string())
        })
        .collect();

    let max_room_clients = std::env::var("MAX_ROOM_CLIENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let max_total_clients = std::env::var("MAX_TOTAL_CLIENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let pop_registry = std::env::var("POP_REGISTRY_FILE")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|path| {
            let ttl = std::env::var("POP_REGISTRY_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3600);
            Arc::new(PopRegistry::new(Some(path.into()), ttl))
        });

    let bridge = std::env::var("BRIDGE_CMD")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|cmd| {
            let ready_timeout = std::env::var("BRIDGE_READY_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(15));
            let fail_cooldown = std::env::var("BRIDGE_FAIL_COOLDOWN_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(30));
            let max_running = std::env::var("BRIDGE_MAX_RUNNING")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8);
            if !cmd.contains("{room}") {
                warn!("BRIDGE_CMD 不含 {{room}} 占位符（自动追加 --room）；建议显式写明");
            }
            Arc::new(BridgeManager::new(
                cmd,
                ready_timeout,
                fail_cooldown,
                max_running,
            ))
        });
    if bridge.is_some() {
        info!(
            "bridge orchestration enabled (BRIDGE_CMD); cross-PoP rooms bridge-first, fallback Redirect"
        );
    }

    Config {
        auth_tokens,
        jwt_secret: std::env::var("JWT_SECRET").ok().filter(|s| !s.is_empty()),
        jwt_secret_old: std::env::var("JWT_SECRET_OLD")
            .ok()
            .filter(|s| !s.is_empty()),
        pop_id: std::env::var("POP_ID").unwrap_or_else(|_| "local".into()),
        room_pop_map,
        pop_registry,
        max_room_clients,
        max_total_clients,
        pop_urls,
        turn,
        sfu_url: std::env::var("SFU_URL").unwrap_or_else(|_| "http://127.0.0.1:3002".into()),
        sfu_token: std::env::var("SFU_TOKEN").ok(),
        bridge,
        bridge_idle_secs: std::env::var("BRIDGE_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(|v| v.max(15)) // 不低于 monitor 轮询间隔（15s），防误杀
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(300)),
    }
}

/// 房间命中映射的 PoP（最长前缀优先）；无映射返回 None。
fn pinned_pop<'a>(config: &'a Config, room: &str) -> Option<&'a str> {
    config
        .room_pop_map
        .iter()
        .filter(|(prefix, _)| room.starts_with(prefix.as_str()))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, pop)| pop.as_str())
}

/// 认证：JWT（新密钥优先，失败回退 JWT_SECRET_OLD 宽限期）→ 静态 token → 开发模式放行。
/// 返回认证通过的 Claims（#171 用户配额用）；开发模式返回 None（视为通过但无用户维度）。
fn auth_result(config: &Config, token: Option<&str>, room: &str, role: Role) -> Option<Claims> {
    let token = token.unwrap_or_default();
    if let Some(secret) = &config.jwt_secret {
        // JWT 认证：校验签名/过期/房间/角色。
        match aerodesk_protocol::jwt::validate_token(secret, token, room, role) {
            Ok(claims) => {
                info!(
                    "jwt auth ok: user={} dev={:?} room={} role={:?}",
                    claims.sub, claims.dev, room, role
                );
                Some(claims)
            }
            Err(new_err) => {
                if let Some(old) = &config.jwt_secret_old {
                    match aerodesk_protocol::jwt::validate_token(old, token, room, role) {
                        Ok(claims) => {
                            info!(
                                "jwt auth ok (legacy secret): user={} dev={:?} room={} role={:?}",
                                claims.sub, claims.dev, room, role
                            );
                            return Some(claims);
                        }
                        Err(e) => {
                            warn!("jwt auth failed (new: {new_err}; legacy: {e})");
                        }
                    }
                } else {
                    warn!("jwt auth failed: {new_err}");
                }
                None
            }
        }
    } else if !config.auth_tokens.is_empty() {
        // 静态 token 认证（兼容模式）：以 token 本身作为用户键。
        if config
            .auth_tokens
            .iter()
            .any(|t| Some(t.as_str()) == token.into())
        {
            Some(Claims {
                sub: token.to_string(),
                dev: None,
                room: None,
                role: None,
                max_conns: None,
                iat: 0,
                exp: 0,
            })
        } else {
            None
        }
    } else {
        // 开发模式：不认证（无用户维度）。
        None
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
                // 多 PoP（#146/#154）：先静态钉住，再查动态注册表；其它 PoP → 重定向。
                let mut target_pop = pinned_pop(&config, &room).map(|s| s.to_string());
                if target_pop.is_none()
                    && let Some(reg) = &config.pop_registry
                {
                    target_pop = reg.lookup(&room);
                }
                match &target_pop {
                    Some(pop) if pop != &config.pop_id => {
                        if let Some(url) = config.pop_urls.get(pop) {
                            // #216 M3：配置 BRIDGE_CMD 时，先认证+配额（防止未授权
                            // 客户端触发进程 spawn），再尝试桥接——viewer 在本 PoP
                            // 经桥收流（不 Redirect）；桥失败/超时/冷却回退 v1 Redirect。
                            if config.bridge.is_some() {
                                // 认证先行（纯校验，不占配额；下方正式流程会再验一次）。
                                let claims_pre =
                                    auth_result(&config, auth_token.as_deref(), &room, role);
                                let auth_ok_pre = if config.jwt_secret.is_some()
                                    || !config.auth_tokens.is_empty()
                                {
                                    claims_pre.is_some()
                                } else {
                                    true
                                };
                                if !auth_ok_pre {
                                    send(
                                        ws.clone(),
                                        SignalMessage::Error {
                                            message: "auth failed".into(),
                                        },
                                    );
                                    break;
                                }
                                // 配额先行（纯检查）：桥自身 publisher 腿豁免。
                                let bridge_leg_pre = config.bridge.as_ref().is_some_and(|b| {
                                    role == Role::Publisher && b.is_running(&room)
                                });
                                if !bridge_leg_pre {
                                    let room_len = rooms
                                        .lock()
                                        .unwrap()
                                        .get(&room)
                                        .map(|peers| peers.len())
                                        .unwrap_or(0);
                                    let total = TOTAL_CLIENTS
                                        .get()
                                        .expect("total initialized")
                                        .load(Ordering::Relaxed);
                                    if let Err(reason) = quota_ok(
                                        room_len,
                                        total,
                                        config.max_room_clients,
                                        config.max_total_clients,
                                    ) {
                                        info!("reject join room={room} role={role:?}: {reason}");
                                        send(
                                            ws.clone(),
                                            SignalMessage::Error {
                                                message: reason.to_string(),
                                            },
                                        );
                                        continue;
                                    }
                                }
                                // 桥决策：桥自身 publisher 腿走 is_running 快路径
                                // （等自身就绪会死锁）；真实 publisher 回退 Redirect
                                // （桥只支持主 PoP→本 PoP 媒体方向）；viewer 统一
                                // ensure_ready（并发 viewer 共享单飞结果，失败一致回退）。
                                let bridge_ok = match config.bridge.as_ref() {
                                    Some(b) if role == Role::Publisher && b.is_running(&room) => {
                                        true
                                    }
                                    Some(_) if role == Role::Publisher => false,
                                    Some(b) => b.ensure_ready(&room) == BridgeOutcome::Ready,
                                    None => false,
                                };
                                if bridge_ok {
                                    info!(
                                        "room {room} -> pop {pop} (self={self}): bridge ready, join locally",
                                        self = config.pop_id
                                    );
                                } else {
                                    info!(
                                        "room {room} -> pop {pop} (self={self}): bridge unavailable, fallback redirect to {url}",
                                        self = config.pop_id
                                    );
                                    send(
                                        ws.clone(),
                                        SignalMessage::Redirect {
                                            pop: pop.clone(),
                                            url: url.clone(),
                                            reason: Some("room pinned to pop".into()),
                                        },
                                    );
                                    continue;
                                }
                            } else {
                                // v1：无桥编排 → 直接 Redirect。
                                info!(
                                    "room {room} -> pop {pop} (self={self}); redirect to {url}",
                                    self = config.pop_id
                                );
                                send(
                                    ws.clone(),
                                    SignalMessage::Redirect {
                                        pop: pop.clone(),
                                        url: url.clone(),
                                        reason: Some("room pinned to pop".into()),
                                    },
                                );
                                continue;
                            }
                        } else {
                            warn!("room {room} -> pop {pop} but no POP_URLS entry; ignoring pin");
                        }
                    }
                    Some(_) => {
                        // 本 PoP：静态钉住或动态命中本 PoP——刷新动态条目 TTL（若开启）。
                        if let Some(reg) = &config.pop_registry {
                            reg.register(&room, &config.pop_id);
                        }
                    }
                    None => {
                        // 未登记：本 PoP 成为房间归属（首个加入者），并持久化。
                        if let Some(reg) = &config.pop_registry {
                            info!(
                                "room {room} registered to pop {} (first joiner)",
                                config.pop_id
                            );
                            reg.register(&room, &config.pop_id);
                        }
                    }
                }
                let claims = auth_result(&config, auth_token.as_deref(), &room, role);
                let auth_ok = if config.jwt_secret.is_some() || !config.auth_tokens.is_empty() {
                    claims.is_some()
                } else {
                    true
                };
                if !auth_ok {
                    send(
                        ws.clone(),
                        SignalMessage::Error {
                            message: "auth failed".into(),
                        },
                    );
                    break;
                }
                // 桥自身 publisher 腿判定（#246）：配额豁免 + Peer 标记 + 空闲回收共用。
                let bridge_leg = config
                    .bridge
                    .as_ref()
                    .is_some_and(|b| role == Role::Publisher && b.is_running(&room));
                // #163 配额：房间/全局上限检查（0=不限）。桥自身 publisher 腿是
                // 内部基础设施，豁免（否则小配额下会把真实 viewer 挤掉）。
                if !bridge_leg {
                    let room_len = rooms
                        .lock()
                        .unwrap()
                        .get(&room)
                        .map(|peers| peers.len())
                        .unwrap_or(0);
                    let total = TOTAL_CLIENTS
                        .get()
                        .expect("total initialized")
                        .load(Ordering::Relaxed);
                    if let Err(reason) = quota_ok(
                        room_len,
                        total,
                        config.max_room_clients,
                        config.max_total_clients,
                    ) {
                        info!("reject join room={room} role={role:?}: {reason}");
                        send(
                            ws.clone(),
                            SignalMessage::Error {
                                message: reason.to_string(),
                            },
                        );
                        continue;
                    }
                }
                // #171 用户配额：JWT max_conns（0=不限）。
                let user = claims.as_ref().map(|c| c.sub.clone());
                if let Some(sub) = &user {
                    let max_conns = claims
                        .as_ref()
                        .map(|c| c.max_conns.unwrap_or(0))
                        .unwrap_or(0);
                    let mut uc = USER_CONNS
                        .get()
                        .expect("user conns initialized")
                        .lock()
                        .unwrap();
                    if let Err(reason) = user_quota_take(&mut uc, sub, max_conns) {
                        info!("reject join user={sub}: {reason}");
                        send(
                            ws.clone(),
                            SignalMessage::Error {
                                message: reason.to_string(),
                            },
                        );
                        continue;
                    }
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
                        user,
                        bridge_leg,
                    });
                TOTAL_CLIENTS
                    .get()
                    .expect("total initialized")
                    .fetch_add(1, Ordering::Relaxed);
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
            | SignalMessage::Error { .. }
            | SignalMessage::Redirect { .. } => {}
        }
    }

    // 断开：移除并广播 PeerLeft
    let found = {
        let mut rooms = rooms.lock().unwrap();
        let mut found = None;
        for (room, peers) in rooms.iter_mut() {
            if let Some(idx) = peers.iter().position(|p| Arc::ptr_eq(&p.ws, &ws)) {
                found = Some((room.clone(), peers[idx].id.clone(), peers[idx].user.clone()));
                peers.remove(idx);
                break;
            }
        }
        if let Some((room, _, _)) = &found
            && rooms.get(room).map(|p| p.is_empty()).unwrap_or(true)
        {
            rooms.remove(room);
        }
        found
    };
    let Some((room, peer_id, user)) = found else {
        info!("session closed");
        return;
    };
    TOTAL_CLIENTS
        .get()
        .expect("total initialized")
        .fetch_sub(1, Ordering::Relaxed);
    if let Some(sub) = &user {
        let mut uc = USER_CONNS
            .get()
            .expect("user conns initialized")
            .lock()
            .unwrap();
        user_quota_release(&mut uc, sub);
    }
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
            pop_id: "pop-a".into(),
            room_pop_map: vec![],
            pop_registry: None,
            max_room_clients: 0,
            max_total_clients: 0,
            pop_urls: HashMap::new(),
            turn: None,
            sfu_url: "http://127.0.0.1:3002".into(),
            sfu_token: None,
            bridge: None,
            bridge_idle_secs: Duration::from_secs(300),
        }
    }

    #[test]
    fn dual_secret_accepts_new_and_legacy() {
        let token_new = mint_token(
            "new-secret",
            "u1",
            None,
            Some("r1"),
            Some(Role::Viewer),
            60,
            None,
        )
        .unwrap();
        let token_old = mint_token(
            "old-secret",
            "u2",
            None,
            Some("r1"),
            Some(Role::Viewer),
            60,
            None,
        )
        .unwrap();
        let config = cfg("new-secret", Some("old-secret"));
        assert!(
            auth_result(&config, Some(&token_new), "r1", Role::Viewer).is_some(),
            "new secret token must pass"
        );
        assert!(
            auth_result(&config, Some(&token_old), "r1", Role::Viewer).is_some(),
            "legacy secret token must pass during grace"
        );
    }

    #[test]
    fn dual_secret_rejects_wrong_secret_and_wrong_room() {
        let token_wrong = mint_token(
            "attacker",
            "u3",
            None,
            Some("r1"),
            Some(Role::Viewer),
            60,
            None,
        )
        .unwrap();
        let config = cfg("new-secret", Some("old-secret"));
        assert!(
            !auth_result(&config, Some(&token_wrong), "r1", Role::Viewer).is_some(),
            "wrong secret must fail"
        );

        let token_room = mint_token(
            "new-secret",
            "u4",
            None,
            Some("r2"),
            Some(Role::Viewer),
            60,
            None,
        )
        .unwrap();
        assert!(
            !auth_result(&config, Some(&token_room), "r1", Role::Viewer).is_some(),
            "room mismatch must fail"
        );
    }

    #[test]
    fn old_secret_only_valid_before_rotation_completes() {
        // 轮换完成（JWT_SECRET_OLD 移除）后，旧密钥 token 必须拒绝
        let token_old = mint_token(
            "old-secret",
            "u5",
            None,
            Some("r1"),
            Some(Role::Viewer),
            60,
            None,
        )
        .unwrap();
        let config = cfg("new-secret", None);
        assert!(
            !auth_result(&config, Some(&token_old), "r1", Role::Viewer).is_some(),
            "old token must fail after grace ends"
        );
    }

    use super::*;

    /// 回归：peer_id 必须在快速连续 Join 下保持唯一（粗时钟下纳秒时间戳会碰撞，
    /// 碰撞会让 Description 按 id 查角色时命中错误条目，导致 SFU #12 误拒）。
    #[test]
    fn user_quota_take_and_release() {
        let mut conns = HashMap::new();
        // max=0 不限
        assert!(user_quota_take(&mut conns, "u1", 0).is_ok());
        assert!(user_quota_take(&mut conns, "u1", 0).is_ok());
        // 上限 1：第一次 ok，第二次拒绝
        let mut conns2 = HashMap::new();
        assert!(user_quota_take(&mut conns2, "u2", 1).is_ok());
        assert_eq!(
            user_quota_take(&mut conns2, "u2", 1),
            Err("user quota exceeded")
        );
        // 不同用户互不影响
        assert!(user_quota_take(&mut conns2, "u3", 1).is_ok());
        // 释放后可再进
        user_quota_release(&mut conns2, "u2");
        assert!(user_quota_take(&mut conns2, "u2", 1).is_ok());
    }

    #[test]
    fn quota_ok_boundaries() {
        // 0 = 不限
        assert!(quota_ok(5, 100, 0, 0).is_ok());
        assert!(quota_ok(5, 40, 0, 50).is_ok());
        assert!(quota_ok(5, 100, 10, 0).is_ok());
        // 房间满
        assert_eq!(quota_ok(2, 0, 2, 0), Err("room full"));
        assert_eq!(quota_ok(2, 0, 3, 0), Ok(()));
        // 全局满
        assert_eq!(quota_ok(0, 2, 0, 2), Err("server full"));
        assert_eq!(quota_ok(0, 2, 0, 3), Ok(()));
        // 两者取交集
        assert_eq!(quota_ok(2, 2, 2, 2), Err("room full"));
        assert_eq!(quota_ok(1, 2, 2, 2), Err("server full"));
    }

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
