//! todo_list — in-memory, revisioned task list for multi-step work. Port of
//! `tools/todo_tool.py`: the `TodoStore` (validate / dedupe / order-normalize
//! / parent sanitization / merge-by-id / monotonic revision) plus the tool
//! dispatcher — pass `todos` to write (replace, or `merge` by id), omit to
//! read; every call returns the full list + counts.
//!
//! The Python store lives on the AIAgent (one per session); here it is a
//! process-global held by the tool module (one per registry process), so UI
//! revision checks behave the same way. `reset()` restores the fresh state.

use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};

/// Caps (`todo_tool.py`): content truncation keeps the actionable head; the
/// list is truncated to the priority head on write.
pub const MAX_TODO_CONTENT_CHARS: usize = 4000;
pub const MAX_TODO_ITEMS: usize = 256;
const TRUNCATION_MARKER: &str = "… [truncated]";
const VALID_STATUSES: [&str; 4] = ["pending", "in_progress", "completed", "cancelled"];

/// One item: `{id, content, status, parent?}` — list position is priority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
    pub parent: Option<String>,
}

impl TodoItem {
    fn to_json(&self) -> Value {
        let mut obj = json!({ "id": self.id, "content": self.content, "status": self.status });
        if let Some(parent) = &self.parent {
            obj.as_object_mut()
                .unwrap()
                .insert("parent".into(), Value::String(parent.clone()));
        }
        obj
    }
}

/// In-memory todo list, one per agent process.
#[derive(Default)]
pub struct TodoStore {
    items: Vec<TodoItem>,
    revision: u64,
}

impl TodoStore {
    pub fn new() -> Self {
        TodoStore {
            items: Vec::new(),
            revision: 0,
        }
    }

    pub fn read(&self) -> Vec<TodoItem> {
        self.items.clone()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replace the list (default) or merge by id; returns the full list after
    /// writing. Err leaves the list untouched (rejected writes never leave a
    /// partial list behind).
    pub fn write(&mut self, todos: &[Value], merge: bool) -> Result<Vec<TodoItem>, String> {
        let before = self.read();
        let result = if merge {
            self.merge(todos)
        } else {
            self.fresh_items(todos)
        };
        let mut items = match result {
            Ok(items) => items,
            Err(e) => {
                self.items = before;
                return Err(e);
            }
        };
        items.truncate(MAX_TODO_ITEMS); // keep the priority head
        sanitize_parents(&mut items);
        self.items = items;
        if self.items != before {
            self.revision += 1;
        }
        Ok(self.read())
    }

    /// Validate, dedupe and order a whole new list.
    fn fresh_items(&self, todos: &[Value]) -> Result<Vec<TodoItem>, String> {
        let mut deduped: Vec<Value> = Vec::new();
        // Collapse duplicate ids, keeping the LAST occurrence in its position.
        let mut last_index: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for (i, item) in todos.iter().enumerate() {
            let key = if item.is_object() {
                item.get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| "?".to_string())
            } else {
                format!("__invalid_{i}")
            };
            last_index.insert(key, i);
        }
        let mut positions: Vec<usize> = last_index.values().copied().collect();
        positions.sort_unstable();
        for &pos in &positions {
            deduped.push(todos[pos].clone());
        }
        let mut out = Vec::with_capacity(deduped.len());
        for item in &deduped {
            out.push(validate(item)?);
        }
        Ok(normalize_order(out))
    }

    /// Update existing items only in the fields provided; append new ones.
    fn merge(&mut self, todos: &[Value]) -> Result<Vec<TodoItem>, String> {
        for t in todos {
            let item_id = t
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            if item_id.is_empty() {
                continue; // can't merge without an id
            }
            let existing = self.items.iter().position(|i| i.id == item_id);
            match existing {
                None => {
                    self.items.push(validate(t)?);
                }
                Some(idx) => {
                    if let Some(content) = t.get("content").and_then(Value::as_str) {
                        let content = content.trim();
                        if !content.is_empty() {
                            self.items[idx].content = cap_content(content);
                        }
                    }
                    if let Some(status) = t.get("status").and_then(Value::as_str) {
                        let status = status.trim().to_lowercase();
                        if VALID_STATUSES.contains(&status.as_str()) {
                            self.items[idx].status = status;
                        }
                    }
                    if let Some(parent) = t.get("parent") {
                        let parent = parent.as_str().map(str::trim).unwrap_or("").to_string();
                        if !parent.is_empty() {
                            self.items[idx].parent = Some(parent);
                        } else {
                            self.items[idx].parent = None;
                        }
                    }
                }
            }
        }
        // Preserve original order for existing items (first occurrence wins).
        Ok(self.items.clone())
    }
}

