//! #470 服务运行体（`--service`，SYSTEM 进程内执行）：
//!   - M2：机器级配置 + `SignalPresence` 信令常驻（断线退避重连、30s 配置热重载）；
//!   - M3：WTS 会话让位状态机——`NoSession`（服务在线，登录界面）⇄
//!     `UserSession`（服务让位断开，spawn 桌面 UI）。
//! - #471 M2：登录界面媒体链路（headless 线程，合成源起步，实测矩阵后接
//!   S0 直抓/helper 抓帧源）。
//! 设计见 docs/PRELOGIN_WINDOWS_SERVICE.md（D2/D3/D4）与
//! docs/PRELOGIN_WINLOGON_CAPTURE.md（#471）。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aerodesk_core::endpoint::ClientEvent;
use aerodesk_core::p2p_call::{P2pCall, P2pCallConfig, P2pRole, offer_video_mid};
use aerodesk_core::platform::Codec;
use aerodesk_core::protocol::signal::Role;
use aerodesk_core::sip_link::{SipCallLink, SipLinkConfig, SipLinkEvent};
use aerodesk_platform::windows::service::{ServiceCtx, ServiceEvent, SessionChangeReason};
use aerodesk_platform::windows::session;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// 服务机器级配置（#470 D2）：SYSTEM 无用户 HOME，统一放 ProgramData。
/// 安装时从当前用户设置同步（`sync_settings_from_user`）；UI 侧改设置后
/// 30s 内热重载生效。
#[derive(Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ServiceSettings {
    /// 信令服务器 WS 地址（安装时经 `normalize_signal_url` 规范化）。
    pub server: String,
    /// 本机设备 ID（presence 房间名，与 UI 左栏 ID 一致）。
    pub device_id: String,
    /// 可选访问凭证 JWT。
    pub token: String,
    /// 用户登录事件后是否 spawn 桌面 UI（默认开）。
    #[serde(default = "default_true")]
    pub spawn_ui: bool,
    /// spawn 目标覆盖；空 = 服务 exe 同目录 `aerodesk-desktop.exe`。
    pub ui_exe: String,
    /// #471 M2：启动即发布登录界面媒体（e2e/联调模式；生产由呼叫接听触发）。
    #[serde(default)]
    pub auto_publish: bool,
    /// #471 M2：帧源（`synthetic`=合成源；`helper`=M3 实测 B 路径（helper
    /// 回连上行真采集帧）；`auto` 预留——实测矩阵 A/B 定稿后接 S0 直抓）。
    #[serde(default)]
    pub frame_source: String,
    /// #471 M3：helper 回连端口（0=临时分配——真机由服务拉起 helper 时用；
    /// 本地联调固定端口手动起 helper）。
    #[serde(default)]
    pub helper_port: u16,
    /// #552 SIP 迁移：传输（"udp"=内网/调试默认；"tls"=公网默认加密）。
    #[serde(default = "default_sip_transport")]
    pub sip_transport: String,
    /// #552 SIP 迁移：SIP 端口（0 = 按传输默认：udp 5060 / tls 5061）。
    #[serde(default)]
    pub sip_port: u16,
    /// #552 SIP 迁移：SIP 域（AoR 域；默认取产品默认域）。
    #[serde(default = "default_sip_domain")]
    pub sip_domain: String,
    /// #552 SIP 迁移：TLS CA PEM 文件路径（空 = 系统根证书包）。
    #[serde(default)]
    pub sip_ca_pem: String,
    /// #552 ICE：TURN 中继 URL（逗号分隔；空 = 直连）。SIP 路径无 join 下发
    /// TurnConfig 一环，须本地配置。
    #[serde(default)]
    pub turn_urls: String,
    /// #552 ICE：TURN 用户名。
    #[serde(default)]
    pub turn_username: String,
    /// #552 ICE：TURN 口令。
    #[serde(default)]
    pub turn_credential: String,
}

fn default_true() -> bool {
    true
}

fn default_sip_transport() -> String {
    "udp".into()
}

fn default_sip_domain() -> String {
    "aerodesk.test".into()
}

impl ServiceSettings {
    pub fn path() -> PathBuf {
        program_data()
            .join("AeroDesk")
            .join("service-settings.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        std::fs::create_dir_all(path.parent().unwrap_or(&path))
            .map_err(|e| format!("创建 {}: {e}", path.parent().unwrap_or(&path).display()))?;
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| format!("写 {}: {e}", path.display()))
    }

    /// 信令常驻的最小可用条件。
    fn usable(&self) -> bool {
        !self.server.is_empty() && !self.device_id.is_empty()
    }

    /// #552：SIP 链路配置（server URL 的 host + sip_port/sip_transport/
    /// sip_domain/sip_ca_pem → [`SipLinkConfig`]，统一经 core 构造）。
    fn sip_config(&self) -> Result<SipLinkConfig, String> {
        SipLinkConfig::from_parts(
            &self.server,
            &self.device_id,
            &self.token,
            &self.sip_transport,
            self.sip_port,
            &self.sip_domain,
            &self.sip_ca_pem,
        )
    }
}

fn program_data() -> PathBuf {
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into()))
}

