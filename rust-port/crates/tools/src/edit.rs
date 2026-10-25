//! edit (patch) — exact-string find-and-replace.
//!
//! Port of `patch_tool` (mode="replace") from the Python hermes-agent,
//! restricted to the contract's exact matching (the Python fuzzy matcher is a
//! shell-layer nicety; uniqueness semantics are kept identical):
//! - `old_string` must appear EXACTLY once unless `replace_all` is true;
//! - `new_string` must differ from `old_string`;
//! - a count of replacements and the edited file path are reported.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;

use crate::read_file::{expand_tilde, path_display};
use crate::util::tool_error;

pub fn execute(args: &Value) -> String {
    let get = |k: &str| args.get(k).and_then(Value::as_str);
    let (path, old, new) = match (get("path"), get("old_string"), get("new_string")) {
        (Some(p), Some(o), Some(n)) => (p, o, n),
        _ => {
            return tool_error(
                "patch: 'path', 'old_string' and 'new_string' are required",
            )
        }
    };
    let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);

    if old == new {
        return tool_error(
            "patch: new_string must differ from old_string (no-op edit refused)",
        );
    }
    match patch(Path::new(path), old, new, replace_all) {
        Ok((resolved, count)) => json!({
            "success": true,
            "path": path,
            "resolved_path": path_display(&resolved),
            "replacements": count,
            "files_modified": [path_display(&resolved)],
        })
        .to_string(),
        Err(e) => tool_error(e),
    }
}

fn patch(
    path: &Path,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(std::path::PathBuf, usize), String> {
    let p = expand_tilde(&path_display(path));
    let content = fs::read_to_string(&p)
        .map_err(|e| format!("Cannot read '{path:?}': {e}"))?;

    if old.is_empty() {
        return Err("patch: old_string must be non-empty".into());
    }
    let count = content.matches(old).count();
    if count == 0 {
        return Err(
            "Could not find old_string in the file. Use read_file to verify the \
             current content, or search to locate the text."
                .into(),
        );
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "old_string appears {count} times; it must be unique in the file. \
             Include more surrounding context, or pass replace_all=true."
        ));
    }
    let patched = content.replace(old, new);
    crate::write_file::write_all(&p, &patched)
        .map_err(|e| format!("write failed for {path:?}: {e}"))?;
    Ok((p, count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_file::unique_suffix;
    use serde_json::json;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-tools-edit-{}",
            unique_suffix(name)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(path: &Path, args: Value) -> Value {
        let mut all = json!({"path": path_display(path)});
        if let Value::Object(b) = args {
            for (k, v) in b {
                all[k] = v;
            }
        }
        let out = execute(&all);
        serde_json::from_str(&out).unwrap()
    }

    #[test]
    fn replaces_unique_match() {
        let dir = tmpdir("unique");
        let file = dir.join("f.txt");
        fs::write(&file, "hello world\n").unwrap();
        let v = run(&file, json!({"old_string": "world", "new_string": "rust"}));
        assert_eq!(v["success"], true);
        assert_eq!(v["replacements"], 1);
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello rust\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn refuses_ambiguous_match_without_replace_all() {
        let dir = tmpdir("ambig");
        let file = dir.join("f.txt");
        fs::write(&file, "a-b-a\n").unwrap();
        let v = run(&file, json!({"old_string": "a", "new_string": "z"}));
        assert!(v["error"].as_str().unwrap().contains("2 times"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "a-b-a\n"); // untouched
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn replace_all_hits_every_occurrence() {
        let dir = tmpdir("all");
        let file = dir.join("f.txt");
        fs::write(&file, "a-b-a\n").unwrap();
        let v = run(&file, json!({"old_string": "a", "new_string": "z", "replace_all": true}));
        assert_eq!(v["success"], true);
        assert_eq!(v["replacements"], 2);
        assert_eq!(fs::read_to_string(&file).unwrap(), "z-b-z\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn deletes_match_with_empty_new_string() {
        let dir = tmpdir("del");
        let file = dir.join("f.txt");
        fs::write(&file, "keep <noise/> keep\n").unwrap();
        let v = run(&file, json!({"old_string": " <noise/>", "new_string": ""}));
        assert_eq!(v["success"], true);
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep keep\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn identical_strings_refused() {
        let dir = tmpdir("same");
        let file = dir.join("f.txt");
        fs::write(&file, "abc\n").unwrap();
        let v = run(&file, json!({"old_string": "abc", "new_string": "abc"}));
        assert!(v["error"].as_str().unwrap().contains("differ"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_old_string_reports_hint() {
        let dir = tmpdir("nf");
        let file = dir.join("f.txt");
        fs::write(&file, "abc\n").unwrap();
        let v = run(&file, json!({"old_string": "xyz", "new_string": "z"}));
        assert!(v["error"].as_str().unwrap().contains("Could not find"));
        let _ = fs::remove_dir_all(dir);
    }
}
