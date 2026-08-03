//! 网络模拟器测试框架（str0m netem）。
//!
//! 用确定性种子模拟丢包/延迟/抖动/乱序/突发/拥塞/重复，验证 SFU 依赖的
//! BWE/重传行为前提：模拟器统计符合配置、时间驱动正确。后续在此框架上
//! 扩展"丢包下媒体送达 + 关键帧请求"场景。

use std::time::{Duration, Instant};

use std::collections::HashSet;

use str0m_netem::{
    Bitrate, DataSize, GilbertElliot, Input, Netem, NetemConfig, Output, Probability, RandomLoss,
};

/// 驱动 netem 到静默：喂包 → 按 Timeout 推进虚拟时钟 → 收集输出。
fn drive(netem: &mut Netem<Vec<u8>>, start: Instant) -> (Vec<Vec<u8>>, Instant) {
    let mut out = Vec::new();
    let mut now = start;
    loop {
        match netem.poll_output() {
            Some(Output::Timeout(t)) => {
                now = t;
                netem.handle_input(Input::Timeout(t));
            }
            Some(Output::Packet(p)) => out.push(p),
            None => break,
        }
    }
    (out, now)
}

#[test]
fn loss_rate_matches_config() {
    let config = NetemConfig::new()
        .loss(RandomLoss::new(Probability::new(0.01)))
        .seed(42);
    let mut netem: Netem<Vec<u8>> = Netem::new(config);
    let base = Instant::now();

    let total = 10_000;
    for i in 0..total {
        netem.handle_input(Input::Packet(base, vec![i as u8; 100]));
    }
    let (out, _) = drive(&mut netem, base);
    let delivered = out.len();
    let loss = 1.0 - delivered as f64 / total as f64;
    println!("delivered {delivered}/{total}, loss {loss:.4}");
    assert!(
        (0.005..=0.02).contains(&loss),
        "loss rate {loss} should be near 1%"
    );
}

#[test]
fn latency_is_applied() {
    let config = NetemConfig::new()
        .latency(Duration::from_millis(50))
        .seed(7);
    let mut netem: Netem<Vec<u8>> = Netem::new(config);
    let base = Instant::now();

    netem.handle_input(Input::Packet(base, vec![1, 2, 3]));
    let (out, now) = drive(&mut netem, base);
    assert_eq!(out.len(), 1);
    let elapsed = now.duration_since(base);
    assert!(
        elapsed >= Duration::from_millis(45) && elapsed <= Duration::from_millis(60),
        "elapsed {elapsed:?} should be ~50ms"
    );
}

#[test]
fn deterministic_with_same_seed() {
    let run = |seed: u64| {
        let config = NetemConfig::new()
            .loss(RandomLoss::new(Probability::new(0.1)))
            .seed(seed);
        let mut netem: Netem<Vec<u8>> = Netem::new(config);
        let base = Instant::now();
        for i in 0..200 {
            netem.handle_input(Input::Packet(base, vec![i as u8; 50]));
        }
        let (out, _) = drive(&mut netem, base);
        out.len()
    };
    assert_eq!(run(42), run(42));
    assert_ne!(run(42), run(43), "different seeds should differ");
}

#[test]
fn jitter_spreads_delivery() {
    let config = NetemConfig::new()
        .latency(Duration::from_millis(20))
        .jitter(Duration::from_millis(10))
        .seed(1);
    let mut netem: Netem<Vec<u8>> = Netem::new(config);
    let base = Instant::now();

    let mut arrivals = Vec::new();
    for i in 0..50 {
        netem.handle_input(Input::Packet(base, vec![i as u8; 10]));
        let (out, now) = drive(&mut netem, base);
        arrivals.extend(out.iter().map(|_| now.duration_since(base)));
    }
    let (out, _) = drive(&mut netem, base);
    arrivals.extend(out.iter().map(|_| Duration::ZERO));
    assert_eq!(arrivals.len(), 50);
    let max = arrivals.iter().max().unwrap();
    println!("jitter max delivery: {max:?}");
    assert!(
        *max >= Duration::from_millis(15),
        "jitter should push some packets past base latency"
    );
}