/// 从当前用户设置（`~/.aerodesk-settings.json`）同步服务配置并落盘。
/// 在 `--install-service`（用户会话上下文）时调用；字段缺失保持空值，
/// 服务侧以 warn 提示（配置就位后热重载生效）。
pub fn sync_settings_from_user() -> Result<ServiceSettings, String> {
    let home = std::env::var("USERPROFILE")
        .map_err(|_| "USERPROFILE 未定义（须在用户会话内执行）".to_string())?;
    let user_settings =
        std::fs::read_to_string(PathBuf::from(home).join(".aerodesk-settings.json"))
            .map_err(|e| format!("读取用户设置失败（先运行一次桌面端生成）：{e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&user_settings).map_err(|e| format!("用户设置解析失败：{e}"))?;
    let get = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let mut s = ServiceSettings {
        server: get("server_default"),
        device_id: get("device_id"),
        token: get("token_default"),
        ..Default::default()
    };
    // #552 SIP 迁移：用户设置里已有的 SIP 字段才同步（缺失保持默认，
    // 避免服务侧已配置的字段被默认值覆盖）。
    if let Some(x) = v.get("sip_transport").and_then(|x| x.as_str()) {
        if !x.is_empty() {
            s.sip_transport = x.to_string();
        }
    }
    if let Some(x) = v.get("sip_port").and_then(|x| x.as_u64()) {
        s.sip_port = x as u16;
    }
    if let Some(x) = v.get("sip_domain").and_then(|x| x.as_str()) {
        if !x.is_empty() {
            s.sip_domain = x.to_string();
        }
    }
    if let Some(x) = v.get("sip_ca_pem").and_then(|x| x.as_str()) {
        if !x.is_empty() {
            s.sip_ca_pem = x.to_string();
        }
    }
    if let Some(x) = v.get("turn_urls").and_then(|x| x.as_str()) {
        if !x.is_empty() {
            s.turn_urls = x.to_string();
        }
    }
    if let Some(x) = v.get("turn_username").and_then(|x| x.as_str()) {
        if !x.is_empty() {
            s.turn_username = x.to_string();
        }
    }
    if let Some(x) = v.get("turn_credential").and_then(|x| x.as_str()) {
        if !x.is_empty() {
            s.turn_credential = x.to_string();
        }
    }
    s.server = aerodesk_core::signaling::normalize_signal_url(&s.server);
    s.save()?;
    Ok(s)
}

/// 服务体入口：配置热重载 + 让位状态机 + presence 驱动。
/// 节拍 500ms（`wait_event` 兼任 sleep 与事件唤醒）。
pub fn service_body(ctx: ServiceCtx) {
    service_body_with(ctx, false);
}

/// 同 [`service_body`]，`force_media`：让位态仍强制拉起登录界面媒体
/// （`--service-fg --force-media` 联调/e2e 专用，真机让位逻辑不受影响）。
pub fn service_body_with(ctx: ServiceCtx, force_media: bool) {
    let mut sup = Supervisor::new(force_media);
    let mut cfg_at = Instant::now();
    while !ctx.stopped() {
        if cfg_at.elapsed() >= Duration::from_secs(30) {
            cfg_at = Instant::now();
            let fresh = ServiceSettings::load();
            if fresh != sup.settings {
                info!("服务配置变更，重载（server/device_id/token）");
                sup.reload(fresh);
            }
        }
        sup.poll();
        if let Some(ev) = ctx.wait_event(Duration::from_millis(500)) {
            sup.on_event(ev);
        }
    }
    sup.shutdown();
    info!("service body 结束");
}

/// 让位状态机持有者（单线程服务体内使用）。
struct Supervisor {
    settings: ServiceSettings,
    /// 是否存在用户会话（true = 让位态，服务 SIP 链路停）。
    user_session: bool,
    /// #552：SIP 呼叫信令链路（替代 presence——规范 §8 语义事件收敛点）。
    link: Option<SipCallLink>,
    last_status: String,
    /// #471 M2：登录界面媒体线程（接听呼叫/auto_publish 启动）。
    media: Option<HeadlessMedia>,
}

impl Supervisor {
    fn new(force_media: bool) -> Self {
        let settings = ServiceSettings::load();
        // 启动时已有已登录会话（含锁屏/断开态——desktop 进程仍在、自带 #450
        // presence）：进入让位态但不 spawn（避免双实例）；仅服务运行期发生的
        // Logon 事件才 spawn。判据须用 logged_in_session：锁屏（Connected）
        // 不是"无会话"，用 active_session 会误判致双 presence。
        let user_session = session::logged_in_session().is_some();
        let mut sup = Supervisor {
            settings,
            user_session,
            link: None,
            last_status: String::new(),
            media: None,
        };
        info!(
            "服务启动：mode={} server={} device_id={}",
            if user_session {
                "UserSession(让位)"
            } else {
                "NoSession"
            },
            if sup.settings.server.is_empty() {
                "(未配置)"
            } else {
                &sup.settings.server
            },
            if sup.settings.device_id.is_empty() {
                "(未配置)"
            } else {
                &sup.settings.device_id
            },
        );
        if !user_session {
            sup.link_start();
            // #471 M2：e2e/联调模式——启动即发布（生产由呼叫接听触发）。
            if sup.settings.auto_publish {
                info!("auto_publish 开启：启动登录界面媒体（联调/e2e 模式）");
                sup.media = Some(media_start(&sup.settings));
            }
        } else if force_media {
            // --service-fg --force-media：本机已有登录会话（让位态）仍强制拉起
            // 登录界面媒体——本地/CI 联调 e2e 用（真机让位逻辑不受影响）。
            info!("FORCE_MEDIA：让位态强制启动登录界面媒体（联调）");
            sup.media = Some(media_start(&sup.settings));
        }
        sup
    }

    fn link_start(&mut self) {
        if !self.settings.usable() {
            warn!(
                "服务配置不完整（server/device_id 为空），SIP 信令常驻未启动——待配置就位后热重载"
            );
            return;
        }
        let cfg = match self.settings.sip_config() {
            Ok(c) => c,
            Err(e) => {
                warn!("SIP 配置解析失败，链路未启动：{e}");
                return;
            }
        };
        let mut link = SipCallLink::new(cfg);
        let st = link.start();
        info!("SIP 链路启动：{st:?}");
        self.last_status.clear();
        self.link = Some(link);
    }

    fn link_stop(&mut self) {
        if let Some(mut l) = self.link.take() {
            let st = l.stop();
            info!("SIP 链路停止（让位）：{st:?}");
        }
    }

    /// 配置热重载：NoSession 态重启 SIP 链路以应用新配置。
    fn reload(&mut self, fresh: ServiceSettings) {
        self.settings = fresh;
        if self.user_session {
            return;
        }
        self.link_stop();
        self.link_start();
    }

    fn on_event(&mut self, ev: ServiceEvent) {
        let ServiceEvent::SessionChange { reason, session_id } = ev;
        match reason {
            // 用户登录：让位 + spawn 桌面 UI（M3 D3/D4）。
            SessionChangeReason::SessionLogon => {
                if !self.user_session {
                    self.user_session = true;
                    info!("WTS Logon（session {session_id}）：进入让位态");
                    self.link_stop();
                    if let Some(mut m) = self.media.take() {
                        info!("WTS Logon：停登录界面媒体，切会话内采集");
                        m.stop_and_join();
                    }
                    self.spawn_ui(session_id);
                }
            }
            // 注销/会话终止：回位，服务重新在线（登录界面可被呼叫）。
            SessionChangeReason::SessionLogoff | SessionChangeReason::SessionTerminate => {
                if self.user_session {
                    self.user_session = false;
                    info!("WTS Logoff/Terminate（session {session_id}）：回位 NoSession");
                    self.link_start();
                }
            }
            // 锁屏/解锁等：P0 仅记录（#471 锁屏路由依据）。
            other => {
                info!("WTS 会话事件：{other:?}（session {session_id}）");
            }
        }
    }

    /// 在用户会话内拉起桌面端 exe（目标：配置覆盖或服务同目录 aerodesk-desktop.exe）。
    fn spawn_ui(&self, session_id: u32) {
        if !self.settings.spawn_ui {
            info!("spawn_ui 已关闭，跳过拉起桌面端");
            return;
        }
        let exe = if self.settings.ui_exe.is_empty() {
            match std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("aerodesk-desktop.exe")))
            {
                Some(p) => p.display().to_string(),
                None => {
                    warn!("无法定位服务 exe 目录，跳过 spawn_ui");
                    return;
                }
            }
        } else {
            self.settings.ui_exe.clone()
        };
        match session::spawn_in_session(&exe, session_id) {
            Ok(()) => info!("已在 session {session_id} 拉起桌面端：{exe}"),
            Err(e) => warn!("拉起桌面端失败（{exe}）：{e}"),
        }
    }

    /// 驱动 SIP 链路：状态变化与事件记日志；#471 M2 起 NoSession 态接听呼叫
    /// （媒体链路随帧源接入；P0 的"不接听"解除——现在直接 200 OK 接听）。
    fn poll(&mut self) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let st = link.poll();
        if st.as_str() != self.last_status {
            info!("SIP 链路：{st:?}");
            self.last_status = st.as_str().to_string();
        }
        let events = link.take_events();
        for ev in events {
            match ev {
                SipLinkEvent::IncomingCall {
                    call_id,
                    from_device,
                    offer_sdp,
                } => {
                    info!(
                        "incoming call {call_id} from {from_device}——NoSession 态接听（登录界面媒体）"
                    );
                    if self.media.is_none() {
                        if let Err(e) = self.start_p2p_media(&call_id, &offer_sdp) {
                            warn!("SIP 接听失败：{e}");
                        }
                    } else {
                        warn!("已有媒体会话，忽略新的 incoming call（{call_id}）");
                    }
                }
                SipLinkEvent::PeerHangup { call_id, reason } => {
                    info!("呼叫挂断（{call_id}，reason={reason:?}），停登录界面媒体");
                    self.stop_media();
                }
                SipLinkEvent::Rejected { call_id, .. } => {
                    info!("呼叫被拒（{call_id}），停登录界面媒体");
                    self.stop_media();
                }
                SipLinkEvent::Trickle { call_id, candidate } => {
                    if let Some(m) = self.media.as_ref()
                        && let Some(tx) = &m.trickle_tx
                    {
                        info!("trickle 候选注入（{call_id}）");
                        let _ = tx.send(candidate.candidate.clone());
                    }
                }
                _ => {}
            }
        }
    }

    /// 被叫媒体闭环：accept_offer → link.accept → 媒体线程（P2pCall 泵 +
    /// 帧源推送）。offer 无视频 mid 时拒绝接听（登录界面媒体只发视频）。
    fn start_p2p_media(&mut self, call_id: &str, offer_sdp: &str) -> Result<(), String> {
        let video_mid = offer_video_mid(offer_sdp)
            .ok_or_else(|| "offer 无视频 m-line（登录界面媒体只发视频）".to_string())?;
        let cfg = P2pCallConfig {
            role: P2pRole::Callee,
            device_role: Role::Publisher,
            codec: None,
            with_audio: false,
            with_camera: false,
            force_relay: false,
            bind: "0.0.0.0:0".parse().unwrap(),
            turn: aerodesk_core::turn_client::p2p_turn_transport(
                &self.settings.turn_urls,
                &self.settings.turn_username,
                &self.settings.turn_credential,
            ),
            inline_candidates: true,
        };
        let mut p2p = P2pCall::new(cfg).map_err(|e| format!("P2P 端点创建：{e}"))?;
        let answer = p2p
            .accept_offer(offer_sdp)
            .map_err(|e| format!("accept_offer：{e}"))?;
        self.link
            .as_mut()
            .ok_or("SIP 链路未启动")?
            .accept(call_id, &answer)
            .map_err(|e| format!("SIP accept：{e}"))?;
        self.media = Some(media_start_p2p(
            p2p,
            video_mid,
            self.settings.frame_source.clone(),
            self.settings.helper_port,
        ));
        Ok(())
    }

    fn stop_media(&mut self) {
        if let Some(mut m) = self.media.take() {
            m.stop_and_join();
        }
    }

    fn shutdown(&mut self) {
        self.stop_media();
        self.link_stop();
    }
}

