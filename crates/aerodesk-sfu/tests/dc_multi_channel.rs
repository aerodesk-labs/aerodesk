//! #192 复现：SFU data channel 多通道 offer / 连接后新增通道 / file 标签转发。
//!
//! 直接驱动两个 str0m Rtc（模拟 Web viewer 与 SFU/publisher），验证：
//! 1) 初始 offer 含多个 data channel（含 file）→ 对端全部 ChannelOpen + 双向数据
//! 2) 连接后新增通道（重协商）→ 对端 ChannelOpen + 数据转发

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::net::Protocol;
use str0m::{Candidate, Event, Input, Output, Rtc};

const CHANNELS: &[&str] = &["offer/answer", "input", "control", "file", "cursor", "cmd"];

struct Node {
    rtc: Rtc,
    sock: UdpSocket,
    peer: Option<std::net::SocketAddr>,
    opened: Vec<(str0m::channel::ChannelId, String)>,
    received: Vec<(String, Vec<u8>)>,
}

impl Node {
    fn new(now: Instant) -> Self {
        let mut rtc = Rtc::new(now);
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
        sock.set_read_timeout(Some(Duration::from_millis(10))).ok();
        let addr = sock.local_addr().unwrap();
        rtc.add_local_candidate(Candidate::host(addr, "udp").unwrap());
        Node {
            rtc,
            sock,
            peer: None,
            opened: Vec::new(),
            received: Vec::new(),
        }
    }

    fn pump(&mut self, now: Instant) {
        let local = self.sock.local_addr().unwrap();
        let mut buf = [0u8; 4096];
        while let Ok((n, source)) = self.sock.recv_from(&mut buf) {
            if let Ok(contents) = buf[..n].try_into() {
                let _ = self.rtc.handle_input(Input::Receive(
                    now,
                    str0m::net::Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: local,
                        contents,
                    },
                ));
            }
        }
        let _ = self.rtc.handle_input(Input::Timeout(now));
        while let Ok(output) = self.rtc.poll_output() {
            match output {
                Output::Transmit(t) => {
                    if let Some(peer) = self.peer {
                        let _ = self.sock.send_to(&t.contents, peer);
                    }
                }
                // 遇到 Timeout 必须退出本轮排空，否则反复返回同一 Timeout
                Output::Timeout(_) => break,
                Output::Event(e) => match e {
                    Event::ChannelOpen(cid, label) => self.opened.push((cid, label)),
                    Event::ChannelData(d) => {
                        let label = self
                            .opened
                            .iter()
                            .rev()
                            .find(|(c, _)| *c == d.id)
                            .map(|(_, l)| l.clone())
                            .unwrap_or_else(|| format!("cid{:?}", d.id));
                        self.received.push((label, d.data.to_vec()));
                    }
                    _ => {}
                },
            }
        }
    }

    fn is_connected(&self) -> bool {
        self.rtc.is_connected()
    }

    fn send(&mut self, label: &str, data: &[u8]) -> bool {
        let cid = self
            .opened
            .iter()
            .find(|(_, l)| l == label)
            .map(|(c, _)| *c);
        let Some(cid) = cid else {
            return false;
        };
        let Some(mut ch) = self.rtc.channel(cid) else {
            return false;
        };
        ch.write(false, data).unwrap_or(false)
    }
}

