//! manage_catalog — a desktop chat's catalog search and install-through-the-
//! card (port of `tools/connectors/catalog_tool.py`).
//!
//! `search` reads the plugin catalog and the skills hub and says what is
//! already installed in this chat's profile; `install` opens the same
//! connection operation `manage_connections` opens, with rows of kind
//! `plugin`/`skill`. The model sends catalog ids and an action, nothing else:
//! the pin, scan, target profile and activation are the host's, injected as
//! the `CatalogHost` trait. Without a card surface the faithful Python
//! `NOT_HERE` pointer to the terminal installer comes back.

use serde_json::{json, Value};

use crate::util::tool_error;

pub const ACTIONS: &[&str] = &["search", "install"];
pub const ITEM_KINDS: &[&str] = &["plugin", "skill"];
const TOP_KEYS: &[&str] = &["action", "kind", "query", "items", "reason"];
const ITEM_KEYS: &[&str] = &["kind", "id"];
pub const SEARCH_LIMIT: usize = 10;

pub const NOT_HERE: &str = "manage_catalog is available only in the Hermes desktop app, where the \
     approval card can be drawn. Tell the user to install from a terminal instead: \
     `hermes plugins install <id>` for a plugin, `hermes skills install <id>` for a skill.";

pub const NOTE: &str = "Settled once; do not re-offer a row the user skipped. Each installed row \
     names target_profile: its tools and skills are live now in that profile's open chats and in \
     every new chat of it; they are callable in this chat only when this chat runs in that \
     profile. A failed row carries the reason in detail.";

/// The host seam: catalog search rows and the install-card opener.
pub trait CatalogHost: Send + Sync {
    /// Filtered catalog rows: `{id, kind, display, tier, platforms, installed}`.
    fn search(&self, query: &str, kind: Option<&str>) -> Vec<Value>;
    /// Open one install card for the (kind, id) rows; blocks until every row
    /// is installed, skipped or failed. Returns the runner's result JSON.
    fn install(&self, items: &[(String, String)]) -> Result<Value, String>;
    /// This chat's profile name (`installed_in`).
    fn installed_in(&self) -> String;
}

