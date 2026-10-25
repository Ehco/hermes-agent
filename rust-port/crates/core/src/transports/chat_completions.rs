//! OpenAI-compatible Chat Completions transport.
//!
//! Port of the Python `chat_completions.py` wire contract: POST
//! `{base_url}/chat/completions` with `stream: true`, `Authorization: Bearer`
//! auth, SSE `data:` frames terminated by `data: [DONE]`, and streamed tool
//! calls accumulated from `delta.tool_calls` fragments keyed by `index`
//! (id/name arrive on the first fragment, argument JSON in fragments after).

use crate::config::ProviderConfig;
use crate::model::{Content, Message, Role, ToolSpec};
use crate::transport::{StreamItem, Transport};
use crate::transports::{truncate_body, ConnectThen, EventSink, SseStream};
use anyhow::{anyhow, Result};
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::pin::Pin;

pub struct ChatCompletionsTransport {
    cfg: ProviderConfig,
    http: reqwest::Client,
}

impl ChatCompletionsTransport {
    pub fn new(cfg: ProviderConfig) -> Self {
        Self { cfg, http: reqwest::Client::new() }
    }

    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'))
    }

    /// `{model, messages, stream: true[, tools]}` request body.
    pub fn build_body(&self, messages: &[Message], tools: &[ToolSpec]) -> Value {
        let mut body = json!({
            "model": self.cfg.model,
            "messages": wire_messages(messages),
            "stream": true,
        });
        if !tools.is_empty() {
            body["tools"] = wire_tools(tools);
        }
        body
    }
}

/// Tools are already OpenAI-shaped: `{type: function, function: {name, description, parameters}}`.
pub fn wire_tools(tools: &[ToolSpec]) -> Value {
    json!(tools
        .iter()
        .map(|t| json!({
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.schema,
            },
        }))
        .collect::<Vec<_>>())
}

/// Messages: near-identity for OpenAI-shaped history.
/// - system/user: `{role, content}` (text contents joined);
/// - assistant: text becomes `content`, ToolUse blocks become `tool_calls`
///   (`{id, type: "function", function: {name, arguments}}`);
/// - tool results: `{role: "tool", tool_call_id, content}`.
pub fn wire_messages(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| match m.role {
            Role::Assistant => {
                let mut tool_calls = Vec::new();
                let mut text = String::new();
                for c in &m.content {
                    match c {
                        Content::Text { text: t } => text.push_str(t),
                        Content::ToolUse { call } => tool_calls.push(json!({
                            "id": call.id,
                            "type": "function",
                            "function": { "name": call.name, "arguments": call.arguments },
                        })),
                        Content::ToolResult { .. } => {}
                    }
                }
                let mut out = json!({ "role": "assistant" });
                if tool_calls.is_empty() {
                    out["content"] = if text.is_empty() { Value::Null } else { json!(text) };
                } else {
                    out["content"] = if text.is_empty() { Value::Null } else { json!(text) };
                    out["tool_calls"] = json!(tool_calls);
                }
                out
            }
            Role::Tool => {
                let call_id = m
                    .content
                    .iter()
                    .find_map(|c| match c {
                        Content::ToolResult { call_id, .. } => Some(call_id.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                let output = m
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::ToolResult { output, .. } => Some(output.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                json!({ "role": "tool", "tool_call_id": call_id, "content": output })
            }
            _ => json!({ "role": m.role, "content": crate::transports::text_of(&m.content) }),
        })
        .collect()
}

/// Streamed tool call accumulating across delta fragments (keyed by `index`).
#[derive(Debug, Default)]
pub struct ChatAccumulator {
    pending: BTreeMap<u64, PendingCall>,
    finished: bool,
}

#[derive(Debug, Clone)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

impl ChatAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle one `data:` payload (the `[DONE]` sentinel included).
    pub fn on_data(&mut self, data: &str, out: &mut Vec<Result<StreamItem>>) {
        if data.trim() == "[DONE]" {
            self.flush(out);
            if !self.finished {
                self.finished = true;
                out.push(Ok(StreamItem::Done));
            }
            return;
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                out.push(Err(anyhow!("chat completions: bad SSE data ({e}): {data}")));
                return;
            }
        };
        if let Some(err) = v.get("error") {
            out.push(Err(anyhow!(
                "chat completions error: {}",
                err.as_str().unwrap_or(&err.to_string())
            )));
            return;
        }
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else { return };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                out.push(Ok(StreamItem::TextDelta(text.to_string())));
            }
            if let Some(fragments) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in fragments {
                    let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let entry = self.pending.entry(index).or_insert_with(|| PendingCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                    if let Some(id) = tc.get("id").and_then(Value::as_str) {
                        entry.id = id.to_string();
                    }
                    if let Some(name) = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                    {
                        entry.name = name.to_string();
                    }
                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                    {
                        entry.arguments.push_str(args);
                    }
                }
            }
        }
        // finish_reason closes the tool-call batch (flush emits full calls).
        if let Some(fr) = choice.get("finish_reason") {
            if fr.is_string() || fr.as_bool() == Some(false) {
                self.flush(out);
            }
        }
    }

    /// Emit completed tool calls in index order.
    fn flush(&mut self, out: &mut Vec<Result<StreamItem>>) {
        for (_, call) in std::mem::take(&mut self.pending) {
            out.push(Ok(StreamItem::ToolCall {
                id: call.id,
                name: call.name,
                arguments: call.arguments,
            }));
        }
    }
}

