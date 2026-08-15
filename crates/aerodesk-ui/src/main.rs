//! AeroDesk UI 壳（Slint）：主页（连接区 + 最近会话）+ 会话视图（#23 初版）。
//!
//! 5 个原生平台（Win/macOS/Linux/Android/iOS）一套 UI；Web 走浏览器原生 WebRTC。

// #417 Windows：release 为窗口程序（非控制台应用）；debug 保留控制台便于看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();
#[cfg(not(target_os = "macos"))]
mod generic_media;
#[cfg(not(target_os = "macos"))]
mod generic_publisher;
mod generic_viewer;
mod keymap;
#[cfg(target_os = "macos")]
mod macos_media;
use slint::Model;

use aerodesk_core::platform::{AppShell, FilePicker, Permissions, Renderer};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
#[cfg(not(target_os = "macos"))]
use std::time::Duration;

const MAX_RECENTS: usize = 10;
const DEMO_W: u32 = 320;
const DEMO_H: u32 = 180;

/// #72 UI → 会话文件/剪贴板命令（经 mpsc 传到 run_viewer 线程）。
#[derive(Debug, PartialEq, Eq)]
pub enum FileCmd {
    /// 发送一个文件。
    SendFile(std::path::PathBuf),
    /// 把文本写入被控端剪贴板。
    SendClipboard(String),
    /// 把图片（PNG）写入被控端剪贴板（#271）。
    SendClipboardImage(Vec<u8>),
    /// 取消当前发送。
    Cancel,
}

/// #72 拖放发送纯路由（可单测）：把文件交给会话 file 通道，返回状态文案。
pub fn dispatch_dropped_files(
    tx: Option<&std::sync::mpsc::Sender<FileCmd>>,
    paths: &[std::path::PathBuf],
) -> String {
    if paths.is_empty() {
        return "发送文件：未选择文件".to_string();
    }
    let Some(tx) = tx else {
        return "发送文件：未连接会话".to_string();
    };
    // 只接受存在的普通文件（目录拖入不发送，避免误把目录当文件传）。
    let files: Vec<_> = paths.iter().filter(|p| p.is_file()).cloned().collect();
    if files.is_empty() {
        return format!("发送文件：{} 不是文件", paths[0].display());
    }
    // 当前一次只传一个文件（FileTransfer 单发送任务）：只发第一个，其余提示逐个发送。
    let _ = tx.send(FileCmd::SendFile(files[0].clone()));
    if files.len() == 1 {
        format!("发送文件：{}", files[0].display())
    } else {
        format!(
            "发送文件：{}（一次一个，其余 {} 个文件请等待完成后再发）",
            files[0].display(),
            files.len() - 1
        )
    }
}

/// 主控端视频区像素坐标 → 远端归一化坐标（0..1）。
///
/// 视频以 image-fit: contain 展示，主控/被控宽高比不同时有 letterbox 黑边；
/// 必须先扣掉黑边、按实际绘制区域归一化，否则点击/滚轮位置会偏移（偏移量
/// 等于黑边宽度）。与 app.slint cursor-pos 的绘制映射保持一致。
pub fn viewer_to_remote_norm(
    mx: f32,
    my: f32,
    area_w: f32,
    area_h: f32,
    frame_w: f32,
    frame_h: f32,
) -> (f32, f32) {
    if frame_w > 0.0 && frame_h > 0.0 && area_w > 0.0 && area_h > 0.0 {
        let scale = (area_w / frame_w).min(area_h / frame_h);
        let draw_w = frame_w * scale;
        let draw_h = frame_h * scale;
        let ox = (area_w - draw_w) / 2.0;
        let oy = (area_h - draw_h) / 2.0;
        let nx = (mx - ox) / draw_w;
        let ny = (my - oy) / draw_h;
        return (nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0));
    }
    // 无帧信息：退回按整个视频区归一化（与旧行为一致，避免除零）。
    let nx = if area_w > 0.0 { mx / area_w } else { 0.0 };
    let ny = if area_h > 0.0 { my / area_h } else { 0.0 };
    (nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0))
}

/// #29 多会话（主控端同时连接多个被控端，UI 标签最多 MAX_SESSIONS 个）：
/// - `SESSIONS` 按标签顺序保存活动会话（稠密，与 UI session-tabs/frames 对齐）
/// - 每会话独立 输入/控制/文件 通道、静音/音量、stop 标志；断开只关当前活动会话
pub const MAX_SESSIONS: usize = 4;

/// 会话最近一帧 RGBA（按稳定 slot 随会话保存：断开中间会话后帧仍归属原会话）。
#[derive(Clone)]
pub struct SessionFrame {
    pub rgba: Arc<Vec<u8>>,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone)]
pub struct SessionHandle {
    pub slot: usize,
    pub room: String,
    pub server: String,
    pub input_tx: std::sync::mpsc::Sender<String>,
    pub control_tx: std::sync::mpsc::Sender<String>,
    pub file_tx: std::sync::mpsc::Sender<FileCmd>,
    pub muted: Arc<AtomicBool>,
    pub volume: Arc<AtomicU16>,
    pub stop: Arc<AtomicBool>,
    /// 画面源切换：false=屏幕 / true=摄像头（观看端本地渲染选择）。
    pub show_camera: Arc<AtomicBool>,
    /// 最近一帧（未收到帧时为 None，UI 显示空槽）。
    pub frame: Option<SessionFrame>,
    /// 远端光标最新位置（None = 尚未收到光标事件）。
    pub cursor: Option<(f32, f32)>,
    /// 文件传输进度（-1 = 无传输；0..=1）。
    pub file_progress: f32,
    /// 文件传输标签（如“发送 x.zip 42%”）。
    pub file_label: String,
}

pub static SESSIONS: std::sync::Mutex<Vec<SessionHandle>> = std::sync::Mutex::new(Vec::new());
/// #75 输入帧序号（全局递增；跨会话共用与旧行为一致）。
pub static INPUT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// #29 会话槽序号（单调递增，作为会话内部标识；UI 稠密索引见 slot_to_ui_index）。
pub static SESSION_NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// 活动会话索引镜像（UI 线程维护；viewer 线程读此值判断是否同步 UI，避免跨线程升级）。
pub static ACTIVE_SESSION: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
/// 文件传输总开关镜像（viewer 线程读；跨线程无法升级 UI 属性）。
pub static FILE_TRANSFER_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);
/// 键鼠捕获开关镜像（on_toggle_input 写，输入转发回调读）：
/// 工具栏「输入」按钮控制——未捕获时鼠标/键盘/滚轮不转发被控端（F3）。
pub static INPUT_CAPTURING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// 会话相关测试共享锁：多会话 e2e 与无头 UI 状态测试都操作全局 SESSIONS，
/// 必须串行执行避免互相污染。
#[cfg(test)]
pub static SESSION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 设置活动会话索引（UI 线程调用）：同步 Slint 属性与全局镜像。
pub fn ui_set_active_session(ui: &AppWindow, idx: i32) {
    ACTIVE_SESSION.store(idx, Ordering::SeqCst);
    ui.set_active_session(idx);
}

/// macOS Dock 图标点击：主窗口弱引用（AppWindow::new 后设置，reopen 回调重显用）。
#[cfg(target_os = "macos")]
static MAIN_WINDOW: std::sync::Mutex<Option<slint::Weak<AppWindow>>> = std::sync::Mutex::new(None);

/// macOS：把主窗口带到最前（makeKeyAndOrderFront + deminiaturize + 激活 App）。
#[cfg(target_os = "macos")]
pub fn focus_window_to_front(window: &slint::Window) {
    use raw_window_handle::{HasRawWindowHandle, HasWindowHandle};
    if let Ok(handle) = window.window_handle().window_handle()
        && let Ok(raw) = handle.raw_window_handle()
        && let raw_window_handle::RawWindowHandle::AppKit(appkit) = raw
    {
        // #277 平台抽象：窗口聚焦走 core `AppShell` trait。
        aerodesk_macos::dock::MacAppShell
            .focus_view(appkit.ns_view.as_ptr() as *mut std::ffi::c_void);
    }
}

/// slot（会话内部标识）→ 当前 UI 稠密索引（SESSIONS 顺序即标签顺序）。
pub fn slot_to_ui_index(slot: usize) -> Option<usize> {
    SESSIONS.lock().unwrap().iter().position(|s| s.slot == slot)
}

/// 从任意线程更新 UI：Slint 1.17 的 `Weak::upgrade()` 仅在创建线程可用
/// （跨线程返回 None），viewer 线程必须经 `upgrade_in_event_loop` 排队到
/// UI 线程执行。闭包收到 UI 线程上的强句柄，可安全调用 setter。
pub fn with_ui<F>(ui_weak: &slint::Weak<AppWindow>, f: F)
where
    F: FnOnce(&AppWindow) + Send + 'static,
{
    let ui_weak = ui_weak.clone();
    let _ = ui_weak.upgrade_in_event_loop(move |ui| f(&ui));
}

/// 会话结束（跨线程版）：viewer 线程调用，排队到 UI 线程执行清理。
pub fn session_cleanup_weak(
    ui_weak: &slint::Weak<AppWindow>,
    slot: usize,
    terminal: Option<String>,
) {
    with_ui(ui_weak, move |ui| session_cleanup(ui, slot, terminal));
}

/// 会话成功连接（跨线程版）：viewer 线程调用。
pub fn session_joined_weak(ui_weak: &slint::Weak<AppWindow>, slot: usize) {
    with_ui(ui_weak, move |ui| session_joined(ui, slot));
}

/// 由 SESSIONS 顺序构建 (标签, 帧数组)。
/// 帧存在各会话句柄里（按稳定 slot 归属）：断开中间会话后剩余会话仍显示自己的帧。
pub fn build_tabs_frames(
    sessions: &[SessionHandle],
) -> (Vec<slint::SharedString>, Vec<slint::Image>) {
    let tabs = sessions.iter().map(|s| s.room.clone().into()).collect();
    let frames = sessions
        .iter()
        .map(|s| match &s.frame {
            Some(f) => {
                let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &f.rgba, f.w, f.h,
                );
                slint::Image::from_rgba8(buf)
            }
            None => slint::Image::default(),
        })
        .collect();
    (tabs, frames)
}

