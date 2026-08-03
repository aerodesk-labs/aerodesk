//! 访问单元（完整视频帧）组装器。
//!
//! str0m 的 [`Output::Media`] 按 **NAL 单元**（AnnexB，带 `00 00 00 01` 起始码）
//! 输出；一帧（访问单元）通常由多条 NAL 组成（SPS/PPS + IDR/非 IDR slice）。
//! 本组装器按 RTP 时间戳把同一帧的 NAL 聚合成完整访问单元，供平台解码器
//! （MediaCodec / VideoToolbox / VAAPI）直接喂入。
//!
//! 与编解码器无关：分组只依赖呈现时间戳，关键帧标志由调用方提供
//! （str0m [`MediaData::is_keyframe`]）。

/// 完整访问单元（一帧编码数据，AnnexB，含起始码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// 帧呈现时间（微秒，来自 RTP 时间戳，90kHz 换算）。
    pub pts_us: u64,
}

/// 访问单元组装器。
///
/// # 用法
/// 逐条喂入 [`crate::endpoint::ClientEvent::Media`] 的 AnnexB 数据；
/// 时间戳变化时返回上一完整帧。流结束调用 [`Self::flush`] 收尾。
#[derive(Debug)]
pub struct AccessUnitAssembler {
    current_pts: Option<u64>,
    current: Vec<u8>,
    current_keyframe: bool,
    /// 当前帧超限被标记丢弃：后续同时间戳数据一律跳过，切帧时整帧放弃（#36）。
    dropping: bool,
    /// 单帧字节数上限（防御异常流导致内存失控；4K 帧量级 ~8MB）。
    max_au_bytes: usize,
    frames: usize,
}

impl Default for AccessUnitAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl AccessUnitAssembler {
    /// 新建组装器。单帧上限默认 64MB。
    pub fn new() -> Self {
        Self {
            current_pts: None,
            current: Vec::new(),
            current_keyframe: false,
            dropping: false,
            max_au_bytes: 64 << 20,
            frames: 0,
        }
    }

    /// 喂入一条/多条 AnnexB NAL（含 `00 00 00 01` 起始码）。
    ///
    /// 时间戳与上一帧相同 → 并入当前帧（返回 `None`）；
    /// 时间戳变化 → 返回上一完整帧并开启新帧。
    pub fn push(&mut self, data: &[u8], pts_us: u64, keyframe: bool) -> Option<AccessUnit> {
        if data.is_empty() {
            return None;
        }
        match self.current_pts {
            Some(p) if p == pts_us => {
                if self.dropping {
                    // 整帧已标记丢弃：同帧数据全部跳过（#36）。
                    return None;
                }
                let would_exceed =
                    self.current.len().saturating_add(data.len()) > self.max_au_bytes;
                if would_exceed {
                    // 超限：丢弃整帧而非保留残缺帧（#36）。
                    self.dropping = true;
                    self.current.clear();
                    return None;
                }
                self.current.extend_from_slice(data);
                self.current_keyframe |= keyframe;
                None
            }
            Some(_) => {
                let out = self.take_current();
                self.begin(pts_us, data, keyframe);
                out
            }
            None => {
                self.begin(pts_us, data, keyframe);
                None
            }
        }
    }

    /// 强制收尾：返回未产出的最后一帧（流结束/断线时调用）。
    pub fn flush(&mut self) -> Option<AccessUnit> {
        self.take_current()
    }

    /// 已产出的完整帧数。
    pub fn frames(&self) -> usize {
        self.frames
    }

    fn begin(&mut self, pts_us: u64, data: &[u8], keyframe: bool) {
        self.current_pts = Some(pts_us);
        self.current.clear();
        self.current_keyframe = keyframe;
        // #35：首条 NAL 同样受单帧上限约束，超限直接丢弃整帧。
        self.dropping = data.len() > self.max_au_bytes;
        if !self.dropping {
            self.current.extend_from_slice(data);
        }
    }

