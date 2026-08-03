//! Android 发布会话（被控端）：连接信令 + 逐帧发送 H.264 → SFU。
//!
//! 采集/编码由 Kotlin 侧 MediaProjection + MediaCodec 完成，
//! 编码输出（AnnexB）通过 JNI `publisherFeedAnnexB` 交给本模块发送。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aerodesk_core::connect::LiveSession;
use aerodesk_protocol::signal::Role;

/// 发布会话（FFI 句柄 = *mut PublisherSession）。
pub struct PublisherSession {
    live: Mutex<LiveSession>,
    stop: Arc<AtomicBool>,
}

impl PublisherSession {
    /// 连接信令（publisher 角色）+ SDP 交换 + ICE 泵。
    pub fn connect(server: &str, room: &str) -> Result<PublisherSession, String> {
        let live = aerodesk_core::connect::connect_live_role(server, room, Role::Publisher)?;
        Ok(PublisherSession {
            live: Mutex::new(live),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 发送一帧 AnnexB H.264 并泵 endpoint（ICE/RTCP/发送）。
    pub fn feed(&self, annexb: &[u8], pts_us: i64) -> bool {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let _ = live.endpoint.handle_timeout(Instant::now());
        while let Some(output) = live.endpoint.poll_output() {
            if let str0m::Output::Transmit(t) = output {
                let _ = live.socket.send_to(&t.contents, t.destination);
            }
        }
        let Some(mid) = live.video_mid else {
            return false;
        };
        let rtp_time =
            str0m::media::MediaTime::new(pts_us as u64, str0m::media::Frequency::NINETY_KHZ);
        live.endpoint
            .send_video_frame(mid, annexb.to_vec(), rtp_time)
            .is_ok()
    }

    /// 停止并释放。
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for PublisherSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}
