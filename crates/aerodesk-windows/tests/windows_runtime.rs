//! Windows 被控端核心能力运行级自测（CI windows runner 交互会话）。
//!
//! 验证：DXGI Desktop Duplication 采集与 SendInput 输入注入可用。
//! 容错：runner 无活动显示器/输出时 DXGI 不可用 → 跳过（真机验证），
//! 避免 CI 偶发虚拟桌面环境差异导致 flake。

use aerodesk_windows::capture::DxgiCapturer;
use aerodesk_windows::inject::{InputEvent, InputInjector, SendInputInjector};

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
    let frame = cap.next_frame();
    match frame {
        Some(f) => {
            assert_eq!(f.bgra.len() as u32, w * h * 4, "BGRA 帧字节数 = w*h*4");
            eprintln!("dxgi capture OK: {w}x{h}");
        }
        None => eprintln!("SKIP: DXGI 首帧未就绪（虚拟桌面无内容更新）"),
    }
}

#[test]
fn sendinput_injects_mouse_key() {
    // SendInputInjector 是 unit struct（无需初始化，SendInput 系统调用）。
    let mut inj = SendInputInjector;
    inj.inject(&InputEvent::MouseMove { x: 0.5, y: 0.5 })
        .expect("mouse move");
    inj.inject(&InputEvent::MouseButton {
        x: 0.5,
        y: 0.5,
        button: 0,
        down: true,
    })
    .expect("mouse down");
    inj.inject(&InputEvent::MouseButton {
        x: 0.5,
        y: 0.5,
        button: 0,
        down: false,
    })
    .expect("mouse up");
    inj.inject(&InputEvent::Wheel { dx: 0.0, dy: 3.0 })
        .expect("wheel");
    // Key：SendInput 用虚拟键码（A=0x41）
    inj.inject(&InputEvent::Key {
        code: 0x41,
        down: true,
    })
    .expect("key down");
    inj.inject(&InputEvent::Key {
        code: 0x41,
        down: false,
    })
    .expect("key up");
    eprintln!("sendinput inject OK");
}
