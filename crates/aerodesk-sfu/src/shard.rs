//! 多核分片：每分片一个线程（SO_REUSEPORT UDP socket + 客户端集合 + 事件循环）。
//!
//! 参考 PulseBeam 架构，v1 务实版：
//! - 房间 → 分片由 [`ShardRouter`] 哈希路由（同房间优先同分片，超载级联）
//! - 跨分片事件走 `CrossShardEvent`（媒体/关键帧/输入通道/UDP 转投）
//! - 全局路由表按 (proto, source) 记忆客户端所在分片，避免逐包广播

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::ops::Deref;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use str0m::bwe::BweKind;
use str0m::channel::{ChannelData, ChannelId};
use str0m::media::{Direction, KeyframeRequest, MediaData, Mid, Rid};
use str0m::media::{KeyframeRequestKind, MediaKind};
use str0m::net::Protocol;
use str0m::{Event, IceConnectionState, Input, Output, Rtc, RtcError, net::Receive};

use aerodesk_protocol::signal::Role;

use crate::bitrate::{BitrateController, Layer};
use aerodesk_sfu::recorder::Recorder;

/// 发往分片的命令。
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum ShardCommand {
    /// 新客户端（信令线程创建 Rtc 后送入）。
    /// role 用于 #12 角色校验：viewer 禁止发布媒体。
    /// dc_ready（#467）：/start 声明客户端会发 signal_ready，SFU 据此门控重协商。
    AddClient {
        rtc: Rtc,
        room: String,
        role: Role,
        dc_ready: bool,
    },
    /// 跨分片事件（媒体/控制/UDP 转投）。
    Cross(CrossShardEvent),
    /// TCP 数据包（由 manager 按路由表分发）。
    TcpPacket {
        source: SocketAddr,
        proto: Protocol,
        data: Vec<u8>,
    },
    /// 踢人（会话管理 API，#240）：按 client_id 断开客户端，下一轮清理回收。
    Kick { client_id: u64 },
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

/// 分片指标（原子计数，供 /metrics 读取）。
pub struct ShardMetrics {
    pub clients: AtomicUsize,
    pub rx_packets: AtomicU64,
    pub rx_bytes: AtomicU64,
    pub tx_packets: AtomicU64,
    pub tx_bytes: AtomicU64,
    /// #238 媒体质量：最近心跳聚合（5s）——
    /// RTT 均值（纳秒，0=无样本；输出转微秒）、egress/ingress loss 均值（×1e6）、
    /// BWE 目标码率均值（bps）、有统计样本的客户端数。
    pub rtt_avg_ns: AtomicU64,
    pub egress_loss_ppm: AtomicU64,
    pub ingress_loss_ppm: AtomicU64,
    pub bwe_tx_bps: AtomicU64,
    pub qos_clients: AtomicUsize,
    /// 分片线程 CPU 占用（百分比 ×100，0..=10000；非 Linux 恒 0）。
    pub cpu_percent_x100: AtomicU64,
}

impl ShardMetrics {
    pub fn new() -> Self {
        Self {
            clients: AtomicUsize::new(0),
            rx_packets: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            tx_packets: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            rtt_avg_ns: AtomicU64::new(0),
            egress_loss_ppm: AtomicU64::new(0),
            ingress_loss_ppm: AtomicU64::new(0),
            bwe_tx_bps: AtomicU64::new(0),
            qos_clients: AtomicUsize::new(0),
            cpu_percent_x100: AtomicU64::new(0),
        }
    }
}

/// 客户端最新媒体质量快照（#238，Event::PeerStats / EgressBitrateEstimate 更新）。
#[derive(Default)]
pub struct ClientQos {
    pub rtt: Option<std::time::Duration>,
    pub egress_loss: Option<f32>,
    pub ingress_loss: Option<f32>,
    pub bwe_bps: u64,
}

/// 客户端会话快照（会话管理 API 用，#240）。
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: u64,
    pub room: String,
    pub role: Role,
    pub shard: usize,
    /// 加入时刻（Unix 微秒）。
    pub joined_at: u64,
}

/// 全局共享状态（所有分片可见）。
#[derive(Clone)]
pub struct Shared {
    /// (proto, source) → 分片索引：UDP/TCP 包快速路由。
    pub route_table: Arc<RwLock<HashMap<(Protocol, SocketAddr), usize>>>,
    /// room → 持有该房间客户端的分片集合（跨分片转发目标）。
    pub room_registry: Arc<RwLock<HashMap<String, HashSet<usize>>>>,
    /// room → 有订阅者（viewer 已接收 track）的分片集合（订阅驱动转发：#132）。
    pub subscribers: Arc<RwLock<HashMap<String, HashSet<usize>>>>,
    /// (room, client_id) → 所在分片（关键帧请求定向：#136）。
    pub client_shards: Arc<RwLock<HashMap<(String, u64), usize>>>,
    /// TCP 写句柄（destination → stream），各分片发送时加锁写。
    pub tcp_streams: Arc<Mutex<HashMap<SocketAddr, TcpStream>>>,
    /// 每分片指标（索引 = shard id）。
    pub metrics: Arc<Vec<ShardMetrics>>,
    /// 房间在线人数（#180 /start 准入配额）。
    pub room_clients: Arc<Mutex<HashMap<String, usize>>>,
    /// 全局在线人数（#180）。
    pub total_clients: Arc<AtomicUsize>,
    /// 每房间人数上限（0=不限，#180）。
    pub max_room_clients: usize,
    /// 全局连接上限（0=不限，#180）。
    pub max_total_clients: usize,
    /// 可选录制器（RECORD_DIR 开启时存在）。
    pub recorder: Option<Arc<Recorder>>,
    /// 会话注册表：client_id → 会话快照（会话管理 API 读取，#240）。
    pub sessions: Arc<RwLock<HashMap<u64, SessionInfo>>>,
    /// 分片线程 TID（-1=未捕获/非 Linux），供 manager 按线程采样 CPU。
    pub shard_tids: Arc<Vec<AtomicI32>>,
}

impl Shared {
    pub fn new(shard_count: usize) -> Self {
        Self {
            route_table: Arc::new(RwLock::new(HashMap::new())),
            room_registry: Arc::new(RwLock::new(HashMap::new())),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            client_shards: Arc::new(RwLock::new(HashMap::new())),
            tcp_streams: Arc::new(Mutex::new(HashMap::new())),
            metrics: Arc::new((0..shard_count).map(|_| ShardMetrics::new()).collect()),
            recorder: None,
            room_clients: Arc::new(Mutex::new(HashMap::new())),
            total_clients: Arc::new(AtomicUsize::new(0)),
            max_room_clients: 0,
            max_total_clients: 0,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            shard_tids: Arc::new((0..shard_count).map(|_| AtomicI32::new(-1)).collect()),
        }
    }

    /// /start 准入预留（#180）：房间/全局任一超限拒绝；通过则计数 +1（AddClient 失败时由调用方 release 回滚）。
    pub fn try_reserve(
        &self,
        room: &str,
        room_cap: usize,
        total_cap: usize,
    ) -> Result<(), &'static str> {
        let mut m = self
            .room_clients
            .lock()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
        if room_cap > 0 && m.get(room).copied().unwrap_or(0) >= room_cap {
            return Err("room full");
        }
        if total_cap > 0 && self.total_clients.load(Ordering::Relaxed) >= total_cap {
            return Err("server full");
        }
        *m.entry(room.to_string()).or_default() += 1;
        self.total_clients.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// 释放一个连接（断线/AddClient 失败回滚，#180）。
    pub fn release(&self, room: &str) {
        let mut m = self
            .room_clients
            .lock()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
        if let Some(n) = m.get_mut(room) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(room);
            }
        }
        self.total_clients.fetch_sub(1, Ordering::Relaxed);
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
        let mut reg = self
            .room_registry
            .write()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
        if let Some(set) = reg.get_mut(room) {
            set.remove(&shard);
            if set.is_empty() {
                reg.remove(room);
            }
        }
        // 订阅者分片同步清理：分片离开房间即不再是媒体转发目标。
        let mut sub = self
            .subscribers
            .write()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
        if let Some(set) = sub.get_mut(room) {
            set.remove(&shard);
            if set.is_empty() {
                sub.remove(room);
            }
        }
    }

    /// 有订阅者（viewer）的分片（订阅驱动媒体转发目标）。
    pub fn subscriber_shards(&self, room: &str) -> Vec<usize> {
        self.subscribers
            .read()
            .unwrap()
            .get(room)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    /// 登记分片为该房间的订阅者（幂等）。
    pub fn join_subscriber(&self, room: &str, shard: usize) {
        self.subscribers
            .write()
            .unwrap()
            .entry(room.to_string())
            .or_default()
            .insert(shard);
    }

    /// 登记客户端所在分片（关键帧请求定向用）。
    pub fn register_client(&self, room: &str, client_id: u64, shard: usize) {
        self.client_shards
            .write()
            .unwrap()
            .insert((room.to_string(), client_id), shard);
    }

    /// 注销单个客户端（分片清理断线时调用）。
    pub fn unregister_client(&self, room: &str, client_id: u64, shard: usize) {
        let mut reg = self
            .client_shards
            .write()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
        if reg.get(&(room.to_string(), client_id)) == Some(&shard) {
            reg.remove(&(room.to_string(), client_id));
        }
    }

    /// 查询客户端所在分片。
    pub fn client_shard(&self, room: &str, client_id: u64) -> Option<usize> {
        self.client_shards
            .read()
            .unwrap()
            .get(&(room.to_string(), client_id))
            .copied()
    }

    /// 登记会话（AddClient 成功后由分片调用，#240）。
    pub fn register_session(&self, info: SessionInfo) {
        self.sessions
            .write()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover)
            .insert(info.id, info);
    }

    /// 注销会话（仅当 shard 匹配时删除，避免跨分片误删，#240）。
    pub fn unregister_session(&self, client_id: u64, shard: usize) {
        let mut reg = self
            .sessions
            .write()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
        if reg.get(&client_id).is_some_and(|s| s.shard == shard) {
            reg.remove(&client_id);
        }
    }

    /// 查询单个会话。
    pub fn session(&self, client_id: u64) -> Option<SessionInfo> {
        self.sessions
            .read()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover)
            .get(&client_id)
            .cloned()
    }

    /// 会话快照（会话管理 API：房间/客户端列表，#240）。
    pub fn session_snapshot(&self) -> Vec<SessionInfo> {
        self.sessions
            .read()
            .unwrap_or_else(aerodesk_protocol::util::lock_recover)
            .values()
            .cloned()
            .collect()
    }
}

