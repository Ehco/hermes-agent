//! Shared helpers for the hermes-tools crate: JSON error/result shaping,
//! pagination clamps, and a small glob matcher for `file_glob` filters.
//!
//! Port of the ubiquitous `tools.registry.tool_error` /
//! `tools.file_operations_common` pagination clamps from the Python agent.

use serde_json::Value;

/// Python `_MAX_TOOL_ERROR_CHARS` (tools/registry.py): tool error bodies are
/// bounded so a runaway exception can't bloat model context across retries.
pub const MAX_TOOL_ERROR_CHARS: usize = 2048;
pub const TOOL_ERROR_TRUNCATION_MARKER: &str = "… [truncated]";

/// Python `tools.tool_output_limits` defaults.
pub const MAX_LINES: usize = 2000;
pub const MAX_LINE_LENGTH: usize = 2000;
pub const DEFAULT_READ_LIMIT: usize = MAX_LINES;
pub const DEFAULT_SEARCH_LIMIT: usize = 50;
/// `terminal_tool.MAX_OUTPUT_CHARS` — terminal stdout/stderr cap per stream.
pub const MAX_TERMINAL_OUTPUT_CHARS: usize = 50_000;

/// `{"error": "<message>"}` — the Python `tool_error` boilerplate.
pub fn tool_error(message: impl AsRef<str>) -> String {
    tool_error_extra(message, None)
}

/// `tool_error(message, **extra)` — error plus extra fields.
pub fn tool_error_extra(message: impl AsRef<str>, extra: Option<Value>) -> String {
    let text = bound_error_text(message.as_ref());
    let mut obj = serde_json::Map::new();
    obj.insert("error".into(), Value::String(text));
    if let Some(Value::Object(map)) = extra {
        for (k, v) in map {
            obj.insert(k, v);
        }
    }
    Value::Object(obj).to_string()
}

/// Python `_bound_error_text`: cap an error body destined for model context.
fn bound_error_text(text: &str) -> String {
    if text.len() <= MAX_TOOL_ERROR_CHARS {
        return text.to_string();
    }
    let mut cut = MAX_TOOL_ERROR_CHARS;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{}", &text[..cut], TOOL_ERROR_TRUNCATION_MARKER)
}

/// Python `normalize_read_pagination`: 1-indexed offset, limit clamped to
/// `[1, MAX_LINES]`. Schemas declare bounds, but not every caller enforces
/// them, so clamp defensively (a `sed 0,-1p`-style range must never happen).
pub fn normalize_read_pagination(offset: Option<i64>, limit: Option<i64>) -> (usize, usize) {
    let offset = offset.unwrap_or(1).max(1) as usize;
    let limit = limit
        .unwrap_or(DEFAULT_READ_LIMIT as i64)
        .clamp(1, MAX_LINES as i64) as usize;
    (offset, limit)
}

/// Python `normalize_search_pagination`: 0-indexed offset, limit clamped to
/// `[1, MAX_LINES]` with a default of 50.
pub fn normalize_search_pagination(offset: Option<i64>, limit: Option<i64>) -> (usize, usize) {
    let offset = offset.unwrap_or(0).max(0) as usize;
    let limit = limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT as i64)
        .clamp(1, MAX_LINES as i64) as usize;
    (offset, limit)
}

/// Compact `<n>|` gutter for a page of lines — `FileOperations._add_line_numbers`.
///
/// A trailing newline terminates the final line; it does NOT start a phantom
/// empty `<N+1>|` line (`cat -n` semantics). Exactly one terminator is dropped.
/// Long lines are clamped with a `... [truncated]` marker.
pub fn add_line_numbers(content: &str, start_line: usize) -> String {
    if content.is_empty() {
        return String::new();
    }
    let content = content.strip_suffix('\n').unwrap_or(content);
    content
        .split('\n')
        .enumerate()
        .map(|(i, line)| {
            let display = if line.len() <= MAX_LINE_LENGTH {
                line
            } else {
                let mut cut = MAX_LINE_LENGTH;
                while !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                &line[..cut]
            };
            let truncated = if line.len() > MAX_LINE_LENGTH {
                "… [truncated]"
            } else {
                ""
            };
            format!("{}|{}{}", start_line + i, display, truncated)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `name` matches a glob pattern with `*` (any run of chars) and `?`
/// (any single char), case-insensitively on Windows — the `file_glob` /
/// target='files' filter semantics of Python `search_files`.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    glob_match_inner(pattern, name, cfg!(windows))
}

fn glob_match_inner(pattern: &str, name: &str, case_insensitive: bool) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = if case_insensitive {
        (
            pattern.to_lowercase().chars().collect(),
            name.to_lowercase().chars().collect(),
        )
    } else {
        (pattern.chars().collect(), name.chars().collect())
    };
    glob_match_chars(&p, &n)
}

fn glob_match_chars(p: &[char], n: &[char]) -> bool {
    // Iterative backtracking on '*' to avoid recursion blowup.
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star_p, mut star_n) = (None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = Some(pi);
            star_n = ni;
            pi += 1; // '*' matches empty first; retry with more of n later
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_n += 1;
            ni = star_n;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_is_bounded() {
        let long = "x".repeat(5000);
        let err = tool_error(long);
        let v: Value = serde_json::from_str(&err).unwrap();
        let body = v["error"].as_str().unwrap();
        assert!(body.len() <= MAX_TOOL_ERROR_CHARS + TOOL_ERROR_TRUNCATION_MARKER.len());
        assert!(body.ends_with(TOOL_ERROR_TRUNCATION_MARKER));
    }

    #[test]
    fn read_pagination_clamps() {
        assert_eq!(normalize_read_pagination(Some(0), Some(0)), (1, 1));
        assert_eq!(normalize_read_pagination(Some(-5), Some(99_999)), (1, MAX_LINES));
        assert_eq!(normalize_read_pagination(None, None), (1, DEFAULT_READ_LIMIT));
    }

    #[test]
    fn search_pagination_clamps() {
        assert_eq!(normalize_search_pagination(Some(-1), None), (0, DEFAULT_SEARCH_LIMIT));
        assert_eq!(normalize_search_pagination(Some(4), Some(10_000)), (4, MAX_LINES));
    }

    #[test]
    fn gutters_match_python_format() {
        // Trailing newline does not produce a phantom final line (cat -n).
        assert_eq!(add_line_numbers("a\nb\n", 3), "3|a\n4|b");
        assert_eq!(add_line_numbers("", 1), "");
        let long = "y".repeat(MAX_LINE_LENGTH + 10);
        let out = add_line_numbers(&long, 1);
        assert!(out.ends_with("… [truncated]"));
        assert!(out.starts_with("1|"));
    }

    #[test]
    fn globs() {
        assert!(glob_match("*.py", "main.py"));
        assert!(!glob_match("*.py", "main.rs"));
        assert!(glob_match("*config*", "app_config.json"));
        assert!(glob_match("a?c*", "abcxyz"));
        assert!(!glob_match("a?c", "ab"));
    }
}
