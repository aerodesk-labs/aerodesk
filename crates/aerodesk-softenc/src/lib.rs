//! 共享软编解码（#3/#4 硬件加速的回退路径）。
//!
//! - 编码：x264 软编（AnnexB / 4:2:0，非 Windows；Windows 无系统 x264）
//! - 解码：OpenH264 软解（全平台）

pub mod decode;
#[cfg(not(windows))]
pub mod encode;
pub mod openh264enc;

/// 编码输出（AnnexB，与 str0m packetizer 对齐；全平台共用）。
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts: i64,
}

/// BGRA（蓝绿红透明度序）→ RGB24（x264 输入；Windows DXGI 输出 BGRA）。
pub fn bgra_to_rgb(bgra: &[u8]) -> Vec<u8> {
    let n = bgra.len() / 4;
    let mut rgb = vec![0u8; n * 3];
    for i in 0..n {
        rgb[i * 3] = bgra[i * 4 + 2];
        rgb[i * 3 + 1] = bgra[i * 4 + 1];
        rgb[i * 3 + 2] = bgra[i * 4];
    }
    rgb
}

/// BGRA → RGBA（OpenH264 软编输入；core `VideoFrame.raw` 统一为 BGRA）。
pub fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let n = bgra.len() / 4;
    let mut rgba = vec![0u8; n * 4];
    for i in 0..n {
        rgba[i * 4] = bgra[i * 4 + 2];
        rgba[i * 4 + 1] = bgra[i * 4 + 1];
        rgba[i * 4 + 2] = bgra[i * 4];
        rgba[i * 4 + 3] = bgra[i * 4 + 3];
    }
    rgba
}

/// RGBA → BGRA（平台采集统一为 core `VideoFrame.raw` 的 BGRA 约定）。
pub fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    let n = rgba.len() / 4;
    let mut bgra = vec![0u8; n * 4];
    for i in 0..n {
        bgra[i * 4] = rgba[i * 4 + 2];
        bgra[i * 4 + 1] = rgba[i * 4 + 1];
        bgra[i * 4 + 2] = rgba[i * 4];
        bgra[i * 4 + 3] = rgba[i * 4 + 3];
    }
    bgra
}

/// RGBA → RGB24（x264 输入；X11 GetImage 读回 RGBA）。
pub fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let n = rgba.len() / 4;
    let mut rgb = vec![0u8; n * 3];
    for i in 0..n {
        rgb[i * 3] = rgba[i * 4];
        rgb[i * 3 + 1] = rgba[i * 4 + 1];
        rgb[i * 3 + 2] = rgba[i * 4 + 2];
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_bgra_rgba() {
        let bgra = [10u8, 20, 30, 255, 40, 50, 60, 255];
        assert_eq!(bgra_to_rgb(&bgra), vec![30, 20, 10, 60, 50, 40]);
        let rgba = [10u8, 20, 30, 255, 40, 50, 60, 255];
        assert_eq!(rgba_to_rgb(&rgba), vec![10, 20, 30, 40, 50, 60]);
    }
}
