//! desktop_preview — open, close, or read the preview pane beside the chat
//! (port of `tools/preview_tool.py` + `open_preview_tool.py`).
//!
//! One action enum instead of three tools. open: show a web URL (bare domains
//! fine), a localhost dev server, or a file path (HTML renders live) — for the
//! current window only. close: dismiss the whole pane, or one tab via url.
//! read: what the pane currently shows — agent-level callback dispatch, so the
//! registry handler (no GUI callback) refuses it, exactly as Python does.

use serde_json::{json, Value};

use crate::desktop_bridge::{emit_or_error, DesktopUi};
use crate::util::tool_error;

/// Python `open_preview_tool._normalize_target`: coax a bare host/domain into a
/// fetchable URL; leave paths + schemes alone (`www.cnn.com` -> https://…,
/// `localhost:3000` -> http://…).
pub fn normalize_target(raw: &str) -> String {
    let v = raw.trim().trim_matches('`').trim();
    if v.is_empty()
        || v.contains("://")
        || v.starts_with('/')
        || v.starts_with("./")
        || v.starts_with("../")
        || v.starts_with('~')
        || v.starts_with("file:")
    {
        return v.to_string();
    }
    let localhost =
        regex::Regex::new(r"^(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\])(:\d+)?(/|$)").unwrap();
    if localhost.is_match(v) {
        return format!("http://{v}");
    }
    let domain = regex::Regex::new(r"^[\w.-]+\.[a-z]{2,}(:\d+)?(/.*)?$").unwrap();
    if domain.is_match(&v.to_lowercase()) {
        return format!("https://{v}");
    }
    v.to_string()
}

fn open_preview(bridge: Option<&dyn DesktopUi>, url: &str, label: &str) -> String {
    let target = normalize_target(url);
    if target.is_empty() {
        return tool_error(
            "url is required — a web URL (https://…), a localhost dev server, or a \
             file path to show in the preview pane.",
        );
    }
    // Reject only when we positively observe an existing directory (the
    // renderer still sees the original target either way).
    if std::path::Path::new(target.trim_start_matches("file://")).is_dir() {
        return tool_error(format!(
            "directories are not previewable — pass a file path or a URL. {target} is a directory."
        ));
    }
    let label = label.trim();
    emit_or_error(
        bridge,
        "preview.open",
        &json!({ "url": target, "label": label }),
        "Failed to open the preview pane: ",
        "The preview pane is only available in the Hermes desktop app.",
        json!({ "success": true, "url": target, "label": label }),
    )
}

fn preview_close(bridge: Option<&dyn DesktopUi>, url: &str) -> String {
    let target = normalize_target(url);
    emit_or_error(
        bridge,
        "preview.close",
        &json!({ "url": target }),
        "Failed to close the preview: ",
        "The preview pane is only available in the Hermes desktop app.",
        json!({ "success": true, "closed": if target.is_empty() { "all" } else { target.as_str() } }),
    )
}

/// Execute `desktop_preview` — the registry entry point (no desktop bridge).
pub fn execute(args: &Value) -> String {
    execute_with(None, args)
}

