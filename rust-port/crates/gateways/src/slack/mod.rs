//! Slack platform — port of `plugins/platforms/slack/adapter.py` (send path +
//! Events API receive contract; slash commands / native streaming not ported).

pub mod block_kit;
pub mod events;
pub mod format;
pub mod rest;
