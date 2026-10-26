//! Desktop API server: the Electron UI's backend (port of the Python
//! `hermes_cli/web_routers/chat_ws.py::gateway_ws` + `hermes_cli/web_routers/*`
//! REST surface that `apps/desktop/src/api/*.ts` speaks).
//!
//! Wire contract (from the desktop client, the source of truth):
//! - `GET /api/ws?native_dial=…&ticket=…` — ticketed upgrade. The desktop sends
//!   the subprotocols `["hermes-gateway-v1", "hermes-gateway-ticket.<t>"]`; the
//!   ticket may ride either the query string or the subprotocol, and is
//!   single-use (Python `hermes_cli/dashboard_auth/ws_tickets.py`, 30 s TTL).
//! - Frames are newline-delimited JSON-RPC both ways. Right after accept the
//!   server pushes a `gateway.ready` event notification; `gateway.ping` is
//!   answered inline with `{"ok": true}` (the client's heartbeat).
//! - Server→client events ride one notification shape:
//!   `{"jsonrpc":"2.0","method":"event","params":{type, session_id?, seq?,
//!   replay_epoch?, execution_generation?, payload}}` — `seq` is a per-session
//!   monotonic counter stamped for session-scoped events (the client keeps seq
//!   watermarks and replays a reconnect through `session.events.since`).
//! - Prompts (approval / clarify) are events (`approval.request`,
//!   `clarify.request`, payload keyed by `prompt_id`) answered with the
//!   `approval.respond` / `clarify.respond` RPCs and closed by a
//!   `<kind>.settled` event.

pub mod dispatch;
pub mod jsonrpc;
pub mod rest;
pub mod tickets;
pub mod turn;
pub mod ws;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::config::GatewayConfig;
use hermes_core::agent::ToolHost;
use hermes_core::transport::Transport;

use tickets::TicketStore;

/// Random-id helper: 32 hex chars from a splitmix64 over (nanos, counter, pid).
/// Same shape budget as the Python `secrets.token_urlsafe` tickets without a
/// new dependency.
pub fn new_id(prefix: &str) -> String {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    fn mix(x: u64) -> u64 {
        // splitmix64 finalizer
        let x = x.wrapping_add(0x9E3779B97F4A7C15);
        let z = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        let z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut acc = mix(nanos ^ mix(COUNTER.fetch_add(1, Ordering::Relaxed) as u64));
    let mut out = String::with_capacity(32);
    for _ in 0..4 {
        acc = mix(acc ^ (acc << 13));
        out.push_str(&format!("{acc:08x}"));
    }
    format!("{prefix}-{out}")
}

/// One pending approval/clarify prompt (`gateway/session_pending_controls.py`
/// shape): delivered as an `<kind>.request` event keyed by `prompt_id`, answered
/// through `<kind>.respond`, closed by `<kind>.settled`.
pub struct PendingPrompt {
    pub prompt_id: String,
    pub kind: PromptKind,
    pub execution_generation: u64,
    pub question: Option<String>,
    pub command: Option<String>,
    pub description: Option<String>,
    pub choices: Vec<String>,
    pub multi_select: bool,
    pub edit: Option<Value>,
    /// Resolved by `approval.respond` / `clarify.respond` with
    /// `{"choice": …}` / `{"answer": …}`; `None` receiver = already answered.
    pub tx: Option<oneshot::Sender<Value>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromptKind {
    Approval,
    Clarify,
}

impl PromptKind {
    pub fn event_name(self) -> &'static str {
        match self {
            PromptKind::Approval => "approval.request",
            PromptKind::Clarify => "clarify.request",
        }
    }

    pub fn settled_name(self) -> &'static str {
        match self {
            PromptKind::Approval => "approval.settled",
            PromptKind::Clarify => "clarify.settled",
        }
    }

    pub fn respond_method(self) -> &'static str {
        match self {
            PromptKind::Approval => "approval.respond",
            PromptKind::Clarify => "clarify.respond",
        }
    }
}