/// Truncate to MAX_TODO_CONTENT_CHARS keeping the head + marker.
fn cap_content(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() > MAX_TODO_CONTENT_CHARS {
        let head: String = chars[..MAX_TODO_CONTENT_CHARS - TRUNCATION_MARKER.chars().count()]
            .iter()
            .collect();
        format!("{head}{TRUNCATION_MARKER}")
    } else {
        content.to_string()
    }
}

/// Normalize one item to `{id, content, status, parent?}` (placeholders when
/// missing). Empty/missing content is a rejected write.
fn validate(item: &Value) -> Result<TodoItem, String> {
    if !item.is_object() {
        return Ok(TodoItem {
            id: "?".into(),
            content: "(invalid item)".into(),
            status: "pending".into(),
            parent: None,
        });
    }
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("?");
    let content = item
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if content.is_empty() {
        return Err(format!(
            "Todo item '{item_id}' has empty or missing content. Each item must have a non-empty description."
        ));
    }
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending")
        .trim()
        .to_lowercase();
    let status = if VALID_STATUSES.contains(&status.as_str()) {
        status
    } else {
        "pending".into()
    };
    let parent = item
        .get("parent")
        .map(|p| p.as_str().map(str::trim).unwrap_or("").to_string())
        .unwrap_or_default();
    Ok(TodoItem {
        id: item_id.to_string(),
        content: cap_content(content),
        status,
        parent: if !parent.is_empty() && parent != item_id {
            Some(parent)
        } else {
            None
        },
    })
}

/// Drop dangling parent refs and break cycles in place (such items become
/// roots).
fn sanitize_parents(items: &mut [TodoItem]) {
    let ids: std::collections::HashSet<String> = items.iter().map(|i| i.id.clone()).collect();
    for item in items.iter_mut() {
        if let Some(p) = &item.parent {
            if !ids.contains(p) {
                item.parent = None;
            }
        }
    }
    for i in 0..items.len() {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        seen.insert(items[i].id.clone());
        let mut node = i;
        loop {
            let parent = items[node].parent.clone();
            let Some(parent) = parent else { break };
            if seen.contains(&parent) {
                items[i].parent = None;
                break;
            }
            seen.insert(parent.clone());
            match items.iter().position(|it| it.id == parent) {
                Some(next) => node = next,
                None => break,
            }
        }
    }
}

/// Lift the in_progress step ahead of any earlier pending placeholder. Nested
/// lists keep authored order — reordering would tear a subtask from its
/// siblings.
fn normalize_order(items: Vec<TodoItem>) -> Vec<TodoItem> {
    let has_parent = items.iter().any(|i| i.parent.is_some());
    let active = items.iter().position(|i| i.status == "in_progress");
    let (Some(active), false) = (active, has_parent) else {
        return items;
    };
    let first_pending = items.iter().position(|i| i.status == "pending");
    let Some(first_pending) = first_pending else {
        return items;
    };
    if first_pending > active {
        return items;
    }
    let mut normalized = items;
    let item = normalized.remove(active);
    normalized.insert(first_pending, item);
    normalized
}

fn store() -> &'static Mutex<TodoStore> {
    static STORE: OnceLock<Mutex<TodoStore>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(TodoStore::new()))
}

/// Test hook: clear the process-global store. Returns the test-lock guard so
/// parallel tests serialize on the store for the guard's lifetime.
#[cfg(test)]
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub fn reset() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock().unwrap();
    *store().lock().unwrap() = TodoStore::new();
    guard
}

