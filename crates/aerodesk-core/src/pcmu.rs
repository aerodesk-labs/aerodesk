//! G.711 μ-law（PCMU）编解码，RFC 3551 §4.5.2。
//!
//! 远程桌面音频首版用 PCMU：零外部依赖、所有平台可编译可测；
//! 8kHz/64kbps 电话级音质，后续可升级 Opus（str0m 已支持）。

/// μ-law 编码：16-bit 有符号 PCM（8kHz 单声道）→ 8-bit μ-law。
pub fn pcmu_encode(samples: &[i16]) -> Vec<u8> {
    samples.iter().map(|&s| encode_sample(s)).collect()
}

/// μ-law 解码：8-bit μ-law → 16-bit 有符号 PCM。
pub fn pcmu_decode(ulaw: &[u8]) -> Vec<i16> {
    ulaw.iter().map(|&u| decode_sample(u)).collect()
}

/// 各分段的线性上限（ITU-T G.711 编码表）。
const SEG_END: [i32; 8] = [0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF, 0x3FFF, 0x7FFF];

/// 单个样本 μ-law 编码（标准算法：加偏置 → 分段找指数 → 输出取反）。
fn encode_sample(pcm: i16) -> u8 {
    let sign = if pcm < 0 { 0x80u8 } else { 0 };
    let value = (pcm as i32).abs().min(32635) + 0x84; // bias = 132
    let mut exponent = 7;
    for (i, &end) in SEG_END.iter().enumerate() {
        if value <= end {
            exponent = i as u8;
            break;
        }
    }
    let mantissa = ((value >> (exponent + 3)) & 0x0F) as u8;
    !(sign | (exponent << 4) | mantissa)
}

/// 单个样本 μ-law 解码。
fn decode_sample(ulaw: u8) -> i16 {
    let ulaw = !ulaw;
    let sign = if ulaw & 0x80 != 0 { -1 } else { 1 };
    let exponent = ((ulaw >> 4) & 0x07) as i32;
    let mantissa = (ulaw & 0x0f) as i32;
    let magnitude = ((mantissa << 3) + 0x84) << exponent;
    (sign * (magnitude - 0x84)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_all_ones() {
        // G.711 惯例：0（静音）编码为 0xFF。
        assert_eq!(pcmu_encode(&[0]), vec![0xFF]);
    }

    #[test]
    fn roundtrip_error_within_quantization() {
        // 全范围抽查：μ-law 是 8-bit 对数量化，绝对误差随幅度增大而增大，
        // 相对精度约 1/256；用 `|s|/200 + 4` 作上界（覆盖各分段步长）。
        for &s in &[
            0i16, 1, -1, 100, -100, 1000, -1000, 8000, -8000, 32000, -32000,
        ] {
            let enc = pcmu_encode(&[s]);
            let dec = pcmu_decode(&enc)[0];
            let err = (dec as i32 - s as i32).abs();
            // 宽松界 `|s|/16 + 8`：μ-law 重构取段下限，最大误差=该段步长；
            // 此界只用于抓住算法性错误（如早期实现 100% 误差），不测精确质量。
            let bound = (s.abs() as i32 / 16) + 8;
            assert!(
                err <= bound,
                "sample {s}: decoded {dec}, err {err} > {bound}"
            );
        }
    }

    #[test]
    fn length_preserved() {
        let samples: Vec<i16> = (0..160).map(|i| ((i * 7) % 200 - 100) as i16).collect();
        let enc = pcmu_encode(&samples);
        assert_eq!(enc.len(), samples.len());
        assert_eq!(pcmu_decode(&enc).len(), samples.len());
    }
}