    fn take_current(&mut self) -> Option<AccessUnit> {
        let pts = self.current_pts.take()?;
        if self.dropping {
            // 整帧被丢弃：不产出、不计帧数。
            self.dropping = false;
            self.current.clear();
            return None;
        }
        self.frames += 1;
        Some(AccessUnit {
            data: std::mem::take(&mut self.current),
            keyframe: self.current_keyframe,
            pts_us: pts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SC: &[u8] = &[0, 0, 0, 1];

    fn nal(typ: u8, payload: u8) -> Vec<u8> {
        let mut v = SC.to_vec();
        v.push(typ);
        v.push(payload);
        v
    }

    #[test]
    fn same_ts_aggregates_nalus_emits_on_change() {
        let mut au = AccessUnitAssembler::new();
        // 帧 1：SPS(7) + PPS(8) + IDR(5)，同一时间戳。
        assert!(au.push(&nal(7, 0x42), 90_000, false).is_none());
        assert!(au.push(&nal(8, 0x1a), 90_000, false).is_none());
        assert!(au.push(&nal(5, 0x65), 90_000, true).is_none());
        // 帧 2 开始：时间戳变化 → 返回帧 1。
        let f1 = au.push(&nal(1, 0x41), 180_000, false).expect("frame 1");
        assert!(f1.keyframe, "含 IDR 应为关键帧");
        assert_eq!(f1.pts_us, 90_000);
        assert!(f1.data.starts_with(SC));
        let mut expected = Vec::new();
        expected.extend_from_slice(SC);
        expected.extend_from_slice(&[7, 0x42]);
        expected.extend_from_slice(SC);
        expected.extend_from_slice(&[8, 0x1a]);
        expected.extend_from_slice(SC);
        expected.extend_from_slice(&[5, 0x65]);
        assert_eq!(f1.data, expected);
        // 收尾：返回帧 2。
        let f2 = au.flush().expect("frame 2");
        assert!(!f2.keyframe);
        assert_eq!(f2.pts_us, 180_000);
        assert_eq!(f2.data, [SC.to_vec(), vec![1u8, 0x41]].concat());
        assert_eq!(au.frames(), 2);
    }

    #[test]
    fn keyframe_flag_propagates_from_any_nal() {
        let mut au = AccessUnitAssembler::new();
        au.push(&nal(6, 0x00), 1, false); // SEI
        au.push(&nal(1, 0x41), 1, false); // slice
        assert!(!au.push(&nal(1, 0x42), 2, false).unwrap().keyframe);
    }

    #[test]
    fn empty_input_ignored() {
        let mut au = AccessUnitAssembler::new();
        assert!(au.push(&[], 1, false).is_none());
        assert!(au.flush().is_none());
        assert_eq!(au.frames(), 0);
    }

    #[test]
    fn timestamp_regression_flushes() {
        let mut au = AccessUnitAssembler::new();
        au.push(&nal(1, 1), 100, false);
        // 时间戳回退（乱序残留）→ 按边界收尾并开新帧。
        let f = au.push(&nal(1, 2), 90, false).expect("boundary flush");
        assert_eq!(f.pts_us, 100);
        let f2 = au.flush().expect("last");
        assert_eq!(f2.pts_us, 90);
    }

    #[test]
    fn oversized_mid_frame_drops_whole_frame() {
        let mut au = AccessUnitAssembler::new();
        au.max_au_bytes = 16;
        au.push(&[0u8; 10], 1, false);
        au.push(&[0u8; 10], 1, false); // 超上限：整帧丢弃
        assert!(au.flush().is_none(), "残缺帧不应产出");
        assert_eq!(au.frames(), 0);
        // 下一帧正常恢复。
        au.push(&[0u8; 4], 2, false);
        let f = au.flush().expect("next frame");
        assert_eq!(f.data.len(), 4);
    }

    #[test]
    fn oversized_first_nal_drops_frame() {
        let mut au = AccessUnitAssembler::new();
        au.max_au_bytes = 16;
        // 首条 NAL 超上限（#35）：整帧丢弃，后续同 ts 也跳过。
        au.push(&[0u8; 20], 1, true);
        au.push(&[0u8; 4], 1, false);
        assert!(au.flush().is_none());
        assert_eq!(au.frames(), 0);
    }

    #[test]
    fn dropped_frame_does_not_leak_into_next() {
        let mut au = AccessUnitAssembler::new();
        au.max_au_bytes = 16;
        au.push(&[0u8; 20], 1, true); // 超限丢弃（push 返回 None）
        assert!(au.push(&[0u8; 4], 2, false).is_none(), "新帧仍在组装中");
        let f2 = au.flush().expect("next frame");
        assert_eq!(f2.data.len(), 4);
        assert!(!f2.keyframe, "丢弃帧的关键帧标志不得泄漏");
    }
}
