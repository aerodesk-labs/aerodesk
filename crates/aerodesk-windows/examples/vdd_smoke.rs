//! Parsec VDD 真机冒烟（ADR-0001）：安装驱动后运行本示例验证
//! 虚拟屏 add/remove 生命周期 + 心跳保活。
//!
//! ```sh
//! # Windows（管理员）：
//! ./target/release/examples/vdd_smoke.exe            # 默认 3840x2160@60
//! ./target/release/examples/vdd_smoke.exe 1920 1080 60
//! ```

fn main() {
    #[cfg(windows)]
    {
        use aerodesk_windows::vdd::VirtualDisplayManager;
        use std::time::Duration;

        let width: u32 = std::env::args()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(3840);
        let height: u32 = std::env::args()
            .nth(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(2160);
        let hz: u32 = std::env::args()
            .nth(3)
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);

        println!("vdd smoke: driver check + add {width}x{height}@{hz}");
        let mut vdd = match VirtualDisplayManager::new() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("FATAL: {e}");
                eprintln!(
                    "hint: 安装 Parsec VDD 驱动（管理员 PowerShell: .\\scripts\\windows-vdd-smoke.ps1 -Install）"
                );
                std::process::exit(1);
            }
        };
        match vdd.add_display(width, height, hz) {
            Ok(idx) => println!(
                "OK added virtual display index={idx} (count={})",
                vdd.display_count()
            ),
            Err(e) => {
                eprintln!("FATAL: add failed: {e}");
                std::process::exit(1);
            }
        }
        println!("keeping display alive 3s (heartbeat thread) ...");
        std::thread::sleep(Duration::from_secs(3));
        // Drop 会停心跳 + 逆序移除全部虚拟屏 + 关句柄
        drop(vdd);
        println!("OK removed virtual displays (Drop cleanup)");
    }
    #[cfg(not(windows))]
    {
        println!("vdd smoke is Windows-only (Parsec VDD); run on a Windows 10/11 machine");
    }
}
