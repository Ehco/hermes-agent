//! memory — persistent curated memory (MEMORY.md = agent notes, USER.md =
//! user profile). Port of `tools/memory_tool.py::memory_tool`, a thin
//! dispatcher over `hermes_skills::memory::MemoryStore` (the port of
//! `tools/memory_tool_store.py`) — the store's add/patch/remove, match
//! semantics, budgets and drift guards are NOT reimplemented here.
//!
//! Contract: `target` is required ('memory' | 'user'); a single op is
//! `action` + `content`/`old_text` (`new_text` aliases `content`); a batch is
//! `operations: [{action, content?, new_text?, old_text?}]` applied
//! all-or-nothing — any failure restores the pre-batch files and reports
//! `"No operations were applied (batch is all-or-nothing)."`. Divergence from
//! the Python: the budget is checked per applied op rather than only on the
//! final result, so an add-alone-overflows batch must still order removals
//! first (the common consolidation shape works unchanged). Like the Python
//! (#103419, upstream faef4e0ab8), a batch that would empty a non-empty store
//! is refused — except a one-item remove, the deliberate single delete.
//!
//! Success: `{"success": true, "message", "usage", "entry_count", ...}`.
//! Failure: `{"success": false, "error", ...}` with `current_entries` /
//! `usage` / `closest_entries` extras exactly where the Python carries them.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

use hermes_skills::memory::{parse_entries, MemoryStore, Target, ENTRY_DELIMITER};

const REPLACE_REMOVED: &str =
    "'replace' is not available; use 'patch'. old_text is the exact span to change and \
content is its new text; the rest of the entry is kept. To rewrite a whole entry, \
set old_text to the whole entry. Nothing was written.";
const PATCH_DROP_HINT: &str = "To drop text inside an entry, patch a longer span that contains it; 'remove' deletes the whole entry.";

/// Profile memories dir: `$HERMES_HOME/memories` (or `~/.hermes/memories`).
fn memories_dir() -> PathBuf {
    hermes_home().join("memories")
}

