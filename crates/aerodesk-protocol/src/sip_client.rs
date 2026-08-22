//! SIP 客户端 UA（#552 slice 1：注册/presence + 生命周期）。
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
//! slice 1 范围：REGISTER（Digest 质询自动应答 + 周期刷新 + 关停注销）与
//! OPTIONS/501 应答器。呼叫面命令（Call/Accept/Reject/Hangup/SendTrickle）先
//! 定义稳定 API，slice 2 实现（收到时仅记 warn，不产生线报文）。
//!
//! 传输：slice 1 仅 UDP（内网/调试，规范 §0 可选项）；TLS 默认传输随呼叫面
//! slice 落地（公网强制加密的口径不变）。

use std::net::SocketAddr;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::registration::Registration;
use rsipstack::sip::{Method, StatusCode};
use rsipstack::transaction::endpoint::EndpointBuilder;
use rsipstack::transport::SipConnection;
use rsipstack::transport::TransportLayer;
use rsipstack::transport::udp::UdpConnection;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::sip::{IMPLEMENTED_METHODS, PROTOCOL_VERSION, TrickleCandidate, device_aor};

/// 信令传输（slice 1 仅 UDP；TLS 随呼叫面 slice）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SipTransport {
    /// UDP（内网/调试；规范 §0 传输矩阵可选项）。
    Udp,
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
    /// 传输（slice 1 仅 [`SipTransport::Udp`]）。
    pub transport: SipTransport,
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
    /// 来电（Call 同构；被叫侧）。slice 2 接线。
    IncomingCall {
        call_id: String,
        from_device: String,
        offer_sdp: String,
    },
    /// 对端响铃（CallRinging 同构；主叫侧）。slice 2 接线。
    Ringing { call_id: String },
    /// 对端接听（CallAccepted 同构；answer_sdp 端到端透传）。slice 2 接线。
    Answered { call_id: String, answer_sdp: String },
    /// 呼叫被拒/失败（CallRejected 同构；error_code 按规范 §3 由响应码映射）。
    Rejected {
        call_id: String,
        status: u16,
        error_code: Option<String>,
    },
    /// 对端挂断（Hangup 同构；对话内 BYE）。slice 2 接线。
    PeerHangup {
        call_id: String,
        reason: Option<String>,
    },
    /// 呼叫已升级至 SFU 会议 AoR（§4.1：BYE cause=302 或 302 重定向）。
    /// core 应按 view_aor 重新发起呼叫（媒体切 SFU）。slice 2 接线。
    EscalatedToSfu { call_id: String, view_aor: String },
    /// 对端 trickle 候选（IceCandidate 同构，INFO sdpfrag）。slice 2 接线。
    Trickle {
        call_id: String,
        candidate: TrickleCandidate,
    },
    /// UA 已停止（通道关闭前的最后一个事件；core 应停止轮询或重启 UA）。
    EndpointStopped,
}

/// core → UA 的命令。呼叫面命令在 slice 2 接线（slice 1 收到仅记 warn）。
#[derive(Debug, Clone)]
pub enum SipCommand {
    /// 发起呼叫（Call 同构：INVITE + SDP offer；call_id 由 core 生成 → Call-ID）。
    Call {
        target_device: String,
        call_id: String,
        offer_sdp: String,
    },
    /// 接听（CallAccepted 同构：200 OK + SDP answer）。
    Accept { call_id: String, answer_sdp: String },
    /// 拒接（CallRejected 同构：规范 §3 error_code → 响应码）。
    Reject { call_id: String, error_code: String },
    /// 挂断（Hangup 同构：BYE，reason → Reason 头）。
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

    /// 关停 UA：注销（best-effort）→ 停 endpoint → join 线程。
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

async fn run_client(
    cfg: SipClientConfig,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<SipCommand>,
    event_tx: std_mpsc::Sender<SipEvent>,
    cancel: CancellationToken,
) {
    if let Err(e) = run_client_inner(&cfg, &mut cmd_rx, &event_tx, &cancel).await {
        let _ = event_tx.send(SipEvent::RegisterFailed {
            status: 0,
            reason: e,
        });
    }
    // 关停：best-effort 注销（expires=0），让 presence 立即下线而非等过期。
    // 注意 endpoint/registration 已被 inner 消费——注销在 inner 内做。
    debug!(device = %cfg.device_id, "SIP 客户端 UA 退出");
}

async fn run_client_inner(
    cfg: &SipClientConfig,
    cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SipCommand>,
    event_tx: &std_mpsc::Sender<SipEvent>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let tl = TransportLayer::new(cancel.clone());
    match cfg.transport {
        SipTransport::Udp => {
            // 客户端用 add_transport（非 add_listener）：get_via 须能看到该传输。
            let conn = UdpConnection::create_connection(
                "0.0.0.0:0".parse().unwrap(),
                None,
                Some(cancel.clone()),
            )
            .await
            .map_err(|e| format!("SIP/UDP 客户端传输创建失败: {e}"))?;
            tl.add_transport(SipConnection::from(conn));
        }
    }

    let endpoint = EndpointBuilder::new()
        .with_user_agent(PROTOCOL_VERSION)
        .with_transport_layer(tl)
        .with_cancel_token(cancel.clone())
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

    // 入站应答器（slice 1）：OPTIONS → 200 + Allow；其余一律 501（规范 §6 纪律）。
    // INVITE/BYE/INFO 的呼叫面接线在 slice 2。
    {
        let cancel_in = cancel.clone();
        tokio::spawn(async move {
            loop {
                let mut tx = tokio::select! {
                    _ = cancel_in.cancelled() => break,
                    maybe = incoming.recv() => match maybe {
                        Some(tx) => tx,
                        None => break,
                    },
                };
                match tx.original.method {
                    Method::Options => {
                        let allow = rsipstack::sip::Header::Other(
                            "Allow".into(),
                            IMPLEMENTED_METHODS
                                .iter()
                                .map(|m| m.to_string())
                                .collect::<Vec<_>>()
                                .join(", "),
                        );
                        let _ = tx.reply_with(StatusCode::OK, vec![allow], None).await;
                    }
                    _ => {
                        let _ = tx.reply(StatusCode::NotImplemented).await;
                    }
                }
            }
        });
    }

    let server_uri: rsipstack::sip::Uri = format!("sip:{}", cfg.domain)
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

    // 首次注册（启动即报 presence）。
    do_register(
        &mut registration,
        &server_uri,
        &aor,
        cfg.register_expires,
        event_tx,
    )
    .await;

    // 命令 / 周期刷新 / 关停 主循环。
    let refresh = Duration::from_secs((cfg.register_expires as u64 / 2).max(5));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            cmd = cmd_rx.recv() => match cmd {
                Some(SipCommand::Reregister) => {
                    do_register(&mut registration, &server_uri, &aor, cfg.register_expires, event_tx).await;
                }
                Some(other) => {
                    warn!(cmd = ?other, "呼叫面命令 slice 2 接线，当前忽略");
                }
                None => break,
            },
            _ = tokio::time::sleep(refresh) => {
                do_register(&mut registration, &server_uri, &aor, cfg.register_expires, event_tx).await;
            }
        }
    }

    // 关停注销（best-effort，2s 兜底不阻塞退出）。
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        registration.register(server_uri, Some(0)),
    )
    .await;
    info!(device = %cfg.device_id, "SIP 客户端 UA 已关停并注销");
    Ok(())
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
