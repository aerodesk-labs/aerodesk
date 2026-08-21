//! 常驻信令连接（presence）管理器。
//!
//! 复用 [`crate::signaling::WsSignalClient`] 的 WSS Join 协议：presence 连接只做
//! `connect -> join` 并保持 WebSocket 不关闭，不创建 WebRTC 媒体会话。上层通过
//! [`SignalPresence::poll`] 驱动连接状态机与自动重连，通过
//! [`SignalPresence::status`] 查询「连接中 / 已在线 / 失败重连」状态。
//!
//! 现有信令协议没有独立的 presence/register 消息；保持在线的最小协议语义就是
//! Join 后不关闭 WebSocket。断开检测依赖 TCP read timeout，重连策略为指数退避 +
//! 上限封顶。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::protocol::signal::{Role, SignalMessage};
use crate::signal_call::{CallDecision, CallState, DEFAULT_CALL_TIMEOUT, IncomingCallInfo};
use crate::signaling::{WsRecvError, WsSignalClient};

/// 信令 presence 连接状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceStatus {
    /// 未启动，或已停止/退出并清理连接。
    Stopped,
    /// 正在建立 WSS 连接或等待 Join 应答。
    Connecting { attempt: u32 },
    /// 已在线并完成 Join；`peer_id` 由信令服务器分配。
    Online { peer_id: String, room: String },
    /// 连接失败或已断开，等待 `delay` 后自动重连。
    Reconnecting {
        attempt: u32,
        delay: Duration,
        last_error: String,
    },
}

impl PresenceStatus {
    /// 是否处于可被呼叫/发现的在线状态。
    pub fn is_online(&self) -> bool {
        matches!(self, Self::Online { .. })
    }

    /// 状态机阶段名（供 UI/日志映射）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Connecting { .. } => "connecting",
            Self::Online { .. } => "online",
            Self::Reconnecting { .. } => "reconnecting",
        }
    }
}

/// presence 收到的一条信令事件（被叫侧视角）。
///
/// 上层 UI 可周期性 [SignalPresence::take_events] 消费这些事件，并据此启动/停止
/// 按需 publisher。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceEvent {
    /// 收到 viewer 呼叫，`target` 为本机设备 ID（房间名）。
    IncomingCall {
        call_id: String,
        from: String,
        target: String,
        timeout_ms: Option<u64>,
    },
    /// 呼叫方挂断。
    Hangup {
        call_id: String,
        from: String,
        reason: Option<String>,
    },
    /// 发布/响铃超时，presence 已代表本端挂断。
    CallTimeout { call_id: String, from: String },
}

/// presence 连接配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceConfig {
    /// 信令服务器地址（可省略协议与 `/ws`，交给 `normalize_signal_url` 归一化）。
    pub server: String,
    /// 常驻房间（设备 ID / 房间名）。
    pub room: String,
    /// presence 连接使用的角色。设备「可被呼叫」通常使用 [`Role::Publisher`]，
    /// 但协议层不绑定媒体发布；该连接只完成 Join，不创建 SDP/ICE。
    pub role: Role,
    /// Join 认证 token（可选）。
    pub auth_token: Option<String>,
    /// 收到呼叫后是否自动接听（默认 true，对应「被呼叫时再出流」）。
    pub auto_accept: bool,
    /// 呼叫缺省超时；Call.timeout_ms 未指定时使用（默认 30s）。
    pub call_timeout: Duration,
    /// 首次重连退避（默认 1s）。
    pub initial_backoff: Duration,
    /// 最大重连退避（默认 30s）。
    pub max_backoff: Duration,
}

