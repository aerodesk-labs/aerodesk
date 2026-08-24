//! iOS 观看会话：连接 + 媒体收流循环 + H.264/HEVC 硬解 → 最新帧槽（屏幕/摄像头双轨）。
//!
//! 由 `ffi.rs` 暴露给 Swift：`ad_viewer_create` 启动后台收流线程，
//! Swift 轮询 `ad_viewer_take_frame` 取最新 CVPixelBuffer 渲染。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use aerodesk_core::access_unit::AccessUnitAssembler;
use aerodesk_core::endpoint::ClientEvent;
use apple_cf::cv::CVPixelBuffer;
use str0m::format::Codec as SCodec;
use str0m::net::Protocol;

use crate::ios::decode::{DecoderKind, H264Decoder, HevcDecoder, detect_codec};

unsafe extern "C" {
    fn CFRetain(cf: *const std::ffi::c_void) -> *const std::ffi::c_void;
}

/// 观看端解码器（H264/HEVC 按关键帧参数集自动选择）。
enum UiDecoder {
    H264(H264Decoder),
    Hevc(HevcDecoder),
}

impl UiDecoder {
    fn feed(
        &mut self,
        data: &[u8],
        pts_us: u64,
    ) -> Result<Option<apple_cf::cv::CVPixelBuffer>, String> {
        match self {
            UiDecoder::H264(d) => d.decode_annexb(data, pts_us as i64),
            UiDecoder::Hevc(d) => d.decode_annexb(data, pts_us as i64),
        }
    }
}

