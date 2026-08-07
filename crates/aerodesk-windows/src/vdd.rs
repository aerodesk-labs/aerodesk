//! 虚拟显示器：Parsec VDD 接入（Windows，被控端）。
//!
//! 生命周期与心跳（ADR-0001）：
//! - [`VirtualDisplayManager::new`] 校验驱动状态并打开设备句柄，启动心跳线程
//!   （`vdd_update` 每 100ms 一次，远低于驱动的 ~1s 保活窗口）；
//! - [`VirtualDisplayManager::add_display`] / [`VirtualDisplayManager::remove_display`]
//!   管理虚拟屏（增删 + 分辨率配置）；
//! - `Drop` 停心跳、按逆序移除全部虚拟屏、关闭句柄，避免残留。
//!
//! 非 Windows 平台编译为 stub（`new()` 返回 [`VddError::Unsupported`]），
//! 保证 workspace 在 macOS/Linux 上可编译与测试。

use std::fmt;

/// 虚拟显示器操作错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VddError {
    /// 非 Windows 平台。
    Unsupported,
    /// Parsec VDD 驱动未安装/未运行/状态异常（含具体状态）。
    DriverNotReady(String),
    /// 无法打开驱动设备句柄。
    OpenFailed,
    /// 增删/配置虚拟屏失败。
    Io(String),
}

impl fmt::Display for VddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VddError::Unsupported => write!(f, "virtual display is only supported on Windows"),
            VddError::DriverNotReady(s) => {
                write!(
                    f,
                    "Parsec VDD driver not ready (install via `nefconw -i`): {s}"
                )
            }
            VddError::OpenFailed => write!(f, "failed to open Parsec VDD device handle"),
            VddError::Io(s) => write!(f, "Parsec VDD operation failed: {s}"),
        }
    }
}

impl std::error::Error for VddError {}

#[cfg(windows)]
mod imp {
    use super::VddError;
    use parsec_vdd_rust::{
        DeviceStatus, VDD_ADAPTER_GUID, VDD_CLASS_GUID, VDD_HARDWARE_ID, VDD_MAX_DISPLAYS,
        close_device_handle, open_device_handle, query_device_status, vdd_add_and_identify_display,
        vdd_remove_display, vdd_update,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;
    use windows062::Win32::Foundation::HANDLE;

    /// 心跳间隔：parsec-vdd-rust 要求 <100ms 保活（驱动 ~1s 不 ping 即拔出全部虚拟屏）。
    const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);

