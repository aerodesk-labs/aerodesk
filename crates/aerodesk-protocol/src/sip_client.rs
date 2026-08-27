//! SIP 客户端 UA（#552 slice 2：呼叫面——INVITE/BYE/INFO trickle/302 升级）。
//!
//! 本模块把 rsipstack 的 dialog/transaction/transport 收敛为一个**语义事件 API**
//! （规范 §8/§9：SIP UA 收敛在 protocol 信令层，core 按事件迁移而非按报文迁移，
//! 媒体核心不 import SIP 类型）。core 侧是 std::thread 模型，因此：
//!
//! - UA 自带独立 tokio runtime（2 线程，与 signal 侧 `run_sip_endpoint` 同构）；
//! - core → UA：[`SipCommand`]（tokio unbounded channel，任意线程可发）；
//! - UA → core：[`SipEvent`]（std mpsc，core 用 [`SipClientHandle::recv_event`]
//!   带超时轮询，与现有 presence 循环的 stop-flag 轮询同构）。
//!
//! 呼叫面模型（规范 §4/§4.1）：
//! - 主叫：`Call` → 经 signal 透明 Proxy 的 INVITE（offer 字节透传）；`180`→
//!   [`SipEvent::Ringing`]，`200`→[`SipEvent::Answered`]（answer 字节透传），
//!   4xx/6xx→[`SipEvent::Rejected`]（error_code 按规范 §3 由响应码映射），
//!   `302`→[`SipEvent::EscalatedToSfu`]（会议 AoR 按 §4.1 由对端设备确定性推导——
//!   rsipstack `reject` 不能带 Contact，推导规则使两端同样收敛）。
//! - 被叫：INVITE → [`SipEvent::IncomingCall`]；core 以 `Ring`/`Accept`/`Reject`/
//!   `RedirectToSfu` 决策；对端 CANCEL → [`SipEvent::PeerHangup`]。
//! - 对话内：`Hangup`→BYE（reason 透传 Reason 头；[`crate::sip::ESCALATE_BYE_REASON`]
//!   即升级语义），对端 BYE → [`SipEvent::PeerHangup`]（cause=302 时改为
//!   [`SipEvent::EscalatedToSfu`] 并抑制 PeerHangup）；`SendTrickle`→INFO sdpfrag，
//!   对端 INFO → [`SipEvent::Trickle`]。
//!
//! 传输（规范 §0 传输矩阵）：[`SipTransport::Udp`] 内网/调试可选项、
//! [`SipTransport::Tls`] 公网默认加密传输——客户端无监听，TLS 连接为
//! 对 signal 的**既出流**（RFC 5923 alias 语义端到端复用：INVITE/BYE/INFO 与
//! 注册共用同一条长连，in-dialog 请求由服务端沿同一流回推，不需要客户端回连）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::{DialogState, DialogStateSender, TerminatedReason};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::dialog::registration::Registration;
use rsipstack::sip::{Header, HeadersExt, Method, Response, StatusCode};
use rsipstack::transaction::endpoint::EndpointBuilder;
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::sip_addr::SipAddr;
use rsipstack::transport::tls::{TlsConfig, TlsConnection};
use rsipstack::transport::udp::UdpConnection;
use rsipstack::transport::{SipConnection, TransportLayer};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::sip::{
    IMPLEMENTED_METHODS, PROTOCOL_VERSION, TRICKLE_ICE_CONTENT_TYPE, TrickleCandidate,
    decode_trickle, device_aor, device_from_uri, encode_trickle, error_code_to_status,
    is_escalation_reason, status_to_error_code, view_aor,
};

/// 信令传输（规范 §0 传输矩阵：UDP=内网/调试可选项，TLS=公网默认加密）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SipTransport {
    /// UDP（内网/调试；规范 §0 传输矩阵可选项）。
    Udp,
    /// SIP over TLS（公网默认传输，强制加密；TLS 层不替代 Digest 应用层认证）。
    Tls,
}

/// TLS 传输配置（[`SipTransport::Tls`] 时必填）。
#[derive(Debug, Clone)]
pub struct SipTlsConfig {
    /// 服务端证书签发 CA 的 PEM（rsipstack 运行时只建 RootStore 信任锚，
    /// 不支持系统根加载；生产由运营下发公网 CA 或经 [`system_ca_pem`] 取系统根，
    /// 开发/联调用测试 CA）。空 Vec = 无信任锚（连接必失败）。
    pub ca_certs: Vec<u8>,
    /// SNI 与证书校验名（缺省取连接目标 host；公网部署必须与服务端证书 SAN 一致）。
    pub sni_hostname: Option<String>,
    /// 可选 mTLS 客户端证书（PEM；公网默认不做客户端证书认证——Digest 已认证）。
    pub client_cert: Option<Vec<u8>>,
    /// 可选 mTLS 客户端私钥（PEM，与 [`Self::client_cert`] 成对）。
    pub client_key: Option<Vec<u8>>,
}

/// 系统根证书 PEM 包（Windows 证书库 / macOS keychain / Linux 系统位置，语义随
/// rustls-native-certs）：rsipstack 不自动加载系统根，TLS 客户端把本包作为
/// [`SipTlsConfig::ca_certs`] 的默认值即可信任公网 CA。DER 证书逐个 PEM 包裹
/// （base64ct 标准字母表 + 64 列换行，PEM 惯例）。
pub fn system_ca_pem() -> Vec<u8> {
    use base64ct::{Base64, Encoding};
    let result = rustls_native_certs::load_native_certs();
    if !result.errors.is_empty() {
        warn!(
            "系统根证书加载部分失败（{} 条，忽略并继续）",
            result.errors.len()
        );
    }
    let mut out = Vec::new();
    for cert in &result.certs {
        let b64 = Base64::encode_string(cert.as_ref());
        out.extend_from_slice(b"-----BEGIN CERTIFICATE-----\n");
        for chunk in b64.as_bytes().chunks(64) {
            out.extend_from_slice(chunk);
            out.push(b'\n');
        }
        out.extend_from_slice(b"-----END CERTIFICATE-----\n");
    }
    out
}

