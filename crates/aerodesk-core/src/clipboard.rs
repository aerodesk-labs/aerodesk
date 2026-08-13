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
/// System.Drawing/Windows.Forms；macOS 经 osascript NSPasteboard `«class PNGf»`
/// （与 pbpaste/pbcopy 同思路，无额外依赖）；其他平台暂返回 None。
pub fn read_image() -> Option<Vec<u8>> {
    #[cfg(target_os = "windows")]
    let result: Option<Vec<u8>> = windows_read_image();
    #[cfg(target_os = "macos")]
    let result: Option<Vec<u8>> = macos_read_image();
    #[cfg(target_os = "linux")]
    let result: Option<Vec<u8>> = linux_read_image();
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    let result: Option<Vec<u8>> = None;
    result
}

/// 写入剪贴板图片（PNG，#271）。Windows 经 PowerShell；macOS 经 osascript
/// NSPasteboard PNGf；其他平台返回 false。
pub fn write_image(png: &[u8]) -> bool {
    #[cfg(target_os = "windows")]
    let result: bool = windows_write_image(png);
    #[cfg(target_os = "macos")]
    let result: bool = macos_write_image(png);
    #[cfg(target_os = "linux")]
    let result: bool = linux_write_image(png);
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    let result: bool = {
        let _ = png;
        false
    };
    result
}

/// macOS：读剪贴板 PNG（osascript 把 NSPasteboard PNGf 写入临时文件再读取）。
/// 剪贴板无图片时 osascript 报错 → 返回 None。
#[cfg(target_os = "macos")]
fn macos_read_image() -> Option<Vec<u8>> {
    let tmp = std::env::temp_dir().join(format!("aerodesk-clip-read-{}.png", std::process::id()));
    let script = format!(
        "set f to open for access POSIX file \"{}\" with write permission\n\
         write (the clipboard as «class PNGf») to f\n\
         close access f",
        tmp.display()
    );
    let out = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // 剪贴板无 PNG 或非交互会话
    }
    let data = std::fs::read(&tmp).ok()?;
    let _ = std::fs::remove_file(&tmp);
    if data.is_empty() { None } else { Some(data) }
}

/// macOS：写 PNG 到剪贴板（osascript 读临时 PNG 文件 → NSPasteboard PNGf）。
#[cfg(target_os = "macos")]
fn macos_write_image(png: &[u8]) -> bool {
    let tmp = std::env::temp_dir().join(format!("aerodesk-clip-write-{}.png", std::process::id()));
    if std::fs::write(&tmp, png).is_err() {
        return false;
    }
    let script = format!(
        "set the clipboard to (read (POSIX file \"{}\") as «class PNGf»)",
        tmp.display()
    );
    let ok = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&tmp);
    ok
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

/// Windows 读剪贴板（原生 Win32 CF_UNICODETEXT，#271 修复）。
///
/// 原 PowerShell `Get-Clipboard` 子进程单次调用实测阻塞 ~1.3s（本机），而
/// `maybe_poll_clipboard` 每轮轮询一次，把 publisher/viewer 主循环拖到 <1fps。
/// 改 Win32 OpenClipboard/GetClipboardData：微秒级、无子进程。
#[cfg(target_os = "windows")]
fn windows_read() -> Option<String> {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(hwnd: *mut std::ffi::c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn GetClipboardData(fmt: u32) -> *mut std::ffi::c_void;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalLock(h: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        fn GlobalUnlock(h: *mut std::ffi::c_void) -> i32;
        fn GlobalSize(h: *mut std::ffi::c_void) -> usize;
    }
    const CF_UNICODETEXT: u32 = 13;
    unsafe {
        // SAFETY: OpenClipboard/CloseClipboard 成对调用；GetClipboardData 返回
        // 的句柄仅在 Open..Close 窗口内有效，GlobalLock/Unlock 成对，且剪贴板
        // CF_UNICODETEXT 内容为 NUL 结尾 UTF-16LE，GlobalSize 兜底越界。
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let h = GetClipboardData(CF_UNICODETEXT);
        if h.is_null() {
            CloseClipboard();
            return None;
        }
        let ptr = GlobalLock(h);
        if ptr.is_null() {
            CloseClipboard();
            return None;
        }
        let size = GlobalSize(h);
        let mut len = 0usize;
        while len + 1 < size && *((ptr as *const u16).add(len)) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr as *const u16, len));
        GlobalUnlock(h);
        CloseClipboard();
        Some(text)
    }
}

