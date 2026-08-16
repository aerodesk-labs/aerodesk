//! Windows 被控端系统服务（#470）：SCM 生命周期 + 服务运行骨架。
//! M1：安装/移除/查询/运行（停止信号、状态机、文件 + 事件日志）。
//! M2 接入信令常驻（`aerodesk-core::signal_presence`），M3 接入 WTS 会话仲裁。
//! 设计见 docs/PRELOGIN_WINDOWS_SERVICE.md。

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

/// SCM 服务名（安装/查询/事件日志 source 共用）。
pub const SERVICE_NAME: &str = "AeroDeskService";
const DISPLAY_NAME: &str = "AeroDesk 远程桌面被控服务";
const DESCRIPTION: &str = "AeroDesk 被控端系统服务（#470）：SYSTEM 常驻，登录前信令在线与会话仲裁";
/// ERROR_SERVICE_DOES_NOT_EXIST：open_service 的"未安装"判定。
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
/// ERROR_ACCESS_DENIED：非管理员操作 SCM。
const ERROR_ACCESS_DENIED: i32 = 5;

/// WTS 会话变化原因（re-export 供服务体 match）。
pub use windows_service::service::SessionChangeReason;

/// 服务体收到的事件（SCM 控制处理器转发，#470 M3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceEvent {
    /// WTS 会话变化（原因 + 发生变化的会话 id）。
    SessionChange {
        reason: SessionChangeReason,
        session_id: u32,
    },
}

/// 服务体运行上下文：停止信号 + SCM 转发事件。
/// `wait_event` 兼任节拍 sleep（事件到达即提前唤醒）。
pub struct ServiceCtx {
    stop: Arc<AtomicBool>,
    events: mpsc::Receiver<ServiceEvent>,
}

impl ServiceCtx {
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// 等待事件至超时；无事件/通道关闭返回 `None`。
    pub fn wait_event(&self, d: Duration) -> Option<ServiceEvent> {
        self.events.recv_timeout(d).ok()
    }
}

/// 前台调试上下文(#471 M2 可测性地基):永不停止、无 SCM 事件——
/// `--service-fg` 直跑服务体(本地/CI e2e 无需 SCM/管理员)。
pub fn foreground_ctx() -> ServiceCtx {
    let (tx, rx) = mpsc::channel::<ServiceEvent>();
    drop(tx); // 断链:wait_event 恒 None,仅作节拍 sleep
    ServiceCtx {
        stop: Arc::new(AtomicBool::new(false)),
        events: rx,
    }
}

/// 服务体签名：接收运行上下文，循环自查直至 SCM Stop/Shutdown。
type ServiceBody = Box<dyn FnOnce(ServiceCtx) + Send>;
static SERVICE_BODY: Mutex<Option<ServiceBody>> = Mutex::new(None);

/// SCM/Win32 错误转可读信息；非管理员给显式提示（M1 验收：非管理员安装被明确拒绝）。
/// 注意 crate 的 `Error::Winapi` Display 只印 "IO error in winapi call"（吞掉 io 错误），
/// 须自行透出 code + message 才能定位。
fn friendly(e: windows_service::Error) -> String {
    match &e {
        windows_service::Error::Winapi(io) => match io.raw_os_error() {
            Some(ERROR_ACCESS_DENIED) => "需要管理员权限（以管理员身份运行）".into(),
            _ => format!("{e}（os error {:?}：{io}）", io.raw_os_error()),
        },
        _ => format!("{e}"),
    }
}

/// open_service 报"服务不存在"（OS error 1060）→ 视为未安装而非错误。
fn is_not_installed(e: &windows_service::Error) -> bool {
    matches!(
        e,
        windows_service::Error::Winapi(io)
            if io.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST)
    )
}

fn connect_manager(access: ServiceManagerAccess) -> Result<ServiceManager, String> {
    ServiceManager::local_computer(None::<&str>, access).map_err(friendly)
}

/// 安装服务（需管理员）：AutoStart + LocalSystem，命令行为 `"<exe>" --service`，
/// 装好即启动。
pub fn install(exe: &str) -> Result<(), String> {
    let manager = connect_manager(ServiceManagerAccess::CREATE_SERVICE)?;
    let service = manager
        .create_service(
            &ServiceInfo {
                name: OsString::from(SERVICE_NAME),
                display_name: OsString::from(DISPLAY_NAME),
                service_type: ServiceType::OWN_PROCESS,
                start_type: ServiceStartType::AutoStart,
                error_control: ServiceErrorControl::Normal,
                executable_path: PathBuf::from(exe),
                launch_arguments: vec![OsString::from("--service")],
                dependencies: vec![],
                account_name: None, // LocalSystem
                account_password: None,
            },
            // 返回句柄的后续用途：写描述 + 启动 + 查询。
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )
        .map_err(friendly)?;
    service.set_description(DESCRIPTION).map_err(friendly)?;
    service.start::<&str>(&[]).map_err(friendly)?;
    Ok(())
}

/// 移除服务（需管理员）：在跑先停（最多等 10s），再标记删除；返回是否曾存在。
pub fn remove() -> Result<bool, String> {
    let manager = connect_manager(ServiceManagerAccess::CONNECT)?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS | ServiceAccess::STOP,
    ) {
        Ok(s) => s,
        Err(e) if is_not_installed(&e) => return Ok(false),
        Err(e) => return Err(friendly(e)),
    };
    let running = service
        .query_status()
        .map(|s| s.current_state == ServiceState::Running)
        .unwrap_or(false);
    if running {
        service.stop().map_err(friendly)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let stopped = service
                .query_status()
                .map(|s| s.current_state == ServiceState::Stopped)
                .unwrap_or(true);
            if stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    service.delete().map_err(friendly)?;
    Ok(true)
}

