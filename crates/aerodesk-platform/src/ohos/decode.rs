//! OH_VideoDecoder 解码（骨架）。

/// 解码帧。
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts_us: i64,
}

/// TODO(P4): OH_VideoDecoder 实现（与 aerodesk-ios::decode 同构：AnnexB → 硬解）。
pub struct OhosVideoDecoder;

impl OhosVideoDecoder {
    pub fn new() -> Result<Self, String> {
        Err("ohos: OH_VideoDecoder not implemented yet (P4)".into())
    }

    pub fn decode_annexb(&mut self, _data: &[u8], _pts_us: i64) -> Option<DecodedFrame> {
        None
    }
}