/// `execute` against an injected bridge (desktop hosts, tests).
pub fn execute_with(bridge: Option<&dyn DesktopUi>, args: &Value) -> String {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    match action {
        "open" => open_preview(
            bridge,
            args.get("url").and_then(Value::as_str).unwrap_or(""),
            args.get("label").and_then(Value::as_str).unwrap_or(""),
        ),
        "close" => preview_close(
            bridge,
            args.get("url").and_then(Value::as_str).unwrap_or(""),
        ),
        // read needs the GUI callback and is dispatched at the agent level.
        "read" => {
            tool_error("preview read must run inside a desktop session (no GUI callback here).")
        }
        _ => tool_error("action must be one of: open, close, read."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_bridge::tests::StubBridge;
    use serde_json::json;

    #[test]
    fn normalize_target_coaxes_bare_hosts() {
        assert_eq!(normalize_target("www.cnn.com"), "https://www.cnn.com");
        assert_eq!(normalize_target("localhost:3000"), "http://localhost:3000");
        assert_eq!(
            normalize_target("127.0.0.1:8080/api"),
            "http://127.0.0.1:8080/api"
        );
        assert_eq!(normalize_target("  `example.io` "), "https://example.io");
        assert_eq!(
            normalize_target("https://x.dev/a?b=1"),
            "https://x.dev/a?b=1"
        );
        assert_eq!(normalize_target("C:/repos/site"), "C:/repos/site");
        assert_eq!(normalize_target("./build/index.html"), "./build/index.html");
        assert_eq!(normalize_target("~/notes.html"), "~/notes.html");
        assert_eq!(normalize_target(""), "");
    }

    #[test]
    fn action_is_required_and_read_needs_a_session() {
        let out = execute(&json!({"url": "example.com"}));
        assert_eq!(
            out,
            r#"{"error":"action must be one of: open, close, read."}"#
        );
        let out = execute(&json!({"action": "read"}));
        assert_eq!(
            out,
            r#"{"error":"preview read must run inside a desktop session (no GUI callback here)."}"#
        );
    }

    #[test]
    fn open_requires_a_target_and_refuses_directories() {
        let out = execute(&json!({"action": "open"}));
        assert!(out.contains("url is required"), "{out}");
        let out =
            execute(&json!({"action": "open", "url": std::env::temp_dir().to_string_lossy()}));
        assert!(out.contains("directories are not previewable"), "{out}");
    }

    #[test]
    fn open_sends_the_normalized_target() {
        let stub = StubBridge::new(vec![Ok(String::new())]);
        let out = execute_with(
            Some(&stub),
            &json!({"action": "open", "url": "localhost:3000", "label": "dev"}),
        );
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed,
            json!({"success": true, "url": "http://localhost:3000", "label": "dev"})
        );
        let (event, payload) = stub.events.lock().unwrap()[0].clone();
        assert_eq!(event, "preview.open");
        assert_eq!(
            payload,
            json!({"url": "http://localhost:3000", "label": "dev"})
        );
    }

    #[test]
    fn close_without_url_closes_everything() {
        let stub = StubBridge::new(vec![Ok(String::new()), Ok(String::new())]);
        let out = execute_with(Some(&stub), &json!({"action": "close"}));
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, json!({"success": true, "closed": "all"}));
        let out = execute_with(Some(&stub), &json!({"action": "close", "url": "cnn.com"}));
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed,
            json!({"success": true, "closed": "https://cnn.com"})
        );
        let events = stub.events.lock().unwrap();
        let (event, payload) = events.last().unwrap();
        assert_eq!(
            (event.as_str(), payload),
            ("preview.close", &json!({"url": "https://cnn.com"}))
        );
    }

    #[test]
    fn without_bridge_is_desktop_only() {
        let out = execute(&json!({"action": "open", "url": "example.com"}));
        assert_eq!(
            out,
            r#"{"error":"The preview pane is only available in the Hermes desktop app."}"#
        );
        let out = execute(&json!({"action": "close"}));
        assert_eq!(
            out,
            r#"{"error":"The preview pane is only available in the Hermes desktop app."}"#
        );
    }

    #[test]
    fn failures_are_prefixed() {
        let out = execute_with(
            Some(&StubBridge::new(vec![Err("renderer gone".into())])),
            &json!({"action": "open", "url": "example.com"}),
        );
        assert_eq!(
            out,
            r#"{"error":"Failed to open the preview pane: renderer gone"}"#
        );
        let out = execute_with(
            Some(&StubBridge::new(vec![Err("renderer gone".into())])),
            &json!({"action": "close"}),
        );
        assert_eq!(
            out,
            r#"{"error":"Failed to close the preview: renderer gone"}"#
        );
    }
}
