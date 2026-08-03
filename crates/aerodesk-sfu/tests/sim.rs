//! 网络模拟器测试框架（str0m netem）。
//!
//! 用确定性种子模拟丢包/延迟/抖动，验证 SFU 依赖的 BWE/重传行为前提：
//! 模拟器统计符合配置、时间驱动正确。后续在此框架上扩展
//! "丢包下媒体送达 + 关键帧请求"场景。

use std::time::{Duration, Instant};

use str0m_netem::{Input, Netem, NetemConfig, Output, Probability, RandomLoss};

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
