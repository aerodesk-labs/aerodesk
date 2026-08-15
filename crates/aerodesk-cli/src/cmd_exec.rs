//! #109 远程命令/文件/进程通道（CLI 侧）：被控端执行器接线 + 控制端意图。
//!
//! 被控端（publisher）：收到 `CmdRequest` → 后台线程执行（aerodesk-core::cmd_exec）
//! → 经 mpsc 回传主循环 → 通过 `cmd` data channel 发 `CmdResponse`。
//! 控制端（viewer）：cmd 通道打开后发意图请求（每 1s 重传直到响应），打印结果并退出。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use aerodesk_core::cmd_exec::{
    allowlist, kill_process, list_processes, read_file, run_command, write_file,
};
use aerodesk_core::endpoint::{ClientEvent, Endpoint};
use aerodesk_protocol::cmd::{CmdAction, CmdRequest, CmdResponse, CmdResult, encode_b64};

static CMD_TX: Mutex<Option<std::sync::mpsc::Sender<CmdResponse>>> = Mutex::new(None);
static CMD_RX: Mutex<Option<std::sync::mpsc::Receiver<CmdResponse>>> = Mutex::new(None);
/// 最近处理过的请求 (id, 时间)：控制端重传（首包丢失）按 id 去重防重复执行；
/// 新会话复用相同 id 超过窗口后重新放行。
static LAST_CMD: Mutex<Option<(u64, std::time::Instant)>> = Mutex::new(None);

/// 后台执行线程并发上限：每请求一线程，远端连发可无限增长，超过上限直接
/// 回 busy 响应（控制端可见清晰错误），不再无界 spawn。
const MAX_CMD_THREADS: usize = 8;
static ACTIVE_CMD_THREADS: AtomicUsize = AtomicUsize::new(0);

/// RAII 并发计数守护：线程闭包退出（含 panic 路径）时自动递减。
struct CmdThreadGuard;

impl CmdThreadGuard {
    /// 满载返回 None（不占用名额）。
    fn acquire() -> Option<Self> {
        if ACTIVE_CMD_THREADS.fetch_add(1, Ordering::SeqCst) >= MAX_CMD_THREADS {
            ACTIVE_CMD_THREADS.fetch_sub(1, Ordering::SeqCst);
            None
        } else {
            Some(Self)
        }
    }
}

