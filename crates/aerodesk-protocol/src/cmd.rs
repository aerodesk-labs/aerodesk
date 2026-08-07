//! 远程命令执行协议（#109）：控制端 → 被控端，经 data channel（label "cmd"）。
//!
//! 与输入通道方向一致：viewer（控制端）发送 `CmdRequest`，被控端执行后回
//! `CmdResponse`（stdout/stderr/exit code）。权限（白名单/危险命令拦截）与
//! 审计在被控端执行器侧落地（aerodesk-core::cmd_exec）。

use serde::{Deserialize, Serialize};

/// 命令请求（控制端 → 被控端）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmdRequest {
    pub id: u64,
    /// 命令（由被控端 shell 执行：unix `sh -c`，Windows `cmd /C`）。
    pub command: String,
    /// 工作目录（可选）。
    #[serde(default)]
    pub cwd: Option<String>,
    /// 超时毫秒（默认 30s；0/None = 默认）。
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl CmdRequest {
    pub fn new(id: u64, command: impl Into<String>) -> Self {
        Self {
            id,
            command: command.into(),
            cwd: None,
            timeout_ms: None,
        }
    }
}

/// 命令响应（被控端 → 控制端）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmdResponse {
    pub id: u64,
    pub ok: bool,
    /// 退出码（启动失败/超时/被拦截时为 None）。
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// 输出被截断（单流上限 1MB）。
    pub truncated: bool,
    /// 执行错误（被拦截/超时/启动失败）。
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip_json() {
        let req = CmdRequest::new(7, "ls -la").cwd_path("/tmp").timeout(5000);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"command\":\"ls -la\""));
        assert!(json.contains("\"cwd\":\"/tmp\""));
        let back: CmdRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn request_without_optional_fields_parses() {
        // 旧/精简控制端可不带 cwd/timeout。
        let back: CmdRequest = serde_json::from_str("{\"id\":1,\"command\":\"pwd\"}").unwrap();
        assert_eq!(back.cwd, None);
        assert_eq!(back.timeout_ms, None);
    }

    #[test]
    fn response_roundtrip_json() {
        let resp = CmdResponse {
            id: 7,
            ok: true,
            exit_code: Some(0),
            stdout: "hi\n".into(),
            stderr: String::new(),
            truncated: false,
            error: None,
        };
        let back: CmdResponse =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(back, resp);
    }
}

// 便捷构造（保持 CmdRequest::new 的调用链可读）。
impl CmdRequest {
    pub fn cwd_path(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
    pub fn timeout(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }
}
