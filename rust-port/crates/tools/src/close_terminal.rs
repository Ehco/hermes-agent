//! close_terminal — hide a background process's read-only terminal tab in the
//! desktop GUI without killing the process (port of
//! `tools/close_terminal_tool.py`).
//!
//! The Python handler routes through the process registry's `on_close` sink,
//! which the desktop gateway wires to a `terminal.close` event. Without the
//! sink wired it returns the faithful desktop-only error — the same
//! degradation pattern as the other desktop-bridge tools.

use serde_json::{json, Value};

use crate::util::tool_error;

/// The desktop gateway's close sink (Python `process_registry.on_close`).
/// Receives the session id and returns whatever the gateway produced.
pub trait CloseTerminalSink: Send + Sync {
    fn close(&self, session_id: &str) -> Result<Value, String>;
}

static SINK: std::sync::RwLock<Option<std::sync::Arc<dyn CloseTerminalSink>>> =
    std::sync::RwLock::new(None);

/// Wire (or with None, unwire) the desktop close sink.
pub fn set_close_sink(sink: Option<std::sync::Arc<dyn CloseTerminalSink>>) {
    *SINK.write().unwrap() = sink;
}

fn on_close() -> Option<std::sync::Arc<dyn CloseTerminalSink>> {
    SINK.read().unwrap().clone()
}

/// Execute `close_terminal` — the registry entry point.
pub fn execute(args: &Value) -> String {
    let process_id = args
        .get("process_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if process_id.is_empty() {
        return tool_error(
            "process_id is required (the background process whose tab to close).",
        );
    }
    let Some(sink) = on_close() else {
        return json!({
            "status": "error",
            "error": "close_terminal is only available in the Hermes desktop app."
        })
        .to_string();
    };
    match sink.close(process_id) {
        Err(e) => json!({ "status": "error", "error": e }).to_string(),
        Ok(result) => result.to_string(),
    }
}

/// `process_registry.request_close_terminal`'s success payload (used by hosts
/// implementing the sink).
pub fn success_payload(session_id: &str) -> Value {
    json!({
        "status": "ok",
        "closed": session_id,
        "note": "Closed the read-only terminal tab. The process was not killed; \
                 its output remains available and the user can reopen the tab \
                 from the status stack."
    })
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
    fn process_id_is_required() {
        let _g = guard();
        set_close_sink(None);
        assert!(execute(&json!({})).contains("process_id is required"));
        assert!(execute(&json!({"process_id": "   "}))
            .contains("process_id is required"));
    }

    #[test]
    fn without_the_sink_it_is_desktop_only() {
        let _g = guard();
        set_close_sink(None);
        let out = execute(&json!({"process_id": "proc_1"}));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(
            v["error"],
            "close_terminal is only available in the Hermes desktop app."
        );
    }

    #[test]
    fn wired_sink_receives_the_session_id() {
        let _g = guard();
        struct Sink(std::sync::Mutex<Vec<String>>);
        impl CloseTerminalSink for Sink {
            fn close(&self, session_id: &str) -> Result<Value, String> {
                self.0.lock().unwrap().push(session_id.to_string());
                Ok(success_payload(session_id))
            }
        }
        let sink = std::sync::Arc::new(Sink(Mutex::new(vec![])));
        set_close_sink(Some(sink.clone()));
        let out = execute(&json!({"process_id": "proc_9"}));
        set_close_sink(None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["closed"], "proc_9");
        assert_eq!(sink.0.lock().unwrap()[0], "proc_9");
    }

    #[test]
    fn sink_errors_surface_verbatim() {
        let _g = guard();
        struct Failing;
        impl CloseTerminalSink for Failing {
            fn close(&self, _s: &str) -> Result<Value, String> {
                Err("tab already closed".into())
            }
        }
        set_close_sink(Some(std::sync::Arc::new(Failing)));
        let out = execute(&json!({"process_id": "proc_1"}));
        set_close_sink(None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error"], "tab already closed");
    }
}