// ---------- #471 M2：headless 登录界面媒体线程 ----------

/// 媒体线程句柄：独立线程全速驱动（33ms 帧节拍 + ICE/DTLS/RTP 收发），
/// 服务体只管启停（500ms 节拍太粗，不适合媒体循环）。
struct HeadlessMedia {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// #552：trickle 候选注入通道（P2P 会话；None = SFU 会话无注入）。
    trickle_tx: Option<std::sync::mpsc::Sender<String>>,
}

impl HeadlessMedia {
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 启动登录界面媒体线程（连接/编码失败在线程内记日志退出，不致命）。
fn media_start(settings: &ServiceSettings) -> HeadlessMedia {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let (server, room, token, frame_source, helper_port) = (
        settings.server.clone(),
        settings.device_id.clone(),
        settings.token.clone(),
        settings.frame_source.clone(),
        settings.helper_port,
    );
    let handle = std::thread::Builder::new()
        .name("logon-media".into())
        .spawn(move || run_media(server, room, token, frame_source, helper_port, stop2))
        .expect("spawn logon-media");
    HeadlessMedia {
        stop,
        handle: Some(handle),
        trickle_tx: None,
    }
}

/// 媒体主循环：帧源→编码→`send_video_frame`，同 cli publisher 驱动模式
/// （UDP 输入→timeout→poll_output→poll_event，1ms 粒度 sleep）。
fn run_media(
    server: String,
    room: String,
    token: String,
    frame_source: String,
    helper_port: u16,
    stop: Arc<AtomicBool>,
) {
    use str0m::net::{Protocol, Receive};
    use str0m::{Input, Output};
    let auth = if token.is_empty() {
        None
    } else {
        Some(token.as_str())
    };
    // host 无 cli 的 connect():走 core 泛型连接(desktop 发布端同款)。
    let live = match aerodesk_core::connect::connect_live_role_codec(
        &server,
        &room,
        Role::Publisher,
        auth,
        Some(Codec::H264),
    ) {
        Ok(l) => l,
        Err(e) => {
            warn!("登录界面媒体连接失败（server={server} room={room}）：{e}");
            return;
        }
    };
    let (mut endpoint, mut socket, video_mid, _audio_mid) = (
        live.endpoint,
        live.socket,
        live.video_mid.ok_or_else(|| {
            warn!("登录界面媒体连接失败：无视频 mid");
        }),
        live.audio_mid,
    );
    let Ok(video_mid) = video_mid else {
        return;
    };
    // #471 M3 帧源解析（见 frame_source_box 注）；SFU 与 P2P 媒体线程共用。
    let mut source = frame_source_box(&frame_source, helper_port);
    let mut encoder =
        match aerodesk_codec::encode::FfmpegEncoder::new(640, 360, 30, 1_500_000, Codec::H264) {
            Ok(e) => e,
            Err(e) => {
                warn!("编码器初始化失败（硬编探测+软编回退均败，S0 环境实属预期待实测项 C）：{e}");
                return;
            }
        };
    info!("登录界面媒体线程启动（合成源 640x360@30 H264，room={room}）");
    let mut pts: u64 = 0;
    let mut stat_at = Instant::now();
    let mut connected = false;
    let mut next_frame = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) || !endpoint.is_alive() {
            break;
        }
        // UDP 输入（STUN/DTLS/RTP）。
        socket.set_read_timeout(Some(Duration::from_millis(2))).ok();
        let mut buf = [0u8; 2000];
        if let Ok((n, src)) = socket.recv_from(&mut buf)
            && let Ok(contents) = buf[..n].try_into()
        {
            let _ = endpoint.handle_input(Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source: src,
                    destination: socket.local_addr().unwrap(),
                    contents,
                },
            ));
        }
        let _ = endpoint.handle_timeout(Instant::now());
        while let Some(out) = endpoint.poll_output() {
            match out {
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Timeout(_) => break,
                Output::Event(_) => {}
            }
        }
        while let Some(ev) = endpoint.poll_event() {
            if let ClientEvent::IceConnected = ev {
                connected = true;
                info!("登录界面媒体 ICE connected");
            }
        }
        // 帧发送（90kHz：30fps = 3000 ticks/帧）。
        if connected && Instant::now() >= next_frame {
            next_frame += Duration::from_millis(33);
            if let Some(f) = source.next_frame() {
                match encoder.encode_bgra(&f.bgra) {
                    Ok(Some(unit)) => {
                        // FFmpeg 编码输出直接进 RTP（annexb 转换是 macOS VT 路径专属）。
                        let rtp_time = str0m::media::MediaTime::new(
                            pts * 3000,
                            str0m::media::Frequency::NINETY_KHZ,
                        );
                        if let Err(e) = endpoint.send_video_frame(video_mid, unit.data, rtp_time) {
                            warn!("send frame：{e:?}");
                        }
                        pts += 1;
                    }
                    Ok(None) => {}
                    Err(e) => warn!("encode：{e}"),
                }
            }
        }
        if stat_at.elapsed() >= Duration::from_secs(5) {
            stat_at = Instant::now();
            info!("媒体已发 {pts} 帧");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    info!("登录界面媒体线程结束（共发 {pts} 帧）");
}