impl Drop for CmdThreadGuard {
    fn drop(&mut self) {
        ACTIVE_CMD_THREADS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 并发满载时按请求动作返回同构错误结果，避免 ReadFile/WriteFile/Ps/Kill
/// 被包装成 `Run` 后破坏控制端与 MCP 对响应类型的解析。
fn busy_result(action: &CmdAction) -> CmdResult {
    let busy = || format!("cmd busy: too many concurrent commands ({MAX_CMD_THREADS})");
    match action {
        CmdAction::Run { .. } => CmdResult::Run {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            error: Some(busy()),
        },
        CmdAction::ReadFile { .. } | CmdAction::WriteFile { .. } => CmdResult::File {
            data: None,
            size: 0,
            error: Some(busy()),
        },
        CmdAction::ListProcesses => CmdResult::ProcessList {
            processes: Vec::new(),
            error: Some(busy()),
        },
        CmdAction::KillProcess { pid } => CmdResult::Killed {
            pid: *pid,
            error: Some(busy()),
        },
    }
}

/// 控制端意图（viewer 命令行）。
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    Run(String),
    Read(String),
    Write(String, String),
    Ps,
    Kill(u32),
}

/// 初始化命令通道（main 调用一次）。
pub fn init() {
    let (tx, rx) = std::sync::mpsc::channel::<CmdResponse>();
    *CMD_TX.lock().unwrap() = Some(tx);
    *CMD_RX.lock().unwrap() = Some(rx);
}

/// 被控端处理 cmd 通道数据：后台线程执行，结果经 mpsc 回传主循环。
pub fn handle_event(ev: &ClientEvent, endpoint: &mut Endpoint) {
    let ClientEvent::ChannelData(cid, _, data) = ev else {
        return;
    };
    if endpoint.channel_label(*cid).as_deref() != Some("cmd") {
        return;
    }
    let Ok(req) = serde_json::from_slice::<CmdRequest>(data) else {
        tracing::warn!("cmd: 无法解析 CmdRequest");
        return;
    };
    // 去重：同 id 在 60s 窗口内只执行一次（控制端重传场景；新会话复用 id 可放行）。
    let now = std::time::Instant::now();
    let duplicate = {
        let mut g = LAST_CMD.lock().unwrap();
        match g.as_ref() {
            Some((id, at))
                if *id == req.id
                    && now.duration_since(*at) < std::time::Duration::from_secs(60) =>
            {
                true
            }
            _ => {
                *g = Some((req.id, now));
                false
            }
        }
    };
    if duplicate {
        tracing::debug!("cmd request #{} duplicate, ignore", req.id);
        return;
    }
    tracing::info!("cmd request #{}: {:?}", req.id, req.action);
    let tx = CMD_TX.lock().unwrap().clone();
    // 并发上限：满载时不 spawn，直接回带 error 的响应（控制端立即得到
    // 清晰错误而非无限等待）。
    let Some(guard) = CmdThreadGuard::acquire() else {
        tracing::warn!("cmd busy: too many concurrent commands ({MAX_CMD_THREADS})");
        let resp = CmdResponse {
            id: req.id,
            result: busy_result(&req.action),
        };
        if let Some(tx) = tx {
            let _ = tx.send(resp);
        }
        return;
    };
    std::thread::spawn(move || {
        let _guard = guard; // 持有到闭包结束，退出时递减并发计数。
        let result = execute(&req.action);
        let resp = CmdResponse { id: req.id, result };
        if let Some(tx) = tx {
            let _ = tx.send(resp);
        }
    });
}

/// 执行一个动作（被控端线程内）。
fn execute(action: &CmdAction) -> CmdResult {
    let allow = allowlist();
    match action {
        CmdAction::Run {
            command,
            cwd,
            timeout_ms,
        } => {
            let out = run_command(command, cwd.as_deref(), *timeout_ms, &allow);
            CmdResult::Run {
                exit_code: out.exit_code,
                stdout: out.stdout,
                stderr: out.stderr,
                truncated: out.truncated,
                error: out.error,
            }
        }
        CmdAction::ReadFile { path, max_bytes } => match read_file(path, *max_bytes) {
            Ok(data) => CmdResult::File {
                data: Some(encode_b64(&data)),
                size: data.len() as u64,
                error: None,
            },
            Err(e) => CmdResult::File {
                data: None,
                size: 0,
                error: Some(e),
            },
        },
        CmdAction::WriteFile { path, data } => match write_file(path, data, &allow) {
            Ok(()) => CmdResult::File {
                data: None,
                size: 0,
                error: None,
            },
            Err(e) => CmdResult::File {
                data: None,
                size: 0,
                error: Some(e),
            },
        },
        CmdAction::ListProcesses => match list_processes() {
            Ok(processes) => CmdResult::ProcessList {
                processes,
                error: None,
            },
            Err(e) => CmdResult::ProcessList {
                processes: vec![],
                error: Some(e),
            },
        },
        CmdAction::KillProcess { pid } => match kill_process(*pid, &allow) {
            Ok(()) => CmdResult::Killed {
                pid: *pid,
                error: None,
            },
            Err(e) => CmdResult::Killed {
                pid: *pid,
                error: Some(e),
            },
        },
    }
}

/// 被控端主循环：把已完成动作的响应发回控制端。
pub fn tick(endpoint: &mut Endpoint) {
    if let Some(rx) = CMD_RX.lock().unwrap().as_ref() {
        while let Ok(resp) = rx.try_recv() {
            if let Ok(json) = serde_json::to_string(&resp) {
                tracing::info!("cmd response #{}: {:?}", resp.id, resp.result);
                // #331：send 返回 false 表示超 SCTP 消息上限被拒（此前静默丢弃），
                // 至少留告警日志，避免大响应无痕丢失。
                if !endpoint.send_channel_data("cmd", false, json.as_bytes()) {
                    tracing::warn!(
                        "cmd response #{} dropped by data channel (bytes={}, 可能超 SCTP 消息上限)",
                        resp.id,
                        json.len()
                    );
                }
            }
        }
    }
}

/// 控制端意图 id 序列（进程内自增，从 1 开始）。
static CMD_SEQ: AtomicU64 = AtomicU64::new(1);

/// 控制端：发送一个意图请求（id = pid<<32 | 进程内自增序列，每次意图唯一：
/// 旧的 pid<<16|1 每进程恒定，被控端 60s 去重会吞掉同进程后续命令，
/// 且重连后新进程可能撞出相同 id）。
pub fn send_intent(endpoint: &mut Endpoint, intent: &Intent) -> bool {
    let id = ((std::process::id() as u64) << 32) | CMD_SEQ.fetch_add(1, Ordering::Relaxed);
    let action = match intent {
        Intent::Run(cmd) => CmdAction::Run {
            command: cmd.clone(),
            cwd: None,
            timeout_ms: None,
        },
        Intent::Read(path) => CmdAction::ReadFile {
            path: path.clone(),
            max_bytes: None,
        },
        Intent::Write(path, content) => CmdAction::WriteFile {
            path: path.clone(),
            data: encode_b64(content.as_bytes()),
        },
        Intent::Ps => CmdAction::ListProcesses,
        Intent::Kill(pid) => CmdAction::KillProcess { pid: *pid },
    };
    let req = CmdRequest::new(id, action);
    let Ok(json) = serde_json::to_string(&req) else {
        return false;
    };
    endpoint.send_channel_data("cmd", false, json.as_bytes())
}

/// 控制端：解析 cmd 通道响应。
pub fn handle_response(data: &[u8]) -> Option<CmdResponse> {
    serde_json::from_slice::<CmdResponse>(data).ok()
}

/// 本地管理命令（#109 权限/审计入口，无需会话）：
/// `--cmd-allowlist list|add <prefix>|remove <prefix>` / `--cmd-audit [n]`。
/// 返回 true 表示已处理（应直接退出）。
pub fn run_admin(args: &[String]) -> bool {
    let Some(pos) = args.iter().position(|a| a == "--cmd-allowlist") else {
        if let Some(pos) = args.iter().position(|a| a == "--cmd-audit") {
            let n = args
                .get(pos + 1)
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(10);
            match aerodesk_core::cmd_exec::tail_audit(n) {
                Ok(lines) => {
                    for l in lines {
                        println!("{l}");
                    }
                }
                Err(e) => {
                    eprintln!("audit: {e}");
                    std::process::exit(1);
                }
            }
            return true;
        }
        return false;
    };
    let sub = args.get(pos + 1).map(|s| s.as_str()).unwrap_or("list");
    match sub {
        "list" => {
            for p in aerodesk_core::cmd_exec::allowlist() {
                println!("{p}");
            }
        }
        "add" => {
            let Some(prefix) = args.get(pos + 2) else {
                eprintln!("usage: --cmd-allowlist add <prefix>");
                std::process::exit(2);
            };
            if let Err(e) = aerodesk_core::cmd_exec::add_allow_prefix(prefix) {
                eprintln!("add allowlist: {e}");
                std::process::exit(1);
            }
            println!("added: {prefix}");
        }
        "remove" => {
            let Some(prefix) = args.get(pos + 2) else {
                eprintln!("usage: --cmd-allowlist remove <prefix>");
                std::process::exit(2);
            };
            if let Err(e) = aerodesk_core::cmd_exec::remove_allow_prefix(prefix) {
                eprintln!("remove allowlist: {e}");
                std::process::exit(1);
            }
            println!("removed: {prefix}");
        }
        other => {
            eprintln!("unknown allowlist subcommand: {other}");
            std::process::exit(2);
        }
    }
    true
}
#[cfg(test)]
mod tests {
    use super::*;
    use aerodesk_protocol::cmd::CmdAction;

