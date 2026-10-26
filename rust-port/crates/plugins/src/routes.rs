//! The framework REST surface: `/api/plugins/...` (the desktop door).
//!
//! - `GET  /api/plugins` — the mounted-plugin listing.
//! - `POST /api/plugins/<name>/enable` / `/disable` — the consent gates.
//! - each enabled plugin's own router nested at `/api/plugins/<name>/`.
//!
//! `attach_gateway` is the thin seam for `crates/gateway`: it merges these
//! routes into an existing router (Python equivalent: `_mount_plugin_api_routes`
//! adding routes to the dashboard FastAPI app). See `port_notes.md`.

use std::sync::Arc;

use axum::extract::{Path as AxPath, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde_json::json;
use tower::ServiceExt;

use crate::host::PluginHost;

/// The framework router: control routes + a live dispatcher for
/// `/api/plugins/<name>/<rest>`. Unlike the Python FastAPI mount (routers stay
/// mounted; consent is re-checked per request), the dispatcher looks the
/// plugin up per request, so enable/disable takes effect at once — the Rust
/// equivalent of the hot-mount fallback.
pub fn plugin_api_router(host: Arc<PluginHost>) -> Router<()> {
    Router::new()
        .route("/api/plugins", get(list_plugins))
        .route("/api/plugins/{name}/enable", post(enable))
        .route("/api/plugins/{name}/disable", post(disable))
        .route("/api/plugins/{name}/{*rest}", any(dispatch))
        .with_state(host)
}

/// `any /api/plugins/<name>/<rest>` — run the request through the plugin's
/// own router with the namespace stripped, refusing disabled/unknown plugins
/// (Python `_plugin_api_request_refused`: an unknown name counts as a user
/// plugin — refused).
async fn dispatch(
    State(host): State<Arc<PluginHost>>,
    AxPath((name, rest)): AxPath<(String, String)>,
    req: axum::extract::Request,
) -> Response {
    if !host.is_enabled(&name) {
        return (StatusCode::NOT_FOUND, "plugin not found").into_response();
    }
    let Some(plugin) = host.plugin(&name) else {
        return (StatusCode::NOT_FOUND, "plugin not found").into_response();
    };
    // Strip `/api/plugins/<name>` — the namespace IS the boundary (desktop
    // `pluginPathSuffix` mirrors this from the client side).
    let mut parts = req.into_parts();
    let query = parts.0.uri.query().map(str::to_string);
    let mut pq = format!("/{rest}");
    if let Some(q) = query.as_deref().filter(|q| !q.is_empty()) {
        pq = format!("/{rest}?{q}");
    }
    parts.0.uri = Uri::builder().path_and_query(pq).build().unwrap();
    // The outer router's match params must not leak into the plugin's Path
    // extractors (axum reads them from the request extensions) — but hyper's
    // upgrade state must survive or WebSocket routes could not upgrade.
    let upgrade = parts
        .0
        .extensions
        .get::<hyper::upgrade::OnUpgrade>()
        .cloned();
    parts.0.extensions.clear();
    if let Some(upgrade) = upgrade {
        parts.0.extensions.insert(upgrade);
    }
    let subreq = axum::extract::Request::from_parts(parts.0, parts.1);
    plugin.router().oneshot(subreq).await.unwrap()
}

async fn list_plugins(State(host): State<Arc<PluginHost>>) -> Json<serde_json::Value> {
    Json(json!({ "plugins": host.list() }))
}

/// `POST /api/plugins/<name>/enable` — mirrors the Python consent gate +
/// mount; 404 on an unknown plugin, 409 when already enabled (the Python
/// runtime gate treats a re-enable as a no-op).
async fn enable(State(host): State<Arc<PluginHost>>, AxPath(name): AxPath<String>) -> Response {
    if host.plugin(&name).is_none() {
        return (StatusCode::NOT_FOUND, "plugin not found").into_response();
    }
    host.enable(&name);
    Json(json!({ "ok": true, "name": name, "enabled": true })).into_response()
}

async fn disable(State(host): State<Arc<PluginHost>>, AxPath(name): AxPath<String>) -> Response {
    if host.plugin(&name).is_none() {
        return (StatusCode::NOT_FOUND, "plugin not found").into_response();
    }
    host.disable(&name);
    Json(json!({ "ok": true, "name": name, "enabled": false })).into_response()
}

/// Thin adapter for `crates/gateway`: merge the plugin platform's routes into
/// an existing axum router. `Gateway::router()` builds a fresh `Router`, so
/// the gateway calls this before layering middleware:
///
/// ```ignore
/// let host = Arc::new(PluginHost::new());
/// let app = hermes_plugins::attach_gateway(Router::new(), Arc::clone(&host))
///     .route("/health", get(health)) // ... rest of the gateway routes
/// ```
pub fn attach_gateway(parent: Router<()>, host: Arc<PluginHost>) -> Router<()> {
    parent.merge(plugin_api_router(host))
}
