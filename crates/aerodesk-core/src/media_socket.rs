//! 媒体网络传输：直连 UDP + 可选 TURN 中继（#157 M2）。
//!
//! 对外暴露与 `UdpSocket` 一致的 `recv_from/send_to/local_addr/set_read_timeout`，
//! 内部：
//! - 收：直连 socket 与 TURN allocation socket 双路收包；TURN 侧解析
//!   Data indication / ChannelData 后还原为（对端地址, 载荷）。
//! - 发：ICE 未锁定前双路发送（直连 + TURN），首个非 STUN Binding 包到达
//!   路径后锁定单路——直连优先、TURN 兜底，同时避免媒体重复。
//! - TURN 定期 Refresh 维持 allocation（刷新失败下轮重试，非阻塞）。

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::turn_client::TurnTransport;

/// 发送路径（ICE 连接后的选定路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    Direct,
    Turn,
}

/// 直连 UDP + TURN 的透明传输封装。
pub struct MediaSocket {
    direct: UdpSocket,
    turn: Option<TurnTransport>,
    /// None = 尚未锁定（双路发送）；Some = 已锁定单路。
    send_path: Option<Path>,
}

impl MediaSocket {
    pub fn new(direct: UdpSocket, turn: Option<TurnTransport>) -> Self {
        MediaSocket {
            direct,
            turn,
            send_path: None,
        }
    }

