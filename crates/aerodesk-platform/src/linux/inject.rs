//! 输入注入（被控端）。
//!
//! X11：XTestFakeInput（x11rb XTEST 扩展）；Wayland：/dev/uinput（真机阶段）。

use aerodesk_core::protocol::input::{ButtonState, InputEvent, MouseButton};

/// 平台无关键码（协议）→ X11 keysym。
pub fn keysym_for_code(code: &str) -> Option<u32> {
    let ks = match code {
        "KeyA" => 0x61,
        "KeyB" => 0x62,
        "KeyC" => 0x63,
        "KeyD" => 0x64,
        "KeyE" => 0x65,
        "KeyF" => 0x66,
        "KeyG" => 0x67,
        "KeyH" => 0x68,
        "KeyI" => 0x69,
        "KeyJ" => 0x6A,
        "KeyK" => 0x6B,
        "KeyL" => 0x6C,
        "KeyM" => 0x6D,
        "KeyN" => 0x6E,
        "KeyO" => 0x6F,
        "KeyP" => 0x70,
        "KeyQ" => 0x71,
        "KeyR" => 0x72,
        "KeyS" => 0x73,
        "KeyT" => 0x74,
        "KeyU" => 0x75,
        "KeyV" => 0x76,
        "KeyW" => 0x77,
        "KeyX" => 0x78,
        "KeyY" => 0x79,
        "KeyZ" => 0x7A,
        "Digit0" => 0x30,
        "Digit1" => 0x31,
        "Digit2" => 0x32,
        "Digit3" => 0x33,
        "Digit4" => 0x34,
        "Digit5" => 0x35,
        "Digit6" => 0x36,
        "Digit7" => 0x37,
        "Digit8" => 0x38,
        "Digit9" => 0x39,
        "Minus" => 0x2D,
        "Equal" => 0x3D,
        "BracketLeft" => 0x5B,
        "BracketRight" => 0x5D,
        "Backslash" => 0x5C,
        "Semicolon" => 0x3B,
        "Quote" => 0x27,
        "Backquote" => 0x60,
        "Comma" => 0x2C,
        "Period" => 0x2E,
        "Slash" => 0x2F,
        "Enter" => 0xFF0D,
        "Tab" => 0xFF09,
        "Space" => 0x20,
        "Backspace" => 0xFF08,
        "Escape" => 0xFF1B,
        "Delete" => 0xFFFF,
        "ArrowUp" => 0xFF52,
        "ArrowDown" => 0xFF54,
        "ArrowLeft" => 0xFF51,
        "ArrowRight" => 0xFF53,
        "Home" => 0xFF50,
        "End" => 0xFF57,
        "PageUp" => 0xFF55,
        "PageDown" => 0xFF56,
        "ShiftLeft" | "ShiftRight" => 0xFFE1,
        "ControlLeft" | "ControlRight" => 0xFFE3,
        "AltLeft" | "AltRight" => 0xFFE9,
        "MetaLeft" | "MetaRight" => 0xFFEB,
        "CapsLock" => 0xFFE5,
        "F1" => 0xFFBE,
        "F2" => 0xFFBF,
        "F3" => 0xFFC0,
        "F4" => 0xFFC1,
        "F5" => 0xFFC2,
        "F6" => 0xFFC3,
        "F7" => 0xFFC4,
        "F8" => 0xFFC5,
        "F9" => 0xFFC6,
        "F10" => 0xFFC7,
        "F11" => 0xFFC8,
        "F12" => 0xFFC9,
        _ => return None,
    };
    Some(ks)
}

/// XTest fake input 类型码（X11 核心事件码）。
#[cfg(target_os = "linux")]
mod fake_input {
    pub const KEY_PRESS: u8 = 2;
    pub const KEY_RELEASE: u8 = 3;
    pub const BUTTON_PRESS: u8 = 4;
    pub const BUTTON_RELEASE: u8 = 5;
    pub const MOTION_NOTIFY: u8 = 6;
}

/// XTest 注入器（X11 桌面）。
#[cfg(target_os = "linux")]
pub struct XTestInjector {
    conn: x11rb::rust_connection::RustConnection,
    root: x11rb::protocol::xproto::Window,
    width: u32,
    height: u32,
}

#[cfg(target_os = "linux")]
impl XTestInjector {
    pub fn new() -> Result<Self, String> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt;

