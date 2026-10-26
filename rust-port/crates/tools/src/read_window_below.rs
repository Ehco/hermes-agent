//! read_window_below — which OS window sits directly underneath the Hermes
//! desktop window (port of `tools/read_window_tool.py`).
//!
//! The window list lives with the OS, so this round-trips through the desktop
//! bridge (`window.read.request`) like `read_terminal` does. JSON:
//! `{window: {app, title, bounds, id}, frontmost, platform}` — metadata only,
//! never pixels.

use serde_json::Value;

use crate::desktop_bridge::read_pane;

const NOT_DESKTOP: &str = "read_window_below is only available in the Hermes desktop app.";
const FAIL_PREFIX: &str = "Failed to read the window below: ";
const EMPTY: &str = "Could not determine the window underneath (the desktop app did \
                     not answer, or window enumeration is unavailable on this system).";

/// Execute `read_window_below` — the registry entry point (no desktop bridge).
pub fn execute(args: &Value) -> String {
    execute_with(None, args)
}

/// `execute` against an injected bridge (desktop hosts, tests). Takes no
/// window parameters — the enumeration is the renderer's job.
pub fn execute_with(
    bridge: Option<&dyn crate::desktop_bridge::DesktopUi>,
    _args: &Value,
) -> String {
    read_pane(
        bridge,
        "window.read.request",
        &[],
        (NOT_DESKTOP, "", FAIL_PREFIX, EMPTY),
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
    fn round_trips_without_window_params() {
        let stub = StubBridge::new(vec![Ok(
            r#"{"window":{"app":"chrome.exe","title":"Docs","bounds":[0,0,800,600],"id":42},"frontmost":false,"platform":"win32"}"#
                .into(),
        )]);
        let out = execute_with(Some(&stub), &json!({}));
        assert!(out.contains("chrome.exe"), "{out}");
        let (event, payload) = stub.events.lock().unwrap()[0].clone();
        assert_eq!(event, "window.read.request");
        assert!(payload.as_object().unwrap().is_empty());
    }

    #[test]
    fn empty_answer_surfaces_the_guidance() {
        let out = execute_with(Some(&StubBridge::new(vec![Ok(String::new())])), &json!({}));
        assert!(
            out.contains("Could not determine the window underneath"),
            "{out}"
        );
        let out = execute_with(
            Some(&StubBridge::new(vec![Err("no answer".into())])),
            &json!({}),
        );
        assert_eq!(
            out,
            r#"{"error":"Failed to read the window below: no answer"}"#
        );
    }
}
