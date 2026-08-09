//! AeroDesk CLI 客户端。
//!
//! publisher：连接 SFU，用真实 VP8 抓包流作为媒体源发送视频。
//! viewer：连接 SFU，接收媒体并打印统计。
//!
//! 用法：
//!   aerodesk-cli --role publisher --signal ws://127.0.0.1:3003 --room demo
//!   aerodesk-cli --role viewer    --signal ws://127.0.0.1:3003 --room demo

#[macro_use]
extern crate tracing;

mod clipboard;
mod cmd_exec;
mod file_transfer;

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::media::{Vp8Frame, parse_vp8_pcap};
use aerodesk_core::media_socket::MediaSocket;
use aerodesk_core::turn_client::setup_turn;
use aerodesk_core::{Endpoint, media_pipeline::Codec, signaling::WsSignalClient};
use aerodesk_ffmpeg::encode::FfmpegEncoder;
use aerodesk_protocol::input::{
    ButtonState, INPUT_PROTOCOL_VERSION, InputEvent, InputFrame, Modifiers, MouseButton,
};
use aerodesk_protocol::signal::Role;
use str0m::media::{Frequency, MediaTime};
use str0m::net::Protocol;
use str0m::{Input, Output, net::Receive};

/// 重连退避（#173）：1s、2s、4s、8s，之后封顶 10s。
fn reconnect_backoff(attempt: u32) -> Duration {
    let secs = 1u64 << attempt.min(4);
    Duration::from_secs(secs.min(10))
}

