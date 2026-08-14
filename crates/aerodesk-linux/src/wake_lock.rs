//! Linux 保持唤醒（#334「SystemWakeLock」平台抽象）。
//!
//! 用 `systemd-inhibit`（systemd 桌面/服务端发行版内置）实现：
//! - `display=true`：`--what=sleep:idle`，阻止系统休眠并避免会话进入
//!    idle 动作（显示器熄屏/锁屏由 logind idle action 触发）
//! - `display=false`：`--what=sleep`，只阻止系统休眠
//!
//! `systemd-inhibit` 会阻塞执行其尾随命令直到被终止；guard Drop/`release`
//! 时 kill 子进程释放锁。非 systemd 环境（容器/非 systemd 发行版）返回 Err，
//! 由上层降级为无唤醒锁继续运行。

use aerodesk_core::platform::{SystemWakeLock, WakeGuard};

/// 构造 `systemd-inhibit` 参数（命令行为 `tail -f /dev/null` 保活）。
fn systemd_inhibit_args(display: bool) -> Vec<String> {
    let what = if display { "sleep:idle" } else { "sleep" };
    let why = if display {
        "AeroDesk remote desktop capture active"
    } else {
        "AeroDesk remote desktop session active"
    };
    vec![
        "--what".to_string(),
        what.to_string(),
        "--who".to_string(),
        "AeroDesk".to_string(),
        "--why".to_string(),
        why.to_string(),
        "tail".to_string(),
        "-f".to_string(),
        "/dev/null".to_string(),
    ]
}

/// Linux 保持唤醒实现（systemd-inhibit）。
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxSystemWakeLock;

impl SystemWakeLock for LinuxSystemWakeLock {
    fn acquire(&self, display: bool) -> Result<Box<dyn WakeGuard>, String> {
        let child = std::process::Command::new("systemd-inhibit")
            .args(systemd_inhibit_args(display))
            .spawn()
            .map_err(|e| format!("systemd-inhibit spawn failed: {e}"))?;
        let kind = if display { "显示器" } else { "系统" };
        let what = if display { "sleep:idle" } else { "sleep" };
        // 注意：不能把 `if display {...}` 作为 tracing 宏的位置参数——宏内部会引入
        // 名为 `display` 的局部绑定（tracing::field::display），遮蔽本函数的 bool 参数。
        tracing::info!("已保持{kind}唤醒（systemd-inhibit --what={what}，guard 释放时自动结束）");
        Ok(Box::new(InhibitGuard { child: Some(child) }))
    }
}

/// systemd-inhibit 子进程句柄：Drop/release 时 kill。
struct InhibitGuard {
    child: Option<std::process::Child>,
}

impl WakeGuard for InhibitGuard {
    fn release(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for InhibitGuard {
    fn drop(&mut self) {
        WakeGuard::release(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inhibit_args_block_sleep_for_system() {
        let args = systemd_inhibit_args(false);
        assert_eq!(args[0].as_str(), "--what");
        assert_eq!(args[1].as_str(), "sleep");
        assert!(args.iter().any(|a| a.as_str() == "tail"));
        assert!(args.iter().any(|a| a.as_str() == "-f"));
    }

    #[test]
    fn inhibit_args_also_block_idle_for_display() {
        let args = systemd_inhibit_args(true);
        assert_eq!(args[0].as_str(), "--what");
        assert_eq!(args[1].as_str(), "sleep:idle");
    }

    #[test]
    fn guard_release_is_idempotent() {
        let mut guard = InhibitGuard { child: None };
        WakeGuard::release(&mut guard);
        WakeGuard::release(&mut guard);
        drop(guard);
    }
}
