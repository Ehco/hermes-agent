//! The streaming turn loop: the same conversation mechanics as
//! `hermes_core::agent::run_turn` (assistant turn → tool calls → tool results
//! → repeat until the assistant answers without tools), but every intermediate
//! step is emitted as a `TurnEvent` so the gateway can stream it as SSE.
//!
//! Event data shapes (each serialized with a `"type"` tag matching the SSE
//! `event:` name):
//! - `text_delta`  `{"type":"text_delta","text":"..."}`
//! - `tool_call`   `{"type":"tool_call","id":"...","name":"...","arguments":"<json str>"}`
//! - `tool_result` `{"type":"tool_result","call_id":"...","output":"..."}`
//! - `done`        `{"type":"done","text":"<final assistant text>"}`
//! - `error`       `{"type":"error","message":"..."}`

use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use hermes_core::agent::ToolHost;
use hermes_core::model::{Content, Message, Role, ToolCall, ToolSpec};
use hermes_core::transport::{StreamItem, Transport};
use serde::Serialize;
use tokio::sync::mpsc;

/// One step of an agent turn, ready to be serialized into an SSE frame.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnEvent {
    TextDelta { text: String },
    ToolCall { id: String, name: String, arguments: String },
    ToolResult { call_id: String, output: String },
    Done { text: String },
    Error { message: String },
}

impl TurnEvent {
    /// SSE `event:` name — always equal to the serialized `"type"` tag.
    pub fn event_name(&self) -> &'static str {
        match self {
            TurnEvent::TextDelta { .. } => "text_delta",
            TurnEvent::ToolCall { .. } => "tool_call",
            TurnEvent::ToolResult { .. } => "tool_result",
            TurnEvent::Done { .. } => "done",
            TurnEvent::Error { .. } => "error",
        }
    }
}

/// Run agent turns until the assistant answers without issuing tool calls,
/// emitting every step through `tx`. Mutates `history` in place; returns the
/// final assistant text (also carried by the `Done` event).
pub async fn run_streaming_turn<T, H>(
    transport: Arc<T>,
    tools: Vec<ToolSpec>,
    history: &mut Vec<Message>,
    host: Arc<H>,
    tx: mpsc::Sender<TurnEvent>,
) -> Result<String>
where
    T: Transport,
    H: ToolHost,
{
    loop {
        let mut stream = transport.stream(history, &tools)?;
        let mut text = String::new();
        let mut calls: Vec<(String, String, String)> = Vec::new();

        while let Some(item) = stream.next().await {
            match item? {
                StreamItem::TextDelta(delta) => {
                    text.push_str(&delta);
                    let _ = tx.send(TurnEvent::TextDelta { text: delta }).await;
                }
                StreamItem::ToolCall { id, name, arguments } => {
                    let _ = tx
                        .send(TurnEvent::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        })
                        .await;
                    calls.push((id, name, arguments));
                }
                StreamItem::Done => break,
            }
        }

        // Same history contract as `agent::run_turn`: the assistant message
        // carries its text plus one ToolUse content block per call.
        let mut assistant = Message::text(Role::Assistant, text.clone());
        for (id, name, arguments) in &calls {
            assistant.content.push(Content::ToolUse {
                call: ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                },
            });
        }
        history.push(assistant);

        if calls.is_empty() {
            return Ok(text);
        }

        for (id, name, arguments) in calls {
            let output = match host.execute(&name, &arguments).await {
                Ok(out) => out,
                Err(e) => format!("tool error: {e}"),
            };
            let _ = tx
                .send(TurnEvent::ToolResult { call_id: id.clone(), output: output.clone() })
                .await;
            history.push(Message {
                role: Role::Tool,
                content: vec![Content::ToolResult { call_id: id, output }],
            });
        }
    }
}