/// 会话自动重连包装（#173）：Err 且开启重连且未达上限 → 退避重试；否则退出。
fn run_with_reconnect<F>(mut f: F, reconnect: bool, max: u32)
where
    F: FnMut() -> Result<(), String>,
{
    let mut attempt = 0u32;
    loop {
        match f() {
            Ok(()) => break,
            Err(e) if reconnect && attempt < max => {
                attempt += 1;
                warn!(
                    "session ended: {e}; reconnecting in {:?} (attempt {attempt}/{max})",
                    reconnect_backoff(attempt)
                );
                std::thread::sleep(reconnect_backoff(attempt));
            }
            Err(e) => {
                eprintln!("session error: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn main() {
    // #122/#102：sctp-proto 深调用链在文件传输/高负载下会栈溢出（与 SFU 修复 #104
    // 同根因）；主逻辑放到 8MB 栈线程执行。
    let handle = std::thread::Builder::new()
        .stack_size(32 << 20)
        .spawn(run)
        .expect("spawn main thread");
    handle.join().expect("main thread join");
}

fn run() {
    init_log();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--issue-token") {
        issue_token(&args);
        return;
    }
    let role = arg(&args, "--role").unwrap_or_else(|| "viewer".into());
    let signal = arg(&args, "--signal").unwrap_or_else(|| "ws://127.0.0.1:3003/ws".into());
    let signal = if signal.contains("/ws") {
        signal
    } else {
        format!("{signal}/ws")
    };
    let room = arg(&args, "--room").unwrap_or_else(|| "demo".into());
    let token = arg(&args, "--token");
    // #173 自动重连：会话结束/连接失败时指数退避重试（--reconnect 开启）。
    let reconnect = args.iter().any(|a| a == "--reconnect");
    let reconnect_max: u32 = arg(&args, "--reconnect-max")
        .and_then(|m| m.parse().ok())
        .unwrap_or(5);
    let encoder = arg(&args, "--encoder").unwrap_or_else(|| "pcap".into());
    // #58：publisher 多路编码（q/h/f 三层），SFU 选层请求才能真正切换画质。
    let simulcast = args.iter().any(|a| a == "--simulcast");
    // 高熵合成源（伪随机噪声）：码率贴近目标档位，用于选层/压测验证。
    let noisy = args.iter().any(|a| a == "--noisy");
    // #58 音频：publisher 发送合成 PCMU 音频 / viewer 接收；--mute-audio 观看端静音。
    let audio = args.iter().any(|a| a == "--audio");
    // #73 音频：--audio-opus 使用 Opus（48kHz）替代 PCMU（8kHz）。
    let audio_opus = args.iter().any(|a| a == "--audio-opus");
    let mute_audio = args.iter().any(|a| a == "--mute-audio");
    // #75/#109 MCP 键鼠：--send-input '<InputEvent JSON>' 单次发送输入事件后退出。
    let send_input: Option<InputEvent> =
        arg(&args, "--send-input").and_then(|json| serde_json::from_str::<InputEvent>(&json).ok());
    // #109 MCP 键鼠：--type-text "<text>" 逐字符按键（US 布局 + Shift）。
    let type_text = arg(&args, "--type-text");
    // #109 远程命令/文件/进程：控制端一次执行（打印结果后以 ok 语义退出）。
    // --cmd-json：把 CmdResponse 以 JSON 输出到 stdout（MCP 桥接用）。
    let cmd_json = args.iter().any(|a| a == "--cmd-json");
    let cmd_intent: Option<cmd_exec::Intent> = if let Some(c) = arg(&args, "--run-command") {
        Some(cmd_exec::Intent::Run(c))
    } else if let Some(p) = arg(&args, "--read-file") {
        Some(cmd_exec::Intent::Read(p))
    } else if let Some(idx) = args.iter().position(|a| a == "--write-file") {
        let path = args.get(idx + 1).cloned();
        let content = args.get(idx + 2).cloned();
        path.zip(content)
            .map(|(p, c)| cmd_exec::Intent::Write(p, c))
    } else if args.iter().any(|a| a == "--list-processes") {
        Some(cmd_exec::Intent::Ps)
    } else if let Some(pid) = arg(&args, "--kill-pid").and_then(|v| v.parse().ok()) {
        Some(cmd_exec::Intent::Kill(pid))
    } else {
        None
    };
    // #58 显示器：publisher 初始采集显示器 / viewer 请求切换（--display N，0 = 主显示器）。
    let display: usize = arg(&args, "--display")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let viewer_display: Option<usize> = arg(&args, "--display").and_then(|v| v.parse().ok());
    // #72 文件传输：--send-file <path> 发送；--recv-dir <dir> 接收落盘；
    // --cancel-send-after <secs>：启动后到达该时刻自动取消发送（e2e 回归用）。
    let send_file = arg(&args, "--send-file").map(std::path::PathBuf::from);
    let recv_dir = arg(&args, "--recv-dir").map(std::path::PathBuf::from);
    // #122：viewer --request-file <path> 请求被控端发送文件（大文件下载）。
    let request_file = arg(&args, "--request-file");
    // #75 输入回传：--input-script 让 viewer 脚本化发送全部事件类型（e2e 断言用）。
    let input_script = args.iter().any(|a| a == "--input-script");
    // #72 取消回归：--cancel-send-after <secs> 启动后定时取消发送。
    let cancel_send_after = arg(&args, "--cancel-send-after")
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_secs);
    // #74 视频编码：--codec h264|h265|vp9|av1（配 --encoder ffmpeg）。
    let video_codec: Codec = match arg(&args, "--codec").as_deref() {
        Some("h265") | Some("hevc") => Codec::Hevc,
        Some("vp9") => Codec::Vp9,
        Some("av1") => Codec::Av1,
        _ => Codec::H264,
    };

    // #72 文件传输状态机（进程级单例；发送/接收同一状态机）。
    file_transfer::init(send_file, recv_dir, cancel_send_after);
    // #109 远程命令通道：被控端执行器（publisher）/ 控制端响应（viewer）。
    cmd_exec::init();

    // #109 权限/审计本地管理（无需会话；处理完直接退出）。
    if cmd_exec::run_admin(&args) {
        return;
    }

    match role.as_str() {
        "publisher" if encoder == "screen" => {
            let vt_capable = video_codec == Codec::H264
                || (video_codec == Codec::Hevc
                    && aerodesk_macos::vt_encoder::VtEncoder::hevc_encoder_available());
            if vt_capable {
                // #74 硬编优先：H264/H265 走 VideoToolbox 硬编（HEVC 无硬编
                // 时探针失败回退 FFmpeg）。
                publisher_capture(
                    &signal,
                    &room,
                    token.as_deref(),
                    simulcast,
                    audio,
                    audio_opus,
                    display,
                    video_codec,
                )
            } else {
                // #74：VP9/AV1 或本机无 VT HEVC 时，屏幕采集走 FFmpeg 软编。
                publisher_capture_ffmpeg(
                    &signal,
                    &room,
                    token.as_deref(),
                    audio,
                    audio_opus,
                    video_codec,
                    display,
                )
            }
        }
        "publisher" if encoder == "vt" => {
            let w: u32 = arg(&args, "--width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(640);
            let h: u32 = arg(&args, "--height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(360);
            let fps: u32 = arg(&args, "--fps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(30);
            let br: u32 = arg(&args, "--bitrate")
                .and_then(|v| v.parse().ok())
                .unwrap_or(800_000);
            publisher_vt(
                &signal,
                &room,
                token.as_deref(),
                VideoParams {
                    width: w,
                    height: h,
                    fps,
                    bitrate: br,
                },
                simulcast,
                noisy,
                audio,
                audio_opus,
            );
        }
        "publisher" if encoder == "ffmpeg" => publisher_ffmpeg(
            &signal,
            &room,
            token.as_deref(),
            audio,
            audio_opus,
            video_codec,
            noisy,
        ),
        "publisher" if encoder == "x264" => publisher_x264(
            &signal,
            &room,
            token.as_deref(),
            simulcast,
            noisy,
            audio,
            audio_opus,
        ),
        "publisher" => {
            let sig = signal.clone();
            let r = room.clone();
            let tok = token.clone();
            run_with_reconnect(
                move || publisher(&sig, &r, tok.as_deref(), audio, audio_opus),
                reconnect,
                reconnect_max,
            )
        }
        "viewer" => {
            let sig = signal.clone();
            let r = room.clone();
            let tok = token.clone();
            let layer = arg(&args, "--layer");
            let si = send_input.clone();
            let tt = type_text.clone();
            let ci = cmd_intent.clone();
            let rf = request_file.clone();
            run_with_reconnect(
                move || {
                    viewer(
                        &sig,
                        &r,
                        tok.as_deref(),
                        layer.as_deref(),
                        audio,
                        mute_audio,
                        viewer_display,
                        input_script,
                        si.as_ref(),
                        tt.as_deref(),
                        ci.as_ref(),
                        cmd_json,
                        rf.as_deref(),
                    )
                },
                reconnect,
                reconnect_max,
            )
        }
        other => panic!("unknown role {other}"),
    }
}

/// 签发信令 JWT（供运维/测试使用）。
///
/// 用法：
///   JWT_SECRET=<secret> aerodesk-cli --issue-token --user u1 --device mac-1 --room demo --role publisher --ttl 3600
///   JWT_SECRET=<secret> aerodesk-cli --issue-token --user u1 --room demo --role "*" --ttl 86400 [--max-conns 4]
fn issue_token(args: &[String]) {
    let secret = match std::env::var("JWT_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            eprintln!("JWT_SECRET 环境变量未设置");
            std::process::exit(1);
        }
    };
    let user = arg(args, "--user").unwrap_or_else(|| {
        eprintln!("缺少 --user");
        std::process::exit(1);
    });
    let device = arg(args, "--device");
    let room = arg(args, "--room");
    let role = match arg(args, "--role").as_deref() {
        Some("publisher") => Some(Role::Publisher),
        Some("viewer") => Some(Role::Viewer),
        Some("*") | None => None,
        Some(other) => {
            eprintln!("unknown role: {other} (publisher/viewer/*)");
            std::process::exit(1);
        }
    };
    let ttl: u64 = arg(args, "--ttl")
        .and_then(|t| t.parse().ok())
        .unwrap_or(3600);
    let max_conns: Option<u32> = arg(args, "--max-conns").and_then(|m| m.parse().ok());

    match aerodesk_protocol::jwt::mint_token(
        &secret,
        &user,
        device.as_deref(),
        room.as_deref(),
        role,
        ttl,
        max_conns,
    ) {
        Ok(token) => println!("{token}"),
        Err(e) => {
            eprintln!("签发失败: {e}");
            std::process::exit(1);
        }
    }
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .map(|i| args[i + 1].clone())
}

fn init_log() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aerodesk_cli=info"));
    tracing_subscriber::registry()
        // 日志写 stderr：`--cmd-json` 需要 stdout 只有 JSON（#109 MCP 桥接）。
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(filter)
        .init();
}

/// 连接信令 + 建立 Endpoint（公共步骤）。返回 (signal, endpoint, mut socket, video_mid, audio_mid)。
fn connect(
    signal_url: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
    audio: bool,
) -> Result<
    (
        WsSignalClient,
        Endpoint,
        MediaSocket,
        str0m::media::Mid,
        Option<str0m::media::Mid>,
    ),
    String,
> {
    connect_inner(signal_url, room, role, None, false, audio, auth)
}

fn connect_h264(
    signal_url: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
    simulcast: bool,
    audio: bool,
) -> Result<
    (
        WsSignalClient,
        Endpoint,
        MediaSocket,
        str0m::media::Mid,
        Option<str0m::media::Mid>,
    ),
    String,
> {
    connect_inner(
        signal_url,
        room,
        role,
        Some(Codec::H264),
        simulcast,
        audio,
        auth,
    )
}

fn connect_codec(
    signal_url: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
    audio: bool,
    codec: Codec,
) -> Result<
    (
        WsSignalClient,
        Endpoint,
        MediaSocket,
        str0m::media::Mid,
        Option<str0m::media::Mid>,
    ),
    String,
> {
    connect_inner(signal_url, room, role, Some(codec), false, audio, auth)
}

fn connect_inner(
    signal_url: &str,
    room: &str,
    role: Role,
    codec: Option<Codec>,
    simulcast: bool,
    audio: bool,
    auth: Option<&str>,
) -> Result<
    (
        WsSignalClient,
        Endpoint,
        MediaSocket,
        str0m::media::Mid,
        Option<str0m::media::Mid>,
    ),
    String,
> {
    let mut signal =
        WsSignalClient::connect(signal_url).map_err(|e| format!("signal connect: {e}"))?;
    let (peer_id, turn) = signal.join(room, role, auth)?;
    info!("joined room {room} as {peer_id}");

    let direct = UdpSocket::bind("127.0.0.1:0").map_err(|e| format!("bind udp: {e}"))?;
    let addr = direct.local_addr().map_err(|e| e.to_string())?;
    info!("local UDP addr: {addr}");

    // #157 M2：join 返回 TURN 配置时建立中继传输（失败仅告警，直连兜底）。
    let turn_transport = turn.as_ref().and_then(|tc| setup_turn(tc, true));
    let socket = MediaSocket::new(direct, turn_transport);

    let mut endpoint = match codec {
        None => Endpoint::new(),
        Some(Codec::H264) => Endpoint::new_h264(),
        Some(c) => Endpoint::new_with_codec(c),
    };
    endpoint
        .add_local_candidate(addr, Protocol::Udp)
        .map_err(|e| format!("candidate: {e}"))?;
    // #157 M2：relayed 候选加入 offer（`typ relay`），ICE 按优先级直连优先、TURN 兜底。
    if let Some(tt) = socket.turn() {
        let relayed = tt.relayed_addr();
        if let Ok(la) = tt.local_addr() {
            let local = std::net::SocketAddr::new(addr.ip(), la.port());
            info!("relayed candidate {relayed} (local {local})");
            if let Err(e) = endpoint.add_relay_candidate(relayed, local) {
                warn!("relay candidate rejected (TURN disabled): {e:?}");
            }
        }
    }

    // #12：viewer 的 offer 用 recvonly（SFU 拒绝 viewer 发布媒体）。
    if role == Role::Viewer {
        endpoint.add_video_recvonly();
    } else if simulcast {
        // #58：publisher 多路编码 → offer 携带 a=simulcast/rid（q/h/f）。
        endpoint.add_video_simulcast();
    } else {
        endpoint.add_video();
    }
    // #58 音频：publisher 发 PCMU，viewer 收 PCMU（recvonly）。
    if audio {
        if role == Role::Viewer {
            endpoint.add_audio_recvonly();
        } else {
            endpoint.add_audio();
        }
    }
    let (offer, pending, video_mid, audio_mid) =
        endpoint.create_offer().map_err(|e| format!("offer: {e}"))?;
    info!("video mid: {video_mid:?} audio mid: {audio_mid:?}");
    let offer_json = serde_json::to_string(&offer).map_err(|e| e.to_string())?;
    let answer_json = signal.exchange_description(&offer_json)?;
    let answer: str0m::change::SdpAnswer =
        serde_json::from_str(&answer_json).map_err(|e| format!("answer parse: {e}"))?;
    debug!("answer media lines: {:?}", answer.media_lines);
    debug!("offer media lines: {:?}", offer.media_lines);
    endpoint
        .accept_answer(pending, answer)
        .map_err(|e| format!("accept answer: {e}"))?;

    info!("SDP negotiated, awaiting ICE...");
    let video_mid = video_mid.ok_or("no video mid")?;
    Ok((signal, endpoint, socket, video_mid, audio_mid))
}

/// VideoToolbox 合成源编码参数（--width/--height/--fps/--bitrate）。
struct VideoParams {
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
}

/// simulcast 层参数（画质档位：0=清晰 f / 1=平衡 h / 2=流畅 q，见 UI quality 映射）。
/// x264 编码器按 kbps 配置。
// x264 软编 + 高熵合成源的 simulcast 层参数（e2e/CI 用）：
// 层间分辨率/码率梯度大（f 像素约 q 的 9 倍，单帧平均大小差 >8x），
// 但 f 层足够轻（960x540），关键帧突发不会超出 pacer 排程
//（太大的层在快机器上会被整帧丢弃，选层 e2e 偶发拿不到 f 层，#66）。
const SIMULCAST_LAYERS_X264: [(&str, u32, u32, u32); 3] = [
    ("q", 320, 180, 300),
    ("h", 640, 360, 700),
    ("f", 960, 540, 1200),
];
/// VideoToolbox 按 bps 配置。
const SIMULCAST_LAYERS_VT: [(&str, u32, u32, u32); 3] = [
    ("q", 640, 360, 800_000),
    ("h", 1280, 720, 2_500_000),
    ("f", 1920, 1080, 8_000_000),
];

/// 把同一时刻的若干层编码帧写入对应 rid（单层 rid=None 走普通发送）。
fn send_frame_layers(
    endpoint: &mut Endpoint,
    mid: str0m::media::Mid,
    rtp_time: str0m::media::MediaTime,
    frames: &[(Option<str0m::media::Rid>, Vec<u8>)],
) {
    for (rid, data) in frames {
        let res = match rid {
            Some(r) => endpoint.send_video_frame_rid(mid, *r, data.clone(), rtp_time),
            None => endpoint.send_video_frame(mid, data.clone(), rtp_time),
        };
        if let Err(e) = res {
            warn!("send layer {rid:?} failed: {e:?}");
        }
    }
}

/// 排空 str0m 的待打包帧队列：每个 `handle_timeout` 只处理一帧，
/// simulcast 每轮写入 `n` 帧（同一 mid），需连续 `n` 次才能避免队列积压。
fn drain_payload_queue(endpoint: &mut Endpoint, n: usize) {
    for _ in 0..n {
        let _ = endpoint.handle_timeout(Instant::now());
    }
}

/// #73 A/V 同步验收：合成源的帧间隔必须精确到纳秒。
/// `Duration::from_millis(1000 / fps)` 在 fps=30 时截断为 33ms → 实际
/// 30.3fps，视频时钟比音频快 ~1%，10 分钟累积漂移可达 ~6s（验收 <50ms 不达标）。
fn frame_interval(fps: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / fps.max(1) as u64)
}

/// #73 音频发送 codec：PCMU（8kHz 电话级）或 Opus（48kHz 高音质）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioCodec {
    Pcmu,
    Opus,
}

/// #58/#73 音频节拍器：合成正弦（440Hz）→ PCMU（8kHz）/ Opus（48kHz），20ms/帧。
struct AudioTicker {
    next: Instant,
    pts: u64,
    phase: u32,
    codec: AudioCodec,
    /// Opus 编码器（首次发帧时惰性创建；libopus 缺失时回退 PCMU）。
    opus: Option<aerodesk_ffmpeg::audio::OpusEncoder>,
}

impl AudioTicker {
    fn new(audio_opus: bool) -> Self {
        Self {
            next: Instant::now(),
            pts: 0,
            phase: 0,
            codec: if audio_opus {
                AudioCodec::Opus
            } else {
                AudioCodec::Pcmu
            },
            opus: None,
        }
    }

    /// 到点则补发若干 20ms 音频帧（PCMU 160 样本 / Opus 960 样本）。
    fn tick(&mut self, endpoint: &mut Endpoint, mid: str0m::media::Mid, now: Instant) {
        if self.next > now {
            return;
        }
        while self.next <= now {
            match self.codec {
                AudioCodec::Pcmu => {
                    let mut samples = [0i16; 160];
                    for s in &mut samples {
                        let t = self.phase as f64 / 8000.0;
                        *s = ((t * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16;
                        self.phase = self.phase.wrapping_add(1);
                    }
                    let data = aerodesk_core::pcmu::pcmu_encode(&samples);
                    let rtp_time = str0m::media::MediaTime::new(
                        self.pts * 160,
                        str0m::media::Frequency::EIGHT_KHZ,
                    );
                    if let Err(e) = endpoint.send_audio_frame(mid, data, rtp_time) {
                        warn!("send audio failed: {e:?}");
                    }
                }
                AudioCodec::Opus => {
                    // 惰性初始化：libopus 不可用（ffmpeg 未编译）时回退 PCMU。
                    if self.opus.is_none() {
                        match aerodesk_ffmpeg::audio::OpusEncoder::new(64_000) {
                            Ok(enc) => self.opus = Some(enc),
                            Err(err) => {
                                warn!("opus encoder init failed, fallback PCMU: {err}");
                                self.codec = AudioCodec::Pcmu;
                                self.phase = 0;
                                return;
                            }
                        }
                    }
                    let mut samples = [0i16; 960];
                    for s in &mut samples {
                        let t = self.phase as f64 / 48_000.0;
                        *s = ((t * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16;
                        self.phase = self.phase.wrapping_add(1);
                    }
                    let data = self
                        .opus
                        .as_mut()
                        .and_then(|enc| enc.encode(&samples).ok().flatten());
                    if let Some(data) = data {
                        let rtp_time = str0m::media::MediaTime::new(
                            self.pts * 960,
                            str0m::media::Frequency::FORTY_EIGHT_KHZ,
                        );
                        if let Err(e) = endpoint.send_audio_frame_opus(mid, data, rtp_time) {
                            warn!("send opus audio failed: {e:?}");
                        }
                    }
                }
            }
            self.pts += 1;
            self.next += Duration::from_millis(20);
        }
    }
}

/// #75 发送远程光标位置（cursor 通道，归一化 0..1）。
/// 当前墙钟（unix ms，#8 端到端延迟测量）。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// #109 控制端：打印命令/文件/进程结果（e2e/CLI 断言用）。
fn print_cmd_result(resp: &aerodesk_protocol::cmd::CmdResponse) {
    use aerodesk_protocol::cmd::CmdResult;
    match &resp.result {
        CmdResult::Run {
            exit_code,
            stdout,
            stderr,
            truncated,
            error,
        } => {
            info!(
                "CMD_RESULT: ok={} exit={:?} truncated={} error={:?}",
                error.is_none() && *exit_code == Some(0),
                exit_code,
                truncated,
                error
            );
            info!("CMD_STDOUT:\n{stdout}");
            info!("CMD_STDERR:\n{stderr}");
        }
        CmdResult::File { data, size, error } => {
            info!(
                "CMD_RESULT: ok={} type=file size={size} error={error:?}",
                error.is_none()
            );
            if let Some(b64) = data {
                if let Some(bytes) = aerodesk_protocol::cmd::decode_b64(b64) {
                    info!("CMD_FILE_CONTENT:\n{}", String::from_utf8_lossy(&bytes));
                }
            }
        }
        CmdResult::ProcessList { processes, error } => {
            info!(
                "CMD_RESULT: ok={} type=ps count={} error={error:?}",
                error.is_none(),
                processes.len()
            );
            for p in processes {
                info!("CMD_PROC: {} {}", p.pid, p.name);
            }
        }
        CmdResult::Killed { pid, error } => {
            info!(
                "CMD_RESULT: ok={} type=kill pid={pid} error={error:?}",
                error.is_none()
            );
        }
    }
}

/// #109 MCP 键鼠：字符 → (键码名, 是否需要 Shift)（US 布局）。
fn char_key(c: char) -> Option<(&'static str, bool)> {
    Some(match c {
        'a'..='z' => (
            match c {
                'a' => "KeyA",
                'b' => "KeyB",
                'c' => "KeyC",
                'd' => "KeyD",
                'e' => "KeyE",
                'f' => "KeyF",
                'g' => "KeyG",
                'h' => "KeyH",
                'i' => "KeyI",
                'j' => "KeyJ",
                'k' => "KeyK",
                'l' => "KeyL",
                'm' => "KeyM",
                'n' => "KeyN",
                'o' => "KeyO",
                'p' => "KeyP",
                'q' => "KeyQ",
                'r' => "KeyR",
                's' => "KeyS",
                't' => "KeyT",
                'u' => "KeyU",
                'v' => "KeyV",
                'w' => "KeyW",
                'x' => "KeyX",
                'y' => "KeyY",
                _ => "KeyZ",
            },
            false,
        ),
        'A'..='Z' => (
            match c {
                'A' => "KeyA",
                'B' => "KeyB",
                'C' => "KeyC",
                'D' => "KeyD",
                'E' => "KeyE",
                'F' => "KeyF",
                'G' => "KeyG",
                'H' => "KeyH",
                'I' => "KeyI",
                'J' => "KeyJ",
                'K' => "KeyK",
                'L' => "KeyL",
                'M' => "KeyM",
                'N' => "KeyN",
                'O' => "KeyO",
                'P' => "KeyP",
                'Q' => "KeyQ",
                'R' => "KeyR",
                'S' => "KeyS",
                'T' => "KeyT",
                'U' => "KeyU",
                'V' => "KeyV",
                'W' => "KeyW",
                'X' => "KeyX",
                'Y' => "KeyY",
                _ => "KeyZ",
            },
            true,
        ),
        '0'..='9' => (
            match c {
                '0' => "Digit0",
                '1' => "Digit1",
                '2' => "Digit2",
                '3' => "Digit3",
                '4' => "Digit4",
                '5' => "Digit5",
                '6' => "Digit6",
                '7' => "Digit7",
                '8' => "Digit8",
                _ => "Digit9",
            },
            false,
        ),
        ' ' => ("Space", false),
        '\n' => ("Enter", false),
        '\t' => ("Tab", false),
        '-' => ("Minus", false),
        '_' => ("Minus", true),
        '=' => ("Equal", false),
        '+' => ("Equal", true),
        '[' => ("BracketLeft", false),
        '{' => ("BracketLeft", true),
        ']' => ("BracketRight", false),
        '}' => ("BracketRight", true),
        '\\' => ("Backslash", false),
        '|' => ("Backslash", true),
        ';' => ("Semicolon", false),
        ':' => ("Semicolon", true),
        '\'' => ("Quote", false),
        '"' => ("Quote", true),
        '`' => ("Backquote", false),
        '~' => ("Backquote", true),
        ',' => ("Comma", false),
        '<' => ("Comma", true),
        '.' => ("Period", false),
        '>' => ("Period", true),
        '/' => ("Slash", false),
        '?' => ("Slash", true),
        '!' => ("Digit1", true),
        '@' => ("Digit2", true),
        '#' => ("Digit3", true),
        '$' => ("Digit4", true),
        '%' => ("Digit5", true),
        '^' => ("Digit6", true),
        '&' => ("Digit7", true),
        '*' => ("Digit8", true),
        '(' => ("Digit9", true),
        ')' => ("Digit0", true),
        _ => return None,
    })
}

/// #109 MCP 键鼠：文本 → 按键事件序列（每个字符 按下+抬起，必要时带 Shift）。
fn type_text_events(text: &str) -> Vec<InputEvent> {
    let mut events = Vec::new();
    for c in text.chars() {
        let Some((code, shift)) = char_key(c) else {
            tracing::warn!("type_text: 跳过不支持字符 {c:?}");
            continue;
        };
        let modifiers = Modifiers {
            ctrl: false,
            shift,
            alt: false,
            meta: false,
        };
        events.push(InputEvent::Key {
            code: code.to_string(),
            state: ButtonState::Pressed,
            modifiers: modifiers.clone(),
        });
        events.push(InputEvent::Key {
            code: code.to_string(),
            state: ButtonState::Released,
            modifiers,
        });
    }
    events
}

/// 发送远程光标（#75）。附带发送端墙钟，viewer 据此计算 one-way latency（#8）。
fn send_cursor(endpoint: &mut Endpoint, x: f64, y: f64) {
    let pos = aerodesk_protocol::cursor::CursorPos::new(x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
        .with_sent_ms(now_ms());
    if let Ok(json) = serde_json::to_string(&pos) {
        endpoint.send_channel_data("cursor", false, json.as_bytes());
    }
}

/// 发布端公共事件处理：输入通道（观看端 → 被控端）。
fn handle_publisher_input(endpoint: &mut Endpoint, ev: ClientEvent) {
    // #72 文件传输：file 通道事件交给状态机（非 file 事件为 no-op）。
    file_transfer::handle_event(&ev, endpoint);
    // #109 远程命令：cmd 通道请求交给执行器（后台线程执行，主循环回传响应）。
    cmd_exec::handle_event(&ev, endpoint);
    match ev {
        ClientEvent::ChannelOpen(label, _) if label == "input" => {
            info!("input channel open");
        }
        ClientEvent::ChannelData(cid, _, data) => {
            if endpoint.channel_label(cid).as_deref() == Some("input") {
                if let Ok(frame) = serde_json::from_slice::<InputFrame>(&data) {
                    info!("input: seq={} {:?}", frame.seq, frame.event);
                    // #72 剪贴板：viewer 发来的文本写入被控端剪贴板（macOS）。
                    match &frame.event {
                        InputEvent::ClipboardText(text) => {
                            info!(
                                "clipboard: apply {} chars from viewer",
                                text.chars().count()
                            );
                            clipboard::set_cache(text.clone());
                            clipboard::write(text);
                        }
                        _ => {
                            // #75：把 viewer 输入注入被控端（macOS CGEvent；无辅助功能
                            // 权限时静默失败，但路径与日志可验证）。
                            match aerodesk_macos::inject::inject(&frame.event) {
                                Ok(()) => info!("inject: seq={} {:?}", frame.seq, frame.event),
                                Err(e) => {
                                    info!("inject failed: seq={} {:?}: {e}", frame.seq, frame.event)
                                }
                            }
                        }
                    }
                }
            } else if endpoint.channel_label(cid).as_deref() == Some("control") {
                // #58 显示器切换：viewer → SFU → publisher（control 通道转发）。
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data)
                    && let Some(n) = v.get("display").and_then(|d| d.as_u64())
                {
                    info!("control: display switch request -> display {n}");
                }
            }
        }
        _ => {}
    }
}

/// 返回 Err=会话结束需重连（#173）。
fn publisher(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    audio: bool,
    audio_opus: bool,
) -> Result<(), String> {
    let pcap = include_bytes!("../../../crates/aerodesk-core/tests/data/vp8.pcap");
    let frames = parse_vp8_pcap(pcap);
    info!("loaded {} VP8 frames from pcap", frames.len());

    let (mut signal, mut endpoint, mut socket, video_mid, audio_mid) =
        connect(signal_url, room, Role::Publisher, auth, audio)?;
    let mut connected = false;
    let mut audio_ticker = AudioTicker::new(audio_opus);
    let mut frame_idx = 0usize;
    let mut last_frame_time = Instant::now();
    let mut next_deadline = Instant::now() + Duration::from_millis(100);
    // #75 合成远程光标（e2e 用）：30Hz 正弦轨迹，验证 cursor 通道端到端。
    let cursor_start = Instant::now();
    let mut last_cursor = Instant::now();

    loop {
        // #173 自动重连：ICE 失效时退出会话由主流程重连。
        if !endpoint.is_alive() {
            info!("publisher: ICE session ended, exiting session for reconnect");
            return Err("ICE session ended".into());
        }
        // UDP 输入
        let wait = next_deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        socket.set_read_timeout(Some(wait)).ok();
        let mut buf = [0u8; 2000];
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                debug!("recv {} bytes from {} type={:#04x}", n, source, buf[0]);
                let Ok(contents) = buf[..n].try_into() else {
                    continue;
                };
                let input = Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: socket.local_addr().unwrap(),
                        contents,
                    },
                );
                let _ = endpoint.handle_input(input);
            }
            Err(e) => {
                if e.kind() != std::io::ErrorKind::WouldBlock
                    && e.kind() != std::io::ErrorKind::TimedOut
                {
                    debug!("recv error: {e:?}");
                }
            }
        }
        let _ = endpoint.handle_timeout(Instant::now());

        // 输出
        let mut deadline = Instant::now() + Duration::from_secs(1);
        while let Some(output) = endpoint.poll_output() {
            match output {
                Output::Transmit(t) => {
                    debug!(
                        "tx {} bytes to {} type={:#04x}",
                        t.contents.len(),
                        t.destination,
                        t.contents.first().copied().unwrap_or(0)
                    );
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Timeout(t) => {
                    deadline = deadline.min(t);
                    break;
                }
                Output::Event(_) => {}
            }
        }
        next_deadline = deadline.min(next_deadline);

        // 客户端事件
        while let Some(ev) = endpoint.poll_event() {
            match ev {
                ClientEvent::IceConnected => {
                    info!("ICE connected, starting stream");
                    connected = true;
                    last_frame_time = Instant::now();
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return Err("connection closed".into());
                }
                ev => handle_publisher_input(&mut endpoint, ev),
            }
        }

        // #58 音频：按 20ms 节拍发送 PCMU 帧。
        if let Some(amid) = audio_mid {
            audio_ticker.tick(&mut endpoint, amid, Instant::now());
        }
        // #72 文件传输：推进发送。
        file_transfer::tick(&mut endpoint);
        cmd_exec::tick(&mut endpoint);

        // #75 远程光标（合成轨迹，30Hz）。
        if last_cursor.elapsed() >= Duration::from_millis(33) {
            last_cursor = Instant::now();
            let t = cursor_start.elapsed().as_secs_f64();
            send_cursor(&mut endpoint, 0.5 + 0.3 * t.sin(), 0.5 + 0.3 * t.cos());
        }

        // 发送帧（按 90kHz 时间戳节奏）
        if connected && frame_idx < frames.len() {
            let f: &Vp8Frame = &frames[frame_idx];
            let interval = if frame_idx > 0 {
                Duration::from_secs_f64(
                    (f.rtp_timestamp as i64 - frames[frame_idx - 1].rtp_timestamp as i64).max(0)
                        as f64
                        / 90_000.0,
                )
            } else {
                Duration::ZERO
            };
            if frame_idx == 0 || last_frame_time.elapsed() >= interval {
                let rtp_time = MediaTime::new(f.rtp_timestamp as u64, Frequency::NINETY_KHZ);
                if let Err(e) = endpoint.send_video_frame(video_mid, f.data.clone(), rtp_time) {
                    warn!("send frame failed: {e:?}");
                }
                if f.keyframe {
                    debug!("sent keyframe #{frame_idx}");
                }
                last_frame_time = Instant::now();
                frame_idx += 1;
                if frame_idx == frames.len() {
                    info!("stream finished ({} frames)", frames.len());
                }
            }
        }

        // #85 流控：pcap 发布端 ~2ms 节拍 → 单发 ~250-300 chunks/s 是
        // SFU/str0m DTLS 接收队列的稳定速率上界；过快（>600/s）会触发
        // SACK 突发导致 Receive queue full 断连（实测）。
        std::thread::sleep(Duration::from_millis(2));
        let _ = &mut signal;
    }
}

#[allow(clippy::too_many_arguments)] // #75 e2e 输入脚本开关；与既有 publisher 系列函数同风格。
/// 返回 Err=会话结束需重连（#173）；Ok=正常完成（一次性模式）。
fn viewer(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    layer: Option<&str>,
    audio: bool,
    mute_audio: bool,
    display: Option<usize>,
    input_script: bool,
    send_input: Option<&InputEvent>,
    type_text: Option<&str>,
    cmd_intent: Option<&cmd_exec::Intent>,
    cmd_json: bool,
    request_file: Option<&str>,
) -> Result<(), String> {
    let (mut signal, mut endpoint, mut socket, _, _audio_mid) =
        connect(signal_url, room, Role::Viewer, auth, audio)?;
    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut keyframes = 0u64;
    let mut audio_frames = 0u64;
    let mut audio_bytes = 0u64;
    let mut last_report = Instant::now();
    let mut input_open = false;
    let mut input_seq = 0u64;
    let mut last_input = Instant::now();
    let mut layer_sent = layer.is_none();
    // #58 观看端静音：--mute-audio 经 control 通道下发；静音后丢弃音频帧。
    let audio_muted = mute_audio;
    let mut mute_sent = false;
    let mut dropped_audio_frames = 0u64;
    // #73 A/V 同步：统一时间轴 + 漂移跟踪 + 音频 jitter buffer。
    let mut avsync = aerodesk_core::avsync::AvSync::new();
    let mut jitter = aerodesk_core::avsync::AudioJitterBuffer::new(0.08);
    let mut audio_played = 0u64;
    // #58 显示器切换：--display N 经 control 通道下发（SFU 转发给被控端）。
    // 未显式指定时不下发（避免每次连接都切到显示器 0）。
    let mut display_sent = display.is_none();
    // #75 远程光标：cursor 通道日志（节流 1s，e2e 断言用）。
    let mut last_cursor_log = Instant::now();
    // #8 端到端延迟：cursor 带发送时间戳，viewer 计算 one-way latency（节流 1s）。
    let mut last_latency_log = Instant::now();
    // #75/#109 单次输入（MCP 键鼠）：input 通道打开后发送一次/序列，500ms 后退出。
    let mut input_sent = send_input.is_none() && type_text.is_none();
    // #122 大文件下载：file 通道打开后发送 FileControl::Request，轮询 recv-dir 落盘后退出。
    let mut file_request_sent = false;
    let mut file_request_started: Option<Instant> = None;
    let mut input_exit_at: Option<Instant> = None;
    // #109 远程命令/文件/进程（控制端一次执行）：请求每 1s 重传直到响应（首包可能被
    // SFU 在通道未就绪时丢弃；被控端按 id 去重，重复执行安全）。
    let cmd_pending = cmd_intent.is_some();
    let mut cmd_done = false;
    let mut last_cmd_send = Instant::now() - Duration::from_secs(1);
    // #74 解码端验证：FFmpeg 软解全部 codec（H264/H265/VP9/AV1），codec-e2e 断言。
    let mut video_decoder: Option<aerodesk_ffmpeg::decode::FfmpegDecoder> = None;
    let mut decoded_frames: u64 = 0;
    // #73 Opus 音频：libopus 解码（惰性创建；不可用时降级为仅统计）。
    let mut opus_decoder: Option<aerodesk_ffmpeg::audio::OpusDecoder> = None;
    // #173 媒体静默检测：收到过包后连续无包超过阈值视为会话死亡（str0m is_alive
    // 在 recvonly 场景下不触发 ICE Failed，需主动探活）。
    let mut last_rx: Option<Instant> = None;
    const DEAD_AFTER_NO_MEDIA: Duration = Duration::from_secs(8);

    loop {
        // #173 自动重连：ICE 失效（SFU/网络断开）时退出会话，由主流程重连。
        if !endpoint.is_alive() {
            info!("viewer: ICE session ended, exiting session for reconnect");
            return Err("ICE session ended".into());
        }
        // #173 媒体静默检测：收到过媒体后连续无包视为会话死亡（recvonly 下 ICE Failed 不可靠）。
        if let Some(t) = last_rx
            && t.elapsed() > DEAD_AFTER_NO_MEDIA
        {
            info!("viewer: no media for {DEAD_AFTER_NO_MEDIA:?}, treating session as dead");
            return Err("no media (ICE dead)".into());
        }
        // #8 高码率吞吐：每轮尽量排空 socket（最多 512 包），有数据时不再
        // sleep 2ms——否则 4K/大帧流 ~5k pps 时每轮只读 1 包，内核缓冲溢出
        // 丢包，关键帧永远不完整（0 keyframes / 解码 0 帧）。
        let wait = Duration::from_millis(50);
        socket.set_read_timeout(Some(wait)).ok();
        let mut got_any = false;
        for _ in 0..512 {
            let mut buf = [0u8; 2000];
            match socket.recv_from(&mut buf) {
                Ok((n, source)) => {
                    got_any = true;
                    last_rx = Some(Instant::now());
                    debug!("recv {} bytes from {} type={:#04x}", n, source, buf[0]);
                    let Ok(contents) = buf[..n].try_into() else {
                        continue;
                    };
                    let input = Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: socket.local_addr().unwrap(),
                            contents,
                        },
                    );
                    let _ = endpoint.handle_input(input);
                }
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::WouldBlock
                        && e.kind() != std::io::ErrorKind::TimedOut
                    {
                        debug!("recv error: {e:?}");
                    }
                    break;
                }
            }
        }
        let _ = endpoint.handle_timeout(Instant::now());

        while let Some(output) = endpoint.poll_output() {
            match output {
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Timeout(_) => break,
                Output::Event(_) => {}
            }
        }

        while let Some(ev) = endpoint.poll_event() {
            // #72 文件传输：file 通道事件交给状态机。
            file_transfer::handle_event(&ev, &mut endpoint);
            match ev {
                ClientEvent::Media(data) => {
                    // #58/#73 音频识别：SFU 转发时 RTP mid 扩展用 SFU 本地 mid（与
                    // viewer 协商的 mid 不同，视频/音频都一样），不能按 mid 过滤；
                    // 用协商 codec（PCMU/Opus）识别音频帧。
                    if data.params.spec().codec == str0m::format::Codec::PCMU {
                        // 静音时丢弃（若接播放设备则无声）。
                        if audio_muted {
                            dropped_audio_frames += 1;
                        } else {
                            audio_frames += 1;
                            audio_bytes += data.data.len() as u64;
                            // #73：解码 → 入 jitter buffer（目标延迟 80ms），
                            // 以音频时间轴为 now 弹出（模拟播放时钟）。
                            let pcm = aerodesk_core::pcmu::pcmu_decode(&data.data);
                            avsync.on_audio(data.time.numer(), data.time.denom());
                            jitter.push(avsync.audio_time_secs(), pcm);
                            while let Some(_f) = jitter.pop(avsync.audio_time_secs()) {
                                audio_played += 1;
                            }
                        }
                    } else if data.params.spec().codec == str0m::format::Codec::Opus {
                        // #73 Opus（48kHz）：解码 → 同一 jitter buffer/时间轴。
                        if audio_muted {
                            dropped_audio_frames += 1;
                        } else {
                            audio_frames += 1;
                            audio_bytes += data.data.len() as u64;
                            if opus_decoder.is_none() {
                                opus_decoder = aerodesk_ffmpeg::audio::OpusDecoder::new().ok();
                            }
                            let pcm = opus_decoder
                                .as_mut()
                                .and_then(|dec| dec.decode(&data.data).ok().flatten())
                                .unwrap_or_default();
                            avsync.on_audio(data.time.numer(), data.time.denom());
                            jitter.push(avsync.audio_time_secs(), pcm);
                            while let Some(_f) = jitter.pop(avsync.audio_time_secs()) {
                                audio_played += 1;
                            }
                        }
                    } else {
                        frames += 1;
                        bytes += data.data.len() as u64;
                        avsync.on_video(data.time.numer(), data.time.denom());
                        if data.is_keyframe() {
                            keyframes += 1;
                        }
                        // #74 解码端：FFmpeg 软解 H264/H265/VP9/AV1 → RGBA。
                        let core_codec = match data.params.spec().codec {
                            str0m::format::Codec::H264 => Some(Codec::H264),
                            str0m::format::Codec::H265 => Some(Codec::Hevc),
                            str0m::format::Codec::Vp9 => Some(Codec::Vp9),
                            str0m::format::Codec::Av1 => Some(Codec::Av1),
                            _ => None,
                        };
                        if let Some(cc) = core_codec {
                            if video_decoder
                                .as_ref()
                                .map(|d| d.codec() != cc)
                                .unwrap_or(true)
                            {
                                video_decoder =
                                    aerodesk_ffmpeg::decode::FfmpegDecoder::new(cc).ok();
                            }
                            if let Some(dec) = &mut video_decoder {
                                let unit = aerodesk_core::media_pipeline::EncodedUnit {
                                    data: data.data.as_ref().to_vec(),
                                    keyframe: data.is_keyframe(),
                                    pts_ms: 0,
                                    rtp_timestamp: 0,
                                };
                                if let Ok(Some(frame)) = dec.decode_unit(&unit)
                                    && frame.raw.is_some()
                                {
                                    decoded_frames += 1;
                                }
                            }
                        }
                    }
                }
                ClientEvent::IceConnected => info!("ICE connected"),
                ClientEvent::ChannelOpen(label, _) if label == "file" => {
                    // #122：请求被控端发送指定文件（配合 --recv-dir 落盘）。
                    if !file_request_sent && request_file.is_some() {
                        let req = aerodesk_protocol::file::FileControl::Request {
                            path: request_file.unwrap().to_string(),
                        };
                        if let Ok(json) = serde_json::to_string(&req)
                            && endpoint.send_channel_data("file", false, json.as_bytes())
                        {
                            file_request_sent = true;
                            file_request_started = Some(Instant::now());
                            info!("file request sent: {}", request_file.unwrap());
                        }
                    }
                }
                ClientEvent::ChannelOpen(label, _) if label == "input" || label == "control" => {
                    if label == "input" {
                        info!("input channel open");
                        input_open = true;
                    }
                    // #29：可选显式选层（--layer q|h|f），经 control 通道发 SFU。
                    // #66：input 与 control 打开顺序不定——只在 input 打开时发一次，
                    // 若 control 尚未就绪会静默丢失选层请求；两个通道任一打开都重试。
                    if !layer_sent {
                        let req = serde_json::json!({ "layer": layer });
                        let data = serde_json::to_vec(&req).unwrap();
                        if endpoint.send_channel_data("control", false, &data) {
                            info!("layer request sent: {layer:?}");
                            layer_sent = true;
                        }
                    }
                    // #58 观看端静音：经 control 通道下发一次。
                    if mute_audio && !mute_sent {
                        let req = serde_json::json!({ "audio_mute": true });
                        let data = serde_json::to_vec(&req).unwrap();
                        if endpoint.send_channel_data("control", false, &data) {
                            info!("audio mute command sent");
                            mute_sent = true;
                        }
                    }
                    // #58 显示器切换：经 control 通道下发一次（SFU 转发给被控端）。
                    if let Some(d) = display
                        && !display_sent
                    {
                        let req = serde_json::json!({ "display": d });
                        let data = serde_json::to_vec(&req).unwrap();
                        if endpoint.send_channel_data("control", false, &data) {
                            info!("display switch command sent: {d}");
                            display_sent = true;
                        }
                    }
                }
                // #109 远程命令：响应打印 stdout/stderr/exit code 后退出（exit code 语义）。
                ClientEvent::ChannelData(cid, _, data)
                    if endpoint.channel_label(cid).as_deref() == Some("cmd") =>
                {
                    if !cmd_done {
                        cmd_done = true;
                        if let Some(resp) = cmd_exec::handle_response(&data) {
                            if cmd_json {
                                // MCP 桥接：stdout 输出纯 JSON。
                                println!(
                                    "{}",
                                    serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into())
                                );
                            } else {
                                print_cmd_result(&resp);
                            }
                            std::process::exit(if resp.result.ok() { 0 } else { 1 });
                        }
                    }
                }
                // #75 远程光标：被控端广播位置，观看端日志（节流）。
                ClientEvent::ChannelData(cid, _, data)
                    if endpoint.channel_label(cid).as_deref() == Some("cursor") =>
                {
                    if let Ok(pos) =
                        serde_json::from_slice::<aerodesk_protocol::cursor::CursorPos>(&data)
                    {
                        if last_cursor_log.elapsed() >= Duration::from_secs(1) {
                            info!("CURSOR: x={:.3} y={:.3}", pos.x, pos.y);
                            last_cursor_log = Instant::now();
                        }
                        // #8 端到端延迟：本地墙钟 - 发送墙钟（同机/同一时钟域有效）。
                        let now = now_ms();
                        if pos.sent_ms > 0
                            && now >= pos.sent_ms
                            && last_latency_log.elapsed() >= Duration::from_secs(1)
                        {
                            info!("LATENCY: {} ms", now - pos.sent_ms);
                            last_latency_log = Instant::now();
                        }
                    }
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return Err("connection closed".into());
                }
                _ => {}
            }
        }

        // #72 文件传输：推进发送 + 剪贴板轮询/落地。
        file_transfer::tick(&mut endpoint);
        // #109 远程命令：未收到响应前每 1s 重传请求（首包丢失自愈）。
        if cmd_pending && !cmd_done && last_cmd_send.elapsed() >= Duration::from_secs(1) {
            if let Some(intent) = cmd_intent {
                let sent = cmd_exec::send_intent(&mut endpoint, intent);
                info!("cmd request sent (retry={sent}): {intent:?}");
                last_cmd_send = Instant::now();
            }
        }
        cmd_exec::tick(&mut endpoint);

        // #75/#109 单次输入（MCP 键鼠）：input 通道打开后发送事件（单事件或逐字符按键序列）。
        if input_open && !input_sent {
            let events: Vec<InputEvent> = if let Some(ev) = send_input {
                vec![ev.clone()]
            } else if let Some(text) = type_text {
                type_text_events(text)
            } else {
                Vec::new()
            };
            if !events.is_empty() {
                let mut sent = 0usize;
                for (i, ev) in events.iter().enumerate() {
                    let frame = InputFrame {
                        version: INPUT_PROTOCOL_VERSION,
                        seq: i as u64 + 1,
                        timestamp_ms: now_ms(),
                        event: ev.clone(),
                    };
                    if let Ok(json) = serde_json::to_string(&frame)
                        && endpoint.send_channel_data("input", false, json.as_bytes())
                    {
                        sent += 1;
                    }
                }
                if sent > 0 {
                    input_sent = true;
                    input_exit_at = Some(Instant::now() + Duration::from_millis(500));
                    info!("input sent: {sent} events");
                }
            }
        }
        // 输入事件回传：input 通道打开后周期性发送鼠标移动（模拟观看端输入）。
        // #75 --input-script：脚本化轮换发送全部事件类型（MouseMove/Button/Wheel/
        // Key+修饰键），供 e2e 断言各事件类型均到达被控端注入路径。
        if input_open && last_input.elapsed() >= Duration::from_millis(100) {
            let event = if input_script {
                match input_seq % 6 {
                    0 => InputEvent::MouseMove { x: 0.3, y: 0.4 },
                    1 => InputEvent::MouseButton {
                        button: MouseButton::Left,
                        state: ButtonState::Pressed,
                        x: 0.5,
                        y: 0.5,
                    },
                    2 => InputEvent::MouseButton {
                        button: MouseButton::Left,
                        state: ButtonState::Released,
                        x: 0.5,
                        y: 0.5,
                    },
                    3 => InputEvent::Wheel {
                        x: 0.5,
                        y: 0.5,
                        delta_x: 0.0,
                        delta_y: -3.0,
                    },
                    4 => InputEvent::Key {
                        code: "KeyA".into(),
                        state: ButtonState::Pressed,
                        modifiers: Modifiers {
                            ctrl: true,
                            shift: true,
                            ..Default::default()
                        },
                    },
                    _ => InputEvent::Key {
                        code: "KeyA".into(),
                        state: ButtonState::Released,
                        modifiers: Modifiers {
                            ctrl: true,
                            shift: true,
                            ..Default::default()
                        },
                    },
                }
            } else {
                InputEvent::MouseMove { x: 0.5, y: 0.5 }
            };
            let frame = InputFrame {
                version: INPUT_PROTOCOL_VERSION,
                seq: input_seq,
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                event,
            };
            if let Ok(json) = serde_json::to_string(&frame)
                && endpoint.send_channel_data("input", false, json.as_bytes())
            {
                input_seq += 1;
                last_input = Instant::now();
            }
        }

        if last_report.elapsed() >= Duration::from_secs(2) {
            let audio_ms = avsync.audio_time_secs() * 1000.0;
            let video_ms = avsync.video_time_secs() * 1000.0;
            let drift_ms = avsync.drift_ms();
            let buffered = jitter.buffered();
            let jdropped = jitter.dropped();
            info!(
                "RECEIVED: {frames} frames, {bytes} bytes, {keyframes} keyframes, DECODED: {decoded_frames}, input sent: {input_seq}, AUDIO: {audio_frames} frames {audio_bytes} bytes muted={audio_muted} dropped={dropped_audio_frames} AVSYNC: audio={audio_ms:.0}ms video={video_ms:.0}ms drift={drift_ms:.0}ms buffered={buffered} dropped={jdropped} played={audio_played}"
            );
            last_report = Instant::now();
        }
        if !got_any {
            std::thread::sleep(Duration::from_millis(2));
        }
        // #75/#109 单次输入：发送后短暂等待即退出（CLI/MCP 桥接语义）。
        if let Some(t) = input_exit_at
            && Instant::now() >= t
        {
            std::process::exit(0);
        }
        // #122 大文件下载：请求后轮询 recv-dir 出现文件 → 退出；超时 120s 失败。
        if file_request_sent {
            if let Some(rd) = file_transfer::recv_dir() {
                let has_file = std::fs::read_dir(&rd)
                    .map(|mut it| {
                        it.any(|e| {
                            e.map(|f| f.metadata().map(|m| m.len() > 0).unwrap_or(false))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if has_file {
                    info!("file request received; exiting");
                    std::process::exit(0);
                }
            }
            if let Some(start) = file_request_started
                && start.elapsed() >= Duration::from_secs(240)
            {
                eprintln!("file request timeout");
                std::process::exit(1);
            }
        }
        // #122 大文件上传：viewer --send-file 发送被确认后退出。
        if file_transfer::send_confirmed() {
            info!("file upload confirmed; exiting");
            std::process::exit(0);
        }
        let _ = &mut signal;
    }
}

/// x264 发布端：合成帧 → H.264 编码 → SFU。
/// `--simulcast` 时编码 q/h/f 三层（640x360 / 1280x720 / 1920x1080），
/// SFU 选层请求（画质档位）才能真正切换分辨率/码率。

/// #211：网络泵排空式读取——单包读取在软编/高负载下饿死 SCTP ACK（input 送达率塌陷）。
/// 与 pcap publisher（#8 高码率吞吐）一致：每轮尽量排空 socket（最多 `max_packets` 包）。
fn drain_udp_input(socket: &mut MediaSocket, endpoint: &mut Endpoint, max_packets: usize) {
    let mut buf = [0u8; 2000];
    for _ in 0..max_packets {
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                let Ok(contents) = buf[..n].try_into() else {
                    continue;
                };
                let input = str0m::Input::Receive(
                    Instant::now(),
                    str0m::net::Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: socket.local_addr().unwrap(),
                        contents,
                    },
                );
                let _ = endpoint.handle_input(input);
            }
            Err(e) => {
                if e.kind() != std::io::ErrorKind::WouldBlock
                    && e.kind() != std::io::ErrorKind::TimedOut
                {
                    debug!("recv error: {e:?}");
                }
                break;
            }
        }
    }
}

