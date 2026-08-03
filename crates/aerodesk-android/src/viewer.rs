//! Android 观看会话：连接 + 后台收流循环 → 最新 AnnexB H.264 帧槽。
//!
//! 解码由 Kotlin 侧 MediaCodec 完成（Surface 渲染），Rust 只负责
//! WebRTC 收流并把最新 H.264 帧交给壳层（与 iOS 的差异：iOS 用 VideoToolbox
//! 在 Rust 侧硬解，Android 走 MediaCodec）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use aerodesk_core::connect::LiveSession;
use aerodesk_core::endpoint::ClientEvent;
use str0m::net::Protocol;

/// 观看会话（FFI 句柄 = *mut ViewerSession）。
pub struct ViewerSession {
    /// 最新 AnnexB 帧（拷贝）。
    latest: Arc<Mutex<Option<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ViewerSession {
    /// 连接信令并启动收流线程。
    pub fn connect(server: &str, room: &str) -> Result<ViewerSession, String> {
        let live = aerodesk_core::connect::connect_live(server, room)?;
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let pump = {
            let latest = latest.clone();
            let stop = stop.clone();
            thread::spawn(move || pump_media(live, latest, stop))
        };
        Ok(ViewerSession {
            latest,
            stop,
            thread: Some(pump),
        })
    }

    /// 取走最新帧（None 表示暂无新帧）。
    pub fn take_annexb(&self) -> Option<Vec<u8>> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).take()
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

/// 后台收流循环：UDP → endpoint → Media 事件 → 最新帧槽。
fn pump_media(mut live: LiveSession, latest: Arc<Mutex<Option<Vec<u8>>>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        live.socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok();
        let mut buf = [0u8; 4096];
        if let Ok((n, source)) = live.socket.recv_from(&mut buf) {
            if let Ok(contents) = buf[..n].try_into() {
                let _ = live.endpoint.handle_input(str0m::Input::Receive(
                    std::time::Instant::now(),
                    str0m::net::Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: live.socket.local_addr().unwrap(),
                        contents,
                    },
                ));
            }
        }
        let _ = live.endpoint.handle_timeout(std::time::Instant::now());
        while let Some(output) = live.endpoint.poll_output() {
            if let str0m::Output::Transmit(t) = output {
                let _ = live.socket.send_to(&t.contents, t.destination);
            }
        }
        while let Some(ev) = live.endpoint.poll_event() {
            match ev {
                ClientEvent::Media(data) => {
                    if let Some(mid) = live.video_mid
                        && data.mid == mid
                    {
                        *latest.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(data.data.to_vec());
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
