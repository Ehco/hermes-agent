//! Discord streaming edit-in-place — port of the streaming slice of
//! `plugins/platforms/discord/adapter.py::edit_message` (the message-create +
//! periodic-edit flow the Python stream consumer drives).
//!
//! What ports (the pure edit-decision logic; the actual PATCH goes through
//! `PATCH /channels/{id}/messages/{message_id}` with `{"content": …}`):
//! - mid-stream (`finalize=false`) edits keep editing the ORIGINAL message
//!   with a truncated preview when the content oversizes — splitting
//!   mid-stream would move the edit target to a continuation and the next
//!   accumulated-token tick would re-split, looping forever (the Telegram
//!   #48648 lesson),
//! - once an oversized streaming edit saturates at the 2000-char preview cap,
//!   every subsequent progressive edit truncates to the SAME text; re-sending
//!   it is a no-op that still burns Discord's edit rate limit, so the
//!   saturated preview is deduped per (chat_id, message_id) until the content
//!   changes or shrinks under the cap (mirrors the Telegram #58563 fix),
//! - `finalize=true` delivers oversized content split-and-deliver: chunk 1
//!   edits the original, chunks 2..N send as replies to the previous
//!   (`_edit_overflow_split`; the reactive 50035 path reuses the same split),
//! - saturation state clears on finalize and when content shrinks under the
//!   cap, so dedup can never mask a real edit.
//!
//! What stays deferred: the fresh-final/preference hooks and the flood/egress
//! classification live in the Python stream consumer
//! (`gateway/stream_consumer_transport.py`) — that consumer is the host
//! process; this module only prices the per-tick edit decision.

use crate::chunking::truncate_message;
use crate::discord::format::{cap_split_chunks, format_message, MAX_MESSAGE_LENGTH};
use std::collections::HashMap;

/// One planned edit tick, mirroring `edit_message`'s pre-flight branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEdit {
    /// Saturated-preview dedupe: the edit is a visual no-op; skip silently
    /// (it would still count against the edit rate limit).
    Skip,
    /// `PATCH` the message with this content — the full text when under the
    /// cap, or the truncated preview of a saturated mid-stream edit.
    InPlace { content: String },
    /// Oversized final: edit chunk 1, send chunks 2..N as replies
    /// (`_edit_overflow_split`). All chunks respect the 2000 cap.
    FinalSplit { chunks: Vec<String> },
}

fn char_len(s: &str) -> i64 {
    s.chars().count() as i64
}

/// `_last_overflow_preview` — last truncated mid-stream preview per
/// (chat_id, message_id).
#[derive(Debug, Default)]
pub struct StreamingEditor {
    last_overflow_preview: HashMap<(String, String), String>,
}

impl StreamingEditor {
    pub fn new() -> Self {
        StreamingEditor::default()
    }

    /// `edit_message`'s pre-flight decision (format → oversize branches).
    /// `finalize` clears the saturation state first — "saturation state is
    /// finished; the final edit delivers full content".
    pub fn plan_edit(
        &mut self,
        chat_id: &str,
        message_id: &str,
        content: &str,
        finalize: bool,
    ) -> StreamEdit {
        let formatted = format_message(content);
        let key = (chat_id.to_string(), message_id.to_string());
        if finalize {
            self.last_overflow_preview.remove(&key);
        }
        if (formatted.chars().count() as i64) > MAX_MESSAGE_LENGTH {
            if finalize {
                return StreamEdit::FinalSplit {
                    chunks: cap_split_chunks(truncate_message(&formatted, MAX_MESSAGE_LENGTH, char_len)),
                };
            }
            // Mid-stream: truncate in place, never split (splitting would move
            // the edit target to a continuation).
            let truncated = truncate_message(&formatted, MAX_MESSAGE_LENGTH, char_len)
                .into_iter()
                .next()
                .unwrap_or_default();
            if self.last_overflow_preview.get(&key) == Some(&truncated) {
                return StreamEdit::Skip;
            }
            return StreamEdit::InPlace { content: truncated };
        }
        // Content shrank under the cap: clear saturation so dedup can't mask
        // a real edit.
        if !finalize {
            self.last_overflow_preview.remove(&key);
        }
        StreamEdit::InPlace {
            content: formatted,
        }
    }

    /// Record the saturated preview after a successful in-place edit
    /// (the Python `_last_overflow_preview[_preview_key] = formatted`).
    pub fn record_saturated(&mut self, chat_id: &str, message_id: &str, preview: String) {
        self.last_overflow_preview
            .insert((chat_id.to_string(), message_id.to_string()), preview);
    }

