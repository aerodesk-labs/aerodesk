//! aerodesk-host —— AeroDesk 宿主二进制（#492）：
//! 被控系统服务（SYSTEM 常驻，#470）与登录界面 helper（#471）的唯一承载。
//! 从 aerodesk-agent 拆出——服务/helper 是宿主级角色，CLI 回归纯命令行工具。
//! 与 ToDesk 同型：单二进制双角色（ToDesk.exe --runservice 对应 --service）。

use tracing::info;

#[cfg(windows)]
mod service_run;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // #470 服务态必须最先分流：init_log() 会占用全局 tracing subscriber，
    // 服务分支的 init_service_log() 二次 init 会 panic（双订阅）→ 服务进程
    // 秒死 → SCM 报 1053（CI 实测教训，本地直跑 --service 前记得也无 stderr 消费者）。
    if args.iter().any(|a| a == "--service") {
        #[cfg(windows)]
        {
            init_service_log();
            info!("aerodesk-service 启动（#470：信令常驻 + 会话仲裁）");
            if let Err(e) =
                aerodesk_platform::windows::service::run(Box::new(service_run::service_body))
            {
                eprintln!("service run failed: {e}");
                std::process::exit(1);
            }
        }
        #[cfg(not(windows))]
        {
            eprintln!("--service 仅 Windows 支持");
            std::process::exit(1);
        }
        return;
    }

    // #470 Windows 系统服务（需管理员）：安装/移除/查询 + 服务运行入口。
    if args.iter().any(|a| a == "--install-service") {
        #[cfg(windows)]
        {
            let exe = std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "aerodesk-agent.exe".into());
            match aerodesk_platform::windows::service::install(&exe) {
                Ok(()) => {
                    println!(
                        "service installed and started: {}",
                        aerodesk_platform::windows::service::SERVICE_NAME
                    );
                    // #470 D2：同步机器级配置（用户设置 → ProgramData）。
                    match service_run::sync_settings_from_user() {
                        Ok(s) => println!(
                            "service config synced: server={} device_id={}",
                            if s.server.is_empty() {
                                "(未配置)"
                            } else {
                                &s.server
                            },
                            if s.device_id.is_empty() {
                                "(未配置)"
                            } else {
                                &s.device_id
                            }
                        ),
                        Err(e) => println!(
                            "warn: 服务配置未同步（{e}）；可运行桌面端生成设置后重装，或手动编辑 {}",
                            service_run::ServiceSettings::path().display()
                        ),
                    }
                    // #470 D7：HKCU 自启共存提示（双实例 = 同 device-id 双在线）。
                    if let Ok(Some(cmd)) = aerodesk_platform::windows::autostart::installed() {
                        println!(
                            "提示：检测到 HKCU 登录后自启（{cmd}）；服务模式下建议 --remove-autostart 移除，避免双实例"
                        );
                    }
                }
                Err(e) => {
                    eprintln!("service install failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        #[cfg(not(windows))]
        {
            eprintln!("--install-service 仅 Windows 支持");
            std::process::exit(1);
        }
        return;
    }

    if args.iter().any(|a| a == "--remove-service") {
        #[cfg(windows)]
        {
            match aerodesk_platform::windows::service::remove() {
                Ok(true) => println!("service removed"),
                Ok(false) => println!("service not installed"),
                Err(e) => {
                    eprintln!("service remove failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        #[cfg(not(windows))]
        {
            eprintln!("--remove-service 仅 Windows 支持");
            std::process::exit(1);
        }
        return;
    }

    if args.iter().any(|a| a == "--service-status") {
        #[cfg(windows)]
        {
            match aerodesk_platform::windows::service::status() {
                Ok(Some(s)) => println!("installed: {s}"),
                Ok(None) => println!("not installed"),
                Err(e) => {
                    eprintln!("service query failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        #[cfg(not(windows))]
        {
            eprintln!("--service-status 仅 Windows 支持");
            std::process::exit(1);
        }
        return;
    }

    // #470 服务配置查看（调试辅助：路径 + 生效值）。
    // 注：--service 运行入口已前移至 run() 顶部（须先于 init_log 分流，
    // 见函数头注释），此处不再重复。
    if args.iter().any(|a| a == "--service-config") {
        #[cfg(windows)]
        {
            let s = service_run::ServiceSettings::load();
            println!("path: {}", service_run::ServiceSettings::path().display());
            println!(
                "server={}\ndevice_id={}\ntoken={}\nspawn_ui={}\nui_exe={}",
                s.server,
                s.device_id,
                if s.token.is_empty() { "(空)" } else { "***" },
                s.spawn_ui,
                s.ui_exe
            );
        }
        #[cfg(not(windows))]
        {
            eprintln!("--service-config 仅 Windows 支持");
            std::process::exit(1);
        }
        return;
    }

    eprintln!(
        "aerodesk-host 仅承载宿主角色子命令，用法：--install-service / --remove-service / --service-status / --service-config / --service"
    );
    std::process::exit(2);
}

/// #470 服务态日志：写 `%ProgramData%\AeroDesk\logs\service.log`。SYSTEM 服务
/// 无控制台、无用户 HOME（docs/PRELOGIN_WINDOWS_SERVICE.md D2），ProgramData
/// 不可用时回退 stderr（便于手动 `--service` 调试）。
#[cfg(windows)]
fn init_service_log() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(service_log_sink()))
        .with(filter)
        .init();
}

#[cfg(windows)]
fn service_log_sink() -> ServiceLogSink {
    let dir = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into());
    let dir = std::path::Path::new(&dir).join("AeroDesk").join("logs");
    let open = std::fs::create_dir_all(&dir).and_then(|_| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("service.log"))
    });
    match open {
        Ok(f) => ServiceLogSink::File(std::sync::Arc::new(f)),
        Err(_) => ServiceLogSink::Stderr,
    }
}

/// 服务日志落点：ProgramData 文件优先，回退 stderr。逐事件克隆句柄，M1 心跳量级无压力。
#[cfg(windows)]
#[derive(Clone)]
enum ServiceLogSink {
    File(std::sync::Arc<std::fs::File>),
    Stderr,
}

#[cfg(windows)]
impl std::io::Write for ServiceLogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ServiceLogSink::File(f) => (&**f).write(buf),
            ServiceLogSink::Stderr => std::io::stderr().write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ServiceLogSink::File(f) => (&**f).flush(),
            ServiceLogSink::Stderr => std::io::stderr().flush(),
        }
    }
}

#[cfg(windows)]
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ServiceLogSink {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
