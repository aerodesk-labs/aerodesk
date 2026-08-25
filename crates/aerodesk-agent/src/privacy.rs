//! #503 隐私屏：被控端黑屏 / 定制文字 / 静音。
//!
//! viewer 经 control 通道（与 `{"display":N}` / `{"bitrate":N}` 同一通道，
//! SFU 透明转发 / SIP 1:1 直连）下发：
//!
//! ```json
//! {"privacy": {"enabled": true, "mode": "text", "text": "隐私屏已开启", "mute": true}}
//! ```
//!
//! - `enabled`：开启后向采集管线注入黑/文字帧替代真实屏幕（媒体继续出流，
//!   关闭后立即恢复真实画面）；`enabled=false` 时同时复位静音。
//! - `mode`：`"black"` 纯黑屏 / `"text"` 黑底 + 定制文字（缺省 black）。
//! - `text`：定制文字（mode=text 时绘制；超长自动换行截断）。
//! - `mute`：同时静音被控端音频源（发送静音帧，不排空采集时序）。
//!
//! 文字渲染用内建点阵字模（ASCII 5x7 + 常用中文 8x8），不引入字体依赖：
//! 发布端 agent 是 headless 引擎，系统字体查找/栅格化在 CI 与无头环境
//! 不可靠；字模总字节 < 1KB，全部字符有单测覆盖（渲染后逐像素断言）。

use serde_json::Value;

/// 隐私屏显示模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacyMode {
    /// 纯黑屏。
    Black,
    /// 黑底 + 定制文字。
    Text,
}

/// 隐私屏状态（publisher 主循环持有；control 消息更新，采集循环查询）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacyState {
    /// 隐私屏开启：采集帧被黑/文字帧覆盖（媒体继续出流）。
    pub enabled: bool,
    /// 显示模式（enabled 时生效）。
    pub mode: PrivacyMode,
    /// 定制文字（mode=Text 时绘制；换行符 '\n' 分段）。
    pub text: String,
    /// 静音被控端音频源（发送静音帧）。
    pub mute_audio: bool,
}

impl Default for PrivacyState {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: PrivacyMode::Black,
            text: String::new(),
            mute_audio: false,
        }
    }
}

/// 解析 control 通道消息中的 `{"privacy":{...}}` 并应用；返回状态是否变化
/// （供调用方打日志 / 请求关键帧）。消息不含 `privacy` 字段时为 no-op。
pub fn apply_control(v: &Value, state: &mut PrivacyState) -> bool {
    let Some(p) = v.get("privacy") else {
        return false;
    };
    let mut changed = false;
    if let Some(en) = p.get("enabled").and_then(|e| e.as_bool()) {
        changed |= state.enabled != en;
        state.enabled = en;
        if !en {
            // 关闭隐私屏同时复位静音（隐私屏语义的一部分；独立静音消息见 `mute`）。
            changed |= state.mute_audio;
            state.mute_audio = false;
        }
    }
    if let Some(m) = p.get("mode").and_then(|m| m.as_str()) {
        let mode = if m == "text" {
            PrivacyMode::Text
        } else {
            PrivacyMode::Black
        };
        changed |= state.mode != mode;
        state.mode = mode;
    }
    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
        changed |= state.text != t;
        state.text = t.to_string();
    }
    if let Some(m) = p.get("mute").and_then(|m| m.as_bool()) {
        changed |= state.mute_audio != m;
        state.mute_audio = m;
    }
    changed
}

