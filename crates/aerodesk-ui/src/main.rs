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
    #[cfg(target_os = "macos")]
    {
        // winit WindowAttributes hook：标题栏透明 + 隐藏标题文字 + 内容铺满，
        // 保留原生红绿灯控制按钮（官方推荐方式）。
        use i_slint_backend_winit::Backend;
        use winit::platform::macos::WindowAttributesExtMacOS;
        let backend = Backend::builder()
            .with_window_attributes_hook(|attrs| {
                attrs
                    .with_titlebar_transparent(true)
                    .with_title_hidden(true)
                    .with_fullsize_content_view(true)
            })
            .build()
            .expect("slint winit backend");
        slint::platform::set_platform(Box::new(backend)).expect("set slint platform");
    }
    let ui = AppWindow::new()?;

    // 最近会话 / 收藏（本地持久化）
    ui.set_recents(slint::ModelRc::new(slint::VecModel::from(load_recents())));
    ui.set_favorites(slint::ModelRc::new(slint::VecModel::from(load_favorites())));
    ui.set_addressbook(slint::ModelRc::new(slint::VecModel::from(
        load_addressbook(),
    )));

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
    ui.set_pw_edit(settings.device_pw.clone().into());
    ui.set_inc_enabled(settings.inc_enabled);
    ui.set_inc_audio(settings.inc_audio);
    ui.set_inc_mouse(settings.inc_mouse);
    ui.set_inc_view_only(settings.inc_view_only);
    ui.set_quality(settings.quality);
    // 服务器地址 UI 上只展示 host:port（协议/路径在连接时由
    // aerodesk_core::signaling::normalize_signal_url 自动补全）。
    let server_display = display_server(&settings.server_default);
    ui.set_server_default(server_display.clone().into());
    ui.set_remember_token(settings.remember_token);
    ui.set_token_default(settings.token_default.clone().into());
    if !settings.server_default.is_empty() {
        ui.set_server_input(server_display.into());
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
    // 重新生成一次性密码：更新左栏显示 + 持久化。
    ui.on_refresh_device_pw({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            let pw = generate_one_time_password();
            ui.set_device_pw(pw.clone().into());
            // 同步设置页「安全」tab 的密码输入框，保证两处一致。
            ui.set_pw_edit(pw.clone().into());
            ui.set_status("一次性密码已刷新".into());
            let mut settings = load_settings();
            settings.device_pw = pw;
            save_settings(&settings);
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
            ui.set_server_input(display_server(&server).into());
            ui.invoke_connect();
        }
    });

    // #29：UI → 会话 control 通道（选层请求）。
    static CONTROL_TX: std::sync::Mutex<Option<std::sync::mpsc::Sender<String>>> =
        std::sync::Mutex::new(None);
    // #29 多会话：会话槽序号（断开时清零）。
    static SESSION_NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    // Peer 标签切换（#57）
    ui.on_set_peer_tab({
        let ui = ui.as_weak();
        move |t| {
            let ui = ui.unwrap();
            ui.set_peer_tab(t);
        }
    });

    // 收藏/取消收藏（#57）：`房间 · 服务器` 条目，持久化。
    ui.on_toggle_favorite({
        let ui = ui.as_weak();
        move |entry: slint::SharedString| {
            let ui = ui.unwrap();
            let model = ui.get_favorites();
            let mut items: Vec<String> = (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .map(|s| s.to_string())
                .collect();
            if items.iter().any(|i| i == entry.as_str()) {
                items.retain(|i| i != entry.as_str());
                ui.set_status("已取消收藏".into());
            } else {
                items.insert(0, entry.to_string());
                ui.set_status("已收藏".into());
            }
            let new: Vec<slint::SharedString> = items.iter().map(|s| s.into()).collect();
            ui.set_favorites(slint::ModelRc::new(slint::VecModel::from(new.clone())));
            save_favorites(&new);
        }
    });

    // 刷新 Peer 数据（#57）：重新加载最近会话与收藏。
    ui.on_refresh_peers({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            let recents: Vec<slint::SharedString> = load_recents();
            let favorites: Vec<slint::SharedString> = load_favorites();
            ui.set_recents(slint::ModelRc::new(slint::VecModel::from(recents.clone())));
            ui.set_favorites(slint::ModelRc::new(slint::VecModel::from(favorites)));
            ui.set_status(
                format!(
                    "已刷新：最近 {} 条 / 收藏 {} 条",
                    recents.len(),
                    ui.get_favorites().row_count()
                )
                .into(),
            );
        }
    });

    // #59 地址簿：添加（用当前连接信息 + 别名/分组）
    ui.on_add_addressbook({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            let alias = ui.get_ab_alias().to_string().trim().to_string();
            let group = ui.get_ab_group().to_string().trim().to_string();
            let room = ui.get_room_input().to_string();
            let server = ui.get_server_input().to_string();
            if room.is_empty() || server.is_empty() {
                ui.set_status("请先填写远端 ID 与信令服务器".into());
                return;
            }
            let alias = if alias.is_empty() {
                room.clone()
            } else {
                alias
            };
            let entry = format!("{alias} · {room} · {server} · {group}");
            let model = ui.get_addressbook();
            let mut items: Vec<String> = (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .map(|s| s.to_string())
                .collect();
            if !items.iter().any(|i| i == &entry) {
                items.push(entry.clone());
                ui.set_status("已添加到地址簿".into());
            } else {
                ui.set_status("地址簿已存在该条目".into());
            }
            let new: Vec<slint::SharedString> = items.iter().map(|s| s.into()).collect();
            ui.set_addressbook(slint::ModelRc::new(slint::VecModel::from(new.clone())));
            save_addressbook(&new);
        }
    });

    // #59 地址簿：删除
    ui.on_remove_addressbook({
        let ui = ui.as_weak();
        move |entry: slint::SharedString| {
            let ui = ui.unwrap();
            let model = ui.get_addressbook();
            let mut items: Vec<String> = (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .map(|s| s.to_string())
                .collect();
            items.retain(|i| i != entry.as_str());
            let new: Vec<slint::SharedString> = items.iter().map(|s| s.into()).collect();
            ui.set_addressbook(slint::ModelRc::new(slint::VecModel::from(new.clone())));
            save_addressbook(&new);
            ui.set_status("已从地址簿删除".into());
        }
    });

    // #59 地址簿/发现：点击连接（解析 别名·房间·服务器·组）
    ui.on_connect_addressbook({
        let ui = ui.as_weak();
        move |entry: slint::SharedString| {
            let ui = ui.unwrap();
            let (_, room, server, _) = parse_addressbook(entry.as_str());
            if room.is_empty() || server.is_empty() {
                ui.set_status("地址簿条目缺少房间/服务器".into());
                return;
            }
            ui.set_room_input(room.into());
            ui.set_server_input(server.into());
            ui.invoke_connect();
        }
    });

    // #59 局域网扫描：扫本网段 3003 端口（信令）
    ui.on_scan_lan({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            ui.set_status("扫描局域网…".into());
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                let found = scan_lan();
                if let Some(ui) = weak.upgrade() {
                    let model = ui.get_discovered();
                    let mut items: Vec<String> = (0..model.row_count())
                        .filter_map(|i| model.row_data(i))
                        .map(|s| s.to_string())
                        .collect();
                    for f in &found {
                        if !items.contains(f) {
                            items.push(f.clone());
                        }
                    }
                    let new: Vec<slint::SharedString> = items.iter().map(|s| s.into()).collect();
                    ui.set_discovered(slint::ModelRc::new(slint::VecModel::from(new.clone())));
                    ui.set_status(format!("扫描完成：发现 {} 台", found.len()).into());
                }
            });
        }
    });

    // #59 发现条目 -> 地址簿（房间固定 demo，服务器 = ip:3003）
    ui.on_add_discovered({
        let ui = ui.as_weak();
        move |entry: slint::SharedString| {
            let ui = ui.unwrap();
            let server = entry.to_string();
            let room = "demo".to_string();
            let alias = server.clone();
            let entry_str = format!("{alias} · {room} · {server} · 未分组");
            let model = ui.get_addressbook();
            let mut items: Vec<String> = (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .map(|s| s.to_string())
                .collect();
            if !items.iter().any(|i| i == &entry_str) {
                items.push(entry_str);
                ui.set_status("发现设备已加入地址簿".into());
            }
            let new: Vec<slint::SharedString> = items.iter().map(|s| s.into()).collect();
            ui.set_addressbook(slint::ModelRc::new(slint::VecModel::from(new.clone())));
            save_addressbook(&new);
        }
    });

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
        // #58 观看端静音：经 control 通道下发真实静音指令（音频链路已接入，
        // SFU 转发 PCMU；静音后观看端丢弃音频帧）。
        let muted = Arc::new(AtomicBool::new(false));
        let muted2 = muted.clone();
        move || {
            let ui = ui.unwrap();
            let m = !muted2.fetch_xor(true, Ordering::SeqCst);
            if let Some(tx) = CONTROL_TX.lock().unwrap().as_ref() {
                let _ = tx.send(format!("{{\"audio_mute\":{m}}}"));
            }
            ui.set_session_status(
                format!(
                    "音频：{}（静音指令已下发）",
                    if m { "已静音" } else { "已开启" }
                )
                .into(),
            );
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
            let ui = ui.unwrap();
            ui.set_settings_tab(t);
            // 进入「安全」tab 时，密码输入框同步为当前一次性密码。
            if t == 2 {
                ui.set_pw_edit(ui.get_device_pw().to_string().into());
            }
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
    // 自动保存：任一设置控件变化即持久化 + 即时生效（无「保存设置」按钮）。
    ui.on_auto_save({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            let mut device_pw = ui.get_device_pw().to_string();
            // 设置页安全 tab：本机接入密码非空则更新（清空表示不修改）。
            let pw_edit = ui.get_pw_edit().to_string();
            if !pw_edit.trim().is_empty() {
                device_pw = pw_edit.trim().to_string();
                ui.set_device_pw(device_pw.clone().into());
            }
            // server-default 与主页 server-input 已在 UI 层双向同步。
            let server_default = display_server(&ui.get_server_default().to_string());
            let settings = AppSettings {
                server_default: server_default.clone(),
                quality: ui.get_quality(),
                remember_token: ui.get_remember_token(),
                token_default: ui.get_token_default().to_string(),
                device_id: ui.get_device_id().to_string(),
                device_pw,
                inc_enabled: ui.get_inc_enabled(),
                inc_audio: ui.get_inc_audio(),
                inc_mouse: ui.get_inc_mouse(),
                inc_view_only: ui.get_inc_view_only(),
            };
            save_settings(&settings);
            // 即时生效：同步主页输入框（无需重启）。
            ui.set_server_input(server_default.into());
            if settings.remember_token {
                ui.set_token_input(settings.token_default.clone().into());
            }
            ui.set_settings_status("已自动保存".into());
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

    // macOS：点击 Dock 图标恢复隐藏窗口（配合托盘隐藏）。
    #[cfg(target_os = "macos")]
    aerodesk_macos::dock::install_reopen_handler();

    // 系统托盘（Slint 1.17 SystemTrayIcon）
    let tray = Tray::new()?;
    let win = ui.as_weak();
    tray.on_show_window(move || {
        if let Some(ui) = win.upgrade() {
            let _ = ui.show();
        }
    });
    tray.on_quit_app(move || {
        std::process::exit(0);
    });
    ui.show()?;
    tray.show()?;
    slint::run_event_loop()
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

/// UI 展示用服务器地址：去掉 ws:// / wss:// 协议前缀和 /ws 路径，只留 host:port。
fn display_server(input: &str) -> String {
    let s = input.trim();
    let s = s
        .strip_prefix("wss://")
        .or_else(|| s.strip_prefix("ws://"))
        .unwrap_or(s);
    s.strip_suffix("/ws").unwrap_or(s).to_string()
}

/// 最近会话格式：`房间 · 服务器`（解析用分隔符）。
fn parse_recent(entry: &str) -> (String, String) {
    match entry.split_once(" · ") {
        Some((r, s)) => (r.to_string(), s.to_string()),
        None => (entry.to_string(), "signal.aerodesk.io".to_string()),
    }
}

fn favorites_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".aerodesk-favorites.json")
}

fn load_favorites() -> Vec<slint::SharedString> {
    let Ok(text) = std::fs::read_to_string(favorites_path()) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .unwrap_or_default()
        .into_iter()
        .map(Into::into)
        .collect()
}

fn save_favorites(items: &[slint::SharedString]) {
    let v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
    if let Ok(json) = serde_json::to_string(&v) {
        let path = favorites_path();
        if std::fs::write(&path, json).is_ok() {
            set_private_perms(&path);
        }
    }
}

fn addressbook_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".aerodesk-addressbook.json")
}

