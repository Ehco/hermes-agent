//! The `/api/ws` RPC dispatcher: the canonical session methods the desktop
//! sends (`apps/desktop/src/api/canonical-protocol.ts::preparePayload`), backed
//! by the in-memory session registry. Unknown methods answer the JSON-RPC
//! `-32601` the shared client reads as version skew.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};

use hermes_core::agent::ToolHost;
use hermes_core::transport::Transport;

use super::{emit_event, DesktopApi, DesktopInner, PromptKind, Session};

/// Dispatch one JSON-RPC request; `Some(response)` is written back on the
/// socket. `handle` is this socket's hub subscription (event routing).
pub async fn dispatch<T, H>(
    api: &DesktopApi<T, H>,
    handle: u64,
    id: Value,
    method: &str,
    params: Value,
) -> Option<Value>
where
    T: Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    let params = params.as_object().cloned().unwrap_or_default();
    match method {
        "session.create" => Some(create(api, handle, id, params).await),
        "session.resume" | "session.activate" => Some(resume(api, handle, id, &params)),
        "session.list" => Some(list(api, id)),
        "session.info" => Some(info(api, id, &params)),
        "session.history" => Some(history(api, id, &params)),
        "prompt.submit" => Some(submit(api, handle, id, params).await),
        "session.interrupt" => Some(interrupt(api, id, &params)),
        "session.mutate" => Some(mutate(api, id, &params)),
        "approval.respond" => Some(respond(api, id, params, true)),
        "clarify.respond" => Some(respond(api, id, params, false)),
        "session.events.since" => Some(events_since(api, id, &params)),
        "gateway.status" => Some(gateway_status(api, id)),
        _ => Some(super::jsonrpc::error(
            id,
            super::jsonrpc::METHOD_NOT_FOUND,
            &format!("method not found: {method}"),
            None,
        )),
    }
}

fn get_session<'a>(inner: &DesktopInner, id: Option<&Value>) -> Option<Arc<Session>> {
    let sid = id?.as_str()?;
    let registry = inner.registry.lock().unwrap();
    registry
        .get(sid)
        .cloned()
        .or_else(|| registry.values().find(|s| s.stored_id == sid).cloned())
}

async fn create<T, H>(
    api: &DesktopApi<T, H>,
    handle: u64,
    id: Value,
    params: serde_json::Map<String, Value>,
) -> Value
where
    T: Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    let cwd = params.get("cwd").and_then(Value::as_str).unwrap_or(".");
    let session = Arc::new(Session::new(cwd, api.inner.store_dir.as_deref()));
    if let Some(title) = params.get("title").and_then(Value::as_str) {
        *session.title.lock().unwrap() = title.to_string();
    }
    if let Some(model) = params.get("model").and_then(Value::as_str) {
        *session.model.lock().unwrap() = Some(model.to_string());
    }
    // Test/dev hook: the canonical create contract carries no approval_mode
    // (it rides config), but the desktop fixtures exercise both gates.
    if let Some(mode) = params.get("approval_mode").and_then(Value::as_str) {
        *session.approval_mode.lock().unwrap() = mode.to_string();
    }
    *session.source.lock().unwrap() = params
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("gui")
        .to_string();
    let session_id = session.id.clone();
    api.inner.registry.lock().unwrap().insert(session_id.clone(), session);
    // Attach THIS socket so live frames flow to the creator.
    api.inner.hub.attach(handle, &session_id);
    emit_event(&api.inner, None, "sessions.changed", json!({}));
    let attach_params = serde_json::Map::from_iter([( "session_id".to_string(), json!(session_id) )]);
    resume(api, handle, id, &attach_params)
}

