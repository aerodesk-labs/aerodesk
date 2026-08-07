//! 远程命令执行（#109）：危险命令拦截 + 白名单 + 超时 + 输出截断 + 审计。
//!
//! 被控端执行器：unix 用 `sh -c`，Windows 用 `cmd /C`。默认禁止破坏性/交互式
//! 命令（白名单前缀可放行）；单流输出上限 1MB；超时强杀；审计写 JSONL。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aerodesk_protocol::cmd::{ProcessInfo, decode_b64, encode_b64};

/// 单流（stdout/stderr）输出上限。
pub const MAX_OUTPUT_BYTES: usize = 1 << 20;
/// 默认超时。
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// 读文件默认上限。
pub const DEFAULT_READ_MAX_BYTES: usize = 4 << 20;
/// 读文件硬上限（防内存失控）。
pub const MAX_READ_BYTES: usize = 16 << 20;

/// 命令执行结果。
#[derive(Debug, Clone, Default)]
pub struct CmdOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub error: Option<String>,
}

/// 危险/交互式命令模式（默认禁止）。命中即拦截，除非命令以白名单前缀开头。
pub fn is_dangerous(command: &str) -> bool {
    let c = command.trim().to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        // 破坏性
        "rm -rf",
        "rm -fr",
        "rm -r -f",
        "dd ",
        "mkfs",
        "fdisk",
        "diskutil erase",
        "diskutil zero",
        "shutdown",
        "reboot",
        "poweroff",
        "halt",
        "init 0",
        "init 6",
        "sudo rm",
        "sudo dd",
        "sudo shutdown",
        "sudo reboot",
        "del /s",
        "format c:",
        "reg delete",
        "chmod -r 777 /",
        "chown -r /",
        "mv / ",
        "cp -r / ",
        // 交互式（AI 无法应答）
        "vim",
        "nano",
        "emacs",
        "less",
        "more",
        "man ",
        "ssh ",
        "telnet ",
        "ftp ",
        "top",
        "htop",
        "crontab -e",
        "mysql",
        "psql",
        "python",
        "python3",
        "node",
        "irb",
        "bc",
        // 其他危险
        ":(){",
        "> /dev/sda",
        "echo > /etc/",
        "mkpasswd",
    ];
    PATTERNS.iter().any(|p| c.contains(p))
}

/// 白名单文件路径：`$AERODESK_CMD_ALLOWLIST` 或 `$HOME/AeroDesk/cmd-allowlist.txt`。
/// 每行一个命令前缀；命令以某前缀开头时放行（可覆盖危险拦截）。
pub fn allowlist_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AERODESK_CMD_ALLOWLIST") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("AeroDesk/cmd-allowlist.txt"))
}

/// 读取白名单前缀列表。
pub fn allowlist() -> Vec<String> {
    let Some(p) = allowlist_path() else {
        return Vec::new();
    };
    std::fs::read_to_string(p)
        .map(|s| {
            s.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect()
        })
        .unwrap_or_default()
}

/// 审计文件路径：`$AERODESK_CMD_AUDIT` 或 `$HOME/AeroDesk/cmd-audit.jsonl`。
pub fn audit_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AERODESK_CMD_AUDIT") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("AeroDesk/cmd-audit.jsonl"))
}

/// 读取管道直到 EOF/达到上限/收到停止信号（设置截断标记）。
/// unix 下用非阻塞读：后台子进程继承管道时不会 EOF，停止信号可立即退出，
/// 避免 `run_command("sleep 100 & ...")` 之类命令卡死读线程（#109）。
#[cfg(unix)]
fn read_with_cap<R: Read + std::os::unix::io::AsRawFd>(
    mut pipe: Option<R>,
    buf: std::sync::Arc<Mutex<Vec<u8>>>,
    trunc: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    if let Some(pipe) = pipe.as_mut() {
        let fd = pipe.as_raw_fd();
        unsafe {
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        let mut tmp = [0u8; 8192];
        loop {
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            let n = match pipe.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            };
            let mut b = buf.lock().unwrap();
            let room = MAX_OUTPUT_BYTES.saturating_sub(b.len());
            if n >= room {
                b.extend_from_slice(&tmp[..room]);
                trunc.store(true, std::sync::atomic::Ordering::SeqCst);
                break;
            }
            b.extend_from_slice(&tmp[..n]);
        }
    }
}

