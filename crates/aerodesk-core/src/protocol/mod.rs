//! aerodesk_core::protocol — 跨平台共享协议类型（2026-08-19 自 aerodesk-protocol 并入）。
//!
//! 一份定义，服务端（aerodesk-sfu）、客户端核心（aerodesk-core）、Web 端共用：
//! - [`input`]：观看端 → 被控端的输入事件协议（鼠标/键盘/触控/剪贴板）
//! - [`chat`]：聊天消息协议（#458，文本消息，经 `chat` data channel）
//! - [`signal`]：信令消息（房间/认证/ICE 交换/TURN 凭证）
//!
//! 编码：JSON 起步（Web 端原生兼容），后续可加二进制变体（同类型，不同 codec）。

pub mod chat;
pub mod cmd;
pub mod cursor;
pub mod file;
pub mod input;
pub mod signal;
pub mod turn;

pub mod jwt;
pub mod tls;
