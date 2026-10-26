//! session_search — recall past conversation turns. Port of
//! `tools/session_search_tool.py`'s contract, adapted to the Rust session
//! store: the Python reads a multi-session SQLite/FTS5 state.db; here the
//! store is the JSONL transcript the session crate persists (one Message per
//! line), so the four shapes collapse to three:
//!
//! - `query` (discovery): keyword search over message text; the top result is
//!   fully hydrated, lower results carry a snippet; `limit` clamps to [1, 10]
//!   (default 3); `role_filter` is a comma list defaulting to "user,assistant".
//! - `around_message_id` (scroll): a window of messages around a 0-based
//!   message ordinal, `window` clamps to [1, 20] (default 5); the boundary
//!   hint matches the Python's scroll contract.
//! - no args (browse): a session summary (totals, role counts, recent tail).
//!
//! The store path is `$HERMES_SESSION_FILE`, else `$HERMES_HOME/session.jsonl`
//! (HERMES_HOME env, else `~/.hermes`). A missing store is an empty session,
//! not an error — first calls in a fresh profile must succeed.

use serde_json::{json, Value};
use std::path::PathBuf;

use hermes_core::model::{Content, Message, Role};

/// The default role filter for discovery (tool output is usually noise).
const DEFAULT_ROLE_FILTER: &str = "user,assistant";
const DISCOVERY_LIMIT_DEFAULT: usize = 3;
const DISCOVERY_LIMIT_CAP: usize = 10;
const SCROLL_WINDOW_DEFAULT: usize = 5;
const SCROLL_WINDOW_CAP: usize = 20;
const SNIPPET_CHARS: usize = 200;

pub fn session_file() -> PathBuf {
    if let Ok(path) = std::env::var("HERMES_SESSION_FILE") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    hermes_home().join("session.jsonl")
}

fn hermes_home() -> PathBuf {
    std::env::var("HERMES_HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".hermes")
        })
}

/// One message flattened to searchable `(role, text)`; tool calls/results
/// become "tool"/"tool_result"-shaped text the role filter can select.
fn flatten(message: &Message) -> (String, String) {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut parts: Vec<String> = Vec::new();
    for content in &message.content {
        match content {
            Content::Text { text } => parts.push(text.clone()),
            Content::ToolUse { call } => parts.push(format!("[tool_use {}] {}", call.name, call.arguments)),
            Content::ToolResult { call_id, output } => {
                parts.push(format!("[tool_result {call_id}] {output}"))
            }
        }
    }
    (role.to_string(), parts.join("\n"))
}

fn load_messages(path: &PathBuf) -> Vec<(String, String)> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<Message>(line).ok())
            .map(|m| flatten(&m))
            .collect(),
        Err(_) => Vec::new(), // missing store == empty session
    }
}

fn clamp(value: Option<i64>, default: usize, low: usize, high: usize) -> usize {
    let value = value.unwrap_or(default as i64);
    if value < low as i64 {
        low
    } else {
        (value as usize).min(high)
    }
}

fn snippet(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > SNIPPET_CHARS {
        let head: String = collapsed.chars().take(SNIPPET_CHARS).collect();
        format!("{head}…")
    } else {
        collapsed
    }
}

fn parse_role_filter(args: &Value) -> Vec<String> {
    args.get("role_filter")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_ROLE_FILTER)
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Execute `session_search` — the registry entry point (JSON string in, out).
pub fn execute(args: &Value) -> String {
    execute_in(&session_file(), args)
}

/// `execute` against an explicit store path (tests, embedding hosts).
pub fn execute_in(path: &PathBuf, args: &Value) -> String {
    let messages = load_messages(path);
    let around = args.get("around_message_id").and_then(Value::as_i64);
    let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim().to_string();

    // Scroll wins when an anchor is set (the Python's `_dispatch` order).
    if let Some(anchor) = around {
        return scroll(&messages, anchor, args);
    }
    if !query.is_empty() {
        return discover(&messages, &query, args);
    }
    browse(&messages)
}

fn browse(messages: &[(String, String)]) -> String {
    let mut role_counts = std::collections::BTreeMap::new();
    for (role, _) in messages {
        *role_counts.entry(role.clone()).or_insert(0u64) += 1;
    }
    let recent: Vec<Value> = messages
        .iter()
        .enumerate()
        .skip(messages.len().saturating_sub(5))
        .map(|(index, (role, text))| {
            json!({ "match_message_id": index, "role": role, "snippet": snippet(text) })
        })
        .collect();
    json!({
        "success": true,
        "mode": "browse",
        "total_messages": messages.len(),
        "role_counts": role_counts,
        "recent": recent,
        "hint": "Pass 'query' to search past conversation text, or 'around_message_id' to page through the transcript.",
    })
    .to_string()
}

fn discover(messages: &[(String, String)], query: &str, args: &Value) -> String {
    let roles = parse_role_filter(args);
    let limit = clamp(args.get("limit").and_then(Value::as_i64), DISCOVERY_LIMIT_DEFAULT, 1, DISCOVERY_LIMIT_CAP);
    let query_lower = query.to_lowercase();
    let matches: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, (role, text))| {
            (roles.is_empty() || roles.iter().any(|r| r == role)) && text.to_lowercase().contains(&query_lower)
        })
        .map(|(index, _)| index)
        .collect();
    let total = matches.len();
    // The top result is fully hydrated; lower ones carry only the anchor
    // message (the Python's 'adaptive' detail).
    let results = hydrate_top(messages, &matches, limit);
    json!({
        "success": true,
        "mode": "discover",
        "query": query,
        "results": results,
        "total_matches": total,
        "total_messages": messages.len(),
        "limit": limit,
        "hint": if total > results.len() {
            "More matches exist: raise 'limit' (max 10) or narrow the query."
        } else {
            ""
        },
    })
    .to_string()
}