/// 从 PEM 文件读 CA 包（路径来源：服务/桌面设置；不存在/不可读返回 Err）。
pub fn load_ca_pem_file(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("读 CA PEM {path}: {e}"))
}

/// 客户端 UA 配置。
#[derive(Debug, Clone)]
pub struct SipClientConfig {
    /// 设备 ID（Digest username = 设备 ID，规范 §1）。
    pub device_id: String,
    /// SIP 域（AoR 路由键，不做 DNS 解析；报文域与实际投递地址解耦）。
    pub domain: String,
    /// Digest 口令（= 现有 auth_token，迁移期同一凭据，规范 §8）。
    pub password: String,
    /// signal 的 SIP 监听地址（实际投递目标，RFC 5626 outbound proxy 语义：
    /// 报文头域保留 domain，UDP 包发往该地址）。
    pub server: SocketAddr,
    /// 传输（[`SipTransport::Udp`] 或 [`SipTransport::Tls`]）。
    pub transport: SipTransport,
    /// TLS 配置（[`SipTransport::Tls`] 时必填）。
    pub tls: Option<SipTlsConfig>,
    /// REGISTER 过期秒数（刷新周期 = expires/2）。
    pub register_expires: u32,
}

/// UA → core 的语义事件（与 SignalMessage 同构，规范 §9）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SipEvent {
    /// 注册成功（Joined 同构；aor = 本端 AoR）。
    Registered { aor: String, expires: u32 },
    /// 注册失败（status=0 表示传输/内部错误，非 SIP 响应码）。
    RegisterFailed { status: u16, reason: String },
    /// 来电（Call 同构；被叫侧）。
    IncomingCall {
        call_id: String,
        from_device: String,
        offer_sdp: String,
    },
    /// 对端响铃（CallRinging 同构；主叫侧）。
    Ringing { call_id: String },
    /// 对端接听（CallAccepted 同构；answer_sdp 端到端透传）。
    Answered { call_id: String, answer_sdp: String },
    /// 呼叫被拒/失败（CallRejected 同构；error_code 按规范 §3 由响应码映射）。
    Rejected {
        call_id: String,
        status: u16,
        error_code: Option<String>,
    },
    /// 对端挂断（Hangup 同构：对话内 BYE；被叫侧亦用于对端 CANCEL 未接呼叫）。
    PeerHangup {
        call_id: String,
        reason: Option<String>,
    },
    /// 呼叫已升级至 SFU 会议 AoR（§4.1：对端 BYE cause=302，或主叫侧收 302）。
    /// core 应按 view_aor 重新发起呼叫（媒体切 SFU）。
    EscalatedToSfu { call_id: String, view_aor: String },
    /// 对端 trickle 候选（IceCandidate 同构，INFO sdpfrag）。
    Trickle {
        call_id: String,
        candidate: TrickleCandidate,
    },
    /// UA 已停止（通道关闭前的最后一个事件；core 应停止轮询或重启 UA）。
    EndpointStopped,
}

/// core → UA 的命令。
#[derive(Debug, Clone)]
pub enum SipCommand {
    /// 发起呼叫（Call 同构：INVITE + SDP offer；call_id 由 core 生成 → Call-ID）。
    Call {
        target_device: String,
        call_id: String,
        offer_sdp: String,
        /// #503-4 呼叫授权口令（被叫设备固定/临时密码）：signal 对 INVITE 做
        /// 407 Proxy-Authorization 质询时以该口令应答（Digest username = 被叫
        /// 设备 ID）；None = 不附凭据（目标无口令的开放部署/未配置设备）。
        call_password: Option<String>,
    },
    /// 响铃（CallRinging 同构：180；被叫侧弹出确认窗时发）。
    Ring { call_id: String },
    /// 接听（CallAccepted 同构：200 OK + SDP answer）。
    Accept { call_id: String, answer_sdp: String },
    /// 拒接（CallRejected 同构：规范 §3 error_code → 响应码）。
    Reject { call_id: String, error_code: String },
    /// 升级重定向（§4.1 被控端决策：对新入呼回 302，呼叫转移至会议 AoR）。
    RedirectToSfu { call_id: String },
    /// 挂断（Hangup 同构：已建立→BYE（reason 透传 Reason 头，
    /// 升级场景用 [`crate::sip::ESCALATE_BYE_REASON`]）；未建立→CANCEL/拒绝）。
    Hangup {
        call_id: String,
        reason: Option<String>,
    },
    /// 发送 trickle 候选（IceCandidate 同构：INFO sdpfrag）。
    SendTrickle {
        call_id: String,
        candidate: TrickleCandidate,
    },
    /// 立即重注册（不等刷新周期；如网络切换后）。
    Reregister,
}

/// 客户端 UA 句柄（core 侧持有）。Drop 不自动关停——显式 [`Self::shutdown`]。
pub struct SipClientHandle {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<SipCommand>,
    event_rx: std_mpsc::Receiver<SipEvent>,
    cancel: CancellationToken,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SipClientHandle {
    /// 发送命令（非阻塞；UA 已停止时返回 Err）。
    pub fn send(&self, cmd: SipCommand) -> Result<(), String> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| "SIP UA 已停止，命令无法投递".to_string())
    }

    /// 带超时收取一个语义事件；超时返回 None（UA 仍在运行）。
    /// UA 停止前先收到 [`SipEvent::EndpointStopped`]，此后返回 None。
    pub fn recv_event(&self, timeout: Duration) -> Option<SipEvent> {
        self.event_rx.recv_timeout(timeout).ok()
    }

    /// 关停 UA：摘所有对话（best-effort）→ 注销 → 停 endpoint → join 线程。
    pub fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 启动客户端 UA：spawn 线程 + 独立 tokio runtime，立即返回句柄。
