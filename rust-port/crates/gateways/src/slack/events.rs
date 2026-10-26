//! Slack Events API receive contract — port of the inbound envelope handling
//! of `plugins/platforms/slack/adapter.py` (the slack-bolt `app.event("message")`
//! / `app.event("app_mention")` subscriptions + the prefilter in
//! `_prefilter_inbound`).
//!
//! Envelope shapes (the two the Python adapter receives):
//! - HTTP Events API: `{"type": "event_callback", "team_id": …, "event": {…}}`
//!   and the verification handshake `{"type": "url_verification", "challenge": …}`
//!   (answered by echoing the challenge),
//! - Socket Mode: `{"type": "events_api", "team_id": …, "payload": {…}}` —
//!   same inner event under a `payload` key; other socket envelopes
//!   (`hello`, `interactive`, `disconnect`) are not the message lane.
//!
//! Inner-event contract (the Python prefilter, in order):
//! - `message_changed` edits normalize to their rewritten `message` payload
//!   (`_normalize_changed_message`) so an @mention added by an edit can wake
//!   the bot once,
//! - events dedup per workspace (`team_id` + `ts` — ts is only unique per team,
//!   socket mode redelivers after reconnects),
//! - bot posts drop under the default `allow_bots = none` (own posts always
//!   drop — echo loops),
//! - housekeeping subtypes drop (joins/leaves/topic changes/deletions…);
//!   `file_share`, `thread_broadcast`, `me_message`, `bot_message` (already
//!   allow-bots-gated) and `document_mention` pass,
//! - `app_mention` events are the explicit wake lane.

use crate::contract::{InboundMessage, Platform};

/// What an inbound envelope decodes to.
#[derive(Debug, PartialEq, Eq)]
pub enum SlackEvent {
    /// URL-verification handshake — respond with the raw challenge string.
    Challenge(String),
    /// A normalizable inbound chat message.
    Inbound(InboundMessage),
    /// Well-formed envelope, nothing for the gateway to do (bot posts,
    /// housekeeping subtypes, non-message lanes, already-seen ts…).
    Ignored,
    /// Not an envelope this port understands.
    Unknown,
}

/// Dedup state across envelopes: workspace-scoped ts set (Slack ts values are
/// only unique per team, and Socket Mode redelivers after reconnects).
#[derive(Debug, Default)]
pub struct EventDedup {
    seen: std::collections::HashSet<String>,
}

impl EventDedup {
    pub fn new() -> Self {
        Default::default()
    }
    /// `_workspace_event_id`: `team_id|event_ts`.
    fn key(team_id: &str, ts: &str) -> String {
        format!("{}|{}", team_id, ts)
    }
    /// True the first time a (team, ts) pair arrives.
    fn first_time(&mut self, team_id: &str, ts: &str) -> bool {
        let key = Self::key(team_id, ts);
        self.seen.insert(key)
    }
}

/// Subtypes that pass the prefilter (`_prefilter_inbound`'s allowlist — an
/// allowlist rather than a denylist so subtypes Slack adds later drop).
const PASSING_SUBTYPES: &[&str] = &[
    "", // real user messages carry no subtype
    "file_share",
    "thread_broadcast",
    "me_message",
    "bot_message", // already allow_bots-gated below
    "document_mention",
];

