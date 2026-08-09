//! ICE-TCP / ICE-SSL-TCP support (UnifiedSocket: same port as UDP).
//!
//! str0m handles protocol logic but leaves the transport to the app.
//! Over TCP, WebRTC traffic is a byte stream that must be demultiplexed into
//! packets:
//!   * STUN  — self-delimiting (20-byte header + message length field)
//!   * DTLS  — self-delimiting (13-byte record header + length field)
//!   * RTP/RTCP — RFC 4571 framing (2-byte big-endian length prefix)
//!
//! "ssltcp" is libwebrtc's *fake* SSL: the client opens the TCP connection and
//! sends a fixed 70-byte client hello; the server answers with a fixed 78-byte
//! server hello, after which ALL traffic is plaintext (no actual TLS).
//! See rtc_base/socket_adapters.{h,cc} in libwebrtc.
//!
//! Incoming packets are pushed into a queue for the run loop. The accepted
//! `TcpStream` write handle is handed to the run loop so it can write
//! `Output::Transmit`s back (framing RTP/RTCP with RFC 4571 on the way out).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use str0m::net::Protocol;

/// Fixed fake-SSL client hello (libwebrtc kSslClientHello, 72 bytes).
const SSL_CLIENT_HELLO: [u8; 72] = [
    0x80, 0x46, 0x01, 0x03, 0x01, 0x00, 0x2d, 0x00, 0x00, 0x00, 0x10, 0x01, 0x00, 0x80, 0x03, 0x00,
    0x80, 0x07, 0x00, 0xc0, 0x06, 0x00, 0x40, 0x02, 0x00, 0x80, 0x04, 0x00, 0x80, 0x00, 0x00, 0x04,
    0x00, 0xfe, 0xff, 0x00, 0x00, 0x0a, 0x00, 0xfe, 0xfe, 0x00, 0x00, 0x09, 0x00, 0x00, 0x64, 0x00,
    0x00, 0x62, 0x00, 0x00, 0x03, 0x00, 0x00, 0x06, 0x1f, 0x17, 0x0c, 0xa6, 0x2f, 0x00, 0x78, 0xfc,
    0x46, 0x55, 0x2e, 0xb1, 0x83, 0x39, 0xf1, 0xea,
];

/// Fixed fake-SSL server hello (libwebrtc kSslServerHello, 79 bytes).
const SSL_SERVER_HELLO: [u8; 79] = [
    0x16, 0x03, 0x01, 0x00, 0x4a, 0x02, 0x00, 0x00, 0x46, 0x03, 0x01, 0x42, 0x85, 0x45, 0xa7, 0x27,
    0xa9, 0x5d, 0xa0, 0xb3, 0xc5, 0xe7, 0x53, 0xda, 0x48, 0x2b, 0x3f, 0xc6, 0x5a, 0xca, 0x89, 0xc1,
    0x58, 0x52, 0xa1, 0x78, 0x3c, 0x5b, 0x17, 0x46, 0x00, 0x85, 0x3f, 0x20, 0x0e, 0xd3, 0x06, 0x72,
    0x5b, 0x5b, 0x1b, 0x5f, 0x15, 0xac, 0x13, 0xf9, 0x88, 0x53, 0x9d, 0x9b, 0xe8, 0x3d, 0x7b, 0x0c,
    0x30, 0x32, 0x6e, 0x38, 0x4d, 0xa2, 0x75, 0x57, 0x41, 0x6c, 0x34, 0x5c, 0x00, 0x04, 0x00,
];

pub enum TcpEvent {
    /// A new TCP connection (carries the write handle).
    New {
        source: SocketAddr,
        stream: TcpStream,
    },
    /// One complete packet (STUN/DTLS/RTP/RTCP, RFC 4571 prefix stripped).
    Packet {
        source: SocketAddr,
        proto: Protocol,
        data: Vec<u8>,
    },
    /// The connection was closed by the peer.
    Close { source: SocketAddr },
}

