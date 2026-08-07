//! 远程命令/文件/进程协议（#109）：控制端 → 被控端，经 data channel（label "cmd"）。
//!
//! 与输入通道方向一致：viewer（控制端）发送 `CmdRequest`，被控端执行后回
//! `CmdResponse`。权限（白名单/危险拦截）与审计在被控端执行器侧落地
//! （aerodesk-core::cmd_exec）。

use base64ct::{Base64, Encoding};
use serde::{Deserialize, Serialize};

/// 命令/文件/进程请求（控制端 → 被控端）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmdRequest {
    pub id: u64,
    pub action: CmdAction,
}

/// 请求动作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CmdAction {
    /// 执行 shell 命令（unix `sh -c`，Windows `cmd /C`）。
    Run {
        command: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// 读文件（默认上限 4MB，可指定）。
    ReadFile {
        path: String,
        #[serde(default)]
        max_bytes: Option<usize>,
    },
    /// 写文件（data 为 base64；系统敏感路径默认禁止，白名单可放行）。
    WriteFile { path: String, data: String },
    /// 列出进程。
    ListProcesses,
    /// 结束进程（pid 0/1 默认禁止）。
    KillProcess { pid: u32 },
}

/// 命令/文件/进程响应（被控端 → 控制端）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmdResponse {
    pub id: u64,
    pub result: CmdResult,
}

/// 响应结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CmdResult {
    Run {
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        truncated: bool,
        #[serde(default)]
        error: Option<String>,
    },
    File {
        /// base64 数据。
        #[serde(default)]
        data: Option<String>,
        size: u64,
        #[serde(default)]
        error: Option<String>,
    },
    ProcessList {
        #[serde(default)]
        processes: Vec<ProcessInfo>,
        #[serde(default)]
        error: Option<String>,
    },
    Killed {
        pid: u32,
        #[serde(default)]
        error: Option<String>,
    },
}

/// 进程信息（list_processes 结果项）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
}

impl CmdRequest {
    pub fn new(id: u64, action: CmdAction) -> Self {
        Self { id, action }
    }

    pub fn run(id: u64, command: impl Into<String>) -> Self {
        Self::new(
            id,
            CmdAction::Run {
                command: command.into(),
                cwd: None,
                timeout_ms: None,
            },
        )
    }

    pub fn read_file(id: u64, path: impl Into<String>) -> Self {
        Self::new(
            id,
            CmdAction::ReadFile {
                path: path.into(),
                max_bytes: None,
            },
        )
    }

    pub fn write_file(id: u64, path: impl Into<String>, data: &[u8]) -> Self {
        Self::new(
            id,
            CmdAction::WriteFile {
                path: path.into(),
                data: Base64::encode_string(data),
            },
        )
    }

    pub fn list_processes(id: u64) -> Self {
        Self::new(id, CmdAction::ListProcesses)
    }

    pub fn kill_process(id: u64, pid: u32) -> Self {
        Self::new(id, CmdAction::KillProcess { pid })
    }
}

impl CmdResult {
    /// 简单成功判定（e2e/CLI 退出码语义）。
    pub fn ok(&self) -> bool {
        match self {
            CmdResult::Run {
                exit_code, error, ..
            } => error.is_none() && *exit_code == Some(0),
            CmdResult::File { error, .. } => error.is_none(),
            CmdResult::ProcessList { error, .. } => error.is_none(),
            CmdResult::Killed { error, .. } => error.is_none(),
        }
    }
}

/// 把二进制数据编解码为 base64（协议层文件传输用）。
pub fn encode_b64(data: &[u8]) -> String {
    Base64::encode_string(data)
}

pub fn decode_b64(s: &str) -> Option<Vec<u8>> {
    Base64::decode_vec(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_request_roundtrip_json() {
        let req = CmdRequest::run(7, "ls -la");
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"run\""));
        assert!(json.contains("\"command\":\"ls -la\""));
        let back: CmdRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn read_write_file_roundtrip() {
        let w = CmdRequest::write_file(1, "/tmp/x.txt", b"hello");
        let json = serde_json::to_string(&w).unwrap();
        assert!(json.contains("\"type\":\"write_file\""));
        let back: CmdRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, w);
        if let CmdAction::WriteFile { data, .. } = &back.action {
            assert_eq!(decode_b64(data).unwrap(), b"hello");
        } else {
            panic!("wrong action");
        }
        let r = CmdRequest::read_file(2, "/tmp/x.txt");
        assert!(
            serde_json::to_string(&r)
                .unwrap()
                .contains("\"type\":\"read_file\"")
        );
    }

    #[test]
    fn response_ok_semantics() {
        let run = CmdResult::Run {
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            error: None,
        };
        assert!(run.ok());
        let run_err = CmdResult::Run {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            error: Some("blocked by policy".into()),
        };
        assert!(!run_err.ok());
    }
}
