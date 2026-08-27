//! AeroDesk 信令服务（SIP 单栈，P3.1）。
//!
//! P3 起本服务为 **SIP 单栈**：REGISTER/INVITE 走 rsipstack 端点（SIP/TLS +
//! SIP/WSS + SIP/UDP 三传输，默认全开，`off` 显式关闭）；HTTP 仅保留运维面
//! （/healthz、/devices、/metrics/prometheus、/admin/temp-password）。
//! 协议见 docs/SIP_SIGNALING.md。
//!
//! 随 JSON 信令面（/ws + session_loop）在本提交一并退役（点名）：
//! - JSON 信令协议面：SignalMessage Join/Description/Call 家族（枚举本体保留于
//!   aerodesk-protocol::signal，客户端侧退役另分支）；
//! - 连接配额（#163/#171）：MAX_ROOM_CLIENTS / MAX_TOTAL_CLIENTS /
//!   SIGNAL_MAX_PREJOIN_CLIENTS 与用户配额；
//! - prejoin 计数；Origin 白名单（SIGNAL_ALLOWED_ORIGINS）；
//! - JWT 信令认证（JWT_SECRET / JWT_SECRET_OLD；protocol::jwt 模块同批删除，
//!   agent --issue-token 一并退役）；
//! - TURN 凭证下发（TURN_SECRET / TURN_URLS）；
//! - JSON 面 PoP 重定向（ROOM_POP_MAP / POP_URLS）——多 PoP 改由 SIP INVITE
//!   302+Contact（POP_SIP_URLS）承接；跨 PoP 桥编排（BRIDGE_CMD 族）随 #601
//!   桥双腿 SIP 化重建。
//!
//! 环境变量：
//!   SIGNAL_OPS_PORT  HTTPS 运维端口（默认 3001；兼容别名 SIGNAL_PORT）
//!   AUTH_TOKENS      静态 token（SIP Digest 回退口令 + /admin 鉴权回退）
//!   SIP_REALM        SIP 域（默认 aerodesk）
//!   SIP_DIGEST_USERS 设备固定密码表（逗号分隔 user=password）
//!   SIP_ADMIN_TOKEN  /admin/temp-password 管理端点鉴权 token（缺省回退首个
//!                    AUTH_TOKEN）
//!   SIP_TLS_PORT     SIP/TLS 端口（默认 5061；off/disabled/none 关闭）
//!   SIP_WSS_PORT     SIP/WSS 端口（默认 3061；off 关闭）
//!   SIP_UDP_PORT     SIP/UDP 端口（默认 5060；off 关闭）
//!   CERT_FILE/KEY_FILE  TLS 身份（未设置回退内嵌开发证书）
//!   SFU_URL / SFU_URLS / SFU_TOKEN / SFU_POLL_INTERVAL_SECS /
//!                    SFU_FAIL_COOLDOWN_SECS / SFU_STICKY_TTL_SECS（SFU 池）
//!   POP_ID / POP_REGISTRY_FILE / POP_REGISTRY_TTL_SECS（多 PoP 注册表）
//!   POP_SIP_URLS     PoP=host:port（逗号分隔；他 PoP 房间 INVITE 的 302
//!                    Contact 载体）
//!
//! SIGHUP：重读 TLS 证书——重建 ops HTTPS server 并重启 SIP 端点（证书轮换
//! 不丢注册；无需重启进程）。
//!
//! #503-4 无人值守密码：
//!   POST   /admin/temp-password {"device_id":"AD-XX","ttl_secs":300}  → 签发临时密码
//!   DELETE /admin/temp-password/<device>                              → 撤销临时密码

#[macro_use]
extern crate tracing;

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pop_registry::PopRegistry;
use rouille::{Request, Response};
use tokio_util::sync::CancellationToken;

mod pop_registry;
mod sip_server;

struct Config {
    auth_tokens: Vec<String>,
    /// 本 PoP 标识（默认 local）。
    pop_id: String,
    /// 动态注册表（POP_REGISTRY_FILE 开启时存在）：房间归属由首个 INVITE 登记
    /// （#154；P3.1 写入点自 JSON Join 迁 SIP INVITE 会议分支）。
    pop_registry: Option<Arc<PopRegistry>>,
    /// PoP → SIP 目标 host:port（POP_SIP_URLS；302 Contact 载体）。
    pop_sip_urls: Vec<(String, String)>,
    /// SFU 池（按房间无状态哈希选路；长度 ≥1）。
    sfu_urls: Vec<String>,
    sfu_token: Option<String>,
    /// SFU 负载轮询间隔（秒；仅池 >1 时启用）。
    sfu_poll_interval_secs: u64,
    /// SFU 探测失败后的冷却期（秒；期间不参与新房间分配）。
    sfu_fail_cooldown_secs: u64,
    /// 房间粘性映射空闲淘汰阈值（秒；仅池 >1 时生效）。
    sfu_sticky_ttl_secs: u64,
}

/// SIGHUP 触发：重读 TLS 证书并重建 ops HTTPS server。
static RELOAD_TLS: AtomicBool = AtomicBool::new(false);
/// SIGHUP 触发：重启 SIP 端点以换用新证书（与 RELOAD_TLS 同一信号双写）。
/// 消费点：主循环读它 cancel 当前端点；supervisor 在端点返回后 swap 判定
/// reload/真停机（Ok 返回 + 标志 false 才停机，防止 reload 被误判为停机）。
static RELOAD_SIP_TLS: AtomicBool = AtomicBool::new(false);
/// 当前 SIP 端点的 cancel token（SIGHUP reload 臂 cancel 之，supervisor 换代）。
static CURRENT_SIP_TOKEN: OnceLock<Mutex<CancellationToken>> = OnceLock::new();
/// SIP 端点是否启用（任一 SIP_*_PORT 非 off）：/healthz `sip` 字段与
/// /admin/temp-password 的 501 判定共用。
static SIP_ENDPOINT_ENABLED: AtomicBool = AtomicBool::new(false);