impl PresenceConfig {
    /// 使用默认退避策略（1s 起、30s 封顶）。
    pub fn new(server: impl Into<String>, room: impl Into<String>, role: Role) -> Self {
        Self {
            server: server.into(),
            room: room.into(),
            role,
            auth_token: None,
            auto_accept: true,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }

    /// 覆盖指数退避参数。
    pub fn with_backoff(mut self, initial_backoff: Duration, max_backoff: Duration) -> Self {
        self.initial_backoff = initial_backoff;
        self.max_backoff = max_backoff;
        self
    }

    /// 设置 Join 认证 token。
    pub fn with_auth_token(mut self, auth_token: impl Into<String>) -> Self {
        self.auth_token = Some(auth_token.into());
        self
    }

    /// 设置收到呼叫后是否自动接听。
    pub fn with_auto_accept(mut self, auto_accept: bool) -> Self {
        self.auto_accept = auto_accept;
        self
    }

    /// 设置呼叫缺省超时（Call.timeout_ms 未指定时生效）。
    pub fn with_call_timeout(mut self, call_timeout: Duration) -> Self {
        self.call_timeout = call_timeout;
        self
    }
}

/// 纯状态机：描述 presence 连接的状态流转与重连退避，不直接做网络 I/O。
///
/// 状态流转：
/// `Stopped -> Connecting -> Online -> Reconnecting -> Connecting -> ...`
#[derive(Debug, Clone)]
pub struct PresenceStateMachine {
    config: PresenceConfig,
    status: PresenceStatus,
    attempt: u32,
    retry_delay: Duration,
}

impl PresenceStateMachine {
    /// 创建状态机，初始为 [`PresenceStatus::Stopped`]。
    pub fn new(config: PresenceConfig) -> Self {
        let retry_delay = config.initial_backoff;
        Self {
            config,
            status: PresenceStatus::Stopped,
            attempt: 1,
            retry_delay,
        }
    }

    pub fn config(&self) -> &PresenceConfig {
        &self.config
    }

    pub fn status(&self) -> PresenceStatus {
        self.status.clone()
    }

    /// 下一次连接尝试编号。
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// 当前已计算出的重连等待时长。
    pub fn retry_delay(&self) -> Duration {
        self.retry_delay
    }

    /// 启动状态机；仅在 `Stopped` 时切入 `Connecting`。
    pub fn start(&mut self) -> PresenceStatus {
        if self.status == PresenceStatus::Stopped {
            self.begin_attempt();
        }
        self.status()
    }

    /// 开始一次连接尝试。
    pub fn begin_attempt(&mut self) -> PresenceStatus {
        self.status = PresenceStatus::Connecting {
            attempt: self.attempt,
        };
        self.status()
    }

    /// 连接 + Join 成功，进入在线状态并重置退避。
    pub fn on_online(&mut self, peer_id: impl Into<String>) -> PresenceStatus {
        self.status = PresenceStatus::Online {
            peer_id: peer_id.into(),
            room: self.config.room.clone(),
        };
        self.attempt = 1;
        self.retry_delay = self.config.initial_backoff;
        self.status()
    }

    /// 连接失败或断开，进入 `Reconnecting` 并返回下一次重连等待时长。
    pub fn on_failure(&mut self, error: impl Into<String>) -> Duration {
        let error = error.into();
        self.retry_delay = self.next_retry_delay();
        self.status = PresenceStatus::Reconnecting {
            attempt: self.attempt,
            delay: self.retry_delay,
            last_error: error,
        };
        self.attempt = self.attempt.saturating_add(1);
        self.retry_delay
    }

    /// `on_failure` 的语义别名（用于已在线连接断开时调用）。
    pub fn on_disconnected(&mut self, error: impl Into<String>) -> Duration {
        self.on_failure(error)
    }

    /// 停止状态机并重置退避。
    pub fn stop(&mut self) -> PresenceStatus {
        self.status = PresenceStatus::Stopped;
        self.attempt = 1;
        self.retry_delay = self.config.initial_backoff;
        self.status()
    }

