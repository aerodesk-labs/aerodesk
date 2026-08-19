//! loopback 传输层：Windows 命名管道 / Unix domain socket（docs/IPC_PROTOCOL.md §1）。
//!
//! 线程模型遵循 ADR-0008：[`Conn`] 的读端由单一 reader 泵线程阻塞 [`Conn::recv`]，
//! 写端经 [`ConnWriter`]（`Arc<Mutex>`）供任意事件源线程 [`ConnWriter::send`]，
//! 不引入 async 运行时。
//!
//! 安全边界（§5）：管道/套接字均为本机 loopback——`\\.\pipe\*` 天然不可经网络
//! 访问；生产部署的 SDDL/文件权限加固（限定 SYSTEM/Administrators + 登录会话）
//! 在 B3 集成切片落地，本层先保证路径约定与协议语义。

use std::io;
use std::sync::{Arc, Mutex};

use crate::frame::{read_frame, write_frame};
use crate::proto::{
    ClientKind, Envelope, Msg, PROTOCOL_VERSION, SessionSummary, negotiate_version, probe_envelope,
};

/// `name` → 平台传输路径。名称只允许 `[a-z0-9-]`（≤32），防路径注入。
pub fn validate_name(name: &str) -> io::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("illegal ipc name {name:?}: want [a-z0-9-]{{1,32}}"),
        ))
    }
}

/// `recv` 的错误分类：连接层据此决定「回 error 帧续传」还是「关连接」（§2）。
#[derive(Debug)]
pub enum RecvError {
    /// 对端干净关闭（帧边界处 EOF）。
    Closed,
    /// 传输层 I/O 错误（含半帧 EOF）——关连接。
    Io(io::Error),
    /// 帧损坏/JSON 畸形——关连接。
    Malformed(String),
    /// 未知 kind（前向兼容路径）——可回 `error{code:"unknown_kind"}`，不断连。
    UnknownKind { v: u32, kind: String },
    /// 主版本不受支持——回 `error{code:"version_unsupported"}` 后关连接。
    UnsupportedVersion(u32),
}

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "peer closed cleanly"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Malformed(m) => write!(f, "malformed frame: {m}"),
            Self::UnknownKind { v, kind } => write!(f, "unknown kind {kind:?} (v{v})"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
        }
    }
}
impl std::error::Error for RecvError {}

/// 握手失败原因。
#[derive(Debug)]
pub enum HandshakeError {
    Io(io::Error),
    Closed,
    /// 首帧不是预期的 hello/welcome。
    Protocol(String),
    /// 版本无交集（server 侧视角）。
    VersionUnsupported {
        min_v: u32,
        max_v: u32,
    },
    /// server 回绝（client 侧视角，携带 error 帧内容）。
    Rejected {
        code: String,
        message: String,
    },
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Closed => write!(f, "peer closed during handshake"),
            Self::Protocol(m) => write!(f, "protocol violation: {m}"),
            Self::VersionUnsupported { min_v, max_v } => {
                write!(f, "no common version with [{min_v}, {max_v}]")
            }
            Self::Rejected { code, message } => write!(f, "rejected: {code}: {message}"),
        }
    }
}
impl std::error::Error for HandshakeError {}

/// 一条已连接的 loopback 双工通道。读端独占（reader 泵线程），
/// 写端经 [`Conn::writer`] 克隆共享。
pub struct Conn {
    rd: Box<dyn io::Read + Send>,
    wr: Arc<Mutex<Box<dyn io::Write + Send>>>,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conn").finish_non_exhaustive()
    }
}