fn resume<T, H>(api: &DesktopApi<T, H>, handle: u64, id: Value, params: &serde_json::Map<String, Value>) -> Value
where
    T: Transport,
    H: ToolHost,
{
    let Some(session) = get_session(&api.inner, params.get("session_id")) else {
        return super::jsonrpc::refused(
            id,
            "session_not_found",
            "session not found",
            None,
        );
    };
    let omit = params.get("omit_messages").and_then(Value::as_bool).unwrap_or(false)
        || params.get("defer_history").and_then(Value::as_bool).unwrap_or(false);
    api.inner.hub.attach(handle, &session.id);
    // Restore-on-resume: a stored transcript replays into the snapshot
    // (Python `SessionDB.get_messages` on resume).
    session.ensure_loaded();
    let snapshot = session.snapshot(omit, &api.inner.epoch);
    // Open prompts re-deliver in the snapshot; the live request events were
    // (or will be) emitted by their raiser.
    super::jsonrpc::response(id, snapshot)
}

fn list<T, H>(api: &DesktopApi<T, H>, id: Value) -> Value
where
    T: Transport,
    H: ToolHost,
{
    let registry = api.inner.registry.lock().unwrap();
    let mut rows: Vec<Value> = registry.values().map(|s| s.row()).collect();
    rows.sort_by(|a, b| {
        b["last_active_at"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["last_active_at"].as_f64().unwrap_or(0.0))
    });
    let total = rows.len();
    super::jsonrpc::response(id, json!({ "sessions": rows, "total": total }))
}

fn info<T, H>(api: &DesktopApi<T, H>, id: Value, params: &serde_json::Map<String, Value>) -> Value
where
    T: Transport,
    H: ToolHost,
{
    let Some(session) = get_session(&api.inner, params.get("session_id")) else {
        return super::jsonrpc::refused(id, "session_not_found", "session not found", None);
    };
    super::jsonrpc::response(id, session.row())
}

fn history<T, H>(api: &DesktopApi<T, H>, id: Value, params: &serde_json::Map<String, Value>) -> Value
where
    T: Transport,
    H: ToolHost,
{
    let Some(session) = get_session(&api.inner, params.get("session_id")) else {
        return super::jsonrpc::refused(id, "session_not_found", "session not found", None);
    };
    let history = session.history.lock().unwrap();
    let messages: Vec<Value> = history
        .iter()
        .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
        .collect();
    let count = messages.len();
    super::jsonrpc::response(id, json!({ "messages": messages, "total": count }))
}

async fn submit<T, H>(
    api: &DesktopApi<T, H>,
    handle: u64,
    id: Value,
    params: serde_json::Map<String, Value>,
) -> Value
where
    T: Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    let Some(session) = get_session(&api.inner, params.get("session_id")) else {
        return super::jsonrpc::refused(
            id,
            "session_not_found",
            "session not found",
            None,
        );
    };
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            params
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let Some(text) = text else {
        return super::jsonrpc::error(
            id,
            super::jsonrpc::INVALID_PARAMS,
            "prompt.submit requires 'text'",
            None,
        );
    };
    api.inner.hub.attach(handle, &session.id);
    let receipt = super::turn::submit(api, &session, &text);
    super::jsonrpc::response(id, receipt)
}

fn interrupt<T, H>(api: &DesktopApi<T, H>, id: Value, params: &serde_json::Map<String, Value>) -> Value
where
    T: Transport,
    H: ToolHost,
{
    let Some(session) = get_session(&api.inner, params.get("session_id")) else {
        return super::jsonrpc::refused(id, "session_not_found", "session not found", None);
    };
    if let Some(task) = session.turn.lock().unwrap().take() {
        task.abort();
    }
    // Fail any pending prompt waiters: their one-shots drop on task abort.
    session.prompts.lock().unwrap().clear();
    session.queue.lock().unwrap().clear();
    session.running.store(false, Ordering::Relaxed);
    emit_event(&api.inner, Some(&session), "session.updated", json!({ "running": false }));
    super::jsonrpc::response(id, json!({ "session_id": session.id, "running": false }))
}

