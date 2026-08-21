//! 跨进程稳定错误码（#487 审查批次 3 / #13）。
//!
//! 所有 wire 层错误以稳定错误码传输；错误文本仅作人读消息（可随语言/版本
//! 演进），程序分支只认 [`ErrorCode`]。此前散落各处的字符串码（CallRejected
//! 的 `timeout` / `user_rejected` / `busy` / `offline`、cmd 响应的
//! `blocked by policy` 等）统一收敛于此，生产端与消费端共用一份定义。

/// 稳定错误码。`as_str()` 即 wire 字符串（契约，勿改拼写）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// 命令为空。
    EmptyCommand,
    /// 被策略拦截（危险命令/路径/进程）。
    BlockedByPolicy,
    /// 子进程 spawn 失败。
    SpawnFailed,
    /// 等待子进程退出失败。
    WaitFailed,
    /// 执行器并发满载。
    Busy,
    /// 文件/进程不存在。
    NotFound,
    /// 权限不足。
    PermissionDenied,
    /// I/O 错误（读/写/进程枚举等）。
    IoError,
    /// 输入无效（坏 base64、非法参数等）。
    InvalidInput,
    /// 超时。
    Timeout,
    /// 文件超限。
    FileTooLarge,
    /// 被叫用户主动拒绝（CallRejected 既有码）。
    UserRejected,
    /// 对端不在线（CallRejected 既有码）。
    Offline,
    /// 被叫端未开启被控（CallRejected 既有码）。
    ControlDisabled,
    /// 已取消。
    Cancelled,
    /// 不支持的操作。
    Unsupported,
    /// 未分类内部错误。
    Internal,
}

impl ErrorCode {
    /// wire 字符串（契约：拼写改动属破坏性变更，须先评审）。
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::EmptyCommand => "empty_command",
            ErrorCode::BlockedByPolicy => "blocked_by_policy",
            ErrorCode::SpawnFailed => "spawn_failed",
            ErrorCode::WaitFailed => "wait_failed",
            ErrorCode::Busy => "busy",
            ErrorCode::NotFound => "not_found",
            ErrorCode::PermissionDenied => "permission_denied",
            ErrorCode::IoError => "io_error",
            ErrorCode::InvalidInput => "invalid_input",
            ErrorCode::Timeout => "timeout",
            ErrorCode::FileTooLarge => "file_too_large",
            ErrorCode::UserRejected => "user_rejected",
            ErrorCode::Offline => "offline",
            ErrorCode::ControlDisabled => "control_disabled",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Internal => "internal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_wire_strings() {
        // 拼写即契约；CallRejected 既有码保持兼容。
        assert_eq!(ErrorCode::Busy.as_str(), "busy");
        assert_eq!(ErrorCode::Timeout.as_str(), "timeout");
        assert_eq!(ErrorCode::UserRejected.as_str(), "user_rejected");
        assert_eq!(ErrorCode::Offline.as_str(), "offline");
        assert_eq!(ErrorCode::ControlDisabled.as_str(), "control_disabled");
        assert_eq!(ErrorCode::EmptyCommand.as_str(), "empty_command");
        assert_eq!(ErrorCode::BlockedByPolicy.as_str(), "blocked_by_policy");
        assert_eq!(ErrorCode::IoError.as_str(), "io_error");
    }
}
