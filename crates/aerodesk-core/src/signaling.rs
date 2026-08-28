//! 信令地址归一化 + /devices 在线设备查询（HTTP 面）。
//!
//! #598 P4：WSS 信令客户端（WsSignalClient）已随 JSON 栈退役删除——
//! 信令统一走 SIP（crates/aerodesk-protocol::sip_client）；本模块仅保留
//! URL 归一化（地址作 SIP host 载体）与设备列表 HTTP 查询。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// 归一化信令服务器地址：补全协议前缀与 `/ws` 路径。
///
/// - 未带 `://` 时自动补协议：回环地址（localhost/127.0.0.1/::1）用 `ws://`，
///   其余默认 `wss://`（用户也可显式写 `ws://host:port`）。
/// - 未带路径时补 `/ws`（服务器 WebSocket 端点统一挂在此路径下，连根路径会被回 200 而卡住）。
/// - 已带协议/路径的输入原样保留。
pub fn normalize_signal_url(input: &str) -> String {
    normalize_signal_url_with_tls(input, true)
}

/// 归一化信令服务器地址，无显式协议时按 `default_tls` 选择 `ws://` / `wss://`（#504）。
///
/// 与 [`normalize_signal_url`] 的唯一区别在非回环裸地址的默认协议：
/// `default_tls=false` 时补 `ws://`（自建明文信令服务器场景）。回环地址始终补
/// `ws://`（loopback 上 TLS 无意义）；已带 `://` 的显式协议输入不受开关影响。
pub fn normalize_signal_url_with_tls(input: &str, default_tls: bool) -> String {
    let input = input.trim();
    if input.is_empty() {
        return input.to_string();
    }
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        let host = if let Some(rest) = input.strip_prefix('[') {
            rest.split(']').next().unwrap_or("")
        } else {
            input.split(':').next().unwrap_or("")
        };
        let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1" | "0:0:0:0:0:0:0:1");
        let scheme = if !loopback && default_tls {
            "wss"
        } else {
            "ws"
        };
        format!("{scheme}://{input}")
    };
    // 路径处理：仅当无路径（或只有根路径 `/`）时补 `/ws`；
    // 已有任何显式路径则原样保留（与 docstring 一致），
    // 避免 `contains("/ws")` 子串误判（如 /wsfoo）与尾部斜杠产生 `//ws`。
    let Some(rest) = with_scheme.split_once("://").map(|(_, r)| r) else {
        return with_scheme;
    };
    match rest.find('/') {
        // 无路径：补 /ws
        None => format!("{with_scheme}/ws"),
        // 只有尾部根斜杠：去掉后补 /ws（避免 //ws）
        Some(i) if i == rest.len() - 1 => format!("{}/ws", &with_scheme[..with_scheme.len() - 1]),
        // 已有路径（含 /ws 或其它）：原样保留
        Some(_) => with_scheme,
    }
}

// ===================== #503 在线设备列表（GET /devices） =====================

/// 从 `host[:port]` 拆出主机与端口（IPv6 支持 `[::1]:3003` 方括号形式）。
/// 未带端口时用 `default_port`（与信令 URL 归一化一致：不显式写端口 = 协议默认）。
fn split_host_port(server: &str, default_port: u16) -> (String, u16) {
    let s = server.trim();
    if let Some(rest) = s.strip_prefix('[') {
        if let Some((host, port)) = rest.split_once("]:") {
            return (host.to_string(), port.parse().unwrap_or(default_port));
        }
        return (rest.trim_end_matches(']').to_string(), default_port);
    }
    match s.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (host.to_string(), port.parse().unwrap_or(default_port))
        }
        _ => (s.to_string(), default_port),
    }
}

