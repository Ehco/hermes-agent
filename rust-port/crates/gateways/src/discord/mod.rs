//! Discord platform — port of `plugins/platforms/discord/adapter.py`
//! (the send/REST + gateway-websocket surface; media/voice/slash lanes not ported).

pub mod format;
pub mod gateway;
pub mod interactions;
pub mod media;
pub mod rest;
pub mod streaming;
