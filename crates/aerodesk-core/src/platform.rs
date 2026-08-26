//! 平台适配抽象：核心只依赖这些 trait，平台差异收敛在各适配器 crate。
//!
//! 平台矩阵（见仓库 Wiki「Platform-Roles」）：
//! - macOS:   ScreenCaptureKit + VideoToolbox + CGEvent + SCK Audio
//! - Windows: DXGI + NVENC/QSV + WASAPI + SendInput
//! - Linux:   PipeWire + VAAPI + XTest/uinput
//! - Android: MediaProjection + MediaCodec + AccessibilityService
//! - iOS:     ReplayKit（仅观看端）
//! - HarmonyOS: AVScreenCapture + 硬件编码 + OH_Input_*
//!
//! 所有实现方必须实现本模块 trait；禁止在各平台 crate 重复定义同名 trait。
//!
//! 平台能力矩阵：MediaSource/Encoder/Decoder/Renderer/InputInjector/AudioSink/AudioCapturer/
//! Clipboard/CursorSource/Permissions/CameraSource/FilePicker/AppShell/VirtualDisplay/Notifier/
//! CommandExecutor（远程命令「bash」，策略层在 [`crate::cmd_exec`]）/ SystemWakeLock（保持唤醒）。

use std::any::Any;
use std::sync::Arc;

use aerodesk_protocol::cmd::PowerAction;

/// 编码输出单元。
#[derive(Debug, Clone)]
pub struct EncodedUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_ms: u64,
    /// 编码帧对应的 RTP 时间戳增量。
    pub rtp_timestamp: u32,
}

/// 编码格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    Vp8,
    Vp9,
}

/// 原始视频帧（平台采集器输出）。
///
/// 三通道并存，按平台能力选一：
/// - `platform`：零拷贝平台帧对象（macOS IOSurface，编码器直接下转使用，不拷贝）
/// - `handle`：平台私有帧句柄（如 NVENC 输入纹理 / MediaCodec buffer id）
/// - `raw`：原始像素，**统一 BGRA32 约定**（无零拷贝通道时使用；
///   macOS/Win DXGI/合成源均 BGRA，Linux 采集在适配器内转 BGRA）
#[derive(Clone)]
pub struct VideoFrame {
    /// 平台零拷贝帧对象（实现方负责 downcast；无则 None）。
    pub platform: Option<Arc<dyn Any + Send>>,
    /// 平台私有帧句柄（NVENC 纹理 / MediaCodec buffer）。
    pub handle: Option<u64>,
    /// 原始像素（platform/handle 都为空时使用）。
    pub raw: Option<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub pts_ms: u64,
}

/// 屏幕/窗口采集源（被控端）。
pub trait MediaSource {
    type Error: std::fmt::Display + std::fmt::Debug;
    /// 启动采集。参数为区域/帧率/是否含光标。
    fn start(&mut self, fps: u32, with_cursor: bool) -> Result<(), Self::Error>;
    /// 取下一帧（阻塞或回调）。
    fn next_frame(&mut self) -> Result<Option<VideoFrame>, Self::Error>;
    fn stop(&mut self);
    /// 当前采集的显示器标识（多显示器坐标基准；单显示器/未知返回 None）。
    fn display_id(&self) -> Option<u32> {
        None
    }
    /// 运行中切换采集显示器（viewer 经 control 通道请求，#58；默认不支持）。
    fn switch_display(&mut self, _display: u32) -> Result<(), String> {
        Err("display switch not implemented for this source".into())
    }
    /// 当前采集显示器在虚拟屏幕中的区域（像素 x,y,w,h；供注入/光标坐标基准同步）。
    fn display_rect(&self) -> Option<(i32, i32, u32, u32)> {
        None
    }
}

