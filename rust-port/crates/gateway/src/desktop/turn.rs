//! The desktop turn runner: the same conversation mechanics as
//! `hermes_core::agent::run_turn` (assistant turn → tool calls → tool results →
//! repeat), but every step is stamped onto the session's event stream the way
//! `tui_gateway`'s `prompt_turn` does: `message.start`, `message.delta`,
//! `tool.start` / `tool.complete`, `message.complete`, bracketed by
//! `session.updated {running}` frames. Runs as a per-session task so the read
//! loop keeps answering `gateway.ping` mid-turn, and `session.interrupt`
//! aborts it.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use hermes_core::agent::ToolHost;
use hermes_core::model::{Content, Message, Role, ToolCall};
use hermes_core::transport::{StreamItem, Transport};

use super::{emit_event, new_id, DesktopApi, PendingPrompt, PromptKind, Session};

/// Spawn the (queued) turn loop for one submitted prompt. Returns the admission
/// receipt the `prompt.submit` RPC answers with
/// (`{status: "streaming"|"queued", admission_id}`).
pub fn submit<T, H>(api: &DesktopApi<T, H>, session: &Arc<Session>, text: &str) -> Value
where
    T: Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    let admission = new_id("adm");
    if session.running.load(Ordering::Relaxed) {
        session
            .queue
            .lock()
            .unwrap()
            .push((text.to_string(), admission.clone()));
        return json!({ "status": "queued", "admission_id": admission, "session_id": session.id });
    }
    let admission2 = admission.clone();
    spawn_turn(api, session.clone(), text.to_string(), admission);
    json!({ "status": "streaming", "admission_id": admission2, "session_id": session.id })
}

fn spawn_turn<T, H>(api: &DesktopApi<T, H>, session: Arc<Session>, text: String, admission: String)
where
    T: Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    let api = api.clone();
    let session_task = session.clone();
    let task = tokio::spawn(async move {
        let session = session_task;
        let mut text = Some(text);
        let mut admission = Some(admission);
        loop {
            let (t, a) = match (text.take(), admission.take()) {
                (Some(t), Some(a)) => (t, a),
                _ => break,
            };
            run_one_turn::<T, H>(&api, &session, t, &a).await;
            // Drain the submission queue FIFO before going idle.
            match session.queue.lock().unwrap().pop() {
                Some((next_text, next_admission)) => {
                    text = Some(next_text);
                    admission = Some(next_admission);
                }
                None => break,
            }
        }
    });
    *session.turn.lock().unwrap() = Some(task);
}

