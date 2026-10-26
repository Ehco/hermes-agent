//! Discord bot-token REST client — port of the `channel.send` surface of
//! `plugins/platforms/discord/adapter.py` (`DiscordAdapter::send` → discord.py
//! `Channel.send` → `POST /channels/{id}/messages`).
//!
//! Delivery contract (mirrored from the Python):
//! - format (identity: Discord renders standard markdown natively),
//! - split at the 2000-char cap with `truncate_message` (fence-preserving,
//!   `(1/N)` indicators — `crate::chunking`),
//! - cap split deliveries at 8 chunks with a truncation notice,
//! - POST each chunk to `/channels/{id}/messages` with `allowed_mentions`
//!   denying @everyone/@here/role pings,
//! - reply-by-id posts a `message_reference` on the first chunk only.

use crate::chunking::truncate_message;
use crate::contract::{HttpBackend, SendOutcome};
use crate::discord::format::{
    cap_split_chunks, default_allowed_mentions, format_message, MAX_MESSAGE_LENGTH,
};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// REST base URL for the bot-token API.
pub const API_BASE: &str = "https://discord.com/api/v10";

/// Message-send client for one bot token.
pub struct DiscordRest<B: HttpBackend> {
    http: B,
    token: String,
    /// Overridable for tests; production uses `API_BASE`.
    base: String,
}

impl<B: HttpBackend> DiscordRest<B> {
    pub fn new(http: B, token: &str) -> Self {
        DiscordRest { http, token: token.to_string(), base: API_BASE.to_string() }
    }

    /// Bearer-auth JSON POST helper (the Python client sends
    /// `Authorization: Bot <token>`; reqwest's `bearer_auth` is equivalent).
    #[allow(dead_code)]
    async fn post(&self, path: &str, payload: Value) -> Result<Value> {
        self.http.post_json(format!("{}{}", self.base, path), payload).await
    }

