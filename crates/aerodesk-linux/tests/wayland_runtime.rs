#![cfg(all(target_os = "linux", feature = "pipewire"))]

//! Wayland/PipeWire 运行级自测：portal 会话 → PipeWire fd → 建流 → 收帧。
//!
//! 依赖真实 Wayland 桌面 + xdg-desktop-portal + PipeWire，且会弹授权对话框；
//! CI（无头/无 portal）与日常测试一律跳过，仅当 `AERODESK_TEST_WAYLAND=1`
//! 显式启用（真机验收时执行：`AERODESK_TEST_WAYLAND=1 cargo test -p aerodesk-linux --test wayland_runtime`）。

use aerodesk_core::platform::MediaSource;
use aerodesk_linux::capture::WaylandPortalCapturer;

#[test]
fn wayland_portal_capture_produces_frame() {
    if std::env::var("AERODESK_TEST_WAYLAND").is_err() {
        eprintln!("SKIP: 未设置 AERODESK_TEST_WAYLAND=1（需 Wayland 桌面 + portal + PipeWire）");
        return;
    }
    let wayland = std::env::var("XDG_SESSION_TYPE")
        .map(|v| v == "wayland")
        .unwrap_or(false);
    if !wayland && std::env::var("WAYLAND_DISPLAY").is_err() {
        eprintln!("SKIP: 非 Wayland 会话（XDG_SESSION_TYPE/WAYLAND_DISPLAY 缺失）");
        return;
    }
    let mut cap = WaylandPortalCapturer::new().expect("构造");
    MediaSource::start(&mut cap, 30, true).expect("portal+pipewire 启动（会弹授权）");
    let mut got = None;
    for _ in 0..50 {
        if let Ok(Some(frame)) = MediaSource::next_frame(&mut cap) {
            got = Some(frame);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let frame = got.expect("30s 内应收到第一帧");
    assert!(frame.width > 0 && frame.height > 0);
    assert_eq!(
        frame.raw.as_ref().unwrap().len(),
        (frame.width * frame.height * 4) as usize,
        "core 约定 raw=紧凑 BGRA32"
    );
    eprintln!(
        "wayland capture OK: {}x{} pts={}ms",
        frame.width, frame.height, frame.pts_ms
    );
    MediaSource::stop(&mut cap);
}
