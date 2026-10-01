//! Regression: SIP/UDP datagram size vs the receive buffer.
//!
//! Before the fix, MAX_UDP_BUF_SIZE was 8192, so an INVITE whose serialized
//! datagram exceeded 8 KiB was dropped by the receiver with no observable
//! error (Linux truncated it silently; Windows failed the read with
//! WSAEMSGSIZE). These tests pin the fixed behaviour and the observable
//! oversize path.

use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rsipstack::transport::connection::{TransportEvent, TransportSender};
use rsipstack::transport::udp::UdpConnection;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn build_invite(client: SocketAddr, server: SocketAddr, idx: usize, body_len: usize) -> Vec<u8> {
    let mut body = String::from("v=0\r\n");
    if body.len() < body_len {
        body.push_str(&"a".repeat(body_len - body.len()));
    }
    let head = format!(
        "INVITE sip:bob@{server} SIP/2.0\r\nVia: SIP/2.0/UDP {client};branch=z9hG4bK{idx};rport\r\nMax-Forwards: 70\r\nFrom: <sip:p@127.0.0.1>;tag=t{idx}\r\nTo: <sip:b@127.0.0.1>\r\nCall-ID: c{idx}@127.0.0.1\r\nCSeq: {idx} INVITE\r\nContact: <sip:p@{client}>\r\nContent-Type: application/sdp\r\nContent-Length: {body_len}\r\n\r\n"
    );
    let mut v = Vec::with_capacity(head.len() + body.len());
    v.extend_from_slice(head.as_bytes());
    v.extend_from_slice(body.as_bytes());
    v
}

/// Sends one INVITE datagram to a serve loop with the given capacity and
/// returns (delivered, captured logs). None uses the default buffer.
fn probe(capacity: Option<usize>, body_len: usize) -> (bool, String) {
    let logs = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sub = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(SharedBuf(logs.clone()))
        .finish();

    let delivered = tracing::subscriber::with_default(sub, || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let conn = UdpConnection::create_connection("127.0.0.1:0".parse().unwrap(), None, None)
                .await
                .unwrap();
            let server = conn.get_addr().get_socketaddr().unwrap();
            let (tx, mut rx) = mpsc::unbounded_channel::<TransportEvent>();
            let sender: TransportSender = tx;
            let c2 = conn.clone();
            tokio::spawn(async move {
                match capacity {
                    Some(cap) => {
                        let _ = c2.serve_loop_with_capacity(sender, None, cap).await;
                    }
                    None => {
                        let _ = c2.serve_loop(sender).await;
                    }
                }
            });
            let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let client_addr = client.local_addr().unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let dg = build_invite(client_addr, server, 1, body_len);
            client.send_to(&dg, server).await.unwrap();
            tokio::time::timeout(Duration::from_millis(500), rx.recv())
                .await
                .is_ok()
        })
    });

    (
        delivered,
        String::from_utf8_lossy(&logs.lock().unwrap()).to_string(),
    )
}

#[test]
fn large_invite_is_delivered_with_default_buffer() {
    // ~8.5 KiB total datagram (body 8198): this exact size was DROPPED while
    // MAX_UDP_BUF_SIZE was 8192.
    let (delivered, _) = probe(None, 8198);
    assert!(delivered, "8.5 KiB INVITE must be delivered");
}

#[test]
fn datagram_larger_than_capacity_is_not_silent() {
    let (delivered, logs) = probe(Some(4096), 5000);
    assert!(
        !delivered,
        "a datagram larger than the receive buffer cannot be read in full"
    );
    assert!(
        logs.contains("filled the receive buffer") || logs.contains("error receiving UDP packet"),
        "the oversize datagram must be logged, got: {logs}"
    );
}

#[test]
fn datagram_below_capacity_is_delivered() {
    let (delivered, _) = probe(Some(8192), 6000);
    assert!(delivered);
}
