//! search — regex over file contents, recursive, with glob filter and
//! content / files_only / count output modes.
//!
//! Port of `search_tool` (Python `search_files`) from the hermes-agent. The
//! Python version shells out to ripgrep; this Rust port performs the walk
//! in-process (stdlib + `regex`), keeping the same result shapes:
//! - `content` mode: `{"matches": [{"path", "line", "line_number"}], ...}`
//! - `files_only` mode: `{"files": [path, ...]}`
//! - `count` mode: `{"counts": {path: n}, ...}`
//! plus `{"truncated": bool}` and an offset/limit continuation hint.

use regex::Regex;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

use crate::read_file::{expand_tilde, path_display};
use crate::util::{normalize_search_pagination, tool_error};

pub fn execute(args: &Value) -> String {
    let pattern = match args.get("pattern").and_then(Value::as_str) {
        Some(p) => p,
        None => return tool_error("search: 'pattern' is required"),
    };
    let target = args
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("content");
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.trim().is_empty())
        .unwrap_or(".");
    let file_glob = args.get("file_glob").and_then(Value::as_str);
    let output_mode = args
        .get("output_mode")
        .and_then(Value::as_str)
        .unwrap_or("content");
    let context = args
        .get("context")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .clamp(0, 10) as usize;
    let (offset, limit) = normalize_search_pagination(
        args.get("offset").and_then(Value::as_i64),
        args.get("limit").and_then(Value::as_i64),
    );

    let re = match Regex::new(pattern) {
        Ok(re) => re,
        Err(e) => return tool_error(format!("Invalid regex pattern: {e}")),
    };
    if target != "content" && target != "files" {
        return tool_error(format!("Unknown target: {target}"));
    }
    match output_mode {
        "content" | "files_only" | "count" => {}
        other => return tool_error(format!("Unknown output_mode: {other}")),
    }

    let root = expand_tilde(path);
    match search(
        &root,
        &re,
        target,
        file_glob,
        output_mode,
        context,
        offset,
        limit,
    ) {
        Ok(v) => v.to_string(),
        Err(e) => tool_error(format!("Path not found: {path} ({e})")),
    }
}

fn search(
    root: &Path,
    re: &Regex,
    target: &str,
    file_glob: Option<&str>,
    output_mode: &str,
    context: usize,
    offset: usize,
    limit: usize,
) -> Result<Value, std::io::Error> {
    let mut files: Vec<PathBuf> = Vec::new();
    if root.is_dir() {
        walk(root, file_glob, &mut files)?;
    } else if root.is_file() {
        // A single file path is searched directly.
        if glob_filter(root, file_glob) {
            files.push(root.to_path_buf());
        }
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such file or directory",
        ));
    }

    if target == "files" {
        // Filename search: the pattern is matched against the file NAME.
        let total = files.iter().filter(|f| re.is_match(&file_name(f))).count();
        let hits: Vec<String> = files
            .iter()
            .filter(|f| re.is_match(&file_name(f)))
            .map(|f| path_display(f))
            .skip(offset)
            .take(limit)
            .collect();
        let truncated = hits.len() == limit && total > offset + limit;
        return Ok(json!({
            "files": hits,
            "count": total,
            "truncated": truncated,
        }));
    }

    let mut matches: Vec<Value> = Vec::new();
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    let mut truncated = false;
    let mut offset_budget = offset;

    for file in &files {
        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            // Non-UTF-8 and unreadable files are skipped, like rg does.
            Err(_) => continue,
        };
        let mut file_count = 0usize;
        let lines: Vec<&str> = content.split('\n').collect();
        for (idx, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                file_count += 1;
                if offset_budget > 0 {
                    offset_budget -= 1;
                    continue;
                }
                if matches.len() >= limit {
                    truncated = true;
                    break;
                }
                let line_number = idx + 1;
                matches.push(json!({
                    "path": path_display(file),
                    "line_number": line_number,
                    "line": render_line(line, context, &lines, idx, re),
                }));
            }
        }
        if file_count > 0 {
            counts.insert(path_display(file), file_count);
        }
        if truncated {
            break;
        }
    }

    match output_mode {
        "files_only" => Ok(json!({
            "files": counts.keys().cloned().collect::<Vec<_>>(),
            "count": counts.len(),
            "truncated": truncated,
        })),
        "count" => Ok(json!({
            "counts": counts,
            "count": counts.values().sum::<usize>(),
            "truncated": truncated,
        })),
        _ => {
            let mut v = json!({
                "matches": matches,
                "count": matches.len(),
                "truncated": truncated,
            });
            if truncated {
                v.as_object_mut().unwrap().insert(
                    "hint".into(),
                    json!(format!(
                        "Results truncated. Use offset={} to see more, or narrow with a more specific pattern or file_glob.",
                        offset + limit
                    )),
                );
            }
            Ok(v)
        }
    }
}

