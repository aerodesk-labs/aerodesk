//! SIP 信令端点（#551 / 规范 docs/SIP_SIGNALING.md）。
//!
//! 本模块把 rsipstack 的 Registrar 能力引入 signal，与现有 JSON/rouille 服务
//! **双栈并存**：SIP 端点由 `SIP_TLS_PORT` / `SIP_WSS_PORT` 显式开启（默认关闭，
//! 生产 JSON 路径不受影响，规范 §8 迁移约束）。
//!
//! 本批次（slice 1）落地：
//! - SIP over TLS（原生端，规范 §0 传输矩阵）+ SIP over WSS（Web 端，RFC 7118）监听；
//! - `REGISTER` + SIP Digest 认证（401 质询 → 带 Authorization 重 REGISTER → 200）；
//!   AoR→Contact 注册表（含 expires 过期、expires=0 注销）；
//! - `OPTIONS` 保活/能力探测（200 + Allow）；
//! - 严格子集：其余方法（含尚未接入的 INVITE/ACK/BYE/CANCEL/INFO）一律 501，
//!   见规范 §6——这些是 slice 2（Proxy/路由）落点，本批先立 Registrar 地基。
//!
//! 认证模型（规范 §1）：Digest username = 设备 ID，口令 = 设备 token。
//! 本批经 `SIP_DIGEST_USERS="dev1=tok1,dev2=tok2"` 注入口令表；HA1-at-rest
//! （服务端只存 H(user:realm:token)）与 nonce 防重放（stale 追踪）为后续硬化项，
//! 不影响线上互通——`verify_digest` 的线行为已端到端测（见 tests）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rsipstack::EndpointBuilder;
use rsipstack::dialog::authenticate::verify_digest;
use rsipstack::sip::{Header, HeadersExt, Method, Request, StatusCode};
use rsipstack::transport::TransportLayer;
use rsipstack::transport::connection::SipConnection;
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

/// 一条注册绑定：AoR → Contact + 过期时刻。
/// `contact` 在 slice 2（INVITE 路由到已注册 Contact）读取，本批先落存储。
#[derive(Debug, Clone)]
pub struct Binding {
    #[allow(dead_code)] // slice 2 路由读取
    pub contact: String,
    pub expires_at: Instant,
}

/// Registrar 注册表（纯数据、可单测）。
#[derive(Debug, Default)]
pub struct Registrar {
    bindings: HashMap<String, Binding>,
}

impl Registrar {
    ///  upsert 绑定，返回过期秒数。
    pub fn register(&mut self, aor: &str, contact: String, expires_secs: u32) -> u32 {
        self.bindings.insert(
            aor.to_string(),
            Binding {
                contact,
                expires_at: Instant::now() + Duration::from_secs(expires_secs as u64),
            },
        );
        expires_secs
    }

    /// 注销（REGISTER expires=0）。返回是否存在过绑定。
    pub fn unregister(&mut self, aor: &str) -> bool {
        self.bindings.remove(aor).is_some()
    }

    /// 查询有效绑定（惰性剔除过期）。slice 2 路由 INVITE 时读取，本批单测覆盖。
    #[allow(dead_code)] // slice 2 路由读取
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

/// SIP 端点配置。
pub struct SipConfig {
    pub realm: String,
    /// SIP over TLS 监听地址（None = 不开）。
    pub tls_addr: Option<SocketAddr>,
    /// SIP over WSS 监听地址（None = 不开）。
    pub wss_addr: Option<SocketAddr>,
    /// SIP over UDP 监听地址（None = 不开；内网/调试用，规范 §0 传输矩阵可选项）。
    pub udp_addr: Option<SocketAddr>,
    /// Digest 口令表：设备 ID → token。
    pub passwords: Arc<HashMap<String, String>>,
    /// TLS 身份（复用 signal 的证书加载）。
    pub tls_identity: Option<TlsIdentity>,
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
    let nonce_counter = Arc::new(AtomicU64::new(0));
    let nonce_secret =
        format!("{:x}", &cfg.realm as *const _ as usize) + &format!("{:x}", std::process::id());

    info!("SIP 端点已就绪（Registrar+OPTIONS；INVITE/INFO/BYE 归 slice 2）");

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

        // 严格子集门禁（规范 §6）。
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
                let password_of = move |user: &str| passwords.get(user).cloned();
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
                            let mut reg = registrar.lock().unwrap();
                            let existed = reg.unregister(&aor);
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
                            let mut reg = registrar.lock().unwrap();
                            reg.register(&aor, contact, expires);
                            let n = reg.len();
                            info!(%aor, expires, online = n, "SIP 注册");
                        }
                        let _ = tx.reply(StatusCode::OK).await;
                    }
                }
            }
            // INVITE/ACK/BYE/CANCEL/INFO：slice 2（Proxy/路由）。本批 501。
            _ => {
                let _ = tx.reply(StatusCode::NotImplemented).await;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsipstack::sip::Request;
    use rsipstack::sip::typed::Authorization;

    const REALM: &str = "aerodesk.test";
    const NONCE: &str = "testnonce123";

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
    fn registrar_upsert_expire_unregister() {
        let mut r = Registrar::default();
        r.register("AD-A", "sip:AD-A@1.2.3.4".into(), 60);
        assert_eq!(r.len(), 1);
        assert!(r.lookup("AD-A").is_some());
        assert!(r.unregister("AD-A"));
        assert!(!r.unregister("AD-A"));
        assert_eq!(r.len(), 0);
        // 过期剔除
        r.register("AD-B", "c".into(), 0);
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

    // -- 端到端：rsipstack 客户端经 UDP 完成 REGISTER→401→REGISTER→200 --

    fn free_udp_port() -> u16 {
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        port
    }

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_register_digest_over_udp() {
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
}