/// Build the discovery rows with the top-ranked result fully hydrated.
fn hydrate_top(messages: &[(String, String)], matches: &[usize], limit: usize) -> Vec<Value> {
    matches
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, &index)| {
            let (role, text) = &messages[index];
            json!({
                "match_message_id": index,
                "role": role,
                "message": if i == 0 { Value::from(text.clone()) } else { Value::Null },
                "snippet": snippet(text),
            })
        })
        .collect()
}

fn scroll(messages: &[(String, String)], anchor: i64, args: &Value) -> String {
    let window = clamp(args.get("window").and_then(Value::as_i64), SCROLL_WINDOW_DEFAULT, 1, SCROLL_WINDOW_CAP);
    if anchor < 0 || anchor as usize >= messages.len() {
        return crate::util::tool_error(&format!(
            "around_message_id {anchor} not in session (valid ids: 0..{}).",
            messages.len().saturating_sub(1)
        ));
    }
    let anchor = anchor as usize;
    let start = anchor.saturating_sub(window);
    let end = (anchor + window).min(messages.len().saturating_sub(1));
    let window_messages: Vec<Value> = (start..=end)
        .map(|index| {
            let (role, text) = &messages[index];
            json!({ "match_message_id": index, "role": role, "message": text })
        })
        .collect();
    json!({
        "success": true,
        "mode": "scroll",
        "around_message_id": anchor,
        "window": window,
        "messages": window_messages,
        "messages_before": start,
        "messages_after": messages.len() - 1 - end,
        "hint": "Scroll forward: re-call with around_message_id = the LAST message's id; backward: the FIRST message's id \
                 (the boundary message repeats as an orientation marker). messages_before/messages_after < window means \
                 you've hit that end of the session.",
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::model::Message;
    use serde_json::json;
    use std::fs;

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    fn tmp_store(lines: &[Message]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "hermes-tools-sess-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        ));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = lines.iter().map(|m| serde_json::to_string(m).unwrap() + "\n").collect();
        fs::write(&path, body).unwrap();
        path
    }

    fn transcript() -> Vec<Message> {
        vec![
            Message::text(Role::User, "we picked cargo ship for the release pipeline"),
            Message::text(Role::Assistant, "Noted. I'll use cargo ship for releases."),
            Message::text(Role::User, "also the deploy target is staging"),
            Message::text(Role::Assistant, "Understood: deploys go to staging."),
            Message::text(Role::User, "what did we decide about the pipeline?"),
        ]
    }

    #[test]
    fn discovery_finds_matches_with_hydrated_top_result() {
        let path = tmp_store(&transcript());
        let out = v(&execute_in(&path, &json!({"query": "pipeline"})));
        assert_eq!(out["mode"], "discover");
        assert_eq!(out["total_matches"], 2);
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["match_message_id"], 0);
        assert!(results[0]["message"].as_str().unwrap().contains("cargo ship"), "{}", out);
        assert!(results[1].get("message").map_or(true, |m| m.is_null()));
        assert!(results[1]["snippet"].as_str().unwrap().len() > 0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn limit_clamps_and_role_filter_selects() {
        let path = tmp_store(&transcript());
        let out = v(&execute_in(&path, &json!({"query": "the", "limit": 99})));
        assert_eq!(out["limit"], DISCOVERY_LIMIT_CAP);

        // Default filter is user,assistant — tool output is usually noise.
        let path = tmp_store(&transcript());
        let out = v(&execute_in(&path, &json!({"query": "tool_result", "role_filter": "tool"})));
        assert_eq!(out["total_matches"], 0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn scroll_windows_clamp_and_repeat_the_boundary() {
        let path = tmp_store(&transcript());
        let out = v(&execute_in(&path, &json!({"around_message_id": 1, "window": 2})));
        assert_eq!(out["mode"], "scroll");
        let ids: Vec<i64> = out["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["match_message_id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![0, 1, 2, 3]); // anchor 1 ± 2, clamped at the head
        assert_eq!(out["messages_before"], 0);
        assert_eq!(out["messages_after"], 1);

        // window=0 clamps to 1; the anchor is always included.
        let out = v(&execute_in(&path, &json!({"around_message_id": 3, "window": 0})));
        let ids: Vec<i64> = out["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["match_message_id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![2, 3, 4]);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn out_of_range_anchor_is_an_error() {
        let path = tmp_store(&transcript());
        let out = v(&execute_in(&path, &json!({"around_message_id": 99})));
        assert!(out["error"].as_str().unwrap().contains("around_message_id 99 not in session"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn no_args_browse_summarizes_and_missing_store_is_empty() {
        let path = tmp_store(&transcript());
        let out = v(&execute_in(&path, &json!({})));
        assert_eq!(out["mode"], "browse");
        assert_eq!(out["total_messages"], 5);
        assert_eq!(out["role_counts"]["user"], 3);
        assert_eq!(out["role_counts"]["assistant"], 2);
        assert_eq!(out["recent"].as_array().unwrap().len(), 5);

        let path = std::env::temp_dir().join("hermes-tools-sess-missing.jsonl");
        let _ = fs::remove_file(&path);
        let out = v(&execute_in(&path, &json!({})));
        assert_eq!(out["total_messages"], 0);
        let out = v(&execute_in(&path, &json!({"query": "anything"})));
        assert_eq!(out["total_matches"], 0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn tool_use_and_result_flatten_to_searchable_text() {
        let path = tmp_store(&transcript());
        let out = v(&execute_in(&path, &json!({"query": "pipeline", "role_filter": "user"})));
        assert_eq!(out["results"].as_array().unwrap()[0]["role"], "user");
        let _ = fs::remove_file(&path);
    }
}
