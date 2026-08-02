//! 房间 → 分片路由（参考 PulseBeam：哈希 locality + 负载级联）。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// 超过该负载的分片不再接收新房间（触发级联到次优分片）。
const MAX_LOAD: f64 = 0.8;

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

    /// 客户端数作为负载代理（v1；后续换成 CPU/包率）。
    pub fn set_load(&mut self, shard: usize, clients: usize) {
        let target = clients as f64 / 2048.0;
        let current = self.loads[shard];
        // 不对称 EWMA：上升快、下降慢（PulseBeam 参数）。
        let alpha = if target > current { 0.8 } else { 0.1 };
        self.loads[shard] = target * alpha + current * (1.0 - alpha);
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
        // 只压满主分片：路由必须级联到健康的次优分片
        r.set_load(primary, 2048);
        let backup = r.choose("room-x");
        assert_ne!(primary, backup, "overloaded shard must be skipped");
    }
}