/// Windows 写剪贴板（原生 Win32 CF_UNICODETEXT，#271 修复）。
#[cfg(target_os = "windows")]
fn windows_write(text: &str) -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(hwnd: *mut std::ffi::c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(fmt: u32, h: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut std::ffi::c_void;
        fn GlobalLock(h: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        fn GlobalUnlock(h: *mut std::ffi::c_void) -> i32;
        fn GlobalFree(h: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }
    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    unsafe {
        // SAFETY: OpenClipboard 成功后 EmptyClipboard 清空，GlobalAlloc/MOVEABLE
        // 分配后 GlobalLock 写入 UTF-16LE（含 NUL）；SetClipboardData 成功时接管
        // h 所有权，失败时 GlobalFree 释放，CloseClipboard 收尾。
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        if EmptyClipboard() == 0 {
            CloseClipboard();
            return false;
        }
        let h = GlobalAlloc(GMEM_MOVEABLE, utf16.len() * 2);
        if h.is_null() {
            CloseClipboard();
            return false;
        }
        let dst = GlobalLock(h);
        if dst.is_null() {
            GlobalFree(h);
            CloseClipboard();
            return false;
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst as *mut u16, utf16.len());
        GlobalUnlock(h);
        let set = SetClipboardData(CF_UNICODETEXT, h);
        if set.is_null() {
            // 失败时剪贴板未接管句柄，需自行释放。
            GlobalFree(h);
        }
        CloseClipboard();
        !set.is_null()
    }
}

#[cfg(target_os = "windows")]
fn windows_read_image() -> Option<Vec<u8>> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("aerodesk-clip-img-{}.txt", std::process::id()));
    let script = format!(
        "Add-Type -AssemblyName System.Drawing\nAdd-Type -AssemblyName System.Windows.Forms\n$ErrorActionPreference = 'Stop'\n$img = [System.Windows.Forms.Clipboard]::GetImage()\nif ($null -eq $img) {{ Set-Content -LiteralPath '{}' -Value '' -Encoding Ascii; exit 0 }}\n$ms = New-Object System.IO.MemoryStream\n$img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)\n$b64 = [Convert]::ToBase64String($ms.ToArray())\n$ms.Dispose()\n$img.Dispose()\nSet-Content -LiteralPath '{}' -Value $b64 -Encoding Ascii",
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
        "Add-Type -AssemblyName System.Drawing\nAdd-Type -AssemblyName System.Windows.Forms\n$ErrorActionPreference = 'Stop'\n$b64 = '{}'\n$bytes = [Convert]::FromBase64String($b64)\n$ms = New-Object System.IO.MemoryStream(,$bytes)\n$img = [System.Drawing.Image]::FromStream($ms)\n[System.Windows.Forms.Clipboard]::SetImage($img)\n$ms.Dispose()\n$img.Dispose()",
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

/// RGBA8 → PNG 字节（真机往返测试用；运行路径直接用 xclip/wl-copy 的 PNG MIME）。
#[cfg(target_os = "linux")]
#[allow(dead_code)]
fn rgba_to_png(rgba: &[u8], width: usize, height: usize) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width as u32, height as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(buf)
}

/// PNG 字节 → RGBA8（支持 RGB/RGBA 8bit；其余格式返回 None）。
#[cfg(target_os = "linux")]
#[allow(dead_code)]
fn png_to_rgba(png: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    let mut dec = png::Decoder::new(png);
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let bytes = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => bytes.to_vec(),
        png::ColorType::Rgb => bytes
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        _ => return None,
    };
    Some((rgba, info.width as usize, info.height as usize))
}

/// Linux 读剪贴板图片（PNG MIME）：
/// Wayland → `wl-paste --type image/png`；X11 → `xclip -t image/png -o`。
/// 与 macOS pbpaste/pbcopy 同思路的命令方案（#271）。
#[cfg(target_os = "linux")]
fn linux_read_image() -> Option<Vec<u8>> {
    let out = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        std::process::Command::new("wl-paste")
            .args(["--type", "image/png"])
            .output()
            .ok()?
    } else {
        std::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "image/png", "-o"])
            .output()
            .ok()?
    };
    if !out.status.success() {
        return None; // 剪贴板无图片或工具缺失
    }
    if out.stdout.is_empty() {
        None
    } else {
        Some(out.stdout)
    }
}

