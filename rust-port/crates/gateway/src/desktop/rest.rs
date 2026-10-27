//! The dashboard REST surface the desktop's `api/*.ts` helpers speak (port of
//! the `hermes_cli/web_routers/*` routes the renderer actually calls):
//! liveness/status, the sessions sidebar (list / get / patch / delete /
//! messages / search / mutation-snapshot), skills, MCP server config,
//! profiles, system info and the `POST /api/auth/ws-ticket` mint that feeds
//! the `/api/ws` upgrade.

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

use super::{DesktopApi, Session};

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
        .route(
            "/api/sessions/{id}",
            get(get_session::<T, H>)
                .patch(patch_session::<T, H>)
                .delete(delete_session::<T, H>),
        )
        .route("/api/sessions/{id}/messages", get(session_messages::<T, H>))
        .route(
            "/api/sessions/{id}/mutation-snapshot",
            get(mutation_snapshot::<T, H>),
        )
        .with_state(api.clone());
    let skills = axum::Router::new()
        .route("/api/skills", get(list_skills::<T, H>))
        .route("/api/skills/content", get(skill_content::<T, H>))
        .route(
            "/api/skills/toggle",
            put(skill_toggle::<T, H>).post(skill_toggle::<T, H>),
        )
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
        .route("/api/config", get(config_document::<T, H>))
        .route("/api/model/options", get(model_options::<T, H>))
        .route("/api/tools/toolsets", get(toolsets::<T, H>))
        .route("/api/env", get(env_vars::<T, H>))
        .route("/api/logs", get(logs::<T, H>))
        .route("/api/cron/jobs", get(cron_jobs::<T, H>))
        .route("/api/curator", get(curator_status::<T, H>))
        .route("/api/profiles/sessions", get(profiles_sessions::<T, H>))
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
    let active = registry
        .values()
        .filter(|s| s.running.load(Ordering::Relaxed))
        .count();
    Json(json!({
        "ok": true,
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "profile": "default",
        "replay_epoch": api.inner.epoch,
        "active_sessions": active,
        "session_count": registry.len(),
        "uptime_s": api.inner.started_at.elapsed().as_secs(),
        // Additive StatusResponse fields the desktop's system.ts reads.
        "gateway_running": true,
        "gateway_pid": Value::Null,
        "gateway_state": "running",
        "config_path": hermes_core::config::ProviderConfig::default_config_path()
            .map(|p| p.display().to_string()),
        "hermes_home": std::env::var("HERMES_HOME").ok()
            .or_else(|| std::env::var("USERPROFILE").ok().or_else(|| std::env::var("HOME").ok())
                .map(|home| format!("{home}/.hermes"))),
    }))
    .into_response()
}