fn publisher_x264(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    simulcast: bool,
    noisy: bool,
    audio: bool,
    audio_opus: bool,
) {
    use aerodesk_macos::synthetic::SyntheticSource;
    use aerodesk_softenc::encode::X264Encoder;
    use str0m::media::Rid;

    const FPS: u32 = 30;
    const W: u32 = 640;
    const H: u32 = 360;

    let (mut signal, mut endpoint, mut socket, video_mid, audio_mid) =
        connect_h264(signal_url, room, Role::Publisher, auth, simulcast, audio).expect("connect");

    let make_source = |w: u32, h: u32| {
        if noisy {
            SyntheticSource::new_noisy(w, h)
        } else {
            SyntheticSource::new(w, h)
        }
    };

    // (rid, encoder, source)：单层 rid=None；simulcast 为 q/h/f 三层。
    let mut layers: Vec<(Option<Rid>, X264Encoder, SyntheticSource)> = if simulcast {
        SIMULCAST_LAYERS_X264
            .iter()
            .map(|(rid, w, h, kbps)| {
                (
                    Some(Rid::from(*rid)),
                    X264Encoder::new(*w, *h, FPS, *kbps).expect("x264 encoder"),
                    make_source(*w, *h),
                )
            })
            .collect()
    } else {
        vec![(
            None,
            X264Encoder::new(W, H, FPS, 800).expect("x264 encoder"),
            make_source(W, H),
        )]
    };

    let mut connected = false;
    let mut audio_ticker = AudioTicker::new(audio_opus);
    let mut next_frame = Instant::now();
    let mut pts = 0i64;

    loop {
        // #211：排空式读取（软编/高负载下保证 SCTP ACK 及时消费，input 送达率不塌陷）。
        let wait = Duration::from_millis(5);
        socket.set_read_timeout(Some(wait)).ok();
        drain_udp_input(&mut socket, &mut endpoint, 512);
        let _ = endpoint.handle_timeout(Instant::now());

        let mut deadline = Instant::now() + Duration::from_secs(1);
        while let Some(output) = endpoint.poll_output() {
            match output {
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Timeout(t) => {
                    deadline = deadline.min(t);
                    break;
                }
                Output::Event(_) => {}
            }
        }

        while let Some(ev) = endpoint.poll_event() {
            match ev {
                ClientEvent::IceConnected => {
                    info!("ICE connected, starting x264 stream (simulcast={simulcast})");
                    connected = true;
                    next_frame = Instant::now();
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return;
                }
                // #66：响应 SFU 关键帧请求（新观看端加入/选层切换时），
                // 强制对应层下一帧 IDR，避免观看端等几十秒才起流。
                ClientEvent::KeyframeRequest(req) => {
                    let mut forced = 0;
                    for (rid, encoder, _) in &mut layers {
                        if req.rid.is_none() || *rid == Some(req.rid.unwrap()) {
                            encoder.force_idr();
                            forced += 1;
                        }
                    }
                    info!(
                        "keyframe request rid={:?}: forcing IDR on {forced} layer(s)",
                        req.rid
                    );
                }
                ev => handle_publisher_input(&mut endpoint, ev),
            }
        }

        // #58 音频：按 20ms 节拍发送 PCMU 帧。
        if let Some(amid) = audio_mid {
            audio_ticker.tick(&mut endpoint, amid, Instant::now());
        }
        // #72 文件传输：推进发送。
        file_transfer::tick(&mut endpoint);
        cmd_exec::tick(&mut endpoint);

        // 30fps 节奏编码发送（simulcast 各层同一 rtp_time，SFU 按 rid 选层）
        if connected && Instant::now() >= next_frame {
            next_frame += frame_interval(FPS);
            let rtp_time = str0m::media::MediaTime::new(
                pts as u64 * 3000,
                str0m::media::Frequency::NINETY_KHZ,
            );
            let mut frames = Vec::with_capacity(layers.len());
            for (rid, encoder, source) in &mut layers {
                let rgb = source.next_frame();
                if let Some(frame) = encoder.encode(rgb).expect("encode") {
                    if frame.keyframe {
                        info!("sent keyframe rid={rid:?} #{pts}");
                    }
                    frames.push((*rid, frame.data));
                }
            }
            send_frame_layers(&mut endpoint, video_mid, rtp_time, &frames);
            // simulcast：每层一帧都需一次 do_payload，多排空避免 WriteWithoutPoll 背压。
            drain_payload_queue(&mut endpoint, layers.len());

            pts += 1;
        }

        std::thread::sleep(Duration::from_millis(2));
        let _ = &mut signal;
    }
}
/// VideoToolbox 硬编发布端：合成 BGRA → 硬编 → SFU。
/// 压测可传 --width/--height/--fps/--bitrate（如 3840x2160@60 8Mbps）。
/// `--simulcast` 时编码 q/h/f 三层（SFU 选层生效）。
fn publisher_vt(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    params: VideoParams,
    simulcast: bool,
    noisy: bool,
    audio: bool,
    audio_opus: bool,
) {
    let VideoParams {
        width,
        height,
        fps,
        bitrate,
    } = params;
    use aerodesk_macos::synthetic::SyntheticSource;
    use aerodesk_macos::vt_encoder::VtEncoder;
    use str0m::media::Rid;

    let (mut signal, mut endpoint, mut socket, video_mid, audio_mid) =
        connect_h264(signal_url, room, Role::Publisher, auth, simulcast, audio).expect("connect");

    let make_source = |w: u32, h: u32| {
        if noisy {
            SyntheticSource::new_noisy(w, h)
        } else {
            SyntheticSource::new(w, h)
        }
    };

    // (rid, encoder, source)
    let mut layers: Vec<(Option<Rid>, VtEncoder, SyntheticSource)> = if simulcast {
        SIMULCAST_LAYERS_VT
            .iter()
            .map(|(rid, w, h, bps)| {
                (
                    Some(Rid::from(*rid)),
                    VtEncoder::new(*w, *h, fps, *bps).expect("vt encoder"),
                    make_source(*w, *h),
                )
            })
            .collect()
    } else {
        vec![(
            None,
            VtEncoder::new(width, height, fps, bitrate).expect("vt encoder"),
            make_source(width, height),
        )]
    };
    info!(
        "VT publisher: {} layer(s), top {width}x{height}@{fps} {bitrate}bps",
        layers.len()
    );
    let mut connected = false;
    let mut audio_ticker = AudioTicker::new(audio_opus);
    let mut next_frame = Instant::now();
    let mut pts = 0i64;
    let pts_inc = 90_000 / fps.max(1) as i64;
    // #8 端到端延迟：合成光标轨迹（30Hz，随 cursor 通道带发送时间戳）。
    let cursor_start = Instant::now();
    let mut last_cursor = Instant::now();

    loop {
        // #211：排空式读取（软编/高负载下保证 SCTP ACK 及时消费，input 送达率不塌陷）。
        let wait = Duration::from_millis(5);
        socket.set_read_timeout(Some(wait)).ok();
        drain_udp_input(&mut socket, &mut endpoint, 512);
        let _ = endpoint.handle_timeout(Instant::now());

        let mut deadline = Instant::now() + Duration::from_secs(1);
        while let Some(output) = endpoint.poll_output() {
            match output {
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Timeout(t) => {
                    deadline = deadline.min(t);
                    break;
                }
                Output::Event(_) => {}
            }
        }

        while let Some(ev) = endpoint.poll_event() {
            match ev {
                ClientEvent::IceConnected => {
                    info!("ICE connected, starting VideoToolbox stream (simulcast={simulcast})");
                    connected = true;
                    next_frame = Instant::now();
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return;
                }
                ev => handle_publisher_input(&mut endpoint, ev),
            }
        }

        // #58 音频：按 20ms 节拍发送 PCMU 帧。
        if let Some(amid) = audio_mid {
            audio_ticker.tick(&mut endpoint, amid, Instant::now());
        }
        // #72 文件传输：推进发送。
        file_transfer::tick(&mut endpoint);
        cmd_exec::tick(&mut endpoint);
        // #8 端到端延迟：合成光标轨迹（30Hz）。
        if last_cursor.elapsed() >= Duration::from_millis(33) {
            last_cursor = Instant::now();
            let t = cursor_start.elapsed().as_secs_f64();
            send_cursor(&mut endpoint, 0.5 + 0.3 * t.sin(), 0.5 + 0.3 * t.cos());
        }

        if connected && Instant::now() >= next_frame {
            next_frame += frame_interval(fps);
            let rtp_time = str0m::media::MediaTime::new(
                pts as u64 * pts_inc as u64,
                str0m::media::Frequency::NINETY_KHZ,
            );
            let mut frames = Vec::with_capacity(layers.len());
            for (rid, encoder, source) in &mut layers {
                let bgra = source.next_frame_bgra();
                match encoder.encode_bgra(bgra) {
                    Ok(Some(frame)) => {
                        let annexb = encoder.to_annexb(&frame);
                        frames.push((*rid, annexb));
                    }
                    Ok(None) => {}
                    Err(e) => warn!("vt encode: {e}"),
                }
            }
            send_frame_layers(&mut endpoint, video_mid, rtp_time, &frames);
            // simulcast：每层一帧都需一次 do_payload，多排空避免 WriteWithoutPoll 背压。
            drain_payload_queue(&mut endpoint, layers.len());
            pts += 1;
        }

        std::thread::sleep(Duration::from_millis(2));
        let _ = &mut signal;
    }
}
/// FFmpeg 发布端（#74）：合成 RGB → FfmpegEncoder（H264/H265/VP9/AV1）→ SFU。
/// `--codec h264|h265|vp9|av1` 选择编码格式；AV1(SVT) 有 ~1s 编码延迟。
fn publisher_ffmpeg(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    audio: bool,
    audio_opus: bool,
    codec: Codec,
    noisy: bool,
) {
    use aerodesk_macos::synthetic::SyntheticSource;

    const W: u32 = 640;
    const H: u32 = 360;
    const FPS: u32 = 30;

    let (mut signal, mut endpoint, mut socket, video_mid, audio_mid) =
        connect_codec(signal_url, room, Role::Publisher, auth, audio, codec).expect("connect");
    let mut encoder = FfmpegEncoder::new(W, H, FPS, 1_500_000, codec).expect("ffmpeg encoder");
    // #8：--noisy 高熵合成源（码率贴近目标档位，压测/高码率回归用）。
    let mut source = if noisy {
        SyntheticSource::new_noisy(W, H)
    } else {
        SyntheticSource::new(W, H)
    };
    let mut connected = false;
    let mut audio_ticker = AudioTicker::new(audio_opus);
    let mut next_frame = Instant::now();
    let mut pts = 0i64;
    // #8 端到端延迟：合成光标轨迹（30Hz）。
    let cursor_start = Instant::now();
    let mut last_cursor = Instant::now();

    loop {
        // #211：排空式读取（软编/高负载下保证 SCTP ACK 及时消费，input 送达率不塌陷）。
        let wait = Duration::from_millis(5);
        socket.set_read_timeout(Some(wait)).ok();
        drain_udp_input(&mut socket, &mut endpoint, 512);
        let _ = endpoint.handle_timeout(Instant::now());

        let mut deadline = Instant::now() + Duration::from_secs(1);
        while let Some(output) = endpoint.poll_output() {
            match output {
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Timeout(t) => {
                    deadline = deadline.min(t);
                    break;
                }
                Output::Event(_) => {}
            }
        }

        while let Some(ev) = endpoint.poll_event() {
            match ev {
                ClientEvent::IceConnected => {
                    info!("ICE connected, starting ffmpeg stream (codec={codec:?})");
                    connected = true;
                    next_frame = Instant::now();
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return;
                }
                // #74：响应 SFU 关键帧请求。
                ClientEvent::KeyframeRequest(_) => {
                    encoder.request_keyframe();
                }
                ev => handle_publisher_input(&mut endpoint, ev),
            }
        }

        // #58 音频 + #72 文件传输推进。
        if let Some(amid) = audio_mid {
            audio_ticker.tick(&mut endpoint, amid, Instant::now());
        }
        file_transfer::tick(&mut endpoint);
        cmd_exec::tick(&mut endpoint);
        // #8 端到端延迟：合成光标轨迹（30Hz）。
        if last_cursor.elapsed() >= Duration::from_millis(33) {
            last_cursor = Instant::now();
            let t = cursor_start.elapsed().as_secs_f64();
            send_cursor(&mut endpoint, 0.5 + 0.3 * t.sin(), 0.5 + 0.3 * t.cos());
        }

        if connected && Instant::now() >= next_frame {
            next_frame += frame_interval(FPS);
            let rgb = source.next_frame();
            if let Some(unit) = encoder.encode_rgb(rgb).expect("encode") {
                let rtp_time = str0m::media::MediaTime::new(
                    pts as u64 * 3000,
                    str0m::media::Frequency::NINETY_KHZ,
                );
                if let Err(e) = endpoint.send_video_frame(video_mid, unit.data, rtp_time) {
                    warn!("send frame failed: {e:?}");
                }
                if unit.keyframe {
                    debug!("sent keyframe #{pts}");
                }
            }
            pts += 1;
        }

        std::thread::sleep(Duration::from_millis(2));
        let _ = &mut signal;
    }
}