        let (conn, screen_num) = x11rb::rust_connection::RustConnection::connect(None)
            .map_err(|e| format!("x11 connect: {e}"))?;
        let root = conn.setup().roots[screen_num].root;
        let geo = conn
            .get_geometry(root)
            .map_err(|e| format!("get_geometry: {e:?}"))?
            .reply()
            .map_err(|e| format!("get_geometry reply: {e}"))?;
        let (width, height) = (geo.width.max(1) as u32, geo.height.max(1) as u32);
        Ok(Self {
            conn,
            root,
            width,
            height,
        })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt;
        use x11rb::protocol::xtest::ConnectionExt as _;

        self.conn
            .xtest_fake_input(
                kind,
                detail,
                x11rb::CURRENT_TIME.into(),
                self.root,
                x,
                y,
                0, // deviceid
            )
            .map_err(|e| format!("xtest_fake_input: {e:?}"))?;
        Ok(())
    }

    /// keysym → keycode（XTestFakeInput 需要硬件 keycode；查键盘映射表）。
    fn keycode_for_keysym(&self, keysym: u32) -> Result<u8, String> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::ConnectionExt;

        let reply = self
            .conn
            .get_keyboard_mapping(8, 248) // X11 keycode 固定 8..=255
            .map_err(|e| format!("get_keyboard_mapping: {e:?}"))?
            .reply()
            .map_err(|e| format!("get_keyboard_mapping reply: {e:?}"))?;
        let per = reply.keysyms_per_keycode.max(1) as usize;
        for (i, chunk) in reply.keysyms.chunks(per).enumerate() {
            if chunk.contains(&keysym) {
                return Ok(8 + i as u8);
            }
        }
        Err(format!("keysym 0x{keysym:x} 无对应 keycode"))
    }
}

#[cfg(target_os = "linux")]
impl aerodesk_core::platform::InputInjector for XTestInjector {
    type Error = String;

    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        let to_px = |v: f64| (v.clamp(0.0, 1.0) * self.width as f64) as i16;
        let to_py = |v: f64| (v.clamp(0.0, 1.0) * self.height as f64) as i16;
        match event {
            InputEvent::MouseMove { x, y } => {
                self.fake(fake_input::MOTION_NOTIFY, 0, to_px(*x), to_py(*y))?;
            }
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state,
                x,
                y,
            } => {
                self.fake(fake_input::MOTION_NOTIFY, 0, to_px(*x), to_py(*y))?;
                let kind = if *state == ButtonState::Pressed {
                    fake_input::BUTTON_PRESS
                } else {
                    fake_input::BUTTON_RELEASE
                };
                self.fake(kind, 1, 0, 0)?; // 左键
            }
            InputEvent::MouseButton { .. } => {
                return Err("unsupported button (only left supported)".into());
            }
            InputEvent::Wheel { delta_y, .. } => {
                // X11 滚轮：按钮 4=上 / 5=下。
                let btn = if *delta_y > 0.0 { 4u8 } else { 5u8 };
                self.fake(fake_input::BUTTON_PRESS, btn, 0, 0)?;
                self.fake(fake_input::BUTTON_RELEASE, btn, 0, 0)?;
            }
            InputEvent::Key {
                code,
                state,
                modifiers,
            } => {
                let keysym =
                    keysym_for_code(code).ok_or_else(|| format!("unsupported key code: {code}"))?;
                let keycode = self.keycode_for_keysym(keysym)?;
                let down = *state == ButtonState::Pressed;
                let mods: [u32; 4] = [
                    if modifiers.ctrl { 0xFFE3 } else { 0 },
                    if modifiers.shift { 0xFFE1 } else { 0 },
                    if modifiers.alt { 0xFFE9 } else { 0 },
                    if modifiers.meta { 0xFFEB } else { 0 },
                ];
                if down {
                    for ms in mods.into_iter().filter(|m| *m != 0) {
                        let mc = self.keycode_for_keysym(ms)?;
                        self.fake(fake_input::KEY_PRESS, mc, 0, 0)?;
                    }
                    self.fake(fake_input::KEY_PRESS, keycode, 0, 0)?;
                } else {
                    self.fake(fake_input::KEY_RELEASE, keycode, 0, 0)?;
                    for ms in mods.into_iter().filter(|m| *m != 0) {
                        let mc = self.keycode_for_keysym(ms)?;
                        self.fake(fake_input::KEY_RELEASE, mc, 0, 0)?;
                    }
                }
            }
            InputEvent::Touch { .. } => {
                return Err("linux: touch injection not implemented".into());
            }
            InputEvent::ClipboardText(text) => {
                // 远程剪贴板粘贴：把文本写入被控端本地剪贴板（X11/Wayland 均可，arboard）。
                if !aerodesk_core::clipboard::write(text) {
                    return Err("linux: clipboard write failed".into());
                }
            }
        }
        Ok(())
    }
}

