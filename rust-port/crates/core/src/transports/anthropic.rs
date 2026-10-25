//! Anthropic Messages transport (`api_mode='anthropic_messages'`).
//!
//! Port of the Python anthropic.py wire contract: POST `{base_url}/v1/messages`
//! with `x-api-key` + `anthropic-version: 2023-06-01` headers, system prompt in
//! a top-level `system` field, tools as `{name, description, input_schema}`, and
//! SSE events (`content_block_start` / `content_block_delta` with `text_delta`
//! and `input_json_delta` / `content_block_stop` / `message_delta` /
//! `message_stop`). Tool-use arguments accumulate from `input_json_delta`
//! `partial_json` fragments on the open `tool_use` block.

use crate::config::ProviderConfig;
use crate::model::{Content, Message, Role, ToolSpec};
use crate::transport::{StreamItem, Transport};
use crate::transports::{truncate_body, ConnectThen, EventSink, SseStream};
use anyhow::{anyhow, Result};
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;

pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Matches the Python `anthropic.py` `_BUILD_KWARG_DEFAULTS["max_tokens"]`.
pub const DEFAULT_MAX_TOKENS: i64 = 16384;

pub struct AnthropicTransport {
    cfg: ProviderConfig,
    http: reqwest::Client,
}

impl AnthropicTransport {
    pub fn new(cfg: ProviderConfig) -> Self {
        Self { cfg, http: reqwest::Client::new() }
    }

    pub fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'))
    }

    /// `{model, max_tokens, messages, system, stream[, tools]}` request body.
    pub fn build_body(&self, messages: &[Message], tools: &[ToolSpec]) -> Value {
        let mut body = json!({
            "model": self.cfg.model,
            "max_tokens": DEFAULT_MAX_TOKENS,
            "messages": wire_messages(messages),
            "stream": true,
        });
        let system = wire_system(messages);
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = wire_tools(tools);
        }
        body
    }
}

/// System-role messages move to the top-level `system` string (joined with \n).
pub fn wire_system(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| crate::transports::text_of(&m.content))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tools in Anthropic format: `{name, description, input_schema}`.
pub fn wire_tools(tools: &[ToolSpec]) -> Value {
    json!(tools
        .iter()
        .map(|t| json!({
            "name": t.name,
            "description": t.description,
            "input_schema": t.schema,
        }))
        .collect::<Vec<_>>())
}

/// OpenAI-shaped history -> Anthropic messages. Assistant ToolUse blocks become
/// `{"type": "tool_use", id, name, input}`; tool results become a `user` message
/// carrying a `tool_result` block (the Anthropic contract for results).
pub fn wire_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        if m.role == Role::System {
            continue; // moved to `system`
        }
        let mut content = Vec::new();
        for c in &m.content {
            match c {
                Content::Text { text } => {
                    if !text.is_empty() {
                        content.push(json!({ "type": "text", "text": text }));
                    }
                }
                Content::ToolUse { call } => {
                    // `arguments` is a JSON string on the shared model; Anthropic
                    // wants the parsed object in `input`.
                    let input = serde_json::from_str::<Value>(&call.arguments).unwrap_or(json!({}));
                    content.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": input,
                    }));
                }
                Content::ToolResult { call_id, output } => {
                    content.push(json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": output,
                    }));
                }
            }
        }
        if content.is_empty() {
            continue;
        }
        let role = match m.role {
            Role::Assistant => "assistant",
            _ => "user", // user messages AND tool results ride the user role
        };
        out.push(json!({ "role": role, "content": content }));
    }
    out
}

/// Stream accumulator for Anthropic SSE events.
#[derive(Debug, Default)]
pub struct AnthropicAccumulator {
    /// The currently open `tool_use` content block, if any.
    open: Option<OpenTool>,
    /// Index of the block `open` belongs to (blocks stream interleaved).
    open_index: i64,
    finished: bool,
}