/// 硬件/软件编码器（H.264 / HEVC / AV1，按平台能力选择）。
pub trait Encoder {
    type Error: std::fmt::Display + std::fmt::Debug;
    fn configure(
        &mut self,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(), Self::Error>;
    /// 编码一帧，产出 RTP 负载前的编码单元（AnnexB/AVCC/OBU...）。
    fn encode(&mut self, frame: &VideoFrame) -> Result<Option<EncodedUnit>, Self::Error>;
    /// 请求关键帧。
    fn request_keyframe(&mut self);
    /// 目标码率/帧率（BitrateController 或远端反馈驱动）。
    fn set_bitrate(&mut self, bitrate_bps: u64, fps: u32);
}

/// 解码器（观看端）。
pub trait Decoder {
    type Error: std::fmt::Display + std::fmt::Debug;
    fn configure(&mut self, codec: Codec, width: u32, height: u32) -> Result<(), Self::Error>;
    fn decode(&mut self, unit: &EncodedUnit) -> Result<Option<VideoFrame>, Self::Error>;
}

/// 渲染器（观看端）。
pub trait Renderer {
    type Error: std::fmt::Display + std::fmt::Debug;
    fn render(&mut self, frame: &VideoFrame) -> Result<(), Self::Error>;
}

/// 输入注入器（被控端；观看端捕获事件则相反，见 crate::protocol::input）。
pub trait InputInjector {
    type Error: std::fmt::Display + std::fmt::Debug;
    fn inject(&mut self, event: &crate::protocol::input::InputEvent) -> Result<(), Self::Error>;
    /// 设置输入注入的坐标基准显示器（多显示器切换时由宿主调用）。
    fn set_active_display(&mut self, _display_id: Option<u32>) {}
}

/// 音频播放（观看端）。
pub trait AudioSink {
    fn push_pcm(&mut self, samples: &[i16]);
    fn set_muted(&mut self, muted: bool);
    /// 音量 0..=100。
    fn set_volume(&mut self, volume: u16);
}

/// 音频采集（被控端：系统音频 / 麦克风，输出单声道 f32 样本流）。
pub trait AudioCapturer {
    type Error: std::fmt::Display + std::fmt::Debug;
    /// 排空最多 `max` 个样本（单声道 f32）。
    fn next_samples(&mut self, max: usize) -> Result<Vec<f32>, Self::Error>;
}

/// 系统剪贴板文本读写（双向同步）。
pub trait Clipboard {
    type Error: std::fmt::Display + std::fmt::Debug;
    fn read_text(&mut self) -> Result<Option<String>, Self::Error>;
    fn write_text(&mut self, text: &str) -> Result<(), Self::Error>;
}

/// 光标位置源（被控端：真实光标归一化坐标 0..1，供观看端叠加层）。
pub trait CursorSource {
    fn position_normalized(&mut self) -> Option<(f64, f64)>;
    /// 设置坐标基准显示器区域（发布端切换显示器后由宿主同步；默认忽略）。
    fn set_active_display(&mut self, _rect: Option<(i32, i32, u32, u32)>) {}
}

/// 系统权限能力（屏幕录制 / 辅助功能等；各平台实现）。
pub trait Permissions {
    fn screen_capture_authorized(&self) -> bool;
    fn accessibility_authorized(&self) -> bool;
    fn request_screen_capture(&self) -> bool;
    fn open_screen_capture_settings(&self);
    fn open_accessibility_settings(&self);
    /// 触发系统权限登记（如 macOS TCC 采集注册）。
    fn trigger_screen_capture_registration(&self);
}

/// 摄像头帧（远端摄像头转发；YUV 或 raw 由实现决定）。
#[derive(Debug, Clone)]
pub struct CameraFrame {
    pub raw: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts_ms: u64,
}

/// 摄像头源（远端摄像头转发；macOS AVFoundation 已实现，其他平台批次）。
pub trait CameraSource {
    type Error: std::fmt::Display + std::fmt::Debug;
    fn start(&mut self, width: u32, height: u32, fps: u32) -> Result<(), Self::Error>;
    fn next_frame(&mut self) -> Result<Option<CameraFrame>, Self::Error>;
    fn stop(&mut self);
}

/// 文件选择器（观看端「发送文件」选择本地文件路径）。
pub trait FilePicker {
    type Error: std::fmt::Display + std::fmt::Debug;
    /// 弹出系统文件选择器；返回所选文件路径（取消返回 None）。
    fn pick_file(&self) -> Result<Option<String>, Self::Error>;
}

/// 应用壳层（窗口激活 / Dock-任务栏重开等平台外壳集成）。
pub trait AppShell {
    /// 把应用/主窗口带到前台。
    fn activate(&self);
    /// 聚焦指定原生视图（平台句柄；非本平台可忽略）。
    fn focus_view(&self, view: *mut std::ffi::c_void);
    /// 安装「点击 Dock/任务栏图标恢复窗口」处理器。
    fn install_reopen_handler(&self);
    /// 注册重开回调（点击 Dock/任务栏图标时触发）。
    fn set_reopen_callback(&self, callback: Box<dyn Fn() + Send + Sync>);
}

/// 虚拟显示器（被控端扩展桌面；Windows Parsec VDD，macOS/Linux 批次）。
pub trait VirtualDisplay {
    type Error: std::fmt::Display + std::fmt::Debug;
    fn add_display(&mut self, width: u32, height: u32, hz: u32) -> Result<i32, Self::Error>;
    fn remove_display(&mut self, index: i32) -> Result<(), Self::Error>;
    fn display_count(&self) -> usize;
}

/// 系统通知（收到连接/文件等事件时提示用户）。
pub trait Notifier {
    fn notify(&self, title: &str, body: &str);
}

/// 命令执行结果（远程命令「bash」抽象，见 [`CommandExecutor`]）。
#[derive(Debug, Clone, Default)]
pub struct CmdOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub error: Option<String>,
    /// #13 结构化错误码（[`crate::protocol::error::ErrorCode`] 的 wire 串；
    /// 无错误为 None）。
    pub code: Option<String>,
}