/// 绑定 TCP 监听（SO_REUSEADDR + 短重试；#216 双 SFU 快速复跑时避免 TIME_WAIT
/// 端口占用导致 Address already in use——与 HTTPS bind_public_with_retry 同类问题）。
pub fn spawn_tcp_listener(bind: SocketAddr) -> (SocketAddr, Receiver<TcpEvent>) {
    let mut last_err = None;
    for _ in 0..5 {
        let sock = match socket2::Socket::new(
            socket2::Domain::for_address(bind),
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        ) {
            Ok(s) => s,
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };
        let _ = sock.set_reuse_address(true);
        match sock.bind(&bind.into()).and_then(|_| sock.listen(1024)) {
            Ok(()) => {
                let listener: TcpListener = sock.into();
                let _ = listener.set_nonblocking(true);
                let addr = listener.local_addr().expect("TCP local addr");
                let (tx, rx) = mpsc::channel::<TcpEvent>();
                thread::spawn(move || accept_loop(listener, tx));
                return (addr, rx);
            }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    panic!(
        "binding TCP listener {bind}: {}",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown".into())
    );
}

fn accept_loop(listener: TcpListener, tx: Sender<TcpEvent>) {
    loop {
        match listener.accept() {
            Ok((stream, addr)) => {
                let write = match stream.try_clone() {
                    Ok(w) => w,
                    Err(_) => continue,
                };
                if tx
                    .send(TcpEvent::New {
                        source: addr,
                        stream: write,
                    })
                    .is_err()
                {
                    return;
                }
                let tx2 = tx.clone();
                thread::spawn(move || read_loop(stream, addr, tx2));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                eprintln!("tcp accept error: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Connection protocol state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnProto {
    /// Waiting for enough bytes to tell fake-SSL hello from plain TCP.
    Pending,
    Plain,
    Ssl,
}

fn read_loop(mut stream: TcpStream, source: SocketAddr, tx: Sender<TcpEvent>) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let mut proto = ConnProto::Pending;
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                proto = detect_proto(&mut buf, &mut stream, proto);
                let p = match proto {
                    ConnProto::Pending => continue,
                    ConnProto::Plain => Protocol::Tcp,
                    ConnProto::Ssl => Protocol::SslTcp,
                };
                while let Some((packet, consumed)) = demux(&buf) {
                    if tx
                        .send(TcpEvent::Packet {
                            source,
                            proto: p,
                            data: packet,
                        })
                        .is_err()
                    {
                        return;
                    }
                    buf.drain(..consumed);
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // If we already know the protocol, a timeout just means idle.
                if proto == ConnProto::Pending {
                    // A plain-TCP client (STUN/DTLS) never starts with 0x80.
                    if buf.first().is_some_and(|b| *b != 0x80) {
                        proto = ConnProto::Plain;
                    }
                }
                continue;
            }
            Err(_) => break,
        }
    }
    let _ = tx.send(TcpEvent::Close { source });
}

/// Detect fake-SSL vs plain TCP. Returns the new protocol state.
fn detect_proto(buf: &mut Vec<u8>, stream: &mut TcpStream, proto: ConnProto) -> ConnProto {
    if proto != ConnProto::Pending {
        return proto;
    }
    // Plain TCP never starts with 0x80 (STUN 0x00/0x01, DTLS 0x16,
    // RFC 4571 prefix high byte 0x00-0x05). The fake-SSL hello does.
    if !buf.first().is_some_and(|b| *b == 0x80) {
        return ConnProto::Plain;
    }
    if buf.len() < SSL_CLIENT_HELLO.len() {
        return ConnProto::Pending;
    }
    if buf[..SSL_CLIENT_HELLO.len()] == SSL_CLIENT_HELLO {
        if stream.write_all(&SSL_SERVER_HELLO).is_err() {
            return ConnProto::Plain;
        }
        buf.drain(..SSL_CLIENT_HELLO.len());
        return ConnProto::Ssl;
    }
    // 0x80 but not the fake hello: treat as plain (will fail demux and drop).
    ConnProto::Plain
}

/// Returns `(packet, consumed)` for the first complete packet in `buf`.
fn demux(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    if buf.is_empty() {
        return None;
    }

    // STUN: message type 0x0000/0x0001/0x0101..., magic cookie at bytes 4..8.
    if (buf[0] == 0x00 || buf[0] == 0x01) && buf.len() >= 8 && buf[4..8] == [0x21, 0x12, 0xA4, 0x42]
    {
        if buf.len() < 20 {
            return None;
        }
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        let total = 20 + len;
        if buf.len() < total {
            return None;
        }
        return Some((buf[..total].to_vec(), total));
    }

    // DTLS record: content type 20..=23, length at bytes 11..12.
    if (0x14..=0x17).contains(&buf[0]) {
        if buf.len() < 13 {
            return None;
        }
        let len = u16::from_be_bytes([buf[11], buf[12]]) as usize;
        let total = 13 + len;
        if buf.len() < total {
            return None;
        }
        return Some((buf[..total].to_vec(), total));
    }

    // RTP/RTCP: RFC 4571 2-byte length prefix. Strip it before handing to str0m.
    if buf.len() < 2 {
        return None;
    }
    let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    let total = 2 + len;
    if buf.len() < total {
        return None;
    }
    Some((buf[2..total].to_vec(), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_ssl_hello_sizes() {
        assert_eq!(SSL_CLIENT_HELLO.len(), 72);
        assert_eq!(SSL_SERVER_HELLO.len(), 79);
        assert_eq!(SSL_CLIENT_HELLO[0], 0x80);
        assert_eq!(SSL_SERVER_HELLO[0], 0x16);
    }

    #[test]
    fn demux_stun() {
        let mut msg = vec![0x00, 0x01, 0x00, 0x00];
        msg.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
        msg.extend_from_slice(&[0; 12]);
        let (pkt, consumed) = demux(&msg).unwrap();
        assert_eq!(pkt, msg);
        assert_eq!(consumed, 20);
    }

    #[test]
    fn demux_rtp_rfc4571() {
        let payload = vec![0x80, 0xe0, 0x12, 0x34];
        let mut framed = (payload.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(&payload);
        let (pkt, consumed) = demux(&framed).unwrap();
        assert_eq!(pkt, payload);
        assert_eq!(consumed, 6);
    }

    #[test]
    fn demux_dtls() {
        let rec = vec![0x16, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 1, 2, 3, 4];
        let (pkt, consumed) = demux(&rec).unwrap();
        assert_eq!(pkt.len(), 17);
        assert_eq!(consumed, 17);
    }

    #[test]
    fn demux_partial() {
        assert!(demux(&[0x80, 0x46]).is_none());
    }
}