/// 拉取信令服务器在线设备列表（`GET /devices`，明文 HTTP 或 HTTPS）。
///
/// #503 设备列表：桌面主控端「无人值守入口管理」的数据源。`server` 为
/// `host[:port]`（UI 展示形态）；`tls=true` 走 HTTPS（wss 服务器，无端口默认
/// 443），否则明文 HTTP（自建明文信令服务器，无端口默认 80；本地常用
/// `127.0.0.1:3003` 显式带端口）。响应结构：
/// `{"devices":[{"id":"AD-..","via":["sip","wss"]}],"pop":".."}`，本函数只取 id。
pub fn fetch_online_devices(server: &str, tls: bool) -> Result<Vec<String>, String> {
    #[derive(serde::Deserialize)]
    struct DeviceItem {
        id: String,
    }
    #[derive(serde::Deserialize)]
    struct DevicesResponse {
        devices: Vec<DeviceItem>,
    }

    let default_port = if tls { 443 } else { 80 };
    let (host, port) = split_host_port(server, default_port);
    // 支持主机名（默认配置 signal.aerodesk.io 等）：ToSocketAddrs 走 DNS 解析，
    // SocketAddr::parse 只接受 IP 字面量——此前默认服务器名永远报「地址无效」。
    let addr = std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port))
        .map_err(|e| format!("服务器地址无效 {host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| format!("服务器地址解析为空 {host}:{port}"))?;
    // 超时兜底：服务器不可达/无响应时快速失败，不阻塞 UI 后台线程。
    let mut tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .map_err(|e| format!("连接信令服务器 {addr} 失败: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    tcp.set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| format!("set_write_timeout: {e}"))?;
    let host_header = if port == default_port {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    let request = format!(
        "GET /devices HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: aerodesk-desktop/1.0\r\nConnection: close\r\n\r\n"
    );
    let body = if tls {
        http_get_tls_body(tcp, &host, &request)?
    } else {
        let mut out = Vec::new();
        let _ = tcp.write_all(request.as_bytes());
        tcp.read_to_end(&mut out)
            .map_err(|e| format!("读取信令服务器响应失败: {e}"))?;
        String::from_utf8_lossy(&out).to_string()
    };
    // 解析 HTTP 响应：状态行 + 头 + body（rouille 响应带 Content-Length）。
    let (head, body) = match body.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.to_string()),
        None => return Err(format!("信令服务器响应格式异常：{body:.200}")),
    };
    if !head.starts_with("HTTP/1.1 200") {
        let status = head.lines().next().unwrap_or("?");
        return Err(format!("信令服务器 /devices 响应 {status}"));
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("解析 /devices 响应失败: {e}"))?;
    let devices = serde_json::from_value::<DevicesResponse>(v)
        .map_err(|e| format!("/devices 响应结构不符: {e}"))?
        .devices;
    let mut ids: Vec<String> = devices.into_iter().map(|d| d.id).collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// HTTPS 版 GET body：rustls 客户端（webpki-roots 系统根，与 turn_client 同款）。
fn http_get_tls_body(tcp: TcpStream, host: &str, request: &str) -> Result<String, String> {
    // 显式安装 ring provider（跨 crate feature 解析可能歧义，见 LESSON rustls0.23）。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let sni = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("无效 TLS 服务器名 {host}: {e}"))?;
    let conn =
        rustls::ClientConnection::new(Arc::new(cfg), sni).map_err(|e| format!("tls conn: {e}"))?;
    let mut tls_stream = rustls::StreamOwned::new(conn, tcp);
    // 显式完成 TLS 握手（read_timeout 会中断 lazy 握手，与 turn_client 同）。
    tls_stream
        .conn
        .complete_io(&mut tls_stream.sock)
        .map_err(|e| format!("tls handshake: {e}"))?;
    let _ = tls_stream.write_all(request.as_bytes());
    let mut out = Vec::new();
    tls_stream
        .read_to_end(&mut out)
        .map_err(|e| format!("读取信令服务器响应失败: {e}"))?;
    Ok(String::from_utf8_lossy(&out).to_string())
}

mod tests {
    use super::{
        fetch_online_devices, normalize_signal_url, normalize_signal_url_with_tls, split_host_port,
    };
    use std::time::Duration;

    #[test]
    fn fetch_online_devices_parses_http_response() {
        use std::io::{Read, Write};
        // 极简 HTTP 假服务器：接受连接，读请求头校验路径，回固定 /devices 响应。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut req = String::new();
            let mut buf = [0u8; 4096];
            while !req.contains("\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                req.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            assert!(req.starts_with("GET /devices HTTP/1.1"), "{req}");
            let body = r#"{"devices":[{"id":"AD-B","via":["sip"]},{"id":"AD-A","via":["wss"]}],"pop":"local"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        });
        // 无端口 → 默认 80；有端口 → 用显式端口。
        let ids = fetch_online_devices(&addr.to_string(), false).expect("fetch 应成功");
        server.join().expect("server 线程");
        // 去重 + 排序。
        assert_eq!(ids, vec!["AD-A".to_string(), "AD-B".to_string()]);
    }

    #[test]
    fn fetch_online_devices_reports_server_errors() {
        // 未监听端口 → 连接失败 → Err（快速失败而非长阻塞）。
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let err = fetch_online_devices(&addr.to_string(), false).unwrap_err();
        assert!(err.contains("连接信令服务器"), "{err}");
    }

    #[test]
    fn split_host_port_variants() {
        assert_eq!(
            split_host_port("127.0.0.1:3003", 80),
            ("127.0.0.1".into(), 3003)
        );
        assert_eq!(
            split_host_port("signal.aerodesk.io", 443),
            ("signal.aerodesk.io".into(), 443)
        );
        assert_eq!(split_host_port("[::1]:3003", 80), ("::1".into(), 3003));
        assert_eq!(split_host_port("[::1]", 443), ("::1".into(), 443));
        assert_eq!(
            split_host_port("host:abc", 80),
            ("host:abc".into(), 80),
            "非数字端口按默认"
        );
    }

    #[test]
    fn normalize_with_tls_toggle_picks_default_scheme() {
        // 裸地址 + TLS 关 → ws://（自建明文服务器场景，#504）
        assert_eq!(
            normalize_signal_url_with_tls("129.226.150.174:14703", false),
            "ws://129.226.150.174:14703/ws"
        );
        // 裸地址 + TLS 开 → wss://
        assert_eq!(
            normalize_signal_url_with_tls("signal.aerodesk.io", true),
            "wss://signal.aerodesk.io/ws"
        );
        // 回环地址不受开关影响，始终 ws://（loopback 上 TLS 无意义）
        assert_eq!(
            normalize_signal_url_with_tls("127.0.0.1:3003", true),
            "ws://127.0.0.1:3003/ws"
        );
        assert_eq!(
            normalize_signal_url_with_tls("localhost:3003", false),
            "ws://localhost:3003/ws"
        );
        // 显式 scheme 优先于开关（两个方向都保留）
        assert_eq!(
            normalize_signal_url_with_tls("wss://h:3003", false),
            "wss://h:3003/ws"
        );
        assert_eq!(
            normalize_signal_url_with_tls("ws://h:3003/ws", true),
            "ws://h:3003/ws"
        );
        // 空串原样返回
        assert_eq!(normalize_signal_url_with_tls("", true), "");
        assert_eq!(normalize_signal_url_with_tls("  ", false), "");
    }

    #[test]
    fn normalize_adds_scheme_and_path() {
        // 回环地址 → ws://
        assert_eq!(
            normalize_signal_url("127.0.0.1:3003"),
            "ws://127.0.0.1:3003/ws"
        );
        assert_eq!(
            normalize_signal_url("localhost:3003"),
            "ws://localhost:3003/ws"
        );
        assert_eq!(normalize_signal_url("[::1]:3003"), "ws://[::1]:3003/ws");
        // 非回环 → wss://（默认安全）
        assert_eq!(
            normalize_signal_url("signal.aerodesk.io"),
            "wss://signal.aerodesk.io/ws"
        );
        assert_eq!(
            normalize_signal_url("signal.aerodesk.io:3001"),
            "wss://signal.aerodesk.io:3001/ws"
        );
        // 已带协议/路径 → 原样保留
        assert_eq!(
            normalize_signal_url("wss://signal.aerodesk.io/ws"),
            "wss://signal.aerodesk.io/ws"
        );
        assert_eq!(
            normalize_signal_url("ws://127.0.0.1:3003"),
            "ws://127.0.0.1:3003/ws"
        );
        // 尾部根斜杠 → 补 /ws 且不产生双斜杠
        assert_eq!(
            normalize_signal_url("127.0.0.1:3003/"),
            "ws://127.0.0.1:3003/ws"
        );
        assert_eq!(
            normalize_signal_url("ws://127.0.0.1:3003/"),
            "ws://127.0.0.1:3003/ws"
        );
        // 已有非 /ws 显式路径 → 原样保留（不追加，不因子串误判）
        assert_eq!(
            normalize_signal_url("ws://h:3003/signaling"),
            "ws://h:3003/signaling"
        );
        assert_eq!(
            normalize_signal_url("ws://h:3003/wsfoo"),
            "ws://h:3003/wsfoo"
        );
        // 空串
        assert_eq!(normalize_signal_url(""), "");
        assert_eq!(normalize_signal_url("   "), "");
    }
}
