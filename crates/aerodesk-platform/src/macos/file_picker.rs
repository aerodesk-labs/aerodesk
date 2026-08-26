//! 文件选择器（观看端「发送文件」）。
//!
//! macOS 用 osascript choose file（无额外依赖）；Windows/Linux 由各自批次实现
//!（Windows 可接 IFileOpenDialog，Linux 走 GTK/Qt 或 zenity）。

use std::process::Command;

/// 核心 `FilePicker` trait 实现（macOS）。
pub struct MacFilePicker;

impl aerodesk_core::platform::FilePicker for MacFilePicker {
    type Error = String;

    fn pick_file(&self) -> Result<Option<String>, Self::Error> {
        let out = Command::new("osascript")
            .args(["-e", "POSIX path of (choose file)"])
            .output()
            .map_err(|e| format!("osascript: {e}"))?;
        if !out.status.success() {
            return Ok(None); // 用户取消 / 无选择
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(if path.is_empty() { None } else { Some(path) })
    }

    /// #503 多选：`choose file with multiple selections allowed` 返回别名列表，
    /// 逐项转 POSIX 路径（每行一个）。
    fn pick_files(&self) -> Result<Option<Vec<String>>, Self::Error> {
        let script = "set fs to choose file with multiple selections allowed\n\
                      set out to \"\"\n\
                      repeat with f in fs\n\
                          set out to out & (POSIX path of f) & linefeed\n\
                      end repeat\n\
                      return out";
        let out = Command::new("osascript")
            .args(["-e", script])
            .output()
            .map_err(|e| format!("osascript: {e}"))?;
        if !out.status.success() {
            return Ok(None); // 用户取消 / 无选择
        }
        let paths: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Ok(if paths.is_empty() { None } else { Some(paths) })
    }
}
