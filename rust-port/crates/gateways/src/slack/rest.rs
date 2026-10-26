//! Slack Web API client — port of the send path of
//! `plugins/platforms/slack/adapter.py` (`send` → `_post_chunks` →
//! `chat.postMessage`).
//!
//! Delivery contract (mirrored from the Python):
//! - `format_message` (markdown → mrkdwn) is applied FIRST, blank results
//!   short-circuit (Slack returns `no_text` for blank posts),
//! - split at `MAX_MESSAGE_LENGTH` (39000 — the API allows 40000, margin kept)
//!   with `truncate_message` (fence-preserving, `(1/N)` indicators),
//! - Block Kit blocks ONLY for single-chunk messages (a >39k response is
//!   pathological for the 50-block / 3000-char limits) and blocks never go
//!   without the mrkdwn `text` fallback,
//! - `thread_ts` threads the reply; `reply_broadcast` broadcasts only the first
//!   chunk to the main channel,
//! - a `bad_blocks`-shaped failure (`no_text`/`invalid_blocks`-class errors)
//!   retries the chunk as plain text without blocks
//!   (`_call_with_block_fallback`).

use crate::chunking::truncate_message;
use crate::contract::{HttpBackend, SendOutcome};
use crate::slack::block_kit::render_blocks;
use crate::slack::format::format_message;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// Slack API allows 40,000 chars; leave margin (`SlackAdapter.MAX_MESSAGE_LENGTH`).
pub const MAX_MESSAGE_LENGTH: i64 = 39000;

/// `chat.postMessage` endpoint.
pub const API_BASE: &str = "https://slack.com/api";

/// Error strings that mean "drop the blocks and retry as plain text" (the
/// Python `_call_with_block_fallback` inspects `error`/`errors`).
const BLOCK_FALLBACK_ERRORS: &[&str] = &[
    "invalid_blocks",
    "invalid_blocks_format",
    "block_not_found",
    "too_many_blocks",
    "message_block_limit_exceeded",
    "too_many_attachments",
    "invalid_metadata",
    "no_text",
];

/// chat.postMessage client for one bot token.
pub struct SlackRest<B: HttpBackend> {
    http: B,
    token: String,
    /// Overridable for tests; production uses `API_BASE`.
    base: String,
    /// Whether Block Kit rendering is enabled (`slack.extra.rich_blocks`).
    rich_blocks: bool,
    /// Broadcast threaded replies to the channel too (`reply_broadcast`).
    reply_broadcast: bool,
}

impl<B: HttpBackend> SlackRest<B> {
    pub fn new(http: B, token: &str) -> Self {
        SlackRest {
            http,
            token: token.to_string(),
            base: API_BASE.to_string(),
            rich_blocks: false,
            reply_broadcast: false,
        }
    }

    pub fn with_options(http: B, token: &str, rich_blocks: bool, reply_broadcast: bool) -> Self {
        SlackRest {
            http,
            token: token.to_string(),
            base: API_BASE.to_string(),
            rich_blocks,
            reply_broadcast,
        }
    }

