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

    /// 主动发起：创建 offer（含 video（可选）+ offer/answer + input 两个数据通道）。
    /// 返回 (offer, pending, video_mid)。
    pub fn create_offer(
        &mut self,
    ) -> Result<(SdpOffer, SdpPendingOffer, Option<Mid>, Option<Mid>), RtcError> {
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
        let _ = change.add_channel("offer/answer".into());
        let _ = change.add_channel("input".into());
        // #29 画质/显示切换：观看端 → SFU 的控制通道（选层请求等）。
        let _ = change.add_channel("control".into());
        let (offer, pending) = change
            .apply()
            .ok_or(RtcError::Io(std::io::Error::other("no changes")))?;
        Ok((offer, pending, video_mid, audio_mid))
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
        match self.rtc.poll_output() {
            Ok(Output::Event(e)) => {
                self.handle_event(e);
                // 继续 poll 后续输出
                self.poll_output()
            }
            Ok(o) => Some(o),
            Err(e) => {
                tracing::warn!("poll_output: {e:?}");
                self.events.push_back(ClientEvent::Closed);
                None
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

    /// 是否存活。
    pub fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    /// 处理数据通道里的 SDP 信令（offer/answer）。
    fn handle_signal_data(&mut self, cid: ChannelId, data: &[u8]) {
        use str0m::change::{SdpAnswer, SdpOffer};

        if let Ok(offer) = serde_json::from_slice::<SdpOffer>(data) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulcast_offer_declares_qhf_rids() {
        let mut ep = Endpoint::new();
        ep.add_video_simulcast();
        let (offer, _pending, video_mid, _audio_mid) = ep.create_offer().expect("offer");
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
        let (offer, _pending, video_mid, _audio_mid) = ep.create_offer().expect("offer");
        assert!(video_mid.is_some());
        assert!(
            !offer.to_sdp_string().contains("simulcast"),
            "plain offer must not advertise simulcast"
        );
    }
}