async fn run_one_turn<T, H>(
    api: &DesktopApi<T, H>,
    session: &Arc<Session>,
    text: String,
    admission: &str,
) where
    T: Transport,
    H: ToolHost,
{
    let inner = &api.inner;
    // One execution generation per turn; clients fence prompts/control on it.
    let generation = session.generation.fetch_add(1, Ordering::Relaxed) + 1;
    session.running.store(true, Ordering::Relaxed);

    // The user row enters the transcript before any event, so the turn's own
    // `session.updated` frames describe a transcript that already has it.
    let user_msg = Message::text(Role::User, &text);
    session.persist(&user_msg);
    session.history.lock().unwrap().push(user_msg);
    session.touch();

    emit_event(
        inner,
        Some(session),
        "session.updated",
        json!({ "running": true, "admission_id": admission, "execution_generation": generation }),
    );
    emit_event(
        inner,
        Some(session),
        "message.start",
        json!({ "text": text }),
    );

    let mut reply = String::new();
    let mut status = "ok".to_string();

    'turn: loop {
        let history = session.history.lock().unwrap().clone();
        let mut stream = match api.transport.stream(&history, &[]) {
            Ok(stream) => stream,
            Err(e) => {
                status = "error".to_string();
                emit_event(
                    inner,
                    Some(session),
                    "error",
                    json!({ "message": format!("{e:#}") }),
                );
                break 'turn;
            }
        };
        let mut calls: Vec<(String, String, String)> = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamItem::TextDelta(delta)) => {
                    reply.push_str(&delta);
                    emit_event(
                        inner,
                        Some(session),
                        "message.delta",
                        json!({ "text": delta }),
                    );
                }
                Ok(StreamItem::ToolCall {
                    id,
                    name,
                    arguments,
                }) => {
                    emit_event(
                        inner,
                        Some(session),
                        "tool.generating",
                        json!({ "name": name }),
                    );
                    calls.push((id, name, arguments));
                }
                Ok(StreamItem::Done) => break,
                Err(e) => {
                    status = "error".to_string();
                    emit_event(
                        inner,
                        Some(session),
                        "error",
                        json!({ "message": format!("{e:#}") }),
                    );
                    break;
                }
            }
        }

        let mut assistant = Message::text(Role::Assistant, reply.clone());
        for (id, name, arguments) in &calls {
            assistant.content.push(Content::ToolUse {
                call: ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                },
            });
        }
        session.persist(&assistant);
        session.history.lock().unwrap().push(assistant);

        if calls.is_empty() {
            break 'turn;
        }

        for (id, name, arguments) in calls {
            emit_event(
                inner,
                Some(session),
                "tool.start",
                json!({ "tool_id": id, "name": name, "args_text": arguments }),
            );
            let allowed = match approval_gate(api, session, &name, &arguments).await {
                Ok(true) => true,
                Ok(false) => false,
                Err(cancelled) => {
                    // Interrupted mid-tool: the interrupt handler owns teardown.
                    let _ = cancelled;
                    return;
                }
            };
            let output = if allowed {
                match api.host.execute(&name, &arguments).await {
                    Ok(out) => out,
                    Err(e) => format!("tool error: {e}"),
                }
            } else {
                "Tool call denied by user.".to_string()
            };
            emit_event(
                inner,
                Some(session),
                "tool.complete",
                json!({ "tool_id": id, "name": name, "result_text": output }),
            );
            let result_msg = Message {
                role: Role::Tool,
                content: vec![Content::ToolResult {
                    call_id: id,
                    output,
                }],
            };
            session.persist(&result_msg);
            session.history.lock().unwrap().push(result_msg);
        }
    }

    // Reply segment boundary (`message.delta {text: null}`), then the complete.
    emit_event(
        inner,
        Some(session),
        "message.delta",
        json!({ "text": null }),
    );
    emit_event(
        inner,
        Some(session),
        "message.complete",
        json!({ "text": reply, "status": status, "admission_id": admission }),
    );
    session.running.store(false, Ordering::Relaxed);
    emit_event(
        inner,
        Some(session),
        "session.updated",
        json!({ "running": false }),
    );
}

/// The approval gate: "yolo" allows everything; "ask" raises an
/// `approval.request` and waits for `approval.respond`. Returns whether the
/// call may run; `Err` when the wait ended by interruption (task abort).
async fn approval_gate<T, H>(
    api: &DesktopApi<T, H>,
    session: &Arc<Session>,
    name: &str,
    arguments: &str,
) -> Result<bool, ()>
where
    T: Transport,
    H: ToolHost,
{
    if *session.approval_mode.lock().unwrap() != "ask" {
        return Ok(true);
    }
    let prompt_id = new_id("pr");
    let (tx, rx) = oneshot::channel();
    let pending = PendingPrompt {
        prompt_id: prompt_id.clone(),
        kind: PromptKind::Approval,
        execution_generation: session.generation.load(Ordering::Relaxed),
        question: None,
        command: Some(name.to_string()),
        description: Some(format!("Execute tool `{name}`")),
        choices: vec!["allow".into(), "always".into(), "deny".into()],
        multi_select: false,
        edit: Some(json!({ "arguments": arguments })),
        tx: Some(tx),
    };
    session.prompts.lock().unwrap().push(pending);
    emit_event(
        &api.inner,
        Some(session),
        "approval.request",
        json!({
            "prompt_id": prompt_id,
            "kind": "approval",
            "command": name,
            "description": format!("Execute tool `{name}`"),
            "choices": ["allow", "always", "deny"],
            "execution_generation": session.generation.load(Ordering::Relaxed),
        }),
    );
    let answer = match rx.await {
        Ok(a) => a,
        Err(_) => return Err(()), // sender dropped: interrupted/reaped
    };
    // The answered prompt retires with its `<kind>.settled` event.
    super::dispatch::settle_prompt(&api.inner, session, &prompt_id);
    Ok(matches!(
        answer.get("choice").and_then(Value::as_str),
        Some("allow") | Some("always")
    ))
}