    fn next_retry_delay(&self) -> Duration {
        // 第 n 次失败（attempt=n）等待 initial_backoff * 2^(n-1)，封顶 max_backoff。
        let shift = self.attempt.saturating_sub(1).min(31);
        let multiplier = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
        let initial_ms = self.config.initial_backoff.as_millis();
        let max_ms = self.config.max_backoff.as_millis();
        let delay_ms = initial_ms
            .saturating_mul(u128::from(multiplier))
            .min(max_ms)
            .max(1);
        let delay_ms = u64::try_from(delay_ms).unwrap_or(u64::MAX);
        Duration::from_millis(delay_ms)
    }
}

/// 常驻信令连接管理器。
///
/// 这是上层 UI 的最小接线面：创建后 `start()`，然后在主循环/定时器里周期性
/// `poll()`；`poll()` 只做一次连接尝试或一小段（默认 250ms）非阻塞读取，状态
/// 变化可通过 `status()` 查询。
pub struct SignalPresence {
    machine: PresenceStateMachine,
    client: Option<WsSignalClient>,
    retry_at: Option<Instant>,
    stopped: bool,
    read_timeout: Duration,
    /// #539 心跳节拍：每 5s 发 ws Ping（驱动服务端读循环 drain 发送队列，
    /// 呼叫等可靠消息可送达；同时维持连接保活）。
    last_ping: Instant,
    call: CallState,
    events: VecDeque<PresenceEvent>,
}

impl SignalPresence {
    /// 创建未启动的 presence 管理器。
    pub fn new(config: PresenceConfig) -> Self {
        Self {
            machine: PresenceStateMachine::new(config),
            client: None,
            retry_at: None,
            stopped: true,
            read_timeout: Duration::from_millis(250),
            last_ping: Instant::now(),
            call: CallState::default(),
            events: VecDeque::new(),
        }
    }

    /// 覆盖在线状态下的读取超时（默认 250ms）。短超时能让 `poll` 更及时响应 `stop`。
    pub fn with_read_timeout(mut self, read_timeout: Duration) -> Self {
        self.read_timeout = read_timeout;
        self
    }

    pub fn config(&self) -> &PresenceConfig {
        self.machine.config()
    }

    pub fn status(&self) -> PresenceStatus {
        self.machine.status()
    }

    pub fn is_online(&self) -> bool {
        self.status().is_online()
    }

    /// 取出并清空已收到的事件队列。
    pub fn take_events(&mut self) -> Vec<PresenceEvent> {
        self.events.drain(..).collect()
    }

    /// 当前待接听呼叫（仅在 `auto_accept=false` 时可能停留在 `Incoming`）。
    pub fn incoming_call(&self) -> Option<&IncomingCallInfo> {
        self.call.incoming()
    }

    /// 当前已接听呼叫。
    pub fn active_call(&self) -> Option<&crate::signal_call::ActiveCallInfo> {
        self.call.active()
    }

    /// 接听当前呼叫：发送 CallRinging + CallAccepted，并启动超时计时。
    pub fn accept_call(&mut self) -> Result<(), String> {
        let Some(active) = self.call.accept(Instant::now()) else {
            return Err("no incoming call".into());
        };
        self.send_call_ringing(&active)?;
        self.send_call_accepted(&active)
    }

    /// 拒绝当前待接听呼叫。`code` 为结构化拒绝码（#539：timeout/user_rejected 等）。
    pub fn reject_call(&mut self, reason: Option<&str>, code: Option<&str>) -> Result<(), String> {
        let Some(incoming) = self.call.reject() else {
            return Err("no incoming call".into());
        };
        self.send_signal(SignalMessage::CallRejected {
            from: self.own_peer_id()?,
            to: incoming.from,
            call_id: incoming.call_id,
            reason: reason.map(str::to_string),
            error_code: code.map(str::to_string),
        })
    }

    /// 本端主动挂断当前呼叫（无进行中呼叫时为空操作）。
    pub fn hangup_call(&mut self, reason: Option<&str>) -> Result<(), String> {
        let Some(end) = self.call.hangup() else {
            return Ok(());
        };
        self.send_signal(SignalMessage::Hangup {
            from: self.own_peer_id()?,
            to: end.from,
            call_id: end.call_id,
            reason: reason.map(str::to_string),
        })
    }

    /// 启动常驻连接；重复调用不会打断当前状态。
    pub fn start(&mut self) -> PresenceStatus {
        self.stopped = false;
        self.retry_at = None;
        self.call = CallState::default();
        self.events.clear();
        self.machine.start()
    }

    /// 停止并清理常驻连接（断开 WebSocket、回到 `Stopped`）。
    pub fn stop(&mut self) -> PresenceStatus {
        self.stopped = true;
        self.client = None;
        self.retry_at = None;
        self.call = CallState::default();
        self.events.clear();
        self.machine.stop()
    }

