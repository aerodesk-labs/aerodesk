//! AeroDesk UI 壳（Slint）：主页（连接区 + 最近会话）+ 会话视图（#23 初版）。
//!
//! 5 个原生平台（Win/macOS/Linux/Android/iOS）一套 UI；Web 走浏览器原生 WebRTC。

slint::include_modules!();
#[cfg(target_os = "macos")]
mod macos_media;
use slint::Model;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(not(target_os = "macos"))]
use std::time::Duration;

const MAX_RECENTS: usize = 10;
const DEMO_W: u32 = 320;
const DEMO_H: u32 = 180;

fn main() -> Result<(), slint::PlatformError> {
    init_log();
    let ui = AppWindow::new()?;

    // 最近会话（本地持久化）
    ui.set_recents(slint::ModelRc::new(slint::VecModel::from(load_recents())));

    // 设置（本地持久化）
    let mut settings = load_settings();
    // 本机 ID：首启生成并持久化（RustDesk 左栏「本机 ID」对齐）。
    if settings.device_id.is_empty() {
        settings.device_id = default_device_id();
        save_settings(&settings);
    }
    ui.set_device_id(settings.device_id.clone().into());
    let pw_display = if settings.device_pw.is_empty() {
        "未设置".to_string()
    } else {
        settings.device_pw.clone()
    };
    ui.set_device_pw(pw_display.into());
    ui.set_quality(settings.quality);
    ui.set_server_default(settings.server_default.clone().into());
    ui.set_remember_token(settings.remember_token);
    ui.set_token_default(settings.token_default.clone().into());
    if !settings.server_default.is_empty() {
        ui.set_server_input(settings.server_default.into());
    }
    if settings.remember_token && !settings.token_default.is_empty() {
        ui.set_token_input(settings.token_default.into());
    }
    // 复制本机 ID / 密码到剪贴板。
    ui.on_copy_device_id({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            copy_to_clipboard(&ui.get_device_id().to_string());
            ui.set_status("本机 ID 已复制".into());
        }
    });
    ui.on_copy_device_pw({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            copy_to_clipboard(&ui.get_device_pw().to_string());
            ui.set_status("密码已复制".into());
        }
    });
    // 会话帧线程代际：断开/新会话时递增，使旧帧线程退出（防线程泄漏）。
    let frame_epoch = Arc::new(AtomicU64::new(0));

    ui.on_set_tab({
        let ui = ui.as_weak();
        move |t| {
            let ui = ui.unwrap();
            ui.set_tab(t);
        }
    });

    ui.on_connect({
        let weak = ui.as_weak();
        let frame_epoch = frame_epoch.clone();
        move || {
            let ui = weak.unwrap();
            let server = ui.get_server_input().to_string();
            let room = ui.get_room_input().to_string();
            let token = ui.get_token_input().to_string();
            ui.set_connecting(true);
            ui.set_conn_state(1);
            ui.set_status(format!("连接 {} @ {} …", room, server).into());
            let weak2 = weak.clone();
            // 本会话代际：断开/新连接会递增 epoch，旧连接/旧帧线程据此退出。
            let my_epoch = frame_epoch.fetch_add(1, Ordering::SeqCst) + 1;
            let epoch2 = frame_epoch.clone();
            std::thread::spawn(move || {
                #[cfg(target_os = "macos")]
                {
                    // #29：macOS 真实 H.264 解码渲染（替换演示帧源）。
                    let session_idx = SESSION_NEXT.fetch_add(1, Ordering::SeqCst);
                    let (control_tx, control_rx) = std::sync::mpsc::channel();
                    *CONTROL_TX.lock().unwrap() = Some(control_tx);
                    crate::macos_media::run_viewer(server, room, Some(token), weak2.clone(), epoch2.clone(), my_epoch, control_rx, session_idx);
                }
                #[cfg(not(target_os = "macos"))]
                {
                let auth = if token.is_empty() { None } else { Some(token.as_str()) };
                let out = aerodesk_core::connect::connect_viewer_auth(&server, &room, auth);
                let ui = weak2.clone();
                let Some(ui) = ui.upgrade() else { return };
                // 连接期间已断开/发起新会话：放弃进入会话视图。
                let stale = epoch2.load(Ordering::SeqCst) != my_epoch;
                match out {
                    Ok(r) if !stale => {
                        ui.set_status(format!("已连接：peer={} ice={}", r.peer_id, r.ice_connected).into());
                        ui.set_log(
                            format!(
                                "房间: {room}\n服务器: {server}\nSDP 交换: OK\nICE: {}\n\n已建立 WebRTC 会话（真实媒体/输入后续接入）。",
                                if r.ice_connected { "connected" } else { "pending(5s 超时)" }
                            )
                            .into(),
                        );
                        add_recent(&ui, &room, &server);
                        ui.set_conn_state(2);
                        // #23：进入会话视图 + 启动演示帧源（验证视频渲染管道）
                        ui.set_in_session(true);
                        ui.set_session_status("会话中 · 演示帧源（15fps）".into());
                        let frame_weak = weak2.clone();
                        std::thread::spawn(move || {
                            let mut t = 0u32;
                            while epoch2.load(Ordering::SeqCst) == my_epoch {
                                let Some(fui) = frame_weak.upgrade() else { break };
                                let px = demo_frame(t);
                                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&px, DEMO_W, DEMO_H);
                                fui.set_video_frame(slint::Image::from_rgba8(buffer));
                                t = t.wrapping_add(1);
                                std::thread::sleep(Duration::from_millis(66));
                            }
                        });
                    }
                    Ok(_) => { /* 连接完成但已断开：静默放弃，不进入会话 */ }
                    Err(e) => {
                        if !stale {
                            ui.set_conn_state(3);
                            ui.set_status(format!("连接失败：{e}").into());
                            ui.set_log(format!("失败原因：{e}").into());
                        }
                    }
                }
                } // cfg(not(target_os = "macos"))
                if let Some(ui) = weak2.upgrade() {
                    ui.set_connecting(false);
                }
            });
        }
    });

    ui.on_disconnect({
        let ui = ui.as_weak();
        let frame_epoch = frame_epoch.clone();
        move || {
            // 递增代际：停止当前会话帧线程（含连接中会话，使其放弃进入会话视图）。
            frame_epoch.fetch_add(1, Ordering::SeqCst);
            let ui = ui.unwrap();
            ui.set_conn_state(0);
            ui.set_input_mode("键鼠已释放".into());
            ui.set_in_session(false);
            ui.set_video_frame(slint::Image::default());
            ui.set_session_tabs(slint::ModelRc::new(slint::VecModel::from(Vec::<
                slint::SharedString,
            >::new(
            ))));
            ui.set_session_frames(slint::ModelRc::new(slint::VecModel::from(Vec::<
                slint::Image,
            >::new(
            ))));
            ui.set_active_session(0);
            SESSION_NEXT.store(0, Ordering::SeqCst);
            *CONTROL_TX.lock().unwrap() = None;
            ui.set_status("已断开".into());
            ui.set_connecting(false);
        }
    });

    ui.on_connect_recent({
        let weak = ui.as_weak();
        move |entry: slint::SharedString| {
            let ui = weak.unwrap();
            let (room, server) = parse_recent(entry.as_ref());
            ui.set_room_input(room.into());
            ui.set_server_input(server.into());
            ui.invoke_connect();
        }
    });

    // #29：UI → 会话 control 通道（选层请求）。
    static CONTROL_TX: std::sync::Mutex<Option<std::sync::mpsc::Sender<String>>> =
        std::sync::Mutex::new(None);
    // #29 多会话：会话槽序号（断开时清零）。
    static SESSION_NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    ui.on_switch_session({
        let ui = ui.as_weak();
        move |idx| {
            let ui = ui.unwrap();
            ui.set_active_session(idx);
            if let Some(frame) = ui.get_session_frames().row_data(idx as usize) {
                ui.set_video_frame(frame);
            }
            let name = ui
                .get_session_tabs()
                .row_data(idx as usize)
                .map(|r| r.to_string())
                .unwrap_or_default();
            ui.set_status(format!("已切换到会话 {name}").into());
        }
    });

    // ---- #23 会话工具栏 ----
    let fs_state = Arc::new(AtomicBool::new(false));
    ui.on_toggle_fullscreen({
        let ui = ui.as_weak();
        let fs_state = fs_state.clone();
        move || {
            let fs = !fs_state.fetch_xor(true, Ordering::SeqCst);
            let ui = ui.unwrap();
            ui.window().set_fullscreen(fs);
            ui.set_session_status(format!("全屏：{}", if fs { "开" } else { "关" }).into());
        }
    });
    ui.on_toggle_audio({
        let ui = ui.as_weak();
        move || {
            ui.unwrap()
                .set_session_status("音频：待接入（数据通道/媒体轨道）".into());
        }
    });
    ui.on_toggle_display({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            // 显示器切换：当前单显示器场景映射为选层切换（f↔h）。
            let cur = ui.get_input_mode().contains("捕获");
            let layer = if cur { "h" } else { "f" };
            if let Some(tx) = CONTROL_TX.lock().unwrap().as_ref() {
                let _ = tx.send(format!("{{\"layer\":\"{layer}\"}}"));
            }
            ui.set_session_status(format!("显示器切换：选层 {layer}（多显示器待接入）").into());
        }
    });
    ui.on_toggle_quality({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            let q = ui.get_quality();
            // 0=清晰(f) 1=平衡(h) 2=流畅(q)
            let layer = match q {
                0 => "f",
                1 => "h",
                _ => "q",
            };
            if let Some(tx) = CONTROL_TX.lock().unwrap().as_ref() {
                let _ = tx.send(format!("{{\"layer\":\"{layer}\"}}"));
            }
            ui.set_session_status(
                format!(
                    "画质：{}（SFU 选层 {layer}）",
                    match q {
                        0 => "清晰",
                        1 => "平衡",
                        _ => "流畅",
                    }
                )
                .into(),
            );
        }
    });
    ui.on_toggle_input({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            let captured = ui.get_input_mode().contains("捕获");
            ui.set_input_mode(if captured {
                "键鼠已释放".into()
            } else {
                "键鼠捕获中".into()
            });
            ui.set_session_status(if captured {
                "输入已释放".into()
            } else {
                "输入捕获中（Esc 可释放）".into()
            });
        }
    });

    // ---- #24 设置 ----
    ui.on_set_settings_tab({
        let ui = ui.as_weak();
        move |t| {
            ui.unwrap().set_settings_tab(t);
        }
    });
    ui.on_set_quality({
        let ui = ui.as_weak();
        move |q| {
            let ui = ui.unwrap();
            ui.set_quality(q);
            ui.set_settings_status(
                format!(
                    "质量：{}",
                    match q {
                        0 => "清晰 8Mbps",
                        1 => "平衡 4Mbps",
                        _ => "流畅 1.5Mbps",
                    }
                )
                .into(),
            );
        }
    });
    ui.on_save_settings({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            let settings = AppSettings {
                server_default: ui.get_server_default().to_string(),
                quality: ui.get_quality(),
                remember_token: ui.get_remember_token(),
                token_default: ui.get_token_default().to_string(),
                device_id: ui.get_device_id().to_string(),
                device_pw: ui.get_device_pw().to_string(),
            };
            save_settings(&settings);
            // 即时生效：同步主页输入框（无需重启）。
            ui.set_server_input(settings.server_default.clone().into());
            if settings.remember_token {
                ui.set_token_input(settings.token_default.clone().into());
            }
            ui.set_settings_status("已保存".into());
        }
    });

    // ---- #29 被控端授权流程 ----
    ui.on_refresh_perms({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            #[cfg(target_os = "macos")]
            {
                let (sc, ax) = (
                    aerodesk_macos::permissions::screen_capture_authorized(),
                    aerodesk_macos::permissions::accessibility_authorized(),
                );
                ui.set_perm_screen(if sc {
                    "已授权".into()
                } else {
                    "未授权".into()
                });
                ui.set_perm_a11y(if ax {
                    "已授权".into()
                } else {
                    "未授权".into()
                });
            }
            #[cfg(not(target_os = "macos"))]
            {
                ui.set_perm_screen("平台未实现".into());
                ui.set_perm_a11y("平台未实现".into());
            }
        }
    });
    ui.on_open_screen_perms({
        let ui = ui.as_weak();
        move || {
            #[cfg(target_os = "macos")]
            aerodesk_macos::permissions::open_system_settings(
                aerodesk_macos::permissions::SettingsPane::ScreenCapture,
            );
            #[cfg(not(target_os = "macos"))]
            if let Some(ui) = ui.upgrade() {
                ui.set_settings_status("被控端权限引导仅 macOS 实现".into());
            }
        }
    });
    ui.on_open_a11y_perms({
        let ui = ui.as_weak();
        move || {
            #[cfg(target_os = "macos")]
            aerodesk_macos::permissions::open_system_settings(
                aerodesk_macos::permissions::SettingsPane::Accessibility,
            );
            #[cfg(not(target_os = "macos"))]
            if let Some(ui) = ui.upgrade() {
                ui.set_settings_status("被控端权限引导仅 macOS 实现".into());
            }
        }
    });

    // 启动时刷一次权限状态
    ui.invoke_refresh_perms();

    ui.run()
}

