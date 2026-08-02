//! 多核分片：每分片一个线程（SO_REUSEPORT UDP socket + 客户端集合 + 事件循环）。
//!
//! 参考 PulseBeam 架构，v1 务实版：
//! - 房间 → 分片由 [`ShardRouter`] 哈希路由（同房间优先同分片，超载级联）
//! - 跨分片事件走 `CrossShardEvent`（媒体/关键帧/输入通道/UDP 转投）
//! - 全局路由表按 (proto, source) 记忆客户端所在分片，避免逐包广播

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use str0m::channel::{ChannelData, ChannelId};
use str0m::media::{Direction, KeyframeRequest, MediaData, Mid, Rid};
use str0m::media::{KeyframeRequestKind, MediaKind};
use str0m::net::Protocol;
use str0m::{Event, IceConnectionState, Input, Output, Rtc, RtcError, net::Receive};

/// 发往分片的命令。
#[allow(clippy::large_enum_variant)]
pub enum ShardCommand {
    /// 新客户端（信令线程创建 Rtc 后送入）。
    AddClient { rtc: Rtc, room: String },
    /// 跨分片事件（媒体/控制/UDP 转投）。
    Cross(CrossShardEvent),
    /// TCP 数据包（由 manager 按路由表分发）。
    TcpPacket {
        source: SocketAddr,
        proto: Protocol,
        data: Vec<u8>,
    },
}

/// 跨分片事件。
#[derive(Debug)]
pub enum CrossShardEvent {
    /// 本分片不认识的 UDP/TCP 包（按路由表或广播转投）。
    Packet {
        source: SocketAddr,
        proto: Protocol,
        data: Vec<u8>,
    },
    TrackOpen {
        room: String,
        origin: ClientId,
        weak: Weak<TrackIn>,
    },
    MediaData {
        room: String,
        origin: ClientId,
        data: Arc<MediaData>,
    },
    KeyframeRequest {
        room: String,
        target: ClientId,
        req: KeyframeRequest,
        mid_in: Mid,
    },
    ChannelData {
        room: String,
        origin: ClientId,
        label: String,
        data: Arc<ChannelData>,
    },
}

/// 全局共享状态（所有分片可见）。
#[derive(Clone)]
pub struct Shared {
    /// (proto, source) → 分片索引：UDP/TCP 包快速路由。
    pub route_table: Arc<RwLock<HashMap<(Protocol, SocketAddr), usize>>>,
    /// room → 持有该房间客户端的分片集合（跨分片转发目标）。
    pub room_registry: Arc<RwLock<HashMap<String, HashSet<usize>>>>,
    /// TCP 写句柄（destination → stream），各分片发送时加锁写。
    pub tcp_streams: Arc<Mutex<HashMap<SocketAddr, TcpStream>>>,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            route_table: Arc::new(RwLock::new(HashMap::new())),
            room_registry: Arc::new(RwLock::new(HashMap::new())),
            tcp_streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register_route(&self, proto: Protocol, source: SocketAddr, shard: usize) {
        self.route_table
            .write()
            .unwrap()
            .insert((proto, source), shard);
    }

    pub fn lookup_route(&self, proto: Protocol, source: SocketAddr) -> Option<usize> {
        self.route_table
            .read()
            .unwrap()
            .get(&(proto, source))
            .copied()
    }

    pub fn room_shards(&self, room: &str) -> Vec<usize> {
        self.room_registry
            .read()
            .unwrap()
            .get(room)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    pub fn join_room(&self, room: &str, shard: usize) {
        self.room_registry
            .write()
            .unwrap()
            .entry(room.to_string())
            .or_default()
            .insert(shard);
    }

    pub fn leave_room(&self, room: &str, shard: usize) {
        let mut reg = self.room_registry.write().unwrap();
        if let Some(set) = reg.get_mut(room) {
            set.remove(&shard);
            if set.is_empty() {
                reg.remove(room);
            }
        }
    }
}

/// 分片线程。
pub struct Shard;

impl Shard {
    /// 启动一个分片线程（channel 由调用方创建，以便各分片互知）。
    pub fn spawn(
        index: usize,
        socket: UdpSocket,
        rx: mpsc::Receiver<ShardCommand>,
        shared: Shared,
        cross_tx: Vec<mpsc::Sender<ShardCommand>>,
        manager_tx: mpsc::Sender<(usize, usize)>,
    ) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name(format!("rd-shard-{index}"))
            .spawn(move || {
                let _ = run_shard(index, socket, rx, shared, cross_tx, manager_tx);
            })
            .expect("spawn shard thread")
    }
}

