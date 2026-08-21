//! aerodesk-protocol —— 跨端共享协议类型（#487 审查批次 3 / #10）。
//!
//! 2026-08-19 曾并入 `aerodesk_core::protocol`（#535 crate 收敛），#10 再拆：
//! 服务端（aerodesk-sfu / aerodesk-signal）只依赖本 crate，不再经共享 crate
//! 被迫链接客户端引擎的重依赖（cpal / png / arboard / 抓包库；SFU 自身的
//! str0m 为直连依赖，属媒体栈本体），并消除 jsonwebtoken→ring 的加密栈重复
//! 来源。注：rouille→tiny_http→rustls 0.20→ring 链仍在 signal/sfu 的直接
//! 依赖里（ring + aws-lc-rs 双栈残余，属遗留项，见 9b66130 提交说明）。
//!
//! core 以 `pub use aerodesk_protocol::*` 再导出，客户端 crate 的
//! `aerodesk_core::protocol::*` 路径保持不变（零改动）。
//!
//! 编码：JSON 起步（Web 端原生兼容），后续可加二进制变体（同类型，不同 codec）。

pub mod access_unit;
pub mod chat;
pub mod cmd;
pub mod cursor;
pub mod error;
pub mod file;
pub mod input;
pub mod jwt;
pub mod signal;
pub mod tls;
pub mod turn;
pub mod util;