/// #471 M3 帧源解析（SFU/P2P 媒体线程共用）：
///   synthetic           — 合成源(测试/联调)
///   auto(默认/空)       — S0 DDA 直抓(实测矩阵 A);失败回退 helper(矩阵 B);再回退合成
///   helper              — 直接走 helper(服务经 winlogon token 拉起,真机登录界面路径)
fn frame_source_box(frame_source: &str, helper_port: u16) -> Box<dyn LogonFrameSource + Send> {
    match frame_source {
        "synthetic" => Box::new(SyntheticLogonSource::new(640, 360)),
        "helper" => match helper_frame_source_auto_spawn(helper_port) {
            Ok(s) => Box::new(s),
            Err(e) => {
                warn!("helper 帧源不可用，回退合成源：{e}");
                Box::new(SyntheticLogonSource::new(640, 360))
            }
        },
        _ => match DxgiFrameSource::new(640, 360) {
            Ok(s) => Box::new(s),
            Err(e) => {
                warn!("S0 DDA 直抓不可用（{e}），回退 helper 帧源");
                match helper_frame_source_auto_spawn(helper_port) {
                    Ok(s) => Box::new(s),
                    Err(e2) => {
                        warn!("helper 帧源也不可用（{e2}），回退合成源");
                        Box::new(SyntheticLogonSource::new(640, 360))
                    }
                }
            }
        },
    }
}