/// One attached desktop session (`SessionRuntime`): the in-memory live runtime
/// behind one conversation, plus its replay ring for reconnect replay
/// (Python: `gateway/session_events.py` + `tui_gateway/event_replay.py`).
/// Max stamped events kept per session; past the cap the oldest frame drops
/// and `session.events.since` answers `truncated` / `snapshot_required`.
pub const RING_CAPACITY: usize = 1024;

pub struct Session {
    pub id: String,
    pub stored_id: String,
    pub title: Mutex<String>,
    pub created_at: f64,
    pub last_active_at: Mutex<f64>,
    pub cwd: Mutex<String>,
    pub model: Mutex<Option<String>>,
    pub archived: std::sync::atomic::AtomicBool,
    /// Server-side pin flag (`sessions.pinned`; the sidebar's Pinned section).
    pub pinned: std::sync::atomic::AtomicBool,
    /// Derived read state (`sessions.unread` watermark).
    pub unread: std::sync::atomic::AtomicBool,
    pub running: std::sync::atomic::AtomicBool,
    /// CAS revision for `session.mutate` (clients send `expected_revision`).
    pub revision: AtomicU64,
    /// Execution identity: bumped at each turn start; prompts and control
    /// methods fence on it (the client caches it per session).
    pub generation: AtomicU64,
    /// Per-session event seq counter (`event_replay._stamp_event`).
    pub seq: AtomicU64,
    pub source: Mutex<String>,
    /// "yolo" (auto-allow tool calls) or "ask" (raise an approval prompt).
    pub approval_mode: Mutex<String>,
    pub history: Mutex<Vec<hermes_core::Message>>,
    pub prompts: Mutex<Vec<PendingPrompt>>,
    /// Stamped event frames for `session.events.since` reconnect replay.
    pub ring: Mutex<std::collections::VecDeque<Value>>,
    pub turn: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Submissions queued while a turn runs (`prompt.submit` → `queued`).
    pub queue: Mutex<Vec<(String, String)>>, // (text, admission_id)
    /// Transcript persistence (JSONL via `hermes_session`) when a store dir is
    /// configured.
    pub store: Mutex<Option<hermes_session::SessionStore>>,
}

impl Session {
    /// Fresh session: a new stored id and (when a store dir is configured) an
    /// empty JSONL transcript beside it.
    pub fn new(cwd: impl Into<String>, store_dir: Option<&std::path::Path>) -> Self {
        Self::with_stored(cwd, &new_id("sess"), store_dir)
    }

    /// Stored session: reopens (or creates) `<stored_id>.jsonl` and replays its
    /// lines into the in-memory history (Python: `SessionDB` resume = replay).
    /// The id is the filename stem, so `stored_session_id` aliases resolve.
    pub fn with_stored(
        cwd: impl Into<String>,
        stored_id: &str,
        store_dir: Option<&std::path::Path>,
    ) -> Self {
        let store = store_dir.and_then(|dir| {
            hermes_session::SessionStore::open(dir.join(format!("{stored_id}.jsonl"))).ok()
        });
        let history = store
            .as_ref()
            .and_then(|s| s.load().ok())
            .unwrap_or_default();
        Self {
            stored_id: stored_id.to_string(),
            id: stored_id.to_string(),
            title: Mutex::new(String::new()),
            created_at: epoch_secs(),
            last_active_at: Mutex::new(epoch_secs()),
            cwd: Mutex::new(cwd.into()),
            model: Mutex::new(None),
            archived: std::sync::atomic::AtomicBool::new(false),
            pinned: std::sync::atomic::AtomicBool::new(false),
            unread: std::sync::atomic::AtomicBool::new(false),
            running: std::sync::atomic::AtomicBool::new(false),
            revision: AtomicU64::new(1),
            generation: AtomicU64::new(1),
            seq: AtomicU64::new(0),
            source: Mutex::new("gui".into()),
            approval_mode: Mutex::new("yolo".into()),
            history: Mutex::new(history),
            prompts: Mutex::new(Vec::new()),
            ring: Mutex::new(std::collections::VecDeque::new()),
            turn: Mutex::new(None),
            queue: Mutex::new(Vec::new()),
            store: Mutex::new(store),
        }
    }

