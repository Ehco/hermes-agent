//! The dashboard REST surface the desktop's `api/*.ts` helpers speak (port of
//! the `hermes_cli/web_routers/*` routes the renderer actually calls):
//! liveness/status, the sessions sidebar (list / get / delete / messages /
//! search), skills, MCP server config, profiles, system info and the
//! `POST /api/auth/ws-ticket` mint that feeds the `/api/ws` upgrade.

use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::extract::{Path as AxumPath, Query as AxumQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Json;
use serde_json::{json, Value};

use hermes_core::agent::ToolHost;
use hermes_core::transport::Transport;

use super::DesktopApi;

type Api<T, H> = DesktopApi<T, H>;

pub fn routes<T, H>(api: Api<T, H>) -> axum::Router
where
    T: Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    // NOTE: axum 0.8 path syntax is `/{param}`.
    let sessions = axum::Router::new()
        .route("/api/sessions", get(list_sessions::<T, H>))
        .route("/api/sessions/search", get(search_sessions::<T, H>))
        .route("/api/sessions/{id}", get(get_session::<T, H>).delete(delete_session::<T, H>))
        .route("/api/sessions/{id}/messages", get(session_messages::<T, H>))
        .with_state(api.clone());
    let skills = axum::Router::new()
        .route("/api/skills", get(list_skills::<T, H>))
        .route("/api/skills/content", get(skill_content::<T, H>))
        .route("/api/skills/toggle", post(skill_toggle::<T, H>))
        .with_state(api.clone());
    let mcp = axum::Router::new()
        .route("/api/mcp/servers", get(list_mcp::<T, H>))
        .route("/api/mcp/servers/{name}/enabled", put(mcp_enabled::<T, H>))
        .route("/api/mcp/servers/{name}/test", post(mcp_test::<T, H>))
        .with_state(api.clone());
    axum::Router::new()
        .route("/api/status", get(status::<T, H>))
        .route("/api/system", get(status::<T, H>))
        .route("/api/profiles", get(profiles::<T, H>))
        .route("/api/auth/ws-ticket", post(mint_ticket::<T, H>))
        .with_state(api)
        .merge(sessions)
        .merge(skills)
        .merge(mcp)
}

// ── status / system ──────────────────────────────────────────────────────────

async fn status<T, H>(State(api): State<Api<T, H>>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let registry = api.inner.registry.lock().unwrap();
    let active = registry.values().filter(|s| s.running.load(Ordering::Relaxed)).count();
    Json(json!({
        "ok": true,
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "profile": "default",
        "replay_epoch": api.inner.epoch,
        "active_sessions": active,
        "session_count": registry.len(),
        "uptime_s": api.inner.started_at.elapsed().as_secs(),
    }))
    .into_response()
}

async fn profiles<T, H>(State(api): State<Api<T, H>>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let _ = &api;
    Json(json!({
        "profiles": [{ "name": "default", "is_default": true }],
        "active": "default",
    }))
    .into_response()
}

/// Mint a one-use WS ticket (30 s TTL) for the next `/api/ws` upgrade — the
/// `POST /api/auth/ws-ticket` contract in `hermes_cli/dashboard_auth/ws_tickets.py`.
async fn mint_ticket<T, H>(State(api): State<Api<T, H>>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let ticket = api.inner.tickets.mint(json!({ "user_id": "desktop", "provider": "local" }));
    Json(json!({ "ticket": ticket, "expires_in": super::tickets::TTL_SECONDS })).into_response()
}

// ── sessions ─────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Default)]
pub struct SessionListQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub min_messages: usize,
}

fn default_limit() -> usize {
    20
}

async fn list_sessions<T, H>(
    State(api): State<Api<T, H>>,
    AxumQuery(q): AxumQuery<SessionListQuery>,
) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let registry = api.inner.registry.lock().unwrap();
    let mut rows: Vec<Value> = registry
        .values()
        .filter(|s| s.history.lock().unwrap().len() >= q.min_messages)
        .map(|s| s.row())
        .collect();
    rows.sort_by(|a, b| {
        b["last_active_at"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["last_active_at"].as_f64().unwrap_or(0.0))
    });
    let total = rows.len();
    let page: Vec<Value> = rows.into_iter().skip(q.offset).take(q.limit).collect();
    Json(json!({ "sessions": page, "total": total, "limit": q.limit, "offset": q.offset, "storage": {} }))
        .into_response()
}

async fn search_sessions<T, H>(
    State(api): State<Api<T, H>>,
    AxumQuery(q): AxumQuery<std::collections::HashMap<String, String>>,
) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let needle = q.get("q").map(|s| s.to_lowercase()).unwrap_or_default();
    let registry = api.inner.registry.lock().unwrap();
    let results: Vec<Value> = registry
        .values()
        .filter(|s| {
            s.title.lock().unwrap().to_lowercase().contains(&needle)
                || s.preview_text().to_lowercase().contains(&needle)
        })
        .map(|s| s.row())
        .collect();
    let count = results.len();
    Json(json!({ "results": results, "count": count })).into_response()
}

async fn get_session<T, H>(State(api): State<Api<T, H>>, AxumPath(id): AxumPath<String>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    match find(&api, &id) {
        Some(s) => Json(s.row()).into_response(),
        None => not_found(),
    }
}

async fn delete_session<T, H>(State(api): State<Api<T, H>>, AxumPath(id): AxumPath<String>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let mut registry = api.inner.registry.lock().unwrap();
    let removed = registry.remove(&id);
    if removed.is_none() {
        // Also allow removal by stored id.
        let stored = registry.iter().find(|(_, s)| s.stored_id == id).map(|(k, _)| k.clone());
        if let Some(stored) = stored {
            registry.remove(&stored);
        } else {
            drop(registry);
            return not_found();
        }
    }
    drop(registry);
    super::emit_event(&api.inner, None, "sessions.changed", json!({}));
    Json(json!({ "ok": true, "deleted": id })).into_response()
}

