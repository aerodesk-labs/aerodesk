//! AeroDesk SFU 服务端。
//!
//! 多核分片架构（P1）：
//! - 每分片一个线程 + 一个 SO_REUSEPORT UDP socket（同一端口 3478）
//! - 房间 → 分片哈希路由（同房间同分片优先）
//! - 跨分片：媒体/关键帧/输入通道事件 + UDP 包转投
//! - TCP/SSL-TCP：全局 accept/读线程，按路由表分发到分片
//!
//! 可选录制/审计：设置 `RECORD_DIR` 后将每个房间媒体载荷落盘（ADREC1 格式）
//! 并输出 `audit.log` JSON 审计日志（房间起止/包数/字节数）。

#[macro_use]
extern crate tracing;

use std::net::SocketAddr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rouille::Server;
use rouille::{Request, Response};
use str0m::crypto::from_feature_flags;
use str0m::net::TcpType;
use str0m::{Candidate, Rtc, net::Protocol};

mod bitrate;
mod recorder;
mod router;
mod shard;
mod tcp;
mod util;

use aerodesk_protocol::signal::{Role, TurnConfig};
use shard::{Shard, ShardCommand, Shared};

/// 统一媒体端口（UDP + TCP + SSL-TCP 复用）。生产用 443。
const MEDIA_PORT: u16 = 3478;
const SIGNAL_PORT: u16 = 3000;
/// SFU 内部接口（信令服务代理用，仅本机回环）。
const INTERNAL_PORT: u16 = 3002;

fn init_log() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("aerodesk_sfu=info,str0m=info,dimpl=info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter)
        .init();
}

/// 创建 SO_REUSEPORT UDP socket（同端口多 socket，内核按流哈希分发）。
fn bind_udp_reuseport(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let domain = if addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let sock = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    // SO_REUSEPORT 仅 Unix；Windows 用 SO_REUSEADDR（set_reuse_address）近似。
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.bind(&addr.into())?;
    Ok(std::net::UdpSocket::from(sock))
}

/// 优雅关闭中（SIGTERM/SIGINT 触发）：拒绝新房间、`/healthz` 返回 503。
static DRAINING: AtomicBool = AtomicBool::new(false);
/// SIGHUP 触发：重读 TLS 证书并重建公共 HTTPS server。
static RELOAD_TLS: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    match sig {
        libc::SIGTERM | libc::SIGINT => DRAINING.store(true, Ordering::Relaxed),
        libc::SIGHUP => RELOAD_TLS.store(true, Ordering::Relaxed),
        _ => {}
    }
}

#[cfg(unix)]
fn install_unix_signal_handlers() {
    // Safety: 信号处理器只做原子写（async-signal-safe），不调用分配/锁/日志。
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
    }
}

fn install_signal_handlers() {
    #[cfg(unix)]
    install_unix_signal_handlers();
    // Windows：Ctrl+C 走 ctrlc crate（SIGTERM/SIGHUP 语义不适用）。
    #[cfg(not(unix))]
    {
        let _ = ctrlc::set_handler(|| DRAINING.store(true, Ordering::Relaxed));
    }
}

/// HTTP server 共享的应用状态（public/internal 各持一份）。
struct AppState {
    media_addr: SocketAddr,
    tcp_addr: SocketAddr,
    shard_txs: Vec<mpsc::Sender<ShardCommand>>,
    router: Arc<Mutex<router::ShardRouter>>,
    turn: Option<TurnConfig>,
    shared: Shared,
    internal_token: Option<String>,
}

static PUBLIC_STATE: OnceLock<Arc<AppState>> = OnceLock::new();
static INTERNAL_STATE: OnceLock<Arc<AppState>> = OnceLock::new();

/// 公共 HTTPS（web + /start + 指标）。fn 指针保证 `Server<fn(&Request)->Response>`
/// 可被 SIGHUP 重建（无闭包类型命名问题）。
fn public_handler(request: &Request) -> Response {
    let state = PUBLIC_STATE.get().expect("public state initialized");
    web_request(
        request,
        state.media_addr,
        state.tcp_addr,
        state.shard_txs.clone(),
        state.router.clone(),
        state.turn.clone(),
        state.shared.clone(),
    )
}

