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
//!   SFU_URLS      SFU 池（逗号分隔，可选）：设置后按房间无状态哈希选路到其中一个 SFU；
//!                 未设置回退单值 SFU_URL（向后兼容）
//!   SFU_TOKEN     SFU 内部接口 token（可选）
//!   SFU_STICKY_TTL_SECS 房间→SFU 粘性映射空闲淘汰阈值（秒，默认 21600=6h；仅池>1）
//!
//! 多 PoP（#146）：
//! 连接配额（#163）：
//!   MAX_ROOM_CLIENTS  每房间人数上限（0=不限）；超限 Join 返回 Error("room full")
//!   MAX_TOTAL_CLIENTS 单实例全局连接上限（0=不限）；超限返回 Error("server full")
//!   SIGNAL_MAX_PREJOIN_CLIENTS 并发「未 Join」连接上限（默认 256，0=不限）：
//!                 认证/Join 前的连接同样占线程与 fd 且不受 MAX_TOTAL_CLIENTS
//!                 约束，超限直接断开（防预认证连接堆积）
//!   SIGNAL_ALLOWED_ORIGINS /ws Origin 白名单（逗号分隔；未设置不校验，`*` 放行
//!                 全部）。浏览器必带 Origin；CLI/native 无 Origin 头始终放行
//!   SIGNAL_PLAIN_PORT 明文 WS 端口（默认 3003）；设为 off/disabled/none 关闭
//!                 明文服务器（生产建议关闭或用防火墙限制）
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
//!   BRIDGE_MONITOR_INTERVAL_SECS 桥 monitor 轮询间隔（默认 15，下限 2；死亡检测/
//!                              空闲回收粒度，e2e 用 2s 快于客户端 8s watchdog）

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
    /// TURN REST secret（用于按 Join 现算临时凭证，避免 1h 过期）。
    turn_secret: Option<String>,
    turn: Option<TurnConfig>,
    /// SFU 池（按房间无状态哈希选路；长度 ≥1）。
    sfu_urls: Vec<String>,
    sfu_token: Option<String>,
    /// SFU 负载轮询间隔（秒；仅池 >1 时启用）。
    sfu_poll_interval_secs: u64,
    /// SFU 探测失败后的冷却期（秒；期间不参与新房间分配）。
    sfu_fail_cooldown_secs: u64,
    /// 房间粘性映射空闲淘汰阈值（秒；仅池 >1 时生效）。
    sfu_sticky_ttl_secs: u64,
    /// 并发「未 Join」连接上限（0=不限）：Join/认证之前连接同样占线程与 fd，
    /// 防止绕过 max_total_clients 的连接堆积（#163 只在 Join 后计数）。
    max_prejoin_clients: usize,
    /// /ws Origin 白名单（None=不校验，兼容现状；`*` 放行全部）。
    /// 非浏览器客户端（CLI/native）无 Origin 头，始终放行。
    allowed_origins: Option<Vec<String>>,
    /// 跨 PoP 桥接编排（#216 M3）：BRIDGE_CMD 设置时启用；桥失败回退 Redirect。
    bridge: Option<Arc<BridgeManager>>,
    /// 桥空闲回收阈值（#246）：房间内无真实客户端超过该时长 → 停止桥。
    bridge_idle_secs: Duration,
    /// 桥生命周期 monitor 轮询间隔（#249 review：可配小值让死亡检测快于客户端
    /// 8s no-media watchdog，e2e 用 2s；默认 15）。
    bridge_monitor_interval: Duration,
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

/// SFU 池：无状态哈希回退 + 负载感知选路状态（#354 第二步）。
struct SfuPool {
    urls: Vec<String>,
    /// 各 SFU 的负载评分（max shard_load ×10000，0..=10000）。
    loads: Vec<AtomicU64>,
    /// 各 SFU 的不可用截止时间戳（Unix 秒；0=可用）。
    down_until: Vec<AtomicU64>,
    /// 房间 → (SFU 下标, 最近使用 Unix 秒)（粘性：新房间分配一次后固定，
    /// SFU 剔除时重选；空闲超过 TTL 由 poller 淘汰，防长期运行无界增长）。
    room_sfu: Mutex<HashMap<String, (usize, u64)>>,
}