/// Parse one inbound envelope. `dedup` carries the workspace-scoped seen set
/// across calls (the adapter's `_dedup`).
pub fn parse_envelope(envelope: &serde_json::Value, dedup: &mut EventDedup) -> SlackEvent {
    let env_type = envelope
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    match env_type {
        "url_verification" => {
            return SlackEvent::Challenge(
                envelope
                    .get("challenge")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
        }
        "event_callback" | "events_api" => {
            let event = if env_type == "event_callback" {
                envelope.get("event")
            } else {
                // Socket Mode wraps the inner event under `payload`.
                envelope.get("payload")
            };
            let Some(event) = event else {
                return SlackEvent::Unknown;
            };
            let team_id = envelope
                .get("team_id")
                .or_else(|| envelope.get("team"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            return match normalize_event(event, team_id, dedup) {
                Some(msg) => SlackEvent::Inbound(msg),
                None => SlackEvent::Ignored,
            };
        }
        // Socket Mode lanes outside the message port (hello, interactive,
        // disconnect, slash_commands, …).
        "hello" | "disconnect" | "interactive" | "slash_commands" => SlackEvent::Ignored,
        _ => SlackEvent::Unknown,
    }
}

/// Normalize one inner event into the shared contract (the Python
/// `_prefilter_inbound` + `_build_message_event` ordering).
fn normalize_event(
    event: &serde_json::Value,
    team_id: &str,
    dedup: &mut EventDedup,
) -> Option<InboundMessage> {
    let mut event = event;
    // `message_changed` edits normalize to the rewritten message.
    if event.get("subtype").and_then(serde_json::Value::as_str) == Some("message_changed") {
        event = event.get("message")?;
    }
    let event_type = event.get("type").and_then(serde_json::Value::as_str)?;
    // Only the two subscribed lanes (`message`, `app_mention`).
    if event_type != "message" && event_type != "app_mention" {
        return None;
    }
    // Dedup per workspace: ts is only unique per team; socket mode redelivers
    // after reconnects (#4777).
    let event_ts = event
        .get("ts")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !event_ts.is_empty() && !dedup.first_time(team_id, event_ts) {
        return None;
    }
    // Housekeeping subtypes are not a person speaking (allowlist).
    let subtype = event
        .get("subtype")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !PASSING_SUBTYPES.contains(&subtype) {
        return None;
    }
    // allow_bots = "none" (default): bot posts drop; own posts always drop
    // (echo loops). `bot_id`/`bot_profile` or a `bot_message` subtype marks one.
    if event.get("bot_id").is_some() || event.get("bot_profile").is_some() {
        return None;
    }
    let text = event
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if text.is_empty() {
        return None;
    }
    Some(InboundMessage {
        platform: Platform::Slack,
        chat_id: event
            .get("channel")
            .and_then(serde_json::Value::as_str)?
            .to_string(),
        user_id: event
            .get("user")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        username: None, // resolved via users.info upstream; not carried in events
        text: text.to_string(),
        message_id: event_ts.to_string(),
        thread_id: event
            .get("thread_ts")
            .and_then(serde_json::Value::as_str)
            // A thread root's own ts is not a thread pointer (the Python skips
            // thread_ts equal to the message ts).
            .filter(|t| *t != event_ts)
            .map(String::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inbound_of(e: SlackEvent) -> InboundMessage {
        match e {
            SlackEvent::Inbound(m) => m,
            other => panic!("expected inbound, got {:?}", other),
        }
    }

    #[test]
    fn url_verification_echoes_challenge() {
        let mut dedup = EventDedup::new();
        let env = json!({ "type": "url_verification", "challenge": "abc123" });
        assert_eq!(
            parse_envelope(&env, &mut dedup),
            SlackEvent::Challenge("abc123".to_string())
        );
    }

    #[test]
    fn event_callback_message_normalizes() {
        let mut dedup = EventDedup::new();
        let env = json!({
            "type": "event_callback",
            "team_id": "T1",
            "event": {
                "type": "message", "channel": "C55", "user": "U9", "text": "hello",
                "ts": "1700.0001",
            },
        });
        let msg = inbound_of(parse_envelope(&env, &mut dedup));
        assert_eq!(msg.platform, Platform::Slack);
        assert_eq!(msg.chat_id, "C55");
        assert_eq!(msg.user_id, "U9");
        assert_eq!(msg.text, "hello");
        assert_eq!(msg.message_id, "1700.0001");
        assert!(msg.thread_id.is_none());
    }

    #[test]
    fn socket_mode_envelope_unwraps_payload() {
        let mut dedup = EventDedup::new();
        let env = json!({
            "type": "events_api",
            "team_id": "T1",
            "payload": {
                "type": "app_mention", "channel": "C1", "user": "U2", "text": "<@U0> ping",
                "ts": "1700.0002",
            },
        });
        let msg = inbound_of(parse_envelope(&env, &mut dedup));
        assert_eq!(msg.text, "<@U0> ping");
    }

    #[test]
    fn thread_ts_becomes_thread_id_but_root_ts_does_not() {
        let mut dedup = EventDedup::new();
        let env = json!({
            "type": "event_callback", "team_id": "T1",
            "event": { "type": "message", "channel": "C1", "user": "U1", "text": "in thread",
                       "ts": "1700.1", "thread_ts": "1700.05" },
        });
        let msg = inbound_of(parse_envelope(&env, &mut dedup));
        assert_eq!(msg.thread_id.as_deref(), Some("1700.05"));
        // A thread root (thread_ts == ts) is not a thread pointer.
        let mut dedup2 = EventDedup::new();
        let root = json!({
            "type": "event_callback", "team_id": "T1",
            "event": { "type": "message", "channel": "C1", "user": "U1", "text": "root",
                       "ts": "1700.2", "thread_ts": "1700.2" },
        });
        let msg2 = inbound_of(parse_envelope(&root, &mut dedup2));
        assert!(msg2.thread_id.is_none());
    }

    #[test]
    fn socket_mode_redelivery_is_deduped_per_workspace() {
        let mut dedup = EventDedup::new();
        let env = json!({
            "type": "event_callback", "team_id": "T1",
            "event": { "type": "message", "channel": "C1", "user": "U1", "text": "once", "ts": "9.9" },
        });
        assert!(matches!(
            parse_envelope(&env, &mut dedup),
            SlackEvent::Inbound(_)
        ));
        // Redelivery (same team + ts) → ignored.
        assert_eq!(parse_envelope(&env, &mut dedup), SlackEvent::Ignored);
        // Same ts in another workspace is a DIFFERENT event (ts only unique per team).
        let other_team = json!({
            "type": "event_callback", "team_id": "T2",
            "event": { "type": "message", "channel": "C1", "user": "U1", "text": "other team", "ts": "9.9" },
        });
        assert!(matches!(
            parse_envelope(&other_team, &mut dedup),
            SlackEvent::Inbound(_)
        ));
    }

    #[test]
    fn bot_posts_drop_under_default_allow_bots() {
        let mut dedup = EventDedup::new();
        let bot_event = json!({
            "type": "event_callback", "team_id": "T1",
            "event": { "type": "message", "channel": "C1", "user": "U1", "text": "echo",
                       "ts": "1.1", "bot_id": "B0" },
        });
        assert_eq!(parse_envelope(&bot_event, &mut dedup), SlackEvent::Ignored);
        let bot_profile = json!({
            "type": "event_callback", "team_id": "T1",
            "event": { "type": "message", "channel": "C1", "user": "U1", "text": "echo",
                       "ts": "1.2", "bot_profile": { "name": "bot" } },
        });
        assert_eq!(
            parse_envelope(&bot_profile, &mut dedup),
            SlackEvent::Ignored
        );
    }

    #[test]
    fn housekeeping_subtypes_drop_but_file_share_passes() {
        let mut dedup = EventDedup::new();
        for (subtype, passes) in [
            ("channel_join", false),
            ("message_deleted", false),
            ("topic_changed", false),
            ("file_comment", false), // deliberately dropped (triage on #110778)
            ("file_share", true),
            ("thread_broadcast", true),
            ("me_message", true),
        ] {
            let env = json!({
                "type": "event_callback", "team_id": "T1",
                "event": { "type": "message", "channel": "C1", "user": "U1", "text": "x",
                           "ts": format!("1.{}", subtype), "subtype": subtype },
            });
            let out = parse_envelope(&env, &mut dedup);
            assert_eq!(
                matches!(out, SlackEvent::Inbound(_)),
                passes,
                "subtype {}: {:?}",
                subtype,
                out
            );
        }
    }

    #[test]
    fn message_changed_edits_normalize() {
        let mut dedup = EventDedup::new();
        let env = json!({
            "type": "event_callback", "team_id": "T1",
            "event": {
                "type": "message", "subtype": "message_changed", "channel": "C1", "ts": "1.5",
                "message": { "type": "message", "channel": "C1", "user": "U1",
                             "text": "edited with <@U0>", "ts": "1.4" },
            },
        });
        let msg = inbound_of(parse_envelope(&env, &mut dedup));
        assert_eq!(msg.text, "edited with <@U0>");
        // The rewritten message's own ts dedups.
        assert_eq!(parse_envelope(&env, &mut dedup), SlackEvent::Ignored);
    }

    #[test]
    fn non_message_lanes_are_ignored_or_unknown() {
        let mut dedup = EventDedup::new();
        assert_eq!(
            parse_envelope(&json!({ "type": "hello" }), &mut dedup),
            SlackEvent::Ignored
        );
        assert_eq!(
            parse_envelope(&json!({ "type": "interactive", "actions": [] }), &mut dedup),
            SlackEvent::Ignored
        );
        assert_eq!(
            parse_envelope(&json!({ "type": "mystery" }), &mut dedup),
            SlackEvent::Unknown
        );
    }
}