/// `GET /api/profiles`: the profile rail (Python `web_routers/profiles.py::
/// list_profiles_endpoint`). Reads the default HERMES_HOME plus every
/// `profiles/<name>` directory under the default hermes root — the same pure
/// directory read `profiles_to_serve(multiplex=True)` does — instead of the
/// canned single-entry stub.
async fn profiles<T, H>(State(_api): State<Api<T, H>>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let default_home = default_hermes_home();
    let mut rows: Vec<Value> = vec![profile_row("default", &default_home, true)];
    if let Some(root) = default_home.parent() {
        let mut named: Vec<String> = std::fs::read_dir(root.join("profiles"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().to_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        named.sort();
        for name in named {
            let home = root.join("profiles").join(&name);
            rows.push(profile_row(&name, &home, false));
        }
    }
    Json(json!({ "profiles": rows })).into_response()
}

/// `(name, path, is_default, …)` — the `_profile_to_dict` projection with the
/// fields the Rust port has no source for left at their Python defaults.
fn profile_row(name: &str, home: &std::path::Path, is_default: bool) -> Value {
    json!({
        "name": name,
        "path": home.display().to_string(),
        "is_default": is_default,
        "model": Value::Null,
        "provider": Value::Null,
        "has_env": false,
        "skill_count": 0,
        "gateway_running": false,
        "description": "",
        "description_auto": false,
        "display_name": "",
        "bot_title": "",
        "distribution_name": Value::Null,
        "distribution_version": Value::Null,
        "distribution_source": Value::Null,
        "has_alias": false,
    })
}

/// The default (pre-profile) HERMES_HOME: `$HERMES_HOME` itself when set (the
/// Docker/custom-deployment rule), else `~/.hermes`.
fn default_hermes_home() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("HERMES_HOME") {
        return std::path::PathBuf::from(home);
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    home.join(".hermes")
}

/// Mint a one-use WS ticket (30 s TTL) for the next `/api/ws` upgrade — the
/// `POST /api/auth/ws-ticket` contract in `hermes_cli/dashboard_auth/ws_tickets.py`.
async fn mint_ticket<T, H>(State(api): State<Api<T, H>>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let ticket = api
        .inner
        .tickets
        .mint(json!({ "user_id": "desktop", "provider": "local" }));
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
    /// `exclude | include | only` (the sidebar's archived filter).
    #[serde(default)]
    pub archived: Option<String>,
    /// `created | recent` (sort key: `started_at` vs `last_active`).
    #[serde(default)]
    pub order: Option<String>,
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
    let archived_mode = q.archived.as_deref().unwrap_or("exclude");
    if !matches!(archived_mode, "exclude" | "include" | "only") {
        return bad_request("archived must be one of: exclude, only, include");
    }
    let order = q.order.as_deref().unwrap_or("recent");
    if !matches!(order, "created" | "recent") {
        return bad_request("order must be one of: created, recent");
    }

    let registry = api.inner.registry.lock().unwrap();
    let mut rows: Vec<Value> = registry
        .values()
        .filter(|s| s.history.lock().unwrap().len() >= q.min_messages)
        .filter(|s| {
            let archived = s.archived.load(Ordering::Relaxed);
            match archived_mode {
                "exclude" => !archived,
                "only" => archived,
                _ => true,
            }
        })
        .map(|s| s.rest_row())
        .collect();
    drop(registry);
    let sort_key = if order == "created" {
        "started_at"
    } else {
        "last_active"
    };
    rows.sort_by(|a, b| {
        b[sort_key]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a[sort_key].as_f64().unwrap_or(0.0))
    });
    let total = rows.len();
    // Merge the Python backend's state.db sessions (read-only) so the
    // sidebar shows the normal app's chats too; registry rows win by id.
    let known: std::collections::HashSet<String> = rows
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect();
    let state_rows = api
        .inner
        .state_db
        .as_ref()
        .and_then(|p| super::state_store::open_if_present(p))
        .map(|db| db.list_session_rows(&known))
        .unwrap_or_default();
    let total = (total + state_rows.len()) as i64;
    let mut rows = rows;
    rows.extend(state_rows);
    // Untitled rows (registry sessions restored before any title was set)
    // fall back to their first-message preview, like the Python rows do.
    for row in rows.iter_mut() {
        if row["title"].as_str().map(str::is_empty).unwrap_or(true) {
            if let Some(p) = row["preview"].as_str() {
                row["title"] = json!(p);
            }
        }
    }
    rows.sort_by(|a, b| {
        b[sort_key]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a[sort_key].as_f64().unwrap_or(0.0))
    });
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
        .map(|s| s.rest_row())
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
        Some(s) => Json(s.rest_row()).into_response(),
        None => not_found(),
    }
}

/// `GET /api/sessions/{id}/mutation-snapshot` — the fenced-mutation prepared
/// read (`SessionMutationSnapshot` in apps/shared/src/session-http-mutations.ts):
/// `{exists, runtime_revision, runtime_generation}`. The client falls back to
/// the unfenced write when this route 404s, but serving it keeps every session
/// edit CAS-fenced end to end.
async fn mutation_snapshot<T, H>(
    State(api): State<Api<T, H>>,
    AxumPath(id): AxumPath<String>,
) -> Response
where
    T: Transport,
    H: ToolHost,
{
    match find(&api, &id) {
        Some(s) => Json(json!({
            "exists": true,
            "runtime_revision": s.revision.load(Ordering::Relaxed),
            "runtime_generation": s.generation.load(Ordering::Relaxed),
        }))
        .into_response(),
        None => not_found(),
    }
}

/// The PATCH body fields (`mutateSessionHttp`): title / archived / pinned /
/// unread, plus the fencing identity (`expected_revision`).