/// #8 网络抗性：5% 丢包率与配置一致（弱网场景）。
#[test]
fn loss_5pct_matches_config() {
    let config = NetemConfig::new()
        .loss(RandomLoss::new(Probability::new(0.05)))
        .seed(1337);
    let mut netem: Netem<Vec<u8>> = Netem::new(config);
    let base = Instant::now();

    let total = 20_000;
    for i in 0..total {
        netem.handle_input(Input::Packet(base, vec![i as u8; 100]));
    }
    let (out, _) = drive(&mut netem, base);
    let delivered = out.len();
    let loss = 1.0 - delivered as f64 / total as f64;
    println!("5%: delivered {delivered}/{total}, loss {loss:.4}");
    assert!(
        (0.04..=0.06).contains(&loss),
        "loss rate {loss} should be near 5%"
    );
}

/// #8 网络抗性：丢包 5% + 延迟 30ms + 抖动 15ms 组合，绝大多数包按时送达。
#[test]
fn combined_loss_jitter_latency_delivers_most() {
    let config = NetemConfig::new()
        .loss(RandomLoss::new(Probability::new(0.05)))
        .latency(Duration::from_millis(30))
        .jitter(Duration::from_millis(15))
        .seed(2026);
    let mut netem: Netem<Vec<u8>> = Netem::new(config);
    let base = Instant::now();

    // 模拟 10s 30fps 媒体流（300 帧 × 每帧 4 包 = 1200 包）。
    let total = 1_200;
    for i in 0..total {
        netem.handle_input(Input::Packet(base, vec![i as u8; 1200]));
    }
    let (out, _) = drive(&mut netem, base);
    let delivered = out.len();
    let loss = 1.0 - delivered as f64 / total as f64;
    println!("combined: delivered {delivered}/{total}, loss {loss:.4}");
    // 5% 丢包下交付率应 ≥ 93%（容差 2%）。
    assert!(
        delivered as f64 >= total as f64 * 0.93,
        "delivered too low: {delivered}"
    );
}

/// #8 网络抗性：1% 丢包下媒体基本无损（高清通话质量线）。
#[test]
fn loss_1pct_near_lossless() {
    let config = NetemConfig::new()
        .loss(RandomLoss::new(Probability::new(0.01)))
        .seed(99);
    let mut netem: Netem<Vec<u8>> = Netem::new(config);
    let base = Instant::now();

    let total = 20_000;
    for i in 0..total {
        netem.handle_input(Input::Packet(base, vec![i as u8; 100]));
    }
    let (out, _) = drive(&mut netem, base);
    let loss = 1.0 - out.len() as f64 / total as f64;
    println!("1%: loss {loss:.4}");
    assert!(
        (0.005..=0.02).contains(&loss),
        "loss rate {loss} should be near 1%"
    );
}

/// #8 网络抗性：乱序（多路径路由）——每 N 包绕过延迟队列提前到达。
#[test]
fn reorder_gap_reorders_packets() {
    let config = NetemConfig::new()
        .latency(Duration::from_millis(30))
        .reorder_gap(5)
        .seed(11);
    let mut netem: Netem<Vec<u8>> = Netem::new(config);
    let base = Instant::now();

    let total = 100;
    for i in 0..total {
        netem.handle_input(Input::Packet(base, vec![i as u8; 20]));
    }
    let (out, _) = drive(&mut netem, base);
    assert_eq!(out.len(), total, "乱序不应丢包");
    let in_order = out
        .iter()
        .zip(0..total)
        .filter(|(p, i)| p[0] == *i as u8)
        .count();
    println!("reorder: {in_order}/{total} in-order");
    assert!(in_order < total, "reorder_gap(5) 应让部分包乱序到达");
}

/// #8 网络抗性：突发丢包（Gilbert-Elliot）——丢包成簇，而非均匀散布。
#[test]
fn burst_loss_gilbert_elliot_is_bursty() {
    // 好状态平均 20 包、坏状态平均 5 包（100% 丢）→ 理论总丢包 ~20%。
    let config = NetemConfig::new()
        .loss(
            GilbertElliot::new()
                .good_duration(20.0)
                .bad_duration(5.0)
                .loss_in_bad(Probability::ONE),
        )
        .seed(2026);
    let mut netem: Netem<Vec<u8>> = Netem::new(config);
    let base = Instant::now();

    let total = 500u16;
    for i in 0..total {
        let idx = i.to_le_bytes();
        netem.handle_input(Input::Packet(base, vec![idx[0], idx[1], 0, 0]));
    }
    let (out, _) = drive(&mut netem, base);
    let delivered: HashSet<u16> = out
        .iter()
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .collect();
    let mut run = 0u32;
    let mut max_run = 0u32;
    for i in 0..total {
        if delivered.contains(&i) {
            run = 0;
        } else {
            run += 1;
            max_run = max_run.max(run);
        }
    }
    let loss = 1.0 - delivered.len() as f64 / total as f64;
    println!(
        "GE: delivered {}/{} loss {loss:.3} max-run {max_run}",
        delivered.len(),
        total
    );
    assert!(
        (0.14..=0.30).contains(&loss),
        "loss {loss} should be near 20%"
    );
    assert!(
        max_run >= 3,
        "Gilbert-Elliot 应产生连续突发丢包（max-run={max_run}）"
    );
}