fn hermes_home() -> PathBuf {
    if let Ok(h) = std::env::var("HERMES_HOME") {
        if !h.trim().is_empty() {
            return PathBuf::from(h);
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".hermes")
}

/// Execute `memory` — the registry entry point (JSON string in, out).
pub fn execute(args: &Value) -> String {
    execute_in(&memories_dir(), args)
}

/// `execute` against an explicit memories directory (tests, embedding hosts).
pub fn execute_in(dir: &Path, args: &Value) -> String {
    let mut store = MemoryStore::new(dir);
    // A malformed store loads anyway (over-limit files are fine); a genuinely
    // unreadable one fails on the first write like the Python.
    let _ = store.load_from_disk();
    run(&mut store, dir, args)
}

fn target_of(args: &Value) -> Result<Target, Value> {
    match args.get("target").and_then(Value::as_str) {
        None | Some("") => Err(json!({
            "success": false,
            "error": "Missing required target. Retry with target='memory' or target='user' at the top level. \
                      For a batch, remove target from every operation; use separate calls for different stores. \
                      Nothing was written or staged."
        })),
        Some("memory") => Ok(Target::Memory),
        Some("user") => Ok(Target::User),
        Some(other) => Err(json!({
            "success": false,
            "error": format!("Invalid memory target '{other}'. Use 'memory' or 'user'.")
        })),
    }
}

fn run(store: &mut MemoryStore, dir: &Path, args: &Value) -> String {
    let target = match target_of(args) {
        Ok(t) => t,
        Err(err) => return err.to_string(),
    };
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .or_else(|| {
            args.get("new_text")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
        });
    let old_text = args.get("old_text").and_then(Value::as_str);

    if let Some(ops) = args.get("operations") {
        let ops = match ops {
            Value::Array(items) => items.clone(),
            _ => {
                return util_err(
                    "operations must be a list of {action, content?, old_text?} objects.",
                );
            }
        };
        return apply_batch(store, dir, target, &ops);
    }

    // Single-op shape.
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    if action.is_empty() {
        return util_err(
            "Missing required parameter: provide 'action' (add/patch/remove) \
             or 'operations' (batch list). Got neither.",
        );
    }
    if action == "replace" {
        return util_err(REPLACE_REMOVED);
    }
    if action != "add" && action != "patch" && action != "remove" {
        return util_err(&format!(
            "Unknown action '{action}'. Use: add, patch, remove"
        ));
    }
    if let Err(err) = validate_single(store, target, action, content, old_text) {
        return err.to_string();
    }
    match apply_single(store, target, action, content, old_text) {
        Ok(payload) => payload.to_string(),
        Err(err) => err.to_string(),
    }
}

/// Pre-store validation so an invalid write is rejected before touching the
/// files; missing `old_text` returns the inventory plus a retry instruction.
fn validate_single(
    store: &MemoryStore,
    target: Target,
    action: &str,
    content: Option<&str>,
    old_text: Option<&str>,
) -> Result<(), Value> {
    let entries = || match target {
        Target::Memory => store.memory_entries.clone(),
        Target::User => store.user_entries.clone(),
    };
    match action {
        "add" if content.is_none() => Err(json!({
            "success": false,
            "error": "Content is required for 'add' action."
        })),
        "patch" if content.is_none() => Err(json!({
            "success": false,
            "error": format!("content (the new text for the old_text span) is required for 'patch' action. {PATCH_DROP_HINT}")
        })),
        "remove" | "patch" if old_text.map_or(true, |o| o.trim().is_empty()) => Err(json!({
            "success": false,
            "error": format!(
                "'{action}' needs old_text -- a short unique substring of the entry to {action}. \
                 None was provided. Reissue the {action} with old_text set to part of one of the current_entries below."
            ),
            "current_entries": entries(),
            "usage": usage_of(store, target),
        })),
        _ => Ok(()),
    }
}

/// One validated op against the store. Ok is the success payload; Err is the
/// error payload (store failure classes: over_budget, no_match, ambiguous,
/// drift, read_failed, write_failed, missing_*).
fn apply_single(
    store: &mut MemoryStore,
    target: Target,
    action: &str,
    content: Option<&str>,
    old_text: Option<&str>,
) -> Result<Value, Value> {
    let outcome = match action {
        "add" => store.add(target, content.unwrap_or_default()),
        "patch" => store.patch(
            target,
            old_text.unwrap_or_default(),
            content.unwrap_or_default(),
        ),
        "remove" => store.remove(target, old_text.unwrap_or_default()),
        _ => {
            return Err(invalid_payload(&format!(
                "Unknown action '{action}'. Use: add, patch, remove"
            )))
        }
    };
    match outcome {
        Ok(out) => {
            let mut payload = json!({
                "success": true,
                "message": out.message,
                "usage": out.usage,
                "entry_count": out.entry_count,
            });
            let obj = payload.as_object_mut().unwrap();
            if let Some(e) = out.replaced_entry {
                obj.insert("replaced_entry".into(), Value::String(e));
            }
            if let Some(e) = out.patched_entry_after {
                obj.insert("patched_entry_after".into(), Value::String(e));
            }
            if let Some(e) = out.removed_entry {
                obj.insert("removed_entry".into(), Value::String(e));
            }
            Ok(payload)
        }
        Err(err) => {
            let mut payload = json!({ "success": false, "error": err.message });
            let obj = payload.as_object_mut().unwrap();
            if let Some(e) = err.current_entries {
                obj.insert("current_entries".into(), json!(e));
            }
            if let Some(u) = err.usage {
                obj.insert("usage".into(), Value::String(u));
            }
            if let Some(c) = err.closest_entries {
                obj.insert("closest_entries".into(), json!(c));
            }
            if let Some(b) = err.drift_backup {
                obj.insert("drift_backup".into(), Value::String(b));
            }
            Err(payload)
        }
    }
}

/// The batch shape: ops applied in order, all-or-nothing. On any failure the
/// pre-batch MEMORY.md/USER.md bytes are restored and the first failure is
/// reported with the batch-abort suffix (Python `_batch_failure`).
fn apply_batch(store: &mut MemoryStore, dir: &Path, target: Target, ops: &[Value]) -> String {
    if ops.is_empty() {
        return util_err("operations list is empty.");
    }
    // Snapshot both files so a failed batch can put the store back exactly.
    let (mem_path, user_path) = (dir.join("MEMORY.md"), dir.join("USER.md"));
    let snapshot: Vec<(PathBuf, Option<Vec<u8>>)> = vec![
        (mem_path.clone(), fs::read(&mem_path).ok()),
        (user_path.clone(), fs::read(&user_path).ok()),
    ];
    let had_entries = match target {
        Target::Memory => !store.memory_entries.is_empty(),
        Target::User => !store.user_entries.is_empty(),
    };

    for (index, op) in ops.iter().enumerate() {
        let op = match op {
            Value::Object(_) => op,
            _ => {
                return abort_batch(
                    &snapshot,
                    store,
                    target,
                    "operations entries must be objects.",
                    "invalid_args",
                    None,
                )
            }
        };
        // A nested target equal to the call's (or null) is redundant; only a
        // different one asks for two stores in one call, which a batch cannot.
        let nested = op.get("target").and_then(Value::as_str).unwrap_or("");
        if !nested.is_empty()
            && nested
                != if target == Target::User {
                    "user"
                } else {
                    "memory"
                }
        {
            return abort_batch(
                &snapshot,
                store,
                target,
                &format!(
                    "Operation {} names a different target than the one at the top level. \
                     One call writes one store: retry with separate calls for different stores. \
                     Nothing was written or staged.",
                    index + 1
                ),
                "invalid_args",
                None,
            );
        }
        let action = op.get("action").and_then(Value::as_str).unwrap_or("");
        if action == "replace" {
            return abort_batch(
                &snapshot,
                store,
                target,
                REPLACE_REMOVED,
                "invalid_args",
                None,
            );
        }
        if action != "add" && action != "patch" && action != "remove" {
            return abort_batch(
                &snapshot,
                store,
                target,
                &format!("Unknown action '{action}'. Use: add, patch, remove"),
                "invalid_args",
                None,
            );
        }
        let content = op
            .get("content")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .or_else(|| {
                op.get("new_text")
                    .and_then(Value::as_str)
                    .filter(|c| !c.is_empty())
            });
        let old_text = op.get("old_text").and_then(Value::as_str);
        if let Err(err) = validate_single(store, target, action, content, old_text) {
            let msg = err
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("invalid operation")
                .to_string();
            return abort_batch(&snapshot, store, target, &msg, "invalid_args", None);
        }
        if let Err(err) = apply_single(store, target, action, content, old_text) {
            let msg = err
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("operation failed")
                .to_string();
            let closest = err.get("closest_entries").cloned();
            let class = if err.get("current_entries").is_some() {
                "over_budget"
            } else {
                "no_match"
            };
            return abort_batch(&snapshot, store, target, &msg, class, closest);
        }
    }
    // #103419: a consolidation batch that removes the last entry would commit
    // an empty file as a normal successful write. Refuse; a batch of exactly
    // one remove is the deliberate single delete (upstream faef4e0ab8).
    let now_empty = match target {
        Target::Memory => store.memory_entries.is_empty(),
        Target::User => store.user_entries.is_empty(),
    };
    if had_entries
        && now_empty
        && !(ops.len() == 1 && ops[0].get("action").and_then(Value::as_str) == Some("remove"))
    {
        let label = if target == Target::User { "USER.md" } else { "MEMORY.md" };
        return abort_batch(
            &snapshot,
            store,
            target,
            &format!(
                "Refusing to empty {label}: this batch would remove every entry from a \
                 previously non-empty store. Keep at least one entry — merge overlapping \
                 entries into a shorter one instead of removing the last one. To delete the \
                 final entry deliberately, remove it alone in a one-item batch."
            ),
            "would_empty",
            None,
        );
    }
    json!({
        "success": true,
        "message": format!("{} operation(s) applied.", ops.len()),
        "usage": usage_of(store, target),
        "entry_count": match target {
            Target::Memory => store.memory_entries.len(),
            Target::User => store.user_entries.len(),
        },
    })
    .to_string()
}

/// Restore the snapshot, reload the store and return the batch-abort error.
/// No `current_entries` on batch failures — the store is unchanged and the
/// caller already holds the inventory (#97316).
fn abort_batch(
    snapshot: &[(PathBuf, Option<Vec<u8>>)],
    store: &mut MemoryStore,
    target: Target,
    message: &str,
    _failure_class: &str,
    closest_entries: Option<Value>,
) -> String {
    for (path, bytes) in snapshot {
        match bytes {
            Some(raw) => {
                let _ = fs::write(path, raw);
            }
            None => {
                let _ = fs::remove_file(path);
            }
        }
    }
    let _ = store.load_from_disk();
    let mut payload = json!({
        "success": false,
        "error": format!("{message} No operations were applied (batch is all-or-nothing)."),
        "usage": usage_of(store, target),
    });
    if let Some(closest) = closest_entries {
        payload
            .as_object_mut()
            .unwrap()
            .insert("closest_entries".into(), closest);
    }
    payload.to_string()
}

fn usage_of(store: &MemoryStore, target: Target) -> String {
    let entries: Vec<String> = match target {
        Target::Memory => store.memory_entries.clone(),
        Target::User => store.user_entries.clone(),
    };
    let limit = if target == Target::User {
        store.user_char_limit
    } else {
        store.memory_char_limit
    };
    let count = ENTRY_DELIMITER.chars().count() * entries.len().saturating_sub(1)
        + entries.iter().map(|e| e.chars().count()).sum::<usize>();
    format!("{count}/{limit} chars")
}

fn invalid_payload(message: &str) -> Value {
    json!({ "success": false, "error": message })
}

fn util_err(message: &str) -> String {
    crate::util::tool_error(message)
}

/// Re-exported for tests: parse the §-delimited store format.
pub fn parse_store_entries(raw: &str) -> Vec<String> {
    parse_entries(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir() -> PathBuf {
        let id = NEXT_TMP.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("hermes-tools-mem-{}-{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn parse(out: &str) -> Value {
        serde_json::from_str(out).unwrap()
    }

    #[test]
    fn add_patch_remove_roundtrip_via_store() {
        let dir = tmp_dir();
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "action": "add", "content": "likes tea"}),
        );
        let parsed = parse(&out);
        assert_eq!(parsed["success"], true, "{out}");
        assert_eq!(parsed["entry_count"], 1);

        // duplicate add is a no-op success
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "action": "add", "content": "likes tea"}),
        );
        assert!(parse(&out)["message"]
            .as_str()
            .unwrap()
            .contains("already exists"));

        // patch replaces only the span
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "action": "patch", "old_text": "tea", "new_text": "coffee"}),
        );
        assert_eq!(parse(&out)["patched_entry_after"], "likes coffee");

        // remove the whole entry
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "action": "remove", "old_text": "likes coffee"}),
        );
        assert_eq!(parse(&out)["removed_entry"], "likes coffee");
        assert_eq!(parse(&out)["entry_count"], 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_text_aliases_content_and_missing_target_is_error() {
        let dir = tmp_dir();
        let out = execute_in(
            &dir,
            &json!({"target": "user", "action": "add", "new_text": "name is Ada"}),
        );
        assert_eq!(parse(&out)["success"], true);
        assert!(dir.join("USER.md").exists());

        let out = execute_in(&dir, &json!({"action": "add", "content": "x"}));
        let parsed = parse(&out);
        assert_eq!(parsed["success"], false);
        assert!(parsed["error"]
            .as_str()
            .unwrap()
            .contains("Missing required target"));

        let out = execute_in(
            &dir,
            &json!({"target": "wallet", "action": "add", "content": "x"}),
        );
        assert!(parse(&out)["error"]
            .as_str()
            .unwrap()
            .contains("Invalid memory target 'wallet'"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_would_empty_guard_and_one_item_remove_exemption() {
        let dir = tmp_dir();
        // Seed two entries, then a batch removing both is refused with would_empty.
        execute_in(&dir, &json!({"target": "memory", "action": "add", "content": "entry one"}));
        execute_in(&dir, &json!({"target": "memory", "action": "add", "content": "entry two"}));
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "operations": [
                {"action": "remove", "old_text": "entry one"},
                {"action": "remove", "old_text": "entry two"},
            ]}),
        );
        let parsed = parse(&out);
        assert_eq!(parsed["success"], false, "{out}");
        assert!(parsed["error"].as_str().unwrap().contains("Refusing to empty MEMORY.md"), "{out}");
        // The store is unchanged and both entries are still there.
        let raw = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert_eq!(parse_entries(&raw), vec!["entry one", "entry two"]);
        // A one-item remove batch IS the deliberate delete and succeeds.
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "operations": [{"action": "remove", "old_text": "entry one"}]}),
        );
        assert_eq!(parse(&out)["success"], true, "{out}");
        let raw = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert_eq!(parse_entries(&raw), vec!["entry two"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_op_validation_errors() {
        let dir = tmp_dir();
        execute_in(
            &dir,
            &json!({"target": "memory", "action": "add", "content": "entry one"}),
        )
        .to_string();

        // add without content
        let out = execute_in(&dir, &json!({"target": "memory", "action": "add"}));
        assert_eq!(
            parse(&out)["error"],
            "Content is required for 'add' action."
        );

        // patch without old_text -> inventory + retry instruction
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "action": "patch", "content": "x"}),
        );
        let parsed = parse(&out);
        assert!(parsed["error"].as_str().unwrap().contains("needs old_text"));
        assert_eq!(parsed["current_entries"], json!(["entry one"]));

        // replace is retired
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "action": "replace", "old_text": "a", "content": "b"}),
        );
        assert!(parse(&out)["error"]
            .as_str()
            .unwrap()
            .contains("'replace' is not available"));

        // unknown action
        let out = execute_in(&dir, &json!({"target": "memory", "action": "frobnicate"}));
        assert!(parse(&out)["error"]
            .as_str()
            .unwrap()
            .contains("Unknown action 'frobnicate'"));

        // neither action nor operations
        let out = execute_in(&dir, &json!({"target": "memory"}));
        assert!(parse(&out)["error"]
            .as_str()
            .unwrap()
            .contains("Missing required parameter"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_applies_in_order_and_reports_usage() {
        let dir = tmp_dir();
        execute_in(
            &dir,
            &json!({"target": "memory", "action": "add", "content": "stale entry"}),
        );
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "operations": [
                {"action": "remove", "old_text": "stale entry"},
                {"action": "add", "content": "fresh entry"}
            ]}),
        );
        let parsed = parse(&out);
        assert_eq!(parsed["success"], true, "{out}");
        assert_eq!(parsed["entry_count"], 1);
        let raw = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert_eq!(parse_store_entries(&raw), vec!["fresh entry"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_is_all_or_nothing() {
        let dir = tmp_dir();
        execute_in(
            &dir,
            &json!({"target": "memory", "action": "add", "content": "keep me"}),
        );
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "operations": [
                {"action": "add", "content": "added anyway?"},
                {"action": "remove", "old_text": "does not exist"}
            ]}),
        );
        let parsed = parse(&out);
        assert_eq!(parsed["success"], false, "{out}");
        assert!(parsed["error"]
            .as_str()
            .unwrap()
            .contains("No operations were applied (batch is all-or-nothing)"));
        // The first op must NOT have stuck.
        assert!(dir.join("MEMORY.md").exists());
        let raw = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert_eq!(parse_store_entries(&raw), vec!["keep me"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn over_budget_failure_carries_entries_and_usage() {
        let dir = tmp_dir();
        // Default limit is 2200 chars; one long entry plus one more overflows.
        let long = "x".repeat(2190);
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "action": "add", "content": long}),
        );
        assert_eq!(parse(&out)["success"], true);
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "action": "add", "content": "overflow entry"}),
        );
        let parsed = parse(&out);
        assert_eq!(parsed["success"], false);
        assert!(
            parsed["error"]
                .as_str()
                .unwrap()
                .contains("would exceed the limit"),
            "{out}"
        );
        assert!(parsed["current_entries"].is_array());
        assert!(parsed["usage"].as_str().unwrap().contains("chars"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_match_failure_suggests_closest_entries() {
        let dir = tmp_dir();
        execute_in(
            &dir,
            &json!({"target": "memory", "action": "add", "content": "deploy steps: build, test, ship"}),
        );
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "action": "remove", "old_text": "deploy steps: build, test, send"}),
        );
        let parsed = parse(&out);
        assert_eq!(parsed["success"], false);
        assert!(parsed["error"]
            .as_str()
            .unwrap()
            .contains("No entry matched"));
        assert!(parsed["closest_entries"].is_array(), "{out}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_and_memory_targets_are_independent() {
        let dir = tmp_dir();
        execute_in(
            &dir,
            &json!({"target": "user", "action": "add", "content": "user fact"}),
        );
        execute_in(
            &dir,
            &json!({"target": "memory", "action": "add", "content": "agent fact"}),
        );
        assert_eq!(
            parse_store_entries(&fs::read_to_string(dir.join("USER.md")).unwrap()),
            vec!["user fact"]
        );
        assert_eq!(
            parse_store_entries(&fs::read_to_string(dir.join("MEMORY.md")).unwrap()),
            vec!["agent fact"]
        );
        // A batch cannot mix stores.
        let out = execute_in(
            &dir,
            &json!({"target": "memory", "operations": [{"action": "add", "content": "x", "target": "user"}]}),
        );
        assert!(parse(&out)["error"]
            .as_str()
            .unwrap()
            .contains("different target"));
        let _ = fs::remove_dir_all(&dir);
    }
}
