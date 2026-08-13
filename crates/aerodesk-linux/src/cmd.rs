//! Linux 远程命令执行器（#330「bash」平台抽象）。
//!
//! 实现 [`aerodesk_core::platform::CommandExecutor`]：Linux 与 core unix 默认
//! 行为一致（`sh -c` / `ps` / `kill`），当前直接委托默认实现；本模块是 Linux
//! 平台扩展点（后续可按需接入 systemd-run 沙箱、XDG 文件处理等）。

use aerodesk_core::cmd_exec::{CmdOutput, DefaultCommandExecutor};
use aerodesk_core::platform::CommandExecutor;
use aerodesk_protocol::cmd::ProcessInfo;

/// Linux 命令执行器（`sh -c`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxCommandExecutor;

impl CommandExecutor for LinuxCommandExecutor {
    fn run_command(&self, command: &str, cwd: Option<&str>, timeout_ms: Option<u64>) -> CmdOutput {
        DefaultCommandExecutor.run_command(command, cwd, timeout_ms)
    }

    fn read_file(&self, path: &str, max_bytes: Option<usize>) -> Result<Vec<u8>, String> {
        DefaultCommandExecutor.read_file(path, max_bytes)
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        DefaultCommandExecutor.write_file(path, data)
    }

    fn list_processes(&self) -> Result<Vec<ProcessInfo>, String> {
        DefaultCommandExecutor.list_processes()
    }

    fn kill_process(&self, pid: u32) -> Result<(), String> {
        DefaultCommandExecutor.kill_process(pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executes_shell_command() {
        let ex = LinuxCommandExecutor;
        let out = ex.run_command("echo linux-command-executor", None, Some(2000));
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("linux-command-executor"));
        assert!(out.error.is_none());
    }

    #[test]
    fn object_safe_extension_point() {
        let ex: Box<dyn CommandExecutor> = Box::new(LinuxCommandExecutor);
        let out = ex.run_command("true", None, Some(1000));
        assert_eq!(out.exit_code, Some(0));
        assert!(out.error.is_none());
    }

    #[test]
    fn lists_processes() {
        let procs = LinuxCommandExecutor.list_processes().unwrap();
        assert!(!procs.is_empty());
    }
}