/// 把一帧 RGBA 呈现到会话帧槽 + 当前显示帧（多会话：按稳定 slot 映射稠密槽位）。
///
/// 线程安全：帧写入与模型重建都在 SESSIONS 锁内完成，避免多个 viewer 线程
/// 并发 get→set 模型导致丢帧/错位。
pub fn present_frame(
    ui_weak: &slint::Weak<AppWindow>,
    rgba: &[u8],
    w: usize,
    h: usize,
    slot: usize,
) {
    // 帧像素必须在 UI 线程外先拷贝进可 Send 的数据，再排队到 UI 线程：
    // Slint 的 Weak::upgrade() 跨线程返回 None，Image 也不可跨线程 Send。
    let rgba: Arc<Vec<u8>> = Arc::new(rgba.to_vec());
    let ui_weak = ui_weak.clone();
    let _ = ui_weak.upgrade_in_event_loop(move |fui| {
        // 读-改-写模型在 UI 线程 + SESSIONS 临界区内完成（多 viewer 线程
        // 的排队闭包在 UI 线程串行执行），保证帧归属与模型一致。
        let (ui_idx, frames) = {
            let mut sessions = SESSIONS.lock().unwrap();
            let Some(ui_idx) = sessions.iter().position(|s| s.slot == slot) else {
                return; // 会话已移除（断开清理中），跳过渲染
            };
            sessions[ui_idx].frame = Some(SessionFrame {
                rgba: rgba.clone(),
                w: w as u32,
                h: h as u32,
            });
            (ui_idx, build_tabs_frames(&sessions).1)
        };
        fui.set_session_frames(slint::ModelRc::new(slint::VecModel::from(frames.clone())));
        if fui.get_active_session() == ui_idx as i32 {
            // 帧尺寸只在活动会话时写全局（切换会话后由 sync_active_session_ui 恢复）。
            fui.set_frame_w(w as f32);
            fui.set_frame_h(h as f32);
            if let Some(f) = frames.get(ui_idx) {
                fui.set_video_frame(f.clone());
            }
        }
    });
}

/// 核心 `Renderer` trait 实现：Slint 渲染适配器（#277）。
/// 包装 `present_frame`，让观看端管线可以按 `Decoder + Renderer` 泛型驱动。
pub struct SlintRenderer {
    ui: slint::Weak<AppWindow>,
    slot: usize,
}

impl SlintRenderer {
    pub fn new(ui: slint::Weak<AppWindow>, slot: usize) -> Self {
        Self { ui, slot }
    }
}

impl Renderer for SlintRenderer {
    type Error = String;

    fn render(&mut self, frame: &aerodesk_core::platform::VideoFrame) -> Result<(), Self::Error> {
        let raw = frame.raw.as_deref().unwrap_or_default();
        present_frame(
            &self.ui,
            raw,
            frame.width as usize,
            frame.height as usize,
            self.slot,
        );
        Ok(())
    }
}

fn img_from_session_frame(s: &SessionHandle) -> slint::Image {
    match &s.frame {
        Some(f) => {
            let buf =
                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&f.rgba, f.w, f.h);
            slint::Image::from_rgba8(buf)
        }
        None => slint::Image::default(),
    }
}

/// 按 SESSIONS 重建 UI 标签/帧槽（会话加入或移除后调用；帧按 slot 归属）。
pub fn session_refresh_ui(ui: &AppWindow) {
    let (tabs, frames, is_empty) = {
        let sessions = SESSIONS.lock().unwrap();
        let (tabs, frames) = build_tabs_frames(&sessions);
        (tabs, frames, sessions.is_empty())
    };
    ui.set_session_tabs(slint::ModelRc::new(slint::VecModel::from(tabs)));
    ui.set_session_frames(slint::ModelRc::new(slint::VecModel::from(frames.clone())));
    if is_empty {
        ui_set_active_session(ui, 0);
        ui.set_in_session(false);
        ui.set_conn_state(0);
        ui.set_video_frame(slint::Image::default());
        // 会话全部结束：键鼠捕获状态随标签文案一并复位。
        INPUT_CAPTURING.store(false, Ordering::SeqCst);
        ui.set_input_mode("键鼠已释放".into());
        ui.set_remote_cursor_visible(false);
        ui.set_frame_w(0.0);
        ui.set_frame_h(0.0);
        ui.set_camera_active(false);
        ui.set_camera_available(false);
        ui.set_status("已断开".into());
        ui.set_connecting(false);
    } else {
        let cur = ui.get_active_session() as usize;
        let new_active = cur.min(frames.len() - 1);
        ui_set_active_session(ui, new_active as i32);
        if let Some(f) = frames.get(new_active) {
            ui.set_video_frame(f.clone());
        }
        ui.set_in_session(true);
        // 活动会话可能因加入/离开变化：同步音量/光标/帧尺寸/文件进度。
        sync_active_session_ui(ui);
    }
}

/// 会话成功连接：登记标签并把活动会话切到该会话。
pub fn session_joined(ui: &AppWindow, slot: usize) {
    session_refresh_ui(ui);
    if let Some(pos) = slot_to_ui_index(slot) {
        ui_set_active_session(ui, pos as i32);
        sync_active_session_ui(ui);
    }
}

/// 会话结束（断开/连接失败/连接关闭）：从注册表移除并刷新 UI（帧随会话丢弃）。
/// `terminal`：会话全部结束后要保留给用户的终态文案（如“连接失败：…”）；
/// 为 None 时显示默认“已断开”。
pub fn session_cleanup(ui: &AppWindow, slot: usize, terminal: Option<String>) {
    {
        let mut sessions = SESSIONS.lock().unwrap();
        sessions.retain(|s| s.slot != slot);
    }
    session_refresh_ui(ui);
    if let Some(msg) = terminal {
        if SESSIONS.lock().unwrap().is_empty() {
            ui.set_status(msg.into());
        }
    }
}

/// 把活动会话的 UI 状态（音量/光标/帧尺寸/文件进度）同步到全局属性。
/// 切换会话、会话加入/离开后调用，保证多会话之间不串状态。
pub fn sync_active_session_ui(ui: &AppWindow) {
    let idx = ui.get_active_session() as usize;
    let (vol, muted, camera_active, cursor, frame, fp, fl) = {
        let sessions = SESSIONS.lock().unwrap();
        let Some(s) = sessions.get(idx) else {
            return;
        };
        (
            s.volume.load(Ordering::SeqCst) as f32 / 100.0,
            s.muted.load(Ordering::SeqCst),
            s.show_camera.load(Ordering::SeqCst),
            s.cursor,
            s.frame.as_ref().map(|f| (f.w as f32, f.h as f32)),
            s.file_progress,
            s.file_label.clone(),
        )
    };
    ui.set_volume(vol);
    ui.set_audio_muted(muted);
    ui.set_camera_active(camera_active);
    match cursor {
        Some((x, y)) => {
            ui.set_remote_cursor_x(x);
            ui.set_remote_cursor_y(y);
            ui.set_remote_cursor_visible(true);
        }
        None => ui.set_remote_cursor_visible(false),
    }
    match frame {
        Some((w, h)) => {
            ui.set_frame_w(w);
            ui.set_frame_h(h);
        }
        None => {
            ui.set_frame_w(0.0);
            ui.set_frame_h(0.0);
        }
    }
    if fp < 0.0 {
        ui.set_file_progress(-1.0);
        ui.set_file_label("".into());
    } else {
        ui.set_file_progress(fp);
        ui.set_file_label(fl.into());
    }
}

/// 修改某会话的 UI 状态；仅当该会话是活动会话时同步到全局 UI。
/// `ui_weak` 供跨线程调用：同步动作排队到 UI 线程执行。
pub fn with_session_ui_state<F>(ui_weak: &slint::Weak<AppWindow>, slot: usize, f: F)
where
    F: FnOnce(&mut SessionHandle) + Send + 'static,
{
    let is_active = {
        let mut sessions = SESSIONS.lock().unwrap();
        let Some(idx) = sessions.iter().position(|s| s.slot == slot) else {
            return;
        };
        f(&mut sessions[idx]);
        ACTIVE_SESSION.load(Ordering::SeqCst) == idx as i32
    };
    if is_active {
        with_ui(ui_weak, |ui| sync_active_session_ui(ui));
    }
}

/// #72 UI 拖拽发送：在 winit 事件进入 Slint 前拦截外部文件拖放。
///
/// Slint 1.17 的 DropArea 只支持应用内 DragArea，外部文件拖放需后端支持；
/// 但 winit 0.30（macOS 已注册 NSDraggingDestination）会派发
/// `WindowEvent::DroppedFile`，而 Slint 1.17.1 的
/// `Backend::builder().with_custom_application_handler()` 允许在 Slint 处理前
/// 拦截任意 winit 窗口事件，因此无需等 Slint 上游即可实现外部拖放。
#[cfg(target_os = "macos")]
struct FileDropHandler {
    /// 会话视图弱引用（AppWindow::new 后填充），用于状态/高亮提示。
    ui: std::sync::Arc<std::sync::Mutex<Option<slint::Weak<AppWindow>>>>,
}

#[cfg(target_os = "macos")]
impl FileDropHandler {
    fn new(ui: std::sync::Arc<std::sync::Mutex<Option<slint::Weak<AppWindow>>>>) -> Self {
        Self { ui }
    }

    /// 处理 winit 窗口事件；返回是否继续交给 Slint 处理。
    fn handle_window_event(
        &self,
        event: &i_slint_backend_winit::winit::event::WindowEvent,
    ) -> i_slint_backend_winit::EventResult {
        use i_slint_backend_winit::winit::event::WindowEvent;
        match event {
            // macOS：每个文件一个 DroppedFile（winit 0.30 各平台一致，无批量变体）。
            WindowEvent::DroppedFile(path) => {
                let status = self.dispatch_drop(std::slice::from_ref(path));
                self.set_hover(false);
                self.set_status(&status);
                i_slint_backend_winit::EventResult::PreventDefault
            }
            WindowEvent::HoveredFile(_) => {
                self.set_hover(true);
                i_slint_backend_winit::EventResult::Propagate
            }
            WindowEvent::HoveredFileCancelled => {
                self.set_hover(false);
                i_slint_backend_winit::EventResult::Propagate
            }
            _ => i_slint_backend_winit::EventResult::Propagate,
        }
    }

