//! 系统剪贴板文本读写（#72 剪贴板双向同步）。
//!
//! macOS 用系统自带 `pbpaste`/`pbcopy`（无额外依赖）；其他平台 no-op。
//! 进程内缓存最近一次已知内容，避免「写入远端内容又被自己轮询发回」的回声环。

use std::io::Write;
use std::sync::Mutex;

static CLIP_CACHE: Mutex<Option<String>> = Mutex::new(None);

/// 读取当前剪贴板文本（macOS；其他平台返回 None）。
pub fn read() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("pbpaste").output().ok()?;
        if out.status.success() {
            return Some(String::from_utf8_lossy(&out.stdout).into_owned());
        }
    }
    None
}

/// 写入剪贴板文本（macOS；其他平台返回 false）。
pub fn write(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        let Ok(mut child) = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
        else {
            return false;
        };
        let written = child
            .stdin
            .as_mut()
            .map(|s| s.write_all(text.as_bytes()).is_ok())
            .unwrap_or(false);
        if !written {
            return false;
        }
        return child.wait().map(|st| st.success()).unwrap_or(false);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
        false
    }
}

/// 记录最近一次已知剪贴板内容（远端写入后更新，防止回声）。
pub fn set_cache(text: String) {
    if let Ok(mut c) = CLIP_CACHE.lock() {
        *c = Some(text);
    }
}

/// 最近一次已知剪贴板内容。
pub fn cached() -> Option<String> {
    CLIP_CACHE.lock().ok().and_then(|c| c.clone())
}
