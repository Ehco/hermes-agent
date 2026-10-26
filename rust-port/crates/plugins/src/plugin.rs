//! The `Plugin` trait — the Rust counterpart of the Python contract of "a
//! plugin directory whose `manifest.json` declares an `api` file exposing a
//! router". Lifecycle mirrors `PluginManager`: `start` is called when the
//! plugin is enabled (Python: consent gate passed and routes mounted), `stop`
//! when it is disabled or the host shuts down.

use axum::Router;

use crate::manifest::PluginManifest;

/// One ported plugin. Implementations are shared-nothing: they get an
/// `Arc<EventBus>` at construction and expose an axum `Router<()>` that the
/// host nests under `/api/plugins/<name>`.
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;

    /// Routes mounted at `/api/plugins/<name>/...` while the plugin is
    /// enabled. A plugin that declares events exposes a `GET /events`
    /// WebSocket here. Stateful plugins keep their state in an inner `Arc`
    /// and hand a clone of it to axum as the route state.
    fn router(&self) -> Router<()>;

    /// Begin background work (default: nothing). Must not block.
    fn start(&self) {}

    /// Undo `start`; the plugin must be quiet afterwards (default: nothing).
    fn stop(&self) {}
}