    pub fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.direct.set_read_timeout(t)?;
        // TURN 泵超时恒定非阻塞（TurnIo::set_read_timeout，#487 节流根因）——
        // 跟随 direct 的 5ms 会把 TURN 吞吐串行化到 ~200 帧/s（2.4Mbps），
        // 高码率视频（8Mbps≈660 包/s）丢帧。
        if let Some(turn) = &self.turn {
            turn.set_read_timeout(t)?;
        }
        Ok(())
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.direct.local_addr()
    }

    /// TURN 传输（查询 relayed 地址 / 是否启用）。
    pub fn turn(&self) -> Option<&TurnTransport> {
        self.turn.as_ref()
    }

    /// 收一帧：先 TURN 非阻塞探测（有积压立即返回），再直连 UDP 阻塞等待。
    ///
    /// #487 根因修复：旧实现「先等直连 UDP 5ms 超时再读 TURN 一帧」，泵循环把
    /// TURN 吞吐钳在 1 帧/5ms ≈ 200 帧/s ≈ 2.4Mbps——8Mbps 视频（~660 包/s）
    /// 3.3× 超限 → TURN 服务器 relay socket 溢出丢包 → 视频大包群整帧丢失、
    /// 音频（低码率）幸存（生产真屏会话 0 帧解码的根因）。TURN 优先探测 +
    /// 非阻塞超时后，泵循环排空式读取吞吐不再受限；TURN 无数据时照旧落到
    /// 直连 UDP，两条路径互不饿死。
    pub fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        if let Some(turn) = &mut self.turn {
            // 非阻塞探测：TURN 有积压立即返回（不阻塞在直连 UDP 超时上）。
            if let Ok(Some((peer, n))) = turn.recv_packet(buf) {
                self.note_packet(Path::Turn, &buf[..n]);
                return Ok((n, peer));
            }
        }
        match self.direct.recv_from(buf) {
            Ok((n, src)) => {
                self.note_packet(Path::Direct, &buf[..n]);
                return Ok((n, src));
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            }
            Err(e) => return Err(e),
        }
        // 兜底：直连 UDP 无数据时再试一次 TURN（维持 refresh、累积半包帧缓冲）。
        if let Some(turn) = &mut self.turn {
            let _ = turn.refresh_if_due(Instant::now());
            if let Ok(Some((peer, n))) = turn.recv_packet(buf) {
                self.note_packet(Path::Turn, &buf[..n]);
                return Ok((n, peer));
            }
        }
        Err(io::Error::new(io::ErrorKind::WouldBlock, "no packet"))
    }

    /// 发一帧：未锁定双路（直连 + TURN），已锁定走单路。
    pub fn send_to(&mut self, buf: &[u8], dst: SocketAddr) -> io::Result<usize> {
        match self.send_path {
            Some(Path::Direct) => self.direct.send_to(buf, dst),
            Some(Path::Turn) => {
                let turn = self
                    .turn
                    .as_mut()
                    .ok_or_else(|| io::Error::other("TURN path locked but transport missing"))?;
                turn.send_to(dst, buf).map_err(io::Error::other)?;
                Ok(buf.len())
            }
            None => {
                // 双路投递：任一成功即成功（直连坏时 TURN 兜底、反之亦然）。
                let mut turn_ok = false;
                if let Some(turn) = &mut self.turn {
                    turn_ok = turn.send_to(dst, buf).is_ok();
                }
                match self.direct.send_to(buf, dst) {
                    Ok(n) => Ok(n),
                    Err(_) if turn_ok => Ok(buf.len()),
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// 首个非 STUN Binding 包决定锁定路径（DTLS/RTP 只在 nominated 路径出现）。
    fn note_packet(&mut self, path: Path, pkt: &[u8]) {
        if self.send_path.is_some() {
            return;
        }
        if is_stun_binding(pkt) {
            return;
        }
        tracing::debug!("media path locked: {path:?}");
        self.send_path = Some(path);
    }

    #[cfg(test)]
    fn send_path(&self) -> Option<Path> {
        self.send_path
    }
}

/// 是否为 STUN Binding（请求 0x0001 / 成功 0x0101 / 错误 0x0111）。
fn is_stun_binding(pkt: &[u8]) -> bool {
    if pkt.len() < 8 {
        return false;
    }
    if (pkt[0] & 0xc0) != 0 {
        return false;
    }
    let method = u16::from_be_bytes([pkt[0], pkt[1]]) & 0x3fff;
    if !matches!(method, 0x0001) {
        return false;
    }
    pkt[4..8] == [0x21, 0x12, 0xa4, 0x42]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_client::testutil::start_mock;
    use std::net::UdpSocket;

    #[test]
    fn direct_only_passthrough() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr_a = a.local_addr().unwrap();
        let mut ms = MediaSocket::new(a, None);
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        b.send_to(b"ping", addr_a).unwrap();
        let mut buf = [0u8; 64];
        let (n, src) = ms.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping");
        assert_eq!(src, b.local_addr().unwrap());
        ms.send_to(b"pong", src).unwrap();
        let mut buf2 = [0u8; 64];
        let (n2, _) = b.recv_from(&mut buf2).unwrap();
        assert_eq!(&buf2[..n2], b"pong");
    }

    #[test]
    fn stun_binding_not_locking() {
        let binding = [
            0x00, 0x01, 0x00, 0x00, 0x21, 0x12, 0xa4, 0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(is_stun_binding(&binding));
        let dtls = [0x16, 0xfe, 0xfd, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(!is_stun_binding(&dtls));
        let rtp = [0x80, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(!is_stun_binding(&rtp));
    }

    #[test]
    fn turn_path_roundtrip_and_lock() {
        let (server, _, _, _h) = start_mock("user1", "pass1");
        let tt = crate::turn_client::TurnTransport::connect(
            server,
            "user1",
            "pass1",
            std::net::Ipv4Addr::LOCALHOST.into(),
            Duration::from_secs(2),
        )
        .expect("connect");
        let direct = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut ms = MediaSocket::new(direct, Some(tt));
        ms.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        assert_eq!(ms.send_path(), None);

        // 真实 peer socket：直连路径的重复包会投递到这里（不读即可），
        // 避免 Windows 上发往无人监听地址触发 ICMP → recv_from ConnectionReset。
        let peer_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer: SocketAddr = peer_sock.local_addr().unwrap();
        // 未锁定：双路发送（直连 + TURN），mock 回显 → recv 经 TURN 返回
        ms.send_to(b"ice-check", peer).unwrap();
        let mut buf = [0u8; 4096];
        let (n, src) = ms.recv_from(&mut buf).unwrap();
        assert_eq!(src, peer);
        assert_eq!(&buf[..n], b"ice-check");
        // 回显载荷非 STUN Binding → 锁定 TURN
        assert_eq!(ms.send_path(), Some(Path::Turn));

        // 锁定后仍可继续收发
        ms.send_to(b"media", peer).unwrap();
        let (n2, src2) = ms.recv_from(&mut buf).unwrap();
        assert_eq!(src2, peer);
        assert_eq!(&buf[..n2], b"media");
    }

    #[test]
    fn direct_packet_locks_direct() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr_a = a.local_addr().unwrap();
        let mut ms = MediaSocket::new(a, None);
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        // DTLS 类型载荷（非 STUN Binding）→ 锁定直连
        b.send_to(&[0x16, 0xfe, 0xfd, 0, 0, 0, 0, 0], addr_a)
            .unwrap();
        let mut buf = [0u8; 64];
        let (n, _) = ms.recv_from(&mut buf).unwrap();
        assert_eq!(n, 8);
        assert_eq!(ms.send_path(), Some(Path::Direct));
    }
}
