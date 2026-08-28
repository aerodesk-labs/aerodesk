//! 通用小工具（服务端与客户端共用的防御样板）。

use std::sync::PoisonError;

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

/// RFC 3264 SDP 方向判定：媒体 m-line 是否「发送媒体」（sendonly/sendrecv/缺省）。
///
/// #598 v0.4：多方会议（§4.1）的会议桥按 offer 方向决定 SFU 角色——
/// 被控端发布方向（sendonly/sendrecv）→ SFU `role=publisher`；
/// 观看端（recvonly/inactive）→ `role=viewer`。与 SFU 侧准入同构
/// （原 sfu/shard.rs 私有实现迁此共享，防两处漂移）。
pub fn offer_sends_media(sdp: &str) -> bool {
    // RFC 3264：m-line 无方向属性时缺省为 sendrecv。
    // 因此媒体 m-line 必须显式 a=recvonly / a=inactive 才视为“不发送”。
    let mut in_media = false; // 当前是否位于媒体 m-line 内
    let mut seen_direction = false; // 当前 m-line 是否已有方向属性
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            // 进入新 m-line：上一个媒体 m-line 若缺省方向 → sendrecv（发送）。
            if in_media && !seen_direction {
                return true;
            }
            let mtype = rest.split_whitespace().next().unwrap_or("");
            in_media = mtype != "application";
            seen_direction = false;
            continue;
        }
        if !in_media {
            continue;
        }
        if let Some(rest) = line.strip_prefix("a=") {
            if rest.starts_with("sendonly") || rest.starts_with("sendrecv") {
                return true;
            }
            if rest.starts_with("recvonly") || rest.starts_with("inactive") {
                seen_direction = true;
            }
        }
    }
    in_media && !seen_direction
}
