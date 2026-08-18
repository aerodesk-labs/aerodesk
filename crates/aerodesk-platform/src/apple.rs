//! Apple 平台共享实现（macOS/iOS）。
//!
//! VideoToolbox 硬解单一实现（#506）：历史上 `macos/decode.rs` 与 `ios/decode.rs`
//! 是两份近重复拷贝且各自漂移（macOS 有会话复用优化，iOS 有 codec 探测与 core
//! Decoder trait 实现）；统一后两端 re-export 保持既有路径稳定。

pub mod vt_decode;