/// #8 网络抗性：带宽瓶颈（限速 + 有限缓冲）——超容量尾丢，低负载无损。
#[test]
fn link_congestion_tail_drop() {
    // 低负载：1Mbps / 16KB 缓冲，10KB 瞬时灌入 → 全部送达。
    let low = NetemConfig::new()
        .link(Bitrate::mbps(1), DataSize::kbytes(16))
        .seed(5);
    let mut netem: Netem<Vec<u8>> = Netem::new(low);
    let base = Instant::now();
    for i in 0..100 {
        netem.handle_input(Input::Packet(base, vec![i as u8; 100]));
    }
    let (out, _) = drive(&mut netem, base);
    assert_eq!(out.len(), 100, "低于链路容量不应丢包");

    // 超容量：1Mbps / 16KB 缓冲，瞬时灌入 600KB → 大量尾丢。
    let high = NetemConfig::new()
        .link(Bitrate::mbps(1), DataSize::kbytes(16))
        .seed(5);
    let mut netem: Netem<Vec<u8>> = Netem::new(high);
    for i in 0..500u16 {
        let idx = i.to_le_bytes();
        let mut pkt = vec![0u8; 600];
        pkt[0] = idx[0];
        pkt[1] = idx[1];
        netem.handle_input(Input::Packet(base, pkt));
    }
    let (out, _) = drive(&mut netem, base);
    let delivered = out.len();
    println!("congestion: delivered {delivered}/500");
    assert!(delivered > 0, "有限缓冲应放行部分包");
    assert!(
        delivered < 100,
        "600KB 瞬时灌入 16KB 缓冲应大量尾丢（delivered={delivered}）"
    );
}

/// #8 网络抗性：重复包（重传/多路径）——按配置比例产生重复交付。
#[test]
fn duplicate_packets_appear() {
    let config = NetemConfig::new().duplicate(Probability::new(0.1)).seed(77);
    let mut netem: Netem<Vec<u8>> = Netem::new(config);
    let base = Instant::now();
    let total = 2_000;
    for i in 0..total {
        netem.handle_input(Input::Packet(base, vec![i as u8; 50]));
    }
    let (out, _) = drive(&mut netem, base);
    let extra = out.len() as isize - total as isize;
    println!("duplicate: {}/{} packets ({extra} extra)", out.len(), total);
    assert!(extra > 0, "应有重复包");
    assert!(
        extra < (total as isize) / 2,
        "10% 重复率不应超过 50%（extra={extra}）"
    );
}

/// #8 网络抗性：真实网络档案的突发丢包模型（wifi_lossy/cellular/satellite/congested）
/// 聚合丢包率与各档案设计值一致（链路带宽/缓冲行为由 `link_congestion_tail_drop` 覆盖）。
#[test]
fn preset_loss_models_match() {
    let cases: [(&str, NetemConfig, f64, f64); 4] = [
        (
            "wifi_lossy",
            NetemConfig::new().loss(GilbertElliot::wifi_lossy()),
            0.03,
            0.08,
        ),
        (
            "cellular",
            NetemConfig::new().loss(GilbertElliot::cellular()),
            0.008,
            0.04,
        ),
        (
            "satellite",
            NetemConfig::new().loss(GilbertElliot::satellite()),
            0.015,
            0.05,
        ),
        (
            "congested",
            NetemConfig::new().loss(GilbertElliot::congested()),
            0.07,
            0.14,
        ),
    ];
    let total = 20_000;
    for (name, cfg, lo, hi) in cases {
        let mut netem: Netem<Vec<u8>> = Netem::new(cfg.seed(2026));
        let base = Instant::now();
        for i in 0..total {
            netem.handle_input(Input::Packet(base, vec![i as u8; 100]));
        }
        let (out, _) = drive(&mut netem, base);
        let loss = 1.0 - out.len() as f64 / total as f64;
        println!("{name}: loss {loss:.4}");
        assert!(
            (lo..=hi).contains(&loss),
            "{name} 丢包率 {loss:.4} 不在预期范围 ({lo}..={hi})"
        );
    }
}