/// 查询服务状态：未安装返回 `None`；已安装返回可读状态串（状态 + pid）。
pub fn status() -> Result<Option<String>, String> {
    let manager = connect_manager(ServiceManagerAccess::CONNECT)?;
    let service = match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(s) => s,
        Err(e) if is_not_installed(&e) => return Ok(None),
        Err(e) => return Err(friendly(e)),
    };
    let st = service.query_status().map_err(friendly)?;
    Ok(Some(format!(
        "{} pid={}",
        state_display(st.current_state),
        st.process_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".into())
    )))
}

fn state_display(s: ServiceState) -> &'static str {
    match s {
        ServiceState::Stopped => "已停止",
        ServiceState::StartPending => "启动中",
        ServiceState::StopPending => "停止中",
        ServiceState::Running => "运行中",
        ServiceState::ContinuePending => "恢复中",
        ServiceState::PausePending => "暂停中",
        ServiceState::Paused => "已暂停",
    }
}

/// 服务入口（由 SCM 派发调用）：注册控制处理器并驱动状态机，阻塞直至服务停止。
/// 直接在控制台运行会因无 SCM 派发上下文而失败——提示先 `--install-service`。
/// 失败路径写事件日志——SCM 报 1053(服务未及时响应)时,服务进程早退的
/// 真实原因只能从事件日志/服务日志文件找到(CI 实测教训)。
pub fn run(body: ServiceBody) -> Result<(), String> {
    SERVICE_BODY.lock().unwrap().replace(body);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| {
        let msg =
            format!("SCM dispatcher 启动失败（请经服务管理器启动，或先 --install-service）：{e}");
        event_log(&msg, true);
        msg
    })
}

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = service_loop() {
        event_log(&format!("service loop 退出：{e}"), true);
        eprintln!("aerodesk-service: {e}");
    }
}

fn service_loop() -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let (event_tx, event_rx) = mpsc::channel::<ServiceEvent>();
    let handler = service_control_handler::register(SERVICE_NAME, {
        let stop = stop.clone();
        move |ctrl| match ctrl {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                stop.store(true, Ordering::SeqCst);
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            // #470 M3：会话变化转发给服务体（让位状态机驱动源）。
            ServiceControl::SessionChange(param) => {
                let _ = event_tx.send(ServiceEvent::SessionChange {
                    reason: param.reason,
                    session_id: param.notification.session_id,
                });
                ServiceControlHandlerResult::NoError
            }
            // Interrogate 由 crate 自动应答当前状态。
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    })
    .map_err(|e| format!("注册服务控制处理器失败：{e}"))?;

    let set_status = |state: ServiceState, checkpoint: u32| -> Result<(), String> {
        handler
            .set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: ServiceControlAccept::STOP
                    | ServiceControlAccept::SESSION_CHANGE,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint,
                wait_hint: Duration::from_secs(5),
                process_id: None,
            })
            .map_err(|e| format!("上报服务状态失败：{e}"))
    };

    set_status(ServiceState::StartPending, 1)?;
    event_log("aerodesk-service 启动（#470：信令常驻 + 会话仲裁）", false);
    if let Some(body) = SERVICE_BODY.lock().unwrap().take() {
        let ctx = ServiceCtx {
            stop,
            events: event_rx,
        };
        std::thread::Builder::new()
            .name("service-body".into())
            .spawn(move || body(ctx))
            .map_err(|e| format!("启动服务体线程失败：{e}"))?;
    }
    set_status(ServiceState::Running, 0)?;
    // 等待停止命令（1s 粒度轮询，保证 Stop 响应延迟 ≤1s）。
    loop {
        match shutdown_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(()) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    set_status(ServiceState::StopPending, 1)?;
    // 给服务体收尾窗口（presence stop + 状态上报约 1s 内完成）。
    std::thread::sleep(Duration::from_secs(1));
    set_status(ServiceState::Stopped, 0)?;
    event_log("aerodesk-service 停止", false);
    Ok(())
}

/// Windows 事件日志输出（最低可见性通道：安装后无文件权限/日志盘满时仍可排障；
/// 常规日志走文件）。未注册消息 DLL 时事件查看器会提示"找不到描述"，属预期。
fn event_log(msg: &str, error: bool) {
    use windows::Win32::System::EventLog::{
        DeregisterEventSource, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE,
        RegisterEventSourceW, ReportEventW,
    };
    use windows::core::PCWSTR;
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    unsafe {
        let source = wide(SERVICE_NAME);
        let Ok(handle) = RegisterEventSourceW(PCWSTR::null(), PCWSTR(source.as_ptr())) else {
            return;
        };
        let line = wide(msg);
        let strings = [PCWSTR(line.as_ptr())];
        let _ = ReportEventW(
            handle,
            if error {
                EVENTLOG_ERROR_TYPE
            } else {
                EVENTLOG_INFORMATION_TYPE
            },
            0,
            0,
            None,
            0,
            Some(&strings),
            None,
        );
        let _ = DeregisterEventSource(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_denied_maps_to_admin_hint() {
        let e =
            windows_service::Error::Winapi(std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED));
        assert!(friendly(e).contains("管理员"));
    }

    #[test]
    fn not_installed_error_is_recognized() {
        let e = windows_service::Error::Winapi(std::io::Error::from_raw_os_error(
            ERROR_SERVICE_DOES_NOT_EXIST,
        ));
        assert!(is_not_installed(&e));
    }
}