/// 分片线程。
/// Demuxer 快路径缓存：(source) → client 索引。
///
/// 首个包线性扫描认领后登记，后续同源包 O(1) 命中，避免多参与者下逐包
/// `clients.iter().position(...)` 的 O(n) 开销（borrow-from-pulsebeam v2 #2）。
/// 有界：超过容量直接清空（简单、可自愈）；客户端增删时由调用方 clear。
struct AddrCache {
    map: HashMap<SocketAddr, usize>,
    cap: usize,
}

impl AddrCache {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            cap,
        }
    }

    fn lookup(&self, addr: &SocketAddr) -> Option<usize> {
        self.map.get(addr).copied()
    }

    fn insert(&mut self, addr: SocketAddr, idx: usize) {
        if self.map.len() >= self.cap {
            self.map.clear();
        }
        self.map.insert(addr, idx);
    }

    fn remove(&mut self, addr: &SocketAddr) {
        self.map.remove(addr);
    }

    fn clear(&mut self) {
        self.map.clear();
    }

    fn len(&self) -> usize {
        self.map.len()
    }
}

/// Demuxer 快路径：命中返回对应 client，未命中线性扫描并登记。
fn demux_client<'a>(
    clients: &'a mut [Client],
    input: &Input<'_>,
    source: SocketAddr,
    cache: &mut AddrCache,
) -> Option<&'a mut Client> {
    if let Some(idx) = cache.lookup(&source) {
        if idx < clients.len() && clients[idx].rtc.accepts(input) {
            return Some(&mut clients[idx]);
        }
        // 源地址复用/索引失效：删除并回退线性扫描
        cache.remove(&source);
    }
    let idx = clients.iter().position(|c| c.rtc.accepts(input))?;
    cache.insert(source, idx);
    Some(&mut clients[idx])
}

/// 当前线程 TID（Linux 用 gettid；其余平台返回 -1，CPU 指标恒 0）。
#[cfg(target_os = "linux")]
fn thread_tid() -> i32 {
    // SAFETY: SYS_gettid 无参，永远成功。
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

#[cfg(not(target_os = "linux"))]
fn thread_tid() -> i32 {
    -1
}

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
            // #102/#85/#122：sctp-proto data channel 分片重组深调用链在 2MB 默认栈下
            // 偶发 stack overflow（file-transfer/cancel e2e 中 Abort）。8MB 仍不够：
            // #122 MCP download_file（大文件回传）在 debug 构建下再次溢出导致 SFU
            // 整个分片崩溃、CLI viewer 拿不到 answer panic（exit 101）。与 CLI 主线程
            // 32MB（#122）一致，放大到 32MB 作为缓解。
            .stack_size(32 * 1024 * 1024)
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
    // 记录本分片线程 TID，供 manager 按线程采样 CPU（/proc/self/task/<tid>/stat）。
    shared.shard_tids[index].store(thread_tid(), Ordering::Relaxed);
    // 本分片内每个房间的客户端数（用于清理 room_registry）。
    let mut room_counts: HashMap<String, usize> = HashMap::new();

    let mut last_heartbeat = Instant::now();
    // Demuxer 快路径：同源包免线性扫描。
    let mut addr_cache = AddrCache::new(4096);
    let metrics = &shared.metrics[index];
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
                let t = poll_until_timeout(client, to_propagate, socket, tcp_streams, metrics);
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
                ShardCommand::AddClient {
                    rtc,
                    room,
                    role,
                    dc_ready,
                } => {
                    let mut client = Client::new(rtc, role, dc_ready);
                    client.recorder = shared.recorder.clone();
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
                    addr_cache.clear();
                    shared.register_client(&room, id.as_u64(), index);
                    shared.register_session(SessionInfo {
                        id: id.as_u64(),
                        room: room.clone(),
                        role,
                        shard: index,
                        joined_at: crate::util::unix_micros(),
                    });
                    info!("shard {index}: client {id:?} joined room {room}");
                }
                ShardCommand::Kick { client_id } => {
                    if let Some(client) = clients.iter_mut().find(|c| c.id.as_u64() == client_id) {
                        info!(
                            "shard {index}: kick client {client_id} (room {})",
                            client.room
                        );
                        client.rtc.disconnect();
                    }
                }
                ShardCommand::Cross(ev) => {
                    handle_cross_event(
                        index,
                        ev,
                        &mut clients,
                        &mut addr_cache,
                        &socket,
                        &shared,
                        &cross_tx,
                    );
                }
                ShardCommand::TcpPacket {
                    source,
                    proto,
                    data,
                } => {
                    if let Some(input) =
                        build_input(source, proto, socket.local_addr().unwrap(), &data)
                        && let Some(client) =
                            demux_client(&mut clients, &input, source, &mut addr_cache)
                    {
                        client.handle_input(input);
                    }
                }
            }
        }

        // 2. 清理断线客户端 + 房间计数
        let before = clients.len();
        let dead: Vec<(String, u64)> = clients
            .iter()
            .filter(|c| !c.rtc.is_alive())
            .map(|c| (c.room.clone(), c.id.as_u64()))
            .collect();
        clients.retain(|c| c.rtc.is_alive());
        if clients.len() != before {
            addr_cache.clear();
            for (room, id) in &dead {
                shared.unregister_client(room, *id, index);
                shared.unregister_session(*id, index);
                shared.release(room); // #180 配额计数释放
            }
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

        // 心跳：向 manager 汇报客户端数（负载路由用）+ 聚合媒体质量（#238）
        if last_heartbeat.elapsed() >= Duration::from_secs(5) {
            last_heartbeat = Instant::now();
            metrics.clients.store(clients.len(), Ordering::Relaxed);
            let n = clients.len().max(1) as u64;
            let (mut rtt_s, mut rtt_n, mut el_s, mut il_s, mut bw_s) =
                (0u64, 0u64, 0u64, 0u64, 0u64);
            for c in &clients {
                let q = c
                    .qos
                    .lock()
                    .unwrap_or_else(aerodesk_protocol::util::lock_recover);
                if let Some(r) = q.rtt {
                    rtt_s += r.as_nanos() as u64;
                    rtt_n += 1;
                }
                if let Some(l) = q.egress_loss {
                    el_s += (l * 1_000_000.0) as u64;
                }
                if let Some(l) = q.ingress_loss {
                    il_s += (l * 1_000_000.0) as u64;
                }
                bw_s += q.bwe_bps;
            }
            metrics.qos_clients.store(rtt_n as usize, Ordering::Relaxed);
            metrics
                .rtt_avg_ns
                .store(if rtt_n > 0 { rtt_s / rtt_n } else { 0 }, Ordering::Relaxed);
            metrics.egress_loss_ppm.store(el_s / n, Ordering::Relaxed);
            metrics.ingress_loss_ppm.store(il_s / n, Ordering::Relaxed);
            metrics.bwe_tx_bps.store(bw_s / n, Ordering::Relaxed);
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
            // 缓冲截断：>2000B 的 UDP 包静默截断后交给 str0m 解析失败会导致
            // 合法客户端被断连（伪造超大包即可踢人）。截断即丢弃并告警。
            if n == buf.len() {
                warn!(
                    "drop oversized udp packet (>= {} bytes) from {source}",
                    buf.len()
                );
                continue;
            }
            let data = buf[..n].to_vec();
            route_udp(
                index,
                source,
                data,
                &mut clients,
                &mut addr_cache,
                &shared,
                &cross_tx,
                metrics,
                &socket,
            );
        }

        // 5. 时间推进
        let now = Instant::now();
        for client in &mut clients {
            client.handle_input(Input::Timeout(now));
        }
    }
}