fn load_addressbook() -> Vec<slint::SharedString> {
    let Ok(text) = std::fs::read_to_string(addressbook_path()) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .unwrap_or_default()
        .into_iter()
        .map(Into::into)
        .collect()
}

fn save_addressbook(items: &[slint::SharedString]) {
    let v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
    if let Ok(json) = serde_json::to_string(&v) {
        let path = addressbook_path();
        if std::fs::write(&path, json).is_ok() {
            set_private_perms(&path);
        }
    }
}

/// 解析地址簿条目 `别名 · 房间 · 服务器 · 组`。
fn parse_addressbook(entry: &str) -> (String, String, String, String) {
    let parts: Vec<&str> = entry.splitn(4, " · ").collect();
    let name = parts.first().map(|s| s.to_string()).unwrap_or_default();
    let room = parts.get(1).map(|s| s.to_string()).unwrap_or_default();
    let server = parts.get(2).map(|s| s.to_string()).unwrap_or_default();
    let group = parts.get(3).map(|s| s.to_string()).unwrap_or_default();
    (name, room, server, group)
}

/// 局域网扫描：取本机 IPv4，扫同 /24 网段的信令端口（默认 3003）。
fn scan_lan() -> Vec<String> {
    use std::net::{TcpStream, UdpSocket};
    use std::time::Duration;

    // 通过 UDP 连接获取本机 IP（不发包）。
    let local_ip = match UdpSocket::bind("0.0.0.0:0").and_then(|s| {
        s.connect("8.8.8.8:80")?;
        Ok(s.local_addr()?.ip())
    }) {
        Ok(ip) => ip,
        Err(_) => return Vec::new(),
    };
    let octets = match local_ip {
        std::net::IpAddr::V4(v4) => v4.octets(),
        _ => return Vec::new(),
    };
    let mut found = Vec::new();
    let port = 3003u16;
    for last in 1..255u8 {
        let ip = format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], last);
        let addr = format!("{ip}:{port}");
        let Ok(mut stream) = TcpStream::connect_timeout(
            &addr
                .parse()
                .unwrap_or_else(|_| "127.0.0.1:3003".parse().unwrap()),
            Duration::from_millis(60),
        ) else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
        found.push(addr);
        if found.len() >= 20 {
            break;
        }
    }
    found
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
    fn one_time_password_is_8_chars_from_safe_alphabet() {
        for _ in 0..100 {
            let pw = generate_one_time_password();
            assert_eq!(pw.len(), 8);
            assert!(pw.chars().all(|c| c.is_ascii_alphanumeric()));
            assert!(
                !pw.chars().any(|c| matches!(c, '0' | 'O' | '1' | 'I' | 'l')),
                "password contains confusing char: {pw}"
            );
        }
        // 连续两次不应相同（CSPRNG）。
        assert_ne!(generate_one_time_password(), generate_one_time_password());
    }

    #[test]
    fn parse_addressbook_entry() {
        // 完整格式：别名 · 房间 · 服务器 · 组
        let (name, room, server, group) =
            parse_addressbook("我的NAS · demo · 192.168.1.10:3003 · 家庭");
        assert_eq!(name, "我的NAS");
        assert_eq!(room, "demo");
        assert_eq!(server, "192.168.1.10:3003");
        assert_eq!(group, "家庭");
        // 缺分组
        let (name, room, server, group) = parse_addressbook("x · demo · h:3003");
        assert_eq!(name, "x");
        assert_eq!(room, "demo");
        assert_eq!(server, "h:3003");
        assert_eq!(group, "");
        // 空/乱输入不 panic
        let (name, room, server, group) = parse_addressbook("");
        assert!(name.is_empty() && room.is_empty() && server.is_empty() && group.is_empty());
    }

    #[test]
    fn addressbook_roundtrip() {
        // 构造条目 -> save -> load -> 一致
        let entry: slint::SharedString = "NAS · demo · 192.168.1.10:3003 · 家庭".into();
        let items = vec![entry.clone()];
        let path = std::env::temp_dir().join(format!("ad-ab-test-{}.json", std::process::id()));
        // 用临时文件验证序列化
        let v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        let json = serde_json::to_string(&v).unwrap();
        std::fs::write(&path, &json).unwrap();
        let loaded: Vec<String> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded, vec!["NAS · demo · 192.168.1.10:3003 · 家庭"]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parse_recent_formats() {
        let (r, s) = parse_recent("demo · 127.0.0.1:3003");
        assert_eq!(r, "demo");
        assert_eq!(s, "127.0.0.1:3003");
        let (r, s) = parse_recent("plain");
        assert_eq!(r, "plain");
        assert_eq!(s, "signal.aerodesk.io");
        // 兼容旧数据：历史记录可能带协议/路径，展示层应剥掉。
        assert_eq!(
            display_server("wss://signal.aerodesk.io/ws"),
            "signal.aerodesk.io"
        );
        assert_eq!(display_server("ws://127.0.0.1:3003"), "127.0.0.1:3003");
        assert_eq!(display_server("signal.aerodesk.io"), "signal.aerodesk.io");
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
    /// 本机接入密码（被控端一次性密码）。
    device_pw: String,
    /// 被控端：是否开启被控。
    #[serde(default)]
    inc_enabled: bool,
    /// 被控端：是否允许声音。
    #[serde(default = "default_true")]
    inc_audio: bool,
    /// 被控端：是否允许鼠标控制。
    #[serde(default = "default_true")]
    inc_mouse: bool,
    /// 被控端：仅观看（只读）。
    #[serde(default)]
    inc_view_only: bool,
}

fn default_true() -> bool {
    true
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

/// 生成随机一次性密码（8 位，去除易混淆字符 0/O/1/I/l）。
///
/// 使用系统 CSPRNG（`getrandom`）：时间/进程状态可预测的伪随机（如 xorshift）
/// 会让攻击者拿到一个历史密码后暴力搜种子预测后续密码，不能用于访问口令。
fn generate_one_time_password() -> String {
    const CHARS: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz";
    // 拒绝采样：只接受 0..216（= 54*4）的字节，避免取模偏差。
    const ACCEPT: usize = CHARS.len() * 4;
    let mut buf = [0u8; 8];
    let mut out = String::with_capacity(8);
    loop {
        getrandom::getrandom(&mut buf).expect("OS random source available");
        for &b in &buf {
            let idx = b as usize;
            if idx < ACCEPT {
                out.push(CHARS[idx % CHARS.len()] as char);
                if out.len() == 8 {
                    return out;
                }
            }
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