/// 远程命令/文件/进程执行器（被控端；「bash」抽象）。
///
/// 平台差异（shell 选择、进程枚举格式、结束进程方式）由各适配器实现：
/// unix 默认 `sh -c` / `ps` / `kill`，Windows 默认 `cmd /C` / `tasklist` / `taskkill`。
/// 本 trait 只负责**原始执行**；危险命令拦截、白名单、审计等策略在
/// [`crate::cmd_exec`]（平台中立，核心统一入口）。
pub trait CommandExecutor {
    /// 执行命令（shell 由平台选择）。返回值自包含：spawn/wait/超时/截断
    /// 错误放入 [`CmdOutput::error`]，不向调用方抛错。
    fn run_command(&self, command: &str, cwd: Option<&str>, timeout_ms: Option<u64>) -> CmdOutput;
    /// 读文件（`max_bytes` 为上限；超出返回错误）。
    fn read_file(
        &self,
        path: &str,
        max_bytes: Option<usize>,
    ) -> Result<Vec<u8>, crate::cmd_exec::CmdExecError>;
    /// 写文件（`data` 为原始字节）。
    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), crate::cmd_exec::CmdExecError>;
    /// 列出进程（平台格式差异收敛于此）。
    fn list_processes(
        &self,
    ) -> Result<Vec<crate::protocol::cmd::ProcessInfo>, crate::cmd_exec::CmdExecError>;
    /// 结束进程。
    fn kill_process(&self, pid: u32) -> Result<(), crate::cmd_exec::CmdExecError>;
    /// #503 系统电源命令（关机/重启/锁屏）原始执行。动作经 [`PowerAction`] 枚举
    /// 校验（不接受自由参数，杜绝 shell 注入）；平台固定命令由
    /// [`crate::cmd_exec::power_command_line`] 构造（命令本身受限且固定，不经
    /// 危险命令拦截）。策略（审计）在 [`crate::cmd_exec::system_power`]。
    /// 默认实现即全平台通用路径（Windows shutdown/rundll32、macOS osascript、
    /// Linux systemctl/loginctl）；平台适配器可按需覆盖（如 macOS 改用系统 API）。
    fn power_command(&self, action: PowerAction) -> CmdOutput {
        let command = crate::cmd_exec::power_command_line(action);
        self.run_command(&command, None, Some(POWER_COMMAND_TIMEOUT_MS))
    }
}