/// 非 unix 回退：阻塞读直到 EOF（后台进程继承管道场景在 Windows 较少见）。
#[cfg(not(unix))]
fn read_with_cap<R: Read>(
    mut pipe: Option<R>,
    buf: std::sync::Arc<Mutex<Vec<u8>>>,
    trunc: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    if let Some(pipe) = pipe.as_mut() {
        let mut tmp = [0u8; 8192];
        loop {
            let n = match pipe.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut b = buf.lock().unwrap();
            let room = MAX_OUTPUT_BYTES.saturating_sub(b.len());
            if n >= room {
                b.extend_from_slice(&tmp[..room]);
                trunc.store(true, std::sync::atomic::Ordering::SeqCst);
                break;
            }
            b.extend_from_slice(&tmp[..n]);
        }
    }
}

/// 执行命令（含策略/超时/截断/审计）。allowlist 为命令前缀放行清单。
pub fn run_command(
    command: &str,
    cwd: Option<&str>,
    timeout_ms: Option<u64>,
    allowlist: &[String],
) -> CmdOutput {
    let command = command.trim().to_string();
    if command.is_empty() {
        return CmdOutput {
            error: Some("empty command".into()),
            ..Default::default()
        };
    }
    // 白名单前缀放行（覆盖危险拦截）。
    let allowed = allowlist
        .iter()
        .any(|p| !p.is_empty() && command.starts_with(p.as_str()));
    if is_dangerous(&command) && !allowed {
        let out = CmdOutput {
            error: Some(format!("blocked by policy: {command}")),
            ..Default::default()
        };
        audit(&command, cwd, &out);
        return out;
    }

    let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).max(100));

    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(&command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&command);
        c
    };
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let out = CmdOutput {
                error: Some(format!("spawn failed: {e}")),
                ..Default::default()
            };
            audit(&command, cwd, &out);
            return out;
        }
    };

    // 两个读线程：并行读 stdout/stderr，各带上限，避免管道写满死锁。
    let stdout_buf: std::sync::Arc<Mutex<Vec<u8>>> = Default::default();
    let stderr_buf: std::sync::Arc<Mutex<Vec<u8>>> = Default::default();
    let stdout_trunc = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stderr_trunc = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stop_reading = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let h1 = std::thread::spawn({
        let buf = stdout_buf.clone();
        let trunc = stdout_trunc.clone();
        let stop = stop_reading.clone();
        move || read_with_cap(stdout_pipe, buf, trunc, stop)
    });
    let h2 = std::thread::spawn({
        let buf = stderr_buf.clone();
        let trunc = stderr_trunc.clone();
        let stop = stop_reading.clone();
        move || read_with_cap(stderr_pipe, buf, trunc, stop)
    });
    let handles = vec![h1, h2];

    // 轮询退出 + 超时强杀。
    let deadline = Instant::now() + timeout;
    let mut exit_code: Option<i32> = None;
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                exit_code = st.code();
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let out = CmdOutput {
                    error: Some(format!("wait failed: {e}")),
                    ..Default::default()
                };
                audit(&command, cwd, &out);
                return out;
            }
        }
    }
    // 子进程退出后给读线程 200ms 排空窗口，随后停止（后台进程继承管道时
    // 不会 EOF，必须主动停，否则 run_command 卡死）。
    std::thread::sleep(Duration::from_millis(200));
    stop_reading.store(true, std::sync::atomic::Ordering::SeqCst);
    for h in handles {
        let _ = h.join();
    }

    let stdout_truncated = stdout_trunc.load(std::sync::atomic::Ordering::SeqCst);
    let stderr_truncated = stderr_trunc.load(std::sync::atomic::Ordering::SeqCst);
    let stdout = String::from_utf8_lossy(&stdout_buf.lock().unwrap()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buf.lock().unwrap()).into_owned();
    let out = CmdOutput {
        exit_code,
        stdout,
        stderr,
        truncated: stdout_truncated || stderr_truncated,
        error: if timed_out {
            Some(format!("timeout after {}ms", timeout.as_millis()))
        } else {
            None
        },
    };
    audit(&command, cwd, &out);
    out
}

/// 追加审计记录（JSONL）：时间/命令/工作目录/退出码/错误/输出字节数。
pub fn audit(command: &str, cwd: Option<&str>, out: &CmdOutput) {
    let Some(path) = audit_path() else {
        return;
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let rec = serde_json::json!({
        "ts": ts,
        "command": command,
        "cwd": cwd,
        "exit_code": out.exit_code,
        "error": out.error,
        "stdout_bytes": out.stdout.len(),
        "stderr_bytes": out.stderr.len(),
        "truncated": out.truncated,
    });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{rec}");
    }
}

