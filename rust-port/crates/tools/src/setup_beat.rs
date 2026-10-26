//! setup_beat — the first-run onboarding tool (port of
//! `tools/setup_beat_tool.py`).
//!
//! The Python tool is run inline by the agent loop with the session's
//! onboarding record and the desktop card callback; a bare registry dispatch
//! has neither, so the faithful errors surface: the beat enum check first,
//! then "Onboarding is not running in this chat." (no record), then the
//! desktop-only card error when a callback is absent.

use serde_json::{json, Value};

use crate::util::tool_error;

/// `agent/onboarding_flow.py::BEAT_IDS` — the beat ids between `next` and
/// `done` (`BEATS = ("next", *BEAT_IDS, "done")`).
pub const BEAT_IDS: &[&str] = &["name", "theme", "layout", "connectors", "plugins", "tour", "build"];

/// `setup_beat_tool`'s `BEATS`.
pub fn beats() -> Vec<&'static str> {
    let mut all = vec!["next"];
    all.extend_from_slice(BEAT_IDS);
    all.push("done");
    all
}

/// The session's onboarding record loader (Python `onboarding_record.load`).
/// `Ok(None)` = not running in this chat.
pub trait OnboardingRecord: Send + Sync {
    fn load(&self) -> Result<Option<Value>, String>;
}

static RECORD: std::sync::RwLock<Option<std::sync::Arc<dyn OnboardingRecord>>> =
    std::sync::RwLock::new(None);

/// Hosts install the session's record store here.
pub fn set_record_loader(loader: Option<std::sync::Arc<dyn OnboardingRecord>>) {
    *RECORD.write().unwrap() = loader;
}

fn load_record() -> Result<Option<Value>, String> {
    match RECORD.read().unwrap().clone() {
        Some(loader) => loader.load(),
        None => Ok(None),
    }
}

/// The desktop card callback (Python `callback(payload)`); without one the
/// cards cannot show.
pub trait CardCallback: Send + Sync {
    fn show(&self, payload: &Value) -> Option<Value>;
}

static CARD: std::sync::RwLock<Option<std::sync::Arc<dyn CardCallback>>> =
    std::sync::RwLock::new(None);

/// Hosts install the desktop card callback here.
pub fn set_card_callback(callback: Option<std::sync::Arc<dyn CardCallback>>) {
    *CARD.write().unwrap() = callback;
}

fn card_callback() -> Option<std::sync::Arc<dyn CardCallback>> {
    CARD.read().unwrap().clone()
}

/// Execute `setup_beat` — the registry entry point.
pub fn execute(args: &Value) -> String {
    let beat = args.get("beat").and_then(Value::as_str).unwrap_or("");
    let beats = beats();
    if !beats.contains(&beat) {
        return tool_error(format!("beat must be one of: {}.", beats.join(", ")));
    }
    match load_record() {
        Err(e) => tool_error(&e),
        Ok(None) => tool_error("Onboarding is not running in this chat."),
        Ok(Some(record)) => {
            let Some(callback) = card_callback() else {
                // unreachable in tests unless a record loader is installed
                // without a card callback — the faithful desktop-only error.
                return tool_error("setup_beat cards show only in the Hermes desktop app.");
            };
            let _ = record;
            let payload = json!({ "beat": beat, "question": args.get("question").cloned().unwrap_or(Value::Null) });
            match callback.show(&payload) {
                Some(answer) => answer.to_string(),
                None => tool_error("No answer recorded for this beat."),
            }
        }
    }
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
    fn beat_enum_is_validated_first() {
        let _g = guard();
        set_record_loader(None);
        set_card_callback(None);
        assert!(execute(&json!({}))
            .contains("beat must be one of: next, name, theme, layout, connectors, plugins, tour, build, done."));
        assert!(execute(&json!({"beat": "bogus"})).contains("beat must be one of"));
    }

    #[test]
    fn no_record_is_the_bare_registry_error() {
        let _g = guard();
        set_record_loader(None);
        let out = execute(&json!({"beat": "next"}));
        assert!(out.contains("Onboarding is not running in this chat."), "{out}");
    }

    #[test]
    fn a_running_onboarding_needs_the_desktop_card() {
        let _g = guard();
        struct Active;
        impl OnboardingRecord for Active {
            fn load(&self) -> Result<Option<Value>, String> {
                Ok(Some(json!({ "phase": "onboarding" })))
            }
        }
        set_record_loader(Some(std::sync::Arc::new(Active)));
        set_card_callback(None);
        let out = execute(&json!({"beat": "theme"}));
        set_record_loader(None);
        assert!(out.contains("setup_beat cards show only in the Hermes desktop app"), "{out}");
    }

    #[test]
    fn card_answers_flow_through() {
        let _g = guard();
        struct Active;
        impl OnboardingRecord for Active {
            fn load(&self) -> Result<Option<Value>, String> {
                Ok(Some(json!({})))
            }
        }
        struct Cards(std::sync::Mutex<Vec<Value>>);
        impl CardCallback for Cards {
            fn show(&self, payload: &Value) -> Option<Value> {
                self.0.lock().unwrap().push(payload.clone());
                Some(json!({ "answer": "dark" }))
            }
        }
        let cards = std::sync::Arc::new(Cards(Mutex::new(vec![])));
        set_record_loader(Some(std::sync::Arc::new(Active)));
        set_card_callback(Some(cards.clone()));
        let out = execute(&json!({"beat": "theme", "question": "pick a look"}));
        set_record_loader(None);
        set_card_callback(None);
        assert!(out.contains("dark"), "{out}");
        assert_eq!(cards.0.lock().unwrap()[0]["beat"], "theme");
    }
}