/// The CAS-fenced `session.mutate` (rename / archive / model / …): a stale
/// `expected_revision` refuses with `reason: "revision_conflict"` carrying the
/// owner's current fences, exactly what the client's `protocol.failure` adopts.
fn mutate<T, H>(api: &DesktopApi<T, H>, id: Value, params: &serde_json::Map<String, Value>) -> Value
where
    T: Transport,
    H: ToolHost,
{
    let Some(session) = get_session(&api.inner, params.get("session_id")) else {
        return super::jsonrpc::refused(id, "session_not_found", "session not found", None);
    };
    let revision = session.revision.load(Ordering::Relaxed);
    let generation = session.generation.load(Ordering::Relaxed);
    if let Some(expected) = params.get("expected_revision").and_then(Value::as_u64) {
        if expected != revision {
            return super::jsonrpc::refused(
                id,
                "revision_conflict",
                "session changed since the edit was prepared",
                Some((revision, generation)),
            );
        }
    }
    let operation = params.get("operation").and_then(Value::as_str).unwrap_or("");
    let payload = params.get("payload").cloned().unwrap_or(json!({}));
    let mut receipt = json!({ "session_id": session.id, "operation": operation });
    match operation {
        "rename" => {
            let title = payload.get("title").and_then(Value::as_str).unwrap_or("");
            *session.title.lock().unwrap() = title.to_string();
        }
        "archive" => {
            let archived = payload.get("archived").and_then(Value::as_bool).unwrap_or(true);
            session.archived.store(archived, Ordering::Relaxed);
        }
        "model" => {
            let model = payload.get("model").and_then(Value::as_str).map(str::to_string);
            *session.model.lock().unwrap() = model;
        }
        "reasoning" | "workspace" | "branch" | "compress" | "rewind" => {
            // Accepted no-ops on this port: the receipt advances the revision.
        }
        _ => {
            return super::jsonrpc::error(
                id,
                super::jsonrpc::INVALID_PARAMS,
                &format!("unknown mutation operation: {operation}"),
                None,
            );
        }
    }
    let new_revision = session.revision.fetch_add(1, Ordering::Relaxed) + 1;
    receipt["revision"] = json!(new_revision);
    receipt["ok"] = json!(true);
    emit_event(&api.inner, Some(&session), "session.updated", json!({ "revision": new_revision }));
    super::jsonrpc::response(id, receipt)
}