fn run_shard(
    index: usize,
    socket: UdpSocket,
    rx: mpsc::Receiver<ShardCommand>,
    shared: Shared,
    cross_tx: Vec<mpsc::Sender<ShardCommand>>,
    manager_tx: mpsc::Sender<(usize, usize)>,
) -> Result<(), RtcError> {
    let mut clients: Vec<Client> = vec![];
    let mut to_propagate: VecDeque<Propagated> = VecDeque::new();
    let mut buf = vec![0; 2000];
    // 本分片内每个房间的客户端数（用于清理 room_registry）。
    let mut room_counts: HashMap<String, usize> = HashMap::new();

    let mut last_heartbeat = Instant::now();
    let cross = |target: usize, ev: CrossShardEvent| {
        if target != index {
            let _ = cross_tx[target].send(ShardCommand::Cross(ev));
        }
    };

    let local_poll =
        |clients: &mut Vec<Client>,
         to_propagate: &mut VecDeque<Propagated>,
         cross: &dyn Fn(usize, CrossShardEvent),
         shared: &Shared,
         socket: &UdpSocket,
         tcp_streams: &Arc<Mutex<HashMap<SocketAddr, TcpStream>>>| {
            let mut timeout = Instant::now() + Duration::from_millis(100);
            for client in clients.iter_mut() {
                let t = poll_until_timeout(client, to_propagate, socket, tcp_streams);
                timeout = timeout.min(t);
            }
            while let Some(p) = to_propagate.pop_front() {
                propagate_local(index, p, clients, cross, shared);
            }
            timeout
        };

    loop {
        // 1. 命令队列
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                ShardCommand::AddClient { rtc, room } => {
                    let mut client = Client::new(rtc);
                    client.room = room.clone();
                    let id = client.id;
                    *room_counts.entry(room.clone()).or_default() += 1;
                    shared.join_room(&room, index);
                    for track in clients.iter().flat_map(|c| c.tracks_in.iter()) {
                        if track.id.origin_room() == room {
                            let weak = Arc::downgrade(&track.id);
                            client.handle_track_open(weak);
                        }
                    }
                    clients.push(client);
                    info!("shard {index}: client {id:?} joined room {room}");
                }
                ShardCommand::Cross(ev) => {
                    handle_cross_event(index, ev, &mut clients, &socket, &shared, &cross_tx);
                }
                ShardCommand::TcpPacket {
                    source,
                    proto,
                    data,
                } => {
                    if let Some(input) = build_input(source, proto, &data)
                        && let Some(idx) = clients.iter().position(|c| c.rtc.accepts(&input))
                    {
                        clients[idx].handle_input(input);
                    }
                }
            }
        }

        // 2. 清理断线客户端 + 房间计数
        let before = clients.len();
        clients.retain(|c| c.rtc.is_alive());
        if clients.len() != before {
            let mut counts = std::mem::take(&mut room_counts);
            for c in &clients {
                *counts.entry(c.room.clone()).or_default() += 1;
            }
            for (room, count) in &counts {
                if *count == 0 {
                    shared.leave_room(room, index);
                }
            }
            room_counts = counts;
        }

        // 心跳：向 manager 汇报客户端数（负载路由用）
        if last_heartbeat.elapsed() >= Duration::from_secs(5) {
            last_heartbeat = Instant::now();
            let _ = manager_tx.send((index, clients.len()));
        }

        // 3. poll 客户端 + 本地/跨分片传播
        let timeout = local_poll(
            &mut clients,
            &mut to_propagate,
            &|t, e| cross(t, e),
            &shared,
            &socket,
            &shared.tcp_streams,
        );

        let duration = (timeout - Instant::now()).max(Duration::from_millis(1));
        socket
            .set_read_timeout(Some(duration))
            .expect("setting socket read timeout");

        // 4. UDP 收包
        if let Ok((n, source)) = socket.recv_from(&mut buf) {
            let data = buf[..n].to_vec();
            route_udp(index, source, data, &mut clients, &shared, &cross_tx);
        }

        // 5. 时间推进
        let now = Instant::now();
        for client in &mut clients {
            client.handle_input(Input::Timeout(now));
        }
    }
}

