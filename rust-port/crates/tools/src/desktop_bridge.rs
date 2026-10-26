//! Shared desktop-GUI bridge seam for the `desktop_ui` tools.
//!
//! Port of the Python agent's `tools.desktop_ui` seam: tools validate their
//! arguments, then round-trip one event to the desktop renderer (which owns the
//! GUI) and surface its answer. There is no Electron dependency here — the
//! renderer side is injected as the `DesktopUi` trait (tests stub it; CLI
//! execution passes `None` and every tool degrades to its faithful
//! "only available in the Hermes desktop app" error).

use serde_json::{json, Value};

use crate::util::tool_error;

/// One round-trip to the desktop renderer: `event` plus a JSON payload, the
/// renderer's raw JSON answer (Python `desktop_ui.emit` / blocking-prompt
/// bridge). `Err` is the transport failure text.
pub trait DesktopUi: Send + Sync {
    fn call(&self, event: &str, payload: &Value) -> Result<String, String>;
}

/// Python `desktop_ui.emit_or_error`: no bridge -> `desktop_only` error text,
/// transport failure -> `fail_prefix` + cause, success -> `result` as JSON.
pub fn emit_or_error(
    bridge: Option<&dyn DesktopUi>,
    event: &str,
    payload: &Value,
    fail_prefix: &str,
    desktop_only: &str,
    result: Value,
) -> String {
    match bridge {
        None => tool_error(desktop_only),
        Some(bridge) => match bridge.call(event, payload) {
            Err(e) => tool_error(format!("{fail_prefix}{e}")),
            Ok(_) => result.to_string(),
        },
    }
}

/// Python `desktop_ui.passthrough_json`: the renderer answers with JSON;
/// pass it through, else wrap the raw text as `{"text": ...}`.
pub fn passthrough_json(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => value.to_string(),
        Err(_) => json!({ "text": raw }).to_string(),
    }
}

/// Shared body of the read_terminal / read_window_below bridges (Python
/// `read_terminal_tool.read_pane`). `window` is `(key, value, floor)` triples —
/// `None` values are omitted, present values are int-coerced (numeric strings
/// accepted) and floored. Empty answers surface the tool's `empty` guidance.
pub fn read_pane(
    bridge: Option<&dyn DesktopUi>,
    event: &str,
    window: &[(&str, Option<&Value>, i64)],
    errors: (&str, &str, &str, &str),
) -> String {
    let Some(bridge) = bridge else {
        return tool_error(errors.0);
    };
    let mut payload = serde_json::Map::new();
    for (key, value, floor) in window {
        let Some(value) = value else { continue };
        let coerced = match value {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            Value::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        };
        let Some(coerced) = coerced else {
            return tool_error(errors.1);
        };
        payload.insert(key.to_string(), json!(coerced.max(*floor)));
    }
    match bridge.call(event, &Value::Object(payload)) {
        Err(e) => tool_error(format!("{}{}", errors.2, e)),
        Ok(raw) => {
            if raw.is_empty() {
                tool_error(errors.3)
            } else {
                passthrough_json(&raw)
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// Scripted bridge: records events, answers fixtures.
    pub(crate) struct StubBridge {
        pub responses: Mutex<Vec<Result<String, String>>>,
        pub events: Mutex<Vec<(String, Value)>>,
    }

    impl StubBridge {
        pub(crate) fn new(responses: Vec<Result<String, String>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                events: Mutex::new(Vec::new()),
            }
        }
    }

    impl DesktopUi for StubBridge {
        fn call(&self, event: &str, payload: &Value) -> Result<String, String> {
            self.events
                .lock()
                .unwrap()
                .push((event.to_string(), payload.clone()));
            self.responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err("no fixture".into()))
        }
    }

    #[test]
    fn passthrough_json_wraps_non_json_text() {
        assert_eq!(passthrough_json(r#"{"a": 1}"#), r#"{"a":1}"#);
        assert_eq!(passthrough_json("42"), "42");
        assert_eq!(passthrough_json("plain"), r#"{"text":"plain"}"#);
    }

    #[test]
    fn emit_or_error_paths() {
        let payload = json!({"pane": "chat"});
        let out = emit_or_error(
            None,
            "pane.reveal",
            &payload,
            "Failed: ",
            "desktop only.",
            json!({"ok": true}),
        );
        assert_eq!(out, r#"{"error":"desktop only."}"#);
        let stub = StubBridge::new(vec![Err("boom".into())]);
        let out = emit_or_error(
            Some(&stub),
            "pane.reveal",
            &payload,
            "Failed: ",
            "desktop only.",
            json!({"ok": true}),
        );
        assert_eq!(out, r#"{"error":"Failed: boom"}"#);
        let stub = StubBridge::new(vec![Ok(r#"{"applied":true}"#.into())]);
        let out = emit_or_error(
            Some(&stub),
            "pane.reveal",
            &payload,
            "Failed: ",
            "desktop only.",
            json!({"ok": true}),
        );
        assert_eq!(out, r#"{"ok":true}"#);
    }

    #[test]
    fn read_pane_coerces_and_floors_window_values() {
        let stub = StubBridge::new(vec![Ok(r#"{"total_lines": 10}"#.into())]);
        let out = read_pane(
            Some(&stub),
            "terminal.read.request",
            &[
                ("start", Some(&json!("5")), 0),
                ("count", Some(&json!(0)), 1),
            ],
            ("not desktop", "must be integers", "Failed: ", "empty"),
        );
        assert_eq!(out, r#"{"total_lines":10}"#);
        let (event, payload) = stub.events.lock().unwrap()[0].clone();
        assert_eq!(event, "terminal.read.request");
        assert_eq!(payload["start"], 5);
        assert_eq!(payload["count"], 1);
    }

    #[test]
    fn read_pane_error_paths() {
        let out = read_pane(None, "e", &[], ("not desktop", "int", "f", "empty"));
        assert_eq!(out, r#"{"error":"not desktop"}"#);
        let stub = StubBridge::new(vec![]);
        let out = read_pane(
            Some(&stub),
            "e",
            &[("start", Some(&json!("abc")), 0)],
            ("nd", "must be integers", "f", "empty"),
        );
        assert_eq!(out, r#"{"error":"must be integers"}"#);
        let stub = StubBridge::new(vec![Err("timed out".into())]);
        let out = read_pane(Some(&stub), "e", &[], ("nd", "int", "Failed: ", "empty"));
        assert_eq!(out, r#"{"error":"Failed: timed out"}"#);
        let stub = StubBridge::new(vec![Ok(String::new())]);
        let out = read_pane(Some(&stub), "e", &[], ("nd", "int", "f", "empty answer"));
        assert_eq!(out, r#"{"error":"empty answer"}"#);
    }
}
