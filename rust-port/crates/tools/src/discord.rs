//! discord / discord_admin — Discord bot tools (port of
//! `tools/discord_tool.py`).
//!
//! The Python tools share one action table: `discord` carries the core
//! participation trio, `discord_admin` the server-management actions. Both
//! need `DISCORD_BOT_TOKEN` (the check_fn gates the tools on it, and the
//! handler re-checks). The Rust port keeps the token gate, the action enums,
//! the required-param table and the error text verbatim; the REST calls sit
//! behind the injected `DiscordRest` seam.

use serde_json::{json, Value};

use crate::util::tool_error;

/// `_ACTION_MANIFEST` — action → required params (from the call signature).
pub const ACTIONS: &[(&str, &[&str])] = &[
    ("list_guilds", &[]),
    ("server_info", &["guild_id"]),
    ("list_channels", &["guild_id"]),
    ("channel_info", &["channel_id"]),
    ("list_roles", &["guild_id"]),
    ("member_info", &["guild_id", "user_id"]),
    ("search_members", &["guild_id", "query"]),
    ("fetch_messages", &["channel_id"]),
    ("list_pins", &["channel_id"]),
    ("pin_message", &["channel_id", "message_id"]),
    ("unpin_message", &["channel_id", "message_id"]),
    ("delete_message", &["channel_id", "message_id"]),
    ("create_thread", &["channel_id", "name"]),
    ("add_role", &["guild_id", "user_id", "role_id"]),
    ("remove_role", &["guild_id", "user_id", "role_id"]),
];

/// `_CORE_ACTION_NAMES`.
pub const CORE_ACTIONS: &[&str] = &["fetch_messages", "search_members", "create_thread"];

/// The valid action set for one tool (`discord` vs `discord_admin`).
pub fn valid_actions(admin: bool) -> Vec<&'static str> {
    ACTIONS
        .iter()
        .filter(|(name, _)| admin == !CORE_ACTIONS.contains(name))
        .map(|(name, _)| *name)
        .collect()
}

/// `_get_bot_token` (env-only in this port).
pub fn bot_token() -> Option<String> {
    std::env::var("DISCORD_BOT_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// The Discord REST transport (Python `_action_fn(token=..., **kwargs)`).
pub trait DiscordRest: Send + Sync {
    fn call(&self, action: &str, token: &str, params: &Value) -> Result<Value, String>;
}

static REST: std::sync::RwLock<Option<std::sync::Arc<dyn DiscordRest>>> =
    std::sync::RwLock::new(None);

/// Install (or with None, clear) the REST transport.
pub fn set_rest(rest: Option<std::sync::Arc<dyn DiscordRest>>) {
    *REST.write().unwrap() = rest;
}

/// `_run_discord_action` for one tool.
fn run_action(args: &Value, tool_label: &str, admin: bool) -> String {
    let Some(token) = bot_token() else {
        return tool_error("DISCORD_BOT_TOKEN not configured.");
    };
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let valid = valid_actions(admin);
    if !valid.contains(&action) {
        return crate::util::tool_error_extra(
            &format!("Unknown action: {action}"),
            Some(json!({ "available_actions": valid })),
        );
    }
    // Required-param defaults (`_HANDLER_DEFAULTS` make params default to ""
    // and the missing check filters the empties).
    let required = ACTIONS
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, req)| *req)
        .unwrap_or(&[]);
    let missing: Vec<&str> = required
        .iter()
        .filter(|p| {
            args.get(**p)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        })
        .copied()
        .collect();
    if !missing.is_empty() {
        return tool_error(format!(
            "Missing required parameters for '{action}': {}",
            missing.join(", ")
        ));
    }
    match REST.read().unwrap().clone() {
        Some(rest) => match rest.call(action, &token, args) {
            Ok(result) => result.to_string(),
            Err(e) => tool_error(&e),
        },
        None => tool_error(&format!(
            "The Discord REST transport is not available in this execution context ({tool_label})."
        )),
    }
}

/// Execute `discord`.
pub fn execute(args: &Value) -> String {
    run_action(args, "discord", false)
}

/// Execute `discord_admin`.
pub fn execute_admin(args: &Value) -> String {
    run_action(args, "discord_admin", true)
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
    fn action_tables_split_core_and_admin() {
        let core = valid_actions(false);
        // ACTIONS-table order (Python `_available_actions` iterates `_ACTIONS`).
        assert_eq!(core, vec!["search_members", "fetch_messages", "create_thread"]);
        let admin = valid_actions(true);
        assert!(admin.contains(&"add_role"));
        assert!(admin.contains(&"delete_message"));
        assert_eq!(core.len() + admin.len(), ACTIONS.len());
    }

    #[test]
    fn token_gate_comes_first() {
        let _g = guard();
        set_rest(None);
        let had = std::env::var("DISCORD_BOT_TOKEN").ok();
        std::env::remove_var("DISCORD_BOT_TOKEN");
        assert!(execute(&json!({"action": "fetch_messages", "channel_id": "c"}))
            .contains("DISCORD_BOT_TOKEN not configured."));
        if let Some(t) = had {
            std::env::set_var("DISCORD_BOT_TOKEN", t);
        }
    }

    #[test]
    fn unknown_action_lists_the_valid_set() {
        let _g = guard();
        set_rest(None);
        std::env::set_var("DISCORD_BOT_TOKEN", "t");
        let out = execute(&json!({"action": "add_role"}));
        std::env::remove_var("DISCORD_BOT_TOKEN");
        assert!(out.contains("Unknown action: add_role"), "{out}");
        assert!(out.contains("fetch_messages"), "{out}");
    }

    #[test]
    fn missing_params_are_reported_per_action() {
        let _g = guard();
        set_rest(None);
        std::env::set_var("DISCORD_BOT_TOKEN", "t");
        let out = execute(&json!({"action": "create_thread"}));
        std::env::remove_var("DISCORD_BOT_TOKEN");
        assert!(
            out.contains("Missing required parameters for 'create_thread': channel_id, name"),
            "{out}"
        );
    }

    #[test]
    fn rest_transport_receives_token_and_params() {
        let _g = guard();
        struct Stub(std::sync::Mutex<Vec<(String, String, Value)>>);
        impl DiscordRest for Stub {
            fn call(&self, action: &str, token: &str, params: &Value) -> Result<Value, String> {
                self.0
                    .lock()
                    .unwrap()
                    .push((action.to_string(), token.to_string(), params.clone()));
                Ok(json!({ "ok": true }))
            }
        }
        let stub = std::sync::Arc::new(Stub(Mutex::new(vec![])));
        set_rest(Some(stub.clone()));
        std::env::set_var("DISCORD_BOT_TOKEN", "sekret");
        let out = execute(&json!({"action": "fetch_messages", "channel_id": "42", "limit": 10}));
        std::env::remove_var("DISCORD_BOT_TOKEN");
        set_rest(None);
        assert!(out.contains("\"ok\":true"), "{out}");
        let (action, token, params) = &stub.0.lock().unwrap()[0];
        assert_eq!((action.as_str(), token.as_str()), ("fetch_messages", "sekret"));
        assert_eq!(params["channel_id"], "42");
    }
}
