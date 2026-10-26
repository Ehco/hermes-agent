//! browser_* — the browser toolset (port of `tools/browser_tool.py`,
//! `browser_use_cli.py`, `browser_vault_tool.py`, `browser_cdp_tool.py`,
//! `browser_dialog_tool.py`).
//!
//! The Python tools drive a live browser through configured backends (Browser
//! Use cloud, local Chromium, Camofox) and are gated at tool-definition time by
//! the backend check. The Rust port keeps each tool's arg validation and
//! faithful error text; the live browser sits behind the injected
//! `BrowserSession` seam. Bare registry execution has no browser, so calls
//! degrade with the backend-availability errors the Python handlers return.

use serde_json::{json, Value};

use crate::util::tool_error;

/// The live browser session (Python `_active_sessions`' value). Granular tools
/// dispatch onto it; the vault's page ops and the CDP supervisor route through
/// the same live session.
pub trait BrowserSession: Send + Sync {
    fn command(&self, tool: &str, args: &Value) -> Result<Value, String>;
}

static SESSION: std::sync::RwLock<Option<std::sync::Arc<dyn BrowserSession>>> =
    std::sync::RwLock::new(None);

/// Install (or with None, clear) the active browser session.
pub fn set_session(session: Option<std::sync::Arc<dyn BrowserSession>>) {
    *SESSION.write().unwrap() = session;
}

fn active_session() -> Option<std::sync::Arc<dyn BrowserSession>> {
    SESSION.read().unwrap().clone()
}

/// `_DEFAULT_TIMEOUT_S` / `_MAX_TIMEOUT_S` (browser_use_cli).
pub const DEFAULT_TIMEOUT_S: i64 = 300;
pub const MAX_TIMEOUT_S: i64 = 1800;

/// `CDP_DOCS_URL` (browser_cdp_tool).
pub const CDP_DOCS_URL: &str = "https://chromedevtools.github.io/devtools-protocol/";

/// `browser_use_cli._SESSION_RE` — 1-64 letters, digits, dashes, underscores.
fn valid_session_name(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 64
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn no_session_error(tool: &str) -> String {
    json!({
        "success": false,
        "error": format!(
            "No browser session is available for {tool}. Configure a browser backend \
             (`hermes tools` → Browser Automation) or run '/browser connect' to attach \
             a running Chrome-family browser."
        ),
    })
    .to_string()
}

fn dispatch_session(tool: &str, args: &Value) -> String {
    match active_session() {
        Some(session) => match session.command(tool, args) {
            Ok(result) => result.to_string(),
            Err(e) => json!({ "success": false, "error": e }).to_string(),
        },
        None => no_session_error(tool),
    }
}

// --- browser_exec ---------------------------------------------------------

/// Execute `browser_exec` (the browser-use CLI bridge).
pub fn execute_exec(args: &Value) -> String {
    let code = args.get("code").and_then(Value::as_str).unwrap_or("");
    if code.trim().is_empty() {
        return tool_error(
            "No code provided. Pass Python that uses the pre-imported helpers, e.g. \
             new_tab(\"https://example.com\") then print(page_info()).",
        );
    }
    let session = args.get("session").and_then(Value::as_str).unwrap_or("");
    if !session.is_empty() && !valid_session_name(session) {
        return tool_error(format!(
            "Invalid session name {session:?}: use 1-64 letters, digits, dashes, or \
             underscores (e.g. 'r7k2')."
        ));
    }
    // The Rust port ships no managed browser-harness CLI, so execution always
    // degrades with the Python missing-CLI error.
    tool_error(
        "browser-harness is missing from Hermes's Python environment. \
         Run `hermes update` to re-sync it.",
    )
}

// --- browser_vault --------------------------------------------------------

/// Execute `browser_vault` (saved logins/cards without secrets in-context).
pub fn execute_vault(args: &Value) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    match action {
        "list" | "fill" | "save_login" | "enter_code" | "unlock" => {}
        other => {
            return json!({
                "success": false,
                "error": format!(
                    "Unknown action {other:?}; use list, fill, save_login, enter_code or unlock."
                ),
            })
            .to_string()
        }
    }
    // Page-bound vault ops need the supervisor-fenced browser session; `list`
    // needs the vault supervisor. Without a browser neither is available.
    dispatch_session("browser_vault", args)
}

// --- browser_cdp ----------------------------------------------------------

/// Execute `browser_cdp` (raw Chrome DevTools Protocol command).
pub fn execute_cdp(args: &Value) -> String {
    let method = args.get("method").and_then(Value::as_str).unwrap_or("");
    if method.is_empty() {
        return crate::util::tool_error_extra(
            "'method' is required (e.g. 'Target.getTargets')",
            Some(json!({ "cdp_docs": CDP_DOCS_URL })),
        );
    }
    // No CDP endpoint resolver exists in this port (the /browser connect flow
    // is desktop-host integrated) — the faithful no-endpoint error.
    crate::util::tool_error_extra(
        "No CDP endpoint is available. Run '/browser connect' to attach to a running \
         Chrome, Brave, Chromium, or Edge browser, or set 'browser.cdp_url' in \
         config.yaml. The Camofox backend is REST-only and does not expose CDP.",
        Some(json!({ "cdp_docs": CDP_DOCS_URL })),
    )
}

// --- browser_dialog -------------------------------------------------------