/// UDP 包路由：本地认领 → 路由表转投 → 广播。
fn route_udp(
    index: usize,
    source: SocketAddr,
    data: Vec<u8>,
    clients: &mut [Client],
    shared: &Shared,
    cross_tx: &[mpsc::Sender<ShardCommand>],
) {
    let proto = Protocol::Udp;

    if let Some(input) = build_input(source, proto, &data)
        && let Some(idx) = clients.iter().position(|c| c.rtc.accepts(&input))
    {
        shared.register_route(proto, source, index);
        clients[idx].handle_input(input);
        return;
    }

    if let Some(target) = shared.lookup_route(proto, source) {
        if target != index {
            let _ = cross_tx[target].send(ShardCommand::Cross(CrossShardEvent::Packet {
                source,
                proto,
                data,
            }));
        }
        return;
    }

    // 未知：广播到其他分片（首个 STUN 认领后登记路由）
    for (i, tx) in cross_tx.iter().enumerate() {
        if i != index {
            let _ = tx.send(ShardCommand::Cross(CrossShardEvent::Packet {
                source,
                proto,
                data: data.clone(),
            }));
        }
    }
}

/// 用原始字节构建 Input（解析失败返回 None）。
fn build_input(source: SocketAddr, proto: Protocol, data: &[u8]) -> Option<Input<'_>> {
    let contents = data.try_into().ok()?;
    Some(Input::Receive(
        Instant::now(),
        Receive {
            proto,
            source,
            destination: source,
            contents,
        },
    ))
}

/// 处理跨分片事件。
fn handle_cross_event(
    index: usize,
    ev: CrossShardEvent,
    clients: &mut [Client],
    socket: &UdpSocket,
    shared: &Shared,
    cross_tx: &[mpsc::Sender<ShardCommand>],
) {
    match ev {
        CrossShardEvent::Packet {
            source,
            proto,
            data,
        } => {
            // 转投来的包：本地尝试认领（认领成功登记路由）
            let Some(input) = build_input(source, proto, &data) else {
                return;
            };
            let Some(idx) = clients.iter().position(|c| c.rtc.accepts(&input)) else {
                return;
            };
            clients[idx].handle_input(input);
            shared.register_route(proto, source, index);
            let _ = socket;
            let _ = cross_tx;
        }
        CrossShardEvent::TrackOpen { room, origin, weak } => {
            for client in clients.iter_mut() {
                if client.room == room && client.id != origin {
                    client.handle_track_open(weak.clone());
                }
            }
        }
        CrossShardEvent::MediaData { room, origin, data } => {
            for client in clients.iter_mut() {
                if client.room == room && client.id != origin {
                    client.handle_media_data_out(origin, &data);
                }
            }
        }
        CrossShardEvent::KeyframeRequest {
            room,
            target,
            req,
            mid_in,
        } => {
            for client in clients.iter_mut() {
                if client.room == room && client.id == target {
                    client.handle_keyframe_request(req, mid_in);
                }
            }
        }
        CrossShardEvent::ChannelData {
            room,
            origin,
            label,
            data,
        } => {
            for client in clients.iter_mut() {
                if client.room == room && client.id != origin {
                    client.handle_channel_data_out(&label, &data);
                }
            }
        }
    }
}

