//! The achievements plugin (port of
//! `plugins/hermes-achievements/dashboard/plugin_api.py`, state slice). The
//! Python plugin is polling-only — it exposes no `/events` WebSocket, and the
//! desktop panel uses its REST fallback — so this port is REST-only too.

use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::bus::EventBus;
use crate::manifest::PluginManifest;
use crate::plugin::Plugin;

pub const PLUGIN_NAME: &str = "hermes-achievements";

/// A tiered achievement definition (Rust shape of the Python `DEFINITIONS`).
#[derive(Debug, Clone, Copy)]
pub struct Definition {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Metric the Python evaluator aggregates (informational in this slice).
    pub metric: &'static str,
    pub tier: u32,
}

/// A representative subset of the Python definitions.
pub const DEFINITIONS: &[Definition] = &[
    Definition {
        id: "first-session",
        title: "Hello, Agent",
        description: "Run your first agent session.",
        metric: "sessions",
        tier: 1,
    },
    Definition {
        id: "tool-slinger",
        title: "Tool Slinger",
        description: "Invoke 50 tool calls.",
        metric: "tool_calls",
        tier: 2,
    },
    Definition {
        id: "ship-it",
        title: "Ship It",
        description: "Land a commit through the agent.",
        metric: "commits",
        tier: 3,
    },
];

/// The router state, shared behind an `Arc` (see `router(&self)`).
struct AchievementsState {
    unlocked: Mutex<Vec<&'static str>>,
}

pub struct AchievementsPlugin {
    manifest: PluginManifest,
    state: Arc<AchievementsState>,
}

impl AchievementsPlugin {
    pub fn new(_bus: Arc<EventBus>) -> Self {
        Self {
            manifest: serde_json::from_value(json!({
                "name": PLUGIN_NAME,
                "label": "Achievements",
                "description": "Steam-style achievements for agentic workflows.",
                "icon": "Star",
                "version": "0.4.0",
                "tab": {"path": "/achievements", "position": "after:analytics"},
                "api": "plugin_api.rs"
            }))
            .expect("static manifest is valid"),
            state: Arc::new(AchievementsState {
                unlocked: Mutex::new(Vec::new()),
            }),
        }
    }
}

impl Plugin for AchievementsPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn router(&self) -> Router<()> {
        let state = Arc::clone(&self.state);
        Router::new()
            .route("/achievements", get(list))
            .route("/achievements/{id}/unlock", post(unlock))
            .with_state(state)
    }
}

/// `GET /achievements` — the payload shape the Python handler emits:
/// definitions decorated with the caller's unlocked state.
async fn list(State(state): State<Arc<AchievementsState>>) -> Json<Value> {
    let unlocked = state.unlocked.lock().unwrap_or_else(|e| e.into_inner());
    let items: Vec<Value> = DEFINITIONS
        .iter()
        .map(|d| {
            json!({
                "id": d.id,
                "title": d.title,
                "description": d.description,
                "metric": d.metric,
                "tier": d.tier,
                "unlocked": unlocked.contains(&d.id),
            })
        })
        .collect();
    Json(json!({
        "achievements": items,
        "unlocked_count": unlocked.len(),
    }))
}

/// `POST /achievements/{id}/unlock` — flips the state bit (the Python flow
/// derives unlocks from session scans; the interactive flip is the port's
/// seam for the evaluator).
async fn unlock(
    State(state): State<Arc<AchievementsState>>,
    AxPath(id): AxPath<String>,
) -> Response {
    let known = DEFINITIONS.iter().any(|d| d.id == id);
    if !known {
        return (StatusCode::NOT_FOUND, "no such achievement").into_response();
    }
    let mut unlocked = state.unlocked.lock().unwrap_or_else(|e| e.into_inner());
    if !unlocked.contains(&id.as_str()) {
        unlocked.push(DEFINITIONS.iter().find(|d| d.id == id).unwrap().id);
    }
    Json(json!({ "ok": true, "id": id })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    #[tokio::test]
    async fn unlock_flips_state_in_the_listing() {
        let plugin = Arc::new(AchievementsPlugin::new(Arc::new(EventBus::new())));
        let app = plugin.router();
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::get("/achievements")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["unlocked_count"], 0);
        assert_eq!(
            v["achievements"].as_array().unwrap().len(),
            DEFINITIONS.len()
        );

        let res = app
            .clone()
            .oneshot(
                axum::http::Request::post("/achievements/first-session/unlock")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .oneshot(
                axum::http::Request::get("/achievements")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["unlocked_count"], 1);
        assert!(v["achievements"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "first-session" && a["unlocked"] == true));
    }
}
