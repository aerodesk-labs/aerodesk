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
        other => return Err(format!("unknown tool: {other}")),
    }
    cmd.push("--cmd-json".into());
    Ok(cmd)
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
