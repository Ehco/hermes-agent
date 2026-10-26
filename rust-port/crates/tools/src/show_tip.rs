//! show_tip — point at something in the desktop GUI with a one-line arrow
//! bubble (port of `tools/tip_tool.py`, the quiet sibling of `tour`).
//!
//! Fire-and-forget: a tip is not a question, so the bridge is not waited on
//! beyond the emit.

use serde_json::{json, Value};

use crate::desktop_bridge::{emit_or_error, DesktopUi};
use crate::util::tool_error;

pub const SIDES: &[&str] = &["top", "right", "bottom", "left"];

/// Execute `show_tip` — the registry entry point (no desktop bridge).
pub fn execute(args: &Value) -> String {
    execute_with(None, args)
}

/// `execute` against an injected bridge (desktop hosts, tests).
pub fn execute_with(bridge: Option<&dyn DesktopUi>, args: &Value) -> String {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let selector = args
        .get("selector")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return tool_error("tip needs text — the one line the bubble says.");
    }
    if selector.is_empty() {
        return tool_error(
            "tip needs a selector to point at. Call tour(action='targets') to see \
             what's on screen and prefer a target reporting stable: true.",
        );
    }
    let side = args
        .get("side")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !side.is_empty() && !SIDES.contains(&side.as_str()) {
        return tool_error(format!("side must be one of: {}.", SIDES.join(", ")));
    }
    let mut payload = json!({ "selector": selector, "text": text });
    if let Some(title) = args
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        payload["title"] = json!(title);
    }
    if !side.is_empty() {
        payload["side"] = json!(side);
    }
    emit_or_error(
        bridge,
        "tip.show",
        &payload,
        "Failed to show the tip: ",
        "tip is only available in the Hermes desktop app.",
        json!({ "success": true, "selector": payload["selector"] }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_bridge::tests::StubBridge;
    use serde_json::json;

    #[test]
    fn text_and_selector_are_required() {
        let stub = StubBridge::new(vec![]);
        let out = execute_with(Some(&stub), &json!({"selector": "#chat-box"}));
        assert_eq!(
            out,
            r#"{"error":"tip needs text — the one line the bubble says."}"#
        );
        let out = execute_with(Some(&stub), &json!({"text": "hi"}));
        assert!(out.contains("tip needs a selector to point at"), "{out}");
        assert!(out.contains("tour(action='targets')"), "{out}");
        let out = execute_with(
            Some(&stub),
            &json!({"text": "hi", "selector": "#x", "side": "north"}),
        );
        assert_eq!(
            out,
            r#"{"error":"side must be one of: top, right, bottom, left."}"#
        );
        assert!(stub.events.lock().unwrap().is_empty());
    }

    #[test]
    fn without_bridge_is_desktop_only() {
        let out = execute(&json!({"text": "hi", "selector": "#x"}));
        assert_eq!(
            out,
            r#"{"error":"tip is only available in the Hermes desktop app."}"#
        );
    }

    #[test]
    fn payload_carries_selector_text_and_optionals() {
        let stub = StubBridge::new(vec![Ok(String::new())]);
        let out = execute_with(
            Some(&stub),
            &json!({"text": "  Search here  ", "selector": " #search ", "title": "Tip", "side": "bottom"}),
        );
        let (event, payload) = stub.events.lock().unwrap()[0].clone();
        assert_eq!(event, "tip.show");
        assert_eq!(
            payload,
            json!({"selector": "#search", "text": "Search here", "title": "Tip", "side": "bottom"})
        );
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, json!({"success": true, "selector": "#search"}));
    }

    #[test]
    fn failure_is_prefixed() {
        let out = execute_with(
            Some(&StubBridge::new(vec![Err("window closed".into())])),
            &json!({"text": "hi", "selector": "#x"}),
        );
        assert_eq!(out, r#"{"error":"Failed to show the tip: window closed"}"#);
    }
}