/// #503-4 临时密码注册表句柄：无条件初始化（main 启动即 set）；SIP INVITE
/// 授权读取（decide_invite 临时口令分支）与 /admin/temp-password 读写共用。
/// 「SIP 端点可用与否」不看它，看 SIP_ENDPOINT_ENABLED。
static TEMP_PASSWORDS: OnceLock<Arc<Mutex<sip_server::TempRegistry>>> = OnceLock::new();

/// 恒定时间字符串比较（防时序侧信道；长度差异直接短路——长度非机密，
/// 内容逐字节 XOR 折叠，任何差异都走满循环）。令牌比较必须大小写敏感。
fn constant_time_eq_str(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 管理端点鉴权 token（SIP_ADMIN_TOKEN，缺省回退首个 AUTH_TOKEN）；None = 未配置
/// （管理端点返回 503——功能不可用而非静默放行）。
fn admin_token(config: &Config) -> Option<String> {
    std::env::var("SIP_ADMIN_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| config.auth_tokens.first().cloned())
}

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
        let mut reg = self
            .room_sfu
            .lock()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
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

    /// 淘汰「空闲超过 `ttl_secs`」的粘性映射（poller 周期调用），返回淘汰数。
    /// P3.1：last_used 的唯一刷新点是 SIP INVITE 会议分支（sfu_candidates →
    /// select，裁8）——池的唯一消费路径；零 INVITE 超过 TTL 的房间视为死房间。
    fn evict_stale(&self, ttl_secs: u64, now: u64) -> usize {
        let mut reg = self
            .room_sfu
            .lock()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
        let before = reg.len();
        reg.retain(|_, (_, t)| now.saturating_sub(*t) < ttl_secs);
        before - reg.len()
    }
}

/// 从 SFU `/metrics` 结构化 JSON 解析各分片 shard_load 的最大值（×10000）。
///
/// #487 审查批次 3（#12）：不再解析 Prometheus 文本——SFU 的 `/metrics` JSON
/// 原生携带 `shards[].shard_load`（与 Prometheus 同源数据），负载均衡消费
/// 结构化契约而非文本格式。解析失败/字段缺失按 load=0 处理（与旧文本解析
/// 空响应同语义）。
fn parse_max_shard_load(body: &str) -> u64 {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return 0;
    };
    let mut max = 0.0f64;
    if let Some(shards) = v.get("shards").and_then(|s| s.as_array()) {
        for shard in shards {
            if let Some(load) = shard.get("shard_load").and_then(|l| l.as_f64()) {
                max = max.max(load);
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
/// 请求（含 /metrics）鉴权，缺头会 403 导致整个池被误判下线。
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
/// 同时淘汰空闲超时的房间粘性映射（防 room_sfu 无界增长）。
fn poll_sfu_pool(
    pool: Arc<SfuPool>,
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
            // #12：消费结构化 /metrics JSON（内部端口鉴权覆盖该端点）。
            let url = format!("{}/metrics", pool.urls[i]);
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
        let evicted = pool.evict_stale(sticky_ttl_secs, unix_secs());
        if evicted > 0 {
            debug!(
                "sfu pool: evicted {evicted} idle sticky room mappings (ttl {sticky_ttl_secs}s)"
            );
        }
        std::thread::sleep(Duration::from_secs(interval_secs));
    }
}

/// SFU_STICKY_TTL_SECS 解析：0/非法 → 默认 6h（TTL=0 会让每轮轮询清空
/// 全部粘性映射，摧毁房间粘性）。
fn parse_sticky_ttl(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(6 * 3600)
}

/// SIP_*_PORT 类端口解析（D2 默认翻转）：未设置 → 默认值（Some）；off/disabled/
/// none（不区分大小写）→ None（关闭该传输）；非法值告警并回退默认（配置笔误
/// 不静默改行为）。
fn parse_port_or_off(raw: Option<&str>, env_name: &str, default: u16) -> Option<u16> {
    match raw {
        None => Some(default),
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "off" | "disabled" | "none" => None,
            other => match other.parse::<u16>() {
                Ok(port) => Some(port),
                Err(_) => {
                    warn!("invalid {env_name}={v:?}; fallback to {default}");
                    Some(default)
                }
            },
        },
    }
}

/// ops HTTP 端口：SIGNAL_OPS_PORT 优先，兼容别名 SIGNAL_PORT，默认 3001。
fn signal_ops_port() -> u16 {
    std::env::var("SIGNAL_OPS_PORT")
        .or_else(|_| std::env::var("SIGNAL_PORT"))
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001)
}

#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    if sig == libc::SIGHUP {
        // 双写：ops HTTPS 重建（RELOAD_TLS）+ SIP 端点重启换证书（RELOAD_SIP_TLS）。
        RELOAD_TLS.store(true, Ordering::Relaxed);
        RELOAD_SIP_TLS.store(true, Ordering::Relaxed);
    }
}

#[cfg(unix)]
fn install_signal_handlers() {
    // Safety: 信号处理器只做原子写（async-signal-safe）。
    unsafe {
        libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
    }
}

/// ops HTTP 处理器类型（Box<dyn Fn>：闭包捕获 Arc<Config>，无 CONFIG 静态）。
type OpsHandler = Box<dyn Fn(&Request) -> Response + Send + Sync>;

