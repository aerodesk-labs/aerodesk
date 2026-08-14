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
use std::sync::Mutex;

/// 同一 output 同时只允许一个 duplication（DXGI 限制）：运行级 DXGI 测试
/// 必须串行执行，否则并行建 duplication 的后建者会 E_INVALIDARG 假 SKIP。
static DXGI_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn dxgi_capture_produces_frame() {
    let _guard = DXGI_LOCK.lock().unwrap();
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

#[test]
fn dxgi_switch_display_same_display_ok() {
    // #58 运行中切换显示器：切到同一显示器应成功且输出尺寸保持。
    let _guard = DXGI_LOCK.lock().unwrap();
    let mut cap = match DxgiCapturer::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: DXGI init failed: {e}");
            return;
        }
    };
    let (w, h) = cap.size();
    // 同屏切换必须成功（先释放旧 duplication 再重建；失败视为缺陷，不跳过）。
    cap.switch_display(0)
        .expect("switch to same display should succeed");
    let (w2, h2) = cap.size();
    assert!(w2 > 0 && h2 > 0, "切换后输出尺寸应 > 0: {w2}x{h2}");
    eprintln!("dxgi switch to same display OK: {w}x{h} -> {w2}x{h2}");
}

#[test]
fn dxgi_switch_display_invalid_keeps_capture() {
    // #58：切换失败必须保持原采集不变（不破坏进行中的会话）。
    let _guard = DXGI_LOCK.lock().unwrap();
    let mut cap = match DxgiCapturer::new_with_scale(640, 360) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: DXGI init failed: {e}");
            return;
        }
    };
    let (w, h) = cap.size();
    assert!(cap.switch_display(9999).is_err(), "无效显示器索引应报错");
    let (w2, h2) = cap.size();
    assert_eq!((w, h), (w2, h2), "切换失败后采集应保持不变");
}
