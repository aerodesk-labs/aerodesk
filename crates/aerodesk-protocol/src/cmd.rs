//! 远程命令/文件/进程协议（#109）：控制端 → 被控端，经 data channel（label "cmd"）。
//!
//! 与输入通道方向一致：viewer（控制端）发送 `CmdRequest`，被控端执行后回
//! `CmdResponse`。权限（白名单/危险拦截）与审计在被控端执行器侧落地
//! （aerodesk-core::cmd_exec）。

use base64ct::{Base64, Encoding};
use serde::{Deserialize, Serialize};

/// #503 系统电源动作（关机/重启/锁屏）：内置安全命令，动作枚举受限、
/// 不接受自由参数（杜绝 shell 注入），由被控端按平台构造固定系统命令执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    /// 关机。
    Shutdown,
    /// 重启。
    Reboot,
    /// 锁屏。
    Lock,
}

impl PowerAction {
    /// 中文动作名（UI 展示/日志用）。
    pub fn label(&self) -> &'static str {
        match self {
            PowerAction::Shutdown => "关机",
            PowerAction::Reboot => "重启",
            PowerAction::Lock => "锁屏",
        }
    }
}

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
    /// #458 发消息：双向文本消息（复用 cmd 通道，避免新增 data channel 破坏媒体协商）。
    Chat {
        text: String,
        #[serde(default)]
        sender: String,
        #[serde(default)]
        timestamp_ms: u64,
    },
    /// #503 系统电源命令（关机/重启/锁屏）：内置安全命令，动作枚举受限，
    /// 不经 shell 拼接（比裸 run_command 更可控），被控端执行后写 cmd 审计。
    SystemPower { action: PowerAction },
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
        /// #13 结构化错误码（[`crate::error::ErrorCode`] 的 wire 串；旧对端缺省 None）。
        #[serde(default)]
        code: Option<String>,
    },
    File {
        /// base64 数据。
        #[serde(default)]
        data: Option<String>,
        size: u64,
        #[serde(default)]
        error: Option<String>,
        /// #13 结构化错误码（[`crate::error::ErrorCode`] 的 wire 串；旧对端缺省 None）。
        #[serde(default)]
        code: Option<String>,
    },
    ProcessList {
        #[serde(default)]
        processes: Vec<ProcessInfo>,
        #[serde(default)]
        error: Option<String>,
        /// #13 结构化错误码（[`crate::error::ErrorCode`] 的 wire 串；旧对端缺省 None）。
        #[serde(default)]
        code: Option<String>,
    },
    Killed {
        pid: u32,
        #[serde(default)]
        error: Option<String>,
        /// #13 结构化错误码（[`crate::error::ErrorCode`] 的 wire 串；旧对端缺省 None）。
        #[serde(default)]
        code: Option<String>,
    },
    /// 发消息回显（被控端收到 Chat 后回给观看端）。
    Chat {
        #[serde(default)]
        sender: String,
        text: String,
    },
    /// #503 电源命令回执（关机/重启成功后对端可能不再回话，控制端应提示预期）。
    Power {
        action: PowerAction,
        #[serde(default)]
        error: Option<String>,
        /// #13 结构化错误码（[`crate::error::ErrorCode`] 的 wire 串；旧对端缺省 None）。
        #[serde(default)]
        code: Option<String>,
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

    /// #503 系统电源命令（关机/重启/锁屏）。
    pub fn system_power(id: u64, action: PowerAction) -> Self {
        Self::new(id, CmdAction::SystemPower { action })
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
            CmdResult::Chat { .. } => true,
            CmdResult::Power { error, .. } => error.is_none(),
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
            code: None,
        };
        assert!(run.ok());
        let run_err = CmdResult::Run {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            error: Some("blocked by policy".into()),
            code: Some("blocked_by_policy".into()),
        };
        assert!(!run_err.ok());

        // #13 兼容性：旧 JSON（无 code 字段）仍可解析，code 缺省 None。
        let old_json = r#"{"id":1,"result":{"type":"run","exit_code":null,"stdout":"","stderr":"","truncated":false,"error":"boom"}}"#;
        let old: CmdResponse = serde_json::from_str(old_json).unwrap();
        match old.result {
            CmdResult::Run { code, error, .. } => {
                assert_eq!(code, None);
                assert_eq!(error.as_deref(), Some("boom"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        // 新 JSON（带 code）round-trip 保持字段。
        let new_json = r#"{"id":2,"result":{"type":"run","exit_code":null,"stdout":"","stderr":"","truncated":false,"error":"boom","code":"blocked_by_policy"}}"#;
        let new: CmdResponse = serde_json::from_str(new_json).unwrap();
        match new.result {
            CmdResult::Run { code, .. } => assert_eq!(code.as_deref(), Some("blocked_by_policy")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// #503：电源命令 wire 格式（snake_case 枚举 + round-trip）。
    #[test]
    fn system_power_roundtrip() {
        let req = CmdRequest::system_power(7, PowerAction::Shutdown);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"system_power\""));
        assert!(json.contains("\"action\":\"shutdown\""));
        let back: CmdRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
        if let CmdAction::SystemPower { action } = &back.action {
            assert_eq!(*action, PowerAction::Shutdown);
        } else {
            panic!("wrong action");
        }
        // 其余动作枚举序列化
        assert!(
            serde_json::to_string(&PowerAction::Reboot)
                .unwrap()
                .contains("reboot")
        );
        assert!(
            serde_json::to_string(&PowerAction::Lock)
                .unwrap()
                .contains("lock")
        );
        // 未知动作拒绝解析（枚举受限，不接受自由参数）
        assert!(serde_json::from_str::<PowerAction>("\"format_disk\"").is_err());
        // 回执 ok 语义
        let ok = CmdResult::Power {
            action: PowerAction::Lock,
            error: None,
            code: None,
        };
        assert!(ok.ok());
        let err = CmdResult::Power {
            action: PowerAction::Lock,
            error: Some("blocked by policy".into()),
            code: Some("blocked_by_policy".into()),
        };
        assert!(!err.ok());
        // 中文动作名（UI/审计展示）
        assert_eq!(PowerAction::Reboot.label(), "重启");
    }
}