static SFU_POOL: OnceLock<Arc<SfuPool>> = OnceLock::new();

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl SfuPool {
    fn new(urls: Vec<String>) -> Self {
        let n = urls.len().max(1);
        Self {
            urls,
            loads: (0..n).map(|_| AtomicU64::new(0)).collect(),
            down_until: (0..n).map(|_| AtomicU64::new(0)).collect(),
            room_sfu: Mutex::new(HashMap::new()),
        }
    }

    fn is_up(&self, i: usize, now: u64) -> bool {
        self.down_until[i].load(Ordering::Relaxed) <= now
    }

    /// 选房间所在 SFU：已分配房间**永不重映射**（粘性），避免瞬态探测失败把活跃
    /// 房间切成两半；新房间选最闲健康 SFU（负载相同时按 rendezvous 权重分摊），
    /// 全部下线回退哈希。锁贯穿「查/选/写」消除同房间并发首连的竞态。
    fn select(&self, room: &str) -> usize {
        let mut reg = self.room_sfu.lock().unwrap_or_else(|e| e.into_inner());
        let now = unix_secs();
        if let Some((i, last_used)) = reg.get_mut(room) {
            *last_used = now;
            return *i;
        }
        let chosen = (0..self.urls.len())
            .filter(|&i| self.is_up(i, now))
            .min_by(|&a, &b| {
                self.loads[a]
                    .load(Ordering::Relaxed)
                    .cmp(&self.loads[b].load(Ordering::Relaxed))
                    .then_with(|| {
                        rendezvous_weight(room, a)
                            .partial_cmp(&rendezvous_weight(room, b))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            })
            .unwrap_or_else(|| sfu_for_room(&self.urls, room));
        reg.insert(room.to_string(), (chosen, now));
        chosen
    }

    /// 淘汰「无活跃 peer 且超过 `ttl_secs` 未使用」的粘性映射（poller 周期调用），
    /// 返回淘汰数。`alive` 为 rooms 表中有 peer 的房间集合：这些房间的映射一律
    /// 保留——活跃房间**永不重映射**（粘性保证）；仅当房间已空（peer 清零、
    /// session_loop 已将其移出 rooms 表）且映射空闲超过 TTL 才淘汰，防无界增长。
    /// 注意 select 只在 Description/kick 路径被调用，Join 不刷新时间戳，因此
    /// 不能用「最近 select 时间」判断房间死活，必须看 rooms 表。
    fn evict_stale(
        &self,
        alive: &std::collections::HashSet<String>,
        ttl_secs: u64,
        now: u64,
    ) -> usize {
        let mut reg = self.room_sfu.lock().unwrap_or_else(|e| e.into_inner());
        let before = reg.len();
        reg.retain(|room, (_, t)| alive.contains(room) || now.saturating_sub(*t) < ttl_secs);
        before - reg.len()
    }
}

/// 从 /metrics/prometheus 解析各分片 shard_load 的最大值（×10000）。
fn parse_max_shard_load(body: &str) -> u64 {
    let mut max = 0.0f64;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("aerodesk_sfu_shard_load{") {
            if let Some(v) = rest.rsplit(' ').next().and_then(|s| s.parse::<f64>().ok()) {
                max = max.max(v);
            }
        }
    }
    ((max * 10_000.0).round() as u64).min(10_000)
}

/// 单个 SFU 负载探测的失败分类（区分配置错误与网络故障）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollErr {
    /// 401/403：`SFU_TOKEN` 与 SFU `INTERNAL_TOKEN` 不匹配（配置错误，非 SFU 故障）。
    Unauthorized,
    /// 其它非 2xx 状态码。
    Http(u16),
    /// 连接失败/超时。
    Unreachable,
    /// 响应体读取失败。
    Body,
}

/// 探测单个 SFU 的 `shard_load` 最大值（×10000）。
/// 必须携带 `X-Internal-Token`：SFU 内部端口在设置 `INTERNAL_TOKEN` 后对所有
/// 请求（含 /metrics/prometheus）鉴权，缺头会 403 导致整个池被误判下线。
fn poll_sfu_load(url: &str, token: Option<&str>) -> Result<u64, PollErr> {
    let mut req = ureq::get(url).timeout(Duration::from_secs(3));
    if let Some(token) = token {
        req = req.set("X-Internal-Token", token);
    }
    match req.call() {
        Ok(resp) => match resp.into_string() {
            Ok(body) => Ok(parse_max_shard_load(&body)),
            Err(_) => Err(PollErr::Body),
        },
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            Err(PollErr::Unauthorized)
        }
        Err(ureq::Error::Status(code, _)) => Err(PollErr::Http(code)),
        Err(_) => Err(PollErr::Unreachable),
    }
}

/// 后台轮询各 SFU 的负载；网络/HTTP 失败按冷却期标记下线，
/// 401/403 属配置错误（SFU 本身健康），保留上次负载不标记下线（error 日志
/// 按每 SFU 每 300s 节流，防刷屏）。
/// 同时淘汰「无活跃 peer 且空闲超时」的房间粘性映射（防 room_sfu 无界增长）。
fn poll_sfu_pool(
    pool: Arc<SfuPool>,
    rooms: Rooms,
    interval_secs: u64,
    cooldown_secs: u64,
    token: Option<String>,
    sticky_ttl_secs: u64,
) {
    let mut last_unauth_log = vec![0u64; pool.urls.len()];
    loop {
        // enumerate 而非 0..len：循环体并行索引 urls/loads/down_until/
        // last_unauth_log 四个集合，range 写法触发 needless_range_loop。
        for (i, _url) in pool.urls.iter().enumerate() {
            let url = format!("{}/metrics/prometheus", pool.urls[i]);
            match poll_sfu_load(&url, token.as_deref()) {
                Ok(load) => {
                    pool.loads[i].store(load, Ordering::Relaxed);
                    pool.down_until[i].store(0, Ordering::Relaxed);
                }
                Err(PollErr::Unauthorized) => {
                    let now = unix_secs();
                    if now.saturating_sub(last_unauth_log[i]) >= 300 {
                        error!(
                            "sfu pool: {url} metrics 401/403: SFU_TOKEN 与 SFU INTERNAL_TOKEN \
                             不匹配（配置错误）；保留上次负载，不标记下线"
                        );
                        last_unauth_log[i] = now;
                    } else {
                        debug!("sfu pool: {url} metrics 401/403 (log throttled)");
                    }
                }
                Err(PollErr::Http(code)) => {
                    warn!("sfu pool: {url} http {code}; mark down");
                    pool.down_until[i].store(unix_secs() + cooldown_secs, Ordering::Relaxed);
                }
                Err(PollErr::Unreachable) => {
                    warn!("sfu pool: {url} unreachable; mark down");
                    pool.down_until[i].store(unix_secs() + cooldown_secs, Ordering::Relaxed);
                }
                Err(PollErr::Body) => {
                    warn!("sfu pool: {url} read body failed; mark down");
                    pool.down_until[i].store(unix_secs() + cooldown_secs, Ordering::Relaxed);
                }
            }
        }
        let alive: std::collections::HashSet<String> = {
            let rooms = rooms.lock().unwrap_or_else(|e| e.into_inner());
            rooms.keys().cloned().collect()
        };
        let evicted = pool.evict_stale(&alive, sticky_ttl_secs, unix_secs());
        if evicted > 0 {
            debug!(
                "sfu pool: evicted {evicted} idle sticky room mappings (ttl {sticky_ttl_secs}s)"
            );
        }
        std::thread::sleep(Duration::from_secs(interval_secs));
    }
}

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