/// UDP 包路由：本地认领 → 路由表转投 → 广播。
#[allow(clippy::too_many_arguments)]
fn route_udp(
    index: usize,
    source: SocketAddr,
    data: Vec<u8>,
    clients: &mut [Client],
    addr_cache: &mut AddrCache,
    shared: &Shared,
    cross_tx: &[mpsc::Sender<ShardCommand>],
    metrics: &ShardMetrics,
    socket: &UdpSocket,
) {
    let proto = Protocol::Udp;

    metrics.rx_packets.fetch_add(1, Ordering::Relaxed);
    metrics
        .rx_bytes
        .fetch_add(data.len() as u64, Ordering::Relaxed);

    if let Some(input) = build_input(source, proto, socket.local_addr().unwrap(), &data)
        && let Some(client) = demux_client(clients, &input, source, addr_cache)
    {
        shared.register_route(proto, source, index);
        client.handle_input(input);
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
fn build_input(
    source: SocketAddr,
    proto: Protocol,
    destination: SocketAddr,
    data: &[u8],
) -> Option<Input<'_>> {
    let contents = data.try_into().ok()?;
    Some(Input::Receive(
        Instant::now(),
        Receive {
            proto,
            source,
            destination,
            contents,
        },
    ))
}

/// 处理跨分片事件。
fn handle_cross_event(
    index: usize,
    ev: CrossShardEvent,
    clients: &mut [Client],
    addr_cache: &mut AddrCache,
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
            let Some(input) = build_input(source, proto, socket.local_addr().unwrap(), &data)
            else {
                debug!("cross packet {}:{} unparseable", proto, source);
                return;
            };
            let Some(client) = demux_client(clients, &input, source, addr_cache) else {
                debug!(
                    "cross packet {}:{} no client accepts ({} clients)",
                    proto,
                    source,
                    clients.len()
                );
                return;
            };
            client.handle_input(input);
            shared.register_route(proto, source, index);
            let _ = socket;
            let _ = cross_tx;
        }
        CrossShardEvent::TrackOpen { room, origin, weak } => {
            let mut subscribed = false;
            for client in clients.iter_mut() {
                if client.room == room && client.id != origin {
                    client.handle_track_open(weak.clone());
                    subscribed = true;
                }
            }
            if subscribed {
                // 本分片有 viewer 接收该房间轨道 → 登记为订阅者，媒体才转投过来。
                shared.join_subscriber(&room, index);
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

    // 房间内其它分片（TrackOpen/ChannelData/KeyframeRequest 仍按房间广播，
    // 让新 viewer 能订阅/输入能回传/关键帧能请求）。
    let room_targets: Vec<usize> = shared
        .room_shards(&origin_room)
        .into_iter()
        .filter(|i| *i != index)
        .collect();
    // 订阅驱动：媒体只转发给有订阅者的分片（#132），避免全量广播。
    let media_targets: Vec<usize> = shared
        .subscriber_shards(&origin_room)
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
            for t in &room_targets {
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
            if media_targets.is_empty() {
                // 无订阅者分片：不跨分片转发媒体（本地扇出已完成）。
                return;
            }
            for t in &media_targets {
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
            for t in &room_targets {
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
            // #136：关键帧请求只发给发布者所在分片；同分片不跨发；
            // 注册表缺失（理论不应发生）时回退房间广播。
            let kfr_targets: Vec<usize> =
                match shared.client_shard(&origin_room, origin_id.as_u64()) {
                    Some(t) if t == index => Vec::new(),
                    Some(t) => vec![t],
                    None => room_targets.clone(),
                };
            for t in &kfr_targets {
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
    metrics: &ShardMetrics,
) -> Instant {
    loop {
        if !client.rtc.is_alive() {
            return Instant::now();
        }
        // #211：SCTP input 发送缓冲堆积监控（每 500ms 检查）。
        client.monitor_sctp_backlog(Instant::now());
        // #85 出站背压队列：每轮先尝试排空（对端缓冲恢复后继续转发）。
        client.drain_pending_out();
        let propagated = client.poll_output(socket, tcp_streams, metrics);
        if let Propagated::Timeout(t) = propagated {
            return t;
        }
        queue.push_back(propagated)
    }
}

// ---------- Client（从单线程版迁移，新增 room 字段） ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(u64);

impl ClientId {
    /// 原始 id（用于跨分片注册表键）。
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

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

/// 数据通道出站优先级（0 最高）。低延迟通道优先于大文件，避免
/// file 回传打满对端缓冲时挤掉 input/clipboard（#134）。
const CHANNEL_PRIORITY_LEVELS: usize = 5;

/// #467：signal_ready 宽限期（自 SFU 侧 offer/answer 通道打开起算）。
/// ready 正常在通道打开后 ~1 RTT 到达；超时视为就绪包异常丢失，放行协商
/// 退回旧行为（偶发），避免门控本身把客户端缺陷放大成永久黑屏。
const SIGNAL_READY_GRACE: Duration = Duration::from_secs(5);

/// #477：重协商 offer 发出后等待 answer 的超时。有损路径（TURN 中继）上
/// viewer 的大消息 answer（~9KB 多 SCTP chunk）可能永不到达——超时即丢弃
/// pending 并复位重协商，未应答的 add 不进会话状态、重建 offer 的 mid
/// 自洽，不会与旧 answer 冲突。
const PENDING_ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

fn channel_priority(label: &str) -> usize {
    match label {
        "input" => 0,     // 观看端输入：延迟最敏感
        "control" => 1,   // 选层/显示切换
        "clipboard" => 2, // 剪贴板
        "cursor" => 3,    // 远程光标
        _ => 4,           // file 及其它批量通道
    }
}

pub struct Client {
    pub id: ClientId,
    pub room: String,
    /// #12：加入时的授权角色（信令 JWT 校验后传入）。
    pub role: Role,
    /// 码率控制器：远端估计 → 目标码率 + simulcast 选层。
    pub bwe: BitrateController,
    pub rtc: Rtc,
    pub pending: Option<str0m::change::SdpPendingOffer>,
    pub channels: HashMap<String, ChannelId>,
    pub tracks_in: Vec<TrackInEntry>,
    pub tracks_out: Vec<TrackOut>,
    pub chosen_rid: Option<Rid>,
    /// #467：/start 声明该客户端会在 offer/answer 通道 DCEP 完成后发
    /// `{"type":"signal_ready"}`（旧客户端为 false，不门控，保持兼容）。
    signal_ready_expected: bool,
    /// #467：已收到客户端的 signal_ready（仅 `signal_ready_expected` 时参与门控）。
    signal_ready: bool,
    /// #477：当前 pending offer 的发出时刻（answer 超时判定用）。
    pending_since: Instant,
    /// #467：signal_ready 等待锚点。初始为客户端创建时刻；offer/answer 通道
    /// 在 SFU 侧打开时重置（ready 正常在其后 ~1 RTT 内到达）。超过
    /// [`SIGNAL_READY_GRACE`] 仍未收到（客户端就绪包发送失败被吞等异常）则
    /// 放行协商，退回旧行为——避免"声明了能力的客户端"因自身缺陷永久黑屏。
    signal_ready_wait_since: Instant,
    /// 可选录制器引用（录制开启时非空）。
    recorder: Option<Arc<Recorder>>,
    /// #238 媒体质量快照（RTT/丢包/BWE）。
    qos: std::sync::Mutex<ClientQos>,
    /// #85/#134 出站 data channel 背压队列：按优先级分桶（0=最高，见
    /// `channel_priority`），对端 SCTP 缓冲满时排队，下一轮重试，不再静默丢包。
    pending_channel_out: [VecDeque<(String, Vec<u8>, bool)>; CHANNEL_PRIORITY_LEVELS],
    /// 跨桶合计背压字节上限（超限丢弃并告警，防内存失控）。
    pending_channel_out_bytes: usize,
    /// #211：SCTP input 发送缓冲堆积监控（对端 ACK 延迟/CPU 饥饿时告警）。
    last_sctp_monitor: Instant,
    sctp_input_high_since: Option<Instant>,
    /// #267：媒体出站连续写失败计数——瞬时拥塞时丢包背压，超阈值才断连。
    write_failures: u32,
    /// #267：最近一次向发布端反馈的目标码率（bps），节流用。
    last_bwe_target: u64,
    last_bwe_at: Instant,
}

impl Client {
    fn new(rtc: Rtc, role: Role, signal_ready_expected: bool) -> Client {
        static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
        let next_id = ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        Client {
            id: ClientId(next_id),
            room: String::new(),
            role,
            bwe: BitrateController::default(),
            rtc,
            pending: None,
            channels: HashMap::new(),
            tracks_in: vec![],
            tracks_out: vec![],
            chosen_rid: None,
            signal_ready_expected,
            signal_ready: false,
            pending_since: Instant::now(),
            signal_ready_wait_since: Instant::now(),
            recorder: None,
            qos: std::sync::Mutex::new(ClientQos::default()),
            pending_channel_out: std::array::from_fn(|_| VecDeque::new()),
            pending_channel_out_bytes: 0,
            last_sctp_monitor: Instant::now(),
            sctp_input_high_since: None,
            write_failures: 0,
            last_bwe_target: 0,
            last_bwe_at: Instant::now(),
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
        metrics: &ShardMetrics,
    ) -> Propagated {
        if !self.rtc.is_alive() {
            return Propagated::Noop;
        }
        if self.negotiate_if_needed() {
            return Propagated::Noop;
        }
        match self.rtc.poll_output() {
            Ok(output) => self.handle_output(output, socket, tcp_streams, metrics),
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
        metrics: &ShardMetrics,
    ) -> Propagated {
        match output {
            Output::Transmit(transmit) => {
                debug!(
                    "TX {} bytes to {} type={:#04x}",
                    transmit.contents.len(),
                    transmit.destination,
                    transmit.contents.first().copied().unwrap_or(0)
                );
                metrics.tx_packets.fetch_add(1, Ordering::Relaxed);
                metrics
                    .tx_bytes
                    .fetch_add(transmit.contents.len() as u64, Ordering::Relaxed);
                match transmit.proto {
                    Protocol::Udp => {
                        // #553 验收发现：发送失败（如 SFU 绑 127.0.0.1 而候选为
                        // 网卡 IP → WSAENETUNREACH 10051）不该 panic 杀 shard 线程
                        // （房间全挂）——warn 跳过，等 ICE 重协商换可达候选。
                        if let Err(e) = socket.send_to(&transmit.contents, transmit.destination) {
                            warn!("发送 UDP 失败（跳过）: {e}");
                        }
                    }
                    Protocol::Tcp | Protocol::SslTcp => {
                        let mut streams = tcp_streams
                            .lock()
                            .unwrap_or_else(aerodesk_protocol::util::lock_recover);
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
                    // #467：offer/answer 通道在 SFU 侧打开时重置宽限锚点——
                    // ready 正常在此后 ~1 RTT 到达，从这一刻起算而非客户端创建
                    // 时刻，避免宽限期被 ICE/DTLS 建立时长挤占。
                    if label == "offer/answer" && self.signal_ready_expected && !self.signal_ready {
                        self.signal_ready_wait_since = Instant::now();
                    }
                    self.channels.insert(label, cid);
                    Propagated::Noop
                }
                Event::ChannelData(data) => self.handle_channel_data(data),
                Event::ChannelClose(cid) => {
                    self.channels.retain(|_, v| *v != cid);
                    Propagated::Noop
                }
                Event::EgressBitrateEstimate(v) => {
                    let estimate = match v {
                        BweKind::Twcc(b) => b,
                        BweKind::Remb(_, b) => b,
                        _ => return Propagated::Noop,
                    };
                    self.bwe.update_estimate(estimate);
                    let target = self.bwe.target();
                    self.rtc.bwe().set_current_bitrate(target);
                    // #238：BWE 目标码率进质量快照。
                    self.qos
                        .lock()
                        .unwrap_or_else(aerodesk_protocol::util::lock_recover)
                        .bwe_bps = target.as_f64() as u64;
                    trace!(
                        "client {} bwe estimate {estimate:?} target {target:?}",
                        *self.id
                    );
                    // #267：向房间内发布端反馈目标码率（control 通道 {"bitrate":N}），
                    // 发布端 Encoder::set_bitrate 降档，避免单层拥塞时出站队列放大断连。
                    // 节流：变化 >10% 且距上次 ≥1s 才发，防 BWE 抖动风暴。
                    let now = std::time::Instant::now();
                    let target_bps = target.as_f64() as u64;
                    let changed = self.last_bwe_target == 0
                        || target_bps.abs_diff(self.last_bwe_target) > target_bps / 10;
                    if changed && now.duration_since(self.last_bwe_at) >= Duration::from_secs(1) {
                        self.last_bwe_target = target_bps;
                        self.last_bwe_at = now;
                        let msg = serde_json::json!({ "bitrate": target_bps })
                            .to_string()
                            .into_bytes();
                        // ChannelId 构造私有：用本客户端 control 通道 id 作载体
                        // （handle_channel_data_out 按目标客户端 label 查自己的 id，
                        // 载体 id 不参与路由）。
                        if let Some(cid) = self.channels.get("control").copied() {
                            return Propagated::ChannelData(
                                self.id,
                                "control".into(),
                                str0m::channel::ChannelData {
                                    id: cid,
                                    binary: false,
                                    data: msg,
                                },
                            );
                        }
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
                    // #238：媒体质量快照（RTT/丢包），供 5s 心跳聚合到 metrics。
                    let mut q = self
                        .qos
                        .lock()
                        .unwrap_or_else(aerodesk_protocol::util::lock_recover);
                    q.rtt = data.rtt;
                    q.egress_loss = data.egress_loss_fraction;
                    q.ingress_loss = data.ingress_loss_fraction;
                    drop(q);
                    Propagated::Noop
                }
                _ => Propagated::Noop,
            },
        }
    }

    fn handle_media_added(&mut self, mid: Mid, kind: MediaKind) -> Propagated {
        // #477：viewer 禁止发布媒体（#12），其 Rtc 上出现的 m-line（初始 offer 的
        // recvonly、SFU 侧 add_media 后的本地事件）都不该成为入站轨。此前无差别
        // 生成 track_in 并 replay 给全房间——viewer 的 recvonly m-line 被当成
        // "viewer 发布的轨"，导致 publisher 与 viewer 自己各多一条幻影出站轨、
        // 被迫做无意义的重协商（每加入一个 viewer 一轮 6KB offer/answer 往返）。
        if self.role == Role::Viewer {
            return Propagated::Noop;
        }
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
        if let Some(rec) = &self.recorder {
            // #234：ADREC2 携带 codec/RTP 时间戳/keyframe，供 rec2mp4 转封装。
            rec.record(
                &self.room,
                data.params.spec().codec,
                Some(data.time.numer() as u64),
                data.is_keyframe(),
                &data.data,
            );
        }
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
        if self.pending.is_some() {
            // #477：answer 丢失容忍——pending 超时（有损路径上 answer 可能永不到达）
            // 丢弃并复位，继续走本轮重建全新 offer；未应答的 add 不进会话状态，
            // 重建 offer 的 mid 自洽。旧 answer 迟到会因 mid 不在 offer 中被
            // handle_answer 丢弃（不再 panic）。
            if self.pending_since.elapsed() > PENDING_ANSWER_TIMEOUT {
                let _ = self.pending.take();
                self.reset_negotiating();
                warn!("Client ({}) 重协商 answer 超时未达，复位重试", *self.id);
            } else {
                return false;
            }
        }
        // #467：声明了 dc_ready 的客户端，必须等它 offer/answer 通道 DCEP 完成后
        // 发来的 signal_ready 才发起重协商——此前 viewer 端 str0m 可能尚未注册
        // 通道，写出的 offer 被丢弃 → viewer 不回 answer → pending 永久卡死。
        // 宽限兜底：超时仍未收到 ready（就绪包异常丢失/客户端缺陷）时放行，
        // 退回旧的偶发行为，避免永久黑屏；正常路径 ready ~1 RTT 内必达不受影响。
        if self.signal_ready_expected
            && !self.signal_ready
            && self.signal_ready_wait_since.elapsed() < SIGNAL_READY_GRACE
        {
            return false;
        }
        // #467：offer 固定写 offer/answer 通道（按 label 取）。此前取"第一个打开
        // 的通道"，跨 stream 无顺序保证，首个 ChannelOpen 可能是 input 等业务
        // 通道，offer 写错通道后被 viewer 当业务数据丢弃。
        let Some(&signal_cid) = self.channels.get("offer/answer") else {
            return false;
        };
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
        let Some(mut channel) = self.rtc.channel(signal_cid) else {
            return false;
        };
        let json = serde_json::to_string(&offer).unwrap();
        channel
            .write(false, json.as_bytes())
            .expect("to write offer");
        self.pending = Some(pending);
        self.pending_since = Instant::now();
        true
    }

    /// #477：协商中断后复位出站轨状态，待下一轮 negotiate_if_needed 重建。
    fn reset_negotiating(&mut self) {
        for track in &mut self.tracks_out {
            match track.state {
                TrackOutState::Negotiating(_) => track.state = TrackOutState::ToOpen,
                TrackOutState::NegotiatingStop(m) => track.state = TrackOutState::ToStop(m),
                _ => {}
            }
        }
    }

    fn handle_channel_data(&mut self, d: ChannelData) -> Propagated {
        use str0m::change::{SdpAnswer, SdpOffer};
        if let Ok(offer) = serde_json::from_slice::<'_, SdpOffer>(&d.data) {
            self.handle_offer(offer, d.id);
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
            // #192：无标签通道（如对端 negotiated 通道未发 DCEP，或通道未注册）——
            // SFU 无法按 label 转发，静默丢弃会掩盖问题，记一条告警便于排查。
            warn!(
                "Client ({}) 收到未注册 data channel cid={:?}（可能是 negotiated 通道未配置）；丢弃",
                *self.id, d.id
            );
            return Propagated::Noop;
        };
        if label == "offer/answer" {
            // #467：客户端就绪声明——offer/answer 通道 DCEP 已完成（opener 侧收到
            // ACK 后才会发），此后 SFU 发出的重协商 offer 不会再落在未注册通道上。
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&d.data)
                && v.get("type").and_then(|t| t.as_str()) == Some("signal_ready")
            {
                if !self.signal_ready {
                    info!(
                        "Client ({}) signal_ready：offer/answer 通道就绪，允许重协商",
                        *self.id
                    );
                }
                self.signal_ready = true;
                return Propagated::Noop;
            }
            warn!("Unrecognized data on signal channel");
            return Propagated::Noop;
        }
        // #29 画质/显示切换：control 通道的选层请求（观看端 → SFU），不转发；
        // #58 显示器切换请求需要转发到 publisher（同房间其它客户端）。
        if label == "control" {
            // 按顶层字段分发：LayerRequest 的 layer 是 Option，若直接反序列化
            // 会把 {"display":N} 也解析成 layer=None（#58 排查）。
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&d.data) {
                if let Some(layer) = v.get("layer").and_then(|l| l.as_str()) {
                    let req = LayerRequest {
                        layer: Some(layer.to_string()),
                    };
                    info!("Client ({}) layer request: {:?}", *self.id, req.layer());
                    self.bwe.set_requested_layer(req.layer());
                    return Propagated::Noop;
                }
                if let Some(disp) = v.get("display").and_then(|d| d.as_u64()) {
                    info!("Client ({}) display request: {disp}", *self.id);
                    return Propagated::ChannelData(self.id, "control".into(), d);
                }
                // #267：其它 control 消息（如发布端码率反馈 {"bitrate":N}）透传给房间，
                // 由 peer（publisher）自行处理；层选请求仍由 SFU 消费不转发。
                return Propagated::ChannelData(self.id, "control".into(), d);
            }
        }
        Propagated::ChannelData(self.id, label, d)
    }

    fn handle_channel_data_out(&mut self, label: &str, data: &ChannelData) {
        let Some(cid) = self.channels.get(label).copied() else {
            // #208：目标通道未注册（ChannelOpen 未到/未开）——入背压队列待通道
            // 打开后重试，不再丢弃（队列 64MB 有界，客户端断开随 Client 释放）。
            debug!(
                "Client ({}) 转发 {label}：通道未注册，入背压队列 {} 字节",
                *self.id,
                data.data.len()
            );
            self.enqueue_pending(label, data.data.clone(), data.binary);
            return;
        };
        let Some(mut channel) = self.rtc.channel(cid) else {
            // #208：label 已注册但 rtc 通道暂不可用——同样保留重试。
            debug!(
                "Client ({}) 转发 {label}：rtc 通道暂不可用，入背压队列 {} 字节",
                *self.id,
                data.data.len()
            );
            self.enqueue_pending(label, data.data.clone(), data.binary);
            return;
        };
        // 先尝试直接写；缓冲满则入背压队列（下一轮重试），不丢包。
        // 注意：str0m write 返回 Result<bool,_>——Ok(false) 表示 SCTP 发送缓冲
        // 满、未写入；用 is_ok() 会把 Ok(false) 当成功导致静默丢包（#211 实测：
        // 高负载软编 publisher 下 write 全 Ok 但送达仅 ~25%）。
        if channel.write(data.binary, &data.data).is_ok_and(|v| v) {
            return;
        }
        self.enqueue_pending(label, data.data.clone(), data.binary);
    }

    /// 入队到优先级分桶（#134）。总量超 64MB 时丢弃并告警（防内存失控）。
    fn enqueue_pending(&mut self, label: &str, data: Vec<u8>, binary: bool) -> bool {
        const CAP: usize = 64 << 20; // 64MB
        if self.pending_channel_out_bytes + data.len() > CAP {
            warn!(
                "Client ({}) outbound channel queue overflow (label={label}), dropping {} bytes",
                *self.id,
                data.len()
            );
            return false;
        }
        self.pending_channel_out_bytes += data.len();
        self.pending_channel_out[channel_priority(label)].push_back((
            label.to_string(),
            data,
            binary,
        ));
        true
    }

    /// 按优先级（0→4）逐桶重试出站背压队列：每桶内保持顺序；
    /// 某桶对端缓冲满只跳过该桶，不阻塞更高/更低优先级通道（#134）。
    /// #211：SCTP input 发送缓冲堆积监控。`channel.write` Ok 但数据长期滞留
    /// str0m SCTP 发送缓冲（对端 ACK 延迟/CPU 饥饿）→ 告警便于定位。
    fn monitor_sctp_backlog(&mut self, now: Instant) {
        if now.duration_since(self.last_sctp_monitor) < Duration::from_millis(500) {
            return;
        }
        self.last_sctp_monitor = now;
        const HIGH_WATER: usize = 128 << 10;
        let Some(cid) = self.channels.get("input").copied() else {
            self.sctp_input_high_since = None;
            return;
        };
        let backlog = self
            .rtc
            .channel(cid)
            .map(|mut c| c.buffered_amount())
            .unwrap_or(0);
        if backlog > HIGH_WATER {
            let since = *self.sctp_input_high_since.get_or_insert(now);
            if now.duration_since(since) >= Duration::from_secs(2) {
                warn!(
                    "Client ({}) SCTP input 发送缓冲堆积 {backlog} 字节 ≥2s（对端 ACK 延迟/CPU 饥饿，#211）",
                    *self.id
                );
                self.sctp_input_high_since = None; // 持续积压会再次触发
            }
        } else {
            self.sctp_input_high_since = None;
        }
    }

    fn drain_pending_out(&mut self) {
        for queue in self.pending_channel_out.iter_mut() {
            while let Some((label, data, binary)) = queue.front() {
                let Some(cid) = self.channels.get(label).copied() else {
                    // #208：目标 label 未注册时保留队列（待通道打开后重试），不再丢弃。
                    warn!(
                        "Client ({}) 背压队列 {label} 通道未注册，暂留 {} 字节",
                        *self.id,
                        data.len()
                    );
                    break;
                };
                let Some(mut channel) = self.rtc.channel(cid) else {
                    // #208：通道尚不可用 → 保持队列顺序，下一轮重试（不丢弃）。
                    break;
                };
                if !channel.write(*binary, data).is_ok_and(|v| v) {
                    break; // 该桶对端缓冲满/未写入（Ok(false)）：保持顺序，下一轮重试
                }
                let (_, data, _) = queue.pop_front().unwrap();
                self.pending_channel_out_bytes =
                    self.pending_channel_out_bytes.saturating_sub(data.len());
            }
        }
    }

    /// 处理客户端经 offer/answer 通道发来的重协商 offer。
    /// #467：answer 回写到 offer 到达的那条通道（`reply_cid`）——此前固定写
    /// "第一个打开的通道"，跨 stream 无顺序保证，可能写错通道被对端当业务数据丢弃。
    fn handle_offer(&mut self, offer: str0m::change::SdpOffer, reply_cid: ChannelId) {
        // #12：viewer 禁止通过重协商发布媒体（初始 offer 在 /start 处同样校验）。
        if self.role == Role::Viewer
            && aerodesk_protocol::util::offer_sends_media(&offer.to_sdp_string())
        {
            warn!(
                "Client ({}) role=viewer 尝试发布媒体，按 #12 断开",
                *self.id
            );
            self.rtc.disconnect();
            return;
        }
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
        let mut channel = self.rtc.channel(reply_cid).expect("channel to be open");
        let json = serde_json::to_string(&answer).unwrap();
        channel
            .write(false, json.as_bytes())
            .expect("to write answer");
    }

    fn handle_answer(&mut self, answer: str0m::change::SdpAnswer) {
        if let Some(pending) = self.pending.take() {
            // #477：过期/不匹配的 answer（如超时重试后旧 answer 迟到，mid 不在
            // 当前 offer 中）不再 panic——那会杀死整个分片线程。丢弃并复位，
            // 下一轮 negotiate 重建。
            if let Err(e) = self.rtc.sdp_api().accept_answer(pending, answer) {
                warn!(
                    "Client ({}) answer 与当前 offer 不匹配（丢弃，复位重协商）：{e:?}",
                    *self.id
                );
                self.reset_negotiating();
                return;
            }
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
        // simulcast/SVC 选层：按控制器目标码率选择 rid（q/h/f）。
        match (&data.rid, self.bwe.selected_layer()) {
            (None, _) => {}
            (Some(r), Layer::Low) if *r == "q".into() => {}
            (Some(r), Layer::Medium) if *r == "h".into() => {}
            (Some(r), Layer::High) if *r == "f".into() => {}
            _ => return,
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
        match writer.write(pt, data.network_time, data.time, data.data.clone()) {
            Ok(()) => {
                // 写成功：重置失败计数（瞬时拥塞恢复后不再累积）。
                self.write_failures = 0;
            }
            Err(e) => {
                // #267：瞬时拥塞不直接断连——丢包背压（实时媒体丢帧优于踢会话）；
                // 连续失败超阈值（~3s @30fps）才判定会话真坏断开。
                self.write_failures += 1;
                if self.write_failures >= 100 {
                    warn!(
                        "Client ({}) {} consecutive write failures, disconnecting",
                        *self.id, self.write_failures
                    );
                    self.rtc.disconnect();
                } else if self.write_failures == 1 || self.write_failures % 20 == 0 {
                    warn!(
                        "Client ({}) write backpressure (failures={}): {e:?}",
                        *self.id, self.write_failures
                    );
                }
            }
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

// ---------- #29 选层控制 ----------

/// 观看端经 control 通道发送的选层请求。
#[derive(Debug, serde::Deserialize)]
struct LayerRequest {
    /// "q" | "h" | "f"；None/缺省 = 回到 BWE 自动。
    layer: Option<String>,
}

impl LayerRequest {
    fn layer(&self) -> Option<Layer> {
        self.layer.as_deref().and_then(|s| match s {
            "q" => Some(Layer::Low),
            "h" => Some(Layer::Medium),
            "f" => Some(Layer::High),
            _ => None,
        })
    }
}

// ---------- #12 角色校验工具 ----------

/// 判断 offer SDP 是否包含发送方向（sendonly/sendrecv）的媒体 m-line。
///
/// `m=application`（数据通道）不算媒体。viewer 的 offer 不允许出现媒体发送方向。

/// 判断 offer 的全部 ICE 候选是否都是回环地址（127.0.0.0/8、::1）。
///
/// #216/#513：SFU 只在此时才往 answer 附带回环候选。同机客户端（桥、本机 CLI
/// 经 127.0.0.1 信令接入）的 offer 候选只有回环——socket 绑在 loopback，够不到
/// 公网通告地址；而远端客户端一旦在 answer 里看到回环候选，str0m 会把发送目的地
/// 漂移到它自己的回环，发布端媒体黑洞。无候选的 offer（纯 trickle）或含不可解析
/// 地址（mDNS .local）一律视为非回环：正常客户端 offer 都内联候选，缺省不附带
/// 是更安全的一侧。
pub(crate) fn offer_is_loopback_only(sdp: &str) -> bool {
    let mut seen = false;
    for line in sdp.lines() {
        let line = line.trim();
        if !line.starts_with("a=candidate:") {
            continue;
        }
        // a=candidate:<foundation> <component> <proto> <prio> <ip> <port> typ <type> ...
        let Some(ip_str) = line.split_whitespace().nth(4) else {
            continue;
        };
        seen = true;
        let loopback = ip_str
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
        if !loopback {
            return false;
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_cache_hit_miss_remove() {
        let mut c = AddrCache::new(4);
        let a1: SocketAddr = "1.2.3.4:5000".parse().unwrap();
        let a2: SocketAddr = "1.2.3.5:5000".parse().unwrap();
        assert_eq!(c.lookup(&a1), None);
        c.insert(a1, 2);
        c.insert(a2, 3);
        assert_eq!(c.lookup(&a1), Some(2));
        assert_eq!(c.lookup(&a2), Some(3));
        c.remove(&a1);
        assert_eq!(c.lookup(&a1), None);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn addr_cache_evicts_when_full() {
        let mut c = AddrCache::new(2);
        for i in 0..3u16 {
            let a: SocketAddr = format!("10.0.0.{i}:1000").parse().unwrap();
            c.insert(a, i as usize);
        }
        // 超过容量整体清空再登记最新，保证有界且可自愈
        assert!(c.len() <= 2, "cache must stay bounded");
        assert_eq!(c.lookup(&"10.0.0.2:1000".parse().unwrap()), Some(2));
    }

    fn sdp(m_lines: &str) -> String {
        format!("v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n{m_lines}")
    }

    #[test]
    fn subscribers_join_and_cleanup() {
        let shared = Shared::new(4);
        assert!(shared.subscriber_shards("r").is_empty());
        shared.join_subscriber("r", 2);
        shared.join_subscriber("r", 2); // 幂等
        assert_eq!(shared.subscriber_shards("r"), vec![2]);
        shared.join_room("r", 2);
        shared.leave_room("r", 2);
        assert!(
            shared.subscriber_shards("r").is_empty(),
            "leave_room must clean subscriber"
        );
    }

    #[test]
    fn media_targets_only_subscriber_shards() {
        let shared = Shared::new(4);
        shared.join_room("r", 1);
        shared.join_room("r", 2);
        shared.join_room("r", 3);
        shared.join_subscriber("r", 3); // 只有分片 3 有 viewer

        let index = 1usize;
        let mut room_targets: Vec<usize> = shared
            .room_shards("r")
            .into_iter()
            .filter(|i| *i != index)
            .collect();
        room_targets.sort_unstable();
        assert_eq!(
            room_targets,
            vec![2, 3],
            "track/channel still broadcast by room"
        );

        let mut media_targets: Vec<usize> = shared
            .subscriber_shards("r")
            .into_iter()
            .filter(|i| *i != index)
            .collect();
        media_targets.sort_unstable();
        assert_eq!(media_targets, vec![3], "media only to subscriber shard");

        // 订阅者离开后媒体不再跨分片
        shared.leave_room("r", 3);
        assert!(shared.subscriber_shards("r").is_empty());
    }

    #[test]
    fn channel_priority_ordering() {
        assert!(channel_priority("input") < channel_priority("control"));
        assert!(channel_priority("control") < channel_priority("clipboard"));
        assert!(channel_priority("clipboard") < channel_priority("cursor"));
        assert!(channel_priority("cursor") < channel_priority("file"));
        assert_eq!(channel_priority("unknown"), channel_priority("file"));
        assert!(channel_priority("input") < CHANNEL_PRIORITY_LEVELS);
    }

    #[test]
    fn pending_out_queues_by_priority_and_retains_when_unregistered() {
        use str0m::Rtc;
        let mut client = Client::new(Rtc::builder().build(Instant::now()), Role::Viewer, false);
        // 先入 file（低优先级），再入 input（高优先级）
        assert!(client.enqueue_pending("file", vec![0u8; 100], true));
        assert!(client.enqueue_pending("input", vec![1u8; 10], false));
        // 分桶正确：input 在 0 号桶、file 在 4 号桶
        assert_eq!(
            client.pending_channel_out[0]
                .front()
                .map(|(l, _, _)| l.as_str()),
            Some("input")
        );
        assert_eq!(
            client.pending_channel_out[4]
                .front()
                .map(|(l, _, _)| l.as_str()),
            Some("file")
        );
        assert_eq!(client.pending_channel_out_bytes, 110);
        // #208：无 channel 注册时 drain 保留队列（待通道打开后重试），不丢弃。
        client.drain_pending_out();
        assert_eq!(client.pending_channel_out_bytes, 110);
        assert!(
            client.pending_channel_out[0].front().is_some(),
            "input 桶应保留"
        );
        assert!(
            client.pending_channel_out[4].front().is_some(),
            "file 桶应保留"
        );
    }

    #[test]
    fn client_shards_register_lookup_cleanup() {
        let shared = Shared::new(4);
        assert_eq!(shared.client_shard("r", 7), None);
        shared.register_client("r", 7, 2);
        assert_eq!(shared.client_shard("r", 7), Some(2));
        shared.unregister_client("r", 7, 2);
        assert_eq!(shared.client_shard("r", 7), None);
        // 其它 shard 的值不被误删
        shared.register_client("r", 7, 2);
        shared.unregister_client("r", 7, 3);
        assert_eq!(shared.client_shard("r", 7), Some(2));
    }

    #[test]
    fn session_registry_register_snapshot_cleanup() {
        let shared = Shared::new(4);
        assert!(shared.session(7).is_none());
        assert!(shared.session_snapshot().is_empty());

        shared.register_session(SessionInfo {
            id: 7,
            room: "r".into(),
            role: Role::Publisher,
            shard: 2,
            joined_at: 1_000,
        });
        shared.register_session(SessionInfo {
            id: 8,
            room: "r".into(),
            role: Role::Viewer,
            shard: 2,
            joined_at: 2_000,
        });
        shared.register_session(SessionInfo {
            id: 9,
            room: "other".into(),
            role: Role::Viewer,
            shard: 3,
            joined_at: 3_000,
        });
        assert_eq!(shared.session(7).map(|s| s.shard), Some(2));
        assert_eq!(shared.session_snapshot().len(), 3);

        // 其它 shard 注销不误删
        shared.unregister_session(7, 3);
        assert!(shared.session(7).is_some(), "错误 shard 不得删除会话");
        shared.unregister_session(7, 2);
        assert!(shared.session(7).is_none(), "正确 shard 应删除会话");
        assert_eq!(shared.session_snapshot().len(), 2);
    }

    #[test]
    fn keyframe_targets_only_publisher_shard_with_fallback() {
        let shared = Shared::new(4);
        shared.join_room("r", 1);
        shared.join_room("r", 2);
        let index = 1usize;
        let room_targets: Vec<usize> = shared
            .room_shards("r")
            .into_iter()
            .filter(|i| *i != index)
            .collect();

        // 未登记：回退房间广播
        let targets: Vec<usize> = match shared.client_shard("r", 7) {
            Some(t) if t != index => vec![t],
            _ => room_targets.clone(),
        };
        assert_eq!(targets, vec![2]);

        // 登记发布者在分片 2：只定向到分片 2
        shared.register_client("r", 7, 2);
        let targets: Vec<usize> = match shared.client_shard("r", 7) {
            Some(t) if t != index => vec![t],
            _ => room_targets.clone(),
        };
        assert_eq!(targets, vec![2]);

        // 发布者在同一分片（index）：不跨分片
        shared.register_client("r", 8, index);
        let targets: Vec<usize> = match shared.client_shard("r", 8) {
            Some(t) if t == index => Vec::new(),
            Some(t) => vec![t],
            None => room_targets.clone(),
        };
        assert!(targets.is_empty(), "same-shard publisher must not cross");
    }

    #[test]
    fn sendonly_video_detected() {
        let s = sdp("m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=sendonly\r\n");
        assert!(aerodesk_protocol::util::offer_sends_media(&s));
    }

    #[test]
    fn sendrecv_video_detected() {
        let s = sdp("m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=sendrecv\r\n");
        assert!(aerodesk_protocol::util::offer_sends_media(&s));
    }

    #[test]
    fn recvonly_not_detected() {
        let s = sdp("m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=recvonly\r\n");
        assert!(!aerodesk_protocol::util::offer_sends_media(&s));
    }

    #[test]
    fn inactive_not_detected() {
        let s = sdp("m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=inactive\r\n");
        assert!(!aerodesk_protocol::util::offer_sends_media(&s));
    }

    #[test]
    fn data_channel_only_not_detected() {
        let s = sdp("m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n");
        assert!(!aerodesk_protocol::util::offer_sends_media(&s));
    }

    #[test]
    fn directionless_media_defaults_to_sendrecv() {
        // RFC 3264：无方向属性 → sendrecv，viewer 应被拒绝。
        let s = sdp("m=video 9 UDP/TLS/RTP/SAVPF 96\r\n");
        assert!(
            aerodesk_protocol::util::offer_sends_media(&s),
            "缺省方向媒体 m-line 应视为发送"
        );
    }

    #[test]
    fn directionless_media_then_recvonly_audio() {
        // 视频缺省方向（发送）+ 音频 recvonly：整体应判为发送。
        let s = sdp(
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\n",
        );
        assert!(aerodesk_protocol::util::offer_sends_media(&s));
    }

    #[test]
    fn recvonly_then_directionless_media() {
        // 音频 recvonly + 视频缺省方向：视频缺省 sendrecv → 发送。
        let s = sdp(
            "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n",
        );
        assert!(aerodesk_protocol::util::offer_sends_media(&s));
    }

    #[test]
    fn second_media_line_sending_detected() {
        // 第一条 recvonly（如观看端），第二条 sendonly → 判定为发送。
        let s = sdp(
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=recvonly\r\n             m=video 9 UDP/TLS/RTP/SAVPF 97\r\na=sendonly\r\n",
        );
        assert!(aerodesk_protocol::util::offer_sends_media(&s));
    }

    #[test]
    fn empty_sdp_not_detected() {
        assert!(!aerodesk_protocol::util::offer_sends_media(""));
    }

    // ---------- #513 回环候选按 offer 判定 ----------

    fn sdp_with_candidates(candidate_lines: &str) -> String {
        sdp(&format!(
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=recvonly\r\n{candidate_lines}"
        ))
    }

    #[test]
    fn loopback_only_offer_detected() {
        // 同机桥/本机 CLI：经 127.0.0.1 信令接入时 offer 候选只有回环。
        let s = sdp_with_candidates("a=candidate:1 1 udp 2130706431 127.0.0.1 54321 typ host\r\n");
        assert!(offer_is_loopback_only(&s));
    }

    #[test]
    fn loopback_only_offer_detected_v6() {
        let s = sdp_with_candidates("a=candidate:1 1 udp 2130706431 ::1 54321 typ host\r\n");
        assert!(offer_is_loopback_only(&s));
    }

    #[test]
    fn mixed_candidates_not_loopback_only() {
        // #513 黑洞场景：远端客户端即便附带 127.0.0.1，只要有非回环候选就不发回环候选。
        let s = sdp_with_candidates(
            "a=candidate:1 1 udp 2130706431 127.0.0.1 54321 typ host\r\n\
             a=candidate:2 1 udp 2130706431 192.168.1.5 54321 typ host\r\n",
        );
        assert!(!offer_is_loopback_only(&s));
    }

    #[test]
    fn lan_and_srflx_offer_not_loopback_only() {
        let s = sdp_with_candidates(
            "a=candidate:1 1 udp 2130706431 192.168.1.5 54321 typ host\r\n\
             a=candidate:2 1 udp 16909060 203.0.113.9 60000 typ srflx raddr 192.168.1.5 rport 54321\r\n",
        );
        assert!(!offer_is_loopback_only(&s));
    }

    #[test]
    fn mdns_offer_not_loopback_only() {
        // 浏览器 mDNS 候选不可解析为 IP：按非回环处理，不附带回环候选。
        let s = sdp_with_candidates(
            "a=candidate:1 1 udp 2130706431 9f3b2c1a-7d4e-4c5b-8a6f-0e1d2c3b4a59.local 54321 typ host\r\n",
        );
        assert!(!offer_is_loopback_only(&s));
    }

    #[test]
    fn no_candidate_offer_not_loopback_only() {
        assert!(!offer_is_loopback_only(&sdp_with_candidates("")));
        assert!(!offer_is_loopback_only(""));
    }

    #[test]
    fn try_reserve_release_room_and_total() {
        let shared = Shared::new(2);
        // 房间上限 1
        assert!(shared.try_reserve("r1", 1, 0).is_ok());
        assert_eq!(shared.try_reserve("r1", 1, 0), Err("room full"));
        // 不同房间不受影响
        assert!(shared.try_reserve("r2", 1, 0).is_ok());
        // 全局上限 2
        assert_eq!(shared.try_reserve("r3", 0, 2), Err("server full"));
        // 释放后可再进
        shared.release("r1");
        assert!(shared.try_reserve("r1", 1, 2).is_ok());
        shared.release("r1");
        shared.release("r2");
        assert_eq!(shared.total_clients.load(Ordering::Relaxed), 0);
    }
}

/// #208：目标通道未注册时，转发数据入背压队列保留（不丢弃）；通道打开后重试。
#[test]
fn pending_out_retains_when_channel_missing() {
    let now = std::time::Instant::now();
    let mut rtc = str0m::Rtc::new(now);
    let cid = rtc.sdp_api().add_channel("input".into());
    let mut client = Client::new(rtc, crate::shard::Role::Publisher, false);
    assert!(client.channels.is_empty());

    let data = ChannelData {
        id: cid,
        binary: false,
        data: b"input-event".to_vec(),
    };
    // 未注册 label：handle_channel_data_out 应入队而非丢弃
    client.handle_channel_data_out("input", &data);
    assert_eq!(
        client.pending_channel_out_bytes,
        data.data.len(),
        "未注册通道数据应入背压队列保留"
    );

    // 通道仍缺失：drain 应保留队列（不 pop 丢弃）
    client.drain_pending_out();
    assert_eq!(
        client.pending_channel_out_bytes,
        data.data.len(),
        "drain 不应丢弃未注册通道数据"
    );

    // 注册 label 但 rtc 通道不可用（未连接）：仍保留
    client.channels.insert("input".into(), data.id);
    client.drain_pending_out();
    assert_eq!(
        client.pending_channel_out_bytes,
        data.data.len(),
        "rtc 通道不可用时也应保留"
    );
}

// ---------- #467：signal_ready 门控回归（真实 str0m 对驱微缩 e2e） ----------

/// 伪 viewer：最小驱动（同 dc_multi_channel.rs 的 Node 思路），记录通道事件。
struct MiniViewer {
    rtc: Rtc,
    sock: UdpSocket,
    sfu_addr: SocketAddr,
    opened: Vec<(ChannelId, String)>,
    received: Vec<(String, Vec<u8>)>,
}

impl MiniViewer {
    fn channel_of(&self, label: &str) -> Option<ChannelId> {
        self.opened
            .iter()
            .find(|(_, l)| l == label)
            .map(|(c, _)| *c)
    }

    fn send(&mut self, label: &str, data: &[u8]) -> bool {
        let Some(cid) = self.channel_of(label) else {
            return false;
        };
        let Some(mut ch) = self.rtc.channel(cid) else {
            return false;
        };
        ch.write(false, data).unwrap_or(false)
    }

    fn pump(&mut self) {
        let now = Instant::now();
        let local = self.sock.local_addr().unwrap();
        let mut buf = [0u8; 2048];
        while let Ok((n, source)) = self.sock.recv_from(&mut buf) {
            if let Ok(contents) = buf[..n].try_into() {
                let _ = self.rtc.handle_input(Input::Receive(
                    now,
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: local,
                        contents,
                    },
                ));
            }
        }
        let _ = self.rtc.handle_input(Input::Timeout(now));
        while let Ok(o) = self.rtc.poll_output() {
            match o {
                Output::Transmit(t) => {
                    let _ = self.sock.send_to(&t.contents, self.sfu_addr);
                }
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
}

/// SFU Client 单连接泵：收包 → 超时 → 排空输出（内含 negotiate_if_needed 门控）。
fn pump_sfu_client(
    client: &mut Client,
    sock: &UdpSocket,
    tcp: &Arc<Mutex<HashMap<SocketAddr, TcpStream>>>,
    metrics: &ShardMetrics,
) {
    let now = Instant::now();
    let mut buf = [0u8; 2048];
    while let Ok((n, source)) = sock.recv_from(&mut buf) {
        if let Ok(contents) = buf[..n].try_into() {
            client.handle_input(Input::Receive(
                now,
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: sock.local_addr().unwrap(),
                    contents,
                },
            ));
        }
    }
    client.handle_input(Input::Timeout(now));
    loop {
        if !client.rtc.is_alive() {
            break;
        }
        if let Propagated::Timeout(_) = client.poll_output(sock, tcp, metrics) {
            break;
        }
    }
}

/// 建立真实连接：viewer 初始 offer（offer/answer + input 通道）→ SFU Client。
/// 返回 (viewer, client, sfu_sock, track)：track 是"publisher 已发布轨"的强引用，
/// 必须由调用方持有到测试结束——Client.tracks_out 只存 Weak（生产中由 publisher
/// 的 tracks_in 持有），提前 drop 会让 upgrade() 失败、add_media 不触发。
fn connect_mini_viewer(dc_ready: bool) -> (MiniViewer, Client, UdpSocket, Arc<TrackIn>) {
    use str0m::Candidate;
    let now = Instant::now();

    let mut vrtc = Rtc::new(now);
    let vsock = UdpSocket::bind("127.0.0.1:0").expect("viewer bind");
    vsock.set_read_timeout(Some(Duration::from_millis(10))).ok();
    let vaddr = vsock.local_addr().unwrap();
    vrtc.add_local_candidate(Candidate::host(vaddr, "udp").unwrap())
        .unwrap();

    let mut change = vrtc.sdp_api();
    change.add_channel("offer/answer".into());
    change.add_channel("input".into());
    let (offer, vpending) = change.apply().expect("viewer offer");

    let sfu_sock = UdpSocket::bind("127.0.0.1:0").expect("sfu bind");
    // 非阻塞泵必需：无包时 recv_from 最多等 10ms，否则测试线程永久挂起。
    sfu_sock
        .set_read_timeout(Some(Duration::from_millis(10)))
        .ok();
    let mut srtc = Rtc::new(now);
    srtc.add_local_candidate(Candidate::host(sfu_sock.local_addr().unwrap(), "udp").unwrap())
        .unwrap();
    let answer = srtc.sdp_api().accept_offer(offer).expect("accept");
    vrtc.sdp_api()
        .accept_answer(vpending, answer)
        .expect("answer");

    let mut client = Client::new(srtc, Role::Viewer, dc_ready);
    // 复现 publisher 先加入：viewer 加入时即 replay TrackOpen → tracks_out=ToOpen。
    let track = Arc::new(TrackIn {
        origin: ClientId(999),
        room: "r467".into(),
        mid: Mid::from("5"),
        kind: MediaKind::Video,
    });
    client.handle_track_open(Arc::downgrade(&track));

    let viewer = MiniViewer {
        rtc: vrtc,
        sock: vsock,
        sfu_addr: sfu_sock.local_addr().unwrap(),
        opened: Vec::new(),
        received: Vec::new(),
    };
    (viewer, client, sfu_sock, track)
}

fn mini_pump_until(
    viewer: &mut MiniViewer,
    client: &mut Client,
    sfu_sock: &UdpSocket,
    cond: impl Fn(&MiniViewer, &Client) -> bool,
    what: &str,
) {
    let tcp: Arc<Mutex<HashMap<SocketAddr, TcpStream>>> = Arc::new(Mutex::new(HashMap::new()));
    let metrics = ShardMetrics::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut iters = 0usize;
    while !cond(viewer, client) {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        viewer.pump();
        pump_sfu_client(client, sfu_sock, &tcp, &metrics);
        std::thread::sleep(Duration::from_millis(5));
        iters += 1;
        if iters % 100 == 0 {
            eprintln!(
                "[probe] {what}: iters={iters} ready={} expected={} pending={} channels={:?} viewer_opened={:?} viewer_recv={:?}",
                client.signal_ready,
                client.signal_ready_expected,
                client.pending.is_some(),
                client.channels.keys().collect::<Vec<_>>(),
                viewer.opened,
                viewer.received,
            );
        }
    }
}

/// #467 主回归：声明 dc_ready 的 viewer 在发 signal_ready 前，SFU 不发重协商
/// offer（pending 保持 None，viewer 收不到任何 offer）；发来 signal_ready 后
/// 立即协商，offer 恰好落在 offer/answer 通道上。
#[test]
fn signal_ready_gates_renegotiation_until_ready() {
    let (mut viewer, mut client, sfu_sock, _track) = connect_mini_viewer(true);
    mini_pump_until(
        &mut viewer,
        &mut client,
        &sfu_sock,
        |v, c| {
            v.rtc.is_connected()
                && c.rtc.is_connected()
                && v.channel_of("offer/answer").is_some()
                && c.channels.contains_key("offer/answer")
        },
        "连接与 DCEP 完成",
    );

    // 门控期：通道已双向就绪（viewer 已收 ACK）、tracks_out=ToOpen，但 ready 未到
    // → negotiate 不发生：pending 为 None，viewer 不应收到任何 offer。
    assert!(client.signal_ready_expected && !client.signal_ready);
    assert!(client.pending.is_none(), "ready 之前不得发起协商");
    assert!(
        !viewer
            .received
            .iter()
            .any(|(l, d)| l == "offer/answer" && is_sdp_offer_json(d)),
        "ready 之前 viewer 不应收到重协商 offer"
    );

    // viewer 声明就绪（endpoint.rs：ChannelOpen("offer/answer") 即 DCEP ACK 已收）。
    assert!(viewer.send("offer/answer", br#"{"type":"signal_ready"}"#));

    mini_pump_until(
        &mut viewer,
        &mut client,
        &sfu_sock,
        |_, c| c.pending.is_some(),
        "ready 后 SFU 发出重协商 offer",
    );
    // offer 必须落在 offer/answer 通道（而非"第一个打开的通道"——此处 input
    // 可能先开，正是原竞态的写错通道路径）。
    mini_pump_until(
        &mut viewer,
        &mut client,
        &sfu_sock,
        |v, _| {
            v.received
                .iter()
                .any(|(l, d)| l == "offer/answer" && is_sdp_offer_json(d))
        },
        "viewer 在 offer/answer 通道收到 offer",
    );
    assert!(!client.channels.is_empty());
}

/// 兼容回归：旧客户端（未声明 dc_ready）不发 signal_ready，SFU 仍照常协商，
/// offer 同样落在 offer/answer 通道。
#[test]
fn legacy_client_negotiates_without_signal_ready() {
    let (mut viewer, mut client, sfu_sock, _track) = connect_mini_viewer(false);
    mini_pump_until(
        &mut viewer,
        &mut client,
        &sfu_sock,
        |v, c| {
            v.rtc.is_connected()
                && c.rtc.is_connected()
                && v.channel_of("offer/answer").is_some()
                && c.pending.is_some()
                // 等到 offer 实际送达 viewer（pending 置位只代表 SFU 已写出）。
                && v.received
                    .iter()
                    .any(|(l, d)| l == "offer/answer" && is_sdp_offer_json(d))
        },
        "旧客户端不受门控影响，正常收到重协商 offer",
    );
}

/// #467 宽限兜底：声明了 dc_ready 但 ready 迟迟不到（就绪包异常丢失）时，
/// 超过 SIGNAL_READY_GRACE 放行协商——门控不得把客户端缺陷放大成永久黑屏。
#[test]
fn signal_ready_gate_falls_back_after_grace() {
    let (mut viewer, mut client, sfu_sock, _track) = connect_mini_viewer(true);
    // 连接与通道打开（正常阶段），但 viewer 故意不发 signal_ready。
    mini_pump_until(
        &mut viewer,
        &mut client,
        &sfu_sock,
        |v, c| {
            v.rtc.is_connected()
                && c.rtc.is_connected()
                && v.channel_of("offer/answer").is_some()
                && c.channels.contains_key("offer/answer")
        },
        "连接与 DCEP 完成",
    );
    assert!(client.pending.is_none(), "宽限期内不得发起协商");
    // 时间快进过宽限期（测试不真等 5s：直接回拨锚点）。
    client.signal_ready_wait_since -= SIGNAL_READY_GRACE + Duration::from_secs(1);
    mini_pump_until(
        &mut viewer,
        &mut client,
        &sfu_sock,
        |v, _| {
            v.received
                .iter()
                .any(|(l, d)| l == "offer/answer" && is_sdp_offer_json(d))
        },
        "宽限期超时后放行协商，offer 送达 viewer",
    );
}

/// 判断数据是否为 str0m SdpOffer JSON（{"type":"offer",...}）。
fn is_sdp_offer_json(d: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(d)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|s| s == "offer"))
        .unwrap_or(false)
}

/// #477 M1：viewer 的 MediaAdded 不生成 track_in（初始 offer 的 recvonly
/// m-line 与 SFU 侧 add_media 都是幻影源），publisher 的照常。
#[test]
fn viewer_media_added_is_not_track_in() {
    use str0m::media::Mid;
    let mut v = Client::new(Rtc::builder().build(Instant::now()), Role::Viewer, true);
    assert!(matches!(
        v.handle_media_added(Mid::from("0"), MediaKind::Video),
        Propagated::Noop
    ));
    assert!(v.tracks_in.is_empty(), "viewer 不应产生入站轨");

    let mut p = Client::new(Rtc::builder().build(Instant::now()), Role::Publisher, true);
    assert!(matches!(
        p.handle_media_added(Mid::from("0"), MediaKind::Video),
        Propagated::TrackOpen(..)
    ));
    assert_eq!(p.tracks_in.len(), 1);
}

/// #477 M3a：pending 超时（answer 丢失）后复位并重建 offer——viewer 应收到
/// 第二份重协商 offer，而不是 pending 永久卡死。
#[test]
fn pending_answer_timeout_renegotiates() {
    let (mut viewer, mut client, sfu_sock, _track) = connect_mini_viewer(true);
    mini_pump_until(
        &mut viewer,
        &mut client,
        &sfu_sock,
        |v, c| {
            v.rtc.is_connected()
                && c.rtc.is_connected()
                && v.channel_of("offer/answer").is_some()
                && c.channels.contains_key("offer/answer")
        },
        "连接与 DCEP 完成",
    );
    // 声明就绪（门控放行），第一份 offer 发出并送达。
    assert!(viewer.send("offer/answer", br#"{"type":"signal_ready"}"#));
    mini_pump_until(
        &mut viewer,
        &mut client,
        &sfu_sock,
        |v, c| {
            c.pending.is_some()
                && v.received
                    .iter()
                    .any(|(l, d)| l == "offer/answer" && is_sdp_offer_json(d))
        },
        "第一份 offer 发出并送达",
    );
    // 模拟 answer 丢失：把 pending 计时拨过超时，应复位并重建第二份 offer。
    client.pending_since -= PENDING_ANSWER_TIMEOUT + Duration::from_secs(1);
    mini_pump_until(
        &mut viewer,
        &mut client,
        &sfu_sock,
        |v, c| {
            c.pending.is_some()
                && v.received
                    .iter()
                    .filter(|(l, d)| l == "offer/answer" && is_sdp_offer_json(d))
                    .count()
                    >= 2
        },
        "超时后重建第二份 offer 并送达",
    );
}

/// #477 M3b：过期/不匹配的 answer（mid 不在当前 offer）不得 panic（旧实现
/// .expect 会杀死整个分片线程），应丢弃并复位待重协商。
#[test]
fn stale_answer_is_dropped_without_panic() {
    let (mut viewer, mut client, sfu_sock, _track) = connect_mini_viewer(true);
    mini_pump_until(
        &mut viewer,
        &mut client,
        &sfu_sock,
        |v, c| {
            v.rtc.is_connected()
                && c.rtc.is_connected()
                && v.channel_of("offer/answer").is_some()
                && c.channels.contains_key("offer/answer")
        },
        "连接与 DCEP 完成",
    );
    assert!(viewer.send("offer/answer", br#"{"type":"signal_ready"}"#));
    mini_pump_until(
        &mut viewer,
        &mut client,
        &sfu_sock,
        |_, c| c.pending.is_some(),
        "pending 就位",
    );
    // 构造可解析但 mid 不匹配的最小 answer。
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n\
               m=video 9 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP4 0.0.0.0\r\na=mid:zz\r\n\
               a=rtpmap:96 VP8/90000\r\na=inactive\r\n";
    let json = format!(r#"{{"type":"answer","sdp":{}}}"#, serde_json::json!(sdp));
    let answer: str0m::change::SdpAnswer =
        serde_json::from_str(&json).expect("最小 answer 应可解析");
    client.handle_answer(answer); // 不得 panic
    assert!(client.pending.is_none(), "不匹配 answer 应丢弃 pending");
    assert!(
        client
            .tracks_out
            .iter()
            .all(|t| matches!(t.state, TrackOutState::ToOpen)),
        " Negotiating 应复位为 ToOpen 待重协商"
    );
}