/// 带重试的 ops HTTPS server 绑定：旧 listener 释放后端口可能短暂 EADDRINUSE
/// （macOS 实测），重试可自愈；失败返回最后一次错误。
fn bind_wss_with_retry(
    port: u16,
    cert: &[u8],
    key: &[u8],
    config: &Arc<Config>,
    attempts: usize,
) -> Result<rouille::Server<OpsHandler>, String> {
    let mut last_err = String::new();
    for i in 0..attempts {
        let config = config.clone();
        let handler: OpsHandler = Box::new(move |req| ops_router(req, &config));
        match rouille::Server::new_ssl(
            format!("0.0.0.0:{port}"),
            handler,
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

/// SIGHUP：重读 TLS 身份并重建 ops HTTPS server（旧连接不受影响；同证书 no-op）。
fn reload_tls(
    server: &mut Option<rouille::Server<OpsHandler>>,
    tls: &mut aerodesk_protocol::tls::TlsIdentity,
    config: &Arc<Config>,
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
            let port = signal_ops_port();
            info!("SIGHUP: reloading TLS identity from {}", new_tls.source);
            server.take();
            match bind_wss_with_retry(port, &new_tls.cert, &new_tls.key, config, 20) {
                Ok(srv) => {
                    *server = Some(srv);
                    *tls = new_tls;
                    info!("TLS reloaded (new connections use updated certificate)");
                }
                Err(e) => {
                    error!("SIGHUP: TLS reload bind failed: {e}; restoring previous identity");
                    match bind_wss_with_retry(port, &tls.cert, &tls.key, config, 20) {
                        Ok(srv) => *server = Some(srv),
                        Err(e2) => error!("restore failed: {e2}; ops HTTPS down until restart"),
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

/// 选房间所在 SFU 下标：负载感知（粘性 + 最闲健康）优先；未初始化回退哈希。
/// 注意：有池时 select 同时刷新该房间 last_used（粘性 TTL 语义，裁8）。
fn selected_sfu_idx(pool: &[String], room: &str) -> usize {
    SFU_POOL
        .get()
        .map(|p| p.select(room))
        .unwrap_or_else(|| sfu_for_room(pool, room))
}

/// 候选次序（D4 有界 failover）：首选在前，其余按「健康优先 + 负载升序」排列
/// （无池保持原序）。返回全部下标，调用方决定尝试几个。
fn order_candidates(first: usize, pool: Option<&SfuPool>, len: usize) -> Vec<usize> {
    let mut rest: Vec<usize> = (0..len).filter(|&i| i != first).collect();
    if let Some(p) = pool {
        let now = unix_secs();
        rest.sort_by(|&a, &b| {
            // 健康（未下线）优先，再按负载升序。
            p.is_up(b, now).cmp(&p.is_up(a, now)).then_with(|| {
                p.loads[a]
                    .load(Ordering::Relaxed)
                    .cmp(&p.loads[b].load(Ordering::Relaxed))
            })
        });
    }
    let mut out = vec![first];
    out.extend(rest);
    out
}

/// SFU 候选序列（SIP 会议桥用）：首选粘性/rendezvous 目标（有池时同时刷新
/// last_used），其余按健康+负载排序。
pub(crate) fn sfu_candidates(urls: &[String], room: &str) -> Vec<usize> {
    if urls.len() <= 1 {
        return (0..urls.len()).collect();
    }
    let first = selected_sfu_idx(urls, room);
    order_candidates(first, SFU_POOL.get().map(|p| p.as_ref()), urls.len())
}

fn main() {
    init_log();
    let config = Arc::new(load_config());
    let port = signal_ops_port();
    let tls = aerodesk_protocol::tls::TlsIdentity::load().unwrap_or_else(|e| {
        eprintln!("fatal: TLS identity load failed: {e}");
        std::process::exit(1);
    });
    info!("TLS identity source: {}", tls.source);

    // #503-4 临时密码注册表：无条件初始化（/admin 读写 + SIP INVITE 授权共用）；
    // 「SIP 端点是否可用」由 SIP_ENDPOINT_ENABLED 决定。
    let temp_passwords = Arc::new(Mutex::new(sip_server::TempRegistry::default()));
    let _ = TEMP_PASSWORDS.set(temp_passwords.clone());

    // P3.1 SIP 单栈（D2 默认翻转）：三传输默认全开，off/disabled/none 显式关闭。
    // #598 P2a：WSS/TLS accept 需进程级 rustls CryptoProvider——sip_client 的
    // ensure_rustls_provider 只在客户端 UA 线程调用，服务端进程（rouille 旧
    // rustls + rsipstack 0.23 并存）无人安装，首个 WSS 握手即 panic
    // （Could not automatically determine the process-level CryptoProvider）。
    aerodesk_protocol::sip_client::ensure_rustls_provider();
    let sip_tls = parse_port_or_off(
        std::env::var("SIP_TLS_PORT").ok().as_deref(),
        "SIP_TLS_PORT",
        5061,
    );
    let sip_wss = parse_port_or_off(
        std::env::var("SIP_WSS_PORT").ok().as_deref(),
        "SIP_WSS_PORT",
        3061,
    );
    let sip_udp = parse_port_or_off(
        std::env::var("SIP_UDP_PORT").ok().as_deref(),
        "SIP_UDP_PORT",
        5060,
    );
    let sip_enabled = sip_tls.is_some() || sip_wss.is_some() || sip_udp.is_some();
    SIP_ENDPOINT_ENABLED.store(sip_enabled, Ordering::Relaxed);
    if sip_enabled {
        let realm = std::env::var("SIP_REALM").unwrap_or_else(|_| "aerodesk".into());
        let mut passwords = HashMap::new();
        for kv in std::env::var("SIP_DIGEST_USERS")
            .unwrap_or_default()
            .split(',')
        {
            if let Some((u, t)) = kv.split_once('=').filter(|(u, _)| !u.is_empty()) {
                passwords.insert(u.to_string(), t.to_string());
            }
        }
        let bind = |port: Option<u16>| port.map(|p| std::net::SocketAddr::from(([0, 0, 0, 0], p)));
        // Registrar 提升到 SipConfig（Arc）：SIGHUP 证书轮换重启端点时不丢注册。
        let registrar = Arc::new(Mutex::new(sip_server::Registrar::default()));
        let sip_cfg = sip_server::SipConfig {
            realm: realm.clone(),
            tls_addr: bind(sip_tls),
            wss_addr: bind(sip_wss),
            udp_addr: bind(sip_udp),
            passwords: Arc::new(passwords),
            // §8：未显式配置的设备以首个静态 token 为 Digest 口令。
            token_password: config.auth_tokens.first().cloned(),
            // 开放注册（无任何鉴权源时，与原 JSON join 同姿态）。
            open_register: config.auth_tokens.is_empty()
                && std::env::var("SIP_DIGEST_USERS")
                    .map(|v| v.trim().is_empty())
                    .unwrap_or(true),
            temp_passwords,
            tls_identity: Some(aerodesk_protocol::tls::TlsIdentity {
                cert: tls.cert.clone(),
                key: tls.key.clone(),
                source: tls.source,
            }),
            // 会议桥：SIP INVITE 非设备 AoR → SFU /start（SIP 侧唯一 SFU 消费点）。
            sfu_urls: config.sfu_urls.clone(),
            sfu_token: config.sfu_token.clone(),
            registrar,
            pop_id: config.pop_id.clone(),
            pop_registry: config.pop_registry.clone(),
            pop_sip_urls: config.pop_sip_urls.clone(),
        };
        // D3 supervisor（SIGHUP 证书热重载，裁7 修正版）：run_sip_endpoint 返回后
        // **先**查 RELOAD_SIP_TLS——true 换证书续跑（加载失败留旧身份），false 才
        // 区分停机/异常：Ok 真停机 break，Err 退避 2s 重试。cancel token 被 reload
        // 与停机双用，不能把 Ok 一律当停机。
        let boot_token = CancellationToken::new();
        let _ = CURRENT_SIP_TOKEN.set(Mutex::new(boot_token.clone()));
        std::thread::Builder::new()
            .name("sip-endpoint".into())
            .spawn(move || {
                let mut cfg = sip_cfg;
                let mut cancel = boot_token;
                loop {
                    let result = sip_server::run_sip_endpoint(cfg.clone(), cancel.clone());
                    let reload = RELOAD_SIP_TLS.swap(false, Ordering::Relaxed);
                    // 换代 token：无论续跑原因（reload / 退避重试），旧 token 已消费。
                    cancel = CancellationToken::new();
                    if let Some(slot) = CURRENT_SIP_TOKEN.get() {
                        *slot
                            .lock()
                            .unwrap_or_else(aerodesk_protocol::util::lock_recover) = cancel.clone();
                    }
                    if reload {
                        match aerodesk_protocol::tls::TlsIdentity::load() {
                            Ok(new_tls) => {
                                cfg.tls_identity = Some(new_tls);
                                info!("SIGHUP: SIP 端点已用新证书重启（注册不丢）");
                            }
                            Err(e) => {
                                error!("SIGHUP: SIP TLS 证书加载失败（{e}），沿用旧证书重启端点");
                            }
                        }
                        continue;
                    }
                    match result {
                        Ok(()) => {
                            info!("SIP 端点已停止");
                            break;
                        }
                        Err(e) => {
                            error!(error=%e, "SIP 端点异常退出，2s 后重试");
                            std::thread::sleep(Duration::from_secs(2));
                        }
                    }
                }
            })
            .ok();
        info!(realm, "SIP 信令端点已启动（单栈：TLS/WSS/UDP 默认全开）");
    } else {
        info!("SIP 端点已按 SIP_*_PORT=off 全部关闭（仅 HTTP 运维面）");
    }

    // SFU 池：仅池 >1 时初始化负载感知状态并启动轮询；池=1 走纯哈希回退，
    // 不维护 room_sfu 注册表（避免单 SFU 部署下无界增长）。
    if config.sfu_urls.len() > 1 {
        let pool = Arc::new(SfuPool::new(config.sfu_urls.clone()));
        let _ = SFU_POOL.set(pool.clone());
        let interval = config.sfu_poll_interval_secs.max(1);
        let cooldown = config.sfu_fail_cooldown_secs;
        let token = config.sfu_token.clone();
        let sticky_ttl = config.sfu_sticky_ttl_secs;
        std::thread::Builder::new()
            .name("sfu-poller".into())
            .spawn(move || poll_sfu_pool(pool, interval, cooldown, token, sticky_ttl))
            .ok();
    }

    #[cfg(unix)]
    install_signal_handlers();

    let mut tls = tls;
    let mut server = Some(
        bind_wss_with_retry(port, &tls.cert, &tls.key, &config, 1)
            .expect("start signaling ops server"),
    );
    info!("Signaling ops (HTTPS) listening on :{port}");
    // 轮询 + SIGHUP 证书热重载（#143，复用 #128 模式）。
    loop {
        if let Some(srv) = &server {
            srv.poll_timeout(Duration::from_millis(10));
        }
        if RELOAD_TLS.swap(false, Ordering::Relaxed) {
            reload_tls(&mut server, &mut tls, &config);
        }
        // D3：SIGHUP 双写 RELOAD_SIP_TLS——这里只触发当前端点 cancel（幂等、
        // 不吞标志）：supervisor 在端点返回后 swap 判定 reload/停机。
        if RELOAD_SIP_TLS.load(Ordering::Relaxed)
            && let Some(token) = CURRENT_SIP_TOKEN
                .get()
                .and_then(|m| m.lock().ok())
                .map(|g| g.clone())
        {
            token.cancel();
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

    Config {
        auth_tokens,
        pop_id: std::env::var("POP_ID").unwrap_or_else(|_| "local".into()),
        pop_registry,
        // POP_SIP_URLS="pop-b=127.0.0.1:3008,..."（PoP=host:port，302 Contact 载体）。
        pop_sip_urls: std::env::var("POP_SIP_URLS")
            .unwrap_or_default()
            .split(',')
            .filter(|kv| kv.contains('='))
            .map(|kv| {
                let (pop, hp) = kv.split_once('=').expect("checked contains '='");
                (pop.trim().to_string(), hp.trim().to_string())
            })
            .collect(),
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
    }
}

/// 运维/管理 HTTP 面（P3.1 起唯一 HTTP 路由）：/healthz、/devices、
/// /metrics/prometheus、/admin/temp-password 与兜底响应。
fn ops_router(request: &Request, config: &Config) -> Response {
    if request.method() == "GET" && request.url() == "/healthz" {
        let payload = serde_json::json!({
            "status": "ok",
            "pop": config.pop_id,
            "sip": sip_health_value(),
        });
        return Response::from_data(
            "application/json",
            serde_json::to_vec(&payload).expect("serialize healthz"),
        );
    }
    // #503 在线设备列表（无人值守入口管理数据源）：SIP Registrar 注册绑定。
    // P3.1：WSS presence 段随 JSON 栈退役，`via` 只剩 "sip"。
    if request.method() == "GET" && request.url() == "/devices" {
        let mut online: std::collections::BTreeMap<String, Vec<&'static str>> =
            std::collections::BTreeMap::new();
        if let Some(aors) = sip_server::registrar_snapshot() {
            for aor in aors {
                online.entry(aor).or_default().push("sip");
            }
        }
        let devices: Vec<serde_json::Value> = online
            .into_iter()
            .map(|(id, via)| {
                serde_json::json!({
                    "id": id,
                    "via": via,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "devices": devices,
            "pop": config.pop_id,
        });
        return Response::from_data(
            "application/json",
            serde_json::to_vec(&payload).expect("serialize devices"),
        );
    }
    if request.method() == "GET" && request.url() == "/metrics/prometheus" {
        // P3.1：JSON 面三 gauge（clients/rooms/bridges）随栈退役；仅剩 sip_*。
        // SIP 端点未启动时 body 为空（保持向后兼容的省略语义）。
        let body = match sip_server::metrics_snapshot() {
            Some((regs, est, term)) => format!(
                "# TYPE sip_registrations gauge\nsip_registrations {regs}\n\
                 # TYPE sip_calls_established counter\nsip_calls_established {est}\n\
                 # TYPE sip_calls_terminated counter\nsip_calls_terminated {term}\n"
            ),
            None => String::new(),
        };
        return Response::from_data(
            "text/plain; version=0.0.4; charset=utf-8",
            body.into_bytes(),
        );
    }
    // #503-4 临时密码管理端点（主控端发起、带有效期；SIP 端点开启时可用）。
    if request.method() == "POST" && request.url() == "/admin/temp-password" {
        return temp_password_issue(request, config);
    }
    if request.method() == "DELETE" && request.url().starts_with("/admin/temp-password/") {
        return temp_password_revoke(request, config);
    }
    if request.method() == "GET" {
        return Response::text("aerodesk-signal: SIP signaling server (ops HTTP plane)");
    }
    Response::text("method not allowed").with_status_code(405)
}

/// /healthz 的 `sip` 字段：端点开启 → 三传输监听状态；关闭 → null。
fn sip_health_value() -> serde_json::Value {
    sip_health_json(
        SIP_ENDPOINT_ENABLED.load(Ordering::Relaxed),
        sip_server::listeners_up(),
    )
}

/// `sip` 字段纯构造（可单测，避免测试进程触碰全局端点状态）。
fn sip_health_json(enabled: bool, up: Option<(bool, bool, bool)>) -> serde_json::Value {
    if !enabled {
        return serde_json::Value::Null;
    }
    let (tls, wss, udp) = up.unwrap_or((false, false, false));
    serde_json::json!({ "tls": tls, "udp": udp, "wss": wss })
}

/// 管理端点统一鉴权：`Authorization: Bearer <token>`（SIP_ADMIN_TOKEN / 首个 AUTH_TOKEN）。
/// 未配置管理 token → 503（功能不可用，不静默放行）；无 SIP 端点 → 501。
fn admin_guard(
    request: &Request,
    config: &Config,
) -> Result<Arc<Mutex<sip_server::TempRegistry>>, Response> {
    let Some(token) = admin_token(config) else {
        return Err(
            Response::text("admin token 未配置（SIP_ADMIN_TOKEN 或 AUTH_TOKENS）")
                .with_status_code(503),
        );
    };
    let auth = request.header("Authorization").unwrap_or_default();
    // 恒定时间 + 大小写敏感（此前 eq_ignore_ascii_case：既缩小令牌有效字母表
    // 又非恒定时间，且与 join/SIP 口令同源的 token 更需严谨比较）。
    if !constant_time_eq_str(auth, &format!("Bearer {token}")) {
        return Err(Response::text("unauthorized").with_status_code(401));
    }
    if !SIP_ENDPOINT_ENABLED.load(Ordering::Relaxed) {
        return Err(Response::text("SIP 端点未开启（SIP_*_PORT=off）").with_status_code(501));
    }
    let registry = TEMP_PASSWORDS
        .get()
        .expect("temp passwords initialized in main")
        .clone();
    Ok(registry)
}

/// POST /admin/temp-password：为设备签发临时密码（固定密码之外的一次性访问凭证）。
/// 请求体：`{"device_id":"AD-XX","ttl_secs":300}`（ttl 60..86400，缺省 300）。
/// 响应：`{"device_id","password","ttl_secs","expires_at_secs"}`。
fn temp_password_issue(request: &Request, config: &Config) -> Response {
    let registry = match admin_guard(request, config) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    // rouille 3.6：data() 返回 Option<RequestBody>（Read），整读成 Vec。
    let mut raw = Vec::new();
    let body_ok = match request.data() {
        Some(mut body) => {
            use std::io::Read;
            body.read_to_end(&mut raw).is_ok()
        }
        None => false,
    };
    if !body_ok {
        return Response::text("缺少请求体").with_status_code(400);
    }
    let body: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => {
            return Response::text(format!("请求体 JSON 解析失败：{e}")).with_status_code(400);
        }
    };
    let device = body
        .get("device_id")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.starts_with("view-"));
    let Some(device) = device else {
        return Response::text("device_id 缺失或非法").with_status_code(400);
    };
    let ttl = body
        .get("ttl_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(300)
        .clamp(60, 86400);
    let password = generate_temp_password();
    let ttl = Duration::from_secs(ttl);
    let _expires_at = registry
        .lock()
        .unwrap_or_else(aerodesk_protocol::util::lock_recover)
        .issue(device, password.clone(), ttl);
    let expires_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() + ttl.as_secs())
        .unwrap_or(0);
    info!(device, ttl_secs = ttl.as_secs(), "临时密码已签发");
    Response::json(&serde_json::json!({
        "device_id": device,
        "password": password,
        "ttl_secs": ttl.as_secs(),
        "expires_at_secs": expires_at_secs,
    }))
}

/// DELETE /admin/temp-password/<device>：撤销设备临时密码（未到期即失效）。
fn temp_password_revoke(request: &Request, config: &Config) -> Response {
    let registry = match admin_guard(request, config) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let url = request.url();
    let device = url.trim_start_matches("/admin/temp-password/");
    let revoked = registry
        .lock()
        .unwrap_or_else(aerodesk_protocol::util::lock_recover)
        .revoke(device);
    info!(device, revoked, "临时密码已撤销");
    Response::json(&serde_json::json!({ "device_id": device, "revoked": revoked }))
}

/// 生成随机临时密码（8 位，去除易混淆字符 0/O/1/I/l）——与桌面端
/// `generate_one_time_password` 同构（getrandom CSPRNG + 拒绝采样；访问口令
/// 不能用时间/进程态伪随机，见桌面端该函数注释）。
fn generate_temp_password() -> String {
    const CHARS: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz";
    const ACCEPT: usize = CHARS.len() * 4;
    let mut buf = [0u8; 8];
    let mut out = String::with_capacity(8);
    loop {
        getrandom::getrandom(&mut buf).expect("OS random source available");
        for &b in &buf {
            let idx = b as usize;
            if idx < ACCEPT {
                out.push(CHARS[idx % CHARS.len()] as char);
                if out.len() == 8 {
                    return out;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            auth_tokens: vec![],
            pop_id: "pop-a".into(),
            pop_registry: None,
            pop_sip_urls: vec![],
            sfu_urls: vec!["http://127.0.0.1:3002".into()],
            sfu_token: None,
            sfu_poll_interval_secs: 5,
            sfu_fail_cooldown_secs: 30,
            sfu_sticky_ttl_secs: 6 * 3600,
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
        // #12：结构化 JSON（SFU /metrics），取最热分片 ×10000。
        let body = r#"{"shards":[{"shard":0,"shard_load":0.25},{"shard":1,"shard_load":0.9}]}"#;
        assert_eq!(parse_max_shard_load(body), 9000);
        assert_eq!(parse_max_shard_load("not json"), 0);
        assert_eq!(parse_max_shard_load(r#"{"shards":[]}"#), 0);
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
    fn devices_endpoint_returns_json_with_sip_only() {
        use std::io::Read;
        let config = cfg();
        let h = ops_router(
            &Request::fake_http("GET", "/devices", vec![], Vec::new()),
            &config,
        );
        assert_eq!(h.status_code, 200);
        let (mut reader, _size) = h.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // 无注册时的空列表（有 SIP e2e 并发注册时只增不减，列表非空亦合法——
        // 只断言结构与 pop 字段）。
        assert!(v["devices"].is_array(), "{body}");
        assert_eq!(v["pop"], "pop-a");
    }

    #[test]
    fn health_and_metrics_endpoints() {
        use std::io::Read;
        let config = cfg();

        let h = ops_router(
            &Request::fake_http("GET", "/healthz", vec![], Vec::new()),
            &config,
        );
        assert_eq!(h.status_code, 200);
        let (mut reader, _size) = h.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["pop"], "pop-a");
        // clients/rooms 字段随 JSON 栈退役（P3.1）；sip 字段形状由
        // sip_health_json_shape 单测覆盖（此处不依赖全局端点状态）。
        assert!(v.get("clients").is_none(), "{body}");
        assert!(v.get("rooms").is_none(), "{body}");

        let m = ops_router(
            &Request::fake_http("GET", "/metrics/prometheus", vec![], Vec::new()),
            &config,
        );
        let (mut reader, _size) = m.data.into_reader_and_size();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        assert!(
            !body.contains("aerodesk_signal_"),
            "JSON 面三 gauge 应退役：{body}"
        );
    }

    /// 极简 HTTP 假服务器：接受一个连接，读完请求头+请求体，回固定响应。
    ///
    /// 必须按 content-length 排空请求体再回包/关闭：若读到头就 drop socket，
    /// body 字节尚在途中（并行负载下常见）时 Windows 对「接收缓冲有未读数据」
    /// 的 closesocket 发 RST，客户端 ureq 报传输错误而非 `Error::Status`，
    /// 把本应 4xx 的响应误分类为可转移故障。
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
            // 兜底读超时：body 迟迟不到时不无限挂起测试线程。
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut req = String::new();
            let mut buf = [0u8; 4096];
            // 先读到请求头结束。
            let head_end = loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => {
                        req.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if let Some(p) = req.find("\r\n\r\n") {
                            break Some(p + 4);
                        }
                    }
                }
            };
            // 再排空请求体（GET 无 body 即 0），保证关闭时接收缓冲为空走优雅 FIN。
            if let Some(head_end) = head_end {
                let clen = req[..head_end]
                    .to_ascii_lowercase()
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut received = req.len() - head_end;
                while received < clen {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            req.push_str(&String::from_utf8_lossy(&buf[..n]));
                            received += n;
                        }
                    }
                }
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
        // #12：结构化 JSON（与 SFU /metrics 同构：shards[].shard_load）。
        let body = r#"{"shards":[{"shard":0,"shard_load":0.9},{"shard":1,"shard_load":0.5}]}"#;
        let (addr, rx) = fake_http_server("HTTP/1.1 200 OK", body);
        let url = format!("http://{addr}/metrics");
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
        let url = format!("http://{addr}/metrics");
        assert_eq!(poll_sfu_load(&url, None), Err(PollErr::Unauthorized));
    }

    /// 粘性映射淘汰（P3.1 纯 TTL 口径）：空闲超过 TTL 即淘汰——last_used 由
    /// SIP INVITE 会议分支（池唯一消费点）刷新，零 INVITE 超 TTL 视为死房间。
    #[test]
    fn sticky_map_evicts_idle_rooms() {
        let pool = SfuPool::new(vec!["http://s1".into(), "http://s2".into()]);
        let _ = pool.select("stale");
        let _ = pool.select("fresh");
        {
            let mut reg = pool
                .room_sfu
                .lock()
                .unwrap_or_else(aerodesk_protocol::util::lock_recover);
            reg.get_mut("stale").unwrap().1 = unix_secs() - 7 * 3600;
            reg.get_mut("fresh").unwrap().1 = unix_secs();
        }
        let evicted = pool.evict_stale(6 * 3600, unix_secs());
        assert_eq!(evicted, 1, "只淘汰空闲超过 TTL 的房间");
        let reg = pool
            .room_sfu
            .lock()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
        assert!(reg.contains_key("fresh"), "活跃（近期有 INVITE）不得淘汰");
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

    // -- P3.1 新增：D2 默认翻转 / D3 健康面 / D4 failover / 多 PoP 302 --

    #[test]
    fn parse_port_or_off_variants() {
        assert_eq!(parse_port_or_off(None, "SIP_UDP_PORT", 5060), Some(5060));
        assert_eq!(parse_port_or_off(Some("5062"), "X", 5060), Some(5062));
        assert_eq!(parse_port_or_off(Some("off"), "X", 5060), None);
        assert_eq!(parse_port_or_off(Some("OFF"), "X", 5060), None);
        assert_eq!(parse_port_or_off(Some("Disabled"), "X", 5060), None);
        assert_eq!(parse_port_or_off(Some("none"), "X", 5060), None);
        assert_eq!(
            parse_port_or_off(Some("bad"), "SIP_TLS_PORT", 5061),
            Some(5061),
            "非法值回退默认"
        );
    }

    /// /admin/temp-password 三分支：503（无 token）/ 401（口令错）/ 501（SIP 端点关）。
    #[test]
    fn admin_guard_three_branches() {
        let req = || Request::fake_http("POST", "/admin/temp-password", vec![], Vec::new());

        // 503：无任何管理 token。
        let c = cfg();
        match admin_guard(&req(), &c) {
            Err(r) => assert_eq!(r.status_code, 503),
            Ok(_) => panic!("无 token 应 503"),
        }

        // 401：token 存在但 Bearer 不匹配。
        let c = Config {
            auth_tokens: vec!["tok".into()],
            ..cfg()
        };
        let req401 = Request::fake_http(
            "POST",
            "/admin/temp-password",
            vec![("Authorization".into(), "Bearer wrong".into())],
            Vec::new(),
        );
        match admin_guard(&req401, &c) {
            Err(r) => assert_eq!(r.status_code, 401),
            Ok(_) => panic!("口令错应 401"),
        }

        // 501：鉴权通过但 SIP 端点关闭（SIP_ENDPOINT_ENABLED=false）。
        let prev = SIP_ENDPOINT_ENABLED.load(Ordering::Relaxed);
        SIP_ENDPOINT_ENABLED.store(false, Ordering::Relaxed);
        let req501 = Request::fake_http(
            "POST",
            "/admin/temp-password",
            vec![("Authorization".into(), "Bearer tok".into())],
            Vec::new(),
        );
        let r = admin_guard(&req501, &c);
        SIP_ENDPOINT_ENABLED.store(prev, Ordering::Relaxed);
        match r {
            Err(resp) => assert_eq!(resp.status_code, 501),
            Ok(_) => panic!("SIP 端点关闭应 501"),
        }
    }

    /// /healthz `sip` 字段形状：关闭 → null；开启 → 三传输布尔。
    #[test]
    fn sip_health_json_shape() {
        assert_eq!(sip_health_json(false, None), serde_json::Value::Null);
        assert_eq!(
            sip_health_json(true, Some((true, true, true))),
            serde_json::json!({"tls": true, "udp": true, "wss": true})
        );
        // 端点开但状态未上报（极端窗口）→ 全 false 而非 panic。
        assert_eq!(
            sip_health_json(true, None),
            serde_json::json!({"tls": false, "udp": false, "wss": false})
        );
    }

    /// D4 候选次序：首选在前，其余健康优先 + 负载升序；无池保持原序。
    #[test]
    fn order_candidates_healthy_and_loaded() {
        let pool = SfuPool::new(vec![
            "http://a".into(),
            "http://b".into(),
            "http://c".into(),
        ]);
        pool.loads[1].store(1000, Ordering::Relaxed);
        pool.loads[2].store(5000, Ordering::Relaxed);
        // 全健康：首选 0，其余按负载升序 [1, 2]。
        assert_eq!(order_candidates(0, Some(&pool), 3), vec![0, 1, 2]);
        // 1 下线：即使负载最低也排到最后（健康优先）。
        pool.down_until[1].store(unix_secs() + 60, Ordering::Relaxed);
        assert_eq!(order_candidates(0, Some(&pool), 3), vec![0, 2, 1]);
        // 无池：保持原序（rendezvous 兜底语义）。
        assert_eq!(order_candidates(1, None, 3), vec![1, 0, 2]);
    }

    /// D4 有界 failover：首选 5xx → 转移到次选成功；4xx（配置/请求错）不转移。
    /// 候选首选由 rendezvous 决定（权重只依赖下标，与 URL 内容无关）——
    /// 把「故障」mock 放到 `sfu_for_room` 首选下标，使断言与哈希结果解耦。
    #[test]
    fn sfu_proxy_start_fails_over_only_on_transient() {
        // 首选回 500（瞬时故障），次选回 200 answer → 应转移并成功。
        let (bad, bad_rx) = fake_http_server("HTTP/1.1 500 Internal Server Error", "");
        let (good, good_rx) = fake_http_server("HTTP/1.1 200 OK", "ANSWER-BODY");
        let mut urls = vec![format!("http://{bad}"), format!("http://{good}")];
        if sfu_for_room(&urls, "room-fo") != 0 {
            urls.swap(0, 1);
        }
        let answer = sip_server::sfu_proxy_start(&urls, None, "room-fo", "OFFER");
        assert_eq!(answer, Ok("ANSWER-BODY".to_string()), "5xx 应转移次选");
        assert!(bad_rx.recv_timeout(Duration::from_secs(5)).is_ok());
        assert!(good_rx.recv_timeout(Duration::from_secs(5)).is_ok());

        // 首选回 400（请求/配置错，转移无意义）→ 立即失败，次选不被打扰。
        let (fatal, fatal_rx) = fake_http_server("HTTP/1.1 400 Bad Request", "");
        let (idle, idle_rx) = fake_http_server("HTTP/1.1 200 OK", "SHOULD-NOT-HIT");
        let mut urls = vec![format!("http://{fatal}"), format!("http://{idle}")];
        if sfu_for_room(&urls, "room-fo") != 0 {
            urls.swap(0, 1);
        }
        let r = sip_server::sfu_proxy_start(&urls, None, "room-fo", "OFFER");
        assert!(r.is_err(), "4xx 应原样失败");
        assert!(fatal_rx.recv_timeout(Duration::from_secs(5)).is_ok());
        assert!(
            idle_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "4xx 不得转移到次选"
        );
    }

    /// 302 Contact 头形状（RFC 3261 §20.10）：`<sip:<room>@<host:port>>`。
    #[test]
    fn pop_contact_value_shape() {
        assert_eq!(
            sip_server::pop_contact_value("meet-1", "127.0.0.1:3008"),
            "<sip:meet-1@127.0.0.1:3008>"
        );
    }

    /// 多 PoP INVITE 归属三态（#146/#154 迁移）：未登记 → 登记（首 INVITE）；
    /// 本 PoP → 刷 TTL；他 PoP → 有目标 302 / 无目标 486。
    #[test]
    fn decide_pop_route_register_refresh_redirect_busy() {
        // 未开注册表（单 PoP 默认）：直接放行，行为零变化。
        assert_eq!(
            sip_server::decide_pop_route("meet-1", "pop-a", None, &[]),
            sip_server::PopRoute::Proceed {
                first_registration: false
            }
        );

        let reg = PopRegistry::new(None, 3600);
        // 未登记 → Proceed{first_registration: true}（首个 INVITE 登记 owner PoP）。
        assert_eq!(
            sip_server::decide_pop_route("meet-2", "pop-a", Some(&reg), &[]),
            sip_server::PopRoute::Proceed {
                first_registration: true
            }
        );
        reg.register("meet-2", "pop-a");
        // 本 PoP 命中 → Proceed{false}（刷 TTL）。
        assert_eq!(
            sip_server::decide_pop_route("meet-2", "pop-a", Some(&reg), &[]),
            sip_server::PopRoute::Proceed {
                first_registration: false
            }
        );
        // 他 PoP 命中 + 有目标 → 302 Redirect。
        reg.register("meet-3", "pop-b");
        let targets = vec![("pop-b".to_string(), "127.0.0.1:3008".to_string())];
        assert_eq!(
            sip_server::decide_pop_route("meet-3", "pop-a", Some(&reg), &targets),
            sip_server::PopRoute::Redirect {
                pop: "pop-b".into(),
                host_port: "127.0.0.1:3008".into()
            }
        );
        // 他 PoP 命中 + 无目标 → 486（即刻失败优于 30s 超时）。
        assert_eq!(
            sip_server::decide_pop_route("meet-3", "pop-a", Some(&reg), &[]),
            sip_server::PopRoute::BusyNoTarget {
                pop: "pop-b".into()
            }
        );
    }
}
