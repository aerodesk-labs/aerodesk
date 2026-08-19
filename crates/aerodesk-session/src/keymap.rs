//! 观看端键盘映射：Slint `KeyEvent.text` → 平台无关键码。
//!
//! Slint 普通按键的 `text` 是实际字符（如 `a`/`A`/`1`/`!`），特殊键是私有
//! Unicode 码位（见 `i-slint-common::key_codes`，`Key.Escape` 等命名空间常量）。
//! 输出与 `aerodesk_platform::macos::inject::keycode_for_code` 对齐；无法映射返回 `None`，
//! 调用方应 `reject` 让本地 UI 继续处理（例如 F13+、输入法组合键）。

use aerodesk_core::protocol::input::Modifiers;

/// 把 Slint `KeyEvent.text` 映射为平台无关键码（`"KeyA"`/`"Digit1"`/`"Enter"`/`"F5"`…）。
pub fn key_code_for_text(text: &str) -> Option<&'static str> {
    if text.len() == 1 {
        let ch = text.as_bytes()[0];
        if ch.is_ascii_alphabetic() {
            const KEYS: [&str; 26] = [
                "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ",
                "KeyK", "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT",
                "KeyU", "KeyV", "KeyW", "KeyX", "KeyY", "KeyZ",
            ];
            return KEYS.get((ch.to_ascii_uppercase() - b'A') as usize).copied();
        }
        if ch.is_ascii_digit() {
            const DIGITS: [&str; 10] = [
                "Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7",
                "Digit8", "Digit9",
            ];
            return DIGITS.get((ch - b'0') as usize).copied();
        }
        // 标点/符号：Shift 符号映射回基键，修饰位随事件单独发送。
        // 控制字符/私有码位不在此处理，落到下方特殊键匹配。
        match ch {
            b'-' | b'_' => return Some("Minus"),
            b'=' | b'+' => return Some("Equal"),
            b'[' | b'{' => return Some("BracketLeft"),
            b']' | b'}' => return Some("BracketRight"),
            b'\\' | b'|' => return Some("Backslash"),
            b';' | b':' => return Some("Semicolon"),
            b'\'' | b'"' => return Some("Quote"),
            b'`' | b'~' => return Some("Backquote"),
            b',' | b'<' => return Some("Comma"),
            b'.' | b'>' => return Some("Period"),
            b'/' | b'?' => return Some("Slash"),
            b'!' => return Some("Digit1"),
            b'@' => return Some("Digit2"),
            b'#' => return Some("Digit3"),
            b'$' => return Some("Digit4"),
            b'%' => return Some("Digit5"),
            b'^' => return Some("Digit6"),
            b'&' => return Some("Digit7"),
            b'*' => return Some("Digit8"),
            b'(' => return Some("Digit9"),
            b')' => return Some("Digit0"),
            _ => {}
        }
    }
    // Slint 特殊键私有 Unicode 码位（i-slint-common key_codes.rs）。
    match text {
        "\u{8}" => Some("Backspace"),
        "\u{9}" => Some("Tab"),
        "\u{a}" => Some("Enter"),
        "\u{1b}" => Some("Escape"),
        "\u{7f}" => Some("Delete"),
        "\u{10}" => Some("ShiftLeft"),
        "\u{11}" => Some("ControlLeft"),
        "\u{12}" => Some("AltLeft"),
        "\u{14}" => Some("CapsLock"),
        "\u{15}" => Some("ShiftRight"),
        "\u{16}" => Some("ControlRight"),
        "\u{17}" => Some("MetaLeft"),
        "\u{18}" => Some("MetaRight"),
        "\u{20}" => Some("Space"),
        "\u{f700}" => Some("ArrowUp"),
        "\u{f701}" => Some("ArrowDown"),
        "\u{f702}" => Some("ArrowLeft"),
        "\u{f703}" => Some("ArrowRight"),
        "\u{f704}" => Some("F1"),
        "\u{f705}" => Some("F2"),
        "\u{f706}" => Some("F3"),
        "\u{f707}" => Some("F4"),
        "\u{f708}" => Some("F5"),
        "\u{f709}" => Some("F6"),
        "\u{f70a}" => Some("F7"),
        "\u{f70b}" => Some("F8"),
        "\u{f70c}" => Some("F9"),
        "\u{f70d}" => Some("F10"),
        "\u{f70e}" => Some("F11"),
        "\u{f70f}" => Some("F12"),
        "\u{f729}" => Some("Home"),
        "\u{f72b}" => Some("End"),
        "\u{f72c}" => Some("PageUp"),
        "\u{f72d}" => Some("PageDown"),
        _ => None,
    }
}

