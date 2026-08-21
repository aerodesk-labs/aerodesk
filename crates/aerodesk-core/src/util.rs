//! 通用小工具（跨 crate 共用的防御样板）。

use std::sync::PoisonError;

/// 观看端「对端停流」判死/提示统一阈值（#487 审查批次 2）：此前三端各写各的
/// （macos_media 10s 判死、generic_viewer 10s 提示、CLI 8s 判死）——统一为一个
/// 常量，语义（判死 or 仅提示）仍由调用方按场景决定。
pub const NO_MEDIA_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// 毒锁恢复：持有锁的线程 panic 后取回可能半更新的状态继续服务，并记 warn
/// 日志（区别于静默吞掉——peer 连接线程 panic 不应拖垮整个服务器，但恢复点
/// 需要可见，否则计数漂移永久化且无迹可查）。
///
/// 用法：`m.lock().unwrap_or_else(lock_recover)`（Mutex/RwLock 读写锁通用）。
/// 对注册表/计数类锁如需 fail-fast（panic 即 bug，应暴露而非吞掉），调用点
/// 自行用 `.lock().unwrap()`。
pub fn lock_recover<T>(e: PoisonError<T>) -> T {
    tracing::warn!("mutex poisoned, recovering with possibly inconsistent state");
    e.into_inner()
}