/// 写文件敏感路径（默认禁止，白名单前缀可放行）：系统目录/关键配置。
pub fn is_forbidden_write_path(path: &str) -> bool {
    let p = path.trim();
    let abs = if p.starts_with('~') {
        std::env::var("HOME")
            .map(|h| format!("{h}{}", &p[1..]))
            .unwrap_or_else(|_| p.to_string())
    } else {
        p.to_string()
    };
    let norm = abs.replace("//", "/");
    const FORBIDDEN_PREFIXES: &[&str] = &[
        "/etc/",
        "/System/",
        "/usr/",
        "/bin/",
        "/sbin/",
        "/var/root/",
        "/dev/",
        "/private/etc/",
        "/private/var/db/",
        "/cores/",
    ];
    FORBIDDEN_PREFIXES.iter().any(|f| norm.starts_with(f)) || norm.contains("/../") || norm == "/"
}

/// 读文件（上限 max_bytes，默认 4MB，硬上限 16MB）。
pub fn read_file(path: &str, max_bytes: Option<usize>) -> Result<Vec<u8>, String> {
    let cap = max_bytes
        .unwrap_or(DEFAULT_READ_MAX_BYTES)
        .min(MAX_READ_BYTES);
    let data = std::fs::read(path).map_err(|e| format!("read failed: {e}"))?;
    if data.len() > cap {
        return Err(format!("file too large ({}B > {}B)", data.len(), cap));
    }
    Ok(data)
}

/// 写文件（敏感路径默认禁止，白名单前缀放行；data 为 base64）。
pub fn write_file(path: &str, data_b64: &str, allowlist: &[String]) -> Result<(), String> {
    let allowed = allowlist
        .iter()
        .any(|p| !p.is_empty() && path.starts_with(p.as_str()));
    if is_forbidden_write_path(path) && !allowed {
        return Err(format!("blocked by policy: write {path}"));
    }
    let data = decode_b64(data_b64).ok_or_else(|| "invalid base64".to_string())?;
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    std::fs::write(path, data).map_err(|e| format!("write failed: {e}"))
}

/// 列出进程（unix：`ps -axo pid=,comm=`；Windows：tasklist）。
pub fn list_processes() -> Result<Vec<ProcessInfo>, String> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("tasklist");
        c.arg("/FO").arg("CSV");
        c
    } else {
        let mut c = Command::new("ps");
        c.arg("-axo").arg("pid=,comm=");
        c
    };
    let out = cmd.output().map_err(|e| format!("ps failed: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut procs = Vec::new();
    if cfg!(windows) {
        for line in text.lines().skip(1) {
            let mut parts = line.trim_matches('"').split("\",\"");
            if let Some(pid_s) = parts.next() {
                if let Ok(pid) = pid_s.trim().parse::<u32>() {
                    procs.push(ProcessInfo {
                        pid,
                        name: "?".into(),
                    });
                }
            }
        }
    } else {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.splitn(2, char::is_whitespace);
            let pid = it.next().and_then(|p| p.trim().parse::<u32>().ok());
            let name = it.next().unwrap_or("?").trim().to_string();
            if let Some(pid) = pid {
                procs.push(ProcessInfo { pid, name });
            }
        }
    }
    Ok(procs)
}

