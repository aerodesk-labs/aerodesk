//! WebRTC 端点（基于 str0m）：Sans-I/O，由调用方驱动 UDP I/O。
//!
//! 职责：SDP 协商（offer/answer）、媒体发送（writer）、媒体接收
//! （MediaData 事件）、数据通道（输入事件通道）、事件循环接口。

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::{Channel, ChannelId};
use str0m::media::{MediaKind, Mid, Writer};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcError, net::Protocol};

/// 客户端事件（对上层应用/适配器的输出）。
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum ClientEvent {
    IceConnected,
    IceDisconnected,
    /// 远端媒体数据（观看端收流）。
    Media(str0m::media::MediaData),
    /// 数据通道打开（label, id）。
    ChannelOpen(String, ChannelId),
    /// 数据通道数据（输入事件等）。
    ChannelData(ChannelId, bool, Vec<u8>),
    /// 远端请求关键帧。
    KeyframeRequest(str0m::media::KeyframeRequest),
    /// 连接关闭/错误。
    Closed,
}

/// str0m 端点封装。
pub struct Endpoint {
    rtc: Rtc,
    events: VecDeque<ClientEvent>,
    /// 数据通道 label → id（识别 offer/answer 与 input 通道）。
    channel_labels: HashMap<ChannelId, String>,
    /// 是否在下一个 offer 中添加视频（str0m 的 SdpApi 需在同一 change 中 apply）。
    want_video: bool,
    /// 视频方向（viewer 用 RecvOnly，publisher 用 SendRecv/SendOnly）。
    video_direction: str0m::media::Direction,
    /// 可选 simulcast 发送层（q/h/f）；Some 时 offer 携带 simulcast 属性。
    video_simulcast: Option<str0m::media::Simulcast>,
    /// 是否在下一个 offer 中添加音频（PCMU，8kHz 单声道，见 pcmu.rs）。
    want_audio: bool,
    /// 音频方向（viewer 用 RecvOnly，publisher 用 SendRecv）。
    audio_direction: str0m::media::Direction,
    /// 是否在下一个 offer 中添加第二路视频（摄像头，同 codec 配置）。
    want_camera: bool,
    /// 摄像头方向（viewer 用 RecvOnly，publisher 用 SendRecv）。
    camera_direction: str0m::media::Direction,
    /// SFU 重协商 offer 中「对端发送」视频 m-line 的 mid 顺序（screen→camera）。
    /// 观看端据此确定性区分 screen/camera，而非依赖媒体到达顺序（#340：摄像头
    /// 关键帧可能先于屏幕到达，到达序会互换）。
    remote_send_video_mids: Vec<str0m::media::Mid>,
    /// 本端 offer 中视频 m-line 的 mid 顺序（screen→camera，观看端 recvonly）。
    /// 用于从远端重协商 offer 里剔除本端 m-line，得到对端新增的发送轨。
    local_video_mids: Vec<str0m::media::Mid>,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl Endpoint {
    pub fn new() -> Self {
        let mut config = Rtc::builder();
        {
            let cfg = config.codec_config();
            // #58 音频：PCMU（G.711）静态 PT 0，8kHz 单声道。
            cfg.enable_pcmu(true);
            // #73 音频：Opus（48kHz 立体声，PT 111）——发布端可按需选择发送。
            cfg.enable_opus(true);
        }
        Self {
            rtc: config.build(Instant::now()),
            events: VecDeque::new(),
            channel_labels: HashMap::new(),
            want_video: false,
            video_direction: str0m::media::Direction::SendRecv,
            video_simulcast: None,
            want_audio: false,
            audio_direction: str0m::media::Direction::SendRecv,
            want_camera: false,
            camera_direction: str0m::media::Direction::SendRecv,
            remote_send_video_mids: Vec::new(),
            local_video_mids: Vec::new(),
        }
    }

    /// 仅启用 H.264 的端点（配合 x264/VideoToolbox 编码器）。
    pub fn new_h264() -> Self {
        let mut config = Rtc::builder();
        {
            let cfg = config.codec_config();
            cfg.clear();
            cfg.add_h264(
                str0m::media::Pt::new_with_value(96),
                None,
                true,
                0x42e01f, // Constrained Baseline
            );
            // #58 音频：PCMU（G.711）静态 PT 0，8kHz 单声道。
            cfg.enable_pcmu(true);
            // #73 音频：Opus（48kHz 立体声，PT 111）——发布端可按需选择发送。
            cfg.enable_opus(true);
        }
        Self {
            rtc: config.build(Instant::now()),
            events: VecDeque::new(),
            channel_labels: HashMap::new(),
            want_video: false,
            video_direction: str0m::media::Direction::SendRecv,
            video_simulcast: None,
            want_audio: false,
            audio_direction: str0m::media::Direction::SendRecv,
            want_camera: false,
            camera_direction: str0m::media::Direction::SendRecv,
            remote_send_video_mids: Vec::new(),
            local_video_mids: Vec::new(),
        }
    }

    /// 仅启用指定视频 codec 的端点（H264/H265/VP9/AV1 + PCMU 音频），
    /// 配合 aerodesk-ffmpeg 编码器（#74）。
    pub fn new_with_codec(codec: crate::media_pipeline::Codec) -> Self {
        use crate::media_pipeline::Codec;
        let mut config = Rtc::builder();
        {
            let cfg = config.codec_config();
            cfg.clear();
            match codec {
                Codec::H264 => {
                    cfg.add_h264(str0m::media::Pt::new_with_value(96), None, true, 0x42e01f);
                }
                Codec::Hevc => {
                    cfg.add_h265(str0m::media::Pt::new_with_value(102), None, 1, 0, 93);
                }
                Codec::Vp9 => {
                    cfg.enable_vp9(true);
                }
                Codec::Av1 => {
                    cfg.enable_av1(true);
                }
                other => panic!("endpoint unsupported video codec: {other:?}"),
            }
            // #58 音频：PCMU（G.711）静态 PT 0，8kHz 单声道。
            cfg.enable_pcmu(true);
            // #73 音频：Opus（48kHz 立体声，PT 111）——发布端可按需选择发送。
            cfg.enable_opus(true);
        }
        Self {
            rtc: config.build(Instant::now()),
            events: VecDeque::new(),
            channel_labels: HashMap::new(),
            want_video: false,
            video_direction: str0m::media::Direction::SendRecv,
            video_simulcast: None,
            want_audio: false,
            audio_direction: str0m::media::Direction::SendRecv,
            want_camera: false,
            camera_direction: str0m::media::Direction::SendRecv,
            remote_send_video_mids: Vec::new(),
            local_video_mids: Vec::new(),
        }
    }

    /// 添加本地 host candidate（调用方绑定 UDP socket 后调用）。
    pub fn add_local_candidate(
        &mut self,
        addr: SocketAddr,
        proto: Protocol,
    ) -> Result<(), RtcError> {
        let proto_str = match proto {
            Protocol::Udp => "udp",
            Protocol::Tcp => "tcp",
            Protocol::SslTcp => "ssltcp",
            _ => "udp",
        };
        let candidate = Candidate::host(addr, proto_str)
            .map_err(|e| RtcError::Io(std::io::Error::other(e.to_string())))?;
        let _ = self.rtc.add_local_candidate(candidate);
        Ok(())
    }

    /// 添加 relayed（TURN）本地候选（#157 M2）：offer 下发 `typ relay`，
    /// 对端直接发包到 relayed 地址，由 TURN 服务器中继到本端 allocation socket。
    pub fn add_relay_candidate(
        &mut self,
        relayed: SocketAddr,
        local: SocketAddr,
    ) -> Result<(), RtcError> {
        let candidate = Candidate::relayed(relayed, local, "udp")
            .map_err(|e| RtcError::Io(std::io::Error::other(e.to_string())))?;
        let _ = self.rtc.add_local_candidate(candidate);
        Ok(())
    }

    /// 请求在下一个 offer 中添加视频（VP8 起步）。
    /// 返回的 mid 在 [`create_offer`][Self::create_offer] 返回的 `Option<Mid>` 中。
    /// 请求在下一个 offer 中添加视频（发布方向 SendRecv，兼容默认）。
    pub fn add_video(&mut self) {
        self.want_video = true;
        self.video_direction = str0m::media::Direction::SendRecv;
    }

    /// 请求在下一个 offer 中添加 simulcast 视频（发送层 q/h/f）。
    /// 画质选层前提：publisher 多路编码按 rid 发送，SFU 按层转发。
    pub fn add_video_simulcast(&mut self) {
        self.want_video = true;
        self.video_direction = str0m::media::Direction::SendRecv;
        let mut sim = str0m::media::Simulcast::new();
        sim.add_send_layer(str0m::media::SimulcastLayer::new("q"));
        sim.add_send_layer(str0m::media::SimulcastLayer::new("h"));
        sim.add_send_layer(str0m::media::SimulcastLayer::new("f"));
        self.video_simulcast = Some(sim);
    }

    /// 请求在下一个 offer 中添加视频，方向为 **RecvOnly**（观看端）。
    /// #12：viewer 的 offer 必须是 recvonly，否则会被 SFU 拒绝（viewer 禁止发布媒体）。
    pub fn add_video_recvonly(&mut self) {
        self.want_video = true;
        self.video_direction = str0m::media::Direction::RecvOnly;
    }

    /// 请求在下一个 offer 中添加音频（发布方向 SendRecv，PCMU）。
    pub fn add_audio(&mut self) {
        self.want_audio = true;
        self.audio_direction = str0m::media::Direction::SendRecv;
    }

    /// 请求在下一个 offer 中添加音频，方向为 **RecvOnly**（观看端）。
    pub fn add_audio_recvonly(&mut self) {
        self.want_audio = true;
        self.audio_direction = str0m::media::Direction::RecvOnly;
    }

    /// 请求在下一个 offer 中添加第二路视频轨（摄像头，发布方向 SendRecv）。
    /// 与屏幕视频轨共用 codec 配置；SFU 按 (origin, mid) 独立转发。
    pub fn add_camera(&mut self) {
        self.want_camera = true;
        self.camera_direction = str0m::media::Direction::SendRecv;
    }

    /// 请求在下一个 offer 中添加第二路视频轨，方向为 **RecvOnly**（观看端）。
    pub fn add_camera_recvonly(&mut self) {
        self.want_camera = true;
        self.camera_direction = str0m::media::Direction::RecvOnly;
    }

    /// 主动发起：创建 offer（含 video（可选）+ camera（可选）+ 数据通道）。
    /// 返回 (offer, pending, video_mid, audio_mid, camera_mid)。
    pub fn create_offer(
        &mut self,
    ) -> Result<
        (
            SdpOffer,
            SdpPendingOffer,
            Option<Mid>,
            Option<Mid>,
            Option<Mid>,
        ),
        RtcError,
    > {
        let mut change = self.rtc.sdp_api();
        let video_mid = if self.want_video {
            Some(change.add_media(
                MediaKind::Video,
                self.video_direction,
                None,
                None,
                self.video_simulcast.clone(),
            ))
        } else {
            None
        };
        let audio_mid = if self.want_audio {
            Some(change.add_media(MediaKind::Audio, self.audio_direction, None, None, None))
        } else {
            None
        };
        // 摄像头第二路视频轨（与屏幕视频独立 mid，SFU 按 mid 转发）。
        let camera_mid = if self.want_camera {
            Some(change.add_media(MediaKind::Video, self.camera_direction, None, None, None))
        } else {
            None
        };
        let _ = change.add_channel("offer/answer".into());
        let _ = change.add_channel("input".into());
        // #29 画质/显示切换：观看端 → SFU 的控制通道（选层请求等）。
        let _ = change.add_channel("control".into());
        // #72 文件传输：双向 data channel（FileMeta/Chunk/Done，见 aerodesk-protocol::file）。
        let _ = change.add_channel("file".into());
        // #75 远程光标：被控端 → 观看端（CursorPos JSON，见 aerodesk-protocol::cursor）。
        let _ = change.add_channel("cursor".into());
        // #109 远程命令：控制端 → 被控端（CmdRequest/CmdResponse，见 cmd.rs）。
        let _ = change.add_channel("cmd".into());
        let (offer, pending) = change
            .apply()
            .ok_or(RtcError::Io(std::io::Error::other("no changes")))?;
        // 记录本端视频 m-line 顺序（screen→camera），供远端重协商剔除（#340）。
        self.local_video_mids = [video_mid, camera_mid].into_iter().flatten().collect();
        Ok((offer, pending, video_mid, audio_mid, camera_mid))
    }

    /// 被动应答：接受 offer。
    pub fn accept_offer(&mut self, offer: SdpOffer) -> Result<SdpAnswer, RtcError> {
        self.rtc.sdp_api().accept_offer(offer)
    }

    /// 接受 answer。
    pub fn accept_answer(
        &mut self,
        pending: SdpPendingOffer,
        answer: SdpAnswer,
    ) -> Result<(), RtcError> {
        self.rtc.sdp_api().accept_answer(pending, answer)?;
        Ok(())
    }

    /// 投喂网络输入（UDP 数据或超时）。
    pub fn handle_input(&mut self, input: Input) -> Result<(), RtcError> {
        if let Err(e) = self.rtc.handle_input(input) {
            self.events.push_back(ClientEvent::Closed);
            return Err(e);
        }
        Ok(())
    }

    /// 驱动时间推进（外部循环每轮调用）。
    pub fn handle_timeout(&mut self, now: Instant) -> Result<(), RtcError> {
        self.handle_input(Input::Timeout(now))
    }

    /// 轮询输出：Transmit → 调用方发送；Timeout → 记录下次超时；
    /// Event → 转入内部客户端事件队列。
    pub fn poll_output(&mut self) -> Option<Output> {
        // 迭代式排空：Event 不递归，避免事件风暴时栈深无界（审查 #255 M1）。
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Event(e)) => self.handle_event(e),
                Ok(o) => return Some(o),
                Err(e) => {
                    tracing::warn!("poll_output: {e:?}");
                    self.events.push_back(ClientEvent::Closed);
                    return None;
                }
            }
        }
    }

    /// 取下一个客户端事件。
    pub fn poll_event(&mut self) -> Option<ClientEvent> {
        self.events.pop_front()
    }

    /// 媒体写入器（发送视频帧）。
    pub fn writer(&mut self, mid: Mid) -> Option<Writer<'_>> {
        self.rtc.writer(mid)
    }

    /// 请求远端关键帧（观看端 PLI/FIR；rid 用于 simulcast 指定层）。
    pub fn request_keyframe(
        &mut self,
        mid: Mid,
        rid: Option<str0m::media::Rid>,
        kind: str0m::media::KeyframeRequestKind,
    ) -> Result<(), str0m::RtcError> {
        let Some(mut w) = self.rtc.writer(mid) else {
            return Err(str0m::RtcError::NoReceiverSource(rid));
        };
        w.request_keyframe(rid, kind)
    }

    /// 数据通道写句柄。
    pub fn channel(&mut self, id: ChannelId) -> Option<Channel<'_>> {
        self.rtc.channel(id)
    }

    /// 向指定 label 的数据通道发送数据（输入事件等）。
    pub fn send_channel_data(&mut self, label: &str, binary: bool, data: &[u8]) -> bool {
        let Some(id) = self
            .channel_labels
            .iter()
            .find(|(_, l)| l.as_str() == label)
            .map(|(id, _)| *id)
        else {
            return false;
        };
        let Some(mut ch) = self.rtc.channel(id) else {
            return false;
        };
        ch.write(binary, data).unwrap_or(false)
    }

    /// 查询数据通道 id 对应的 label。
    pub fn channel_label(&self, cid: ChannelId) -> Option<String> {
        self.channel_labels.get(&cid).cloned()
    }

    /// 发送一帧视频（内部匹配 PT 并写入）。
    pub fn send_video_frame(
        &mut self,
        mid: Mid,
        data: impl Into<std::sync::Arc<[u8]>>,
        rtp_time: str0m::media::MediaTime,
    ) -> Result<(), RtcError> {
        let Some(writer) = self.rtc.writer(mid) else {
            return Err(RtcError::Io(std::io::Error::other("no writer for mid")));
        };
        let Some(params) = writer.payload_params().next().cloned() else {
            return Err(RtcError::Io(std::io::Error::other("no payload params")));
        };
        let Some(pt) = writer.match_params(params) else {
            return Err(RtcError::Io(std::io::Error::other("no matching pt")));
        };
        writer.write(pt, Instant::now(), rtp_time, data)
    }

    /// 按 simulcast rid 发送视频帧（画质选层：q/h/f 层）。
    pub fn send_video_frame_rid(
        &mut self,
        mid: Mid,
        rid: str0m::media::Rid,
        data: impl Into<std::sync::Arc<[u8]>>,
        rtp_time: str0m::media::MediaTime,
    ) -> Result<(), RtcError> {
        let Some(writer) = self.rtc.writer(mid) else {
            return Err(RtcError::Io(std::io::Error::other("no writer for mid")));
        };
        let Some(params) = writer.payload_params().next().cloned() else {
            return Err(RtcError::Io(std::io::Error::other("no payload params")));
        };
        let Some(pt) = writer.match_params(params) else {
            return Err(RtcError::Io(std::io::Error::other("no matching pt")));
        };
        writer.rid(rid).write(pt, Instant::now(), rtp_time, data)
    }

    /// 发送一帧音频（PCMU，RTP 时间戳按 8kHz 时钟）。
    /// 必须显式匹配 PCMU 参数：默认 codec 配置里 Opus 排最前，
    /// 用 `payload_params().next()` 会把 μ-law 字节标成 Opus（#58 排查）。
    pub fn send_audio_frame(
        &mut self,
        mid: Mid,
        data: impl Into<std::sync::Arc<[u8]>>,
        rtp_time: str0m::media::MediaTime,
    ) -> Result<(), RtcError> {
        let Some(writer) = self.rtc.writer(mid) else {
            return Err(RtcError::Io(std::io::Error::other("no writer for mid")));
        };
        let Some(params) = writer
            .payload_params()
            .find(|p| p.spec().codec == str0m::format::Codec::PCMU)
            .cloned()
        else {
            return Err(RtcError::Io(std::io::Error::other(
                "no PCMU payload params",
            )));
        };
        let Some(pt) = writer.match_params(params) else {
            return Err(RtcError::Io(std::io::Error::other("no matching pt")));
        };
        writer.write(pt, Instant::now(), rtp_time, data)
    }

    /// 发送一帧音频（Opus，RTP 时间戳按 48kHz 时钟，#73）。
    /// 与 PCMU 一样必须显式匹配 Opus 参数（默认 codec 配置里 Opus 排最前）。
    pub fn send_audio_frame_opus(
        &mut self,
        mid: Mid,
        data: impl Into<std::sync::Arc<[u8]>>,
        rtp_time: str0m::media::MediaTime,
    ) -> Result<(), RtcError> {
        let Some(writer) = self.rtc.writer(mid) else {
            return Err(RtcError::Io(std::io::Error::other("no writer for mid")));
        };
        let Some(params) = writer
            .payload_params()
            .find(|p| p.spec().codec == str0m::format::Codec::Opus)
            .cloned()
        else {
            return Err(RtcError::Io(std::io::Error::other(
                "no Opus payload params",
            )));
        };
        let Some(pt) = writer.match_params(params) else {
            return Err(RtcError::Io(std::io::Error::other("no matching pt")));
        };
        writer.write(pt, Instant::now(), rtp_time, data)
    }

    /// 是否存活。
    pub fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    /// 远端（SFU）发送的视频轨 mid 顺序（screen→camera），观看端分类用。
    pub fn remote_send_video_mids(&self) -> &[str0m::media::Mid] {
        &self.remote_send_video_mids
    }

    /// 处理数据通道里的 SDP 信令（offer/answer）。
    fn handle_signal_data(&mut self, cid: ChannelId, data: &[u8]) {
        use str0m::change::{SdpAnswer, SdpOffer};

        if let Ok(offer) = serde_json::from_slice::<SdpOffer>(data) {
            // #340：SFU 重协商 offer 含「本端视频 m-line + 对端新增发送轨」。
            // 剔除本端 mid 后，剩余视频 mid 顺序即 SFU 的 screen→camera 发送轨，
            // 观看端据此确定性区分两条视频轨（媒体到达顺序不可靠）。
            let sdp = offer.to_sdp_string();
            self.remote_send_video_mids = parse_video_mids_in_order(&sdp)
                .into_iter()
                .filter(|m| !self.local_video_mids.contains(m))
                .collect();
            match self.rtc.sdp_api().accept_offer(offer) {
                Ok(answer) => {
                    let json = serde_json::to_string(&answer).expect("answer json");
                    if let Some(mut channel) = self.rtc.channel(cid) {
                        let _ = channel.write(false, json.as_bytes());
                    }
                }
                Err(e) => tracing::warn!("accept_offer on signal channel: {e:?}"),
            }
            return;
        }
        if let Ok(answer) = serde_json::from_slice::<SdpAnswer>(data) {
            // 数据通道 answer：CLI 目前不主动发 track offer，忽略（可扩展）。
            let _ = answer;
        }
    }

    fn handle_event(&mut self, e: Event) {
        use str0m::media::KeyframeRequestKind;
        match e {
            Event::IceConnectionStateChange(v) => {
                use str0m::IceConnectionState::*;
                match v {
                    Connected | Completed => self.events.push_back(ClientEvent::IceConnected),
                    Disconnected => self.events.push_back(ClientEvent::IceDisconnected),
                    _ => {}
                }
            }
            Event::MediaData(data) => self.events.push_back(ClientEvent::Media(data)),
            Event::ChannelOpen(cid, label) => {
                self.channel_labels.insert(cid, label.clone());
                // #467：offer/answer 通道在 opener 侧收到 DCEP ACK 才会触发本事件，
                // 即通道双向确已就绪。此刻主动向 SFU 声明 signal_ready，SFU 收到后
                // 才发重协商 offer——消除"DCEP 未完成即写 offer 被对端丢弃"的竞态。
                if label == "offer/answer"
                    && let Some(mut channel) = self.rtc.channel(cid)
                    && channel.write(false, br#"{"type":"signal_ready"}"#).is_err()
                {
                    tracing::warn!("发送 signal_ready 失败（通道刚打开即写失败）");
                }
                self.events.push_back(ClientEvent::ChannelOpen(label, cid))
            }
            Event::ChannelData(d) => {
                // 数据通道信令：offer/answer 通道承载 SDP（track 增删协商）
                if self
                    .channel_labels
                    .get(&d.id)
                    .map(|l| l == "offer/answer")
                    .unwrap_or(false)
                {
                    self.handle_signal_data(d.id, &d.data);
                    return;
                }
                self.events
                    .push_back(ClientEvent::ChannelData(d.id, d.binary, d.data))
            }
            Event::KeyframeRequest(req) => {
                let _ = KeyframeRequestKind::Fir;
                self.events.push_back(ClientEvent::KeyframeRequest(req))
            }
            _ => {}
        }
    }
}