/// 启动 P2P 被叫媒体线程（#552：accept_offer 已由调用方完成，P2pCall 已就绪）。
fn media_start_p2p(
    p2p: P2pCall,
    video_mid: str0m::media::Mid,
    frame_source: String,
    helper_port: u16,
) -> HeadlessMedia {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let (trickle_tx, trickle_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::Builder::new()
        .name("logon-media-p2p".into())
        .spawn(move || run_media_p2p(p2p, video_mid, frame_source, helper_port, trickle_rx, stop2))
        .expect("spawn logon-media-p2p");
    HeadlessMedia {
        stop,
        handle: Some(handle),
        trickle_tx: Some(trickle_tx),
    }
}

/// P2P 媒体主循环（#552 被叫侧）：与 SFU 路径同泵结构，区别在（1）无
/// connect 步骤——P2pCall 已建；（2）就绪门槛是首个 `ChannelOpen`（DTLS 完成
/// ⟺ SRTP 密钥就绪；ICE connected 早于会话就绪，早写媒体帧会被对端丢帧）。
fn run_media_p2p(
    mut p2p: P2pCall,
    video_mid: str0m::media::Mid,
    frame_source: String,
    helper_port: u16,
    trickle_rx: std::sync::mpsc::Receiver<String>,
    stop: Arc<AtomicBool>,
) {
    let mut source = frame_source_box(&frame_source, helper_port);
    let mut encoder =
        match aerodesk_codec::encode::FfmpegEncoder::new(640, 360, 30, 1_500_000, Codec::H264) {
            Ok(e) => e,
            Err(e) => {
                warn!("编码器初始化失败：{e}");
                return;
            }
        };
    info!("P2P 登录界面媒体线程启动（640x360@30 H264）");
    let mut pts: u64 = 0;
    let mut stat_at = Instant::now();
    let mut ready = false;
    let mut next_frame = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) || !p2p.is_alive() {
            break;
        }
        // trickle 候选注入（信令 INFO sdpfrag；无积压时一次空转）。
        while let Ok(cand) = trickle_rx.try_recv() {
            if let Err(e) = p2p.add_remote_candidate(&cand) {
                warn!("trickle 候选注入失败：{e}");
            }
        }
        let _ = p2p.poll();
        while let Some(ev) = p2p.poll_event() {
            if let ClientEvent::ChannelOpen(..) = ev {
                ready = true;
                info!("P2P 会话就绪（SCTP 打开 = DTLS 完成）");
            }
        }
        // 帧发送（90kHz：30fps = 3000 ticks/帧）。
        if ready && Instant::now() >= next_frame {
            next_frame += Duration::from_millis(33);
            if let Some(f) = source.next_frame() {
                match encoder.encode_bgra(&f.bgra) {
                    Ok(Some(unit)) => {
                        let rtp_time = str0m::media::MediaTime::new(
                            pts * 3000,
                            str0m::media::Frequency::NINETY_KHZ,
                        );
                        if let Err(e) = p2p
                            .endpoint()
                            .send_video_frame(video_mid, unit.data, rtp_time)
                        {
                            warn!("send frame：{e:?}");
                        }
                        pts += 1;
                    }
                    Ok(None) => {}
                    Err(e) => warn!("encode：{e}"),
                }
            }
        }
        if stat_at.elapsed() >= Duration::from_secs(5) {
            stat_at = Instant::now();
            info!("P2P 媒体已发 {pts} 帧");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    info!("P2P 登录界面媒体线程结束（共发 {pts} 帧）");
}

// ---------- #471 M2：登录界面帧源 ----------

/// 登录界面帧（BGRA，与 windows::CapturedFrame 同构；跨采集实现统一）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogonFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// #471 M2 帧源：实测矩阵 A（服务 S0 直抓）/B（helper 抓）二选一为默认，
/// 合成源供单测/e2e。适配器接线随切片三（编码发送）落地。
pub trait LogonFrameSource {
    /// 取下一帧；无帧（采集未就绪/连接断开）返回 `None`，调用方下轮再试。
    fn next_frame(&mut self) -> Option<LogonFrame>;
}

/// 合成帧源（测试/e2e）：每帧按计数渐变填充，无外部依赖。
pub struct SyntheticLogonSource {
    width: u32,
    height: u32,
    frame: u32,
}

impl SyntheticLogonSource {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            frame: 0,
        }
    }
}

