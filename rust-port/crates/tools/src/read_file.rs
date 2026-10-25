//! read_file — paginated, line-numbered reads.
//!
//! Port of `read_file_tool` / `FileOperations.read_file` from the Python
//! hermes-agent. Output contract (must stay identical for the model):
//! `{"content": "1|line\n2|line", "total_lines": N, "file_size": B,
//!   "truncated": bool, "hint": "...", "next_offset": M}`.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

use crate::util::{
    add_line_numbers, normalize_read_pagination, tool_error,
};

/// Read a file with 1-indexed `offset`/`limit` pagination and an `N|` gutter.
///
/// Errors (missing path, not a file, not UTF-8) come back as
/// `{"error": ...}` JSON, matching the Python handler's `tool_error` shape.
pub fn execute(args: &Value) -> String {
    let path = match args.get("path").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return tool_error("read_file: 'path' is required"),
    };
    let (offset, limit) = normalize_read_pagination(
        args.get("offset").and_then(Value::as_i64),
        args.get("limit").and_then(Value::as_i64),
    );
    match read(&path, offset, limit) {
        Ok(v) => v.to_string(),
        Err(e) => tool_error(format!("File not found: {path} ({e})")),
    }
}

fn read(path: &str, offset: usize, limit: usize) -> Result<Value, std::io::Error> {
    let p: PathBuf = expand_tilde(path);
    let meta = fs::metadata(&p)?;
    if meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "is a directory",
        ));
    }
    let raw = fs::read_to_string(&p)?;
    let file_size = meta.len();
    // `cat -n` semantics: a trailing newline terminates the last line.
    let text = raw.strip_suffix('\n').unwrap_or(&raw);
    let total_lines = if text.is_empty() && raw.is_empty() { 0 } else { text.split('\n').count() };

    let start = offset.saturating_sub(1);
    let lines: Vec<&str> = text.split('\n').skip(start).take(limit).collect();
    let page = lines.join("\n");
    let end_line = start + lines.len();
    let truncated = end_line < total_lines;

    let mut result = json!({
        "content": add_line_numbers(&page, offset),
        "total_lines": total_lines,
        "file_size": file_size,
        "truncated": truncated,
    });
    let obj = result.as_object_mut().unwrap();
    if truncated {
        let next = end_line + 1;
        obj.insert(
            "hint".into(),
            json!(format!(
                "Use offset={next} to continue reading (showing {offset}-{end_line} of {total_lines} lines)"
            )),
        );
        obj.insert("next_offset".into(), json!(next));
    }
    if offset > 1 {
        obj.insert("offset".into(), json!(offset));
    }
    Ok(result)
}

/// `~/`-expansion against $HOME (Python `_expand_tilde`). Windows/Unix both.
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = home_dir() {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
}

pub(crate) fn path_display(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Unique-per-call temp dir name component so parallel #[test]s never share
/// a directory (tests in one module run concurrently on one pid).
#[cfg(test)]
pub(crate) fn unique_suffix(name: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{name}-{}", n ^ (std::process::id() as u64).rotate_left(16))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-tools-read-{}",
            unique_suffix(name)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(path: &Path, extra: Value) -> Value {
        let mut args = serde_json::Map::new();
        args.insert("path".into(), json!(path_display(path)));
        if let Value::Object(map) = extra {
            for (k, v) in map {
                args.insert(k, v);
            }
        }
        let out = execute(&Value::Object(args));
        serde_json::from_str(&out).unwrap()
    }

    #[test]
    fn paginates_with_line_numbers() {
        let dir = tmpdir("page");
        let file = dir.join("sample.txt");
        fs::write(&file, "l1\nl2\nl3\nl4\nl5\n").unwrap();

        // Default: whole file, 1-indexed gutter.
        let v = run(&file, json!({}));
        assert_eq!(v["content"], "1|l1\n2|l2\n3|l3\n4|l4\n5|l5");
        assert_eq!(v["total_lines"], 5);
        assert_eq!(v["file_size"], 15);
        assert_eq!(v["truncated"], false);

        // Page 2 with limit 2: offset/limit honored, hint offers next offset.
        let v = run(&file, json!({"offset": 3, "limit": 2}));
        assert_eq!(v["content"], "3|l3\n4|l4");
        assert_eq!(v["truncated"], true);
        assert_eq!(v["next_offset"], 5);
        assert!(v["hint"].as_str().unwrap().contains("offset=5"));

        // Last page: no more truncation.
        let v = run(&file, json!({"offset": 5, "limit": 2}));
        assert_eq!(v["content"], "5|l5");
        assert_eq!(v["truncated"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn no_phantom_trailing_line() {
        let dir = tmpdir("trail");
        let file = dir.join("t.txt");
        fs::write(&file, "only\n").unwrap();
        let v = run(&file, json!({}));
        assert_eq!(v["content"], "1|only");
        assert_eq!(v["total_lines"], 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_file_reads_empty() {
        let dir = tmpdir("empty");
        let file = dir.join("e.txt");
        fs::write(&file, "").unwrap();
        let v = run(&file, json!({}));
        assert_eq!(v["content"], "");
        assert_eq!(v["total_lines"], 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_is_tool_error() {
        let dir = tmpdir("miss");
        let v = run(&dir.join("nope.txt"), json!({}));
        assert!(v["error"].as_str().unwrap().contains("File not found"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn directory_read_is_an_error() {
        let dir = tmpdir("isdir");
        let v = run(&dir, json!({}));
        assert!(v.get("error").is_some());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn offset_zero_is_clamped_not_panicking() {
        let dir = tmpdir("clamp");
        let file = dir.join("c.txt");
        fs::write(&file, "a\nb\n").unwrap();
        let v = run(&file, json!({"offset": 0, "limit": 0}));
        assert_eq!(v["content"], "1|a");
        let _ = fs::remove_dir_all(dir);
    }
}
