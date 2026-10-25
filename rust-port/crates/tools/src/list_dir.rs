//! list_dir — directory listing with entry sizes.
//!
//! Faithful to Python `search_files(target='files')` / `ls` replacement
//! behavior the agent relies on: entries with kind + size, deterministic
//! order, bounded by `limit`, and errors surfaced as `{"error": ...}`.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;

use crate::read_file::{expand_tilde, path_display};
use crate::util::tool_error;

pub fn execute(args: &Value) -> String {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .unwrap_or(".");
    let dir = expand_tilde(path);
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(1000)
        .clamp(1, 10_000) as usize;

    match list(&dir, limit) {
        Ok(v) => v.to_string(),
        Err(e) => tool_error(format!("Path not found: {path} ({e})")),
    }
}

fn list(dir: &Path, limit: usize) -> Result<Value, std::io::Error> {
    let meta = fs::metadata(dir)?;
    if !meta.is_dir() {
        // A single file path lists itself (like `ls file`).
        return Ok(json!({
            "path": path_display(dir),
            "entries": [{
                "name": dir.file_name().map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "type": "file",
                "size": meta.len(),
            }],
            "truncated": false,
        }));
    }

    let mut entries: Vec<Value> = Vec::new();
    let mut truncated = false;
    for entry in fs::read_dir(dir)? {
        if entries.len() >= limit {
            truncated = true;
            break;
        }
        let entry = entry?;
        let ft = entry.file_type()?;
        let meta = entry.metadata()?;
        let kind = if ft.is_dir() {
            "dir"
        } else if ft.is_symlink() {
            "symlink"
        } else {
            "file"
        };
        // `size` is the byte size for files; 0 for directories/symlinks, the
        // same convention Python's list_dir result used.
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "type": kind,
            "size": if ft.is_file() { meta.len() } else { 0 },
        }));
    }
    // Deterministic output: name order, case-insensitive.
    entries.sort_by(|a, b| {
        let an = a["name"].as_str().unwrap_or_default().to_lowercase();
        let bn = b["name"].as_str().unwrap_or_default().to_lowercase();
        an.cmp(&bn)
    });

    Ok(json!({
        "path": path_display(dir),
        "entries": entries,
        "count": entries.len(),
        "truncated": truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_file::unique_suffix;
    use serde_json::json;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-tools-ls-{}",
            unique_suffix(name)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(path: &Path, extra: Value) -> Value {
        let mut all = json!({"path": path_display(path)});
        if let Value::Object(map) = extra {
            for (k, v) in map {
                all[k] = v;
            }
        }
        let out = execute(&all);
        serde_json::from_str(&out).unwrap()
    }

    #[test]
    fn lists_entries_with_sizes() {
        let dir = tmpdir("entries");
        fs::write(dir.join("b.txt"), "12345").unwrap(); // 5 bytes
        fs::write(dir.join("a.txt"), "1").unwrap(); // 1 byte
        fs::create_dir(dir.join("sub")).unwrap();

        let v = run(&dir, json!({}));
        assert!(v.get("error").is_none(), "{}", v);
        let entries = v["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        // Deterministic name order: a.txt < b.txt < sub
        assert_eq!(entries[0]["name"], "a.txt");
        assert_eq!(entries[0]["size"], 1);
        assert_eq!(entries[1]["name"], "b.txt");
        assert_eq!(entries[1]["size"], 5);
        assert_eq!(entries[2]["type"], "dir");
        assert_eq!(v["count"], 3);
        assert_eq!(v["truncated"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn limit_truncates() {
        let dir = tmpdir("limit");
        for i in 0..5 {
            fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        let v = run(&dir, json!({"limit": 2}));
        assert_eq!(v["entries"].as_array().unwrap().len(), 2);
        assert_eq!(v["truncated"], true);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_dir_is_tool_error() {
        let dir = tmpdir("miss");
        let v = run(&dir.join("nope"), json!({}));
        assert!(v["error"].as_str().unwrap().contains("Path not found"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn file_path_lists_itself() {
        let dir = tmpdir("self");
        let file = dir.join("only.txt");
        fs::write(&file, "abc").unwrap();
        let v = run(&file, json!({}));
        let entries = v["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["type"], "file");
        assert_eq!(entries[0]["size"], 3);
        let _ = fs::remove_dir_all(dir);
    }
}
