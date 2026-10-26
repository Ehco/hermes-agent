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
use std::time::Duration;

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

/// Base retry backoff for transient upload failures — the Python
/// `_upload_with_retry` sleeps `1.5 * (attempt + 1)` seconds between tries.
pub const RETRY_BASE: Duration = Duration::from_millis(1500);

/// Transient upload failures worth retrying (Python `_TRANSIENT_UPLOAD_MARKERS`
/// plus the HTTP status the Python inspects: 429 / any 5xx).
const TRANSIENT_UPLOAD_MARKERS: &[&str] = &[
    "rate_limited",
    "ratelimited",
    "429",
    "connection reset",
    "service unavailable",
    "temporarily unavailable",
];

/// Python `_is_retryable_upload_error`: HTTP status 429/5xx (surfaced in the
/// error text by the HTTP layer) or a transient marker in the message.
fn is_retryable_upload_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    if TRANSIENT_UPLOAD_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // "HTTP 500 …"-style statuses carried by our backend errors.
    if let Some(rest) = lower.split("http ").nth(1) {
        if let Some(code) = rest
            .chars()
            .take(3)
            .try_fold(0u32, |acc, c| c.to_digit(10).map(|d| acc * 10 + d))
        {
            if code == 429 || code >= 500 {
                return true;
            }
        }
    }
    false
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
        let resp = self
            .http
            .post_json(format!("{}/chat.postMessage", self.base), payload)
            .await?;
        if resp.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = resp
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
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
        if chunks.is_empty() {
            vec![formatted]
        } else {
            chunks
        }
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
            return Ok(SendOutcome {
                chunks: 0,
                message_ids: Vec::new(),
            });
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
                    let blocks_error = BLOCK_FALLBACK_ERRORS
                        .iter()
                        .any(|needle| msg.contains(needle));
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
        Ok(SendOutcome {
            chunks: chunks.len(),
            message_ids,
        })
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
        let resp = self
            .http
            .post_json(format!("{}/chat.postEphemeral", self.base), payload)
            .await?;
        if resp.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = resp
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(anyhow!("chat.postEphemeral failed: {}", err));
        }
        Ok(resp
            .get("message_ts")
            .or_else(|| resp.get("ts"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    // ---- file lane (Python `files_upload_v2` port) ----

    /// Slack `files.upload`-style upload via the v2 three-step flow (the
    /// slack_sdk `files_upload_v2` the Python adapter calls):
    /// 1. `files.getUploadURLExternal` (filename + length) → upload URL,
    /// 2. POST the bytes to that URL (multipart),
    /// 3. `files.completeUploadExternal` (file id + channel + thread/comment).
    /// Retries transient failures up to `attempts` times (`_upload_with_retry`
    /// contract: rate limits / 5xx / connection resets, 1.5s·(attempt+1) backoff).
    pub async fn upload_file(
        &self,
        chat_id: &str,
        filename: &str,
        content: Vec<u8>,
        caption: Option<&str>,
        thread_ts: Option<&str>,
    ) -> Result<()> {
        self.upload_file_with_delay(chat_id, filename, content, caption, thread_ts, 3, RETRY_BASE)
            .await
    }

    /// [`upload_file`] with attempts/backoff injectable (tests keep sleeps at
    /// zero and stub responses per attempt).
    pub async fn upload_file_with_delay(
        &self,
        chat_id: &str,
        filename: &str,
        content: Vec<u8>,
        caption: Option<&str>,
        thread_ts: Option<&str>,
        attempts: u32,
        retry_base: Duration,
    ) -> Result<()> {
        for attempt in 0..attempts.max(1) {
            match self.upload_once(chat_id, filename, &content, caption, thread_ts).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let last = attempt + 1 == attempts.max(1);
                    if last || !is_retryable_upload_error(&e.to_string()) {
                        return Err(e);
                    }
                    tokio::time::sleep(retry_base * (attempt as u32 + 1)).await;
                }
            }
        }
        unreachable!()
    }

    /// One full v2 upload attempt (three API calls).
    async fn upload_once(
        &self,
        chat_id: &str,
        filename: &str,
        content: &[u8],
        caption: Option<&str>,
        thread_ts: Option<&str>,
    ) -> Result<()> {
        // 1. Ask for an upload URL (length = the byte count Slack will accept).
        let ask = self
            .http
            .post_json(
                format!("{}/files.getUploadURLExternal", self.base),
                json!({
                    "token": self.token,
                    "filename": filename,
                    "length": content.len(),
                }),
            )
            .await?;
        if ask.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = ask
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(anyhow!("files.getUploadURLExternal failed: {}", err));
        }
        let upload_url = ask
            .get("upload_url")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("files.getUploadURLExternal: no upload_url"))?;
        let file_id = ask.get("file_id").and_then(Value::as_str).unwrap_or("");

        // 2. Upload the bytes (multipart; Slack answers plain "OK").
        let upload = self
            .http
            .post_multipart(
                upload_url.to_string(),
                vec![("filename".into(), filename.to_string())],
                filename.to_string(),
                content.to_vec(),
            )
            .await?;
        anyhow::ensure!(
            upload.trim() == "OK" || !upload.contains("error"),
            "file upload rejected: {}",
            upload
        );

        // 3. Publish: attach the uploaded file to the channel/thread/comment.
        let mut complete = json!({
            "token": self.token,
            "files": json!([{ "id": file_id, "title": filename }]).to_string(),
            "channel_id": chat_id,
        });
        if let Some(ts) = thread_ts {
            complete["thread_ts"] = json!(ts);
        }
        if let Some(caption) = caption {
            complete["initial_comment"] = json!(format_message(caption));
        }
        let resp = self
            .http
            .post_json(format!("{}/files.completeUploadExternal", self.base), complete)
            .await?;
        if resp.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = resp
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(anyhow!("files.completeUploadExternal failed: {}", err));
        }
        Ok(())
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
        /// Canned plain-text answers for the multipart upload URL lane.
        multipart_responses: Mutex<Vec<(String, String)>>,
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
                StubApi {
                    responses: Mutex::new(responses),
                    multipart_responses: Mutex::new(Vec::new()),
                    calls: calls.clone(),
                },
                StubHandle { calls },
            )
        }

        /// Queue one canned plain-text answer for the multipart upload lane.
        fn with_multipart(mut self, needle: &str, resp: &str) -> Self {
            self.multipart_responses
                .lock()
                .unwrap()
                .push((needle.to_string(), resp.to_string()));
            self
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

        fn post_multipart<'a>(
            &'a self,
            url: String,
            fields: Vec<(String, String)>,
            filename: String,
            content: Vec<u8>,
        ) -> BoxFut<'a, Result<String>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push((
                    url.clone(),
                    json!({
                        "filename": filename,
                        "bytes": content.len(),
                        "fields": fields,
                    }),
                ));
                let mut queue = self.multipart_responses.lock().unwrap();
                for (idx, (needle, resp)) in queue.iter().enumerate() {
                    if url.contains(needle.as_str()) {
                        let resp = resp.clone();
                        queue.remove(idx);
                        return Ok(resp);
                    }
                }
                Err(anyhow!("stub: no canned multipart response for {}", url))
            })
        }
    }

    fn ok_ts(ts: &str) -> Value {
        json!({ "ok": true, "ts": ts, "message": { "ts": ts } })
    }

    /// Clone the shared call log handle out of a stub before it is moved into
    /// the client under test.
    fn stub_handle(stub: &StubApi) -> StubHandle {
        StubHandle {
            calls: stub.calls.clone(),
        }
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
        assert!(
            calls[0].1["text"].is_string(),
            "blocks must pair with a text fallback"
        );

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
        assert!(
            calls[1].1.get("blocks").is_none(),
            "retry must drop the blocks"
        );
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
        let ts = rest
            .send_ephemeral("C1", "U7", "**hi**", None)
            .await
            .unwrap();
        assert_eq!(ts, "9.9");
        let calls = handle.calls();
        assert!(calls[0].0.ends_with("/chat.postEphemeral"));
        assert_eq!(calls[0].1["user"], "U7");
        assert_eq!(calls[0].1["text"], "*hi*");
    }

    // ---- file lane (files_upload_v2 port) ----

    #[tokio::test]
    async fn upload_runs_the_v2_three_step_flow() {
        let (stub, handle) = StubApi::new(vec![
            (
                "files.getUploadURLExternal".into(),
                json!({ "ok": true, "upload_url": "https://files.slack.com/upload", "file_id": "F1" }),
            ),
            (
                "files.completeUploadExternal".into(),
                json!({ "ok": true, "file": { "id": "F1" } }),
            ),
        ]);
        let stub = stub.with_multipart("files.slack.com", "OK");
        let handle = stub_handle(&stub);
        let rest = SlackRest::new(stub, "xoxb-TOK");
        rest.upload_file("C1", "report.png", b"bytes".to_vec(), Some("**cap**"), Some("17.5"))
            .await
            .unwrap();

        let calls = handle.calls();
        assert_eq!(calls.len(), 3);
        // 1. URL request carries filename + exact byte length + the token.
        assert!(calls[0].0.ends_with("/files.getUploadURLExternal"));
        assert_eq!(calls[0].1["filename"], "report.png");
        assert_eq!(calls[0].1["length"], 5);
        assert_eq!(calls[0].1["token"], "xoxb-TOK");
        // 2. Binary part goes to the returned upload URL with the file name.
        assert_eq!(calls[1].0, "https://files.slack.com/upload");
        assert_eq!(calls[1].1["bytes"], 5);
        assert_eq!(calls[1].1["filename"], "report.png");
        // 3. Completion binds the file to the channel/thread, caption is
        // mrkdwn-formatted (Python `initial_comment`).
        let complete = &calls[2].1;
        assert!(calls[2].0.ends_with("/files.completeUploadExternal"));
        assert_eq!(complete["channel_id"], "C1");
        assert_eq!(complete["thread_ts"], "17.5");
        let files: Value = serde_json::from_str(complete["files"].as_str().unwrap()).unwrap();
        assert_eq!(files[0]["id"], "F1");
        assert_eq!(files[0]["title"], "report.png");
        assert_eq!(complete["initial_comment"], "*cap*");
    }

    #[tokio::test]
    async fn upload_retries_transient_failures_and_succeeds() {
        let (stub, handle) = StubApi::new(vec![
            // First attempt: rate limited at the URL step.
            (
                "files.getUploadURLExternal".into(),
                json!({ "ok": false, "error": "rate_limited" }),
            ),
            // Retry: full happy path.
            (
                "files.getUploadURLExternal".into(),
                json!({ "ok": true, "upload_url": "https://files.slack.com/up", "file_id": "F2" }),
            ),
            (
                "files.completeUploadExternal".into(),
                json!({ "ok": true }),
            ),
        ]);
        let stub = stub.with_multipart("files.slack.com", "OK");
        let handle = stub_handle(&stub);
        let rest = SlackRest::new(stub, "t");
        rest.upload_file_with_delay("C1", "f.txt", b"x".to_vec(), None, None, 3, Duration::ZERO)
            .await
            .unwrap();
        let calls = handle.calls();
        assert_eq!(calls.len(), 4); // failed URL + retried URL + upload + complete
        assert!(calls[0].0.ends_with("/files.getUploadURLExternal"));
    }

    #[tokio::test]
    async fn upload_does_not_retry_permanent_failures() {
        let (stub, handle) = StubApi::new(vec![(
            "files.getUploadURLExternal".into(),
            json!({ "ok": false, "error": "file_access_denied" }),
        )]);
        let rest = SlackRest::new(stub, "t");
        let err = rest
            .upload_file_with_delay("C1", "f.txt", b"x".to_vec(), None, None, 3, Duration::ZERO)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("file_access_denied"));
        assert_eq!(handle.calls().len(), 1); // no retries on a permanent error
    }

    #[test]
    fn transient_upload_detection_matches_the_python_markers() {
        assert!(is_retryable_upload_error("rate_limited"));
        assert!(is_retryable_upload_error("connection reset by peer"));
        assert!(is_retryable_upload_error("HTTP 503: service unavailable"));
        assert!(!is_retryable_upload_error("file_access_denied"));
        assert!(!is_retryable_upload_error("channel_not_found"));
        // 4xx other than 429 is permanent.
        assert!(!is_retryable_upload_error("HTTP 404: not found"));
    }
}
