//! 文件选择器（观看端「发送文件」；#277 `FilePicker` trait 的 Linux 实现）。
//!
//! 依次尝试 `zenity --file-selection`（GNOME）与 `kdialog --getopenfilename`（KDE）
//! ——两者都是各桌面环境预装的 portal 前端，零额外依赖（与 core 剪贴板「命令方案」一致）。
//! 无桌面/未安装时返回 Err，UI 显示错误而不是静默失败。

use std::process::{Command, Output};

use aerodesk_core::platform::FilePicker;

/// 把命令输出映射为选择结果：成功→路径；取消（退出码 1/空输出）→ None。
fn parse_picked(out: &Output) -> Option<Result<Option<String>, String>> {
    if out.status.success() {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return Some(Ok(if path.is_empty() { None } else { Some(path) }));
    }
    // 常见取消语义：zenity/kdialog 用户取消都返回退出码 1 且无输出。
    if out.status.code() == Some(1) {
        return Some(Ok(None));
    }
    None
}

/// Linux 文件选择器（zenity → kdialog）。
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxFilePicker;

impl FilePicker for LinuxFilePicker {
    type Error = String;

    fn pick_file(&self) -> Result<Option<String>, String> {
        let zenity_args: &[&str] = &["--file-selection", "--title=AeroDesk 发送文件"];
        let kdialog_args: &[&str] = &["--getopenfilename", "."];
        let candidates: [(&str, &[&str]); 2] = [("zenity", zenity_args), ("kdialog", kdialog_args)];
        for (cmd, args) in candidates {
            match Command::new(cmd).args(args).output() {
                Ok(out) => {
                    if let Some(r) = parse_picked(&out) {
                        return r;
                    }
                    // 命令存在但异常退出：尝试下一个前端。
                }
                Err(_) => continue, // 未安装 → 尝试下一个
            }
        }
        Err("没有可用的文件选择器（请安装 zenity 或 kdialog）".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::ExitStatus;

    #[cfg(unix)]
    fn status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        // from_raw 接收原始 wait status：退出码在 8..15 位（低 7 位是信号）。
        ExitStatus::from_raw((code & 0xff) << 8)
    }

    #[test]
    fn success_with_path() {
        let out = Output {
            status: status(0),
            stdout: b"/home/u/Documents/a.txt
"
            .to_vec(),
            stderr: vec![],
        };
        assert_eq!(
            parse_picked(&out),
            Some(Ok(Some("/home/u/Documents/a.txt".to_string())))
        );
    }

    #[test]
    fn cancel_maps_to_none() {
        let out = Output {
            status: status(1),
            stdout: vec![],
            stderr: vec![],
        };
        assert_eq!(parse_picked(&out), Some(Ok(None)));
    }

    #[test]
    fn weird_exit_is_none_parse() {
        let out = Output {
            status: status(2),
            stdout: vec![],
            stderr: vec![],
        };
        assert_eq!(parse_picked(&out), None);
    }
}