/// 注册在后台异步进行，结果经 [`SipEvent::Registered`] / [`SipEvent::RegisterFailed`] 上报。
pub fn start_sip_client(cfg: SipClientConfig) -> Result<SipClientHandle, String> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, event_rx) = std_mpsc::channel();
    let cancel = CancellationToken::new();
    let cancel_thread = cancel.clone();
    let thread = std::thread::Builder::new()
        .name("sip-client".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = event_tx.send(SipEvent::RegisterFailed {
                        status: 0,
                        reason: format!("tokio runtime 构建失败: {e}"),
                    });
                    let _ = event_tx.send(SipEvent::EndpointStopped);
                    return;
                }
            };
            rt.block_on(run_client(cfg, cmd_rx, event_tx.clone(), cancel_thread));
            let _ = event_tx.send(SipEvent::EndpointStopped);
        })
        .map_err(|e| format!("spawn sip-client 线程失败: {e}"))?;
    Ok(SipClientHandle {
        cmd_tx,
        event_rx,
        cancel,
        thread: Some(thread),
    })
}

/// 一通呼叫的 UA 侧状态（按 Call-ID 索引；UAC/UAS 双腿同构存 `InviteDialog`）。
struct CallEntry {
    dialog: InviteDialog,
    /// 对端设备 ID（升级时推导会议 AoR 用，§4.1）。
    peer_device: String,
    /// 本端是否主叫（UAC 腿）。
    initiator: bool,
    /// 对话已 Confirmed（200/ACK 之后）。
    established: bool,
    /// 终局事件（Answered/Rejected/EscalatedToSfu）已上报——去重
    /// Confirmed 事件与 do_invite JoinHandle 的双通道到达。
    final_notified: bool,
    /// 升级事件已上报（BYE cause=302）——抑制后续 PeerHangup。
    escalate_notified: bool,
}

/// `handle_command` 的静态上下文（收束参数个数）。
struct CmdCtx<'a> {
    cfg: &'a SipClientConfig,
    dialog_layer: &'a Arc<DialogLayer>,
    state_tx: &'a DialogStateSender,
    invite_final_tx: &'a tokio::sync::mpsc::UnboundedSender<(String, Option<Response>)>,
    contact: &'a Option<rsipstack::sip::Uri>,
    dialogs: &'a mut HashMap<String, CallEntry>,
    event_tx: &'a std_mpsc::Sender<SipEvent>,
}

/// rustls 0.23 在 feature 联合构建下无法自动判定 CryptoProvider：
/// 本工作区 core/rsipstack 均 pin `ring`，aerodesk-sfu pin `aws_lc_rs`，
/// `cargo test --workspace` 把两者合并 → `from_crate_features()` 返回 None →
/// rsipstack 内部 `ClientConfig::builder()` 直接 panic（本文件 274 行 UA 线程
/// 就此死于「Could not automatically determine the process-level CryptoProvider」）。
/// 按 rustls 文档在应用层显式安装（进程级一次性；重复安装返回 Err 忽略）。
/// 服务端（signal WSS accept / sfu TLS）同样需要——进程级安装任一方先到即生效。
pub fn ensure_rustls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

async fn run_client(
    cfg: SipClientConfig,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<SipCommand>,
    event_tx: std_mpsc::Sender<SipEvent>,
    cancel: CancellationToken,
) {
    ensure_rustls_provider();
    if let Err(e) = run_client_inner(&cfg, &mut cmd_rx, &event_tx, &cancel).await {
        let _ = event_tx.send(SipEvent::RegisterFailed {
            status: 0,
            reason: e,
        });
    }
    debug!(device = %cfg.device_id, "SIP 客户端 UA 退出");
}

