//! read_terminal — the in-app terminal pane in the desktop GUI (port of
//! `tools/read_terminal_tool.py`).
//!
//! The buffer lives in the desktop renderer (xterm.js), so this round-trips
//! through the desktop bridge: `terminal.read.request` -> the renderer's main
//! process -> the answer. No args = visible screen + total_lines; page
//! scrollback with start_line (0 = oldest) + count.

use serde_json::Value;

use crate::desktop_bridge::read_pane;

const NOT_DESKTOP: &str = "read_terminal is only available in the Hermes desktop app.";
const NOT_INTEGERS: &str = "start_line and count must be integers.";
const FAIL_PREFIX: &str = "Failed to read terminal: ";
const EMPTY: &str = "No in-app terminal answered: nothing is open, or the bridge timed out. \
                     If the terminal pane IS open, the desktop app may be older than this \
                     backend — update it and retry.";

/// Execute `read_terminal` — the registry entry point (no desktop bridge).
pub fn execute(args: &Value) -> String {
    execute_with(None, args)
}

/// `execute` against an injected bridge (desktop hosts, tests).
pub fn execute_with(bridge: Option<&dyn crate::desktop_bridge::DesktopUi>, args: &Value) -> String {
    read_pane(
        bridge,
        "terminal.read.request",
        &[
            ("start", args.get("start_line"), 0),
            ("count", args.get("count"), 1),
        ],
        (NOT_DESKTOP, NOT_INTEGERS, FAIL_PREFIX, EMPTY),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_bridge::tests::StubBridge;
    use serde_json::json;

    #[test]
    fn no_bridge_is_desktop_only() {
        assert_eq!(
            execute(&json!({})),
            format!(r#"{{"error":"{NOT_DESKTOP}"}}"#)
        );
    }

    #[test]
    fn pages_scrollback_and_passes_the_answer_through() {
        let stub = StubBridge::new(vec![Ok(
            r#"{"total_lines":240,"start":200,"end":240,"viewport_rows":40,"cursor_row":238,"text":"..."}"#.into(),
        )]);
        let out = execute_with(Some(&stub), &json!({"start_line": 200, "count": 40}));
        assert!(out.contains("\"total_lines\":240"), "{out}");
        let (event, payload) = stub.events.lock().unwrap()[0].clone();
        assert_eq!(event, "terminal.read.request");
        assert_eq!(payload, json!({"start": 200, "count": 40}));
    }

    #[test]
    fn count_is_floored_at_one_and_bad_values_error() {
        let stub = StubBridge::new(vec![Ok(r#"{"total_lines":1}"#.into())]);
        let out = execute_with(Some(&stub), &json!({"count": 0}));
        assert!(out.contains("\"total_lines\":1"));
        assert_eq!(stub.events.lock().unwrap()[0].1["count"], 1);
        let stub = StubBridge::new(vec![]);
        let out = execute_with(Some(&stub), &json!({"start_line": "x"}));
        assert_eq!(out, format!(r#"{{"error":"{NOT_INTEGERS}"}}"#));
    }

    #[test]
    fn empty_answer_surfaces_the_guidance() {
        let out = execute_with(Some(&StubBridge::new(vec![Ok(String::new())])), &json!({}));
        assert!(out.contains("No in-app terminal answered"), "{out}");
        let out = execute_with(
            Some(&StubBridge::new(vec![Err("timed out".into())])),
            &json!({}),
        );
        assert_eq!(out, r#"{"error":"Failed to read terminal: timed out"}"#);
    }
}
