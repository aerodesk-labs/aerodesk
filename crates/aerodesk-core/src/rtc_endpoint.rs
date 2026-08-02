//! WebRTC 端点抽象（基于 str0m，与 aerodesk-sfu 同栈同版本）。
//!
//! 发布端：屏幕帧 → 编码 → str0m writer → RTP
//! 观看端：RTP → str0m 解包 → 解码 → 渲染
//! 输入：aerodesk-protocol::input 帧经数据通道收发。
//!
//! P2 填充 str0m `Rtc` 事件循环封装（Sans-I/O：由平台适配层驱动 I/O）。

/// 端点角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointRole {
    /// 被控端：发布屏幕流，接收输入事件。
    Publisher,
    /// 观看端：接收屏幕流，发送输入事件。
    Viewer,
}

/// 端点事件（对上层 UI/适配层的输出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointEvent {
    IceStateChanged(String),
    TrackOpened { mid: String, kind: String },
    TrackClosed { mid: String },
    ChannelOpened { label: String },
    ChannelData { label: String, data: Vec<u8> },
    KeyframeRequested { mid: String },
    RemoteBitrateChanged { bitrate_bps: u64 },
}