impl LogonFrameSource for SyntheticLogonSource {
    fn next_frame(&mut self) -> Option<LogonFrame> {
        let base = (self.frame % 64) as u8;
        self.frame += 1;
        // 每像素 4 字节 BGRA，首像素埋帧计数便于 e2e 断言帧序。
        let px = [base, 0x80, 0x80, 0xff];
        let mut bgra = px.repeat((self.width * self.height) as usize);
        bgra[0] = (self.frame & 0xff) as u8;
        Some(LogonFrame {
            width: self.width,
            height: self.height,
            bgra,
        })
    }
}

/// helper→服务 帧协议（M1 行协议的二进制扩展）：
/// `frame\n` 行头 + LE u32 宽/高 + BGRA 裸载荷。
pub fn encode_helper_frame(f: &LogonFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + 8 + f.bgra.len());
    out.extend_from_slice(b"frame\n");
    out.extend_from_slice(&f.width.to_le_bytes());
    out.extend_from_slice(&f.height.to_le_bytes());
    out.extend_from_slice(&f.bgra);
    out
}

/// 从 `buf` 起始处解码一帧，返回 (帧, 消费字节数)；不足一帧返回 `None`。
pub fn decode_helper_frame(buf: &[u8]) -> Option<(LogonFrame, usize)> {
    let head = b"frame\n";
    if buf.len() < head.len() + 8 || &buf[..head.len()] != head {
        return None;
    }
    let width = u32::from_le_bytes(buf[6..10].try_into().ok()?);
    let height = u32::from_le_bytes(buf[10..14].try_into().ok()?);
    let len = (width as usize) * (height as usize) * 4;
    if buf.len() < 14 + len {
        return None;
    }
    Some((
        LogonFrame {
            width,
            height,
            bgra: buf[14..14 + len].to_vec(),
        },
        14 + len,
    ))
}

/// #471 M3:helper 帧源客户端——服务侧 listener,读 helper 上行的 TCP 帧流。
/// `next_frame` 非阻塞读 + 流式解码(半包缓存于 `buf`);无完整帧返回 `None`
/// 下轮再试,对端断开返回 `None`(由调用方决定重连/回退)。
struct HelperFrameSource {
    stream: std::net::TcpStream,
    buf: Vec<u8>,
}

impl HelperFrameSource {
    /// 绑定 helper 回连端口（`port`=0 临时分配），返回 (listener, 实际端口)。
    /// 与 [`Self::accept_on_listener`] 分离：服务需先知道端口才能拉起 helper。
    fn bind(port: u16) -> Result<(std::net::TcpListener, u16), String> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| format!("helper listener bind({port})：{e}"))?;
        let real = listener.local_addr().map_err(|e| e.to_string())?.port();
        Ok((listener, real))
    }

    /// 在已绑定 listener 上等 helper 回连(10s 超时),握手校验 hello 行。
    fn accept_on_listener(listener: std::net::TcpListener) -> Result<Self, String> {
        let real = listener.local_addr().map_err(|e| e.to_string())?.port();
        info!("helper 帧源：等待 helper 回连 127.0.0.1:{real}（10s 超时）");
        // #522 审查：旧实现 `accept()` 无限阻塞（注释声称 10s 超时但未实现）——
        // helper 未拉起/崩溃时 run_media 线程卡死、stop 标志不可见、服务挂死。
        // 非阻塞 + 10s 轮询超时，等待期间可观察 stop/expires。
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut stream, peer) = loop {
            if Instant::now() >= deadline {
                return Err("等待 helper 回连超时（10s）".into());
            }
            match listener.accept() {
                Ok(conn) => break conn,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("等待 helper 回连失败：{e}")),
            }
        };
        stream.set_nodelay(true).ok();
        // 握手：helper 先发 "hello <token>" 行（token 联调场景不校验内容）。
        // 逐字节读——BufReader 会把紧跟首行的帧字节吞进内部缓冲随 drop 丢失
        // （实测教训：helper 握手后 33ms 即发首帧）。
        use std::io::Read;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        loop {
            stream
                .read_exact(&mut b)
                .map_err(|e| format!("helper 握手读失败：{e}"))?;
            line.push(b[0]);
            if b[0] == b'\n' {
                break;
            }
        }
        let line = String::from_utf8_lossy(&line).to_string();
        if !line.trim().starts_with("hello") {
            return Err(format!("helper 握手异常：{line:?}（peer {peer}）"));
        }
        stream.set_read_timeout(Some(Duration::from_millis(1))).ok();
        info!("helper 帧源已连接（peer {peer}）");
        Ok(HelperFrameSource {
            stream,
            buf: Vec::new(),
        })
    }
}