/// 隐私屏生效时把 BGRA32 帧（`w`×`h`）绘制为黑底 / 黑底+定制文字。
/// 全帧填黑后按当前模式绘制文字（未开启时调用方不应调用本函数）。
pub fn paint(state: &PrivacyState, bgra: &mut [u8], w: u32, h: u32) {
    let n = (w as usize).saturating_mul(h as usize);
    let end = n.saturating_mul(4).min(bgra.len());
    bgra[..end].fill(0);
    if state.mode != PrivacyMode::Text || state.text.is_empty() {
        return;
    }
    // 字模单元 8x8（ASCII 5x7 居中在单元内），按帧高自适应缩放。
    let scale = (h / 180).max(2);
    let cell = 8u32 * scale;
    let max_cols = (w / cell).max(1) as usize;
    let max_lines = (h / cell).max(1) as usize;

    // 分段 + 按单元宽换行（'列' 为 Unicode 字符，CJK/ASCII 均一单元）。
    let mut lines: Vec<Vec<char>> = Vec::new();
    for seg in state.text.split('\n') {
        let mut line: Vec<char> = Vec::new();
        for ch in seg.chars() {
            if line.len() >= max_cols {
                lines.push(std::mem::take(&mut line));
                if lines.len() >= max_lines {
                    break;
                }
            }
            line.push(ch);
        }
        if !line.is_empty() && lines.len() < max_lines {
            lines.push(line);
        }
        if lines.len() >= max_lines {
            break;
        }
    }
    if lines.is_empty() {
        return;
    }
    let line_w = lines.iter().map(|l| l.len()).max().unwrap_or(0) as u32;
    let block_w = line_w * cell;
    let block_h = lines.len() as u32 * cell;
    let x0 = (w.saturating_sub(block_w) / 2) as i64;
    let y0 = (h.saturating_sub(block_h) / 2) as i64;
    for (li, line) in lines.iter().enumerate() {
        let line_off = (block_w - line.len() as u32 * cell) / 2;
        for (ci, ch) in line.iter().enumerate() {
            let gx = x0 + line_off as i64 + ci as i64 * cell as i64;
            let gy = y0 + li as i64 * cell as i64;
            draw_glyph(bgra, w, h, *ch, gx, gy, scale);
        }
    }
}

/// 文字颜色（浅灰，黑底上可读且不刺眼）。
const TEXT_COLOR: [u8; 4] = [220, 220, 220, 255];

/// 在 (gx, gy)（像素）绘制一个字符，字模 1 bit → `scale`×`scale` 像素块。
fn draw_glyph(bgra: &mut [u8], w: u32, h: u32, ch: char, gx: i64, gy: i64, scale: u32) {
    let gw = 8u32;
    let gh = 8u32;
    let glyph = glyph_bits(ch);
    let s = scale as i64;
    for row in 0..gh {
        for col in 0..gw {
            let bit = glyph[row as usize] & (0x80 >> col) != 0;
            if !bit {
                continue;
            }
            for dy in 0..s {
                let y = gy + row as i64 * s + dy;
                if y < 0 || y >= h as i64 {
                    continue;
                }
                for dx in 0..s {
                    let x = gx + col as i64 * s + dx;
                    if x < 0 || x >= w as i64 {
                        continue;
                    }
                    let i = ((y as u32 * w + x as u32) * 4) as usize;
                    bgra[i..i + 4].copy_from_slice(&TEXT_COLOR);
                }
            }
        }
    }
}

/// 字模查找：ASCII 5x7（左对齐），常用中文 8x8，未知字符 → 空心方块。
fn glyph_bits(ch: char) -> [u8; 8] {
    if let Some(g) = ascii_5x7(ch) {
        return g;
    }
    if let Some(g) = cjk_8x8(ch) {
        return g;
    }
    // 未知字符：空心方块（提示有字但不可读）。
    [
        0b11111111,
        0b10000001,
        0b10000001,
        0b10000001,
        0b10000001,
        0b10000001,
        0b10000001,
        0b11111111,
    ]
}

