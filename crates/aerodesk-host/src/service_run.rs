//! #470 服务运行体（`--service`，SYSTEM 进程内执行）：
//!   - M2：机器级配置 + `SignalPresence` 信令常驻（断线退避重连、30s 配置热重载）；
//!   - M3：WTS 会话让位状态机——`NoSession`（服务在线，登录界面）⇄
//!   `UserSession`（服务让位断开，spawn 桌面 UI）。
//! 设计见 docs/PRELOGIN_WINDOWS_SERVICE.md（D2/D3/D4）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aerodesk_core::protocol::signal::Role;
use aerodesk_core::signal_presence::{PresenceConfig, PresenceEvent, SignalPresence};
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
}

fn default_true() -> bool {
    true
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
    s.server = aerodesk_core::signaling::normalize_signal_url(&s.server);
    s.save()?;
    Ok(s)
}

/// 服务体入口：配置热重载 + 让位状态机 + presence 驱动。
/// 节拍 500ms（`wait_event` 兼任 sleep 与事件唤醒）。
pub fn service_body(ctx: ServiceCtx) {
    let mut sup = Supervisor::new();
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
    /// 是否存在用户会话（true = 让位态，服务 presence 停）。
    user_session: bool,
    presence: Option<SignalPresence>,
    last_status: String,
}

impl Supervisor {
    fn new() -> Self {
        let settings = ServiceSettings::load();
        // 启动时已有已登录会话（含锁屏/断开态——desktop 进程仍在、自带 #450
        // presence）：进入让位态但不 spawn（避免双实例）；仅服务运行期发生的
        // Logon 事件才 spawn。判据须用 logged_in_session：锁屏（Connected）
        // 不是"无会话"，用 active_session 会误判致双 presence。
        let user_session = session::logged_in_session().is_some();
        let mut sup = Supervisor {
            settings,
            user_session,
            presence: None,
            last_status: String::new(),
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
            sup.presence_start();
        }
        sup
    }

    fn presence_start(&mut self) {
        if !self.settings.usable() {
            warn!("服务配置不完整（server/device_id 为空），信令常驻未启动——待配置就位后热重载");
            return;
        }
        let mut config = PresenceConfig::new(
            self.settings.server.clone(),
            self.settings.device_id.clone(),
            Role::Publisher,
        )
        .with_auto_accept(false); // P0：登录前阶段不接听（呼叫超时自动挂断），#471 再接
        if !self.settings.token.is_empty() {
            config = config.with_auth_token(self.settings.token.clone());
        }
        let mut presence =
            SignalPresence::new(config).with_read_timeout(Duration::from_millis(500));
        let st = presence.start();
        info!("presence 启动：{}", st.as_str());
        self.last_status.clear();
        self.presence = Some(presence);
    }

    fn presence_stop(&mut self) {
        if let Some(mut p) = self.presence.take() {
            let st = p.stop();
            info!("presence 停止（让位）：{}", st.as_str());
        }
    }

    /// 配置热重载：NoSession 态重启 presence 以应用新配置。
    fn reload(&mut self, fresh: ServiceSettings) {
        self.settings = fresh;
        if self.user_session {
            return;
        }
        self.presence_stop();
        self.presence_start();
    }

    fn on_event(&mut self, ev: ServiceEvent) {
        let ServiceEvent::SessionChange { reason, session_id } = ev;
        match reason {
            // 用户登录：让位 + spawn 桌面 UI（M3 D3/D4）。
            SessionChangeReason::SessionLogon => {
                if !self.user_session {
                    self.user_session = true;
                    info!("WTS Logon（session {session_id}）：进入让位态");
                    self.presence_stop();
                    self.spawn_ui(session_id);
                }
            }
            // 注销/会话终止：回位，服务重新在线（登录界面可被呼叫）。
            SessionChangeReason::SessionLogoff | SessionChangeReason::SessionTerminate => {
                if self.user_session {
                    self.user_session = false;
                    info!("WTS Logoff/Terminate（session {session_id}）：回位 NoSession");
                    self.presence_start();
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

    /// 驱动 presence：状态变化与事件记日志；#471 M2 起 NoSession 态接听呼叫
    /// (媒体链路随帧源接入;P0 的"不接听"解除)。
    fn poll(&mut self) {
        let Some(presence) = self.presence.as_mut() else {
            return;
        };
        let st = presence.poll();
        if st.as_str() != self.last_status {
            info!("presence：{st:?}");
            self.last_status = st.as_str().to_string();
        }
        for ev in presence.take_events() {
            match ev {
                PresenceEvent::IncomingCall { call_id, from, .. } => {
                    info!("incoming call {call_id} from {from}——NoSession 态接听(登录界面媒体)");
                    if let Err(e) = presence.accept_call() {
                        warn!("接听失败：{e}");
                    }
                }
                other => info!("presence 事件：{other:?}"),
            }
        }
    }

    fn shutdown(&mut self) {
        self.presence_stop();
    }
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

/// #471 M1：登录界面 helper 主循环——回连服务（loopback TCP，零 FFI IPC）。
/// 协议（行式 UTF-8）：`hello <token>` 握手；服务发 `ping` 回 `pong`；
/// `shutdown` 退出。M2 起扩展帧下行/输入上行（长度前缀二进制帧）。
pub fn logon_helper_main(addr: &str, token: &str) -> Result<(), String> {
    use std::io::{BufRead, BufReader, Write};
    let stream =
        std::net::TcpStream::connect(addr).map_err(|e| format!("回连服务 {addr} 失败：{e}"))?;
    let mut writer = stream
        .try_clone()
        .map_err(|e| format!("clone 流失败：{e}"))?;
    writeln!(writer, "hello {token}").map_err(|e| format!("握手发送失败：{e}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("读服务消息失败：{e}"))?;
        if n == 0 {
            return Err("服务侧断开".into());
        }
        match line.trim() {
            "ping" => writeln!(writer, "pong").map_err(|e| format!("心跳回复失败：{e}"))?,
            "shutdown" => return Ok(()),
            other => info!("logon-helper 收到未知消息：{other}（M2 帧协议接入点）"),
        }
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
        logon_helper_main(&addr, "t0ken").expect("helper 应正常退出");
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
    }

    /// M2 联通验证：本地 signal server 在线时 presence 应 Online。
    /// 无本地 server（CI/普通环境）→ detect-and-return（stderr 打印后返回，
    /// 不 skip 凑绿，见 RULE_可达性）。
    #[test]
    fn presence_connects_when_signal_available() {
        let probe = std::net::TcpStream::connect(("127.0.0.1", 3003));
        if probe.is_err() {
            eprintln!(
                "presence_connects_when_signal_available：本地 3003 无 signal server，跳过执行"
            );
            return;
        }
        drop(probe);
        let config =
            PresenceConfig::new("ws://127.0.0.1:3003/ws", "svc-unit-test", Role::Publisher)
                .with_auto_accept(false);
        let mut presence =
            SignalPresence::new(config).with_read_timeout(Duration::from_millis(200));
        presence.start();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let st = presence.poll();
            if st.is_online() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "5s 内应 Online（本地 signal server 应答 Join），当前 {st:?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        presence.stop();
    }
}
