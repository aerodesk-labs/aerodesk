//! Linux 被控端核心能力运行级自测（X11 + XTest，CI Xvfb :99）。
//!
//! 验证：X11 屏幕采集（x11rb GetImage → RGBA）与 XTest 输入注入
//! （MouseMove/Button/Wheel/Key）在真实 X 连接下可用。
//! 依赖：DISPLAY 指向可用 X server（CI 用 Xvfb）。

use aerodesk_linux::capture::X11Capturer;
use aerodesk_linux::inject::{InputEvent, InputInjector, XTestInjector};

#[test]
fn x11_capture_produces_rgba_frame() {
    let mut cap = X11Capturer::new().expect("X11 连接（DISPLAY 需可用）");
    let (w, h) = cap.size();
    assert!(w > 0 && h > 0, "root 窗口尺寸应 > 0: {w}x{h}");
    let frame = cap.next_frame().expect("采集到帧");
    assert_eq!(
        frame.rgba.len() as u32,
        w * h * 4,
        "RGBA 帧字节数 = w*h*4"
    );
    assert_eq!(frame.width, w);
    assert_eq!(frame.height, h);
    eprintln!("capture OK: {w}x{h} pts={}", frame.pts_us);
}

#[test]
fn xtest_injects_mouse_key_wheel() {
    let mut inj = XTestInjector::new().expect("XTest 扩展（Xvfb 默认支持）");
    // 坐标 0..1 归一化 → 注入器内部换算
    inj.inject(&InputEvent::MouseMove { x: 0.5, y: 0.5 })
        .expect("mouse move");
    inj.inject(&InputEvent::MouseButton {
        x: 0.5,
        y: 0.5,
        button: 1,
        down: true,
    })
    .expect("mouse down");
    inj.inject(&InputEvent::MouseButton {
        x: 0.5,
        y: 0.5,
        button: 1,
        down: false,
    })
    .expect("mouse up");
    inj.inject(&InputEvent::Wheel { dx: 0.0, dy: 3.0 })
        .expect("wheel");
    // Key：FakeInput 用 X11 keysym→keycode 映射（A 键）
    inj.inject(&InputEvent::Key { code: 0x61, down: true })
        .expect("key down");
    inj.inject(&InputEvent::Key { code: 0x61, down: false })
        .expect("key up");
    eprintln!("xtest inject OK");
}