/// 演示帧源：移动渐变（验证 Slint 视频渲染管道；真实解码后续接入）。
fn demo_frame(t: u32) -> Vec<u8> {
    let w = DEMO_W as usize;
    let h = DEMO_H as usize;
    let mut px = vec![0u8; w * h * 4];
    let bar = (t % 240) as usize;
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let band = x.wrapping_add(bar) % 240;
            px[i] = (band) as u8; // R
            px[i + 1] = (y % 256) as u8; // G
            px[i + 2] = 128; // B
            px[i + 3] = 255; // A
        }
    }
    px
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
        let path = recent_path();
        if std::fs::write(&path, json).is_ok() {
            set_private_perms(&path);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_frame_rgba() {
        let px = demo_frame(0);
        assert_eq!(px.len(), (DEMO_W * DEMO_H * 4) as usize);
        // alpha 全 255
        assert!(px[3] == 255 && px[px.len() - 1] == 255);
        // 不同帧内容不同（移动条）
        assert_ne!(demo_frame(0), demo_frame(120));
    }

    #[test]
    fn parse_recent_formats() {
        let (r, s) = parse_recent("demo · wss://x:3001/ws");
        assert_eq!(r, "demo");
        assert_eq!(s, "wss://x:3001/ws");
        let (r, s) = parse_recent("plain");
        assert_eq!(r, "plain");
        assert_eq!(s, "wss://signal.aerodesk.io");
    }
}

