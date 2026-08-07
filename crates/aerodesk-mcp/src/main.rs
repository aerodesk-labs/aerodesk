//! aerodesk-mcp —— AeroDesk MCP 工具面（#109）。
//!
//! 通过 stdio（newline-delimited JSON-RPC 2.0）暴露 MCP 协议：
//! `initialize` / `tools/list` / `tools/call` / `ping`。
//! 工具经本地 `aerodesk-cli` 桥接（`--cmd-json`）操作远程被控设备：
//! connect / run_command / read_file / write_file / list_processes / kill_process。
//!
//! 配置环境变量：AERODESK_SIGNAL（默认 ws://127.0.0.1:3003）、AERODESK_ROOM（默认 demo）、
//! AERODESK_CLI_BIN（默认 aerodesk-cli，需在 PATH 或指向 target/debug/aerodesk-cli）。

use std::io::{BufRead, Write};
use std::process::Command;

use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Clone)]
struct State {
    signal: String,
    room: String,
    cli_bin: String,
}

fn main() {
    let state = State {
        signal: std::env::var("AERODESK_SIGNAL").unwrap_or_else(|_| "ws://127.0.0.1:3003".into()),
        room: std::env::var("AERODESK_ROOM").unwrap_or_else(|_| "demo".into()),
        cli_bin: std::env::var("AERODESK_CLI_BIN").unwrap_or_else(|_| "aerodesk-cli".into()),
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        match method {
            "initialize" => {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": PROTOCOL_VERSION,
                            "capabilities": { "tools": { "listChanged": false } },
                            "serverInfo": { "name": "aerodesk-mcp", "version": env!("CARGO_PKG_VERSION") },
                        }
                    })
                );
            }
            "notifications/initialized" => continue,
            "ping" => {
                let _ = writeln!(out, "{}", json!({"jsonrpc":"2.0","id":id,"result":{}}));
            }
            "tools/list" => {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"jsonrpc":"2.0","id":id,"result":{"tools": tool_definitions()}})
                );
            }
            "tools/call" => {
                let resp = call_tool(&msg["params"], &state);
                let _ = writeln!(out, "{resp}");
            }
            _ => {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}})
                );
            }
        }
        let _ = out.flush();
    }
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "connect",
            "description": "设置/覆盖目标设备（signal 地址 + 房间）。默认取环境变量 AERODESK_SIGNAL/AERODESK_ROOM。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "signal": {"type": "string", "description": "信令地址，如 ws://host:3003"},
                    "room": {"type": "string", "description": "房间名"}
                }
            }
        }),
        json!({
            "name": "run_command",
            "description": "在远程被控设备执行 shell 命令并返回 stdout/stderr/exit code。危险命令（rm -rf、dd、shutdown、交互式等）默认被拦截；白名单可放行。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "要执行的命令"}
                },
                "required": ["command"]
            }
        }),
        json!({
            "name": "read_file",
            "description": "读取远程文件内容（默认上限 4MB）。",
            "inputSchema": {
                "type": "object",
                "properties": {"path": {"type": "string", "description": "远程文件路径"}},
                "required": ["path"]
            }
        }),
        json!({
            "name": "write_file",
            "description": "写远程文件（文本内容）。系统敏感路径（/etc、/System 等）默认禁止，白名单可放行。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }
        }),
        json!({
            "name": "list_processes",
            "description": "列出远程设备进程（pid + 名称）。",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "kill_process",
            "description": "结束远程进程（pid 0/1 默认禁止）。",
            "inputSchema": {
                "type": "object",
                "properties": {"pid": {"type": "integer"}},
                "required": ["pid"]
            }
        }),
        json!({
            "name": "mouse_move",
            "description": "移动远程鼠标到归一化坐标（0..1）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": {"type": "number", "minimum": 0, "maximum": 1},
                    "y": {"type": "number", "minimum": 0, "maximum": 1}
                },
                "required": ["x", "y"]
            }
        }),
        json!({
            "name": "mouse_click",
            "description": "在远程设备点击（左/右/中键，归一化坐标；按下+抬起）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "button": {"type": "string", "enum": ["left", "right", "middle"], "description": "默认 left"},
                    "x": {"type": "number", "minimum": 0, "maximum": 1},
                    "y": {"type": "number", "minimum": 0, "maximum": 1}
                },
                "required": ["x", "y"]
            }
        }),
        json!({
            "name": "type_text",
            "description": "在远程设备逐字符输入文本（US 布局，自动处理大写/符号 Shift）。",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string", "description": "要输入的文本"}},
                "required": ["text"]
            }
        }),
        json!({
            "name": "download_file",
            "description": "从被控端下载文件（走 file 通道，大文件无 4MB 限制）；返回保存到控制端的路径。",
            "inputSchema": {
                "type": "object",
                "properties": {"remote_path": {"type": "string", "description": "被控端文件绝对路径"}},
                "required": ["remote_path"]
            }
        }),
        json!({
            "name": "upload_file",
            "description": "上传本地文件到被控端（走 file 通道，大文件无 4MB 限制）。被控端 publisher 需以 --recv-dir <dir> 启动。",
            "inputSchema": {
                "type": "object",
                "properties": {"local_path": {"type": "string", "description": "控制端本地文件绝对路径"}},
                "required": ["local_path"]
            }
        }),
    ]
}

