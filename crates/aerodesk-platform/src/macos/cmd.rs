//! macOS 远程命令执行器（#330「bash」平台抽象）。
//!
//! 实现 [`aerodesk_core::platform::CommandExecutor`]：macOS 与 core unix 默认
//! 行为一致（`sh -c` / `ps` / `kill`），当前直接委托默认实现；本模块是 macOS
//! 平台扩展点（后续可按需接入 `sandbox-exec`、`open` 文件处理、AppleScript 等）。

use aerodesk_core::cmd_exec::{CmdExecError, CmdOutput, DefaultCommandExecutor};
use aerodesk_core::platform::CommandExecutor;
use aerodesk_core::protocol::cmd::ProcessInfo;

/// macOS 命令执行器（`sh -c`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct MacCommandExecutor;

impl CommandExecutor for MacCommandExecutor {
    fn run_command(&self, command: &str, cwd: Option<&str>, timeout_ms: Option<u64>) -> CmdOutput {
        DefaultCommandExecutor.run_command(command, cwd, timeout_ms)
    }

    fn read_file(&self, path: &str, max_bytes: Option<usize>) -> Result<Vec<u8>, CmdExecError> {
        DefaultCommandExecutor.read_file(path, max_bytes)
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), CmdExecError> {
        DefaultCommandExecutor.write_file(path, data)
    }

    fn list_processes(&self) -> Result<Vec<ProcessInfo>, CmdExecError> {
        DefaultCommandExecutor.list_processes()
    }

    fn kill_process(&self, pid: u32) -> Result<(), CmdExecError> {
        DefaultCommandExecutor.kill_process(pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executes_shell_command() {
        let ex = MacCommandExecutor;
        let out = ex.run_command("echo mac-command-executor", None, Some(2000));
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("mac-command-executor"));
        assert!(out.error.is_none());
    }

    #[test]
    fn object_safe_extension_point() {
        let ex: Box<dyn CommandExecutor> = Box::new(MacCommandExecutor);
        let out = ex.run_command("true", None, Some(1000));
        assert_eq!(out.exit_code, Some(0));
        assert!(out.error.is_none());
    }

    #[test]
    fn lists_processes() {
        let procs = MacCommandExecutor.list_processes().unwrap();
        assert!(!procs.is_empty());
    }
}
