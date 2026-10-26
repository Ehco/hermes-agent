//! react_to_message — the agent's emoji reaction in the Hermes desktop app
//! (port of `tools/react_to_message_tool.py`).
//!
//! Counterpart to the user's tapback: same store, one-per-author, written with
//! `author="agent"`. Defaults to the triggering user message; `messages_back`
//! steps to earlier user turns (row ids aren't visible to the model). An empty
//! emoji retracts. The store is injected as the `ReactionStore` trait (the
//! desktop host supplies the profile's SessionDB; CLI execution passes `None`
//! and every path degrades to its faithful error).

use serde_json::{json, Value};

use crate::desktop_bridge::DesktopUi;
use crate::util::tool_error;

/// The durable-session store seam (Python `hermes_state_registry` SessionDB).
pub trait ReactionStore: Send + Sync {
    /// Latest message row id for `role`, `offset` messages back (0 = latest).
    fn latest_message_row_id(&self, session_key: &str, role: &str, offset: usize) -> Option<i64>;
    /// Role of one row in the session (None when not part of the conversation).
    fn get_message_role(&self, session_key: &str, row_id: i64) -> Option<String>;
    /// Set (or with an empty emoji, retract) the agent's reaction; returns the
    /// reaction map, or None when the row is foreign to this conversation.
    fn set_message_reaction(&self, session_key: &str, row_id: i64, emoji: &str) -> Option<Value>;
}

/// Session-key + store + bridge wiring the host supplies per turn. Mirrors the
/// Python kwargs: `session_key` (HERMES_SESSION_ID/KEY), the SessionDB and the
/// desktop bridge that paints `message.reaction` live.
pub struct ReactContext<'a> {
    pub session_key: Option<&'a str>,
    pub store: Option<&'a dyn ReactionStore>,
    pub bridge: Option<&'a dyn DesktopUi>,
}

/// Execute `react_to_message` — the registry entry point (no host context).
pub fn execute(args: &Value) -> String {
    execute_with(
        ReactContext {
            session_key: None,
            store: None,
            bridge: None,
        },
        args,
    )
}