/// 非 Linux 主机上的编译期骨架。
#[cfg(not(target_os = "linux"))]
pub struct XTestInjector;

#[cfg(not(target_os = "linux"))]
impl aerodesk_core::platform::InputInjector for XTestInjector {
    type Error = String;

    fn inject(&mut self, _event: &InputEvent) -> Result<(), String> {
        Err("linux: XTest injection only available on Linux".into())
    }
}

/// uinput 注入器（Wayland / 无 X 环境）：通过 `/dev/uinput` 创建虚拟输入设备，
/// 把远端协议事件注入为 evdev 事件（绝对坐标鼠标 + 按键 + 滚轮）。
///
/// 与 XTestInjector 的关系：X11 会话用 XTest（随 X 连接，无需 root）；
/// Wayland / 无头环境用 uinput（需要 `/dev/uinput` 写权限：root 或 input 组）。
#[cfg(target_os = "linux")]
pub struct UinputInjector {
    file: std::fs::File,
    /// 绝对坐标轴范围（input_absinfo.maximum，坐标按 0..1 归一化映射）。
    abs_max_x: i32,
    abs_max_y: i32,
}

/// Linux evdev 事件码（linux/input-event-codes.h，稳定 ABI）。
#[cfg(target_os = "linux")]
pub mod ev {
    pub const EV_SYN: u16 = 0x00;
    pub const EV_KEY: u16 = 0x01;
    pub const EV_REL: u16 = 0x02;
    pub const EV_ABS: u16 = 0x03;

    pub const SYN_REPORT: u16 = 0x00;

    pub const REL_X: u16 = 0x00;
    pub const REL_Y: u16 = 0x01;
    pub const REL_HWHEEL: u16 = 0x06;
    pub const REL_WHEEL: u16 = 0x08;

    pub const ABS_X: u16 = 0x00;
    pub const ABS_Y: u16 = 0x01;

    pub const BTN_LEFT: u16 = 0x110;
    pub const BTN_RIGHT: u16 = 0x111;
    pub const BTN_MIDDLE: u16 = 0x112;
    pub const BTN_SIDE: u16 = 0x113;
    pub const BTN_EXTRA: u16 = 0x114;

