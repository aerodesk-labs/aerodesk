//! Android 观看会话：连接 + 后台收流循环 → 最新完整访问单元（视频帧）槽。
//!
//! 解码由 Kotlin 侧 MediaCodec 完成（Surface 渲染），Rust 负责 WebRTC 收流，
//! 并把 str0m 输出的 NAL 事件经 [`AccessUnitAssembler`] 聚合成完整帧交给壳层
//! （与 iOS 的差异：iOS 用 VideoToolbox 在 Rust 侧硬解，Android 走 MediaCodec）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use aerodesk_core::access_unit::AccessUnitAssembler;
use aerodesk_core::endpoint::ClientEvent;
use str0m::net::Protocol;

/// 观看会话（FFI 句柄 = *mut ViewerSession）。
pub struct ViewerSession {
    /// 最新 AnnexB 帧（拷贝）。
    latest: Arc<Mutex<Option<Vec<u8>>>>,
    /// 输入事件发送通道（viewer → 被控端，经 input 数据通道）。
    input_tx: mpsc::Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ViewerSession {
    /// 连接信令并启动收流线程。`force_relay` 为 true 时 ICE 只通告 relayed
    /// 候选（#201：规避 NAT/模拟器下直连候选假通导致媒体入站被丢）。
    pub fn connect(server: &str, room: &str, force_relay: bool) -> Result<ViewerSession, String> {
        // #552：WSS join → SIP（REGISTER + INVITE；SipViewerSession 看护线程持链）。
        let (_sip, mut endpoint, socket, _video_mid, _audio_mid, _camera_mid) =
            aerodesk_core::connect::connect_viewer_sip(
                server,
                room,
                None,
                force_relay,
                true,
                false,
                None,
                None,
            )?;
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (input_tx, input_rx) = mpsc::channel();
        let pump = {
            let latest = latest.clone();
            let stop = stop.clone();
            thread::spawn(move || pump_media(endpoint, socket, latest, stop, input_rx))
        };
        Ok(ViewerSession {
            latest,
            input_tx,
            stop,
            thread: Some(pump),
        })
    }

    /// 取走最新完整视频帧（AnnexB 访问单元；None 表示暂无新帧）。
    pub fn take_frame(&self) -> Option<Vec<u8>> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// 发送输入事件（JSON InputFrame）。失败返回 false（通道已断开）。
    pub fn send_input(&self, json: &[u8]) -> bool {
        self.input_tx.send(json.to_vec()).is_ok()
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

/// 后台收流循环：UDP → endpoint → Media 事件 → 访问单元组装 → 最新帧槽。
fn pump_media(
    mut endpoint: aerodesk_core::Endpoint,
    socket: aerodesk_core::media_socket::MediaSocket,
    latest: Arc<Mutex<Option<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    input_rx: mpsc::Receiver<Vec<u8>>,
) {
    // 把 str0m 输出的 NAL 事件按 RTP 时间戳聚合成完整访问单元。
    let mut assembler = AccessUnitAssembler::new();
    while !stop.load(Ordering::SeqCst) {
        // 输入事件：观看端触摸/按键 → input 数据通道 → SFU → 被控端。
        while let Ok(json) = input_rx.try_recv() {
            endpoint.send_channel_data("input", false, &json);
        }
        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok();
        let mut buf = [0u8; 4096];
        if let Ok((n, source)) = socket.recv_from(&mut buf) {
            if let Ok(contents) = buf[..n].try_into() {
                let _ = endpoint.handle_input(str0m::Input::Receive(
                    std::time::Instant::now(),
                    str0m::net::Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: socket.local_addr().unwrap(),
                        contents,
                    },
                ));
            }
        }
        let _ = endpoint.handle_timeout(std::time::Instant::now());
        while let Some(output) = endpoint.poll_output() {
            match output {
                str0m::Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                // 关键：遇到 Timeout 必须退出本轮排空，否则 str0m 反复返回
                // 同一个 Timeout → 100% CPU 死循环，媒体永远不处理
                // （同 connect.rs / CLI / iOS 的既有修复，Android 侧漏了）。
                str0m::Output::Timeout(_) => break,
                str0m::Output::Event(_) => {}
            }
        }
        while let Some(ev) = endpoint.poll_event() {
            match ev {
                ClientEvent::Media(data) => {
                    // 不能按 video_mid 过滤——SFU 转发时 RTP mid 扩展用 SFU
                    // 本地 mid（与 viewer 协商的 mid 不同，CLI/iOS 同款处理，
                    // 见 main.rs #58/#73 注释）。Android 观看端仅订阅视频，
                    // 直接喂组装器（与 iOS viewer 一致）。
                    if let Some(frame) = assembler.push(
                        data.data.as_ref(),
                        data.time.as_micros(),
                        data.is_keyframe(),
                    ) {
                        *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame.data);
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