/// 真实屏幕采集发布端：ScreenCaptureKit → VideoToolbox 硬编（零拷贝）→ SFU。
/// 需要屏幕录制权限（TCC）。`--simulcast`：q/h/f 三层各一路 SCK 采集 + 硬编，
/// 分辨率越低开销越小，选层切换立即生效。
/// 屏幕采集 + FFmpeg 多 codec（#74）：ScreenCaptureKit → IOSurface → BGRA →
/// FfmpegEncoder（H265/VP9/AV1）。H.264 走原 VtEncoder 零拷贝路径。
/// 需要屏幕录制权限（TCC）。
fn publisher_capture_ffmpeg(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    audio: bool,
    audio_opus: bool,
    codec: Codec,
    initial_display: usize,
) {
    use aerodesk_ffmpeg::encode::FfmpegEncoder;
    use aerodesk_macos::capture::ScreenCapture;

    const W: u32 = 1920;
    const H: u32 = 1080;
    const FPS: u32 = 30;

    let (mut signal, mut endpoint, mut socket, video_mid, audio_mid) =
        connect_codec(signal_url, room, Role::Publisher, auth, audio, codec).expect("connect");
    let mut capture = match ScreenCapture::start(initial_display, FPS, W, H) {
        Ok(c) => c,
        Err(e) => {
            error!("screen capture init failed: {e}");
            info!("grant Screen Recording permission in System Settings > Privacy & Security");
            return;
        }
    };
    // #75：输入注入坐标按被控显示器（不总是主屏）换算。
    aerodesk_macos::inject::set_active_display(Some(capture.display_id()));
    let mut encoder = FfmpegEncoder::new(W, H, FPS, 8_000_000, codec).expect("ffmpeg encoder");
    let mut connected = false;
    let mut audio_ticker = AudioTicker::new(audio_opus);
    let mut pts = 0i64;
    // #75 远程光标：真实光标位置（30Hz）。
    let mut last_cursor = Instant::now();

    loop {
        // #211：排空式读取（软编/高负载下保证 SCTP ACK 及时消费，input 送达率不塌陷）。
        let wait = Duration::from_millis(5);
        socket.set_read_timeout(Some(wait)).ok();
        drain_udp_input(&mut socket, &mut endpoint, 512);
        let _ = endpoint.handle_timeout(Instant::now());

        let mut deadline = Instant::now() + Duration::from_secs(1);
        while let Some(output) = endpoint.poll_output() {
            match output {
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Timeout(t) => {
                    deadline = deadline.min(t);
                    break;
                }
                Output::Event(_) => {}
            }
        }

        while let Some(ev) = endpoint.poll_event() {
            match ev {
                ClientEvent::IceConnected => {
                    info!("ICE connected, starting screen+ffmpeg stream (codec={codec:?})");
                    connected = true;
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return;
                }
                ClientEvent::KeyframeRequest(_) => {
                    encoder.request_keyframe();
                }
                ev => handle_publisher_input(&mut endpoint, ev),
            }
        }

        if let Some(amid) = audio_mid {
            audio_ticker.tick(&mut endpoint, amid, Instant::now());
        }
        file_transfer::tick(&mut endpoint);
        cmd_exec::tick(&mut endpoint);
        // #75 远程光标：读取被控端真实光标位置（30Hz）。
        if last_cursor.elapsed() >= Duration::from_millis(33) {
            last_cursor = Instant::now();
            #[cfg(target_os = "macos")]
            if let Some((x, y)) = aerodesk_macos::cursor::cursor_position_normalized() {
                send_cursor(&mut endpoint, x, y);
            }
        }

        if connected && let Some(surface) = capture.next_frame(Duration::from_millis(50)) {
            // IOSurface（BGRA）→ 行复制到 CPU 缓冲 → FFmpeg 编码。
            let bgra = match aerodesk_macos::capture::surface_to_bgra(&surface, W, H) {
                Ok(b) => b,
                Err(e) => {
                    warn!("surface read failed: {e}");
                    continue;
                }
            };
            if let Some(unit) = encoder.encode_bgra(&bgra).expect("encode_bgra") {
                let rtp_time = str0m::media::MediaTime::new(
                    pts as u64 * 3000,
                    str0m::media::Frequency::NINETY_KHZ,
                );
                if let Err(e) = endpoint.send_video_frame(video_mid, unit.data, rtp_time) {
                    warn!("send frame failed: {e:?}");
                }
            }
            pts += 1;
        }

        std::thread::sleep(Duration::from_millis(2));
        let _ = &mut signal;
    }
}