    // 主区按键（KEY_*）：字母/数字/符号/功能键。
    pub const KEY_ESC: u16 = 1;
    pub const KEY_1: u16 = 2;
    pub const KEY_2: u16 = 3;
    pub const KEY_3: u16 = 4;
    pub const KEY_4: u16 = 5;
    pub const KEY_5: u16 = 6;
    pub const KEY_6: u16 = 7;
    pub const KEY_7: u16 = 8;
    pub const KEY_8: u16 = 9;
    pub const KEY_9: u16 = 10;
    pub const KEY_0: u16 = 11;
    pub const KEY_MINUS: u16 = 12;
    pub const KEY_EQUAL: u16 = 13;
    pub const KEY_BACKSPACE: u16 = 14;
    pub const KEY_TAB: u16 = 15;
    pub const KEY_Q: u16 = 16;
    pub const KEY_W: u16 = 17;
    pub const KEY_E: u16 = 18;
    pub const KEY_R: u16 = 19;
    pub const KEY_T: u16 = 20;
    pub const KEY_Y: u16 = 21;
    pub const KEY_U: u16 = 22;
    pub const KEY_I: u16 = 23;
    pub const KEY_O: u16 = 24;
    pub const KEY_P: u16 = 25;
    pub const KEY_LEFTBRACE: u16 = 26;
    pub const KEY_RIGHTBRACE: u16 = 27;
    pub const KEY_ENTER: u16 = 28;
    pub const KEY_LEFTCTRL: u16 = 29;
    pub const KEY_A: u16 = 30;
    pub const KEY_S: u16 = 31;
    pub const KEY_D: u16 = 32;
    pub const KEY_F: u16 = 33;
    pub const KEY_G: u16 = 34;
    pub const KEY_H: u16 = 35;
    pub const KEY_J: u16 = 36;
    pub const KEY_K: u16 = 37;
    pub const KEY_L: u16 = 38;
    pub const KEY_SEMICOLON: u16 = 39;
    pub const KEY_APOSTROPHE: u16 = 40;
    pub const KEY_GRAVE: u16 = 41;
    pub const KEY_LEFTSHIFT: u16 = 42;
    pub const KEY_BACKSLASH: u16 = 43;
    pub const KEY_Z: u16 = 44;
    pub const KEY_X: u16 = 45;
    pub const KEY_C: u16 = 46;
    pub const KEY_V: u16 = 47;
    pub const KEY_B: u16 = 48;
    pub const KEY_N: u16 = 49;
    pub const KEY_M: u16 = 50;
    pub const KEY_COMMA: u16 = 51;
    pub const KEY_DOT: u16 = 52;
    pub const KEY_SLASH: u16 = 53;
    pub const KEY_RIGHTSHIFT: u16 = 54;
    pub const KEY_KPASTERISK: u16 = 55;
    pub const KEY_LEFTALT: u16 = 56;
    pub const KEY_SPACE: u16 = 57;
    pub const KEY_CAPSLOCK: u16 = 58;
    pub const KEY_F1: u16 = 59;
    pub const KEY_F2: u16 = 60;
    pub const KEY_F3: u16 = 61;
    pub const KEY_F4: u16 = 62;
    pub const KEY_F5: u16 = 63;
    pub const KEY_F6: u16 = 64;
    pub const KEY_F7: u16 = 65;
    pub const KEY_F8: u16 = 66;
    pub const KEY_F9: u16 = 67;
    pub const KEY_F10: u16 = 68;
    pub const KEY_F11: u16 = 87;
    pub const KEY_F12: u16 = 88;
    pub const KEY_HOME: u16 = 102;
    pub const KEY_UP: u16 = 103;
    pub const KEY_PAGEUP: u16 = 104;
    pub const KEY_LEFT: u16 = 105;
    pub const KEY_RIGHT: u16 = 106;
    pub const KEY_END: u16 = 107;
    pub const KEY_DOWN: u16 = 108;
    pub const KEY_PAGEDOWN: u16 = 109;
    pub const KEY_DELETE: u16 = 111;
    pub const KEY_RIGHTCTRL: u16 = 97;
    pub const KEY_RIGHTALT: u16 = 100;
    pub const KEY_LEFTMETA: u16 = 125;
    pub const KEY_RIGHTMETA: u16 = 126;
}

/// uinput ioctl 请求号（asm-generic/ioctl.h 宏展开；Linux 稳定 ABI）。
#[cfg(target_os = "linux")]
const fn ioc(dir: u32, typ: u8, nr: u8, size: usize) -> libc::c_ulong {
    const NRBITS: u32 = 8;
    const TYPEBITS: u32 = 8;
    const SIZEBITS: u32 = 14;
    const NRSHIFT: u32 = 0;
    const TYPESHIFT: u32 = NRSHIFT + NRBITS;
    const SIZESHIFT: u32 = TYPESHIFT + TYPEBITS;
    const DIRSHIFT: u32 = SIZESHIFT + SIZEBITS;
    ((dir << DIRSHIFT)
        | ((typ as u32) << TYPESHIFT)
        | ((nr as u32) << NRSHIFT)
        | ((size as u32) << SIZESHIFT)) as libc::c_ulong
}

#[cfg(target_os = "linux")]
const fn _io(typ: u8, nr: u8) -> libc::c_ulong {
    ioc(0, typ, nr, 0)
}

#[cfg(target_os = "linux")]
const fn iow(typ: u8, nr: u8, size: usize) -> libc::c_ulong {
    ioc(1, typ, nr, size) // _IOC_WRITE
}

#[cfg(target_os = "linux")]
pub(crate) const UI_SET_EVBIT: libc::c_ulong = iow(b'U', 100, std::mem::size_of::<i32>());
#[cfg(target_os = "linux")]
pub(crate) const UI_SET_KEYBIT: libc::c_ulong = iow(b'U', 101, std::mem::size_of::<i32>());
#[cfg(target_os = "linux")]
pub(crate) const UI_SET_RELBIT: libc::c_ulong = iow(b'U', 102, std::mem::size_of::<i32>());
#[cfg(target_os = "linux")]
pub(crate) const UI_SET_ABSBIT: libc::c_ulong = iow(b'U', 103, std::mem::size_of::<i32>());
#[cfg(target_os = "linux")]
pub(crate) const UI_DEV_SETUP: libc::c_ulong = iow(b'U', 3, std::mem::size_of::<UinputSetup>());
#[cfg(target_os = "linux")]
pub(crate) const UI_ABS_SETUP: libc::c_ulong = iow(b'U', 5, std::mem::size_of::<UinputAbsSetup>());
#[cfg(target_os = "linux")]
pub(crate) const UI_DEV_CREATE: libc::c_ulong = _io(b'U', 1);
#[cfg(target_os = "linux")]
pub(crate) const UI_DEV_DESTROY: libc::c_ulong = _io(b'U', 2);