    pub fn touch(&self) {
        *self.last_active_at.lock().unwrap() = epoch_secs();
    }

    pub fn last_active(&self) -> f64 {
        *self.last_active_at.lock().unwrap()
    }

    /// Persist one message to the session's JSONL store (best-effort).
    pub fn persist(&self, msg: &hermes_core::Message) {
        if let Some(store) = self.store.lock().unwrap().as_ref() {
            let _ = store.append(msg);
        }
    }

    /// Restore-on-resume: when the in-memory history is empty but the JSONL
    /// store holds lines, replay them (covers a store written before this
    /// process registered the session — e.g. another gateway instance).
    pub fn ensure_loaded(&self) {
        {
            let history = self.history.lock().unwrap();
            if !history.is_empty() {
                return;
            }
        }
        let store = self.store.lock().unwrap();
        if let Some(store) = store.as_ref() {
            if let Ok(loaded) = store.load() {
                if !loaded.is_empty() {
                    *self.history.lock().unwrap() = loaded;
                }
            }
        }
    }

    /// Remove the transcript file (the sidebar's DELETE).
    pub fn delete_store(&self) {
        if let Some(store) = self.store.lock().unwrap().as_ref() {
            let _ = std::fs::remove_file(store.path());
        }
    }

    /// Last user-or-assistant text, newest first (the REST `preview` field and
    /// the sessions search).
    pub fn preview_text(&self) -> String {
        let history = self.history.lock().unwrap();
        preview_of(&history).unwrap_or_default()
    }

    /// The sidebar/REST row (`GET /api/sessions`).
    pub fn row(&self) -> Value {
        let history = self.history.lock().unwrap();
        let count = history.len();
        let preview = preview_of(&history);
        json!({
            "session_id": self.id,
            "stored_session_id": self.stored_id,
            "title": self.title.lock().unwrap().clone(),
            "created_at": self.created_at,
            "last_active_at": self.last_active(),
            "message_count": count,
            "preview": preview,
            "archived": self.archived.load(Ordering::Relaxed),
            "pinned": self.pinned.load(Ordering::Relaxed),
            "unread": self.unread.load(Ordering::Relaxed),
            "is_active": self.running.load(Ordering::Relaxed),
            "source": self.source.lock().unwrap().clone(),
            "cwd": self.cwd.lock().unwrap().clone(),
            "profile": "default",
        })
    }

