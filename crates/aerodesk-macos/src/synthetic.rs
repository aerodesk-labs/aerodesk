//! 合成测试帧源：彩条 + 移动方块（模拟屏幕内容变化，无需采集权限）。

pub struct SyntheticSource {
    width: u32,
    height: u32,
    frame: u64,
    buf: Vec<u8>,
    bgra: Vec<u8>,
}

impl SyntheticSource {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            frame: 0,
            buf: vec![0; (width * height * 3) as usize],
            bgra: vec![0; (width * height * 4) as usize],
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// 生成下一帧 RGB24。
    pub fn next_frame(&mut self) -> &[u8] {
        let w = self.width as usize;
        let h = self.height as usize;
        let t = self.frame;

        // 彩条：8 条色带
        let bars = [
            (180u8, 180u8, 180u8),
            (180, 180, 0),
            (0, 180, 180),
            (0, 180, 0),
            (180, 0, 180),
            (180, 0, 0),
            (0, 0, 180),
            (0, 0, 0),
        ];
        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) * 3;
                let bar = bars[((x * 8) / w).min(7)];
                self.buf[idx] = bar.0;
                self.buf[idx + 1] = bar.1;
                self.buf[idx + 2] = bar.2;
            }
        }

        // 移动方块（模拟鼠标指针/内容变化）
        let size = (w / 12).max(8);
        let bx = (t as usize * (w / 20)) % (w - size);
        let by = (t as usize * (h / 30)) % (h - size);
        for y in by..(by + size).min(h) {
            for x in bx..(bx + size).min(w) {
                let idx = (y * w + x) * 3;
                self.buf[idx] = 255;
                self.buf[idx + 1] = 255;
                self.buf[idx + 2] = 255;
            }
        }

        // 帧计数条（底部 8 像素显示亮度变化）
        let counter = (t % 32) as u8 * 8;
        for x in 0..w {
            let idx = ((h - 8) * w + x) * 3;
            self.buf[idx] = counter;
            self.buf[idx + 1] = counter;
            self.buf[idx + 2] = counter;
        }

        self.frame += 1;
        &self.buf
    }

    /// 生成下一帧 BGRA32（VideoToolbox 硬编输入）。
    pub fn next_frame_bgra(&mut self) -> &[u8] {
        let rgb = self.next_frame().to_vec();
        for (dst, src) in self.bgra.chunks_exact_mut(4).zip(rgb.chunks_exact(3)) {
            dst[0] = src[2]; // B
            dst[1] = src[1]; // G
            dst[2] = src[0]; // R
            dst[3] = 255; // A
        }
        &self.bgra
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_have_expected_size_and_vary() {
        let mut src = SyntheticSource::new(64, 64);
        let a = src.next_frame().to_vec();
        let b = src.next_frame().to_vec();
        assert_eq!(a.len(), 64 * 64 * 3);
        assert_ne!(a, b, "moving block should change content");
    }
}
