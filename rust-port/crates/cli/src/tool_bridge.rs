//! Tool execution bridge for the CLI REPL.
//!
//! The `ToolHost` seam delegates here by tool name. The hermes-tools crate is a
//! parallel workstream whose registry is not published yet; once it exposes its
//! registry (its modules already follow the same `execute(args: &Value) ->
//! String` calling convention), each match arm below becomes a one-line
//! delegation, e.g. `"read_file" => hermes_tools::read_file::execute(args)`.
//! Until then the bridge implements the same tools with the same JSON contract.

use serde_json::{json, Value};

/// Execute a tool call; errors are surfaced to the model as `{"error": ...}`
/// JSON (the Python `tool_error` shape), never as panics.
pub fn bridge(name: &str, args: &Value) -> String {
    match name {
        "terminal" => terminal(args),
        "read_file" => read_file(args),
        "list_dir" => list_dir(args),
        other => json!({ "error": format!("unknown tool: {other}") }).to_string(),
    }
}

fn tool_error(message: impl std::fmt::Display) -> String {
    json!({ "error": message.to_string() }).to_string()
}

fn shell(command: &str) -> std::process::Command {
    #[cfg(windows)]
    let cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", command]);
        c
    };
    #[cfg(not(windows))]
    let cmd = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };
    cmd
}

fn terminal(args: &Value) -> String {
    let Some(command) = args.get("command").and_then(Value::as_str) else {
        return tool_error("terminal: 'command' is required");
    };
    let output = shell(command).output();
    match output {
        Ok(out) => {
            let mut report = format!(
                "exit code: {}\n--- stdout ---\n{}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout),
            );
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.trim().is_empty() {
                report.push_str(&format!("\n--- stderr ---\n{stderr}"));
            }
            report
        }
        Err(e) => tool_error(format!("terminal: failed to run command: {e}")),
    }
}

fn read_file(args: &Value) -> String {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return tool_error("read_file: 'path' is required");
    };
    let offset = args
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as usize;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(2000)
        .min(2000) as usize;
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let total = lines.len();
            let selected: Vec<&str> = lines.iter().skip(offset - 1).take(limit).copied().collect();
            let numbered: Vec<String> = selected
                .iter()
                .enumerate()
                .map(|(i, l)| format!("{}|{l}", offset + i))
                .collect();
            let truncated = offset - 1 + limit < total;
            let mut out = json!({
                "content": numbered.join("\n"),
                "total_lines": total,
                "truncated": truncated,
            });
            if truncated {
                out["next_offset"] = json!(offset + limit);
            }
            out.to_string()
        }
        Err(e) => tool_error(format!("read_file: {path}: {e}")),
    }
}

fn list_dir(args: &Value) -> String {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .unwrap_or(".");
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(1000)
        .clamp(1, 10_000) as usize;
    match std::fs::read_dir(path) {
        Ok(entries) => {
            let mut listing: Vec<Value> = Vec::new();
            for entry in entries.take(limit).flatten() {
                let file_type = match entry.file_type() {
                    Ok(t) if t.is_dir() => "dir",
                    Ok(_) => "file",
                    Err(_) => "?",
                };
                let size = entry.metadata().ok().map(|m| m.len()).unwrap_or(0);
                listing.push(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "kind": file_type,
                    "size": size,
                }));
            }
            listing.sort_by(|a, b| {
                let ka = a.get("name").and_then(Value::as_str).unwrap_or("");
                let kb = b.get("name").and_then(Value::as_str).unwrap_or("");
                ka.cmp(kb)
            });
            json!({ "path": path, "entries": listing }).to_string()
        }
        Err(e) => tool_error(format!("Path not found: {path} ({e})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_tools_error() {
        assert_eq!(
            bridge("nope", &json!({})),
            r#"{"error":"unknown tool: nope"}"#
        );
    }

    #[test]
    fn terminal_runs_a_command_and_reports_exit() {
        let out = bridge("terminal", &json!({ "command": "echo hi" }));
        assert!(out.contains("hi"), "got: {out}");
        assert!(out.contains("exit code: 0"));
    }

    #[test]
    fn read_file_numbering_and_pagination_contract() {
        let dir = std::env::temp_dir().join(format!("bridge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.txt");
        std::fs::write(&p, "a\nb\nc").unwrap();
        let out = bridge(
            "read_file",
            &json!({ "path": p.to_string_lossy(), "offset": 2, "limit": 1 }),
        );
        assert!(out.contains("\"2|b\""), "got: {out}");
        assert!(out.contains("\"total_lines\":3"));
        assert!(out.contains("\"next_offset\":3"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_args_surface_tool_errors() {
        assert!(bridge("read_file", &json!({})).contains("\"error\""));
        assert!(bridge("terminal", &json!({})).contains("\"error\""));
    }
}
