//! SIP 信令端点（#551 / 规范 docs/SIP_SIGNALING.md）。
//!
//! 本模块把 rsipstack 引入 signal，与现有 JSON/rouille 服务**双栈并存**：SIP 端点由
//! `SIP_TLS_PORT` / `SIP_WSS_PORT` / `SIP_UDP_PORT` 显式开启（默认关闭，生产 JSON
//! 路径不受影响，规范 §8 迁移约束）。
//!
//! 已落地（slice 1 Registrar + slice 2 透明 Proxy）：
//! - 传输：SIP/TLS（原生端默认）+ SIP/WSS（Web，RFC 7118）+ SIP/UDP（内网/调试可选，
//!   规范 §0 传输矩阵）；TLS 身份复用 signal 证书加载；
//! - `REGISTER` + SIP Digest 认证（401 质询 → 200）；AoR→Contact 注册表，含路由地址
//!   （可靠传输复用注册 flow / UDP 经 Via received+rport）、expires 过期、expires=0 注销；
//! - 透明 INVITE Proxy（规范 §4）：A 腿（主叫）与 B 腿（被叫）dialog 配对，INVITE/
//!   180/200/ACK 端到端透传、SDP 零修改（非 B2BUA——仅 From/To tag 由 dialog API 重生成，
//!   见 proxy_call 备注）；CANCEL/BYE 级联、INFO（trickle-ice-sdpfrag）双向透传；
//!   被叫离线/未注册 → 404（规范 §3 offline）；
//! - `OPTIONS` 保活/能力探测（200 + Allow）；
//! - **INVITE 授权（#503-4）**：被叫设备有口令（固定密码/临时密码）时要求
//!   `Proxy-Authorization`——407 质询（Digest，与 REGISTER 同款）→ 客户端以
//!   被叫口令应答 → 校验通过放行、口令错 403；无口令设备不设卡（旧行为）；
//! - 严格子集（规范 §6）：SUBSCRIBE/NOTIFY/REFER/UPDATE/… 一律 501（ACK/CANCEL 由
//!   rsipstack 事务层吸收，不进分发循环）；
//! - 指标 `sip_registrations`/`sip_calls_established`/`sip_calls_terminated`（/metrics）。
//!
//! 硬化项（后续，不影响线上互通）：HA1-at-rest、nonce stale 防重放、连接关闭时清理
//! 注册 flow（transport inspector janitor）、多 PoP 302。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bridge;
use rsipstack::EndpointBuilder;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::authenticate::verify_digest;
use rsipstack::dialog::dialog::{DialogState, DialogStateReceiver, TerminatedReason};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::{Header, HeadersExt, Method, Request, StatusCode};
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::TransportLayer;
use rsipstack::transport::connection::SipConnection;
use rsipstack::transport::sip_addr::SipAddr;
use rsipstack::transport::tls::{TlsConfig, TlsListenerConnection};
use rsipstack::transport::websocket::WebSocketListenerConnection;
use tokio_util::sync::CancellationToken;

use aerodesk_protocol::sip::PROTOCOL_VERSION;
use aerodesk_protocol::tls::TlsIdentity;

/// 严格子集（规范 §6）：本端点实现的方法；其余一律 501。
const IMPLEMENTED: &[Method] = &[
    Method::Register,
    Method::Invite,
    Method::Ack,
    Method::Bye,
    Method::Cancel,
    Method::Info,
    Method::Options,
];

/// 注册默认有效期（秒），客户端 Contact/Expires 未给时采用。
const DEFAULT_EXPIRES_SECS: u32 = 300;

/// 一条注册绑定：AoR → Contact + 路由地址 + 过期时刻。
/// `route` 是 INVITE 的转发目的地（`InviteOption.destination`）：
/// 可靠传输（TLS/WSS/TCP）= REGISTER 连接的 remote addr（命中连接表复用 flow，
/// RFC 5626）；UDP = 经 Via received/rport 解析的源地址。
#[derive(Debug, Clone)]
pub struct Binding {
    #[allow(dead_code)] // 原始 Contact 头，保留供排障/未来 Request-URI 改写
    pub contact: String,
    /// INVITE 路由目的地（透明转发关键）。
    pub route: Option<SipAddr>,
    pub expires_at: Instant,
}

/// Registrar 注册表（纯数据、可单测）。
#[derive(Debug, Default)]
pub struct Registrar {
    bindings: HashMap<String, Binding>,
}

impl Registrar {
    /// upsert 绑定，返回过期秒数。重复 REGISTER 覆盖（不并行响铃，规范 §6 forking=P0 单 Contact）。
    pub fn register(
        &mut self,
        aor: &str,
        contact: String,
        route: Option<SipAddr>,
        expires_secs: u32,
    ) -> u32 {
        self.bindings.insert(
            aor.to_string(),
            Binding {
                contact,
                route,
                expires_at: Instant::now() + Duration::from_secs(expires_secs as u64),
            },
        );
        expires_secs
    }

    /// 注销（REGISTER expires=0）。返回是否存在过绑定。
    pub fn unregister(&mut self, aor: &str) -> bool {
        self.bindings.remove(aor).is_some()
    }

    /// 查询有效绑定（惰性剔除过期）。INVITE 路由读取。
    pub fn lookup(&mut self, aor: &str) -> Option<&Binding> {
        if self
            .bindings
            .get(aor)
            .map(|b| b.expires_at <= Instant::now())
            .unwrap_or(false)
        {
            self.bindings.remove(aor);
        }
        self.bindings.get(aor)
    }

    pub fn len(&mut self) -> usize {
        let now = Instant::now();
        self.bindings.retain(|_, b| b.expires_at > now);
        self.bindings.len()
    }

    /// 当前有效注册的 AoR 列表（惰性剔除过期；#503 设备列表数据源）。
    pub fn online_aors(&mut self) -> Vec<String> {
        let now = Instant::now();
        self.bindings.retain(|_, b| b.expires_at > now);
        let mut aors: Vec<String> = self.bindings.keys().cloned().collect();
        aors.sort();
        aors
    }
}

/// 临时口令条目（#503-4：主控端发起、带有效期，用于无人值守访问）。
#[derive(Debug, Clone)]
pub struct TempPassword {
    pub password: String,
    pub expires_at: Instant,
}

/// 临时口令注册表（纯数据、可单测）：设备 ID → 有效期内一次性口令。
/// 生效域 = INVITE 授权（[`decide_invite`] 临时口令分支），不参与 REGISTER。
#[derive(Debug, Default)]
pub struct TempRegistry {
    entries: HashMap<String, TempPassword>,
}

impl TempRegistry {
    /// 签发：覆盖同设备旧临时口令，返回过期时刻。
    pub fn issue(&mut self, device: &str, password: String, ttl: Duration) -> Instant {
        let expires_at = Instant::now() + ttl;
        self.entries.insert(
            device.to_string(),
            TempPassword {
                password,
                expires_at,
            },
        );
        expires_at
    }

    /// 查询有效临时口令（惰性剔除过期）。无/已过期 → None。
    pub fn lookup(&mut self, device: &str) -> Option<String> {
        if self
            .entries
            .get(device)
            .map(|e| e.expires_at <= Instant::now())
            .unwrap_or(false)
        {
            self.entries.remove(device);
        }
        self.entries.get(device).map(|e| e.password.clone())
    }

    /// 撤销（主控端主动作废，未到期即失效）。
    pub fn revoke(&mut self, device: &str) -> bool {
        self.entries.remove(device).is_some()
    }

    pub fn len(&mut self) -> usize {
        let now = Instant::now();
        self.entries.retain(|_, e| e.expires_at > now);
        self.entries.len()
    }
}

/// SIP 指标（#551 任务清单）：经 `/metrics/prometheus` 暴露。
#[derive(Default)]
pub struct SipMetrics {
    /// 当前在线注册绑定数（gauge）。
    pub registrations: AtomicU64,
    /// 累计建立的呼叫（Confirmed 去重后）。
    pub calls_established: AtomicU64,
    /// 累计结束的呼叫（Terminated 去重后）。
    pub calls_terminated: AtomicU64,
}

/// 当前 SIP 端点的指标句柄。每次 `serve()` 启动时**替换**（生产进程内仅一个端点，
/// 语义不变；测试多实例并存时以最新启动者为准——测试侧另有串行锁防交叉）。
static SIP_METRICS: std::sync::RwLock<Option<Arc<SipMetrics>>> = std::sync::RwLock::new(None);

/// 当前 SIP 端点的注册表句柄（#503 `/devices` 数据源；与 SIP_METRICS 同替换模式）。
static SIP_REGISTRAR: std::sync::RwLock<Option<Arc<Mutex<Registrar>>>> =
    std::sync::RwLock::new(None);

/// 供 main.rs `/devices` 读取在线注册 AoR（未启动 SIP 端点时为 None）。
pub fn registrar_snapshot() -> Option<Vec<String>> {
    let guard = SIP_REGISTRAR.read().ok()?;
    let reg = guard.as_ref()?;
    Some(reg.lock().unwrap().online_aors())
}

/// 供 main.rs `/metrics/prometheus` 读取（未启动 SIP 端点时为 None）。
pub fn metrics_snapshot() -> Option<(u64, u64, u64)> {
    let guard = SIP_METRICS.read().ok()?;
    let m = guard.as_ref()?;
    Some((
        m.registrations.load(Ordering::Relaxed),
        m.calls_established.load(Ordering::Relaxed),
        m.calls_terminated.load(Ordering::Relaxed),
    ))
}

/// 生成 Digest 质询 nonce（无状态，slice 1 不追踪 stale——见模块头硬化项）。
fn make_nonce(counter: &AtomicU64, secret: &str) -> String {
    let n = counter.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 20))
        .unwrap_or(0);
    // 简单混合，避免引入额外哈希依赖；nonce 仅为质询一次性值。
    let mut x = secret
        .as_bytes()
        .iter()
        .fold(0x9e3779b97f4a7c15u64, |acc, &b| {
            (acc ^ b as u64).wrapping_mul(0x100000001b3)
        });
    x ^= n.wrapping_mul(0x9e3779b97f4a7c15);
    x ^= nanos.rotate_left(17);
    format!("{x:016x}{n:08x}")
}

/// 构造 401 的 WWW-Authenticate 头值（Digest 质询）。
fn www_authenticate_value(realm: &str, nonce: &str) -> String {
    format!("Digest realm=\"{realm}\", nonce=\"{nonce}\", algorithm=MD5, qop=\"auth\"")
}

/// 从请求里取 Authorization 头原始值（Digest 计算需保留原文大小写）。
fn raw_authorization(req: &Request) -> Option<String> {
    req.headers
        .iter()
        .find(|h| h.name().eq_ignore_ascii_case("Authorization"))
        .map(|h| h.value().to_string())
}

/// 取注册 AoR：优先 To 头 URI 的 user（REGISTER 的 To = 注册的 AoR）。
fn request_aor(req: &Request) -> Option<String> {
    let to = req.to_header().ok()?;
    let uri = to.uri().ok()?;
    let user = uri.auth.as_ref().map(|a| a.user.clone())?;
    if user.is_empty() { None } else { Some(user) }
}

/// REGISTER 决策结果（纯逻辑，便于单测）。
#[derive(Debug, PartialEq, Eq)]
pub enum RegisterDecision {
    /// 未带/带错 Authorization → 401 质询（值为 WWW-Authenticate 头值）。
    Challenge(String),
    /// 带了 Authorization 但口令错误 → 403。
    Forbidden,
    /// 认证通过且 expires=0 → 注销（200）。
    Unregistered,
    /// 认证通过 → 已注册（200，值为过期秒数）。
    Registered(u32),
}

/// REGISTER 的核心判定（与传输无关，可单测）。
///
/// `password_of`：username(=设备 ID) → 口令（None = 未知设备）。
pub fn decide_register(
    req: &Request,
    realm: &str,
    nonce: &str,
    password_of: &dyn Fn(&str) -> Option<String>,
) -> RegisterDecision {
    let Some(aor) = request_aor(req) else {
        // 无法定位 AoR —— 当作质询处理（客户端会带齐信息重试）。
        return RegisterDecision::Challenge(www_authenticate_value(realm, nonce));
    };
    let Some(raw_auth) = raw_authorization(req) else {
        return RegisterDecision::Challenge(www_authenticate_value(realm, nonce));
    };
    let auth = match rsipstack::sip::typed::Authorization::parse(&raw_auth) {
        Ok(a) => a,
        Err(_) => return RegisterDecision::Challenge(www_authenticate_value(realm, nonce)),
    };
    let Some(password) = password_of(&auth.username) else {
        // 未知设备：不泄露存在性，按口令错误处理（403）。
        return RegisterDecision::Forbidden;
    };
    if !verify_digest(&auth, &password, &Method::Register, &raw_auth) {
        return RegisterDecision::Forbidden;
    }
    // expires=0 → 注销；否则注册。过期取 Expires 头或 Contact 参数，缺省默认。
    let expires = req
        .expires_header()
        .and_then(|e| e.value().parse::<u32>().ok())
        .unwrap_or(DEFAULT_EXPIRES_SECS);
    if expires == 0 {
        let _ = aor;
        RegisterDecision::Unregistered
    } else {
        RegisterDecision::Registered(expires)
    }
}

