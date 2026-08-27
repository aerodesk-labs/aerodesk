//! #72 文件传输 + 剪贴板（thin wrapper，状态机在 aerodesk-core）。
//!
//! CLI 进程内单例：`init` 一次性创建，事件循环里 `handle_event`/`tick` 驱动。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aerodesk_core::Endpoint;

/// 进程内文件传输状态机。
static FILE_TX: std::sync::Mutex<Option<aerodesk_core::file_transfer::FileTransfer>> =
    std::sync::Mutex::new(None);

/// #72 取消回归：启动后多少秒触发发送端取消（None = 不触发）。
static CANCEL_SEND_AFTER: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
static CANCEL_SEND_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// #122：发送是否已确认（viewer --send-file 模式发送完成）。
/// 闩锁语义（#595 审查修复）：一旦观察到确认即永久为 true——事件循环里
/// tick()（内含剪贴板轮询，可替换已确认的发送槽位）先于本判定执行，
/// 无闩锁时残留剪贴板图片会在确认与观察之间顶掉上传记录 → viewer 永不退出。
static SEND_DONE_LATCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn send_confirmed() -> bool {
    let confirmed = FILE_TX
        .lock()
        .unwrap()
        .as_ref()
        .map(|f| f.send_complete())
        .unwrap_or(false);
    if confirmed {
        SEND_DONE_LATCH.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    SEND_DONE_LATCH.load(std::sync::atomic::Ordering::SeqCst)
}

/// #122：当前接收落盘目录（viewer --request-file 模式轮询落盘）。
pub fn recv_dir() -> Option<std::path::PathBuf> {
    FILE_TX
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|f| f.recv_dir().map(|p| p.to_path_buf()))
}

/// 初始化进程内状态机（main 入口调用一次）。
/// `cancel_send_after` 为可选秒数：到达后自动取消当前发送（e2e 回归用）。
pub fn init(
    send_file: Option<PathBuf>,
    recv_dir: Option<PathBuf>,
    cancel_send_after: Option<Duration>,
    allow_request: bool,
) {
    let mut ft = aerodesk_core::file_transfer::FileTransfer::new(recv_dir);
    // 仅被控端（publisher）允许响应 FileControl::Request 提供文件；
    // viewer 默认拒绝，防房间内任意对端读取本机文件（审查 #255 Critical）。
    ft.set_allow_request(allow_request);
    if let Some(path) = send_file {
        if let Err(e) = ft.send_file(&path) {
            eprintln!("file transfer send init failed: {e}");
            std::process::exit(1);
        }
    }
    *FILE_TX.lock().unwrap() = Some(ft);
    *CANCEL_SEND_AFTER.lock().unwrap() = cancel_send_after.map(|d| Instant::now() + d);
    CANCEL_SEND_DONE.store(false, std::sync::atomic::Ordering::SeqCst);
    SEND_DONE_LATCH.store(false, std::sync::atomic::Ordering::SeqCst);
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
        // #72 取消回归：到达设定时刻自动取消当前发送（只触发一次）。
        if !CANCEL_SEND_DONE.load(std::sync::atomic::Ordering::SeqCst)
            && CANCEL_SEND_AFTER
                .lock()
                .unwrap()
                .is_some_and(|t| Instant::now() >= t)
        {
            ft.cancel_send(endpoint);
            CANCEL_SEND_DONE.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        ft.tick(endpoint);
        maybe_poll_clipboard(ft, endpoint);
        // #72/#503-2 远端剪贴板落地（publisher/viewer 共用；UI 用 core 状态机自行处理）。
        // 图片（PNG，#271 FileKind::ClipboardImage 分片通道）与文本同优先级落地。
        if let Some(text) = ft.take_incoming_clipboard() {
            crate::clipboard::set_cache(text.clone());
            crate::clipboard::write(&text);
            tracing::info!(
                "clipboard: apply {} chars from remote",
                text.chars().count()
            );
        }
        if let Some(png) = ft.take_incoming_clipboard_image() {
            // 防回声：落地内容视为已同步，轮询不再原样发回。
            *LAST_CLIP_IMG.lock().unwrap() = Some(png.clone());
            if crate::clipboard::write_image(&png) {
                tracing::info!("clipboard: apply image {} bytes from remote", png.len());
            } else {
                tracing::warn!("clipboard: apply image failed (write_image)");
            }
        }
    }
}

/// 最近一次已发送/已落地的剪贴板图片字节（防回声，与文本 cached() 同思路）。
static LAST_CLIP_IMG: std::sync::Mutex<Option<Vec<u8>>> = std::sync::Mutex::new(None);

/// 剪贴板轮询节流（1s）：本地剪贴板变化时经 file 通道同步给远端。
static LAST_CLIP_POLL: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

fn maybe_poll_clipboard(
    ft: &mut aerodesk_core::file_transfer::FileTransfer,
    endpoint: &mut Endpoint,
) {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        let mut last = LAST_CLIP_POLL.lock().unwrap();
        if last
            .map(|t| t.elapsed() < Duration::from_secs(1))
            .unwrap_or(false)
        {
            return;
        }
        *last = Some(Instant::now());
        // #503-2 图片优先（与 session viewer decide_clipboard_sync 同语义）：
        // 剪贴板有图片（PNG）时只同步图片，否则同步文本；内容未变化不发。
        if let Some(png) = crate::clipboard::read_image() {
            let cached_img = LAST_CLIP_IMG.lock().unwrap().clone();
            if Some(png.as_slice()) == cached_img.as_deref() {
                return;
            }
            if png.is_empty() {
                return;
            }
            match ft.send_clipboard_image(png.clone()) {
                Ok(()) => {
                    tracing::info!("clipboard: sent image {} bytes", png.len());
                    *LAST_CLIP_IMG.lock().unwrap() = Some(png);
                }
                // 发送槽位被文件传输占用等：下一轮重试，不置缓存。
                Err(e) => tracing::debug!("clipboard image send deferred: {e}"),
            }
            return;
        }
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
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = (ft, endpoint);
    }
}
