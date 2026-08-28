//! Android 发布会话（被控端）：SIP 被叫建链 + 逐帧发送 H.264。
//!
//! 采集/编码由 Kotlin 侧 MediaProjection + MediaCodec 完成，
//! 编码输出（AnnexB）通过 JNI `publisherFeedAnnexB` 交给本模块发送。
//! #598 P1d：信令从 JSON WSS join 迁 `connect_publisher_sip`（UAS 形态：
//! 注册即等被拨，观看端 INVITE 后建链）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::p2p_call::P2pCall;

/// 发布会话（FFI 句柄 = *mut PublisherSession）。
pub struct PublisherSession {
    p2p: Mutex<P2pCall>,
    video_mid: str0m::media::Mid,
    /// 输入事件（观看端 → 本被控端），JSON InputFrame。
    input_rx: Mutex<mpsc::Receiver<String>>,
    input_tx: mpsc::Sender<String>,
    stop: Arc<AtomicBool>,
}

impl PublisherSession {
    /// SIP 被叫建链（注册 + 等 INVITE + 静默接听 + ICE 收敛，返回即就绪）。
    pub fn connect(
        server: &str,
        room: &str,
        token: Option<&str>,
    ) -> Result<PublisherSession, String> {
        let (p2p, video_mid, _audio_mid) = aerodesk_core::connect::connect_publisher_sip(
            server, room, token, false, None, None, None,
        )?;
        let video_mid = video_mid.ok_or("publisher: 无视频 mid")?;
        let (input_tx, input_rx) = mpsc::channel();
        Ok(PublisherSession {
            p2p: Mutex::new(p2p),
            video_mid,
            input_rx: Mutex::new(input_rx),
            input_tx,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 发送一帧 AnnexB H.264 并泵 endpoint（ICE/RTCP/发送/输入事件）。
    pub fn feed(&self, annexb: &[u8], pts_us: i64) -> bool {
        let mut p2p = self.p2p.lock().unwrap_or_else(|e| e.into_inner());
        self.pump(&mut p2p);
        let rtp_time =
            str0m::media::MediaTime::new(pts_us as u64, str0m::media::Frequency::NINETY_KHZ);
        p2p.endpoint()
            .send_video_frame(self.video_mid, annexb.to_vec(), rtp_time)
            .is_ok()
    }

    /// 泵 P2pCall（内部含 UDP 输入→timeout→输出发送）+ 事件（输入通道 → input_tx）。
    fn pump(&self, p2p: &mut P2pCall) {
        let _ = p2p.poll();
        while let Some(ev) = p2p.poll_event() {
            if let ClientEvent::ChannelData(cid, _, data) = ev
                && p2p.endpoint().channel_label(cid).as_deref() == Some("input")
                && let Ok(s) = String::from_utf8(data)
            {
                let _ = self.input_tx.send(s);
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
