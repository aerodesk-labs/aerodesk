//! macOS 保持唤醒（#334「SystemWakeLock」平台抽象）。
//!
//! 用 `caffeinate`（macOS 内置）实现：
//! - `display=true`：`caffeinate -d` 阻止显示器休眠（远控采集期间必需，#315）
//! - `display=false`：`caffeinate -i` 阻止系统空闲休眠
//!
//! guard Drop/`release` 时 kill 子进程释放锁（与原 `capture::KeepAwake` 语义等价）。

use aerodesk_core::platform::{SystemWakeLock, WakeGuard};

/// macOS 保持唤醒实现（caffeinate）。
#[derive(Debug, Clone, Copy, Default)]
pub struct MacSystemWakeLock;

impl SystemWakeLock for MacSystemWakeLock {
    fn acquire(&self, display: bool) -> Result<Box<dyn WakeGuard>, String> {
        let arg = if display { "-d" } else { "-i" };
        let child = std::process::Command::new("caffeinate")
            .arg(arg)
            .spawn()
            .map_err(|e| format!("caffeinate spawn failed: {e}"))?;
        let kind = if display { "显示器" } else { "系统" };
        tracing::info!("已保持{kind}唤醒（caffeinate {arg}，guard 释放时自动结束）");
        Ok(Box::new(CaffeinateGuard { child: Some(child) }))
    }
}

/// caffeinate 子进程句柄：Drop/release 时 kill。
struct CaffeinateGuard {
    child: Option<std::process::Child>,
}

impl WakeGuard for CaffeinateGuard {
    fn release(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for CaffeinateGuard {
    fn drop(&mut self) {
        WakeGuard::release(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_returns_guard_and_drops() {
        let lock = MacSystemWakeLock;
        let guard = lock.acquire(true).expect("caffeinate 应可启动");
        // 存活期间锁持有；Drop 释放（不 panic）。
        drop(guard);
        let _ = lock.acquire(false).expect("caffeinate -i 应可启动");
    }

    #[test]
    fn object_safe_extension_point() {
        let lock: Box<dyn SystemWakeLock> = Box::new(MacSystemWakeLock);
        let mut guard = lock.acquire(true).unwrap();
        guard.release(); // 显式释放幂等
        guard.release();
    }
}