/// `struct input_event`（linux/input.h；64 位下 24 字节，32 位下 16 字节）。
#[cfg(target_os = "linux")]
#[repr(C)]
struct InputEventRaw {
    sec: libc::time_t,
    usec: libc::suseconds_t,
    type_: u16,
    code: u16,
    value: i32,
}

/// `struct input_id`（uinput_setup 内嵌）。
#[cfg(target_os = "linux")]
#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

/// `struct uinput_setup`（UINPUT_MAX_NAME_SIZE = 80）。
#[cfg(target_os = "linux")]
#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

/// `struct input_absinfo`。
#[cfg(target_os = "linux")]
#[repr(C)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// `struct uinput_abs_setup`。
#[cfg(target_os = "linux")]
#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsInfo,
}

/// 平台无关键码（协议）→ evdev KEY_* 码。
#[cfg(target_os = "linux")]
fn evdev_code_for_code(code: &str) -> Option<u16> {
    use ev::*;
    let key = match code {
        "KeyA" => KEY_A,
        "KeyB" => KEY_B,
        "KeyC" => KEY_C,
        "KeyD" => KEY_D,
        "KeyE" => KEY_E,
        "KeyF" => KEY_F,
        "KeyG" => KEY_G,
        "KeyH" => KEY_H,
        "KeyI" => KEY_I,
        "KeyJ" => KEY_J,
        "KeyK" => KEY_K,
        "KeyL" => KEY_L,
        "KeyM" => KEY_M,
        "KeyN" => KEY_N,
        "KeyO" => KEY_O,
        "KeyP" => KEY_P,
        "KeyQ" => KEY_Q,
        "KeyR" => KEY_R,
        "KeyS" => KEY_S,
        "KeyT" => KEY_T,
        "KeyU" => KEY_U,
        "KeyV" => KEY_V,
        "KeyW" => KEY_W,
        "KeyX" => KEY_X,
        "KeyY" => KEY_Y,
        "KeyZ" => KEY_Z,
        "Digit0" => KEY_0,
        "Digit1" => KEY_1,
        "Digit2" => KEY_2,
        "Digit3" => KEY_3,
        "Digit4" => KEY_4,
        "Digit5" => KEY_5,
        "Digit6" => KEY_6,
        "Digit7" => KEY_7,
        "Digit8" => KEY_8,
        "Digit9" => KEY_9,
        "Minus" => KEY_MINUS,
        "Equal" => KEY_EQUAL,
        "BracketLeft" => KEY_LEFTBRACE,
        "BracketRight" => KEY_RIGHTBRACE,
        "Backslash" => KEY_BACKSLASH,
        "Semicolon" => KEY_SEMICOLON,
        "Quote" => KEY_APOSTROPHE,
        "Backquote" => KEY_GRAVE,
        "Comma" => KEY_COMMA,
        "Period" => KEY_DOT,
        "Slash" => KEY_SLASH,
        "Enter" => KEY_ENTER,
        "Tab" => KEY_TAB,
        "Space" => KEY_SPACE,
        "Backspace" => KEY_BACKSPACE,
        "Escape" => KEY_ESC,
        "Delete" => KEY_DELETE,
        "ArrowUp" => KEY_UP,
        "ArrowDown" => KEY_DOWN,
        "ArrowLeft" => KEY_LEFT,
        "ArrowRight" => KEY_RIGHT,
        "Home" => KEY_HOME,
        "End" => KEY_END,
        "PageUp" => KEY_PAGEUP,
        "PageDown" => KEY_PAGEDOWN,
        "ShiftLeft" => KEY_LEFTSHIFT,
        "ShiftRight" => KEY_RIGHTSHIFT,
        "ControlLeft" => KEY_LEFTCTRL,
        "ControlRight" => KEY_RIGHTCTRL,
        "AltLeft" => KEY_LEFTALT,
        "AltRight" => KEY_RIGHTALT,
        "MetaLeft" => KEY_LEFTMETA,
        "MetaRight" => KEY_RIGHTMETA,
        "CapsLock" => KEY_CAPSLOCK,
        "F1" => KEY_F1,
        "F2" => KEY_F2,
        "F3" => KEY_F3,
        "F4" => KEY_F4,
        "F5" => KEY_F5,
        "F6" => KEY_F6,
        "F7" => KEY_F7,
        "F8" => KEY_F8,
        "F9" => KEY_F9,
        "F10" => KEY_F10,
        "F11" => KEY_F11,
        "F12" => KEY_F12,
        _ => return None,
    };
    Some(key)
}