impl Conn {
    /// 阻塞读下一帧消息。
    pub fn recv(&mut self) -> Result<Msg, RecvError> {
        let bytes = match read_frame(&mut self.rd) {
            Ok(Some(b)) => b,
            Ok(None) => return Err(RecvError::Closed),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                return Err(RecvError::Malformed(e.to_string()));
            }
            Err(e) => return Err(RecvError::Io(e)),
        };
        match Envelope::from_json(&bytes) {
            Ok(env) if env.v > PROTOCOL_VERSION => Err(RecvError::UnsupportedVersion(env.v)),
            Ok(env) => Ok(env.msg),
            Err(_) => match probe_envelope(&bytes) {
                Ok((v, kind)) => Err(RecvError::UnknownKind { v, kind }),
                Err(e) => Err(RecvError::Malformed(e.to_string())),
            },
        }
    }

    /// 取一个可跨线程共享的写端。
    pub fn writer(&self) -> ConnWriter {
        ConnWriter {
            wr: self.wr.clone(),
        }
    }

    /// 客户端握手（§2/§3.1）：发 `hello`，等 `welcome` 返回现存会话清单。
    pub fn client_hello(
        &mut self,
        client: ClientKind,
        client_version: &str,
    ) -> Result<Vec<SessionSummary>, HandshakeError> {
        self.writer()
            .send(&Msg::Hello {
                client,
                client_version: client_version.to_string(),
                min_v: PROTOCOL_VERSION,
                max_v: PROTOCOL_VERSION,
            })
            .map_err(HandshakeError::Io)?;
        match self.recv() {
            Ok(Msg::Welcome { sessions, .. }) => Ok(sessions),
            Ok(Msg::Error { code, message, .. }) => Err(HandshakeError::Rejected { code, message }),
            Ok(other) => Err(HandshakeError::Protocol(format!(
                "expected welcome, got {other:?}"
            ))),
            Err(RecvError::Closed) => Err(HandshakeError::Closed),
            Err(e) => Err(HandshakeError::Protocol(e.to_string())),
        }
    }

    /// 服务端握手：等 `hello`，版本协商，回 `welcome`（或 error 后报错）。
    pub fn server_welcome(
        &mut self,
        server_version: &str,
        sessions: Vec<SessionSummary>,
    ) -> Result<ClientKind, HandshakeError> {
        let (client, min_v, max_v) = match self.recv() {
            Ok(Msg::Hello {
                client,
                min_v,
                max_v,
                ..
            }) => (client, min_v, max_v),
            Ok(other) => {
                return Err(HandshakeError::Protocol(format!(
                    "expected hello, got {other:?}"
                )));
            }
            Err(RecvError::Closed) => return Err(HandshakeError::Closed),
            Err(e) => return Err(HandshakeError::Protocol(e.to_string())),
        };
        if negotiate_version(min_v, max_v).is_none() {
            let _ = self.writer().send(&Msg::Error {
                code: "version_unsupported".to_string(),
                message: format!("server speaks v{PROTOCOL_VERSION}"),
                session: None,
            });
            return Err(HandshakeError::VersionUnsupported { min_v, max_v });
        }
        self.writer()
            .send(&Msg::Welcome {
                server_version: server_version.to_string(),
                sessions,
            })
            .map_err(HandshakeError::Io)?;
        Ok(client)
    }
}

/// 可克隆共享的写端（S2C 事件多生产者场景）。
#[derive(Clone)]
pub struct ConnWriter {
    wr: Arc<Mutex<Box<dyn io::Write + Send>>>,
}

impl ConnWriter {
    /// 序列化并写一帧。写路径互斥，帧间不交错。
    pub fn send(&self, msg: &Msg) -> io::Result<()> {
        let env = Envelope::new(msg.clone());
        let bytes = env
            .to_json()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut wr = self.wr.lock().unwrap();
        write_frame(&mut *wr, &bytes)
    }
}

// ---------- Windows：命名管道 ----------

#[cfg(windows)]
mod imp {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
    use std::time::Duration;
    use windows::Win32::Foundation::{ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE};
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::core::{HRESULT, HSTRING, PCWSTR};

    fn pipe_path(name: &str) -> String {
        format!(r"\\.\pipe\aerodesk-{name}")
    }