fn propagate_local(
    index: usize,
    propagated: Propagated,
    clients: &mut [Client],
    cross: &dyn Fn(usize, CrossShardEvent),
    shared: &Shared,
) {
    let (origin, origin_room) = match &propagated {
        Propagated::TrackOpen(c, _)
        | Propagated::MediaData(c, _)
        | Propagated::ChannelData(c, _, _)
        | Propagated::KeyframeRequest(c, _, _, _) => {
            let room = clients
                .iter()
                .find(|cl| cl.id == *c)
                .map(|cl| cl.room.clone());
            match room {
                Some(r) => (*c, r),
                None => return,
            }
        }
        _ => return,
    };

    let targets: Vec<usize> = shared
        .room_shards(&origin_room)
        .into_iter()
        .filter(|i| *i != index)
        .collect();

    match propagated {
        Propagated::TrackOpen(_, weak) => {
            for client in clients.iter_mut() {
                if client.id == origin || client.room != origin_room {
                    continue;
                }
                client.handle_track_open(weak.clone());
            }
            for t in &targets {
                cross(
                    *t,
                    CrossShardEvent::TrackOpen {
                        room: origin_room.clone(),
                        origin,
                        weak: weak.clone(),
                    },
                );
            }
        }
        Propagated::MediaData(_, data) => {
            for client in clients.iter_mut() {
                if client.id == origin || client.room != origin_room {
                    continue;
                }
                client.handle_media_data_out(origin, &data);
            }
            let arc = Arc::new(data);
            for t in &targets {
                cross(
                    *t,
                    CrossShardEvent::MediaData {
                        room: origin_room.clone(),
                        origin,
                        data: arc.clone(),
                    },
                );
            }
        }
        Propagated::ChannelData(_, label, data) => {
            for client in clients.iter_mut() {
                if client.id == origin || client.room != origin_room {
                    continue;
                }
                client.handle_channel_data_out(&label, &data);
            }
            let arc = Arc::new(data);
            for t in &targets {
                cross(
                    *t,
                    CrossShardEvent::ChannelData {
                        room: origin_room.clone(),
                        origin,
                        label: label.clone(),
                        data: arc.clone(),
                    },
                );
            }
        }
        Propagated::KeyframeRequest(_, req, origin_id, mid_in) => {
            for client in clients.iter_mut() {
                if client.id == origin || client.room != origin_room {
                    continue;
                }
                if origin_id == client.id {
                    client.handle_keyframe_request(req, mid_in);
                }
            }
            for t in &targets {
                cross(
                    *t,
                    CrossShardEvent::KeyframeRequest {
                        room: origin_room.clone(),
                        target: origin_id,
                        req,
                        mid_in,
                    },
                );
            }
        }
        Propagated::Noop | Propagated::Timeout(_) => {}
    }
}
fn poll_until_timeout(
    client: &mut Client,
    queue: &mut VecDeque<Propagated>,
    socket: &UdpSocket,
    tcp_streams: &Arc<Mutex<HashMap<SocketAddr, TcpStream>>>,
) -> Instant {
    loop {
        if !client.rtc.is_alive() {
            return Instant::now();
        }
        let propagated = client.poll_output(socket, tcp_streams);
        if let Propagated::Timeout(t) = propagated {
            return t;
        }
        queue.push_back(propagated)
    }
}

// ---------- Client（从单线程版迁移，新增 room 字段） ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(u64);

impl Deref for ClientId {
    type Target = u64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct TrackIn {
    pub origin: ClientId,
    pub room: String,
    pub mid: Mid,
    pub kind: MediaKind,
}

impl TrackIn {
    fn origin_room(&self) -> &str {
        &self.room
    }
}

pub struct TrackInEntry {
    pub id: Arc<TrackIn>,
    pub last_keyframe_request: Option<Instant>,
}

pub struct TrackOut {
    pub(crate) track_in: Weak<TrackIn>,
    pub(crate) state: TrackOutState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackOutState {
    ToOpen,
    Negotiating(Mid),
    Open(Mid),
    ToStop(Mid),
    NegotiatingStop(Mid),
}

impl TrackOut {
    fn mid(&self) -> Option<Mid> {
        match self.state {
            TrackOutState::ToOpen => None,
            TrackOutState::Negotiating(m)
            | TrackOutState::Open(m)
            | TrackOutState::ToStop(m)
            | TrackOutState::NegotiatingStop(m) => Some(m),
        }
    }
}

pub struct Client {
    pub id: ClientId,
    pub room: String,
    pub rtc: Rtc,
    pub pending: Option<str0m::change::SdpPendingOffer>,
    pub cid: Option<ChannelId>,
    pub channels: HashMap<String, ChannelId>,
    pub tracks_in: Vec<TrackInEntry>,
    pub tracks_out: Vec<TrackOut>,
    pub chosen_rid: Option<Rid>,
}

impl Client {
    fn new(rtc: Rtc) -> Client {
        static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
        let next_id = ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        Client {
            id: ClientId(next_id),
            room: String::new(),
            rtc,
            pending: None,
            cid: None,
            channels: HashMap::new(),
            tracks_in: vec![],
            tracks_out: vec![],
            chosen_rid: None,
        }
    }

