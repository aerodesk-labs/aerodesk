//! 通用小工具（跨 crate 共用的防御样板）。

/// 观看端「对端停流」判死/提示统一阈值（#487 审查批次 2）：此前三端各写各的
/// （macos_media 10s 判死、generic_viewer 10s 提示、CLI 8s 判死）——统一为一个
/// 常量，语义（判死 or 仅提示）仍由调用方按场景决定。
pub const NO_MEDIA_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// 毒锁恢复（自 aerodesk-protocol 再导出，服务端与客户端共用）。
pub use aerodesk_protocol::util::lock_recover;

#[cfg(test)]
mod tests {
    use super::*;

    /// #487 批次 2 统一后的判死/提示阈值钉值（RULE_阈值变更：新阈值落地即带验证）。
    /// 原值：CLI 判死 8s / macos_media 判死 10s / generic_viewer 提示 10s；
    /// 放宽至统一 10s 的理由：三端对齐，避免观看端比桌面端先判死。
    #[test]
    fn no_media_deadline_is_pinned() {
        assert_eq!(NO_MEDIA_DEADLINE, std::time::Duration::from_secs(10));
    }
}