async fn run_client_inner(
    cfg: &SipClientConfig,
    cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SipCommand>,
    event_tx: &std_mpsc::Sender<SipEvent>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    // 传输/端点生命周期独立于主循环 cancel：关停顺序是先撤对话、再注销，
    // 复位期间仍需收发（BYE/REGISTER 的响应要能回来）——若与主循环共用
    // token，cancel 瞬间传输即死，drain 的 hangup() 将永等不到 200 OK。
    // 传输随 runtime 收尾 drop，无需显式 cancel。
    let transport_cancel = CancellationToken::new();
    let tl = TransportLayer::new(transport_cancel.clone());
    // 本地传输地址（Contact 兜底；0.0.0.0 绑定不能作为对话目标——ACK/INFO 会
    // 发到不可达地址，故 Contact 优先取注册时 Via received/rport 发现的公网地址）。
    let local_addr = match cfg.transport {
        SipTransport::Udp => {
            // 客户端用 add_transport（非 add_listener）：get_via 须能看到该传输。
            let conn = UdpConnection::create_connection(
                "0.0.0.0:0".parse().unwrap(),
                None,
                Some(transport_cancel.clone()),
            )
            .await
            .map_err(|e| format!("SIP/UDP 客户端传输创建失败: {e}"))?;
            tl.add_transport(SipConnection::from(conn));
            tl.get_addrs().into_iter().next()
        }
        SipTransport::Tls => {
            let tls_cfg = cfg
                .tls
                .as_ref()
                .ok_or("TLS 传输必须提供 tls 配置（CA 证书）")?;
            // 预连（非懒建）：REGISTER 的 Via/Contact 需要本端地址（get_via 取
            // get_addrs().first()，纯 TLS 客户端无监听，首个连接建立前为空）。
            // 连接一经 add_connection 即注册进 connections 表：后续 REGISTER/
            // INVITE/BYE 的 lookup 按目标 TLS 地址直接命中复用，且 serve_connection
            // 自动接管入站报文（INVITE/BYE/INFO 均在既有流上到达）。
            let mut server_addr = SipAddr::from(cfg.server);
            server_addr.r#type = Some(rsipstack::sip::Transport::Tls);
            let srv_tls = TlsConfig {
                cert: None,
                key: None,
                client_cert: tls_cfg.client_cert.clone(),
                client_key: tls_cfg.client_key.clone(),
                ca_certs: Some(tls_cfg.ca_certs.clone()),
                sni_hostname: tls_cfg.sni_hostname.clone(),
            };
            let conn = TlsConnection::connect(
                &server_addr,
                Some(&srv_tls),
                None,
                Some(transport_cancel.clone()),
            )
            .await
            .map_err(|e| format!("SIP/TLS 客户端传输创建失败: {e}"))?;
            tl.add_connection(SipConnection::from(conn));
            // 连接断开后的懒重连兜底（lookup 无命中时按 tls_config 自建）。
            tl.set_tls_config(srv_tls);
            tl.get_addrs().into_iter().next()
        }
    };

    let endpoint = EndpointBuilder::new()
        .with_user_agent(PROTOCOL_VERSION)
        .with_transport_layer(tl)
        .with_cancel_token(transport_cancel.clone())
        .with_allows(IMPLEMENTED_METHODS.to_vec())
        .build();

    let mut incoming = endpoint
        .incoming_transactions()
        .map_err(|e| format!("取 incoming_transactions 失败: {e}"))?;

    {
        let inner = endpoint.inner.clone();
        tokio::spawn(async move {
            if let Err(e) = inner.serve().await {
                warn!(error = %e, "SIP 客户端 endpoint serve 退出");
            }
        });
    }

    let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
    let (state_tx, mut state_rx) = dialog_layer.new_dialog_state_channel();
    // do_invite_async 的 JoinHandle 结果（终局响应）回注主循环。
    let (invite_final_tx, mut invite_final_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Option<Response>)>();

    // REGISTER 的 Request-URI：报文域保留 domain + 传输参数（TLS 时;transport=tls；
    // 注册逻辑从该参数继承出站目的地的传输类型）。
    let server_uri: rsipstack::sip::Uri = match cfg.transport {
        SipTransport::Udp => format!("sip:{}", cfg.domain),
        SipTransport::Tls => format!("sip:{};transport=tls", cfg.domain),
    }
    .try_into()
    .map_err(|e| format!("server URI 构造失败: {e}"))?;
    let aor = device_aor(&cfg.device_id, &cfg.domain);

    let mut registration = Registration::new(
        endpoint.inner.clone(),
        Some(Credential {
            username: cfg.device_id.clone(),
            password: cfg.password.clone(),
            realm: None, // realm 以 401 质询为准（RFC 3261），无需预置
        }),
    );
    // 域与实际投递解耦：报文头域保留 domain，包发往 signal 监听地址。
    registration.outbound_proxy = Some(cfg.server);
    // 可靠传输的 REGISTER Contact 必须带 transport 参数（RFC 3261 §20.10）；
    // 显式设置，避免 rsipstack 内部从 Via 构造的裸 `sip:user@addr` 无参数版本。
    let sip_transport = match cfg.transport {
        SipTransport::Udp => rsipstack::sip::Transport::Udp,
        SipTransport::Tls => rsipstack::sip::Transport::Tls,
    };
    registration.contact = Some(nat_contact_typed(
        &registration,
        local_addr.as_ref(),
        &cfg.device_id,
        sip_transport,
    ));

    // 首次注册（启动即报 presence）。
    do_register(
        &mut registration,
        &server_uri,
        &aor,
        cfg.register_expires,
        event_tx,
    )
    .await;
    // NAT 感知 Contact（对话级本端地址：INVITE 的 Contact / UAS 腿的 local contact）。
    let mut contact_uri = nat_contact(
        &registration,
        local_addr.as_ref(),
        &cfg.device_id,
        sip_transport,
    );

    let mut dialogs: HashMap<String, CallEntry> = HashMap::new();
    let refresh = Duration::from_secs((cfg.register_expires as u64 / 2).max(5));

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            cmd = cmd_rx.recv() => match cmd {
                Some(SipCommand::Reregister) => {
                    do_register(&mut registration, &server_uri, &aor, cfg.register_expires,
                        event_tx).await;
                    contact_uri = nat_contact(&registration, local_addr.as_ref(), &cfg.device_id, sip_transport);
                }
                Some(cmd) => handle_command(
                    cmd,
                    CmdCtx {
                        cfg,
                        dialog_layer: &dialog_layer,
                        state_tx: &state_tx,
                        invite_final_tx: &invite_final_tx,
                        contact: &contact_uri,
                        dialogs: &mut dialogs,
                        event_tx,
                    },
                ).await,
                None => break,
            },
            maybe = incoming.recv() => match maybe {
                Some(tx) => handle_incoming(
                    tx, cfg, &dialog_layer, &state_tx, &contact_uri, &mut dialogs, event_tx,
                ).await,
                None => break,
            },
            st = state_rx.recv() => match st {
                Some(st) => {
                    handle_dialog_state(st, cfg, &dialog_layer, &mut dialogs, event_tx).await
                }
                None => break,
            },
            fin = invite_final_rx.recv() => match fin {
                Some((call_id, resp)) => {
                    handle_invite_final(&call_id, resp, cfg, &mut dialogs, event_tx)
                }
                None => break,
            },
            _ = tokio::time::sleep(refresh) => {
                do_register(&mut registration, &server_uri, &aor, cfg.register_expires, event_tx)
                    .await;
                contact_uri = nat_contact(&registration, local_addr.as_ref(), &cfg.device_id, sip_transport);
            }
        }
    }

    // 关停：摘所有对话（best-effort）+ 注销（expires=0），presence 立即下线。
    for (_, entry) in dialogs.drain() {
        // best-effort：BYE 响应可能因对端/代理已下线而不至（UDP 非 INVITE
        // 事务超时以十秒计），关停路径统一 2s 封顶，绝不让 shutdown 悬挂。
        let _ = tokio::time::timeout(Duration::from_secs(2), entry.dialog.hangup()).await;
        dialog_layer.remove_dialog(&entry.dialog.id());
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        registration.register(server_uri, Some(0)),
    )
    .await;
    info!(device = %cfg.device_id, "SIP 客户端 UA 已关停并注销");
    Ok(())
}

