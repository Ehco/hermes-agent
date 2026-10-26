//! Discord native media attachments — port of the portable slice of
//! `plugins/platforms/discord/adapter_media.py` (the `DiscordMediaMixin`).
//!
//! What ports (pure logic + the raw multipart send, behind the
//! [`crate::contract::HttpBackend::post_raw_body`] seam):
//! - the upload-limit preflight (`_discord_upload_limit_bytes` +
//!   `_reject_oversized_upload`): the 20 MiB platform default (since the
//!   Sep 3 2026 API change; 10 MiB before), the guild's boost-aware
//!   `filesize_limit` preferred and floored at the default, and the
//!   "File too large for Discord upload: …" error/notice text,
//! - the multi-image batching (`send_multiple_images`): URLs/local paths
//!   batch into chunks of at most 10 attachments per message, oversized
//!   images are skipped with a notice instead of 413-ing the whole chunk,
//! - the attachment multipart envelope: `payload_json` text part first,
//!   then the binary `files[0]` part (the shape every native attachment
//!   POST uses, including the voice-message flags=8192 envelope).
//!
//! What stays deferred (Electron/host-entangled or discord.py-shaped):
//! - forum-parent channel routing (`_is_forum_parent` / `_forum_post_file`),
//! - URL download with proxy resolution and redirect guarding
//!   (`_read_url_image_with_redirect_guard` + `DISCORD_PROXY`) — the caller
//!   downloads and hands this module bytes,
//! - the voice-mixer/ffmpeg subsystem and `play_tts` voice-channel routing
//!   (adapter voice state), and the `mutagen` OggOpus duration probe (the
//!   `len / 2000` byte-rate fallback IS ported, see
//!   [`voice_message_payload`]).

use crate::contract::HttpBackend;
use crate::discord::rest::DiscordRest;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// `adapter_media.py::_DISCORD_DEFAULT_UPLOAD_LIMIT_BYTES` — 20 MiB since the
/// Sep 3 2026 API change (10 MiB before).
pub const DEFAULT_UPLOAD_LIMIT_BYTES: u64 = 20 * 1024 * 1024;

/// `_discord_upload_limit_bytes` — the effective attachment cap for a channel.
/// The guild's boost-aware `filesize_limit` wins when present, floored at the
/// platform default (discord.py's unboosted-tier constant can lag a
/// platform-wide raise, and under-reporting makes the preflight reject files
/// Discord would accept).
pub fn upload_limit_bytes(guild_filesize_limit: Option<u64>) -> u64 {
    match guild_filesize_limit {
        Some(limit) if limit > 0 => limit.max(DEFAULT_UPLOAD_LIMIT_BYTES),
        _ => DEFAULT_UPLOAD_LIMIT_BYTES,
    }
}

/// `adapter_media.py` size/limit MB rendering (one decimal, limit rounded).
fn mb(value: u64) -> String {
    format!("{:.1}", value as f64 / (1024.0 * 1024.0))
}

fn limit_mb(limit: u64) -> String {
    format!("{:.0}", limit as f64 / (1024.0 * 1024.0))
}

/// `_reject_oversized_upload`'s failure message (pre-#50846 a doomed 413
/// round-trip; now skipped in favor of this notice).
pub fn oversized_upload_error(filename: &str, size_bytes: u64, limit: u64) -> String {
    format!(
        "File too large for Discord upload: {filename} is {} MB (limit {} MB)",
        mb(size_bytes),
        limit_mb(limit)
    )
}

/// The user-facing "image skipped" notice text
/// (`platform.discord.media.image_skipped_too_large`).
pub fn oversized_skip_notice(filename: &str, size_bytes: u64, limit: u64) -> String {
    format!(
        "Skipped {filename}: {} MB exceeds the {} MB Discord upload limit",
        mb(size_bytes),
        limit_mb(limit)
    )
}

/// `send_multiple_images` — `CHUNK = 10`: at most 10 attachments per message.
pub fn chunk_images<T>(images: &[T]) -> Vec<&[T]> {
    images.chunks(10).collect()
}

/// `_reject_oversized_upload` as a decision: `Err` = reject with the notice
/// (the upload is skipped before the doomed round-trip).
pub fn preflight_upload(
    size_bytes: u64,
    filename: &str,
    limit: u64,
) -> std::result::Result<(), String> {
    if size_bytes <= limit {
        Ok(())
    } else {
        Err(oversized_upload_error(filename, size_bytes, limit))
    }
}