#[derive(Debug, Clone)]
struct OpenTool {
    id: String,
    name: String,
    arguments: String,
}

impl AnthropicAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle one SSE event: only the `data:` JSON matters (event names mirror it).
    pub fn on_data(&mut self, data: &str, out: &mut Vec<Result<StreamItem>>) {
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                out.push(Err(anyhow!("anthropic: bad SSE data ({e}): {data}")));
                return;
            }
        };
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "content_block_start" => {
                let index = v.get("index").and_then(Value::as_i64).unwrap_or(0);
                if let Some(cb) = v.get("content_block") {
                    if cb.get("type").and_then(Value::as_str) == Some("tool_use") {
                        self.open = Some(OpenTool {
                            id: cb.get("id").and_then(Value::as_str).unwrap_or("").into(),
                            name: cb.get("name").and_then(Value::as_str).unwrap_or("").into(),
                            arguments: String::new(),
                        });
                        self.open_index = index;
                    }
                }
            }
            "content_block_delta" => {
                let Some(delta) = v.get("delta") else { return };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            out.push(Ok(StreamItem::TextDelta(text.to_string())));
                        }
                    }
                    Some("input_json_delta") => {
                        let index = v.get("index").and_then(Value::as_i64);
                        if let Some(open) = self.open.as_mut() {
                            if index.map_or(true, |i| i == self.open_index) {
                                if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                                    open.arguments.push_str(partial);
                                }
                            }
                        }
                    }
                    // "thinking_delta" and friends: reasoning text is not
                    // surfaced in the shared stream; ignore.
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(open) = self.open.take() {
                    out.push(Ok(StreamItem::ToolCall {
                        id: open.id,
                        name: open.name,
                        arguments: open.arguments,
                    }));
                }
            }
            "message_delta" => {
                // stop_reason rides here; the normalized stream needs only the
                // tool calls + Done, so nothing to do.
            }
            "message_stop" => {
                if !self.finished {
                    self.finished = true;
                    out.push(Ok(StreamItem::Done));
                }
            }
            "error" => {
                let msg = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown anthropic error");
                out.push(Err(anyhow!("anthropic error: {msg}")));
            }
            // "message_start", "ping": nothing to do.
            _ => {}
        }
    }
}

impl EventSink for AnthropicAccumulator {
    fn on_event(&mut self, event: &crate::transports::sse::SseEvent, out: &mut Vec<Result<StreamItem>>) {
        self.on_data(&event.data, out);
    }

    fn on_finish(&mut self, out: &mut Vec<Result<StreamItem>>) {
        // Flush an unterminated tool call (stream cut before content_block_stop).
        if let Some(open) = self.open.take() {
            out.push(Ok(StreamItem::ToolCall {
                id: open.id,
                name: open.name,
                arguments: open.arguments,
            }));
        }
        if !self.finished {
            self.finished = true;
            out.push(Ok(StreamItem::Done));
        }
    }
}

