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

    fn on_event(&mut self, e: Event) {
        match e {
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
        }
    }

    fn drain_outputs(&mut self) {
        while let Ok(output) = self.rtc.poll_output() {
            match output {
                Output::Transmit(t) => {
                    if let Some(peer) = self.peer {
                        let _ = self.sock.send_to(&t.contents, peer);
                    }
                }
                // 遇到 Timeout 必须退出本轮排空，否则反复返回同一 Timeout
                Output::Timeout(_) => break,
                Output::Event(e) => self.on_event(e),
            }
        }
    }

    /// 收一个包并处理（不批量）：str0m `handle_input` 调用链深，批量收包会把
    /// 多个 DTLS/SCTP 解包叠在同一栈上导致 stack overflow（见 LESSON）。
    fn pump_once(&mut self, now: Instant) {
        let local = self.sock.local_addr().unwrap();
        let mut buf = [0u8; 4096];
        if let Ok((n, source)) = self.sock.recv_from(&mut buf) {
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
        self.drain_outputs();
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
        self.drain_outputs();
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

/// 3) 大消息（>256KiB）data channel 往返：#331 回归——SCTP max-message-size
///    提升到 1MiB 后，超过旧 256KiB 上限的 cmd 响应不应被静默丢弃。
///
/// str0m 的 DTLS/SCTP 解包调用链深，512KiB 消息又会被分片成大量 SCTP 块；
/// 在默认 2MiB 测试线程栈上会 stack overflow（Windows 尤甚）。放到 8MiB 栈
/// 线程运行——本测试验证的是 SCTP 消息上限，不是栈占用。
#[test]
fn large_data_channel_message_roundtrips() {
    std::thread::Builder::new()
        .name("large-dc-message".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(large_data_channel_message_roundtrips_inner)
        .expect("spawn large-dc-message thread")
        .join()
        .expect("large-dc-message thread panicked");
}

fn large_data_channel_message_roundtrips_inner() {
    let now = Instant::now();
    let mut viewer = Node::new(now);
    let mut publisher = Node::new(now);
    connect_pair(&mut viewer, &mut publisher, now);

    // 512KiB：复现 #331 list-processes 真实响应量级（>256KiB 旧上限，<1MiB 新上限）。
    let payload: Vec<u8> = (0..(512 * 1024)).map(|i| (i % 251) as u8).collect();
    assert!(
        viewer.send("cmd", &payload),
        "512KiB 消息应被接受（send=true），否则 SCTP 消息上限未提升"
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let t = Instant::now();
        // 大消息会分片成大量 UDP 包：逐包处理（pump_once），避免批量 handle_input
        // 叠栈导致 stack overflow（见 LESSON_SFU主循环改批量UDP收包会栈溢出）。
        viewer.pump_once(t);
        publisher.pump_once(t);
        if publisher
            .received
            .iter()
            .any(|(l, d)| l == "cmd" && d.as_slice() == payload.as_slice())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "512KiB data channel 消息未完整到达 publisher（#331 回归）"
        );
    }
}