/// The multipart/form-data body for one attachment POST
/// (`POST /channels/{id}/messages`): text fields first (`payload_json`),
/// then the binary `files[0]` part. Pure builder — the send goes through
/// [`DiscordRest::send_attachment`].
pub fn attachment_multipart_body(
    payload_json: &str,
    filename: &str,
    content_type: &str,
    data: &[u8],
    boundary: &str,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"payload_json\"\r\n\r\n{payload_json}\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"files[0]\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// `send_voice`'s native voice-message envelope: `flags: 8192` (IS_VOICE_MESSAGE),
/// the `voice-message.ogg` attachment with `duration_secs` and a flat base64
/// waveform. `duration_secs` follows the Python fallback when the OggOpus
/// duration probe is unavailable: `max(1.0, bytes / 2000.0)` (2000 B/s floor
/// for an Opus voice message).
pub fn voice_message_payload(audio_bytes: usize, opus_duration_secs: Option<f64>) -> Value {
    let duration_secs = opus_duration_secs
        .filter(|d| *d > 0.0)
        .unwrap_or_else(|| (audio_bytes as f64 / 2000.0).max(1.0));
    let encoded = base64_flat(&[128u8; 256]);
    json!({
        "flags": 8192,
        "attachments": [{
            "id": "0",
            "filename": "voice-message.ogg",
            "duration_secs": (duration_secs * 100.0).round() / 100.0,
            "waveform": encoded,
        }],
    })
}

/// `base64.b64encode(...).decode()` (RFC 4648, padded; the waveform is a
/// multiple of 3? No — 256 bytes ≡ 1 mod 3, so one '=' of padding).
fn base64_flat(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        out.push(TABLE[(b[0] >> 2) as usize] as char);
        out.push(TABLE[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b[2] & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

impl<B: HttpBackend> DiscordRest<B> {
    /// `adapter_media.py::_send_file_attachment` / `send_voice` raw lane:
    /// POST one attachment as `payload_json` + `files[0]`. `payload_json`
    /// carries `{"content": caption, "attachments": [...], "flags": ...}`;
    /// on failure the caller falls back to a plain URL message (the Python
    /// `fallback(metadata)` contract).
    pub async fn send_attachment(
        &self,
        channel_id: &str,
        payload_json: &str,
        filename: &str,
        content_type: &str,
        data: Vec<u8>,
    ) -> Result<Value> {
        let boundary = format!(
            "hermes-discord-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let body = attachment_multipart_body(payload_json, filename, content_type, &data, &boundary);
        let url = format!("{}/channels/{}/messages?files=0", self.base_url(), channel_id);
        self.http_backend()
            .post_raw_body(
                url,
                format!("multipart/form-data; boundary={boundary}"),
                body,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::BoxFut;

    struct StubApi {
        responses: std::sync::Mutex<Vec<Value>>,
        calls: std::sync::Arc<std::sync::Mutex<Vec<(String, String, Vec<u8>)>>>,
    }

    impl HttpBackend for StubApi {
        fn post_json<'a>(
            &'a self,
            _url: String,
            _payload: Value,
        ) -> BoxFut<'a, Result<Value>> {
            Box::pin(async move { Err(anyhow!("not used")) })
        }
        fn post_raw_body<'a>(
            &'a self,
            url: String,
            content_type: String,
            body: Vec<u8>,
        ) -> BoxFut<'a, Result<Value>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push((url, content_type, body.clone()));
                self.responses
                    .lock()
                    .unwrap()
                    .pop()
                    .map(Ok)
                    .unwrap_or_else(|| Err(anyhow!("no canned response")))
            })
        }
    }

    #[test]
    fn upload_limit_floors_guild_value_at_platform_default() {
        // A guild reporting the pre-Sept 10 MiB constant still gets the
        // 20 MiB floor (the Python max()).
        assert_eq!(upload_limit_bytes(Some(10 * 1024 * 1024)), DEFAULT_UPLOAD_LIMIT_BYTES);
        // A boost-raised guild limit wins.
        assert_eq!(
            upload_limit_bytes(Some(50 * 1024 * 1024)),
            50 * 1024 * 1024
        );
        // DMs / missing guild: the platform default.
        assert_eq!(upload_limit_bytes(None), DEFAULT_UPLOAD_LIMIT_BYTES);
        assert_eq!(upload_limit_bytes(Some(0)), DEFAULT_UPLOAD_LIMIT_BYTES);
    }

    #[test]
    fn oversize_messages_name_size_and_limit() {
        let limit = DEFAULT_UPLOAD_LIMIT_BYTES; // 20 MiB
        let size = 25_461_598_u64; // ~24.3 MB
        assert_eq!(
            oversized_upload_error("video.mp4", size, limit),
            "File too large for Discord upload: video.mp4 is 24.3 MB (limit 20 MB)"
        );
        assert_eq!(
            oversized_skip_notice("cat.png", 21_000_000_u64, limit),
            "Skipped cat.png: 20.0 MB exceeds the 20 MB Discord upload limit"
        );
    }

    #[test]
    fn preflight_accepts_at_and_under_the_limit() {
        let limit = DEFAULT_UPLOAD_LIMIT_BYTES;
        assert!(preflight_upload(limit, "a.bin", limit).is_ok());
        assert!(preflight_upload(limit - 1, "a.bin", limit).is_ok());
        assert!(preflight_upload(limit + 1, "a.bin", limit).is_err());
    }

    #[test]
    fn images_batch_at_ten_per_message() {
        let images: Vec<u32> = (0..23).collect();
        let chunks = chunk_images(&images);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 10);
        assert_eq!(chunks[2].len(), 3);
    }

    #[test]
    fn multipart_body_is_payload_json_then_files0() {
        let body = attachment_multipart_body(
            r#"{"content":"hi"}"#,
            "report.pdf",
            "application/pdf",
            b"%PDF-1.4",
            "BOUNDARY",
        );
        let text = String::from_utf8(body).unwrap();
        // Text part first, named payload_json…
        assert!(text.contains("name=\"payload_json\""));
        assert!(text.contains("{\"content\":\"hi\"}"));
        // …binary part named files[0] with filename + content type…
        assert!(text.contains("name=\"files[0]\"; filename=\"report.pdf\""));
        assert!(text.contains("Content-Type: application/pdf"));
        // …raw bytes verbatim…
        assert!(text.contains("%PDF-1.4"));
        // …and a closing boundary.
        assert!(text.ends_with("--BOUNDARY--\r\n"));
    }

    #[test]
    fn voice_payload_carries_the_flag_and_flat_waveform() {
        let payload = voice_message_payload(4000, None);
        assert_eq!(payload["flags"], 8192);
        let attachment = &payload["attachments"][0];
        assert_eq!(attachment["filename"], "voice-message.ogg");
        // 4000 bytes / 2000 = 2.0s.
        assert_eq!(attachment["duration_secs"], 2.0);
        // Flat waveform: 256 bytes of 0x80 → 85 "gICA" groups + the "gA==" tail.
        let expected = format!("{}{}", "gICA".repeat(85), "gA==");
        assert_eq!(attachment["waveform"], expected);
        // The byte-rate floor never drops under a second.
        assert_eq!(voice_message_payload(100, None)["attachments"][0]["duration_secs"], 1.0);
        // A real OggOpus probe wins over the fallback.
        assert_eq!(
            voice_message_payload(100, Some(7.25))["attachments"][0]["duration_secs"],
            7.25
        );
    }

    #[tokio::test]
    async fn send_attachment_posts_the_multipart_envelope() {
        let responses = std::sync::Mutex::new(vec![json!({ "id": "55" })]);
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let rest = DiscordRest::new(
            StubApi { responses, calls: calls.clone() },
            "TOKEN",
        );
        let out = rest
            .send_attachment(
                "42",
                r#"{"content":null,"attachments":[{"id":"0","filename":"v.ogg","flags":0}]}"#,
                "v.ogg",
                "audio/ogg",
                b"OggS".to_vec(),
            )
            .await
            .unwrap();
        assert_eq!(out["id"], "55");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (url, content_type, body) = &calls[0];
        assert!(url.ends_with("/channels/42/messages?files=0"));
        assert!(content_type.starts_with("multipart/form-data; boundary=hermes-discord-"));
        let body = String::from_utf8(body.clone()).unwrap();
        assert!(body.contains("name=\"files[0]\"; filename=\"v.ogg\""));
        assert!(body.contains("OggS"));
    }
}