    /// `POST /channels/{id}/messages` with one already-chunked content string.
    /// `reply_to` (a snowflake) adds a `message_reference` (the Python
    /// `_reply_reference_for_send`); `thread_id` (metadata `thread_id`) retargets
    /// the send into a thread the same way the Python resolves it first.
    pub async fn send_message(
        &self,
        channel_id: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> Result<Value> {
        if content.trim().is_empty() {
            // Python: "Refusing to send empty message" (caller bug).
            return Err(anyhow!("Refusing to send empty message"));
        }
        let mut payload = json!({
            "content": content,
            "allowed_mentions": default_allowed_mentions(),
        });
        if let Some(msg_id) = reply_to {
            payload["message_reference"] = json!({ "message_id": msg_id });
        }
        self.http
            .post_json(format!("{}/channels/{}/messages", self.base, channel_id), payload)
            .await
    }

    /// One logical send: format → chunk at the 2000-char cap → cap split
    /// deliveries → POST each chunk. Returns the delivered chunk ids in order
    /// (`SendOutcome.message_ids`, the Python `raw_response["message_ids"]`).
    pub async fn send(
        &self,
        chat_id: &str,
        raw_markdown: &str,
        reply_to: Option<&str>,
        thread_id: Option<&str>,
    ) -> Result<SendOutcome> {
        let target = thread_id.unwrap_or(chat_id);
        let formatted = format_message(raw_markdown);
        // Discord counts plain characters (the Python truncate_message default len).
        let char_len = |s: &str| s.chars().count() as i64;
        let chunks = cap_split_chunks(truncate_message(&formatted, MAX_MESSAGE_LENGTH, char_len));
        let mut message_ids = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.iter().enumerate() {
            // Reply-reference mode "first" (default): only the first chunk
            // carries the message_reference.
            let reference = if i == 0 { reply_to } else { None };
            let resp = self.send_message(target, chunk, reference).await?;
            message_ids.push(
                resp.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            );
        }
        Ok(SendOutcome { chunks: chunks.len(), message_ids })
    }

    /// `POST /channels/{id}/typing` (`send_typing`).
    pub async fn send_typing(&self, channel_id: &str) -> Result<()> {
        self.http
            .post_json(format!("{}/channels/{}/typing", self.base, channel_id), json!({}))
            .await
            .map(|_| ())
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

    impl StubApi {
        fn new(responses: Vec<(String, Value)>) -> (Self, StubHandle) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (
                StubApi { responses: Mutex::new(responses), calls: calls.clone() },
                StubHandle { calls },
            )
        }
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
    async fn single_message_posts_once_with_allowed_mentions() {
        let (stub, handle) = StubApi::new(vec![(
            "/messages".into(),
            json!({ "id": "101", "content": "hi" }),
        )]);
        let rest = DiscordRest::new(stub, "TOKEN");
        let out = rest.send("555", "hi there", None, None).await.unwrap();
        assert_eq!(out.chunks, 1);
        assert_eq!(out.message_ids, vec!["101"]);
        let calls = handle.calls();
        assert_eq!(calls.len(), 1);
        let (url, payload) = &calls[0];
        assert!(url.ends_with("/channels/555/messages"));
        assert_eq!(payload["content"], "hi there");
        // Everyone/role pings denied by default (#_build_allowed_mentions).
        assert_eq!(payload["allowed_mentions"]["parse"], json!(["users"]));
        assert_eq!(payload["allowed_mentions"]["replied_user"], true);
        // No message_reference without reply_to.
        assert!(payload.get("message_reference").is_none());
    }

    #[tokio::test]
    async fn chunks_at_2000_char_cap() {
        // 5000 chars → 3 chunks (2000/2000/1000 at the Python split points).
        let long = "x".repeat(5000);
        let (stub, handle) = StubApi::new(vec![
            ("/messages".into(), json!({ "id": "1" })),
            ("/messages".into(), json!({ "id": "2" })),
            ("/messages".into(), json!({ "id": "3" })),
        ]);
        let rest = DiscordRest::new(stub, "TOKEN");
        let out = rest.send("555", &long, None, None).await.unwrap();
        assert!(out.chunks > 1);
        let calls = handle.calls();
        assert_eq!(calls.len(), out.chunks);
        for (url, payload) in &calls {
            assert!(url.ends_with("/channels/555/messages"));
            let text = payload["content"].as_str().unwrap();
            // The 2000-char cap includes the (N/M) indicator (truncate_message
            // reserves 10 chars for it).
            assert!(
                text.chars().count() as i64 <= MAX_MESSAGE_LENGTH,
                "chunk over the 2000-char cap: {}",
                text.chars().count()
            );
        }
        // Indicator suffixes on multi-chunk deliveries.
        assert!(calls[0].1["content"].as_str().unwrap().ends_with(" (1/3)"));
        assert!(calls[2].1["content"].as_str().unwrap().ends_with(" (3/3)"));
        assert_eq!(out.message_ids, vec!["1", "2", "3"]);
    }

    #[tokio::test]
    async fn reply_reference_only_on_first_chunk() {
        let long = "y".repeat(4500);
        let (stub, handle) = StubApi::new(vec![
            ("/messages".into(), json!({ "id": "1" })),
            ("/messages".into(), json!({ "id": "2" })),
            ("/messages".into(), json!({ "id": "3" })),
        ]);
        let rest = DiscordRest::new(stub, "TOKEN");
        rest.send("555", &long, Some("999"), None).await.unwrap();
        let calls = handle.calls();
        assert!(calls[0].1["message_reference"]["message_id"] == "999");
        assert!(calls[1].1.get("message_reference").is_none());
        assert!(calls[2].1.get("message_reference").is_none());
    }

    #[tokio::test]
    async fn thread_id_retargets_the_send() {
        let (stub, handle) =
            StubApi::new(vec![("/messages".into(), json!({ "id": "7" }))]);
        let rest = DiscordRest::new(stub, "TOKEN");
        rest.send("555", "in a thread", None, Some("777")).await.unwrap();
        let calls = handle.calls();
        assert!(calls[0].0.ends_with("/channels/777/messages"));
    }

    #[tokio::test]
    async fn empty_message_is_refused() {
        let (stub, _handle) = StubApi::new(vec![]);
        let rest = DiscordRest::new(stub, "TOKEN");
        let err = rest.send("555", "   ", None, None).await.unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn oversized_delivery_capped_at_8_chunks_with_notice() {
        // 8 * 2000 chars of content → far more than 8 chunks → capped.
        let long = "z".repeat(19_000);
        let responses: Vec<(String, Value)> = (0..8)
            .map(|_| ("/messages".into(), json!({ "id": "x" })))
            .collect();
        let (stub, handle) = StubApi::new(responses);
        let rest = DiscordRest::new(stub, "TOKEN");
        let out = rest.send("555", &long, None, None).await.unwrap();
        assert_eq!(out.chunks, 8); // MAX_SPLIT_MESSAGES: 7 chunks + notice
        let calls = handle.calls();
        assert_eq!(calls.len(), 8);
        let last = calls.last().unwrap().1["content"].as_str().unwrap();
        assert!(last.contains("Response truncated"));
        assert!(last.contains("characters were not delivered"));
        // The notice chunk itself stays within the cap.
        assert!(last.chars().count() as i64 <= MAX_MESSAGE_LENGTH);
    }

    #[tokio::test]
    async fn transport_error_propagates_as_send_path_degraded() {
        let (stub, _handle) = StubApi::new(vec![]); // no canned response → error
        let rest = DiscordRest::new(stub, "TOKEN");
        let err = rest.send("555", "hi", None, None).await;
        assert!(err.is_err());
    }
}