/// 内部 HTTP（信令代理专用，INTERNAL_TOKEN 保护）。
fn internal_handler(request: &Request) -> Response {
    let state = INTERNAL_STATE.get().expect("internal state initialized");
    if let Some(token) = &state.internal_token
        && request.header("X-Internal-Token") != Some(token.as_str())
    {
        return Response::text("forbidden").with_status_code(403);
    }
    web_request(
        request,
        state.media_addr,
        state.tcp_addr,
        state.shard_txs.clone(),
        state.router.clone(),
        state.turn.clone(),
        state.shared.clone(),
    )
}

/// `/healthz` 载荷：draining 时 503，否则 200。
fn healthz_payload(draining: bool, shard_count: usize, clients: usize) -> (u16, serde_json::Value) {
    let status = if draining { "draining" } else { "ok" };
    let code = if draining { 503 } else { 200 };
    (
        code,
        serde_json::json!({
            "status": status,
            "shards": shard_count,
            "clients": clients,
        }),
    )
}

/// `/metrics/prometheus`：Prometheus 文本格式（每分片 + 合计 + draining gauge）。
/// 保留 `/metrics` JSON 兼容 bench_report.py。
fn prometheus_body(shared: &Shared, draining: bool) -> String {
    let mut per_shard = String::new();
    let mut totals = [0u64; 5];
    for (i, m) in shared.metrics.iter().enumerate() {
        let c = m.clients.load(Ordering::Relaxed) as u64;
        let rxp = m.rx_packets.load(Ordering::Relaxed);
        let rxb = m.rx_bytes.load(Ordering::Relaxed);
        let txp = m.tx_packets.load(Ordering::Relaxed);
        let txb = m.tx_bytes.load(Ordering::Relaxed);
        totals[0] += c;
        totals[1] += rxp;
        totals[2] += rxb;
        totals[3] += txp;
        totals[4] += txb;
        per_shard.push_str(&format!(
            "aerodesk_sfu_clients{{shard=\"{i}\"}} {c}\n\
             aerodesk_sfu_rx_packets_total{{shard=\"{i}\"}} {rxp}\n\
             aerodesk_sfu_rx_bytes_total{{shard=\"{i}\"}} {rxb}\n\
             aerodesk_sfu_tx_packets_total{{shard=\"{i}\"}} {txp}\n\
             aerodesk_sfu_tx_bytes_total{{shard=\"{i}\"}} {txb}\n"
        ));
    }
    format!(
        "# TYPE aerodesk_sfu_clients gauge\n\
         # TYPE aerodesk_sfu_rx_packets_total counter\n\
         # TYPE aerodesk_sfu_rx_bytes_total counter\n\
         # TYPE aerodesk_sfu_tx_packets_total counter\n\
         # TYPE aerodesk_sfu_tx_bytes_total counter\n\
         # TYPE aerodesk_sfu_draining gauge\n\
         {per_shard}\
         aerodesk_sfu_clients {}\n\
         aerodesk_sfu_rx_packets_total {}\n\
         aerodesk_sfu_rx_bytes_total {}\n\
         aerodesk_sfu_tx_packets_total {}\n\
         aerodesk_sfu_tx_bytes_total {}\n\
         aerodesk_sfu_draining {}\n",
        totals[0],
        totals[1],
        totals[2],
        totals[3],
        totals[4],
        if draining { 1 } else { 0 }
    )
}

/// SIGHUP：重读 TLS 身份并重建公共 HTTPS server（旧连接由各自 TLS 会话继续）。
fn reload_tls(
    public: &mut Option<Server<fn(&Request) -> Response>>,
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
            info!("SIGHUP: reloading TLS identity from {}", new_tls.source);
            // 同端口不能同时绑定两个 server：先释放旧 listener 再绑定新的。
            public.take();
            match Server::new_ssl(
                format!("0.0.0.0:{SIGNAL_PORT}"),
                public_handler as fn(&Request) -> Response,
                new_tls.cert.clone(),
                new_tls.key.clone(),
            ) {
                Ok(srv) => {
                    *public = Some(srv);
                    *tls = new_tls;
                    info!("TLS reloaded (new connections use updated certificate)");
                }
                Err(e) => {
                    error!("TLS reload bind failed: {e}; restoring previous identity");
                    match Server::new_ssl(
                        format!("0.0.0.0:{SIGNAL_PORT}"),
                        public_handler as fn(&Request) -> Response,
                        tls.cert.clone(),
                        tls.key.clone(),
                    ) {
                        Ok(srv) => *public = Some(srv),
                        Err(e2) => error!(
                            "restore failed: {e2}; public HTTPS down until next SIGHUP or restart"
                        ),
                    }
                }
            }
        }
        Err(e) => error!("SIGHUP: TLS identity reload failed: {e}"),
    }
}

