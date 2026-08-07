//! #109 远程命令通道（CLI 侧）：被控端执行器接线 + 控制端请求/响应。
//!
//! 被控端（publisher）：收到 `CmdRequest` → 后台线程执行（aerodesk-core::cmd_exec）
//! → 经 mpsc 回传主循环 → 通过 `cmd` data channel 发 `CmdResponse`。
//! 控制端（viewer `--run-command`）：cmd 通道打开后发请求，收到响应后打印并退出。

use std::sync::Mutex;

use aerodesk_core::cmd_exec::{allowlist, run_command};
use aerodesk_core::endpoint::{ClientEvent, Endpoint};
use aerodesk_protocol::cmd::{CmdRequest, CmdResponse};

static CMD_TX: Mutex<Option<std::sync::mpsc::Sender<CmdResponse>>> = Mutex::new(None);
static CMD_RX: Mutex<Option<std::sync::mpsc::Receiver<CmdResponse>>> = Mutex::new(None);
/// 最近处理过的请求 (id, 时间)：控制端重传（首包丢失）按 id 去重防重复执行；
/// 新会话复用相同 id（如 1）超过窗口后重新放行。
static LAST_CMD: Mutex<Option<(u64, std::time::Instant)>> = Mutex::new(None);

/// 初始化命令通道（main 调用一次；viewer 只发请求不启用执行器也无需 rx）。
pub fn init() {
    let (tx, rx) = std::sync::mpsc::channel::<CmdResponse>();
    *CMD_TX.lock().unwrap() = Some(tx);
    *CMD_RX.lock().unwrap() = Some(rx);
}

/// 被控端处理 cmd 通道数据：后台线程执行命令，结果经 mpsc 回传主循环。
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
    tracing::info!("cmd request #{}: {}", req.id, req.command);
    let tx = CMD_TX.lock().unwrap().clone();
    let allow = allowlist();
    std::thread::spawn(move || {
        let out = run_command(&req.command, req.cwd.as_deref(), req.timeout_ms, &allow);
        let resp = CmdResponse {
            id: req.id,
            ok: out.error.is_none() && out.exit_code == Some(0),
            exit_code: out.exit_code,
            stdout: out.stdout,
            stderr: out.stderr,
            truncated: out.truncated,
            error: out.error,
        };
        if let Some(tx) = tx {
            let _ = tx.send(resp);
        }
    });
}

/// 被控端主循环：把已完成命令的响应发回控制端。
pub fn tick(endpoint: &mut Endpoint) {
    if let Some(rx) = CMD_RX.lock().unwrap().as_ref() {
        while let Ok(resp) = rx.try_recv() {
            if let Ok(json) = serde_json::to_string(&resp) {
                tracing::info!(
                    "cmd response #{}: ok={} exit={:?} stdout={}B stderr={}B",
                    resp.id,
                    resp.ok,
                    resp.exit_code,
                    resp.stdout.len(),
                    resp.stderr.len()
                );
                endpoint.send_channel_data("cmd", false, json.as_bytes());
            }
        }
    }
}

/// 控制端（viewer --run-command）：发送一个命令请求并等待响应。
/// 请求 id 按进程唯一（pid<<16|1）：同一控制端重传同 id（被控端去重），
/// 不同控制端（不同 pid）id 不同，避免跨会话误去重。
pub fn send_request(endpoint: &mut Endpoint, command: &str, timeout_ms: Option<u64>) -> bool {
    let id = ((std::process::id() as u64) << 16) | 1;
    let req = CmdRequest::new(id, command).timeout(timeout_ms.unwrap_or(30_000));
    let Ok(json) = serde_json::to_string(&req) else {
        return false;
    };
    endpoint.send_channel_data("cmd", false, json.as_bytes())
}

/// 控制端：处理 cmd 通道响应（--run-command 模式：打印并退出）。
pub fn handle_response(data: &[u8]) -> Option<CmdResponse> {
    serde_json::from_slice::<CmdResponse>(data).ok()
}