/// Execute `browser_dialog` (respond to a native JS dialog).
pub fn execute_dialog(args: &Value) -> String {
    // Faithful: the supervisor lookup happens before any action validation.
    if active_session().is_none() {
        return json!({
            "success": false,
            "error": (
                "No CDP supervisor is attached to this task. Either the browser backend \
                 doesn't expose CDP (Camofox, default Playwright) or no browser session \
                 has been started yet. Call browser_navigate or /browser connect first."
            ),
        })
        .to_string();
    }
    dispatch_session("browser_dialog", args)
}

// --- the granular tools ---------------------------------------------------

/// Execute one of the granular browser tools (navigate/snapshot/click/...).
/// Required-param validation runs first, then the session dispatch.
pub fn execute_granular(tool: &str, args: &Value) -> String {
    let required: &[&str] = match tool {
        "browser_navigate" => &["url"],
        "browser_click" => &["ref"],
        "browser_type" => &["ref", "text"],
        "browser_scroll" => &["direction"],
        "browser_press" => &["key"],
        "browser_vision" => &["question"],
        // snapshot / back / get_images / console take no required params.
        _ => &[],
    };
    for key in required {
        let missing = args
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .map(str::is_empty)
            .unwrap_or(true);
        if missing {
            return tool_error(format!(
                "'{key}' is required for {tool} (see the tool description)."
            ));
        }
    }
    dispatch_session(tool, args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct Stub(std::sync::Mutex<Vec<(String, Value)>>, Value);
    impl BrowserSession for Stub {
        fn command(&self, tool: &str, args: &Value) -> Result<Value, String> {
            self.0.lock().unwrap().push((tool.to_string(), args.clone()));
            Ok(self.1.clone())
        }
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap()
    }

    #[test]
    fn browser_exec_validation() {
        let _g = guard();
        set_session(None);
        let out = execute_exec(&json!({}));
        assert!(out.contains("No code provided"), "{out}");
        assert!(out.contains("new_tab"), "{out}");
        let out = execute_exec(&json!({"code": "print(1)", "session": "bad name!"}));
        assert!(out.contains("Invalid session name"), "{out}");
        assert!(out.contains("r7k2"), "{out}");
    }

    #[test]
    fn browser_exec_degrades_without_the_harness() {
        let _g = guard();
        set_session(None);
        let out = execute_exec(&json!({"code": "print(1)"}));
        assert!(out.contains("browser-harness is missing"), "{out}");
        assert!(out.contains("hermes update"), "{out}");
    }

    #[test]
    fn vault_unknown_action_is_faithful() {
        let _g = guard();
        set_session(None);
        let out = execute_vault(&json!({"action": "hug"}));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], false);
        assert!(
            v["error"].as_str().unwrap().contains("use list, fill, save_login, enter_code or unlock"),
            "{out}"
        );
    }

    #[test]
    fn vault_needs_a_browser_session() {
        let _g = guard();
        set_session(None);
        let out = execute_vault(&json!({"action": "list"}));
        assert!(out.contains("No browser session is available"), "{out}");
        let out = execute_vault(&json!({"action": "fill", "handle": "@h1"}));
        assert!(out.contains("No browser session is available"), "{out}");
    }

    #[test]
    fn cdp_validation_and_no_endpoint() {
        let _g = guard();
        set_session(None);
        let out = execute_cdp(&json!({}));
        assert!(out.contains("'method' is required"), "{out}");
        assert!(out.contains("Target.getTargets"), "{out}");
        assert!(out.contains("chromedevtools.github.io"), "{out}");
        let out = execute_cdp(&json!({"method": "Target.getTargets"}));
        assert!(out.contains("No CDP endpoint is available"), "{out}");
        assert!(out.contains("'/browser connect'"), "{out}");
    }

    #[test]
    fn dialog_error_is_verbatim_without_a_supervisor() {
        let _g = guard();
        set_session(None);
        let out = execute_dialog(&json!({"action": "accept"}));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], false);
        assert!(
            v["error"].as_str().unwrap().starts_with("No CDP supervisor is attached to this task."),
            "{out}"
        );
    }

    #[test]
    fn granular_tools_validate_required_params() {
        let _g = guard();
        set_session(None);
        for (tool, args) in [
            ("browser_navigate", json!({})),
            ("browser_click", json!({})),
            ("browser_type", json!({"ref": "@e1"})),
            ("browser_scroll", json!({})),
            ("browser_press", json!({})),
            ("browser_vision", json!({})),
        ] {
            let out = execute_granular(tool, &args);
            assert!(out.contains("is required"), "{tool}: {out}");
        }
    }

    #[test]
    fn granular_tools_dispatch_to_the_session() {
        let _g = guard();
        let stub = std::sync::Arc::new(Stub(Mutex::new(vec![]), json!({"success": true})));
        set_session(Some(stub.clone()));
        let out = execute_granular("browser_navigate", &json!({"url": "https://e.com"}));
        set_session(None);
        assert!(out.contains("\"success\":true"), "{out}");
        let (tool, args) = &stub.0.lock().unwrap()[0];
        assert_eq!(tool, "browser_navigate");
        assert_eq!(args["url"], "https://e.com");
    }

    #[test]
    fn timeout_bounds_match_the_cli() {
        assert_eq!(DEFAULT_TIMEOUT_S, 300);
        assert_eq!(MAX_TIMEOUT_S, 1800);
    }
}
