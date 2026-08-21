//! 呼叫状态机（presence 被叫侧）。
//!
//! 该模块只描述「收到呼叫 → 响铃/接听 → 挂断/超时」的状态流转，不直接做网络 I/O；
//! 发送 CallRinging/CallAccepted/CallRejected/Hangup 由 [`crate::signal_presence::SignalPresence`]
//! 按状态机输出执行。

use std::time::{Duration, Instant};

/// 默认呼叫超时：`Call.timeout_ms` 缺省时被叫端按 30s 自动挂断。
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// 一条进入呼叫（被叫端视角）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingCallInfo {
    pub call_id: String,
    /// 呼叫方 peer_id。
    pub from: String,
    /// 响铃/发布超时。
    pub timeout: Duration,
}

impl IncomingCallInfo {
    /// 按协议字段构造；`timeout_ms` 为 `None` 时使用 `default_timeout`。
    pub fn new(
        call_id: impl Into<String>,
        from: impl Into<String>,
        timeout_ms: Option<u64>,
        default_timeout: Duration,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            from: from.into(),
            timeout: timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(default_timeout),
        }
    }
}

/// 已接听、正在等待媒体发布/挂断的呼叫。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveCallInfo {
    pub call_id: String,
    /// 呼叫方 peer_id。
    pub from: String,
    /// 到达该时间仍未挂断时，被叫端主动停止 publisher 并发送 Hangup。
    pub deadline: Instant,
}

/// 呼叫结束信息（用于发送挂断或抛出事件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEnd {
    pub call_id: String,
    pub from: String,
}

/// 新呼叫的准入决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallDecision {
    /// 当前空闲，接受响铃。
    Accept(IncomingCallInfo),
    /// 已有一通呼叫，拒绝新呼叫。
    Busy(IncomingCallInfo),
}

/// 被叫侧呼叫状态机。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CallState {
    /// 没有进行中的呼叫。
    #[default]
    Idle,
    /// 已收到呼叫，等待自动接听或上层确认。
    Incoming(IncomingCallInfo),
    /// 已接听，等待挂断或超时。
    Active(ActiveCallInfo),
}

impl CallState {
    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    pub fn incoming(&self) -> Option<&IncomingCallInfo> {
        match self {
            Self::Incoming(call) => Some(call),
            _ => None,
        }
    }

    pub fn active(&self) -> Option<&ActiveCallInfo> {
        match self {
            Self::Active(call) => Some(call),
            _ => None,
        }
    }

    /// 处理新呼叫：空闲则转入 `Incoming`，否则保持原状态并返回 `Busy`。
    pub fn on_call(&mut self, call: IncomingCallInfo) -> CallDecision {
        if self.is_idle() {
            *self = Self::Incoming(call.clone());
            CallDecision::Accept(call)
        } else {
            CallDecision::Busy(call)
        }
    }

    /// 接听当前呼叫并进入 `Active`；无 `Incoming` 时返回 `None`。
    pub fn accept(&mut self, now: Instant) -> Option<ActiveCallInfo> {
        let Self::Incoming(call) = self else {
            return None;
        };
        let active = ActiveCallInfo {
            call_id: call.call_id.clone(),
            from: call.from.clone(),
            // #539 实测：30s deadline 会让活跃会话（媒体在推）被无条件挂断——
            // Windows 被控每 30s 停流。活跃呼叫持续到显式挂断；24h 仅作防悬挂
            // 兜底（正常会话远短于此）。
            deadline: now + Duration::from_secs(24 * 3600),
        };
        *self = Self::Active(active.clone());
        Some(active)
    }

    /// 拒绝当前 `Incoming` 呼叫。
    pub fn reject(&mut self) -> Option<IncomingCallInfo> {
        let Self::Incoming(call) = self else {
            return None;
        };
        let call = call.clone();
        *self = Self::Idle;
        Some(call)
    }

    /// 本端主动挂断：清除当前呼叫并返回对端信息。
    pub fn hangup(&mut self) -> Option<CallEnd> {
        let end = match self {
            Self::Incoming(call) => CallEnd {
                call_id: call.call_id.clone(),
                from: call.from.clone(),
            },
            Self::Active(call) => CallEnd {
                call_id: call.call_id.clone(),
                from: call.from.clone(),
            },
            Self::Idle => return None,
        };
        *self = Self::Idle;
        Some(end)
    }