/// 电源命令执行超时（15s）：正常命令秒回；osascript 授权弹窗未点可能挂起，
/// 超时强杀后以 error 回执（控制端可见明确错误而非无限等待）。
pub const POWER_COMMAND_TIMEOUT_MS: u64 = 15_000;

/// 唤醒锁句柄：Drop 即释放（平台实现负责 kill 子进程/恢复系统状态）。
/// `release` 可显式提前释放；默认空实现，平台按需覆盖。
pub trait WakeGuard {
    fn release(&mut self) {}
}

/// 保持系统/显示器唤醒（流媒体采集/播放期间防止休眠，见 #334）。
///
/// 平台差异由各适配器实现：macOS `caffeinate -d`、Windows
/// `SetThreadExecutionState`、Linux `systemd-inhibit`（后两者批次）。
/// core 提供 [`NoopSystemWakeLock`] 默认实现，保证未接适配器的平台
/// 编译与运行不受影响（可达性，见 `RULE_可达性`）。
pub trait SystemWakeLock {
    /// 获取唤醒锁；`display=true` 时同时阻止显示器休眠。
    /// 返回的 guard 存活期间锁保持；Drop/release 后释放。
    fn acquire(&self, display: bool) -> Result<Box<dyn WakeGuard>, String>;
}

/// 默认空实现：不做任何事（平台未接适配器时的安全回退）。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSystemWakeLock;

impl SystemWakeLock for NoopSystemWakeLock {
    fn acquire(&self, _display: bool) -> Result<Box<dyn WakeGuard>, String> {
        Ok(Box::new(NoopWakeGuard))
    }
}

struct NoopWakeGuard;
impl WakeGuard for NoopWakeGuard {}

/// 便捷 re-export：`use crate::platform::*` 同时拿到 Codec/EncodedUnit。

#[cfg(test)]
mod tests {
    use super::*;

    struct DummySource;
    impl MediaSource for DummySource {
        type Error = String;
        fn start(&mut self, _fps: u32, _with_cursor: bool) -> Result<(), Self::Error> {
            Ok(())
        }
        fn next_frame(&mut self) -> Result<Option<VideoFrame>, Self::Error> {
            Ok(None)
        }
        fn stop(&mut self) {}
    }

    struct DummyInjector;
    impl InputInjector for DummyInjector {
        type Error = String;
        fn inject(
            &mut self,
            _event: &crate::protocol::input::InputEvent,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// #277 默认方法平台中立：未知显示器返回 None、坐标基准切换无副作用。
    #[test]
    fn trait_defaults_are_platform_neutral() {
        let mut src = DummySource;
        assert_eq!(MediaSource::display_id(&src), None);
        let mut inj = DummyInjector;
        InputInjector::set_active_display(&mut inj, Some(1));
        InputInjector::set_active_display(&mut inj, None);
    }

    /// #334：Noop 唤醒锁可获取、可显式释放、可 Drop（不 panic）。
    #[test]
    fn noop_wake_lock_acquire_release() {
        let lock = NoopSystemWakeLock;
        let mut guard = lock.acquire(true).unwrap();
        guard.release();
        drop(guard);
        let _ = lock.acquire(false).unwrap();
    }

    /// #334：SystemWakeLock / WakeGuard 均可对象化（适配器扩展点）。
    #[test]
    fn wake_lock_traits_are_object_safe() {
        let lock: Box<dyn SystemWakeLock> = Box::new(NoopSystemWakeLock);
        let guard: Box<dyn WakeGuard> = lock.acquire(true).unwrap();
        drop(guard);
    }
}
