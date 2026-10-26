//! Telegram Bot API transport — port of the polling/send surface of
//! `plugins/platforms/telegram/adapter.py` (the python-telegram-bot wiring).

use crate::chunking::truncate_message;
use crate::contract::{
    BoxFut, GatewayTransport, HttpBackend, InboundMessage, Platform, SendOutcome,
};
use crate::telegram::format;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// Telegram's message cap counts UTF-16 code units (see `utf16_len`).
pub const MAX_MESSAGE_LENGTH: i64 = 4096;
/// Long-poll window passed to `getUpdates` (the Python adapter lets the poll
/// server-hold the connection; the HTTP read timeout rides above it).
pub const POLL_TIMEOUT_SECS: i64 = 50;

/// Poll/send client for one bot token.
pub struct TelegramTransport<B: HttpBackend> {
    http: B,
    token: String,
    /// `getUpdates` offset: highest delivered `update_id` + 1.
    offset: i64,
}

impl<B: HttpBackend> TelegramTransport<B> {
    pub fn new(http: B, token: &str) -> Self {
        TelegramTransport {
            http,
            token: token.to_string(),
            offset: 0,
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = self.http.post_json(self.api_url(method), params).await?;
        // Bot API envelope: {"ok": bool, "result" | "description", "error_code"}
        let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if !ok {
            let desc = body
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(anyhow!("{}: {}", method, desc));
        }
        Ok(body.get("result").cloned().unwrap_or(Value::Null))
    }

    /// One long-poll `getUpdates` round; advances the offset past every
    /// delivered update (the Python/PTB contract: never deliver twice).
    async fn poll_once(&mut self) -> Result<Vec<InboundMessage>> {
        let result = self
            .call(
                "getUpdates",
                json!({
                    "offset": self.offset,
                    "timeout": POLL_TIMEOUT_SECS,
                    "allowed_updates": [
                        "message", "edited_message", "channel_post",
                        "callback_query", "inline_query",
                    ],
                }),
            )
            .await?;
        let mut out = Vec::new();
        if let Some(updates) = result.as_array() {
            for u in updates {
                if let Some(msg) = normalize_update(u) {
                    out.push(msg);
                }
                // PTB advances the offset before dispatch: max(update_id) + 1.
                if let Some(uid) = u.get("update_id").and_then(Value::as_i64) {
                    if uid + 1 > self.offset {
                        self.offset = uid + 1;
                    }
                }
            }
        }
        Ok(out)
    }

    /// sendMessage with MarkdownV2, falling back to plain text when Telegram
    /// rejects the parse (the Python `_edit_markdown_or_plain` / send fallback).
    async fn send_one(&self, chat_id: &str, text: &str, thread_id: Option<&str>) -> Result<String> {
        let mut params = json!({ "chat_id": chat_id, "text": text, "parse_mode": "MarkdownV2" });
        if let Some(t) = thread_id {
            params["message_thread_id"] = json!(t);
        }
        let plain_params = || {
            let mut p = json!({ "chat_id": chat_id, "text": format::strip_mdv2(text) });
            if let Some(t) = thread_id {
                p["message_thread_id"] = json!(t);
            }
            p
        };
        match self.call("sendMessage", params).await {
            Ok(r) => Ok(message_id_of(&r).unwrap_or_default()),
            Err(_) => {
                // MarkdownV2 rejected → plain-text fallback (the Python
                // markdown-or-plain send path).
                let r = self.call("sendMessage", plain_params()).await?;
                Ok(message_id_of(&r).unwrap_or_default())
            }
        }
    }

    /// editMessageText for in-place streaming updates: mid-stream edits go out
    /// without a parse mode; the final edit is MarkdownV2 with a plain fallback.
    /// "message is not modified" is a success no-op.
    pub async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        content: &str,
        finalize: bool,
    ) -> Result<()> {
        let build = |parse_mode: Option<&str>| -> Value {
            let mut p = json!({ "chat_id": chat_id, "message_id": message_id, "text": content });
            if let Some(m) = parse_mode {
                p["parse_mode"] = json!(m);
            }
            p
        };
        let (formatted, parse_mode) = if finalize {
            (format::format_message(content), Some("MarkdownV2"))
        } else {
            (content.to_string(), None)
        };
        let attempt = |pm: Option<&str>| build(pm);
        let mut params = attempt(parse_mode);
        params["text"] = json!(formatted);
        match self.call("editMessageText", params).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let err = e.to_string().to_lowercase();
                if err.contains("not modified") {
                    return Ok(()); // no-op success, caller may skip further work
                }
                if err.contains("too long") || err.contains("message_too_long") {
                    return Err(e); // caller splits (finalize) or truncates (streaming)
                }
                if finalize {
                    // MarkdownV2 edit failed → plain-text fallback edit.
                    let mut params = build(None);
                    params["text"] = json!(format::strip_mdv2(content));
                    return self.call("editMessageText", params).await.map(|_| ());
                }
                Err(e)
            }
        }
    }
}

