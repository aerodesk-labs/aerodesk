//! 通用小工具（跨 crate 共用的防御样板）。

/// 观看端「对端停流」判死/提示统一阈值（#487 审查批次 2）：此前三端各写各的
/// （macos_media 10s 判死、generic_viewer 10s 提示、CLI 8s 判死）——统一为一个
/// 常量，语义（判死 or 仅提示）仍由调用方按场景决定。
pub const NO_MEDIA_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// 毒锁恢复（自 aerodesk-protocol 再导出，服务端与客户端共用）。
pub use aerodesk_protocol::util::lock_recover;