/// INVITE 授权决策结果（#503-4：无人值守固定密码 + 临时密码）。
#[derive(Debug, PartialEq, Eq)]
pub enum InviteDecision {
    /// 放行（目标无任何口令配置 → 与旧行为一致）。
    Allow,
    /// 407 质询（值 = Proxy-Authenticate 头；客户端应以被叫口令应答）。
    Challenge(String),
    /// 已带 Proxy-Authorization 但口令错误 → 403。
    Forbidden,
}

/// INVITE 授权核心判定（与传输无关，可单测）。
///
/// `fixed_password`：被叫设备的固定口令（设备表显式配置或共享 token；None = 未配置）；
/// `temp_password`：该设备当前有效的临时口令（None = 无）。
/// 任一匹配即放行——临时口令在有效期内等效固定口令。407 质询与 REGISTER 同款
/// Digest（realm/nonce/algorithm/qop），客户端 rsipstack 原生处理
/// Proxy-Authorization（`handle_client_authenticate`）。
pub fn decide_invite(
    req: &Request,
    realm: &str,
    nonce: &str,
    fixed_password: Option<&str>,
    temp_password: Option<&str>,
) -> InviteDecision {
    if fixed_password.is_none() && temp_password.is_none() {
        // 目标无任何口令：不设卡（开放部署/未配置口令的设备，保持旧行为）。
        return InviteDecision::Allow;
    }
    let Some(raw) = raw_proxy_authorization(req) else {
        return InviteDecision::Challenge(www_authenticate_value(realm, nonce));
    };
    let Ok(proxy) = rsipstack::sip::typed::ProxyAuthorization::parse(&raw) else {
        // 无法解析按质询处理（客户端会带齐参数重试）。
        return InviteDecision::Challenge(www_authenticate_value(realm, nonce));
    };
    // ProxyAuthorization 与 Authorization 字段同构：转成后者复用 verify_digest。
    let auth = rsipstack::sip::typed::Authorization {
        scheme: proxy.scheme,
        username: proxy.username,
        realm: proxy.realm,
        nonce: proxy.nonce,
        uri: proxy.uri,
        response: proxy.response,
        algorithm: proxy.algorithm,
        opaque: proxy.opaque,
        qop: proxy.qop,
    };
    // 固定口令与临时口令任一匹配即放行（ha1 以报文中 username 为准，两分支独立计算）。
    let ok = fixed_password.is_some_and(|p| verify_digest(&auth, p, &Method::Invite, &raw))
        || temp_password.is_some_and(|p| verify_digest(&auth, p, &Method::Invite, &raw));
    if ok {
        InviteDecision::Allow
    } else {
        InviteDecision::Forbidden
    }
}

/// 从请求里取 Proxy-Authorization 头原始值（Digest 计算需保留原文大小写）。
fn raw_proxy_authorization(req: &Request) -> Option<String> {
    req.headers
        .iter()
        .find(|h| h.name().eq_ignore_ascii_case("Proxy-Authorization"))
        .map(|h| h.value().to_string())
}

/// 被叫设备的固定口令（#503-4）：设备表显式配置优先，未列设备回退共享 token
/// （与 REGISTER 路径 password_of 同口径）；开放注册模式无口令（不设卡）。
fn callee_fixed_password(cfg: &SipConfig, device: &str) -> Option<String> {
    if cfg.open_register {
        return None;
    }
    cfg.passwords
        .get(device)
        .cloned()
        .or_else(|| cfg.token_password.clone())
}

/// SIP 端点配置。
pub struct SipConfig {
    pub realm: String,
    /// SIP over TLS 监听地址（None = 不开）。
    pub tls_addr: Option<SocketAddr>,
    /// SIP over WSS 监听地址（None = 不开）。
    pub wss_addr: Option<SocketAddr>,
    /// SIP over UDP 监听地址（None = 不开；内网/调试用，规范 §0 传输矩阵可选项）。
    pub udp_addr: Option<SocketAddr>,
    /// Digest 口令表：设备 ID → token（显式覆盖，SIP_DIGEST_USERS）。
    pub passwords: Arc<HashMap<String, String>>,
    /// 通用口令回退（规范 §8「迁移期同一凭据」）：未列设备以首个 AUTH_TOKEN
    /// 为口令——服务器无需逐设备配置即可承接存量 token 客户端；多 token 部署
    /// 用 SIP_DIGEST_USERS 显式覆盖。
    pub token_password: Option<String>,
    /// 开放注册（开发/e2e）：口令表与 token 均未配置时跳过 Digest 校验——
    /// 与 WSS join 同姿态（无鉴权源即开放，main.rs auth_ok 语义）。
    pub open_register: bool,
    /// 临时口令注册表（#503-4：主控端经 /admin/temp-password 签发、带有效期；
    /// INVITE 授权时与固定口令并列校验）。main.rs 与 HTTP 管理端点共享。
    pub temp_passwords: Arc<Mutex<TempRegistry>>,
    /// TLS 身份（复用 signal 的证书加载）。
    pub tls_identity: Option<TlsIdentity>,
    /// SFU 池（会议桥用）：非设备 AoR 的 INVITE 代理到 SFU /start（规范 §4
    /// 会议语义）。空 = 未配置（会议 INVITE 回 404）。
    pub sfu_urls: Vec<String>,
    /// SFU 内部接口 token（可选）。
    pub sfu_token: Option<String>,
}

/// 在独立 tokio runtime 上跑 SIP 端点（阻塞当前线程；由调用方 spawn 线程）。
/// 返回 Err 表示监听/建端点失败（启动 fail-fast）。
pub fn run_sip_endpoint(cfg: SipConfig, cancel: CancellationToken) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime 构建失败: {e}"))?;
    rt.block_on(async move { serve(cfg, cancel).await })
}

async fn serve(cfg: SipConfig, cancel: CancellationToken) -> Result<(), String> {
    let tl = TransportLayer::new(cancel.clone());

    // TLS 监听（原生端）。
    if let Some(addr) = cfg.tls_addr {
        let id = cfg
            .tls_identity
            .as_ref()
            .ok_or("SIP_TLS_PORT 已开但未提供 TLS 身份（CERT_FILE/KEY_FILE 或内嵌证书）")?;
        let tls_cfg = TlsConfig {
            cert: Some(id.cert.clone()),
            key: Some(id.key.clone()),
            client_cert: None,
            client_key: None,
            ca_certs: None,
            sni_hostname: None,
        };
        let listener = TlsListenerConnection::new(addr, None, tls_cfg)
            .await
            .map_err(|e| format!("SIP/TLS 监听 {addr} 失败: {e}"))?;
        tl.inner.add_listener(SipConnection::from(listener));
        info!(%addr, "SIP/TLS 监听已起");
    }

    // UDP 监听（内网/调试，规范 §0 可选项）。
    if let Some(addr) = cfg.udp_addr {
        let conn = rsipstack::transport::udp::UdpConnection::create_connection(
            addr,
            None,
            Some(cancel.clone()),
        )
        .await
        .map_err(|e| format!("SIP/UDP 监听 {addr} 失败: {e}"))?;
        tl.inner.add_listener(SipConnection::from(conn));
        info!(%addr, "SIP/UDP 监听已起");
    }

    // WSS 监听（Web 端，RFC 7118；rsipstack 复用 transport_layer 的 TLS 配置）。
    if let Some(addr) = cfg.wss_addr {
        if let Some(id) = cfg.tls_identity.as_ref() {
            tl.set_tls_config(TlsConfig {
                cert: Some(id.cert.clone()),
                key: Some(id.key.clone()),
                client_cert: None,
                client_key: None,
                ca_certs: None,
                sni_hostname: None,
            });
        }
        let listener = WebSocketListenerConnection::new(addr, None, true)
            .await
            .map_err(|e| format!("SIP/WSS 监听 {addr} 失败: {e}"))?;
        tl.inner.add_listener(SipConnection::from(listener));
        info!(%addr, "SIP/WSS 监听已起");
    }

    let endpoint = EndpointBuilder::new()
        .with_user_agent(PROTOCOL_VERSION)
        .with_transport_layer(tl)
        .with_cancel_token(cancel.clone())
        .with_allows(IMPLEMENTED.to_vec())
        .build();

    let mut incoming = endpoint
        .incoming_transactions()
        .map_err(|e| format!("取 incoming_transactions 失败: {e}"))?;

    // 起 endpoint 服务循环（transport + transaction 层）。
    {
        let inner = endpoint.inner.clone();
        tokio::spawn(async move {
            if let Err(e) = inner.serve().await {
                error!(error=%e, "SIP endpoint serve 退出");
            }
        });
    }

    let registrar = Arc::new(Mutex::new(Registrar::default()));
    // #503 设备列表：登记注册表句柄供 main.rs `/devices` 读取在线 AoR。
    let _ = SIP_REGISTRAR.write().unwrap().replace(registrar.clone());
    let nonce_counter = Arc::new(AtomicU64::new(0));
    let nonce_secret =
        format!("{:x}", &cfg.realm as *const _ as usize) + &format!("{:x}", std::process::id());

    // slice 2：dialog 层（透明 Proxy 的对话配对/状态机）+ 指标。
    let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
    let metrics = Arc::new(SipMetrics::default());
    let _ = SIP_METRICS.write().unwrap().replace(metrics.clone());
    // BYE Reason 头转发表（serve 循环捕获 → relay 级联时转发；Call-ID 键，Terminated 时消费）。
    let bye_reasons: Arc<Mutex<HashMap<String, String>>> = Arc::default();

    info!("SIP 端点已就绪（Registrar + 透明 INVITE Proxy + OPTIONS）");

    loop {
        let mut tx = tokio::select! {
            _ = cancel.cancelled() => break,
            maybe = incoming.recv() => match maybe {
                Some(tx) => tx,
                None => break,
            },
        };
        let req = &tx.original;
        let method = req.method;

        // 严格子集门禁（规范 §6）。ACK/CANCEL 由事务层吸收，不会到这里。
        if !IMPLEMENTED.contains(&method) {
            let _ = tx.reply(StatusCode::NotImplemented).await;
            continue;
        }

        match method {
            Method::Options => {
                let allow = Header::Other(
                    "Allow".into(),
                    IMPLEMENTED
                        .iter()
                        .map(|m| m.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                let _ = tx.reply_with(StatusCode::OK, vec![allow], None).await;
            }
            Method::Register => {
                let nonce = make_nonce(&nonce_counter, &nonce_secret);
                let passwords = cfg.passwords.clone();
                let token_password = cfg.token_password.clone();
                let open_register = cfg.open_register;
                let password_of = move |user: &str| {
                    if open_register {
                        // 开放模式：任意口令放行（占位口令使 Digest 校验必然通过
                        // 不可行——校验按对端口令算哈希；改为跳过校验走捷径分支）。
                        return Some(String::new());
                    }
                    passwords
                        .get(user)
                        .cloned()
                        .or_else(|| token_password.clone())
                };
                if open_register {
                    // 开放模式：解析出 AoR 即注册（跳过 Digest），与 WSS 无鉴权
                    // 源放行同姿态。路由地址提取与 Digest 路径同款（可靠传输复用
                    // flow；UDP 经 Via received/rport）——开放模式曾传 None 致
                    // INVITE 路由丢失（agent e2e 实测 B 腿静默死）。
                    if let Some(aor) = request_aor(req) {
                        let route = match &tx.connection {
                            Some(c) if c.is_reliable() => c.get_remote_addr().cloned(),
                            _ => {
                                endpoint
                                    .inner
                                    .get_destination_from_request(&tx.original)
                                    .await
                            }
                        };
                        let mut reg = registrar.lock().unwrap();
                        reg.register(&aor, "open".into(), route, DEFAULT_EXPIRES_SECS);
                        let n = reg.len();
                        drop(reg);
                        metrics.registrations.store(n as u64, Ordering::Relaxed);
                        info!(%aor, expires = DEFAULT_EXPIRES_SECS, online = n, "SIP 注册（开放模式，未验 Digest）");
                    }
                    let _ = tx.reply(StatusCode::OK).await;
                    continue;
                }
                match decide_register(req, &cfg.realm, &nonce, &password_of) {
                    RegisterDecision::Challenge(www) => {
                        let _ = tx
                            .reply_with(
                                StatusCode::Unauthorized,
                                vec![Header::Other("WWW-Authenticate".into(), www)],
                                None,
                            )
                            .await;
                    }
                    RegisterDecision::Forbidden => {
                        let _ = tx.reply(StatusCode::Forbidden).await;
                    }
                    RegisterDecision::Unregistered => {
                        if let Some(aor) = request_aor(req) {
                            let existed = registrar.lock().unwrap().unregister(&aor);
                            metrics
                                .registrations
                                .store(registrar.lock().unwrap().len() as u64, Ordering::Relaxed);
                            info!(%aor, existed, "SIP 注销");
                        }
                        let _ = tx.reply(StatusCode::OK).await;
                    }
                    RegisterDecision::Registered(expires) => {
                        if let Some(aor) = request_aor(req) {
                            let contact = req
                                .contact_header()
                                .ok()
                                .map(|c| c.value().to_string())
                                .unwrap_or_default();
                            // 路由地址：可靠传输复用注册连接的 remote addr（flow）；
                            // UDP 经 Via received/rport 解析源地址。
                            let route = match &tx.connection {
                                Some(c) if c.is_reliable() => c.get_remote_addr().cloned(),
                                _ => {
                                    endpoint
                                        .inner
                                        .get_destination_from_request(&tx.original)
                                        .await
                                }
                            };
                            let mut reg = registrar.lock().unwrap();
                            reg.register(&aor, contact, route, expires);
                            let n = reg.len();
                            drop(reg);
                            metrics.registrations.store(n as u64, Ordering::Relaxed);
                            info!(%aor, expires, online = n, "SIP 注册");
                        }
                        let _ = tx.reply(StatusCode::OK).await;
                    }
                }
            }
            // slice 2：透明 INVITE Proxy。
            Method::Invite => {
                let callee = request_aor(req);
                let binding = callee
                    .as_deref()
                    .and_then(|a| registrar.lock().unwrap().lookup(a).cloned());
                match binding {
                    None => {
                        let user = callee.clone().unwrap_or_default();
                        if !user.starts_with("AD-")
                            && !cfg.sfu_urls.is_empty()
                            && bridge::sanitize_room(&user)
                        {
                            // 会议语义（规范 §4）：非设备 AoR = SFU 房间名——
                            // 桥接到 SFU /start（offer→answer），200 OK 回 answer。
                            let offer = tx.original.body.clone();
                            let urls = cfg.sfu_urls.clone();
                            let token = cfg.sfu_token.clone();
                            let dl = dialog_layer.clone();
                            tokio::spawn(async move {
                                if let Err(e) =
                                    conference_bridge(dl, tx, user, offer, urls, token).await
                                {
                                    warn!(error=%e, "conference_bridge 失败");
                                }
                            });
                        } else {
                            // 设备不在线（AD-*）或非法房间名/未配 SFU → 404（规范 §3 offline）。
                            let _ = tx.reply(StatusCode::NotFound).await;
                        }
                    }
                    Some(binding) => {
                        // #503-4 无人值守口令授权：被叫设备有口令（固定/临时）时
                        // 要求 Proxy-Authorization——407 质询 → 客户端以被叫口令应答
                        // → 校验通过放行；口令错 403。无口令设备不设卡（旧行为）。
                        let nonce = make_nonce(&nonce_counter, &nonce_secret);
                        let fixed = callee
                            .as_deref()
                            .and_then(|c| callee_fixed_password(&cfg, c));
                        let temp = callee
                            .as_deref()
                            // 毒化容忍：Mutex 中毒时取回内部值继续服务——serve 循环内
                            // panic（如 lock().unwrap() 命中毒化）会拖垮整个 SIP 端点。
                            .and_then(|c| {
                                cfg.temp_passwords
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .lookup(c)
                            });
                        match decide_invite(
                            req,
                            &cfg.realm,
                            &nonce,
                            fixed.as_deref(),
                            temp.as_deref(),
                        ) {
                            InviteDecision::Allow => {
                                let dl = dialog_layer.clone();
                                let m = metrics.clone();
                                let br = bye_reasons.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = proxy_call(dl, tx, binding, m, br).await {
                                        warn!(error=%e, "proxy_call 失败");
                                    }
                                });
                            }
                            InviteDecision::Challenge(pa) => {
                                let _ = tx
                                    .reply_with(
                                        StatusCode::ProxyAuthenticationRequired,
                                        vec![Header::Other("Proxy-Authenticate".into(), pa)],
                                        None,
                                    )
                                    .await;
                            }
                            InviteDecision::Forbidden => {
                                warn!(callee = %callee.clone().unwrap_or_default(), "INVITE 授权失败（口令错）");
                                let _ = tx.reply(StatusCode::Forbidden).await;
                            }
                        }
                    }
                }
            }
            // 对话内 BYE/INFO：路由到已配对 dialog 处理（BYE 自动 200+Terminated；
            // INFO 经状态事件透传到对腿）。
            // BYE 的 Reason 头（如升级 cause=302，§4.1）不进 Terminated 事件——
            // 在此捕获到共享表，relay 级联 BYE 时转发（端到端保真）。
            Method::Bye => {
                if let Ok(h) = tx.original.call_id_header()
                    && let Some(r) = tx.original.headers.iter().find_map(|h| match h {
                        Header::Reason(reason) => Some(reason.value().to_string()),
                        _ => None,
                    })
                {
                    bye_reasons.lock().unwrap().insert(h.value().to_string(), r);
                }
                match dialog_layer.match_dialog(&tx) {
                    Some(mut dlg) => {
                        tokio::spawn(async move {
                            let _ = dlg.handle(&mut tx).await;
                        });
                    }
                    None => {
                        let _ = tx.reply(StatusCode::CallTransactionDoesNotExist).await;
                    }
                }
            }
            Method::Info => match dialog_layer.match_dialog(&tx) {
                Some(mut dlg) => {
                    tokio::spawn(async move {
                        let _ = dlg.handle(&mut tx).await;
                    });
                }
                None => {
                    let _ = tx.reply(StatusCode::CallTransactionDoesNotExist).await;
                }
            },
            // 其余实现清单内方法（ACK/CANCEL 已被事务层吸收，不会到这）一律 501。
            _ => {
                let _ = tx.reply(StatusCode::NotImplemented).await;
            }
        }
    }
    Ok(())
}