impl<B: HttpBackend> GatewayTransport for TelegramTransport<B> {
    fn poll(&mut self) -> BoxFut<'_, Result<Vec<InboundMessage>>> {
        Box::pin(async move { self.poll_once().await })
    }

    fn send_reply<'a>(
        &'a self,
        chat_id: &'a str,
        text: &'a str,
        thread_id: Option<&'a str>,
    ) -> BoxFut<'a, Result<SendOutcome>> {
        Box::pin(async move {
            // Chunk the RAW text at the UTF-16 cap, then format each chunk —
            // MarkdownV2 escapes can inflate the payload past the limit, so the
            // raw pre-format text is what gets measured (Python formats first,
            // then reactively splits on message_too_long; measuring raw + the
            // 10-char indicator reserve covers the inflation for typical text).
            let chunks = truncate_message(text, MAX_MESSAGE_LENGTH, crate::chunking::utf16_len);
            let mut message_ids = Vec::new();
            for chunk in chunks.iter() {
                let formatted = format::format_message(chunk);
                let id = match self.send_one(chat_id, &formatted, thread_id).await {
                    Ok(id) => id,
                    Err(e) => {
                        // Reactive split: MarkdownV2 escapes can inflate the payload
                        // past the limit even when raw text fit → retry the chunk as
                        // plain text (Python's markdown-or-plain fallback).
                        let stripped = format::strip_mdv2(chunk);
                        let _ = e;
                        let id = self
                            .call(
                                "sendMessage",
                                json!({ "chat_id": chat_id, "text": stripped }),
                            )
                            .await?;
                        message_id_of(&id).unwrap_or_default()
                    }
                };
                message_ids.push(id);
            }
            Ok(SendOutcome {
                message_ids,
                chunks: chunks.len(),
            })
        })
    }
}

fn message_id_of(result: &Value) -> Option<String> {
    result
        .get("message_id")
        .and_then(Value::as_i64)
        .map(|id| id.to_string())
}

