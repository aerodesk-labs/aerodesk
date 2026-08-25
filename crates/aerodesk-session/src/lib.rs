//! aerodesk-session —— 会话编排层（#508 B1 / ADR-0009）。
//!
//! 从 aerodesk-desktop 抽出的 UI 无关会话引擎：主控 viewer（generic_viewer /
//! generic_media / macos_media）与被控 publisher（generic_publisher /
//! macos_publisher），外加纯键位逻辑 keymap。UI 副作用全部经 [`SessionUi`]
//! trait 与 [`PublisherEvent`] 回调回传，desktop 用 Slint 适配器实现，
//! host（B3/B4）将用同一接口接入。

pub mod clipboard_sync;
pub mod generic_publisher;
pub mod generic_viewer;
pub mod keymap;

// #553 验收前置：macOS 观看端已并入统一 SIP 路径（#578/#580）——generic_media
// 的 macOS 门控随 desktop 侧 cfg 移除而拆除（原两侧 cfg 互为"配合"：desktop
// 不看 macOS、session 不给 macOS 提供泛型观看端，遗留断链）。
pub mod generic_media;
#[cfg(target_os = "macos")]
pub mod macos_media;
#[cfg(target_os = "macos")]
pub mod macos_publisher;
//#[cfg(target_os = "macos")]
//pub mod macos_media;
//#[cfg(target_os = "macos")]
//pub mod macos_publisher;

/// #72 UI → 会话文件/剪贴板命令（经 mpsc 传到会话线程）。
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

/// #458 UI → 会话聊天命令（经 mpsc 传到会话线程）。
#[derive(Debug, PartialEq, Eq)]
pub enum ChatCmd {
    /// 发送一条文本消息。
    Send(String),
}

/// 当前墙钟（unix 毫秒），用于聊天消息 timestamp_ms。
pub fn system_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 会话层 → UI 的事件缝（#508 B1）：desktop 以 `Weak<AppWindow> + slot` 实现，
/// 全部方法可从会话线程调用（实现方负责排队到 UI 线程）。
///
/// 方法语义与 desktop 既有 UI 写入一一对应（行为零变化）；仅 macOS 路径使用的
/// 方法带默认空实现（参数以下划线前缀占位），非 macOS 实现方无需关心。
pub trait SessionUi: Send {
    /// 主窗口状态条。
    fn set_status(&self, msg: String);
    /// 连接状态机（0=idle 1=connecting 2=connected 3=failed）。
    fn set_conn_state(&self, state: i32);
    /// 连接日志区（设备/服务器/SDP/ICE 明细）。
    fn set_log(&self, msg: String);
    /// 会话状态：主窗口 session_status + 会话独立窗口文案。
    fn session_status(&self, msg: String);
    /// 首帧后登记：标签入库 + 切活动会话。
    fn joined(&self);
    /// 会话结束清理（注销槽位、关功能窗口；`terminal` 为终态文案）。
    fn cleanup(&self, terminal: Option<String>);
    /// 远端光标位置（归一化 0..1）。
    fn set_remote_cursor(&self, x: f32, y: f32);
    /// 会话延时统计（端到端单向延时 ms / 网络 RTT ms / 接收帧率；
    /// 各值 None 表示该口径尚未测得，调用方节流推送）。
    fn set_session_stats(&self, _latency_ms: Option<u64>, _rtt_ms: Option<u64>, _fps: f32) {}
    /// 插入最近会话记录。
    fn add_recent(&self, room: &str, server: &str);
    /// 终端独立窗口追加输出。
    fn append_terminal_output(&self, text: String);
    /// 聊天消息入库并回显（own=本端发出）。
    fn append_chat_message(&self, sender: String, text: String, own: bool);
    /// 聊天窗口状态文案（无窗口时 no-op）。
    fn set_message_window_status(&self, status: String);
    /// 文件传输独立窗口进度。
    fn update_file_window_progress(&self, progress: f32, label: String, status: String);
    /// 清除文件传输独立窗口进度。
    fn clear_file_window_progress(&self, status: Option<String>);

    // ---- 仅 macOS 路径使用（非 macOS 实现方可保持默认空实现）----
    /// 仅主窗口 session_status（macOS 文件/剪贴板/统计文案）。
    fn main_session_status(&self, _msg: String) {}
    /// 会话句柄上的文件进度投影（macOS 会话窗口进度条）。
    fn set_file_progress(&self, _progress: f32, _label: String) {}
    /// 远端发布了摄像头轨（macOS 摄像头切换按钮显隐）。
    fn set_camera_available(&self, _available: bool) {}
}

/// 被控端启动配置快照（UI 属性在调用点读取，引擎不再回读 UI）。
#[derive(Debug, Clone)]
pub struct PublisherConfig {
    /// 信令服务器地址（调用点已按 TLS 开关归一化，#513 B1；显式协议输入原样透传）。
    pub server: String,
    /// 发布房间（本机设备 ID；引擎侧经 `valid_publisher_room` 校验）。
    pub room: String,
    /// 访问凭证（可空）。
    pub token: String,
    /// 采集系统音频。
    pub audio: bool,
    /// 允许远端鼠标/键盘控制。
    pub mouse: bool,
    /// 仅观看（忽略全部远端输入）。
    pub view_only: bool,
}

/// 被控端 → UI 的生命周期事件（desktop 适配器映射为既有一组属性写，
/// 保持文案/语义与 B1 前完全一致）。
#[derive(Debug, Clone)]
pub enum PublisherEvent {
    /// 启动中：设置页「正在启动被控端…」+ 信令「正在连接信令…」+ 离线。
    Starting,
    /// 运行状态：状态条/设置页/信令条同文案；online 由 UI 按内容（含「已在线」）判定。
    Status(String),
    /// 启动失败：仅设置页文案（房间无效/线程创建失败/平台未实现）。
    StartFailed(String),
    /// 已停止：设置页「被控端已停止」+ 信令「信令未连接（未开启被控）」+ 离线。
    Stopped,
}

#[cfg(test)]
mod tests {}