/// 构造 NAT 感知 Contact（typed 形态，供 [`Registration::contact`] 显式设置）：
/// 优先注册时 Via received/rport 发现的公网地址，兜底本地传输地址。
/// 0.0.0.0 监听地址不可作对话目标（对端 ACK/INFO 不可达）；非 UDP 传输
/// 追加 `;transport=<t>`（RFC 3261 §20.10——可靠传输的 Contact 必须声明传输，
/// 否则对端/代理按默认 UDP 投递，呼叫面会静默失败）。
fn nat_contact_typed(
    registration: &Registration,
    local_addr: Option<&SipAddr>,
    device_id: &str,
    transport: rsipstack::sip::Transport,
) -> rsipstack::sip::typed::Contact {
    let local = local_addr.cloned().unwrap_or_default();
    let mut contact = Registration::create_nat_aware_contact(
        device_id,
        registration.public_address.clone(),
        &local,
    );
    if transport != rsipstack::sip::Transport::Udp {
        contact
            .uri
            .params
            .push(rsipstack::sip::Param::Transport(transport));
    }
    contact
}

/// 构造 NAT 感知 Contact 的 URI（对话级：INVITE 的 Contact / UAS 腿 local contact）。
fn nat_contact(
    registration: &Registration,
    local_addr: Option<&SipAddr>,
    device_id: &str,
    transport: rsipstack::sip::Transport,
) -> Option<rsipstack::sip::Uri> {
    Some(nat_contact_typed(registration, local_addr, device_id, transport).uri)
}

/// core 命令处理（呼叫面全量）。静态上下文收束为 [`CmdCtx`]，主循环只传两项。
async fn handle_command(cmd: SipCommand, ctx: CmdCtx<'_>) {
    let CmdCtx {
        cfg,
        dialog_layer,
        state_tx,
        invite_final_tx,
        contact,
        dialogs,
        event_tx,
    } = ctx;
    match cmd {
        SipCommand::Call {
            target_device,
            call_id,
            offer_sdp,
            call_password,
        } => {
            let callee: rsipstack::sip::Uri =
                match device_aor(&target_device, &cfg.domain).try_into() {
                    Ok(u) => u,
                    Err(e) => {
                        warn!(error = %e, "callee URI 构造失败");
                        return;
                    }
                };
            let caller: rsipstack::sip::Uri =
                match device_aor(&cfg.device_id, &cfg.domain).try_into() {
                    Ok(u) => u,
                    Err(e) => {
                        warn!(error = %e, "caller URI 构造失败");
                        return;
                    }
                };
            // #503-4：呼叫授权凭据——407 质询时以「被叫设备 ID + 被叫口令」应答
            // （Digest Proxy-Authorization；realm 由质询回填）。口令即被叫设备的
            // 固定/临时密码，rsipstack handle_client_authenticate 自动处理往返。
            let credential = call_password.as_ref().map(|pw| Credential {
                username: target_device.clone(),
                password: pw.clone(),
                realm: None,
            });
            // 经 signal 透明 Proxy：destination 固定为 signal 监听地址（outbound
            // proxy 语义，报文头域保留 domain）；传输类型随配置（UDP 直发 / TLS
            // 复用注册建立的既有流——lookup 按目标地址命中 connections 表）。
            let mut destination = SipAddr::from(cfg.server);
            destination.r#type = Some(match cfg.transport {
                SipTransport::Udp => rsipstack::sip::Transport::Udp,
                SipTransport::Tls => rsipstack::sip::Transport::Tls,
            });
            let contact = match contact
                .clone()
                .or_else(|| dialog_layer.build_local_contact(None, None).ok())
            {
                Some(c) => c,
                None => {
                    warn!("local contact 构造失败");
                    return;
                }
            };
            let opt = InviteOption {
                caller,
                callee,
                destination: Some(destination),
                content_type: Some("application/sdp".into()),
                offer: Some(offer_sdp.into_bytes()),
                contact,
                call_id: Some(call_id.clone()),
                credential,
                ..Default::default()
            };
            match dialog_layer.do_invite_async(opt, state_tx.clone()) {
                Ok((dlg, join)) => {
                    dialogs.insert(
                        call_id.clone(),
                        CallEntry {
                            dialog: dlg,
                            peer_device: target_device,
                            initiator: true,
                            established: false,
                            final_notified: false,
                            escalate_notified: false,
                        },
                    );
                    // JoinHandle 终局响应回注主循环（302/4xx/6xx 的权威语义）。
                    let final_tx = invite_final_tx.clone();
                    let cid = call_id.clone();
                    tokio::spawn(async move {
                        let resp = match join.await {
                            Ok(Ok((_, resp))) => resp,
                            _ => None,
                        };
                        let _ = final_tx.send((cid, resp));
                    });
                }
                Err(e) => {
                    warn!(%call_id, error = %e, "INVITE 发起失败");
                    let _ = event_tx.send(SipEvent::Rejected {
                        call_id,
                        status: 0,
                        error_code: Some("invite_failed".into()),
                    });
                }
            }
        }
        SipCommand::Ring { call_id } => {
            if let Some(entry) = dialogs.get(&call_id) {
                // 180 必须不带 body（带 body 会被 rsipstack 升为 183）。
                if let Err(e) = entry.dialog.ringing(None, None) {
                    warn!(%call_id, error = %e, "180 Ringing 发送失败");
                }
            }
        }
        SipCommand::Accept {
            call_id,
            answer_sdp,
        } => {
            if let Some(entry) = dialogs.get(&call_id) {
                let ct = Header::Other("Content-Type".into(), "application/sdp".into());
                if let Err(e) = entry
                    .dialog
                    .accept(Some(vec![ct]), Some(answer_sdp.into_bytes()))
                {
                    warn!(%call_id, error = %e, "200 OK(answer) 发送失败");
                }
            }
        }
        SipCommand::Reject {
            call_id,
            error_code,
        } => {
            if let Some(entry) = dialogs.get(&call_id) {
                let status = StatusCode::from(error_code_to_status(&error_code));
                if let Err(e) = entry.dialog.reject(Some(status), Some(error_code)) {
                    warn!(%call_id, error = %e, "拒接响应发送失败");
                }
            }
        }
        SipCommand::RedirectToSfu { call_id } => {
            if let Some(entry) = dialogs.get(&call_id) {
                // rsipstack reject 不能带 Contact——302 语义 + §4.1 确定性推导
                // 使 aerodesk 两端收敛（view AoR = sip:view-<对端设备>@<domain>）。
                if let Err(e) = entry.dialog.reject(
                    Some(StatusCode::MovedTemporarily),
                    Some("aerodesk SFU escalation".into()),
                ) {
                    warn!(%call_id, error = %e, "302 重定向发送失败");
                }
            }
        }
        SipCommand::Hangup { call_id, reason } => {
            if let Some(entry) = dialogs.get(&call_id) {
                let r = if entry.established {
                    match reason {
                        Some(reason) => entry.dialog.bye_with_reason(reason).await,
                        None => entry.dialog.bye().await,
                    }
                } else if entry.initiator {
                    entry.dialog.cancel().await
                } else {
                    // 被叫未接即挂 = 拒接（603）。
                    entry.dialog.reject(Some(StatusCode::Decline), reason)
                };
                if let Err(e) = r {
                    warn!(%call_id, error = %e, "挂断发送失败");
                }
            }
        }
        SipCommand::SendTrickle { call_id, candidate } => {
            if let Some(entry) = dialogs.get(&call_id) {
                let ct = Header::Other("Content-Type".into(), TRICKLE_ICE_CONTENT_TYPE.into());
                let body = encode_trickle(&candidate).into_bytes();
                if let Err(e) = entry.dialog.info(Some(vec![ct]), Some(body)).await {
                    warn!(%call_id, error = %e, "INFO trickle 发送失败");
                }
            }
        }
        // Reregister 由主循环臂直接处理（registration 不在此作用域）。
        SipCommand::Reregister => {}
    }
}

