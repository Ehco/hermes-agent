//! set_theme — the Hermes desktop app's accent colour, light/dark mode and
//! layout, or a picker card (port of `tools/set_theme_tool.py`).
//!
//! The renderer applies the values and answers with what it applied, so the
//! model reports what the user sees: an unknown layout preset, a session that
//! is not on screen and an absent desktop window all come back as errors.
//! Swatch/preset ids are validated here and again in the renderer.

use serde_json::{json, Value};

use crate::desktop_bridge::DesktopUi;
use crate::util::tool_error;

pub const ACCENT_SWATCHES: &[&str] = &[
    "mono", "green", "cyan", "nous", "violet", "pink", "red", "orange",
];
pub const THEME_ACCENT: &str = "theme";
pub const MODES: &[&str] = &["light", "dark", "system"];
pub const PICKERS: &[&str] = &["accent", "layout"];
pub const SIMPLE_LAYOUTS: &[&str] = &["sidebar-left", "sidebar-right"];
pub const ADVANCED_LAYOUTS: &[&str] = &["default", "basic", "focus", "terminal-deck", "quad"];

/// `#rrggbb` hex accent.
pub fn is_hex_accent(accent: &str) -> bool {
    let body = accent.strip_prefix('#').unwrap_or("");
    body.len() == 6 && body.chars().all(|c| c.is_ascii_hexdigit())
}

fn accent_error(accent: &str) -> Option<String> {
    if ACCENT_SWATCHES.contains(&accent) || accent == THEME_ACCENT || is_hex_accent(accent) {
        return None;
    }
    let mut available = ACCENT_SWATCHES.to_vec();
    available.push(THEME_ACCENT);
    Some(
        json!({
            "success": false,
            "error": format!("Unknown accent '{accent}': use a swatch id, 'theme' or #rrggbb."),
            "available": available,
        })
        .to_string(),
    )
}

/// Execute `set_theme` — the registry entry point (no desktop bridge).
pub fn execute(args: &Value) -> String {
    execute_with(None, args)
}