    fn handle_input(&mut self, input: Input) {
        if !self.rtc.is_alive() {
            return;
        }
        if let Err(e) = self.rtc.handle_input(input) {
            warn!("Client ({}) disconnected: {:?}", *self.id, e);
            self.rtc.disconnect();
        }
    }

    fn poll_output(
        &mut self,
        socket: &UdpSocket,
        tcp_streams: &Arc<Mutex<HashMap<SocketAddr, TcpStream>>>,
    ) -> Propagated {
        if !self.rtc.is_alive() {
            return Propagated::Noop;
        }
        if self.negotiate_if_needed() {
            return Propagated::Noop;
        }
        match self.rtc.poll_output() {
            Ok(output) => self.handle_output(output, socket, tcp_streams),
            Err(e) => {
                warn!("Client ({}) poll_output failed: {:?}", *self.id, e);
                self.rtc.disconnect();
                Propagated::Noop
            }
        }
    }

    fn handle_output(
        &mut self,
        output: Output,
        socket: &UdpSocket,
        tcp_streams: &Arc<Mutex<HashMap<SocketAddr, TcpStream>>>,
    ) -> Propagated {
        match output {
            Output::Transmit(transmit) => {
                match transmit.proto {
                    Protocol::Udp => {
                        socket
                            .send_to(&transmit.contents, transmit.destination)
                            .expect("sending UDP data");
                    }
                    Protocol::Tcp | Protocol::SslTcp => {
                        let mut streams = tcp_streams.lock().unwrap();
                        let Some(stream) = streams.get_mut(&transmit.destination) else {
                            warn!(
                                "No TCP stream for {}, dropping {} bytes",
                                transmit.destination,
                                transmit.contents.len()
                            );
                            return Propagated::Noop;
                        };
                        let is_media = transmit.contents.first().is_some_and(|b| b & 0xC0 == 0x80);
                        let res = if is_media {
                            let len = (transmit.contents.len() as u16).to_be_bytes();
                            use std::io::Write;
                            stream
                                .write_all(&len)
                                .and_then(|_| stream.write_all(&transmit.contents))
                        } else {
                            use std::io::Write;
                            stream.write_all(&transmit.contents)
                        };
                        if let Err(e) = res {
                            warn!("TCP write to {} failed: {:?}", transmit.destination, e);
                            streams.remove(&transmit.destination);
                        }
                    }
                    p => warn!("Unsupported transmit protocol: {:?}", p),
                }
                Propagated::Noop
            }
            Output::Timeout(t) => Propagated::Timeout(t),
            Output::Event(e) => match e {
                Event::IceConnectionStateChange(v) => {
                    if v == IceConnectionState::Disconnected {
                        self.rtc.disconnect();
                    }
                    Propagated::Noop
                }
                Event::MediaAdded(e) => self.handle_media_added(e.mid, e.kind),
                Event::MediaData(data) => self.handle_media_data_in(data),
                Event::KeyframeRequest(req) => self.handle_incoming_keyframe_req(req),
                Event::ChannelOpen(cid, label) => {
                    self.channels.insert(label, cid);
                    if self.cid.is_none() {
                        self.cid = Some(cid);
                    }
                    Propagated::Noop
                }
                Event::ChannelData(data) => self.handle_channel_data(data),
                Event::ChannelClose(cid) => {
                    self.channels.retain(|_, v| *v != cid);
                    if self.cid == Some(cid) {
                        self.cid = None;
                    }
                    Propagated::Noop
                }
                Event::MediaIngressStats(data) => {
                    info!("{:?}", data);
                    Propagated::Noop
                }
                Event::MediaEgressStats(data) => {
                    info!("{:?}", data);
                    Propagated::Noop
                }
                Event::PeerStats(data) => {
                    info!("{:?}", data);
                    Propagated::Noop
                }
                _ => Propagated::Noop,
            },
        }
    }

