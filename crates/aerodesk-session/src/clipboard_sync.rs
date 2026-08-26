//! #503-2 被控端（publisher）剪贴板双向接线：file 通道接收 + 本地轮询回传。
//!
//! 观看端（viewer）的轮询与落地在 generic_viewer 内（#271 decide_clipboard_sync）；
//! 此处统一被控端两侧实现（generic_publisher / macos_publisher 共用），与 CLI
//! 被控端（aerodesk-agent::file_transfer::maybe_poll_clipboard）同语义：
//! - 图片优先：剪贴板有图片（PNG）时只同步图片，否则同步文本；
//! - 内容未变化不发（文本经 core CLIP_CACHE，图片经本地 last_img），防回声环。

use std::time::{Duration, Instant};

use aerodesk_core::Endpoint;

/// 被控端剪贴板轮询器（1s 节流；图片优先；未变化不发）。
#[derive(Default)]
pub struct ClipboardPoller {
    last_poll: Option<Instant>,
    last_img: Option<Vec<u8>>,
}

impl ClipboardPoller {
    pub fn new() -> Self {
        Self::default()
    }

    /// 轮询本地剪贴板并经 file 通道回传给观看端（被控→主控方向）。
    pub fn poll_and_send(
        &mut self,
        ft: &mut aerodesk_core::file_transfer::FileTransfer,
        endpoint: &mut Endpoint,
    ) {
        if self
            .last_poll
            .map(|t| t.elapsed() < Duration::from_secs(1))
            .unwrap_or(false)
        {
            return;
        }
        self.last_poll = Some(Instant::now());
        if let Some(png) = aerodesk_core::clipboard::read_image() {
            if !png.is_empty() && self.last_img.as_deref() != Some(png.as_slice()) {
                match ft.send_clipboard_image(png.clone()) {
                    Ok(()) => {
                        self.last_img = Some(png);
                        tracing::info!("clipboard auto-sync: image sent");
                    }
                    // 发送槽位被文件传输占用等：下一轮重试，不置缓存。
                    Err(e) => tracing::debug!("clipboard image send deferred: {e}"),
                }
            }
            return;
        }
        if let Some(text) = aerodesk_core::clipboard::read()
            && !text.is_empty()
            && aerodesk_core::clipboard::cached().as_deref() != Some(text.as_str())
        {
            if ft.send_clipboard(&text, endpoint) {
                aerodesk_core::clipboard::set_cache(text.clone());
                tracing::info!("clipboard auto-sync: text sent");
            }
        }
    }

    /// 登记已落地的远端图片字节（防回声：轮询不再原样发回）。
    pub fn mark_applied(&mut self, png: Vec<u8>) {
        self.last_img = Some(png);
    }
}

/// 应用 file 通道收到的剪贴板文本（写入系统剪贴板 + 防回声缓存）；返回状态文案。
pub fn apply_incoming_text(text: String) -> String {
    aerodesk_core::clipboard::set_cache(text.clone());
    if !aerodesk_core::clipboard::write(&text) {
        tracing::warn!("写入远端剪贴板文本失败");
        return "远端剪贴板文本写入失败".to_string();
    }
    format!("已应用远端剪贴板：{} 字", text.chars().count())
}

/// 应用 file 通道收到的剪贴板图片（PNG；写入系统剪贴板）；返回状态文案。
pub fn apply_incoming_image(png: Vec<u8>) -> String {
    if aerodesk_core::clipboard::write_image(&png) {
        format!("已应用远端剪贴板图片（{} 字节）", png.len())
    } else {
        "远端剪贴板图片写入失败".to_string()
    }
}

/// 推进被控端 file 通道：应用远端剪贴板文本/图片 + 轮询回传。
/// 返回要展示的状态文案（无事件时为空）。
pub fn tick_publisher_clipboard(
    ft: &mut aerodesk_core::file_transfer::FileTransfer,
    endpoint: &mut Endpoint,
    poller: &mut ClipboardPoller,
) -> Option<String> {
    ft.tick(endpoint);
    let mut msg = None;
    if let Some(text) = ft.take_incoming_clipboard() {
        msg = Some(apply_incoming_text(text));
    }
    if let Some(png) = ft.take_incoming_clipboard_image() {
        poller.mark_applied(png.clone());
        msg = Some(apply_incoming_image(png));
    }
    poller.poll_and_send(ft, endpoint);
    msg
}