fn publisher_capture(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    simulcast: bool,
    audio: bool,
    audio_opus: bool,
    initial_display: usize,
    codec: Codec,
) {
    use aerodesk_macos::capture::ScreenCapture;
    use aerodesk_macos::vt_encoder::VtEncoder;
    use str0m::media::Rid;

    const FPS: u32 = 30;
    const W: u32 = 1920;
    const H: u32 = 1080;
    // core Codec -> videotoolbox Codec（仅 H264/HEVC 走此路径）。
    use videotoolbox::Codec as VtCodec;
    let vt_codec = match codec {
        Codec::Hevc => VtCodec::HEVC,
        _ => VtCodec::H264,
    };

    let (mut signal, mut endpoint, mut socket, video_mid, audio_mid) = connect_inner(
        signal_url,
        room,
        Role::Publisher,
        Some(codec),
        simulcast,
        audio,
        auth,
    )
    .expect("connect");

    // #75 远程光标：真实光标位置（30Hz）。
    let mut last_cursor = Instant::now();

    // #58 显示器切换：按 (display, w, h) 重建采集器（编码器与显示器无关，保留）。
    let rebuild_captures = |idx: usize,
                            layers: &mut Vec<(Option<Rid>, VtEncoder, ScreenCapture)>|
     -> Result<(), String> {
        let specs: Vec<(Option<Rid>, u32, u32)> = layers
            .iter()
            .map(|(rid, _enc, cap)| (*rid, cap.width(), cap.height()))
            .collect();
        layers.clear();
        for (rid, w, h) in specs {
            let capture = ScreenCapture::start(idx, FPS, w, h)
                .map_err(|e| format!("display {idx} init failed: {e}"))?;
            // 重建：编码器按分辨率新建（与 display 无关，但保持同样参数）。
            let bps = if w >= 1280 { 8_000_000 } else { 4_000_000 };
            layers.push((
                rid,
                VtEncoder::new_with_codec(w, h, FPS, bps, vt_codec).expect("vt encoder"),
                capture,
            ));
        }
        // #75：切换显示器后输入注入坐标基准同步。
        if let Some((_, _, cap)) = layers.first() {
            aerodesk_macos::inject::set_active_display(Some(cap.display_id()));
        }
        info!("screen capture switched to display {idx}");
        Ok(())
    };

    // (rid, encoder, capture)
    let mut layers: Vec<(Option<Rid>, VtEncoder, ScreenCapture)> = Vec::new();
    let init_capture = |display: usize, w: u32, h: u32| -> Result<ScreenCapture, String> {
        ScreenCapture::start(display, FPS, w, h)
            .map_err(|e| format!("screen capture init failed: {e}"))
    };
    if simulcast {
        for (rid, w, h, bps) in SIMULCAST_LAYERS_VT.iter() {
            let capture = match init_capture(initial_display, *w, *h) {
                Ok(c) => c,
                Err(e) => {
                    error!("{e}");
                    info!(
                        "grant Screen Recording permission in System Settings > Privacy & Security"
                    );
                    return;
                }
            };
            layers.push((
                Some(Rid::from(*rid)),
                VtEncoder::new_with_codec(*w, *h, FPS, *bps, vt_codec).expect("vt encoder"),
                capture,
            ));
        }
        if let Some((_, _, cap)) = layers.first() {
            aerodesk_macos::inject::set_active_display(Some(cap.display_id()));
        }
    } else {
        let capture = match init_capture(initial_display, W, H) {
            Ok(c) => c,
            Err(e) => {
                error!("{e}");
                info!("grant Screen Recording permission in System Settings > Privacy & Security");
                return;
            }
        };
        layers.push((
            None,
            VtEncoder::new_with_codec(W, H, FPS, 8_000_000, vt_codec).expect("vt encoder"),
            capture,
        ));
        aerodesk_macos::inject::set_active_display(Some(layers[0].2.display_id()));
    }

    let mut connected = false;
    let mut audio_ticker = AudioTicker::new(audio_opus);
    let mut pts = 0i64;
    let pts_inc = 90_000 / FPS as i64;

    loop {
        // #211：排空式读取（软编/高负载下保证 SCTP ACK 及时消费，input 送达率不塌陷）。
        let wait = Duration::from_millis(5);
        socket.set_read_timeout(Some(wait)).ok();
        drain_udp_input(&mut socket, &mut endpoint, 512);
        let _ = endpoint.handle_timeout(Instant::now());

        let mut deadline = Instant::now() + Duration::from_secs(1);
        while let Some(output) = endpoint.poll_output() {
            match output {
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Timeout(t) => {
                    deadline = deadline.min(t);
                    break;
                }
                Output::Event(_) => {}
            }
        }

        while let Some(ev) = endpoint.poll_event() {
            match ev {
                ClientEvent::IceConnected => {
                    info!("ICE connected, starting screen capture stream (simulcast={simulcast})");
                    connected = true;
                }
                ClientEvent::Closed => {
                    info!("connection closed");
                    return;
                }
                // #58 显示器切换：viewer 经 control 通道请求，SFU 转发到 publisher。
                ClientEvent::ChannelData(cid, binary, data) => {
                    if endpoint.channel_label(cid).as_deref() == Some("control") {
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data)
                            && let Some(n) = v.get("display").and_then(|d| d.as_u64())
                        {
                            info!("control: display switch request -> display {n}");
                            if let Err(e) = rebuild_captures(n as usize, &mut layers) {
                                warn!("display switch failed（保持当前显示器）: {e}");
                            }
                        }
                    } else {
                        handle_publisher_input(
                            &mut endpoint,
                            ClientEvent::ChannelData(cid, binary, data),
                        );
                    }
                }
                ev => handle_publisher_input(&mut endpoint, ev),
            }
        }

        // #58 音频：按 20ms 节拍发送 PCMU 帧。
        if let Some(amid) = audio_mid {
            audio_ticker.tick(&mut endpoint, amid, Instant::now());
        }
        // #72 文件传输：推进发送。
        file_transfer::tick(&mut endpoint);
        cmd_exec::tick(&mut endpoint);
        // #75 远程光标：读取被控端真实光标位置（30Hz）。
        if last_cursor.elapsed() >= Duration::from_millis(33) {
            last_cursor = Instant::now();
            #[cfg(target_os = "macos")]
            if let Some((x, y)) = aerodesk_macos::cursor::cursor_position_normalized() {
                send_cursor(&mut endpoint, x, y);
            }
        }

        if connected {
            // 每层各自采集一帧（simulcast 下 SCK 按层分辨率采集；单层维持原路径）。
            let mut frames = Vec::with_capacity(layers.len());
            let mut captured_any = false;
            for (rid, encoder, capture) in &mut layers {
                if let Some(surface) = capture.next_frame(Duration::from_millis(50)) {
                    captured_any = true;
                    match encoder.encode_surface(&surface) {
                        Ok(Some(frame)) => {
                            let annexb = encoder.to_annexb(&frame);
                            frames.push((*rid, annexb));
                        }
                        Ok(None) => {}
                        Err(e) => warn!("vt encode: {e}"),
                    }
                }
            }
            if captured_any {
                let rtp_time = str0m::media::MediaTime::new(
                    pts as u64 * pts_inc as u64,
                    str0m::media::Frequency::NINETY_KHZ,
                );
                send_frame_layers(&mut endpoint, video_mid, rtp_time, &frames);
                // simulcast：每层一帧都需一次 do_payload，多排空避免 WriteWithoutPoll 背压。
                drain_payload_queue(&mut endpoint, layers.len());
                pts += 1;
            }
        }

        std::thread::sleep(Duration::from_millis(2));
        let _ = &mut signal;
    }
    #[test]
    fn reconnect_backoff_values() {
        assert_eq!(reconnect_backoff(0), Duration::from_secs(1));
        assert_eq!(reconnect_backoff(1), Duration::from_secs(2));
        assert_eq!(reconnect_backoff(2), Duration::from_secs(4));
        assert_eq!(reconnect_backoff(3), Duration::from_secs(8));
        assert_eq!(reconnect_backoff(4), Duration::from_secs(10), "cap at 10s");
        assert_eq!(reconnect_backoff(99), Duration::from_secs(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #73 合成源帧间隔精度：30fps 必须是 1/30s（33333333ns），
    /// 而不是 1000/30=33ms 截断导致的 30.3fps（10 分钟漂移 ~6s）。
    #[test]
    fn char_key_maps_ascii_and_shift() {
        assert_eq!(char_key('a'), Some(("KeyA", false)));
        assert_eq!(char_key('A'), Some(("KeyA", true)));
        assert_eq!(char_key('5'), Some(("Digit5", false)));
        assert_eq!(char_key('!'), Some(("Digit1", true)));
        assert_eq!(char_key(' '), Some(("Space", false)));
        assert_eq!(char_key('\n'), Some(("Enter", false)));
        assert_eq!(char_key('中'), None);
    }

    #[test]
    fn type_text_events_cover_shift_and_release() {
        let evs = type_text_events("Ab1!");
        // 4 字符 × (按下+抬起) = 8 事件
        assert_eq!(evs.len(), 8);
        // 'A' 按下带 shift
        if let InputEvent::Key {
            code,
            state,
            modifiers,
        } = &evs[0]
        {
            assert_eq!(code, "KeyA");
            assert_eq!(*state, ButtonState::Pressed);
            assert!(modifiers.shift);
        } else {
            panic!("expect Key");
        }
        // 每个字符后有 Released
        assert!(matches!(
            &evs[1],
            InputEvent::Key {
                state: ButtonState::Released,
                ..
            }
        ));
    }

    #[test]
    fn frame_interval_is_precise_for_30fps() {
        let d = frame_interval(30);
        assert_eq!(d.as_nanos(), 33_333_333);
        // 30 帧累计应≈1s（误差 < 1ms）
        let total = (0..30).map(|_| d.as_nanos()).sum::<u128>();
        assert!(
            (total as i128 - 1_000_000_000i128).abs() < 1_000_000,
            "30帧累计 {total}ns"
        );
        // 60fps 同样精确
        assert_eq!(frame_interval(60).as_nanos(), 16_666_666);
        // fps=0 不应除零
        assert!(frame_interval(0).as_nanos() > 0);
    }
}
