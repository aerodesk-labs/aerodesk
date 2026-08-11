//! 系统剪贴板文本读写（#72 剪贴板双向同步，#271 跨平台增强）。
//!
//! 平台实现（保持零额外依赖，与 #72 的 macOS 命令方案一致）：
//! - macOS：`pbpaste` / `pbcopy`
//! - Windows：PowerShell `Get-Clipboard` / `Set-Clipboard`（Win10+ 内置；
//!   经 UTF-16LE Base64 `-EncodedCommand` 传入，避免转义与编码坑）
//! - 其他平台 no-op（Linux 批次：#271）
//!
//! 进程内缓存最近一次已知内容，避免「写入远端内容又被自己轮询发回」的回声环。

use std::sync::Mutex;

static CLIP_CACHE: Mutex<Option<String>> = Mutex::new(None);

/// 读取当前剪贴板文本（macOS/Windows；其他平台返回 None）。
pub fn read() -> Option<String> {
    #[cfg(target_os = "macos")]
    let result: Option<String> = {
        let out = std::process::Command::new("pbpaste").output().ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            None
        }
    };
    #[cfg(target_os = "windows")]
    let result: Option<String> = windows_read();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let result: Option<String> = None;
    result
}

/// 写入剪贴板文本（macOS/Windows；其他平台返回 false）。
/// 写入剪贴板文本（macOS/Windows；其他平台返回 false）。
pub fn write(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    let result: bool = {
        use std::io::Write;
        let Some(mut child) = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .ok()
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
        child.wait().map(|st| st.success()).unwrap_or(false)
    };
    #[cfg(target_os = "windows")]
    let result: bool = windows_write(text);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let result: bool = {
        let _ = text;
        false
    };
    result
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

#[cfg(target_os = "windows")]
use base64ct::{Base64, Encoding};

/// Windows：PowerShell 命令经 UTF-16LE + Base64 的 `-EncodedCommand` 执行，
/// 避免命令行转义（任意文本）与控制台代码页编码问题。
#[cfg(target_os = "windows")]
fn powershell_encoded(script: &str) -> Option<std::process::Output> {
    let utf16: Vec<u16> = script.encode_utf16().collect();
    let mut bytes = Vec::with_capacity(utf16.len() * 2);
    for u in utf16 {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    let encoded = Base64::encode_string(&bytes);
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded])
        .output()
        .ok()
}

/// Windows 读剪贴板：`Get-Clipboard -Raw` 写 UTF-8 临时文件后读回，
/// 避免 PowerShell 管道输出编码（控制台代码页 / UTF-16）不确定。
#[cfg(target_os = "windows")]
fn windows_read() -> Option<String> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("aerodesk-clip-{}.txt", std::process::id()));
    let script = format!(
        "Get-Clipboard -Raw | Set-Content -LiteralPath '{}' -Encoding UTF8",
        path.to_string_lossy().replace('\'', "''")
    );
    let out = powershell_encoded(&script)?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let text = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    // Set-Content -Encoding UTF8 输出带 BOM（U+FEFF），以及 PowerShell 追加的 CRLF。
    let text = text.as_deref().unwrap_or("");
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let text = text.trim_end_matches(['\r', '\n']);
    Some(text.to_string())
}

/// Windows 写剪贴板：`Set-Clipboard -Value <base64 解码出的文本>`。
#[cfg(target_os = "windows")]
fn windows_write(text: &str) -> bool {
    let b64 = Base64::encode_string(text.as_bytes());
    // 文本本身经 Base64 进脚本，脚本再经 UTF-16LE Base64 进命令行，全程无转义面。
    let script = format!(
        "Set-Clipboard -Value ([System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String(\"{b64}\")))"
    );
    powershell_encoded(&script)
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_roundtrip() {
        set_cache("hello 你好".to_string());
        assert_eq!(cached().as_deref(), Some("hello 你好"));
    }

    /// Windows 真机剪贴板往返（CI windows runner 交互会话）。
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_clipboard_unicode_roundtrip() {
        let text = "AeroDesk 剪贴板 🚀";
        assert!(write(text), "Set-Clipboard 应成功");
        match read() {
            Some(got) => assert_eq!(got, text, "读回内容应与写入一致"),
            None => eprintln!("SKIP: 剪贴板读回失败（无交互会话/受限环境）"),
        }
    }
}
