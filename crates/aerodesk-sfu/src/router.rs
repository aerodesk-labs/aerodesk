//! 房间 → 分片路由（参考 PulseBeam：哈希 locality + 负载级联）。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// 超过该负载的分片不再接收新房间（触发级联到次优分片）。
const MAX_LOAD: f64 = 0.8;

/// 每分片客户端容量（纯客户端数满分对应的并发数）。
const CLIENT_CAPACITY: f64 = 2048.0;
/// 每分片包率容量（rx+tx pps 满分对应值；远程桌面媒体包小且密集，包率比
/// 客户端数更能反映实际负载，例如单个 4K60 房间的 pps 远高于百个空闲房间）。
const PPS_CAPACITY: f64 = 200_000.0;
/// 负载评分权重：客户端数 + 包率（默认 4:6，可调）。
const CLIENT_WEIGHT: f64 = 0.4;
const PPS_WEIGHT: f64 = 0.6;

/// 分片路由：同一房间总是倾向同一分片，只有超载时才溢出。
pub struct ShardRouter {
    shard_count: usize,
    loads: Vec<f64>,
}

impl ShardRouter {
    pub fn new(shard_count: usize) -> Self {
        assert!(shard_count > 0);
        Self {
            shard_count,
            loads: vec![0.0; shard_count],
        }
    }

    /// 更新分片负载：客户端数 + 双向包率（pps）加权评分（0..=1），
    /// 不对称 EWMA 平滑（上升快、下降慢，防抖动）。
    pub fn set_load(&mut self, shard: usize, clients: usize, rx_pps: f64, tx_pps: f64) {
        let pps = (rx_pps + tx_pps).max(0.0);
        let target = (CLIENT_WEIGHT * (clients as f64 / CLIENT_CAPACITY)
            + PPS_WEIGHT * (pps / PPS_CAPACITY))
            .clamp(0.0, 1.0);
        let current = self.loads[shard];
        // 不对称 EWMA：上升快、下降慢（PulseBeam 参数）。
        let alpha = if target > current { 0.8 } else { 0.1 };
        self.loads[shard] = target * alpha + current * (1.0 - alpha);
    }

    /// 当前负载评分（测试/观测用）。
    pub fn load(&self, shard: usize) -> f64 {
        self.loads[shard]
    }

    /// 为房间选择分片：取 (room, shard) 哈希最高的健康分片。
    pub fn choose(&self, room: &str) -> usize {
        let mut best = 0usize;
        let mut best_hash = -1.0f64;
        for i in 0..self.shard_count {
            if self.loads[i] >= MAX_LOAD {
                continue;
            }
            let mut h = DefaultHasher::new();
            room.hash(&mut h);
            i.hash(&mut h);
            let v = (h.finish() as f64) / (u64::MAX as f64);
            if v > best_hash {
                best_hash = v;
                best = i;
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_room_same_shard() {
        let r = ShardRouter::new(4);
        let a = r.choose("room-a");
        let b = r.choose("room-a");
        assert_eq!(a, b);
    }

    #[test]
    fn overflow_when_loaded() {
        let mut r = ShardRouter::new(4);
        let primary = r.choose("room-x");
        // 只压满主分片（客户端满 + 高包率）：路由必须级联到健康的次优分片
        r.set_load(primary, 2048, 150_000.0, 150_000.0);
        let backup = r.choose("room-x");
        assert_ne!(primary, backup, "overloaded shard must be skipped");
    }

    #[test]
    fn load_reflects_packet_rate() {
        let mut r = ShardRouter::new(2);
        // 客户端数相同，但包率差异巨大 → 负载评分应拉开
        r.set_load(0, 10, 0.0, 0.0);
        r.set_load(1, 10, 100_000.0, 100_000.0);
        assert!(
            r.load(1) > r.load(0) + 0.2,
            "high pps shard should score higher: {} vs {}",
            r.load(1),
            r.load(0)
        );
    }

    #[test]
    fn load_clamps_negative_rate_and_over_capacity() {
        let mut r = ShardRouter::new(2);
        r.set_load(0, 0, -5.0, -3.0);
        assert!(r.load(0) >= 0.0, "negative rate must clamp");
        // 远超容量 → 评分封顶 1.0（且 ≥ MAX_LOAD 触发级联）
        r.set_load(1, 10_000, 1_000_000.0, 1_000_000.0);
        assert!(r.load(1) >= 0.8);
    }
}