    /// The REST row (`GET /api/sessions`): the `SessionInfo` contract in
    /// apps/desktop/src/types/hermes.ts — the sidebar renders `id` (not the
    /// WS shape's `session_id`), `last_active`, `started_at`, token counts.
    pub fn rest_row(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title.lock().unwrap().clone(),
            "started_at": self.created_at,
            "ended_at": Value::Null,
            "last_active": self.last_active(),
            "message_count": self.history.lock().unwrap().len(),
            "preview": self.preview_text(),
            "source": self.source.lock().unwrap().clone(),
            "cwd": self.cwd.lock().unwrap().clone(),
            "model": self.model.lock().unwrap().clone(),
            "is_active": self.running.load(Ordering::Relaxed),
            "archived": self.archived.load(Ordering::Relaxed),
            "pinned": self.pinned.load(Ordering::Relaxed),
            "unread": self.unread.load(Ordering::Relaxed),
            "hidden": false,
            "profile": "default",
            "is_default_profile": true,
            "git_branch": Value::Null,
            "git_repo_root": Value::Null,
            "input_tokens": 0,
            "output_tokens": 0,
            "tool_call_count": 0,
        })
    }

    /// Canonical snapshot (the `session.resume` / `session.create` result; the
    /// `CanonicalSnapshot` contract in apps/shared/src/gateway-contract.generated.ts).
    pub fn snapshot(&self, omit_messages: bool, epoch: &str) -> Value {
        let history = self.history.lock().unwrap();
        let messages: Vec<Value> = if omit_messages {
            Vec::new()
        } else {
            history
                .iter()
                .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
                .collect()
        };
        let count = history.len();
        let prompts_json: Vec<Value> = self
            .prompts
            .lock()
            .unwrap()
            .iter()
            .map(prompt_row)
            .collect();
        let running = self.running.load(Ordering::Relaxed);
        let revision = self.revision.load(Ordering::Relaxed);
        let generation = self.generation.load(Ordering::Relaxed);
        json!({
            "session_id": self.id,
            "stored_session_id": self.stored_id,
            "messages": messages,
            "message_count": count,
            "running": running,
            "authority_epoch": 1,
            "replay_epoch": epoch,
            "last_sequence": self.seq.load(Ordering::Relaxed),
            "subscription_id": new_id("sub"),
            "revision": revision,
            "execution_generation": generation,
            "pending": [],
            "prompts": prompts_json,
            "info": {
                "desktop_protocol": "hermes-gateway-v1",
                "running": running,
                "stored_session_id": self.stored_id,
                "profile_id": "default",
                "profile_name": "default",
                "cwd": self.cwd.lock().unwrap().clone(),
                "model": self.model.lock().unwrap().clone(),
                "revision": revision,
                "execution_generation": generation,
            },
        })
    }
}

/// The `prompts` entry of a snapshot (`CanonicalPendingPrompt`).
pub fn prompt_row(p: &PendingPrompt) -> Value {
    json!({
        "kind": match p.kind { PromptKind::Approval => "approval", PromptKind::Clarify => "clarify" },
        "prompt_id": p.prompt_id,
        "execution_generation": p.execution_generation,
        "choices": p.choices,
        "question": p.question,
        "command": p.command,
        "description": p.description,
        "multi_select": p.multi_select,
        "edit": p.edit,
    })
}

pub fn epoch_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Newest user-or-assistant text of a transcript slice (caller holds the lock).
fn preview_of(history: &[hermes_core::Message]) -> Option<String> {
    use hermes_core::model::Content;
    history.iter().rev().find_map(|m| {
        m.content.iter().find_map(|c| match c {
            Content::Text { text } => Some(text.trim().to_string()),
            _ => None,
        })
    })
}

// ── The desktop server ┠──────────────────────────────────────────────────────

/// One subscribed socket: its event sender plus the sessions it attached
/// (`session.resume` / `session.create`); unattached sockets get only
/// session-less broadcasts (`gateway.ready`, `sessions.changed`).
pub struct Subscriber {
    pub tx: mpsc::UnboundedSender<Value>,
    pub sessions: Mutex<HashSet<String>>,
}

impl Clone for Subscriber {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            sessions: Mutex::new(self.sessions.lock().unwrap().clone()),
        }
    }
}

pub struct EventHub {
    subscribers: Mutex<Vec<(u64, Subscriber)>>,
    next_id: AtomicU64,
}

impl Clone for EventHub {
    fn clone(&self) -> Self {
        Self {
            subscribers: Mutex::new(self.subscribers.lock().unwrap().clone()),
            next_id: AtomicU64::new(self.next_id.load(Ordering::Relaxed)),
        }
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
        }
    }
}

impl EventHub {
    /// Register one socket; returns (handle, event receiver). The handle is
    /// used to attach sessions and to unregister at disconnect.
    pub fn subscribe(&self) -> (u64, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.subscribers.lock().unwrap().push((
            id,
            Subscriber {
                tx,
                sessions: Mutex::new(HashSet::new()),
            },
        ));
        (id, rx)
    }

    pub fn unsubscribe(&self, handle: u64) {
        self.subscribers
            .lock()
            .unwrap()
            .retain(|(id, _)| *id != handle);
    }

