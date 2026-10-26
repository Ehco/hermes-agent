//! annotate_preview — persistent element annotations in the desktop GUI's
//! in-app browser (port of `tools/annotate_preview_tool.py`).
//!
//! Unlike `drive_preview`'s self-retiring marks, an annotation outlines an
//! element (or, with `hold`, the whole visible field) until removed.
//! Annotations bind to elements, not coordinates: they ride scrolls and vanish
//! with their element, so navigation clears them. Same `preview.act` bridge as
//! `drive_preview` (the renderer resolves refs and owns the overlay).

use serde_json::{json, Value};

use crate::desktop_bridge::{passthrough_json, DesktopUi};
use crate::util::tool_error;

pub const ACTIONS: &[&str] = &["add", "hold", "remove", "clear"];

/// Renderer verbs keyed by ours; `clear` is `unpin` with no target = "all of
/// them".
pub const WIRE: &[(&str, &str)] = &[
    ("add", "pin"),
    ("hold", "hold"),
    ("remove", "unpin"),
    ("clear", "unpin"),
];

const NOT_DESKTOP: &str = "annotate_preview is only available in the Hermes desktop app.";
const NO_PAGE: &str = "No GUI window answered with a page: no preview tab is open. \
     Open a page with open_preview first. If the pane IS open, the desktop app \
     may be older than this backend — its bridge-unavailable error names that case.";

pub fn wire_verb(action: &str) -> Option<&'static str> {
    WIRE.iter()
        .find(|(ours, _)| *ours == action)
        .map(|(_, wire)| *wire)
}

/// Execute `annotate_preview` — the registry entry point (no desktop bridge).
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
        .unwrap_or("add")
        .trim()
        .to_lowercase();
    let Some(wire) = wire_verb(&verb) else {
        return tool_error(format!("action must be one of: {}.", ACTIONS.join(", ")));
    };
    let str_arg = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let (ref_, selector, label) = (str_arg("ref"), str_arg("selector"), str_arg("label"));
    if (verb == "add" || verb == "remove") && ref_.is_none() && selector.is_none() {
        return tool_error(format!(
            "{verb} needs a ref from drive_preview action='elements' (e.g. 'btn-sign-in') or a CSS selector."
        ));
    }
    let targeted = verb != "clear" && verb != "hold";
    let mut payload = json!({ "action": wire });
    if targeted {
        if let Some(r) = ref_ {
            payload["ref"] = json!(r);
        }
        if let Some(s) = selector {
            payload["selector"] = json!(s);
        }
    }
    if let Some(label) = label {
        payload["text"] = json!(label);
    }
    match bridge.call("preview.act.request", &payload) {
        Err(e) => tool_error(format!("Failed to annotate the in-app browser: {e}")),
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
    fn wire_mapping_matches_the_renderer() {
        for (ours, wire) in WIRE {
            assert_eq!(wire_verb(ours), Some(*wire));
        }
        assert_eq!(wire_verb("pin"), None);
    }

    #[test]
    fn no_bridge_is_desktop_only() {
        assert_eq!(
            execute(&json!({"action": "add"})),
            format!(r#"{{"error":"{NOT_DESKTOP}"}}"#)
        );
    }

    #[test]
    fn add_and_remove_need_a_target_clear_and_hold_do_not() {
        let stub = StubBridge::new(vec![]);
        let out = execute_with(Some(&stub), &json!({"action": "add"}));
        assert_eq!(
            out,
            r#"{"error":"add needs a ref from drive_preview action='elements' (e.g. 'btn-sign-in') or a CSS selector."}"#
        );
        let out = execute_with(Some(&stub), &json!({"action": "remove"}));
        assert!(out.contains("remove needs a ref"), "{out}");
        assert!(stub.events.lock().unwrap().is_empty());
        // hold / clear go straight through (hold needs no target either).
        let stub = StubBridge::new(vec![Ok(String::new()), Ok(String::new())]);
        let out = execute_with(Some(&stub), &json!({"action": "hold"}));
        assert!(out.contains("No GUI window answered"), "{out}");
    }

    #[test]
    fn payloads_map_to_renderer_verbs() {
        let stub = StubBridge::new(vec![
            Ok(r#"{"pinned":1}"#.into()),
            Ok(r#"{"pinned":2}"#.into()),
        ]);
        execute_with(
            Some(&stub),
            &json!({"action": "add", "ref": "btn-sign-in", "label": "cheapest"}),
        );
        execute_with(Some(&stub), &json!({"action": "clear"}));
        let events = stub.events.lock().unwrap();
        assert_eq!(
            events[0].1,
            json!({"action": "pin", "ref": "btn-sign-in", "text": "cheapest"})
        );
        assert_eq!(events[1].1, json!({"action": "unpin"}));
    }

    #[test]
    fn selector_fallback_and_unknown_action() {
        let stub = StubBridge::new(vec![Ok(r#"{"ok":1}"#.into())]);
        let out = execute_with(
            Some(&stub),
            &json!({"action": "remove", "selector": ".badge"}),
        );
        assert_eq!(out, r#"{"ok":1}"#);
        let (event, payload) = stub.events.lock().unwrap()[0].clone();
        assert_eq!(event, "preview.act.request");
        assert_eq!(payload, json!({"action": "unpin", "selector": ".badge"}));
        let out = execute_with(Some(&stub), &json!({"action": "outline"}));
        assert_eq!(
            out,
            r#"{"error":"action must be one of: add, hold, remove, clear."}"#
        );
    }

    #[test]
    fn empty_answer_and_transport_failure() {
        let out = execute_with(
            Some(&StubBridge::new(vec![Ok(String::new())])),
            &json!({"action": "add", "ref": "r1"}),
        );
        assert!(out.contains("No GUI window answered with a page"), "{out}");
        let out = execute_with(
            Some(&StubBridge::new(vec![Err("renderer gone".into())])),
            &json!({"action": "hold"}),
        );
        assert_eq!(
            out,
            r#"{"error":"Failed to annotate the in-app browser: renderer gone"}"#
        );
    }
}