impl Transport for AnthropicTransport {
    fn stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamItem>> + Send>>> {
        let body = self.build_body(messages, tools);
        let url = self.endpoint();
        let key = self.cfg.resolve_api_key()?;
        let http = self.http.clone();

        let connect: Pin<Box<dyn Future<Output = Result<SseStream<AnthropicAccumulator>>> + Send>> =
            Box::pin(async move {
                let resp = http
                    .post(url)
                    .header("x-api-key", key)
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .json(&body)
                    .send()
                    .await?;
                let status = resp.status();
                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("anthropic HTTP {}: {}", status, truncate_body(&text)));
                }
                let raw = resp.bytes_stream().map(|r| r.map(|b| b.to_vec()));
                Ok(SseStream::new(Box::pin(raw), AnthropicAccumulator::new()))
            });
        Ok(Box::pin(ConnectThen::new(connect)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolCall;
    use crate::transports::sse::SseParser;

    fn drive(acc: &mut AnthropicAccumulator, bytes: &[u8]) -> Vec<StreamItem> {
        let mut p = SseParser::new();
        let mut items = Vec::new();
        for ev in p.feed(bytes) {
            acc.on_event(&ev, &mut items);
        }
        for ev in p.finish() {
            acc.on_event(&ev, &mut items);
        }
        items.into_iter().collect::<Result<Vec<_>>>().unwrap()
    }

    /// StreamItem has no PartialEq (shared enum); compare via Debug.
    fn debug(items: &[StreamItem]) -> Vec<String> {
        items.iter().map(|i| format!("{i:?}")).collect()
    }

    const STREAM: &[u8] = br#"event: message_start
data: {"type":"message_start","message":{"role":"assistant"}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi "}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"there"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"Paris\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}

event: message_stop
data: {"type":"message_stop"}

"#;

    #[test]
    fn streams_text_and_accumulates_input_json_deltas() {
        let mut acc = AnthropicAccumulator::new();
        let items = drive(&mut acc, STREAM);
        assert_eq!(
            debug(&items),
            vec![
                format!("{:?}", StreamItem::TextDelta("Hi ".into())),
                format!("{:?}", StreamItem::TextDelta("there".into())),
                format!("{:?}", StreamItem::ToolCall {
                    id: "toolu_1".into(),
                    name: "get_weather".into(),
                    arguments: "{\"city\":\"Paris\"}".into(),
                }),
                format!("{:?}", StreamItem::Done),
            ]
        );
    }

    #[test]
    fn flushes_open_tool_call_when_stream_is_cut() {
        let mut acc = AnthropicAccumulator::new();
        let items = drive(
            &mut acc,
            b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"f\",\"input\":{}}}\n\n"
                .as_slice(),
        );
        assert!(items.is_empty());
        let mut tail = Vec::new();
        acc.on_finish(&mut tail);
        let items = tail.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(
            debug(&items),
            vec![
                format!("{:?}", StreamItem::ToolCall { id: "t1".into(), name: "f".into(), arguments: String::new() }),
                format!("{:?}", StreamItem::Done),
            ]
        );
    }

    #[test]
    fn error_events_fail_the_stream() {
        let mut acc = AnthropicAccumulator::new();
        let mut items = Vec::new();
        acc.on_data(
            "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"bad key\"}}",
            &mut items,
        );
        assert!(items[0].is_err());
    }

    #[test]
    fn request_body_matches_anthropic_wire() {
        let cfg = ProviderConfig::from_yaml_str(
            "model:\n  default: claude-fable-5\n  provider: anthropic\n  base_url: https://gw.example.com/anthropic\n  api_key: k\n",
        )
        .unwrap();
        let t = AnthropicTransport::new(cfg);
        assert_eq!(t.endpoint(), "https://gw.example.com/anthropic/v1/messages");

        let history = vec![
            Message::text(Role::System, "be brief"),
            Message::text(Role::User, "hi"),
            {
                let mut m = Message::text(Role::Assistant, "");
                m.content.push(Content::ToolUse {
                    call: ToolCall { id: "t1".into(), name: "read_file".into(), arguments: "{\"path\":\"a\"}".into() },
                });
                m
            },
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult { call_id: "t1".into(), output: "data".into() }],
            },
        ];
        let tools = vec![ToolSpec {
            name: "read_file".into(),
            description: "read a file".into(),
            schema: json!({"type": "object"}),
        }];
        let body = t.build_body(&history, &tools);
        assert_eq!(body["model"], "claude-fable-5");
        assert_eq!(body["max_tokens"], 16384);
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"], "be brief");
        // system not duplicated into messages; tool results ride user role
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "read_file", "input": {"path": "a"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "data"}]},
            ])
        );
        assert_eq!(
            body["tools"][0],
            json!({"name": "read_file", "description": "read a file", "input_schema": {"type": "object"}}),
        );
    }
}
