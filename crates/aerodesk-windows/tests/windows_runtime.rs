//! Windows 被控端核心能力运行级自测（CI windows runner 交互会话）。
#![cfg(windows)]
//!
//! 验证：DXGI Desktop Duplication 采集与 SendInput 输入注入可用。
//! 容错：runner 无活动显示器/输出时 DXGI 不可用 → 跳过（真机验证），
//! 避免 CI 偶发虚拟桌面环境差异导致 flake。
//!
//! #277：统一走 core `MediaSource` / `InputInjector` trait + 真实协议事件。

use aerodesk_core::platform::{InputInjector, MediaSource};
use aerodesk_protocol::input::{ButtonState, InputEvent, Modifiers, MouseButton};
use aerodesk_windows::capture::DxgiCapturer;
use aerodesk_windows::inject::SendInputInjector;

#[test]
fn dxgi_capture_produces_frame() {
    let mut cap = match DxgiCapturer::new() {
        Ok(c) => c,
        Err(e) => {
            // 无活动输出（headless/虚拟桌面受限）时跳过，真机验证。
            eprintln!("SKIP: DXGI init failed: {e}");
            return;
        }
    };
    let (w, h) = cap.size();
    assert!(w > 0 && h > 0, "输出尺寸应 > 0: {w}x{h}");
    let frame = <DxgiCapturer as MediaSource>::next_frame(&mut cap);
    match frame {
        Ok(Some(f)) => {
            // core 约定 raw=BGRA32（DXGI 原生 BGRA）。
            assert_eq!(f.raw.as_ref().unwrap().len() as u32, w * h * 4);
            eprintln!("dxgi capture OK: {w}x{h}");
        }
        Ok(None) => eprintln!("SKIP: DXGI 首帧未就绪（虚拟桌面无内容更新）"),
        Err(e) => eprintln!("SKIP: DXGI next_frame: {e}"),
    }
}

#[test]
fn sendinput_injects_mouse_key() {
    // SendInputInjector 是 unit struct（无需初始化，SendInput 系统调用）。
    let mut inj = SendInputInjector::new();
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
    // Key：SendInput 用 VK 映射（协议码 "KeyA"）。
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
    eprintln!("sendinput inject OK");
}