/// SIGTERM/SIGINT 优雅关闭：拒绝新房间（draining 已置位）→ 限时等现有客户端
/// 自行断开 → finalize 录制 → 退出。录制开启时保证 meta.json 落盘。
fn drain_and_exit(shared: &Shared, public: &Option<Server<fn(&Request) -> Response>>) -> ! {
    const DRAIN_GRACE: Duration = Duration::from_secs(3);
    info!("draining: rejecting new rooms; waiting up to 3s for existing clients");
    let deadline = Instant::now() + DRAIN_GRACE;
    while Instant::now() < deadline {
        if let Some(srv) = public {
            srv.poll_timeout(Duration::from_millis(50));
        }
        let clients: usize = shared
            .metrics
            .iter()
            .map(|m| m.clients.load(Ordering::Relaxed))
            .sum();
        if clients == 0 {
            break;
        }
    }
    if let Some(rec) = &shared.recorder {
        info!("finalizing recordings");
        rec.finalize_all();
    }
    info!("shutdown complete");
    std::process::exit(0);
}

pub fn main() {
    init_log();
    from_feature_flags().install_process_default();

    let shard_count = std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(1);
    info!("Shards: {shard_count}");

    let tls = aerodesk_protocol::tls::TlsIdentity::load().unwrap_or_else(|e| {
        eprintln!("fatal: TLS identity load failed: {e}");
        std::process::exit(1);
    });
    info!("TLS identity source: {}", tls.source);

    let host_addr = util::select_host_address();
    let media_addr = SocketAddr::new(host_addr, MEDIA_PORT);
    let tcp_listen_addr = media_addr;

    // TURN 配置（coturn REST secret；未设置则不下发）
    let turn = std::env::var("TURN_SECRET").ok().map(|secret| {
        let urls = std::env::var("TURN_URLS").unwrap_or_else(|_| {
            format!(
                "turn:{host_addr}:3478?transport=udp,turn:{host_addr}:3478?transport=tcp,turns:{host_addr}:5349?transport=tcp"
            )
        });
        let urls = urls.split(',').map(|u| u.to_string()).collect();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_secs();
        let creds = aerodesk_protocol::turn::generate_turn_credentials(&secret, "aerodesk", 3600, now);
        TurnConfig {
            urls,
            username: creds.username,
            credential: creds.credential,
        }
    });
    if turn.is_some() {
        info!("TURN relay configured (coturn REST credentials)");
    } else {
        warn!("TURN_SECRET not set: no TURN relay config will be issued");
    }

    // 1. UDP：每分片一个 SO_REUSEPORT socket
    let mut udp_sockets = Vec::new();
    for _ in 0..shard_count {
        udp_sockets.push(bind_udp_reuseport(media_addr).expect("bind UDP media socket"));
    }
    info!("Bound UDP media port: {media_addr} (x{shard_count} SO_REUSEPORT)");

    // 2. TCP/SSL-TCP：单个 listener + 全局读线程
    let (tcp_addr, tcp_rx) = tcp::spawn_tcp_listener(tcp_listen_addr);
    info!("Bound TCP/SSL-TCP media port: {tcp_addr}");

    // 3. 分片通道（先建 channel，后启线程）
    let mut shared = Shared::new(shard_count);
    if let Ok(dir) = std::env::var("RECORD_DIR") {
        match recorder::Recorder::new(&dir) {
            Ok(rec) => {
                let rec = Arc::new(rec);
                // SIGINT（Ctrl+C）时先 finalize 录制再退出，保证 meta.json 落盘。
                let rec_final = rec.clone();
                if ctrlc::set_handler(move || {
                    rec_final.finalize_all();
                    std::process::exit(0);
                })
                .is_ok()
                {
                    info!("recording: SIGINT handler registered");
                }
                shared.recorder = Some(rec);
                info!("recording enabled: {dir}");
            }
            Err(e) => warn!("RECORD_DIR set but recorder init failed: {e}"),
        }
    }
    let mut shard_txs: Vec<mpsc::Sender<ShardCommand>> = Vec::new();
    let mut shard_rxs = Vec::new();
    for _ in 0..shard_count {
        let (tx, rx) = mpsc::channel::<ShardCommand>();
        shard_txs.push(tx);
        shard_rxs.push(rx);
    }
    let (manager_tx, manager_rx) = mpsc::channel::<(usize, usize)>();
    let router = Arc::new(Mutex::new(router::ShardRouter::new(shard_count)));

    for i in 0..shard_count {
        let rx = shard_rxs.remove(0);
        let _handle = Shard::spawn(
            i,
            udp_sockets.remove(0),
            rx,
            shared.clone(),
            shard_txs.clone(),
            manager_tx.clone(),
        );
    }

    // 4. manager 线程：TCP 事件分发 + 分片负载更新
    {
        let shared = shared.clone();
        let shard_txs = shard_txs.clone();
        let router = router.clone();
        thread::Builder::new()
            .name("rd-manager".into())
            .spawn(move || {
                loop {
                    for ev in tcp_rx.try_iter() {
                        match ev {
                            tcp::TcpEvent::New { source, stream } => {
                                shared.tcp_streams.lock().unwrap().insert(source, stream);
                            }
                            tcp::TcpEvent::Close { source } => {
                                shared.tcp_streams.lock().unwrap().remove(&source);
                                shared
                                    .route_table
                                    .write()
                                    .unwrap()
                                    .remove(&(Protocol::Tcp, source));
                                shared
                                    .route_table
                                    .write()
                                    .unwrap()
                                    .remove(&(Protocol::SslTcp, source));
                            }
                            tcp::TcpEvent::Packet {
                                source,
                                proto,
                                data,
                            } => {
                                if let Some(target) = shared.lookup_route(proto, source) {
                                    let _ = shard_txs[target].send(ShardCommand::TcpPacket {
                                        source,
                                        proto,
                                        data,
                                    });
                                } else {
                                    // 未知：广播到所有分片（首个包认领后登记路由）
                                    for tx in &shard_txs {
                                        let _ = tx.send(ShardCommand::Cross(
                                            shard::CrossShardEvent::Packet {
                                                source,
                                                proto,
                                                data: data.clone(),
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    for (shard, clients) in manager_rx.try_iter() {
                        router.lock().unwrap().set_load(shard, clients);
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            })
            .expect("spawn manager thread");
    }

    // 5. 信令/HTTP：公共 HTTPS（web + /start + 指标）与内部 HTTP（信令代理专用）。
    //    rouille `poll_timeout` 轮询驱动：SIGHUP 可重建公共 server 热重载证书；
    //    SIGTERM/SIGINT 进入 drain（拒绝新房间 → 限时等现有客户端 → finalize 录制 → 退出）。
    let internal_token = std::env::var("INTERNAL_TOKEN").ok();
    let state = Arc::new(AppState {
        media_addr,
        tcp_addr,
        shard_txs,
        router,
        turn,
        shared: shared.clone(),
        internal_token,
    });
    let _ = PUBLIC_STATE.set(state.clone());
    let _ = INTERNAL_STATE.set(state);

    install_signal_handlers();

    let mut tls = tls;
    let mut public = match Server::new_ssl(
        format!("0.0.0.0:{SIGNAL_PORT}"),
        public_handler as fn(&Request) -> Response,
        tls.cert.clone(),
        tls.key.clone(),
    ) {
        Ok(srv) => {
            info!(
                "Connect a browser to https://{host_addr}:{}",
                srv.server_addr().port()
            );
            Some(srv)
        }
        Err(e) => {
            eprintln!("fatal: starting public HTTPS server failed: {e}");
            std::process::exit(1);
        }
    };

    let internal = match Server::new(
        format!("127.0.0.1:{INTERNAL_PORT}"),
        internal_handler as fn(&Request) -> Response,
    ) {
        Ok(srv) => {
            info!(
                "SFU internal API (HTTP) on 127.0.0.1:{}",
                srv.server_addr().port()
            );
            srv
        }
        Err(e) => {
            eprintln!("fatal: starting internal server failed: {e}");
            std::process::exit(1);
        }
    };
    // 内部接口不涉及 TLS，无需热重载；独立线程持续服务。
    std::thread::spawn(move || internal.run());

    // 主循环：轮询请求 + SIGHUP 证书热重载 + SIGTERM/SIGINT 优雅关闭。
    loop {
        if let Some(srv) = &public {
            srv.poll_timeout(Duration::from_millis(10));
        }
        if RELOAD_TLS.swap(false, Ordering::Relaxed) {
            reload_tls(&mut public, &mut tls);
        }
        if DRAINING.load(Ordering::Relaxed) {
            drain_and_exit(&shared, &public);
        }
    }
}

fn web_request(
    request: &Request,
    udp_addr: SocketAddr,
    tcp_addr: SocketAddr,
    shard_txs: Vec<mpsc::Sender<ShardCommand>>,
    router: Arc<Mutex<router::ShardRouter>>,
    turn: Option<TurnConfig>,
    shared: Shared,
) -> Response {
    if request.method() == "GET" && request.url() == "/metrics" {
        let metrics = shared.metrics.clone();
        let shards: Vec<serde_json::Value> = metrics
            .iter()
            .enumerate()
            .map(|(i, m)| {
                serde_json::json!({
                    "shard": i,
                    "clients": m.clients.load(std::sync::atomic::Ordering::Relaxed),
                    "rx_packets": m.rx_packets.load(std::sync::atomic::Ordering::Relaxed),
                    "rx_bytes": m.rx_bytes.load(std::sync::atomic::Ordering::Relaxed),
                    "tx_packets": m.tx_packets.load(std::sync::atomic::Ordering::Relaxed),
                    "tx_bytes": m.tx_bytes.load(std::sync::atomic::Ordering::Relaxed),
                })
            })
            .collect();
        let total: serde_json::Value = serde_json::json!({
            "shards": shards,
        });
        return Response::from_data(
            "application/json",
            serde_json::to_vec(&total).expect("serialize metrics"),
        );
    }

    if request.method() == "GET" && request.url() == "/healthz" {
        let draining = DRAINING.load(Ordering::Relaxed);
        let clients: usize = shared
            .metrics
            .iter()
            .map(|m| m.clients.load(Ordering::Relaxed))
            .sum();
        let (code, payload) = healthz_payload(draining, shared.metrics.len(), clients);
        return Response::from_data("application/json", serde_json::to_vec(&payload).unwrap())
            .with_status_code(code);
    }

    if request.method() == "GET" && request.url() == "/metrics/prometheus" {
        let body = prometheus_body(&shared, DRAINING.load(Ordering::Relaxed));
        return Response::from_data(
            "text/plain; version=0.0.4; charset=utf-8",
            body.into_bytes(),
        );
    }

    if request.method() == "GET" && request.url() == "/config" {
        let body =
            serde_json::to_vec(&serde_json::json!({ "turn": turn })).expect("serialize config");
        return Response::from_data("application/json", body);
    }

    if request.method() == "GET" {
        return Response::html(include_str!("../../../web/index.html"));
    }

    // 优雅关闭中：拒绝新房间（503），已有连接继续服务直至 drain 超时。
    if DRAINING.load(Ordering::Relaxed) {
        return Response::text("draining").with_status_code(503);
    }

    // POST /start?room=xxx&role=xxx
    let query = request.raw_query_string();
    let param = |key: &str| {
        query
            .split('&')
            .find(|kv| kv.starts_with(&format!("{key}=")))
            .map(|kv| kv[key.len() + 1..].to_string())
    };
    let room = param("room").unwrap_or_else(|| "default".to_string());
    // #12：角色必填（信令代理必带）；viewer 禁止发布媒体。
    let role = match param("role").as_deref() {
        Some("publisher") => Role::Publisher,
        Some("viewer") => Role::Viewer,
        _ => {
            return Response::text("role required (publisher|viewer)").with_status_code(403);
        }
    };

    let mut data = request.data().expect("body to be available");
    let offer: str0m::change::SdpOffer =
        serde_json::from_reader(&mut data).expect("serialized offer");
    if role == Role::Viewer && shard::offer_sends_media(&offer.to_sdp_string()) {
        warn!("拒绝 viewer 发布媒体：room={room}（#12）");
        return Response::text("viewer cannot publish media").with_status_code(403);
    }

    let mut rtc = Rtc::builder();
    {
        let cfg = rtc.codec_config();
        // #58 音频：启用 PCMU，转发 publisher 的 G.711 音频（默认配置只有 Opus）。
        cfg.enable_pcmu(true);
    }
    let mut rtc = rtc.build(std::time::Instant::now());
    let candidate = Candidate::host(udp_addr, "udp").expect("a host candidate");
    rtc.add_local_candidate(candidate).unwrap();
    let tcp_candidate = Candidate::builder()
        .tcp()
        .host(tcp_addr)
        .tcptype(TcpType::Passive)
        .build()
        .expect("a TCP host candidate");
    rtc.add_local_candidate(tcp_candidate).unwrap();
    let ssltcp_candidate = Candidate::builder()
        .ssl_tcp()
        .host(tcp_addr)
        .tcptype(TcpType::Passive)
        .build()
        .expect("a SSL-TCP host candidate");
    rtc.add_local_candidate(ssltcp_candidate).unwrap();

    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .expect("offer to be accepted");

    // 房间 → 分片路由（哈希 locality + 负载级联）
    let shard = router.lock().unwrap().choose(&room);
    info!("POST /start room={room} -> shard {shard}");
    let res = shard_txs[shard].send(ShardCommand::AddClient { rtc, room, role });
    if res.is_err() {
        warn!("Failed to deliver client to shard {shard}");
    }

    let body = serde_json::to_vec(&answer).expect("answer to serialize");
    Response::from_data("application/json", body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shard::Shared;
    use std::io::Read;

    #[test]
    fn healthz_ok_vs_draining() {
        let (code, payload) = healthz_payload(false, 4, 7);
        assert_eq!(code, 200);
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["shards"], 4);
        assert_eq!(payload["clients"], 7);

        let (code, payload) = healthz_payload(true, 4, 7);
        assert_eq!(code, 503);
        assert_eq!(payload["status"], "draining");
    }

    #[test]
    fn prometheus_contains_shards_totals_and_draining() {
        let shared = Shared::new(2);
        shared.metrics[0].clients.store(3, Ordering::Relaxed);
        shared.metrics[1].clients.store(4, Ordering::Relaxed);
        shared.metrics[0].rx_packets.store(100, Ordering::Relaxed);
        shared.metrics[1].tx_bytes.store(2048, Ordering::Relaxed);

        let body = prometheus_body(&shared, true);
        assert!(
            body.contains("aerodesk_sfu_clients{shard=\"0\"} 3"),
            "{body}"
        );
        assert!(
            body.contains("aerodesk_sfu_clients{shard=\"1\"} 4"),
            "{body}"
        );
        assert!(body.contains("aerodesk_sfu_clients 7"), "{body}");
        assert!(body.contains("aerodesk_sfu_rx_packets_total 100"), "{body}");
        assert!(body.contains("aerodesk_sfu_tx_bytes_total 2048"), "{body}");
        assert!(body.contains("aerodesk_sfu_draining 1"), "{body}");

        let body = prometheus_body(&shared, false);
        assert!(body.contains("aerodesk_sfu_draining 0"), "{body}");
    }

    #[test]
    fn healthz_endpoint_ok() {
        let shared = Shared::new(1);
        let router = Arc::new(Mutex::new(crate::router::ShardRouter::new(1)));
        let req = Request::fake_http("GET", "/healthz", vec![], Vec::new());
        let resp = web_request(
            &req,
            "127.0.0.1:3478".parse().unwrap(),
            "127.0.0.1:3478".parse().unwrap(),
            Vec::new(),
            router,
            None,
            shared,
        );
        assert_eq!(resp.status_code, 200);
    }

    #[test]
    fn prometheus_endpoint_ok() {
        let shared = Shared::new(1);
        let router = Arc::new(Mutex::new(crate::router::ShardRouter::new(1)));
        let req = Request::fake_http("GET", "/metrics/prometheus", vec![], Vec::new());
        let resp = web_request(
            &req,
            "127.0.0.1:3478".parse().unwrap(),
            "127.0.0.1:3478".parse().unwrap(),
            Vec::new(),
            router,
            None,
            shared,
        );
        assert_eq!(resp.status_code, 200);
        let (mut reader, _size) = resp.data.into_reader_and_size();
        let mut body = String::new();
        let ok = reader.read_to_string(&mut body).is_ok() && body.contains("aerodesk_sfu_clients");
        assert!(ok, "prometheus body expected, got: {body:.80}");
    }

    #[test]
    fn start_rejected_while_draining() {
        DRAINING.store(true, Ordering::Relaxed);
        let shared = Shared::new(1);
        let router = Arc::new(Mutex::new(crate::router::ShardRouter::new(1)));
        let req = Request::fake_http("POST", "/start?room=x&role=viewer", vec![], Vec::new());
        let resp = web_request(
            &req,
            "127.0.0.1:3478".parse().unwrap(),
            "127.0.0.1:3478".parse().unwrap(),
            Vec::new(),
            router,
            None,
            shared,
        );
        DRAINING.store(false, Ordering::Relaxed);
        assert_eq!(resp.status_code, 503);
    }
}