    /// 把拖放路径派发到会话 file 通道（活动会话）；返回给用户的状态文案。
    fn dispatch_drop(&self, paths: &[std::path::PathBuf]) -> String {
        let Some(ui) = self.ui.lock().unwrap().as_ref().and_then(|w| w.upgrade()) else {
            return "发送文件：未连接会话".to_string();
        };
        // #72 文件传输开关：关闭时拒绝拖放发送。
        if !ui.get_file_transfer_enabled() {
            return "发送文件：文件传输已关闭".to_string();
        }
        let idx = ui.get_active_session() as usize;
        let tx = {
            let sessions = crate::SESSIONS.lock().unwrap();
            sessions.get(idx).map(|s| s.file_tx.clone())
        };
        dispatch_dropped_files(tx.as_ref(), paths)
    }

    fn set_status(&self, msg: &str) {
        if let Some(ui) = self.ui.lock().unwrap().as_ref().and_then(|w| w.upgrade()) {
            ui.set_session_status(msg.into());
        }
    }

    fn set_hover(&self, active: bool) {
        if let Some(ui) = self.ui.lock().unwrap().as_ref().and_then(|w| w.upgrade()) {
            ui.set_file_drop_hover(active);
        }
    }
}

#[cfg(target_os = "macos")]
impl i_slint_backend_winit::CustomApplicationHandler for FileDropHandler {
    fn window_event(
        &mut self,
        _event_loop: &i_slint_backend_winit::winit::event_loop::ActiveEventLoop,
        _window_id: i_slint_backend_winit::winit::window::WindowId,
        _winit_window: Option<&i_slint_backend_winit::winit::window::Window>,
        _slint_window: Option<&slint::Window>,
        event: &i_slint_backend_winit::winit::event::WindowEvent,
    ) -> i_slint_backend_winit::EventResult {
        self.handle_window_event(event)
    }
}

/// #277 平台文件选择器（发送文件用）：macOS 原生 / Linux zenity-kdialog。
fn pick_file() -> Result<Option<String>, String> {
    #[cfg(target_os = "macos")]
    {
        return aerodesk_macos::file_picker::MacFilePicker.pick_file();
    }
    #[cfg(target_os = "linux")]
    {
        return aerodesk_linux::file_picker::LinuxFilePicker.pick_file();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err("发送文件仅 macOS/Linux 支持".into())
    }
}

/// #277 后台线程选择文件并发送到被控端（选择器阻塞，避免卡 UI 事件循环）。
fn pick_file_and_send(ui: slint::Weak<AppWindow>) {
    std::thread::spawn(move || {
        let picked = pick_file();
        // Slint 1.17 Weak::upgrade() 跨线程恒 None：选择结果经 with_ui 排队回 UI 线程。
        with_ui(&ui, move |ui| match picked {
            Ok(Some(path)) => {
                let idx = ui.get_active_session() as usize;
                let sessions = SESSIONS.lock().unwrap();
                if let Some(s) = sessions.get(idx) {
                    let _ = s.file_tx.send(FileCmd::SendFile(path.clone().into()));
                    drop(sessions);
                    ui.set_session_status(format!("发送文件：{path}").into());
                } else {
                    drop(sessions);
                    ui.set_session_status("发送文件：未连接会话".into());
                }
            }
            Ok(None) => {
                ui.set_session_status("已取消选择文件".into());
            }
            Err(e) => {
                ui.set_session_status(format!("发送文件：无法打开文件选择器（{e}）").into());
            }
        });
    });
}

/// 「开启被控」开关接入：Windows 启动/停止屏幕发布线程；macOS 暂不实现。
fn handle_toggle_inc(ui: &AppWindow) {
    #[cfg(not(target_os = "macos"))]
    crate::generic_publisher::toggle_publisher(ui);
    #[cfg(target_os = "macos")]
    {
        let _ = ui;
    }
}

