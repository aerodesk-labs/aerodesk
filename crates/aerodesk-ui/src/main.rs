//! AeroDesk UI 壳（Slint）：主页（连接区 + 最近会话）+ 设置占位。
//!
//! 5 个原生平台（Win/macOS/Linux/Android/iOS）一套 UI；Web 走浏览器原生 WebRTC。

slint::include_modules!();
use slint::Model;

use std::path::PathBuf;

const MAX_RECENTS: usize = 10;

fn main() -> Result<(), slint::PlatformError> {
    init_log();
    let ui = AppWindow::new()?;

    // 最近会话（本地持久化）
    ui.set_recents(slint::ModelRc::new(slint::VecModel::from(load_recents())));

    ui.on_set_tab({
        let ui = ui.as_weak();
        move |t| {
            let ui = ui.unwrap();
            ui.set_tab(t);
        }
    });

    ui.on_connect({
        let weak = ui.as_weak();
        move || {
            let ui = weak.unwrap();
            let server = ui.get_server_input().to_string();
            let room = ui.get_room_input().to_string();
            let token = ui.get_token_input().to_string();
            ui.set_connecting(true);
            ui.set_status(format!("连接 {} @ {} …", room, server).into());
            let weak2 = weak.clone();
            std::thread::spawn(move || {
                let auth = if token.is_empty() { None } else { Some(token.as_str()) };
                let out = aerodesk_core::connect::connect_viewer_auth(&server, &room, auth);
                let ui = weak2.unwrap();
                match out {
                    Ok(r) => {
                        ui.set_status(format!("已连接：peer={} ice={}", r.peer_id, r.ice_connected).into());
                        ui.set_log(
                            format!(
                                "房间: {room}\n服务器: {server}\nSDP 交换: OK\nICE: {}\n\n已建立 WebRTC 会话（媒体/输入由后续里程碑接入）。",
                                if r.ice_connected { "connected" } else { "pending(5s 超时)" }
                            )
                            .into(),
                        );
                        add_recent(&ui, &room, &server);
                    }
                    Err(e) => {
                        ui.set_status(format!("连接失败：{e}").into());
                        ui.set_log(String::new().into());
                    }
                }
                ui.set_connecting(false);
            });
        }
    });

    ui.on_disconnect({
        let weak = ui.as_weak();
        move || {
            let ui = weak.unwrap();
            ui.set_status("已断开".into());
            ui.set_connecting(false);
        }
    });

    ui.on_connect_recent({
        let weak = ui.as_weak();
        move |entry: slint::SharedString| {
            let ui = weak.unwrap();
            let (room, server) = parse_recent(&entry.to_string());
            ui.set_room_input(room.into());
            ui.set_server_input(server.into());
            // 复用连接回调
            ui.invoke_connect();
        }
    });

    ui.run()
}

/// 最近会话格式：`房间 · 服务器`（解析用分隔符）。
fn parse_recent(entry: &str) -> (String, String) {
    match entry.split_once(" · ") {
        Some((r, s)) => (r.to_string(), s.to_string()),
        None => (entry.to_string(), "wss://signal.aerodesk.io".to_string()),
    }
}

fn recent_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".aerodesk-recent.json")
}

fn load_recents() -> Vec<slint::SharedString> {
    let Ok(text) = std::fs::read_to_string(recent_path()) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .unwrap_or_default()
        .into_iter()
        .map(Into::into)
        .collect()
}

fn save_recents(items: &[slint::SharedString]) {
    let v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
    if let Ok(json) = serde_json::to_string(&v) {
        let _ = std::fs::write(recent_path(), json);
    }
}

fn add_recent(ui: &AppWindow, room: &str, server: &str) {
    let model = ui.get_recents();
    let mut items: Vec<String> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .map(|s| s.to_string())
        .collect();
    let entry = format!("{room} · {server}");
    items.retain(|i| i != &entry);
    items.insert(0, entry);
    items.truncate(MAX_RECENTS);
    let new: Vec<slint::SharedString> = items.iter().map(|s| s.into()).collect();
    ui.set_recents(slint::ModelRc::new(slint::VecModel::from(new.clone())));
    save_recents(&new);
}

fn init_log() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aerodesk_ui=info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
}
