//! hermes-gateways: chat-platform bridges (Telegram, Discord, Slack) ported
//! from the Python gateway platforms (`plugins/platforms/{telegram,discord,slack}`).
//!
//! Shared pieces (contract, chunking, tables) live in `contract.rs`,
//! `chunking.rs`, `tables.rs`; one module per platform:
//!
//! - `telegram/` — Bot API long-poll + MarkdownV2 (already ported)
//! - `discord/`  — bot-token REST (2000-char chunking) + gateway websocket
//!   (RFC6455 over tokio TcpStream: heartbeat/identify/dispatch)
//! - `slack/`    — chat.postMessage (39k chunking, mrkdwn + Block Kit) +
//!   Events API envelope parsing

pub mod chunking;
pub mod contract;
pub mod discord;
pub mod slack;
pub mod tables;
pub mod telegram;