async fn session_messages<T, H>(State(api): State<Api<T, H>>, AxumPath(id): AxumPath<String>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    match find(&api, &id) {
        Some(s) => {
            let history = s.history.lock().unwrap();
            let messages: Vec<Value> = history
                .iter()
                .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
                .collect();
            let total = messages.len();
            Json(json!({ "messages": messages, "total": total })).into_response()
        }
        None => not_found(),
    }
}

fn find<T, H>(api: &Api<T, H>, id: &str) -> Option<std::sync::Arc<super::Session>> {
    let registry = api.inner.registry.lock().unwrap();
    registry
        .get(id)
        .cloned()
        .or_else(|| registry.values().find(|s| s.stored_id == id).cloned())
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "detail": "Session not found" }))).into_response()
}

// ── skills ───────────────────────────────────────────────────────────────────

async fn list_skills<T, H>(State(api): State<Api<T, H>>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let root_guard = api.inner.skills_root.lock().unwrap();
    let Some(root) = root_guard.as_ref() else {
        return Json(json!({ "skills": [], "count": 0 })).into_response();
    };
    let disabled = api.inner.skills_disabled.lock().unwrap();
    let skills: Vec<Value> = hermes_skills::discover_skills(root)
        .into_iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "category": s.category,
                "enabled": !disabled.contains(&s.name),
                "path": s.path.display().to_string(),
            })
        })
        .collect();
    let count = skills.len();
    Json(json!({ "skills": skills, "count": count })).into_response()
}

async fn skill_content<T, H>(
    State(api): State<Api<T, H>>,
    AxumQuery(q): AxumQuery<std::collections::HashMap<String, String>>,
) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let Some(name) = q.get("name").cloned() else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "detail": "missing ?name=" }))).into_response();
    };
    let root_guard = api.inner.skills_root.lock().unwrap();
    let Some(root) = root_guard.as_ref() else {
        return (StatusCode::NOT_FOUND, Json(json!({ "detail": "no skills root" }))).into_response();
    };
    match hermes_skills::load_skill(root, &name) {
        Ok((content, _files)) => Json(json!({ "name": name, "content": content })).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "detail": format!("skill not found: {e:#}") })),
        )
            .into_response(),
    }
}

async fn skill_toggle<T, H>(
    State(api): State<Api<T, H>>,
    Json(body): Json<Value>,
) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let Some(name) = body.get("name").and_then(Value::as_str).map(str::to_string) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "detail": "missing 'name'" }))).into_response();
    };
    let enabled = body.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let mut disabled = api.inner.skills_disabled.lock().unwrap();
    if enabled {
        disabled.remove(&name);
    } else {
        disabled.insert(name.clone());
    }
    Json(json!({ "ok": true, "name": name, "enabled": enabled })).into_response()
}

// ── MCP ──────────────────────────────────────────────────────────────────────

async fn list_mcp<T, H>(State(api): State<Api<T, H>>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let servers = api.inner.mcp_servers.lock().unwrap().clone();
    let count = servers.len();
    Json(json!({ "servers": servers, "count": count })).into_response()
}

async fn mcp_enabled<T, H>(
    State(api): State<Api<T, H>>,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<Value>,
) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let enabled = body.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let mut servers = api.inner.mcp_servers.lock().unwrap();
    match servers.iter_mut().find(|s| s.get("name").and_then(Value::as_str) == Some(name.as_str())) {
        Some(s) => s["enabled"] = json!(enabled),
        None => {
            drop(servers);
            return (StatusCode::NOT_FOUND, Json(json!({ "detail": "unknown MCP server" })))
                .into_response();
        }
    }
    Json(json!({ "ok": true, "name": name, "enabled": enabled })).into_response()
}

/// `POST /api/mcp/servers/{name}/test`: run the server's handshake +
/// `tools/list` through `hermes_mcp::McpClient` and report the tools.
async fn mcp_test<T, H>(
    State(api): State<Api<T, H>>,
    AxumPath(name): AxumPath<String>,
) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let config = {
        let servers = api.inner.mcp_servers.lock().unwrap();
        servers
            .iter()
            .find(|s| s.get("name").and_then(Value::as_str) == Some(name.as_str()))
            .cloned()
    };
    let Some(config) = config else {
        return (StatusCode::NOT_FOUND, Json(json!({ "detail": "unknown MCP server" })))
            .into_response();
    };
    let started = Instant::now();
    let result: Result<Vec<String>, String> = async move {
        let command = config.get("command").and_then(Value::as_str).unwrap_or("").to_string();
        let args: Vec<String> = config
            .get("args")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let mut client = hermes_mcp::client::McpClient::stdio(&command, &args, &Default::default(), None)
            .map_err(|e| format!("spawn failed: {e:#}"))?;
        client.handshake().await.map_err(|e| format!("handshake failed: {e:#}"))?;
        let tools = client.list_tools().await.map_err(|e| format!("tools/list failed: {e:#}"))?;
        Ok(tools.into_iter().map(|t| t.name).collect())
    }
    .await;
    let duration_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(tools) => Json(json!({ "ok": true, "name": name, "tools": tools, "duration_ms": duration_ms }))
            .into_response(),
        Err(e) => Json(json!({ "ok": false, "name": name, "error": e, "duration_ms": duration_ms }))
            .into_response(),
    }
}