/// 透明 Proxy 一通呼叫：A 腿（主叫→proxy，UAS）与 B 腿（proxy→被叫，UAC）配对。
///
/// 端到端透传：Call-ID（`InviteOption.call_id` 复用）、SDP body（字节透传不解析）、
/// 状态码（180/200/486/603 等中继）。**透明性边界（实现备注）**：From/To tag 由
/// rsipstack dialog API 经 `make_tag()` 重新生成——这是 dialog 层固有的 B2BUA-lite
/// 特征，与规范 §4「同一对话端到端」的偏差仅在这些 tag；媒体协商（SDP）与呼叫
/// 语义（状态码/Call-ID）完全端到端。offerless INVITE（offer 在 ACK 里）不支持——
/// rsipstack 自动 ACK 不带 body，本协议 offer/answer 只在 INVITE/200 承载。
async fn proxy_call(
    dl: Arc<DialogLayer>,
    mut tx: Transaction,
    binding: Binding,
    metrics: Arc<SipMetrics>,
    bye_reasons: Arc<Mutex<HashMap<String, String>>>,
) -> Result<(), String> {
    let (state_tx, state_rx) = dl.new_dialog_state_channel();

    // 提取 A 侧 INVITE 字段（先克隆，随后 tx 移入 A 腿 handle 任务）。
    // 全部失败出口都回 final response——否则 INVITE 事务悬死，客户端
    // UI 静默卡住（与 conference_bridge 同责，#576 自审）。
    let orig = tx.original.clone();
    let call_id = orig.call_id_header().ok().map(|c| c.value().to_string());
    let offer = orig.body.clone();
    let Some(caller) = orig.from_header().ok().and_then(|f| f.uri().ok()) else {
        let _ = tx.reply(StatusCode::BadRequest).await;
        return Err("INVITE 缺 From 头".into());
    };
    let callee = orig.uri().clone();

    // A 腿 server dialog（生成 to-tag；自动 100 Trying/吸收 ACK/CANCEL 需驱动 handle）。
    let server_dlg = match dl.get_or_create_server_invite(&tx, state_tx.clone(), None, None) {
        Ok(d) => d,
        Err(e) => {
            let _ = tx.reply(StatusCode::ServerInternalError).await;
            return Err(format!("建 server dialog 失败: {e}"));
        }
    };
    let a_id = server_dlg.id();
    {
        let mut d = server_dlg.clone();
        tokio::spawn(async move {
            let _ = d.handle(&mut tx).await;
        });
    }

    // B 腿 client dialog：destination = 注册 flow（可靠复用连接 / UDP 源地址）。
    let contact = match dl.build_local_contact(None, None) {
        Ok(c) => c,
        Err(e) => {
            let _ = server_dlg.reject(Some(StatusCode::ServerInternalError), None);
            return Err(format!("构造 local contact 失败: {e}"));
        }
    };
    let opt = InviteOption {
        caller,
        callee,
        destination: binding.route.clone(),
        content_type: Some("application/sdp".into()),
        offer: Some(offer),
        contact,
        call_id,
        ..Default::default()
    };
    let (client_dlg, _final) = match dl.do_invite_async(opt, state_tx) {
        Ok(r) => r,
        Err(e) => {
            let _ = server_dlg.reject(Some(StatusCode::ServiceUnavailable), None);
            return Err(format!("转发 INVITE 到被叫失败: {e}"));
        }
    };
    let b_id = client_dlg.id();
    info!(%a_id, %b_id, "Proxy 呼叫双腿已配对");

    relay(
        state_rx,
        server_dlg,
        client_dlg,
        RelayCtx {
            a_id,
            b_id,
            dl,
            metrics,
            bye_reasons,
        },
    )
    .await;
    Ok(())
}

/// 双腿状态接力循环：18x/200 中继回主叫、INFO 双向透传、CANCEL/BYE 级联、
/// 挂断统计与 dialog 清理。双腿均 Terminated 后退出。
///
/// **腿匹配按 `local_tag`（腿创建即定、稳定），不能用整个 `DialogId`**——client 腿
/// 的 remote_tag（对端 to-tag）在 18x/200 后才补上，创建时抓的 `id()` 与事件里的 id
/// 不相等。两腿共享同一 Call-ID，故 local_tag 是唯一区分键。
/// 双腿接力循环的静态上下文（对腿 id/注册表/指标/升级 BYE Reason 表）。
/// 与 dialog 对（state_rx, server_dlg, client_dlg）分离，relay 签名保持在 4 参内。
struct RelayCtx {
    a_id: DialogId,
    b_id: DialogId,
    dl: Arc<DialogLayer>,
    metrics: Arc<SipMetrics>,
    bye_reasons: Arc<Mutex<HashMap<String, String>>>,
}

async fn relay(
    mut state_rx: DialogStateReceiver,
    server_dlg: InviteDialog,
    client_dlg: InviteDialog,
    ctx: RelayCtx,
) {
    let RelayCtx {
        a_id,
        b_id,
        dl,
        metrics,
        bye_reasons,
    } = ctx;
    let a_local = a_id.local_tag.clone();
    let b_local = b_id.local_tag.clone();
    let is_a = |id: &DialogId| id.local_tag == a_local;
    let is_b = |id: &DialogId| id.local_tag == b_local;

    let mut a_done = false;
    let mut b_done = false;
    let mut established = false;
    while let Some(st) = state_rx.recv().await {
        match st {
            // B 腿 18x → 中继 A 腿。180 不能带 body（rsipstack 见 body 升 183）；
            // 仅 183（带 early SDP）透 body。
            DialogState::Early(id, resp) if is_b(&id) => {
                let body = if resp.status_code == StatusCode::SessionProgress {
                    Some(resp.body.clone())
                } else {
                    None
                };
                let _ = server_dlg.ringing(None, body);
            }
            // B 腿 200 → A 腿 accept，SDP answer 字节透传。一次呼叫只计一次建立。
            DialogState::Confirmed(id, resp) if is_b(&id) && !established => {
                established = true;
                metrics.calls_established.fetch_add(1, Ordering::Relaxed);
                let ct = Header::Other("Content-Type".into(), "application/sdp".into());
                let _ = server_dlg.accept(Some(vec![ct]), Some(resp.body.clone()));
            }
            // INFO（trickle-ice-sdpfrag）双向透传：转发到对腿并把对腿响应中继回来。
            DialogState::Info(id, req, handle) => {
                let peer = if is_a(&id) { &client_dlg } else { &server_dlg };
                let ct = req
                    .headers
                    .iter()
                    .find(|h| h.name().eq_ignore_ascii_case("Content-Type"))
                    .map(|h| h.value().to_string());
                let hdrs = ct.map(|c| vec![Header::Other("Content-Type".into(), c)]);
                let body = req.body.clone();
                match peer.info(hdrs, Some(body)).await {
                    Ok(Some(resp)) => {
                        let _ = handle
                            .respond(resp.status_code, None, Some(resp.body.clone()))
                            .await;
                    }
                    Ok(None) => {
                        let _ = handle.reply(StatusCode::OK).await;
                    }
                    Err(_) => {
                        let _ = handle.reply(StatusCode::BadRequest).await;
                    }
                }
            }
            DialogState::Terminated(id, reason) => {
                if is_a(&id) {
                    a_done = true;
                }
                if is_b(&id) {
                    b_done = true;
                }
                // BYE 的 Reason 头（升级 cause=302 等）消费并随级联 BYE 端到端转发。
                let bye_reason = bye_reasons.lock().unwrap().remove(&id.call_id);
                match reason {
                    // 主叫取消（A 腿）→ 级联 CANCEL 到被叫腿（487 已由栈自动回主叫）。
                    TerminatedReason::UacCancel if is_a(&id) => {
                        let _ = client_dlg.cancel().await;
                    }
                    // 任一侧 BYE → 级联 BYE 到对腿（带 Reason 头则原样转发）。
                    TerminatedReason::UacBye | TerminatedReason::UasBye => {
                        let peer = if is_a(&id) { &client_dlg } else { &server_dlg };
                        let r = match bye_reason {
                            Some(r) => {
                                peer.bye_with_headers(Some(vec![Header::Reason(r.into())]))
                                    .await
                            }
                            None => peer.bye().await,
                        };
                        let _ = r;
                    }
                    // 被叫拒绝/忙/超时（B 腿）→ 原码回主叫（486/603/408/487…）。
                    TerminatedReason::UasOther(code) | TerminatedReason::UacOther(code)
                        if is_b(&id) =>
                    {
                        let _ = server_dlg.reject(Some(code), None);
                    }
                    _ => {}
                }
                // 挂断统计一次呼叫只计一次；双腿都从注册表移除（防泄漏）。
                if established {
                    established = false;
                    metrics.calls_terminated.fetch_add(1, Ordering::Relaxed);
                }
                dl.remove_dialog(&id);
                if a_done && b_done {
                    break;
                }
            }
            _ => {}
        }
    }
}

