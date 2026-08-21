//! HarmonyOS 发布会话（被控端骨架）：连接信令 + 逐帧发送 H.264 → SFU。
//!
//! 真机路径：AVScreenCapture 输出 → OH_VideoEncoder 编码 → 本模块发送；
//! 编码输出（AnnexB）由 ArkTS 侧通过 `publisherFeedAnnexB` 交给 Rust。
//! 输入注入仍需系统权限，当前保留通道与占位实现。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use aerodesk_core::connect::LiveSession;
use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::protocol::signal::Role;

/// 发布会话（NAPI `startPublish` 返回的 session id 即指向本结构）。
pub struct PublisherSession {
    live: Mutex<LiveSession>,
    /// 输入事件（观看端 → 本被控端），JSON InputFrame。
    input_rx: Mutex<mpsc::Receiver<String>>,
    input_tx: mpsc::Sender<String>,
    stop: Arc<AtomicBool>,
}

impl PublisherSession {
    /// 连接信令（publisher 角色）+ SDP 交换 + ICE 泵。
    pub fn connect(
        server: &str,
        room: &str,
        token: Option<&str>,
    ) -> Result<PublisherSession, String> {
        let live = aerodesk_core::connect::connect_live_role(server, room, Role::Publisher, token)
            .map_err(|e| e.to_string())?;
        let (input_tx, input_rx) = mpsc::channel();
        Ok(PublisherSession {
            live: Mutex::new(live),
            input_rx: Mutex::new(input_rx),
            input_tx,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 发送一帧 AnnexB H.264 并泵 endpoint（ICE/RTCP/发送/输入事件）。
    pub fn feed(&self, annexb: &[u8], pts_us: i64) -> bool {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        self.pump(&mut live);
        let Some(mid) = live.video_mid else {
            return false;
        };
        let rtp_time =
            str0m::media::MediaTime::new(pts_us as u64, str0m::media::Frequency::NINETY_KHZ);
        live.endpoint
            .send_video_frame(mid, annexb.to_vec(), rtp_time)
            .is_ok()
    }

    /// 泵 endpoint：超时/输出/事件（输入通道 → input_tx）。
    fn pump(&self, live: &mut LiveSession) {
        let _ = live.endpoint.handle_timeout(Instant::now());
        while let Some(output) = live.endpoint.poll_output() {
            match output {
                str0m::Output::Transmit(t) => {
                    let _ = live.socket.send_to(&t.contents, t.destination);
                }
                // 同 viewer：Timeout 必须 break，否则死循环卡死发布会话。
                str0m::Output::Timeout(_) => break,
                str0m::Output::Event(_) => {}
            }
        }
        while let Some(ev) = live.endpoint.poll_event() {
            if let ClientEvent::ChannelData(cid, _, data) = ev {
                if live.endpoint.channel_label(cid).as_deref() == Some("input") {
                    if let Ok(s) = String::from_utf8(data) {
                        let _ = self.input_tx.send(s);
                    }
                }
            }
        }
    }

    /// 取一条输入事件 JSON（None 表示暂无）。
    pub fn take_input(&self) -> Option<String> {
        self.input_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_recv()
            .ok()
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
