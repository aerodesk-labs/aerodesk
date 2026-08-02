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
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl Endpoint {
    pub fn new() -> Self {
        Self {
            rtc: Rtc::new(Instant::now()),
            events: VecDeque::new(),
            channel_labels: HashMap::new(),
            want_video: false,
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
    pub fn add_video(&mut self) {
        self.want_video = true;
    }

    /// 主动发起：创建 offer（含 video（可选）+ offer/answer + input 两个数据通道）。
    /// 返回 (offer, pending, video_mid)。
    pub fn create_offer(&mut self) -> Result<(SdpOffer, SdpPendingOffer, Option<Mid>), RtcError> {
        let mut change = self.rtc.sdp_api();
        let video_mid = if self.want_video {
            Some(change.add_media(
                MediaKind::Video,
                str0m::media::Direction::SendRecv,
                None,
                None,
                None,
            ))
        } else {
            None
        };
        let _ = change.add_channel("offer/answer".into());
        let _ = change.add_channel("input".into());
        let (offer, pending) = change
            .apply()
            .ok_or(RtcError::Io(std::io::Error::other("no changes")))?;
        Ok((offer, pending, video_mid))
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
