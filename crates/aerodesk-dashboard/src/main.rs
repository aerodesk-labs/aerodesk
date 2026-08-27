//! aerodesk-dashboard —— 只读运维代理 + 单页 dashboard（#369）。
//!
//! 复用 SFU/signal 现有内部 API，不新增后端耦合：本进程只做
//! 「静态页托管 + 带 INTERNAL_TOKEN 的鉴权代理 + 少量指标解析」。
//!
//! 环境变量：
//! - ADMIN_BIND（默认 127.0.0.1:3080）
//! - SFU_ADMIN_URL（默认 http://127.0.0.1:3002）
//! - SIGNAL_ADMIN_URL（默认 https://127.0.0.1:3001，P3 ops HTTPS 面；自签证书需信任或忽略校验）
//! - INTERNAL_TOKEN（SFU 管理接口鉴权；未设置则后端 loopback 模式，仍可试）
//! - ADMIN_TOKEN（dashboard 自身鉴权；设置后 /api/* 需 X-Admin-Token 头）

use std::io::{Read, Write};
use std::net::TcpStream;

use rouille::{Request, Response};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// 极简 HTTP/1.1 客户端（内部 API 是明文 HTTP，无需 TLS）。
/// 返回 (status, body)。
fn http(
    method: &str,
    base_url: &str,
    path_and_query: &str,
    token: &str,
    body: Option<&str>,
) -> Result<(u16, String), String> {
    let url = base_url.trim_end_matches('/');
    let host_port = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// supported: {base_url}"))?;
    let path = if path_and_query.starts_with('/') {
        path_and_query.to_string()
    } else {
        format!("/{path_and_query}")
    };
    let body = body.unwrap_or("");
    let mut stream =
        TcpStream::connect(host_port).map_err(|e| format!("connect {host_port}: {e}"))?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nX-Internal-Token: {token}\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write {host_port}: {e}"))?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read {host_port}: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(500);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Ok((status, body))
}

/// 代理 JSON：把内部 API 的响应透传给浏览器（保留状态码与 JSON body）。
fn proxy_json(method: &str, base: &str, pq: &str, token: &str) -> Response {
    match http(method, base, pq, token, None) {
        Ok((status, body)) => Response::text(body)
            .with_status_code(status)
            .with_unique_header("Content-Type", "application/json")
            .with_unique_header("Access-Control-Allow-Origin", "*"),
        Err(e) => Response::text(format!("{{\"error\":\"{e}\"}}"))
            .with_status_code(502)
            .with_unique_header("Content-Type", "application/json"),
    }
}

/// POST 代理（kick / record start/stop）。
fn proxy_post(base: &str, pq: &str, token: &str) -> Response {
    match http("POST", base, pq, token, None) {
        Ok((status, body)) => Response::text(body)
            .with_status_code(status)
            .with_unique_header("Content-Type", "application/json")
            .with_unique_header("Access-Control-Allow-Origin", "*"),
        Err(e) => Response::text(format!("{{\"error\":\"{e}\"}}"))
            .with_status_code(502)
            .with_unique_header("Content-Type", "application/json"),
    }
}

/// 从 Prometheus 文本里抽关键 gauge：总量 + 分片维度（shard_load/clients_by_shard）。
fn parse_metrics(prom: &str) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    let mut shard_load = serde_json::Map::new();
    let mut clients_by_shard = serde_json::Map::new();
    for line in prom.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(space) = line.rfind(' ') else {
            continue;
        };
        let value = line[space + 1..].trim();
        let name_labels = &line[..space];
        let (name, shard) = match name_labels.split_once('{') {
            Some((n, rest)) => {
                let shard = rest
                    .trim_end_matches('}')
                    .split('=')
                    .nth(1)
                    .map(|s| s.trim_matches('"'));
                (n, shard)
            }
            None => (name_labels, None),
        };
        match name {
            "aerodesk_sfu_clients" => {
                if let Some(shard) = shard {
                    clients_by_shard.insert(shard.to_string(), serde_json::json!(value));
                } else {
                    out.insert("clients".to_string(), serde_json::json!(value));
                }
            }
            "aerodesk_sfu_shard_load" => {
                if let Some(shard) = shard {
                    shard_load.insert(shard.to_string(), serde_json::json!(value));
                }
            }
            "aerodesk_sfu_turn_allocations" => {
                out.insert("turn_allocations".to_string(), serde_json::json!(value));
            }
            "aerodesk_sfu_recordings_active" => {
                out.insert("recordings_active".to_string(), serde_json::json!(value));
            }
            "aerodesk_sfu_draining" => {
                out.insert("draining".to_string(), serde_json::json!(value));
            }
            _ => {}
        }
    }
    out.insert(
        "clients_by_shard".to_string(),
        serde_json::Value::Object(clients_by_shard),
    );
    out.insert(
        "shard_load".to_string(),
        serde_json::Value::Object(shard_load),
    );
    serde_json::Value::Object(out)
}