    /// `HANDLE` 非 Send/Sync，包装后供心跳线程使用。
    ///
    /// Safety：句柄仅用于 DeviceIoControl（`vdd_update`/`vdd_add_display`/`vdd_remove_display`），
    /// 且 `Imp::drop` 先 join 心跳线程再 `close_device_handle`，保证线程退出后才关句柄。
    #[derive(Clone, Copy)]
    struct Handle(HANDLE);
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    impl Handle {
        /// 取回底层句柄。用方法而非字段访问，避免闭包按字段精确捕获
        /// `HANDLE`（!Send）导致心跳线程不满足 Send 约束。
        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    pub struct Imp {
        handle: Handle,
        alive: Arc<AtomicBool>,
        heartbeat: Option<JoinHandle<()>>,
        displays: Vec<i32>,
    }

    impl Imp {
        pub fn new() -> Result<Self, VddError> {
            // 驱动未就绪时明确报错，不静默回退（ADR-0001 风险表）。
            let status = query_device_status(&VDD_CLASS_GUID, VDD_HARDWARE_ID);
            if status != DeviceStatus::Ok {
                return Err(VddError::DriverNotReady(format!("{status:?}")));
            }
            let handle = open_device_handle(&VDD_ADAPTER_GUID).ok_or(VddError::OpenFailed)?;
            let alive = Arc::new(AtomicBool::new(true));
            let alive_clone = Arc::clone(&alive);
            let handle_clone = Handle(handle);
            let heartbeat = thread::Builder::new()
                .name("aerodesk-vdd-heartbeat".into())
                .spawn(move || {
                    while alive_clone.load(Ordering::Relaxed) {
                        // 心跳失败（句柄失效等）即退出，交给 Drop 回收。
                        if vdd_update(handle_clone.raw()).is_err() {
                            break;
                        }
                        thread::sleep(HEARTBEAT_INTERVAL);
                    }
                })
                .map_err(|e| VddError::Io(e.to_string()))?;
            Ok(Self {
                handle: Handle(handle),
                alive,
                heartbeat: Some(heartbeat),
                displays: Vec::new(),
            })
        }

        /// 添加一台虚拟屏并配置分辨率/刷新率，返回其 index。
        pub fn add_display(&mut self, width: u32, height: u32, hz: u32) -> Result<i32, VddError> {
            if self.displays.len() >= VDD_MAX_DISPLAYS as usize {
                return Err(VddError::Io(format!(
                    "virtual display limit {VDD_MAX_DISPLAYS} reached"
                )));
            }
            let (index, mut display) = vdd_add_and_identify_display(self.handle.0)
                .map_err(|e| VddError::Io(e.to_string()))?;
            if width > 0
                && height > 0
                && hz > 0
                && !display.change_mode(
                    Some(width as i32),
                    Some(height as i32),
                    Some(hz as i32),
                    None,
                    None,
                )
            {
                tracing::warn!(
                    index,
                    width,
                    height,
                    hz,
                    "failed to configure virtual display mode, using driver default"
                );
            }
            self.displays.push(index);
            Ok(index)
        }

        /// 按 index 移除一台虚拟屏。
        pub fn remove_display(&mut self, index: i32) -> Result<(), VddError> {
            vdd_remove_display(self.handle.0, index).map_err(|e| VddError::Io(e.to_string()))?;
            self.displays.retain(|&i| i != index);
            Ok(())
        }

        pub fn display_count(&self) -> usize {
            self.displays.len()
        }
    }

    impl Drop for Imp {
        fn drop(&mut self) {
            self.alive.store(false, Ordering::Relaxed);
            if let Some(heartbeat) = self.heartbeat.take() {
                let _ = heartbeat.join();
            }
            // Win10 拔中间屏有布局缓存 quirk，逆序（右到左）移除更稳（ADR-0001）。
            for &index in self.displays.iter().rev() {
                let _ = vdd_remove_display(self.handle.0, index);
            }
            close_device_handle(self.handle.0);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::VddError;

    pub struct Imp;

    impl Imp {
        pub fn new() -> Result<Self, VddError> {
            Err(VddError::Unsupported)
        }
        pub fn add_display(&mut self, _w: u32, _h: u32, _hz: u32) -> Result<i32, VddError> {
            Err(VddError::Unsupported)
        }
        pub fn remove_display(&mut self, _index: i32) -> Result<(), VddError> {
            Err(VddError::Unsupported)
        }
        pub fn display_count(&self) -> usize {
            0
        }
    }
}

/// 虚拟显示器管理器（被控端会话生命周期内持有）。
///
/// 默认配置：4K60（3840×2160@60），可按会话需求传入。
pub struct VirtualDisplayManager {
    imp: imp::Imp,
}

impl VirtualDisplayManager {
    /// 打开 Parsec VDD 并启动心跳；驱动未安装/未就绪时返回明确错误。
    pub fn new() -> Result<Self, VddError> {
        imp::Imp::new().map(|imp| Self { imp })
    }

    /// 添加一台虚拟屏并配置分辨率/刷新率，返回其 index。
    pub fn add_display(&mut self, width: u32, height: u32, hz: u32) -> Result<i32, VddError> {
        self.imp.add_display(width, height, hz)
    }

    /// 按 index 移除一台虚拟屏。
    pub fn remove_display(&mut self, index: i32) -> Result<(), VddError> {
        self.imp.remove_display(index)
    }

    /// 当前已添加的虚拟屏数量。
    pub fn display_count(&self) -> usize {
        self.imp.display_count()
    }
}

#[cfg(test)]
mod tests {
    use super::VddError;

    #[test]
    fn error_display_messages() {
        assert!(
            VddError::Unsupported
                .to_string()
                .contains("only supported on Windows")
        );
        assert!(
            VddError::DriverNotReady("Disabled".into())
                .to_string()
                .contains("nefconw -i")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unsupported_on_non_windows() {
        let manager = super::VirtualDisplayManager::new();
        assert!(matches!(manager, Err(VddError::Unsupported)));
    }
}