fn main() -> Result<(), slint::PlatformError> {
    init_log();
    let drop_ui: std::sync::Arc<std::sync::Mutex<Option<slint::Weak<AppWindow>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
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
            // #72 拖放发送：拦截 winit DroppedFile（backend 先于 AppWindow 创建，
            // 弱引用在 AppWindow::new 后填充）。
            .with_custom_application_handler(Box::new(FileDropHandler::new(drop_ui.clone())))
            .build()
            .expect("slint winit backend");
        slint::platform::set_platform(Box::new(backend)).expect("set slint platform");
    }
    let ui = AppWindow::new()?;
    // macOS：Dock 图标点击 → 重显/置前主窗口（含最小化还原）。
    #[cfg(target_os = "macos")]
    {
        *MAIN_WINDOW.lock().unwrap() = Some(ui.as_weak());
        // #277 平台抽象：Dock 重开回调走 core `AppShell` trait。
        aerodesk_macos::dock::MacAppShell.set_reopen_callback(Box::new(|| {
            let weak = MAIN_WINDOW.lock().unwrap().clone();
            if let Some(weak) = weak {
                let _ = weak.upgrade_in_event_loop(|ui| {
                    let _ = ui.show();
                    focus_window_to_front(ui.window());
                });
            }
        }));
    }
    // #72 拖放发送：填充会话 UI 弱引用（非 macOS 无 handler，仅写入无副作用）。
    *drop_ui.lock().unwrap() = Some(ui.as_weak());

    // 最近会话 / 收藏（本地持久化）
    ui.set_recents(slint::ModelRc::new(slint::VecModel::from(load_recents())));
    ui.set_favorites(slint::ModelRc::new(slint::VecModel::from(load_favorites())));
    ui.set_addressbook(slint::ModelRc::new(slint::VecModel::from(
        load_addressbook(),
    )));
    ui.set_device_groups(slint::ModelRc::new(slint::VecModel::from(
        build_device_groups(&load_addressbook()),
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
    // #417 开机自启状态回填（Windows HKCU Run；登录后自动启动并恢复被控）。
    #[cfg(target_os = "windows")]
    if let Ok(Some(_)) = aerodesk_windows::autostart::installed() {
        ui.set_auto_start(true);
    }
    ui.set_inc_audio(settings.inc_audio);
    ui.set_inc_mouse(settings.inc_mouse);
    ui.set_inc_view_only(settings.inc_view_only);
    ui.set_show_remote_cursor(settings.show_remote_cursor);
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
            let text = ui.get_device_id().to_string();
            // pbcopy/xclip/clip 会阻塞，放后台线程避免卡 UI 事件循环。
            std::thread::spawn(move || copy_to_clipboard(&text));
            ui.set_status("本机 ID 已复制".into());
        }
    });
    ui.on_copy_device_pw({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            let text = ui.get_device_pw().to_string();
            std::thread::spawn(move || copy_to_clipboard(&text));
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
            {
                let sessions = SESSIONS.lock().unwrap();
                if sessions.len() >= MAX_SESSIONS {
                    ui.set_status(format!("最多同时 {MAX_SESSIONS} 个会话（请先断开一个）").into());
                    return;
                }
            }
            ui.set_connecting(true);
            ui.set_conn_state(1);
            ui.set_status(format!("连接 {} @ {} …", room, server).into());
            // 每会话独立通道/状态（多会话互不干扰）：输入/控制/文件/静音/音量/stop。
            let slot = SESSION_NEXT.fetch_add(1, Ordering::SeqCst);
            let (control_tx, control_rx) = std::sync::mpsc::channel();
            let (input_tx, input_rx) = std::sync::mpsc::channel();
            let (file_cmd_tx, file_cmd_rx) = std::sync::mpsc::channel();
            let muted = Arc::new(AtomicBool::new(false));
            let volume = Arc::new(AtomicU16::new(100));
            let stop = Arc::new(AtomicBool::new(false));
            let show_camera = Arc::new(AtomicBool::new(false));
            {
                let mut sessions = SESSIONS.lock().unwrap();
                sessions.push(SessionHandle {
                    slot,
                    room: room.clone(),
                    server: server.clone(),
                    input_tx: input_tx.clone(),
                    control_tx: control_tx.clone(),
                    file_tx: file_cmd_tx.clone(),
                    muted: muted.clone(),
                    volume: volume.clone(),
                    stop: stop.clone(),
                    show_camera: show_camera.clone(),
                    frame: None,
                    cursor: None,
                    file_progress: -1.0,
                    file_label: String::new(),
                });
            }
            // 连接中的会话立即显示为标签：可看到“连接中”状态，也可直接断开取消。
            session_refresh_ui(&ui);
            let weak2 = weak.clone();
            // 数据通道收发链（str0m/SCTP）调用栈深，放大线程栈防溢出（RULE 数据通道大块传输线程栈需放大默认2MB.md）。
            std::thread::Builder::new()
                .stack_size(16 * 1024 * 1024)
                .spawn(move || {
                    #[cfg(target_os = "macos")]
                    {
                        // macOS：真实 H.264 解码渲染 + 音频/文件/多会话（每会话独立）。
                        crate::macos_media::run_viewer(
                            server,
                            room,
                            Some(token),
                            weak2.clone(),
                            slot,
                            control_rx,
                            input_rx,
                            file_cmd_rx,
                            muted,
                            volume,
                            show_camera,
                            stop,
                        );
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        // Windows/Linux 主控端：真实媒体观看（OpenH264 软解 + Slint 渲染）。
                        crate::generic_media::run_generic_viewer(
                            server,
                            room,
                            Some(token),
                            weak2.clone(),
                            slot,
                            input_rx,
                            file_cmd_rx,
                            stop,
                        );
                    }
                    // 跨线程 upgrade() 恒 None：经 with_ui 排队到 UI 线程复位连接中状态。
                    with_ui(&weak2, |ui| ui.set_connecting(false));
                })
                .expect("spawn viewer thread");
        }
    });

    ui.on_disconnect({
        let ui = ui.as_weak();
        move || {
            // 多会话：只断开当前活动会话（stop 置位后会话线程退出并刷新 UI）。
            let ui = ui.unwrap();
            let idx = ui.get_active_session() as usize;
            let stopped = {
                let sessions = SESSIONS.lock().unwrap();
                if let Some(s) = sessions.get(idx) {
                    s.stop.store(true, Ordering::SeqCst);
                    true
                } else {
                    false
                }
            };
            if stopped {
                ui.set_status("正在断开当前会话…".into());
                INPUT_CAPTURING.store(false, Ordering::SeqCst);
                ui.set_input_mode("键鼠已释放".into());
                ui.set_remote_cursor_visible(false);
            } else {
                // 无活动会话：直接重置 UI。
                session_refresh_ui(&ui);
            }
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

    // #29 多会话：输入/控制/文件/静音/音量全部按“活动会话”路由（SESSIONS[idx]）。
    // F3：键鼠未捕获时不转发（工具栏「输入」开关控制）；F4：转发实际按键（左/中/右）。
    ui.on_send_input({
        let weak = ui.as_weak();
        move |kind: i32,
              button: i32,
              mx: f32,
              my: f32,
              area_w: f32,
              area_h: f32,
              fw: f32,
              fh: f32| {
            if !INPUT_CAPTURING.load(Ordering::SeqCst) {
                return; // 键鼠已释放：本地操作不注入被控端
            }
            // 主控/被控宽高比不同时视频区有 letterbox：先扣黑边再归一化。
            let (x, y) = viewer_to_remote_norm(mx, my, area_w, area_h, fw, fh);
            // 按键：0=左键（默认，含 Move）1=中键 2=右键。
            let button = match button {
                1 => aerodesk_protocol::input::MouseButton::Middle,
                2 => aerodesk_protocol::input::MouseButton::Right,
                _ => aerodesk_protocol::input::MouseButton::Left,
            };
            let event = match kind {
                1 => aerodesk_protocol::input::InputEvent::MouseButton {
                    button,
                    state: aerodesk_protocol::input::ButtonState::Pressed,
                    x: x as f64,
                    y: y as f64,
                },
                2 => aerodesk_protocol::input::InputEvent::MouseButton {
                    button,
                    state: aerodesk_protocol::input::ButtonState::Released,
                    x: x as f64,
                    y: y as f64,
                },
                _ => aerodesk_protocol::input::InputEvent::MouseMove {
                    x: x as f64,
                    y: y as f64,
                },
            };
            let seq = INPUT_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
            let frame = aerodesk_protocol::input::InputFrame::new(seq, event);
            if let Ok(json) = serde_json::to_string(&frame) {
                let ui = weak.unwrap();
                let sessions = SESSIONS.lock().unwrap();
                let idx = ui.get_active_session() as usize;
                if let Some(s) = sessions.get(idx) {
                    let _ = s.input_tx.send(json);
                }
            }
        }
    });

    // #75：会话视图键盘输入 → InputFrame JSON → input 通道 → SFU → 被控端注入。
    // Slint 在 macOS 上把 Command 报告为 modifiers.control、Control 报告为
    // modifiers.meta（builtin_structs），此处映射回协议语义（meta=Command）。
    ui.on_send_key({
        // 返回是否已处理：未映射的键 reject，让本地 UI 继续处理。
        // F3：键鼠未捕获时不转发；捕获中 Esc 不转发、只用于释放键鼠。
        let weak = ui.as_weak();
        move |state: i32,
              text: slint::SharedString,
              ctrl: bool,
              shift: bool,
              alt: bool,
              meta: bool|
              -> bool {
            let Some(code) = keymap::key_code_for_text(text.as_str()) else {
                return false;
            };
            if !INPUT_CAPTURING.load(Ordering::SeqCst) {
                return false; // 键鼠已释放：按键不注入被控端，交回本地 UI
            }
            if code == "Escape" {
                // 捕获中 Esc = 释放键鼠（与工具栏「输入」按钮同一动作），不转发被控端。
                weak.unwrap().invoke_toggle_input();
                return true;
            }
            let state = if state == 0 {
                aerodesk_protocol::input::ButtonState::Pressed
            } else {
                aerodesk_protocol::input::ButtonState::Released
            };
            #[cfg(target_os = "macos")]
            let modifiers = aerodesk_protocol::input::Modifiers {
                ctrl: meta,
                shift,
                alt,
                meta: ctrl,
            };
            #[cfg(not(target_os = "macos"))]
            let modifiers = aerodesk_protocol::input::Modifiers {
                ctrl,
                shift,
                alt,
                meta,
            };
            let event = aerodesk_protocol::input::InputEvent::Key {
                code: code.to_string(),
                state,
                modifiers,
            };
            let seq = INPUT_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
            let frame = aerodesk_protocol::input::InputFrame::new(seq, event);
            if let Ok(json) = serde_json::to_string(&frame) {
                let ui = weak.unwrap();
                let sessions = SESSIONS.lock().unwrap();
                let idx = ui.get_active_session() as usize;
                if let Some(s) = sessions.get(idx) {
                    let _ = s.input_tx.send(json);
                }
            }
            true
        }
    });

    // #75：会话视图滚轮输入 → InputFrame JSON（归一化坐标 + 像素增量）。
    ui.on_send_wheel({
        let weak = ui.as_weak();
        move |mx: f32, my: f32, area_w: f32, area_h: f32, fw: f32, fh: f32, dx: f32, dy: f32| {
            if !INPUT_CAPTURING.load(Ordering::SeqCst) {
                return; // 键鼠已释放：滚轮不注入被控端
            }
            let (x, y) = viewer_to_remote_norm(mx, my, area_w, area_h, fw, fh);
            let event = aerodesk_protocol::input::InputEvent::Wheel {
                x: x as f64,
                y: y as f64,
                delta_x: dx as f64,
                delta_y: dy as f64,
            };
            let seq = INPUT_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
            let frame = aerodesk_protocol::input::InputFrame::new(seq, event);
            if let Ok(json) = serde_json::to_string(&frame) {
                let ui = weak.unwrap();
                let sessions = SESSIONS.lock().unwrap();
                let idx = ui.get_active_session() as usize;
                if let Some(s) = sessions.get(idx) {
                    let _ = s.input_tx.send(json);
                }
            }
        }
    });

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
            ui.set_device_groups(slint::ModelRc::new(slint::VecModel::from(
                build_device_groups(&new),
            )));
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
            ui.set_device_groups(slint::ModelRc::new(slint::VecModel::from(
                build_device_groups(&new),
            )));
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
                // 跨线程 upgrade() 恒 None：扫描结果经 with_ui 排队回 UI 线程。
                with_ui(&weak, move |ui| {
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
                });
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
            ui.set_device_groups(slint::ModelRc::new(slint::VecModel::from(
                build_device_groups(&new),
            )));
            save_addressbook(&new);
        }
    });

    ui.on_switch_session({
        let ui = ui.as_weak();
        move |idx| {
            let ui = ui.unwrap();
            ui_set_active_session(&ui, idx);
            if let Some(frame) = ui.get_session_frames().row_data(idx as usize) {
                ui.set_video_frame(frame);
            }
            // 切换会话：恢复该会话的音量/光标/帧尺寸/文件进度。
            sync_active_session_ui(&ui);
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
        let weak = ui.as_weak();
        // #58 观看端静音：经当前会话 control 通道下发真实静音指令（音频链路已接入，
        // SFU 转发 PCMU；静音后观看端丢弃音频帧）。
        move || {
            let ui = weak.unwrap();
            let idx = ui.get_active_session() as usize;
            let m = {
                let sessions = SESSIONS.lock().unwrap();
                let Some(s) = sessions.get(idx) else {
                    ui.set_session_status("没有活动会话".into());
                    return;
                };
                !s.muted.fetch_xor(true, Ordering::SeqCst)
            };
            // 本地静音只对当前会话生效（观看端丢帧）；不下发控制指令，
            // 避免把共享音频流的其它观看者一起静音（审查 #255 Important）。
            ui.set_audio_muted(m);
            ui.set_session_status(
                format!("音频：{}（仅本会话）", if m { "已静音" } else { "已开启" }).into(),
            );
        }
    });
    // 摄像头画面切换：当前会话渲染源 屏幕/摄像头（本地选择，不下发控制指令）。
    ui.on_toggle_camera({
        let weak = ui.as_weak();
        move || {
            let ui = weak.unwrap();
            let idx = ui.get_active_session() as usize;
            let m = {
                let sessions = SESSIONS.lock().unwrap();
                let Some(s) = sessions.get(idx) else {
                    ui.set_session_status("没有活动会话".into());
                    return;
                };
                !s.show_camera.fetch_xor(true, Ordering::SeqCst)
            };
            ui.set_camera_active(m);
            ui.set_session_status(
                format!("画面：{}（本会话）", if m { "摄像头" } else { "屏幕" }).into(),
            );
        }
    });
    // #73 观看端音量滑块：写当前会话 volume（run_viewer 同步到 AudioSink）。
    ui.on_change_volume({
        let weak = ui.as_weak();
        move |v: f32| {
            let ui = weak.unwrap();
            let pct = (v.clamp(0.0, 1.0) * 100.0).round() as u16;
            let idx = ui.get_active_session() as usize;
            {
                let sessions = SESSIONS.lock().unwrap();
                if let Some(s) = sessions.get(idx) {
                    s.volume.store(pct, Ordering::SeqCst);
                }
            }
            ui.set_session_status(format!("音量：{pct}%").into());
        }
    });
    // #109 AI 远控权限/审计管理（本机设置页）。
    ui.on_cmd_allowlist_refresh({
        let ui = ui.as_weak();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let items: Vec<slint::SharedString> = aerodesk_core::cmd_exec::allowlist()
                .into_iter()
                .map(Into::into)
                .collect();
            ui.set_cmd_allowlist(slint::ModelRc::new(slint::VecModel::from(items)));
        }
    });
    ui.on_cmd_allowlist_add({
        let ui = ui.as_weak();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let input = ui.get_cmd_allowlist_input().to_string();
            if input.trim().is_empty() {
                return;
            }
            match aerodesk_core::cmd_exec::add_allow_prefix(&input) {
                Ok(()) => {
                    ui.set_cmd_allowlist_input("".into());
                    ui.invoke_cmd_allowlist_refresh();
                    ui.set_settings_status(format!("已添加白名单：{input}").into());
                }
                Err(e) => ui.set_settings_status(format!("添加失败：{e}").into()),
            }
        }
    });
    ui.on_cmd_allowlist_remove({
        let ui = ui.as_weak();
        move |prefix: slint::SharedString| {
            let Some(ui) = ui.upgrade() else { return };
            match aerodesk_core::cmd_exec::remove_allow_prefix(prefix.as_str()) {
                Ok(()) => {
                    ui.invoke_cmd_allowlist_refresh();
                    ui.set_settings_status(format!("已移除白名单：{prefix}").into());
                }
                Err(e) => ui.set_settings_status(format!("移除失败：{e}").into()),
            }
        }
    });
    ui.on_cmd_audit_refresh({
        let ui = ui.as_weak();
        move || {
            let Some(ui) = ui.upgrade() else { return };
            let text = aerodesk_core::cmd_exec::tail_audit(30)
                .map(|lines| lines.join("\n"))
                .unwrap_or_else(|_| "（无审计记录或读取失败）".to_string());
            ui.set_cmd_audit(text.into());
        }
    });
    // 启动时预填设置页数据。
    ui.invoke_cmd_allowlist_refresh();
    ui.invoke_cmd_audit_refresh();

    ui.on_toggle_display({
        let ui = ui.as_weak();
        // #58 显示器切换：经 control 通道下发 {"display":N}（SFU 转发给被控端）。
        // 依次循环 0/1/2；被控端无对应显示器时保持当前并报错（日志可见）。
        let display_idx = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let display_idx2 = display_idx.clone();
        move || {
            let ui = ui.unwrap();
            let n = display_idx2.fetch_add(1, Ordering::SeqCst) % 3;
            let idx = ui.get_active_session() as usize;
            {
                let sessions = SESSIONS.lock().unwrap();
                if let Some(s) = sessions.get(idx) {
                    let _ = s.control_tx.send(format!("{{\"display\":{n}}}"));
                }
            }
            ui.set_session_status(format!("显示器：{n}（切换指令已下发）").into());
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
            let idx = ui.get_active_session() as usize;
            {
                let sessions = SESSIONS.lock().unwrap();
                if let Some(s) = sessions.get(idx) {
                    let _ = s.control_tx.send(format!("{{\"layer\":\"{layer}\"}}"));
                }
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
            // F3：状态落全局镜像，鼠标/键盘/滚轮转发回调按此门控；
            // 捕获中按 Esc 也走这里释放（on_send_key 转发 invoke_toggle_input）。
            INPUT_CAPTURING.store(!captured, Ordering::SeqCst);
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

    // ---- #72 文件/剪贴板 ----
    // 取消当前文件发送（进度条旁的取消按钮）。
    ui.on_cancel_file({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            let ok = {
                let sessions = SESSIONS.lock().unwrap();
                sessions
                    .get(ui.get_active_session() as usize)
                    .map(|s| s.file_tx.send(FileCmd::Cancel))
                    .is_some()
            };
            ui.set_session_status(if ok {
                "正在取消文件发送…".into()
            } else {
                "取消发送：未连接会话".into()
            });
        }
    });
    // 文件传输总开关：关闭后禁用发文件/剪贴板/拖放（接收端在 macos_media 暂停落盘）。
    ui.on_toggle_file_transfer({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            let on = !ui.get_file_transfer_enabled();
            ui.set_file_transfer_enabled(on);
            FILE_TRANSFER_ENABLED.store(on, Ordering::SeqCst);
            ui.set_session_status(if on {
                "文件传输：开".into()
            } else {
                "文件传输：关（发文件/剪贴板/拖放已禁用）".into()
            });
        }
    });
    // 发文件：平台文件选择器（macOS 原生 / Linux zenity-kdialog）→ file 通道 → 被控端。
    ui.on_send_file({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            if !ui.get_file_transfer_enabled() {
                ui.set_session_status("发送文件：文件传输已关闭".into());
                return;
            }
            pick_file_and_send(ui.as_weak());
        }
    });
    // 发送本地剪贴板文本到被控端（macOS pbpaste / Windows Get-Clipboard / Linux arboard；其他平台 no-op）。
    ui.on_send_clipboard({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            if !ui.get_file_transfer_enabled() {
                ui.set_session_status("剪贴板：文件传输已关闭".into());
                return;
            }
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            {
                // pbpaste/Get-Clipboard/arboard 会阻塞，放后台线程避免卡 UI 事件循环。
                let ui = ui.as_weak();
                std::thread::spawn(move || {
                    // #271：剪贴板有图片时优先发图片（PNG），否则发文本。
                    // 阻塞读取在本线程完成；UI 更新经 with_ui 排队回 UI 线程
                    //（跨线程 upgrade() 恒 None）。
                    if let Some(png) = aerodesk_core::clipboard::read_image() {
                        with_ui(&ui, move |ui| {
                            let idx = ui.get_active_session() as usize;
                            let sessions = SESSIONS.lock().unwrap();
                            if let Some(s) = sessions.get(idx) {
                                let _ = s.file_tx.send(FileCmd::SendClipboardImage(png));
                                drop(sessions);
                                ui.set_session_status("已发送剪贴板图片到被控端".into());
                            } else {
                                drop(sessions);
                                ui.set_session_status("剪贴板：未连接会话".into());
                            }
                        });
                        return;
                    }
                    match aerodesk_core::clipboard::read() {
                        Some(text) if !text.is_empty() => {
                            with_ui(&ui, move |ui| {
                                let idx = ui.get_active_session() as usize;
                                let sessions = SESSIONS.lock().unwrap();
                                if let Some(s) = sessions.get(idx) {
                                    let _ = s.file_tx.send(FileCmd::SendClipboard(text));
                                    drop(sessions);
                                    ui.set_session_status("已发送剪贴板到被控端".into());
                                } else {
                                    drop(sessions);
                                    ui.set_session_status("剪贴板：未连接会话".into());
                                }
                            });
                        }
                        _ => {
                            with_ui(&ui, |ui| {
                                ui.set_session_status("剪贴板为空".into());
                            });
                        }
                    }
                });
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            {
                ui.set_session_status("剪贴板：仅 macOS/Windows/Linux 支持".into());
            }
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
                show_remote_cursor: ui.get_show_remote_cursor(),
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

    // #417 开机自启开关（Windows HKCU Run）：登录后自动启动 UI 并恢复被控。
    ui.on_toggle_auto_start({
        let ui = ui.as_weak();
        move || {
            #[cfg(target_os = "windows")]
            if let Some(ui) = ui.upgrade() {
                let on = ui.get_auto_start();
                let exe = std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let res = if on {
                    aerodesk_windows::autostart::install(&format!("\"{exe}\""))
                } else {
                    aerodesk_windows::autostart::remove().map(|_| ())
                };
                match res {
                    Ok(()) => ui.set_settings_status(if on {
                        "已开启开机自启：登录后自动接受被控".into()
                    } else {
                        "已关闭开机自启".into()
                    }),
                    Err(e) => {
                        ui.set_auto_start(!on);
                        ui.set_settings_status(format!("开机自启设置失败：{e}").into());
                    }
                }
            }
            #[cfg(not(target_os = "windows"))]
            if let Some(ui) = ui.upgrade() {
                ui.set_settings_status("开机自启仅 Windows 实现".into());
            }
        }
    });
    // 「开启被控」开关：接入非 macOS（Windows）发布端；macOS 回调为 no-op。
    ui.on_toggle_inc({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                handle_toggle_inc(&ui);
            }
        }
    });

    // ---- #29 被控端授权流程 ----
    ui.on_refresh_perms({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            #[cfg(target_os = "macos")]
            {
                // #277 平台抽象：权限查询走 core `Permissions` trait。
                let p = aerodesk_macos::permissions::MacPermissions;
                let (sc, ax) = (p.screen_capture_authorized(), p.accessibility_authorized());
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
            #[cfg(target_os = "windows")]
            {
                // #417 Windows 被控授权：无 TCC 弹窗，交互会话即已授权；
                // 授权语义 = 用户显式开启「开启被控」开关。
                let p = aerodesk_windows::permissions::WindowsPermissions;
                let (sc, ax) = (p.screen_capture_authorized(), p.accessibility_authorized());
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
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
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
            {
                // 显式请求屏幕录制：CGRequestScreenCaptureAccess 会把本应用
                // 登记进「屏幕录制」授权列表（不在列表时打开设置窗口），
                // 后台线程避免阻塞 UI；随后再打开系统设置对应面板。
                // #277 平台抽象：权限请求/引导走 core `Permissions` trait。
                let p = aerodesk_macos::permissions::MacPermissions;
                std::thread::spawn(move || {
                    let _ = p.request_screen_capture();
                });
                let p2 = aerodesk_macos::permissions::MacPermissions;
                p2.open_screen_capture_settings();
            }
            #[cfg(target_os = "windows")]
            if let Some(ui) = ui.upgrade() {
                ui.set_settings_status("Windows 无系统权限弹窗：开启「开启被控」即授权".into());
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            if let Some(ui) = ui.upgrade() {
                ui.set_settings_status("被控端权限引导仅 macOS/Windows 实现".into());
            }
        }
    });
    ui.on_open_a11y_perms({
        let ui = ui.as_weak();
        move || {
            #[cfg(target_os = "macos")]
            aerodesk_macos::permissions::MacPermissions.open_accessibility_settings();
            #[cfg(target_os = "windows")]
            if let Some(ui) = ui.upgrade() {
                ui.set_settings_status("Windows 无系统权限弹窗：开启「开启被控」即授权".into());
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            if let Some(ui) = ui.upgrade() {
                ui.set_settings_status("被控端权限引导仅 macOS/Windows 实现".into());
            }
        }
    });

    // 启动时先尝试一次采集，把应用登记进「屏幕录制」授权列表
    // （macOS TCC 只列出尝试过受保护资源的应用，否则系统设置里看不到本应用）。
    // 放后台线程避免 SCShareableContent 首调阻塞首屏。
    #[cfg(target_os = "macos")]
    std::thread::spawn(|| {
        aerodesk_macos::permissions::MacPermissions.trigger_screen_capture_registration();
    });
    // 启动时刷一次权限状态
    ui.invoke_refresh_perms();

    // Windows：上次退出前已开启被控，则事件循环起来后恢复发布（开关持久化）。
    #[cfg(target_os = "windows")]
    if ui.get_inc_enabled() {
        let weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(800), move || {
            if let Some(ui) = weak.upgrade() {
                // #404 评审：触发前重读开关——用户在前 800ms 内关闭时不得再启动被控端，
                // 否则界面上开关为关而采集/连接/「已启动」提示照常，需再开关一次才能停。
                if ui.get_inc_enabled() {
                    crate::generic_publisher::start_publisher(&ui);
                }
            }
        });
    }

    // macOS：点击 Dock 图标恢复隐藏窗口（配合托盘隐藏）。
    #[cfg(target_os = "macos")]
    // #277 平台抽象：Dock 重开处理器走 core `AppShell` trait。
    aerodesk_macos::dock::MacAppShell.install_reopen_handler();

    // 系统托盘（Slint 1.17 SystemTrayIcon）：仅 macOS 创建；Linux/Windows 下
    // Slint 托盘创建失败会 abort（Xvfb/CI 无 StatusNotifier），故非 macOS 不创建。
    #[cfg(target_os = "macos")]
    let tray = Tray::new().ok();
    #[cfg(not(target_os = "macos"))]
    let tray: Option<Tray> = None;
    let win = ui.as_weak();
    if let Some(tray) = &tray {
        tray.on_show_window(move || {
            if let Some(ui) = win.upgrade() {
                let _ = ui.show();
                // “显示主窗口”：已打开时也要把窗口带到最前（含最小化还原）。
                #[cfg(target_os = "macos")]
                focus_window_to_front(ui.window());
            }
        });
        tray.on_quit_app(move || {
            std::process::exit(0);
        });
    }
    // 模拟器/CI 自测：-server/-room/-autoconnect 启动参数（无头/自动化驱动）。
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "-server") {
        if let Some(v) = args.get(i + 1) {
            ui.set_server_input(v.clone().into());
        }
    }
    if let Some(i) = args.iter().position(|a| a == "-room") {
        if let Some(v) = args.get(i + 1) {
            ui.set_room_input(v.clone().into());
        }
    }
    if args.iter().any(|a| a == "-autoconnect") {
        // 事件循环内触发连接（事件循环启动前 invoke 会因 UI 未就绪失败）。
        let auto_ui = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
            eprintln!("autoconnect: timer fired");
            if let Some(ui) = auto_ui.upgrade() {
                ui.invoke_connect();
                eprintln!("autoconnect: invoke_connect called");
            } else {
                eprintln!("autoconnect: ui weak expired");
            }
        });
    }
    ui.show()?;
    if let Some(tray) = &tray {
        if let Err(e) = tray.show() {
            eprintln!("system tray unavailable: {e:?}");
        }
    }
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

/// 最近会话格式：`设备 · 服务器`（解析用分隔符）。
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

/// 解析地址簿条目 `别名 · 设备 · 服务器 · 组`。
fn parse_addressbook(entry: &str) -> (String, String, String, String) {
    let parts: Vec<&str> = entry.splitn(4, " · ").collect();
    let name = parts.first().map(|s| s.to_string()).unwrap_or_default();
    let room = parts.get(1).map(|s| s.to_string()).unwrap_or_default();
    let server = parts.get(2).map(|s| s.to_string()).unwrap_or_default();
    let group = parts.get(3).map(|s| s.to_string()).unwrap_or_default();
    (name, room, server, group)
}

/// 按“分组”字段把地址簿聚合成设备组展示行：组标题行 + 组内设备行。
/// 未分组始终排最前，其余组按组名排序；组内保持地址簿原有顺序。
fn build_device_groups(items: &[slint::SharedString]) -> Vec<DeviceGroupEntry> {
    const UNGROUPED: &str = "未分组";
    // (组名, 组内原始条目)，保持首见顺序。
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for item in items {
        let (_, _, _, group) = parse_addressbook(item.as_str());
        let key = if group.is_empty() {
            UNGROUPED.to_string()
        } else {
            group
        };
        if let Some(g) = groups.iter_mut().find(|(k, _)| k == &key) {
            g.1.push(item.to_string());
        } else {
            groups.push((key, vec![item.to_string()]));
        }
    }
    groups.sort_by(|a, b| {
        let a_ug = a.0 == UNGROUPED;
        let b_ug = b.0 == UNGROUPED;
        match (a_ug, b_ug) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.cmp(&b.0),
        }
    });
    let mut rows = Vec::new();
    for (group, entries) in groups {
        rows.push(DeviceGroupEntry {
            is_header: true,
            text: group.into(),
            entry: Default::default(),
        });
        for entry in entries {
            let (name, _, _, _) = parse_addressbook(&entry);
            rows.push(DeviceGroupEntry {
                is_header: false,
                text: name.into(),
                entry: entry.into(),
            });
        }
    }
    rows
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
    fn device_groups_grouped_and_sorted() {
        let items: Vec<slint::SharedString> = vec![
            "NAS2 · demo · 10.0.0.2:3003 · 家庭".into(),
            "办公室A · demo · 10.0.0.3:3003 · 办公室".into(),
            "单机 · demo · 10.0.0.4:3003".into(),
            "NAS1 · demo · 10.0.0.1:3003 · 家庭".into(),
        ];
        let rows = build_device_groups(&items);
        // 未分组 -> 办公室 -> 家庭（组名排序）
        let headers: Vec<String> = rows
            .iter()
            .filter(|r| r.is_header)
            .map(|r| r.text.to_string())
            .collect();
        assert_eq!(headers, vec!["未分组", "办公室", "家庭"]);
        // 组内设备行按地址簿顺序，设备行 text=别名、entry=原始条目
        let group_rows: Vec<(&str, &str)> = rows
            .iter()
            .filter(|r| !r.is_header)
            .map(|r| (r.text.as_str(), r.entry.as_str()))
            .collect();
        assert_eq!(
            group_rows,
            vec![
                ("单机", "单机 · demo · 10.0.0.4:3003"),
                ("办公室A", "办公室A · demo · 10.0.0.3:3003 · 办公室"),
                ("NAS2", "NAS2 · demo · 10.0.0.2:3003 · 家庭"),
                ("NAS1", "NAS1 · demo · 10.0.0.1:3003 · 家庭"),
            ]
        );
    }

    #[test]
    fn device_groups_empty() {
        assert!(build_device_groups(&[]).is_empty());
        // 全部未分组 -> 单个“未分组”标题 + 设备行
        let items: Vec<slint::SharedString> = vec!["x · demo · h:3003".into()];
        let rows = build_device_groups(&items);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].is_header);
        assert_eq!(rows[0].text.to_string(), "未分组");
        assert!(!rows[1].is_header);
        assert_eq!(rows[1].text.to_string(), "x");
    }

    fn session_handle(slot: usize, room: &str) -> SessionHandle {
        let (input_tx, _) = std::sync::mpsc::channel();
        let (control_tx, _) = std::sync::mpsc::channel();
        let (file_tx, _) = std::sync::mpsc::channel();
        SessionHandle {
            slot,
            room: room.into(),
            server: "127.0.0.1:3003".into(),
            input_tx,
            control_tx,
            file_tx,
            muted: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU16::new(100)),
            stop: Arc::new(AtomicBool::new(false)),
            show_camera: Arc::new(AtomicBool::new(false)),
            frame: None,
            cursor: None,
            file_progress: -1.0,
            file_label: String::new(),
        }
    }

    fn frame_image(w: u32, h: u32) -> slint::Image {
        let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
        slint::Image::from_rgba8(buf)
    }

    fn with_frame(mut h: SessionHandle, rgba: Vec<u8>, w: u32, hh: u32) -> SessionHandle {
        h.frame = Some(SessionFrame {
            rgba: Arc::new(rgba),
            w,
            h: hh,
        });
        h
    }

    #[test]
    fn session_frames_keyed_by_slot_not_position() {
        let a = with_frame(session_handle(0, "A"), vec![0u8; 2 * 1 * 4], 2, 1);
        let b = with_frame(session_handle(1, "B"), vec![0u8; 3 * 1 * 4], 3, 1);

        // 双会话并存：帧按各自 slot 归属
        let (tabs, arr) = build_tabs_frames(&[a.clone(), b.clone()]);
        assert_eq!(tabs, vec![slint::SharedString::from("A"), "B".into()]);
        assert_eq!(arr[0].size().width, 2);
        assert_eq!(arr[1].size().width, 3);

        // 断开 A（首位会话）：剩余 B 必须显示 B 自己的帧，而不是 A 的旧帧
        let (tabs, arr) = build_tabs_frames(&[b.clone()]);
        assert_eq!(tabs, vec![slint::SharedString::from("B")]);
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].size().width, 3, "B 的帧被错位成了 A 的帧");

        // 断开 B：A 仍显示自己的帧
        let (tabs, arr) = build_tabs_frames(&[a.clone()]);
        assert_eq!(tabs, vec![slint::SharedString::from("A")]);
        assert_eq!(arr[0].size().width, 2);
    }

    #[test]
    fn session_frames_missing_slot_defaults_to_empty() {
        // 会话在册但尚未收到帧：帧槽为默认空图，不 panic
        let a = session_handle(0, "A");
        let (_, arr) = build_tabs_frames(&[a]);
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].size().width, 0);
    }

    /// UI 链路 e2e：真实 AppWindow（无头测试后端）下验证
    /// session_refresh_ui / sync_active_session_ui / session_cleanup / 帧按 slot 归属。
    #[test]
    fn ui_session_state_mapping_real_component() {
        // 与多会话 e2e 串行（都操作全局 SESSIONS）。
        let _guard = crate::SESSION_TEST_LOCK.lock().unwrap();
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();

        let mut a = session_handle(0, "A");
        a.volume.store(60, Ordering::SeqCst);
        a.cursor = Some((0.5, 0.5));
        a.frame = Some(SessionFrame {
            rgba: Arc::new(vec![0u8; 2 * 1 * 4]),
            w: 2,
            h: 1,
        });
        a.file_progress = 0.4;
        a.file_label = "发送 a.zip 40%".into();
        let mut b = session_handle(1, "B");
        b.volume.store(20, Ordering::SeqCst);
        b.cursor = Some((0.2, 0.3));
        b.frame = Some(SessionFrame {
            rgba: Arc::new(vec![0u8; 3 * 1 * 4]),
            w: 3,
            h: 1,
        });
        SESSIONS.lock().unwrap().push(a);
        SESSIONS.lock().unwrap().push(b);

        // 双会话并存：active=0 → UI 显示 A 的状态
        session_refresh_ui(&ui);
        assert_eq!(ui.get_active_session(), 0);
        assert_eq!(ui.get_session_tabs().row_count(), 2);
        assert_eq!(ui.get_volume(), 0.6);
        assert_eq!(ui.get_remote_cursor_x(), 0.5);
        assert_eq!(ui.get_remote_cursor_y(), 0.5);
        assert!(ui.get_remote_cursor_visible());
        assert_eq!(ui.get_frame_w(), 2.0);
        assert_eq!(ui.get_file_progress(), 0.4);
        assert_eq!(ui.get_file_label().as_str(), "发送 a.zip 40%");

        // 切到 B：音量/光标/帧尺寸/文件进度全部切到 B
        ui.set_active_session(1);
        sync_active_session_ui(&ui);
        assert_eq!(ui.get_volume(), 0.2);
        assert_eq!(ui.get_remote_cursor_x(), 0.2);
        assert_eq!(ui.get_remote_cursor_y(), 0.3);
        assert_eq!(ui.get_frame_w(), 3.0);
        assert_eq!(ui.get_frame_h(), 1.0);
        assert_eq!(ui.get_file_progress(), -1.0);
        assert_eq!(ui.get_file_label().as_str(), "");

        // 断开 A：只剩 B，active 修正为 0，UI 仍显示 B 的帧与状态
        session_cleanup(&ui, 0, None);
        assert_eq!(ui.get_session_tabs().row_count(), 1);
        assert_eq!(ui.get_session_tabs().row_data(0).unwrap().as_str(), "B");
        assert_eq!(ui.get_active_session(), 0);
        assert_eq!(ui.get_volume(), 0.2);
        assert_eq!(ui.get_frame_w(), 3.0);
        assert_eq!(ui.get_session_frames().row_data(0).unwrap().size().width, 3);
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

    fn assert_near(actual: (f32, f32), expect: (f32, f32), eps: f32) {
        assert!(
            (actual.0 - expect.0).abs() <= eps && (actual.1 - expect.1).abs() <= eps,
            "坐标换算不符：got {actual:?}, expect {expect:?}"
        );
    }

    #[test]
    fn viewer_norm_same_aspect_is_identity() {
        // 主控 1000x562.5（16:9）与远端 1920x1080 同比例：归一化 = 像素/面积
        assert_near(
            viewer_to_remote_norm(500.0, 281.25, 1000.0, 562.5, 1920.0, 1080.0),
            (0.5, 0.5),
            1e-4,
        );
        assert_near(
            viewer_to_remote_norm(0.0, 0.0, 1000.0, 562.5, 1920.0, 1080.0),
            (0.0, 0.0),
            1e-4,
        );
        assert_near(
            viewer_to_remote_norm(1000.0, 562.5, 1000.0, 562.5, 1920.0, 1080.0),
            (1.0, 1.0),
            1e-4,
        );
    }

    #[test]
    fn viewer_norm_letterbox_vertical_bars() {
        // 主控 1000x680、远端 1920x1080：上下黑边各 58.75，绘制区 1000x562.5
        assert_near(
            viewer_to_remote_norm(0.0, 58.75, 1000.0, 680.0, 1920.0, 1080.0),
            (0.0, 0.0),
            1e-3,
        );
        assert_near(
            viewer_to_remote_norm(1000.0, 680.0 - 58.75, 1000.0, 680.0, 1920.0, 1080.0),
            (1.0, 1.0),
            1e-3,
        );
        // 黑边内点击：y 夹到 0（x 保持 0.5）
        let (x, y) = viewer_to_remote_norm(500.0, 20.0, 1000.0, 680.0, 1920.0, 1080.0);
        assert!((x - 0.5).abs() <= 1e-3, "x 应保持 0.5，got {x}");
        assert!(y <= 1e-6, "黑边内 y 应夹到 0，got {y}");
    }

    #[test]
    fn viewer_norm_letterbox_horizontal_bars() {
        // 远端竖屏 1080x1920 放进横屏视频区 1000x680：左右黑边各 308.75
        assert_near(
            viewer_to_remote_norm(308.75, 340.0, 1000.0, 680.0, 1080.0, 1920.0),
            (0.0, 0.5),
            1e-3,
        );
        assert_near(
            viewer_to_remote_norm(1000.0 - 308.75, 340.0, 1000.0, 680.0, 1080.0, 1920.0),
            (1.0, 0.5),
            1e-3,
        );
    }

    #[test]
    fn viewer_norm_no_frame_falls_back_to_area() {
        // 无帧信息：退回按视频区归一化（与旧行为一致）
        assert_near(
            viewer_to_remote_norm(50.0, 25.0, 100.0, 50.0, 0.0, 0.0),
            (0.5, 0.5),
            1e-4,
        );
        // 无视频区也不 panic，坐标归零
        assert_near(
            viewer_to_remote_norm(10.0, 10.0, 0.0, 0.0, 0.0, 0.0),
            (0.0, 0.0),
            1e-6,
        );
    }

    #[test]
    fn viewer_norm_multi_resolution_switch() {
        // #75 多分辨率切换：同一个视频区，远端分辨率变化时，归一化中心/四角
        // 映射必须始终正确（比例不同走 letterbox 分支）。
        let area = (1000.0_f32, 680.0_f32);
        for (fw, fh) in [
            (1920.0_f32, 1080.0_f32), // 16:9（横屏）
            (2560.0_f32, 1440.0_f32), // 16:9 更高分辨率
            (1280.0_f32, 720.0_f32),  // 16:9 低分辨率
            (1080.0_f32, 1920.0_f32), // 竖屏（触发左右黑边）
        ] {
            let (x, y) = viewer_to_remote_norm(500.0, 340.0, area.0, area.1, fw, fh);
            assert!(
                (x - 0.5).abs() <= 1e-3 && (y - 0.5).abs() <= 1e-3,
                "center mismatch for {fw}x{fh}: ({x},{y})"
            );
            let (x0, y0) = viewer_to_remote_norm(0.0, 0.0, area.0, area.1, fw, fh);
            assert!(
                x0 <= 1e-6 && y0 <= 1e-6,
                "top-left clamp mismatch for {fw}x{fh}: ({x0},{y0})"
            );
            let (x1, y1) = viewer_to_remote_norm(1000.0, 680.0, area.0, area.1, fw, fh);
            assert!(
                (x1 - 1.0).abs() <= 1e-3 && (y1 - 1.0).abs() <= 1e-3,
                "bottom-right clamp mismatch for {fw}x{fh}: ({x1},{y1})"
            );
        }
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
    // ---- #72 拖放发送（macOS winit 拦截）----
    #[cfg(target_os = "macos")]
    fn drop_handler() -> FileDropHandler {
        FileDropHandler::new(std::sync::Arc::new(std::sync::Mutex::new(None)))
    }

    #[test]
    fn dropped_files_route_to_file_cmd() {
        let (tx, rx) = std::sync::mpsc::channel();
        let path = std::env::temp_dir().join("aerodesk-ui-drop-test.txt");
        std::fs::write(&path, b"hello").unwrap();
        let status = dispatch_dropped_files(Some(&tx), &[path.clone()]);
        assert!(status.contains("发送文件"), "status={status}");
        assert_eq!(rx.recv().unwrap(), FileCmd::SendFile(path.clone()));
        // 无会话 → 未连接会话
        assert!(dispatch_dropped_files(None, &[path.clone()]).contains("未连接会话"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dropped_directory_is_not_sent() {
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = std::env::temp_dir().join("aerodesk-ui-drop-dir-test");
        std::fs::create_dir_all(&dir).unwrap();
        let status = dispatch_dropped_files(Some(&tx), &[dir.clone()]);
        assert!(status.contains("不是文件"), "status={status}");
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn multiple_dropped_files_first_sent_rest_queued_notice() {
        // 一次一个：多文件拖放只发第一个，状态文案提示其余逐个发送。
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = std::env::temp_dir();
        let p1 = dir.join("aerodesk-ui-drop-batch-1.txt");
        let p2 = dir.join("aerodesk-ui-drop-batch-2.txt");
        std::fs::write(&p1, b"1").unwrap();
        std::fs::write(&p2, b"2").unwrap();
        let status = dispatch_dropped_files(Some(&tx), &[p1.clone(), p2.clone()]);
        assert!(status.contains("一次一个"), "status={status}");
        assert!(status.contains("其余 1 个文件"), "status={status}");
        let mut got = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            got.push(cmd);
        }
        assert_eq!(got, vec![FileCmd::SendFile(p1.clone())]);
        let _ = std::fs::remove_file(p1);
        let _ = std::fs::remove_file(p2);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hover_events_propagate() {
        use i_slint_backend_winit::winit::event::WindowEvent;
        let h = drop_handler();
        let r = h.handle_window_event(&WindowEvent::HoveredFile(std::path::PathBuf::from(
            "/tmp/x",
        )));
        assert!(matches!(r, i_slint_backend_winit::EventResult::Propagate));
        let r = h.handle_window_event(&WindowEvent::HoveredFileCancelled);
        assert!(matches!(r, i_slint_backend_winit::EventResult::Propagate));
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
    /// 观看端：是否显示远端光标叠加层（#75；默认关，对齐 RustDesk/TeamViewer
    /// 主流默认；蓝色半透明区别于真实鼠标）。
    #[serde(default)]
    show_remote_cursor: bool,
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

// ===================== 多会话端到端（真实 SFU/signal/publisher） =====================
// #29：主控端同时连接两个被控端 → 双会话并存、输入按活动会话路由、断开只关当前。
// macOS only：用本机已构建的 aerodesk-sfu/signal/cli 二进制；CI 未构建时自动 SKIP。
#[cfg(all(test, target_os = "macos"))]
mod multi_session_e2e {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    const SFU_INTERNAL: u16 = 15002;
    const SIGNAL_PLAIN: u16 = 15003;
    const ROOM_A: &str = "e2e-a";
    const ROOM_B: &str = "e2e-b";

    struct Procs {
        kids: Vec<Child>,
    }
    impl Procs {
        fn spawn(cmd: &mut Command, tag: &str) -> Option<Child> {
            match cmd
                .stdout(Stdio::from(
                    std::fs::File::create(format!("/tmp/mse2e-{}-{tag}.log", std::process::id()))
                        .unwrap(),
                ))
                .stderr(Stdio::from(
                    std::fs::File::create(format!(
                        "/tmp/mse2e-{}-{tag}.err.log",
                        std::process::id()
                    ))
                    .unwrap(),
                ))
                .spawn()
            {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("SKIP multi-session e2e: 无法启动 {tag}（{e}），二进制未构建？");
                    None
                }
            }
        }
    }
    impl Drop for Procs {
        fn drop(&mut self) {
            for k in &mut self.kids {
                let _ = k.kill();
                let _ = k.wait();
            }
        }
    }

    fn port_open(port: u16) -> bool {
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    }

    fn wait_port(port: u16, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if port_open(port) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }

    fn send_input_to_session(slot: usize, x: f64, y: f64) {
        let seq = INPUT_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
        let frame = aerodesk_protocol::input::InputFrame::new(
            seq,
            aerodesk_protocol::input::InputEvent::MouseMove { x, y },
        );
        let json = serde_json::to_string(&frame).unwrap();
        let sessions = SESSIONS.lock().unwrap();
        if let Some(s) = sessions.iter().find(|s| s.slot == slot) {
            let _ = s.input_tx.send(json);
        }
    }

    fn publisher_log_has(room: &str, needle: &str) -> bool {
        // CLI tracing 写 stderr（Procs::spawn 把 stderr 落 .err.log）。
        std::fs::read_to_string(format!(
            "/tmp/mse2e-{}-pub-{room}.err.log",
            std::process::id()
        ))
        .map(|t| t.contains(needle))
        .unwrap_or(false)
    }

    #[test]
    fn two_sessions_coexist_input_routes_and_disconnect_isolation() {
        assert!(multi_session_e2e_run());
    }

    /// 多会话端到端主体（返回是否通过；服务进程由 Procs::drop 回收）。
    fn multi_session_e2e_run() -> bool {
        // 0) 防并行污染：SESSIONS/平台是全局的（与无头 UI 状态测试共用锁）。
        let _guard = crate::SESSION_TEST_LOCK.lock().unwrap();
        let bin = format!("{}/../../target/debug", env!("CARGO_MANIFEST_DIR"));

        // 1) 起 SFU + signal（独立端口）
        let mut procs = Procs { kids: Vec::new() };
        let mut sfu_cmd = Command::new(format!("{bin}/aerodesk-sfu"));
        sfu_cmd
            .env("SFU_INTERNAL_PORT", SFU_INTERNAL.to_string())
            .env("SFU_MEDIA_PORT", "15478")
            .env("SFU_SIGNAL_PORT", "15000");
        let sfu = Procs::spawn(&mut sfu_cmd, "sfu");
        let mut sig_cmd = Command::new(format!("{bin}/aerodesk-signal"));
        sig_cmd
            .env("SIGNAL_PLAIN_PORT", SIGNAL_PLAIN.to_string())
            .env("SIGNAL_PORT", "15001") // WSS：独立端口避免与其它实例冲突
            .env("SFU_URL", format!("http://127.0.0.1:{SFU_INTERNAL}"));
        let sig = Procs::spawn(&mut sig_cmd, "sig");
        let (Some(sfu), Some(sig)) = (sfu, sig) else {
            return true; // 二进制未构建：SKIP（CI 未构建服务时跳过）
        };
        procs.kids.push(sfu);
        procs.kids.push(sig);
        assert!(wait_port(SFU_INTERNAL, 10), "SFU 未就绪");
        assert!(wait_port(SIGNAL_PLAIN, 10), "signal 未就绪");

        // 2) 两个被控端（发布端）
        for room in [ROOM_A, ROOM_B] {
            let mut cmd = Command::new(format!("{bin}/aerodesk-cli"));
            cmd.args([
                "--role",
                "publisher",
                "--encoder",
                "x264",
                "--noisy",
                "--signal",
                &format!("ws://127.0.0.1:{SIGNAL_PLAIN}"),
                "--room",
                room,
            ]);
            match Procs::spawn(&mut cmd, &format!("pub-{room}")) {
                Some(c) => procs.kids.push(c),
                None => {
                    eprintln!("SKIP multi-session e2e: 被控端 {room} 二进制未构建");
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(3));

        // 3) 真实网络双会话（不依赖 Slint 测试后端）：
        //    每个会话 = connect_live_role + 独立 input 通道 + pump 线程。
        let mut ice_flags: Vec<bool> = Vec::new();
        let mut stops: Vec<Arc<AtomicBool>> = Vec::new();
        let server = format!("127.0.0.1:{SIGNAL_PLAIN}");
        for room in [ROOM_A, ROOM_B] {
            let live = match aerodesk_core::connect::connect_live_role(
                &server,
                room,
                aerodesk_protocol::signal::Role::Viewer,
                None,
            ) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("MSE2E connect {room} failed: {e}");
                    return false;
                }
            };
            eprintln!("MSE2E connected {room} ice={}", live.ice_connected);
            ice_flags.push(live.ice_connected);
            let (input_tx, input_rx) = std::sync::mpsc::channel();
            let slot = SESSION_NEXT.fetch_add(1, Ordering::SeqCst);
            let stop = Arc::new(AtomicBool::new(false));
            SESSIONS.lock().unwrap().push(SessionHandle {
                slot,
                room: room.to_string(),
                server: server.clone(),
                input_tx: input_tx.clone(),
                control_tx: std::sync::mpsc::channel().0,
                file_tx: std::sync::mpsc::channel().0,
                muted: Arc::new(AtomicBool::new(false)),
                volume: Arc::new(AtomicU16::new(100)),
                stop: stop.clone(),
                show_camera: Arc::new(AtomicBool::new(false)),
                frame: None,
                cursor: None,
                file_progress: -1.0,
                file_label: String::new(),
            });
            let st = stop.clone();
            std::thread::spawn(move || {
                // 迷你收流泵：转发输入 + 推进 endpoint；stop 或连接关闭即退出。
                let mut live = live;
                let mut buf = [0u8; 4096];
                loop {
                    if st.load(Ordering::SeqCst) {
                        break;
                    }
                    while let Ok(json) = input_rx.try_recv() {
                        live.endpoint
                            .send_channel_data("input", false, json.as_bytes());
                    }
                    live.socket
                        .set_read_timeout(Some(Duration::from_millis(10)))
                        .ok();
                    if let Ok((n, source)) = live.socket.recv_from(&mut buf)
                        && let Ok(contents) = buf[..n].try_into()
                    {
                        let _ = live.endpoint.handle_input(str0m::Input::Receive(
                            std::time::Instant::now(),
                            str0m::net::Receive {
                                proto: str0m::net::Protocol::Udp,
                                source,
                                destination: live.socket.local_addr().unwrap(),
                                contents,
                            },
                        ));
                    }
                    let _ = live.endpoint.handle_timeout(std::time::Instant::now());
                    while let Some(output) = live.endpoint.poll_output() {
                        match output {
                            str0m::Output::Transmit(t) => {
                                let _ = live.socket.send_to(&t.contents, t.destination);
                            }
                            str0m::Output::Timeout(_) => break,
                            str0m::Output::Event(_) => {}
                        }
                    }
                    while let Some(ev) = live.endpoint.poll_event() {
                        if matches!(ev, aerodesk_core::endpoint::ClientEvent::Closed) {
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                // 退出（断开）：从注册表移除本会话。
                SESSIONS.lock().unwrap().retain(|s| s.slot != slot);
            });
            stops.push(stop);
        }

        // 4) 断言：双会话并存（SESSIONS=2，两个 ICE 都连通）
        let d = Instant::now() + Duration::from_secs(20);
        loop {
            let ice_ok = ice_flags.iter().all(|f| *f);
            let reg_ok = SESSIONS.lock().unwrap().len() == 2;
            if ice_ok && reg_ok {
                break;
            }
            if Instant::now() > d {
                eprintln!(
                    "MSE2E 双会话未就绪 ice={ice_flags:?} SESSIONS={}",
                    SESSIONS.lock().unwrap().len()
                );
                return false;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        eprintln!("MSE2E 双会话并存 OK");

        // 5) 输入路由：active=0 → 被控端 A；active=1 → 被控端 B。
        //    data channel 打开有时延，轮询重发直到被控端收到（模拟真实用户连续移动）。
        fn send_until(slot: usize, room: &str, x: f64, y: f64, expect_b: &str) -> bool {
            let d = Instant::now() + Duration::from_secs(12);
            while Instant::now() < d {
                if publisher_log_has(room, "input: seq=") {
                    return true;
                }
                send_input_to_session(slot, x, y);
                std::thread::sleep(Duration::from_millis(400));
            }
            false
        }
        if !send_until(0, ROOM_A, 0.5, 0.5, ROOM_B) {
            eprintln!("MSE2E 会话A输入未到达被控端A");
            return false;
        }
        if publisher_log_has(ROOM_B, "input: seq=") {
            eprintln!("MSE2E 会话A输入误发到被控端B");
            return false;
        }
        eprintln!("MSE2E 会话A输入到达被控端A（且未误发B）");
        if !send_until(1, ROOM_B, 0.6, 0.6, ROOM_A) {
            eprintln!("MSE2E 会话B输入未到达被控端B");
            return false;
        }
        eprintln!("MSE2E 输入按活动会话路由 OK");

        // 6) 断开隔离：停会话 A → SESSIONS 剩 1；B 仍可收输入
        stops[0].store(true, Ordering::SeqCst);
        let d = Instant::now() + Duration::from_secs(10);
        let mut removed = false;
        while Instant::now() < d {
            if SESSIONS.lock().unwrap().len() == 1 {
                removed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        if !removed {
            eprintln!(
                "MSE2E 断开会话A后未清理 SESSIONS={}",
                SESSIONS.lock().unwrap().len()
            );
            return false;
        }
        // B 仍可收输入（重新发送直到新增 input 计数）
        let d = Instant::now() + Duration::from_secs(12);
        let mut b_alive = false;
        let before_b = std::fs::read_to_string(format!(
            "/tmp/mse2e-{}-pub-{ROOM_B}.err.log",
            std::process::id()
        ))
        .map(|t| t.matches("input: seq=").count())
        .unwrap_or(0);
        while Instant::now() < d {
            let now_b = std::fs::read_to_string(format!(
                "/tmp/mse2e-{}-pub-{ROOM_B}.err.log",
                std::process::id()
            ))
            .map(|t| t.matches("input: seq=").count())
            .unwrap_or(0);
            if now_b > before_b {
                b_alive = true;
                break;
            }
            send_input_to_session(1, 0.7, 0.7);
            std::thread::sleep(Duration::from_millis(400));
        }
        if !b_alive {
            eprintln!("MSE2E 断开会话A后会话B输入中断");
            return false;
        }
        eprintln!("MSE2E 断开隔离 OK");

        // 清理
        stops[1].store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(500));
        true
    }
}