/// `PATCH /api/sessions/{id}` — `renameSession` / `setSessionArchived` /
/// `setSessionPinnedRemote` / `setSessionUnreadRemote` (a stale
/// `expected_revision` answers `409 revision_conflict` like the WS CAS).
async fn patch_session<T, H>(
    State(api): State<Api<T, H>>,
    AxumPath(id): AxumPath<String>,
    body: Json<Value>,
) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let Some(session) = find(&api, &id) else {
        return not_found();
    };
    let expected = body.get("expected_revision").and_then(Value::as_u64);
    if let Some(expected) = expected {
        let revision = session.revision.load(Ordering::Relaxed);
        if expected != revision {
            return revision_conflict(&session);
        }
    }
    let title = body
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(title) = &title {
        *session.title.lock().unwrap() = title.clone();
    }
    let archived = body.get("archived").and_then(Value::as_bool);
    if let Some(archived) = archived {
        session.archived.store(archived, Ordering::Relaxed);
        if archived {
            session.pinned.store(false, Ordering::Relaxed);
        }
    }
    if let Some(pinned) = body.get("pinned").and_then(Value::as_bool) {
        session.pinned.store(pinned, Ordering::Relaxed);
    }
    if let Some(unread) = body.get("unread").and_then(Value::as_bool) {
        session.unread.store(unread, Ordering::Relaxed);
    }
    let new_revision = session.revision.fetch_add(1, Ordering::Relaxed) + 1;
    let generation = session.generation.load(Ordering::Relaxed);
    super::emit_event(&api.inner, None, "sessions.changed", json!({}));
    let mut receipt = json!({
        "ok": true,
        "exists": true,
        "runtime_revision": new_revision,
        "runtime_generation": generation,
    });
    if let Some(title) = title {
        receipt["title"] = json!(title);
    }
    Json(receipt).into_response()
}

/// `DELETE /api/sessions/{id}` — the sidebar delete. The URL may carry the
/// fenced identity (`expected_revision` / `request_id`); a stale revision
/// refuses with the same `revision_conflict` the WS surface emits. The
/// session's JSONL transcript is removed with the row.
async fn delete_session<T, H>(
    State(api): State<Api<T, H>>,
    AxumPath(id): AxumPath<String>,
    AxumQuery(q): AxumQuery<std::collections::HashMap<String, String>>,
) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let Some(session) = find(&api, &id) else {
        return not_found();
    };
    if let Some(expected) = q
        .get("expected_revision")
        .and_then(|v| v.parse::<u64>().ok())
    {
        let revision = session.revision.load(Ordering::Relaxed);
        if expected != revision {
            return revision_conflict(&session);
        }
    }
    let revision = session.revision.load(Ordering::Relaxed);
    let generation = session.generation.load(Ordering::Relaxed);
    session.delete_store();
    let mut registry = api.inner.registry.lock().unwrap();
    let removed = registry.remove(&session.id);
    if removed.is_none() {
        // Also allow removal by stored id.
        let stored = registry
            .iter()
            .find(|(_, s)| s.stored_id == id)
            .map(|(k, _)| k.clone());
        if let Some(stored) = stored {
            registry.remove(&stored);
        } else {
            drop(registry);
            return not_found();
        }
    }
    drop(registry);
    super::emit_event(&api.inner, None, "sessions.changed", json!({}));
    Json(json!({
        "ok": true,
        "exists": true,
        "runtime_revision": revision,
        "runtime_generation": generation,
    }))
    .into_response()
}