impl LogonFrameSource for HelperFrameSource {
    fn next_frame(&mut self) -> Option<LogonFrame> {
        use std::io::Read;
        let mut chunk = [0u8; 65536];
        // 连续读到 WouldBlock/TimedOut:一帧 ~900KB 需多次 read,若每次 read 后
        // 空返回,凑帧期每轮都付 1ms-timeout 的 15.6ms 时钟粒度 → 实测 3.6fps;
        // 内核缓冲有数据时 read 立即返回,循环读可在单个等待周期内凑满整帧。
        loop {
            if let Some((frame, used)) = decode_helper_frame(&self.buf) {
                self.buf.drain(..used);
                return Some(frame);
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => return None, // helper 断开
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return None; // 暂无更多数据,未凑齐帧,下轮再试
                }
                Err(_) => return None,
            }
        }
    }
}

/// #471 M3 真机路径：服务绑端口 → 经 winlogon token 拉起 helper 回连 →
/// accept。SYSTEM 服务上下文专属（`--service-fg` 非服务态拉不起 helper，
/// 失败由调用方回退）。真机登录界面即实测矩阵 B。
fn helper_frame_source_auto_spawn(port: u16) -> Result<HelperFrameSource, String> {
    let (listener, real) = HelperFrameSource::bind(port)?;
    let exe = std::env::current_exe()
        .map_err(|e| format!("定位本 exe 失败：{e}"))?
        .display()
        .to_string();
    let real_s = real.to_string();
    let args = [
        "--logon-helper",
        "--port",
        &real_s,
        "--token",
        "svc",
        "--capture",
    ];
    session::spawn_logon_helper(&exe, &args)?;
    info!("已请求经 winlogon token 拉起登录界面 helper（port {real}）");
    HelperFrameSource::accept_on_listener(listener)
}

/// #471 实测矩阵 A：S0 服务进程直抓 DDA——登录界面画面走 GPU 输出，
/// 不依赖目标 desktop。真机矩阵 A 的载体（能否采到登录界面为实测项）。
struct DxgiFrameSource {
    capturer: aerodesk_platform::windows::capture::DxgiCapturer,
}

impl DxgiFrameSource {
    fn new(width: u32, height: u32) -> Result<Self, String> {
        aerodesk_platform::windows::capture::DxgiCapturer::new_with_scale(width, height)
            .map(|capturer| Self { capturer })
    }
}

impl LogonFrameSource for DxgiFrameSource {
    fn next_frame(&mut self) -> Option<LogonFrame> {
        self.capturer.capture_frame().map(|f| LogonFrame {
            width: f.width,
            height: f.height,
            bgra: f.bgra,
        })
    }
}