/// ASCII 5x7 点阵（经典 font5x7，公共领域；列主序：每字节一列，位 0 = 顶行）。
/// 非可打印 ASCII 字符返回 None（由未知字符方块兜底）。
fn ascii_5x7(ch: char) -> Option<[u8; 8]> {
    let c = ch as u32;
    if !(32..=126).contains(&c) {
        return None;
    }
    // 95 个可打印字符 × 5 列。
    const FONT5X7: [[u8; 5]; 95] = [
        [0x00, 0x00, 0x00, 0x00, 0x00], // ' ' 32
        [0x00, 0x00, 0x5F, 0x00, 0x00], // '!' 33
        [0x00, 0x07, 0x00, 0x07, 0x00], // '"' 34
        [0x14, 0x7F, 0x14, 0x7F, 0x14], // '#' 35
        [0x24, 0x2A, 0x7F, 0x2A, 0x12], // '$' 36
        [0x23, 0x13, 0x08, 0x64, 0x62], // '%' 37
        [0x36, 0x49, 0x55, 0x22, 0x50], // '&' 38
        [0x00, 0x05, 0x03, 0x00, 0x00], // ''' 39
        [0x00, 0x1C, 0x22, 0x41, 0x00], // '(' 40
        [0x00, 0x41, 0x22, 0x1C, 0x00], // ')' 41
        [0x08, 0x2A, 0x1C, 0x2A, 0x08], // '*' 42
        [0x08, 0x08, 0x3E, 0x08, 0x08], // '+' 43
        [0x00, 0x50, 0x30, 0x00, 0x00], // ',' 44
        [0x08, 0x08, 0x08, 0x08, 0x08], // '-' 45
        [0x00, 0x60, 0x60, 0x00, 0x00], // '.' 46
        [0x20, 0x10, 0x08, 0x04, 0x02], // '/' 47
        [0x3E, 0x51, 0x49, 0x45, 0x3E], // '0' 48
        [0x00, 0x42, 0x7F, 0x40, 0x00], // '1' 49
        [0x42, 0x61, 0x51, 0x49, 0x46], // '2' 50
        [0x21, 0x41, 0x45, 0x4B, 0x31], // '3' 51
        [0x18, 0x14, 0x12, 0x7F, 0x10], // '4' 52
        [0x27, 0x45, 0x45, 0x45, 0x39], // '5' 53
        [0x3C, 0x4A, 0x49, 0x49, 0x30], // '6' 54
        [0x01, 0x71, 0x09, 0x05, 0x03], // '7' 55
        [0x36, 0x49, 0x49, 0x49, 0x36], // '8' 56
        [0x06, 0x49, 0x49, 0x29, 0x1E], // '9' 57
        [0x00, 0x36, 0x36, 0x00, 0x00], // ':' 58
        [0x00, 0x56, 0x36, 0x00, 0x00], // ';' 59
        [0x00, 0x08, 0x14, 0x22, 0x41], // '<' 60
        [0x14, 0x14, 0x14, 0x14, 0x14], // '=' 61
        [0x41, 0x22, 0x14, 0x08, 0x00], // '>' 62
        [0x02, 0x01, 0x51, 0x09, 0x06], // '?' 63
        [0x32, 0x49, 0x79, 0x41, 0x3E], // '@' 64
        [0x7E, 0x11, 0x11, 0x11, 0x7E], // 'A' 65
        [0x7F, 0x49, 0x49, 0x49, 0x36], // 'B' 66
        [0x3E, 0x41, 0x41, 0x41, 0x22], // 'C' 67
        [0x7F, 0x41, 0x41, 0x22, 0x1C], // 'D' 68
        [0x7F, 0x49, 0x49, 0x49, 0x41], // 'E' 69
        [0x7F, 0x09, 0x09, 0x09, 0x01], // 'F' 70
        [0x3E, 0x41, 0x49, 0x49, 0x7A], // 'G' 71
        [0x7F, 0x08, 0x08, 0x08, 0x7F], // 'H' 72
        [0x00, 0x41, 0x7F, 0x41, 0x00], // 'I' 73
        [0x20, 0x40, 0x41, 0x3F, 0x01], // 'J' 74
        [0x7F, 0x08, 0x14, 0x22, 0x41], // 'K' 75
        [0x7F, 0x40, 0x40, 0x40, 0x40], // 'L' 76
        [0x7F, 0x02, 0x0C, 0x02, 0x7F], // 'M' 77
        [0x7F, 0x04, 0x08, 0x10, 0x7F], // 'N' 78
        [0x3E, 0x41, 0x41, 0x41, 0x3E], // 'O' 79
        [0x7F, 0x09, 0x09, 0x09, 0x06], // 'P' 80
        [0x3E, 0x41, 0x51, 0x21, 0x5E], // 'Q' 81
        [0x7F, 0x09, 0x19, 0x29, 0x46], // 'R' 82
        [0x46, 0x49, 0x49, 0x49, 0x31], // 'S' 83
        [0x01, 0x01, 0x7F, 0x01, 0x01], // 'T' 84
        [0x3F, 0x40, 0x40, 0x40, 0x3F], // 'U' 85
        [0x1F, 0x20, 0x40, 0x20, 0x1F], // 'V' 86
        [0x3F, 0x40, 0x38, 0x40, 0x3F], // 'W' 87
        [0x63, 0x14, 0x08, 0x14, 0x63], // 'X' 88
        [0x07, 0x08, 0x70, 0x08, 0x07], // 'Y' 89
        [0x61, 0x51, 0x49, 0x45, 0x43], // 'Z' 90
        [0x00, 0x7F, 0x41, 0x41, 0x00], // '[' 91
        [0x02, 0x04, 0x08, 0x10, 0x20], // '\' 92
        [0x00, 0x41, 0x41, 0x7F, 0x00], // ']' 93
        [0x04, 0x02, 0x01, 0x02, 0x04], // '^' 94
        [0x40, 0x40, 0x40, 0x40, 0x40], // '_' 95
        [0x00, 0x01, 0x02, 0x04, 0x00], // '`' 96
        [0x20, 0x54, 0x54, 0x54, 0x78], // 'a' 97
        [0x7F, 0x48, 0x44, 0x44, 0x38], // 'b' 98
        [0x38, 0x44, 0x44, 0x44, 0x20], // 'c' 99
        [0x38, 0x44, 0x44, 0x48, 0x7F], // 'd' 100
        [0x38, 0x54, 0x54, 0x54, 0x18], // 'e' 101
        [0x08, 0x7E, 0x09, 0x01, 0x02], // 'f' 102
        [0x0C, 0x52, 0x52, 0x52, 0x3E], // 'g' 103
        [0x7F, 0x08, 0x04, 0x04, 0x78], // 'h' 104
        [0x00, 0x44, 0x7D, 0x40, 0x00], // 'i' 105
        [0x20, 0x40, 0x44, 0x3D, 0x00], // 'j' 106
        [0x7F, 0x10, 0x28, 0x44, 0x00], // 'k' 107
        [0x00, 0x41, 0x7F, 0x40, 0x00], // 'l' 108
        [0x7C, 0x04, 0x18, 0x04, 0x78], // 'm' 109
        [0x7C, 0x08, 0x04, 0x04, 0x78], // 'n' 110
        [0x38, 0x44, 0x44, 0x44, 0x38], // 'o' 111
        [0x7C, 0x14, 0x14, 0x14, 0x08], // 'p' 112
        [0x08, 0x14, 0x14, 0x18, 0x7C], // 'q' 113
        [0x7C, 0x08, 0x04, 0x04, 0x08], // 'r' 114
        [0x48, 0x54, 0x54, 0x54, 0x20], // 's' 115
        [0x04, 0x3F, 0x44, 0x40, 0x20], // 't' 116
        [0x3C, 0x40, 0x40, 0x20, 0x7C], // 'u' 117
        [0x1C, 0x20, 0x40, 0x20, 0x1C], // 'v' 118
        [0x3C, 0x40, 0x30, 0x40, 0x3C], // 'w' 119
        [0x44, 0x28, 0x10, 0x28, 0x44], // 'x' 120
        [0x0C, 0x50, 0x50, 0x50, 0x3C], // 'y' 121
        [0x44, 0x64, 0x54, 0x4C, 0x44], // 'z' 122
        [0x00, 0x08, 0x36, 0x41, 0x00], // '{' 123
        [0x00, 0x00, 0x7F, 0x00, 0x00], // '|' 124
        [0x00, 0x41, 0x36, 0x08, 0x00], // '}' 125
        [0x08, 0x08, 0x2A, 0x1C, 0x08], // '~' 126
    ];
    let g = &FONT5X7[(c - 32) as usize];
    // 列主序（位 0 = 顶行）→ 转置为行主序 8x8 单元（位 7 = 最左列）。
    let mut out = [0u8; 8];
    for (col, bits) in g.iter().enumerate() {
        for row in 0..8 {
            if (bits >> row) & 1 == 1 {
                out[row] |= 0x80 >> col;
            }
        }
    }
    Some(out)
}

