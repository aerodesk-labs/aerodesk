//! 编码器（骨架）。
//!
//! VAAPI：`vaCreateContext` + H.264/AV1 编码；回退 libx264（复用 aerodesk-macos 的
//! 思路：RGB24→I420 + x264 单线程单 slice，VideoToolbox 兼容性已验证）。

/// 编码输出（AnnexB，与 str0m packetizer 对齐）。
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts: i64,
}

/// TODO(P4): VAAPI 实现；软编回退可直接复用 aerodesk-macos::encoder（抽出共享）。
pub struct VaapiEncoder;

impl VaapiEncoder {
    pub fn new(_width: u32, _height: u32, _fps: u32, _bitrate_kbps: u32) -> Result<Self, String> {
        Err("linux: VAAPI encoder not implemented yet (P4)".into())
    }

    pub fn encode(&mut self, _rgba: &[u8]) -> Result<Option<EncodedFrame>, String> {
        Ok(None)
    }
}