/// #471 M1：登录界面 helper 主循环——回连服务（loopback TCP，零 FFI IPC）。
/// 协议（行式 UTF-8）：`hello <token>` 握手；服务发 `ping` 回 `pong`；
/// `shutdown` 退出。M2 起扩展帧下行/输入上行（长度前缀二进制帧）。
/// `capture`：同时采集当前 desktop（DxgiCapturer 缩放到目标分辨率）按
/// 30fps 上行帧（M3 实测 B 路径——本机用户会话可联调,真机 winlogon 桌面
/// 由服务经 winlogon token 拉起本入口）。读侧保持非阻塞以应答 ping/shutdown。
pub fn logon_helper_main(
    addr: &str,
    token: &str,
    capture: bool,
    synthetic: bool,
) -> Result<(), String> {
    use std::io::{BufRead, BufReader, Write};
    let stream =
        std::net::TcpStream::connect(addr).map_err(|e| format!("回连服务 {addr} 失败：{e}"))?;
    stream
        .set_nodelay(true)
        .map_err(|e| format!("nodelay：{e}"))?;
    let mut writer = stream
        .try_clone()
        .map_err(|e| format!("clone 流失败：{e}"))?;
    writeln!(writer, "hello {token}").map_err(|e| format!("握手发送失败：{e}"))?;
    let mut reader = BufReader::new(stream);
    // 读超时 30ms 定步长:写侧保持阻塞(整流非阻塞会让 write 撞 10035,
    // 实测);有帧时 read 立即返回,空闲时循环按 30ms 步进,30fps 门限不受
    // Windows 1ms→15.6ms 时钟粒度拖累。
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(30)))
        .ok();
    // 采集器：失败降级为纯心跳 helper（联调环境无显示器/权限时仍可验证控制面）。
    // synthetic：联调/CI 模式——上行合成帧（确定性验证链路，不依赖桌面重绘）；
    // 真值（DDA 采 winlogon/用户桌面）在 VM/真机走 capture 分支。
    let mut synth = if synthetic {
        Some(SyntheticLogonSource::new(640, 360))
    } else {
        None
    };
    let mut capturer = if capture && synth.is_none() {
        match aerodesk_platform::windows::capture::DxgiCapturer::new_with_scale(640, 360) {
            Ok(c) => Some(c),
            Err(e) => {
                warn!("helper DDA 采集初始化失败，降级纯心跳：{e}");
                None
            }
        }
    } else {
        None
    };
    let mut next_frame = Instant::now();
    let mut sent: u64 = 0;
    let mut stat_at = Instant::now();
    let mut line = String::new();
    loop {
        // 控制面：非阻塞读一行（多数轮次 WouldBlock 直接走采集）。
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Err("服务侧断开".into()),
            Ok(_) => match line.trim() {
                "ping" => writeln!(writer, "pong").map_err(|e| format!("心跳回复失败：{e}"))?,
                "shutdown" => {
                    info!("helper 收到 shutdown，退出（已发 {sent} 帧）");
                    return Ok(());
                }
                other => info!("logon-helper 收到未知消息：{other}"),
            },
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("读服务消息失败：{e}")),
        }
        // 数据面：30fps 上行帧。
        if (capturer.is_some() || synth.is_some()) && Instant::now() >= next_frame {
            next_frame += Duration::from_millis(33);
            let frame = if let Some(s) = synth.as_mut() {
                s.next_frame()
            } else if let Some(f) = capturer.as_mut().and_then(|c| c.capture_frame()) {
                Some(LogonFrame {
                    width: f.width,
                    height: f.height,
                    bgra: f.bgra,
                })
            } else {
                None
            };
            if let Some(frame) = frame {
                if let Err(e) = writer.write_all(&encode_helper_frame(&frame)) {
                    return Err(format!("帧上行失败：{e}"));
                }
                sent += 1;
            }
        }
        if stat_at.elapsed() >= Duration::from_secs(5) {
            stat_at = Instant::now();
            info!("helper 采集上行：累计 {sent} 帧");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #471 M2：helper 帧协议 roundtrip + 半包容错（流式解码场景）。
    #[test]
    fn helper_frame_codec_roundtrip_and_partial() {
        let mut src = SyntheticLogonSource::new(8, 4);
        let frame = src.next_frame().expect("合成源应有帧");
        let wire = encode_helper_frame(&frame);
        let (back, used) = decode_helper_frame(&wire).expect("完整帧应可解码");
        assert_eq!(back, frame);
        assert_eq!(used, wire.len());
        // 半包：截断后不可解码,补齐后成功。
        assert!(decode_helper_frame(&wire[..wire.len() - 1]).is_none());
    }

    /// 合成源帧序埋点：首像素随帧计数变化（e2e 断言帧推进用）。
    #[test]
    fn synthetic_source_frame_counter() {
        let mut src = SyntheticLogonSource::new(4, 4);
        let f1 = src.next_frame().unwrap();
        let f2 = src.next_frame().unwrap();
        assert_ne!(f1.bgra[0], f2.bgra[0], "帧计数埋点应递增");
    }

    /// M1 联测：本地 listener 模拟服务侧,helper 握手/心跳/退出全链路。
    #[test]
    fn logon_helper_handshake_ping_shutdown() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let server = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            let (mut s, _) = listener.accept().expect("accept");
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            assert_eq!(line.trim(), "hello t0ken");
            writeln!(s, "ping").unwrap();
            r.read_line(&mut line).unwrap(); // 复用 line:读 pong(追加后 trim 校验)
            assert!(line.trim().ends_with("pong"));
            writeln!(s, "shutdown").unwrap();
        });
        logon_helper_main(&addr, "t0ken", false, false).expect("helper 应正常退出");
        server.join().expect("server 线程");
    }

    /// 服务配置序列化兼容：缺字段走 serde default（spawn_ui=true 等旧文件兼容）。
    #[test]
    fn settings_defaults_on_missing_fields() {
        let s: ServiceSettings =
            serde_json::from_str(r#"{"server":"ws://127.0.0.1:3003/ws"}"#).expect("缺字段应可解析");
        assert_eq!(s.server, "ws://127.0.0.1:3003/ws");
        assert_eq!(s.device_id, "");
        assert_eq!(s.token, "");
        assert!(s.spawn_ui, "spawn_ui 缺省应为 true");
        assert_eq!(s.ui_exe, "");
        assert_eq!(s.sip_transport, "udp", "sip_transport 缺省 udp");
        assert_eq!(s.sip_port, 0, "sip_port 缺省 0（=按传输默认）");
        assert_eq!(s.sip_domain, "aerodesk.test");
        assert_eq!(s.sip_ca_pem, "");
    }

    /// #552：SIP 配置映射（server URL host + 传输/端口/CA 默认值）。
    #[test]
    fn sip_config_maps_settings() {
        let mut s = ServiceSettings {
            server: "ws://127.0.0.1:3003/ws".into(),
            device_id: "AD-TEST".into(),
            token: "tok".into(),
            ..Default::default()
        };
        let cfg = s.sip_config().unwrap();
        assert_eq!(cfg.device_id, "AD-TEST");
        assert_eq!(cfg.domain, "aerodesk.test", "sip_domain 缺省");
        assert_eq!(cfg.password, "tok");
        assert_eq!(
            cfg.server,
            "127.0.0.1:5060".parse().unwrap(),
            "udp 默认 5060"
        );
        assert!(cfg.tls.is_none(), "udp 无 TLS 配置");
        // tls 传输：默认端口 5061 + 系统根 CA。
        s.sip_transport = "tls".into();
        let cfg2 = s.sip_config().unwrap();
        assert_eq!(cfg2.server, "127.0.0.1:5061".parse().unwrap());
        let tls = cfg2.tls.expect("tls 应带配置");
        assert!(!tls.ca_certs.is_empty(), "默认应带系统根 CA");
        assert_eq!(tls.sni_hostname.as_deref(), Some("127.0.0.1"));
        // 显式端口。
        s.sip_port = 5070;
        assert_eq!(
            s.sip_config().unwrap().server,
            "127.0.0.1:5070".parse().unwrap()
        );
        // 非法传输。
        s.sip_transport = "sctp".into();
        assert!(s.sip_config().is_err());
    }
}
