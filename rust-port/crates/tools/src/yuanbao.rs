//! yb_* — the Yuanbao platform toolset (port of `tools/yuanbao_tools.py`,
//! the "hermes-yuanbao" toolset).
//!
//! The Python tools share the active YuanbaoAdapter singleton; without a
//! connected adapter every tool raises the faithful "Yuanbao adapter is not
//! connected" envelope. The Rust port keeps the per-tool validation and the
//! `{"success": false, "error": ...}` envelope, with the adapter behind the
//! injected `YuanbaoAdapter` seam.

use serde_json::{json, Value};

#[cfg(test)]
use crate::util::tool_error;

/// The active adapter (Python `YuanbaoAdapter.get_active()`).
pub trait YuanbaoAdapter: Send + Sync {
    fn call(&self, tool: &str, args: &Value) -> Result<Value, String>;
}

static ADAPTER: std::sync::RwLock<Option<std::sync::Arc<dyn YuanbaoAdapter>>> =
    std::sync::RwLock::new(None);

/// Install (or with None, clear) the active adapter.
pub fn set_adapter(adapter: Option<std::sync::Arc<dyn YuanbaoAdapter>>) {
    *ADAPTER.write().unwrap() = adapter;
}

fn active_adapter() -> Option<std::sync::Arc<dyn YuanbaoAdapter>> {
    ADAPTER.read().unwrap().clone()
}

/// The built-in sticker catalogue (Python
/// `plugins.platforms.yuanbao.yuanbao_sticker.search_stickers`). `query` empty
/// returns the first N; the seam carries the catalogue.
pub trait StickerCatalog: Send + Sync {
    fn search(&self, query: &str, limit: usize) -> Vec<Value>;
}

static STICKERS: std::sync::RwLock<Option<std::sync::Arc<dyn StickerCatalog>>> =
    std::sync::RwLock::new(None);

/// Install (or with None, clear) the sticker catalogue.
pub fn set_sticker_catalog(catalog: Option<std::sync::Arc<dyn StickerCatalog>>) {
    *STICKERS.write().unwrap() = catalog;
}

fn not_connected() -> String {
    json!({ "success": false, "error": "Yuanbao adapter is not connected" }).to_string()
}

fn arg_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Execute `yb_query_group_info`.
pub fn execute_query_group_info(args: &Value) -> String {
    if arg_str(args, "group_code").is_empty() {
        return json!({ "success": false, "error": "group_code is required" }).to_string();
    }
    match active_adapter() {
        None => not_connected(),
        Some(adapter) => match adapter.call("yb_query_group_info", args) {
            Ok(result) => result.to_string(),
            Err(e) => json!({ "success": false, "error": e }).to_string(),
        },
    }
}

/// Execute `yb_query_group_members`.
pub fn execute_query_group_members(args: &Value) -> String {
    if arg_str(args, "group_code").is_empty() {
        return json!({ "success": false, "error": "group_code is required" }).to_string();
    }
    match active_adapter() {
        None => not_connected(),
        Some(adapter) => match adapter.call("yb_query_group_members", args) {
            Ok(result) => result.to_string(),
            Err(e) => json!({ "success": false, "error": e }).to_string(),
        },
    }
}

/// Execute `yb_search_sticker` — catalogue-only, no adapter needed. The
/// `{"success", "query", "count", "results"}` envelope is faithful.
pub fn execute_search_sticker(args: &Value) -> String {
    let query = arg_str(args, "query");
    let limit = match args.get("limit") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(10),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(10),
        _ => 10,
    };
    let safe_limit = limit.clamp(1, 50) as usize;
    let results: Vec<Value> = match STICKERS.read().unwrap().clone() {
        Some(catalog) => catalog
            .search(&query, safe_limit)
            .into_iter()
            .map(|s| {
                json!({
                    "sticker_id": s.get("sticker_id").cloned().unwrap_or(Value::Null),
                    "name": s.get("name").cloned().unwrap_or(Value::Null),
                    "description": s.get("description").cloned().unwrap_or(Value::Null),
                    "package_id": s.get("package_id").cloned().unwrap_or(Value::Null),
                })
            })
            .collect(),
        None => vec![],
    };
    json!({
        "success": true,
        "query": query,
        "count": results.len(),
        "results": results,
    })
    .to_string()
}

/// Execute `yb_send_sticker` / `yb_send_dm` — adapter-gated senders.
pub fn execute_send(tool: &str, args: &Value) -> String {
    match active_adapter() {
        None => not_connected(),
        Some(adapter) => match adapter.call(tool, args) {
            Ok(result) => result.to_string(),
            Err(e) => json!({ "success": false, "error": e }).to_string(),
        },
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
    fn group_tools_validate_group_code_then_degrade() {
        let _g = guard();
        set_adapter(None);
        let out = execute_query_group_info(&json!({}));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"], "group_code is required");
        let out = execute_query_group_members(&json!({"group_code": ""}));
        assert!(out.contains("group_code is required"), "{out}");
        let out = execute_query_group_info(&json!({"group_code": "123"}));
        assert_eq!(out, not_connected());
        let out = execute_send("yb_send_dm", &json!({}));
        assert!(out.contains("Yuanbao adapter is not connected"), "{out}");
    }

    #[test]
    fn search_sticker_clamps_limit_and_shapes_results() {
        let _g = guard();
        set_sticker_catalog(None);
        let out = execute_search_sticker(&json!({"limit": 999}));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], true);
        assert_eq!(v["count"], 0);
        struct Catalog;
        impl StickerCatalog for Catalog {
            fn search(&self, query: &str, limit: usize) -> Vec<Value> {
                assert_eq!(limit, 5); // clamped from the string "5"
                vec![json!({
                    "sticker_id": "278", "name": "666", "description": "d", "package_id": "p1",
                    "extra": "dropped"
                })]
            }
        }
        set_sticker_catalog(Some(std::sync::Arc::new(Catalog)));
        let out = execute_search_sticker(&json!({"query": "666", "limit": "5"}));
        set_sticker_catalog(None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["results"][0]["sticker_id"], "278");
        assert!(v["results"][0].get("extra").is_none());
    }

    #[test]
    fn adapter_calls_flow_through_with_the_tool_name() {
        let _g = guard();
        struct Stub(std::sync::Mutex<Vec<(String, Value)>>);
        impl YuanbaoAdapter for Stub {
            fn call(&self, tool: &str, args: &Value) -> Result<Value, String> {
                self.0.lock().unwrap().push((tool.to_string(), args.clone()));
                Ok(json!({ "success": true, "group_name": "Pai" }))
            }
        }
        let stub = std::sync::Arc::new(Stub(Mutex::new(vec![])));
        set_adapter(Some(stub.clone()));
        let out = execute_query_group_info(&json!({"group_code": "328306697"}));
        set_adapter(None);
        assert!(out.contains("Pai"), "{out}");
        let (tool, args) = &stub.0.lock().unwrap()[0];
        assert_eq!(tool, "yb_query_group_info");
        assert_eq!(args["group_code"], "328306697");
    }
}
