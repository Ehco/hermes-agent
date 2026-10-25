//! The agent turn loop (port of the Python agent runtime): push the
//! conversation through a transport, execute tool calls, feed results back,
//! until the assistant produces a turn with no pending tool calls.

use crate::model::{Content, Message, Role};
use crate::transport::{StreamItem, Transport};
use anyhow::Result;
use futures::StreamExt;

/// Executes tool calls on behalf of the loop. Implemented by the tools crate
/// (and by tests with a stub).
pub trait ToolHost {
    fn execute(&self, name: &str, arguments: &str) -> impl Future<Output = Result<String>> + Send;
}
use std::future::Future;

/// Run agent turns until the assistant answers without issuing tool calls.
/// Mutates `history` in place; returns the final assistant text.
pub async fn run_turn<T: Transport, H: ToolHost>(
    transport: &T,
    tools: &[crate::model::ToolSpec],
    history: &mut Vec<Message>,
    host: &H,
) -> Result<String> {
    loop {
        let mut stream = transport.stream(history, tools)?;
        let mut text = String::new();
        let mut calls = Vec::new();

        while let Some(item) = stream.next().await {
            match item? {
                StreamItem::TextDelta(d) => text.push_str(&d),
                StreamItem::ToolCall { id, name, arguments } => calls.push((id, name, arguments)),
                StreamItem::Done => break,
            }
        }

        let mut assistant = Message::text(Role::Assistant, text.clone());
        for (id, name, arguments) in &calls {
            assistant.content.push(Content::ToolUse {
                call: crate::model::ToolCall { id: id.clone(), name: name.clone(), arguments: arguments.clone() },
            });
        }
        history.push(assistant);

        if calls.is_empty() {
            return Ok(text);
        }

        for (id, name, arguments) in calls {
            let output = host.execute(&name, &arguments).await.unwrap_or_else(|e| format!("tool error: {e}"));
            history.push(Message {
                role: Role::Tool,
                content: vec![Content::ToolResult { call_id: id, output }],
            });
        }
    }
}