    /// The reactive 50035 path: `format_message` inflation pushed a
    /// mid-stream edit over the cap even after pre-flight. Returns the
    /// truncated preview, or `None` when the saturated-preview dedupe masks
    /// it (the caller skips the PATCH, as the Python `return SendResult(…)`
    /// early-out does).
    pub fn reactive_truncate(
        &self,
        chat_id: &str,
        message_id: &str,
        content: &str,
    ) -> Option<String> {
        let formatted = format_message(content);
        let truncated = truncate_message(&formatted, MAX_MESSAGE_LENGTH, char_len)
            .into_iter()
            .next()
            .unwrap_or_default();
        if self
            .last_overflow_preview
            .get(&(chat_id.to_string(), message_id.to_string()))
            == Some(&truncated)
        {
            None
        } else {
            Some(truncated)
        }
    }
}

/// The PATCH route + payload for one planned edit (`msg.edit(content=…)`;
/// `channel.get_partial_message(id).edit`).
pub fn edit_route(base: &str, channel_id: &str, message_id: &str) -> String {
    format!("{base}/channels/{channel_id}/messages/{message_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oversize_text() -> String {
        "x".repeat((MAX_MESSAGE_LENGTH * 3) as usize)
    }

    #[test]
    fn short_content_edits_in_place_with_no_state() {
        let mut editor = StreamingEditor::new();
        let edit = editor.plan_edit("c1", "m1", "hello", false);
        assert_eq!(edit, StreamEdit::InPlace { content: "hello".into() });
        // Nothing saturated: finalize clears nothing, reactive returns full.
        assert!(editor.reactive_truncate("c1", "m1", "hello").is_some());
    }

    #[test]
    fn mid_stream_oversize_truncates_in_place_then_skips() {
        let mut editor = StreamingEditor::new();
        let long = oversize_text();
        match editor.plan_edit("c1", "m1", &long, false) {
            StreamEdit::InPlace { content } => {
                assert!((content.chars().count() as i64) <= MAX_MESSAGE_LENGTH);
                editor.record_saturated("c1", "m1", content);
            }
            other => panic!("expected InPlace, got {other:?}"),
        }
        // The next tick truncates to the SAME text → skipped (no rate-limit burn).
        assert_eq!(editor.plan_edit("c1", "m1", &long, false), StreamEdit::Skip);
    }

    #[test]
    fn shrunk_content_clears_saturation_so_dedup_never_masks() {
        let mut editor = StreamingEditor::new();
        let long = oversize_text();
        if let StreamEdit::InPlace { content } = editor.plan_edit("c1", "m1", &long, false) {
            editor.record_saturated("c1", "m1", content);
        }
        // Growth that leaves the truncated PREFIX unchanged (same first 2000
        // chars) stays a Skip — the Python dedupe is prefix-stable.
        let grown = format!("{long} more tail text");
        assert_eq!(editor.plan_edit("c1", "m1", &grown, false), StreamEdit::Skip);
        // …and shrinking under the cap clears the state entirely.
        let edit = editor.plan_edit("c1", "m1", "short now", false);
        assert_eq!(edit, StreamEdit::InPlace { content: "short now".into() });
        // After the shrink the saturation record is gone: a fresh oversize
        // produces an InPlace preview again, not a Skip.
        let fresh = "z".repeat((MAX_MESSAGE_LENGTH * 3) as usize);
        assert!(matches!(
            editor.plan_edit("c1", "m1", &fresh, false),
            StreamEdit::InPlace { .. }
        ));
    }

    #[test]
    fn finalize_oversize_splits_and_clears_state() {
        let mut editor = StreamingEditor::new();
        let long = oversize_text();
        if let StreamEdit::InPlace { content } = editor.plan_edit("c1", "m1", &long, false) {
            editor.record_saturated("c1", "m1", content);
        }
        match editor.plan_edit("c1", "m1", &long, true) {
            StreamEdit::FinalSplit { chunks } => {
                assert!(chunks.len() > 1);
                for chunk in &chunks {
                    assert!((chunk.chars().count() as i64) <= MAX_MESSAGE_LENGTH);
                }
            }
            other => panic!("expected FinalSplit, got {other:?}"),
        }
        // State finished: a later identical mid-stream edit is InPlace again.
        assert!(!matches!(editor.plan_edit("c1", "m1", &long, false), StreamEdit::Skip));
    }

    #[test]
    fn finalize_under_cap_delivers_full_content() {
        let mut editor = StreamingEditor::new();
        let long = "y".repeat(1900);
        let edit = editor.plan_edit("c1", "m1", &long, true);
        assert_eq!(edit, StreamEdit::InPlace { content: long });
    }

    #[test]
    fn reactive_truncate_dedupes_saturated_previews() {
        let mut editor = StreamingEditor::new();
        let long = oversize_text();
        // First sighting: Some(truncated) — the caller PATCHes and records.
        let preview = editor.reactive_truncate("c1", "m1", &long).unwrap();
        editor.record_saturated("c1", "m1", preview);
        // Second sighting of the same text: masked → skip the PATCH.
        assert!(editor.reactive_truncate("c1", "m1", &long).is_none());
    }

    #[test]
    fn edit_route_points_at_the_message() {
        assert_eq!(
            edit_route("https://discord.com/api/v10", "42", "77"),
            "https://discord.com/api/v10/channels/42/messages/77"
        );
    }
}