/// `execute` against an injected bridge (desktop hosts, tests).
pub fn execute_with(bridge: Option<&dyn DesktopUi>, args: &Value) -> String {
    let get = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let values: Vec<(&str, String)> = [
        ("accent", get("accent")),
        ("mode", get("mode")),
        ("layout", get("layout")),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.map(|v| (k, v.to_string())))
    .collect();
    let picker = args
        .get("picker")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty());
    if let Some(picker) = picker {
        if !PICKERS.contains(&picker) {
            return tool_error(format!("picker must be one of: {}.", PICKERS.join(", ")));
        }
        if !values.is_empty() {
            return tool_error(
                "picker shows a card for the user to choose; pass picker alone, without values.",
            );
        }
        return match bridge {
            None => tool_error("set_theme is only available in the Hermes desktop app."),
            Some(bridge) => match bridge.call("set_theme", &json!({ "picker": picker })) {
                Err(e) => tool_error(format!("Failed to apply the theme: {e}")),
                Ok(reply) => reply,
            },
        };
    }
    if values.is_empty() {
        return tool_error(
            "Pass at least one of accent, mode or layout, or picker to let the user choose.",
        );
    }
    if let Some(mode) = values.iter().find(|(k, _)| *k == "mode").map(|(_, v)| v) {
        if !MODES.contains(&mode.as_str()) {
            return json!({
                "success": false,
                "error": format!("Unknown mode '{mode}'."),
                "available": MODES,
            })
            .to_string();
        }
    }
    if let Some(accent) = values.iter().find(|(k, _)| *k == "accent").map(|(_, v)| v) {
        if let Some(error) = accent_error(accent) {
            return error;
        }
    }
    let payload: Value = values.iter().fold(json!({}), |mut acc, (k, v)| {
        acc[*k] = json!(v);
        acc
    });
    match bridge {
        None => tool_error("set_theme is only available in the Hermes desktop app."),
        Some(bridge) => match bridge.call("set_theme", &payload) {
            Err(e) => tool_error(format!("Failed to apply the theme: {e}")),
            Ok(reply) => reply,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_bridge::tests::StubBridge;
    use serde_json::json;

    #[test]
    fn no_bridge_is_desktop_only() {
        let out = execute(&json!({"accent": "nous"}));
        assert_eq!(
            out,
            r#"{"error":"set_theme is only available in the Hermes desktop app."}"#
        );
        let out = execute(&json!({"picker": "accent"}));
        assert_eq!(
            out,
            r#"{"error":"set_theme is only available in the Hermes desktop app."}"#
        );
    }

    #[test]
    fn validation_precedes_the_bridge() {
        let stub = StubBridge::new(vec![]);
        let out = execute_with(Some(&stub), &json!({}));
        assert!(
            out.contains("Pass at least one of accent, mode or layout"),
            "{out}"
        );
        let out = execute_with(Some(&stub), &json!({"mode": "sepia"}));
        assert!(out.contains("Unknown mode 'sepia'"), "{out}");
        assert!(out.contains("\"available\":[\"light\",\"dark\",\"system\"]"));
        let out = execute_with(Some(&stub), &json!({"accent": "teal"}));
        assert!(out.contains("Unknown accent 'teal'"), "{out}");
        assert!(out.contains("\"available\":[\"mono\",\"green\",\"cyan\",\"nous\",\"violet\",\"pink\",\"red\",\"orange\",\"theme\"]"));
        let out = execute_with(Some(&stub), &json!({"picker": "font"}));
        assert!(
            out.contains("picker must be one of: accent, layout"),
            "{out}"
        );
        let out = execute_with(Some(&stub), &json!({"picker": "layout", "mode": "dark"}));
        assert!(out.contains("pass picker alone"), "{out}");
        assert!(
            stub.events.lock().unwrap().is_empty(),
            "no round-trip on validation failure"
        );
    }

    #[test]
    fn values_are_sent_and_renderer_answer_passthrough() {
        let stub = StubBridge::new(vec![Ok(
            r#"{"applied":{"accent":"nous","mode":"dark"},"success":true}"#.into(),
        )]);
        let out = execute_with(Some(&stub), &json!({"accent": " nous ", "mode": "dark"}));
        assert!(out.contains("\"applied\""), "{out}");
        let (event, payload) = stub.events.lock().unwrap()[0].clone();
        assert_eq!(event, "set_theme");
        assert_eq!(payload, json!({"accent": "nous", "mode": "dark"}));
    }

    #[test]
    fn picker_goes_alone_to_the_renderer() {
        let stub = StubBridge::new(vec![Ok(r#"{"picked":"layout:quad"}"#.into())]);
        let out = execute_with(Some(&stub), &json!({"picker": "layout"}));
        assert_eq!(out, r#"{"picked":"layout:quad"}"#);
        let (event, payload) = stub.events.lock().unwrap()[0].clone();
        assert_eq!(event, "set_theme");
        assert_eq!(payload, json!({"picker": "layout"}));
    }

    #[test]
    fn hex_and_theme_accents_are_valid() {
        assert!(is_hex_accent("#a1b2c3"));
        assert!(is_hex_accent("#A1B2C3"));
        assert!(!is_hex_accent("#a1b2"));
        assert!(!is_hex_accent("a1b2c3"));
        let stub = StubBridge::new(vec![Ok(r#"{"ok":2}"#.into()), Ok(r#"{"ok":1}"#.into())]);
        let out = execute_with(Some(&stub), &json!({"accent": "#12abef"}));
        assert_eq!(out, r#"{"ok":1}"#);
        let out = execute_with(Some(&stub), &json!({"accent": "theme"}));
        assert_eq!(out, r#"{"ok":2}"#);
    }

    #[test]
    fn bridge_failure_is_an_error() {
        let out = execute_with(
            Some(&StubBridge::new(vec![Err("gone".into())])),
            &json!({"mode": "dark"}),
        );
        assert_eq!(out, r#"{"error":"Failed to apply the theme: gone"}"#);
    }
}