    fn handle_media_added(&mut self, mid: Mid, kind: MediaKind) -> Propagated {
        let track_in = TrackInEntry {
            id: Arc::new(TrackIn {
                origin: self.id,
                room: self.room.clone(),
                mid,
                kind,
            }),
            last_keyframe_request: None,
        };
        let weak = Arc::downgrade(&track_in.id);
        self.tracks_in.push(track_in);
        Propagated::TrackOpen(self.id, weak)
    }

    fn handle_media_data_in(&mut self, data: MediaData) -> Propagated {
        if !data.contiguous {
            self.request_keyframe_throttled(data.mid, data.rid, KeyframeRequestKind::Fir);
        }
        Propagated::MediaData(self.id, data)
    }

    fn request_keyframe_throttled(
        &mut self,
        mid: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    ) {
        let Some(mut writer) = self.rtc.writer(mid) else {
            return;
        };
        let Some(track_entry) = self.tracks_in.iter_mut().find(|t| t.id.mid == mid) else {
            return;
        };
        if track_entry
            .last_keyframe_request
            .map(|t| t.elapsed() < Duration::from_secs(1))
            .unwrap_or(false)
        {
            return;
        }
        _ = writer.request_keyframe(rid, kind);
        track_entry.last_keyframe_request = Some(Instant::now());
    }

    fn handle_incoming_keyframe_req(&self, mut req: KeyframeRequest) -> Propagated {
        let Some(track_out) = self.tracks_out.iter().find(|t| t.mid() == Some(req.mid)) else {
            return Propagated::Noop;
        };
        let Some(track_in) = track_out.track_in.upgrade() else {
            return Propagated::Noop;
        };
        req.rid = self.chosen_rid;
        Propagated::KeyframeRequest(self.id, req, track_in.origin, track_in.mid)
    }

    fn negotiate_if_needed(&mut self) -> bool {
        if self.cid.is_none() || self.pending.is_some() {
            return false;
        }
        for track in &mut self.tracks_out {
            if let TrackOutState::Open(m) = track.state
                && track.track_in.upgrade().is_none()
            {
                track.state = TrackOutState::ToStop(m);
            }
        }
        let mut change = self.rtc.sdp_api();
        for track in &mut self.tracks_out {
            match track.state {
                TrackOutState::ToOpen => {
                    if let Some(track_in) = track.track_in.upgrade() {
                        let stream_id = track_in.origin.to_string();
                        let mid = change.add_media(
                            track_in.kind,
                            Direction::SendOnly,
                            Some(stream_id),
                            None,
                            None,
                        );
                        track.state = TrackOutState::Negotiating(mid);
                    }
                }
                TrackOutState::ToStop(mid) => {
                    change.stop_media(mid);
                    track.state = TrackOutState::NegotiatingStop(mid);
                }
                _ => {}
            }
        }
        if !change.has_changes() {
            return false;
        }
        let Some((offer, pending)) = change.apply() else {
            return false;
        };
        let Some(mut channel) = self.cid.and_then(|id| self.rtc.channel(id)) else {
            return false;
        };
        let json = serde_json::to_string(&offer).unwrap();
        channel
            .write(false, json.as_bytes())
            .expect("to write offer");
        self.pending = Some(pending);
        true
    }