    /// Attach a session to a subscribed socket (live events start flowing).
    pub fn attach(&self, handle: u64, session_id: &str) {
        if let Some((_, sub)) = self
            .subscribers
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(id, _)| *id == handle)
        {
            sub.sessions.lock().unwrap().insert(session_id.to_string());
        }
    }

    fn fanout(&self, session_id: Option<&str>, frame: &Value) {
        let subs = self.subscribers.lock().unwrap();
        for (_, sub) in subs.iter() {
            let deliver = match session_id {
                None => true,
                Some(sid) => sub.sessions.lock().unwrap().contains(sid),
            };
            if deliver {
                let _ = sub.tx.send(frame.clone());
            }
        }
    }
}

/// Shared immutable server state behind the router (transport + tool host are
/// injected, so tests drive the whole stack with a scripted transport).
pub struct DesktopInner {
    pub tickets: TicketStore,
    /// Live sessions by id (created via `session.create`, attached via
    /// `session.resume`; `stored_session_id` aliases resolve here too).
    pub registry: Mutex<HashMap<String, Arc<Session>>>,
    /// Process replay epoch (`event_replay.replay_epoch`): clients reset their
    /// seq watermarks when it changes across a restart.
    pub epoch: String,
    /// Optional static bearer token (`?token=` auth, remote gateways).
    pub token: Option<String>,
    /// Optional transcript store dir (one JSONL per session).
    pub store_dir: Option<PathBuf>,
    /// Optional skills root (`discover_skills`).
    pub skills_root: Mutex<Option<PathBuf>>,
    /// Skill enable-state (the /api/skills toggle); absent = enabled.
    pub skills_disabled: Mutex<std::collections::HashSet<String>>,
    /// MCP server configs (name → enabled + launch spec) for /api/mcp.
    pub mcp_servers: Mutex<Vec<Value>>,
    pub started_at: Instant,
    pub hub: EventHub,
}

impl DesktopInner {
    /// Server-start recovery: register every stored transcript in `dir` (one
    /// `<session_id>.jsonl` per file) so the sidebar lists restored sessions
    /// with their on-disk message counts before anyone resumes them.
    pub fn restore_sessions(&self, dir: &std::path::Path) -> usize {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return 0,
        };
        let mut restored = 0;
        let mut registry = self.registry.lock().unwrap();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if registry.contains_key(stem) {
                continue;
            }
            let session = Arc::new(Session::with_stored(".", stem, Some(dir)));
            registry.insert(session.id.clone(), session);
            restored += 1;
        }
        restored
    }
}

/// Manual: `std::sync::Mutex` is not `Clone`; the clone takes a snapshot
/// (builder-pattern use only — a cloned `DesktopInner` is a fresh server).
impl Clone for DesktopInner {
    fn clone(&self) -> Self {
        Self {
            tickets: self.tickets.clone(),
            registry: Mutex::new(self.registry.lock().unwrap().clone()),
            epoch: self.epoch.clone(),
            token: self.token.clone(),
            store_dir: self.store_dir.clone(),
            skills_root: Mutex::new(self.skills_root.lock().unwrap().clone()),
            skills_disabled: Mutex::new(self.skills_disabled.lock().unwrap().clone()),
            mcp_servers: Mutex::new(self.mcp_servers.lock().unwrap().clone()),
            started_at: self.started_at,
            hub: self.hub.clone(),
        }
    }
}

/// The desktop API server: generic over the provider transport and tool host
/// exactly like the `/v1/chat` `Gateway`, so tests inject a `ScriptedTransport`.
pub struct DesktopApi<T, H> {
    pub transport: Arc<T>,
    pub host: Arc<H>,
    pub config: GatewayConfig,
    pub inner: Arc<DesktopInner>,
}