/// 入站事务处理（UAS 腿 + 对话内 BYE/INFO/re-INVITE 路由 + 严格子集 501）。
async fn handle_incoming(
    mut tx: Transaction,
    cfg: &SipClientConfig,
    dialog_layer: &Arc<DialogLayer>,
    state_tx: &DialogStateSender,
    contact: &Option<rsipstack::sip::Uri>,
    dialogs: &mut HashMap<String, CallEntry>,
    event_tx: &std_mpsc::Sender<SipEvent>,
) {
    let method = tx.original.method;

    // 严格子集门禁（规范 §6）。ACK/CANCEL 由事务层吸收，不会到这里。
    if !IMPLEMENTED_METHODS.contains(&method) {
        let _ = tx.reply(StatusCode::NotImplemented).await;
        return;
    }

    match method {
        Method::Options => {
            let allow = Header::Other(
                "Allow".into(),
                IMPLEMENTED_METHODS
                    .iter()
                    .map(|m| m.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            let _ = tx.reply_with(StatusCode::OK, vec![allow], None).await;
        }
        Method::Invite => {
            // re-INVITE（对话内）走 match_dialog；新呼叫走 get_or_create。
            if let Some(mut dlg) = dialog_layer.match_dialog(&tx) {
                tokio::spawn(async move {
                    let _ = dlg.handle(&mut tx).await;
                });
                return;
            }
            let req = tx.original.clone();
            let call_id = match req.call_id_header() {
                Ok(h) => h.value().to_string(),
                Err(_) => {
                    let _ = tx.reply(StatusCode::BadRequest).await;
                    return;
                }
            };
            let from_device = req
                .from_header()
                .ok()
                .and_then(|f| f.uri().ok())
                .and_then(|u| device_from_uri(&u.to_string()).map(str::to_string))
                .unwrap_or_default();
            let offer_sdp = String::from_utf8_lossy(req.body()).to_string();
            match dialog_layer.get_or_create_server_invite(
                &tx,
                state_tx.clone(),
                None,
                contact.clone(),
            ) {
                Ok(dlg) => {
                    // 驱动 server 腿：自动 100 Trying + ACK/CANCEL 吸收（不 spawn
                    // 则 CANCEL 无人 487、ACK 无人 Confirmed）。
                    let mut handle_dlg = dlg.clone();
                    tokio::spawn(async move {
                        let _ = handle_dlg.handle(&mut tx).await;
                    });
                    dialogs.insert(
                        call_id.clone(),
                        CallEntry {
                            dialog: dlg,
                            peer_device: from_device.clone(),
                            initiator: false,
                            established: false,
                            final_notified: false,
                            escalate_notified: false,
                        },
                    );
                    info!(%call_id, from = %from_device, "SIP 来电");
                    let _ = event_tx.send(SipEvent::IncomingCall {
                        call_id,
                        from_device,
                        offer_sdp,
                    });
                }
                Err(e) => {
                    warn!(%call_id, error = %e, "server dialog 创建失败");
                    let _ = tx.reply(StatusCode::ServerInternalError).await;
                }
            }
        }
        Method::Bye => {
            // §4.1 升级识别：BYE 的 Reason 头含 cause=302 → 先上报 EscalatedToSfu，
            // 随后正常 handle（自动 200 + Terminated 时抑制 PeerHangup）。
            let escalate = tx
                .original
                .headers
                .iter()
                .any(|h| matches!(h, Header::Reason(r) if is_escalation_reason(r.value())));
            if escalate && let Ok(h) = tx.original.call_id_header() {
                let cid = h.value().to_string();
                if let Some(entry) = dialogs.get_mut(&cid) {
                    entry.escalate_notified = true;
                    let view = view_aor(&entry.peer_device, &cfg.domain);
                    info!(call_id = %cid, %view, "对端升级至 SFU（BYE cause=302）");
                    let _ = event_tx.send(SipEvent::EscalatedToSfu {
                        call_id: cid,
                        view_aor: view,
                    });
                }
            }
            if let Some(mut dlg) = dialog_layer.match_dialog(&tx) {
                tokio::spawn(async move {
                    let _ = dlg.handle(&mut tx).await;
                });
            } else {
                let _ = tx.reply(StatusCode::CallTransactionDoesNotExist).await;
            }
        }
        Method::Info => {
            if let Some(mut dlg) = dialog_layer.match_dialog(&tx) {
                tokio::spawn(async move {
                    let _ = dlg.handle(&mut tx).await;
                });
            } else {
                let _ = tx.reply(StatusCode::CallTransactionDoesNotExist).await;
            }
        }
        // REGISTER 不面向客户端；ACK/CANCEL 已被事务层吸收。
        _ => {
            let _ = tx.reply(StatusCode::NotImplemented).await;
        }
    }
}

/// 对话状态事件 → 语义事件（双腿合一通道，按 Call-ID 关联）。
async fn handle_dialog_state(
    st: DialogState,
    cfg: &SipClientConfig,
    dialog_layer: &Arc<DialogLayer>,
    dialogs: &mut HashMap<String, CallEntry>,
    event_tx: &std_mpsc::Sender<SipEvent>,
) {
    match st {
        DialogState::Early(id, _resp) => {
            if let Some(entry) = dialogs.get(&id.call_id)
                && entry.initiator
                && !entry.final_notified
            {
                let _ = event_tx.send(SipEvent::Ringing {
                    call_id: id.call_id.clone(),
                });
            }
        }
        DialogState::Confirmed(id, resp) => {
            if let Some(entry) = dialogs.get_mut(&id.call_id) {
                entry.established = true;
                if entry.initiator && !entry.final_notified {
                    entry.final_notified = true;
                    let _ = event_tx.send(SipEvent::Answered {
                        call_id: id.call_id.clone(),
                        answer_sdp: String::from_utf8_lossy(&resp.body).to_string(),
                    });
                }
            }
        }
        DialogState::Info(id, req, handle) => {
            let body = String::from_utf8_lossy(req.body()).to_string();
            match decode_trickle(&body) {
                Some(candidate) => {
                    if dialogs.contains_key(&id.call_id) {
                        let _ = event_tx.send(SipEvent::Trickle {
                            call_id: id.call_id.clone(),
                            candidate,
                        });
                    }
                    let _ = handle.reply(StatusCode::OK).await;
                }
                None => {
                    warn!(call_id = %id.call_id, "INFO sdpfrag 解析失败");
                    let _ = handle.reply(StatusCode::BadRequest).await;
                }
            }
        }
        DialogState::Terminated(id, reason) => {
            handle_terminated(id, reason, cfg, dialog_layer, dialogs, event_tx);
        }
        _ => {}
    }
}

/// Terminated 归一化：区分「对端 BYE」「对端 CANCEL」「终局拒绝」「本端动作」。
fn handle_terminated(
    id: rsipstack::dialog::DialogId,
    reason: TerminatedReason,
    cfg: &SipClientConfig,
    dialog_layer: &Arc<DialogLayer>,
    dialogs: &mut HashMap<String, CallEntry>,
    event_tx: &std_mpsc::Sender<SipEvent>,
) {
    let call_id = id.call_id.clone();
    let Some(entry) = dialogs.get(&call_id) else {
        dialog_layer.remove_dialog(&id);
        return;
    };
    // BYE 方向判定（recon：收 BYE 时 server 腿→UacBye、client 腿→UasBye；
    // 本端 bye() 反之）。
    let peer_bye = matches!(
        (&reason, entry.initiator),
        (TerminatedReason::UasBye, true) | (TerminatedReason::UacBye, false)
    );
    if peer_bye {
        if entry.established && !entry.escalate_notified {
            let _ = event_tx.send(SipEvent::PeerHangup {
                call_id: call_id.clone(),
                reason: None,
            });
        }
    } else if !entry.initiator && matches!(reason, TerminatedReason::UacCancel) {
        // 被叫侧：对端在未接期间 CANCEL → 呼叫结束（关弹窗）。
        if !entry.final_notified {
            let _ = event_tx.send(SipEvent::PeerHangup {
                call_id: call_id.clone(),
                reason: Some("cancelled".into()),
            });
        }
    } else if entry.initiator && !entry.final_notified {
        // 主叫腿终局拒绝（与 invite_final 双通道，先到先报）。
        match &reason {
            TerminatedReason::UasOther(code) | TerminatedReason::UacOther(code) => {
                report_final_status(&call_id, code, cfg, dialogs, event_tx);
            }
            TerminatedReason::UasBusy | TerminatedReason::UacBusy => {
                report_final_status(&call_id, &StatusCode::BusyHere, cfg, dialogs, event_tx);
            }
            TerminatedReason::UasDecline => {
                report_final_status(&call_id, &StatusCode::Decline, cfg, dialogs, event_tx);
            }
            // #503-4：被叫设备有口令而本端未带/带错——栈按质询失败收尾
            // （ProxyAuthRequired），与 invite_final 通道的 407/403 终局双通道去重。
            TerminatedReason::ProxyAuthRequired => {
                report_final_status(
                    &call_id,
                    &StatusCode::ProxyAuthenticationRequired,
                    cfg,
                    dialogs,
                    event_tx,
                );
            }
            TerminatedReason::Timeout => {
                if let Some(entry) = dialogs.get_mut(&call_id) {
                    entry.final_notified = true;
                }
                let _ = event_tx.send(SipEvent::Rejected {
                    call_id: call_id.clone(),
                    status: 408,
                    error_code: Some("timeout".into()),
                });
            }
            _ => {}
        }
    }
    dialogs.remove(&call_id);
    dialog_layer.remove_dialog(&id);
}

/// 主叫腿终局响应码 → Rejected / EscalatedToSfu（302，§4.1 推导会议 AoR）。
fn report_final_status(
    call_id: &str,
    code: &StatusCode,
    cfg: &SipClientConfig,
    dialogs: &mut HashMap<String, CallEntry>,
    event_tx: &std_mpsc::Sender<SipEvent>,
) {
    let status = u16::from(code.clone());
    if let Some(entry) = dialogs.get_mut(call_id) {
        entry.final_notified = true;
        if status == 302 {
            entry.escalate_notified = true;
            let view = view_aor(&entry.peer_device, &cfg.domain);
            info!(%call_id, %view, "呼叫被 302 重定向至 SFU 会议");
            let _ = event_tx.send(SipEvent::EscalatedToSfu {
                call_id: call_id.to_string(),
                view_aor: view,
            });
            return;
        }
    }
    let _ = event_tx.send(SipEvent::Rejected {
        call_id: call_id.to_string(),
        status,
        error_code: status_to_error_code(status).map(str::to_string),
    });
}

/// do_invite_async 的 JoinHandle 终局（302/4xx/6xx 权威；2xx 与 Confirmed 去重）。
fn handle_invite_final(
    call_id: &str,
    resp: Option<Response>,
    cfg: &SipClientConfig,
    dialogs: &mut HashMap<String, CallEntry>,
    event_tx: &std_mpsc::Sender<SipEvent>,
) {
    let Some(entry) = dialogs.get_mut(call_id) else {
        return;
    };
    if entry.final_notified {
        return;
    }
    match resp {
        Some(resp) => {
            let status = u16::from(resp.status_code.clone());
            match status {
                200..=299 => {
                    entry.final_notified = true;
                    entry.established = true;
                    let _ = event_tx.send(SipEvent::Answered {
                        call_id: call_id.to_string(),
                        answer_sdp: String::from_utf8_lossy(&resp.body).to_string(),
                    });
                }
                302 => {
                    entry.final_notified = true;
                    entry.escalate_notified = true;
                    let view = view_aor(&entry.peer_device, &cfg.domain);
                    info!(%call_id, %view, "呼叫被 302 重定向至 SFU 会议");
                    let _ = event_tx.send(SipEvent::EscalatedToSfu {
                        call_id: call_id.to_string(),
                        view_aor: view,
                    });
                }
                _ => {
                    entry.final_notified = true;
                    let _ = event_tx.send(SipEvent::Rejected {
                        call_id: call_id.to_string(),
                        status,
                        error_code: status_to_error_code(status).map(str::to_string),
                    });
                }
            }
        }
        None => {
            entry.final_notified = true;
            let _ = event_tx.send(SipEvent::Rejected {
                call_id: call_id.to_string(),
                status: 0,
                error_code: Some("transport".into()),
            });
        }
    }
}

/// 一次 REGISTER（Digest 质询由 Registration 自动应答），结果上报为语义事件。
async fn do_register(
    registration: &mut Registration,
    server_uri: &rsipstack::sip::Uri,
    aor: &str,
    expires: u32,
    event_tx: &std_mpsc::Sender<SipEvent>,
) {
    match registration
        .register(server_uri.clone(), Some(expires))
        .await
    {
        Ok(resp) if resp.status_code == StatusCode::OK => {
            let granted = registration.expires();
            info!(%aor, expires = granted, "SIP 注册成功");
            let _ = event_tx.send(SipEvent::Registered {
                aor: aor.to_string(),
                expires: granted,
            });
        }
        Ok(resp) => {
            let status = u16::from(resp.status_code.clone());
            let reason = resp.status_code.to_string();
            warn!(%aor, status, %reason, "SIP 注册失败");
            let _ = event_tx.send(SipEvent::RegisterFailed { status, reason });
        }
        Err(e) => {
            warn!(%aor, error = %e, "SIP 注册传输/内部错误");
            let _ = event_tx.send(SipEvent::RegisterFailed {
                status: 0,
                reason: e.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_ca_pem_produces_pem_bundle() {
        let pem = system_ca_pem();
        assert!(
            !pem.is_empty(),
            "系统根证书包不应为空（Windows 证书库可取系统根）"
        );
        let text = String::from_utf8_lossy(&pem);
        assert!(text.contains("-----BEGIN CERTIFICATE-----"));
        assert!(text.contains("-----END CERTIFICATE-----"));
    }

    #[test]
    fn load_ca_pem_file_roundtrip() {
        let path =
            std::env::temp_dir().join(format!("aerodesk-test-ca-{}.pem", std::process::id()));
        let body = b"-----BEGIN CERTIFICATE-----\nZm9v\n-----END CERTIFICATE-----\n";
        std::fs::write(&path, body).unwrap();
        let got = load_ca_pem_file(path.to_str().unwrap()).unwrap();
        assert_eq!(got, body);
        std::fs::remove_file(&path).ok();
        assert!(load_ca_pem_file("Z:\no-such-file.pem").is_err());
    }
}
