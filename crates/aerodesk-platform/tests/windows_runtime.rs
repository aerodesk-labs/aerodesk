//! Windows 被控端核心能力运行级自测（CI windows runner 交互会话）。
#![cfg(windows)]
//!
//! 验证：DXGI Desktop Duplication 采集与 SendInput 输入注入可用。
//! 容错：runner 无活动显示器/输出时 DXGI 不可用 → 跳过（真机验证），
//! 避免 CI 偶发虚拟桌面环境差异导致 flake。
//!
//! #277：统一走 core `MediaSource` / `InputInjector` trait + 真实协议事件。

use aerodesk_core::platform::{InputInjector, MediaSource};
use aerodesk_core::protocol::input::{ButtonState, InputEvent, Modifiers, MouseButton};
use aerodesk_platform::windows::capture::DxgiCapturer;
use aerodesk_platform::windows::inject::SendInputInjector;
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

#[test]
fn wgc_capture_produces_frame() {
    // #514：WGC 主采集路径冒烟。runner 无交互会话/WinRT 受限 → SKIP（真机验证）。
    // 不与 DXGI_LOCK 串行：WGC 会话与 DXGI duplication 互不排斥。
    use aerodesk_platform::windows::capture_wgc::WgcCapturer;
    let mut cap = match WgcCapturer::new_with_scale(640, 360) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: WGC init failed: {e}");
            return;
        }
    };
    let (w, h) = cap.size();
    assert_eq!((w, h), (640, 360), "缩放目标应生效");
    match <WgcCapturer as MediaSource>::next_frame(&mut cap) {
        Ok(Some(f)) => {
            assert_eq!(
                f.raw.as_ref().unwrap().len() as u32,
                w * h * 4,
                "BGRA32 帧大小应匹配"
            );
            eprintln!("wgc capture OK: {w}x{h}");
        }
        Ok(None) => eprintln!("SKIP: WGC 首帧未就绪（静态桌面+引导失败时）"),
        Err(e) => eprintln!("SKIP: WGC next_frame: {e}"),
    }
}

#[test]
fn screen_capturer_chain_produces_frame() {
    // #514：生产回退链（WGC 主 → DXGI 备）端到端——链构造成功且输出尺寸正确。
    // 两路均不可用的受限环境 → SKIP。
    let _guard = DXGI_LOCK.lock().unwrap();
    let mut cap =
        match aerodesk_platform::windows::capture::ScreenCapturer::new_with_scale(640, 360) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: 采集链初始化失败: {e}");
                return;
            }
        };
    let (w, h) = cap.size();
    assert_eq!((w, h), (640, 360));
    match aerodesk_core::platform::MediaSource::next_frame(&mut cap) {
        Ok(Some(f)) => {
            assert_eq!(f.raw.as_ref().unwrap().len() as u32, w * h * 4);
            eprintln!("screen capturer chain OK: {w}x{h}");
        }
        Ok(None) => eprintln!("SKIP: 采集链首帧未就绪"),
        Err(e) => eprintln!("SKIP: 采集链 next_frame: {e}"),
    }
}

#[test]
fn clipboard_text_inject_roundtrip() {
    // #72/#271：SendInputInjector 的 ClipboardText 注入写入系统剪贴板（Win32）。
    // 非交互会话 OpenClipboard 可能失败 → 跳过；成功则必须读回一致。
    let mut inj = SendInputInjector::new();
    let text = format!("aerodesk-clip-{}", std::process::id());
    match InputInjector::inject(&mut inj, &InputEvent::ClipboardText(text.clone())) {
        Ok(()) => {
            let read = aerodesk_core::clipboard::read();
            assert_eq!(
                read.as_deref(),
                Some(text.as_str()),
                "系统剪贴板应等于注入文本"
            );
            eprintln!("clipboard inject roundtrip OK: {text}");
        }
        Err(e) => eprintln!("SKIP: clipboard inject: {e}"),
    }
}

#[test]
fn mf_camera_lists_and_captures() {
    // #385 Windows 摄像头（MF SourceReader）：枚举 + 采集首帧（BGRA）。
    // 无摄像头（CI/裸机）时 SKIP；有设备（如虚拟摄像头）必须出帧。
    use aerodesk_core::platform::CameraSource;
    let cams = aerodesk_platform::windows::camera::list_cameras();
    if cams.is_empty() {
        eprintln!("SKIP: no camera device（真机/虚拟摄像头验证）");
        return;
    }
    eprintln!("cameras: {cams:?}");
    let mut cam = match aerodesk_platform::windows::camera::MfCamera::new(Some("0")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: MfCamera::new: {e}");
            return;
        }
    };
    if let Err(e) = CameraSource::start(&mut cam, 640, 480, 30) {
        eprintln!("SKIP: camera start: {e}");
        return;
    }
    match CameraSource::next_frame(&mut cam) {
        Ok(Some(f)) => {
            assert_eq!(
                f.raw.len() as u64,
                f.width as u64 * f.height as u64 * 4,
                "BGRA32 帧大小应匹配"
            );
            eprintln!("camera capture OK: {}x{}", f.width, f.height);
        }
        Ok(None) => eprintln!("SKIP: camera 首帧未就绪"),
        Err(e) => eprintln!("SKIP: camera next_frame: {e}"),
    }
}

#[test]
fn windows_cursor_normalized_in_range() {
    // #487 光标列缺口：真实被控端 CursorPos 上报的光标源。GetCursorPos 在
    // 交互会话恒可用（headless/服务会话也返回合法坐标），归一化应落 [0,1]。
    use aerodesk_core::platform::CursorSource;
    let mut cur = aerodesk_platform::windows::cursor::WindowsCursor::default();
    match cur.position_normalized() {
        Some((x, y)) => {
            eprintln!("cursor OK: ({x:.3}, {y:.3})");
            assert!((0.0..=1.0).contains(&x), "x 应在 [0,1]: {x}");
            assert!((0.0..=1.0).contains(&y), "y 应在 [0,1]: {y}");
        }
        None => eprintln!("SKIP: GetCursorPos 不可用"),
    }
}
