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
mod router;
mod shard;
mod tcp;
mod turn_server;
mod util;

use aerodesk_protocol::signal::{Role, TurnConfig};
use aerodesk_sfu::recorder::Recorder;
use shard::{Shard, ShardCommand, Shared};

/// 统一媒体端口（UDP + TCP + SSL-TCP 复用）。生产用 443。
/// 默认端口；可用环境变量覆盖（支持单机多 PoP 测试，如 multipop-e2e，#146）。
const MEDIA_PORT: u16 = 3478;
const SIGNAL_PORT: u16 = 3000;
/// SFU 内部接口（信令服务代理用，仅本机回环）。
const INTERNAL_PORT: u16 = 3002;

/// 读环境变量端口（非法/缺失时回退默认）。
fn env_port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// 解析 IP 字符串（非法返回 None）。
fn parse_ip(s: &str) -> Option<std::net::IpAddr> {
    s.parse().ok()
}

/// 读环境变量 IP（缺失/非法返回 None，由调用方回退自动选择）。
fn env_ip(name: &str) -> Option<std::net::IpAddr> {
    std::env::var(name).ok().and_then(|v| parse_ip(&v))
}

/// 解析 `/proc/<pid>/stat` 的 utime+stime（字段 14/15，单位 clock ticks）。
/// 纯函数便于单测（无需真实 /proc）。
fn parse_thread_stat_ticks(stat: &str) -> Option<u64> {
    let rp = stat.rfind(')')?;
    // ") " 之后第一个字段是 state（字段 3），utime=字段 14、stime=字段 15。
    let fields: Vec<&str> = stat[rp + 2..].split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// 读线程累计 CPU ticks（Linux；非法/失败返回 0）。
#[cfg(target_os = "linux")]
fn read_thread_ticks(tid: i32) -> u64 {
    if tid <= 0 {
        return 0;
    }
    std::fs::read_to_string(format!("/proc/self/task/{tid}/stat"))
        .ok()
        .and_then(|s| parse_thread_stat_ticks(&s))
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_sec() -> u64 {
    // SAFETY: `sysconf` 无失败语义；<=0 时回退常见的 100。
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as u64 } else { 100 }
}

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
    /// 对外通告的 ICE host 候选地址（SFU_HOST_ADDRESS 覆盖；默认=自动选择）。
    candidate_udp_addr: SocketAddr,
    candidate_tcp_addr: SocketAddr,
    shard_txs: Vec<mpsc::Sender<ShardCommand>>,
    router: Arc<Mutex<router::ShardRouter>>,
    turn: Option<TurnConfig>,
    /// 内嵌 TURN server 句柄（#220：暴露 allocation 指标；外部 TURN_URLS 时为 None）。
    turn_server: Option<Arc<turn_server::TurnServer>>,
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
        state.candidate_udp_addr,
        state.candidate_tcp_addr,
        state.shard_txs.clone(),
        state.router.clone(),
        state.turn.clone(),
        state.turn_server.clone(),
        state.shared.clone(),
        false,
    )
}

