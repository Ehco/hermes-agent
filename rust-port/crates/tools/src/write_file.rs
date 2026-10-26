//! write_file — create/overwrite a file, creating parent directories.
//!
//! Port of `write_file_tool` / `FileOperations.write_file` from the Python
//! hermes-agent (minus the task-tracker machinery, which has no Rust session
//! counterpart yet). The write replaces the ENTIRE file.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;

use crate::read_file::{expand_tilde, path_display};
use crate::util::tool_error;

/// Write `content` to `path`, creating parent directories as needed.
pub fn execute(args: &Value) -> String {
    let (path, content) = match (args.get("path"), args.get("content")) {
        (Some(Value::String(p)), Some(Value::String(c))) => (p, c),
        (Some(_), Some(v)) => {
            return tool_error(format!(
                "write_file: 'content' must be a string, got {}.",
                type_name(v)
            ))
        }
        _ => return tool_error("write_file: missing required field 'path' or 'content'"),
    };
    match write(path, content) {
        Ok(resolved) => json!({
            "success": true,
            "path": path,
            "resolved_path": path_display(&resolved),
            "bytes_written": content.len(),
        })
        .to_string(),
        Err(e) => tool_error(format!("write_file failed for {path:?}: {e}")),
    }
}

fn write(path: &str, content: &str) -> Result<std::path::PathBuf, std::io::Error> {
    let p = expand_tilde(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(&p, content)?;
    Ok(p)
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::Object(_) => "object",
        Value::Array(_) => "array",
        Value::String(_) => "string",
    }
}

/// Direct access for other tools/tests.
pub(crate) fn write_all(path: &Path, content: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_file::unique_suffix;
    use serde_json::json;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hermes-tools-write-{}", unique_suffix(name)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(path: &Path, content: &str) -> Value {
        let out = execute(&json!({"path": path_display(path), "content": content}));
        serde_json::from_str(&out).unwrap()
    }

    #[test]
    fn creates_and_overwrites() {
        let dir = tmpdir("basic");
        let file = dir.join("a.txt");

        // Create.
        let v = run(&file, "hello");
        assert_eq!(v["success"], true);
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello");

        // Overwrite entirely.
        let v = run(&file, "second");
        assert_eq!(v["success"], true);
        assert_eq!(fs::read_to_string(&file).unwrap(), "second");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn creates_parent_directories() {
        let dir = tmpdir("parents");
        let file = dir.join("deep/nested/dir/file.txt");
        let v = run(&file, "x");
        assert_eq!(v["success"], true);
        assert!(file.is_file());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_content_truncates() {
        let dir = tmpdir("trunc");
        let file = dir.join("t.txt");
        let _ = run(&file, "gone soon");
        let v = run(&file, "");
        assert_eq!(v["success"], true);
        assert_eq!(fs::read_to_string(&file).unwrap(), "");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_args_are_errors() {
        let out = execute(&json!({"path": "x"}));
        assert!(out.contains("error"));
        let out = execute(&json!({"content": "x"}));
        assert!(out.contains("error"));
        // Non-string content is rejected.
        let out = execute(&json!({"path": "x", "content": 42}));
        assert!(out.contains("must be a string"));
    }
}
