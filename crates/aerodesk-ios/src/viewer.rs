//! iOS 观看会话：连接 + 媒体收流循环 + H.264 硬解 → 最新帧槽。
//!
//! 由 `ffi.rs` 暴露给 Swift：`ad_viewer_create` 启动后台收流线程，
//! Swift 轮询 `ad_viewer_take_frame` 取最新 CVPixelBuffer 渲染。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use aerodesk_core::access_unit::AccessUnitAssembler;
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
    /// 输入事件发送通道（viewer → publisher，经 input 数据通道）。
    input_tx: mpsc::Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ViewerSession {
    /// 连接信令并启动收流解码线程。
    pub fn connect(server: &str, room: &str) -> Result<ViewerSession, String> {
        let session = aerodesk_core::connect::connect_live(server, room)?;
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (input_tx, input_rx) = mpsc::channel();
        let pump = {
            let latest = latest.clone();
            let stop = stop.clone();
            thread::spawn(move || pump_media(session, latest, stop, input_rx))
        };
        Ok(ViewerSession {
            latest,
            input_tx,
            stop,
            thread: Some(pump),
        })
    }
}

impl ViewerSession {
    /// 发送输入事件（JSON InputFrame）。失败返回 false（通道未开/已断开）。
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

/// 后台收流循环：UDP → endpoint → 事件 → 解码 → 最新帧槽；并转发输入事件。
fn pump_media(
    mut session: LiveSession,
    latest: Arc<Mutex<Option<CVPixelBuffer>>>,
    stop: Arc<AtomicBool>,
    input_rx: mpsc::Receiver<Vec<u8>>,
) {
    let mut decoder = H264Decoder::new();
    // str0m 输出单条 AnnexB NAL；按 RTP 时间戳聚合为完整访问单元后再解码
    // （SPS/PPS 与 VCL 同帧，VideoToolbox 才能建 format description）。
    let mut assembler = AccessUnitAssembler::new();
    // 冒烟诊断：ICE/收包/解码计数（模拟器 --console 可见）。
    let mut diag_pkts = 0u64;
    let mut diag_media = 0u64;
    let mut diag_frames = 0u64;
    eprintln!(
        "pump: started, socket local={:?}",
        session.socket.local_addr()
    );
    while !stop.load(Ordering::SeqCst) {
        // 输入事件：观看端捕获 → input 数据通道 → SFU → 被控端。
        while let Ok(json) = input_rx.try_recv() {
            session.endpoint.send_channel_data("input", false, &json);
        }

        session
            .socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok();
        let mut buf = [0u8; 4096];
        if let Ok((n, source)) = session.socket.recv_from(&mut buf) {
            diag_pkts += 1;
            if diag_pkts % 200 == 0 {
                eprintln!("pump: udp={diag_pkts} media={diag_media} frames={diag_frames}");
            }
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
            match output {
                str0m::Output::Transmit(t) => {
                    let _ = session.socket.send_to(&t.contents, t.destination);
                }
                // 同 connect.rs：遇到 Timeout 必须退出本轮排空，否则 str0m
                // 反复返回同一 Timeout → 100% CPU 死循环，永远轮不到收包。
                str0m::Output::Timeout(_) => break,
                str0m::Output::Event(_) => {}
            }
        }
        while let Some(ev) = session.endpoint.poll_event() {
            match ev {
                ClientEvent::Media(data) => {
                    diag_media += 1;
                    if diag_media % 200 == 0 {
                        eprintln!(
                            "pump: media event #{diag_media} mid={:?} bytes={}",
                            data.mid,
                            data.data.len()
                        );
                    }
                    // #1：不能按 session.video_mid 过滤——SFU 转发时 RTP mid 扩展
                    // 用 SFU 本地 mid（与 viewer 协商的 mid 不同，CLI 同款处理，
                    // 见 main.rs #58/#73 注释）。iOS 仅订阅视频，直接喂组装器。
                    if let Some(au) = assembler.push(
                        data.data.as_ref(),
                        data.time.as_micros(),
                        data.is_keyframe(),
                    ) {
                        diag_frames += 1;
                        if diag_frames <= 3 || diag_frames % 60 == 0 {
                            eprintln!(
                                "pump: decoded frame #{diag_frames} au_bytes={}",
                                au.data.len()
                            );
                        }
                        feed_frame(&mut decoder, &au.data, au.pts_us, &latest);
                    }
                }
                ClientEvent::IceConnected => {
                    eprintln!("pump: ICE connected");
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
    pts_us: u64,
    latest: &Arc<Mutex<Option<CVPixelBuffer>>>,
) {
    let Ok(Some(buf)) = decoder.decode_annexb(annexb, pts_us as i64) else {
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
