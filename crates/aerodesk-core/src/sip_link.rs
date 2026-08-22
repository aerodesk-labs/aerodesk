//! SIP 呼叫信令链路（#552 核心接线）。
//!
//! 把 [`crate::protocol::sip_client`] 的客户端 UA（语义事件 API）收敛为与
//! [`crate::signal_presence::SignalPresence`] 同形的常驻面：`start/poll/stop` +
//! `status` + `take_events`，上层（desktop/agent/移动壳）接线时几乎只换构造。
//!
//! 分层（规范 §8/§9）：core 状态机按**语义事件**迁移而不按报文迁移——本模块是
//! `SipEvent` ↔ core 语义的**唯一翻译点**；媒体核心不 import SIP 类型（Trickle
//! 事件里的候选经 [`crate::protocol::sip::TrickleCandidate`] 传递，媒体层自行
//! 转 str0m 类型）。
//!
//! UA 自带 REGISTER 生命周期（启动注册 + expires/2 定时刷新 + 断开重连由本层
//! 驱动：[`SipEvent::RegisterFailed`] / [`SipEvent::EndpointStopped`] 触发退避重启，
//! [`SipEvent::Registered`] 复位）。与 WSS 路径的区别：UA 无 per-消息心跳，SIP
//! 的保活是 REGISTER 刷新（本层不再发 Ping）。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use aerodesk_protocol::sip::TrickleCandidate;
use aerodesk_protocol::sip_client::{
    SipClientConfig, SipClientHandle, SipCommand, SipEvent, SipTlsConfig, SipTransport,
    start_sip_client,
};

/// SIP 链路连接状态（与 [`crate::signal_presence::PresenceStatus`] 同形）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SipLinkStatus {
    /// 未启动，或已停止/退出并清理 UA。
    Stopped,
    /// 已拉起 UA，等待注册（Connecting 中的网络 I/O 在 UA 线程内）。
    Connecting { attempt: u32 },
    /// 已注册（AoR 命名与 §1 一致：`sip:<device>@<domain>`）。
    Online { aor: String },
    /// 注册失败/UA 退出，等待 `delay` 后重启 UA（退避 1s→30s 封顶）。
    Reconnecting {
        attempt: u32,
        delay: Duration,
        last_error: String,
    },
}

impl SipLinkStatus {
    pub fn is_online(&self) -> bool {
        matches!(self, Self::Online { .. })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Connecting { .. } => "connecting",
            Self::Online { .. } => "online",
            Self::Reconnecting { .. } => "reconnecting",
        }
    }
}

/// SIP 链路收到的语义事件（与 SignalMessage 呼叫族同构，#550 映射表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SipLinkEvent {
    /// 来电（被叫侧）：`from_device` 为对端设备 ID。
    IncomingCall {
        call_id: String,
        from_device: String,
        offer_sdp: String,
    },
    /// 对端响铃（主叫侧）。
    Ringing { call_id: String },
    /// 对端接听（answer SDP 端到端透传）。
    Answered { call_id: String, answer_sdp: String },
    /// 呼叫被拒/失败（`status` 为 SIP 响应码；`error_code` 为规范 §3 机器码）。
    Rejected {
        call_id: String,
        status: u16,
        error_code: Option<String>,
    },
    /// 对端挂断（对话内 BYE；未接呼叫场景亦指对端 CANCEL）。
    PeerHangup {
        call_id: String,
        reason: Option<String>,
    },
    /// 已升级至 SFU 会议 AoR（§4.1）：上层应按 `view_aor` 重新呼叫（媒体切 SFU）。
    EscalatedToSfu { call_id: String, view_aor: String },
    /// 对端 trickle 候选（INFO sdpfrag）。
    Trickle {
        call_id: String,
        candidate: TrickleCandidate,
    },
}