    /// 驱动状态机：连接、读取在线消息、检测断开并安排重连。
    pub fn poll(&mut self) -> PresenceStatus {
        if self.stopped {
            return self.status();
        }

        match self.machine.status() {
            PresenceStatus::Stopped => {}
            PresenceStatus::Connecting { .. } => self.connect_if_needed(),
            PresenceStatus::Reconnecting { .. } => {
                let due = self
                    .retry_at
                    .is_some_and(|retry_at| Instant::now() >= retry_at);
                if due {
                    self.retry_at = None;
                    self.machine.begin_attempt();
                    self.connect_if_needed();
                }
            }
            PresenceStatus::Online { .. } => self.poll_online(),
        }

        self.status()
    }

    fn connect_if_needed(&mut self) {
        if self.client.is_some() {
            return;
        }

        match self.connect_once() {
            Ok(peer_id) => {
                // 重连成功后清除上一连接的呼叫与事件，避免旧呼叫误触发按需 publisher。
                self.call = CallState::default();
                self.events.clear();
                self.machine.on_online(peer_id);
                self.retry_at = None;
            }
            Err(error) => {
                let delay = self.machine.on_failure(error);
                self.retry_at = Some(Instant::now() + delay);
            }
        }
    }

    fn connect_once(&mut self) -> Result<String, String> {
        let config = self.machine.config();
        let mut signal = WsSignalClient::connect(&config.server)
            .map_err(|error| format!("signal connect: {error}"))?;
        let (peer_id, _turn) = signal
            .join(&config.room, config.role, config.auth_token.as_deref())
            .map_err(|error| format!("signal join: {error}"))?;
        self.client = Some(signal);
        Ok(peer_id)
    }

    fn poll_online(&mut self) {
        // #539：每 5s 发 ws Ping——服务端 session_loop 的阻塞 next() 收到即
        // 返回并 drain 发送队列（呼叫可送达）；同时维持连接保活（纯收不发
        // 会被中间层/服务端判死，LESSON_WS 心跳）。
        if self.last_ping.elapsed() >= Duration::from_secs(5) {
            self.last_ping = Instant::now();
            if let Some(client) = self.client.as_mut() {
                let _ = client.send_ping();
            }
        }
        let result = match self.client.as_mut() {
            Some(client) => client.recv_timeout(self.read_timeout),
            None => {
                self.call = CallState::default();
                let delay = self.machine.on_disconnected("presence transport missing");
                self.retry_at = Some(Instant::now() + delay);
                return;
            }
        };

        match result {
            Ok(msg) => self.handle_signal_message(msg),
            Err(WsRecvError::Timeout) => self.expire_call(Instant::now()),
            Err(WsRecvError::Closed(error)) => {
                self.client = None;
                self.call = CallState::default();
                let delay = self.machine.on_disconnected(error);
                self.retry_at = Some(Instant::now() + delay);
            }
        }
    }

    fn handle_signal_message(&mut self, msg: SignalMessage) {
        match msg {
            SignalMessage::Call {
                from,
                target,
                call_id,
                timeout_ms,
            } => self.handle_incoming_call(call_id, from, target, timeout_ms),
            SignalMessage::Hangup {
                from,
                call_id,
                reason,
                ..
            } => {
                if let Some(end) = self.call.remote_hangup(&call_id) {
                    self.events.push_back(PresenceEvent::Hangup {
                        call_id: end.call_id,
                        from: end.from,
                        reason,
                    });
                    let _ = from; // `to` 由服务器填充，presence 只需按 call_id 匹配。
                }
            }
            // #487 对端离开（主控断开/退出）：结束当前呼叫（Active/Incoming 的
            // 对端离开 → 清理状态 + 事件，上层据此停发布；主控不主动发 Hangup，
            // 由 Signal 的 PeerLeft 广播兜底）。
            SignalMessage::PeerLeft { peer_id } => {
                let ended = match &self.call {
                    CallState::Active(a) if a.from == peer_id => self.call.hangup(),
                    CallState::Incoming(c) if c.from == peer_id => self.call.hangup(),
                    _ => None,
                };
                if let Some(end) = ended {
                    self.events.push_back(PresenceEvent::Hangup {
                        call_id: end.call_id,
                        from: end.from,
                        reason: Some("peer_left".into()),
                    });
                }
            }
            _ => {}
        }
    }

