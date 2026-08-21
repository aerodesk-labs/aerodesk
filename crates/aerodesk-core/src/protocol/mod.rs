//! 协议类型（自 aerodesk-protocol 再导出；拆分配置见该 crate 文档）。
//!
//! 2026-08-19 曾并入本模块（#535 crate 收敛），#487 审查批次 3 再拆为独立
//! crate：服务端不再被迫链接客户端引擎重依赖。客户端 crate 路径零改动。

pub use aerodesk_protocol::{chat, cmd, cursor, error, file, input, jwt, signal, tls, turn};
