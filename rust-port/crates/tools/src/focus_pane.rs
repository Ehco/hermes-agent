//! focus_pane — reveal/focus a pane in the Hermes desktop GUI (port of
//! `tools/focus_pane_tool.py`, `pane.reveal` via the desktop bridge).
//!
//! The renderer runs each pane's own reveal path and only acts on the active
//! window, so a background turn never moves the user's focus. URLs/files go
//! through `desktop_preview`.

use serde_json::{json, Value};

use crate::desktop_bridge::{emit_or_error, DesktopUi};
use crate::util::tool_error;

pub const PANES: &[&str] = &["chat", "files", "terminal", "review", "sessions"];

/// Execute `focus_pane` — the registry entry point (no desktop bridge).
pub fn execute(args: &Value) -> String {
    execute_with(None, args)
}

/// `execute` against an injected bridge (desktop hosts, tests).
pub fn execute_with(bridge: Option<&dyn DesktopUi>, args: &Value) -> String {
    let name = args
        .get("pane")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if !PANES.contains(&name.as_str()) {
        return tool_error(format!("pane must be one of: {}.", PANES.join(", ")));
    }
    emit_or_error(
        bridge,
        "pane.reveal",
        &json!({ "pane": name }),
        &format!("Failed to focus the {name} pane: "),
        "Pane focus is only available in the Hermes desktop app.",
        json!({ "success": true, "pane": name }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_bridge::tests::StubBridge;
    use serde_json::json;

    #[test]
    fn validates_pane_name_before_any_bridge_call() {
        let stub = StubBridge::new(vec![]);
        let out = execute_with(Some(&stub), &json!({"pane": "git"}));
        assert_eq!(
            out,
            r#"{"error":"pane must be one of: chat, files, terminal, review, sessions."}"#
        );
        let out = execute_with(Some(&stub), &json!({}));
        assert_eq!(
            out,
            r#"{"error":"pane must be one of: chat, files, terminal, review, sessions."}"#
        );
        assert!(stub.events.lock().unwrap().is_empty());
    }

    #[test]
    fn without_bridge_is_desktop_only() {
        let out = execute(&json!({"pane": "review"}));
        assert_eq!(
            out,
            r#"{"error":"Pane focus is only available in the Hermes desktop app."}"#
        );
    }

    #[test]
    fn reveal_sends_the_pane_and_answers_success() {
        let stub = StubBridge::new(vec![Ok(String::new())]);
        let out = execute_with(Some(&stub), &json!({"pane": " Review "}));
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, json!({"success": true, "pane": "review"}));
        let (event, payload) = stub.events.lock().unwrap()[0].clone();
        assert_eq!(event, "pane.reveal");
        assert_eq!(payload, json!({"pane": "review"}));
    }

    #[test]
    fn failure_is_prefixed_with_the_pane() {
        let out = execute_with(
            Some(&StubBridge::new(vec![Err("renderer gone".into())])),
            &json!({"pane": "sessions"}),
        );
        assert_eq!(
            out,
            r#"{"error":"Failed to focus the sessions pane: renderer gone"}"#
        );
    }
}