    fn new_instance(path: &str) -> io::Result<File> {
        let wide = HSTRING::from(path);
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                255,
                1 << 16,
                1 << 16,
                0,
                None,
            )
        };
        if h.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_handle(h.0 as RawHandle) })
    }

    /// 命名管道监听端。`accept` 串行接客（B3/B4 每客户端一条连接）。
    pub struct Listener {
        path: String,
        pending: Option<File>,
    }

    impl Listener {
        pub fn bind(name: &str) -> io::Result<Self> {
            validate_name(name)?;
            let path = pipe_path(name);
            let first = new_instance(&path)?;
            Ok(Self {
                path,
                pending: Some(first),
            })
        }

        pub fn accept(&mut self) -> io::Result<Conn> {
            let inst = self
                .pending
                .take()
                .expect("listener always holds a pending instance");
            // 客户端抢先连入时 ERROR_PIPE_CONNECTED 属成功路径（MSDN）。
            let r = unsafe { ConnectNamedPipe(HANDLE(inst.as_raw_handle() as _), None) };
            if let Err(e) = r {
                if e.code() != HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) {
                    return Err(io::Error::other(e));
                }
            }
            let rd = inst.try_clone()?;
            let conn = Conn {
                rd: Box::new(rd),
                wr: Arc::new(Mutex::new(Box::new(inst) as Box<dyn io::Write + Send>)),
            };
            // 预建下一实例，后续客户端不必等服务端绕回 accept。
            self.pending = Some(new_instance(&self.path)?);
            Ok(conn)
        }
    }

    impl Conn {
        /// 单发连接（服务端未就绪即失败）；要等待用 [`Conn::connect_wait`]。
        pub fn connect(name: &str) -> io::Result<Self> {
            validate_name(name)?;
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(pipe_path(name))?;
            let rd = f.try_clone()?;
            Ok(Self {
                rd: Box::new(rd),
                wr: Arc::new(Mutex::new(Box::new(f) as Box<dyn io::Write + Send>)),
            })
        }

        /// 带等待的连接：实例忙（ERROR_PIPE_BUSY）或路径未存在时按 50ms
        /// 节拍重试，直至 `timeout`。
        pub fn connect_wait(name: &str, timeout: Duration) -> io::Result<Self> {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                match Self::connect(name) {
                    Ok(c) => return Ok(c),
                    Err(e) => {
                        let busy = e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32);
                        let absent = e.kind() == io::ErrorKind::NotFound;
                        if !(busy || absent) || std::time::Instant::now() >= deadline {
                            return Err(e);
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
    }
}

// ---------- Unix：domain socket ----------

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::time::Duration;

    fn sock_path(name: &str) -> PathBuf {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(dir).join(format!("aerodesk-{name}.sock"))
    }

    /// UDS 监听端；bind 前清残留套接字文件，Drop 时回收。
    pub struct Listener {
        path: PathBuf,
        inner: UnixListener,
    }

    impl Listener {
        pub fn bind(name: &str) -> io::Result<Self> {
            validate_name(name)?;
            let path = sock_path(name);
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            let inner = UnixListener::bind(&path)?;
            Ok(Self { path, inner })
        }

        pub fn accept(&mut self) -> io::Result<Conn> {
            let (stream, _) = self.inner.accept()?;
            let rd = stream.try_clone()?;
            Ok(Conn {
                rd: Box::new(rd),
                wr: Arc::new(Mutex::new(Box::new(stream) as Box<dyn io::Write + Send>)),
            })
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    impl Conn {
        pub fn connect(name: &str) -> io::Result<Self> {
            validate_name(name)?;
            let s = UnixStream::connect(sock_path(name))?;
            let rd = s.try_clone()?;
            Ok(Self {
                rd: Box::new(rd),
                wr: Arc::new(Mutex::new(Box::new(s) as Box<dyn io::Write + Send>)),
            })
        }

        pub fn connect_wait(name: &str, timeout: Duration) -> io::Result<Self> {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                match Self::connect(name) {
                    Ok(c) => return Ok(c),
                    Err(e) => {
                        if e.kind() != io::ErrorKind::NotFound
                            || std::time::Instant::now() >= deadline
                        {
                            return Err(e);
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
    }
}

pub use imp::Listener;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    fn unique_name(tag: &str) -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        format!("t{}-{}", N.fetch_add(1, Ordering::Relaxed), tag)
    }

    fn spawn_server(
        name: &str,
        sessions: Vec<SessionSummary>,
    ) -> std::thread::JoinHandle<Result<Conn, HandshakeError>> {
        let name = name.to_string();
        std::thread::spawn(move || {
            let mut l = Listener::bind(&name).map_err(HandshakeError::Io)?;
            let mut conn = l.accept().map_err(HandshakeError::Io)?;
            conn.server_welcome("0.1.0-test", sessions)?;
            Ok(conn)
        })
    }

    #[test]
    fn handshake_and_echo_roundtrip() {
        let name = unique_name("echo");
        let server = spawn_server(&name, vec![]);
        let mut client = Conn::connect_wait(&name, Duration::from_secs(5)).unwrap();
        let sessions = client
            .client_hello(ClientKind::Desktop, "0.1.0-test")
            .unwrap();
        assert!(sessions.is_empty());
        let mut conn = server.join().unwrap().unwrap();

        // C2S → S2C 各跑一条真实帧。
        client
            .writer()
            .send(&Msg::Connect {
                room: "r".into(),
                server: "wss://s".into(),
                token: "t".into(),
                mode: crate::proto::ConnectMode::Control,
            })
            .unwrap();
        match conn.recv().unwrap() {
            Msg::Connect { room, .. } => assert_eq!(room, "r"),
            other => panic!("expected connect, got {other:?}"),
        }
        conn.writer()
            .send(&Msg::SessionOpened { session: 1 })
            .unwrap();
        assert_eq!(client.recv().unwrap(), Msg::SessionOpened { session: 1 });

        // 干净关闭：客户端 drop 写端后服务端见 Closed。
        drop(client);
        match conn.recv() {
            Err(RecvError::Closed) => {}
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn unknown_kind_replyable_and_connection_survives() {
        let name = unique_name("unk");
        let server = spawn_server(&name, vec![]);
        let mut client = Conn::connect_wait(&name, Duration::from_secs(5)).unwrap();
        client.client_hello(ClientKind::Cli, "0.1.0-test").unwrap();
        let mut conn = server.join().unwrap().unwrap();

        // 客户端绕过类型系统手写一帧未知 kind（模拟更新版本的对端）。
        {
            let mut wr = client.wr.lock().unwrap();
            write_frame(&mut *wr, br#"{"v":1,"kind":"mystery_future","x":1}"#).unwrap();
        }
        match conn.recv() {
            Err(RecvError::UnknownKind { v, kind }) => {
                assert_eq!((v, kind.as_str()), (1, "mystery_future"));
            }
            other => panic!("expected UnknownKind, got {other:?}"),
        }
        conn.writer()
            .send(&Msg::Error {
                code: "unknown_kind".into(),
                message: "mystery_future".into(),
                session: None,
            })
            .unwrap();
        match client.recv().unwrap() {
            Msg::Error { code, .. } => assert_eq!(code, "unknown_kind"),
            other => panic!("expected error, got {other:?}"),
        }
        // 不断连：后续正常消息照收。
        client
            .writer()
            .send(&Msg::Ping {
                nonce: 1,
                sent_ms: 2,
            })
            .unwrap();
        assert_eq!(
            conn.recv().unwrap(),
            Msg::Ping {
                nonce: 1,
                sent_ms: 2
            }
        );
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let name = unique_name("ver");
        let server = spawn_server(&name, vec![]);
        let mut client = Conn::connect_wait(&name, Duration::from_secs(5)).unwrap();

        // 手写高版本 hello（client_hello 只会发当前版本）。
        client
            .writer()
            .send(&Msg::Hello {
                client: ClientKind::Desktop,
                client_version: "9.9.9".into(),
                min_v: 99,
                max_v: 100,
            })
            .unwrap();
        match client.recv().unwrap() {
            Msg::Error { code, .. } => assert_eq!(code, "version_unsupported"),
            other => panic!("expected error, got {other:?}"),
        }
        match server.join().unwrap() {
            Err(HandshakeError::VersionUnsupported { min_v, max_v }) => {
                assert_eq!((min_v, max_v), (99, 100));
            }
            other => panic!("expected VersionUnsupported, got {other:?}"),
        }
    }

    #[test]
    fn malformed_frame_is_reported() {
        let name = unique_name("bad");
        let server = spawn_server(&name, vec![]);
        let mut client = Conn::connect_wait(&name, Duration::from_secs(5)).unwrap();
        client.client_hello(ClientKind::Cli, "0.1.0-test").unwrap();
        let mut conn = server.join().unwrap().unwrap();

        {
            let mut wr = client.wr.lock().unwrap();
            write_frame(&mut *wr, b"{not json at all").unwrap();
        }
        match conn.recv() {
            Err(RecvError::Malformed(_)) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn illegal_names_rejected() {
        assert!(validate_name("host-agent").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("UPPER").is_err());
        assert!(validate_name("has/slash").is_err());
        assert!(validate_name("back\\slash").is_err());
        assert!(validate_name(&"x".repeat(33)).is_err());
    }
}
