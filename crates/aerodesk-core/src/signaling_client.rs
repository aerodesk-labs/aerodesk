//! 信令客户端抽象（WSS + aerodesk-protocol::signal 消息）。

/// 信令客户端。
pub trait SignalClient {
    type Error: std::error::Error;

    /// 连接信令服务器并加入房间。
    fn connect(
        &mut self,
        url: &str,
        room: &str,
        role: aerodesk_protocol::signal::Role,
    ) -> Result<(), Self::Error>;

    /// 发送一条信令消息。
    fn send(&mut self, msg: aerodesk_protocol::signal::SignalMessage) -> Result<(), Self::Error>;

    /// 拉取收到的消息（非阻塞）。
    fn poll(&mut self) -> Option<aerodesk_protocol::signal::SignalMessage>;

    fn disconnect(&mut self);
}