/// The JSON type name for a non-list `todos` value (`type(x).__name__`).
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Execute `todo_list` — the registry entry point (JSON string in, out).
pub fn execute(args: &Value) -> String {
    let mut guard = store().lock().unwrap();
    let items: Vec<TodoItem> = match args.get("todos") {
        None | Some(Value::Null) => guard.read(),
        Some(Value::Array(todos)) => match guard.write(
            todos,
            args.get("merge").and_then(Value::as_bool).unwrap_or(false),
        ) {
            Ok(items) => items,
            Err(e) => return crate::util::tool_error(e),
        },
        Some(other) => {
            return crate::util::tool_error(format!(
                "todos must be a list, got {}",
                json_type_name(other)
            ));
        }
    };
    let mut summary = json!({ "total": items.len() });
    for status in VALID_STATUSES {
        let count = items.iter().filter(|i| i.status == status).count();
        summary
            .as_object_mut()
            .unwrap()
            .insert(status.to_string(), json!(count));
    }
    json!({
        "todos": items.iter().map(TodoItem::to_json).collect::<Vec<_>>(),
        "revision": guard.revision(),
        "summary": summary,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(id: &str, content: &str, status: &str) -> Value {
        json!({"id": id, "content": content, "status": status})
    }

    fn ids(items: &[TodoItem]) -> Vec<String> {
        items.iter().map(|i| i.id.clone()).collect()
    }

    #[test]
    fn write_replace_read_and_revision_bump() {
        let _guard = reset();
        let out = v(&execute(&json!({"todos": [
            item("1", "first", "pending"),
            item("2", "second", "in_progress"),
        ]})));
        assert_eq!(out["revision"], 1);
        assert_eq!(out["todos"].as_array().unwrap().len(), 2);
        assert_eq!(out["summary"]["total"], 2);
        assert_eq!(out["summary"]["in_progress"], 1);

        // A no-op write does not bump the revision.
        let before = out["revision"].clone();
        let out = v(&execute(&json!({"todos": [
            item("1", "first", "pending"),
            item("2", "second", "in_progress"),
        ]})));
        assert_eq!(out["revision"], before);
        drop(_guard);
        reset();
    }

    #[test]
    fn read_shape_and_counts() {
        let _guard = reset();
        execute(&json!({"todos": [item("1", "a", "completed"), item("2", "b", "pending")]}));
        let out = v(&execute(&json!({})));
        assert_eq!(out["todos"].as_array().unwrap().len(), 2);
        assert_eq!(out["summary"]["completed"], 1);
        assert_eq!(out["summary"]["pending"], 1);
        assert_eq!(out["summary"]["cancelled"], 0);
        drop(_guard);
        reset();
    }

    #[test]
    fn empty_content_is_rejected_and_list_untouched() {
        let _guard = reset();
        execute(&json!({"todos": [item("1", "kept", "pending")]}));
        let out = v(&execute(&json!({"todos": [item("2", "", "pending")]})));
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("empty or missing content"),
            "{}",
            out
        );
        let out = v(&execute(&json!({})));
        assert_eq!(todo_items(&out).len(), 1);
        drop(_guard);
        reset();
    }

    #[test]
    fn merge_updates_fields_and_appends() {
        let _guard = reset();
        execute(
            &json!({"todos": [item("1", "original", "pending"), item("2", "other", "pending")]}),
        );
        let out = v(&execute(&json!({
            "merge": true,
            "todos": [
                {"id": "1", "status": "completed"},
                {"id": "3", "content": "appended", "status": "pending"},
            ],
        })));
        let items = todo_items(&out);
        assert_eq!(items.len(), 3);
        let first = items.iter().find(|i| i["id"] == "1").unwrap();
        assert_eq!(first["content"], "original"); // content untouched when absent
        assert_eq!(first["status"], "completed");
        drop(_guard);
        reset();
    }

    #[test]
    fn in_progress_is_lifted_ahead_of_pending_placeholders() {
        let _guard = reset();
        let out = v(&execute(&json!({"todos": [
            item("1", "placeholder", "pending"),
            item("2", "active", "in_progress"),
            item("3", "later", "pending"),
        ]})));
        assert_eq!(
            todo_items(&out)
                .iter()
                .map(|i| i["id"].clone())
                .collect::<Vec<_>>(),
            vec![json!("2"), json!("1"), json!("3")]
        );
        drop(_guard);
        reset();
    }

    #[test]
    fn parents_sanitize_dangling_refs_and_cycles() {
        let _guard = reset();
        let out = v(&execute(&json!({"todos": [
            {"id": "1", "content": "root", "status": "pending", "parent": "missing"},
            {"id": "2", "content": "loop a", "status": "pending", "parent": "2"},
        ]})));
        let items = todo_items(&out);
        for item in items {
            assert!(item.get("parent").is_none(), "{}", item);
        }
        drop(_guard);
        reset();
    }

    #[test]
    fn duplicate_ids_keep_last_occurrence_in_position() {
        let _guard = reset();
        let out = v(&execute(&json!({"todos": [
            item("1", "first", "pending"),
            item("2", "second", "pending"),
            item("1", "last wins", "pending"),
        ]})));
        let items = todo_items(&out);
        assert_eq!(items.len(), 2);
        // Kept entries stay at their original positions: "2" (index 1) before
        // the last "1" (index 2).
        assert_eq!(items[0]["id"], "2");
        assert_eq!(items[1]["id"], "1");
        assert_eq!(items[1]["content"], "last wins");
        drop(_guard);
        reset();
    }

    #[test]
    fn long_content_is_capped_with_marker() {
        let _guard = reset();
        let long = "x".repeat(MAX_TODO_CONTENT_CHARS + 50);
        let out = v(&execute(
            &json!({"todos": [{"id": "1", "content": long, "status": "pending"}]}),
        ));
        let items = todo_items(&out);
        let content = items[0]["content"].as_str().unwrap();
        assert!(content.ends_with(TRUNCATION_MARKER));
        assert!(content.chars().count() <= MAX_TODO_CONTENT_CHARS);
        drop(_guard);
        reset();
    }

    #[test]
    fn non_list_todos_is_error() {
        let _guard = reset();
        let out = v(&execute(&json!({"todos": "nope"})));
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("todos must be a list, got str"));
        drop(_guard);
        reset();
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    fn todo_items(out: &Value) -> Vec<Value> {
        out["todos"].as_array().unwrap().clone()
    }
}
