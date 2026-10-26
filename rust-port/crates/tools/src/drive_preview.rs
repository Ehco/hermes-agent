//! drive_preview — interact with the in-app browser / preview pane in the
//! desktop GUI: click, type, scroll (port of `tools/drive_preview_tool.py`).
//!
//! Elements are addressed by legible refs from `action="elements"`
//! (`btn-sign-in`); the renderer resolves refs and answers with a delta.
//! Round-trips through the desktop bridge (`preview.act.request` /
//! `preview.act.respond`); this module is schema + a thin dispatcher over the
//! injected bridge.

use serde_json::{json, Value};

use crate::desktop_bridge::{passthrough_json, DesktopUi};
use crate::util::tool_error;

pub const ACTIONS: &[&str] = &[
    "elements", "click", "hover", "type", "scroll", "press", "strobe", "back", "forward", "reload",
];
pub const SCROLL_TO: &[&str] = &["top", "bottom"];

/// Verbs that need something to act on — a ref from the last inventory, or a
/// raw CSS selector. `scroll` is deliberately absent: bare, it scrolls the page.
const NEEDS_TARGET: &[&str] = &["click", "hover", "type", "press"];

const NOT_DESKTOP: &str = "drive_preview is only available in the Hermes desktop app.";
const NO_PAGE: &str = "No GUI window answered with a page: no preview tab is open. \
     Open a page with open_preview first. If the pane IS open, the desktop app \
     may be older than this backend — its bridge-unavailable error names that case.";

fn as_i64(value: Option<&Value>) -> Result<Option<i64>, ()> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .map(Some)
            .unwrap_or(None)),
        Some(Value::String(s)) if s.trim().parse::<i64>().is_ok() => {
            Ok(Some(s.trim().parse::<i64>().unwrap()))
        }
        _ => Err(()),
    }
}

/// Execute `drive_preview` — the registry entry point (no desktop bridge).
pub fn execute(args: &Value) -> String {
    execute_with(None, args)
}