fn admin_request(
    request: &Request,
    sfu: &str,
    signal: &str,
    token: &str,
    admin_token: &str,
) -> Response {
    let url = request.url();
    let qs = request.raw_query_string();
    // 页面无需鉴权（静态 UI）；/api/* 在配置 ADMIN_TOKEN 时要求 X-Admin-Token。
    if !admin_token.is_empty()
        && url.starts_with("/api/")
        && request.header("X-Admin-Token") != Some(admin_token)
    {
        return Response::text("unauthorized")
            .with_status_code(401)
            .with_unique_header("Access-Control-Allow-Origin", "*");
    }
    // 页面
    if request.method() == "GET" && url == "/" {
        return Response::html(include_str!("../../../web/admin.html"));
    }
    // SFU 只读 API
    if request.method() == "GET" && url == "/api/health" {
        return proxy_json("GET", sfu, "/healthz", token);
    }
    if request.method() == "GET" && url == "/api/signal-health" {
        return proxy_json("GET", signal, "/healthz", token);
    }
    if request.method() == "GET" && url == "/api/rooms" {
        return proxy_json("GET", sfu, "/session/rooms", token);
    }
    if request.method() == "GET" && url == "/api/clients" {
        return proxy_json("GET", sfu, &format!("/session/clients?{qs}"), token);
    }
    if request.method() == "GET" && url == "/api/recordings" {
        return proxy_json("GET", sfu, "/record/status", token);
    }
    // 指标：只抽关键 gauge 返回 JSON
    if request.method() == "GET" && url == "/api/metrics" {
        return match http("GET", sfu, "/metrics/prometheus", token, None) {
            Ok((200, body)) => Response::text(parse_metrics(&body).to_string())
                .with_status_code(200)
                .with_unique_header("Content-Type", "application/json"),
            Ok((status, body)) => Response::text(body).with_status_code(status),
            Err(e) => Response::text(format!("{{\"error\":\"{e}\"}}")).with_status_code(502),
        };
    }
    // 写操作：kick / record（POST，透传 query）
    if request.method() == "POST" && url == "/api/kick" {
        return proxy_post(sfu, &format!("/session/kick?{qs}"), token);
    }
    if request.method() == "POST" && url == "/api/record/start" {
        return proxy_post(sfu, &format!("/record/start?{qs}"), token);
    }
    if request.method() == "POST" && url == "/api/record/stop" {
        return proxy_post(sfu, &format!("/record/stop?{qs}"), token);
    }
    if request.method() == "OPTIONS" {
        return Response::empty_204()
            .with_unique_header("Access-Control-Allow-Origin", "*")
            .with_unique_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
            .with_unique_header(
                "Access-Control-Allow-Headers",
                "Content-Type, X-Admin-Token",
            );
    }
    Response::text("not found").with_status_code(404)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let bind = env_or("ADMIN_BIND", "127.0.0.1:3080");
    let sfu = env_or("SFU_ADMIN_URL", "http://127.0.0.1:3002");
    let signal = env_or("SIGNAL_ADMIN_URL", "https://127.0.0.1:3001");
    let token = env_or("INTERNAL_TOKEN", "");
    let admin_token = env_or("ADMIN_TOKEN", "");
    tracing::info!(
        "aerodesk-dashboard listening on http://{bind} (sfu={sfu} signal={signal} auth={})",
        if admin_token.is_empty() { "off" } else { "on" }
    );
    let bind_url = bind.clone();
    let server = rouille::Server::new(bind, move |request| {
        admin_request(request, &sfu, &signal, &token, &admin_token)
    })
    .expect("bind admin server");
    println!("Admin dashboard: http://{bind_url}");
    server.run();
}