    /// 取各变体的 error 字段（busy 响应必带）。
    fn busy_err(r: &CmdResult) -> &str {
        match r {
            CmdResult::Run { error, .. } => error.as_deref().unwrap_or(""),
            CmdResult::File { error, .. } => error.as_deref().unwrap_or(""),
            CmdResult::ProcessList { error, .. } => error.as_deref().unwrap_or(""),
            CmdResult::Killed { error, .. } => error.as_deref().unwrap_or(""),
        }
    }

    #[test]
    fn busy_result_shapes_match_action() {
        let run = busy_result(&CmdAction::Run {
            command: "echo hi".into(),
            cwd: None,
            timeout_ms: None,
        });
        assert!(matches!(
            run,
            CmdResult::Run {
                exit_code: None,
                truncated: false,
                ..
            }
        ));
        assert!(busy_err(&run).contains("cmd busy"));

        let rf = busy_result(&CmdAction::ReadFile {
            path: "C:\\tmp\\a.txt".into(),
            max_bytes: None,
        });
        assert!(matches!(
            rf,
            CmdResult::File {
                data: None,
                size: 0,
                ..
            }
        ));
        assert!(busy_err(&rf).contains("cmd busy"));

        let wf = busy_result(&CmdAction::WriteFile {
            path: "C:\\tmp\\b.txt".into(),
            data: "".into(),
        });
        assert!(matches!(
            wf,
            CmdResult::File {
                data: None,
                size: 0,
                ..
            }
        ));
        assert!(busy_err(&wf).contains("cmd busy"));

        let ps = busy_result(&CmdAction::ListProcesses);
        assert!(matches!(ps, CmdResult::ProcessList { ref processes, .. } if processes.is_empty()));
        assert!(busy_err(&ps).contains("cmd busy"));

        let kill = busy_result(&CmdAction::KillProcess { pid: 42 });
        assert!(matches!(kill, CmdResult::Killed { pid: 42, .. }));
        assert!(busy_err(&kill).contains("cmd busy"));
    }
}