/// Render a matched line, adding `context` lines around it when requested.
/// Context lines are prefixed with a space; the match line with `>`.
fn render_line(line: &str, context: usize, lines: &[&str], idx: usize, re: &Regex) -> String {
    if context == 0 {
        // A single very long line is clamped like a read_file gutter.
        return clamp_line(line);
    }
    let start = idx.saturating_sub(context);
    let end = (idx + context + 1).min(lines.len());
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let prefix = if re.is_match(l) && start + i == idx {
                ">"
            } else {
                " "
            };
            format!("{prefix}{}", clamp_line(l))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn clamp_line(line: &str) -> String {
    if line.len() <= 500 {
        line.to_string()
    } else {
        let mut cut = 500;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}… [truncated]", &line[..cut])
    }
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn glob_filter(path: &Path, file_glob: Option<&str>) -> bool {
    match file_glob {
        Some(glob) => crate::util::glob_match(glob, &file_name(path)),
        None => true,
    }
}

/// Recursive walk, bounded depth-free (follows the directory tree only;
/// symlink cycles are avoided by not following symlinked directories).
fn walk(dir: &Path, file_glob: Option<&str>, out: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
    let mut dirs: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(d) = dirs.pop() {
        for entry in fs::read_dir(&d)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let path = entry.path();
            if ft.is_dir() {
                // Do NOT follow symlinks: cycle-safe like Python's default walk.
                if !ft.is_symlink() {
                    dirs.push(path);
                }
            } else if glob_filter(&path, file_glob) {
                out.push(path);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_file::unique_suffix;
    use serde_json::json;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hermes-tools-search-{}", unique_suffix(name)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn parse(out: String) -> Value {
        serde_json::from_str(&out).unwrap()
    }

    fn make_tree() -> PathBuf {
        let dir = tmpdir("tree");
        fs::write(dir.join("alpha.rs"), "fn main() {}\nlet TODO = 1;\n").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/beta.rs"), "// TODO later\n").unwrap();
        fs::write(dir.join("sub/gamma.txt"), "todo in txt\n").unwrap();
        dir
    }

    #[test]
    fn content_mode_reports_line_numbers() {
        let dir = make_tree();
        let v = parse(execute(
            &json!({"pattern": "TODO", "path": path_display(&dir)}),
        ));
        assert_eq!(v["count"], 2);
        let ms = v["matches"].as_array().unwrap();
        assert_eq!(ms[0]["path"].as_str().unwrap().ends_with("alpha.rs"), true);
        assert_eq!(ms[0]["line_number"], 2);
        assert_eq!(ms[1]["line_number"], 1);
        assert_eq!(v["truncated"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn regex_syntax_works() {
        let dir = make_tree();
        let v = parse(execute(
            &json!({"pattern": "TO\\w+", "path": path_display(&dir)}),
        ));
        assert_eq!(v["count"], 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn glob_filter_restricts_files() {
        let dir = make_tree();
        let v = parse(execute(&json!({
            "pattern": "todo", "path": path_display(&dir), "file_glob": "*.txt"
        })));
        assert_eq!(v["count"], 1);
        assert!(v["matches"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("gamma.txt"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn files_only_and_count_modes() {
        let dir = make_tree();
        let v = parse(execute(&json!({
            "pattern": "TODO", "path": path_display(&dir), "output_mode": "files_only"
        })));
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);

        let v = parse(execute(&json!({
            "pattern": "TODO", "path": path_display(&dir), "output_mode": "count"
        })));
        let counts = v["counts"].as_object().unwrap();
        assert_eq!(counts.len(), 2);
        let total: usize = counts.values().map(|c| c.as_u64().unwrap() as usize).sum();
        assert_eq!(total, 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn offset_limit_pagination() {
        let dir = make_tree();
        let v = parse(execute(&json!({
            "pattern": "TODO", "path": path_display(&dir), "limit": 1
        })));
        assert_eq!(v["count"], 1);
        assert_eq!(v["truncated"], true);
        assert!(v["hint"].as_str().unwrap().contains("offset=1"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn files_target_matches_names() {
        let dir = make_tree();
        let v = parse(execute(&json!({
            "pattern": "beta", "path": path_display(&dir), "target": "files"
        })));
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].as_str().unwrap().ends_with("beta.rs"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_regex_is_tool_error() {
        let dir = make_tree();
        let v = parse(execute(
            &json!({"pattern": "(", "path": path_display(&dir)}),
        ));
        assert!(v["error"].as_str().unwrap().contains("Invalid regex"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_root_is_tool_error() {
        let dir = make_tree();
        let v = parse(execute(
            &json!({"pattern": "x", "path": path_display(&dir.join("nope"))}),
        ));
        assert!(v["error"].as_str().unwrap().contains("Path not found"));
        let _ = fs::remove_dir_all(dir);
    }
}