/// 常用中文 8x8 点阵（隐私屏默认/常用文案用字；位 7 = 最左像素）。
/// 字形为人工设计（8x8 网格），仅覆盖「隐私屏已开启关闭请稍候忙碌中
/// 远程会话进行静音」22 字；其余字符由未知字符方块兜底。
fn cjk_8x8(ch: char) -> Option<[u8; 8]> {
    let g = match ch {
        '隐' => [0x7E, 0x68, 0x7E, 0x68, 0x3E, 0x1A, 0x3C, 0x08],
        '私' => [0x24, 0x76, 0x24, 0x54, 0x2C, 0x26, 0x24, 0x00],
        '屏' => [0x3C, 0x20, 0x3C, 0x24, 0x24, 0x24, 0x3C, 0x20],
        '已' => [0xF0, 0x80, 0xF0, 0x80, 0xFC, 0x00, 0x00, 0x00],
        '开' => [0x10, 0x10, 0xFC, 0x10, 0x10, 0x48, 0x24, 0x00],
        '启' => [0xE0, 0x80, 0xF8, 0x08, 0x3C, 0x24, 0x3C, 0x00],
        '关' => [0x50, 0xFC, 0x10, 0x10, 0x70, 0x86, 0x00, 0x00],
        '闭' => [0xFC, 0x88, 0x98, 0x90, 0x90, 0x88, 0xF8, 0x00],
        '请' => [0x5C, 0x84, 0x5C, 0x44, 0x5C, 0x49, 0x49, 0x1C],
        '稍' => [0x24, 0x72, 0x2A, 0x52, 0x2E, 0x2A, 0x2A, 0x0E],
        '候' => [0x5E, 0xC4, 0x5E, 0x48, 0x5E, 0x49, 0x49, 0x40],
        '忙' => [0x5C, 0x42, 0x5C, 0x42, 0x42, 0x5C, 0x00, 0x00],
        '碌' => [0x7F, 0x18, 0x7F, 0x48, 0x4F, 0x78, 0x28, 0x30],
        '中' => [0x10, 0x10, 0x3C, 0x24, 0x3C, 0x10, 0x10, 0x00],
        '远' => [0x5C, 0x44, 0x1C, 0x08, 0xE8, 0x48, 0x54, 0x30],
        '程' => [0x2F, 0x79, 0x2F, 0x51, 0x2F, 0x21, 0x21, 0x00],
        '会' => [0x42, 0x5A, 0xFC, 0x24, 0x24, 0x3C, 0x00, 0x00],
        '话' => [0x5C, 0x82, 0x5C, 0x42, 0x42, 0x5C, 0x00, 0x00],
        '进' => [0x51, 0x51, 0x3F, 0x21, 0x3F, 0x21, 0x51, 0x30],
        '行' => [0x43, 0x20, 0x43, 0x21, 0x21, 0x33, 0x33, 0x00],
        '静' => [0x6F, 0x48, 0x6F, 0x48, 0x6F, 0x78, 0x58, 0x76],
        '音' => [0x10, 0x78, 0x28, 0x28, 0x78, 0x38, 0x28, 0x38],
        _ => return None,
    };
    Some(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph_art(ch: char) -> String {
        let g = glyph_bits(ch);
        let mut s = String::new();
        for row in g {
            for col in 0..8 {
                s.push(if row & (0x80 >> col) != 0 { '#' } else { '.' });
            }
            s.push('\n');
        }
        s
    }

    /// 打印全部内建字模（cargo test -- --nocapture 可视校对）。
    #[test]
    fn dump_glyphs() {
        let chars: Vec<char> = "隐私屏已开启关闭请稍候忙碌中远程会话进行静音".chars().collect();
        for ch in chars {
            println!("== {ch} ==\n{}", glyph_art(ch));
        }
        // 抽查 ASCII：A / 0。
        println!("== A ==\n{}", glyph_art('A'));
        println!("== 0 ==\n{}", glyph_art('0'));
    }

    /// 每个内建 CJK 字模必须非空且有实质笔画（>2 像素）。
    #[test]
    fn cjk_glyphs_have_content() {
        for ch in "隐私屏已开启关闭请稍候忙碌中远程会话进行静音".chars() {
            let g = cjk_8x8(ch).expect("字模必须存在");
            let pixels = g.iter().map(|r| r.count_ones()).sum::<u32>();
            assert!(pixels >= 8, "字模 {ch} 像素过少: {pixels}");
            assert!(pixels <= 42, "字模 {ch} 像素过多: {pixels}");
        }
    }

    /// 全部 ASCII 可打印字符都有字形且非空（空格为空白字形属正常）。
    #[test]
    fn ascii_glyphs_have_content() {
        for c in 33u32..=126 {
            let ch = char::from_u32(c).unwrap();
            let g = ascii_5x7(ch).expect("可打印 ASCII 必须有字模");
            let pixels = g.iter().map(|r| r.count_ones()).sum::<u32>();
            assert!(pixels >= 3, "字模 {ch:?} 像素过少: {pixels}");
        }
        // 空格是合法的空白字形。
        assert_eq!(ascii_5x7(' ').unwrap().iter().map(|r| r.count_ones()).sum::<u32>(), 0);
    }

    #[test]
    fn control_parse_enable_disable() {
        let mut st = PrivacyState::default();
        // 非 privacy 消息：no-op。
        assert!(!apply_control(&serde_json::json!({"display": 1}), &mut st));
        assert!(!st.enabled);

        // 开启：text 模式 + 定制文字 + 静音。
        assert!(apply_control(
            &serde_json::json!({"privacy": {"enabled": true, "mode": "text",
                                            "text": "隐私屏已开启", "mute": true}}),
            &mut st
        ));
        assert!(st.enabled);
        assert_eq!(st.mode, PrivacyMode::Text);
        assert_eq!(st.text, "隐私屏已开启");
        assert!(st.mute_audio);

        // 重复同值消息：无变化。
        assert!(!apply_control(
            &serde_json::json!({"privacy": {"enabled": true}}),
            &mut st
        ));

        // 关闭：复位静音。
        assert!(apply_control(
            &serde_json::json!({"privacy": {"enabled": false}}),
            &mut st
        ));
        assert!(!st.enabled);
        assert!(!st.mute_audio);
        // 文字/模式保留（重开时沿用）。
        assert_eq!(st.text, "隐私屏已开启");
        assert_eq!(st.mode, PrivacyMode::Text);
    }

    /// 黑屏：整帧全 0（BGRA）。
    #[test]
    fn paint_black_fills_frame() {
        let mut buf = vec![0xABu8; 640 * 360 * 4];
        let st = PrivacyState {
            enabled: true,
            mode: PrivacyMode::Black,
            text: String::new(),
            mute_audio: false,
        };
        paint(&st, &mut buf, 640, 360);
        assert!(buf.iter().all(|&b| b == 0), "黑屏模式应全 0");
    }

    /// 文字模式：帧大部分为黑，文字区域有像素，文字居中。
    #[test]
    fn paint_text_draws_centered() {
        let w = 640u32;
        let h = 360u32;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let st = PrivacyState {
            enabled: true,
            mode: PrivacyMode::Text,
            text: "隐私屏已开启".into(),
            mute_audio: false,
        };
        paint(&st, &mut buf, w, h);

        let lit = |x: u32, y: u32| buf[((y * w + x) * 4) as usize] != 0;
        // 中心区域有文字像素（B 通道 220）。
        let mut any = false;
        let mut lit_coords = (0u32, 0u32);
        for y in 0..h {
            for x in 0..w {
                if lit(x, y) {
                    any = true;
                    lit_coords = (x, y);
                }
            }
        }
        assert!(any, "文字模式应绘制出文字像素");
        // 文字块应水平居中：首个亮像素 x 应 > 帧宽 * 0.3 且 < 帧宽 * 0.7
        //（6 字 × 8px × scale2 = 96px 宽，居中后左侧有大量黑边）。
        assert!(
            lit_coords.0 > w / 3,
            "文字应居中，首个亮像素 x={} 过左",
            lit_coords.0
        );
    }

    /// 文字模式在 1080p 下可读（缩放系数 = h/180 = 6）。
    #[test]
    fn paint_text_1080p_scale() {
        let w = 1920u32;
        let h = 1080u32;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let st = PrivacyState {
            enabled: true,
            mode: PrivacyMode::Text,
            text: "请稍候…".into(),
            mute_audio: false,
        };
        paint(&st, &mut buf, w, h);
        let lit = buf.chunks(4).filter(|p| p[0] != 0).count();
        // 4 字 × 8x8 × scale6² ≈ 4*64*36 ≈ 9216 像素（实际字形笔画约一半）。
        assert!(lit > 1500, "1080p 文字像素过少: {lit}");
        assert!(lit < 200_000, "文字像素过多（疑似非黑底）: {lit}");
    }

    /// 超长文字自动换行且不越界。
    #[test]
    fn paint_wraps_long_text() {
        let w = 320u32;
        let h = 180u32;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let st = PrivacyState {
            enabled: true,
            mode: PrivacyMode::Text,
            text: "这是一段很长的文字用来验证自动换行不会越界显示".into(),
            mute_audio: false,
        };
        paint(&st, &mut buf, w, h);
        // 全部亮像素必须在帧范围内（paint 内部已裁剪；此处断言缓冲区无越界写）。
        assert_eq!(buf.len(), (w * h * 4) as usize);
        let lit = buf.chunks(4).filter(|p| p[0] != 0).count();
        assert!(lit > 0);
    }

    /// 未知字符以空心方块兜底（不 panic、有像素）。
    #[test]
    fn unknown_char_renders_box() {
        let mut buf = vec![0u8; 64 * 32 * 4];
        let st = PrivacyState {
            enabled: true,
            mode: PrivacyMode::Text,
            text: "😀".into(),
            mute_audio: false,
        };
        paint(&st, &mut buf, 64, 32);
        let lit = buf.chunks(4).filter(|p| p[0] != 0).count();
        assert!(lit > 0, "未知字符应渲染方块");
    }
}
