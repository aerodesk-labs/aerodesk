#![cfg(target_os = "linux")]

//! uinput 运行级自测：创建虚拟输入设备并注入鼠标/滚轮/按键事件。
//!
//! 依赖：`/dev/uinput` 可写（root 或 input 组；CI ubuntu runner 无该设备时跳过）。
//! 可用 `AERODESK_UINPUT=/path` 覆盖设备路径（容器/devtmpfs 场景）。

use aerodesk_core::platform::InputInjector;
use aerodesk_linux::inject::UinputInjector;
use aerodesk_protocol::input::{ButtonState, InputEvent, Modifiers, MouseButton};

fn uinput_available() -> bool {
    let path = std::env::var("AERODESK_UINPUT").unwrap_or_else(|_| "/dev/uinput".to_string());
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .map(|_| true)
        .unwrap_or(false)
}

#[test]
fn uinput_injects_mouse_key_wheel() {
    if !uinput_available() {
        eprintln!("SKIP: /dev/uinput 不可写（非 root/input 组或 CI 容器）");
        return;
    }
    let mut inj = UinputInjector::new().expect("uinput 设备创建");
    InputInjector::inject(&mut inj, &InputEvent::MouseMove { x: 0.5, y: 0.25 })
        .expect("mouse move");
    InputInjector::inject(
        &mut inj,
        &InputEvent::MouseButton {
            button: MouseButton::Right,
            state: ButtonState::Pressed,
            x: 0.5,
            y: 0.5,
        },
    )
    .expect("mouse down");
    InputInjector::inject(
        &mut inj,
        &InputEvent::MouseButton {
            button: MouseButton::Right,
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
            delta_x: -1.0,
            delta_y: 3.0,
        },
    )
    .expect("wheel");
    InputInjector::inject(
        &mut inj,
        &InputEvent::Key {
            code: "KeyA".into(),
            state: ButtonState::Pressed,
            modifiers: Modifiers {
                ctrl: true,
                ..Default::default()
            },
        },
    )
    .expect("key down with ctrl");
    InputInjector::inject(
        &mut inj,
        &InputEvent::Key {
            code: "KeyA".into(),
            state: ButtonState::Released,
            modifiers: Modifiers {
                ctrl: true,
                ..Default::default()
            },
        },
    )
    .expect("key up");
    eprintln!("uinput inject OK");
}