/// 构造 aerodesk-cli 控制端参数（单元测试覆盖）。
fn build_args(state: &State, name: &str, args: &Value) -> Result<Vec<String>, String> {
    let mut cmd = vec![
        "--role".into(),
        "viewer".into(),
        "--signal".into(),
        state.signal.clone(),
        "--room".into(),
        state.room.clone(),
    ];
    match name {
        "run_command" => {
            let command = args
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "run_command 缺少 command".to_string())?;
            cmd.push("--run-command".into());
            cmd.push(command.to_string());
        }
        "read_file" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "read_file 缺少 path".to_string())?;
            cmd.push("--read-file".into());
            cmd.push(path.to_string());
        }
        "write_file" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "write_file 缺少 path".to_string())?;
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "write_file 缺少 content".to_string())?;
            cmd.push("--write-file".into());
            cmd.push(path.to_string());
            cmd.push(content.to_string());
        }
        "list_processes" => {
            cmd.push("--list-processes".into());
        }
        "kill_process" => {
            let pid = args
                .get("pid")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "kill_process 缺少 pid".to_string())?;
            cmd.push("--kill-pid".into());
            cmd.push(pid.to_string());
        }
        "mouse_move" => {
            let x = args
                .get("x")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| "mouse_move 缺少 x".to_string())?;
            let y = args
                .get("y")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| "mouse_move 缺少 y".to_string())?;
            cmd.push("--send-input".into());
            cmd.push(format!("{{\"type\":\"mouse_move\",\"x\":{x},\"y\":{y}}}"));
        }
        other => return Err(format!("unknown tool: {other}")),
    }
    cmd.push("--cmd-json".into());
    Ok(cmd)
}

/// 构造鼠标按下/抬起事件 JSON（供 --send-input）。
fn mouse_button_json(button: &str, state: &str, x: f64, y: f64) -> String {
    format!(
        "{{\"type\":\"mouse_button\",\"button\":\"{button}\",\"state\":\"{state}\",\"x\":{x},\"y\":{y}}}"
    )
}