/// SIP 链路配置（归一化自现有 signal 配置：room=domain、auth_token=Digest 口令）。
#[derive(Debug, Clone)]
pub struct SipLinkConfig {
    /// 设备 ID（Digest username，§1）。
    pub device_id: String,
    /// SIP 域（= WSS 时代的 room/设备 ID 语境；AoR 路由键）。
    pub domain: String,
    /// Digest 口令（= auth_token，迁移期同一凭据，§8）。
    pub password: String,
    /// signal 的 SIP 监听地址（实际投递目标）。
    pub server: SocketAddr,
    /// 传输（UDP=内网/调试；TLS=公网默认加密）。
    pub transport: SipTransport,
    /// TLS 配置（[`SipTransport::Tls`] 时必填）。
    pub tls: Option<SipTlsConfig>,
    /// REGISTER 过期秒数（刷新周期 = expires/2）。
    pub register_expires: u32,
}

impl SipLinkConfig {
    pub fn to_client_config(&self) -> SipClientConfig {
        SipClientConfig {
            device_id: self.device_id.clone(),
            domain: self.domain.clone(),
            password: self.password.clone(),
            server: self.server,
            transport: self.transport,
            tls: self.tls.clone(),
            register_expires: self.register_expires,
        }
    }
}

/// 常驻 SIP 呼叫信令链路。
///
/// 线程模型：UA 自带独立 tokio runtime（start_sip_client 内部 thread）；本层
/// 是纯 std 同步面（poll 非阻塞 drain，事件队列供上层 take_events 消费）。
pub struct SipCallLink {
    config: SipLinkConfig,
    handle: Option<SipClientHandle>,
    status: SipLinkStatus,
    attempt: u32,
    retry_at: Option<Instant>,
    retry_delay: Duration,
    events: VecDeque<SipLinkEvent>,
}

impl SipCallLink {
    /// 创建未启动的链路（初始 [`SipLinkStatus::Stopped`]）。
    pub fn new(config: SipLinkConfig) -> Self {
        Self {
            config,
            handle: None,
            status: SipLinkStatus::Stopped,
            attempt: 1,
            retry_at: None,
            retry_delay: Duration::from_secs(1),
            events: VecDeque::new(),
        }
    }

    pub fn status(&self) -> SipLinkStatus {
        self.status.clone()
    }

    pub fn is_online(&self) -> bool {
        self.status().is_online()
    }

    /// 取出并清空已收到的事件队列。
    pub fn take_events(&mut self) -> Vec<SipLinkEvent> {
        self.events.drain(..).collect()
    }

    /// 启动链路（拉起 UA 并注册）。重复调用不打断当前状态。
    pub fn start(&mut self) -> SipLinkStatus {
        if self.status == SipLinkStatus::Stopped {
            self.attempt = 1;
            self.retry_delay = Duration::from_secs(1);
            self.retry_at = None;
            self.events.clear();
            self.spawn_ua(self.attempt);
        }
        self.status.clone()
    }

    /// 停止并清理链路（UA 注销 + 线程 join，与 SignalPresence::stop 同形）。
    pub fn stop(&mut self) -> SipLinkStatus {
        if let Some(handle) = self.handle.take() {
            handle.shutdown();
        }
        self.status = SipLinkStatus::Stopped;
        self.retry_at = None;
        self.events.clear();
        self.status.clone()
    }

    /// 驱动状态机：处理到期的重连/重启、drain UA 事件（非阻塞）。
    pub fn poll(&mut self) -> SipLinkStatus {
        if self.status == SipLinkStatus::Stopped {
            return self.status.clone();
        }
        if matches!(self.status, SipLinkStatus::Reconnecting { .. }) {
            let due = self.retry_at.is_some_and(|at| Instant::now() >= at);
            if due {
                self.retry_at = None;
                if self.handle.is_some() {
                    // UA 仍存活（注册失败非进程级故障）：Reregister 立即重试。
                    let _ = self.handle.as_ref().unwrap().send(SipCommand::Reregister);
                    self.status = SipLinkStatus::Connecting {
                        attempt: self.attempt,
                    };
                } else {
                    // UA 已死（EndpointStopped/启动失败）：拉起重启。
                    self.spawn_ua(self.attempt);
                }
            }
        }
        self.drain_events();
        self.status.clone()
    }