impl EventSink for ChatAccumulator {
    fn on_event(&mut self, event: &crate::transports::sse::SseEvent, out: &mut Vec<Result<StreamItem>>) {
        self.on_data(&event.data, out);
    }

    fn on_finish(&mut self, out: &mut Vec<Result<StreamItem>>) {
        self.flush(out);
        if !self.finished {
            self.finished = true;
            out.push(Ok(StreamItem::Done));
        }
    }
}

impl Transport for ChatCompletionsTransport {
    fn stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamItem>> + Send>>> {
        let body = self.build_body(messages, tools);
        let url = self.endpoint();
        let key = self.cfg.resolve_api_key()?;
        let http = self.http.clone();

        let connect: Pin<Box<dyn Future<Output = Result<SseStream<ChatAccumulator>>> + Send>> =
            Box::pin(async move {
                let resp = http
                    .post(url)
                    .bearer_auth(key)
                    .json(&body)
                    .send()
                    .await?;
                let status = resp.status();
                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!(
                        "chat completions HTTP {}: {}",
                        status,
                        truncate_body(&text)
                    ));
                }
                // `Bytes` chunks -> `Vec<u8>` so the pump never names the bytes crate.
                let raw = resp.bytes_stream().map(|r| r.map(|b| b.to_vec()));
                Ok(SseStream::new(Box::pin(raw), ChatAccumulator::new()))
            });
        Ok(Box::pin(ConnectThen::new(connect)))
    }
}

use std::future::Future;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, ToolCall};
    use crate::transports::sse::SseParser;

    fn drive(acc: &mut ChatAccumulator, bytes: &[u8]) -> Vec<StreamItem> {
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

    const STREAM: &[u8] = br#"data: {"choices":[{"delta":{"role":"assistant","content":"Hel"}}]}

data: {"choices":[{"delta":{"content":"lo"}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":"{\"city\":"}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}

data: [DONE]

"#;

    #[test]
    fn streams_text_and_accumulates_tool_call_fragments() {
        let mut acc = ChatAccumulator::new();
        let items = drive(&mut acc, STREAM);
        assert_eq!(
            debug(&items),
            vec![
                format!("{:?}", StreamItem::TextDelta("Hel".into())),
                format!("{:?}", StreamItem::TextDelta("lo".into())),
                format!("{:?}", StreamItem::ToolCall {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    arguments: "{\"city\":\"Paris\"}".into(),
                }),
                format!("{:?}", StreamItem::Done),
            ]
        );
    }

    #[test]
    fn handles_tool_calls_split_across_chunks_and_multiple_indexes() {
        let mut acc = ChatAccumulator::new();
        let mut parser = SseParser::new();
        // Feed everything in small slices through ONE parser: the fixture
        // exercises fragment accumulation (index 0 split across chunks, a
        // second call on index 1) plus the incremental SSE parser.
        let lines = [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"f","arguments":"{\"x\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"g"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#,
            "data: [DONE]",
        ];
        let bytes = lines.join("\n\n");
        let bytes = format!("{bytes}\n\n");
        let mut items = Vec::new();
        for chunk in bytes.as_bytes().chunks(9) {
            for ev in parser.feed(chunk) {
                acc.on_event(&ev, &mut items);
            }
        }
        for ev in parser.finish() {
            acc.on_event(&ev, &mut items);
        }
        let items = items.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(
            debug(&items),
            vec![
                format!("{:?}", StreamItem::ToolCall { id: "a".into(), name: "f".into(), arguments: "{\"x\":1}".into() }),
                format!("{:?}", StreamItem::ToolCall { id: "b".into(), name: "g".into(), arguments: String::new() }),
                format!("{:?}", StreamItem::Done),
            ]
        );
    }

    #[test]
    fn error_payloads_fail_the_stream() {
        let mut acc = ChatAccumulator::new();
        let mut items = Vec::new();
        acc.on_data("{\"error\":{\"message\":\"bad key\"}}", &mut items);
        assert!(items[0].is_err());
    }

    #[test]
    fn request_body_matches_openai_wire() {
        let cfg = ProviderConfig::from_yaml_str(
            "model:\n  default: glm-5.3-flash\n  base_url: http://host:8080/v1\n  api_key: k\n",
        )
        .unwrap();
        let t = ChatCompletionsTransport::new(cfg);
        assert_eq!(t.endpoint(), "http://host:8080/v1/chat/completions");

        let history = vec![
            Message::text(Role::System, "sys"),
            Message::text(Role::User, "hi"),
            {
                let mut m = Message::text(Role::Assistant, "");
                m.content.push(Content::ToolUse {
                    call: ToolCall { id: "c1".into(), name: "terminal".into(), arguments: "{}".into() },
                });
                m
            },
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult { call_id: "c1".into(), output: "done" .into() }],
            },
        ];
        let tools = vec![ToolSpec {
            name: "terminal".into(),
            description: "run a command".into(),
            schema: json!({"type": "object"}),
        }];
        let body = t.build_body(&history, &tools);
        assert_eq!(body["model"], "glm-5.3-flash");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0], json!({"role": "system", "content": "sys"}));
        assert_eq!(body["messages"][2]["tool_calls"][0], json!({
            "id": "c1", "type": "function",
            "function": {"name": "terminal", "arguments": "{}"},
        }));
        assert_eq!(
            body["messages"][3],
            json!({"role": "tool", "tool_call_id": "c1", "content": "done"}),
        );
        assert_eq!(
            body["tools"][0],
            json!({"type": "function", "function": {"name": "terminal", "description": "run a command", "parameters": {"type": "object"}}}),
        );
    }
}
