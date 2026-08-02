//! AeroDesk SFU 服务端。
//!
//! 多核分片架构（P1）：
//! - 每分片一个线程 + 一个 SO_REUSEPORT UDP socket（同一端口 3478）
//! - 房间 → 分片哈希路由（同房间同分片优先）
//! - 跨分片：媒体/关键帧/输入通道事件 + UDP 包转投
//! - TCP/SSL-TCP：全局 accept/读线程，按路由表分发到分片

#[macro_use]
extern crate tracing;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rouille::Server;
use rouille::{Request, Response};
use str0m::crypto::from_feature_flags;
use str0m::net::TcpType;
use str0m::{Candidate, Rtc, net::Protocol};

mod router;
mod shard;
mod tcp;
mod turn;
mod util;

use aerodesk_protocol::signal::TurnConfig;
use shard::{Shard, ShardCommand, Shared};

/// 统一媒体端口（UDP + TCP + SSL-TCP 复用）。生产用 443。
const MEDIA_PORT: u16 = 3478;
const SIGNAL_PORT: u16 = 3000;

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
    sock.set_reuse_port(true)?;
    sock.bind(&addr.into())?;
    Ok(std::net::UdpSocket::from(sock))
}

pub fn main() {
    init_log();
    from_feature_flags().install_process_default();

    let shard_count = std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(1);
    info!("Shards: {shard_count}");

    let certificate = include_bytes!("../../../certs/cer.pem").to_vec();
    let private_key = include_bytes!("../../../certs/key.pem").to_vec();

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
        let creds = turn::generate_turn_credentials(&secret, "aerodesk", 3600, now);
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
    let shared = Shared::new();
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

    // 5. 信令（HTTPS）
    let shard_txs_web = shard_txs.clone();
    let router_web = router.clone();
    let server = Server::new_ssl(
        format!("0.0.0.0:{SIGNAL_PORT}"),
        move |request| {
            web_request(
                request,
                media_addr,
                tcp_addr,
                shard_txs_web.clone(),
                router_web.clone(),
                turn.clone(),
            )
        },
        certificate,
        private_key,
    )
    .expect("starting the web server");

    let port = server.server_addr().port();
    info!("Connect a browser to https://{:?}:{:?}", host_addr, port);
    server.run();
}

fn web_request(
    request: &Request,
    udp_addr: SocketAddr,
    tcp_addr: SocketAddr,
    shard_txs: Vec<mpsc::Sender<ShardCommand>>,
    router: Arc<Mutex<router::ShardRouter>>,
    turn: Option<TurnConfig>,
) -> Response {
    if request.method() == "GET" && request.url() == "/config" {
        let body =
            serde_json::to_vec(&serde_json::json!({ "turn": turn })).expect("serialize config");
        return Response::from_data("application/json", body);
    }

    if request.method() == "GET" {
        return Response::html(include_str!("../../../web/index.html"));
    }

    // POST /start?room=xxx
    let room = request
        .raw_query_string()
        .split('&')
        .find(|kv| kv.starts_with("room="))
        .map(|kv| kv[5..].to_string())
        .unwrap_or_else(|| "default".to_string());

    let mut data = request.data().expect("body to be available");
    let offer: str0m::change::SdpOffer =
        serde_json::from_reader(&mut data).expect("serialized offer");

    let mut rtc = Rtc::builder().build(std::time::Instant::now());
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
    let res = shard_txs[shard].send(ShardCommand::AddClient { rtc, room });
    if res.is_err() {
        warn!("Failed to deliver client to shard {shard}");
    }

    let body = serde_json::to_vec(&answer).expect("answer to serialize");
    Response::from_data("application/json", body)
}