    // -- core → UA 命令（与 SignalPresence 的接听/拒接/挂断同形） --

    /// 发起呼叫（INVITE + SDP offer；call_id 由上层生成 → Call-ID）。
    pub fn call(
        &mut self,
        target_device: &str,
        call_id: &str,
        offer_sdp: &str,
    ) -> Result<(), String> {
        self.send(SipCommand::Call {
            target_device: target_device.into(),
            call_id: call_id.into(),
            offer_sdp: offer_sdp.into(),
        })
    }

    /// 响铃（180；被叫侧弹出确认窗时发）。
    pub fn ring(&mut self, call_id: &str) -> Result<(), String> {
        self.send(SipCommand::Ring {
            call_id: call_id.into(),
        })
    }

    /// 接听（200 OK + SDP answer）。
    pub fn accept(&mut self, call_id: &str, answer_sdp: &str) -> Result<(), String> {
        self.send(SipCommand::Accept {
            call_id: call_id.into(),
            answer_sdp: answer_sdp.into(),
        })
    }

    /// 拒接（规范 §3 error_code → 响应码）。
    pub fn reject(&mut self, call_id: &str, error_code: &str) -> Result<(), String> {
        self.send(SipCommand::Reject {
            call_id: call_id.into(),
            error_code: error_code.into(),
        })
    }

    /// 升级重定向（§4.1：对新入呼回 302，呼叫转移至会议 AoR）。
    pub fn redirect_to_sfu(&mut self, call_id: &str) -> Result<(), String> {
        self.send(SipCommand::RedirectToSfu {
            call_id: call_id.into(),
        })
    }

    /// 挂断（已建立→BYE（reason 透传 Reason 头；升级场景用
    /// [`crate::protocol::sip::ESCALATE_BYE_REASON`]）；未建立→CANCEL/拒绝）。
    pub fn hangup(&mut self, call_id: &str, reason: Option<&str>) -> Result<(), String> {
        self.send(SipCommand::Hangup {
            call_id: call_id.into(),
            reason: reason.map(str::to_string),
        })
    }

    /// 发送 trickle 候选（INFO sdpfrag）。
    pub fn send_trickle(
        &mut self,
        call_id: &str,
        candidate: &TrickleCandidate,
    ) -> Result<(), String> {
        self.send(SipCommand::SendTrickle {
            call_id: call_id.into(),
            candidate: candidate.clone(),
        })
    }

    /// 立即重注册（不等刷新周期；如网络切换后）。
    pub fn reregister(&mut self) -> Result<(), String> {
        self.send(SipCommand::Reregister)
    }

    // -- 内部 --

    fn send(&mut self, cmd: SipCommand) -> Result<(), String> {
        self.handle
            .as_ref()
            .ok_or("SIP 链路未启动或已停止")?
            .send(cmd)
    }

    fn spawn_ua(&mut self, attempt: u32) {
        let handle = match start_sip_client(self.config.to_client_config()) {
            Ok(h) => h,
            Err(e) => {
                self.schedule_retry(attempt, format!("SIP UA 启动失败: {e}"));
                return;
            }
        };
        self.handle = Some(handle);
        self.status = SipLinkStatus::Connecting { attempt };
    }

    fn schedule_retry(&mut self, attempt: u32, error: String) {
        self.attempt = attempt.saturating_add(1);
        // 退避：1s * 2^(attempt-1)，封顶 30s（与 presence 重连策略同构）。
        let shift = (attempt as i32 - 1).clamp(0, 31);
        let multiplier = 1u32.checked_shl(shift as u32).unwrap_or(u32::MAX);
        let delay = Duration::from_secs(u64::from(multiplier).min(30).max(1));
        self.retry_delay = delay;
        self.retry_at = Some(Instant::now() + delay);
        self.status = SipLinkStatus::Reconnecting {
            attempt,
            delay,
            last_error: error,
        };
    }