/// #496 G1/G3：Slint 在 macOS 把 Control↔Super 的键码文本互换（物理 Cmd 到达
/// 时文本是 ControlLeft）。main.rs 对 flags 做了 ctrl↔meta 交换，键码必须同步
/// 交换，否则 wire 键码与 flags 矛盾——被控端按键码+flag 双重注入，释放时
/// flags 已空，修饰键卡死。非 macOS 平台不得调用（Slint 不交换）。
pub fn macos_swap_control_meta(code: &'static str) -> &'static str {
    match code {
        "ControlLeft" => "MetaLeft",
        "ControlRight" => "MetaRight",
        "MetaLeft" => "ControlLeft",
        "MetaRight" => "ControlRight",
        _ => code,
    }
}

/// #496 G2：跨端修饰键翻译（三态开关：0=直通/物理保真，1=翻译到 Windows，
/// 2=翻译到 macOS）——按目标 OS 的习惯映射修饰键，复制/粘贴/剪切/撤销等
/// 快捷键跨端可用。
/// - 目标 Windows：meta（mac Cmd / Win 键）→ ctrl；Meta 键码 → Control 键码；
/// - 目标 macOS：ctrl → meta（Command）；Control 键码 → Meta 键码。
///
/// 键码交换不看 flag（释放事件的 flags 已空，按码位交换才对称）；
/// 码位非修饰键时仅翻译 flags。两修饰键同按的边角（如 Ctrl+Cmd）不追求完美。
pub fn translate_cross_end(
    code: &'static str,
    modifiers: &Modifiers,
    target: u8,
) -> (&'static str, Modifiers) {
    match target {
        1 => (
            match code {
                "MetaLeft" => "ControlLeft",
                "MetaRight" => "ControlRight",
                _ => code,
            },
            Modifiers {
                ctrl: modifiers.ctrl || modifiers.meta,
                meta: false,
                ..*modifiers
            },
        ),
        2 => (
            match code {
                "ControlLeft" => "MetaLeft",
                "ControlRight" => "MetaRight",
                _ => code,
            },
            Modifiers {
                ctrl: false,
                meta: modifiers.meta || modifiers.ctrl,
                ..*modifiers
            },
        ),
        _ => (code, *modifiers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits() {
        assert_eq!(key_code_for_text("a"), Some("KeyA"));
        assert_eq!(key_code_for_text("A"), Some("KeyA"));
        assert_eq!(key_code_for_text("z"), Some("KeyZ"));
        assert_eq!(key_code_for_text("Z"), Some("KeyZ"));
        assert_eq!(key_code_for_text("0"), Some("Digit0"));
        assert_eq!(key_code_for_text("9"), Some("Digit9"));
    }

    #[test]
    fn punctuation_maps_to_base_key() {
        assert_eq!(key_code_for_text("-"), Some("Minus"));
        assert_eq!(key_code_for_text("_"), Some("Minus"));
        assert_eq!(key_code_for_text("!"), Some("Digit1"));
        assert_eq!(key_code_for_text("("), Some("Digit9"));
        assert_eq!(key_code_for_text("."), Some("Period"));
        assert_eq!(key_code_for_text(">"), Some("Period"));
        assert_eq!(key_code_for_text("/"), Some("Slash"));
        assert_eq!(key_code_for_text("?"), Some("Slash"));
        assert_eq!(key_code_for_text("\""), Some("Quote"));
    }

    #[test]
    fn slint_special_keys() {
        // 与 i-slint-common key_codes 私有码位一致。
        assert_eq!(key_code_for_text("\u{8}"), Some("Backspace"));
        assert_eq!(key_code_for_text("\u{9}"), Some("Tab"));
        assert_eq!(key_code_for_text("\u{a}"), Some("Enter"));
        assert_eq!(key_code_for_text("\u{1b}"), Some("Escape"));
        assert_eq!(key_code_for_text("\u{7f}"), Some("Delete"));
        assert_eq!(key_code_for_text("\u{20}"), Some("Space"));
        assert_eq!(key_code_for_text("\u{f700}"), Some("ArrowUp"));
        assert_eq!(key_code_for_text("\u{f704}"), Some("F1"));
        assert_eq!(key_code_for_text("\u{f70f}"), Some("F12"));
        assert_eq!(key_code_for_text("\u{f729}"), Some("Home"));
        assert_eq!(key_code_for_text("\u{f72d}"), Some("PageDown"));
        assert_eq!(key_code_for_text("\u{10}"), Some("ShiftLeft"));
        assert_eq!(key_code_for_text("\u{17}"), Some("MetaLeft"));
    }

    #[test]
    fn unmapped_returns_none() {
        assert_eq!(key_code_for_text(""), None);
        assert_eq!(key_code_for_text("é"), None);
        assert_eq!(key_code_for_text("\u{f735}"), None); // Menu：注入层不支持
        assert_eq!(key_code_for_text("abc"), None);
    }

    /// #496：mac 交换映射——Control↔Meta 四种键码互换，其余原样。
    #[test]
    fn macos_swap_control_meta_swaps_four_codes_only() {
        assert_eq!(macos_swap_control_meta("ControlLeft"), "MetaLeft");
        assert_eq!(macos_swap_control_meta("ControlRight"), "MetaRight");
        assert_eq!(macos_swap_control_meta("MetaLeft"), "ControlLeft");
        assert_eq!(macos_swap_control_meta("MetaRight"), "ControlRight");
        assert_eq!(macos_swap_control_meta("KeyA"), "KeyA");
        assert_eq!(macos_swap_control_meta("ShiftLeft"), "ShiftLeft");
        assert_eq!(macos_swap_control_meta("AltRight"), "AltRight");
    }

    /// #496 G2：翻译三态——直通、翻译到 Windows、翻译到 macOS。
    #[test]
    fn translate_cross_end_maps_modifiers_by_target() {
        let mods = |ctrl: bool, meta: bool| Modifiers {
            ctrl,
            shift: false,
            alt: false,
            meta,
        };
        // 直通：原样。
        let (c, m) = translate_cross_end("KeyC", &mods(true, false), 0);
        assert_eq!((c, m.ctrl, m.meta), ("KeyC", true, false));
        // mac 主控 Cmd+C → Windows：{KeyC, meta} → {KeyC, ctrl}。
        let (c, m) = translate_cross_end("KeyC", &mods(false, true), 1);
        assert_eq!((c, m.ctrl, m.meta), ("KeyC", true, false));
        // mac 主控 Cmd 裸按 → Windows：{MetaLeft, meta} → {ControlLeft, ctrl}。
        let (c, m) = translate_cross_end("MetaLeft", &mods(false, true), 1);
        assert_eq!((c, m.ctrl, m.meta), ("ControlLeft", true, false));
        // 释放对称：{MetaLeft, 无 flag} → {ControlLeft, 无 flag}（不卡键）。
        let (c, m) = translate_cross_end("MetaLeft", &mods(false, false), 1);
        assert_eq!((c, m.ctrl, m.meta), ("ControlLeft", false, false));
        // Win 主控 Ctrl+C → macOS：{KeyC, ctrl} → {KeyC, meta}。
        let (c, m) = translate_cross_end("KeyC", &mods(true, false), 2);
        assert_eq!((c, m.ctrl, m.meta), ("KeyC", false, true));
        // Win 主控 Ctrl 裸按 → macOS：{ControlLeft, ctrl} → {MetaLeft, meta}。
        let (c, m) = translate_cross_end("ControlLeft", &mods(true, false), 2);
        assert_eq!((c, m.ctrl, m.meta), ("MetaLeft", false, true));
        // 释放对称（target 2）。
        let (c, m) = translate_cross_end("ControlLeft", &mods(false, false), 2);
        assert_eq!((c, m.ctrl, m.meta), ("MetaLeft", false, false));
        // Win 键裸按 → macOS：{MetaLeft, meta} 保持原样（Command）。
        let (c, m) = translate_cross_end("MetaLeft", &mods(false, true), 2);
        assert_eq!((c, m.ctrl, m.meta), ("MetaLeft", false, true));
    }
}
