//! AeroDesk agent —— 客户端引擎（被控端发布/观看/控制，headless）。
//! 桌面端 spawn 本二进制、自启安装本二进制、mcp 经本二进制桥接；与
//! aerodesk-host（ADR-0009 服务宿主）配套：host 是服务壳，agent 是引擎。
//!
//! publisher：连接 SFU，用真实 VP8 抓包流作为媒体源发送视频。
//! viewer：连接 SFU，接收媒体并打印统计。
//!
//! 用法：
//!   aerodesk-agent --role publisher --signal ws://127.0.0.1:3003 --room demo
//!   aerodesk-agent --role viewer    --signal ws://127.0.0.1:3003 --room demo

#[macro_use]
extern crate tracing;

mod cli_video_decoder;
mod clipboard;
mod cmd_exec;
mod file_transfer;

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use aerodesk_codec::audio::RealAudioSender;
use aerodesk_codec::encode::FfmpegEncoder;
use aerodesk_core::access_unit::AccessUnitAssembler;
use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::media::{Vp8Frame, parse_vp8_pcap};
use aerodesk_core::media_socket::MediaSocket;
use aerodesk_core::platform::SystemWakeLock;
use aerodesk_core::protocol::input::{
    ButtonState, INPUT_PROTOCOL_VERSION, InputEvent, InputFrame, Modifiers, MouseButton,
};
use aerodesk_core::protocol::cmd::PowerAction;
use aerodesk_core::protocol::signal::Role;
use aerodesk_core::{Endpoint, platform::Codec};
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

