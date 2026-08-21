//! aerodesk-protocol —— 跨端共享协议类型（#487 审查批次 3 / #10）。
//!
//! 2026-08-19 曾并入 `aerodesk_core::protocol`（#535 crate 收敛），#10 再拆：
//! 服务端（aerodesk-sfu / aerodesk-signal）只依赖本 crate，不再被迫链接客户端
//! 引擎的重依赖（str0m / cpal / png / arboard / 抓包库），并消除 SFU 二进制
//! 的双加密栈（ring + aws-lc-rs）。
//!
//! core 以 `pub use aerodesk_protocol::*` 再导出，客户端 crate 的
//! `aerodesk_core::protocol::*` 路径保持不变（零改动）。
//!
//! 编码：JSON 起步（Web 端原生兼容），后续可加二进制变体（同类型，不同 codec）。

pub mod access_unit;
pub mod chat;
pub mod cmd;
pub mod cursor;
pub mod file;
pub mod input;
pub mod jwt;
pub mod signal;
pub mod tls;
pub mod turn;
pub mod util;