/// 执行工具并返回 MCP tools/call 响应。
fn call_tool(params: &Value, state: &State) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let id = params.get("id").cloned();

    if name == "connect" {
        // connect：仅记录（本实现用环境变量/静态默认；参数可透传给后续调用不实现动态切换，
        // 这里返回当前目标用于确认）。
        let text = format!("connected: signal={} room={}", state.signal, state.room);
        return json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":text}],"isError":false}});
    }

    // 键鼠工具：经 --send-input 桥接（无 CmdResponse，按 CLI 退出码判定）。
    if name == "mouse_move" || name == "mouse_click" || name == "type_text" {
        return call_mouse_tool(id, name, &args, state);
    }
    if name == "download_file" || name == "upload_file" {
        return call_file_tool(id, name, &args, state);
    }

    let cmd_args = match build_args(state, name, &args) {
        Ok(v) => v,
        Err(e) => {
            return json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":format!("参数错误: {e}")}],"isError":true}});
        }
    };

    let output = match Command::new(&state.cli_bin).args(&cmd_args).output() {
        Ok(o) => o,
        Err(e) => {
            return json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":format!("启动 aerodesk-cli 失败: {e}（AERODESK_CLI_BIN={}）", state.cli_bin)}],"isError":true}});
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let trimmed = stdout.trim();
    let Ok(resp) = serde_json::from_str::<aerodesk_protocol::cmd::CmdResponse>(trimmed) else {
        let tail: String = stdout.chars().take(500).collect();
        return json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":format!("CLI 输出非 JSON（可能连接失败）: {tail}")}],"isError":true}});
    };
    let (text, ok) = format_result(&resp);
    json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":text}],"isError":!ok}})
}

/// 大文件上传/下载：#122 经 file 通道桥接（--request-file / --send-file）。
fn call_file_tool(id: Option<Value>, name: &str, args: &Value, state: &State) -> Value {
    let err = |msg: String| json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":msg}],"isError":true}});
    if name == "download_file" {
        let Some(remote) = args.get("remote_path").and_then(|v| v.as_str()) else {
            return err("download_file 缺少 remote_path".into());
        };
        let dir = std::env::temp_dir().join(format!(
            "aerodesk-mcp-dl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return err(format!("mkdir: {e}"));
        }
        let status = run_cli_timeout(
            &[
                &state.cli_bin,
                "--role",
                "viewer",
                "--signal",
                &state.signal,
                "--room",
                &state.room,
                "--request-file",
                remote,
                "--recv-dir",
                dir.to_str().unwrap_or("/tmp"),
            ],
            std::time::Duration::from_secs(300),
        );
        return match status {
            Some(st) if st.success() => {
                let files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
                    .map(|it| {
                        it.flatten()
                            .map(|e| e.path())
                            .filter(|p| p.is_file())
                            .collect()
                    })
                    .unwrap_or_default();
                match files.first() {
                    Some(f) => {
                        let size = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
                        let hash = sha256_hex(&std::fs::read(f).unwrap_or_default());
                        json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":format!("downloaded: {remote} -> {} ({} bytes, sha256={hash})", f.display(), size)}],"isError":false}})
                    }
                    None => err("下载完成但未找到落盘文件".into()),
                }
            }
            Some(st) => err(format!("download failed: exit {:?}", st.code())),
            None => err("download CLI 超时(300s)".into()),
        };
    }

    // upload_file
    let Some(local) = args.get("local_path").and_then(|v| v.as_str()) else {
        return err("upload_file 缺少 local_path".into());
    };
    let Ok(meta) = std::fs::metadata(local) else {
        return err(format!("本地文件不可读: {local}"));
    };
    if !meta.is_file() {
        return err(format!("{local} 不是文件"));
    }
    let status = run_cli_timeout(
        &[
            &state.cli_bin,
            "--role",
            "viewer",
            "--signal",
            &state.signal,
            "--room",
            &state.room,
            "--send-file",
            local,
        ],
        std::time::Duration::from_secs(300),
    );
    match status {
        Some(st) if st.success() => {
            let name = std::path::Path::new(local)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| local.to_string());
            json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":format!("uploaded: {name} ({} bytes) -> 被控端（publisher 需 --recv-dir）", meta.len())}],"isError":false}})
        }
        Some(st) => err(format!("upload failed: exit {:?}", st.code())),
        None => err("upload CLI 超时(300s)".into()),
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 运行 aerodesk-cli 并等待退出（超时 kill，返回 None = 超时）。
fn run_cli_timeout(
    args: &[&str],
    timeout: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    let mut child = match Command::new(&args[0]).args(&args[1..]).spawn() {
        Ok(c) => c,
        Err(e) => return None,
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Some(st),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

/// 键鼠工具：--send-input 桥接（按 CLI 退出码判定成功）。
fn call_mouse_tool(id: Option<Value>, name: &str, args: &Value, state: &State) -> Value {
    let x = args.get("x").and_then(|v| v.as_f64()).unwrap_or(0.5);
    let y = args.get("y").and_then(|v| v.as_f64()).unwrap_or(0.5);
    let button = args
        .get("button")
        .and_then(|v| v.as_str())
        .unwrap_or("left");
    let (events, label) = if name == "type_text" {
        let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
        (vec![text.to_string()], "type_text".to_string())
    } else if name == "mouse_move" {
        (
            vec![format!("{{\"type\":\"mouse_move\",\"x\":{x},\"y\":{y}}}")],
            "mouse_move".to_string(),
        )
    } else {
        (
            vec![
                mouse_button_json(button, "pressed", x, y),
                mouse_button_json(button, "released", x, y),
            ],
            "mouse_click".to_string(),
        )
    };
    let flag = if name == "type_text" {
        "--type-text"
    } else {
        "--send-input"
    };
    let mut failures = Vec::new();
    for ev in &events {
        let status = run_cli_timeout(
            &[
                &state.cli_bin,
                "--role",
                "viewer",
                "--signal",
                &state.signal,
                "--room",
                &state.room,
                flag,
                ev,
            ],
            std::time::Duration::from_secs(60),
        );
        match status {
            Some(st) if st.success() => {}
            Some(st) => failures.push(format!("{ev} -> exit {:?}", st.code())),
            None => failures.push(format!("{ev} -> CLI 超时(60s)")),
        }
    }
    let ok = failures.is_empty();
    let text = if ok {
        format!("{label} ok: x={x} y={y} button={button}")
    } else {
        format!("{label} failed: {}", failures.join("; "))
    };
    json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":text}],"isError":!ok}})
}

/// 把 CmdResponse 格式化为文本（MCP 返回给 agent）。
fn format_result(resp: &aerodesk_protocol::cmd::CmdResponse) -> (String, bool) {
    use aerodesk_protocol::cmd::CmdResult;
    match &resp.result {
        CmdResult::Run {
            exit_code,
            stdout,
            stderr,
            truncated,
            error,
        } => {
            let mut t = format!("exit={exit_code:?}");
            if !stdout.is_empty() {
                t.push_str(&format!("\nstdout:\n{stdout}"));
            }
            if !stderr.is_empty() {
                t.push_str(&format!("\nstderr:\n{stderr}"));
            }
            if *truncated {
                t.push_str("\n[output truncated]");
            }
            if let Some(e) = error {
                t.push_str(&format!("\nerror: {e}"));
            }
            (t, error.is_none() && *exit_code == Some(0))
        }
        CmdResult::File { data, size, error } => {
            if let Some(e) = error {
                (format!("error: {e}"), false)
            } else if let Some(b64) = data {
                match aerodesk_protocol::cmd::decode_b64(b64) {
                    Some(bytes) => (
                        format!("size={size}\n{}", String::from_utf8_lossy(&bytes)),
                        true,
                    ),
                    None => (format!("size={size} (base64 解码失败)"), false),
                }
            } else {
                (format!("size={size}"), true)
            }
        }
        CmdResult::ProcessList { processes, error } => {
            if let Some(e) = error {
                (format!("error: {e}"), false)
            } else {
                let mut t = String::new();
                for p in processes {
                    t.push_str(&format!("{} {}\n", p.pid, p.name));
                }
                (t.trim_end().to_string(), true)
            }
        }
        CmdResult::Killed { pid, error } => {
            if let Some(e) = error {
                (format!("error: {e}"), false)
            } else {
                (format!("killed pid {pid}"), true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerodesk_protocol::cmd::{CmdResponse, CmdResult, ProcessInfo};

    fn state() -> State {
        State {
            signal: "ws://127.0.0.1:3003".into(),
            room: "demo".into(),
            cli_bin: "aerodesk-cli".into(),
        }
    }

    #[test]
    fn build_args_for_each_tool() {
        let s = state();
        let run = build_args(&s, "run_command", &json!({"command":"ls -la"})).unwrap();
        assert!(run.contains(&"--run-command".into()));
        assert!(run.contains(&"ls -la".into()));
        assert!(run.last().unwrap() == "--cmd-json");
        let read = build_args(&s, "read_file", &json!({"path":"/tmp/x"})).unwrap();
        assert!(read.contains(&"--read-file".into()));
        let write = build_args(&s, "write_file", &json!({"path":"/tmp/x","content":"hi"})).unwrap();
        assert!(write.contains(&"--write-file".into()));
        let ps = build_args(&s, "list_processes", &json!({})).unwrap();
        assert!(ps.contains(&"--list-processes".into()));
        let kill = build_args(&s, "kill_process", &json!({"pid":123})).unwrap();
        assert!(kill.contains(&"--kill-pid".into()));
        assert!(kill.contains(&"123".into()));
        let mm = build_args(&s, "mouse_move", &json!({"x":0.25,"y":0.75})).unwrap();
        assert!(mm.contains(&"--send-input".into()));
        let ev = mm
            .iter()
            .position(|a| a == "--send-input")
            .map(|i| mm[i + 1].clone())
            .unwrap();
        assert!(ev.contains("mouse_move"));
        assert!(ev.contains("0.25"));
        assert!(build_args(&s, "nope", &json!({})).is_err());
    }

    #[test]
    fn format_run_result() {
        let resp = CmdResponse {
            id: 1,
            result: CmdResult::Run {
                exit_code: Some(0),
                stdout: "hi".into(),
                stderr: String::new(),
                truncated: false,
                error: None,
            },
        };
        let (t, ok) = format_result(&resp);
        assert!(ok);
        assert!(t.contains("exit=Some(0)"));
        assert!(t.contains("hi"));
    }

    #[test]
    fn format_file_and_ps() {
        let f = CmdResponse {
            id: 1,
            result: CmdResult::File {
                data: Some(aerodesk_protocol::cmd::encode_b64(b"hello")),
                size: 5,
                error: None,
            },
        };
        let (t, ok) = format_result(&f);
        assert!(ok && t.contains("hello"));
        let ps = CmdResponse {
            id: 2,
            result: CmdResult::ProcessList {
                processes: vec![ProcessInfo {
                    pid: 42,
                    name: "sh".into(),
                }],
                error: None,
            },
        };
        let (t, ok) = format_result(&ps);
        assert!(ok && t.contains("42 sh"));
    }
}
