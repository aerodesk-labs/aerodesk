//! Wayland portal RemoteDesktop 输入注入（feature `pipewire`）。
//!
//! 经 xdg-desktop-portal RemoteDesktop 接口（compositor 级）注入键鼠，
//! 无需 root/uinput。与视频采集（ScreenCast）共用同一 portal 会话类型；
//! 本注入器自建会话（会弹一次「远程控制」授权，与采集授权分开）。
//!
//! 键码映射复用 [`crate::inject::keysym_for_code`]（X11 keysym，布局无关，
//! portal `NotifyKeyboardKeysym` 直接接受）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use aerodesk_core::platform::InputInjector;
use aerodesk_protocol::input::{ButtonState, InputEvent, MouseButton};

/// 注入命令：事件 + 结果回传通道。
type Command = (InputEvent, mpsc::Sender<Result<(), String>>);

/// Wayland portal RemoteDesktop 注入器。
pub struct PortalInjector {
    tx: mpsc::Sender<Command>,
    thread: Option<std::thread::JoinHandle<()>>,
    stop: std::sync::Arc<AtomicBool>,
}

/// 鼠标键 → portal 按钮号（xdg-desktop-portal 约定：1 左 / 2 中 / 3 右 / 8 后退 / 9 前进）。
fn portal_button(button: MouseButton) -> i32 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
    }
}

/// 单条事件注入（异步执行于 portal 线程）。
async fn inject_one(
    portal: &lamco_portal::PortalManager,
    session: &lamco_portal::PortalSessionHandle,
    stream_size: (u32, u32),
    event: &InputEvent,
) -> Result<(), String> {
    let rd = portal.remote_desktop();
    let sess = session.ashpd_session();
    let to_px = |v: f64, max: u32| (v.clamp(0.0, 1.0) * max as f64).round();

    match event {
        InputEvent::MouseMove { x, y } => rd
            .notify_pointer_motion_absolute(
                sess,
                0,
                to_px(*x, stream_size.0),
                to_px(*y, stream_size.1),
            )
            .await
            .map_err(|e| format!("portal pointer motion: {e}")),
        InputEvent::MouseButton {
            button,
            state,
            x,
            y,
        } => {
            rd.notify_pointer_motion_absolute(
                sess,
                0,
                to_px(*x, stream_size.0),
                to_px(*y, stream_size.1),
            )
            .await
            .map_err(|e| format!("portal pointer motion: {e}"))?;
            rd.notify_pointer_button(sess, portal_button(*button), *state == ButtonState::Pressed)
                .await
                .map_err(|e| format!("portal button: {e}"))
        }
        InputEvent::Wheel {
            delta_x, delta_y, ..
        } => {
            // 协议 delta_y>0 = 上滚；portal axis 正值 = 下滚。
            rd.notify_pointer_axis(sess, *delta_x, -(*delta_y))
                .await
                .map_err(|e| format!("portal axis: {e}"))
        }
        InputEvent::Key {
            code,
            state,
            modifiers,
        } => {
            let keysym = crate::inject::keysym_for_code(code)
                .ok_or_else(|| format!("unsupported key code: {code}"))?;
            let down = *state == ButtonState::Pressed;
            let mods: [(i32, bool); 4] = [
                (0xFFE3, modifiers.ctrl),  // Control_L
                (0xFFE1, modifiers.shift), // Shift_L
                (0xFFE9, modifiers.alt),   // Alt_L
                (0xFFEB, modifiers.meta),  // Super_L
            ];
            if down {
                for (m, _on) in mods.into_iter().filter(|(_, on)| *on) {
                    rd.notify_keyboard_keysym(sess, m, true)
                        .await
                        .map_err(|e| format!("portal mod: {e}"))?;
                }
                rd.notify_keyboard_keysym(sess, keysym as i32, true)
                    .await
                    .map_err(|e| format!("portal key: {e}"))
            } else {
                rd.notify_keyboard_keysym(sess, keysym as i32, false)
                    .await
                    .map_err(|e| format!("portal key: {e}"))?;
                for (m, _on) in mods.into_iter().filter(|(_, on)| *on) {
                    rd.notify_keyboard_keysym(sess, m, false)
                        .await
                        .map_err(|e| format!("portal mod: {e}"))?;
                }
                Ok(())
            }
        }
        InputEvent::Touch { .. } => Err("linux: portal touch injection not implemented".into()),
        InputEvent::ClipboardText(_) => {
            Err("linux: portal clipboard inject not implemented".into())
        }
    }
}

impl PortalInjector {
    /// 建立 RemoteDesktop portal 会话（触发用户授权）并启动注入线程。
    pub fn new() -> Result<Self, String> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();

        let thread = std::thread::Builder::new()
            .name("aerodesk-portal-inject".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("tokio runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(portal_inject_loop(cmd_rx, ready_tx, stop2));
            })
            .map_err(|e| format!("spawn portal inject thread: {e}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => Ok(Self {
                tx: cmd_tx,
                thread: Some(thread),
                stop,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("portal inject init timeout（portal 未响应）".into()),
        }
    }
}

async fn portal_inject_loop(
    rx: mpsc::Receiver<Command>,
    ready_tx: mpsc::Sender<Result<(), String>>,
    stop: std::sync::Arc<AtomicBool>,
) {
    use lamco_portal::PortalManager;

    let init = async {
        let portal = PortalManager::with_default()
            .await
            .map_err(|e| format!("portal: {e}"))?;
        let (session, _token) = portal
            .create_session("aerodesk-input".to_string(), None)
            .await
            .map_err(|e| format!("portal session: {e}"))?;
        let size = session.streams().first().map(|s| s.size).unwrap_or((0, 0));
        Ok::<_, String>((portal, session, size))
    }
    .await;

    let (portal, session, size) = match init {
        Ok(x) => x,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let _ = ready_tx.send(Ok(()));

    while !stop.load(Ordering::Relaxed) {
        match rx.recv() {
            Ok((event, reply)) => {
                let r = inject_one(&portal, &session, size, &event).await;
                let _ = reply.send(r);
            }
            Err(_) => break, // 发送端已 drop
        }
    }
    drop(session);
    drop(portal);
}

impl Drop for PortalInjector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take();
    }
}

impl InputInjector for PortalInjector {
    type Error = String;

    fn inject(&mut self, event: &InputEvent) -> Result<(), Self::Error> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send((event.clone(), reply_tx))
            .map_err(|_| "portal inject thread closed".to_string())?;
        match reply_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(r) => r,
            Err(_) => Err("portal inject timeout".into()),
        }
    }
}