    /// One `chat.postMessage` call; errors carry the Slack `error` string.
    async fn post_message(&self, payload: &Value) -> Result<Value> {
        let mut payload = payload.clone();
        payload["token"] = json!(self.token);
        let resp = self.http.post_json(format!("{}/chat.postMessage", self.base), payload).await?;
        if resp.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = resp.get("error").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(anyhow!("chat.postMessage failed: {}", err));
        }
        Ok(resp)
    }

    /// mrkdwn-format `content` and split to `MAX_MESSAGE_LENGTH` (never empty)
    /// — the Python `_format_chunks`.
    fn format_chunks(&self, content: &str) -> Vec<String> {
        let formatted = format_message(content);
        if formatted.is_empty() || formatted.trim().is_empty() {
            return Vec::new();
        }
        let char_len = |s: &str| s.chars().count() as i64;
        let chunks = truncate_message(&formatted, MAX_MESSAGE_LENGTH, char_len);
        if chunks.is_empty() { vec![formatted] } else { chunks }
    }

    /// One logical send: format → chunk → `chat.postMessage` each chunk.
    /// `thread_ts` (the Python `_resolve_thread_ts(reply_to, metadata)`) threads
    /// the reply under a parent message. Returns the delivered message ids
    /// (the Slack `ts` values) in order.
    pub async fn send(
        &self,
        chat_id: &str,
        raw_markdown: &str,
        thread_ts: Option<&str>,
    ) -> Result<SendOutcome> {
        let chunks = self.format_chunks(raw_markdown);
        if chunks.is_empty() {
            // Slack returns `no_text` for blank posts; still the end of a
            // delivery attempt (the "is thinking…" status must clear).
            return Ok(SendOutcome { chunks: 0, message_ids: Vec::new() });
        }
        // Block Kit only for single-chunk messages; blocks never without text.
        let blocks = if self.rich_blocks && chunks.len() == 1 {
            render_blocks(raw_markdown)
        } else {
            None
        };
        let mut message_ids = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.iter().enumerate() {
            let mut payload = json!({
                "channel": chat_id,
                "text": chunk,
                "mrkdwn": true, // Slack's mrkdwn autolink/unfurl kwargs
            });
            if i == 0 && self.reply_broadcast && thread_ts.is_some() {
                payload["reply_broadcast"] = json!(true);
            }
            if let Some(ts) = thread_ts {
                payload["thread_ts"] = json!(ts);
            }
            if i == 0 {
                if let Some(blocks) = &blocks {
                    payload["blocks"] = json!(blocks);
                }
            }
            match self.post_message(&payload).await {
                Ok(resp) => {
                    let ts = resp
                        .pointer("/message/ts")
                        .or_else(|| resp.get("ts"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    message_ids.push(ts);
                }
                Err(e) => {
                    // A blocks-shaped failure retries the chunk as plain text
                    // (`_call_with_block_fallback`); anything else propagates.
                    let msg = e.to_string();
                    let blocks_error =
                        BLOCK_FALLBACK_ERRORS.iter().any(|needle| msg.contains(needle));
                    if blocks_error && payload.get("blocks").is_some() {
                        payload.as_object_mut().unwrap().remove("blocks");
                        let resp = self.post_message(&payload).await?;
                        let ts = resp
                            .pointer("/message/ts")
                            .or_else(|| resp.get("ts"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        message_ids.push(ts);
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        Ok(SendOutcome { chunks: chunks.len(), message_ids })
    }

    /// `chat.postEphemeral` (`send_private_notice`): visible only to one user.
    pub async fn send_ephemeral(
        &self,
        chat_id: &str,
        user_id: &str,
        content: &str,
        thread_ts: Option<&str>,
    ) -> Result<String> {
        let formatted = format_message(content);
        let mut payload = json!({
            "channel": chat_id, "user": user_id, "text": formatted, "mrkdwn": true,
        });
        if let Some(ts) = thread_ts {
            payload["thread_ts"] = json!(ts);
        }
        let resp = self.http.post_json(format!("{}/chat.postEphemeral", self.base), payload).await?;
        if resp.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = resp.get("error").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(anyhow!("chat.postEphemeral failed: {}", err));
        }
        Ok(resp
            .get("message_ts")
            .or_else(|| resp.get("ts"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::BoxFut;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// Stub HTTP layer recording calls and answering from a canned queue
    /// (matched by URL needle) — no network.
    struct StubApi {
        responses: Mutex<Vec<(String, Value)>>,
        calls: Arc<Mutex<Vec<(String, Value)>>>,
    }

    #[derive(Clone)]
    struct StubHandle {
        calls: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl StubHandle {
        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().unwrap().drain(..).collect()
        }
    }

    impl StubApi {
        fn new(responses: Vec<(String, Value)>) -> (Self, StubHandle) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (
                StubApi { responses: Mutex::new(responses), calls: calls.clone() },
                StubHandle { calls },
            )
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

    fn ok_ts(ts: &str) -> Value {
        json!({ "ok": true, "ts": ts, "message": { "ts": ts } })
    }

    #[tokio::test]
    async fn single_message_posts_mrkdwn_with_text_fallback() {
        let (stub, handle) = StubApi::new(vec![("chat.postMessage".into(), ok_ts("1700.1"))]);
        let rest = SlackRest::new(stub, "xoxb-TOK");
        let out = rest.send("C1", "**bold** reply", None).await.unwrap();
        assert_eq!(out.chunks, 1);
        assert_eq!(out.message_ids, vec!["1700.1"]);
        let calls = handle.calls();
        assert_eq!(calls.len(), 1);
        let (url, payload) = &calls[0];
        assert!(url.ends_with("/chat.postMessage"));
        // mrkdwn conversion applied.
        assert_eq!(payload["text"], "*bold* reply");
        assert_eq!(payload["channel"], "C1");
        assert_eq!(payload["mrkdwn"], true);
        // Bearer token rides in the payload (Slack Web API token param).
        assert_eq!(payload["token"], "xoxb-TOK");
        // No blocks without rich_blocks opt-in.
        assert!(payload.get("blocks").is_none());
        // No thread_ts in main chat.
        assert!(payload.get("thread_ts").is_none());
    }

    #[tokio::test]
    async fn blocks_only_for_single_chunk_and_only_with_rich_blocks() {
        // rich_blocks + single chunk → blocks paired with the text fallback.
        let (stub, handle) = StubApi::new(vec![("chat.postMessage".into(), ok_ts("1"))]);
        let rest = SlackRest::with_options(stub, "t", true, false);
        rest.send("C1", "# Title\n\nbody", None).await.unwrap();
        let calls = handle.calls();
        assert!(calls[0].1.get("blocks").is_some());
        assert!(calls[0].1["text"].is_string(), "blocks must pair with a text fallback");

        // rich_blocks + multi-chunk → no blocks (pathological >39k for Block Kit).
        let (stub, handle) = StubApi::new(vec![
            ("chat.postMessage".into(), ok_ts("1")),
            ("chat.postMessage".into(), ok_ts("2")),
        ]);
        let rest = SlackRest::with_options(stub, "t", true, false);
        let long = "y".repeat(45_000);
        rest.send("C1", &long, None).await.unwrap();
        let calls = handle.calls();
        for (_, payload) in &calls {
            assert!(payload.get("blocks").is_none());
        }
    }

    #[tokio::test]
    async fn chunks_at_39000_cap_with_indicators() {
        let long = "x".repeat(85_000); // → 3 chunks at 39000
        let (stub, handle) = StubApi::new(vec![
            ("chat.postMessage".into(), ok_ts("1")),
            ("chat.postMessage".into(), ok_ts("2")),
            ("chat.postMessage".into(), ok_ts("3")),
        ]);
        let rest = SlackRest::new(stub, "t");
        let out = rest.send("C1", &long, None).await.unwrap();
        assert_eq!(out.chunks, 3);
        let calls = handle.calls();
        assert_eq!(calls.len(), 3);
        for (_, payload) in &calls {
            let text = payload["text"].as_str().unwrap();
            assert!(
                text.chars().count() as i64 <= MAX_MESSAGE_LENGTH,
                "chunk over the 39000 cap: {}",
                text.chars().count()
            );
        }
        assert!(calls[0].1["text"].as_str().unwrap().ends_with(" (1/3)"));
        assert!(calls[2].1["text"].as_str().unwrap().ends_with(" (3/3)"));
        assert_eq!(out.message_ids, vec!["1", "2", "3"]);
    }

    #[tokio::test]
    async fn thread_ts_every_chunk_broadcast_first_only() {
        let (stub, handle) = StubApi::new(vec![
            ("chat.postMessage".into(), ok_ts("1")),
            ("chat.postMessage".into(), ok_ts("2")),
        ]);
        let rest = SlackRest::with_options(stub, "t", false, true);
        let long = "y".repeat(45_000);
        rest.send("C1", &long, Some("1700.5")).await.unwrap();
        let calls = handle.calls();
        // thread_ts rides on every chunk; reply_broadcast only the first.
        assert_eq!(calls[0].1["thread_ts"], "1700.5");
        assert_eq!(calls[1].1["thread_ts"], "1700.5");
        assert_eq!(calls[0].1["reply_broadcast"], true);
        assert!(calls[1].1.get("reply_broadcast").is_none());
    }

    #[tokio::test]
    async fn blocks_failure_retries_as_plain_text() {
        let (stub, handle) = StubApi::new(vec![
            (
                "chat.postMessage".into(),
                json!({ "ok": false, "error": "invalid_blocks" }),
            ),
            ("chat.postMessage".into(), ok_ts("42")),
        ]);
        let rest = SlackRest::with_options(stub, "t", true, false);
        let out = rest.send("C1", "# Title", None).await.unwrap();
        assert_eq!(out.message_ids, vec!["42"]);
        let calls = handle.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].1.get("blocks").is_some());
        assert!(calls[1].1.get("blocks").is_none(), "retry must drop the blocks");
        assert_eq!(calls[1].1["text"], "*Title*");
    }

    #[tokio::test]
    async fn blank_post_short_circuits() {
        let (stub, handle) = StubApi::new(vec![]);
        let rest = SlackRest::new(stub, "t");
        let out = rest.send("C1", "   ", None).await.unwrap();
        assert_eq!(out.chunks, 0);
        assert!(handle.calls().is_empty());
    }

    #[tokio::test]
    async fn api_error_carries_slack_error_text() {
        let (stub, _handle) = StubApi::new(vec![(
            "chat.postMessage".into(),
            json!({ "ok": false, "error": "channel_not_found" }),
        )]);
        let rest = SlackRest::new(stub, "t");
        let err = rest.send("C1", "hi", None).await.unwrap_err();
        assert!(err.to_string().contains("channel_not_found"));
    }

    #[tokio::test]
    async fn ephemeral_posts_to_user() {
        let (stub, handle) = StubApi::new(vec![(
            "chat.postEphemeral".into(),
            json!({ "ok": true, "message_ts": "9.9" }),
        )]);
        let rest = SlackRest::new(stub, "t");
        let ts = rest.send_ephemeral("C1", "U7", "**hi**", None).await.unwrap();
        assert_eq!(ts, "9.9");
        let calls = handle.calls();
        assert!(calls[0].0.ends_with("/chat.postEphemeral"));
        assert_eq!(calls[0].1["user"], "U7");
        assert_eq!(calls[0].1["text"], "*hi*");
    }
}