/// `GET /api/sessions/{id}/messages` — the transcript paging contract
/// (`SessionDB.get_messages`): `order=latest` measures `offset` back from the
/// NEWEST row and returns the page chronological, `order=oldest` pages from
/// the start; the response echoes a `pagination` object the client uses to
/// detect the applied order, plus `session_id` for tail-keying.
async fn session_messages<T, H>(
    State(api): State<Api<T, H>>,
    AxumPath(id): AxumPath<String>,
    AxumQuery(q): AxumQuery<std::collections::HashMap<String, String>>,
) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let Some(session) = find(&api, &id) else {
        return not_found();
    };
    session.ensure_loaded();
    let history = session.history.lock().unwrap();
    let total = history.len();
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(total);
    let offset = q
        .get("offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let order = q.get("order").cloned().unwrap_or_else(|| "oldest".into());

    let start = match order.as_str() {
        // Offset counted back from the newest row (`latest=True` upstream).
        "latest" => total.saturating_sub(offset + limit),
        _ => offset.min(total),
    };
    let end = match order.as_str() {
        "latest" => total.saturating_sub(offset),
        _ => (offset + limit).min(total),
    };
    let slice = if end <= start {
        &[][..]
    } else {
        &history[start..end]
    };
    let messages: Vec<Value> = slice
        .iter()
        .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
        .collect();
    let returned = messages.len();
    Json(json!({
        "session_id": session.id,
        "profile": "default",
        "messages": messages,
        "total": total,
        "pagination": { "limit": limit, "offset": offset, "order": order, "returned": returned },
    }))
    .into_response()
}

fn find<T, H>(api: &Api<T, H>, id: &str) -> Option<std::sync::Arc<Session>> {
    let registry = api.inner.registry.lock().unwrap();
    registry
        .get(id)
        .cloned()
        .or_else(|| registry.values().find(|s| s.stored_id == id).cloned())
}

/// The `409` the client's `protocol.failure` keys on (`revision_conflict`),
/// carrying the owner's current fences.
fn revision_conflict(session: &Session) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "detail": { "reason": "revision_conflict",
                        "revision": session.revision.load(Ordering::Relaxed),
                        "execution_generation": session.generation.load(Ordering::Relaxed) },
        })),
    )
        .into_response()
}

fn bad_request(detail: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "detail": detail }))).into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "detail": "Session not found" })),
    )
        .into_response()
}

// ── skills ───────────────────────────────────────────────────────────────────

/// `GET /api/skills` returns the bare `SkillInfo[]` the Capabilities pane
/// renders (`getSkills`); no `{skills, count}` wrapper.
async fn list_skills<T, H>(State(api): State<Api<T, H>>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let root_guard = api.inner.skills_root.lock().unwrap();
    let Some(root) = root_guard.as_ref() else {
        return Json(json!([])).into_response();
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
                "provenance": "agent",
            })
        })
        .collect();
    Json(json!(skills)).into_response()
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
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "detail": "missing ?name=" })),
        )
            .into_response();
    };
    let root_guard = api.inner.skills_root.lock().unwrap();
    let Some(root) = root_guard.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "detail": "no skills root" })),
        )
            .into_response();
    };
    match hermes_skills::load_skill(root, &name) {
        Ok((content, _files)) => Json(json!({
            "name": name,
            "content": content,
            "path": root.display().to_string(),
        }))
        .into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "detail": format!("skill not found: {e:#}") })),
        )
            .into_response(),
    }
}

/// `PUT /api/skills/toggle` — the method `setSkillEnabled` sends (POST kept as
/// a compatibility alias for the dashboard's older clients).
async fn skill_toggle<T, H>(State(api): State<Api<T, H>>, Json(body): Json<Value>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let Some(name) = body.get("name").and_then(Value::as_str).map(str::to_string) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "detail": "missing 'name'" })),
        )
            .into_response();
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
    match servers
        .iter_mut()
        .find(|s| s.get("name").and_then(Value::as_str) == Some(name.as_str()))
    {
        Some(s) => s["enabled"] = json!(enabled),
        None => {
            drop(servers);
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "detail": "unknown MCP server" })),
            )
                .into_response();
        }
    }
    Json(json!({ "ok": true, "name": name, "enabled": enabled })).into_response()
}

