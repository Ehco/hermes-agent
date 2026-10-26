//! gui_tour — guided tour (highlight + narrate UI elements) in the Hermes
//! desktop GUI (port of `tools/tour_tool.py`).
//!
//! `start` with no steps runs the app's built-in tour (`preset` quick or
//! full); a custom tour discovers targets (`action="targets"`) then highlights
//! one step at a time (`show`) or hands over a step list the user pages
//! (`start` + steps). Every action round-trips through the desktop bridge
//! (`tour.request`), so the agent learns whether the selector matched. Without
//! a bridge it degrades to its faithful "only available in the Hermes desktop
//! app" error.

use serde_json::{json, Value};

use crate::desktop_bridge::DesktopUi;
use crate::util::tool_error;

pub const ACTIONS: &[&str] = &["targets", "show", "start", "next", "prev", "stop"];
pub const SURFACES: &[&str] = &["app", "preview"];
pub const SIDES: &[&str] = &["top", "right", "bottom", "left"];
pub const PRESETS: &[&str] = &["quick", "full"];

/// Execute `gui_tour` — the registry entry point (no desktop bridge).
pub fn execute(args: &Value) -> String {
    execute_with(None, args)
}

/// `execute` against an injected bridge (desktop hosts, tests).
pub fn execute_with(bridge: Option<&dyn DesktopUi>, args: &Value) -> String {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_lowercase();
    if action.is_empty() {
        // Python reads `action: str = ""` then fails the ACTIONS check with "".
        return tool_error(format!("action must be one of: {}.", ACTIONS.join(", ")));
    }
    if !ACTIONS.contains(&action.as_str()) {
        return tool_error(format!("action must be one of: {}.", ACTIONS.join(", ")));
    }
    let surface = args
        .get("surface")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_else(|| "app".into());
    if !SURFACES.contains(&surface.as_str()) {
        return tool_error(format!("surface must be one of: {}.", SURFACES.join(", ")));
    }
    let side = optional_str(args, "side");
    if let Some(side) = &side {
        if !SIDES.contains(&side.as_str()) {
            return tool_error(format!("side must be one of: {}.", SIDES.join(", ")));
        }
    }
    let preset = optional_str(args, "preset");
    if let Some(preset) = &preset {
        if !PRESETS.contains(&preset.as_str()) {
            return tool_error(format!("preset must be one of: {}.", PRESETS.join(", ")));
        }
    }
    let steps = args.get("steps").cloned();
    if preset.is_some() && steps.is_some() {
        return tool_error("preset picks a built-in tour; pass steps or preset, not both.");
    }
    let selector = optional_str(args, "selector");
    let title = optional_str(args, "title");
    let text = optional_str(args, "text");
    // Every highlighted moment needs something to point at or something to say.
    if action == "show" && selector.is_none() && title.is_none() && text.is_none() {
        return tool_error("show needs a selector (and/or title/text for the popover).");
    }
    // start without steps is the app's built-in tour, which only exists on the app surface.
    if action == "start" && steps.is_none() && surface != "app" {
        return tool_error(
            "The built-in tour runs on surface='app'; pass steps for a preview tour.",
        );
    }
    if action == "start" && steps.is_some() {
        let error = match steps.as_ref() {
            Some(Value::Array(items)) if items.is_empty() => Some(
                "start needs a non-empty steps array, or no steps for the built-in tour.".into(),
            ),
            Some(Value::Array(items)) => items.iter().enumerate().find_map(|(i, step)| {
                let ok = step.as_object().is_some_and(|o| {
                    ["selector", "title", "text"].iter().any(|k| {
                        o.get(*k)
                            .and_then(Value::as_str)
                            .map_or(false, |v| !v.is_empty())
                    })
                });
                if ok {
                    None
                } else {
                    Some(format!("steps[{i}] needs a selector and/or title/text."))
                }
            }),
            _ => Some(
                "start needs a non-empty steps array, or no steps for the built-in tour.".into(),
            ),
        };
        if let Some(err) = error {
            return tool_error(err);
        }
    }
    let payload = json!({
        "action": action,
        "surface": surface,
        "selector": selector,
        "title": title,
        "text": text,
        "side": side,
        "steps": steps,
        "step_index": args.get("step_index").cloned(),
        "preset": preset,
    });
    match bridge {
        None => tool_error("tour is only available in the Hermes desktop app."),
        Some(bridge) => match bridge.call("tour.request", &payload) {
            Err(e) => tool_error(format!("Tour action failed: {e}")),
            Ok(raw) => {
                if raw.is_empty() {
                    return tool_error(
                        "The tour request timed out, or no GUI window answered. \
                         For surface='preview' open a page in the preview pane first.",
                    );
                }
                crate::desktop_bridge::passthrough_json(&raw)
            }
        },
    }
}

fn optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_bridge::tests::StubBridge;
    use serde_json::json;

    #[test]
    fn no_bridge_is_desktop_only() {
        let out = execute(&json!({"action": "targets"}));
        assert_eq!(
            out,
            r#"{"error":"tour is only available in the Hermes desktop app."}"#
        );
    }

    #[test]
    fn validation_precedes_the_bridge() {
        let stub = StubBridge::new(vec![]);
        for (args, expected) in [
            (json!({}), "action must be one of"),
            (json!({"action": "teleport"}), "action must be one of"),
            (
                json!({"action": "show", "surface": "stage"}),
                "surface must be one of",
            ),
            (
                json!({"action": "show", "side": "diagonal"}),
                "side must be one of",
            ),
            (
                json!({"action": "start", "preset": "mega"}),
                "preset must be one of",
            ),
            (
                json!({"action": "start", "preset": "quick", "steps": []}),
                "not both",
            ),
            (json!({"action": "show"}), "show needs a selector"),
            (
                json!({"action": "start", "surface": "preview"}),
                "built-in tour runs on surface='app'",
            ),
            (
                json!({"action": "start", "steps": []}),
                "non-empty steps array",
            ),
            (
                json!({"action": "start", "steps": [{"nobody": "home"}]}),
                "steps[0] needs a selector",
            ),
        ] {
            let out = execute_with(Some(&stub), &args);
            assert!(out.contains(expected), "{args}: {out}");
        }
        assert!(
            stub.events.lock().unwrap().is_empty(),
            "no round-trip on validation failure"
        );
    }

    #[test]
    fn valid_action_round_trips_through_the_bridge() {
        let stub = StubBridge::new(vec![Ok(
            r#"{"targets":[{"ref":"btn-1","stable":true}]}"#.into()
        )]);
        let out = execute_with(Some(&stub), &json!({"action": "targets"}));
        assert!(out.contains("\"targets\""), "{out}");
        let (event, payload) = stub.events.lock().unwrap()[0].clone();
        assert_eq!(event, "tour.request");
        assert_eq!(payload["action"], "targets");
        assert_eq!(payload["surface"], "app");
        assert_eq!(payload["selector"], Value::Null);
    }

    #[test]
    fn timed_out_bridge_is_the_timeout_error() {
        let stub = StubBridge::new(vec![Ok(String::new())]);
        let out = execute_with(Some(&stub), &json!({"action": "show", "selector": "#x"}));
        assert!(out.contains("timed out"), "{out}");
        let stub = StubBridge::new(vec![Err("gone".into())]);
        let out = execute_with(Some(&stub), &json!({"action": "stop"}));
        assert_eq!(out, r#"{"error":"Tour action failed: gone"}"#);
    }
}