/// --probe-audio：验证 macOS 系统音频采集（audio-only SCStream）。
fn probe_audio() {
    #[cfg(target_os = "macos")]
    {
        use std::time::Instant;
        match aerodesk_platform::macos::audio_capture::SystemAudioCapture::start() {
            Ok(cap) => {
                info!("probe-audio: SCStream audio started");
                let start = Instant::now();
                let mut peak = 0u64;
                while start.elapsed() < std::time::Duration::from_secs(5) {
                    let n = cap.take_samples(48_000);
                    if !n.is_empty() {
                        peak = peak.max(n.len() as u64);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                info!(
                    "probe-audio: total={} peak_batch={} -> {}",
                    cap.total_samples(),
                    peak,
                    if cap.total_samples() > 1000 {
                        "OK（系统音频可采集）"
                    } else {
                        "EMPTY（无音频输出/无权限/SCStream 音频不可用）"
                    }
                );
            }
            Err(e) => info!("probe-audio: start failed: {e}"),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        info!("probe-audio: 仅 macOS 支持");
    }
}

fn run() {
    let args: Vec<String> = std::env::args().collect();
    init_log();

    if args.iter().any(|a| a == "--issue-token") {
        issue_token(&args);
        return;
    }
    // macOS 系统音频采集探针：--probe-audio 启动 audio-only SCStream 5s，
    // 打印采集样本统计（验证 SCK 系统音频在本机可用）。
    if args.iter().any(|a| a == "--probe-audio") {
        probe_audio();
        return;
    }
    // #3 Windows 被控端开机自启（HKCU Run，无需管理员）：安装/移除/查询。
    if args.iter().any(|a| a == "--install-autostart") {
        #[cfg(windows)]
        {
            let exe = std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "aerodesk-agent.exe".into());
            let signal = arg(&args, "--signal").unwrap_or_else(|| "ws://127.0.0.1:3003/ws".into());
            let room = arg(&args, "--room").unwrap_or_else(|| "default".into());
            let cmd =
                aerodesk_platform::windows::autostart::autostart_command(&exe, &signal, &room);
            match aerodesk_platform::windows::autostart::install(&cmd) {
                Ok(()) => println!("autostart installed (HKCU Run): {cmd}"),
                Err(e) => {
                    eprintln!("autostart install failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        #[cfg(not(windows))]
        {
            eprintln!("--install-autostart 仅 Windows 支持");
            std::process::exit(1);
        }
    }
    if args.iter().any(|a| a == "--remove-autostart") {
        #[cfg(windows)]
        {
            match aerodesk_platform::windows::autostart::remove() {
                Ok(true) => println!("autostart removed"),
                Ok(false) => println!("autostart not installed"),
                Err(e) => {
                    eprintln!("autostart remove failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        #[cfg(not(windows))]
        {
            eprintln!("--remove-autostart 仅 Windows 支持");
            std::process::exit(1);
        }
    }
    if args.iter().any(|a| a == "--autostart-status") {
        #[cfg(windows)]
        {
            match aerodesk_platform::windows::autostart::installed() {
                Ok(Some(cmd)) => println!("installed: {cmd}"),
                Ok(None) => println!("not installed"),
                Err(e) => {
                    eprintln!("autostart query failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        #[cfg(not(windows))]
        {
            eprintln!("--autostart-status 仅 Windows 支持");
            std::process::exit(1);
        }
    }

    // #470 Windows 系统服务（需管理员）：安装/移除/查询 + 服务运行入口。

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
    // Windows 暂无 simulcast 路径（DXGI 单层），标记避免 unused 告警。
    #[cfg(target_os = "windows")]
    let _ = simulcast;
    // 高熵合成源（伪随机噪声）：码率贴近目标档位，用于选层/压测验证。
    let noisy = args.iter().any(|a| a == "--noisy");
    // #58 音频：publisher 发送合成 PCMU 音频 / viewer 接收；--mute-audio 观看端静音。
    let audio = args.iter().any(|a| a == "--audio");
    // #73 音频：--audio-opus 使用 Opus（48kHz）替代 PCMU（8kHz）。
    let audio_opus = args.iter().any(|a| a == "--audio-opus");
    let mute_audio = args.iter().any(|a| a == "--mute-audio");
    // 摄像头：publisher --camera 发布本地摄像头（第二路视频轨）；viewer --camera 接收统计。
    let camera = args.iter().any(|a| a == "--camera");
    let camera_device = arg(&args, "--camera-device");
    // --list-cameras：打印本机摄像头 id/名称（配合 --camera-device 选择），然后退出。
    if args.iter().any(|a| a == "--list-cameras") {
        #[cfg(target_os = "macos")]
        {
            for c in aerodesk_platform::macos::camera::list_cameras() {
                println!("{}\t{}", c.id, c.name);
            }
        }
        #[cfg(target_os = "linux")]
        {
            for c in aerodesk_platform::linux::camera::list_cameras() {
                println!("{c}");
            }
        }
        #[cfg(target_os = "windows")]
        {
            for (id, name) in aerodesk_platform::windows::camera::list_cameras() {
                println!("{id}\t{name}");
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            eprintln!("--list-cameras 仅 macOS/Linux/Windows 支持");
            std::process::exit(1);
        }
        return;
    }
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
    } else if let Some(p) = arg(&args, "--power") {
        // #503 系统电源命令：--power shutdown|reboot|lock（内置安全命令，
        // 动作枚举受限；参数非法时显式报错而非静默忽略）。
        match p.as_str() {
            "shutdown" => Some(cmd_exec::Intent::Power(PowerAction::Shutdown)),
            "reboot" => Some(cmd_exec::Intent::Power(PowerAction::Reboot)),
            "lock" => Some(cmd_exec::Intent::Power(PowerAction::Lock)),
            other => {
                eprintln!("--power 取值必须是 shutdown|reboot|lock，收到: {other}");
                std::process::exit(2);
            }
        }
    } else {
        None
    };
    // #58 显示器：publisher 初始采集显示器 / viewer 请求切换（--display N，0 = 主显示器）。
    // 仅 macOS 屏幕采集使用（Windows DXGI 采集主输出；Linux 批次 #4）。
    #[cfg(target_os = "macos")]
    let display: usize = arg(&args, "--display")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    #[cfg(not(target_os = "macos"))]
    let _display: usize = arg(&args, "--display")
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
    // 默认：macOS 支持硬编 HEVC 时优先 h265（同画质码率低 30-50%），
    // 否则回退 h264（全兼容）。显式 --codec 时尊重用户选择。
    let codec_arg = arg(&args, "--codec").map(|s| s.to_string());
    let video_codec: Codec = match codec_arg.as_deref() {
        Some("h265") | Some("hevc") => Codec::Hevc,
        Some("vp9") => Codec::Vp9,
        Some("av1") => Codec::Av1,
        Some("h264") => Codec::H264,
        None => {
            #[cfg(target_os = "macos")]
            {
                if aerodesk_platform::macos::vt_encoder::VtEncoder::hevc_encoder_available() {
                    Codec::Hevc
                } else {
                    Codec::H264
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                Codec::H264
            }
        }
        _ => Codec::H264,
    };

    // #72 文件传输状态机（进程级单例；发送/接收同一状态机）。
    // 仅 publisher（被控端）响应远端文件请求，viewer 拒绝（安全审查 #255）。
    file_transfer::init(send_file, recv_dir, cancel_send_after, role == "publisher");
    // #109 远程命令通道：被控端执行器（publisher）/ 控制端响应（viewer）。
    cmd_exec::init();

    // #109 权限/审计本地管理（无需会话；处理完直接退出）。
    if cmd_exec::run_admin(&args) {
        return;
    }

    match role.as_str() {
        "publisher" if encoder == "screen" => {
            #[cfg(target_os = "macos")]
            {
                let vt_capable = video_codec == Codec::H264
                    || (video_codec == Codec::Hevc
                        && aerodesk_platform::macos::vt_encoder::VtEncoder::hevc_encoder_available(
                        ));
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
                        camera,
                        camera_device.clone(),
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
            #[cfg(not(target_os = "macos"))]
            {
                // Windows：DXGI 采集 + OpenH264 软编 + SendInput 注入（被控端）。
                // 其他平台（Linux）采集批次 #4，先回退合成源。
                #[cfg(target_os = "windows")]
                {
                    // #3 缩放：4K 显示器软编性能不足，默认缩放到 1920x1080；
                    // --width/--height 覆盖；0 = 原生分辨率。
                    let scale_w: u32 = arg(&args, "--width")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1920);
                    let scale_h: u32 = arg(&args, "--height")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1080);
                    // #8：屏幕采集编码码率（默认 8Mbps，与合成源/OpenH264 旧默认一致）。
                    let bitrate: u32 = arg(&args, "--bitrate")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(8_000_000);
                    publisher_capture_windows(
                        &signal,
                        &room,
                        token.as_deref(),
                        audio,
                        audio_opus,
                        video_codec,
                        scale_w,
                        scale_h,
                        _display as u32,
                        bitrate,
                        camera,
                        camera_device.clone(),
                    );
                }
                #[cfg(target_os = "linux")]
                publisher_capture_linux(
                    &signal,
                    &room,
                    token.as_deref(),
                    audio,
                    audio_opus,
                    video_codec,
                    camera,
                    camera_device.clone(),
                );
                #[cfg(not(any(target_os = "windows", target_os = "linux")))]
                {
                    info!("--encoder screen 仅 macOS/Windows/Linux 支持；回退合成源");
                    publisher_ffmpeg(
                        &signal,
                        &room,
                        token.as_deref(),
                        audio,
                        audio_opus,
                        video_codec,
                        noisy,
                    );
                }
            }
        }
        "publisher" if encoder == "vt" => {
            #[cfg(target_os = "macos")]
            {
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
            #[cfg(not(target_os = "macos"))]
            {
                info!("--encoder vt 仅 macOS（VideoToolbox）；Windows 请用 --encoder screen");
            }
        }
        "publisher" if encoder == "ffmpeg" => {
            // #8：参数化合成源（默认 640x360@30@1.5Mbps），支持 4K60 压测。
            let w: u32 = arg(&args, "--width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(640);
            let h: u32 = arg(&args, "--height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(360);
            let fps: u32 = arg(&args, "--fps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(30);
            let bitrate: u32 = arg(&args, "--bitrate")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1_500_000);
            publisher_ffmpeg(
                &signal,
                &room,
                token.as_deref(),
                audio,
                audio_opus,
                video_codec,
                noisy,
                w,
                h,
                fps,
                bitrate,
            );
        }
        "publisher" if encoder == "x264" => {
            #[cfg(not(target_os = "windows"))]
            publisher_x264(
                &signal,
                &room,
                token.as_deref(),
                simulcast,
                noisy,
                audio,
                audio_opus,
            );
            #[cfg(target_os = "windows")]
            {
                info!(
                    "--encoder x264 不支持 Windows（x264 crate 仅非 Windows 编译）；请用 --encoder screen"
                );
            }
        }
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
            let sc = arg(&args, "--send-control");
            run_with_reconnect(
                move || {
                    viewer(
                        &sig,
                        &r,
                        tok.as_deref(),
                        layer.as_deref(),
                        audio,
                        camera,
                        mute_audio,
                        viewer_display,
                        input_script,
                        si.as_ref(),
                        tt.as_deref(),
                        ci.as_ref(),
                        cmd_json,
                        rf.as_deref(),
                        sc.as_deref(),
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
///   JWT_SECRET=<secret> aerodesk-agent --issue-token --user u1 --device mac-1 --room demo --role publisher --ttl 3600
///   JWT_SECRET=<secret> aerodesk-agent --issue-token --user u1 --room demo --role "*" --ttl 86400 [--max-conns 4]
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

    match aerodesk_core::protocol::jwt::mint_token(
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
        // flag 是最后一个参数时无值可取：返回 None（调用方均有默认值/错误处理），
        // 不得越界 panic。
        .and_then(|i| args.get(i + 1).cloned())
}

fn init_log() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aerodesk_agent=info"));
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
        SipSession,
        Endpoint,
        MediaSocket,
        str0m::media::Mid,
        Option<str0m::media::Mid>,
        Option<str0m::media::Mid>,
    ),
    String,
> {
    connect_inner(signal_url, room, role, None, false, audio, auth, false)
}

/// SIP 会话句柄（#552：替代 connect 返回的 WsSignalClient——会话循环不消费
/// 信令面；link 由 connect 内的看护线程持有至进程退出）。
pub struct SipSession {
    #[allow(dead_code)]
    pub call_id: String,
}

/// #552 SIP 环境配置：AERO_SIP_TRANSPORT（udp|tls，默认 udp——e2e/内网）/
/// AERO_SIP_PORT（0=按传输默认）/ AERO_SIP_DOMAIN / AERO_SIP_CA_PEM（TLS CA
/// 路径，空=系统根）。TURN：AERO_TURN_URLS/USERNAME/CREDENTIAL（SIP 无 join
/// 下发一环，须本地配置；空=直连）。
fn sip_env_cfg(
    signal_url: &str,
    device_id: &str,
    token: &str,
) -> Result<aerodesk_core::sip_link::SipLinkConfig, String> {
    aerodesk_core::sip_link::SipLinkConfig::from_parts(
        signal_url,
        device_id,
        token,
        &std::env::var("AERO_SIP_TRANSPORT").unwrap_or_default(),
        std::env::var("AERO_SIP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0),
        &std::env::var("AERO_SIP_DOMAIN").unwrap_or_default(),
        &std::env::var("AERO_SIP_CA_PEM").unwrap_or_default(),
    )
}

/// 探测本机出接口 IP（绑 0.0.0.0 连公共地址后取 local_addr；失败回退 loopback）。
fn discover_egress_ip(port: u16) -> std::net::SocketAddr {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    if let Ok(probe) = UdpSocket::bind("0.0.0.0:0") {
        if probe.connect("8.8.8.8:53").is_ok()
            && let Ok(la) = probe.local_addr()
            && !la.ip().is_unspecified()
            && !la.ip().is_loopback()
        {
            return SocketAddr::new(la.ip(), port);
        }
    }
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

#[cfg(not(target_os = "windows"))]
fn connect_h264(
    signal_url: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
    simulcast: bool,
    audio: bool,
) -> Result<
    (
        SipSession,
        Endpoint,
        MediaSocket,
        str0m::media::Mid,
        Option<str0m::media::Mid>,
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
        false,
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
        SipSession,
        Endpoint,
        MediaSocket,
        str0m::media::Mid,
        Option<str0m::media::Mid>,
        Option<str0m::media::Mid>,
    ),
    String,
> {
    connect_inner(
        signal_url,
        room,
        role,
        Some(codec),
        false,
        audio,
        auth,
        false,
    )
}

/// 带第二路视频轨（摄像头）的连接：publisher 发布 / viewer 接收（recvonly）。
/// codec=None 用默认端点（H264 全兼容 + PCMU/Opus）。
fn connect_camera(
    signal_url: &str,
    room: &str,
    role: Role,
    auth: Option<&str>,
    audio: bool,
    codec: Option<Codec>,
) -> Result<
    (
        SipSession,
        Endpoint,
        MediaSocket,
        str0m::media::Mid,
        Option<str0m::media::Mid>,
        Option<str0m::media::Mid>,
    ),
    String,
> {
    connect_inner(signal_url, room, role, codec, false, audio, auth, true)
}

fn connect_inner(
    signal_url: &str,
    room: &str,
    role: Role,
    codec: Option<Codec>,
    simulcast: bool,
    audio: bool,
    auth: Option<&str>,
    camera: bool,
) -> Result<
    (
        SipSession,
        Endpoint,
        MediaSocket,
        str0m::media::Mid,
        Option<str0m::media::Mid>,
        Option<str0m::media::Mid>,
    ),
    String,
> {
    // #552 SIP 信令面（替代 WSS join）：REGISTER → viewer INVITE 目标（房间名
    // =设备 AoR 时 1:1 透明代理；无绑定时服务端会议桥入 SFU）；publisher 等
    // IncomingCall（1:1 被叫，以 --room 值为设备 AoR——e2e 脚本 viewer 呼同
    // 名房间即接通，脚本零改动）。
    let _ = simulcast;
    // publisher 以 --room 值为设备 AoR（viewer 呼同名房间即 1:1 接通——e2e
    // 脚本零改动）；viewer 身份仅用于 REGISTER（任意名）。
    let device_id = match role {
        Role::Publisher => room.to_string(),
        Role::Viewer => format!("agent-viewer-{}", std::process::id()),
    };
    info!("SIP device_id={device_id} target={room} role={role:?}");
    let mut link = aerodesk_core::sip_link::SipCallLink::new(sip_env_cfg(
        signal_url,
        &device_id,
        auth.unwrap_or(""),
    )?);
    link.start();
    {
        // 等 Online（10s；口令错/服务器不可达在此显式失败，回 Err 而非 panic——
        // 便于脚本诊断"signal 未启 SIP 面/口令错"而非笼统崩溃）。
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let st = link.poll();
            if st.is_online() {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "SIP 注册未完成（10s）：{st:?}——检查 signal 的 SIP 端口/口令"
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        info!("SIP registered: {device_id}");
    }

    // publisher：等 IncomingCall（300s；被叫 offer 到达后才建媒体端点）。
    let incoming: Option<(String, String)> = if role == Role::Publisher {
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut got: Option<(String, String)> = None;
        while Instant::now() < deadline && got.is_none() {
            let _st = link.poll();
            for ev in link.take_events() {
                if let aerodesk_core::sip_link::SipLinkEvent::IncomingCall {
                    call_id,
                    offer_sdp,
                    ..
                } = ev
                {
                    got = Some((call_id, offer_sdp));
                    break;
                }
            }
            if got.is_none() {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Some(got.ok_or("publisher 等待来电超时（300s 无 INVITE）")?)
    } else {
        None
    };

    // #216 外部 NAT 场景：非回环信令绑 0.0.0.0（否则 127.0.0.1 源地址发不出外部
    // UDP，TURN UDP 中继不可用，只能退 TCP TURN）；与 aerodesk-core connect 对齐。
    let loopback_signal = signal_url.contains("127.0.0.1")
        || signal_url.contains("localhost")
        || signal_url.contains("::1");
    // #218：force-relay（AERODESK_FORCE_RELAY=1|true）——ICE 只通告 relayed
    // 候选、跳过 host 候选，强制媒体走 TURN 中继（NAT/弱网压测中继路径）。
    let force_relay = aerodesk_core::connect::force_relay_env();
    let direct = if loopback_signal {
        UdpSocket::bind("127.0.0.1:0").map_err(|e| format!("bind udp: {e}"))?
    } else {
        UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind udp: {e}"))?
    };
    let addr = direct.local_addr().map_err(|e| e.to_string())?;
    info!("local UDP addr: {addr}");

    // TURN：SIP 无 join 下发一环——AERO_TURN_* 环境配置（失败仅告警直连兜底）。
    let turn_transport = aerodesk_core::turn_client::p2p_turn_transport(
        &std::env::var("AERO_TURN_URLS").unwrap_or_default(),
        &std::env::var("AERO_TURN_USERNAME").unwrap_or_default(),
        &std::env::var("AERO_TURN_CREDENTIAL").unwrap_or_default(),
    );
    if turn_transport.is_none() {
        info!("TURN 未配置（AERO_TURN_* 为空）——直连");
    }
    let mut socket = MediaSocket::new(direct, turn_transport);

    let mut endpoint = match codec {
        None => Endpoint::new(),
        Some(Codec::H264) => Endpoint::new_with_codec(Codec::H264),
        Some(c) => Endpoint::new_with_codec(c),
    };
    // 通配绑定（0.0.0.0）的 local_addr 不能作为候选（str0m 拒绝）：探测出接口 IP
    // 作为 host 候选（与 aerodesk-core 一致；失败回退 loopback）。
    let mut host_candidate = addr;
    if addr.ip().is_unspecified() {
        host_candidate = discover_egress_ip(addr.port());
    }
    if force_relay {
        info!("force-relay: skip host candidate {host_candidate}");
    } else {
        endpoint
            .add_local_candidate(host_candidate, Protocol::Udp)
            .map_err(|e| format!("candidate: {e}"))?;
    }
    // relayed 候选（typ relay）：ICE 按优先级直连优先、TURN 兜底。
    if let Some(tt) = socket.turn() {
        let relayed = tt.relayed_addr();
        if let Ok(la) = tt.local_addr() {
            let local = std::net::SocketAddr::new(host_candidate.ip(), la.port());
            info!("relayed candidate {relayed} (local {local}) force_relay={force_relay}");
            if let Err(e) = endpoint.add_relay_candidate(relayed, local) {
                warn!("relay candidate rejected (TURN disabled): {e:?}");
            }
        }
    } else if force_relay {
        warn!("force-relay requested but no TURN transport (AERO_TURN_* 未配置)");
    }

    // 媒体轨：viewer 预配（recvonly）；publisher 不预配（被叫按 INVITE offer 反演）。
    let camera_mid: Option<str0m::media::Mid>;
    let audio_mid: Option<str0m::media::Mid>;
    let video_mid: Option<str0m::media::Mid>;
    let call_id_out;
    if role == Role::Viewer {
        // #12：viewer 的 offer 用 recvonly。
        endpoint.add_video_recvonly();
        if audio {
            endpoint.add_audio_recvonly();
        }
        if camera {
            endpoint.add_camera_recvonly();
        }
        let (offer, pending, vm, am, cm) =
            endpoint.create_offer().map_err(|e| format!("offer: {e}"))?;
        info!("video mid: {vm:?} audio mid: {am:?} camera mid: {cm:?}");
        let offer_json = serde_json::to_string(&offer).map_err(|e| e.to_string())?;
        let call_id = format!("c-{}", std::process::id());
        link.call(room, &call_id, &offer_json)
            .map_err(|e| format!("SIP INVITE: {e}"))?;
        // 等 Answered/Rejected（30s；180 仅记日志）。
        let answer_json = {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut got: Result<String, String> = Err("SIP INVITE 无应答（30s）".into());
            'ans: while Instant::now() < deadline {
                let _st = link.poll();
                for ev in link.take_events() {
                    match ev {
                        aerodesk_core::sip_link::SipLinkEvent::Answered { answer_sdp, .. } => {
                            got = Ok(answer_sdp);
                            break 'ans;
                        }
                        aerodesk_core::sip_link::SipLinkEvent::Rejected { status, .. } => {
                            got = Err(format!("SIP 呼叫被拒（{status}）"));
                            break 'ans;
                        }
                        aerodesk_core::sip_link::SipLinkEvent::Ringing { .. } => {
                            info!("SIP 180 Ringing");
                        }
                        _ => {}
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            got?
        };
        let answer: str0m::change::SdpAnswer =
            serde_json::from_str(&answer_json).map_err(|e| format!("answer parse: {e}"))?;
        debug!("answer media lines: {:?}", answer.media_lines);
        endpoint
            .accept_answer(pending, answer)
            .map_err(|e| format!("accept answer: {e}"))?;
        video_mid = vm;
        audio_mid = am;
        camera_mid = cm;
        call_id_out = call_id;
    } else {
        let (call_id, offer_sdp) = incoming.expect("publisher 必有来电");
        info!("incoming call {call_id}（offer {}B）", offer_sdp.len());
        let offer: str0m::change::SdpOffer =
            serde_json::from_str(&offer_sdp).map_err(|e| format!("offer parse: {e}"))?;
        let answer = endpoint
            .accept_offer(offer)
            .map_err(|e| format!("accept offer: {e}"))?;
        let answer_json = serde_json::to_string(&answer).map_err(|e| e.to_string())?;
        // 被叫 mid 从 offer SDP 推导（answer 未带 mid 摘要）。
        let vm = aerodesk_core::p2p_call::offer_video_mid(&offer_sdp);
        let am = aerodesk_core::p2p_call::offer_audio_mid(&offer_sdp);
        let cm: Option<str0m::media::Mid> = None;
        info!("callee mids: video={vm:?} audio={am:?} camera={cm:?}");
        link.accept(&call_id, &answer_json)
            .map_err(|e| format!("SIP accept: {e}"))?;
        video_mid = vm;
        audio_mid = am;
        camera_mid = cm;
        call_id_out = call_id;
    }

    info!("SDP negotiated, awaiting ICE...");
    // #477：ICE 等待收敛到 connect 阶段——超时显式失败。TURN 路径建链慢
    // （实测中继下 5-12s），给 15s。
    {
        let ice_deadline =
            Instant::now() + Duration::from_secs(if socket.turn().is_some() { 15 } else { 5 });
        while Instant::now() < ice_deadline && endpoint.is_alive() {
            socket
                .set_read_timeout(Some(Duration::from_millis(10)))
                .ok();
            let mut buf = [0u8; 2048];
            if let Ok((n, source)) = socket.recv_from(&mut buf)
                && let Ok(contents) = buf[..n].try_into()
            {
                let _ = endpoint.handle_input(Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: socket.local_addr().unwrap(),
                        contents,
                    },
                ));
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
            // 查状态标志而非 poll_event：事件队列留给会话循环。
            if endpoint.ice_connected() {
                break;
            }
        }
        if !endpoint.ice_connected() {
            return Err("ICE 连接超时（直连 5s / TURN 15s 未建立）".into());
        }
        info!("ICE connected (connect 阶段)");
    }

    // 信令看护线程：持有 link 至进程退出（Drop 即 BYE/注销——会话循环只驱动
    // endpoint/socket）；泵事件记 PeerHangup，后到 trickle 候选忽略（候选内联）。
    {
        let mut link = link;
        std::thread::Builder::new()
            .name("sip-link-watch".into())
            .spawn(move || {
                loop {
                    let _ = link.poll();
                    for ev in link.take_events() {
                        if let aerodesk_core::sip_link::SipLinkEvent::PeerHangup {
                            call_id, ..
                        } = ev
                        {
                            info!("SIP 对端挂断：{call_id}");
                        }
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
            .ok();
    }

    let video_mid = video_mid.ok_or("no video mid")?;
    Ok((
        SipSession {
            call_id: call_id_out,
        },
        endpoint,
        socket,
        video_mid,
        audio_mid,
        camera_mid,
    ))
}

/// VideoToolbox 合成源编码参数（--width/--height/--fps/--bitrate）。
#[cfg(target_os = "macos")]
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
    opus: Option<aerodesk_codec::audio::OpusEncoder>,
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
        // #216：每次 tick 最多补发一帧（欠帧只追一帧）——while 补发全部欠帧会
        // 突发塞满 str0m 输出队列，导致后续 send_video_frame 持续 WriteWithoutPoll，
        // 视频被饿死（--audio 时视频几乎不发送）。
        if self.next <= now {
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
                        match aerodesk_codec::audio::OpusEncoder::new(64_000) {
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

/// 无摄像头源占位（publisher_generic 泛型参数用；传 None 时不会构造实例）。
struct NoCameraCapture;

impl aerodesk_core::platform::CameraSource for NoCameraCapture {
    type Error = String;

    fn start(&mut self, _width: u32, _height: u32, _fps: u32) -> Result<(), String> {
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<aerodesk_core::platform::CameraFrame>, String> {
        Ok(None)
    }

    fn stop(&mut self) {}
}

/// 无光标源占位（publisher_generic 泛型参数用；传 None 时用合成光标轨迹）。
struct NoCursor;

impl aerodesk_core::platform::CursorSource for NoCursor {
    fn position_normalized(&mut self) -> Option<(f64, f64)> {
        None
    }
}

/// 无真实音频源占位（publisher_generic 泛型参数用；传 None 时不会构造实例）。
struct NoAudioCapture;

impl aerodesk_core::platform::AudioCapturer for NoAudioCapture {
    type Error = String;

    fn next_samples(&mut self, _max: usize) -> Result<Vec<f32>, String> {
        Ok(Vec::new())
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
fn print_cmd_result(resp: &aerodesk_core::protocol::cmd::CmdResponse) {
    use aerodesk_core::protocol::cmd::CmdResult;
    match &resp.result {
        CmdResult::Run {
            exit_code,
            stdout,
            stderr,
            truncated,
            error,
            code: _,
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
        CmdResult::File {
            data,
            size,
            error,
            code: _,
        } => {
            info!(
                "CMD_RESULT: ok={} type=file size={size} error={error:?}",
                error.is_none()
            );
            if let Some(b64) = data {
                if let Some(bytes) = aerodesk_core::protocol::cmd::decode_b64(b64) {
                    info!("CMD_FILE_CONTENT:\n{}", String::from_utf8_lossy(&bytes));
                }
            }
        }
        CmdResult::ProcessList {
            processes,
            error,
            code: _,
        } => {
            info!(
                "CMD_RESULT: ok={} type=ps count={} error={error:?}",
                error.is_none(),
                processes.len()
            );
            for p in processes {
                info!("CMD_PROC: {} {}", p.pid, p.name);
            }
        }
        CmdResult::Killed {
            pid,
            error,
            code: _,
        } => {
            info!(
                "CMD_RESULT: ok={} type=kill pid={pid} error={error:?}",
                error.is_none()
            );
        }
        CmdResult::Chat { sender, text } => {
            info!("CHAT: {sender}: {text}");
        }
        // #503 电源命令回执：动作 + 错误（成功即返回；关机/重启后对端可能不再回话）。
        CmdResult::Power {
            action,
            error,
            code: _,
        } => {
            info!(
                "CMD_RESULT: ok={} type=power action={} error={error:?}",
                error.is_none(),
                action.label()
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
    let pos = aerodesk_core::protocol::cursor::CursorPos::new(x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
        .with_sent_ms(now_ms());
    if let Ok(json) = serde_json::to_string(&pos) {
        endpoint.send_channel_data("cursor", false, json.as_bytes());
    }
}

/// #8 端到端延迟：合成光标轨迹（30Hz 正弦，e2e/延迟测量用）。
fn synthetic_cursor_pos(start: std::time::Instant) -> (f64, f64) {
    let t = start.elapsed().as_secs_f64();
    (0.5 + 0.3 * t.sin(), 0.5 + 0.3 * t.cos())
}

/// 发布端公共事件处理：输入通道（观看端 → 被控端）。
fn handle_publisher_input(endpoint: &mut Endpoint, ev: ClientEvent) {
    // #72 文件传输：file 通道事件交给状态机（非 file 事件为 no-op）。
    file_transfer::handle_event(&ev, endpoint);
    // #109 远程命令：cmd 通道请求交给执行器（后台线程执行，主循环回传响应）。
    cmd_exec::handle_event(&ev, endpoint);
    match ev {
        // #553 验收诊断：所有通道打开打日志（bitrate/display 的 control 未达
        // 排查——input/file 开而 control 是否在 pub 侧打开需可见）。
        ClientEvent::ChannelOpen(label, _)
            if label == "input"
                || label == "control"
                || label == "file"
                || label == "cursor"
                || label == "cmd" =>
        {
            info!("channel open: {label}");
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
                            // #75：把 viewer 输入注入被控端（macOS CGEvent / Windows SendInput）。
                            inject_input(frame.seq, &frame.event);
                        }
                    }
                }
            } else if endpoint.channel_label(cid).as_deref() == Some("control") {
                // #58 显示器切换：viewer → SFU → publisher（control 通道转发）。
                // #267 码率反馈：SFU/观看端经 control 下发 {"bitrate":N}。
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
                    if let Some(n) = v.get("display").and_then(|d| d.as_u64()) {
                        info!("control: display switch request -> display {n}");
                    }
                    if let Some(bps) = v.get("bitrate").and_then(|b| b.as_u64()) {
                        // 合成发布端（vt/x264/ffmpeg）无持久编码器句柄：日志验证；
                        // 真实屏幕发布端（publisher_capture）在此处应用 set_bitrate。
                        info!("control: bitrate feedback -> {bps} bps");
                    }
                }
            }
        }
        _ => {}
    }
}

/// Linux 输入注入器：X11 会话用 XTest；Wayland（无 DISPLAY）优先
/// portal RemoteDesktop（compositor 级，无需 root），失败回退 uinput。
#[cfg(target_os = "linux")]
enum LinuxInjector {
    XTest(aerodesk_platform::linux::inject::XTestInjector),
    Uinput(aerodesk_platform::linux::inject::UinputInjector),
    Portal(aerodesk_platform::linux::portal_inject::PortalInjector),
}

#[cfg(target_os = "linux")]
impl LinuxInjector {
    fn new() -> Option<Self> {
        if std::env::var("DISPLAY").is_ok() {
            match aerodesk_platform::linux::inject::XTestInjector::new() {
                Ok(i) => Some(Self::XTest(i)),
                Err(e) => {
                    warn!("XTest injector init failed: {e}");
                    None
                }
            }
        } else {
            // Wayland：portal RemoteDesktop 优先（#319；aerodesk-platform 依赖固定启用 pipewire）。
            {
                match aerodesk_platform::linux::portal_inject::PortalInjector::new() {
                    Ok(i) => {
                        info!("Linux input injector: portal RemoteDesktop");
                        return Some(Self::Portal(i));
                    }
                    Err(e) => warn!("portal injector init failed, fallback uinput: {e}"),
                }
            }
            match aerodesk_platform::linux::inject::UinputInjector::new() {
                Ok(i) => Some(Self::Uinput(i)),
                Err(e) => {
                    warn!("uinput injector init failed: {e}");
                    None
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl aerodesk_core::platform::InputInjector for LinuxInjector {
    type Error = String;

    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        match self {
            Self::XTest(i) => aerodesk_core::platform::InputInjector::inject(i, event),
            Self::Uinput(i) => aerodesk_core::platform::InputInjector::inject(i, event),
            Self::Portal(i) => aerodesk_core::platform::InputInjector::inject(i, event),
        }
    }
}

/// Windows 被控端当前采集显示器在虚拟屏幕中的区域（#75 多显示器注入坐标映射）。
#[cfg(target_os = "windows")]
static ACTIVE_DISPLAY_RECT: std::sync::Mutex<Option<(i32, i32, u32, u32)>> =
    std::sync::Mutex::new(None);

/// 把观看端输入事件注入被控端（平台分支：macOS CGEvent / Windows SendInput / Linux XTest-uinput）。
fn inject_input(seq: u64, event: &InputEvent) {
    #[cfg(target_os = "macos")]
    {
        match aerodesk_platform::macos::inject::inject(event) {
            Ok(()) => info!("inject: seq={seq} {event:?}"),
            Err(e) => info!("inject failed: seq={seq} {event:?}: {e}"),
        }
    }
    #[cfg(target_os = "windows")]
    {
        let mut inj = aerodesk_platform::windows::inject::SendInputInjector::new();
        if let Ok(guard) = ACTIVE_DISPLAY_RECT.lock() {
            inj.set_active_display(*guard);
        }
        match aerodesk_core::platform::InputInjector::inject(&mut inj, event) {
            Ok(()) => info!("inject: seq={seq} {event:?}"),
            Err(e) => info!("inject failed: seq={seq} {event:?}: {e}"),
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::cell::RefCell;
        use std::thread_local;

        thread_local! {
            static LINUX_INJ: RefCell<Option<LinuxInjector>> = const { RefCell::new(None) };
        }
        LINUX_INJ.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                *opt = LinuxInjector::new();
            }
            match opt
                .as_mut()
                .map(|inj| aerodesk_core::platform::InputInjector::inject(inj, event))
            {
                Some(Ok(())) => info!("inject: seq={seq} {event:?}"),
                Some(Err(e)) => info!("inject failed: seq={seq} {event:?}: {e}"),
                None => info!("inject: 无可用注入器（X11/uinput 均不可用）"),
            }
        });
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = (seq, event);
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

    let (_signal, mut endpoint, mut socket, video_mid, audio_mid, _camera_mid) =
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
    }
}

#[allow(clippy::too_many_arguments)] // #75 e2e 输入脚本开关；与既有 publisher 系列函数同风格。
/// 返回 Err=会话结束需重连（#173）；Ok=正常完成（一次性模式）。
/// viewer 诊断：RGBA 原始像素 → PNG 字节（供 AERODESK_DUMP_FRAME 落盘；
/// 镜像 core clipboard::rgba_to_png，此处为 CLI 本地副本，避免为它改 core API）。
fn rgba_to_png(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wtr = enc.write_header().ok()?;
        wtr.write_image_data(rgba).ok()?;
    }
    Some(buf)
}

fn viewer(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    layer: Option<&str>,
    audio: bool,
    camera: bool,
    mute_audio: bool,
    display: Option<usize>,
    input_script: bool,
    send_input: Option<&InputEvent>,
    type_text: Option<&str>,
    cmd_intent: Option<&cmd_exec::Intent>,
    cmd_json: bool,
    request_file: Option<&str>,
    send_control: Option<&str>,
) -> Result<(), String> {
    let (_signal, mut endpoint, mut socket, _, _audio_mid, _camera_mid) = if camera {
        connect_camera(signal_url, room, Role::Viewer, auth, audio, None)?
    } else {
        connect(signal_url, room, Role::Viewer, auth, audio)?
    };
    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut keyframes = 0u64;
    // 摄像头第二路视频轨统计（--camera）：SFU 转发 mid 为 SFU 本地 mid，
    // 无法与本地协商 mid 直接比对；按「首个视频 mid=屏幕、第二个=摄像头」
    // 的到达顺序区分（SFU 按发布端 offer 顺序开轨）。
    let mut video_mids: Vec<str0m::media::Mid> = Vec::new();
    // #340：远端（SFU）重协商 offer 的发送视频轨顺序（screen→camera），确定性
    // 区分两轨；为空时回退到达顺序。
    let mut camera_frames = 0u64;
    let mut camera_bytes = 0u64;
    let mut camera_decoded = 0u64;
    let mut camera_decoder: Option<cli_video_decoder::CliVideoDecoder> = None;
    // #340：摄像头轨按 NAL 分片到达，需组装为完整访问单元再解码
    //（屏幕轨单 NAL/帧直接可解；摄像头 hevc_videotoolbox 多 NAL/帧）。
    let mut camera_assembler = AccessUnitAssembler::new();
    // 屏幕轨同样经 assembler 组装：借用 &[u8]，避免每帧 data.data.to_vec()
    // 全量拷贝（str0m 媒体数据为 Arc<[u8]>，无法移动进 EncodedUnit）。
    let mut screen_assembler = AccessUnitAssembler::new();
    let mut audio_frames = 0u64;
    let mut audio_bytes = 0u64;
    let mut last_report = Instant::now();
    let mut input_open = false;
    let mut input_seq = 0u64;
    let mut last_input = Instant::now();
    let mut layer_sent = layer.is_none();
    let mut control_sent = send_control.is_none();
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
    // #8 端到端延迟：cursor 带发送时间戳，viewer 计算 one-way latency（节流 200ms，#253）。
    let mut last_latency_log = Instant::now();
    // #75/#109 单次输入（MCP 键鼠）：input 通道打开后发送一次/序列，500ms 后退出。
    let mut input_sent = send_input.is_none() && type_text.is_none();
    // #122 大文件下载：file 通道打开后发送 FileControl::Request，轮询 recv-dir 落盘后退出。
    let mut file_request_sent = false;
    let mut file_request_started: Option<Instant> = None;
    let mut input_exit_at: Option<Instant> = None;
    // #109 远程命令/文件/进程（控制端一次执行）：请求每 1s 重传直到响应（首包可能被
    // SFU 在通道未就绪时丢弃；被控端按 id 去重，重复执行安全）。响应一旦解析成功
    // 即 process::exit，无需单独的重传停止标志。
    let cmd_pending = cmd_intent.is_some();
    let mut last_cmd_send = Instant::now() - Duration::from_secs(1);
    // #74 解码端验证：FFmpeg 软解全部 codec（H264/H265/VP9/AV1），codec-e2e 断言。
    let mut video_decoder: Option<cli_video_decoder::CliVideoDecoder> = None;
    // #136 关键帧请求：首包/不连续/切层时向 SFU 发 PLI（节流 1s）。
    let mut last_kf_request: Option<Instant> = None;
    let mut last_kf_rid: Option<str0m::media::Rid> = None;
    let mut seen_video = false;
    let mut decoded_frames: u64 = 0;
    // viewer 诊断（#487 端到端可视验证）：AERODESK_DUMP_FRAME=<path> 时，把解码出的
    // 第 AERODESK_DUMP_AFTER（默认 30）帧屏幕帧落盘成 PNG——证明「采集→编码→SFU→解码」
    // 全链路真的出了图，而不只是统计计数。与 AERODESK_BGRA 等同为诊断开关。
    let dump_frame: Option<String> = std::env::var("AERODESK_DUMP_FRAME").ok();
    let dump_after: u64 = std::env::var("AERODESK_DUMP_AFTER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    // #73 Opus 音频：libopus 解码（惰性创建；不可用时降级为仅统计）。
    let mut opus_decoder: Option<aerodesk_codec::audio::OpusDecoder> = None;
    // #173 媒体静默检测：收到过包后连续无包超过阈值视为会话死亡（str0m is_alive
    // 在 recvonly 场景下不触发 ICE Failed，需主动探活）。
    let mut last_rx: Option<Instant> = None;
    // #487 审查批次 2：统一为 core 常量（原 8s，与桌面端 10s 对齐）。
    const DEAD_AFTER_NO_MEDIA: Duration = aerodesk_core::util::NO_MEDIA_DEADLINE;

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
                                opus_decoder = aerodesk_codec::audio::OpusDecoder::new().ok();
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
                        // 摄像头/屏幕轨区分：SFU 转发 mid 无法与本地协商 mid 比对，
                        // 按「首个视频 mid=屏幕、第二个=摄像头」到达顺序区分。
                        if !video_mids.contains(&data.mid) {
                            video_mids.push(data.mid);
                        }
                        let is_camera = camera && {
                            let send_mids = endpoint.remote_send_video_mids();
                            if send_mids.len() >= 2 {
                                send_mids.get(1) == Some(&data.mid)
                            } else {
                                video_mids.len() > 1 && video_mids[1] == data.mid
                            }
                        };
                        // #136 首包 / 不连续 / 切层 → 请求关键帧（PLI，节流 1s）。
                        let now = Instant::now();
                        let rid_changed = last_kf_rid != data.rid;
                        let due = last_kf_request
                            .map(|t| now.duration_since(t) >= Duration::from_secs(1))
                            .unwrap_or(true);
                        if due && (rid_changed || !data.contiguous || !seen_video) {
                            let _ = endpoint.request_keyframe(
                                data.mid,
                                data.rid,
                                str0m::media::KeyframeRequestKind::Fir,
                            );
                            last_kf_request = Some(now);
                            last_kf_rid = data.rid;
                        }
                        seen_video = true;
                        let core_codec = match data.params.spec().codec {
                            str0m::format::Codec::H264 => Some(Codec::H264),
                            str0m::format::Codec::H265 => Some(Codec::Hevc),
                            str0m::format::Codec::Vp9 => Some(Codec::Vp9),
                            str0m::format::Codec::Av1 => Some(Codec::Av1),
                            _ => None,
                        };
                        if is_camera {
                            // 摄像头轨：独立计数 + 独立解码器。
                            camera_frames += 1;
                            camera_bytes += data.data.len() as u64;
                            if let Some(cc) = core_codec {
                                if camera_decoder
                                    .as_ref()
                                    .map(|d| d.codec() != Some(cc))
                                    .unwrap_or(true)
                                {
                                    camera_decoder =
                                        cli_video_decoder::CliVideoDecoder::new(cc).ok();
                                }
                                if let Some(dec) = &mut camera_decoder
                                    && let Some(au) = camera_assembler.push(
                                        data.data.as_ref(),
                                        data.time.as_micros(),
                                        data.is_keyframe(),
                                    )
                                {
                                    let unit = aerodesk_core::platform::EncodedUnit {
                                        data: au.data,
                                        keyframe: au.keyframe,
                                        pts_ms: 0,
                                        rtp_timestamp: 0,
                                    };
                                    if let Ok(Some(frame)) = dec.decode(&unit)
                                        && frame.raw.is_some()
                                    {
                                        camera_decoded += 1;
                                    }
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
                            if let Some(cc) = core_codec {
                                if video_decoder
                                    .as_ref()
                                    .map(|d| d.codec() != Some(cc))
                                    .unwrap_or(true)
                                {
                                    video_decoder =
                                        cli_video_decoder::CliVideoDecoder::new(cc).ok();
                                }
                                if let Some(dec) = &mut video_decoder
                                    && let Some(au) = screen_assembler.push(
                                        data.data.as_ref(),
                                        data.time.as_micros(),
                                        data.is_keyframe(),
                                    )
                                {
                                    let unit = aerodesk_core::platform::EncodedUnit {
                                        data: au.data,
                                        keyframe: au.keyframe,
                                        pts_ms: 0,
                                        rtp_timestamp: 0,
                                    };
                                    if let Ok(Some(frame)) = dec.decode(&unit)
                                        && frame.raw.is_some()
                                    {
                                        decoded_frames += 1;
                                        // AERODESK_DUMP_FRAME：命中第 dump_after 帧就把 RGBA 落盘成 PNG。
                                        if let (Some(path), Some(raw)) = (&dump_frame, &frame.raw)
                                            && decoded_frames == dump_after
                                        {
                                            match rgba_to_png(raw, frame.width, frame.height) {
                                                Some(png) => match std::fs::write(path, &png) {
                                                    Ok(()) => info!(
                                                        "dump-frame: 第 {decoded_frames} 帧已落盘 → {path} ({}x{} RGBA)",
                                                        frame.width, frame.height
                                                    ),
                                                    Err(e) => {
                                                        tracing::warn!(
                                                            "dump-frame 写盘失败 {path}: {e}"
                                                        )
                                                    }
                                                },
                                                None => tracing::warn!("dump-frame: PNG 编码失败"),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                ClientEvent::IceConnected => info!("ICE connected"),
                ClientEvent::ChannelOpen(label, _) if label == "file" => {
                    // #122：请求被控端发送指定文件（配合 --recv-dir 落盘）。
                    if !file_request_sent && request_file.is_some() {
                        let req = aerodesk_core::protocol::file::FileControl::Request {
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
                    // #553 验收发现：control 类消息只在 control 通道就绪后发送——
                    // input 先开时对端（pub/SFU）control 通道可能未就绪，消息被
                    // 丢弃且 sent 标志已置位不再重发（bitrate/display e2e CI 连败
                    // 根因）；control 打开事件必然晚于或等于双向通道就绪。
                    if label == "control" {
                        // #553：对端（pub/SFU）control 通道可能晚于本端 ~1s 打开
                        // （macOS vt 环境实测：本端 ChannelOpen(control) 时对端
                        // SCTP 流未就绪，消息被静默丢弃且 sent 标志已置位不再重发）
                        // ——发送前短暂等待对端就绪（一次性，消息幂等无副作用）。
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        // #29：可选显式选层（--layer q|h|f），经 control 通道发 SFU。
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
                        // #267 测试/自动化钩子：--send-control '<json>' 经 control 通道下发一次
                        // （如 {"bitrate":N}，验证码率反馈回路）。
                        if let Some(ctl) = send_control
                            && !control_sent
                        {
                            if endpoint.send_channel_data("control", false, ctl.as_bytes()) {
                                info!("control command sent: {ctl}");
                                control_sent = true;
                            }
                        }
                    }
                }
                // #109 远程命令：响应打印 stdout/stderr/exit code 后退出（exit code 语义）。
                ClientEvent::ChannelData(cid, _, data)
                    if endpoint.channel_label(cid).as_deref() == Some("cmd") =>
                {
                    // 仅在响应真正解析成功时处理并退出：坏包（解析失败）不影响
                    // 后续重传（旧实现先置 cmd_done 再解析，一次坏包即终止重传、
                    // 控制端永远等不到响应、无法退出；成功路径必然 process::exit，
                    // 标志位本身不再需要）。
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
                // #75 远程光标：被控端广播位置，观看端日志（节流）。
                ClientEvent::ChannelData(cid, _, data)
                    if endpoint.channel_label(cid).as_deref() == Some("cursor") =>
                {
                    if let Ok(pos) =
                        serde_json::from_slice::<aerodesk_core::protocol::cursor::CursorPos>(&data)
                    {
                        if last_cursor_log.elapsed() >= Duration::from_secs(1) {
                            info!("CURSOR: x={:.3} y={:.3}", pos.x, pos.y);
                            last_cursor_log = Instant::now();
                        }
                        // #8 端到端延迟：本地墙钟 - 发送墙钟（同机/同一时钟域有效）。
                        // #253：节流 200ms（30Hz cursor 下 ~5 样本/s），p99 更快收敛；
                        // 真实验收需 ≥30 样本（runbook）。
                        let now = now_ms();
                        if pos.sent_ms > 0
                            && now >= pos.sent_ms
                            && last_latency_log.elapsed() >= Duration::from_millis(200)
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
        // #109 远程命令：未收到响应前每 1s 重传请求（首包丢失自愈；响应到达
        // 即退出进程，无需停止标志）。
        if cmd_pending && last_cmd_send.elapsed() >= Duration::from_secs(1) {
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
                "RECEIVED: {frames} frames, {bytes} bytes, {keyframes} keyframes, DECODED: {decoded_frames}, CAMERA: {camera_frames} frames {camera_bytes} bytes decoded={camera_decoded}, input sent: {input_seq}, AUDIO: {audio_frames} frames {audio_bytes} bytes muted={audio_muted} dropped={dropped_audio_frames} AVSYNC: audio={audio_ms:.0}ms video={video_ms:.0}ms drift={drift_ms:.0}ms buffered={buffered} dropped={jdropped} played={audio_played}"
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

#[cfg(not(target_os = "windows"))]
fn publisher_x264(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    simulcast: bool,
    noisy: bool,
    audio: bool,
    audio_opus: bool,
) {
    use aerodesk_codec::softenc::encode::X264Encoder;
    use aerodesk_core::synthetic::SyntheticSource;
    use str0m::media::Rid;

    const FPS: u32 = 30;
    const W: u32 = 640;
    const H: u32 = 360;

    let (_signal, mut endpoint, mut socket, video_mid, audio_mid, _camera_mid) =
        match connect_h264(signal_url, room, Role::Publisher, auth, simulcast, audio) {
            Ok(v) => v,
            Err(e) => {
                error!("connect failed: {e}");
                return;
            }
        };

    let make_source = |w: u32, h: u32| {
        if noisy {
            SyntheticSource::new_noisy(w, h)
        } else {
            SyntheticSource::new(w, h)
        }
    };

    // (rid, encoder, source)：单层 rid=None；simulcast 为 q/h/f 三层。
    // 编码器构建失败报错退出（旧 expect 一错即 panic）。
    let layer_specs: Vec<(Option<&str>, u32, u32, u32)> = if simulcast {
        SIMULCAST_LAYERS_X264
            .iter()
            .map(|(rid, w, h, kbps)| (Some(*rid), *w, *h, *kbps))
            .collect()
    } else {
        vec![(None, W, H, 800)]
    };
    let mut layers: Vec<(Option<Rid>, X264Encoder, SyntheticSource)> = Vec::new();
    for (rid, w, h, kbps) in layer_specs {
        let encoder = match X264Encoder::new(w, h, FPS, kbps) {
            Ok(e) => e,
            Err(e) => {
                error!("x264 encoder init failed ({w}x{h}@{FPS}): {e}");
                return;
            }
        };
        layers.push((rid.map(|r| Rid::from(r)), encoder, make_source(w, h)));
    }

    let mut connected = false;
    let mut audio_ticker = AudioTicker::new(audio_opus);
    let mut next_frame = Instant::now();
    let mut pts = 0i64;

    loop {
        // 会话失效（ICE 断开/对端离开）时干净退出循环，不再空转采集/编码。
        if !endpoint.is_alive() {
            warn!("endpoint dead, exiting publisher loop");
            break;
        }
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
                // 运行期编码错误降级为丢帧 + 告警（与 VT 路径同口径，不 panic 会话）。
                match encoder.encode(rgb) {
                    Ok(Some(frame)) => {
                        if frame.keyframe {
                            info!("sent keyframe rid={rid:?} #{pts}");
                        }
                        frames.push((*rid, frame.data));
                    }
                    Ok(None) => {}
                    Err(e) => warn!("encode failed（丢帧继续）: {e}"),
                }
            }
            send_frame_layers(&mut endpoint, video_mid, rtp_time, &frames);
            // simulcast：每层一帧都需一次 do_payload，多排空避免 WriteWithoutPoll 背压。
            drain_payload_queue(&mut endpoint, layers.len());

            pts += 1;
        }

        std::thread::sleep(Duration::from_millis(2));
    }
}
/// VideoToolbox 硬编发布端：合成 BGRA → 硬编 → SFU。
/// 压测可传 --width/--height/--fps/--bitrate（如 3840x2160@60 8Mbps）。
/// `--simulcast` 时编码 q/h/f 三层（SFU 选层生效）。
#[cfg(target_os = "macos")]
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
    use aerodesk_core::synthetic::SyntheticSource;
    use aerodesk_platform::macos::vt_encoder::VtEncoder;
    use str0m::media::Rid;

    let (_signal, mut endpoint, mut socket, video_mid, audio_mid, _camera_mid) =
        match connect_h264(signal_url, room, Role::Publisher, auth, simulcast, audio) {
            Ok(v) => v,
            Err(e) => {
                error!("connect failed: {e}");
                return;
            }
        };

    let make_source = |w: u32, h: u32| {
        if noisy {
            SyntheticSource::new_noisy(w, h)
        } else {
            SyntheticSource::new(w, h)
        }
    };

    // (rid, encoder, source)：单层 rid=None；simulcast 为 q/h/f 三层。
    // 编码器构建失败报错退出（旧 expect 一错即 panic）。
    let layer_specs: Vec<(Option<&str>, u32, u32, u32)> = if simulcast {
        SIMULCAST_LAYERS_VT
            .iter()
            .map(|(rid, w, h, bps)| (Some(*rid), *w, *h, *bps))
            .collect()
    } else {
        vec![(None, width, height, bitrate)]
    };
    let mut layers: Vec<(Option<Rid>, VtEncoder, SyntheticSource)> = Vec::new();
    for (rid, w, h, bps) in layer_specs {
        let encoder = match VtEncoder::new(w, h, fps, bps) {
            Ok(e) => e,
            Err(e) => {
                error!("vt encoder init failed ({w}x{h}@{fps}): {e}");
                return;
            }
        };
        layers.push((rid.map(|r| Rid::from(r)), encoder, make_source(w, h)));
    }
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
        // 会话失效（ICE 断开/对端离开）时干净退出循环，不再空转采集/编码。
        if !endpoint.is_alive() {
            warn!("endpoint dead, exiting publisher loop");
            break;
        }
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
                // #136 关键帧请求：SFU/新 viewer 请求 IDR。VT 硬编必须显式
                // force keyframe，否则要等自然 IDR（最长 2s，甚至花屏）。
                ClientEvent::KeyframeRequest(req) => {
                    for (rid, enc, _) in layers.iter_mut() {
                        if req.rid.is_none() || *rid == req.rid {
                            match enc.force_keyframe() {
                                Ok(()) => info!("vt keyframe requested (rid={:?})", req.rid),
                                Err(e) => warn!("vt force keyframe failed: {e}"),
                            }
                        }
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
    }
}
/// FFmpeg 发布端（#74）：合成 RGB → FfmpegEncoder（H264/H265/VP9/AV1）→ SFU。
/// `--codec h264|h265|vp9|av1` 选择编码格式；AV1(SVT) 有 ~1s 编码延迟。
/// 泛型发布循环（跨平台抽象落地 #277）：只依赖 core 的 `MediaSource` + `Encoder`，
/// 平台具体采集器/编码器由调用方构造后传入。cfg 只出现在调用方（适配器工厂）。
#[allow(clippy::too_many_arguments)]
fn publisher_generic<
    S: aerodesk_core::platform::MediaSource,
    E: aerodesk_core::platform::Encoder,
    C: aerodesk_core::platform::AudioCapturer<Error = String>,
    CC: aerodesk_core::platform::CameraSource<Error = String>,
    CS: aerodesk_core::platform::CursorSource,
>(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    audio: bool,
    audio_opus: bool,
    codec: Codec,
    fps: u32,
    mut source: S,
    mut encoder: E,
    audio_cap: Option<C>,
    camera_cap: Option<CC>,
    mut cursor: Option<CS>,
) {
    let (_signal, mut endpoint, mut socket, video_mid, audio_mid, camera_mid) =
        match if camera_cap.is_some() {
            connect_camera(signal_url, room, Role::Publisher, auth, audio, Some(codec))
        } else {
            connect_codec(signal_url, room, Role::Publisher, auth, audio, codec)
        } {
            Ok(v) => v,
            Err(e) => {
                error!("connect failed: {e}");
                return;
            }
        };
    let mut connected = false;
    // #316：有真实系统音频采集则优先（RealAudioSender），否则合成音 AudioTicker。
    let mut real_audio = audio_cap.map(|cap| RealAudioSender::new(cap, audio_opus));
    let mut audio_ticker = AudioTicker::new(audio_opus);
    // #385：摄像头第二路视频轨（--camera，BGRA → FFmpeg 软编 → camera_mid）。
    let mut camera_cap = camera_cap;
    let mut camera_enc: Option<aerodesk_codec::encode::FfmpegEncoder> = None;
    // 摄像头编码器初始化失败（如奇数分辨率）时停用摄像头轨，避免逐帧重试刷告警。
    let mut camera_dead = false;
    let mut camera_pts = 0i64;
    let mut next_camera = Instant::now();
    let mut next_frame = Instant::now();
    let mut pts = 0i64;
    // #477 机制 B（静态屏零输出）：缓存末帧原始像素。屏幕静止时变化驱动采集源
    // 不再产出新帧，晚加入 viewer 永远等不到首帧（服务器端录制零字节实证）；
    // 无新帧 ≥2s 时以缓存末帧强制 IDR 心跳重发。（M1 误删回归，自 main 恢复。）
    let mut last_frame_raw: Option<(Vec<u8>, u32, u32)> = None;
    let mut last_capture_at = Instant::now();
    // #8 端到端延迟：合成光标轨迹（30Hz）。
    let cursor_start = Instant::now();
    let mut last_cursor = Instant::now();

    loop {
        // 会话失效（ICE 断开/对端离开）时干净退出循环，不再空转采集/编码。
        if !endpoint.is_alive() {
            warn!("endpoint dead, exiting publisher loop");
            break;
        }
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
                    info!("ICE connected, starting generic stream (codec={codec:?})");
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
                // #58 显示器切换：viewer 经 control 通道请求 → 运行中切换采集源
                // （Windows DxgiCapturer 重建；其余源默认不支持则告警保持现状）。
                ClientEvent::ChannelData(cid, _, data)
                    if endpoint.channel_label(cid).as_deref() == Some("control") =>
                {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
                        if let Some(n) = v.get("display").and_then(|d| d.as_u64()) {
                            match source.switch_display(n as u32) {
                                Ok(()) => {
                                    info!("display switch -> display {n}");
                                    // #75：切换后同步注入坐标与光标基准到新显示器区域。
                                    #[cfg(target_os = "windows")]
                                    {
                                        if let Some(rect) = source.display_rect() {
                                            if let Ok(mut guard) = ACTIVE_DISPLAY_RECT.lock() {
                                                *guard = Some(rect);
                                            }
                                            if let Some(c) = &mut cursor {
                                                aerodesk_core::platform::CursorSource::
                                                    set_active_display(c, Some(rect));
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("display switch failed（保持当前显示器）: {e}");
                                }
                            }
                        }
                        if let Some(bps) = v.get("bitrate").and_then(|b| b.as_u64()) {
                            // #267 码率反馈：真实屏幕发布端（Windows/Linux）应用 BWE
                            // 降/升档（对齐 macOS publisher_screen；FFmpeg 编码器按需重建）。
                            encoder.set_bitrate(bps, fps);
                            info!("control: bitrate feedback applied -> {bps} bps");
                        }
                    }
                }
                ev => handle_publisher_input(&mut endpoint, ev),
            }
        }

        // #58 音频 + #72 文件传输推进：真实系统音频优先，合成音回退。
        if let Some(amid) = audio_mid {
            if let Some(ra) = &mut real_audio {
                ra.tick(&mut endpoint, amid, Instant::now());
            } else {
                audio_ticker.tick(&mut endpoint, amid, Instant::now());
            }
        }
        file_transfer::tick(&mut endpoint);
        cmd_exec::tick(&mut endpoint);
        // #8 端到端延迟：合成光标轨迹（30Hz）；#75 有真实 CursorSource（Linux X11）时
        // 优先真实光标，真实源不可用（如 Wayland 无 X11）时回退合成轨迹，cursor 通道常活。
        if last_cursor.elapsed() >= Duration::from_millis(33) {
            last_cursor = Instant::now();
            let (x, y) = match &mut cursor {
                Some(c) => c
                    .position_normalized()
                    .unwrap_or_else(|| synthetic_cursor_pos(cursor_start)),
                None => synthetic_cursor_pos(cursor_start),
            };
            send_cursor(&mut endpoint, x, y);
        }

        if connected && Instant::now() >= next_frame {
            next_frame += frame_interval(fps);
            // 屏幕源无帧（DXGI 静态画面返回 None）或瞬时错误：只跳过本帧屏幕
            // 编码/发送，摄像头轨与循环节拍照常（旧 continue 会一并跳过，
            // 屏幕静止时摄像头轨冻结、sleep 节拍失效）。
            // 运行期编码错误降级为丢帧 + 告警：硬件编码器可能瞬时失败（模式切换/
            // 设备丢失），不应 panic 整个发布端（旧实现 expect 一错即崩）。
            match source.next_frame() {
                Ok(Some(frame)) => {
                    // #477 机制 B：缓存末帧供静态屏心跳重发。
                    if let Some(raw) = frame.raw.as_ref() {
                        last_frame_raw = Some((raw.clone(), frame.width, frame.height));
                    }
                    last_capture_at = Instant::now();
                    match encoder.encode(&frame) {
                        Ok(Some(unit)) => {
                            let rtp_time = str0m::media::MediaTime::new(
                                pts as u64 * 3000,
                                str0m::media::Frequency::NINETY_KHZ,
                            );
                            if let Err(e) =
                                endpoint.send_video_frame(video_mid, unit.data, rtp_time)
                            {
                                warn!("send frame failed: {e:?}");
                            }
                            if unit.keyframe {
                                debug!("sent keyframe #{pts}");
                            }
                            pts += 1;
                        }
                        Ok(None) => {}
                        Err(e) => warn!("encode failed（丢帧继续）: {e}"),
                    }
                }
                Ok(None) => {
                    // #477 机制 B：无新帧 ≥2s（静态屏）时以缓存末帧强制 IDR 心跳
                    // 重发，保证晚加入 viewer 在 ≤2s 内拿到可解码关键帧（静态 IDR
                    // 帧间压缩后仅几十 KB，带宽可忽略）。
                    if last_capture_at.elapsed() >= Duration::from_secs(2)
                        && let Some((raw, w, h)) = last_frame_raw.as_ref()
                    {
                        last_capture_at = Instant::now();
                        let hb = aerodesk_core::platform::VideoFrame {
                            platform: None,
                            handle: None,
                            raw: Some(raw.clone()),
                            width: *w,
                            height: *h,
                            pts_ms: 0,
                        };
                        encoder.request_keyframe();
                        debug!("static-screen heartbeat IDR");
                        // 重建后的编码器需喂满管线深度才吐包（本机 h264_mf 实测
                        // 12 帧；libx264 约 1-2 帧）。静屏没有"下一帧"来冲刷——
                        // 连喂同帧 16 次（超出部分仅产生近零字节的重复 P 帧，
                        // 每 2s 约 30-60ms CPU，可忽略）。
                        for _ in 0..16 {
                            if let Ok(Some(unit)) = encoder.encode(&hb) {
                                let rtp_time = str0m::media::MediaTime::new(
                                    pts as u64 * 3000,
                                    str0m::media::Frequency::NINETY_KHZ,
                                );
                                if let Err(e) =
                                    endpoint.send_video_frame(video_mid, unit.data, rtp_time)
                                {
                                    warn!("heartbeat send failed: {e:?}");
                                }
                                pts += 1;
                            }
                        }
                    }
                }
                Err(e) => warn!("source next_frame: {e}"),
            }
        }

        // #385 摄像头第二路视频轨：30fps 节拍，BGRA → FfmpegEncoder → camera_mid。
        // ICE 未连通（connected=false）时不轮询摄像头，避免空转与 send 失败刷屏。
        if connected
            && !camera_dead
            && let Some(cmid) = camera_mid
            && let Some(cap) = &mut camera_cap
            && Instant::now() >= next_camera
        {
            next_camera += Duration::from_millis(33);
            match aerodesk_core::platform::CameraSource::next_frame(cap) {
                Ok(Some(frame)) => {
                    if camera_enc.is_none() {
                        // 编码器初始化失败降级停用摄像头轨（仅告警一次，不 panic；
                        // 旧实现 expect 在奇数分辨率等场景一错即崩）。
                        camera_enc = match aerodesk_codec::encode::FfmpegEncoder::new(
                            frame.width,
                            frame.height,
                            30,
                            8_000_000,
                            codec,
                        ) {
                            Ok(enc) => Some(enc),
                            Err(e) => {
                                warn!("camera encoder init failed（停用摄像头轨）: {e}");
                                camera_dead = true;
                                None
                            }
                        };
                    }
                    if let Some(enc) = &mut camera_enc {
                        match enc.encode_bgra(&frame.raw) {
                            Ok(Some(unit)) => {
                                let rtp_time = str0m::media::MediaTime::new(
                                    camera_pts as u64 * 3000,
                                    str0m::media::Frequency::NINETY_KHZ,
                                );
                                if let Err(e) = endpoint.send_video_frame(cmid, unit.data, rtp_time)
                                {
                                    warn!("send camera frame failed: {e:?}");
                                }
                                camera_pts += 1;
                            }
                            // 编码错误与屏幕轨同口径告警，不再静默丢帧。
                            Ok(None) => {}
                            Err(e) => warn!("camera encode failed（丢帧继续）: {e}"),
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => warn!("camera next_frame: {e}"),
            }
        }

        std::thread::sleep(Duration::from_millis(2));
    }
}

/// FFmpeg 软编发布端（合成源，全平台可用）：SyntheticSource + FfmpegEncoder，
/// 走泛型 `publisher_generic`（#277 消费方泛型化证明）。
#[allow(clippy::too_many_arguments)] // #8 参数化合成源（分辨率/fps/码率），与既有 publisher 系列同风格。
fn publisher_ffmpeg(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    audio: bool,
    audio_opus: bool,
    codec: Codec,
    noisy: bool,
    w: u32,
    h: u32,
    fps: u32,
    bitrate: u32,
) {
    use aerodesk_core::synthetic::SyntheticSource;

    let encoder = match FfmpegEncoder::new(w, h, fps, bitrate as u64, codec) {
        Ok(e) => e,
        Err(e) => {
            error!("ffmpeg encoder init failed ({w}x{h}@{fps} {codec:?}): {e}");
            return;
        }
    };
    // #8：--noisy 高熵合成源（码率贴近目标档位，压测/高码率回归用）。
    let source = if noisy {
        SyntheticSource::new_noisy(w, h)
    } else {
        SyntheticSource::new(w, h)
    };
    publisher_generic(
        signal_url,
        room,
        auth,
        audio,
        audio_opus,
        codec,
        fps,
        source,
        encoder,
        None::<NoAudioCapture>,
        None::<NoCameraCapture>,
        None::<NoCursor>,
    );
}

/// 真实屏幕采集发布端：ScreenCaptureKit → VideoToolbox 硬编（零拷贝）→ SFU。
/// 需要屏幕录制权限（TCC）。`--simulcast`：q/h/f 三层各一路 SCK 采集 + 硬编，
/// 分辨率越低开销越小，选层切换立即生效。
/// 屏幕采集 + FFmpeg 多 codec（#74）：ScreenCaptureKit → IOSurface → BGRA →
/// FfmpegEncoder（H265/VP9/AV1）。H.264 走原 VtEncoder 零拷贝路径。
/// 需要屏幕录制权限（TCC）。
/// Windows 屏幕采集发布端（被控端）：WGC（主，#514）/DXGI（备）→ FFmpeg 编码 → SFU。
/// 输入注入走 SendInput（aerodesk-platform）；系统音频走 WASAPI loopback
/// （采集系统正在播放的声音，失败回退合成音）；需要交互桌面会话（DXGI 输出可用）。
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)] // 采集参数（显示器/缩放/码率/编解码），与既有 publisher 系列同风格。
fn publisher_capture_windows(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    audio: bool,
    audio_opus: bool,
    codec: Codec,
    target_w: u32,
    target_h: u32,
    display: u32,
    bitrate: u32,
    camera: bool,
    camera_device: Option<String>,
) {
    use aerodesk_core::platform::MediaSource;
    use aerodesk_platform::windows::capture::ScreenCapturer;

    const FPS: u32 = 30;

    // #514 采集链：WGC 主（DWM 出帧，不受适配器/输出枚举序影响）→ DXGI 备。
    let mut capture = match ScreenCapturer::new_with_display(display, target_w, target_h) {
        Ok(c) => c,
        Err(e) => {
            error!("screen capture init failed: {e}");
            info!("Windows 屏幕采集需要交互桌面会话（非 headless/服务会话）");
            return;
        }
    };
    // #75 多显示器：注入坐标按被控显示器在虚拟屏幕中的区域映射。
    if let Ok(mut guard) = ACTIVE_DISPLAY_RECT.lock() {
        *guard = Some(capture.display_rect());
    }
    let (w, h) = capture.size();
    if w == 0 || h == 0 {
        error!("screen capture: 无可用显示器输出");
        return;
    }
    // #75 远程光标：真实光标按被控显示器区域归一化（在 capture 移入 publisher 前取值）。
    let display_rect = capture.display_rect();
    info!("Windows screen capture started at {w}x{h}");
    // #334：采集会话期间保持系统/显示器唤醒（防闲置休眠后 DXGI 无输出）。
    let _keep_awake = aerodesk_platform::windows::wake_lock::WindowsSystemWakeLock
        .acquire(true)
        .map_err(|e| warn!("保持显示器唤醒失败: {e}"))
        .ok();
    let _ = MediaSource::start(&mut capture, FPS, false);
    // #3/#8：屏幕采集改用 FFmpeg 编码器——Windows h264_mf/hevc_mf 硬件编码
    // （2560x1440/4K 源头不再受 OpenH264 软编瓶颈），不可用时自动回退
    // libx264/libx265 软编；同时让 --codec h265/vp9/av1 在屏幕采集路径真实生效。
    let encoder = match aerodesk_codec::encode::FfmpegEncoder::new(w, h, FPS, bitrate as u64, codec)
    {
        Ok(e) => e,
        Err(e) => {
            error!("FFmpeg encoder init failed: {e}");
            return;
        }
    };
    // #3 Windows 系统音频：WASAPI loopback 采集系统播放的声音；失败回退合成音。
    let audio_cap: Option<aerodesk_platform::windows::audio_capture::WasapiLoopbackCapture> =
        if audio {
            match aerodesk_platform::windows::audio_capture::WasapiLoopbackCapture::start() {
                Ok(cap) => {
                    info!("Windows system audio capture started (WASAPI loopback)");
                    Some(cap)
                }
                Err(e) => {
                    warn!("WASAPI capture failed, fallback synthetic: {e}");
                    None
                }
            }
        } else {
            None
        };
    // #385 摄像头（MF SourceReader）：--camera 时启动本地摄像头第二路视频轨，
    // 失败仅告警（屏幕视频轨照常）；SourceReader 输出 RGB32/BGRA。
    let camera_cap: Option<aerodesk_platform::windows::camera::MfCamera> = if camera {
        match aerodesk_platform::windows::camera::MfCamera::new(camera_device.as_deref()) {
            Ok(mut cam) => match cam.start(1280, 720, FPS) {
                Ok(()) => {
                    info!("Windows camera capture started (device={camera_device:?})");
                    Some(cam)
                }
                Err(e) => {
                    warn!("camera capture disabled: {e}");
                    None
                }
            },
            Err(e) => {
                warn!("camera capture disabled: {e}");
                None
            }
        }
    } else {
        None
    };
    publisher_generic(
        signal_url,
        room,
        auth,
        audio,
        audio_opus,
        codec,
        FPS,
        capture,
        encoder,
        audio_cap,
        camera_cap,
        // #75 远程光标：Windows 被控端真实光标位置（GetCursorPos，活动显示器归一化）。
        Some(aerodesk_platform::windows::cursor::WindowsCursor::new(
            Some(display_rect),
        )),
    );
}

/// Linux 屏幕采集发布端（被控端）：X11（DISPLAY 会话，x11rb GetImage）或
/// Wayland（xdg-desktop-portal ScreenCast + PipeWire）→ 编码 → SFU。
/// H.264：VAAPI 硬编优先（#282），x264 软编回退；HEVC/VP9/AV1 走 FFmpeg 软编。
/// 输入注入：XTest（X11）/ uinput（Wayland/无 X，见 `inject_input`）。
#[cfg(target_os = "linux")]
fn publisher_capture_linux(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    audio: bool,
    audio_opus: bool,
    codec: Codec,
    camera: bool,
    camera_device: Option<String>,
) {
    use aerodesk_core::platform::MediaSource;
    use aerodesk_platform::linux::capture::{WaylandPortalCapturer, X11Capturer};

    const FPS: u32 = 30;

    // #385 摄像头（V4L2）：--camera 时启动本地摄像头，失败仅告警（视频轨照常）。
    let camera_cap: Option<aerodesk_platform::linux::camera::V4l2Camera> = if camera {
        let dev = camera_device.unwrap_or_else(|| "/dev/video0".to_string());
        match aerodesk_platform::linux::camera::V4l2Camera::new(&dev) {
            Ok(mut cam) => {
                use aerodesk_core::platform::CameraSource;
                match CameraSource::start(&mut cam, 1280, 720, 30) {
                    Ok(()) => {
                        info!("Linux camera started ({dev})");
                        Some(cam)
                    }
                    Err(e) => {
                        warn!("Linux camera start failed ({dev}): {e}");
                        None
                    }
                }
            }
            Err(e) => {
                warn!("Linux camera open failed ({dev}): {e}");
                None
            }
        }
    } else {
        None
    };

    // DISPLAY 存在 → X11；否则 → Wayland portal（PipeWire，触发用户授权）。
    let mut capture = if std::env::var("DISPLAY").is_ok() {
        match X11Capturer::new() {
            Ok(c) => LinuxScreenSource::X11(c),
            Err(e) => {
                error!("X11 capture init failed: {e}");
                return;
            }
        }
    } else {
        match WaylandPortalCapturer::new() {
            Ok(c) => LinuxScreenSource::Wayland(c),
            Err(e) => {
                error!("Wayland portal capture init failed: {e}");
                return;
            }
        }
    };
    // start 先行：Wayland 需 portal 会话建立后才能拿到流尺寸；X11 start 为 no-op。
    if let Err(e) = MediaSource::start(&mut capture, FPS, false) {
        error!("capture start failed: {e}");
        info!(
            "Linux 屏幕采集：X11 需 DISPLAY 会话；Wayland 需 xdg-desktop-portal + PipeWire + 用户授权"
        );
        return;
    }
    let (w, h) = capture.size();
    if w == 0 || h == 0 {
        error!("capture: 无可用显示器输出");
        return;
    }
    info!("Linux screen capture started at {w}x{h}");

    let encoder = match codec {
        Codec::H264 => {
            // VAAPI 硬编优先，失败回退 x264 软编。
            match aerodesk_platform::linux::vaapi::VaapiEncoder::new(
                w,
                h,
                FPS,
                8_000_000,
                Codec::H264,
            ) {
                Ok(e) => {
                    info!("Linux screen encoder: VAAPI (h264_vaapi)");
                    LinuxScreenEncoder::Vaapi(e)
                }
                Err(e) => {
                    warn!("VAAPI encoder unavailable ({e})，回退 x264 软编");
                    match aerodesk_platform::linux::encode::SoftEncoder::new(w, h, FPS, 8_000) {
                        Ok(e) => LinuxScreenEncoder::Soft(e),
                        Err(e) => {
                            error!("x264 encoder init failed: {e}");
                            return;
                        }
                    }
                }
            }
        }
        other => {
            // HEVC/VP9/AV1：FFmpeg 软编（BGRA 输入，全平台 aerodesk-codec）。
            match aerodesk_codec::encode::FfmpegEncoder::new(w, h, FPS, 8_000_000, other) {
                Ok(e) => LinuxScreenEncoder::Ffmpeg(e),
                Err(e) => {
                    error!("ffmpeg encoder init failed: {e}");
                    return;
                }
            }
        }
    };
    // #334：采集会话期间保持显示器唤醒（防系统/显示器休眠导致采集失效）。
    let _keep_awake = SystemWakeLock::acquire(
        &aerodesk_platform::linux::wake_lock::LinuxSystemWakeLock,
        true,
    )
    .map_err(|e| warn!("保持显示器唤醒失败: {e}"))
    .ok();
    // #316 Linux 系统音频（PipeWire sink 捕获）：可用则真实音频，失败回退合成音。
    let audio_cap: Option<aerodesk_platform::linux::audio::SystemAudioCapture> = if audio {
        match aerodesk_platform::linux::audio::SystemAudioCapture::new() {
            Ok(cap) => {
                info!("Linux system audio capture started (PipeWire)");
                Some(cap)
            }
            Err(e) => {
                warn!("Linux system audio capture failed, fallback synthetic: {e}");
                None
            }
        }
    } else {
        None
    };
    publisher_generic(
        signal_url,
        room,
        auth,
        audio,
        audio_opus,
        codec,
        FPS,
        capture,
        encoder,
        audio_cap,
        camera_cap,
        Some(aerodesk_platform::linux::cursor::LinuxCursor::new()),
    );
}

/// Linux 屏幕采集源：X11 x11rb / Wayland portal（PipeWire）。
#[cfg(target_os = "linux")]
#[allow(clippy::large_enum_variant)]
enum LinuxScreenSource {
    X11(aerodesk_platform::linux::capture::X11Capturer),
    Wayland(aerodesk_platform::linux::capture::WaylandPortalCapturer),
}

#[cfg(target_os = "linux")]
impl LinuxScreenSource {
    fn size(&self) -> (u32, u32) {
        match self {
            Self::X11(c) => c.size(),
            Self::Wayland(c) => c.size(),
        }
    }
}

#[cfg(target_os = "linux")]
impl aerodesk_core::platform::MediaSource for LinuxScreenSource {
    type Error = String;

    fn start(&mut self, fps: u32, with_cursor: bool) -> Result<(), Self::Error> {
        match self {
            Self::X11(c) => aerodesk_core::platform::MediaSource::start(c, fps, with_cursor),
            Self::Wayland(c) => aerodesk_core::platform::MediaSource::start(c, fps, with_cursor),
        }
    }

    fn next_frame(&mut self) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        match self {
            Self::X11(c) => aerodesk_core::platform::MediaSource::next_frame(c),
            Self::Wayland(c) => aerodesk_core::platform::MediaSource::next_frame(c),
        }
    }

    fn stop(&mut self) {
        match self {
            Self::X11(c) => aerodesk_core::platform::MediaSource::stop(c),
            Self::Wayland(c) => aerodesk_core::platform::MediaSource::stop(c),
        }
    }
}

/// Linux 屏幕发布编码器：VAAPI 硬编 / x264 软编 / FFmpeg 软编（多 codec）。
#[cfg(target_os = "linux")]
#[allow(clippy::large_enum_variant)]
enum LinuxScreenEncoder {
    Vaapi(aerodesk_platform::linux::vaapi::VaapiEncoder),
    Soft(aerodesk_platform::linux::encode::SoftEncoder),
    Ffmpeg(aerodesk_codec::encode::FfmpegEncoder),
}

#[cfg(target_os = "linux")]
impl aerodesk_core::platform::Encoder for LinuxScreenEncoder {
    type Error = String;

    fn configure(
        &mut self,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), Self::Error> {
        match self {
            Self::Vaapi(e) => {
                aerodesk_core::platform::Encoder::configure(e, codec, width, height, fps)
            }
            Self::Soft(e) => {
                aerodesk_core::platform::Encoder::configure(e, codec, width, height, fps)
            }
            Self::Ffmpeg(e) => {
                aerodesk_core::platform::Encoder::configure(e, codec, width, height, fps)
            }
        }
    }

    fn encode(
        &mut self,
        frame: &aerodesk_core::platform::VideoFrame,
    ) -> Result<Option<aerodesk_core::platform::EncodedUnit>, Self::Error> {
        match self {
            Self::Vaapi(e) => aerodesk_core::platform::Encoder::encode(e, frame),
            Self::Soft(e) => aerodesk_core::platform::Encoder::encode(e, frame),
            Self::Ffmpeg(e) => aerodesk_core::platform::Encoder::encode(e, frame),
        }
    }

    fn request_keyframe(&mut self) {
        match self {
            Self::Vaapi(e) => aerodesk_core::platform::Encoder::request_keyframe(e),
            Self::Soft(e) => aerodesk_core::platform::Encoder::request_keyframe(e),
            Self::Ffmpeg(e) => aerodesk_core::platform::Encoder::request_keyframe(e),
        }
    }

    fn set_bitrate(&mut self, bitrate_bps: u64, fps: u32) {
        match self {
            Self::Vaapi(e) => aerodesk_core::platform::Encoder::set_bitrate(e, bitrate_bps, fps),
            Self::Soft(e) => aerodesk_core::platform::Encoder::set_bitrate(e, bitrate_bps, fps),
            Self::Ffmpeg(e) => aerodesk_core::platform::Encoder::set_bitrate(e, bitrate_bps, fps),
        }
    }
}

#[cfg(target_os = "macos")]
fn publisher_capture_ffmpeg(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    audio: bool,
    audio_opus: bool,
    codec: Codec,
    initial_display: usize,
) {
    use aerodesk_codec::encode::FfmpegEncoder;
    use aerodesk_platform::macos::capture::ScreenCapture;

    // 采集分辨率：0,0 = 按显示器原生宽高比等比缩放（与 VT 路径 publisher_capture
    // 一致）。固定 1920x1080 会拉伸非 16:9 显示器并使输入坐标错位。
    const W: u32 = 0;
    const H: u32 = 0;
    const FPS: u32 = 30;

    let (_signal, mut endpoint, mut socket, video_mid, audio_mid, _camera_mid) =
        match connect_codec(signal_url, room, Role::Publisher, auth, audio, codec) {
            Ok(v) => v,
            Err(e) => {
                error!("connect failed: {e}");
                return;
            }
        };
    let mut capture = match ScreenCapture::start(initial_display, FPS, W, H) {
        Ok(c) => c,
        Err(e) => {
            error!("screen capture init failed: {e}");
            info!("grant Screen Recording permission in System Settings > Privacy & Security");
            return;
        }
    };
    // #315：采集会话期间保持显示器唤醒（防闲置休眠后 SCK 无显示器）。
    let _keep_awake = aerodesk_platform::macos::wake_lock::MacSystemWakeLock
        .acquire(true)
        .map_err(|e| warn!("保持显示器唤醒失败: {e}"))
        .ok();
    // #75：输入注入坐标按被控显示器（不总是主屏）换算。
    aerodesk_platform::macos::inject::set_active_display(Some(capture.display_id()));
    // 编码分辨率 = 采集实际尺寸（保持显示器宽高比；旧固定 1920x1080 拉伸非 16:9）。
    let (w, h) = (capture.width(), capture.height());
    info!(
        "screen capture started at {w}x{h} (display {})",
        capture.display_id()
    );
    let mut encoder = match FfmpegEncoder::new(w, h, FPS, 8_000_000, codec) {
        Ok(e) => e,
        Err(e) => {
            error!("ffmpeg encoder init failed ({w}x{h}): {e}");
            return;
        }
    };
    let mut connected = false;
    let mut audio_ticker = AudioTicker::new(audio_opus);
    let mut pts = 0i64;
    // #75 远程光标：真实光标位置（30Hz）。
    let mut last_cursor = Instant::now();

    loop {
        // 会话失效（ICE 断开/对端离开）时干净退出循环，不再空转采集/编码。
        if !endpoint.is_alive() {
            warn!("endpoint dead, exiting publisher loop");
            break;
        }
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
            if let Some((x, y)) = aerodesk_platform::macos::cursor::cursor_position_normalized() {
                send_cursor(&mut endpoint, x, y);
            }
        }

        if connected && let Some(surface) = capture.capture_frame(Duration::from_millis(50)) {
            // IOSurface（BGRA）→ 行复制到 CPU 缓冲 → FFmpeg 编码。
            let bgra = match aerodesk_platform::macos::capture::surface_to_bgra(&surface, w, h) {
                Ok(b) => b,
                Err(e) => {
                    warn!("surface read failed: {e}");
                    continue;
                }
            };
            // 运行期编码错误降级为丢帧 + 告警（旧 expect 一错即崩整个发布端）。
            match encoder.encode_bgra(&bgra) {
                Ok(Some(unit)) => {
                    let rtp_time = str0m::media::MediaTime::new(
                        pts as u64 * 3000,
                        str0m::media::Frequency::NINETY_KHZ,
                    );
                    if let Err(e) = endpoint.send_video_frame(video_mid, unit.data, rtp_time) {
                        warn!("send frame failed: {e:?}");
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("encode_bgra failed（丢帧继续）: {e}");
                    continue;
                }
            }
            pts += 1;
        }

        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(target_os = "macos")]
fn publisher_capture(
    signal_url: &str,
    room: &str,
    auth: Option<&str>,
    simulcast: bool,
    audio: bool,
    audio_opus: bool,
    initial_display: usize,
    codec: Codec,
    camera: bool,
    camera_device: Option<String>,
) {
    use aerodesk_core::platform::CameraSource;
    use aerodesk_platform::macos::capture::ScreenCapture;
    use aerodesk_platform::macos::vt_encoder::VtEncoder;
    use str0m::media::Rid;

    const FPS: u32 = 30;
    // 采集分辨率：0,0 = 按显示器原生宽高比等比缩放（见 capture::build_capture）。
    // 保持与显示器同宽高比，避免画面拉伸导致输入坐标错位。
    const W: u32 = 0;
    const H: u32 = 0;
    // core Codec -> videotoolbox Codec（仅 H264/HEVC 走此路径；vp9/av1 走 ffmpeg 路径）。
    use videotoolbox::Codec as VtCodec;
    let vt_codec = match codec {
        Codec::Hevc => VtCodec::HEVC,
        _ => VtCodec::H264,
    };

    let (_signal, mut endpoint, mut socket, video_mid, audio_mid, camera_mid) = match connect_inner(
        signal_url,
        room,
        Role::Publisher,
        Some(codec),
        simulcast,
        audio,
        auth,
        camera,
    ) {
        Ok(v) => v,
        Err(e) => {
            error!("connect failed: {e}");
            return;
        }
    };

    // #75 远程光标：真实光标位置（30Hz）。
    let mut last_cursor = Instant::now();

    // #58 显示器切换：按 (display, w, h) 重建采集器（编码器与显示器无关，保留）。
    let rebuild_captures = |idx: usize,
                            layers: &mut Vec<(Option<Rid>, VtEncoder, ScreenCapture)>|
     -> Result<(), String> {
        let specs: Vec<(Option<Rid>, u32, u32)> = layers
            .iter()
            .map(|(rid, _enc, cap)| {
                // simulcast 固定层尺寸沿用原档位；单层传 0,0 让新显示器按原生
                // 宽高比重新缩放，避免切到不同宽高比屏幕后继续沿用旧尺寸拉伸。
                let (w, h) = if rid.is_some() {
                    (cap.width(), cap.height())
                } else {
                    (0, 0)
                };
                (*rid, w, h)
            })
            .collect();
        // 先在临时 Vec 中完整重建：任一层失败即返回 Err、旧层原样保留
        //（旧实现先 clear 再逐层重建 + expect，远端一条切换消息即可让
        // 发布端 panic，或把 layers 留成半重建状态）。
        let mut new_layers: Vec<(Option<Rid>, VtEncoder, ScreenCapture)> =
            Vec::with_capacity(specs.len());
        for (rid, w, h) in specs {
            let capture = ScreenCapture::start(idx, FPS, w, h)
                .map_err(|e| format!("display {idx} init failed: {e}"))?;
            let (cw, ch) = (capture.width(), capture.height());
            // 重建：编码器按分辨率新建；码率沿用原 simulcast 档位（按 rid 查
            // SIMULCAST_LAYERS_VT），单层沿用初始路径的 8Mbps。
            let bps = match rid {
                Some(r) => SIMULCAST_LAYERS_VT
                    .iter()
                    .find(|(lr, _, _, _)| *lr == &*r)
                    .map(|(_, _, _, bps)| *bps)
                    .unwrap_or(if cw >= 1280 { 8_000_000 } else { 4_000_000 }),
                None => 8_000_000,
            };
            let encoder = VtEncoder::new_with_codec(cw, ch, FPS, bps, vt_codec)
                .map_err(|e| format!("display {idx} encoder init failed: {e}"))?;
            new_layers.push((rid, encoder, capture));
        }
        // 全部层重建成功后原子替换。
        *layers = new_layers;
        // #75：切换显示器后输入注入坐标基准同步。
        if let Some((_, _, cap)) = layers.first() {
            aerodesk_platform::macos::inject::set_active_display(Some(cap.display_id()));
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
            let encoder = match VtEncoder::new_with_codec(*w, *h, FPS, *bps, vt_codec) {
                Ok(e) => e,
                Err(e) => {
                    error!("vt encoder init failed ({w}x{h}): {e}");
                    return;
                }
            };
            layers.push((Some(Rid::from(*rid)), encoder, capture));
        }
        if let Some((_, _, cap)) = layers.first() {
            aerodesk_platform::macos::inject::set_active_display(Some(cap.display_id()));
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
        // 编码分辨率 = 采集实际尺寸（保持显示器宽高比）。
        let (cw, ch) = (capture.width(), capture.height());
        info!(
            "screen capture started at {cw}x{ch} (display {}), codec={codec:?}",
            capture.display_id()
        );
        let encoder = match VtEncoder::new_with_codec(cw, ch, FPS, 8_000_000, vt_codec) {
            Ok(e) => e,
            Err(e) => {
                error!("vt encoder init failed ({cw}x{ch}): {e}");
                return;
            }
        };
        layers.push((None, encoder, capture));
        aerodesk_platform::macos::inject::set_active_display(Some(layers[0].2.display_id()));
    }
    // #315：采集会话期间保持显示器唤醒（防闲置休眠后 SCK 无显示器）。
    let _keep_awake = aerodesk_platform::macos::wake_lock::MacSystemWakeLock
        .acquire(true)
        .map_err(|e| warn!("保持显示器唤醒失败: {e}"))
        .ok();

    let mut connected = false;
    // #73 真实系统音频：SCK audio-only SCStream 采集本机正在播放的声音；
    // 采集失败/未开 --audio 时回退合成音 AudioTicker。
    let mut real_audio: Option<
        RealAudioSender<aerodesk_platform::macos::audio_capture::SystemAudioCapture>,
    > = None;
    if audio {
        match aerodesk_platform::macos::audio_capture::SystemAudioCapture::start() {
            Ok(cap) => {
                info!("system audio capture started (SCK audio)");
                real_audio = Some(RealAudioSender::new(cap, audio_opus));
            }
            Err(e) => warn!("system audio capture failed, fallback synthetic: {e}"),
        }
    }
    let mut audio_ticker = AudioTicker::new(audio_opus);
    let mut pts = 0i64;
    let pts_inc = 90_000 / FPS as i64;

    // 摄像头第二路视频轨（--camera）：AVFoundation 采集 + FFmpeg 软编（BGRA）。
    let mut camera_cap: Option<aerodesk_platform::macos::camera::MacCamera> = None;
    let mut camera_enc: Option<aerodesk_codec::encode::FfmpegEncoder> = None;
    let mut camera_pts = 0i64;
    let camera_pts_inc = 90_000 / 30;
    let mut camera_frames = 0u64;
    if camera {
        // 未授权时先弹系统授权框（TCC「相机」），授权后才真正启动采集。
        if !aerodesk_platform::macos::camera::camera_authorized() {
            info!("camera permission not granted, requesting…");
            if aerodesk_platform::macos::camera::request_camera_access() {
                info!("camera permission granted");
            } else {
                warn!("camera permission denied (System Settings > Privacy & Security > Camera)");
            }
        }
        let mut cam = aerodesk_platform::macos::camera::MacCamera::new();
        if let Some(id) = &camera_device {
            cam = cam.with_device(id.clone());
        }
        match cam.start(1280, 720, 30) {
            Ok(()) => {
                info!("camera capture started (device={camera_device:?})");
                camera_cap = Some(cam);
            }
            Err(e) => warn!("camera capture disabled: {e}"),
        }
    }

    loop {
        // 会话失效（ICE 断开/对端离开）时干净退出循环，不再空转采集/编码。
        if !endpoint.is_alive() {
            warn!("endpoint dead, exiting publisher loop");
            break;
        }
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
                // #136 关键帧请求：屏幕采集（VT 硬编）同样必须显式 force keyframe。
                ClientEvent::KeyframeRequest(req) => {
                    for (rid, enc, _) in layers.iter_mut() {
                        if req.rid.is_none() || *rid == req.rid {
                            match enc.force_keyframe() {
                                Ok(()) => {
                                    info!("vt capture keyframe requested (rid={:?})", req.rid)
                                }
                                Err(e) => warn!("vt capture force keyframe failed: {e}"),
                            }
                        }
                    }
                    // 摄像头轨关键帧（SFU 转发 mid 为发布端 mid，可按 mid 路由）。
                    if Some(req.mid) == camera_mid
                        && let Some(enc) = &mut camera_enc
                    {
                        enc.request_keyframe();
                    }
                }
                // #58 显示器切换：viewer 经 control 通道请求，SFU 转发到 publisher。
                // #267 码率反馈：SFU 经 control 通道下发 {"bitrate":N} → 编码器降档。
                ClientEvent::ChannelData(cid, binary, data) => {
                    if endpoint.channel_label(cid).as_deref() == Some("control") {
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
                            if let Some(n) = v.get("display").and_then(|d| d.as_u64()) {
                                info!("control: display switch request -> display {n}");
                                if let Err(e) = rebuild_captures(n as usize, &mut layers) {
                                    warn!("display switch failed（保持当前显示器）: {e}");
                                }
                            }
                            if let Some(bps) = v.get("bitrate").and_then(|b| b.as_u64()) {
                                info!("control: bitrate feedback -> {bps} bps");
                                for (_, enc, _) in layers.iter_mut() {
                                    aerodesk_core::platform::Encoder::set_bitrate(
                                        enc, bps, FPS as u32,
                                    );
                                }
                                // 摄像头编码器同样响应 BWE 反馈（FFmpeg 重建，节流 1s）。
                                if let Some(enc) = &mut camera_enc {
                                    aerodesk_core::platform::Encoder::set_bitrate(enc, bps, 30);
                                }
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

        // #58/#73 音频：真实系统音频（SCK）优先，否则合成音节拍器。
        if let Some(amid) = audio_mid {
            if let Some(sender) = &mut real_audio {
                sender.tick(&mut endpoint, amid, Instant::now());
            } else {
                audio_ticker.tick(&mut endpoint, amid, Instant::now());
            }
        }
        // #72 文件传输：推进发送。
        file_transfer::tick(&mut endpoint);
        cmd_exec::tick(&mut endpoint);
        // #75 远程光标：读取被控端真实光标位置（30Hz）。
        if last_cursor.elapsed() >= Duration::from_millis(33) {
            last_cursor = Instant::now();
            #[cfg(target_os = "macos")]
            if let Some((x, y)) = aerodesk_platform::macos::cursor::cursor_position_normalized() {
                send_cursor(&mut endpoint, x, y);
            }
        }

        if connected {
            // 每层各自采集一帧（simulcast 下 SCK 按层分辨率采集；单层维持原路径）。
            let mut frames = Vec::with_capacity(layers.len());
            let mut captured_any = false;
            for (rid, encoder, capture) in &mut layers {
                if let Some(surface) = capture.capture_frame(Duration::from_millis(50)) {
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
            // 摄像头：drain 帧 → FFmpeg 软编（BGRA）→ 第二路 mid。
            // 编码器按首帧实际尺寸懒创建（AVFoundation 分辨率不固定）。
            if let (Some(cam), Some(cmid)) = (&mut camera_cap, camera_mid) {
                match cam.next_frame() {
                    Ok(Some(frame)) => {
                        camera_frames += 1;
                        if camera_frames == 1 || camera_frames % 150 == 0 {
                            info!(
                                "camera frame #{camera_frames} {}x{}",
                                frame.width, frame.height
                            );
                        }
                        if camera_enc.is_none() {
                            let bps = if frame.width >= 1920 {
                                4_000_000
                            } else {
                                1_500_000
                            };
                            match aerodesk_codec::encode::FfmpegEncoder::new(
                                frame.width,
                                frame.height,
                                30,
                                bps,
                                codec,
                            ) {
                                Ok(enc) => {
                                    info!(
                                        "camera encoder ready {}x{} {codec:?}",
                                        frame.width, frame.height
                                    );
                                    camera_enc = Some(enc);
                                }
                                Err(e) => {
                                    warn!("camera encoder init failed (ffmpeg {codec:?}): {e}");
                                    camera_enc = None;
                                }
                            }
                        }
                        if let Some(enc) = &mut camera_enc {
                            match enc.encode_bgra(&frame.raw) {
                                Ok(Some(unit)) => {
                                    let rtp_time = str0m::media::MediaTime::new(
                                        camera_pts as u64 * camera_pts_inc as u64,
                                        str0m::media::Frequency::NINETY_KHZ,
                                    );
                                    if let Err(e) =
                                        endpoint.send_video_frame(cmid, unit.data, rtp_time)
                                    {
                                        warn!("camera send failed: {e:?}");
                                    } else {
                                        camera_pts += 1;
                                    }
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    if camera_frames <= 3 || camera_frames % 150 == 0 {
                                        warn!("camera encode: {e}");
                                    }
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if camera_frames <= 3 || camera_frames % 150 == 0 {
                            warn!("camera next_frame: {e}");
                        }
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(2));
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