    fn handle_incoming_call(
        &mut self,
        call_id: String,
        from: String,
        target: String,
        timeout_ms: Option<u64>,
    ) {
        let incoming = IncomingCallInfo::new(call_id, from, timeout_ms, self.config().call_timeout);
        match self.call.on_call(incoming) {
            CallDecision::Accept(incoming) => {
                self.events.push_back(PresenceEvent::IncomingCall {
                    call_id: incoming.call_id.clone(),
                    from: incoming.from.clone(),
                    target,
                    timeout_ms,
                });
                if self.config().auto_accept {
                    match self.accept_call() {
                        Ok(()) => {}
                        Err(error) => {
                            tracing::debug!("presence auto-accept call failed: {error}");
                        }
                    }
                }
            }
            CallDecision::Busy(incoming) => {
                // 已有进行中的呼叫：直接回 busy，不打断当前会话。
                let _ = self.send_signal(SignalMessage::CallRejected {
                    from: self.own_peer_id().unwrap_or_else(|_| "signal".into()),
                    to: incoming.from,
                    call_id: incoming.call_id,
                    reason: Some("busy".into()),
                    error_code: Some("busy".into()),
                });
            }
        }
    }

    fn send_call_ringing(
        &mut self,
        active: &crate::signal_call::ActiveCallInfo,
    ) -> Result<(), String> {
        self.send_signal(SignalMessage::CallRinging {
            from: self.own_peer_id()?,
            to: active.from.clone(),
            call_id: active.call_id.clone(),
        })
    }

    fn send_call_accepted(
        &mut self,
        active: &crate::signal_call::ActiveCallInfo,
    ) -> Result<(), String> {
        self.send_signal(SignalMessage::CallAccepted {
            from: self.own_peer_id()?,
            to: active.from.clone(),
            call_id: active.call_id.clone(),
        })
    }

    fn expire_call(&mut self, now: Instant) {
        let Some(end) = self.call.expire(now) else {
            return;
        };
        self.events.push_back(PresenceEvent::CallTimeout {
            call_id: end.call_id.clone(),
            from: end.from.clone(),
        });
        let _ = self.send_signal(SignalMessage::Hangup {
            from: self.own_peer_id().unwrap_or_else(|_| "signal".into()),
            to: end.from,
            call_id: end.call_id,
            reason: Some("timeout".into()),
        });
    }

    fn own_peer_id(&self) -> Result<String, String> {
        self.client
            .as_ref()
            .and_then(|client| client.peer_id().map(str::to_string))
            .ok_or_else(|| "presence not joined".into())
    }