/// 会议桥（规范 §4）：INVITE 会议 AoR（无设备绑定的合法房间名）→ SFU /start
/// 代理（offer→answer），200 OK 回 answer body。
///
/// 生命周期：BYE 由 dialog 层自动 200+Terminate（SIP 侧）；SFU 会话依赖媒体
/// 超时回收（无 kick——/start 不返回会话 id，kick 按 room 会误伤同房其他端）。
/// 会议端 ICE 为内联候选（v1 非 trickle），INFO trickle 不适用。
async fn conference_bridge(
    dl: Arc<DialogLayer>,
    mut tx: Transaction,
    room: String,
    offer: Vec<u8>,
    sfu_urls: Vec<String>,
    sfu_token: Option<String>,
) -> Result<(), String> {
    let offer = String::from_utf8_lossy(&offer).to_string();
    info!(%room, "SIP 会议 INVITE → SFU 桥");
    // 本函数全部失败出口都回 final response——否则 INVITE 事务悬死，客户端
    // 既无 Answered 也无 Rejected，UI 静默卡住（#576 CI 实测）。dialog 注册表
    // 条目由终态看护清理（rsipstack 仅在显式 remove_dialog 时移除，否则无界增长）。
    let (state_tx, mut state_rx) = dl.new_dialog_state_channel();
    // A 腿 server dialog：自动 100 Trying/吸收 ACK/BYE（Contact/to-tag 由
    // dialog 机制生成——裸 reply_with 缺 Contact 客户端建不了 dialog）。
    let server_dlg = match dl.get_or_create_server_invite(&tx, state_tx, None, None) {
        Ok(d) => d,
        Err(e) => {
            // 腿未建（如缺 Contact）：事务仍在手——直接回 500，不悬死。
            warn!(%room, error=%e, "conference_bridge 建腿失败，回 500");
            let _ = tx.reply(StatusCode::ServerInternalError).await;
            return Ok(());
        }
    };
    {
        let mut d = server_dlg.clone();
        tokio::spawn(async move {
            let _ = d.handle(&mut tx).await;
        });
    }
    // 终态看护：Terminated（BYE/CANCEL/超时）后移除注册表条目。
    {
        let dl = dl.clone();
        let id = server_dlg.id();
        tokio::spawn(async move {
            while let Some(st) = state_rx.recv().await {
                if matches!(st, DialogState::Terminated(..)) {
                    dl.remove_dialog(&id);
                    break;
                }
            }
        });
    }
    let answer = match tokio::task::spawn_blocking(move || {
        sfu_proxy_start(&sfu_urls, sfu_token.as_deref(), &room, &offer)
    })
    .await
    {
        Ok(Ok(answer)) => answer,
        // 桥失败必须回 final response（客户端走 Rejected→失败提示）。
        Ok(Err(e)) => {
            warn!(error=%e, "SFU 桥接失败，回 503");
            eprintln!("conference_bridge: sfu_proxy_start error: {e}");
            let _ = server_dlg.reject(Some(StatusCode::ServiceUnavailable), None);
            return Ok(());
        }
        Err(e) => {
            warn!(error=%e, "SFU 桥接任务失败，回 503");
            let _ = server_dlg.reject(Some(StatusCode::ServiceUnavailable), None);
            return Ok(());
        }
    };
    let ct = Header::Other("Content-Type".into(), "application/sdp".into());
    if let Err(e) = server_dlg.accept(Some(vec![ct]), Some(answer.into_bytes())) {
        warn!(error=%e, "accept 200 失败，回 500");
        let _ = server_dlg.reject(Some(StatusCode::ServerInternalError), None);
    }
    Ok(())
}

/// SFU /start 代理（viewer 角色；与 main.rs `proxy_to_sfu` 同构，SIP 侧独立
/// 实现避免依赖 WSS Config）：房间名 FNV 哈希选池内 SFU（与 WSS 侧
/// `selected_sfu_idx` 语义一致——同房间稳定同 SFU）。
fn sfu_proxy_start(
    sfu_urls: &[String],
    sfu_token: Option<&str>,
    room: &str,
    offer: &str,
) -> Result<String, String> {
    let idx = fnv1a(room) as usize % sfu_urls.len();
    let mut url = format!("{}/start?room={room}&role=viewer", sfu_urls[idx]);
    url.push_str("&dc_ready=1");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let mut req = agent.post(&url).set("Content-Type", "application/json");
    if let Some(token) = sfu_token {
        req = req.set("X-Internal-Token", token);
    }
    let resp = req.send_string(offer).map_err(|e| e.to_string())?;
    resp.into_string().map_err(|e| e.to_string())
}

