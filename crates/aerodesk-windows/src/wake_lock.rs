//! Windows 保持唤醒（#334「SystemWakeLock」平台抽象）。
//!
//! `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED [| ES_DISPLAY_REQUIRED])`
//! 阻止系统/显示器在流媒体采集或播放期间休眠；`release`/Drop 时以 `ES_CONTINUOUS`
//! 单独调用清除标志（等价 `caffeinate` 的 macOS 行为）。

use aerodesk_core::platform::{SystemWakeLock, WakeGuard};

/// Windows 保持唤醒锁（线程级，无子进程）。
pub struct WindowsSystemWakeLock;

impl SystemWakeLock for WindowsSystemWakeLock {
    fn acquire(&self, display: bool) -> Result<Box<dyn WakeGuard>, String> {
        const ES_CONTINUOUS: u32 = 0x8000_0000;
        const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
        const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetThreadExecutionState(es_flags: u32) -> u32;
        }
        let mut flags = ES_CONTINUOUS | ES_SYSTEM_REQUIRED;
        if display {
            flags |= ES_DISPLAY_REQUIRED;
        }
        // SAFETY: SetThreadExecutionState 为线程级 API，任意线程可调；失败返回 0。
        let prev = unsafe { SetThreadExecutionState(flags) };
        if prev == 0 {
            return Err("SetThreadExecutionState failed".to_string());
        }
        Ok(Box::new(WindowsWakeGuard { released: false }))
    }
}

/// 唤醒锁句柄：release/Drop 时清除标志。
struct WindowsWakeGuard {
    released: bool,
}

impl WakeGuard for WindowsWakeGuard {
    fn release(&mut self) {
        if !self.released {
            self.released = true;
            // ES_CONTINUOUS 单独调用会清除之前设置的标志。
            const ES_CONTINUOUS: u32 = 0x8000_0000;
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn SetThreadExecutionState(es_flags: u32) -> u32;
            }
            // SAFETY: 与 acquire 同 API；清除标志失败仅影响唤醒状态，忽略返回值。
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS);
            }
        }
    }
}

impl Drop for WindowsWakeGuard {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #334：acquire → release → drop 不 panic；环境不支持（返回 0）时打印跳过。
    #[test]
    fn acquire_release_drop() {
        let lock = WindowsSystemWakeLock;
        let mut guard = match lock.acquire(true) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("wake lock unavailable in this session: {e}");
                return;
            }
        };
        guard.release();
        drop(guard);
        let _ = lock
            .acquire(false)
            .expect("SetThreadExecutionState 应可重复获取");
    }
}
