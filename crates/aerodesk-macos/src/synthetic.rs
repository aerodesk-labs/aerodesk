//! 合成测试帧源：彩条 + 移动方块（模拟屏幕内容变化，无需采集权限）。

pub struct SyntheticSource {
    width: u32,
    height: u32,
    frame: u64,
    buf: Vec<u8>,
    bgra: Vec<u8>,
    /// 彩条基础行（每行相同，逐行复制避免全帧逐像素计算）。
    base_row: Vec<u8>,
    /// 高熵内容（确定性伪随机噪声）：编码码率贴近目标档位，
    /// 用于 simulcast 选层验证与 4K 压测（#8/#58）。
    noise: bool,
}

impl SyntheticSource {
    pub fn new(width: u32, height: u32) -> Self {
        let mut src = Self {
            width,
            height,
            frame: 0,
            buf: vec![0; (width * height * 3) as usize],
            bgra: vec![0; (width * height * 4) as usize],
            base_row: vec![0; (width * 3) as usize],
            noise: false,
        };
        // 预计算彩条基础行（8 条色带）。
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
        for x in 0..width as usize {
            let idx = x * 3;
            let bar = bars[((x * 8) / width as usize).min(7)];
            src.base_row[idx] = bar.0;
            src.base_row[idx + 1] = bar.1;
            src.base_row[idx + 2] = bar.2;
        }
        src
    }

    /// 高熵合成源（伪随机噪声，每帧变化）。
    pub fn new_noisy(width: u32, height: u32) -> Self {
        let mut src = Self::new(width, height);
        src.noise = true;
        src
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// 生成下一帧 RGB24（行级复制 + 方块覆盖，4K 下比逐像素快一个量级）。
    pub fn next_frame(&mut self) -> &[u8] {
        let w = self.width as usize;
        let h = self.height as usize;
        let t = self.frame;

        if self.noise {
            // 彩条基础 + 底部 1/8 高噪声带（确定性 xorshift，按帧变化）。
            // 噪声像素量随分辨率缩放（f≈9x q），码率差可观测但编码量可控
            //（避免关键帧突发超出 pacer 排程，见 #66）。
            let row_bytes = w * 3;
            for y in 0..h {
                let start = y * row_bytes;
                self.buf[start..start + row_bytes].copy_from_slice(&self.base_row);
            }
            let mut seed = t.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x1234_5678_9ABC_DEF0;
            let band_top = (h * 3 / 4).max(1);
            let band_bottom = (h - 8).max(band_top + 1);
            for y in band_top..band_bottom {
                for x in 0..w {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let idx = (y * w + x) * 3;
                    self.buf[idx] = seed as u8;
                    self.buf[idx + 1] = (seed >> 8) as u8;
                    self.buf[idx + 2] = (seed >> 16) as u8;
                }
            }
            self.frame += 1;
            return &self.buf;
        }

        // 彩条：每行复制预计算的 base_row。
        let row_bytes = w * 3;
        for y in 0..h {
            let start = y * row_bytes;
            self.buf[start..start + row_bytes].copy_from_slice(&self.base_row);
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
    /// 直接在 bgra 缓冲转换，避免中间 to_vec 拷贝（4K 下省 ~25MB/帧）。
    pub fn next_frame_bgra(&mut self) -> &[u8] {
        self.next_frame();
        let w = self.width as usize;
        let h = self.height as usize;
        let buf = &self.buf;
        let bgra = &mut self.bgra;
        let n = w * h;
        for i in 0..n {
            let s = i * 3;
            let d = i * 4;
            bgra[d] = buf[s + 2]; // B
            bgra[d + 1] = buf[s + 1]; // G
            bgra[d + 2] = buf[s]; // R
            bgra[d + 3] = 255; // A
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

    #[test]
    fn four_k_frame_generation_speed() {
        // 4K 合成源应达到较高帧率（优化后行级复制）；回归护栏：单帧生成 < 20ms。
        let mut src = SyntheticSource::new(3840, 2160);
        let start = std::time::Instant::now();
        let n = 30;
        for _ in 0..n {
            let _ = src.next_frame();
        }
        let per_frame = start.elapsed() / n;
        // 实测优化后应远小于 20ms；此处只做软断言（CI 负载差异大），打日志供参考。
        eprintln!("4K synthetic per-frame: {:?}", per_frame);
        assert!(per_frame.as_millis() < 50, "4K 合成帧过慢: {per_frame:?}");
    }
}