/// 解析 SDP 字符串，返回视频 m-line 的 mid（按出现顺序，不论方向）。
/// 用于配合 [`Endpoint::local_video_mids`] 剔除本端 m-line，得到远端新增发送轨。
fn parse_video_mids_in_order(sdp: &str) -> Vec<str0m::media::Mid> {
    use str0m::media::Mid;
    let mut out = Vec::new();
    let mut cur_video = false;
    let mut cur_mid: Option<Mid> = None;
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            if cur_video && let Some(m) = cur_mid.take() {
                out.push(m);
            }
            cur_video = rest.starts_with("video");
        } else if cur_video && let Some(mid) = line.strip_prefix("a=mid:") {
            cur_mid = Some(Mid::from(mid));
        }
    }
    if cur_video && let Some(m) = cur_mid {
        out.push(m);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulcast_offer_declares_qhf_rids() {
        let mut ep = Endpoint::new();
        ep.add_video_simulcast();
        let (offer, _pending, video_mid, _audio_mid, _camera_mid) =
            ep.create_offer().expect("offer");
        assert!(video_mid.is_some(), "simulcast offer should include video");
        let sdp = offer.to_sdp_string();
        assert!(
            sdp.contains("simulcast"),
            "offer missing a=simulcast: {sdp}"
        );
        for rid in ["q", "h", "f"] {
            assert!(
                sdp.contains(&format!("rid:{rid}")),
                "offer missing rid {rid}: {sdp}"
            );
        }
    }

    #[test]
    fn plain_video_offer_has_no_simulcast() {
        let mut ep = Endpoint::new();
        ep.add_video();
        let (offer, _pending, video_mid, _audio_mid, _camera_mid) =
            ep.create_offer().expect("offer");
        assert!(video_mid.is_some());
        assert!(
            !offer.to_sdp_string().contains("simulcast"),
            "plain offer must not advertise simulcast"
        );
    }

    /// 摄像头第二路视频轨（#304）：add_camera_recvonly 后 offer 含两个 video m-line，
    /// camera_mid 独立返回；未请求时 camera_mid=None。
    #[test]
    fn camera_offer_adds_second_video_mline() {
        let mut ep = Endpoint::new();
        ep.add_video_recvonly();
        ep.add_camera_recvonly();
        let (offer, _pending, video_mid, _audio_mid, camera_mid) =
            ep.create_offer().expect("offer");
        assert!(video_mid.is_some(), "screen video mid expected");
        let cam = camera_mid.expect("camera mid expected");
        assert_ne!(
            cam,
            video_mid.unwrap(),
            "camera mid must differ from screen mid"
        );
        let sdp = offer.to_sdp_string();
        let video_lines = sdp.matches("m=video").count();
        assert_eq!(
            video_lines, 2,
            "offer should contain 2 video m-lines: {sdp}"
        );
        assert!(
            sdp.contains("recvonly"),
            "viewer camera line should be recvonly"
        );
    }

    /// #340：解析视频 m-line 顺序（含本端与远端），配合剔除本端 mid 得到远端发送轨。
    #[test]
    fn parse_video_mids_in_order_keeps_sdp_order() {
        let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97\na=mid:MY_SCREEN\na=sendonly\n\
m=video 9 UDP/TLS/RTP/SAVPF 102 103\na=mid:MY_CAM\na=sendonly\n\
m=application 9 UDP/DTLS/SCTP webrtc\na=mid:data\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97\na=mid:SFU_SCREEN\na=sendonly\n\
m=video 9 UDP/TLS/RTP/SAVPF 102 103\na=mid:SFU_CAM\na=sendonly\n";
        let mids = parse_video_mids_in_order(sdp);
        assert_eq!(mids.len(), 4);
        assert_eq!(&*mids[0], "MY_SCREEN");
        assert_eq!(&*mids[1], "MY_CAM");
        assert_eq!(&*mids[2], "SFU_SCREEN");
        assert_eq!(&*mids[3], "SFU_CAM");
    }
}