/// SIGNAL_PLAIN_PORT 解析：未设置 → 默认 3003；off/disabled/none（不区分大小写）
/// → None（完全关闭明文服务器）；非法值告警并回退 3003（配置笔误不静默改行为）。
fn parse_plain_port(raw: Option<&str>) -> Option<u16> {
    const DEFAULT: u16 = 3003;
    match raw {
        None => Some(DEFAULT),
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "off" | "disabled" | "none" => None,
            other => match other.parse::<u16>() {
                Ok(port) => Some(port),
                Err(_) => {
                    warn!("invalid SIGNAL_PLAIN_PORT={v:?}; fallback to {DEFAULT}");
                    Some(DEFAULT)
                }
            },
        },
    }
}

/// 预 Join（未认证/未加入）连接计数：MAX_TOTAL_CLIENTS 只在 Join 后计数，
/// 认证前的连接同样占用线程与 fd，单独设限防连接堆积 DoS。
static PREJOIN_CLIENTS: AtomicUsize = AtomicUsize::new(0);

/// 预占一个 pre-join 槽位：`None`=超上限拒绝；`Some(counted)`=放行，
/// `counted` 指示是否实际计数（cap=0 不限时释放须传回原值，防止多减）。
fn reserve_prejoin(counter: &AtomicUsize, cap: usize) -> Option<bool> {
    if cap == 0 {
        return Some(false);
    }
    if counter.fetch_add(1, Ordering::Relaxed) >= cap {
        counter.fetch_sub(1, Ordering::Relaxed);
        return None;
    }
    Some(true)
}