    fn drain_events(&mut self) {
        // 每 poll 至多 32 条（防单次 poll 被事件洪峰卡死）。
        for _ in 0..32 {
            // 分块借用以避免与 on_event(&mut self) 长期共存借用。
            let ev = match self.handle.as_ref() {
                Some(handle) => handle.recv_event(Duration::ZERO),
                None => break,
            };
            match ev {
                Some(ev) => self.on_event(ev),
                None => break,
            }
        }
    }

    fn on_event(&mut self, ev: SipEvent) {
        match ev {
            SipEvent::Registered { aor, .. } => {
                self.attempt = 1;
                self.retry_delay = Duration::from_secs(1);
                self.retry_at = None;
                self.status = SipLinkStatus::Online { aor };
            }
            SipEvent::RegisterFailed { status, reason } => {
                // UA 仍存活（内部定时刷新会继续重试），本层额外按退避驱动
                // 立即 Reregister，加速首次失败后的恢复。
                let attempt = self.attempt;
                self.retry_at = None;
                self.attempt = self.attempt.saturating_add(1);
                let error = format!("register failed (status={status}): {reason}");
                self.retry_delay = self.backoff(attempt);
                self.retry_at = Some(Instant::now() + self.retry_delay);
                self.status = SipLinkStatus::Reconnecting {
                    attempt,
                    delay: self.retry_delay,
                    last_error: error,
                };
                // 到期后由 poll 发 Reregister（而非重启 UA——UA 活着，刷新即恢复）。
            }
            SipEvent::IncomingCall {
                call_id,
                from_device,
                offer_sdp,
            } => self.events.push_back(SipLinkEvent::IncomingCall {
                call_id,
                from_device,
                offer_sdp,
            }),
            SipEvent::Ringing { call_id } => {
                self.events.push_back(SipLinkEvent::Ringing { call_id });
            }
            SipEvent::Answered {
                call_id,
                answer_sdp,
            } => self.events.push_back(SipLinkEvent::Answered {
                call_id,
                answer_sdp,
            }),
            SipEvent::Rejected {
                call_id,
                status,
                error_code,
            } => self.events.push_back(SipLinkEvent::Rejected {
                call_id,
                status,
                error_code,
            }),
            SipEvent::PeerHangup { call_id, reason } => {
                self.events
                    .push_back(SipLinkEvent::PeerHangup { call_id, reason });
            }
            SipEvent::EscalatedToSfu { call_id, view_aor } => {
                self.events
                    .push_back(SipLinkEvent::EscalatedToSfu { call_id, view_aor });
            }
            SipEvent::Trickle { call_id, candidate } => {
                self.events
                    .push_back(SipLinkEvent::Trickle { call_id, candidate });
            }
            SipEvent::EndpointStopped => {
                // UA 通道已断（线程退出）：丢弃旧句柄并按退避重启。若同轮
                // drain 已经因 RegisterFailed 排好了重试（启动失败 = 先报
                // RegisterFailed 再停），沿用已排计划，不重复计数。
                self.handle = None;
                if !matches!(self.status, SipLinkStatus::Reconnecting { .. }) {
                    let attempt = self.attempt;
                    self.schedule_retry(attempt, "SIP UA 已停止".into());
                }
            }
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let shift = (attempt.saturating_sub(1)).min(5);
        Duration::from_secs(1u64 << shift)
    }
}

impl Drop for SipCallLink {
    fn drop(&mut self) {
        // 显式停关（注销 + join）；未停时兜底清理（不再补发注销——best-effort）。
        if let Some(handle) = self.handle.take() {
            handle.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerodesk_protocol::sip::{ESCALATE_BYE_REASON, TrickleCandidate};

    /// 无服务端可用时的链路配置：TLS 连 127.0.0.1:1——TCP 拒绝立即失败
    /// （UDP 到闭端口的行为随 OS 的 ICMP 处理而异，不可作确定性测试输入）。
    fn offline_config() -> SipLinkConfig {
        SipLinkConfig {
            device_id: "AD-TEST".into(),
            domain: "aerodesk.test".into(),
            password: "tok-test".into(),
            server: "127.0.0.1:1".parse().unwrap(),
            transport: SipTransport::Tls,
            tls: Some(SipTlsConfig {
                ca_certs: Vec::new(), // TCP 连接即失败，证书校验不会走到
                sni_hostname: Some("aerodesk.test".into()),
                client_cert: None,
                client_key: None,
            }),
            register_expires: 60,
        }
    }

    #[test]
    fn starts_connecting_and_stops_clean() {
        let mut link = SipCallLink::new(offline_config());
        assert_eq!(link.status(), SipLinkStatus::Stopped);
        // 启动即 Connecting（UA 拉起是异步的）。
        assert_eq!(link.start(), SipLinkStatus::Connecting { attempt: 1 });
        assert_eq!(link.stop(), SipLinkStatus::Stopped);
        assert_eq!(link.status(), SipLinkStatus::Stopped);
    }

    #[test]
    fn command_methods_error_before_start() {
        let mut link = SipCallLink::new(offline_config());
        assert!(link.call("AD-X", "c-1", "sdp").is_err());
        assert!(link.ring("c-1").is_err());
        assert!(link.accept("c-1", "sdp").is_err());
        assert!(link.reject("c-1", "busy").is_err());
        assert!(link.redirect_to_sfu("c-1").is_err());
        assert!(link.hangup("c-1", None).is_err());
        assert!(link.reregister().is_err());
        assert!(link.send_trickle("c-1", &trickle()).is_err());
    }

    #[test]
    fn register_failure_enters_reconnecting_with_backoff() {
        let mut link = SipCallLink::new(offline_config());
        link.start();
        // 走到 RegisterFailed（无服务端：连接拒绝 → UA 报失败事件）。
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            link.poll();
            if matches!(link.status(), SipLinkStatus::Reconnecting { .. }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "5s 内应进入 Reconnecting，当前 {:?}",
                link.status()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let SipLinkStatus::Reconnecting {
            attempt,
            delay,
            last_error,
        } = link.status()
        else {
            unreachable!()
        };
        assert_eq!(attempt, 1);
        assert!(delay >= Duration::from_secs(1));
        assert!(!last_error.is_empty());
        link.stop();
    }

    #[test]
    fn status_helpers_report_online_only_when_online() {
        let status = SipLinkStatus::Online {
            aor: "sip:AD-TEST@aerodesk.test".into(),
        };
        assert!(status.is_online());
        assert_eq!(status.as_str(), "online");
        assert!(!SipLinkStatus::Stopped.is_online());
        assert_eq!(SipLinkStatus::Stopped.as_str(), "stopped");
        assert_eq!(
            SipLinkStatus::Connecting { attempt: 1 }.as_str(),
            "connecting"
        );
    }

    /// 事件翻译：SipEvent（含 302/BYE-302 升级）→ 链路事件，字段端到端保真。
    /// 翻译在 on_event（poll 内）；此处以状态机单元面覆盖升级语义（§4.1）。
    #[test]
    fn escalation_events_carry_view_aor_and_reason() {
        let mut link = SipCallLink::new(offline_config());
        link.on_event(SipEvent::EscalatedToSfu {
            call_id: "c-1".into(),
            view_aor: "sip:view-AD-CALLEE@aerodesk.test".into(),
        });
        let evs = link.take_events();
        assert_eq!(evs.len(), 1);
        assert_eq!(
            evs[0],
            SipLinkEvent::EscalatedToSfu {
                call_id: "c-1".into(),
                view_aor: "sip:view-AD-CALLEE@aerodesk.test".into(),
            }
        );
        // 升级 BYE 的 Reason 常量在协议层（core 转发语义用）。
        assert!(ESCALATE_BYE_REASON.contains("cause=302"));
    }

    fn trickle() -> TrickleCandidate {
        TrickleCandidate {
            candidate: "candidate:1 1 UDP 2130706431 192.0.2.1 5000 typ host".into(),
            sdp_mid: Some("0".into()),
            sdp_m_line_index: Some(0),
        }
    }
}