/// Python `_check_args`: faithful unknown-key and shape validation.
pub fn check_args(args: &Value) -> Option<String> {
    let Some(obj) = args.as_object() else {
        return Some("manage_catalog takes action, kind, query, items and reason.".into());
    };
    let mut unknown: Vec<&String> = obj
        .keys()
        .filter(|k| !TOP_KEYS.contains(&k.as_str()))
        .collect();
    unknown.sort();
    if !unknown.is_empty() {
        return Some(format!(
            "unknown parameter(s) {}: manage_catalog takes action, kind, query, items and reason. \
             The host decides source, version, profile and settings.",
            unknown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let action = obj.get("action").and_then(Value::as_str).unwrap_or("");
    if !ACTIONS.contains(&action) {
        return Some(format!("action must be one of {}.", ACTIONS.join(", ")));
    }
    match obj.get("kind") {
        None | Some(Value::Null) => {}
        Some(Value::String(kind)) if ITEM_KINDS.contains(&kind.as_str()) => {}
        Some(_) => return Some(format!("kind must be one of {}.", ITEM_KINDS.join(", "))),
    }
    if action == "search" {
        return None;
    }
    let items = obj.get("items");
    let Some(items) = items.and_then(Value::as_array).filter(|a| !a.is_empty()) else {
        return Some(
            "install needs 'items': [{\"kind\": \"plugin\", \"id\": \"<catalog id>\"}, ...]".into(),
        );
    };
    for item in items {
        let Some(obj) = item.as_object() else {
            return Some("every item is an object {kind, id}.".into());
        };
        let mut extra: Vec<&String> = obj
            .keys()
            .filter(|k| !ITEM_KEYS.contains(&k.as_str()))
            .collect();
        extra.sort();
        if !extra.is_empty() {
            return Some(format!(
                "unknown item field(s) {}: an item is {{kind, id}}. The host decides source, \
                 version, profile and settings.",
                extra
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("");
        let id = obj.get("id").and_then(Value::as_str).unwrap_or("").trim();
        if !ITEM_KINDS.contains(&kind) || id.is_empty() {
            return Some("every item needs kind (plugin or skill) and a non-empty id.".into());
        }
    }
    None
}

/// Execute `manage_catalog` — the registry entry point (no card surface).
pub fn execute(args: &Value) -> String {
    execute_with(None, args)
}

/// `execute` against an injected host (desktop hosts, tests).
pub fn execute_with(host: Option<&dyn CatalogHost>, args: &Value) -> String {
    if let Some(error) = check_args(args) {
        return tool_error(error);
    }
    // `card_surface` is the host's presence: no host, no card to draw.
    let Some(host) = host else {
        return tool_error(NOT_HERE);
    };
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    if action == "search" {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
        let kind = args.get("kind").and_then(Value::as_str);
        return json!({
            "results": host.search(query, kind),
            "installed_in": host.installed_in(),
        })
        .to_string();
    }
    let mut seen = std::collections::HashSet::new();
    let items: Vec<(String, String)> = args["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["kind"].as_str().unwrap_or("").to_string(),
                item["id"].as_str().unwrap_or("").trim().to_string(),
            )
        })
        .filter(|key| seen.insert(key.clone()))
        .collect();
    match host.install(&items) {
        Ok(result) => result.to_string(),
        Err(e) => tool_error(e),
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

    struct StubHost {
        installs: Mutex<Vec<Vec<(String, String)>>>,
    }

    impl CatalogHost for StubHost {
        fn search(&self, query: &str, kind: Option<&str>) -> Vec<Value> {
            let mut rows = vec![
                json!({"id": "gmail", "kind": "plugin", "display": "Gmail", "tier": "official",
                       "platforms": [], "installed": false}),
                json!({"id": "summarize", "kind": "skill", "display": "Summarize", "tier": "official",
                       "platforms": [], "installed": true}),
            ];
            if let Some(kind) = kind {
                rows.retain(|r| r["kind"] == json!(kind));
            }
            if !query.is_empty() {
                rows.retain(|r| {
                    r["display"]
                        .as_str()
                        .unwrap()
                        .to_lowercase()
                        .contains(&query.to_lowercase())
                });
            }
            rows.truncate(SEARCH_LIMIT);
            rows
        }
        fn install(&self, items: &[(String, String)]) -> Result<Value, String> {
            self.installs.lock().unwrap().push(items.to_vec());
            Ok(json!({"installed": [{"id": items[0].1, "target_profile": "default"}]}))
        }
        fn installed_in(&self) -> String {
            "default".into()
        }
    }

    #[test]
    fn arg_validation_is_faithful() {
        for (args, expected) in [
            (json!({}), "action must be one of search, install"),
            (json!({"action": "browse"}), "action must be one of"),
            (
                json!({"action": "search", "kind": "theme"}),
                "kind must be one of plugin, skill",
            ),
            (
                json!({"action": "search", "zap": 1}),
                "unknown parameter(s) zap",
            ),
            (json!({"action": "install"}), "install needs 'items'"),
            (
                json!({"action": "install", "items": []}),
                "install needs 'items'",
            ),
            (
                json!({"action": "install", "items": ["gmail"]}),
                "every item is an object",
            ),
            (
                json!({"action": "install", "items": [{"kind": "plugin", "id": "x", "pin": true}]}),
                "unknown item field(s) pin",
            ),
            (
                json!({"action": "install", "items": [{"id": "x"}]}),
                "every item needs kind",
            ),
            (
                json!({"action": "install", "items": [{"kind": "theme", "id": "x"}]}),
                "every item needs kind",
            ),
            (
                json!({"action": "install", "items": [{"kind": "plugin", "id": " " }]}),
                "every item needs kind",
            ),
        ] {
            let out = execute(&args);
            assert!(out.contains(expected), "{args}: {out}");
        }
    }

    #[test]
    fn without_a_card_surface_is_not_here() {
        let _g = guard();
        let out = execute(&json!({"action": "search"}));
        assert!(
            out.contains("available only in the Hermes desktop app"),
            "{out}"
        );
        let out =
            execute(&json!({"action": "install", "items": [{"kind": "plugin", "id": "gmail"}]}));
        assert!(out.contains("hermes plugins install <id>"), "{out}");
    }

    #[test]
    fn search_lists_rows_and_installed_in() {
        let _g = guard();
        let host = StubHost {
            installs: Mutex::new(vec![]),
        };
        let out = execute_with(Some(&host), &json!({"action": "search"}));
        assert!(out.contains("\"results\""), "{out}");
        assert!(out.contains("\"installed_in\":\"default\""), "{out}");
        assert!(out.contains("gmail"), "{out}");
        let out = execute_with(Some(&host), &json!({"action": "search", "kind": "skill"}));
        assert!(out.contains("summarize") && !out.contains("gmail"), "{out}");
        let out = execute_with(Some(&host), &json!({"action": "search", "query": "gma"}));
        assert!(out.contains("gmail"), "{out}");
    }

    #[test]
    fn install_dedupes_rows_into_one_card() {
        let _g = guard();
        let host = StubHost {
            installs: Mutex::new(vec![]),
        };
        let out = execute_with(
            Some(&host),
            &json!({"action": "install", "reason": "user asked", "items": [
                {"kind": "plugin", "id": "gmail"},
                {"kind": "skill", "id": "summarize"},
            ]}),
        );
        assert!(out.contains(r#""target_profile":"default""#), "{out}");
        let calls = host.installs.lock().unwrap();
        assert_eq!(calls.len(), 1, "one card");
        assert_eq!(calls[0].len(), 2);
        assert_eq!(calls[0][0], ("plugin".into(), "gmail".into()));
    }
}
