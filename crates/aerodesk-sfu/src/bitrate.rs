//! 码率控制器：根据远端带宽估计（EgressBitrateEstimate）决定
//! 目标码率（pacing floor）与 simulcast 选层。
//!
//! 设计参考 PulseBeam BitrateController：headroom、下降 EWMA 平滑、
//! 量化步长与滞回（防止抖动导致频繁降档）。

use str0m::bwe::Bitrate;

/// simulcast 分层（与浏览器 rid 约定 q/h/f 对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Low,
    Medium,
    High,
}

impl Layer {
    /// 各层 seed 码率（参考 PulseBeam simulcast 配置）。
    pub fn seed_bitrate(self) -> u64 {
        match self {
            Layer::High => 1_250_000,
            Layer::Medium => 400_000,
            Layer::Low => 150_000,
        }
    }
}

pub struct BitrateController {
    min: Bitrate,
    max: Bitrate,
    headroom: f64,
    down_smoothing: f64,
    quantization_step: Bitrate,
    hysteresis: Bitrate,
    target: Bitrate,
    estimate: Option<Bitrate>,
}

impl Default for BitrateController {
    fn default() -> Self {
        Self::new(Bitrate::kbps(150), Bitrate::mbps(10), Bitrate::kbps(500))
    }
}

impl BitrateController {
    pub fn new(min: Bitrate, max: Bitrate, initial: Bitrate) -> Self {
        Self {
            min,
            max,
            headroom: 1.0,
            down_smoothing: 0.95,
            quantization_step: Bitrate::kbps(200),
            hysteresis: Bitrate::kbps(150),
            target: initial.clamp(min, max),
            estimate: None,
        }
    }

    /// 更新远端带宽估计，返回调整后的目标码率。
    pub fn update_estimate(&mut self, estimate: Bitrate) -> Bitrate {
        let raw = estimate * self.headroom;
        let old = self.target;

        // 上升：直接采用（量化到步长）
        if raw > old {
            self.target = quantize(raw, self.quantization_step);
        } else if raw + self.hysteresis < old {
            if raw < old * 0.5 {
                // 剧烈下降（链路突变）：快速跟随，避免持续过度发送
                self.target = quantize(raw, self.quantization_step);
            } else {
                // 缓降：EWMA 平滑，防抖动
                let smoothed = old * self.down_smoothing + raw * (1.0 - self.down_smoothing);
                self.target = quantize(smoothed, self.quantization_step);
            }
        }
        self.target = self.target.clamp(self.min, self.max);
        self.estimate = Some(estimate);
        self.target
    }

    /// 当前目标码率（用于 `Bwe::set_current_bitrate`）。
    pub fn target(&self) -> Bitrate {
        self.target
    }

    /// 按目标码率选择 simulcast 层。
    pub fn selected_layer(&self) -> Layer {
        let bps = self.target.as_f64();
        if bps >= Layer::High.seed_bitrate() as f64 {
            Layer::High
        } else if bps >= Layer::Medium.seed_bitrate() as f64 {
            Layer::Medium
        } else {
            Layer::Low
        }
    }
}

fn quantize(v: Bitrate, step: Bitrate) -> Bitrate {
    let bps = v.as_f64();
    let step = step.as_f64();
    Bitrate::bps((bps / step).floor() as u64 * step as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_target_used_when_no_estimate() {
        let c = BitrateController::default();
        assert_eq!(c.target(), Bitrate::kbps(500));
        assert_eq!(c.selected_layer(), Layer::Medium);
    }

    #[test]
    fn estimate_raises_target() {
        let mut c = BitrateController::default();
        let t = c.update_estimate(Bitrate::mbps(5));
        assert!(t >= Bitrate::mbps(4), "target {t:?}");
        assert_eq!(c.selected_layer(), Layer::High);
    }

    #[test]
    fn drop_needs_hysteresis() {
        let mut c = BitrateController::default();
        c.update_estimate(Bitrate::mbps(5));
        let before = c.target();
        // 小幅度下降：被滞回挡住
        let small = c.update_estimate(before - Bitrate::kbps(50));
        assert!(
            small >= before - Bitrate::kbps(200),
            "small drop should be absorbed: {small:?}"
        );
        // 大幅度下降：生效并平滑
        let big = c.update_estimate(Bitrate::kbps(100));
        assert!(big < before);
    }

    #[test]
    fn clamps_to_min_max() {
        let mut c =
            BitrateController::new(Bitrate::kbps(100), Bitrate::mbps(2), Bitrate::kbps(300));
        let t = c.update_estimate(Bitrate::mbps(100));
        assert!(t <= Bitrate::mbps(2));
        let t = c.update_estimate(Bitrate::kbps(1));
        assert!(t >= Bitrate::kbps(100));
    }

    #[test]
    fn layer_transitions() {
        let mut c = BitrateController::default();
        assert_eq!(c.selected_layer(), Layer::Medium);
        c.update_estimate(Bitrate::mbps(3));
        assert_eq!(c.selected_layer(), Layer::High);
        c.update_estimate(Bitrate::kbps(200));
        assert_eq!(c.selected_layer(), Layer::Low);
    }
}