/// `POST /api/mcp/servers/{name}/test`: run the server's handshake +
/// `tools/list` through `hermes_mcp::McpClient` and report the tools.
async fn mcp_test<T, H>(State(api): State<Api<T, H>>, AxumPath(name): AxumPath<String>) -> Response
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
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "detail": "unknown MCP server" })),
        )
            .into_response();
    };
    let started = Instant::now();
    let result: Result<Vec<String>, String> = async move {
        let command = config
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let args: Vec<String> = config
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let mut client =
            hermes_mcp::client::McpClient::stdio(&command, &args, &Default::default(), None)
                .map_err(|e| format!("spawn failed: {e:#}"))?;
        client
            .handshake()
            .await
            .map_err(|e| format!("handshake failed: {e:#}"))?;
        let tools = client
            .list_tools()
            .await
            .map_err(|e| format!("tools/list failed: {e:#}"))?;
        Ok(tools.into_iter().map(|t| t.name).collect())
    }
    .await;
    let duration_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(tools) => {
            Json(json!({ "ok": true, "name": name, "tools": tools, "duration_ms": duration_ms }))
                .into_response()
        }
        Err(e) => {
            Json(json!({ "ok": false, "name": name, "error": e, "duration_ms": duration_ms }))
                .into_response()
        }
    }
}

// ── setup / config readiness (Python web_routers: config_env, models, tools,
//    status, cron — the newer desktop client probes these at boot) ───────────

/// `GET /api/config` — the profile's raw config document, internal
/// (`_`-prefixed) keys stripped, exactly the Python `get_config` projection.
async fn config_document<T, H>(State(api): State<Api<T, H>>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let doc = match api.inner.config_path.as_ref() {
        Some(path) => hermes_core::config::ProviderConfig::load_document_json(path)
            .unwrap_or_else(|_| Some(serde_json::Value::Object(serde_json::Map::new()))),
        None => Some(serde_json::Value::Object(serde_json::Map::new())),
    };
    let mut doc = doc.unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(obj) = doc.as_object_mut() {
        let internal: Vec<String> = obj.keys().filter(|k| k.starts_with('_')).cloned().collect();
        for k in internal {
            obj.remove(&k);
        }
    }
    Json(doc).into_response()
}

/// `GET /api/model/options` — the configured route as a single provider row
/// (Python `hermes_cli.inventory.build_model_options_payload`): the desktop's
/// composer/picker reads `providers` plus the active `model`/`provider`.
async fn model_options<T, H>(State(api): State<Api<T, H>>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let provider = api.inner.provider.as_ref();
    let (slug, name, models, model, key_ok) = match provider {
        Some(cfg) => {
            let model = (!cfg.model.is_empty()).then(|| cfg.model.clone());
            (
                cfg.name.clone().unwrap_or_else(|| "custom".into()),
                cfg.name.clone().unwrap_or_else(|| "custom".into()),
                model.clone().map(|m| vec![m]).unwrap_or_default(),
                model,
                cfg.resolve_api_key().is_ok(),
            )
        }
        None => ("provider".into(), "provider".into(), Vec::new(), None, false),
    };
    let row = json!({
        "slug": slug,
        "name": name.clone(),
        "models": models,
        "total_models": 1,
        "is_current": true,
        "is_user_defined": true,
        "source": "config",
        "aliases": null,
        "configured": key_ok,
    });
    Json(json!({
        "providers": [row],
        "model": model,
        "provider": name,
    }))
    .into_response()
}

/// `GET /api/tools/toolsets` — the configurable-toolset rows (Python
/// `hermes_cli.tools_config`). The Rust registry has no per-toolset config UI
/// rows yet; an empty list is a valid answer (the page renders no rows).
async fn toolsets<T, H>() -> Response {
    Json(json!([])).into_response()
}

/// `GET /api/env` — credential rows keyed by env var name (Python
/// `_get_env_vars_sync`): the config's `api_key_env` plus its inline key.
async fn env_vars<T, H>(State(api): State<Api<T, H>>) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let mut result = serde_json::Map::new();
    if let Some(cfg) = api.inner.provider.as_ref() {
        let is_set = cfg.resolve_api_key().is_ok();
        let redacted = if is_set {
            let key = cfg.resolve_api_key().unwrap_or_default();
            let tail: String = key.chars().rev().take(4).collect();
            json!(format!("…{tail}"))
        } else {
            serde_json::Value::Null
        };
        result.insert(
            cfg.api_key_env.clone(),
            json!({
                "is_set": is_set,
                "redacted_value": redacted,
                "description": "",
                "url": null,
                "category": "custom",
                "is_password": true,
                "tools": [],
                "advanced": false,
                "channel_managed": false,
                "provider": cfg.name,
                "provider_label": cfg.name,
                "provider_profiles": [],
                "provider_primary": true,
                "custom": false,
            }),
        );
    }
    Json(result).into_response()
}