/// 内部 HTTP（信令代理专用，INTERNAL_TOKEN 保护）。
fn internal_handler(request: &Request) -> Response {
    let state = INTERNAL_STATE.get().expect("internal state initialized");
    if let Some(token) = &state.internal_token
        && request.header("X-Internal-Token") != Some(token.as_str())
    {
        // #240：未授权调用留痕（audit.log 存在时写 record_api/session_api 403，
        // 否则以 tracing 日志兜底），供 SIEM/追责。
        audit_denied_api(&state.shared, request);
        return Response::text("forbidden").with_status_code(403);
    }
    web_request(
        request,
        state.candidate_udp_addr,
        state.candidate_tcp_addr,
        state.shard_txs.clone(),
        state.router.clone(),
        state.turn.clone(),
        state.turn_server.clone(),
        state.shared.clone(),
        true,
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
fn prometheus_body(
    shared: &Shared,
    draining: bool,
    turn: Option<&turn_server::TurnServer>,
    loads: &[f64],
) -> String {
    let mut per_shard = String::new();
    let mut totals = [0u64; 10];
    for (i, m) in shared.metrics.iter().enumerate() {
        let c = m.clients.load(Ordering::Relaxed) as u64;
        let rxp = m.rx_packets.load(Ordering::Relaxed);
        let rxb = m.rx_bytes.load(Ordering::Relaxed);
        let txp = m.tx_packets.load(Ordering::Relaxed);
        let txb = m.tx_bytes.load(Ordering::Relaxed);
        // #238 媒体质量（最近 5s 心跳聚合；rtt 0=无样本）
        let rtt = m.rtt_avg_ns.load(Ordering::Relaxed) / 1000;
        let el = m.egress_loss_ppm.load(Ordering::Relaxed) as f64 / 1e6;
        let il = m.ingress_loss_ppm.load(Ordering::Relaxed) as f64 / 1e6;
        let bw = m.bwe_tx_bps.load(Ordering::Relaxed);
        let qc = m.qos_clients.load(Ordering::Relaxed) as u64;
        // 分片负载评分（router 的 client+pps 加权，0..=1）——观测容量/级联。
        let load = loads.get(i).copied().unwrap_or(0.0);
        // 分片线程 CPU（%×100 → 0.0..=100.0；非 Linux 恒 0）。
        let cpu = m.cpu_percent_x100.load(Ordering::Relaxed) as f64 / 100.0;
        totals[0] += c;
        totals[1] += rxp;
        totals[2] += rxb;
        totals[3] += txp;
        totals[4] += txb;
        // 总量按客户端数加权（跨分片语义正确）：rtt 按 qos_clients 加权、
        // loss 按 clients 加权、bwe 汇总为分片总带宽（而非平均带宽之和）。
        totals[5] += rtt * qc;
        totals[6] += m.egress_loss_ppm.load(Ordering::Relaxed) * c;
        totals[7] += m.ingress_loss_ppm.load(Ordering::Relaxed) * c;
        totals[8] += bw * c;
        totals[9] += qc;
        per_shard.push_str(&format!(
            "aerodesk_sfu_clients{{shard=\"{i}\"}} {c}\n\
             aerodesk_sfu_rx_packets_total{{shard=\"{i}\"}} {rxp}\n\
             aerodesk_sfu_rx_bytes_total{{shard=\"{i}\"}} {rxb}\n\
             aerodesk_sfu_tx_packets_total{{shard=\"{i}\"}} {txp}\n\
             aerodesk_sfu_tx_bytes_total{{shard=\"{i}\"}} {txb}\n\
             aerodesk_sfu_rtt_us{{shard=\"{i}\"}} {rtt}\n\
             aerodesk_sfu_egress_loss{{shard=\"{i}\"}} {el:.6}\n\
             aerodesk_sfu_ingress_loss{{shard=\"{i}\"}} {il:.6}\n\
             aerodesk_sfu_bwe_tx_bps{{shard=\"{i}\"}} {bw}\n\
             aerodesk_sfu_qos_clients{{shard=\"{i}\"}} {qc}\n\
             aerodesk_sfu_shard_load{{shard=\"{i}\"}} {load:.4}\n\
             aerodesk_sfu_shard_cpu{{shard=\"{i}\"}} {cpu:.2}\n"
        ));
    }
    let turn_metrics = match turn {
        Some(srv) => format!(
            "# TYPE aerodesk_sfu_turn_allocations gauge\n\
             # TYPE aerodesk_sfu_turn_allocations_total counter\n\
             # TYPE aerodesk_sfu_turn_evictions_total counter\n\
             aerodesk_sfu_turn_allocations {}\n\
             aerodesk_sfu_turn_allocations_total {}\n\
             aerodesk_sfu_turn_evictions_total {}\n",
            srv.active_allocations(),
            srv.allocations_total(),
            srv.evictions_total()
        ),
        None => String::new(),
    };
    // #240：在录房间数（RECORD_DIR 未开时为 0），供录制停摆/水位告警。
    let recordings_active = shared
        .recorder
        .as_ref()
        .map(|r| r.active_count())
        .unwrap_or(0);
    let total_clients = totals[0].max(1) as f64;
    let total_qos = totals[9].max(1) as f64;
    // 跨分片加权总量：与分片指标单位一致（rtt 微秒整数、loss 0..=1）。
    let rtt_total_us = (totals[5] as f64 / total_qos).round() as u64;
    let egress_total = totals[6] as f64 / total_clients / 1e6;
    let ingress_total = totals[7] as f64 / total_clients / 1e6;
    format!(
        "# TYPE aerodesk_sfu_clients gauge\n\
         # TYPE aerodesk_sfu_rx_packets_total counter\n\
         # TYPE aerodesk_sfu_rx_bytes_total counter\n\
         # TYPE aerodesk_sfu_tx_packets_total counter\n\
         # TYPE aerodesk_sfu_tx_bytes_total counter\n\
         # TYPE aerodesk_sfu_rtt_us gauge\n\
         # TYPE aerodesk_sfu_egress_loss gauge\n\
         # TYPE aerodesk_sfu_ingress_loss gauge\n\
         # TYPE aerodesk_sfu_bwe_tx_bps gauge\n\
         # TYPE aerodesk_sfu_qos_clients gauge\n\
         # TYPE aerodesk_sfu_shard_load gauge\n\
         # TYPE aerodesk_sfu_shard_cpu gauge\n\
         # TYPE aerodesk_sfu_recordings_active gauge\n\
         # TYPE aerodesk_sfu_draining gauge\n\
         {per_shard}\
         aerodesk_sfu_clients {}\n\
         aerodesk_sfu_rx_packets_total {}\n\
         aerodesk_sfu_rx_bytes_total {}\n\
         aerodesk_sfu_tx_packets_total {}\n\
         aerodesk_sfu_tx_bytes_total {}\n\
         aerodesk_sfu_rtt_us {}\n\
         aerodesk_sfu_egress_loss {:.6}\n\
         aerodesk_sfu_ingress_loss {:.6}\n\
         aerodesk_sfu_bwe_tx_bps {}\n\
         aerodesk_sfu_qos_clients {}\n\
         aerodesk_sfu_recordings_active {}\n\
         aerodesk_sfu_draining {}\n\
         {turn_metrics}",
        totals[0],
        totals[1],
        totals[2],
        totals[3],
        totals[4],
        rtt_total_us,
        egress_total,
        ingress_total,
        totals[8],
        totals[9],
        recordings_active,
        if draining { 1 } else { 0 }
    )
}

/// SIGHUP：重读 TLS 身份并重建公共 HTTPS server（旧连接由各自 TLS 会话继续）。
/// 带重试的公共 HTTPS server 绑定：旧 listener 释放后端口可能短暂 EADDRINUSE
/// （macOS 实测，signal 同款修复），重试可自愈；失败返回最后一次错误。
fn bind_public_with_retry(
    cert: &[u8],
    key: &[u8],
    attempts: usize,
) -> Result<Server<fn(&Request) -> Response>, String> {
    let mut last_err = String::new();
    for i in 0..attempts {
        match Server::new_ssl(
            format!("0.0.0.0:{}", env_port("SFU_SIGNAL_PORT", SIGNAL_PORT)),
            public_handler as fn(&Request) -> Response,
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
            match bind_public_with_retry(&new_tls.cert, &new_tls.key, 20) {
                Ok(srv) => {
                    *public = Some(srv);
                    *tls = new_tls;
                    info!("TLS reloaded (new connections use updated certificate)");
                }
                Err(e) => {
                    error!("TLS reload bind failed: {e}; restoring previous identity");
                    match bind_public_with_retry(&tls.cert, &tls.key, 20) {
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

/// 解析分片数：`SFU_SHARD_COUNT` 覆盖（1..=64），否则用 CPU 核数（上限 8）。
/// 供不同规格机器/容器按容量基线调整并发分片（可扩展性旋钮）。
fn resolve_shard_count() -> usize {
    match std::env::var("SFU_SHARD_COUNT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        Some(n) if (1..=64).contains(&n) => n,
        _ => std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(1),
    }
}

pub fn main() {
    init_log();
    from_feature_flags().install_process_default();

    let shard_count = resolve_shard_count();
    info!("Shards: {shard_count}");

    let tls = aerodesk_protocol::tls::TlsIdentity::load().unwrap_or_else(|e| {
        eprintln!("fatal: TLS identity load failed: {e}");
        std::process::exit(1);
    });
    info!("TLS identity source: {}", tls.source);

    // #216 真实部署：SFU_HOST_ADDRESS 覆盖对外通告地址（ICE 候选/TURN/web 地址），
    // SFU_BIND_ADDRESS 覆盖绑定地址（默认跟随通告）。NAT/带 docker0 等虚拟网卡的
    // 服务器建议 SFU_HOST_ADDRESS=<公网IP> + SFU_BIND_ADDRESS=0.0.0.0（见 DEPLOYMENT.md）。
    let host_override = env_ip("SFU_HOST_ADDRESS");
    let host_addr = host_override.unwrap_or_else(util::select_host_address);
    // SFU_BIND_ADDRESS 未设时：显式通告外部地址 → 默认通配绑定 0.0.0.0（NAT/虚拟网卡）；
    // 否则跟随自动选择的通告地址（保持原行为）。
    let bind_addr = env_ip("SFU_BIND_ADDRESS").unwrap_or_else(|| {
        if host_override.is_some() {
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
        } else {
            host_addr
        }
    });
    let media_port = env_port("SFU_MEDIA_PORT", MEDIA_PORT);
    let media_addr = SocketAddr::new(bind_addr, media_port);
    let tcp_listen_addr = media_addr;
    let candidate_udp_addr = SocketAddr::new(host_addr, media_port);
    if host_addr != bind_addr {
        info!("advertised host address: {host_addr} (media bind {bind_addr})");
    }

    // TURN 配置（#191：TURN_SECRET 未设置则不下发；显式 TURN_URLS 走外部 coturn；
    // 否则启动内嵌 TURN+STUN server（SFU_TURN_PORT，默认 3479，与媒体 3478 不冲突）。
    let turn_secret = std::env::var("TURN_SECRET").ok();
    let mut embedded_turn: Option<Arc<turn_server::TurnServer>> = None;
    let turn = turn_secret.as_ref().and_then(|secret| {
        let urls: Vec<String> = match std::env::var("TURN_URLS") {
            // 空字符串视为未设置（#191：走内嵌 server）。
            Ok(u) if !u.is_empty() => u.split(',').map(|s| s.to_string()).collect(),
            _ => {
                // #196：UDP + TCP 同端口（SFU_TURN_PORT），TLS 用 SFU_TURN_TLS_PORT（默认 5349）。
                let turn_port = env_port("SFU_TURN_PORT", 3479);
                let turn_tls_port = env_port("SFU_TURN_TLS_PORT", 5349);
                match turn_server::spawn(secret, host_addr, turn_port, Some(turn_tls_port)) {
                    Ok(srv) => {
                        embedded_turn = Some(Arc::new(srv));
                        let srv = embedded_turn.as_ref().expect("just set");
                        let mut urls = vec![format!("turn:{}?transport=udp", srv.udp_addr)];
                        if let Some(tcp) = srv.tcp_addr {
                            urls.push(format!("turn:{tcp}?transport=tcp"));
                        }
                        if let Some(tls) = srv.tls_addr {
                            urls.push(format!("turns:{tls}?transport=tcp"));
                        }
                        info!("embedded TURN server: {}", urls.join(","));
                        urls
                    }
                    Err(e) => {
                        warn!("embedded TURN server failed ({e}); no TURN relay issued");
                        return None;
                    }
                }
            }
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_secs();
        let creds =
            aerodesk_protocol::turn::generate_turn_credentials(secret, "aerodesk", 3600, now);
        Some(TurnConfig {
            urls,
            username: creds.username,
            credential: creds.credential,
        })
    });
    if turn.is_some() {
        info!("TURN relay configured (embedded server or coturn REST credentials)");
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
    let candidate_tcp_addr = SocketAddr::new(host_addr, tcp_addr.port());

    // 3. 分片通道（先建 channel，后启线程）
    let mut shared = Shared::new(shard_count);
    // #180 /start 准入配额（0=不限；信令层 #163/#171 之外的纵深防御）。
    shared.max_room_clients = std::env::var("MAX_ROOM_CLIENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    shared.max_total_clients = std::env::var("MAX_TOTAL_CLIENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if let Ok(dir) = std::env::var("RECORD_DIR") {
        // #160：RECORD_ON_DEMAND=1 时只录显式 start() 的房间（按需录制 API）。
        let on_demand = std::env::var("RECORD_ON_DEMAND")
            .map(|v| v == "1")
            .unwrap_or(false);
        // #180 录制轮转（RECORD_MAX_BYTES / RECORD_MAX_SECS，0=不限）。
        let max_bytes: u64 = std::env::var("RECORD_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let max_secs: u64 = std::env::var("RECORD_MAX_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
            * 1_000_000; // env 为秒，内部微秒
        // 审计日志轮转上限（0=不限；超限归档为 audit.log.1）。
        let audit_max_bytes: u64 = std::env::var("AUDIT_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        match aerodesk_sfu::recorder::Recorder::new_with_audit(
            &dir,
            on_demand,
            max_bytes,
            max_secs,
            audit_max_bytes,
        ) {
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
                let shard_count = shared.metrics.len();
                let mut clients = vec![0usize; shard_count];
                let mut last_counters = vec![(0u64, 0u64); shard_count];
                let mut last_sample = Instant::now();
                #[cfg(target_os = "linux")]
                let mut last_cpu_ticks = vec![0u64; shard_count];
                loop {
                    for ev in tcp_rx.try_iter() {
                        match ev {
                            tcp::TcpEvent::New { source, stream } => {
                                shared
                                    .tcp_streams
                                    .lock()
                                    .unwrap_or_else(aerodesk_protocol::util::lock_recover)
                                    .insert(source, stream);
                            }
                            tcp::TcpEvent::Close { source } => {
                                shared
                                    .tcp_streams
                                    .lock()
                                    .unwrap_or_else(aerodesk_protocol::util::lock_recover)
                                    .remove(&source);
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
                    for (shard, count) in manager_rx.try_iter() {
                        clients[shard] = count;
                    }
                    // 每 500ms 采样每分片 rx/tx 包率，喂给包率感知负载路由（#130）。
                    let now = Instant::now();
                    if now.duration_since(last_sample) >= Duration::from_millis(500) {
                        let dt = now.duration_since(last_sample).as_secs_f64().max(0.001);
                        for i in 0..shard_count {
                            let m = &shared.metrics[i];
                            let rx = m.rx_packets.load(Ordering::Relaxed);
                            let tx = m.tx_packets.load(Ordering::Relaxed);
                            let rx_pps = rx.saturating_sub(last_counters[i].0) as f64 / dt;
                            let tx_pps = tx.saturating_sub(last_counters[i].1) as f64 / dt;
                            last_counters[i] = (rx, tx);
                            router
                                .lock()
                                .unwrap()
                                .set_load(i, clients[i], rx_pps, tx_pps);
                        }
                        #[cfg(target_os = "linux")]
                        {
                            // 每分片线程 CPU（%×100）：/proc 累计 ticks 增量 / 墙钟增量。
                            let tps = clock_ticks_per_sec() as f64;
                            for i in 0..shard_count {
                                let tid = shared.shard_tids[i].load(Ordering::Relaxed);
                                let ticks = read_thread_ticks(tid);
                                let cpu_x100 =
                                    if last_cpu_ticks[i] > 0 && ticks >= last_cpu_ticks[i] {
                                        let delta = (ticks - last_cpu_ticks[i]) as f64;
                                        (delta * 10_000.0 / tps / dt).round() as u64
                                    } else {
                                        0
                                    };
                                last_cpu_ticks[i] = ticks;
                                shared.metrics[i]
                                    .cpu_percent_x100
                                    .store(cpu_x100.min(10_000), Ordering::Relaxed);
                            }
                        }
                        last_sample = now;
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
    if internal_token.is_none() {
        // #240：record/session 管理接口（含踢人）未受保护，仅限开发/回环使用。
        warn!(
            "INTERNAL_TOKEN not set: /record/* and /session/* management API are UNAUTHENTICATED (loopback only; production must set INTERNAL_TOKEN)"
        );
    }
    let state = Arc::new(AppState {
        candidate_udp_addr,
        candidate_tcp_addr,
        shard_txs,
        router,
        turn,
        turn_server: embedded_turn,
        shared: shared.clone(),
        internal_token,
    });
    let _ = PUBLIC_STATE.set(state.clone());
    let _ = INTERNAL_STATE.set(state);

    install_signal_handlers();

    let mut tls = tls;
    let mut public = match Server::new_ssl(
        format!("0.0.0.0:{}", env_port("SFU_SIGNAL_PORT", SIGNAL_PORT)),
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
        format!("127.0.0.1:{}", env_port("SFU_INTERNAL_PORT", INTERNAL_PORT)),
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

/// 查询参数（首个匹配）。
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .find(|kv| kv.starts_with(&format!("{key}=")))
        .map(|kv| &kv[key.len() + 1..])
}

fn recorder_or_503(shared: &Shared) -> Result<&Recorder, Response> {
    shared.recorder.as_deref().ok_or_else(|| {
        Response::text("recording disabled (RECORD_DIR not set)").with_status_code(503)
    })
}

/// 内部 API 审计（#240）：成功/失败/403 都留痕。audit.log 存在时写
/// `record_api`/`session_api` 事件（含 action/room/status/ok/detail），
/// 否则 tracing 日志兜底。
fn audit_api_event(
    shared: &Shared,
    event: &str,
    action: &str,
    room: Option<&str>,
    status: u16,
    ok: bool,
    detail: Option<&str>,
) {
    let room = room.unwrap_or("");
    if let Some(rec) = &shared.recorder {
        rec.audit_event(
            event,
            serde_json::json!({
                "action": action,
                "room": room,
                "status": status,
                "ok": ok,
                "detail": detail.unwrap_or(""),
            }),
        );
    }
    if ok {
        info!("internal api {action} room={room} status={status}");
    } else {
        warn!(
            "internal api {action} room={room} status={status} detail={:?}",
            detail
        );
    }
}

fn audit_record_api(
    shared: &Shared,
    action: &str,
    room: Option<&str>,
    status: u16,
    ok: bool,
    detail: Option<&str>,
) {
    audit_api_event(shared, "record_api", action, room, status, ok, detail);
}

fn audit_session_api(
    shared: &Shared,
    action: &str,
    room: Option<&str>,
    status: u16,
    ok: bool,
    detail: Option<&str>,
) {
    audit_api_event(shared, "session_api", action, room, status, ok, detail);
}

/// 未授权内部调用审计（#240）：仅对录制/会话管理类路径写 audit.log（带 query
/// 中的 room/client 便于 SIEM 关联），避免探测性请求刷爆审计日志（其它路径走
/// tracing warn）。
fn audit_denied_api(shared: &Shared, request: &Request) {
    let url = request.url();
    let action = url.trim_start_matches('/').to_string();
    let room = query_param(request.raw_query_string(), "room");
    let client = query_param(request.raw_query_string(), "client");
    if url.starts_with("/record/") {
        audit_record_api(shared, &action, room, 403, false, Some("forbidden"));
    } else if url.starts_with("/session/") {
        let detail = client
            .map(|c| format!("forbidden client={c}"))
            .unwrap_or_else(|| "forbidden".to_string());
        audit_session_api(shared, &action, room, 403, false, Some(&detail));
    } else {
        warn!("internal api denied: {action}");
    }
}

/// POST /record/start?room=xxx（内部接口，#160）。
fn record_start(shared: &Shared, room: Option<&str>) -> Response {
    let Some(room) = room.filter(|r| !r.is_empty()) else {
        audit_record_api(
            shared,
            "record/start",
            None,
            400,
            false,
            Some("room required"),
        );
        return Response::text("room required").with_status_code(400);
    };
    let rec = match recorder_or_503(shared) {
        Ok(r) => r,
        Err(_) => {
            audit_record_api(
                shared,
                "record/start",
                Some(room),
                503,
                false,
                Some("RECORD_DIR not set"),
            );
            return Response::text("recording disabled (RECORD_DIR not set)").with_status_code(503);
        }
    };
    match rec.start(room) {
        Ok(()) => {
            audit_record_api(shared, "record/start", Some(room), 200, true, None);
            Response::from_data(
                "application/json",
                serde_json::to_vec(&serde_json::json!({ "room": room, "started": true })).unwrap(),
            )
        }
        Err(e) => {
            audit_record_api(shared, "record/start", Some(room), 500, false, Some(&e));
            Response::text(format!("start failed: {e}")).with_status_code(500)
        }
    }
}

/// POST /record/stop?room=xxx（内部接口，#160）。
fn record_stop(shared: &Shared, room: Option<&str>) -> Response {
    let Some(room) = room.filter(|r| !r.is_empty()) else {
        audit_record_api(
            shared,
            "record/stop",
            None,
            400,
            false,
            Some("room required"),
        );
        return Response::text("room required").with_status_code(400);
    };
    let rec = match recorder_or_503(shared) {
        Ok(r) => r,
        Err(_) => {
            audit_record_api(
                shared,
                "record/stop",
                Some(room),
                503,
                false,
                Some("RECORD_DIR not set"),
            );
            return Response::text("recording disabled (RECORD_DIR not set)").with_status_code(503);
        }
    };
    let stopped = rec.stop(room);
    if stopped {
        audit_record_api(shared, "record/stop", Some(room), 200, true, None);
    } else {
        audit_record_api(
            shared,
            "record/stop",
            Some(room),
            200,
            true,
            Some("already stopped"),
        );
    }
    Response::from_data(
        "application/json",
        serde_json::to_vec(&serde_json::json!({ "room": room, "stopped": stopped })).unwrap(),
    )
}

/// GET /record/status（内部接口，#160）。只读查询不写 audit.log（避免轮询刷爆
/// 审计日志），以 tracing debug 留痕。
fn record_status(shared: &Shared) -> Response {
    let rec = match recorder_or_503(shared) {
        Ok(r) => r,
        Err(_) => {
            // 503 罕见（RECORD_DIR 未配置）且非轮询主路径，写审计便于排障；
            // 成功路径只读不写 audit.log（防轮询刷爆）。
            audit_record_api(
                shared,
                "record/status",
                None,
                503,
                false,
                Some("RECORD_DIR not set"),
            );
            return Response::text("recording disabled (RECORD_DIR not set)").with_status_code(503);
        }
    };
    let recordings = rec.status();
    debug!("internal api record/status recordings={}", recordings.len());
    Response::from_data(
        "application/json",
        serde_json::to_vec(&serde_json::json!({ "recordings": recordings })).unwrap(),
    )
}

/// GET /session/rooms（内部接口，#240）：房间列表 + 客户端数（按 shard 汇总）。
fn session_rooms(shared: &Shared) -> Response {
    let mut rooms: Vec<serde_json::Value> = Vec::new();
    let mut by_room: std::collections::BTreeMap<String, (usize, Vec<usize>)> =
        std::collections::BTreeMap::new();
    for s in shared.session_snapshot() {
        let e = by_room.entry(s.room).or_default();
        e.0 += 1;
        if !e.1.contains(&s.shard) {
            e.1.push(s.shard);
        }
    }
    for (room, (clients, mut shards)) in by_room {
        // 分片集合来自 HashMap 快照，顺序不确定；排序保证响应确定性（CI 复现）。
        shards.sort_unstable();
        rooms.push(serde_json::json!({
            "room": room,
            "clients": clients,
            "shards": shards,
        }));
    }
    Response::from_data(
        "application/json",
        serde_json::to_vec(&serde_json::json!({ "rooms": rooms })).unwrap(),
    )
}

/// GET /session/clients?room=xxx（内部接口，#240）：客户端明细。
/// 不传 room 返回全部客户端（运维/排障用）。
fn session_clients(shared: &Shared, room: Option<&str>) -> Response {
    let now = crate::util::unix_micros();
    let room = room.filter(|r| !r.is_empty());
    let mut clients: Vec<serde_json::Value> = shared
        .session_snapshot()
        .into_iter()
        .filter(|s| room.is_none_or(|r| s.room == r))
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "room": s.room,
                "role": s.role,
                "shard": s.shard,
                "joined_at": s.joined_at,
                "uptime_us": now.saturating_sub(s.joined_at),
            })
        })
        .collect();
    clients.sort_by(|a, b| a["id"].as_u64().cmp(&b["id"].as_u64()));
    Response::from_data(
        "application/json",
        serde_json::to_vec(&serde_json::json!({ "clients": clients })).unwrap(),
    )
}

/// POST /session/kick?room=xxx&client=xxx（内部接口，#240）：踢人断连。
/// 会话存在 → 向所在分片发 Kick（下一轮清理回收），幂等可重试；不存在 → 404。
fn session_kick(
    shared: &Shared,
    shard_txs: &[mpsc::Sender<ShardCommand>],
    room: Option<&str>,
    client: Option<&str>,
) -> Response {
    let Some(room) = room.filter(|r| !r.is_empty()) else {
        audit_session_api(
            shared,
            "session/kick",
            None,
            400,
            false,
            Some("room required"),
        );
        return Response::text("room required").with_status_code(400);
    };
    let Some(client) = client.filter(|c| !c.is_empty()) else {
        // #249：省略 client = 踢掉整个房间（桥死亡恢复/运维排障用）。
        return session_kick_room(shared, shard_txs, room);
    };
    let Ok(client_id) = client.parse::<u64>() else {
        audit_session_api(
            shared,
            "session/kick",
            Some(room),
            400,
            false,
            Some("client must be u64"),
        );
        return Response::text("client must be a numeric id").with_status_code(400);
    };
    let Some(info) = shared.session(client_id) else {
        audit_session_api(
            shared,
            "session/kick",
            Some(room),
            404,
            false,
            Some("client not found"),
        );
        return Response::from_data(
            "application/json",
            serde_json::to_vec(&serde_json::json!({
                "kicked": false,
                "error": "client not found",
            }))
            .unwrap(),
        )
        .with_status_code(404);
    };
    if info.room != room {
        audit_session_api(
            shared,
            "session/kick",
            Some(room),
            404,
            false,
            Some("client not in room"),
        );
        return Response::from_data(
            "application/json",
            serde_json::to_vec(&serde_json::json!({
                "kicked": false,
                "error": "client not in room",
            }))
            .unwrap(),
        )
        .with_status_code(404);
    }
    let Some(shard_tx) = shard_txs.get(info.shard) else {
        audit_session_api(
            shared,
            "session/kick",
            Some(room),
            500,
            false,
            Some("shard unavailable"),
        );
        return Response::text("shard unavailable").with_status_code(500);
    };
    if shard_tx.send(ShardCommand::Kick { client_id }).is_err() {
        audit_session_api(
            shared,
            "session/kick",
            Some(room),
            500,
            false,
            Some("shard channel closed"),
        );
        return Response::text("shard unavailable").with_status_code(500);
    }
    audit_session_api(shared, "session/kick", Some(room), 200, true, None);
    Response::from_data(
        "application/json",
        serde_json::to_vec(&serde_json::json!({
            "kicked": true,
            "room": room,
            "client": client_id,
        }))
        .unwrap(),
    )
}

/// POST /session/kick?room=xxx（内部接口，#249）：踢掉房间全部客户端。
/// 幂等：房间无客户端返回 200 且 kicked=0。返回 JSON { room, kicked }。
fn session_kick_room(
    shared: &Shared,
    shard_txs: &[mpsc::Sender<ShardCommand>],
    room: &str,
) -> Response {
    let sessions = shared.session_snapshot();
    let mut kicked = 0u64;
    let mut failed_sends = 0u64;
    for info in sessions.iter().filter(|s| s.room == *room) {
        if let Some(tx) = shard_txs.get(info.shard)
            && tx.send(ShardCommand::Kick { client_id: info.id }).is_ok()
        {
            kicked += 1;
        } else {
            failed_sends += 1;
        }
    }
    // Kick 是异步命令：kicked 表示成功投递数，不代表已确认断开。
    let detail = if failed_sends > 0 {
        Some(format!("{failed_sends} shard sends failed"))
    } else {
        None
    };
    audit_session_api(
        shared,
        "session/kick",
        Some(room),
        200,
        true,
        detail.as_deref(),
    );
    Response::from_data(
        "application/json",
        serde_json::to_vec(&serde_json::json!({ "room": room, "kicked": kicked })).unwrap(),
    )
}

fn web_request(
    request: &Request,
    udp_addr: SocketAddr,
    tcp_addr: SocketAddr,
    shard_txs: Vec<mpsc::Sender<ShardCommand>>,
    router: Arc<Mutex<router::ShardRouter>>,
    turn: Option<TurnConfig>,
    turn_server: Option<Arc<turn_server::TurnServer>>,
    shared: Shared,
    internal: bool,
) -> Response {
    // 按需录制 API（#160）：仅内部接口（INTERNAL_TOKEN 保护）暴露。
    if internal {
        let record = match (request.method(), request.url().as_str()) {
            ("POST", "/record/start") => Some(record_start(
                &shared,
                query_param(request.raw_query_string(), "room"),
            )),
            ("POST", "/record/stop") => Some(record_stop(
                &shared,
                query_param(request.raw_query_string(), "room"),
            )),
            ("GET", "/record/status") => Some(record_status(&shared)),
            // 会话管理 API（#240）：仅内部接口暴露。
            ("GET", "/session/rooms") => Some(session_rooms(&shared)),
            ("GET", "/session/clients") => Some(session_clients(
                &shared,
                query_param(request.raw_query_string(), "room"),
            )),
            ("POST", "/session/kick") => Some(session_kick(
                &shared,
                &shard_txs,
                query_param(request.raw_query_string(), "room"),
                query_param(request.raw_query_string(), "client"),
            )),
            _ => None,
        };
        if let Some(resp) = record {
            return resp;
        }
        // #240：内部接口收到未知/错误方法的 record·session 路径直接 404，
        // 不落到公共 web/start 处理（避免 GET /session/kick 返回 web 页）。
        let url = request.url();
        if url.starts_with("/record/") || url.starts_with("/session/") {
            return Response::text("not found").with_status_code(404);
        }
    }
    if request.method() == "GET" && request.url() == "/metrics" {
        let metrics = shared.metrics.clone();
        let loads: Vec<f64> = {
            let r = router
                .lock()
                .unwrap_or_else(aerodesk_protocol::util::lock_recover);
            (0..metrics.len()).map(|i| r.load(i)).collect()
        };
        let shards: Vec<serde_json::Value> = metrics
            .iter()
            .enumerate()
            .map(|(i, m)| {
                serde_json::json!({
                    "shard": i,
                    "shard_load": loads.get(i).copied().unwrap_or(0.0),
                    "clients": m.clients.load(std::sync::atomic::Ordering::Relaxed),
                    "rx_packets": m.rx_packets.load(std::sync::atomic::Ordering::Relaxed),
                    "rx_bytes": m.rx_bytes.load(std::sync::atomic::Ordering::Relaxed),
                    "tx_packets": m.tx_packets.load(std::sync::atomic::Ordering::Relaxed),
                    "tx_bytes": m.tx_bytes.load(std::sync::atomic::Ordering::Relaxed),
                    // #238 媒体质量（最近 5s 心跳聚合）。
                    "rtt_us": m.rtt_avg_ns.load(std::sync::atomic::Ordering::Relaxed) / 1000,
                    "egress_loss": m.egress_loss_ppm.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e6,
                    "ingress_loss": m.ingress_loss_ppm.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e6,
                    "bwe_tx_bps": m.bwe_tx_bps.load(std::sync::atomic::Ordering::Relaxed),
                    "qos_clients": m.qos_clients.load(std::sync::atomic::Ordering::Relaxed),
                })
            })
            .collect();
        let total: serde_json::Value = serde_json::json!({
            "shards": shards,
            "turn_allocations": turn_server.as_ref().map(|s| s.active_allocations()),
            "turn_allocations_total": turn_server.as_ref().map(|s| s.allocations_total()),
            "turn_evictions_total": turn_server.as_ref().map(|s| s.evictions_total()),
            "recordings_active": shared.recorder.as_ref().map(|r| r.active_count()).unwrap_or(0),
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
        let loads: Vec<f64> = {
            let r = router
                .lock()
                .unwrap_or_else(aerodesk_protocol::util::lock_recover);
            (0..shared.metrics.len()).map(|i| r.load(i)).collect()
        };
        let body = prometheus_body(
            &shared,
            DRAINING.load(Ordering::Relaxed),
            turn_server.as_deref(),
            &loads,
        );
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
    // #467：dc_ready=1 声明客户端会发 signal_ready（旧客户端不带 → 兼容路径）。
    let dc_ready = param("dc_ready").as_deref() == Some("1");

    let mut data = request.data().expect("body to be available");
    // 畸形 offer 直接 400——expect 会让 /start 线程 panic（远程可 DoS 打挂进程）。
    let offer: str0m::change::SdpOffer = match serde_json::from_reader(&mut data) {
        Ok(o) => o,
        Err(e) => {
            warn!("start: 非法 SDP offer（room={room}）：{e}");
            return Response::text("invalid SDP offer").with_status_code(400);
        }
    };
    let offer_sdp = offer.to_sdp_string();
    if role == Role::Viewer && shard::offer_sends_media(&offer_sdp) {
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
    // #216：通告地址为公网/非回环时，同机客户端（桥、本机 CLI）需要回环候选
    // 才能直连——公网地址 hairpin 回不到 loopback 绑定的 socket（桥 ICE 会 20s 超时）。
    // #513：只对「候选全回环」的 offer 附带——远端客户端拿到回环候选会把发送目的地
    // 漂移到它自己的回环（str0m 候选漂移），发布端媒体黑洞；逐连接按 offer 判定。
    if udp_addr.ip() != std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        && shard::offer_is_loopback_only(&offer_sdp)
    {
        let loopback_addr = std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            udp_addr.port(),
        );
        if let Ok(loopback) = Candidate::host(loopback_addr, "udp") {
            let _ = rtc.add_local_candidate(loopback);
        }
    }
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

    // #180 /start 准入配额（0=不限）：预留计数，AddClient 失败回滚。
    if let Err(reason) =
        shared.try_reserve(&room, shared.max_room_clients, shared.max_total_clients)
    {
        info!("reject /start room={room}: {reason}");
        return Response::text(reason).with_status_code(503);
    }

    // 房间 → 分片路由：同房间粘性优先（复用已分配 shard），否则哈希 locality + 负载级联。
    // 修复：publisher 先加入 shard 0 后负载升高，viewer 再加入时 choose 会跳到 shard 1，
    // 同房间被拆到不同 shard 导致媒体不互通。
    let shard = {
        let existing = shared.room_shards(&room);
        if let Some(&first) = existing.first() {
            first
        } else {
            router
                .lock()
                .unwrap_or_else(aerodesk_protocol::util::lock_recover)
                .choose(&room)
        }
    };
    info!("POST /start room={room} -> shard {shard} dc_ready={dc_ready}");
    let room_for_release = room.clone();
    let res = shard_txs[shard].send(ShardCommand::AddClient {
        rtc,
        room,
        role,
        dc_ready,
    });
    if res.is_err() {
        warn!("Failed to deliver client to shard {shard}");
        shared.release(&room_for_release);
    }

    let body = serde_json::to_vec(&answer).expect("answer to serialize");
    Response::from_data("application/json", body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shard::Shared;
    use std::io::Read;

    /// 串行化依赖全局 `DRAINING` 的端点测试（cargo test 默认并行线程）。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn parse_ip_valid_and_invalid() {
        assert_eq!(parse_ip("192.0.2.1"), Some("192.0.2.1".parse().unwrap()));
        assert_eq!(
            parse_ip("2001:db8::1"),
            Some("2001:db8::1".parse().unwrap())
        );
        assert_eq!(parse_ip("not-an-ip"), None);
        assert_eq!(parse_ip(""), None);
        assert_eq!(parse_ip("127.0.0.1:3478"), None);
    }

    #[test]
    fn parse_thread_stat_ticks_reads_utime_stime() {
        // /proc/<pid>/stat：字段 14=utime、15=stime；comm 可含空格/括号。
        let stat = "1234 (rd-shard-0) R 0 0 0 0 0 0 0 0 0 0 1000 500 0 0 0 0";
        assert_eq!(parse_thread_stat_ticks(stat), Some(1500));
        let busy = "1234 (rd-shard-0 [busy]) S 0 0 0 0 0 0 0 0 0 0 200 300 0 0";
        assert_eq!(parse_thread_stat_ticks(busy), Some(500));
        assert_eq!(parse_thread_stat_ticks("garbage"), None);
    }

    #[test]
    fn resolve_shard_count_env_override_and_fallback() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
        // 有效覆盖（1..=64）
        unsafe { std::env::set_var("SFU_SHARD_COUNT", "3") };
        assert_eq!(resolve_shard_count(), 3);
        unsafe { std::env::set_var("SFU_SHARD_COUNT", "64") };
        assert_eq!(resolve_shard_count(), 64);
        // 非法/越界回退到 CPU 默认（1..=8）
        for bad in ["0", "65", "abc", "-1", ""] {
            unsafe { std::env::set_var("SFU_SHARD_COUNT", bad) };
            let n = resolve_shard_count();
            assert!((1..=8).contains(&n), "bad shard count {bad:?} -> {n}");
        }
        unsafe { std::env::remove_var("SFU_SHARD_COUNT") };
    }

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
        // #238 媒体质量：0 分片 250us/0.5% egress/1% ingress/1.2Mbps，1 分片 0 样本。
        shared.metrics[0]
            .rtt_avg_ns
            .store(250_000, Ordering::Relaxed);
        shared.metrics[0]
            .egress_loss_ppm
            .store((0.5 * 1e6) as u64, Ordering::Relaxed);
        shared.metrics[0]
            .ingress_loss_ppm
            .store((1.0 * 1e6) as u64, Ordering::Relaxed);
        shared.metrics[0]
            .bwe_tx_bps
            .store(1_200_000, Ordering::Relaxed);
        shared.metrics[0].qos_clients.store(2, Ordering::Relaxed);

        let body = prometheus_body(&shared, true, None, &[0.25, 0.9]);
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
        // #238 质量指标（分片 + 合计）
        assert!(
            body.contains("aerodesk_sfu_rtt_us{shard=\"0\"} 250"),
            "{body}"
        );
        assert!(
            body.contains("aerodesk_sfu_egress_loss{shard=\"0\"} 0.500000"),
            "{body}"
        );
        assert!(
            body.contains("aerodesk_sfu_ingress_loss{shard=\"0\"} 1.000000"),
            "{body}"
        );
        assert!(
            body.contains("aerodesk_sfu_bwe_tx_bps{shard=\"0\"} 1200000"),
            "{body}"
        );
        assert!(
            body.contains("aerodesk_sfu_qos_clients{shard=\"0\"} 2"),
            "{body}"
        );
        assert!(
            body.contains("aerodesk_sfu_shard_load{shard=\"0\"} 0.2500"),
            "{body}"
        );
        assert!(
            body.contains("aerodesk_sfu_shard_load{shard=\"1\"} 0.9000"),
            "{body}"
        );
        assert!(body.contains("aerodesk_sfu_rtt_us 250"), "{body}");
        assert!(body.contains("aerodesk_sfu_egress_loss 0.214286"), "{body}");
        assert!(
            body.contains("aerodesk_sfu_ingress_loss 0.428571"),
            "{body}"
        );
        assert!(body.contains("aerodesk_sfu_bwe_tx_bps 3600000"), "{body}");
        assert!(body.contains("aerodesk_sfu_qos_clients 2"), "{body}");
        assert!(body.contains("aerodesk_sfu_draining 1"), "{body}");
        // #240 录制 gauge：无 RECORD_DIR → 0
        assert!(body.contains("aerodesk_sfu_recordings_active 0"), "{body}");

        let body = prometheus_body(&shared, false, None, &[0.25, 0.9]);
        assert!(body.contains("aerodesk_sfu_draining 0"), "{body}");
        assert!(body.contains("aerodesk_sfu_recordings_active 0"), "{body}");
    }

    #[test]
    fn healthz_endpoint_ok() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
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
            None,
            shared,
            false,
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
            None,
            shared,
            false,
        );
        assert_eq!(resp.status_code, 200);
        let (mut reader, _size) = resp.data.into_reader_and_size();
        let mut body = String::new();
        let ok = reader.read_to_string(&mut body).is_ok() && body.contains("aerodesk_sfu_clients");
        assert!(ok, "prometheus body expected, got: {body:.80}");
    }

    #[test]
    fn session_api_lists_rooms_and_clients() {
        let shared = Shared::new(2);
        shared.register_session(shard::SessionInfo {
            id: 10,
            room: "demo".into(),
            role: Role::Publisher,
            shard: 0,
            joined_at: 1_000_000,
        });
        shared.register_session(shard::SessionInfo {
            id: 11,
            room: "demo".into(),
            role: Role::Viewer,
            shard: 1,
            joined_at: 2_000_000,
        });
        shared.register_session(shard::SessionInfo {
            id: 12,
            room: "other".into(),
            role: Role::Viewer,
            shard: 1,
            joined_at: 3_000_000,
        });

        // 房间列表：demo=2、other=1
        let resp = session_rooms(&shared);
        assert_eq!(resp.status_code, 200);
        let (mut reader, _) = resp.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let rooms = v["rooms"].as_array().unwrap();
        assert_eq!(rooms.len(), 2);
        assert_eq!(rooms[0]["room"], "demo");
        assert_eq!(rooms[0]["clients"], 2);
        assert_eq!(
            rooms[0]["shards"].as_array().unwrap(),
            serde_json::json!([0, 1]).as_array().unwrap()
        );
        assert_eq!(rooms[1]["room"], "other");
        assert_eq!(rooms[1]["clients"], 1);

        // 客户端明细（按房间过滤）
        let resp = session_clients(&shared, Some("demo"));
        let (mut reader, _) = resp.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let clients = v["clients"].as_array().unwrap();
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0]["id"], 10);
        assert_eq!(clients[0]["role"], "publisher");
        assert_eq!(clients[0]["shard"], 0);
        assert!(clients[0]["uptime_us"].as_u64().is_some());

        // 不传 room → 全部
        let resp = session_clients(&shared, None);
        let (mut reader, _) = resp.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["clients"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn session_kick_validates_and_sends_command() {
        let shared = Shared::new(2);
        shared.register_session(shard::SessionInfo {
            id: 10,
            room: "demo".into(),
            role: Role::Publisher,
            shard: 1,
            joined_at: 1_000_000,
        });
        let (tx, rx) = mpsc::channel::<ShardCommand>();
        let shard_txs = vec![mpsc::channel::<ShardCommand>().0, tx];

        // 参数缺失（room 都没有）→ 400
        let resp = session_kick(&shared, &shard_txs, None, None);
        assert_eq!(resp.status_code, 400);

        // 省略 client → room 级踢人（#249）：demo 房间 1 个客户端被踢
        let resp = session_kick(&shared, &shard_txs, Some("demo"), None);
        assert_eq!(resp.status_code, 200);
        let (mut reader, _) = resp.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["kicked"], 1);
        match rx.try_recv() {
            Ok(ShardCommand::Kick { client_id }) => assert_eq!(client_id, 10),
            other => panic!("expected Kick command, got {other:?}"),
        }

        // room 级踢人幂等：无客户端 → 200 kicked=0
        let resp = session_kick(&shared, &shard_txs, Some("empty"), None);
        assert_eq!(resp.status_code, 200);
        let (mut reader, _) = resp.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["kicked"], 0);

        // 未知客户端 → 404
        let resp = session_kick(&shared, &shard_txs, Some("demo"), Some("99"));
        assert_eq!(resp.status_code, 404);

        // 房间不匹配 → 404
        let resp = session_kick(&shared, &shard_txs, Some("other"), Some("10"));
        assert_eq!(resp.status_code, 404);

        // 分片通道不可用（shard_txs 为空）→ 500
        let resp = session_kick(&shared, &[], Some("demo"), Some("10"));
        assert_eq!(resp.status_code, 500);

        // 正常踢人 → 200 + Kick 命令送达对应分片
        let resp = session_kick(&shared, &shard_txs, Some("demo"), Some("10"));
        assert_eq!(resp.status_code, 200);
        let (mut reader, _) = resp.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["kicked"], true);
        assert_eq!(v["client"], 10);
        match rx.try_recv() {
            Ok(ShardCommand::Kick { client_id }) => assert_eq!(client_id, 10),
            other => panic!("expected Kick command, got {other:?}"),
        }
    }

    #[test]
    fn session_kick_room_sends_all_clients() {
        let shared = Shared::new(2);
        shared.register_session(shard::SessionInfo {
            id: 10,
            room: "demo".into(),
            role: Role::Publisher,
            shard: 0,
            joined_at: 1_000_000,
        });
        shared.register_session(shard::SessionInfo {
            id: 11,
            room: "demo".into(),
            role: Role::Viewer,
            shard: 1,
            joined_at: 2_000_000,
        });
        shared.register_session(shard::SessionInfo {
            id: 12,
            room: "other".into(),
            role: Role::Viewer,
            shard: 1,
            joined_at: 3_000_000,
        });
        let (tx0, rx0) = mpsc::channel::<ShardCommand>();
        let (tx1, rx1) = mpsc::channel::<ShardCommand>();
        let shard_txs = vec![tx0, tx1];

        let resp = session_kick_room(&shared, &shard_txs, "demo");
        assert_eq!(resp.status_code, 200);
        let (mut reader, _) = resp.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["kicked"], 2, "只踢 demo 房间的 2 个客户端");
        assert_eq!(v["room"], "demo");

        let mut kicked_ids = Vec::new();
        while let Ok(ShardCommand::Kick { client_id }) = rx0.try_recv() {
            kicked_ids.push(client_id);
        }
        while let Ok(ShardCommand::Kick { client_id }) = rx1.try_recv() {
            kicked_ids.push(client_id);
        }
        kicked_ids.sort_unstable();
        assert_eq!(kicked_ids, vec![10, 11], "其它房间客户端不被误踢");
    }

    #[test]
    fn record_api_audits_failures_without_recorder() {
        // 无 RECORD_DIR（无 Recorder）：start 400/503 有审计（tracing 兜底）且状态码正确。
        let shared = Shared::new(1);
        let resp = record_start(&shared, None);
        assert_eq!(resp.status_code, 400);
        let resp = record_start(&shared, Some("demo"));
        assert_eq!(resp.status_code, 503);
        let resp = record_stop(&shared, None);
        assert_eq!(resp.status_code, 400);
        let resp = record_stop(&shared, Some("demo"));
        assert_eq!(resp.status_code, 503);
        let resp = record_status(&shared);
        assert_eq!(resp.status_code, 503);
    }

    #[test]
    fn internal_unknown_session_path_returns_404_not_web() {
        let shared = Shared::new(1);
        let router = Arc::new(Mutex::new(crate::router::ShardRouter::new(1)));
        // 认证通过但路径/方法不匹配（GET /session/kick）→ 404，不回落 web 页。
        let req = Request::fake_http(
            "GET",
            "/session/kick?room=demo&client=1",
            vec![],
            Vec::new(),
        );
        let resp = web_request(
            &req,
            "127.0.0.1:3478".parse().unwrap(),
            "127.0.0.1:3478".parse().unwrap(),
            Vec::new(),
            router,
            None,
            None,
            shared,
            true,
        );
        assert_eq!(resp.status_code, 404);
        let (mut reader, _) = resp.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        assert!(
            !body.contains("<html"),
            "must not fall through to web page: {body:.60}"
        );
    }

    #[test]
    fn start_rejected_while_draining() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
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
            None,
            shared,
            false,
        );
        DRAINING.store(false, Ordering::Relaxed);
        assert_eq!(resp.status_code, 503);
    }
}