/// 应用设置（本地持久化）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AppSettings {
    server_default: String,
    quality: i32,
    remember_token: bool,
    token_default: String,
    /// 本机 ID（被控端身份，首启生成并持久化）。
    device_id: String,
    /// 本机接入密码（占位；发布端鉴权接入后使用）。
    device_pw: String,
}

fn settings_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".aerodesk-settings.json")
}

fn load_settings() -> AppSettings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_settings(s: &AppSettings) {
    if let Ok(json) = serde_json::to_string_pretty(s) {
        let path = settings_path();
        if std::fs::write(&path, json).is_ok() {
            set_private_perms(&path);
        }
    }
}

/// 生成本机 ID（AD- 前缀 + 6 位十六进制，基于时间+进程熵）。
fn default_device_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let n = (t ^ (pid << 32)) as u64;
    format!("AD-{:06X}", (n % 0xF4_23F) as u32)
}

/// 复制文本到系统剪贴板（macOS pbcopy；其他平台占位）。
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    #[cfg(target_os = "macos")]
    {
        if let Ok(mut child) = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            let _ = child.stdin.as_mut().map(|s| s.write_all(text.as_bytes()));
            let _ = child.wait();
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xclip")
            .arg("-selection")
            .arg("clipboard")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                c.stdin.as_mut().map(|s| s.write_all(text.as_bytes()));
                c.wait()
            });
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("clip")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                c.stdin.as_mut().map(|s| s.write_all(text.as_bytes()));
                c.wait()
            });
    }
}

/// 凭据/敏感文件权限收紧为 0600（#28 审查）。
fn set_private_perms(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}
