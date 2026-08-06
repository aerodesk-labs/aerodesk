//! #72 文件传输 + 剪贴板（thin wrapper，状态机在 aerodesk-core）。
//!
//! CLI 进程内单例：`init` 一次性创建，事件循环里 `handle_event`/`tick` 驱动。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aerodesk_core::Endpoint;

/// 进程内文件传输状态机。
static FILE_TX: std::sync::Mutex<Option<aerodesk_core::file_transfer::FileTransfer>> =
    std::sync::Mutex::new(None);

/// 初始化进程内状态机（main 入口调用一次）。
pub fn init(send_file: Option<PathBuf>, recv_dir: Option<PathBuf>) {
    let mut ft = aerodesk_core::file_transfer::FileTransfer::new(recv_dir);
    if let Some(path) = send_file {
        if let Err(e) = ft.send_file(&path) {
            eprintln!("file transfer send init failed: {e}");
            std::process::exit(1);
        }
    }
    *FILE_TX.lock().unwrap() = Some(ft);
}

/// 路由 data channel 事件到状态机（no-op 除非 label == "file"）。
pub fn handle_event(ev: &aerodesk_core::ClientEvent, endpoint: &mut Endpoint) {
    if let Ok(mut ft) = FILE_TX.lock()
        && let Some(ft) = ft.as_mut()
    {
        ft.handle_event(ev, endpoint);
    }
}

/// 每轮事件循环推进一次文件发送 + 剪贴板轮询。
pub fn tick(endpoint: &mut Endpoint) {
    if let Ok(mut ft) = FILE_TX.lock()
        && let Some(ft) = ft.as_mut()
    {
        ft.tick(endpoint);
        maybe_poll_clipboard(ft, endpoint);
        // #72 远端剪贴板落地（publisher/viewer 共用；UI 用 core 状态机自行处理）。
        if let Some(text) = ft.take_incoming_clipboard() {
            crate::clipboard::set_cache(text.clone());
            crate::clipboard::write(&text);
            tracing::info!(
                "clipboard: apply {} chars from remote",
                text.chars().count()
            );
        }
    }
}

/// 剪贴板轮询节流（1s）：本地剪贴板变化时经 file 通道同步给远端。
static LAST_CLIP_POLL: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

fn maybe_poll_clipboard(
    ft: &mut aerodesk_core::file_transfer::FileTransfer,
    endpoint: &mut Endpoint,
) {
    #[cfg(target_os = "macos")]
    {
        let mut last = LAST_CLIP_POLL.lock().unwrap();
        if last
            .map(|t| t.elapsed() < Duration::from_secs(1))
            .unwrap_or(false)
        {
            return;
        }
        *last = Some(Instant::now());
        let Some(text) = crate::clipboard::read() else {
            return;
        };
        if crate::clipboard::cached().as_deref() == Some(text.as_str()) {
            return;
        }
        // 发送成功才更新缓存：通道未就绪时下一轮重试（避免首包丢失后静默放弃）。
        if ft.send_clipboard(&text, endpoint) {
            crate::clipboard::set_cache(text.clone());
            tracing::info!("clipboard: sent {} chars", text.chars().count());
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (ft, endpoint);
    }
}

/// 取走收到的远端剪贴板文本（调用方写入系统剪贴板）。
pub fn take_incoming_clipboard() -> Option<String> {
    FILE_TX
        .lock()
        .ok()
        .and_then(|mut ft| ft.as_mut().and_then(|ft| ft.take_incoming_clipboard()))
}