/// Linux 写剪贴板图片（PNG MIME）：
/// Wayland → `wl-copy --type image/png`；X11 → `xclip -t image/png -i`。
#[cfg(target_os = "linux")]
fn linux_write_image(png: &[u8]) -> bool {
    use std::io::Write;
    let (cmd, args) = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        ("wl-copy", vec!["--type", "image/png"])
    } else {
        (
            "xclip",
            vec!["-selection", "clipboard", "-t", "image/png", "-i"],
        )
    };
    let Some(mut child) = std::process::Command::new(cmd)
        .args(&args)
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
        .map(|s| s.write_all(png).is_ok())
        .unwrap_or(false);
    if !written {
        return false;
    }
    child.wait().map(|st| st.success()).unwrap_or(false)
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

    /// Linux PNG 编解码往返（无剪贴板也运行，纯函数）。
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_png_codec_roundtrip() {
        let (w, h) = (4usize, 3usize);
        let mut rgba = Vec::with_capacity(w * h * 4);
        for i in 0..(w * h) {
            rgba.extend_from_slice(&[(i * 37 % 256) as u8, 128, 64, 255]);
        }
        let png = rgba_to_png(&rgba, w, h).expect("encode");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "PNG 魔数");
        let (out, dw, dh) = png_to_rgba(&png).expect("decode");
        assert_eq!((dw, dh), (w, h));
        assert_eq!(out, rgba, "RGBA 往返一致");
    }

    /// Linux 真机图片剪贴板往返（#271）：写 PNG → 读回。
    /// 显式 opt-in（`AERODESK_TEST_CLIPBOARD_IMAGE=1`）：CI runner 可能设了
    /// WAYLAND_DISPLAY 但无 compositor，`wl-paste`/`xclip` 会阻塞挂死，
    /// 故默认 SKIP；真机/有剪贴板管理器环境显式启用。
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_clipboard_image_roundtrip() {
        if std::env::var("AERODESK_TEST_CLIPBOARD_IMAGE").is_err() {
            eprintln!(
                "SKIP: 未设置 AERODESK_TEST_CLIPBOARD_IMAGE=1（需 X11/Wayland + xclip/wl-copy）"
            );
            return;
        }
        let has_display =
            std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
        if !has_display {
            eprintln!("SKIP: 无 DISPLAY/WAYLAND_DISPLAY（headless）");
            return;
        }
        let (w, h) = (8usize, 6usize);
        let mut rgba = Vec::with_capacity(w * h * 4);
        for i in 0..(w * h) {
            rgba.extend_from_slice(&[(i * 13 % 256) as u8, 200, 100, 255]);
        }
        let png = rgba_to_png(&rgba, w, h).expect("encode");
        assert!(write_image(&png), "xclip/wl-copy 写入应成功");
        match read_image() {
            Some(got) => {
                assert!(got.starts_with(b"\x89PNG\r\n\x1a\n"), "读回应为 PNG");
                let (_out, dw, dh) = png_to_rgba(&got).expect("decode");
                assert_eq!((dw, dh), (w, h), "尺寸一致");
            }
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
        if !write_image(&png) {
            // 剪贴板被占用/无交互会话时跳过（CI 交互会话会完整验证）。
            eprintln!("SKIP: 剪贴板图片写入失败（被占用/无交互会话）");
            return;
        }
        let got = read_image().expect("读回剪贴板图片");
        assert!(got.starts_with(b"\x89PNG\r\n\x1a\n"), "读回应为合法 PNG");
        // 幂等：System.Drawing 重编码字节稳定（写回再读应一致）。
        assert!(write_image(&got));
        let got2 = read_image().expect("再次读回");
        assert_eq!(got, got2, "重编码 PNG 应幂等");
    }

    /// macOS 真机图片剪贴板往返（#271）：写入 PNG → 读回 → 幂等稳定。
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_clipboard_image_roundtrip() {
        let _guard = CLIP_TEST_LOCK.lock().unwrap();
        // 1x1 红色 PNG（与 Windows 同款标准字节序列）。
        let png: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        if !write_image(&png) {
            eprintln!("SKIP: 剪贴板图片写入失败（无交互会话/被占用）");
            return;
        }
        let got = read_image().expect("读回剪贴板图片");
        assert!(got.starts_with(b"\x89PNG\r\n\x1a\n"), "读回应为合法 PNG");
        // 幂等：写回再读应一致（NSPasteboard PNGf 不重编码）。
        assert!(write_image(&got));
        let got2 = read_image().expect("再次读回");
        assert_eq!(got, got2, "PNG 往返应幂等");
    }

    /// Windows 真机剪贴板往返（CI windows runner 交互会话）。
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_clipboard_unicode_roundtrip() {
        let _guard = CLIP_TEST_LOCK.lock().unwrap();
        let text = "AeroDesk 剪贴板 🚀";
        if !write(text) {
            // 剪贴板被占用/无交互会话时跳过（CI 交互会话会完整验证）。
            eprintln!("SKIP: 剪贴板写入失败（被占用/无交互会话）");
            return;
        }
        match read() {
            Some(got) => assert_eq!(got, text, "读回内容应与写入一致"),
            None => eprintln!("SKIP: 剪贴板读回失败（无交互会话/受限环境）"),
        }
    }
}