fn connect_pair(a: &mut Node, b: &mut Node, now: Instant) {
    // a 提供 offer（含全部通道），b 应答
    let mut change = a.rtc.sdp_api();
    for label in CHANNELS {
        change.add_channel(label.to_string());
    }
    let (offer, pending) = change.apply().expect("offer");
    let answer = b.rtc.sdp_api().accept_offer(offer).expect("accept");
    a.rtc
        .sdp_api()
        .accept_answer(pending, answer)
        .expect("answer");
    a.peer = Some(b.sock.local_addr().unwrap());
    b.peer = Some(a.sock.local_addr().unwrap());

    // ICE 泵到连接
    let deadline = now + Duration::from_secs(5);
    let mut t = now;
    while (t < deadline) && !(a.is_connected() && b.is_connected()) {
        a.pump(t);
        b.pump(t);
        t += Duration::from_millis(10);
    }
    assert!(a.is_connected() && b.is_connected(), "ICE 未连接");
    // 连接后继续泵，直到全部通道在两侧打开（SCTP 建立后逐通道 DCEP/协商）
    let open_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let t = Instant::now();
        a.pump(t);
        b.pump(t);
        let all_open = |n: &Node| {
            CHANNELS
                .iter()
                .all(|l| n.opened.iter().any(|(_, x)| x == l))
        };
        if all_open(a) && all_open(b) {
            break;
        }
        assert!(
            Instant::now() < open_deadline,
            "通道未全部打开 a={:?} b={:?}",
            a.opened,
            b.opened
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 1) 初始 offer 多通道（含 file）：对端全部打开 + 双向数据。
#[test]
fn multi_channel_offer_file_label_forwards_both_ways() {
    let now = Instant::now();
    let mut viewer = Node::new(now);
    let mut publisher = Node::new(now);
    connect_pair(&mut viewer, &mut publisher, now);

    for label in CHANNELS {
        assert!(
            publisher.opened.iter().any(|(_, l)| l == label),
            "publisher 未打开 {label}: opened={:?}",
            publisher.opened
        );
        assert!(
            viewer.opened.iter().any(|(_, l)| l == label),
            "viewer 未打开 {label}"
        );
    }

    // viewer → publisher：file 通道数据
    assert!(viewer.send("file", b"hello-file-viewer"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let t = Instant::now();
        viewer.pump(t);
        publisher.pump(t);
        if publisher
            .received
            .iter()
            .any(|(l, d)| l == "file" && d == b"hello-file-viewer")
        {
            break;
        }
        assert!(Instant::now() < deadline, "file 数据未到达 publisher");
        std::thread::sleep(Duration::from_millis(5));
    }

    // publisher → viewer：file 通道数据（双向）
    assert!(publisher.send("file", b"hello-file-publisher"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let t = Instant::now();
        publisher.pump(t);
        viewer.pump(t);
        if viewer
            .received
            .iter()
            .any(|(l, d)| l == "file" && d == b"hello-file-publisher")
        {
            break;
        }
        assert!(Instant::now() < deadline, "file 回传未到达 viewer");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 2) 连接后新增通道（模拟 Web 连接后 createDataChannel('file')）：
///    触发重协商 offer → 对端打开 + 数据转发。
#[test]
fn channel_added_after_connect_renegotiates_and_forwards() {
    let now = Instant::now();
    let mut viewer = Node::new(now);
    let mut publisher = Node::new(now);
    connect_pair(&mut viewer, &mut publisher, now);

    // viewer 连接后新增 file 通道（Web 模式：signalChannel.onopen → createDataChannel）。
    // str0m：连接后 add_channel().apply() 返回 None = 走 in-band DCEP（无需重协商 offer），
    // 与浏览器 createDataChannel 行为一致；返回 Some 时走 SFU 同款重协商路径。
    let mut change = viewer.rtc.sdp_api();
    change.add_channel("file".to_string());
    if let Some((offer, pending)) = change.apply() {
        let answer = publisher
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .expect("accept re-offer");
        viewer
            .rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .expect("accept re-answer");
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let t = Instant::now();
        viewer.pump(t);
        publisher.pump(t);
        if viewer.opened.iter().any(|(_, l)| l == "file")
            && publisher.opened.iter().any(|(_, l)| l == "file")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "重协商后 file 未打开: view={:?} pub={:?}",
            viewer.opened,
            publisher.opened
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(viewer.send("file", b"late-file-data"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let t = Instant::now();
        viewer.pump(t);
        publisher.pump(t);
        if publisher
            .received
            .iter()
            .any(|(l, d)| l == "file" && d == b"late-file-data")
        {
            break;
        }
        assert!(Instant::now() < deadline, "重协商 file 数据未转发");
        std::thread::sleep(Duration::from_millis(5));
    }
}
