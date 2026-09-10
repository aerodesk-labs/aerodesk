//! aerodesk-vdev-bridge：把本 SFU 房间的媒体流灌进本机 vdev 虚拟设备。
//!
//! 它是 **aerodesk 的观众端 + 一个本地设备 sink**：REGISTER/INVITE 房间收流 →
//! FFmpeg/Opus 解码 → 视频写进 vdev 虚拟摄像头的 FrameChannel（TCP 127.0.0.1:27890）、
//! 音频写进 vdev 虚拟声卡（CoreAudio AudioUnit）。
//!
//! 归属说明（2026-09-10 从 vdev 仓库迁来）：本工具耗的是 aerodesk 的重依赖
//! （str0m/SIP/编解码），vdev 那侧只是设备；放在 vdev 里会让 vdev 主仓库被迫
//! 依赖 aerodesk。设备侧的接口都是**协议/A系统 API**（TCP 帧协议 + CoreAudio），
//! 所以本文件不依赖任何 vdev crate。美颜/背景替换等成像滤镜归 vdev 设备侧
//! （vdev-camera-ext），本工具只负责把原样解码帧送进去。
//!
//! 用法：aerodesk-vdev-bridge <signal_server> <room> [auth]
//! 环境变量：
//!   VDEV_WIDTH / VDEV_HEIGHT  输出分辨率（默认 1920x1080）

// 本工具只在 macOS 有目标设备（vdev 的虚拟摄像头/声卡都是 macOS 侧），
// 但它常驻 aerodesk-agent 这个跨平台 crate，所以按平台门控：
// 非 macOS 编译成一个明确报错的桩，保证 Windows/Linux 的 cargo build 不受影响。

#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod frame;

#[cfg(target_os = "macos")]
mod app {
    use super::{audio, frame};

    use std::time::Instant;

    use aerodesk_core::access_unit::AccessUnitAssembler;
    use aerodesk_core::connect::connect_viewer_sip;
    use aerodesk_core::endpoint::ClientEvent;
    use aerodesk_core::platform::Codec;
    use str0m::{Input, Output};

    use aerodesk_codec::audio::OpusDecoder;
    use aerodesk_codec::decode::FfmpegDecoder;