impl<T, H> Clone for DesktopApi<T, H> {
    fn clone(&self) -> Self {
        Self {
            transport: Arc::clone(&self.transport),
            host: Arc::clone(&self.host),
            config: self.config.clone(),
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T, H> DesktopApi<T, H>
where
    T: Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    pub fn new(transport: Arc<T>, host: Arc<H>) -> Self {
        Self {
            transport,
            host,
            config: GatewayConfig::default(),
            inner: Arc::new(DesktopInner {
                tickets: TicketStore::new(),
                registry: Mutex::new(HashMap::new()),
                epoch: new_id("epoch"),
                token: None,
                store_dir: None,
                skills_root: Mutex::new(None),
                skills_disabled: Mutex::new(std::collections::HashSet::new()),
                mcp_servers: Mutex::new(Vec::new()),
                started_at: Instant::now(),
                hub: EventHub::default(),
            }),
        }
    }

    /// Configure the static `?token=` credential (remote/gated mode).
    pub fn with_token(self, token: impl Into<String>) -> Self {
        let mut inner = (*self.inner).clone();
        inner.token = Some(token.into());
        Self {
            inner: Arc::new(inner),
            ..self
        }
    }

    /// Configure the per-session JSONL transcript store dir. Existing
    /// transcripts in the dir are restored into the registry (server-start
    /// recovery), so the sidebar lists them with their on-disk counts.
    pub fn with_store_dir(self, dir: impl Into<PathBuf>) -> Self {
        let mut inner = (*self.inner).clone();
        let dir = dir.into();
        inner.restore_sessions(&dir);
        inner.store_dir = Some(dir);
        Self {
            inner: Arc::new(inner),
            ..self
        }
    }

    /// Configure the skills root for the /api/skills routes.
    pub fn with_skills_root(self, dir: impl Into<PathBuf>) -> Self {
        let inner = (*self.inner).clone();
        *inner.skills_root.lock().unwrap() = Some(dir.into());
        Self {
            inner: Arc::new(inner),
            ..self
        }
    }

    /// Seed MCP server configs for the /api/mcp routes.
    pub fn with_mcp_servers(self, servers: Vec<Value>) -> Self {
        let inner = (*self.inner).clone();
        *inner.mcp_servers.lock().unwrap() = servers;
        Self {
            inner: Arc::new(inner),
            ..self
        }
    }

    /// The fully-layered router: REST + the ticketed `/api/ws` upgrade, under
    /// the same CORS/security-header middleware as `/v1/chat`.
    pub fn router(&self) -> axum::Router {
        let app = axum::Router::new().merge(rest::routes(self.clone())).route(
            "/api/ws",
            axum::routing::get(ws::upgrade::<T, H>).with_state(self.clone()),
        );
        app.layer(axum::middleware::from_fn_with_state(
            self.config.clone(),
            crate::middleware::security_headers_middleware,
        ))
    }
}

/// Stamp and emit one gateway event: assigns the per-session `seq`,
/// `replay_epoch` and `execution_generation`, records it in the session's
/// replay ring, and fans it out to subscribed sockets
/// (`event_replay._stamp_event` + the write_json fanout).
pub fn emit_event(
    inner: &DesktopInner,
    session: Option<&Session>,
    event_type: &str,
    payload: Value,
) -> Value {
    let (seq, generation) = match session {
        Some(s) => {
            let seq = s.seq.fetch_add(1, Ordering::Relaxed) + 1;
            (Some(seq), Some(s.generation.load(Ordering::Relaxed)))
        }
        None => (None, None),
    };
    let mut params = json!({ "type": event_type, "payload": payload });
    if let Some(s) = session {
        params["session_id"] = json!(s.id);
    }
    if let Some(seq) = seq {
        params["seq"] = json!(seq);
    }
    if let Some(gen) = generation {
        params["execution_generation"] = json!(gen);
    }
    params["replay_epoch"] = json!(inner.epoch);
    if let Some(s) = session {
        let mut ring = s.ring.lock().unwrap();
        ring.push_back(params.clone());
        while ring.len() > RING_CAPACITY {
            ring.pop_front();
        }
        drop(ring);
        s.touch();
    }
    inner.hub.fanout(session.map(|s| s.id.as_str()), &params);
    params
}

/// The full event notification frame.
pub fn event_frame(params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": "event", "params": params })
}