/// FNV-1a 房间哈希（SFU 池选路；与 WSS 侧同义）。
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsipstack::sip::Request;
    use rsipstack::sip::typed::Authorization;

    const REALM: &str = "aerodesk.test";
    const NONCE: &str = "testnonce123";

    /// serve() 系 e2e 的串行锁：SIP_METRICS 是进程级句柄（serve 启动时替换），
    /// 并发起服的多实例会互相覆盖指标视图，须串行。
    static SERVE_E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serve_e2e_guard() -> std::sync::MutexGuard<'static, ()> {
        SERVE_E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn register_request(aor: &str, auth_header: Option<&str>, expires: Option<u32>) -> Request {
        let mut headers = String::new();
        if let Some(a) = auth_header {
            headers.push_str(&format!("Authorization: {a}\r\n"));
        }
        if let Some(e) = expires {
            headers.push_str(&format!("Expires: {e}\r\n"));
        }
        let text = format!(
            "REGISTER sip:{REALM} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-t\r\n\
             From: <sip:{aor}@{REALM}>;tag=f1\r\n\
             To: <sip:{aor}@{REALM}>\r\n\
             Call-ID: reg-1\r\n\
             CSeq: 1 REGISTER\r\n\
             {headers}\
             Contact: <sip:{aor}@127.0.0.1:5060>\r\n\
             Content-Length: 0\r\n\r\n"
        );
        Request::try_from(text).expect("parse REGISTER")
    }

    fn passwords() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("AD-DEV1".to_string(), "tok-dev1".to_string());
        m
    }

    #[test]
    fn registrar_online_aors_lists_valid_bindings_only() {
        let mut r = Registrar::default();
        r.register("AD-A", "sip:AD-A@1.2.3.4".into(), None, 60);
        r.register("AD-B", "sip:AD-B@1.2.3.5".into(), None, 60);
        // 过期绑定（expires=0）不出现。
        r.register("AD-C", "c".into(), None, 0);
        let aors = r.online_aors();
        assert_eq!(aors, vec!["AD-A".to_string(), "AD-B".to_string()]);
        // 注销后不再出现。
        r.unregister("AD-A");
        assert_eq!(r.online_aors(), vec!["AD-B".to_string()]);
    }

    #[test]
    fn registrar_upsert_expire_unregister() {
        let mut r = Registrar::default();
        r.register("AD-A", "sip:AD-A@1.2.3.4".into(), None, 60);
        assert_eq!(r.len(), 1);
        assert!(r.lookup("AD-A").is_some());
        assert!(r.unregister("AD-A"));
        assert!(!r.unregister("AD-A"));
        assert_eq!(r.len(), 0);
        // 过期剔除
        r.register("AD-B", "c".into(), None, 0);
        // expires=0 立即过期
        assert!(r.lookup("AD-B").is_none());
    }

    #[test]
    fn register_no_auth_challenges() {
        let req = register_request("AD-DEV1", None, None);
        let pw = passwords();
        let d = decide_register(&req, REALM, NONCE, &|u| pw.get(u).cloned());
        match d {
            RegisterDecision::Challenge(www) => {
                assert!(www.contains("Digest realm=\"aerodesk.test\""));
                assert!(www.contains("nonce=\"testnonce123\""));
            }
            other => panic!("应质询，得 {other:?}"),
        }
    }

    #[test]
    fn register_good_and_bad_password() {
        let pw = passwords();
        let lookup = |u: &str| pw.get(u).cloned();
        let uri = format!("sip:{REALM}");

        // 正确口令 → Registered
        let good = rsipstack::dialog::authenticate::compute_digest(
            "AD-DEV1",
            "tok-dev1",
            REALM,
            NONCE,
            &Method::Register,
            &uri,
            rsipstack::sip::headers::auth::Algorithm::Md5,
            None,
        );
        let auth_hdr = format!(
            "Digest username=\"AD-DEV1\", realm=\"{REALM}\", nonce=\"{NONCE}\", uri=\"{uri}\", response=\"{good}\", algorithm=MD5"
        );
        let req = register_request("AD-DEV1", Some(&auth_hdr), Some(120));
        assert_eq!(
            decide_register(&req, REALM, NONCE, &lookup),
            RegisterDecision::Registered(120)
        );

        // 错口令 → Forbidden
        let bad = rsipstack::dialog::authenticate::compute_digest(
            "AD-DEV1",
            "WRONG",
            REALM,
            NONCE,
            &Method::Register,
            &uri,
            rsipstack::sip::headers::auth::Algorithm::Md5,
            None,
        );
        let auth_hdr = format!(
            "Digest username=\"AD-DEV1\", realm=\"{REALM}\", nonce=\"{NONCE}\", uri=\"{uri}\", response=\"{bad}\", algorithm=MD5"
        );
        let req = register_request("AD-DEV1", Some(&auth_hdr), None);
        assert_eq!(
            decide_register(&req, REALM, NONCE, &lookup),
            RegisterDecision::Forbidden
        );

        // 未知设备 → Forbidden（不泄露存在性）
        let auth_hdr = format!(
            "Digest username=\"AD-GHOST\", realm=\"{REALM}\", nonce=\"{NONCE}\", uri=\"{uri}\", response=\"{good}\", algorithm=MD5"
        );
        let req = register_request("AD-GHOST", Some(&auth_hdr), None);
        assert_eq!(
            decide_register(&req, REALM, NONCE, &lookup),
            RegisterDecision::Forbidden
        );
    }

    #[test]
    fn register_expires_zero_unregisters() {
        let pw = passwords();
        let lookup = |u: &str| pw.get(u).cloned();
        let uri = format!("sip:{REALM}");
        let resp = rsipstack::dialog::authenticate::compute_digest(
            "AD-DEV1",
            "tok-dev1",
            REALM,
            NONCE,
            &Method::Register,
            &uri,
            rsipstack::sip::headers::auth::Algorithm::Md5,
            None,
        );
        let auth_hdr = format!(
            "Digest username=\"AD-DEV1\", realm=\"{REALM}\", nonce=\"{NONCE}\", uri=\"{uri}\", response=\"{resp}\", algorithm=MD5"
        );
        let req = register_request("AD-DEV1", Some(&auth_hdr), Some(0));
        assert_eq!(
            decide_register(&req, REALM, NONCE, &lookup),
            RegisterDecision::Unregistered
        );
    }

    #[test]
    fn authorization_typed_parse_smoke() {
        let raw =
            "Digest username=\"u\", realm=\"r\", nonce=\"n\", uri=\"sip:r\", response=\"abc\"";
        let a = Authorization::parse(raw).expect("parse");
        assert_eq!(a.username, "u");
        assert_eq!(a.realm, "r");
    }

    // -- #503-4 无人值守口令授权：decide_invite 纯逻辑 --

    /// 构造带/不带 Proxy-Authorization 的 INVITE 请求（URI 与 REGISTER 同款）。
    fn invite_request(proxy_auth: Option<&str>) -> Request {
        let auth_line = match proxy_auth {
            Some(a) => format!("Proxy-Authorization: {a}\r\n"),
            None => String::new(),
        };
        let text = format!(
            "INVITE sip:AD-CALLEE@{REALM} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-t\r\n\
             From: <sip:AD-CALLER@{REALM}>;tag=f1\r\n\
             To: <sip:AD-CALLEE@{REALM}>\r\n\
             Call-ID: invite-1\r\n\
             CSeq: 1 INVITE\r\n\
             {auth_line}\
             Content-Type: application/sdp\r\n\
             Content-Length: 0\r\n\r\n"
        );
        Request::try_from(text).expect("parse INVITE")
    }

    /// 计算 Proxy-Authorization 的 Digest 响应（username = 被叫设备 ID，口令 = 被叫口令）。
    fn invite_digest(username: &str, password: &str) -> String {
        rsipstack::dialog::authenticate::compute_digest(
            username,
            password,
            REALM,
            NONCE,
            &Method::Invite,
            &format!("sip:{REALM}"),
            rsipstack::sip::headers::auth::Algorithm::Md5,
            None,
        )
    }

    #[test]
    fn invite_no_password_allows() {
        // 目标无口令（开放部署/未配置设备）→ 直接放行，不设卡。
        let req = invite_request(None);
        assert_eq!(
            decide_invite(&req, REALM, NONCE, None, None),
            InviteDecision::Allow
        );
    }

    #[test]
    fn invite_without_auth_challenges() {
        // 目标有固定口令但 INVITE 未带 Proxy-Authorization → 407 质询。
        let req = invite_request(None);
        match decide_invite(&req, REALM, NONCE, Some("tok-callee"), None) {
            InviteDecision::Challenge(pa) => {
                assert!(pa.contains("Digest realm=\"aerodesk.test\""));
                assert!(pa.contains("nonce=\"testnonce123\""));
            }
            other => panic!("应 407 质询，得 {other:?}"),
        }
    }

    #[test]
    fn invite_fixed_password_gates_call() {
        // 正确固定口令 → Allow。
        let resp = invite_digest("AD-CALLEE", "tok-callee");
        let auth_hdr = format!(
            "Digest username=\"AD-CALLEE\", realm=\"{REALM}\", nonce=\"{NONCE}\", uri=\"sip:{REALM}\", response=\"{resp}\", algorithm=MD5"
        );
        let req = invite_request(Some(&auth_hdr));
        assert_eq!(
            decide_invite(&req, REALM, NONCE, Some("tok-callee"), None),
            InviteDecision::Allow
        );
        // 口令错 → Forbidden。
        let bad = invite_digest("AD-CALLEE", "WRONG");
        let auth_hdr = format!(
            "Digest username=\"AD-CALLEE\", realm=\"{REALM}\", nonce=\"{NONCE}\", uri=\"sip:{REALM}\", response=\"{bad}\", algorithm=MD5"
        );
        let req = invite_request(Some(&auth_hdr));
        assert_eq!(
            decide_invite(&req, REALM, NONCE, Some("tok-callee"), None),
            InviteDecision::Forbidden
        );
        // 固定口令错、临时口令对 → 临时口令放行（有效期等效固定口令）。
        let req = invite_request(Some(&auth_hdr));
        assert_eq!(
            decide_invite(&req, REALM, NONCE, Some("tok-callee"), Some("temp-9X2")),
            InviteDecision::Forbidden,
            "两个都错才 403"
        );
        let temp_ok = invite_digest("AD-CALLEE", "temp-9X2");
        let auth_hdr = format!(
            "Digest username=\"AD-CALLEE\", realm=\"{REALM}\", nonce=\"{NONCE}\", uri=\"sip:{REALM}\", response=\"{temp_ok}\", algorithm=MD5"
        );
        let req = invite_request(Some(&auth_hdr));
        assert_eq!(
            decide_invite(&req, REALM, NONCE, Some("tok-callee"), Some("temp-9X2")),
            InviteDecision::Allow,
            "临时口令匹配即放行"
        );
    }

    #[test]
    fn invite_unparseable_auth_challenges() {
        let req = invite_request(Some("Digest garbage"));
        assert!(matches!(
            decide_invite(&req, REALM, NONCE, Some("tok-callee"), None),
            InviteDecision::Challenge(_)
        ));
    }

    #[test]
    fn temp_registry_issue_lookup_revoke_expire() {
        let mut r = TempRegistry::default();
        r.issue("AD-DEV1", "temp-1".into(), Duration::from_secs(60));
        assert_eq!(r.lookup("AD-DEV1").as_deref(), Some("temp-1"));
        assert_eq!(r.len(), 1);
        // 重新签发覆盖旧值。
        r.issue("AD-DEV1", "temp-2".into(), Duration::from_secs(60));
        assert_eq!(r.lookup("AD-DEV1").as_deref(), Some("temp-2"));
        // 撤销。
        assert!(r.revoke("AD-DEV1"));
        assert_eq!(r.lookup("AD-DEV1"), None);
        assert!(!r.revoke("AD-DEV1"));
        // 过期剔除。
        r.issue("AD-DEV2", "temp-3".into(), Duration::ZERO);
        assert_eq!(r.lookup("AD-DEV2"), None, "ttl=0 立即过期");
        assert_eq!(r.len(), 0);
    }

    // -- 端到端：rsipstack 客户端经 UDP 完成 REGISTER→401→REGISTER→200 --

    fn free_udp_port() -> u16 {
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        port
    }

    use aerodesk_protocol::sip_client::SipClientConfig;

    async fn udp_client(
        cancel: &CancellationToken,
        username: &str,
        password: &str,
    ) -> rsipstack::dialog::registration::Registration {
        use rsipstack::dialog::authenticate::Credential;
        use rsipstack::dialog::registration::Registration;
        use rsipstack::transport::udp::UdpConnection;

        let ctl = TransportLayer::new(cancel.clone());
        let udp = UdpConnection::create_connection(
            "127.0.0.1:0".parse().unwrap(),
            None,
            Some(cancel.clone()),
        )
        .await
        .expect("client udp");
        ctl.add_transport(SipConnection::from(udp));
        let ep = EndpointBuilder::new()
            .with_user_agent("sip-e2e-client")
            .with_transport_layer(ctl)
            .with_cancel_token(cancel.clone())
            .build();
        let inner = ep.inner.clone();
        tokio::spawn(async move {
            inner.serve().await.ok();
        });
        Registration::new(
            ep.inner.clone(),
            Some(Credential {
                username: username.into(),
                password: password.into(),
                realm: Some(REALM.into()),
            }),
        )
    }

    // 串行锁须贯穿整个测试（含 await），属有意持有。
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_register_digest_over_udp() {
        let _serial = serve_e2e_guard();
        let port = free_udp_port();
        let cancel = CancellationToken::new();

        let mut pw = HashMap::new();
        pw.insert("AD-DEV1".to_string(), "tok-dev1".to_string());
        let cfg = SipConfig {
            realm: REALM.into(),
            tls_addr: None,
            wss_addr: None,
            udp_addr: Some(format!("127.0.0.1:{port}").parse().unwrap()),
            passwords: Arc::new(pw),
            tls_identity: None,
            sfu_urls: vec![],
            sfu_token: None,
            token_password: None,
            open_register: false,
            temp_passwords: Arc::default(),
        };
        let server_cancel = cancel.clone();
        let server = tokio::spawn(async move { serve(cfg, server_cancel).await });
        tokio::time::sleep(Duration::from_millis(400)).await; // 等 listener 起来

        // 正确口令 → 200
        let mut reg = udp_client(&cancel, "AD-DEV1", "tok-dev1").await;
        let uri: rsipstack::sip::Uri = format!("sip:AD-DEV1@127.0.0.1:{port};transport=udp")
            .try_into()
            .unwrap();
        let resp = reg.register(uri, Some(60)).await.expect("register 应成功");
        assert_eq!(resp.status_code, StatusCode::OK, "正确口令应 200");

        // 错口令 → 403
        let mut bad = udp_client(&cancel, "AD-DEV1", "WRONG").await;
        let uri: rsipstack::sip::Uri = format!("sip:AD-DEV1@127.0.0.1:{port};transport=udp")
            .try_into()
            .unwrap();
        let resp = bad
            .register(uri, Some(60))
            .await
            .expect("register 应有响应");
        assert_eq!(resp.status_code, StatusCode::Forbidden, "错口令应 403");

        cancel.cancel();
        let _ = server.await;
    }

    // -- 端到端 slice 2：UAC 经透明 Proxy 呼叫 UAS，INVITE→180→200→ACK→BYE --

    const CALLER_SDP: &str =
        "v=0\r\no=caller 1 1 IN IP4 127.0.0.1\r\ns=call\r\nt=0 0\r\nm=video 5004 RTP/AVP 96\r\n";
    const CALLEE_SDP: &str =
        "v=0\r\no=callee 2 2 IN IP4 127.0.0.1\r\ns=call\r\nt=0 0\r\nm=video 6004 RTP/AVP 96\r\n";

    /// 构建一个 UDP UA：endpoint（serve 已起）+ DialogLayer。
    async fn build_ua(
        cancel: &CancellationToken,
    ) -> (Arc<rsipstack::transaction::Endpoint>, Arc<DialogLayer>) {
        use rsipstack::transport::udp::UdpConnection;
        let ctl = TransportLayer::new(cancel.clone());
        let udp = UdpConnection::create_connection(
            "127.0.0.1:0".parse().unwrap(),
            None,
            Some(cancel.clone()),
        )
        .await
        .expect("ua udp");
        ctl.add_transport(SipConnection::from(udp));
        let ep = EndpointBuilder::new()
            .with_user_agent("e2e-ua")
            .with_transport_layer(ctl)
            .with_cancel_token(cancel.clone())
            .build();
        let inner = ep.inner.clone();
        tokio::spawn(async move {
            inner.serve().await.ok();
        });
        let dl = Arc::new(DialogLayer::new(ep.inner.clone()));
        (Arc::new(ep), dl)
    }

    async fn ua_register(
        ep: &rsipstack::transaction::Endpoint,
        port: u16,
        user: &str,
        token: &str,
    ) {
        use rsipstack::dialog::authenticate::Credential;
        use rsipstack::dialog::registration::Registration;
        let mut reg = Registration::new(
            ep.inner.clone(),
            Some(Credential {
                username: user.into(),
                password: token.into(),
                realm: Some(REALM.into()),
            }),
        );
        let uri: rsipstack::sip::Uri = format!("sip:127.0.0.1:{port};transport=udp")
            .try_into()
            .unwrap();
        let r = reg.register(uri, Some(3600)).await.expect("register");
        assert_eq!(r.status_code, StatusCode::OK, "{user} 注册应 200");
    }

    // 串行锁须贯穿整个测试（含 await），属有意持有。
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn end_to_end_invite_through_proxy() {
        let _serial = serve_e2e_guard();
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
        let port = free_udp_port();
        let cancel = CancellationToken::new();
        let mut pw = HashMap::new();
        pw.insert("AD-CALLER".to_string(), "tok-caller".to_string());
        pw.insert("AD-CALLEE".to_string(), "tok-callee".to_string());
        let cfg = SipConfig {
            realm: REALM.into(),
            tls_addr: None,
            wss_addr: None,
            udp_addr: Some(format!("127.0.0.1:{port}").parse().unwrap()),
            passwords: Arc::new(pw),
            tls_identity: None,
            sfu_urls: vec![],
            sfu_token: None,
            token_password: None,
            open_register: false,
            temp_passwords: Arc::default(),
        };
        let sc = cancel.clone();
        let server = tokio::spawn(async move { serve(cfg, sc).await });
        tokio::time::sleep(Duration::from_millis(400)).await;

        // ---- 被叫 UAS：注册 + 自动 180→200 应答 ----
        let (callee_ep, callee_dl) = build_ua(&cancel).await;
        ua_register(&callee_ep, port, "AD-CALLEE", "tok-callee").await;
        let mut callee_inc = callee_ep.incoming_transactions().unwrap();
        {
            let dl = callee_dl.clone();
            let (stx, _srx) = dl.new_dialog_state_channel();
            tokio::spawn(async move {
                while let Some(mut tx) = callee_inc.recv().await {
                    match tx.original.method {
                        Method::Invite => {
                            let dlg = dl
                                .get_or_create_server_invite(&tx, stx.clone(), None, None)
                                .unwrap();
                            let mut d = dlg.clone();
                            tokio::spawn(async move {
                                let _ = d.handle(&mut tx).await;
                            });
                            dlg.ringing(None, None).ok();
                            tokio::time::sleep(Duration::from_millis(30)).await;
                            let ct = Header::Other("Content-Type".into(), "application/sdp".into());
                            dlg.accept(Some(vec![ct]), Some(CALLEE_SDP.as_bytes().to_vec()))
                                .ok();
                        }
                        _ => {
                            if let Some(mut d) = dl.match_dialog(&tx) {
                                tokio::spawn(async move {
                                    let _ = d.handle(&mut tx).await;
                                });
                            }
                        }
                    }
                }
            });
        }

        // ---- 主叫 UAC：注册 + 经 proxy INVITE 被叫 ----
        let (caller_ep, caller_dl) = build_ua(&cancel).await;
        ua_register(&caller_ep, port, "AD-CALLER", "tok-caller").await;
        let (stx, _srx) = caller_dl.new_dialog_state_channel();
        let sa: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut proxy_addr = SipAddr::from(sa);
        proxy_addr.r#type = Some(rsipstack::sip::Transport::Udp);
        let opt = InviteOption {
            caller: format!("sip:AD-CALLER@{REALM}").try_into().unwrap(),
            callee: format!("sip:AD-CALLEE@{REALM}").try_into().unwrap(),
            destination: Some(proxy_addr),
            content_type: Some("application/sdp".into()),
            offer: Some(CALLER_SDP.as_bytes().to_vec()),
            contact: caller_dl.build_local_contact(None, None).unwrap(),
            call_id: Some("e2e-call-1".into()),
            // #503-4：被叫有固定口令 → 407 质询时以被叫口令应答（否则 407 拒绝）。
            credential: Some(rsipstack::dialog::authenticate::Credential {
                username: "AD-CALLEE".into(),
                password: "tok-callee".into(),
                realm: Some(REALM.into()),
            }),
            ..Default::default()
        };
        let (dlg, resp) = caller_dl.do_invite(opt, stx).await.expect("invite 应完成");
        let resp = resp.expect("应有 final response");
        assert_eq!(resp.status_code, StatusCode::OK, "呼叫应 200");
        // SDP 端到端透传：proxy 不解析不修改。
        assert_eq!(
            String::from_utf8_lossy(&resp.body),
            CALLEE_SDP,
            "SDP answer 应端到端一致"
        );

        // BYE：主叫挂断 → proxy 级联到被叫。
        dlg.bye().await.ok();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 指标：恰好一次建立、一次结束。
        let (regs, est, term) = metrics_snapshot().expect("metrics");
        assert_eq!(regs, 2, "两个 UA 在线");
        assert_eq!(est, 1, "一次呼叫建立");
        assert_eq!(term, 1, "一次呼叫结束");

        cancel.cancel();
        let _ = server.await;
    }

    // -- 端到端 #552 slice 12：会议 AoR INVITE → SFU 桥 --

    /// 非设备 AoR（会议房间名）INVITE → mock SFU /start → answer 端到端透传；
    /// 离线设备（AD-*）与非法房间名仍 404。
    #[test]
    fn end_to_end_conference_invite_bridged_to_sfu() {
        let _serial = serve_e2e_guard();
        // mock SFU：接受一次 POST /start，校验 room/role 透传，回固定 answer。
        let sfu = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let sfu_port = sfu.local_addr().unwrap().port();
        let mock = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = sfu.accept().expect("sfu accept");
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            // 读满请求再应答（Content-Length 或对端写完关闭）：单次 read 后立即
            // 关闭会对仍在写请求体的客户端发 RST——ureq 报连接错误 → 桥 503
            // （CI 上偶现，#576 run 32636138388）。
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match s.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf);
                        if let Some(cl) = text
                            .split(
                                "

",
                            )
                            .next()
                            .and_then(|head| {
                                head.lines().find_map(|l| {
                                    l.strip_prefix("Content-Length:")
                                        .map(|v| v.trim().to_string())
                                })
                            })
                            .and_then(|v| v.parse::<usize>().ok())
                            && buf.len()
                                >= text
                                    .find(
                                        "

",
                                    )
                                    .map(|i| i + 4)
                                    .unwrap_or(0)
                                    + cl
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let req = String::from_utf8_lossy(&buf).to_string();
            assert!(
                req.contains("/start?room=meet-123&role=viewer"),
                "room/role 应透传 SFU：{req}"
            );
            let body = r#"{"type":"answer","sdp":"v=0\r\nmock-sfu-answer\r\n"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            s.write_all(resp.as_bytes()).unwrap();
            use std::net::Shutdown;
            let _ = s.shutdown(Shutdown::Write);
            let mut drain = [0u8; 512];
            loop {
                match s.read(&mut drain) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });

        let port = free_udp_port();
        let mut pw = HashMap::new();
        pw.insert("AD-CALLER".to_string(), "tok-caller".to_string());
        let cfg = SipConfig {
            realm: REALM.into(),
            tls_addr: None,
            wss_addr: None,
            udp_addr: Some(format!("127.0.0.1:{port}").parse().unwrap()),
            passwords: Arc::new(pw),
            tls_identity: None,
            sfu_urls: vec![format!("http://127.0.0.1:{sfu_port}")],
            sfu_token: None,
            token_password: None,
            open_register: false,
            temp_passwords: Arc::default(),
        };
        let server_cancel = CancellationToken::new();
        let sc = server_cancel.clone();
        let server = std::thread::spawn(move || run_sip_endpoint(cfg, sc));
        std::thread::sleep(Duration::from_millis(400));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cancel = CancellationToken::new();
            let (ep, dl) = build_ua(&cancel).await;
            ua_register(&ep, port, "AD-CALLER", "tok-caller").await;
            let (stx, _srx) = dl.new_dialog_state_channel();
            let sa: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            let mut proxy_addr = SipAddr::from(sa);
            proxy_addr.r#type = Some(rsipstack::sip::Transport::Udp);
            let opt = InviteOption {
                caller: format!("sip:AD-CALLER@{REALM}").try_into().unwrap(),
                callee: format!("sip:meet-123@{REALM}").try_into().unwrap(),
                destination: Some(proxy_addr),
                content_type: Some("application/sdp".into()),
                offer: Some(CALLER_SDP.as_bytes().to_vec()),
                contact: dl.build_local_contact(None, None).unwrap(),
                call_id: Some("e2e-conf-1".into()),
                ..Default::default()
            };
            let (dlg, resp) = dl.do_invite(opt, stx).await.expect("会议 INVITE 应完成");
            let resp = resp.expect("应有 final response");
            assert_eq!(resp.status_code, StatusCode::OK, "会议桥应回 200");
            assert_eq!(
                String::from_utf8_lossy(&resp.body),
                r#"{"type":"answer","sdp":"v=0\r\nmock-sfu-answer\r\n"}"#,
                "SFU answer 应端到端透传"
            );
            dlg.bye().await.ok();

            // 离线设备（AD-* 无绑定）→ 404（规范 §3 offline）。
            let (stx2, _srx2) = dl.new_dialog_state_channel();
            let mut proxy_addr2 = SipAddr::from(sa);
            proxy_addr2.r#type = Some(rsipstack::sip::Transport::Udp);
            let opt2 = InviteOption {
                caller: format!("sip:AD-CALLER@{REALM}").try_into().unwrap(),
                callee: format!("sip:AD-GHOST@{REALM}").try_into().unwrap(),
                destination: Some(proxy_addr2),
                content_type: Some("application/sdp".into()),
                offer: Some(CALLER_SDP.as_bytes().to_vec()),
                contact: dl.build_local_contact(None, None).unwrap(),
                call_id: Some("e2e-conf-2".into()),
                ..Default::default()
            };
            let (_, resp2) = dl
                .do_invite(opt2, stx2)
                .await
                .expect("离线 INVITE 应有 final response");
            let resp2 = resp2.expect("final");
            assert_eq!(resp2.status_code, StatusCode::NotFound, "离线设备应 404");
            cancel.cancel();
        });

        mock.join().expect("mock sfu 线程");
        server_cancel.cancel();
        let _ = server.join();
    }

    /// §8 迁移期同一凭据：未列设备以首个 AUTH_TOKEN 为 Digest 口令——
    /// 服务器无需 SIP_DIGEST_USERS 逐设备配置即可承接存量 token 客户端。
    #[test]
    fn register_token_password_fallback() {
        let pw = passwords();
        let shared = "tok-shared".to_string();
        let lookup = move |u: &str| pw.get(u).cloned().or_else(|| Some(shared.clone()));
        let uri = format!("sip:{REALM}");
        let resp = rsipstack::dialog::authenticate::compute_digest(
            "AD-ANYONE",
            "tok-shared",
            REALM,
            NONCE,
            &Method::Register,
            &uri,
            rsipstack::sip::headers::auth::Algorithm::Md5,
            None,
        );
        let auth_hdr = format!(
            "Digest username=\"AD-ANYONE\", realm=\"{REALM}\", nonce=\"{NONCE}\", uri=\"{uri}\", response=\"{resp}\", algorithm=MD5"
        );
        // 用户名任意 + 口令=共享 token → Registered。
        let req = register_request("AD-ANYONE", Some(&auth_hdr), None);
        assert_eq!(
            decide_register(&req, REALM, NONCE, &lookup),
            RegisterDecision::Registered(DEFAULT_EXPIRES_SECS)
        );
        // 错口令 → Forbidden。
        let bad = auth_hdr.replace(&resp, "deadbeef");
        let req2 = register_request("AD-ANYONE", Some(&bad), None);
        assert_eq!(
            decide_register(&req2, REALM, NONCE, &lookup),
            RegisterDecision::Forbidden
        );
    }

    /// #576 回归：会议桥 SFU 失败必须回 final response（503）——修复前
    /// INVITE 事务悬死（客户端无 Answered/Rejected，UI 静默卡住）。
    #[test]
    fn end_to_end_conference_invite_sfu_down_replies_503() {
        let _serial = serve_e2e_guard();
        // mock SFU 立即拒绝连接（bind 后关 listener）。
        let sfu = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let sfu_port = sfu.local_addr().unwrap().port();
        drop(sfu); // 端口无人监听 → connect refused

        let port = free_udp_port();
        let mut pw = HashMap::new();
        pw.insert("AD-CALLER".to_string(), "tok-caller".to_string());
        let cfg = SipConfig {
            realm: REALM.into(),
            tls_addr: None,
            wss_addr: None,
            udp_addr: Some(format!("127.0.0.1:{port}").parse().unwrap()),
            passwords: Arc::new(pw),
            tls_identity: None,
            sfu_urls: vec![format!("http://127.0.0.1:{sfu_port}")],
            sfu_token: None,
            token_password: None,
            open_register: false,
            temp_passwords: Arc::default(),
        };
        let server_cancel = CancellationToken::new();
        let sc = server_cancel.clone();
        let server = std::thread::spawn(move || run_sip_endpoint(cfg, sc));
        std::thread::sleep(Duration::from_millis(400));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let cancel = CancellationToken::new();
            let (ep, dl) = build_ua(&cancel).await;
            ua_register(&ep, port, "AD-CALLER", "tok-caller").await;
            let (stx, _srx) = dl.new_dialog_state_channel();
            let sa: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            let mut proxy_addr = SipAddr::from(sa);
            proxy_addr.r#type = Some(rsipstack::sip::Transport::Udp);
            let opt = InviteOption {
                caller: format!("sip:AD-CALLER@{REALM}").try_into().unwrap(),
                callee: format!("sip:meet-down@{REALM}").try_into().unwrap(),
                destination: Some(proxy_addr),
                content_type: Some("application/sdp".into()),
                offer: Some(CALLER_SDP.as_bytes().to_vec()),
                contact: dl.build_local_contact(None, None).unwrap(),
                call_id: Some("e2e-conf-503".into()),
                ..Default::default()
            };
            // do_invite 在非 2xx final 时返回 Err（含响应码）——收到任何
            // final（而非事务超时悬死）即为修复生效。
            match dl.do_invite(opt, stx).await {
                Err(e) => {
                    assert!(
                        e.to_string().contains("503")
                            || e.to_string().contains("Service")
                            || e.to_string().contains("Unavail"),
                        "应 503，实得：{e}"
                    );
                }
                Ok((_, Some(resp))) => {
                    assert_eq!(
                        resp.status_code,
                        StatusCode::ServiceUnavailable,
                        "应 503，实得 {}",
                        resp.status_code
                    );
                }
                Ok((_, None)) => panic!("不应无 final response（悬死回归）"),
            }
            cancel.cancel();
        });

        server_cancel.cancel();
        let _ = server.join();
    }

    // -- 端到端 #552 slice 1：SipClient UA（protocol 侧）注册到本服务端 --

    #[test]
    fn end_to_end_sip_client_register_and_bad_password() {
        let _serial = serve_e2e_guard();
        use aerodesk_protocol::sip_client::{
            SipClientConfig, SipEvent, SipTransport, start_sip_client,
        };

        let port = free_udp_port();
        let mut pw = HashMap::new();
        pw.insert("AD-C1".to_string(), "tok-c1".to_string());
        let cfg = SipConfig {
            realm: REALM.into(),
            tls_addr: None,
            wss_addr: None,
            udp_addr: Some(format!("127.0.0.1:{port}").parse().unwrap()),
            passwords: Arc::new(pw),
            tls_identity: None,
            sfu_urls: vec![],
            sfu_token: None,
            token_password: None,
            open_register: false,
            temp_passwords: Arc::default(),
        };
        let server_cancel = CancellationToken::new();
        let sc = server_cancel.clone();
        let server = std::thread::spawn(move || run_sip_endpoint(cfg, sc));
        std::thread::sleep(Duration::from_millis(400)); // 等 listener 起来

        let client_cfg = |device: &str, password: &str| SipClientConfig {
            device_id: device.into(),
            domain: REALM.into(),
            password: password.into(),
            server: format!("127.0.0.1:{port}").parse().unwrap(),
            transport: SipTransport::Udp,
            tls: None,
            register_expires: 60,
        };

        // 正确口令 → Registered（AoR = sip:<device>@<domain>）。
        let good = start_sip_client(client_cfg("AD-C1", "tok-c1")).expect("client 启动");
        let ev = good
            .recv_event(Duration::from_secs(5))
            .expect("应收到注册结果事件");
        match ev {
            SipEvent::Registered { aor, expires } => {
                assert_eq!(aor, format!("sip:AD-C1@{REALM}"));
                assert!(expires > 0);
            }
            other => panic!("应 Registered，实际 {other:?}"),
        }

        // 错口令 → RegisterFailed(403)（服务端不泄露设备存在性，统一 403）。
        let bad = start_sip_client(client_cfg("AD-C1", "WRONG")).expect("client 启动");
        let ev = bad
            .recv_event(Duration::from_secs(5))
            .expect("应收到注册结果事件");
        match ev {
            SipEvent::RegisterFailed { status, .. } => {
                assert_eq!(status, 403, "错口令应 403")
            }
            other => panic!("应 RegisterFailed，实际 {other:?}"),
        }

        // 关停：注销 + join，不悬挂。
        good.shutdown();
        bad.shutdown();
        server_cancel.cancel();
        server
            .join()
            .expect("server 线程应退出")
            .expect("server 应正常退出");
    }

    // -- 端到端 #552 slice 2/3：双 SipClient 经透明 Proxy 的全呼叫面 --
    // INVITE→180→200(SDP 透传)→INFO trickle→BYE；拒接 486；302 升级；
    // BYE cause=302 升级（抑制 PeerHangup）。同一场景在 UDP 与 TLS 传输各跑一遍
    // （slice 3：TLS=公网默认加密传输，客户端无监听，全呼叫面沿注册既有流）。

    /// 用 rcgen 现生成测试 CA + 服务端 EE 证书（SAN：DNS aerodesk.test + IP
    /// 127.0.0.1）。不用内嵌开发证书（自签 CA:TRUE 端实体——rustls 0.23 webpki
    /// 拒绝 caUsedAsEndEntity），也不用 openssl CLI（非 Windows 可移植）。
    /// 返回（服务端身份, CA PEM）。
    fn test_tls_material() -> (TlsIdentity, Vec<u8>) {
        use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};

        let ca_key = KeyPair::generate().expect("CA key");
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "aerodesk.test test CA");
        let ca = ca_params.self_signed(&ca_key).expect("CA cert");

        let ee_key = KeyPair::generate().expect("EE key");
        // 字符串自动识别：IP → SanType::IpAddress，域名 → SanType::DnsName。
        let ee_params =
            CertificateParams::new(vec!["aerodesk.test".to_string(), "127.0.0.1".to_string()])
                .expect("EE params");
        let ee = ee_params
            .signed_by(&ee_key, &Issuer::from_params(&ca_params, &ca_key))
            .expect("EE cert");

        let identity = TlsIdentity {
            cert: ee.pem().into_bytes(),
            key: ee_key.serialize_pem().into_bytes(),
            source: "test-rcgen",
        };
        (identity, ca.pem().into_bytes())
    }

    fn free_tcp_port() -> u16 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        port
    }

    /// 全呼叫面场景主体（传输无关）：启动 server（监听差异由 `server_cfg` 注入）、
    /// 双客户端注册、呼叫 1-4（全流程/拒接/302/BYE-302）、关停。
    /// 事件携带的 Call-ID（Registered/EndpointStopped 无）。
    fn call_id_of(ev: &aerodesk_protocol::sip_client::SipEvent) -> Option<&str> {
        use aerodesk_protocol::sip_client::SipEvent;
        match ev {
            SipEvent::IncomingCall { call_id, .. }
            | SipEvent::Ringing { call_id }
            | SipEvent::Answered { call_id, .. }
            | SipEvent::Rejected { call_id, .. }
            | SipEvent::PeerHangup { call_id, .. }
            | SipEvent::EscalatedToSfu { call_id, .. }
            | SipEvent::RedirectedTo { call_id, .. }
            | SipEvent::Trickle { call_id, .. } => Some(call_id),
            SipEvent::Registered { .. }
            | SipEvent::RegisterFailed { .. }
            | SipEvent::EndpointStopped => None,
        }
    }

    /// 收事件（跳过非目标种类与 call_id 不匹配的事件，30s 兜底）。模块级：
    /// 呼叫面场景与 P2P 媒体 e2e 共用（近重复不可留两处，改事件集只改这一处）。
    /// `want_call_id`：全部 UA 事件共用一条通道，多步/多呼叫测试在 CI 调度
    /// 抖动下可能收到跨步骤的迟到事件——种类命中后再按 call_id 过滤，
    /// 防误交付把断言引向错误状态；不匹配事件记入诊断序列，超时时一并带出
    /// （把「静默超时」变成可定位日志）。
    fn recv_until(
        h: &aerodesk_protocol::sip_client::SipClientHandle,
        want: &str,
        want_call_id: Option<&str>,
    ) -> aerodesk_protocol::sip_client::SipEvent {
        use aerodesk_protocol::sip_client::SipEvent;
        // #553 验收前置：窗口从 5s 放宽到 15s——TLS 传输（呼叫 + 302 升级）在
        // CI 慢 runner 偶发超 5s/15s（本地 0.5s 内完成；UDP 302 升级链路在
        // 负载 runner 上可超 15s）；测试为顺序等待，放宽无损。
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        // 已收到但不匹配的事件序列（种类#call_id）：超时诊断用。
        let mut seen_mismatch: Vec<String> = Vec::new();
        loop {
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remain.is_zero(),
                "等 {want}（call_id={want_call_id:?}）超时；已收到但不匹配：{seen_mismatch:?}"
            );
            // None 两种含义：窗口内超时（重试至 deadline）或对端通道关闭（UA 已停）。
            let Some(ev) = h.recv_event(remain) else {
                panic!(
                    "等 {want}（call_id={want_call_id:?}）：窗口内无事件或 UA 通道已关闭；已收到但不匹配：{seen_mismatch:?}"
                );
            };
            let hit = matches!(
                (&ev, want),
                (SipEvent::Registered { .. }, "Registered")
                    | (SipEvent::IncomingCall { .. }, "IncomingCall")
                    | (SipEvent::Ringing { .. }, "Ringing")
                    | (SipEvent::Answered { .. }, "Answered")
                    | (SipEvent::Rejected { .. }, "Rejected")
                    | (SipEvent::PeerHangup { .. }, "PeerHangup")
                    | (SipEvent::EscalatedToSfu { .. }, "EscalatedToSfu")
                    | (SipEvent::Trickle { .. }, "Trickle")
            );
            if !hit {
                continue;
            }
            // 种类命中后再比对 call_id（Registered/EndpointStopped 无 call_id）。
            match (want_call_id, call_id_of(&ev)) {
                (Some(want_id), Some(got_id)) if got_id != want_id => {
                    seen_mismatch.push(format!("{want}#{got_id}"));
                    continue;
                }
                _ => return ev,
            }
        }
    }

    fn run_call_plane_scenario(
        server_cfg: SipConfig,
        client_cfg: &dyn Fn(&str) -> SipClientConfig,
    ) {
        use aerodesk_protocol::sip::{ESCALATE_BYE_REASON, TrickleCandidate};
        use aerodesk_protocol::sip_client::{SipCommand, SipEvent, start_sip_client};

        let server_cancel = CancellationToken::new();
        let sc = server_cancel.clone();
        let server = std::thread::spawn(move || run_sip_endpoint(server_cfg, sc));
        std::thread::sleep(Duration::from_millis(400));

        let caller = start_sip_client(client_cfg("AD-CALLER")).expect("caller 启动");
        let callee = start_sip_client(client_cfg("AD-CALLEE")).expect("callee 启动");

        assert!(matches!(
            recv_until(&caller, "Registered", None),
            SipEvent::Registered { .. }
        ));
        assert!(matches!(
            recv_until(&callee, "Registered", None),
            SipEvent::Registered { .. }
        ));

        // ---- 呼叫 1：全流程 ----
        caller
            .send(SipCommand::Call {
                target_device: "AD-CALLEE".into(),
                call_id: "c-1".into(),
                offer_sdp: CALLER_SDP.into(),
                call_password: Some("tok-callee".into()),
            })
            .unwrap();
        match recv_until(&callee, "IncomingCall", Some("c-1")) {
            SipEvent::IncomingCall {
                call_id,
                from_device,
                offer_sdp,
            } => {
                assert_eq!(call_id, "c-1");
                assert_eq!(from_device, "AD-CALLER");
                assert_eq!(offer_sdp, CALLER_SDP, "offer 应端到端字节一致");
            }
            _ => unreachable!(),
        }
        callee
            .send(SipCommand::Ring {
                call_id: "c-1".into(),
            })
            .unwrap();
        assert!(
            matches!(recv_until(&caller, "Ringing", Some("c-1")), SipEvent::Ringing { ref call_id } if call_id == "c-1")
        );
        callee
            .send(SipCommand::Accept {
                call_id: "c-1".into(),
                answer_sdp: CALLEE_SDP.into(),
            })
            .unwrap();
        match recv_until(&caller, "Answered", Some("c-1")) {
            SipEvent::Answered {
                call_id,
                answer_sdp,
                ..
            } => {
                assert_eq!(call_id, "c-1");
                assert_eq!(answer_sdp, CALLEE_SDP, "answer 应端到端字节一致");
            }
            _ => unreachable!(),
        }
        // trickle：主叫 → 被叫（INFO sdpfrag）。
        caller
            .send(SipCommand::SendTrickle {
                call_id: "c-1".into(),
                candidate: TrickleCandidate {
                    candidate: "candidate:1 1 UDP 2130706431 192.0.2.1 5000 typ host".into(),
                    sdp_mid: Some("0".into()),
                    sdp_m_line_index: Some(0),
                },
            })
            .unwrap();
        match recv_until(&callee, "Trickle", Some("c-1")) {
            SipEvent::Trickle {
                call_id, candidate, ..
            } => {
                assert_eq!(call_id, "c-1");
                assert_eq!(candidate.sdp_mid.as_deref(), Some("0"));
                assert!(candidate.candidate.contains("192.0.2.1 5000"));
            }
            _ => unreachable!(),
        }
        // 被叫挂断 → 主叫 PeerHangup。
        callee
            .send(SipCommand::Hangup {
                call_id: "c-1".into(),
                reason: None,
            })
            .unwrap();
        assert!(
            matches!(recv_until(&caller, "PeerHangup", Some("c-1")), SipEvent::PeerHangup { ref call_id, .. } if call_id == "c-1")
        );

        // ---- 呼叫 2：拒接（busy → 486）----
        caller
            .send(SipCommand::Call {
                target_device: "AD-CALLEE".into(),
                call_id: "c-2".into(),
                offer_sdp: CALLER_SDP.into(),
                call_password: Some("tok-callee".into()),
            })
            .unwrap();
        let _ = recv_until(&callee, "IncomingCall", Some("c-2"));
        callee
            .send(SipCommand::Reject {
                call_id: "c-2".into(),
                error_code: "busy".into(),
            })
            .unwrap();
        match recv_until(&caller, "Rejected", Some("c-2")) {
            SipEvent::Rejected {
                call_id,
                status,
                error_code,
            } => {
                assert_eq!(call_id, "c-2");
                assert_eq!(status, 486);
                assert_eq!(error_code.as_deref(), Some("busy"));
            }
            _ => unreachable!(),
        }

        // ---- 呼叫 3：302 升级重定向（§4.1）----
        caller
            .send(SipCommand::Call {
                target_device: "AD-CALLEE".into(),
                call_id: "c-3".into(),
                offer_sdp: CALLER_SDP.into(),
                call_password: Some("tok-callee".into()),
            })
            .unwrap();
        let _ = recv_until(&callee, "IncomingCall", Some("c-3"));
        callee
            .send(SipCommand::RedirectToSfu {
                call_id: "c-3".into(),
            })
            .unwrap();
        match recv_until(&caller, "EscalatedToSfu", Some("c-3")) {
            SipEvent::EscalatedToSfu { call_id, view_aor } => {
                assert_eq!(call_id, "c-3");
                assert_eq!(view_aor, format!("sip:view-AD-CALLEE@{REALM}"));
            }
            _ => unreachable!(),
        }

        // ---- 呼叫 4：已建立后 BYE cause=302 升级（抑制 PeerHangup）----
        caller
            .send(SipCommand::Call {
                target_device: "AD-CALLEE".into(),
                call_id: "c-4".into(),
                offer_sdp: CALLER_SDP.into(),
                call_password: Some("tok-callee".into()),
            })
            .unwrap();
        let _ = recv_until(&callee, "IncomingCall", Some("c-4"));
        callee
            .send(SipCommand::Accept {
                call_id: "c-4".into(),
                answer_sdp: CALLEE_SDP.into(),
            })
            .unwrap();
        let _ = recv_until(&caller, "Answered", Some("c-4"));
        callee
            .send(SipCommand::Hangup {
                call_id: "c-4".into(),
                reason: Some(ESCALATE_BYE_REASON.into()),
            })
            .unwrap();
        match recv_until(&caller, "EscalatedToSfu", Some("c-4")) {
            SipEvent::EscalatedToSfu { call_id, view_aor } => {
                assert_eq!(call_id, "c-4");
                assert_eq!(view_aor, format!("sip:view-AD-CALLEE@{REALM}"));
            }
            _ => unreachable!(),
        }

        caller.shutdown();
        callee.shutdown();
        server_cancel.cancel();
        server
            .join()
            .expect("server 线程应退出")
            .expect("server 应正常退出");
    }

    #[test]
    fn end_to_end_sip_client_call_plane() {
        let _serial = serve_e2e_guard();
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
        use aerodesk_protocol::sip_client::{SipClientConfig, SipTransport};

        let port = free_udp_port();
        let mut pw = HashMap::new();
        pw.insert("AD-CALLER".to_string(), "tok-caller".to_string());
        pw.insert("AD-CALLEE".to_string(), "tok-callee".to_string());
        let cfg = SipConfig {
            realm: REALM.into(),
            tls_addr: None,
            wss_addr: None,
            udp_addr: Some(format!("127.0.0.1:{port}").parse().unwrap()),
            passwords: Arc::new(pw),
            tls_identity: None,
            sfu_urls: vec![],
            sfu_token: None,
            token_password: None,
            open_register: false,
            temp_passwords: Arc::default(),
        };
        let client_cfg = |device: &str| SipClientConfig {
            device_id: device.into(),
            domain: REALM.into(),
            password: format!("tok-{}", device.trim_start_matches("AD-").to_lowercase()),
            server: format!("127.0.0.1:{port}").parse().unwrap(),
            transport: SipTransport::Udp,
            tls: None,
            register_expires: 60,
        };
        run_call_plane_scenario(cfg, &client_cfg);
    }

    /// slice 3：同一全呼叫面场景走 TLS（公网默认传输）。服务端仅开 TLS 监听，
    /// 客户端无监听、复用注册建立的既出流。
    #[test]
    fn end_to_end_sip_client_call_plane_tls() {
        let _serial = serve_e2e_guard();
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
        use aerodesk_protocol::sip_client::{SipClientConfig, SipTlsConfig, SipTransport};

        let port = free_tcp_port();
        let (identity, ca_pem) = test_tls_material();
        let mut pw = HashMap::new();
        pw.insert("AD-CALLER".to_string(), "tok-caller".to_string());
        pw.insert("AD-CALLEE".to_string(), "tok-callee".to_string());
        let cfg = SipConfig {
            realm: REALM.into(),
            tls_addr: Some(format!("127.0.0.1:{port}").parse().unwrap()),
            wss_addr: None,
            udp_addr: None,
            passwords: Arc::new(pw),
            tls_identity: Some(identity),
            sfu_urls: vec![],
            sfu_token: None,
            token_password: None,
            open_register: false,
            temp_passwords: Arc::default(),
        };
        let ca_pem_cfg = ca_pem.clone();
        let client_cfg = |device: &str| SipClientConfig {
            device_id: device.into(),
            domain: REALM.into(),
            password: format!("tok-{}", device.trim_start_matches("AD-").to_lowercase()),
            server: format!("127.0.0.1:{port}").parse().unwrap(),
            transport: SipTransport::Tls,
            tls: Some(SipTlsConfig {
                ca_certs: ca_pem_cfg.clone(),
                sni_hostname: Some("aerodesk.test".into()),
                client_cert: None,
                client_key: None,
            }),
            register_expires: 60,
        };
        run_call_plane_scenario(cfg, &client_cfg);
    }

    /// slice 5：SIP 信令承载 1:1 P2P 媒体——两 UA 经 INVITE/200 交换真实
    /// str0m offer/answer（协商对象是对端，不经 SFU），双侧 ICE 建链，
    /// PCMU 载荷直达主叫（DTLS/SRTP 贯通）。
    #[test]
    fn end_to_end_sip_client_p2p_media() {
        let _serial = serve_e2e_guard();
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
        use aerodesk_core::p2p_call::{P2pCall, P2pCallConfig, P2pRole};
        use aerodesk_core::protocol::signal::Role;
        use aerodesk_protocol::sip_client::{
            SipClientConfig, SipCommand, SipEvent, SipTransport, start_sip_client,
        };

        let port = free_udp_port();
        let mut pw = HashMap::new();
        pw.insert("AD-CALLER".to_string(), "tok-caller".to_string());
        pw.insert("AD-CALLEE".to_string(), "tok-callee".to_string());
        let server_cancel = CancellationToken::new();
        let sc = server_cancel.clone();
        let server = std::thread::spawn(move || {
            run_sip_endpoint(
                SipConfig {
                    realm: REALM.into(),
                    tls_addr: None,
                    wss_addr: None,
                    udp_addr: Some(format!("127.0.0.1:{port}").parse().unwrap()),
                    passwords: Arc::new(pw),
                    tls_identity: None,
                    sfu_urls: vec![],
                    sfu_token: None,
                    token_password: None,
                    open_register: false,
                    temp_passwords: Arc::default(),
                },
                sc,
            )
        });
        std::thread::sleep(Duration::from_millis(400));

        let client_cfg = |device: &str| SipClientConfig {
            device_id: device.into(),
            domain: REALM.into(),
            password: format!("tok-{}", device.trim_start_matches("AD-").to_lowercase()),
            server: format!("127.0.0.1:{port}").parse().unwrap(),
            transport: SipTransport::Udp,
            tls: None,
            register_expires: 60,
        };
        let caller = start_sip_client(client_cfg("AD-CALLER")).expect("caller 启动");
        let callee = start_sip_client(client_cfg("AD-CALLEE")).expect("callee 启动");
        assert!(matches!(
            recv_until(&caller, "Registered", None),
            SipEvent::Registered { .. }
        ));
        assert!(matches!(
            recv_until(&callee, "Registered", None),
            SipEvent::Registered { .. }
        ));

        // 媒体端双侧：主叫=Viewer（收流），被叫=Publisher（发流）；回环直连。
        let media_cfg = |role: P2pRole, device_role: Role| P2pCallConfig {
            role,
            device_role,
            codec: None,
            with_audio: true,
            with_camera: false,
            force_relay: false,
            bind: "127.0.0.1:0".parse().unwrap(),
            turn: None,
            inline_candidates: true,
        };
        let mut caller_media =
            P2pCall::new(media_cfg(P2pRole::Caller, Role::Viewer)).expect("主叫媒体端");
        let mut callee_media =
            P2pCall::new(media_cfg(P2pRole::Callee, Role::Publisher)).expect("被叫媒体端");

        let offer = caller_media.create_offer().expect("主叫 offer");
        caller
            .send(SipCommand::Call {
                target_device: "AD-CALLEE".into(),
                call_id: "c-p2p".into(),
                offer_sdp: offer.sdp.clone(),
                call_password: Some("tok-callee".into()),
            })
            .unwrap();
        // 被叫收到 offer（端到端字节一致），出 answer。
        let SipEvent::IncomingCall { offer_sdp, .. } =
            recv_until(&callee, "IncomingCall", Some("c-p2p"))
        else {
            unreachable!()
        };
        assert_eq!(offer_sdp, offer.sdp, "P2P offer 应经 SIP 端到端字节一致");
        let answer = callee_media.accept_offer(&offer_sdp).expect("被叫 answer");
        callee
            .send(SipCommand::Accept {
                call_id: "c-p2p".into(),
                answer_sdp: answer.clone(),
            })
            .unwrap();
        let SipEvent::Answered { answer_sdp, .. } = recv_until(&caller, "Answered", Some("c-p2p"))
        else {
            unreachable!()
        };
        assert_eq!(answer_sdp, answer, "P2P answer 应经 SIP 端到端字节一致");
        caller_media
            .accept_answer(&answer_sdp)
            .expect("主叫 accept");

        // 双侧泵：ICE 建链 + 通道打开（= DTLS 完成 = SRTP 密钥就绪，10s 兜底）。
        // 只等 ICE 就写媒体会被对端以「无 SRTP 接收上下文」丢帧（PCMU 无重传）。
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut got_media = false;
        let mut channels_open = (0u32, 0u32);
        while std::time::Instant::now() < deadline {
            let _ = caller_media.poll();
            let _ = callee_media.poll();
            // 排空事件（ChannelOpen 等）；Media 事件单独捕获。
            while let Some(ev) = caller_media.poll_event() {
                if matches!(ev, aerodesk_core::endpoint::ClientEvent::ChannelOpen(..)) {
                    channels_open.0 += 1;
                }
                if matches!(ev, aerodesk_core::endpoint::ClientEvent::Media(_)) {
                    got_media = true;
                }
            }
            while let Some(ev) = callee_media.poll_event() {
                if matches!(ev, aerodesk_core::endpoint::ClientEvent::ChannelOpen(..)) {
                    channels_open.1 += 1;
                }
            }
            if (caller_media.ice_connected()
                && callee_media.ice_connected()
                && channels_open.0 >= 1
                && channels_open.1 >= 1)
                || got_media
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            caller_media.ice_connected() && callee_media.ice_connected(),
            "SIP 承载的 P2P 呼叫 10s 内应 ICE 建链（caller_media={} callee_media={}）",
            caller_media.bytes_received(),
            callee_media.bytes_received()
        );
        assert!(
            caller_media.bytes_received() > 0 && callee_media.bytes_received() > 0,
            "双侧应有收包（ICE/DTLS）"
        );
        // 媒体载荷：被叫发一帧 PCMU → 主叫 Media 事件（不依赖编解码器）。
        if !got_media {
            let audio_mid = offer.audio_mid.expect("with_audio 应有音频 mid");
            let payload: std::sync::Arc<[u8]> = std::sync::Arc::from(vec![0u8; 160]);
            callee_media
                .endpoint()
                .send_audio_frame(
                    audio_mid,
                    payload,
                    str0m::media::MediaTime::new(0, str0m::media::Frequency::EIGHT_KHZ),
                )
                .expect("写 PCMU 帧");
            let media_deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < media_deadline && !got_media {
                let _ = caller_media.poll();
                let _ = callee_media.poll();
                while let Some(ev) = caller_media.poll_event() {
                    if matches!(ev, aerodesk_core::endpoint::ClientEvent::Media(_)) {
                        got_media = true;
                    }
                }
                while callee_media.poll_event().is_some() {}
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        assert!(got_media, "主叫应收到被叫的 PCMU 媒体事件");

        caller.shutdown();
        callee.shutdown();
        server_cancel.cancel();
        server
            .join()
            .expect("server 线程应退出")
            .expect("server 应正常退出");
    }

    /// #503-4 INVITE 授权 e2e：被叫有固定口令时——不带口令 → 407 拒绝；
    /// 错口令 → 403；临时口令 → 放行并完成呼叫。
    #[test]
    fn end_to_end_invite_authorization() {
        let _serial = serve_e2e_guard();
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
        use aerodesk_protocol::sip_client::{
            SipClientConfig, SipCommand, SipEvent, SipTransport, start_sip_client,
        };

        let port = free_udp_port();
        let mut pw = HashMap::new();
        pw.insert("AD-CALLER".to_string(), "tok-caller".to_string());
        pw.insert("AD-CALLEE".to_string(), "tok-callee".to_string());
        let temps = Arc::new(Mutex::new(TempRegistry::default()));
        let server_cancel = CancellationToken::new();
        let sc = server_cancel.clone();
        let temps2 = temps.clone();
        let server = std::thread::spawn(move || {
            run_sip_endpoint(
                SipConfig {
                    realm: REALM.into(),
                    tls_addr: None,
                    wss_addr: None,
                    udp_addr: Some(format!("127.0.0.1:{port}").parse().unwrap()),
                    passwords: Arc::new(pw),
                    tls_identity: None,
                    sfu_urls: vec![],
                    sfu_token: None,
                    token_password: None,
                    open_register: false,
                    temp_passwords: temps2,
                },
                sc,
            )
        });
        std::thread::sleep(Duration::from_millis(400));

        let client_cfg = |device: &str| SipClientConfig {
            device_id: device.into(),
            domain: REALM.into(),
            password: format!("tok-{}", device.trim_start_matches("AD-").to_lowercase()),
            server: format!("127.0.0.1:{port}").parse().unwrap(),
            transport: SipTransport::Udp,
            tls: None,
            register_expires: 60,
        };
        let caller = start_sip_client(client_cfg("AD-CALLER")).expect("caller 启动");
        let callee = start_sip_client(client_cfg("AD-CALLEE")).expect("callee 启动");
        assert!(matches!(
            recv_until(&caller, "Registered", None),
            SipEvent::Registered { .. }
        ));
        assert!(matches!(
            recv_until(&callee, "Registered", None),
            SipEvent::Registered { .. }
        ));

        // 1) 未带口令 → 407 拒绝。
        caller
            .send(SipCommand::Call {
                target_device: "AD-CALLEE".into(),
                call_id: "c-auth-1".into(),
                offer_sdp: CALLER_SDP.into(),
                call_password: None,
            })
            .unwrap();
        match recv_until(&caller, "Rejected", Some("c-auth-1")) {
            SipEvent::Rejected {
                call_id, status, ..
            } => {
                assert_eq!(call_id, "c-auth-1");
                assert_eq!(status, 407, "未带口令应 407 质询拒绝");
            }
            other => panic!("应 Rejected(407)，得 {other:?}"),
        }

        // 2) 错口令 → 403。
        caller
            .send(SipCommand::Call {
                target_device: "AD-CALLEE".into(),
                call_id: "c-auth-2".into(),
                offer_sdp: CALLER_SDP.into(),
                call_password: Some("WRONG".into()),
            })
            .unwrap();
        match recv_until(&caller, "Rejected", Some("c-auth-2")) {
            SipEvent::Rejected {
                call_id, status, ..
            } => {
                assert_eq!(call_id, "c-auth-2");
                assert_eq!(status, 403, "口令错应 403");
            }
            other => panic!("应 Rejected(403)，得 {other:?}"),
        }

        // 3) 临时口令（有效期 60s）→ 放行并完成呼叫。
        temps
            .lock()
            .unwrap()
            .issue("AD-CALLEE", "temp-abc".into(), Duration::from_secs(60));
        caller
            .send(SipCommand::Call {
                target_device: "AD-CALLEE".into(),
                call_id: "c-auth-3".into(),
                offer_sdp: CALLER_SDP.into(),
                call_password: Some("temp-abc".into()),
            })
            .unwrap();
        let _ = recv_until(&callee, "IncomingCall", Some("c-auth-3"));
        callee
            .send(SipCommand::Accept {
                call_id: "c-auth-3".into(),
                answer_sdp: CALLEE_SDP.into(),
            })
            .unwrap();
        match recv_until(&caller, "Answered", Some("c-auth-3")) {
            SipEvent::Answered { call_id, .. } => assert_eq!(call_id, "c-auth-3"),
            other => panic!("临时口令应放行，得 {other:?}"),
        }
        // 收尾挂断走主叫而非被叫：被叫侧 established 唯一由 DialogState::Confirmed
        // 事件置位，与命令在同一 select! 主循环里按调度公平性竞争——CI 抖动下
        // 输了竞态会把挂断当「未建立拒接」本地 603 吞掉（无 BYE 上线），主叫
        // PeerHangup 永久不可达（曾致本测试 CI 侧「等 PeerHangup 超时」红）。
        // 主叫侧 established 由 2xx→Answered 双通道确定性置位，挂断必发 BYE，
        // 被叫收 UasBye → PeerHangup 必然产出。授权断言（407/403/放行）不变。
        caller
            .send(SipCommand::Hangup {
                call_id: "c-auth-3".into(),
                reason: None,
            })
            .unwrap();
        let _ = recv_until(&callee, "PeerHangup", Some("c-auth-3"));

        caller.shutdown();
        callee.shutdown();
        server_cancel.cancel();
        server
            .join()
            .expect("server 线程应退出")
            .expect("server 应正常退出");
    }
}
