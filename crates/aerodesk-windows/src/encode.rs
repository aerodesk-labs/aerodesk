//! 编码器（骨架）。
//!
//! 真机路径：Media Foundation `H.264 Encoder MFT` / `HEVC Encoder MFT`；
//! 回退 NVENC（ffmpeg 静态库）或软编 x264（复用 aerodesk-macos::encoder）。

/// 编码输出（AnnexB）。
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts: i64,
}

/// TODO(P4): Media Foundation 实现。
pub struct MfEncoder;

impl MfEncoder {
    pub fn new(_width: u32, _height: u32, _fps: u32, _bitrate_kbps: u32) -> Result<Self, String> {
        Err("windows: Media Foundation encoder not implemented yet (P4)".into())
    }

    pub fn encode(&mut self, _bgra: &[u8]) -> Result<Option<EncodedFrame>, String> {
        Ok(None)
    }
}
