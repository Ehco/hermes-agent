//! computer_use — universal desktop control via cua-driver (port of
//! `tools/computer_use_tool.py` + `tools/computer_use/schema.py`).
//!
//! The Python tool's order is: hard blocks (missing `action`, unsafe combos)
//! → Bot-Desktop lease → approval scopes → backend dispatch. The Rust port
//! keeps the arg validation and the faithful backend-unavailable error; the
//! cua backend itself is host-integrated (GUI/computer-use), so bare registry
//! execution surfaces the install-hint error the Python surfaces when the
//! backend cannot start.

use serde_json::{json, Value};

#[cfg(test)]
use crate::util::tool_error;

/// `computer_use/schema.py::_PROPERTIES["action"]["enum"]`.
pub const ACTIONS: &[&str] = &[
    "capture",
    "click",
    "double_click",
    "right_click",
    "middle_click",
    "drag",
    "scroll",
    "type",
    "key",
    "set_value",
    "wait",
    "list_apps",
    "list_windows",
    "focus_app",
];

/// The cua backend (Python `_backend_for_call`). `handle` runs one action with
/// the validated args and returns the backend's JSON result.
pub trait ComputerUseBackend: Send + Sync {
    fn handle(&self, action: &str, args: &Value) -> Result<Value, String>;
}

static BACKEND: std::sync::RwLock<Option<std::sync::Arc<dyn ComputerUseBackend>>> =
    std::sync::RwLock::new(None);

/// Install (or with None, clear) the active cua backend.
pub fn set_backend(backend: Option<std::sync::Arc<dyn ComputerUseBackend>>) {
    *BACKEND.write().unwrap() = backend;
}

fn active_backend() -> Option<std::sync::Arc<dyn ComputerUseBackend>> {
    BACKEND.read().unwrap().clone()
}

/// Execute `computer_use` — the registry entry point.
pub fn execute(args: &Value) -> String {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .map(|a| a.trim().to_lowercase())
        .unwrap_or_default();
    if action.is_empty() {
        return json!({ "error": "missing `action`" }).to_string();
    }
    if !ACTIONS.contains(&action.as_str()) {
        return json!({
            "error": format!("unknown action '{action}' (expected one of: {})", ACTIONS.join(", ")),
        })
        .to_string();
    }
    match active_backend() {
        Some(backend) => match backend.handle(&action, args) {
            Ok(result) => result.to_string(),
            Err(e) => backend_error(&e),
        },
        None => backend_error(
            "no computer_use backend is registered in this execution context",
        ),
    }
}

/// `_backend_for_call`'s failure shape (with the cua-driver install hint).
fn backend_error(message: &str) -> String {
    json!({
        "error": format!("computer_use backend unavailable: {message}"),
        "hint": (
            "If the cua-driver binary is missing, run `hermes computer-use install`. \
             If a Python dependency is missing, the error above shows the exact \
             install command."
        ),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap()
    }

    #[test]
    fn missing_and_unknown_action() {
        let _g = guard();
        set_backend(None);
        assert_eq!(execute(&json!({})), r#"{"error":"missing `action`"}"#);
        let out = execute(&json!({"action": "teleport"}));
        assert!(out.contains("unknown action 'teleport'"), "{out}");
        assert!(out.contains("capture"), "{out}");
    }

    #[test]
    fn no_backend_surfaces_the_install_hint() {
        let _g = guard();
        set_backend(None);
        let out = execute(&json!({"action": "capture", "mode": "som"}));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(
            v["error"].as_str().unwrap().starts_with("computer_use backend unavailable:"),
            "{out}"
        );
        assert!(v["hint"].as_str().unwrap().contains("hermes computer-use install"));
    }

    #[test]
    fn backend_receives_the_action_and_args() {
        let _g = guard();
        struct Stub(std::sync::Mutex<Vec<(String, Value)>>);
        impl ComputerUseBackend for Stub {
            fn handle(&self, action: &str, args: &Value) -> Result<Value, String> {
                self.0.lock().unwrap().push((action.to_string(), args.clone()));
                Ok(json!({ "ok": true, "action": action }))
            }
        }
        let stub = std::sync::Arc::new(Stub(Mutex::new(vec![])));
        set_backend(Some(stub.clone()));
        let out = execute(&json!({"action": "click", "element": 3}));
        set_backend(None);
        assert!(out.contains("\"ok\":true"), "{out}");
        let (action, args) = &stub.0.lock().unwrap()[0];
        assert_eq!(action, "click");
        assert_eq!(args["element"], 3);
    }

    #[test]
    fn backend_errors_keep_the_envelope() {
        let _g = guard();
        struct Failing;
        impl ComputerUseBackend for Failing {
            fn handle(&self, _a: &str, _args: &Value) -> Result<Value, String> {
                Err("screen is asleep".into())
            }
        }
        set_backend(Some(std::sync::Arc::new(Failing)));
        let out = execute(&json!({"action": "capture"}));
        set_backend(None);
        assert!(out.contains("computer_use backend unavailable: screen is asleep"), "{out}");
        assert!(tool_error("x").contains("error"));
    }
}
