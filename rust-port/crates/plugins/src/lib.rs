//! Plugin platform framework (port of the Python `plugins/` + dashboard plugin
//! API mount in `hermes_cli/web_server_dashboard.py`).
//!
//! The client-visible surface matches `apps/desktop/src/api/plugins.ts`:
//!
//! - REST: every enabled plugin's router is nested under
//!   `/api/plugins/<name>/...`; the framework also serves
//!   `GET /api/plugins` (list), and
//!   `POST /api/plugins/<name>/enable` / `/disable`.
//! - WebSocket: a plugin's `GET /events` endpoint fans out through the shared
//!   event bus with the desktop envelope `{"events": [...], "cursor": <int>}`
//!   (the kanban contract the desktop `pluginSocket` consumers rely on).
//!
//! See `port_notes.md` next to this crate's `Cargo.toml` for the gateway
//! adapter and the list of deferred plugins.

pub mod bus;
pub mod host;
pub mod manifest;
pub mod plugin;
pub mod plugins;
pub mod routes;

pub use bus::EventBus;
pub use host::{HostedPluginInfo, PluginFactory, PluginHost};
pub use manifest::PluginManifest;
pub use plugin::Plugin;
pub use plugins::{achievements::AchievementsPlugin, example::ExamplePlugin, kanban::KanbanPlugin};
pub use routes::{attach_gateway, plugin_api_router};