#[cfg(target_os = "linux")]
fn btn_evdev(button: MouseButton) -> u16 {
    use ev::*;
    match button {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Back => BTN_SIDE,
        MouseButton::Forward => BTN_EXTRA,
    }
}

/// 归一化坐标 → 绝对轴值（0..1 → 0..=max）。
#[cfg(target_os = "linux")]
fn map_abs(v: f64, max: i32) -> i32 {
    (v.clamp(0.0, 1.0) * max as f64).round() as i32
}

#[cfg(target_os = "linux")]
impl UinputInjector {
    /// 打开 `/dev/uinput` 并创建虚拟设备（EV_KEY + EV_ABS 绝对指针 + EV_REL 滚轮）。
    ///
    /// 权限要求：root，或 `input` 组写 `/dev/uinput`（常见发行版默认 root:input 660）。
    /// 可用环境变量 `AERODESK_UINPUT` 覆盖设备路径（测试/容器映射场景）。
    pub fn new() -> Result<Self, String> {
        use std::os::fd::AsRawFd;

        let path = std::env::var("AERODESK_UINPUT").unwrap_or_else(|_| "/dev/uinput".to_string());
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(|e| format!("open {path}: {e}"))?;
        let fd = file.as_raw_fd();

        let abs_max_x = 32_767_i32;
        let abs_max_y = 32_767_i32;

        // 事件类型：按键 / 绝对坐标 / 相对滚轮 / 同步。
        for bit in [
            ev::EV_KEY as i32,
            ev::EV_ABS as i32,
            ev::EV_REL as i32,
            ev::EV_SYN as i32,
        ] {
            let r = unsafe { libc::ioctl(fd, UI_SET_EVBIT, bit) };
            if r < 0 {
                return Err(format!(
                    "uinput UI_SET_EVBIT({bit}): {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        // 绝对坐标轴。
        for bit in [ev::ABS_X as i32, ev::ABS_Y as i32] {
            let r = unsafe { libc::ioctl(fd, UI_SET_ABSBIT, bit) };
            if r < 0 {
                return Err(format!(
                    "uinput UI_SET_ABSBIT({bit}): {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        // 滚轮相对轴。
        for bit in [ev::REL_WHEEL as i32, ev::REL_HWHEEL as i32] {
            let r = unsafe { libc::ioctl(fd, UI_SET_RELBIT, bit) };
            if r < 0 {
                return Err(format!(
                    "uinput UI_SET_RELBIT({bit}): {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        // 全部按键位（含鼠标键 + 修饰键 + F 键；KEY_MAX 内按需枚举）。
        for bit in 1..=0x2ffu16 {
            let r = unsafe { libc::ioctl(fd, UI_SET_KEYBIT, bit as i32) };
            if r < 0 {
                return Err(format!(
                    "uinput UI_SET_KEYBIT({bit}): {}",
                    std::io::Error::last_os_error()
                ));
            }
        }

        let mut setup = UinputSetup {
            id: InputId {
                bustype: 0x03,  // BUS_USB
                vendor: 0x1d6b, // Linux
                product: 0x0001,
                version: 1,
            },
            name: {
                let mut name = [0u8; 80];
                let b = b"aerodesk-virtual-input";
                name[..b.len()].copy_from_slice(b);
                name
            },
            ff_effects_max: 0,
        };
        let r = unsafe { libc::ioctl(fd, UI_DEV_SETUP, &mut setup) };
        if r < 0 {
            return Err(format!(
                "uinput UI_DEV_SETUP: {}",
                std::io::Error::last_os_error()
            ));
        }

        let abs_x = UinputAbsSetup {
            code: ev::ABS_X,
            absinfo: InputAbsInfo {
                value: 0,
                minimum: 0,
                maximum: abs_max_x,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            },
        };
        let abs_y = UinputAbsSetup {
            code: ev::ABS_Y,
            absinfo: InputAbsInfo {
                value: 0,
                minimum: 0,
                maximum: abs_max_y,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            },
        };
        for setup in [&abs_x, &abs_y] {
            let r = unsafe { libc::ioctl(fd, UI_ABS_SETUP, setup) };
            if r < 0 {
                return Err(format!(
                    "uinput UI_ABS_SETUP: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }

        let r = unsafe { libc::ioctl(fd, UI_DEV_CREATE) };
        if r < 0 {
            return Err(format!(
                "uinput UI_DEV_CREATE: {}",
                std::io::Error::last_os_error()
            ));
        }

        Ok(Self {
            file,
            abs_max_x,
            abs_max_y,
        })
    }

    fn write_event(&self, type_: u16, code: u16, value: i32) -> Result<(), String> {
        use std::io::Write;
        let ev = InputEventRaw {
            sec: 0,
            usec: 0,
            type_,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&ev as *const InputEventRaw).cast::<u8>(),
                std::mem::size_of::<InputEventRaw>(),
            )
        };
        let mut file = &self.file;
        file.write_all(bytes)
            .map_err(|e| format!("uinput write: {e}"))
    }

    fn sync(&self) -> Result<(), String> {
        self.write_event(ev::EV_SYN, ev::SYN_REPORT, 0)
    }

    fn abs_value(&self, v: f64, max: i32) -> i32 {
        map_abs(v, max)
    }

    fn move_abs(&self, x: f64, y: f64) -> Result<(), String> {
        self.write_event(ev::EV_ABS, ev::ABS_X, self.abs_value(x, self.abs_max_x))?;
        self.write_event(ev::EV_ABS, ev::ABS_Y, self.abs_value(y, self.abs_max_y))?;
        self.sync()
    }

    fn key_event(&self, code: u16, pressed: bool) -> Result<(), String> {
        self.write_event(ev::EV_KEY, code, i32::from(pressed))?;
        self.sync()
    }
}

#[cfg(target_os = "linux")]
impl Drop for UinputInjector {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

#[cfg(target_os = "linux")]
impl aerodesk_core::platform::InputInjector for UinputInjector {
    type Error = String;

    fn inject(&mut self, event: &InputEvent) -> Result<(), Self::Error> {
        match event {
            InputEvent::MouseMove { x, y } => self.move_abs(*x, *y),
            InputEvent::MouseButton {
                button,
                state,
                x,
                y,
            } => {
                self.move_abs(*x, *y)?;
                self.key_event(btn_evdev(*button), *state == ButtonState::Pressed)
            }
            InputEvent::Wheel {
                delta_x, delta_y, ..
            } => {
                let dy = delta_y.clamp(-1.0, 1.0).round() as i32;
                let dx = delta_x.clamp(-1.0, 1.0).round() as i32;
                if dy != 0 {
                    self.write_event(ev::EV_REL, ev::REL_WHEEL, dy)?;
                }
                if dx != 0 {
                    self.write_event(ev::EV_REL, ev::REL_HWHEEL, dx)?;
                }
                self.sync()
            }
            InputEvent::Key {
                code,
                state,
                modifiers,
            } => {
                let keycode = evdev_code_for_code(code)
                    .ok_or_else(|| format!("unsupported key code: {code}"))?;
                let down = *state == ButtonState::Pressed;
                let mods: [(u16, bool); 4] = [
                    (ev::KEY_LEFTCTRL, modifiers.ctrl),
                    (ev::KEY_LEFTSHIFT, modifiers.shift),
                    (ev::KEY_LEFTALT, modifiers.alt),
                    (ev::KEY_LEFTMETA, modifiers.meta),
                ];
                if down {
                    for (m, _on) in mods.into_iter().filter(|(_, on)| *on) {
                        self.key_event(m, true)?;
                    }
                    self.key_event(keycode, true)?;
                } else {
                    self.key_event(keycode, false)?;
                    for (m, _on) in mods.into_iter().filter(|(_, on)| *on) {
                        self.key_event(m, false)?;
                    }
                }
                Ok(())
            }
            InputEvent::Touch { .. } => Err("linux: uinput touch injection not implemented".into()),
            InputEvent::ClipboardText(text) => {
                if !aerodesk_core::clipboard::write(text) {
                    Err("linux: uinput clipboard write failed".into())
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// 非 Linux 主机上的编译期骨架。
#[cfg(not(target_os = "linux"))]
pub struct UinputInjector;

#[cfg(not(target_os = "linux"))]
impl aerodesk_core::platform::InputInjector for UinputInjector {
    type Error = String;

    fn inject(&mut self, _event: &InputEvent) -> Result<(), String> {
        Err("linux: uinput injection only available on Linux".into())
    }
}

#[cfg(test)]
mod tests {
    use super::keysym_for_code;

    #[test]
    fn keysym_map_covers_letters_digits_and_common_keys() {
        assert_eq!(keysym_for_code("KeyA"), Some(0x61));
        assert_eq!(keysym_for_code("KeyZ"), Some(0x7A));
        assert_eq!(keysym_for_code("Digit0"), Some(0x30));
        assert_eq!(keysym_for_code("Enter"), Some(0xFF0D));
        assert_eq!(keysym_for_code("Space"), Some(0x20));
        assert_eq!(keysym_for_code("ArrowUp"), Some(0xFF52));
        assert_eq!(keysym_for_code("ControlLeft"), Some(0xFFE3));
        assert_eq!(keysym_for_code("F12"), Some(0xFFC9));
        assert_eq!(keysym_for_code("NotAKey"), None);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    #[test]
    fn evdev_map_covers_protocol_keys() {
        // 字母/数字/常见键与 XTest keysym 表同源覆盖。
        assert_eq!(evdev_code_for_code("KeyA"), Some(ev::KEY_A));
        assert_eq!(evdev_code_for_code("KeyZ"), Some(ev::KEY_Z));
        assert_eq!(evdev_code_for_code("Digit0"), Some(ev::KEY_0));
        assert_eq!(evdev_code_for_code("Digit9"), Some(ev::KEY_9));
        assert_eq!(evdev_code_for_code("Enter"), Some(ev::KEY_ENTER));
        assert_eq!(evdev_code_for_code("Space"), Some(ev::KEY_SPACE));
        assert_eq!(evdev_code_for_code("ArrowUp"), Some(ev::KEY_UP));
        assert_eq!(evdev_code_for_code("ControlLeft"), Some(ev::KEY_LEFTCTRL));
        assert_eq!(evdev_code_for_code("MetaRight"), Some(ev::KEY_RIGHTMETA));
        assert_eq!(evdev_code_for_code("F12"), Some(ev::KEY_F12));
        assert_eq!(evdev_code_for_code("NotAKey"), None);
    }

    #[test]
    fn input_event_layout_is_kernel_abi() {
        // 64 位 Linux：struct input_event = 2×i64 + u16 + u16 + i32 = 24 字节。
        if std::mem::size_of::<libc::c_long>() == 8 {
            assert_eq!(std::mem::size_of::<InputEventRaw>(), 24);
        } else {
            assert_eq!(std::mem::size_of::<InputEventRaw>(), 16);
        }
        // uinput_setup = input_id(8) + name(80) + u32 = 92；对齐后 92。
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        // uinput_abs_setup = u16 + padding(2) + input_absinfo(24) = 28。
        assert_eq!(std::mem::size_of::<UinputAbsSetup>(), 28);
        assert_eq!(std::mem::size_of::<InputAbsInfo>(), 24);
    }

    #[test]
    fn ioctl_numbers_match_linux_headers() {
        // asm-generic/ioctl.h 手算基准（_IO 无方向位；_IOW 带 _IOC_WRITE=1<<30）：
        // UI_DEV_CREATE = _IO('U',1) = 0x5501；UI_DEV_DESTROY = _IO('U',2) = 0x5502。
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        // UI_SET_EVBIT = _IOW('U',100,sizeof(int))：(1<<30) | 0x5500 | 100 | (4<<16)。
        assert_eq!(UI_SET_EVBIT, 0x4000_0000 | 0x5500 | 100 | (4 << 16));
        // UI_SET_ABSBIT = _IOW('U',103,sizeof(int))。
        assert_eq!(UI_SET_ABSBIT, 0x4000_0000 | 0x5500 | 103 | (4 << 16));
        // UI_DEV_SETUP = _IOW('U',3,sizeof(uinput_setup))。
        assert_eq!(
            UI_DEV_SETUP,
            0x4000_0000 | 0x5500 | 3 | (std::mem::size_of::<UinputSetup>() as u64) << 16
        );
    }

    #[test]
    fn normalized_coords_map_to_abs_range() {
        assert_eq!(map_abs(0.0, 32_767), 0);
        assert_eq!(map_abs(1.0, 32_767), 32_767);
        assert_eq!(map_abs(0.5, 32_767), 16_384);
        assert_eq!(map_abs(-1.0, 32_767), 0);
        assert_eq!(map_abs(2.0, 32_767), 32_767);
        assert_eq!(map_abs(0.0, 100), 0);
        assert_eq!(map_abs(1.0, 100), 100);
    }
}
