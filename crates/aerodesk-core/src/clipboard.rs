//! 系统剪贴板文本读写（#72 剪贴板双向同步，#271 跨平台增强）。
//!
//! 平台实现（#72 起保持零额外依赖的命令方案；Linux 用 arboard 纯 Rust 无外部命令）：
//! - macOS：`pbpaste` / `pbcopy`
//! - Windows：PowerShell `Get-Clipboard` / `Set-Clipboard`（Win10+ 内置；
//!   经 UTF-16LE Base64 `-EncodedCommand` 传入，避免转义与编码坑）
//! - Linux：arboard（X11 x11rb / Wayland data-control）
//! - 其他平台 no-op
//!
//! 进程内缓存最近一次已知内容，避免「写入远端内容又被自己轮询发回」的回声环。

use std::sync::Mutex;

static CLIP_CACHE: Mutex<Option<String>> = Mutex::new(None);

/// 读取当前剪贴板文本（macOS/Windows/Linux；其他平台返回 None）。
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
    #[cfg(target_os = "linux")]
    let result: Option<String> = linux_read();
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let result: Option<String> = None;
    result
}

/// 写入剪贴板文本（macOS/Windows/Linux；其他平台返回 false）。
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
    #[cfg(target_os = "linux")]
    let result: bool = linux_write(text);
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let result: bool = {
        let _ = text;
        false
    };
    result
}

/// 读取剪贴板图片（PNG 编码；#271）。Windows 经 PowerShell
/// System.Drawing/Windows.Forms；其他平台暂返回 None（后续批次）。
pub fn read_image() -> Option<Vec<u8>> {
    #[cfg(target_os = "windows")]
    {
        return windows_read_image();
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// 写入剪贴板图片（PNG，#271）。Windows 经 PowerShell；其他平台返回 false。
pub fn write_image(png: &[u8]) -> bool {
    #[cfg(target_os = "windows")]
    {
        return windows_write_image(png);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = png;
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

/// Windows 读剪贴板图片：`Clipboard.GetImage()` → PNG → base64 落临时文件读回。
#[cfg(target_os = "windows")]
fn windows_read_image() -> Option<Vec<u8>> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("aerodesk-clip-img-{}.txt", std::process::id()));
    let script = format!(
        "Add-Type -AssemblyName System.Drawing\nAdd-Type -AssemblyName System.Windows.Forms\n$img = [System.Windows.Forms.Clipboard]::GetImage()\nif ($null -eq $img) {{ Set-Content -LiteralPath '{}' -Value '' -Encoding Ascii; exit 0 }}\n$ms = New-Object System.IO.MemoryStream\n$img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)\n$b64 = [Convert]::ToBase64String($ms.ToArray())\n$ms.Dispose()\n$img.Dispose()\nSet-Content -LiteralPath '{}' -Value $b64 -Encoding Ascii",
        path.to_string_lossy().replace('\'', "''"),
        path.to_string_lossy().replace('\'', "''"),
    );
    let out = powershell_encoded(&script)?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let b64 = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    let b64 = b64?;
    let b64 = b64.trim();
    if b64.is_empty() {
        return None;
    }
    use base64ct::{Base64, Encoding};
    let bytes = Base64::decode_vec(b64).ok()?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    Some(bytes)
}

/// Windows 写剪贴板图片：PNG 字节经 base64 内嵌脚本，`SetImage` 写入系统剪贴板。
#[cfg(target_os = "windows")]
fn windows_write_image(png: &[u8]) -> bool {
    use base64ct::{Base64, Encoding};
    let b64 = Base64::encode_string(png);
    let script = format!(
        "Add-Type -AssemblyName System.Drawing\nAdd-Type -AssemblyName System.Windows.Forms\n$b64 = '{}'\n$bytes = [Convert]::FromBase64String($b64)\n$ms = New-Object System.IO.MemoryStream(,$bytes)\n$img = [System.Drawing.Image]::FromStream($ms)\n[System.Windows.Forms.Clipboard]::SetImage($img)\n$ms.Dispose()\n$img.Dispose()",
        b64,
    );
    powershell_encoded(&script)
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Linux：arboard（X11 x11rb / Wayland data-control）。
/// 单进程内串行访问（arboard Clipboard 非 Sync；且 X11/Wayland 连接每操作重建成本高）。
#[cfg(target_os = "linux")]
static LINUX_CLIP: std::sync::Mutex<Option<arboard::Clipboard>> = std::sync::Mutex::new(None);

#[cfg(target_os = "linux")]
fn linux_read() -> Option<String> {
    let mut guard = LINUX_CLIP.lock().ok()?;
    let cb = match guard.as_mut() {
        Some(cb) => cb,
        None => {
            let cb = arboard::Clipboard::new().ok()?;
            guard.insert(cb)
        }
    };
    cb.get_text().ok()
}

#[cfg(target_os = "linux")]
fn linux_write(text: &str) -> bool {
    let Ok(mut guard) = LINUX_CLIP.lock() else {
        return false;
    };
    let cb = match guard.as_mut() {
        Some(cb) => cb,
        None => match arboard::Clipboard::new() {
            Ok(cb) => guard.insert(cb),
            Err(_) => return false,
        },
    };
    cb.set_text(text).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 系统剪贴板是全局共享资源：测试并行时互斥，避免相互覆盖。
    static CLIP_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn cache_roundtrip() {
        set_cache("hello 你好".to_string());
        assert_eq!(cached().as_deref(), Some("hello 你好"));
    }

    /// Linux 真机剪贴板往返（CI ubuntu runner 交互会话；无显示环境时 SKIP）。
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_clipboard_unicode_roundtrip() {
        let has_display =
            std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
        if !has_display {
            eprintln!("SKIP: 无 DISPLAY/WAYLAND_DISPLAY（headless）");
            return;
        }
        let text = "AeroDesk 剪贴板 🚀";
        assert!(write(text), "arboard set_text 应成功");
        match read() {
            Some(got) => assert_eq!(got, text, "读回内容应与写入一致"),
            None => eprintln!("SKIP: 剪贴板读回失败（无剪贴板管理器/受限环境）"),
        }
    }

    /// Windows 真机图片剪贴板往返（#271）：写入 PNG → 读回 → 幂等稳定。
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_clipboard_image_roundtrip() {
        let _guard = CLIP_TEST_LOCK.lock().unwrap();
        // 1x1 红色 PNG（标准字节序列，System.Drawing 可解码）。
        let png: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        assert!(write_image(&png), "SetImage 应成功");
        let got = read_image().expect("读回剪贴板图片");
        assert!(got.starts_with(b"\x89PNG\r\n\x1a\n"), "读回应为合法 PNG");
        // 幂等：System.Drawing 重编码字节稳定（写回再读应一致）。
        assert!(write_image(&got));
        let got2 = read_image().expect("再次读回");
        assert_eq!(got, got2, "重编码 PNG 应幂等");
    }

    /// Windows 真机剪贴板往返（CI windows runner 交互会话）。
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_clipboard_unicode_roundtrip() {
        let _guard = CLIP_TEST_LOCK.lock().unwrap();
        let text = "AeroDesk 剪贴板 🚀";
        assert!(write(text), "Set-Clipboard 应成功");
        match read() {
            Some(got) => assert_eq!(got, text, "读回内容应与写入一致"),
            None => eprintln!("SKIP: 剪贴板读回失败（无交互会话/受限环境）"),
        }
    }
}