/// `execute` against an injected bridge (desktop hosts, tests).
pub fn execute_with(bridge: Option<&dyn DesktopUi>, args: &Value) -> String {
    let Some(bridge) = bridge else {
        return tool_error(NOT_DESKTOP);
    };
    let verb = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if !ACTIONS.contains(&verb.as_str()) {
        return tool_error(format!("action must be one of: {}.", ACTIONS.join(", ")));
    }
    let str_arg = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let (ref_, selector, text, key) = (
        str_arg("ref"),
        str_arg("selector"),
        str_arg("text"),
        str_arg("key"),
    );
    let submit = args.get("submit").and_then(Value::as_bool);
    let full = args.get("full").and_then(Value::as_bool);
    let allow_shortcut = args.get("allow_shortcut").and_then(Value::as_bool);
    if NEEDS_TARGET.contains(&verb.as_str()) && ref_.is_none() && selector.is_none() {
        return tool_error(format!(
            "{verb} needs a ref from action='elements' (e.g. 'btn-sign-in') or a CSS selector."
        ));
    }
    if verb == "type" && text.is_none() {
        return tool_error("type needs the text to enter.");
    }
    if verb == "press" && key.is_none() {
        return tool_error("press needs a key, e.g. 'Enter' or 'Escape'.");
    }
    if let Some(to) = args
        .get("to")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        if !SCROLL_TO.contains(&to) {
            return tool_error(format!("to must be one of: {}.", SCROLL_TO.join(", ")));
        }
    }
    let (amount, max) = match (as_i64(args.get("amount")), as_i64(args.get("max"))) {
        (Ok(amount), Ok(max)) => (amount, max),
        (Err(_) | Ok(_), Err(_)) | (Err(_), _) => {
            return tool_error("amount and max must be integers.")
        }
    };
    let mut payload = json!({ "action": verb });
    let insert = |payload: &mut Value, key: &str, value: Option<Value>| {
        if let Some(value) = value {
            payload
                .as_object_mut()
                .unwrap()
                .insert(key.to_string(), value);
        }
    };
    for (field, value) in [
        ("ref", ref_),
        ("selector", selector),
        ("text", text),
        ("key", key),
        (
            "to",
            args.get("to")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty()),
        ),
    ] {
        insert(&mut payload, field, value.map(|v| json!(v)));
    }
    for (field, value) in [
        ("submit", submit),
        ("full", full),
        ("allow_shortcut", allow_shortcut),
    ] {
        insert(&mut payload, field, value.map(|v| json!(v)));
    }
    insert(&mut payload, "amount", amount.map(|v| json!(v)));
    insert(&mut payload, "max", max.map(|v| json!(v)));
    match bridge.call("preview.act.request", &payload) {
        Err(e) => tool_error(format!("Failed to act on the in-app browser: {e}")),
        Ok(raw) => {
            if raw.is_empty() {
                tool_error(NO_PAGE)
            } else {
                passthrough_json(&raw)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_bridge::tests::StubBridge;
    use serde_json::json;

    #[test]
    fn no_bridge_is_desktop_only() {
        assert_eq!(
            execute(&json!({"action": "elements"})),
            format!(r#"{{"error":"{NOT_DESKTOP}"}}"#)
        );
    }

    #[test]
    fn validation_holds_before_the_bridge() {
        let stub = StubBridge::new(vec![]);
        let out = execute_with(Some(&stub), &json!({"action": "teleport"}));
        assert_eq!(
            out,
            r#"{"error":"action must be one of: elements, click, hover, type, scroll, press, strobe, back, forward, reload."}"#
        );
        let out = execute_with(Some(&stub), &json!({"action": "click"}));
        assert_eq!(
            out,
            r#"{"error":"click needs a ref from action='elements' (e.g. 'btn-sign-in') or a CSS selector."}"#
        );
        let out = execute_with(Some(&stub), &json!({"action": "type", "ref": "user"}));
        assert_eq!(out, r#"{"error":"type needs the text to enter."}"#);
        let out = execute_with(Some(&stub), &json!({"action": "press", "ref": "user"}));
        assert_eq!(
            out,
            r#"{"error":"press needs a key, e.g. 'Enter' or 'Escape'."}"#
        );
        let out = execute_with(Some(&stub), &json!({"action": "scroll", "to": "middle"}));
        assert_eq!(out, r#"{"error":"to must be one of: top, bottom."}"#);
        let out = execute_with(Some(&stub), &json!({"action": "scroll", "amount": "lots"}));
        assert_eq!(out, r#"{"error":"amount and max must be integers."}"#);
        assert!(stub.events.lock().unwrap().is_empty());
    }

    #[test]
    fn elements_payload_maps_max_and_passes_the_answer_through() {
        let stub = StubBridge::new(vec![Ok(
            r#"{"added":[{"ref":"btn-sign-in","role":"button"}],"removed":[]}"#.into(),
        )]);
        let out = execute_with(Some(&stub), &json!({"action": "elements", "max": 20}));
        assert!(out.contains("btn-sign-in"), "{out}");
        let (event, payload) = stub.events.lock().unwrap()[0].clone();
        assert_eq!(event, "preview.act.request");
        assert_eq!(payload, json!({"action": "elements", "max": 20}));
    }

    #[test]
    fn type_click_and_scroll_send_faithful_payloads() {
        let stub = StubBridge::new(vec![
            Ok(r#"{"ok":1}"#.into()),
            Ok(r#"{"ok":2}"#.into()),
            Ok(r#"{"ok":3}"#.into()),
        ]);
        execute_with(
            Some(&stub),
            &json!({"action": "type", "selector": "#user", "text": "  ada  ", "submit": true}),
        );
        execute_with(
            Some(&stub),
            &json!({"action": "press", "ref": "r1", "key": "Enter"}),
        );
        execute_with(
            Some(&stub),
            &json!({"action": "scroll", "amount": -500, "to": "bottom"}),
        );
        let events = stub.events.lock().unwrap();
        assert_eq!(
            events[0].1,
            json!({"action": "type", "selector": "#user", "text": "ada", "submit": true})
        );
        assert_eq!(
            events[1].1,
            json!({"action": "press", "ref": "r1", "key": "Enter"})
        );
        assert_eq!(
            events[2].1,
            json!({"action": "scroll", "amount": -500, "to": "bottom"})
        );
    }

    #[test]
    fn empty_answer_and_transport_failure() {
        let out = execute_with(
            Some(&StubBridge::new(vec![Ok(String::new())])),
            &json!({"action": "elements"}),
        );
        assert!(out.contains("No GUI window answered with a page"), "{out}");
        let out = execute_with(
            Some(&StubBridge::new(vec![Err("bridge down".into())])),
            &json!({"action": "click", "ref": "r1"}),
        );
        assert_eq!(
            out,
            r#"{"error":"Failed to act on the in-app browser: bridge down"}"#
        );
    }
}
