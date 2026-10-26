//! The example-dashboard plugin (port of
//! `tests/fixtures/plugins/example-dashboard/dashboard/plugin_api.py`): a
//! stable, side-effect-free REST surface used to exercise the mount path.

use std::sync::Arc;

use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::bus::EventBus;
use crate::manifest::PluginManifest;
use crate::plugin::Plugin;

pub struct ExamplePlugin {
    manifest: PluginManifest,
}

impl ExamplePlugin {
    pub fn new(_bus: Arc<EventBus>) -> Self {
        Self {
            manifest: serde_json::from_value(json!({
                "name": "example",
                "label": "Example",
                "description": "Example dashboard plugin (test fixture)",
                "version": "1.0.0",
                "api": "plugin_api.rs"
            }))
            .expect("static manifest is valid"),
        }
    }
}

impl Plugin for ExamplePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn router(&self) -> Router<()> {
        Router::new()
            .route("/hello", get(hello))
            .route("/whoami", get(whoami))
    }
}

/// `GET /hello` — the Python fixture's greeting contract, byte-for-byte.
async fn hello() -> Json<Value> {
    Json(json!({
        "message": "Hello from the example plugin!",
        "plugin": "example",
        "version": "1.0.0"
    }))
}

/// `GET /whoami` — the plugin "no data" contract: never raise out to the
/// client. (Python reads a secret here; the port folds any failure the same
/// way and probes an environment variable instead of a secret scope.)
async fn whoami() -> Json<Value> {
    match std::env::var("EXAMPLE_PLUGIN_PROBE_KEY") {
        Ok(key) => Json(json!({ "ok": true, "key": key })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    #[tokio::test]
    async fn hello_matches_the_python_fixture() {
        let app = Arc::new(ExamplePlugin::new(Arc::new(EventBus::new()))).router();
        let res = app
            .oneshot(
                axum::http::Request::get("/hello")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v,
            json!({"message": "Hello from the example plugin!", "plugin": "example", "version": "1.0.0"})
        );
    }
}
