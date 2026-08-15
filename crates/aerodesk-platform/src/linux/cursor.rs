//! 被控端光标读取（#75）：X11 QueryPointer → 归一化 0..1（供观看端叠加层）。
//!
//! 仅 X11 会话（DISPLAY）可用；Wayland 原生会话无 X11 时 `position_normalized`
//! 返回 None，上层不发光标（观看端保留本地光标）。

use aerodesk_core::platform::CursorSource;

/// 归一化：root 尺寸内光标坐标 → 0..1（与 macOS 主显示器归一化约定一致）。
fn normalize(width: u16, height: u16, x: i16, y: i16) -> Option<(f64, f64)> {
    let (w, h) = (width as f64, height as f64);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let nx = (x as f64 / w).clamp(0.0, 1.0);
    let ny = (y as f64 / h).clamp(0.0, 1.0);
    Some((nx, ny))
}

/// Linux 光标源（X11 QueryPointer，root window 归一化）。
#[cfg(target_os = "linux")]
pub struct LinuxCursor {
    conn: Option<x11rb::rust_connection::RustConnection>,
    root: x11rb::protocol::xproto::Window,
}

#[cfg(target_os = "linux")]
impl LinuxCursor {
    /// 连接当前 DISPLAY；无 X11（Wayland 原生/无显示）时返回可用的空实现。
    pub fn new() -> Self {
        use x11rb::connection::Connection;
        let (conn, screen_num) = match x11rb::rust_connection::RustConnection::connect(None) {
            Ok(v) => v,
            Err(_) => {
                return Self {
                    conn: None,
                    root: 0,
                };
            }
        };
        let root = conn.setup().roots[screen_num].root;
        Self {
            conn: Some(conn),
            root,
        }
    }
}

#[cfg(target_os = "linux")]
impl Default for LinuxCursor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
impl CursorSource for LinuxCursor {
    fn position_normalized(&mut self) -> Option<(f64, f64)> {
        use x11rb::protocol::xproto::ConnectionExt;
        let conn = self.conn.as_ref()?;
        let geo = conn.get_geometry(self.root).ok()?.reply().ok()?;
        let ptr = conn.query_pointer(self.root).ok()?.reply().ok()?;
        normalize(geo.width, geo.height, ptr.root_x, ptr.root_y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_center() {
        assert_eq!(normalize(1024, 768, 512, 384), Some((0.5, 0.5)));
    }

    #[test]
    fn normalize_corners_and_clamp() {
        assert_eq!(normalize(1024, 768, 0, 0), Some((0.0, 0.0)));
        // 越界坐标（多显示器负区/超界）收敛到 0..1。
        assert_eq!(normalize(1024, 768, -100, 2000), Some((0.0, 1.0)));
    }

    #[test]
    fn normalize_zero_size_returns_none() {
        assert_eq!(normalize(0, 768, 10, 10), None);
        assert_eq!(normalize(1024, 0, 10, 10), None);
    }
}