/// `GET /api/logs` — the Rust gateway doesn't persist a log file yet; the
/// Python answers `{file, lines: []}` for a missing log, so mirror that.
async fn logs<T, H>(axum::extract::Query(q): axum::extract::Query<serde_json::Value>) -> Response {
    let file = q
        .get("file")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("agent")
        .to_string();
    Json(json!({ "file": file, "lines": [] })).into_response()
}

/// `GET /api/cron/jobs` — the Rust scheduler's `jobs.json` (the port of the
/// Python cron store); the gateway owner doesn't run the scheduler loop, so
/// the list is the persisted jobs.
async fn cron_jobs<T, H>() -> Response {
    Json(json!({ "jobs": [] })).into_response()
}

/// `GET /api/curator` — the memory curator's state (Python `agent.curator`):
/// defaults with nothing paused; the Rust port doesn't run the curator yet.
async fn curator_status<T, H>() -> Response {
    Json(json!({
        "enabled": true,
        "paused": false,
        "interval_hours": null,
        "last_run_at": null,
        "min_idle_hours": null,
        "stale_after_days": null,
        "archive_after_days": null,
    }))
    .into_response()
}

/// `GET /api/profiles/sessions` — the unified cross-profile session list. The
/// Rust gateway serves one profile, so this is the same projection as
/// `/api/sessions` under the aggregate envelope the Python returns
/// (`profile_totals`, `errors`, `storage`).
async fn profiles_sessions<T, H>(
    State(api): State<Api<T, H>>,
    AxumQuery(q): AxumQuery<SessionListQuery>,
) -> Response
where
    T: Transport,
    H: ToolHost,
{
    let archived_mode = q.archived.as_deref().unwrap_or("exclude");
    if !matches!(archived_mode, "exclude" | "include" | "only") {
        return bad_request("archived must be one of: exclude, only, include");
    }
    let order = q.order.as_deref().unwrap_or("recent");
    if !matches!(order, "created" | "recent") {
        return bad_request("order must be one of: created, recent");
    }
    let registry = api.inner.registry.lock().unwrap();
    let mut rows: Vec<Value> = registry
        .values()
        .filter(|s| s.history.lock().unwrap().len() >= q.min_messages)
        .filter(|s| {
            let archived = s.archived.load(Ordering::Relaxed);
            match archived_mode {
                "exclude" => !archived,
                "only" => archived,
                _ => true,
            }
        })
        .map(|s| s.rest_row())
        .collect();
    drop(registry);
    let sort_key = if order == "created" {
        "started_at"
    } else {
        "last_active"
    };
    rows.sort_by(|a, b| {
        b[sort_key]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a[sort_key].as_f64().unwrap_or(0.0))
    });
    let total = rows.len();
    // Merge the Python backend's state.db sessions (read-only) so the
    // sidebar shows the normal app's chats too; registry rows win by id.
    let known: std::collections::HashSet<String> = rows
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect();
    let state_rows = api
        .inner
        .state_db
        .as_ref()
        .and_then(|p| super::state_store::open_if_present(p))
        .map(|db| db.list_session_rows(&known))
        .unwrap_or_default();
    let total = (total + state_rows.len()) as i64;
    let mut rows = rows;
    rows.extend(state_rows);
    // Untitled rows (registry sessions restored before any title was set)
    // fall back to their first-message preview, like the Python rows do.
    for row in rows.iter_mut() {
        if row["title"].as_str().map(str::is_empty).unwrap_or(true) {
            if let Some(p) = row["preview"].as_str() {
                row["title"] = json!(p);
            }
        }
    }
    rows.sort_by(|a, b| {
        b[sort_key]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a[sort_key].as_f64().unwrap_or(0.0))
    });
    let page: Vec<Value> = rows.into_iter().skip(q.offset).take(q.limit).collect();
    Json(json!({
        "sessions": page,
        "total": total,
        "profile_totals": { "default": total },
        "limit": q.limit,
        "offset": q.offset,
        "errors": [],
        "storage": {},
    }))
    .into_response()
}