/// 结束进程（unix：`kill`；Windows：taskkill）。pid 0/1 默认禁止。
pub fn kill_process(pid: u32, allowlist: &[String]) -> Result<(), String> {
    let allowed = allowlist
        .iter()
        .any(|p| p == "*" || p == &format!("kill:{pid}"));
    if (pid == 0 || pid == 1) && !allowed {
        return Err(format!("blocked by policy: kill pid {pid}"));
    }
    let status = if cfg!(windows) {
        Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/F")
            .status()
            .map_err(|e| format!("taskkill failed: {e}"))?
    } else {
        Command::new("kill")
            .arg(pid.to_string())
            .status()
            .map_err(|e| format!("kill failed: {e}"))?
    };
    if status.success() {
        Ok(())
    } else {
        Err(format!("kill pid {pid} exit {:?}", status.code()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_patterns_are_blocked() {
        for c in [
            "rm -rf /",
            "sudo rm -rf /tmp/x",
            "dd if=/dev/zero of=/dev/sda",
            "shutdown -h now",
            "reboot",
            "vim /etc/hosts",
            "ssh root@host",
            "top",
        ] {
            assert!(is_dangerous(c), "{c} 应判为危险");
        }
        for c in [
            "echo hello",
            "ls -la",
            "cat /etc/hosts",
            "grep -v ignore /tmp/x",
        ] {
            assert!(!is_dangerous(c), "{c} 不应判为危险");
        }
    }

    #[test]
    fn allowlist_overrides_dangerous() {
        let allow = vec!["echo".to_string()];
        // echo 本身不危险；用允许前缀放行一条被误判的（如 "echo rm -rf"）。
        assert!(is_dangerous("echo rm -rf /"));
        let out = run_command("echo rm -rf /", None, Some(1000), &allow);
        assert!(out.error.is_none(), "白名单应放行: {:?}", out.error);
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("rm -rf /"));
    }

    #[test]
    fn blocked_dangerous_command_returns_error_without_running() {
        let out = run_command("rm -rf /", None, Some(1000), &[]);
        assert!(out.error.unwrap().contains("blocked by policy"));
        assert_eq!(out.exit_code, None);
    }

    #[test]
    fn echo_returns_stdout_and_exit_code() {
        let out = run_command("echo hello-aerodesk", None, Some(2000), &[]);
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("hello-aerodesk"));
        assert!(out.error.is_none());
    }

    #[test]
    fn timeout_kills_long_command() {
        let out = run_command("sleep 5", None, Some(300), &[]);
        assert!(out.exit_code.is_none() || out.exit_code != Some(0));
        assert!(out.error.unwrap().contains("timeout"));
    }

    #[test]
    fn output_is_truncated_at_cap() {
        // 产出 >1MB：单流截断。
        let out = run_command("yes x | head -c 2000000", None, Some(5000), &[]);
        assert!(out.truncated, "应标记截断");
        assert!(out.stdout.len() <= MAX_OUTPUT_BYTES + 1);
    }

    #[test]
    fn audit_writes_jsonl() {
        let dir = std::env::temp_dir().join(format!("aerodesk-cmd-audit-{}", std::process::id()));
        let audit = dir.join("audit.jsonl");
        // edition 2024：进程环境变量修改为 unsafe。
        unsafe { std::env::set_var("AERODESK_CMD_AUDIT", &audit) };
        let out = run_command("echo audited", None, Some(1000), &[]);
        unsafe { std::env::remove_var("AERODESK_CMD_AUDIT") };
        assert_eq!(out.exit_code, Some(0));
        let text = std::fs::read_to_string(&audit).expect("audit file");
        assert!(text.contains("echo audited"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_roundtrip_and_cap() {
        let dir = std::env::temp_dir().join(format!("aerodesk-cmd-f-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("hello.txt");
        std::fs::write(&f, b"hello-aerodesk-file").unwrap();
        let data = read_file(f.to_str().unwrap(), None).unwrap();
        assert_eq!(data, b"hello-aerodesk-file");
        // 超上限报错
        let err = read_file(f.to_str().unwrap(), Some(4)).unwrap_err();
        assert!(err.contains("too large"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_roundtrip_and_policy() {
        let dir = std::env::temp_dir().join(format!("aerodesk-cmd-w-{}", std::process::id()));
        let f = dir.join("out.txt");
        write_file(f.to_str().unwrap(), &encode_b64(b"hello-write"), &[]).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"hello-write");
        // 敏感路径默认禁止
        let err = write_file("/etc/aerodesk-test.txt", &encode_b64(b"x"), &[]).unwrap_err();
        assert!(err.contains("blocked by policy"));
        // 白名单放行：策略不再拦截（剩余为系统权限错误，而非 blocked by policy）
        let err2 = write_file(
            "/etc/aerodesk-test.txt",
            &encode_b64(b"x"),
            &["/etc/aerodesk-test".to_string()],
        )
        .unwrap_err();
        assert!(
            !err2.contains("blocked by policy"),
            "白名单应放行策略层拦截: {err2}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_and_kill_process() {
        // 列出进程：至少包含自己（当前测试进程由 sh 派生，存在 sh 或本进程）。
        let procs = list_processes().unwrap();
        assert!(!procs.is_empty());
        // 禁止 kill pid 0/1
        assert!(
            kill_process(0, &[])
                .unwrap_err()
                .contains("blocked by policy")
        );
        assert!(
            kill_process(1, &[])
                .unwrap_err()
                .contains("blocked by policy")
        );
    }

    #[test]
    fn forbidden_write_paths() {
        assert!(is_forbidden_write_path("/etc/hosts"));
        assert!(is_forbidden_write_path("/usr/bin/x"));
        assert!(is_forbidden_write_path("/System/Library/x"));
        assert!(is_forbidden_write_path("/tmp/a/../b"));
        assert!(!is_forbidden_write_path("/tmp/aerodesk-test.txt"));
        assert!(!is_forbidden_write_path("relative/path.txt"));
    }
}