    pub fn run() -> Result<(), String> {
        let args: Vec<String> = std::env::args().collect();
        if args.len() < 3 {
            eprintln!("用法: vdev-bridge <signal_server> <room> [auth]");
            std::process::exit(1);
        }
        let server = &args[1];
        let room = &args[2];
        let auth = args.get(3).map(|s| s.as_str());

        // 1. 连接 SFU（viewer）。信令走 SIP：REGISTER → INVITE 房间 AoR → ICE 收敛，
        //    全流程在 connect_viewer_sip 内完成并持有看护线程；返回即已连通，
        //    不需要再自己跑 signal 保活线程。
        let (_sip, mut endpoint, mut socket, _video_mid, _audio_mid, _camera_mid) =
            connect_viewer_sip(
                server, room, auth,
                false, // force_relay：默认直连，TURN 由 MediaSocket 按需兜底
                true,  // with_audio：需要 Opus 音频轨喂虚拟声卡
                false, // with_camera：不要额外的摄像头轨
                None,  // sip_transport：由 server scheme 推导（ws→udp / wss→tls）
                None,  // sip_port
            )?;
        println!("已连接：room={room}");

        // 2. 摄像头推流客户端
        let mut frame_client = match frame::connect() {
            Ok(f) => {
                println!("已连接虚拟摄像头 FrameChannel");
                Some(f)
            }
            Err(e) => {
                eprintln!("警告：{e}，仅解码不推流");
                None
            }
        };

        // 3. 输出分辨率（滤镜在设备侧：vdev-camera-ext）
        let out_w: u32 = std::env::var("VDEV_WIDTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1920);
        let out_h: u32 = std::env::var("VDEV_HEIGHT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1080);

        // 4. 解码器 + 组装器
        let mut decoder: Option<FfmpegDecoder> = None;
        let mut assembler = AccessUnitAssembler::new();

        // 5. 音频输出（声卡）+ Opus 解码
        let mut audio_sink = match audio::AudioSink::new() {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("警告：{e}，跳过音频");
                None
            }
        };
        let mut opus_decoder = audio_sink.as_ref().and_then(|_| OpusDecoder::new().ok());

        let mut frames = 0u64;
        let mut audio_frames = 0u64;
        let mut buf = [0u8; 2048];
        println!("收流中… Ctrl-C 退出");
        loop {
            // 收 UDP → endpoint
            if let Ok((n, source)) = socket.recv_from(&mut buf)
                && let Ok(contents) = buf[..n].try_into()
            {
                let _ = endpoint.handle_input(Input::Receive(
                    Instant::now(),
                    str0m::net::Receive {
                        proto: str0m::net::Protocol::Udp,
                        source,
                        destination: socket.local_addr().map_err(|e| e.to_string())?,
                        contents,
                    },
                ));
            }
            let _ = endpoint.handle_timeout(Instant::now());

            // endpoint 输出 → socket
            while let Some(output) = endpoint.poll_output() {
                match output {
                    Output::Transmit(t) => {
                        let _ = socket.send_to(&t.contents, t.destination);
                    }
                    Output::Timeout(_) => break,
                    Output::Event(_) => {}
                }
            }

            // 事件 → 媒体
            while let Some(ev) = endpoint.poll_event() {
                if let ClientEvent::Media(data) = ev {
                    // 音频轨：Opus 解码 → 声卡
                    if data.params.spec().codec == str0m::format::Codec::Opus {
                        if let (Some(sink), Some(dec)) = (&mut audio_sink, &mut opus_decoder)
                            && let Ok(Some(pcm)) = dec.decode(&data.data)
                        {
                            sink.push_mono_i16(&pcm);
                            audio_frames += 1;
                            if audio_frames.is_multiple_of(100) {
                                println!("音频帧 {}（samples={}）", audio_frames, pcm.len());
                            }
                        }
                        continue;
                    }
                    if data.params.spec().codec == str0m::format::Codec::PCMU {
                        continue;
                    }
                    // 视频轨：识别 codec
                    let codec = match data.params.spec().codec {
                        str0m::format::Codec::H264 => Some(Codec::H264),
                        str0m::format::Codec::H265 => Some(Codec::Hevc),
                        str0m::format::Codec::Vp9 => Some(Codec::Vp9),
                        str0m::format::Codec::Av1 => Some(Codec::Av1),
                        _ => None,
                    };
                    let Some(codec) = codec else { continue };

                    // 解码器按 codec 重建
                    if decoder.as_ref().map(|d| d.codec() != codec).unwrap_or(true) {
                        decoder = FfmpegDecoder::new(codec).ok();
                    }

                    // 组装完整访问单元
                    let Some(au) = assembler.push(
                        data.data.as_ref(),
                        data.time.as_micros(),
                        data.is_keyframe(),
                    ) else {
                        continue;
                    };
                    let unit = aerodesk_core::platform::EncodedUnit {
                        data: au.data,
                        keyframe: au.keyframe,
                        pts_ms: 0,
                        rtp_timestamp: 0,
                    };
                    let Some(dec) = decoder.as_mut() else {
                        continue;
                    };
                    let Ok(Some(vf)) = dec.decode_unit(&unit) else {
                        continue;
                    };
                    let Some(rgba) = vf.raw else { continue };

                    // RGBA → BGRA + 缩放（帧分辨率可能是源分辨率，需缩放到输出）
                    // 滤镜不在这里做：归设备侧（vdev-camera-ext）。
                    let bgra = rgba_to_bgra_scaled(&rgba, vf.width, vf.height, out_w, out_h);

                    // 推摄像头
                    if let Some(fc) = &mut frame_client {
                        let _ = fc.send_frame(&bgra, out_w, out_h, out_w * 4, host_time_ns());
                    }
                    frames += 1;
                    if frames.is_multiple_of(60) {
                        println!("已推 {frames} 帧（{}x{}）", out_w, out_h);
                    }
                }
            }
        }
    }

    /// RGBA → BGRA，并缩放到目标尺寸（简单双线性，够 MVP 用）
    fn rgba_to_bgra_scaled(rgba: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
        let mut out = vec![0u8; (dw * dh * 4) as usize];
        for y in 0..dh {
            let sy = (y * sh / dh.max(1)) as usize;
            for x in 0..dw {
                let sx = (x * sw / dw.max(1)) as usize;
                let si = (sy * sw as usize + sx) * 4;
                let di = (y * dw + x) as usize * 4;
                out[di] = rgba[si + 2]; // B ← R
                out[di + 1] = rgba[si + 1]; // G
                out[di + 2] = rgba[si]; // R ← B
                out[di + 3] = rgba[si + 3]; // A
            }
        }
        out
    }

    fn host_time_ns() -> u64 {
        unsafe extern "C" {
            fn mach_absolute_time() -> u64;
        }
        // 近似：mach ticks 直接当纳秒（摄像头侧只看单调性，不要求绝对精度）
        unsafe { mach_absolute_time() }
    }
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), String> {
    app::run()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("aerodesk-vdev-bridge 仅支持 macOS：vdev 的虚拟摄像头/声卡是 macOS 侧设备");
    std::process::exit(1);
}