/// `approval.respond` / `clarify.respond`: resolve the pending prompt's
/// one-shot, emit `<kind>.settled`, answer `{status: "resolved", prompt_id}`.
fn respond<T, H>(api: &DesktopApi<T, H>, id: Value, params: serde_json::Map<String, Value>, approval: bool) -> Value
where
    T: Transport,
    H: ToolHost,
{
    let prompt_id = params
        .get("prompt_id")
        .or_else(|| params.get("request_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let Some(session) = get_session(&api.inner, params.get("session_id")) else {
        return super::jsonrpc::refused(id, "session_not_found", "session not found", None);
    };
    let prompts = session.prompts.lock().unwrap();
    let Some(pending) = prompts.iter().find(|p| p.prompt_id == prompt_id) else {
        drop(prompts);
        return super::jsonrpc::refused(
            id,
            "prompt_not_found",
            "prompt is stale or unavailable",
            None,
        );
    };
    let kind = pending.kind;
    let generation = pending.execution_generation;
    drop(prompts);
    if approval != (kind == PromptKind::Approval) {
        return super::jsonrpc::error(
            id,
            super::jsonrpc::INVALID_PARAMS,
            "prompt kind does not match the respond method",
            None,
        );
    }
    let answer = if approval {
        json!({ "choice": params.get("choice").and_then(Value::as_str).unwrap_or("allow") })
    } else {
        json!({ "answer": params.get("answer").and_then(Value::as_str).unwrap_or("") })
    };
    let tx = {
        let mut prompts = session.prompts.lock().unwrap();
        let pos = prompts.iter().position(|p| p.prompt_id == prompt_id);
        pos.and_then(|pos| prompts.remove(pos).tx)
    };
    if let Some(tx) = tx {
        let _ = tx.send(answer);
    }
    settle_prompt(&api.inner, &session, &prompt_id);
    super::jsonrpc::response(
        id,
        json!({ "status": "resolved", "prompt_id": prompt_id, "resolved": 1,
               "execution_generation": generation }),
    )
}

/// Remove the answered prompt and emit its `<kind>.settled` event.
pub fn settle_prompt(inner: &DesktopInner, session: &Arc<Session>, prompt_id: &str) {
    let mut prompts = session.prompts.lock().unwrap();
    let pos = prompts.iter().position(|p| p.prompt_id == prompt_id);
    let kind = pos.map(|pos| prompts.remove(pos).kind);
    drop(prompts);
    if let Some(kind) = kind {
        emit_event(
            inner,
            Some(session),
            kind.settled_name(),
            json!({ "prompt_id": prompt_id, "kind": match kind { PromptKind::Approval => "approval", PromptKind::Clarify => "clarify" } }),
        );
    }
}

/// Reconnect replay (`session.events.since`): frames after `last_seen` out of
/// the session's ring, plus the still-open prompts as `open_requests` so a
/// reconnecting viewer restores its cards.
fn events_since<T, H>(api: &DesktopApi<T, H>, id: Value, params: &serde_json::Map<String, Value>) -> Value
where
    T: Transport,
    H: ToolHost,
{
    let Some(session) = get_session(&api.inner, params.get("session_id")) else {
        return super::jsonrpc::refused(id, "session_not_found", "session not found", None);
    };
    let last_seen = params
        .get("last_seen")
        .or_else(|| params.get("last_sequence"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let epoch_given = params.get("replay_epoch").and_then(Value::as_str);
    let epoch_current = &api.inner.epoch;
    let epoch_changed = epoch_given.is_some_and(|e| e != epoch_current);

    let ring = session.ring.lock().unwrap();
    let latest_seq = session.seq.load(Ordering::Relaxed);
    let total_after = ring.iter().filter(|e| e["seq"].as_u64().is_some_and(|s| s > last_seen)).count();
    let events: Vec<Value> = ring
        .iter()
        .filter(|e| e["seq"].as_u64().is_some_and(|s| s > last_seen))
        .cloned()
        .collect();
    let truncated = total_after > events.len() || epoch_changed;
    // A changed epoch or a truncated ring means the client must re-resume.
    let snapshot_required = truncated;
    let open_requests: Vec<Value> = session
        .prompts
        .lock()
        .unwrap()
        .iter()
        .map(|p| {
            let kind = match p.kind { PromptKind::Approval => "approval", PromptKind::Clarify => "clarify" };
            json!({ "id": p.prompt_id, "method": kind, "params": {
                "session_id": session.id, "prompt_id": p.prompt_id, "kind": kind,
                "command": p.command, "description": p.description,
                "question": p.question, "choices": p.choices, "multi_select": p.multi_select,
            } })
        })
        .collect();
    super::jsonrpc::response(
        id,
        json!({
            "events": events,
            "latest_seq": latest_seq,
            "last_sequence": latest_seq,
            "epoch": epoch_current,
            "replay_epoch": epoch_current,
            "truncated": truncated,
            "snapshot_required": snapshot_required,
            "count": events.len(),
            "open_requests": open_requests,
        }),
    )
}

fn gateway_status<T, H>(api: &DesktopApi<T, H>, id: Value) -> Value
where
    T: Transport,
    H: ToolHost,
{
    let registry = api.inner.registry.lock().unwrap();
    super::jsonrpc::response(
        id,
        json!({
            "ok": true,
            "replay_epoch": api.inner.epoch,
            "sessions": registry.len(),
        }),
    )
}
