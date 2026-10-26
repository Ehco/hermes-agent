//! The kanban board plugin (port of `plugins/kanban/dashboard/plugin_api.py`,
//! in-memory slice): task CRUD on a single board plus the `GET /events`
//! WebSocket that the desktop `pluginSocket('kanban', '/events', ...)`
//! consumers dial.
//!
//! Event envelope (the contract the desktop tests pin): each frame is
//! `{"events": [<task_event>...], "cursor": <int>}` where a task_event is
//! `{id, task_id, kind, payload, created_at}` — the Rust shape of the Python
//! `task_events` row. `?since=<id>` replays history after that id; no `since`
//! starts at the tail. The poll-interval details of the Python tailer are
//! replaced by direct bus fanout (same frames, no 300 ms latency).

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::bus::EventBus;
use crate::manifest::PluginManifest;
use crate::plugin::Plugin;

pub const PLUGIN_NAME: &str = "kanban";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Task {
    pub id: u64,
    pub title: String,
    #[serde(default = "default_column")]
    pub column: String,
    #[serde(default)]
    pub profile: String,
}

fn default_column() -> String {
    "todo".into()
}

#[derive(Debug, Clone)]
struct TaskEvent {
    id: u64,
    task_id: u64,
    kind: &'static str,
    payload: Value,
    created_at: u64,
}

impl TaskEvent {
    fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "task_id": self.task_id,
            "kind": self.kind,
            "payload": self.payload,
            "created_at": self.created_at,
        })
    }
}

#[derive(Default)]
struct Board {
    next_task_id: u64,
    tasks: Vec<Task>,
}

/// The router state: everything the handlers need, shared behind one `Arc`
/// (`router(&self)` hands a clone of it to axum as the route state).
struct KanbanState {
    bus: Arc<EventBus>,
    board: Mutex<Board>,
}

pub struct KanbanPlugin {
    manifest: PluginManifest,
    state: Arc<KanbanState>,
}

impl KanbanPlugin {
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self {
            manifest: serde_json::from_value(json!({
                "name": "kanban",
                "label": "Kanban",
                "description": "Multi-agent collaboration board",
                "icon": "Package",
                "version": "1.0.0",
                "tab": {"path": "/kanban", "position": "after:skills"},
                "api": "plugin_api.rs"
            }))
            .expect("static manifest is valid"),
            state: Arc::new(KanbanState {
                bus,
                board: Mutex::new(Board::default()),
            }),
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

fn emit(state: &KanbanState, task_id: u64, kind: &'static str, payload: Value) -> u64 {
    let event = TaskEvent {
        id: 0,
        task_id,
        kind,
        payload,
        created_at: KanbanPlugin::now_ms(),
    };
    state.bus.publish(PLUGIN_NAME, vec![event.to_value()])
}

impl Plugin for KanbanPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn router(&self) -> Router<()> {
        Router::new()
            .route("/board", get(board))
            .route("/tasks", post(create_task))
            .route("/tasks/{id}/move", post(move_task))
            .route("/events", get(stream_events))
            .with_state(Arc::clone(&self.state))
    }
}

// -- REST --------------------------------------------------------------------

/// `GET /board` — slurps the board (Python `_board_payload`): tasks plus the
/// event cursor a fresh client should resume from.
async fn board(State(state): State<Arc<KanbanState>>) -> Json<Value> {
    let board = state.board.lock().unwrap_or_else(|e| e.into_inner());
    Json(json!({
        "tasks": board.tasks,
        "cursor": state.bus.cursor(PLUGIN_NAME),
    }))
}

#[derive(Deserialize)]
struct CreateTask {
    title: String,
    #[serde(default)]
    column: Option<String>,
    #[serde(default)]
    profile: Option<String>,
}

/// `POST /tasks {"title", "column"?}` — emits a `created` event.
async fn create_task(
    State(state): State<Arc<KanbanState>>,
    Json(body): Json<CreateTask>,
) -> Response {
    if body.title.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "title must not be empty").into_response();
    }
    let mut board = state.board.lock().unwrap_or_else(|e| e.into_inner());
    board.next_task_id += 1;
    let task = Task {
        id: board.next_task_id,
        title: body.title,
        column: body.column.unwrap_or_else(default_column),
        profile: body.profile.unwrap_or_default(),
    };
    let id = task.id;
    board.tasks.push(task);
    drop(board);
    let cursor = emit(&state, id, "created", json!({}));
    (
        StatusCode::CREATED,
        Json(json!({ "id": id, "cursor": cursor })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct MoveTask {
    column: String,
}

/// `POST /tasks/{id}/move {"column"}` — emits a `moved` event; 404 on an
/// unknown task, like the Python handlers.
async fn move_task(
    State(state): State<Arc<KanbanState>>,
    AxPath(id): AxPath<u64>,
    Json(body): Json<MoveTask>,
) -> Response {
    let mut board = state.board.lock().unwrap_or_else(|e| e.into_inner());
    let Some(task) = board.tasks.iter_mut().find(|t| t.id == id) else {
        return (StatusCode::NOT_FOUND, "no such task").into_response();
    };
    task.column = body.column;
    drop(board);
    let cursor = emit(&state, id, "moved", json!({}));
    Json(json!({ "ok": true, "cursor": cursor })).into_response()
}

// -- WebSocket: /events?since=<event_id> -------------------------------------

/// `GET /events?since=<id>` — upgrade, replay history after `since` (explicit
/// `since=0` replays everything retained; no `since` starts at the tail), then
/// forward live frames. Client messages are ignored, as in Python.
async fn stream_events(
    State(state): State<Arc<KanbanState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    // Python `_since_param`: absent or garbage → None → start at the tail.
    let since = params
        .get("since")
        .and_then(|raw| raw.trim().parse::<u64>().ok());
    let bus = Arc::clone(&state.bus);
    ws.on_upgrade(move |socket| async move { tail_socket(bus, socket, since).await })
}

async fn tail_socket(bus: Arc<EventBus>, socket: WebSocket, since: Option<u64>) {
    let (mut sink, mut stream) = socket.split();
    // The subscription is taken before the replay so no event can fall in the
    // gap between the two.
    let mut rx = bus.subscribe(PLUGIN_NAME);
    if let Some(since) = since {
        for event in bus.replay(PLUGIN_NAME, since) {
            let cursor = event.get("id").and_then(Value::as_u64).unwrap_or(0);
            let frame = json!({ "events": [event], "cursor": cursor });
            if sink
                .send(axum::extract::ws::Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                return;
            }
        }
    }
    // One forwarder until the client disconnects (or the process exits).
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if sink
                .send(axum::extract::ws::Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    while let Some(_ignored) = stream.next().await {} // drain client messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    #[tokio::test]
    async fn create_then_move_emits_the_two_kinds() {
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe(PLUGIN_NAME);
        let plugin = Arc::new(KanbanPlugin::new(bus));
        let app = plugin.router();

        let res = app
            .clone()
            .oneshot(
                axum::http::Request::post("/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(br#"{"title": "Port kanban"}"#.as_slice()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);

        let res = app
            .oneshot(
                axum::http::Request::post("/tasks/1/move")
                    .header("content-type", "application/json")
                    .body(Body::from(br#"{"column": "doing"}"#.as_slice()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let created = rx.recv().await.unwrap();
        assert_eq!(created["events"][0]["kind"], "created");
        assert_eq!(created["events"][0]["task_id"], 1);
        let moved = rx.recv().await.unwrap();
        assert_eq!(moved["events"][0]["kind"], "moved");
        assert_eq!(moved["cursor"], 2);
        assert_eq!(created["cursor"], 1);
    }
}