/// Normalize a Bot API update into the shared inbound contract (text/caption
/// messages only — the ported surface).
fn normalize_update(update: &Value) -> Option<InboundMessage> {
    let msg = update
        .get("message")
        .or_else(|| update.get("edited_message"))
        .or_else(|| update.get("channel_post"))?;
    let chat = msg.get("chat")?;
    let chat_id = chat
        .get("id")
        .and_then(Value::as_i64)
        .map(|id| id.to_string())
        .or_else(|| chat.get("id").and_then(Value::as_str).map(String::from))?;
    let text = msg
        .get("text")
        .or_else(|| msg.get("caption"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if text.is_empty() {
        // Non-text updates (photos without caption, stickers, …) are dropped —
        // only a person speaking wakes the agent.
        return None;
    }
    let from = msg.get("from");
    let user_id = from
        .and_then(|f| f.get("id").and_then(Value::as_i64))
        .map(|id| id.to_string())
        .unwrap_or_default();
    let username = from
        .and_then(|f| f.get("username"))
        .and_then(Value::as_str)
        .map(String::from);
    let message_id = msg
        .get("message_id")
        .and_then(Value::as_i64)
        .map(|id| id.to_string());
    let thread_id = msg
        .get("message_thread_id")
        .and_then(Value::as_i64)
        .map(|id| id.to_string());
    Some(InboundMessage {
        platform: Platform::Telegram,
        chat_id,
        user_id,
        username,
        text,
        message_id: message_id.unwrap_or_default(),
        thread_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// Stub HTTP layer: canned JSON responses per (method, url), recording calls.
    /// A `StubHandle` shares the recorded calls with the test. Responses are a
    /// consuming queue: each call pops the first entry whose needle matches.
    struct StubApi {
        responses: std::sync::Arc<Mutex<Vec<(String, Value)>>>, // (url-needle, body)
        calls: std::sync::Arc<Mutex<Vec<(String, Value)>>>,     // (url, payload)
    }

    #[derive(Clone)]
    struct StubHandle {
        calls: std::sync::Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl StubHandle {
        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().unwrap().drain(..).collect()
        }
    }

    impl StubApi {
        fn new(responses: Vec<(String, Value)>) -> (Self, StubHandle) {
            let responses = std::sync::Arc::new(Mutex::new(responses));
            let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
            let handle = StubHandle {
                calls: calls.clone(),
            };
            (StubApi { responses, calls }, handle)
        }
    }

    impl HttpBackend for StubApi {
        fn post_json<'a>(&'a self, url: String, payload: Value) -> BoxFut<'a, Result<Value>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push((url.clone(), payload));
                let mut queue = self.responses.lock().unwrap();
                for (idx, (needle, resp)) in queue.iter().enumerate() {
                    if url.contains(needle.as_str()) {
                        let resp = resp.clone();
                        queue.remove(idx);
                        return Ok(resp);
                    }
                }
                Err(anyhow!("stub: no canned response for {}", url))
            })
        }
    }

    #[tokio::test]
    async fn poll_loop_tracks_offset_and_normalizes_updates() {
        let updates = json!({
            "ok": true,
            "result": [
                { "update_id": 41, "message": {
                    "message_id": 7, "from": { "id": 900, "username": "ada" },
                    "chat": { "id": 55, "type": "private" }, "text": "hello" } },
                { "update_id": 42, "message": {
                    "message_id": 8, "from": { "id": 901 },
                    "chat": { "id": 55 }, "text": "again" } },
                { "update_id": 43, "message": {
                    "message_id": 9, "chat": { "id": 55 }, "photo": [] } }, // non-text: dropped
            ]
        });
        let empty = json!({ "ok": true, "result": [] });
        let (stub, handle) = StubApi::new(vec![
            ("getUpdates".into(), updates),
            ("getUpdates".into(), empty),
        ]);
        let mut t = TelegramTransport::new(stub, "TOKEN");
        let msgs = GatewayTransport::poll(&mut t).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].chat_id, "55");
        assert_eq!(msgs[0].user_id, "900");
        assert_eq!(msgs[0].text, "hello");
        assert_eq!(msgs[0].message_id, "7");
        assert_eq!(msgs[1].text, "again");
        // Second poll must use offset = max(update_id)+1 = 44 and deliver nothing.
        let msgs2 = GatewayTransport::poll(&mut t).await.unwrap();
        assert!(msgs2.is_empty());
        let calls = handle.calls();
        // The LAST getUpdates call carries the advanced offset (the first poll
        // legitimately starts at 0).
        let second = calls
            .iter()
            .rev()
            .find(|(u, _)| u.ends_with("/getUpdates"))
            .unwrap();
        assert_eq!(second.1["offset"], 44);
        assert_eq!(second.1["timeout"], POLL_TIMEOUT_SECS);
        // Long-poll state machine: no re-delivery of update 41 (offset monotonic).
        assert_eq!(second.1["offset"], 44);
    }

    #[tokio::test]
    async fn send_reply_chunks_at_utf16_cap_and_uses_markdownv2() {
        let long = "\u{1F600}".repeat(3000); // 6000 UTF-16 units > 4096
        let sent = json!({ "ok": true, "result": { "message_id": 99 } });
        // Emojis carry no markdown, so the formatted chunks are the raw chunks:
        // one canned OK per chunk (generous margin).
        let (stub, handle) = StubApi::new(vec![
            ("sendMessage".into(), sent.clone()),
            ("sendMessage".into(), sent.clone()),
            ("sendMessage".into(), sent.clone()),
            ("sendMessage".into(), sent.clone()),
            ("sendMessage".into(), sent.clone()),
        ]);
        let t = TelegramTransport::new(stub, "TOKEN");
        let out = GatewayTransport::send_reply(&t, "55", &long, None)
            .await
            .unwrap();
        assert!(out.chunks >= 2);
        let calls = handle.calls();
        assert_eq!(calls.len(), out.chunks);
        for (url, payload) in &calls {
            assert!(url.ends_with("/sendMessage"));
            assert_eq!(payload["parse_mode"], "MarkdownV2");
            // Each chunk fits the UTF-16 cap (chunking measures raw text).
            let text = payload["text"].as_str().unwrap();
            assert!(
                crate::chunking::utf16_len(text) <= MAX_MESSAGE_LENGTH,
                "chunk over cap: {}",
                crate::chunking::utf16_len(text)
            );
        }
        // The final chunk's id is the edit target (last delivered id).
        assert_eq!(out.message_ids.last().unwrap(), "99");
    }

    #[tokio::test]
    async fn send_reply_falls_back_to_plain_text_on_parse_error() {
        let sent = json!({ "ok": true, "result": { "message_id": 5 } });
        // First sendMessage answers a parse failure, then any later one succeeds.
        let (stub, handle) = StubApi::new(vec![
            (
                "sendMessage".into(),
                json!({
                    "ok": false, "error_code": 400,
                    "description": "Bad Request: can't parse entities"
                }),
            ),
            ("sendMessage".into(), sent),
        ]);
        let t = TelegramTransport::new(stub, "TOKEN");
        let out = GatewayTransport::send_reply(&t, "55", "plain **text**", None)
            .await
            .unwrap();
        assert_eq!(out.chunks, 1);
        let calls = handle.calls();
        assert_eq!(calls.len(), 2); // MarkdownV2 attempt + plain retry
        assert_eq!(calls[0].1["parse_mode"], "MarkdownV2");
        assert!(calls[1].1.get("parse_mode").is_none());
        assert_eq!(calls[1].1["text"], "plain text");
    }

    #[tokio::test]
    async fn edit_message_treats_not_modified_as_success() {
        let (stub, _handle) = StubApi::new(vec![(
            "editMessageText".into(),
            json!({
                "ok": false, "error_code": 400,
                "description": "Bad Request: message is not modified"
            }),
        )]);
        let t = TelegramTransport::new(stub, "TOKEN");
        // Finalize edit: not-modified is a success no-op.
        let r = t.edit_message("55", "7", "same", true).await;
        assert!(r.is_ok());
    }
}