    fn send_signal(&mut self, msg: SignalMessage) -> Result<(), String> {
        self.client
            .as_mut()
            .ok_or("presence not joined")?
            .send_signal(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> PresenceConfig {
        PresenceConfig::new("127.0.0.1:3003", "device-room", Role::Publisher)
            .with_backoff(Duration::from_millis(100), Duration::from_millis(1600))
    }

    fn reconnect(
        status: PresenceStatus,
        expected_attempt: u32,
        expected_delay_ms: u64,
        expected_error: &str,
    ) {
        match status {
            PresenceStatus::Reconnecting {
                attempt,
                delay,
                last_error,
            } => {
                assert_eq!(attempt, expected_attempt);
                assert_eq!(delay, Duration::from_millis(expected_delay_ms));
                assert_eq!(last_error, expected_error);
            }
            other => panic!("expected Reconnecting, got {other:?}"),
        }
    }

    #[test]
    fn starts_stopped_then_connecting() {
        let mut machine = PresenceStateMachine::new(test_config());
        assert_eq!(machine.status(), PresenceStatus::Stopped);
        assert_eq!(machine.start(), PresenceStatus::Connecting { attempt: 1 });
    }

    #[test]
    fn online_resets_attempt_and_backoff() {
        let mut machine = PresenceStateMachine::new(test_config());
        machine.start();
        machine.on_failure("first");
        assert_eq!(machine.attempt(), 2);

        assert_eq!(
            machine.on_online("peer-1"),
            PresenceStatus::Online {
                peer_id: "peer-1".into(),
                room: "device-room".into(),
            }
        );
        assert_eq!(machine.attempt(), 1);
        assert_eq!(machine.retry_delay(), Duration::from_millis(100));
    }

    #[test]
    fn failures_use_exponential_backoff_and_cap() {
        let mut machine = PresenceStateMachine::new(test_config());
        machine.start();

        let cases: &[(u32, u64)] = &[(1, 100), (2, 200), (3, 400), (4, 800), (5, 1600), (6, 1600)];

        for (expected_attempt, expected_delay_ms) in cases {
            assert_eq!(
                machine.begin_attempt(),
                PresenceStatus::Connecting {
                    attempt: *expected_attempt,
                }
            );
            let delay = machine.on_failure(format!("fail-{expected_attempt}"));
            assert_eq!(delay, Duration::from_millis(*expected_delay_ms));
            reconnect(
                machine.status(),
                *expected_attempt,
                *expected_delay_ms,
                &format!("fail-{expected_attempt}"),
            );
        }
    }

    #[test]
    fn disconnect_from_online_schedules_reconnect() {
        let mut machine = PresenceStateMachine::new(test_config());
        machine.start();
        machine.on_online("peer-1");

        let delay = machine.on_disconnected("connection lost");
        assert_eq!(delay, Duration::from_millis(100));
        reconnect(machine.status(), 1, 100, "connection lost");
        assert_eq!(
            machine.begin_attempt(),
            PresenceStatus::Connecting { attempt: 2 }
        );
    }

    #[test]
    fn stop_resets_machine_and_manager_stops_without_io() {
        let mut machine = PresenceStateMachine::new(test_config());
        machine.start();
        machine.on_online("peer-1");
        assert_eq!(machine.stop(), PresenceStatus::Stopped);
        assert_eq!(machine.attempt(), 1);

        let mut presence = SignalPresence::new(test_config());
        assert_eq!(presence.status(), PresenceStatus::Stopped);
        assert_eq!(presence.start(), PresenceStatus::Connecting { attempt: 1 });
        assert_eq!(presence.stop(), PresenceStatus::Stopped);
        assert_eq!(presence.poll(), PresenceStatus::Stopped);
    }

    #[test]
    fn config_defaults_auto_accept_and_call_timeout() {
        let config = test_config();
        assert!(config.auto_accept, "被叫端默认应自动接听");
        assert_eq!(config.call_timeout, DEFAULT_CALL_TIMEOUT);
        assert!(!config.clone().with_auto_accept(false).auto_accept);
        assert_eq!(
            config
                .clone()
                .with_call_timeout(Duration::from_secs(5))
                .call_timeout,
            Duration::from_secs(5)
        );
    }

    #[test]
    fn presence_starts_with_no_call_or_events() {
        let mut presence = SignalPresence::new(test_config());
        assert!(presence.take_events().is_empty());
        assert!(presence.incoming_call().is_none());
        assert!(presence.active_call().is_none());
    }

    #[test]
    fn status_helpers_report_online_only_in_online_state() {
        let status = PresenceStatus::Online {
            peer_id: "peer-1".into(),
            room: "room".into(),
        };
        assert!(status.is_online());
        assert_eq!(status.as_str(), "online");
        assert!(!PresenceStatus::Stopped.is_online());
    }

    /// #487 回归：对端离开（PeerLeft 广播）结束活跃呼叫——清理状态 + Hangup 事件。
    #[test]
    fn peer_left_ends_active_call() {
        let mut presence = SignalPresence::new(test_config());
        presence.call = CallState::Active(crate::signal_call::ActiveCallInfo {
            call_id: "call-1".into(),
            from: "caller-peer".into(),
            deadline: Instant::now() + Duration::from_secs(60),
        });
        presence.handle_signal_message(SignalMessage::PeerLeft {
            peer_id: "caller-peer".into(),
        });
        assert!(presence.active_call().is_none(), "对端离开应清除 Active");
        let evs = presence.take_events();
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], PresenceEvent::Hangup { from, .. } if from == "caller-peer"));
        // 无关 peer 离开不影响当前呼叫。
        presence.call = CallState::Active(crate::signal_call::ActiveCallInfo {
            call_id: "call-2".into(),
            from: "caller-peer".into(),
            deadline: Instant::now() + Duration::from_secs(60),
        });
        presence.handle_signal_message(SignalMessage::PeerLeft {
            peer_id: "other-peer".into(),
        });
        assert!(presence.active_call().is_some(), "无关 peer 离开不影响");
        assert!(presence.take_events().is_empty());
    }
}
