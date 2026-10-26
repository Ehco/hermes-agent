//! Shared gateway contract (port of `gateway/platforms/base.py` + the shared
//! pieces of `plugins/platforms/*`).
//!
//! An inbound chat message becomes an [`InboundMessage`]; the agent's answer is
//! formatted per platform (markdown dialect), split into chunks that respect the
//! platform's message-size cap, and delivered back over the platform transport.

use anyhow::Result;
use std::future::Future;
use std::pin::Pin;

/// Boxed async trait method return (avoids an `async-trait` dependency).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Chat platforms with a ported gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Telegram,
    Discord,
    Slack,
}

impl Platform {
    pub fn name(&self) -> &'static str {
        match self {
            Platform::Telegram => "telegram",
            Platform::Discord => "discord",
            Platform::Slack => "slack",
        }
    }
}

/// One inbound chat message, normalized across platforms.
///
/// This is the payload the gateway feeds to the agent loop to run a turn; the
/// reply goes back through [`GatewayTransport::send_reply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    pub platform: Platform,
    /// Platform-native chat/channel identifier (Telegram chat id, Discord
    /// channel/snowflake id, Slack channel id).
    pub chat_id: String,
    /// Platform-native sender identifier.
    pub user_id: String,
    pub username: Option<String>,
    /// Message text (or caption); empty for non-text messages.
    pub text: String,
    /// Platform-native message id (Telegram message_id, Discord snowflake,
    /// Slack `ts`).
    pub message_id: String,
    /// Thread/topic identifier (Telegram forum topic, Slack `thread_ts`);
    /// `None` in main-chat DMs.
    pub thread_id: Option<String>,
}

/// Result of delivering one logical reply (possibly several chunks).
#[derive(Debug, Clone)]
pub struct SendOutcome {
    /// Platform-native ids of the delivered message(s), last-first delivery
    /// order (the final chunk's id is the streaming edit target).
    pub message_ids: Vec<String>,
    pub chunks: usize,
}

/// A chat-platform transport: poll inbound messages + send replies.
///
/// Mirrors the Python `PlatformAdapter` surface actually used by the gateway
/// loop (`run_inbound` → agent turn → `send`/`edit_message`), stripped of the
/// media/voice/slash-command lanes (not ported).
pub trait GatewayTransport: Send + Sync {
    /// Long-poll / fetch new inbound messages, advancing internal cursors
    /// (Telegram `update_id` offset) so no message is delivered twice.
    fn poll(&mut self) -> BoxFut<'_, Result<Vec<InboundMessage>>>;

    /// Deliver `raw_markdown` (standard markdown from the agent) to a chat,
    /// formatting to the platform dialect and chunking at the platform cap.
    fn send_reply<'a>(
        &'a self,
        chat_id: &'a str,
        text: &'a str,
        thread_id: Option<&'a str>,
    ) -> BoxFut<'a, Result<SendOutcome>>;
}

/// One platform HTTP API. Implemented by [`ReqwestBackend`] for production and
/// by stubs in tests (canned responses, no network).
pub trait HttpBackend: Send + Sync + 'static {
    /// POST a JSON body to `url` and parse the JSON response. Non-success
    /// payloads are returned as `Err` with the platform's error text.
    fn post_json<'a>(
        &'a self,
        url: String,
        payload: serde_json::Value,
    ) -> BoxFut<'a, Result<serde_json::Value>>;

    /// POST a multipart form (the Slack `files_upload_v2` binary lane):
    /// `fields` are plain text parts, then one binary part `filename` +
    /// `content`. Returns the response body text (the Slack upload endpoint
    /// answers plain `OK`, not JSON). JSON-only backends default to a clear
    /// error.
    fn post_multipart<'a>(
        &'a self,
        _url: String,
        _fields: Vec<(String, String)>,
        _filename: String,
        _content: Vec<u8>,
    ) -> BoxFut<'a, Result<String>> {
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "multipart upload not supported by this HTTP backend"
            ))
        })
    }
    /// POST a raw body with an explicit Content-Type (the Discord attachment
    /// lane: a prebuilt multipart/form-data body). JSON-only backends default
    /// to a clear error.
    fn post_raw_body<'a>(
        &'a self,
        _url: String,
        _content_type: String,
        _body: Vec<u8>,
    ) -> BoxFut<'a, Result<serde_json::Value>> {
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "raw-body POST not supported by this HTTP backend"
            ))
        })
    }
}

/// Production backend over the shared `reqwest::Client`.
pub struct ReqwestBackend {
    http: reqwest::Client,
    /// Default `Authorization: Bearer` token (Discord bot token / Slack bot
    /// token); Telegram API uses no auth header (token is in the URL).
    bearer: Option<String>,
}

impl ReqwestBackend {
    /// One client per process (connection pooling), as in the Python adapters.
    pub fn shared() -> Self {
        ReqwestBackend {
            http: reqwest::Client::new(),
            bearer: None,
        }
    }

    /// Client pre-configured with an `Authorization: Bearer` header
    /// (Discord bot token / Slack bot token).
    pub fn with_bearer(token: &str) -> Self {
        ReqwestBackend {
            http: reqwest::Client::new(),
            bearer: Some(token.to_string()),
        }
    }
}

impl HttpBackend for ReqwestBackend {
    fn post_json<'a>(
        &'a self,
        url: String,
        payload: serde_json::Value,
    ) -> BoxFut<'a, Result<serde_json::Value>> {
        Box::pin(async move {
            let mut req = self.http.post(url).json(&payload);
            if let Some(token) = &self.bearer {
                req = req.bearer_auth(token);
            }
            let resp = req.send().await?;
            let status = resp.status();
            let body: serde_json::Value = resp.json().await?;
            if !status.is_success() {
                anyhow::bail!("HTTP {}: {}", status, body);
            }
            Ok(body)
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
            let mut req = self.http.post(url);
            for (k, v) in &fields {
                req = req.header(k.as_str(), v.as_str());
            }
            // Multipart parts: text fields first, then the binary file part.
            let mut m = reqwest::multipart::Form::new();
            for (k, v) in fields {
                m = m.part(k, reqwest::multipart::Part::text(v));
            }
            m = m.part(
                "file",
                reqwest::multipart::Part::bytes(content)
                    .file_name(filename)
                    .mime_str("application/octet-stream")?,
            );
            req = req.multipart(m);
            let resp = req.send().await?;
            let status = resp.status();
            let body = resp.text().await?;
            if !status.is_success() {
                anyhow::bail!("HTTP {}: {}", status, body);
            }
            Ok(body)
        })
    }

    fn post_raw_body<'a>(
        &'a self,
        url: String,
        content_type: String,
        body: Vec<u8>,
    ) -> BoxFut<'a, Result<serde_json::Value>> {
        Box::pin(async move {
            let resp = self
                .http
                .post(url)
                .header("Content-Type", content_type)
                .body(body)
                .send()
                .await?;
            let status = resp.status();
            let text = resp.text().await?;
            if !status.is_success() {
                anyhow::bail!("HTTP {}: {}", status, text);
            }
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("bad JSON: {}", e))
        })
    }
}