    /// 远端挂断：仅当 `call_id` 与当前呼叫匹配时结束呼叫。
    pub fn remote_hangup(&mut self, call_id: &str) -> Option<CallEnd> {
        let matches = match self {
            Self::Incoming(call) => call.call_id == call_id,
            Self::Active(call) => call.call_id == call_id,
            Self::Idle => false,
        };
        if matches { self.hangup() } else { None }
    }

    /// 检查是否已到发布超时；超时则结束呼叫并返回对端信息。
    pub fn expire(&mut self, now: Instant) -> Option<CallEnd> {
        let Self::Active(call) = self else {
            return None;
        };
        if call.deadline <= now {
            let end = CallEnd {
                call_id: call.call_id.clone(),
                from: call.from.clone(),
            };
            *self = Self::Idle;
            Some(end)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, timeout_ms: Option<u64>) -> IncomingCallInfo {
        IncomingCallInfo::new(id, "caller", timeout_ms, DEFAULT_CALL_TIMEOUT)
    }

    #[test]
    fn incoming_timeout_defaults_and_override() {
        assert_eq!(call("c", None).timeout, DEFAULT_CALL_TIMEOUT);
        assert_eq!(call("c", Some(1_500)).timeout, Duration::from_millis(1_500));
    }

    #[test]
    fn idle_accepts_first_call() {
        let mut state = CallState::default();
        assert!(state.is_idle());
        assert_eq!(
            state.on_call(call("c1", None)),
            CallDecision::Accept(call("c1", None))
        );
        assert!(state.incoming().is_some());
    }

    #[test]
    fn busy_rejects_second_call_without_changing_state() {
        let mut state = CallState::default();
        state.on_call(call("c1", None));
        let before = state.clone();
        assert_eq!(
            state.on_call(call("c2", None)),
            CallDecision::Busy(call("c2", None))
        );
        assert_eq!(state, before);
    }

    #[test]
    fn accept_reject_hangup_and_remote_hangup_flow() {
        let now = Instant::now();
        let mut state = CallState::default();
        state.on_call(call("c1", Some(30_000)));

        let active = state.accept(now).expect("incoming call should accept");
        assert_eq!(active.call_id, "c1");
        assert_eq!(active.from, "caller");
        // #539：活跃呼叫 deadline 为 24h 防悬挂兜底（远大于响铃超时 30s）。
        assert!(active.deadline > now + Duration::from_secs(3600));
        assert!(state.active().is_some());

        assert_eq!(
            state.remote_hangup("c1"),
            Some(CallEnd {
                call_id: "c1".into(),
                from: "caller".into()
            })
        );
        assert!(state.is_idle());
    }

    #[test]
    fn local_hangup_and_reject_clear_state() {
        let mut state = CallState::default();
        state.on_call(call("c1", None));
        assert_eq!(
            state.reject(),
            Some(call("c1", None)),
            "incoming call can be rejected"
        );
        assert!(state.is_idle());

        state.on_call(call("c2", None));
        state.accept(Instant::now()).unwrap();
        assert_eq!(
            state.hangup(),
            Some(CallEnd {
                call_id: "c2".into(),
                from: "caller".into()
            })
        );
        assert!(state.is_idle());
    }

    #[test]
    fn expire_only_clears_matching_active_call() {
        let now = Instant::now();
        let mut state = CallState::default();
        state.on_call(call("c1", Some(10)));
        state.accept(now).unwrap();

        // #539：活跃呼叫 deadline 为 24h 兜底——短于 deadline 不 expire。
        let before_deadline = now + Duration::from_secs(24 * 3600 - 1);
        assert_eq!(state.expire(before_deadline), None);
        assert!(state.active().is_some());

        assert_eq!(
            state.expire(now + Duration::from_secs(24 * 3600 + 1)),
            Some(CallEnd {
                call_id: "c1".into(),
                from: "caller".into()
            })
        );
        assert!(state.is_idle());
    }
}
