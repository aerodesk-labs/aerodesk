//! AeroDesk UI 壳（Slint）：连接/房间/状态。
//!
//! 7 平台一套 UI（桌面/移动/Web）；鸿蒙走 ArkTS 壳 + Rust NAPI，UI 组件保持可迁移。
//! 连接逻辑复用 aerodesk-core（WsSignalClient + Endpoint），与 CLI/App 共用。

slint::include_modules!();



fn main() -> Result<(), slint::PlatformError> {
    init_log();
    let ui = AppWindow::new()?;

    ui.on_connect({
        let weak = ui.as_weak();
        move || {
            let ui = weak.unwrap();
            let server = ui.get_server_input().to_string();
            let room = ui.get_room_input().to_string();
            ui.set_connecting(true);
            ui.set_status(format!("连接 {} @ {} …", server, room).into());
            let weak2 = weak.clone();
            std::thread::spawn(move || {
                let out = aerodesk_core::connect::connect_viewer(&server, &room);
                let ui = weak2.unwrap();
                match out {
                    Ok(r) => {
                        ui.set_status(format!("已加入房间，peer={}", r.peer_id).into());
                        ui.set_log(
                            format!(
                                "信令: {server}\n房间: {room}\n角色: viewer\nSDP 交换: OK\nICE: {}\n\n已建立 WebRTC 会话（媒体收发循环由后续里程碑接入）。",
                                if r.ice_connected { "connected" } else { "pending(5s 超时)" }
                            )
                            .into(),
                        );
                    }
                    Err(e) => {
                        ui.set_status(format!("连接失败：{e}").into());
                        ui.set_log(String::new().into());
                    }
                }
                ui.set_connecting(false);
            });
        }
    });

    ui.on_disconnect({
        let weak = ui.as_weak();
        move || {
            let ui = weak.unwrap();
            ui.set_status("已断开".into());
            ui.set_connecting(false);
        }
    });

    ui.run()
}

fn init_log() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aerodesk_ui=info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
}