/// 观看会话（FFI 句柄 = *mut ViewerSession）。
/// 连接与媒体组件归后台 pump 线程所有；结构体只保留控制面。
pub struct ViewerSession {
    /// 最新解码帧（+1 retained，调用方 take 后转移所有权）。
    latest: Arc<Mutex<Option<CVPixelBuffer>>>,
    /// 摄像头轨最新解码帧（发布端 --camera 且观看端请求第二路视频轨时才有）。
    latest_camera: Arc<Mutex<Option<CVPixelBuffer>>>,
    /// 画面源选择：false=屏幕 / true=摄像头（take_frame 按此返回对应槽）。
    show_camera: Arc<AtomicBool>,
    /// 是否已收到摄像头轨（Swift 侧据此启用切换按钮）。
    camera_available: Arc<AtomicBool>,
    /// 输入事件发送通道（viewer → publisher，经 input 数据通道）。
    input_tx: mpsc::Sender<Vec<u8>>,
    /// 解码后的 PCM i16 音频样本（8kHz 单声道；Swift 侧轮询取走播放）。
    audio_rx: mpsc::Receiver<Vec<i16>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ViewerSession {
    /// 连接信令并启动收流解码线程。
    pub fn connect(server: &str, room: &str) -> Result<ViewerSession, String> {
        // #552：WSS join → SIP（REGISTER + INVITE；SipViewerSession 看护线程持链）。
        // 请求第二路视频轨（摄像头 recvonly）：发布端未开 --camera 时 m-line
        // inactive，不影响屏幕流。
        let (_sip, mut endpoint, socket, _video_mid, _audio_mid, camera_mid) =
            aerodesk_core::connect::connect_viewer_sip(
                server, room, None, false, true, true, None, None,
            )?;
        let latest = Arc::new(Mutex::new(None));
        let latest_camera = Arc::new(Mutex::new(None));
        let show_camera = Arc::new(AtomicBool::new(false));
        let camera_available = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (input_tx, input_rx) = mpsc::channel();
        let (audio_tx, audio_rx) = mpsc::channel();
        let pump = {
            let latest = latest.clone();
            let latest_camera = latest_camera.clone();
            let show_camera = show_camera.clone();
            let camera_available = camera_available.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                pump_media(
                    endpoint,
                    socket,
                    camera_mid,
                    latest,
                    latest_camera,
                    show_camera,
                    camera_available,
                    stop,
                    input_rx,
                    audio_tx,
                )
            })
        };
        Ok(ViewerSession {
            latest,
            latest_camera,
            show_camera,
            camera_available,
            input_tx,
            audio_rx,
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

    /// 切换画面源：true=摄像头 / false=屏幕（take_frame 按此返回对应帧）。
    pub fn set_show_camera(&self, show: bool) {
        self.show_camera.store(show, Ordering::SeqCst);
    }

    /// 是否已收到摄像头轨（远端发布端带 --camera 且本端请求了第二路视频轨）。
    pub fn camera_available(&self) -> bool {
        self.camera_available.load(Ordering::SeqCst)
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
    mut endpoint: aerodesk_core::Endpoint,
    socket: aerodesk_core::media_socket::MediaSocket,
    camera_mid: Option<str0m::media::Mid>,
    latest: Arc<Mutex<Option<CVPixelBuffer>>>,
    latest_camera: Arc<Mutex<Option<CVPixelBuffer>>>,
    _show_camera: Arc<AtomicBool>,
    camera_available: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    input_rx: mpsc::Receiver<Vec<u8>>,
    audio_tx: mpsc::Sender<Vec<i16>>,
) {
    // H264/HEVC 自动识别：首关键帧含参数集后创建对应硬解器。
    let mut decoder: Option<UiDecoder> = None;
    let mut decoder_kind: Option<DecoderKind> = None;
    // 摄像头轨独立解码（屏幕/摄像头是两条 RTP 流，必须各自 assembler/解码器）。
    let mut camera_decoder: Option<UiDecoder> = None;
    let mut camera_decoder_kind: Option<DecoderKind> = None;
    let mut camera_assembler = AccessUnitAssembler::new();
    // 音频帧统计（本轮：音视频分流，音频暂不入视频组装器）。
    let mut audio_pkts = 0u64;
    // str0m 输出单条 AnnexB NAL；按 RTP 时间戳聚合为完整访问单元后再解码
    // （SPS/PPS 与 VCL 同帧，VideoToolbox 才能建 format description）。
    let mut assembler = AccessUnitAssembler::new();
    // 屏幕/摄像头轨区分：SFU 转发 mid 无法与本地协商 mid 直接比对。
    // 优先用 SFU 重协商 offer 的 sendonly 视频轨顺序（screen→camera，#340，
    // 确定性）；无该信息时回退到「首个视频 mid=屏幕、第二个=摄像头」到达序。
    let mut video_mids: Vec<str0m::media::Mid> = Vec::new();
    // 冒烟诊断：ICE/收包/解码计数（模拟器 --console 可见）。
    let mut diag_pkts = 0u64;
    let mut diag_media = 0u64;
    let mut diag_frames = 0u64;
    eprintln!(
        "pump: started, socket local={:?} camera_mid={:?}",
        socket.local_addr(),
        camera_mid
    );
    while !stop.load(Ordering::SeqCst) {
        // 输入事件：观看端捕获 → input 数据通道 → SFU → 被控端。
        while let Ok(json) = input_rx.try_recv() {
            endpoint.send_channel_data("input", false, &json);
        }

        session
            .socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok();
        let mut buf = [0u8; 4096];
        if let Ok((n, source)) = socket.recv_from(&mut buf) {
            diag_pkts += 1;
            if diag_pkts % 200 == 0 {
                eprintln!("pump: udp={diag_pkts} media={diag_media} frames={diag_frames}");
            }
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
                // 同 connect.rs：遇到 Timeout 必须退出本轮排空，否则 str0m
                // 反复返回同一 Timeout → 100% CPU 死循环，永远轮不到收包。
                str0m::Output::Timeout(_) => break,
                str0m::Output::Event(_) => {}
            }
        }
        while let Some(ev) = endpoint.poll_event() {
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
                    // 音视频分流：#1 不能按 session.video_mid 过滤（SFU 转发用本地
                    // mid），按 RTP payload codec 识别。音频帧不进视频组装器
                    // （否则会污染 AccessUnit 重组，画面花屏/黑屏）。
                    let spec = data.params.spec().codec;
                    let is_audio = matches!(spec, SCodec::PCMU | SCodec::PCMA | SCodec::Opus);
                    if is_audio {
                        audio_pkts += 1;
                        if audio_pkts % 200 == 0 {
                            eprintln!("pump: audio pkts={audio_pkts} codec={spec:?}");
                        }
                        // PCMU（8kHz μ-law）→ i16 样本 → Swift 播放。Opus 暂不播
                        // （iOS 无内置 Opus 解码，需 libopus/ffmpeg，后续再加）。
                        if spec == SCodec::PCMU {
                            let pcm = aerodesk_core::pcmu::pcmu_decode(data.data.as_ref());
                            let _ = audio_tx.send(pcm);
                        }
                        continue;
                    }
                    // 视频：先按参数集确认 codec，再组装 + 解码。
                    if !video_mids.contains(&data.mid) {
                        video_mids.push(data.mid);
                    }
                    let is_camera = {
                        let send_mids = endpoint.remote_send_video_mids();
                        if send_mids.len() >= 2 {
                            // #340：SFU offer 顺序确定 screen=mids[0]、camera=mids[1]。
                            send_mids.get(1) == Some(&data.mid)
                        } else {
                            video_mids.len() > 1 && video_mids[1] == data.mid
                        }
                    };
                    let target_latest = if is_camera { &latest_camera } else { &latest };
                    let dec = if is_camera {
                        if let Some(kind) = detect_codec(data.data.as_ref()) {
                            if camera_decoder_kind != Some(kind) || camera_decoder.is_none() {
                                camera_decoder = Some(match kind {
                                    DecoderKind::H264 => UiDecoder::H264(H264Decoder::new()),
                                    DecoderKind::Hevc => UiDecoder::Hevc(HevcDecoder::new()),
                                });
                                camera_decoder_kind = Some(kind);
                                eprintln!("pump: using camera decoder {kind:?}");
                            }
                        }
                        camera_decoder.as_mut()
                    } else {
                        if let Some(kind) = detect_codec(data.data.as_ref()) {
                            if decoder_kind != Some(kind) || decoder.is_none() {
                                decoder = Some(match kind {
                                    DecoderKind::H264 => UiDecoder::H264(H264Decoder::new()),
                                    DecoderKind::Hevc => UiDecoder::Hevc(HevcDecoder::new()),
                                });
                                decoder_kind = Some(kind);
                                eprintln!("pump: using decoder {kind:?}");
                            }
                        }
                        decoder.as_mut()
                    };
                    let Some(dec) = dec else {
                        continue; // 等关键帧
                    };
                    let au = if is_camera {
                        camera_assembler.push(
                            data.data.as_ref(),
                            data.time.as_micros(),
                            data.is_keyframe(),
                        )
                    } else {
                        assembler.push(
                            data.data.as_ref(),
                            data.time.as_micros(),
                            data.is_keyframe(),
                        )
                    };
                    if let Some(au) = au {
                        if is_camera && !camera_available.load(Ordering::SeqCst) {
                            camera_available.store(true, Ordering::SeqCst);
                            eprintln!("pump: 摄像头轨已接收");
                        }
                        diag_frames += 1;
                        if diag_frames <= 3 || diag_frames % 60 == 0 {
                            eprintln!(
                                "pump: decoded frame #{diag_frames} au_bytes={}",
                                au.data.len()
                            );
                        }
                        feed_frame(dec, &au.data, au.pts_us, target_latest);
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
    decoder: &mut UiDecoder,
    annexb: &[u8],
    pts_us: u64,
    latest: &Arc<Mutex<Option<CVPixelBuffer>>>,
) {
    let Ok(Some(buf)) = decoder.feed(annexb, pts_us) else {
        return;
    };
    let raw = buf.as_ptr();
    unsafe { CFRetain(raw) }; // +1 → 2
    drop(buf); // -1 → 1（槽持有）
    let mut slot = latest.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(buf2) = CVPixelBuffer::from_raw(raw) {
        if let Some(old) = slot.replace(buf2) {
            drop(old); // -1 → 0
        }
    }
}

/// 取解码后的 PCM i16 音频样本（8kHz 单声道）：拷贝到 `dst`，返回拷贝样本数。
/// 0=暂无新样本；-1=参数错误。
///
/// # Safety
/// `dst` 必须指向至少 `max` 个 i16 的有效空间。
pub unsafe fn take_audio(session: &ViewerSession, dst: *mut i16, max: usize) -> i32 {
    if dst.is_null() || max == 0 {
        return -1;
    }
    let mut copied = 0i32;
    while copied as usize + 320 <= max {
        let Ok(samples) = session.audio_rx.try_recv() else {
            break;
        };
        let n = samples.len().min(max - copied as usize);
        unsafe { std::ptr::copy_nonoverlapping(samples.as_ptr(), dst.add(copied as usize), n) };
        copied += n as i32;
    }
    copied
}

/// 取最新帧：有则转移 +1 引用给调用方（返回 0），无则返回 1。
///
/// # Safety
/// `out` 必须有效且可写。
pub unsafe fn take_frame(session: &ViewerSession, out: *mut *mut std::ffi::c_void) -> i32 {
    // 摄像头视图：优先返回摄像头帧；未出帧时回退屏幕帧（避免黑屏）。
    let mut slot = if session.show_camera.load(Ordering::SeqCst) {
        let cam = session
            .latest_camera
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let screen = session.latest.lock().unwrap_or_else(|e| e.into_inner());
        if cam.is_some() { cam } else { screen }
    } else {
        session.latest.lock().unwrap_or_else(|e| e.into_inner())
    };
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
