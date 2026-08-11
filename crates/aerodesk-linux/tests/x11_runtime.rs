#![cfg(target_os = "linux")]

//! Linux 被控端核心能力运行级自测（X11 + XTest，CI Xvfb :99）。
//!
//! 验证：X11 屏幕采集（x11rb GetImage → BGRA）与 XTest 输入注入
//! （MouseMove/Button/Wheel/Key）在真实 X 连接下可用。
//! 依赖：DISPLAY 指向可用 X server（CI 用 Xvfb）。
//!
//! #277：统一走 core `MediaSource` / `InputInjector` trait + 真实协议事件。

use aerodesk_core::platform::{InputInjector, MediaSource};
use aerodesk_linux::capture::X11Capturer;
use aerodesk_linux::inject::XTestInjector;
use aerodesk_protocol::input::{ButtonState, InputEvent, Modifiers, MouseButton};

#[test]
fn x11_capture_produces_frame() {
    // 无 DISPLAY（如普通 cargo test / CI test job）时跳过；Xvfb 场景（linux-ui-e2e）真正跑。
    if std::env::var("DISPLAY").is_err() {
        eprintln!("SKIP: no DISPLAY");
        return;
    }
    let mut cap = X11Capturer::new().expect("X11 连接（DISPLAY 需可用）");
    let (w, h) = cap.size();
    assert!(w > 0 && h > 0, "root 窗口尺寸应 > 0: {w}x{h}");
    let frame = <X11Capturer as MediaSource>::next_frame(&mut cap)
        .expect("采集")
        .expect("帧");
    // core 约定 raw=BGRA32（适配器内从 RGBA 转 BGRA）。
    assert_eq!(frame.raw.as_ref().unwrap().len() as u32, w * h * 4);
    assert_eq!(frame.width, w);
    assert_eq!(frame.height, h);
    eprintln!("capture OK: {w}x{h} pts={}ms", frame.pts_ms);
}

#[test]
fn xtest_injects_mouse_key_wheel() {
    if std::env::var("DISPLAY").is_err() {
        eprintln!("SKIP: no DISPLAY");
        return;
    }
    let mut inj = XTestInjector::new().expect("XTest 扩展（Xvfb 默认支持）");
    // 坐标 0..1 归一化 → 注入器内部换算；事件为真实协议类型。
    InputInjector::inject(&mut inj, &InputEvent::MouseMove { x: 0.5, y: 0.5 }).expect("mouse move");
    InputInjector::inject(
        &mut inj,
        &InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ButtonState::Pressed,
            x: 0.5,
            y: 0.5,
        },
    )
    .expect("mouse down");
    InputInjector::inject(
        &mut inj,
        &InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ButtonState::Released,
            x: 0.5,
            y: 0.5,
        },
    )
    .expect("mouse up");
    InputInjector::inject(
        &mut inj,
        &InputEvent::Wheel {
            x: 0.0,
            y: 0.0,
            delta_x: 0.0,
            delta_y: 3.0,
        },
    )
    .expect("wheel");
    // Key：FakeInput 用 keysym→keycode 映射（协议码 "KeyA"）。
    InputInjector::inject(
        &mut inj,
        &InputEvent::Key {
            code: "KeyA".into(),
            state: ButtonState::Pressed,
            modifiers: Modifiers::default(),
        },
    )
    .expect("key down");
    InputInjector::inject(
        &mut inj,
        &InputEvent::Key {
            code: "KeyA".into(),
            state: ButtonState::Released,
            modifiers: Modifiers::default(),
        },
    )
    .expect("key up");
    eprintln!("xtest inject OK");
}