/// 释放 pre-join 槽位（仅在 `reserve_prejoin` 实际计数时递减）。
fn release_prejoin(counter: &AtomicUsize, counted: bool) {
    if counted {
        counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// SFU_STICKY_TTL_SECS 解析：0/非法 → 默认 6h（TTL=0 会让每轮轮询清空
/// 全部粘性映射，摧毁房间粘性）。
fn parse_sticky_ttl(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(6 * 3600)
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

/// FNV-1a 64（稳定、跨 Rust 版本一致）：选路哈希必须稳定，不能用
/// `DefaultHasher`（算法未指定，跨版本可能变，导致滚动发布重分片切分房间）。
fn fnv1a64(room: &str, i: usize) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in room.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    for b in i.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 房间在候选 i 上的 rendezvous 权重（0..1，稳定、确定性）。
fn rendezvous_weight(room: &str, i: usize) -> f64 {
    fnv1a64(room, i) as f64 / (u64::MAX as f64)
}

/// 无状态选路：按房间名 rendezvous 哈希到 SFU 池中的某一个。
/// 同一房间 + 同一池顺序 → 恒同一下标（signal 重启/多实例一致，无需存映射）。
fn sfu_for_room(pool: &[String], room: &str) -> usize {
    debug_assert!(!pool.is_empty());
    let mut best = 0usize;
    let mut best_w = -1.0f64;
    for (i, _) in pool.iter().enumerate() {
        let w = rendezvous_weight(room, i);
        if w > best_w {
            best_w = w;
            best = i;
        }
    }
    best
}

/// 按 Join 现算 TURN 临时凭证（REST：username=过期时间戳，1h 有效），
/// 避免启动时一次性生成导致 1 小时后新加入者拿到过期凭证。
fn fresh_turn(config: &Config) -> Option<TurnConfig> {
    let urls = config.turn.as_ref()?.urls.clone();
    let secret = config.turn_secret.as_deref()?;
    let creds =
        aerodesk_protocol::turn::generate_turn_credentials(secret, "aerodesk", 3600, unix_secs());
    Some(TurnConfig {
        urls,
        username: creds.username,
        credential: creds.credential,
    })
}

/// 选房间所在 SFU 下标：负载感知（粘性 + 最闲健康）优先；未初始化回退哈希。
fn selected_sfu_idx(pool: &[String], room: &str) -> usize {
    SFU_POOL
        .get()
        .map(|p| p.select(room))
        .unwrap_or_else(|| sfu_for_room(pool, room))
}

/// 调 SFU 内部接口踢掉房间全部客户端（#249：桥死亡后触发 viewer --reconnect 恢复）。
/// 返回是否成功（2xx）；失败由调用方决定重试（#249 review）。
/// 注意：room 仅来自经过 sanitize_room 校验的房间名（[A-Za-z0-9._-]），
/// 直接拼进 query 无注入风险——若将来放开调用方需先 URL 编码。
fn kick_sfu_room(sfu_url: &str, token: Option<&str>, room: &str) -> bool {
    let url = format!("{sfu_url}/session/kick?room={room}");
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .build();
    let mut req = agent.post(&url);
    if let Some(token) = token {
        req = req.set("X-Internal-Token", token);
    }
    // ureq 2 对 >=400 直接返回 Err(Status)，故 Ok 即成功。
    match req.call() {
        Ok(resp) => {
            info!("bridge monitor: SFU kick room {room} -> {}", resp.status());
            true
        }
        Err(e) => {
            warn!("bridge monitor: SFU kick room {room} failed: {e}");
            false
        }
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
    // SFU 池：仅池 >1 时初始化负载感知状态并启动轮询；池=1 走纯哈希回退，
    // 不维护 room_sfu 注册表（避免单 SFU 部署下无界增长）。
    if config.sfu_urls.len() > 1 {
        let pool = Arc::new(SfuPool::new(config.sfu_urls.clone()));
        let _ = SFU_POOL.set(pool.clone());
        let interval = config.sfu_poll_interval_secs.max(1);
        let cooldown = config.sfu_fail_cooldown_secs;
        let token = config.sfu_token.clone();
        let sticky_ttl = config.sfu_sticky_ttl_secs;
        let poll_rooms = rooms.clone();
        std::thread::Builder::new()
            .name("sfu-poller".into())
            .spawn(move || poll_sfu_pool(pool, poll_rooms, interval, cooldown, token, sticky_ttl))
            .ok();
    }

    // #246 桥生命周期 monitor：房间无真实客户端超过 BRIDGE_IDLE_SECS → 停桥。
    // 空闲判定用纯函数 idle_rooms_to_stop（按 spawn 代数防旧时间戳误杀新桥）；
    // 停桥前持 rooms 锁二次确认并执行 stop，避免与并发 Join 的 TOCTOU。
    if let Some(bridge) = &config.bridge {
        let bridge = bridge.clone();
        let rooms = rooms.clone();
        let idle = config.bridge_idle_secs;
        let monitor_interval = config.bridge_monitor_interval;
        let sfu_urls = config.sfu_urls.clone();
        let sfu_token = config.sfu_token.clone();
        std::thread::Builder::new()
            .name("bridge-monitor".into())
            .spawn(move || {
                let mut idle_since: HashMap<String, Instant> = HashMap::new();
                let mut last_epoch: HashMap<String, u64> = HashMap::new();
                let mut alive_prev: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut monitor_stopped: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                // 待重试踢人（房间 → 已尝试次数，#249 review：kick 失败有限重试）。
                let mut kick_retry: HashMap<String, u32> = HashMap::new();
                loop {
                    std::thread::sleep(monitor_interval);
                    let now = Instant::now();
                    let running = bridge.running_rooms();
                    let mut alive_now: std::collections::HashSet<String> =
                        running.iter().map(|(r, _)| r.clone()).collect();
                    let real_peers: HashMap<String, usize> = {
                        let rooms = rooms.lock().unwrap_or_else(|e| e.into_inner());
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
                        // 记录为 monitor 主动停止（死亡检测排除），再二次确认 + 停桥。
                        monitor_stopped.insert(room.clone());
                        let stop = {
                            let rooms = rooms.lock().unwrap_or_else(|e| e.into_inner());
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
                            // #249 review：成功停掉的桥要从本轮存活集合移除，
                            // 否则下一轮会被误判为「自然死亡」触发无谓 kick。
                            alive_now.remove(&room);
                        }
                    }
                    // #249 桥死亡检测：自然死亡（消失且非 monitor 停止）→ 踢本地
                    // SFU 房间，断开已连接 viewer 的 WebRTC，触发 --reconnect 恢复。
                    for room in bridge::died_rooms(&alive_prev, &alive_now, &monitor_stopped) {
                        if bridge.is_running(&room) {
                            debug!("bridge monitor: room {room} died but already rebuilt; skip kick");
                            kick_retry.remove(&room);
                            continue;
                        }
                        info!(
                            "bridge monitor: bridge died for room {room}; kicking SFU room to trigger client recovery"
                        );
                        let sfu = &sfu_urls[selected_sfu_idx(&sfu_urls, &room)];
                        if kick_sfu_room(sfu, sfu_token.as_deref(), &room) {
                            kick_retry.remove(&room);
                        } else {
                            // 首次失败计入 1：下一次 tick 再重试（同 tick 不立即重复踢）。
                            kick_retry.entry(room).or_insert(1);
                        }
                    }
                    // 踢人失败有限重试（最多 3 次；期间房间若被重建则放弃）。
                    for (room, attempts) in kick_retry.iter_mut() {
                        if bridge.is_running(room) {
                            *attempts = u32::MAX; // 已重建，无需再踢
                            continue;
                        }
                        if *attempts >= 3 {
                            error!(
                                "bridge monitor: kick room {room} failed after {} attempts; check SFU_URL/SFU_TOKEN",
                                *attempts
                            );
                            *attempts = u32::MAX; // 放弃重试
                            continue;
                        }
                        let sfu = &sfu_urls[selected_sfu_idx(&sfu_urls, room)];
                        if kick_sfu_room(sfu, sfu_token.as_deref(), room) {
                            *attempts = u32::MAX; // 成功，稍后清理
                        } else {
                            *attempts += 1;
                        }
                    }
                    kick_retry.retain(|_, a| *a != u32::MAX);
                    alive_prev = alive_now;
                    monitor_stopped.clear();
                }
            })
            .expect("spawn bridge monitor");
    }

    // 明文 WS（开发用；生产只开 WSS 端口，SIGNAL_PLAIN_PORT=off 可完全关闭）。
    if let Some(plain_port) = parse_plain_port(std::env::var("SIGNAL_PLAIN_PORT").ok().as_deref()) {
        let plain_config = config.clone();
        let plain_rooms = rooms.clone();
        let plain = rouille::Server::new(format!("0.0.0.0:{plain_port}"), move |request| {
            handle(request, plain_config.clone(), plain_rooms.clone())
        })
        .expect("start plain signaling server");
        std::thread::spawn(move || plain.run());
        info!("Signaling (WS plain) listening on :{plain_port}");
    } else {
        info!("Signaling (WS plain) disabled by SIGNAL_PLAIN_PORT=off");
    }

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

    let turn_secret = std::env::var("TURN_SECRET").ok().filter(|s| !s.is_empty());
    let turn = turn_secret.clone().map(|secret| {
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
        turn_secret,
        turn,
        // SFU 池：SFU_URLS（逗号分隔）优先；未设置回退单值 SFU_URL（向后兼容）。
        sfu_urls: {
            let list = std::env::var("SFU_URLS")
                .ok()
                .map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty());
            match list {
                Some(urls) => urls,
                None => vec![
                    std::env::var("SFU_URL").unwrap_or_else(|_| "http://127.0.0.1:3002".into()),
                ],
            }
        },
        sfu_token: std::env::var("SFU_TOKEN").ok(),
        sfu_poll_interval_secs: std::env::var("SFU_POLL_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5),
        sfu_fail_cooldown_secs: std::env::var("SFU_FAIL_COOLDOWN_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30),
        sfu_sticky_ttl_secs: parse_sticky_ttl(std::env::var("SFU_STICKY_TTL_SECS").ok().as_deref()),
        max_prejoin_clients: std::env::var("SIGNAL_MAX_PREJOIN_CLIENTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256),
        allowed_origins: std::env::var("SIGNAL_ALLOWED_ORIGINS")
            .ok()
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty()),
        bridge,
        bridge_idle_secs: std::env::var("BRIDGE_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(|v| v.max(2)) // 至少 ≥ monitor 轮询间隔下限
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(300)),
        bridge_monitor_interval: std::env::var("BRIDGE_MONITOR_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(|v| v.max(2))
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(15)),
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
        // Origin 白名单（可选）：浏览器 WebSocket 必带 Origin（CSWSH 防护）；
        // 非浏览器客户端（CLI/native）无 Origin 头，始终放行。
        if let Some(allowed) = &config.allowed_origins
            && let Some(origin) = request.header("Origin")
            && !allowed
                .iter()
                .any(|a| a == "*" || a.eq_ignore_ascii_case(origin))
        {
            warn!("ws rejected: origin {origin} not in allowlist");
            return Response::text("origin not allowed").with_status_code(403);
        }
        return match websocket::start(request, None::<&str>) {
            Ok((response, rx)) => {
                std::thread::spawn(move || session_loop(rx, config, rooms));
                response
            }
            Err(_) => Response::text("websocket upgrade required").with_status_code(400),
        };
    }
    // 可观测性（#180 后续）：信号服务器此前只有 /ws，无健康/指标端点。
    if request.method() == "GET" && request.url() == "/healthz" {
        let clients = total_clients();
        let room_count = rooms.lock().unwrap_or_else(|e| e.into_inner()).len();
        let payload = serde_json::json!({
            "status": "ok",
            "clients": clients,
            "rooms": room_count,
            "pop": config.pop_id,
        });
        return Response::from_data(
            "application/json",
            serde_json::to_vec(&payload).expect("serialize healthz"),
        );
    }
    if request.method() == "GET" && request.url() == "/metrics/prometheus" {
        let clients = total_clients();
        let room_count = rooms.lock().unwrap_or_else(|e| e.into_inner()).len();
        let bridges = config
            .bridge
            .as_ref()
            .map(|b| b.running_rooms().len())
            .unwrap_or(0);
        let body = format!(
            "# TYPE aerodesk_signal_clients gauge\naerodesk_signal_clients {clients}\n\
             # TYPE aerodesk_signal_rooms gauge\naerodesk_signal_rooms {room_count}\n\
             # TYPE aerodesk_signal_bridges gauge\naerodesk_signal_bridges {bridges}\n"
        );
        return Response::from_data(
            "text/plain; version=0.0.4; charset=utf-8",
            body.into_bytes(),
        );
    }
    if request.method() == "GET" {
        return Response::text("aerodesk-signal: connect to /ws");
    }
    Response::text("method not allowed").with_status_code(405)
}

/// 当前全局在线客户端数（TOTAL_CLIENTS 未初始化时为 0，测试/异常兜底）。
fn total_clients() -> usize {
    TOTAL_CLIENTS
        .get()
        .map(|c| c.load(Ordering::Relaxed))
        .unwrap_or(0)
}

fn session_loop(rx: std::sync::mpsc::Receiver<Websocket>, config: Arc<Config>, rooms: Rooms) {
    // 预 Join 连接上限（H2 缓解）：未认证/未加入的连接同样占线程与 fd，
    // 且不受 MAX_TOTAL_CLIENTS 约束（其只在 Join 后计数）；超限直接断开。
    let Some(mut prejoin_counted) = reserve_prejoin(&PREJOIN_CLIENTS, config.max_prejoin_clients)
    else {
        warn!(
            "reject connection: too many pre-join connections (SIGNAL_MAX_PREJOIN_CLIENTS={})",
            config.max_prejoin_clients
        );
        return;
    };
    // 兜底：rouille 升级线程理论上总会在 recv 前 build 出 Websocket；若异常
    // 未交付，必须释放 pre-join 槽位再退出（避免 panic 留下永久泄漏）。
    let Ok(ws_owned) = rx.recv() else {
        release_prejoin(&PREJOIN_CLIENTS, prejoin_counted);
        return;
    };
    let ws = Arc::new(Mutex::new(ws_owned));
    info!("session open");
    // 本连接加入时的 peer_id：用于校验 Description.from 属于本连接（防冒用）。
    let mut own_peer_id: Option<String> = None;

    loop {
        let msg = match ws.lock().unwrap_or_else(|e| e.into_inner()).next() {
            Some(Message::Text(t)) => t,
            Some(_) => continue,
            None => break,
        };

        // 消息体上限（信令消息远小于 1MiB）。注意：rouille 的 next() 在返回前
        // 已把整条消息缓冲进内存，此检查只能阻止后续解析/转发——因此超限直接
        // 断开连接（而非仅回错误），终止该连接上的重复分配。真正的分配前上限
        // 需更换 WS 层实现（如 tungstenite 的 max_message_size）。
        if msg.len() > 1 << 20 {
            send(
                ws.clone(),
                SignalMessage::Error {
                    message: "message too large".into(),
                },
            );
            break;
        }

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
                // 房间名校验（防 query 注入 + 注册表滥用）。
                if !bridge::sanitize_room(&room) {
                    send(
                        ws.clone(),
                        SignalMessage::Error {
                            message: "invalid room".into(),
                        },
                    );
                    continue;
                }
                // 单连接只允许 Join 一次（防重复 Join 泄漏 peer / 计数漂移）。
                if own_peer_id.is_some() {
                    send(
                        ws.clone(),
                        SignalMessage::Error {
                            message: "already joined".into(),
                        },
                    );
                    continue;
                }
                // 认证先行：任何 PopRegistry 变更之前先验权。
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
                    continue;
                }
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
                                // 配额先行（纯检查，spawn 之前拦截）：桥自身 publisher 腿豁免。
                                // 认证无需重查——Join 入口已先验（同输入的确定性校验）。
                                let bridge_leg_pre = config.bridge.as_ref().is_some_and(|b| {
                                    role == Role::Publisher && b.is_running(&room)
                                });
                                if !bridge_leg_pre {
                                    let room_len = rooms
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
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
                // 桥自身 publisher 腿判定（#246）：配额豁免 + Peer 标记 + 空闲回收共用。
                let bridge_leg = config
                    .bridge
                    .as_ref()
                    .is_some_and(|b| role == Role::Publisher && b.is_running(&room));
                // 全局配额原子预订：所有路径统一 fetch_add 占位，非桥腿超限回滚；
                // 后续（用户/房间）失败也回滚，消除 load-then-increment 的 TOCTOU。
                let total_reserved = {
                    let total = TOTAL_CLIENTS
                        .get()
                        .expect("total initialized")
                        .fetch_add(1, Ordering::Relaxed);
                    if !bridge_leg
                        && config.max_total_clients > 0
                        && total >= config.max_total_clients
                    {
                        TOTAL_CLIENTS
                            .get()
                            .expect("total initialized")
                            .fetch_sub(1, Ordering::Relaxed);
                        false
                    } else {
                        true
                    }
                };
                if !total_reserved {
                    info!("reject join room={room} role={role:?}: server full");
                    send(
                        ws.clone(),
                        SignalMessage::Error {
                            message: "server full".into(),
                        },
                    );
                    continue;
                }
                // #171 用户配额：JWT max_conns（0=不限）。
                let user = claims.as_ref().map(|c| c.sub.clone());
                let mut user_quota_inc = false;
                if let Some(sub) = &user {
                    let max_conns = claims
                        .as_ref()
                        .map(|c| c.max_conns.unwrap_or(0))
                        .unwrap_or(0);
                    // max_conns==0 时 user_quota_take 不占位，回滚时不能 release。
                    if max_conns > 0 {
                        let mut uc = USER_CONNS
                            .get()
                            .expect("user conns initialized")
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if let Err(reason) = user_quota_take(&mut uc, sub, max_conns) {
                            TOTAL_CLIENTS
                                .get()
                                .expect("total initialized")
                                .fetch_sub(1, Ordering::Relaxed);
                            info!("reject join user={sub}: {reason}");
                            send(
                                ws.clone(),
                                SignalMessage::Error {
                                    message: reason.to_string(),
                                },
                            );
                            continue;
                        }
                        user_quota_inc = true;
                    }
                }
                let peer_id = format!("{}-{}", room, fastrand_id());
                // 房间配额检查 + peers 快照 + push 在同一 rooms 锁内（原子）。
                let peers: Vec<PeerInfo> = {
                    let mut r = rooms.lock().unwrap_or_else(|e| e.into_inner());
                    let cur = r.get(&room).map(|peers| peers.len()).unwrap_or(0);
                    if !bridge_leg && config.max_room_clients > 0 && cur >= config.max_room_clients
                    {
                        TOTAL_CLIENTS
                            .get()
                            .expect("total initialized")
                            .fetch_sub(1, Ordering::Relaxed);
                        if user_quota_inc && let Some(sub) = &user {
                            let mut uc = USER_CONNS
                                .get()
                                .expect("user conns initialized")
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            user_quota_release(&mut uc, sub.as_str());
                        }
                        info!("reject join room={room} role={role:?}: room full");
                        send(
                            ws.clone(),
                            SignalMessage::Error {
                                message: "room full".into(),
                            },
                        );
                        continue;
                    }
                    let peers: Vec<PeerInfo> = r
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
                    r.entry(room.clone()).or_default().push(Peer {
                        id: peer_id.clone(),
                        role,
                        ws: ws.clone(),
                        user,
                        bridge_leg,
                    });
                    peers
                };
                info!("peer {peer_id} joined room {room} as {role:?}");
                own_peer_id = Some(peer_id.clone());
                // 已正式计入 TOTAL_CLIENTS，释放 pre-join 槽位。
                release_prejoin(&PREJOIN_CLIENTS, prejoin_counted);
                prejoin_counted = false;
                send(
                    ws.clone(),
                    SignalMessage::Joined {
                        peer_id,
                        peers,
                        turn: fresh_turn(&config),
                    },
                );
            }
            SignalMessage::Description {
                from, description, ..
            } => {
                // 防冒用：Description.from 必须是本连接 Join 时分配的 peer_id，
                // 否则 viewer 可用别人（尤其 publisher）的 peer_id 让信令以 publisher
                // 角色代发 SDP，绕过 SFU 的 viewer 禁发媒体检查。
                if own_peer_id.as_deref() != Some(from.as_str()) {
                    send(
                        ws.clone(),
                        SignalMessage::Error {
                            message: "description from mismatch".into(),
                        },
                    );
                    continue;
                }
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
                    .unwrap_or_else(|e| e.into_inner())
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

    // 未 Join 即断开：释放 pre-join 槽位。
    release_prejoin(&PREJOIN_CLIENTS, prejoin_counted);

    // 断开：移除并广播 PeerLeft
    let found = {
        let mut rooms = rooms.lock().unwrap_or_else(|e| e.into_inner());
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
            .unwrap_or_else(|e| e.into_inner());
        user_quota_release(&mut uc, sub);
    }
    info!("peer {peer_id} left room {room}");
    // #66：先快照同房间其它连接、释放 rooms 锁，再尽力广播 PeerLeft。
    // 不能在持有 rooms 锁时去锁其它连接的 ws（锁序反转 → 死锁）。
    let peers: Vec<Arc<Mutex<Websocket>>> = {
        let rooms = rooms.lock().unwrap_or_else(|e| e.into_inner());
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
    let rooms = rooms.lock().unwrap_or_else(|e| e.into_inner());
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
    let sfu = &config.sfu_urls[selected_sfu_idx(&config.sfu_urls, room)];
    let url = format!("{sfu}/start?room={room}&role={role_name}");
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
            turn_secret: None,
            turn: None,
            sfu_urls: vec!["http://127.0.0.1:3002".into()],
            sfu_token: None,
            sfu_poll_interval_secs: 5,
            sfu_fail_cooldown_secs: 30,
            sfu_sticky_ttl_secs: 6 * 3600,
            max_prejoin_clients: 0,
            allowed_origins: None,
            bridge: None,
            bridge_idle_secs: Duration::from_secs(300),
            bridge_monitor_interval: Duration::from_secs(15),
        }
    }

    #[test]
    fn sfu_for_room_is_stateless_and_deterministic() {
        let pool = vec![
            "http://sfu-1:3002".to_string(),
            "http://sfu-2:3002".to_string(),
            "http://sfu-3:3002".to_string(),
        ];
        // 同房间恒同一下标（无状态：signal 重启/多实例一致）。
        let a = sfu_for_room(&pool, "room-a");
        for _ in 0..100 {
            assert_eq!(sfu_for_room(&pool, "room-a"), a);
        }
        // 落在合法范围。
        for r in 0..100 {
            assert!(sfu_for_room(&pool, &format!("room-{r}")) < pool.len());
        }
        // 多房间应分布到多个 SFU（rendezvous 哈希不会全挤一个）。
        let hit: std::collections::HashSet<usize> = (0..100)
            .map(|r| sfu_for_room(&pool, &format!("room-{r}")))
            .collect();
        assert!(hit.len() > 1, "100 个房间应分布到多个 SFU：{hit:?}");
    }

    #[test]
    fn parse_max_shard_load_picks_hottest_shard() {
        let body = "aerodesk_sfu_shard_load{shard=\"0\"} 0.2500\naerodesk_sfu_shard_load{shard=\"1\"} 0.9000\naerodesk_sfu_clients 5\n";
        assert_eq!(parse_max_shard_load(body), 9000);
        assert_eq!(parse_max_shard_load("no metric here"), 0);
    }

    #[test]
    fn sfu_pool_select_is_sticky_load_aware_and_fails_over() {
        let pool = SfuPool::new(vec![
            "http://s1".to_string(),
            "http://s2".to_string(),
            "http://s3".to_string(),
        ]);
        pool.loads[0].store(9000, Ordering::Relaxed); // 高负载
        pool.loads[1].store(1000, Ordering::Relaxed); // 最闲
        pool.loads[2].store(5000, Ordering::Relaxed); // 中负载

        let a = pool.select("room-a");
        assert_eq!(a, 1, "新房间应选最闲 SFU");
        assert_eq!(pool.select("room-a"), 1, "同房间应粘性");

        // s2 下线：已分配房间**不重映射**（防瞬态失败切分活跃房间）。
        pool.down_until[1].store(unix_secs() + 60, Ordering::Relaxed);
        assert_eq!(
            pool.select("room-a"),
            1,
            "已分配房间即使 SFU 下线也不重映射"
        );

        // 但新房间会避开下线 SFU，选次闲的 s3。
        let b = pool.select("room-b");
        assert_eq!(b, 2, "新房间应避开下线 SFU 选次闲");
    }

    #[test]
    fn health_and_metrics_endpoints() {
        use std::io::Read;
        // TOTAL_CLIENTS 是进程级 OnceLock，本测试首个设置；其它测试不触碰。
        let _ = TOTAL_CLIENTS.set(Arc::new(AtomicUsize::new(3)));
        let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));
        rooms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("room-a".into(), Vec::new());
        rooms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("room-b".into(), Vec::new());
        let config = Arc::new(cfg("s", None));

        let h = handle(
            &Request::fake_http("GET", "/healthz", vec![], Vec::new()),
            config.clone(),
            rooms.clone(),
        );
        assert_eq!(h.status_code, 200);
        let (mut reader, _size) = h.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["clients"], 3);
        assert_eq!(v["rooms"], 2);
        assert_eq!(v["pop"], "pop-a");

        let m = handle(
            &Request::fake_http("GET", "/metrics/prometheus", vec![], Vec::new()),
            config,
            rooms,
        );
        let (mut reader, _size) = m.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        assert!(body.contains("aerodesk_signal_clients 3"), "{body}");
        assert!(body.contains("aerodesk_signal_rooms 2"), "{body}");
        assert!(body.contains("aerodesk_signal_bridges 0"), "{body}");
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

    /// 极简 HTTP 假服务器：接受一个连接，读取请求头，回固定响应。
    fn fake_http_server(
        status_line: &'static str,
        body: &'static str,
    ) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = String::new();
            let mut buf = [0u8; 4096];
            // GET 无 body，读到请求头结束即可。
            while !req.contains("\r\n\r\n") {
                let n = match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                req.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            let _ = tx.send(req);
            let resp = format!(
                "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        });
        (addr, rx)
    }

    /// 回归（H1）：metrics 探测必须携带 X-Internal-Token——SFU 内部端口设置
    /// INTERNAL_TOKEN 后对所有请求鉴权，缺头会 403 导致整个池被误判下线。
    #[test]
    fn poll_sfu_load_sends_token_and_parses() {
        let body = "aerodesk_sfu_shard_load{shard=\"0\"} 0.9000\n";
        let (addr, rx) = fake_http_server("HTTP/1.1 200 OK", body);
        let url = format!("http://{addr}/metrics/prometheus");
        assert_eq!(poll_sfu_load(&url, Some("tok123")), Ok(9000));
        let req = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("fake server got request");
        assert!(
            req.to_ascii_lowercase()
                .contains("x-internal-token: tok123"),
            "metrics 探测必须带内部 token：{req}"
        );
    }

    /// 403 必须归类为 Unauthorized（配置错误），而非网络故障。
    #[test]
    fn poll_sfu_load_classifies_unauthorized() {
        let (addr, _rx) = fake_http_server("HTTP/1.1 403 Forbidden", "");
        let url = format!("http://{addr}/metrics/prometheus");
        assert_eq!(poll_sfu_load(&url, None), Err(PollErr::Unauthorized));
    }

    #[test]
    fn reserve_prejoin_caps_and_releases() {
        let counter = AtomicUsize::new(0);
        assert_eq!(
            reserve_prejoin(&counter, 0),
            Some(false),
            "cap=0 不限且不计数"
        );
        assert_eq!(counter.load(Ordering::Relaxed), 0);
        assert_eq!(reserve_prejoin(&counter, 2), Some(true));
        assert_eq!(reserve_prejoin(&counter, 2), Some(true));
        assert_eq!(reserve_prejoin(&counter, 2), None, "超过上限拒绝");
        assert_eq!(counter.load(Ordering::Relaxed), 2, "被拒的不得占位");
        release_prejoin(&counter, true);
        assert_eq!(reserve_prejoin(&counter, 2), Some(true), "释放后可再进");
        release_prejoin(&counter, false);
        release_prejoin(&counter, true);
        release_prejoin(&counter, true);
        assert_eq!(counter.load(Ordering::Relaxed), 0, "未计数的释放不得多减");
    }

    #[test]
    fn parse_plain_port_variants() {
        assert_eq!(parse_plain_port(None), Some(3003));
        assert_eq!(parse_plain_port(Some("3005")), Some(3005));
        assert_eq!(parse_plain_port(Some("off")), None);
        assert_eq!(parse_plain_port(Some("DISABLED")), None);
        assert_eq!(parse_plain_port(Some("none")), None);
        assert_eq!(parse_plain_port(Some("bad")), Some(3003), "非法值回退默认");
    }

    #[test]
    fn origin_whitelist_blocks_and_allows() {
        let mut config = cfg("s", None);
        config.allowed_origins = Some(vec!["https://good.example".into()]);
        let config = Arc::new(config);
        let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));

        // 非白名单 Origin → 403。
        let req = Request::fake_http(
            "GET",
            "/ws",
            vec![("Origin".into(), "https://evil.example".into())],
            Vec::new(),
        );
        assert_eq!(handle(&req, config.clone(), rooms.clone()).status_code, 403);

        // 白名单 Origin / 无 Origin（CLI/native）→ 通过 Origin 检查，
        // 进入 websocket 升级（fake 请求缺升级头 → 400）。
        let req = Request::fake_http(
            "GET",
            "/ws",
            vec![("Origin".into(), "https://good.example".into())],
            Vec::new(),
        );
        assert_eq!(handle(&req, config.clone(), rooms.clone()).status_code, 400);
        let req = Request::fake_http("GET", "/ws", vec![], Vec::new());
        assert_eq!(handle(&req, config, rooms).status_code, 400);
    }

    /// 粘性映射淘汰（M3）：有活跃 peer 的房间**永不淘汰**（即使映射空闲超过
    /// TTL——审查反馈：Join 不调用 select，静默信令的活跃房间不能用时间戳判死）；
    /// 无 peer 且空闲超过 TTL 的房间才被淘汰。
    #[test]
    fn sticky_map_evicts_idle_rooms() {
        let pool = SfuPool::new(vec!["http://s1".into(), "http://s2".into()]);
        let _ = pool.select("live-quiet");
        let _ = pool.select("stale");
        {
            let mut reg = pool.room_sfu.lock().unwrap_or_else(|e| e.into_inner());
            reg.get_mut("live-quiet").unwrap().1 = unix_secs() - 7 * 3600;
            reg.get_mut("stale").unwrap().1 = unix_secs() - 7 * 3600;
        }
        let mut alive = std::collections::HashSet::new();
        alive.insert("live-quiet".to_string());
        let evicted = pool.evict_stale(&alive, 6 * 3600, unix_secs());
        assert_eq!(evicted, 1, "只淘汰无 peer 且空闲超过 TTL 的房间");
        let reg = pool.room_sfu.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            reg.contains_key("live-quiet"),
            "有活跃 peer 的房间即使映射空闲超时也不得淘汰"
        );
        assert!(!reg.contains_key("stale"));
    }

    /// SFU_STICKY_TTL_SECS 解析：0/非法 → 默认 6h（TTL=0 会让每轮轮询清空
    /// 全部粘性映射，摧毁房间粘性）。
    #[test]
    fn parse_sticky_ttl_defaults_and_rejects_zero() {
        assert_eq!(parse_sticky_ttl(None), 6 * 3600);
        assert_eq!(parse_sticky_ttl(Some("0")), 6 * 3600);
        assert_eq!(parse_sticky_ttl(Some("bad")), 6 * 3600);
        assert_eq!(parse_sticky_ttl(Some("3600")), 3600);
    }
}