/// `execute` against an injected context (desktop hosts, tests).
pub fn execute_with(ctx: ReactContext, args: &Value) -> String {
    let emoji = args
        .get("emoji")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let session_key = ctx
        .session_key
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string);
    let Some(session_key) = session_key else {
        return tool_error("No active session — reactions need a persisted conversation.");
    };
    let Some(store) = ctx.store else {
        return tool_error("Session storage is unavailable.");
    };
    let row_id = match args.get("message_row_id").and_then(Value::as_i64) {
        Some(row_id) => {
            // Explicit rows may be the agent's own or a user's; the store's
            // set_message_reaction decides whether the row is in this
            // conversation (None => the "not part of this conversation" error).
            Some(row_id)
        }
        None => {
            let back = args
                .get("messages_back")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0) as usize;
            match store.latest_message_row_id(&session_key, "user", back) {
                Some(row_id) => Some(row_id),
                None => {
                    return tool_error(if back == 0 {
                        "No user message to react to yet.".into()
                    } else {
                        format!("No user message found {back} back.")
                    })
                }
            }
        }
    };
    let Some(row_id) = row_id else {
        return tool_error("Message is not part of this conversation.");
    };
    let Some(reactions) = store.set_message_reaction(&session_key, row_id, emoji) else {
        return tool_error(format!(
            "Message {row_id} is not part of this conversation."
        ));
    };
    // Paint it live; a missing bridge (non-desktop) is not an error — the
    // reaction is persisted. `role` lets the renderer match a live message
    // without a durable row id.
    if let Some(bridge) = ctx.bridge {
        let role = store
            .get_message_role(&session_key, row_id)
            .unwrap_or_else(|| "user".into());
        let _ = bridge.call(
            "message.reaction",
            &json!({"row_id": row_id, "reactions": reactions, "role": role}),
        );
    }
    json!({"success": true, "row_id": row_id, "reactions": reactions}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_bridge::tests::StubBridge;
    use serde_json::json;
    use std::sync::Mutex;

    struct StubStore {
        rows: Mutex<Vec<(i64, String)>>, // (row_id, role), oldest first
        reactions: Mutex<std::collections::HashMap<i64, Value>>,
    }

    impl StubStore {
        fn new(rows: &[(i64, &str)]) -> Self {
            Self {
                rows: Mutex::new(rows.iter().map(|(id, r)| (*id, r.to_string())).collect()),
                reactions: Mutex::new(std::collections::HashMap::new()),
            }
        }
    }

    impl ReactionStore for StubStore {
        fn latest_message_row_id(&self, _k: &str, role: &str, offset: usize) -> Option<i64> {
            let rows = self.rows.lock().unwrap();
            rows.iter()
                .rev()
                .filter(|(_, r)| r == role)
                .nth(offset)
                .map(|(id, _)| *id)
        }
        fn get_message_role(&self, _k: &str, row_id: i64) -> Option<String> {
            self.rows
                .lock()
                .unwrap()
                .iter()
                .find(|(id, _)| *id == row_id)
                .map(|(_, r)| r.clone())
        }
        fn set_message_reaction(&self, _k: &str, row_id: i64, emoji: &str) -> Option<Value> {
            if !self
                .rows
                .lock()
                .unwrap()
                .iter()
                .any(|(id, _)| *id == row_id)
            {
                return None;
            }
            let mut map = self.reactions.lock().unwrap();
            let entry = map.entry(row_id).or_insert_with(|| json!({}));
            if emoji.is_empty() {
                entry.as_object_mut().unwrap().remove("agent");
            } else {
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert("agent".into(), json!(emoji));
            }
            Some(entry.clone())
        }
    }

    fn ctx<'a>(store: &'a StubStore, bridge: Option<&'a dyn DesktopUi>) -> ReactContext<'a> {
        ReactContext {
            session_key: Some("sess-1"),
            store: Some(store),
            bridge,
        }
    }

    #[test]
    fn no_session_is_an_error() {
        assert_eq!(
            execute(&json!({"emoji": "👍"})),
            r#"{"error":"No active session — reactions need a persisted conversation."}"#
        );
        let out = execute_with(
            ReactContext {
                session_key: Some("s"),
                store: None,
                bridge: None,
            },
            &json!({"emoji": "👍"}),
        );
        assert_eq!(out, r#"{"error":"Session storage is unavailable."}"#);
    }

    #[test]
    fn defaults_to_the_latest_user_message() {
        let store = StubStore::new(&[(1, "user"), (2, "assistant"), (3, "user")]);
        let out = execute_with(ctx(&store, None), &json!({"emoji": "😂"}));
        assert!(out.contains(r#""row_id":3"#), "{out}");
        assert!(out.contains(r#""agent":"😂""#), "{out}");
    }

    #[test]
    fn messages_back_steps_earlier_user_turns() {
        let store = StubStore::new(&[(1, "user"), (2, "assistant"), (3, "user")]);
        let out = execute_with(
            ctx(&store, None),
            &json!({"emoji": "❤️", "messages_back": 1}),
        );
        assert!(out.contains(r#""row_id":1"#), "{out}");
        let out = execute_with(
            ctx(&store, None),
            &json!({"emoji": "❤️", "messages_back": 7}),
        );
        assert_eq!(out, r#"{"error":"No user message found 7 back."}"#);
        let out = execute_with(ctx(&store, None), &json!({"emoji": ""}));
        // Empty emoji retracts the agent's reaction on the latest user message
        // (the StubStore starts with an empty reactions map).
        assert!(out.contains(r#""row_id":3"#), "{out}");
        assert!(out.contains(r#""reactions":{}"#), "{out}");
    }

    #[test]
    fn explicit_row_id_and_live_paint() {
        let store = StubStore::new(&[(1, "user"), (3, "user")]);
        let bridge = StubBridge::new(vec![]);
        let out = execute_with(
            ctx(&store, Some(&bridge)),
            &json!({"emoji": "👍", "message_row_id": 1}),
        );
        assert!(out.contains(r#""row_id":1"#), "{out}");
        let (event, payload) = bridge.events.lock().unwrap()[0].clone();
        assert_eq!(event, "message.reaction");
        assert_eq!(payload["row_id"], 1);
        assert_eq!(payload["role"], "user");
    }

    #[test]
    fn empty_emoji_retracts_and_foreign_row_errors() {
        let store = StubStore::new(&[(1, "user")]);
        let _ = execute_with(ctx(&store, None), &json!({"emoji": "👍"}));
        let out = execute_with(ctx(&store, None), &json!({"emoji": ""}));
        assert!(out.contains(r#""row_id":1"#), "{out}");
        let out = execute_with(
            ctx(&store, None),
            &json!({"emoji": "👍", "message_row_id": 99}),
        );
        assert_eq!(
            out,
            r#"{"error":"Message 99 is not part of this conversation."}"#
        );
    }
}