    fn handle_channel_data(&mut self, d: ChannelData) -> Propagated {
        use str0m::change::{SdpAnswer, SdpOffer};
        if let Ok(offer) = serde_json::from_slice::<'_, SdpOffer>(&d.data) {
            self.handle_offer(offer);
            return Propagated::Noop;
        }
        if let Ok(answer) = serde_json::from_slice::<'_, SdpAnswer>(&d.data) {
            self.handle_answer(answer);
            return Propagated::Noop;
        }
        let Some(label) = self
            .channels
            .iter()
            .find(|(_, v)| **v == d.id)
            .map(|(l, _)| l.clone())
        else {
            return Propagated::Noop;
        };
        if label == "offer/answer" {
            warn!("Unrecognized data on signal channel");
            return Propagated::Noop;
        }
        Propagated::ChannelData(self.id, label, d)
    }

    fn handle_channel_data_out(&mut self, label: &str, data: &ChannelData) {
        let Some(cid) = self.channels.get(label).copied() else {
            return;
        };
        let Some(mut channel) = self.rtc.channel(cid) else {
            return;
        };
        if let Err(e) = channel.write(data.binary, &data.data) {
            warn!("Client ({}) channel write failed: {:?}", *self.id, e);
        }
    }

    fn handle_offer(&mut self, offer: str0m::change::SdpOffer) {
        let answer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .expect("offer to be accepted");
        for track in &mut self.tracks_out {
            match track.state {
                TrackOutState::Negotiating(_) => track.state = TrackOutState::ToOpen,
                TrackOutState::NegotiatingStop(m) => track.state = TrackOutState::ToStop(m),
                _ => {}
            }
        }
        let mut channel = self
            .cid
            .and_then(|id| self.rtc.channel(id))
            .expect("channel to be open");
        let json = serde_json::to_string(&answer).unwrap();
        channel
            .write(false, json.as_bytes())
            .expect("to write answer");
    }

    fn handle_answer(&mut self, answer: str0m::change::SdpAnswer) {
        if let Some(pending) = self.pending.take() {
            self.rtc
                .sdp_api()
                .accept_answer(pending, answer)
                .expect("answer to be accepted");
            for track in &mut self.tracks_out {
                if let TrackOutState::Negotiating(m) = track.state {
                    track.state = TrackOutState::Open(m);
                }
            }
            self.tracks_out
                .retain(|t| !matches!(t.state, TrackOutState::NegotiatingStop(_)));
        }
    }

    fn handle_track_open(&mut self, track_in: Weak<TrackIn>) {
        self.tracks_out.push(TrackOut {
            track_in,
            state: TrackOutState::ToOpen,
        });
    }

    fn handle_media_data_out(&mut self, origin: ClientId, data: &MediaData) {
        let Some(mid) = self
            .tracks_out
            .iter()
            .find(|o| {
                o.track_in
                    .upgrade()
                    .filter(|i| i.origin == origin && i.mid == data.mid)
                    .is_some()
            })
            .and_then(|o| o.mid())
        else {
            return;
        };
        if data.rid.is_some() && data.rid != Some("h".into()) {
            return;
        }
        if self.chosen_rid != data.rid {
            self.chosen_rid = data.rid;
        }
        let Some(writer) = self.rtc.writer(mid) else {
            return;
        };
        let Some(pt) = writer.match_params(data.params) else {
            return;
        };
        if let Err(e) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
            warn!("Client ({}) failed: {:?}", *self.id, e);
            self.rtc.disconnect();
        }
    }

    fn handle_keyframe_request(&mut self, req: KeyframeRequest, mid_in: Mid) {
        let has_incoming_track = self.tracks_in.iter().any(|i| i.id.mid == mid_in);
        if !has_incoming_track {
            return;
        }
        let Some(mut writer) = self.rtc.writer(mid_in) else {
            return;
        };
        if let Err(e) = writer.request_keyframe(req.rid, req.kind) {
            info!("request_keyframe failed: {:?}", e);
        }
    }
}

/// 客户端间传播的事件。
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Propagated {
    Noop,
    Timeout(Instant),
    TrackOpen(ClientId, Weak<TrackIn>),
    MediaData(ClientId, MediaData),
    ChannelData(ClientId, String, ChannelData),
    KeyframeRequest(ClientId, KeyframeRequest, ClientId, Mid),
}
