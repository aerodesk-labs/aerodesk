//! iOS 观看会话：连接 + 媒体收流循环 + H.264 硬解 → 最新帧槽。
//!
//! 由 `ffi.rs` 暴露给 Swift：`ad_viewer_create` 启动后台收流线程，
//! Swift 轮询 `ad_viewer_take_frame` 取最新 CVPixelBuffer 渲染。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use aerodesk_core::connect::LiveSession;
use aerodesk_core::endpoint::ClientEvent;
use apple_cf::cv::CVPixelBuffer;
use str0m::net::Protocol;

use crate::decode::H264Decoder;

unsafe extern "C" {
    fn CFRetain(cf: *const std::ffi::c_void) -> *const std::ffi::c_void;
}

/// 观看会话（FFI 句柄 = *mut ViewerSession）。
/// 连接与媒体组件归后台 pump 线程所有；结构体只保留控制面。
pub struct ViewerSession {
    /// 最新解码帧（+1 retained，调用方 take 后转移所有权）。
    latest: Arc<Mutex<Option<CVPixelBuffer>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ViewerSession {
    /// 连接信令并启动收流解码线程。
    pub fn connect(server: &str, room: &str) -> Result<ViewerSession, String> {
        let session = aerodesk_core::connect::connect_live(server, room)?;
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let pump = {
            let latest = latest.clone();
            let stop = stop.clone();
            thread::spawn(move || pump_media(session, latest, stop))
        };
        Ok(ViewerSession {
            latest,
            stop,
            thread: Some(pump),
        })
    }
}

impl Drop for ViewerSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 后台收流循环：UDP → endpoint → 事件 → 解码 → 最新帧槽。
fn pump_media(
    mut session: LiveSession,
    latest: Arc<Mutex<Option<CVPixelBuffer>>>,
    stop: Arc<AtomicBool>,
) {
    let mut decoder = H264Decoder::new();
    while !stop.load(Ordering::SeqCst) {
        session
            .socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok();
        let mut buf = [0u8; 4096];
        if let Ok((n, source)) = session.socket.recv_from(&mut buf) {
            if let Ok(contents) = buf[..n].try_into() {
                let _ = session.endpoint.handle_input(str0m::Input::Receive(
                    std::time::Instant::now(),
                    str0m::net::Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: session.socket.local_addr().unwrap(),
                        contents,
                    },
                ));
            }
        }
        let _ = session.endpoint.handle_timeout(std::time::Instant::now());
        while let Some(output) = session.endpoint.poll_output() {
            if let str0m::Output::Transmit(t) = output {
                let _ = session.socket.send_to(&t.contents, t.destination);
            }
        }
        while let Some(ev) = session.endpoint.poll_event() {
            match ev {
                ClientEvent::Media(data) => {
                    if let Some(mid) = session.video_mid
                        && data.mid == mid
                    {
                        feed_frame(&mut decoder, &data.data, &latest);
                    }
                }
                ClientEvent::Closed => {
                    stop.store(true, Ordering::SeqCst);
                    return;
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// 解码一帧并更新最新帧槽（槽内 +1 retained）。
fn feed_frame(
    decoder: &mut H264Decoder,
    annexb: &[u8],
    latest: &Arc<Mutex<Option<CVPixelBuffer>>>,
) {
    let Ok(Some(buf)) = decoder.decode_annexb(annexb, 0) else {
        return;
    };
    let raw = buf.as_ptr();
    unsafe { CFRetain(raw) }; // +1 → 2
    drop(buf); // -1 → 1（槽持有）
    let mut slot = latest.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(buf2) = unsafe { CVPixelBuffer::from_raw(raw) } {
        if let Some(old) = slot.replace(buf2) {
            drop(old); // -1 → 0
        }
    }
}

/// 取最新帧：有则转移 +1 引用给调用方（返回 0），无则返回 1。
///
/// # Safety
/// `out` 必须有效且可写。
pub unsafe fn take_frame(session: &ViewerSession, out: *mut *mut std::ffi::c_void) -> i32 {
    let mut slot = session.latest.lock().unwrap_or_else(|e| e.into_inner());
    match slot.take() {
        Some(buf) => {
            let raw = buf.as_ptr();
            unsafe { CFRetain(raw) }; // +1 → 2
            unsafe { *out = raw }; // 调用方持有
            drop(buf); // -1 → 1（归调用方）
            0
        }
        None => 1,
    }
}
