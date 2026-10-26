//! skills_list / skill_view — progressive disclosure over the skill catalog.
//! Port of `tools/skills_tool.py`'s two registry entries as thin wrappers
//! around `hermes_skills::skills` (the port of the catalog walk, name lookup
//! and linked-files listing — none of that is reimplemented here).
//!
//! - `skills_list`: `{"skills": [{name, description, category?, path}], "count"}`,
//!   with an optional exact `category` filter.
//! - `skill_view`: full SKILL.md content by name (frontmatter name or parent
//!   dir), plus `linked_files`; `file_path` serves one linked file relative to
//!   the skill dir (path-traversal-guarded).
//!
//! The skills root is `$HERMES_SKILLS_DIR`, else `$HERMES_HOME/skills`, else
//! `~/.hermes/skills`.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use hermes_skills::frontmatter::parse_frontmatter;
use hermes_skills::skills::{discover_skills, linked_files, locate_skill};

/// Skills root: `$HERMES_SKILLS_DIR`, else `$HERMES_HOME/skills`, else
/// `~/.hermes/skills`.
pub fn skills_root() -> PathBuf {
    if let Ok(dir) = std::env::var("HERMES_SKILLS_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HERMES_HOME").ok().filter(|h| !h.trim().is_empty());
    let base = home.map(PathBuf::from).unwrap_or_else(|| {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".hermes")
    });
    base.join("skills")
}

/// Execute `skills_list` — the registry entry point (JSON string in, out).
pub fn execute_list(args: &Value) -> String {
    execute_list_in(&skills_root(), args)
}

/// `execute_list` against an explicit root (tests, embedding hosts).
pub fn execute_list_in(root: &Path, args: &Value) -> String {
    let category = args.get("category").and_then(Value::as_str).unwrap_or("");
    let rows: Vec<Value> = discover_skills(root)
        .into_iter()
        .filter(|meta| category.is_empty() || meta.category.as_deref() == Some(category))
        .map(|meta| {
            let mut row = json!({
                "name": meta.name,
                "description": meta.description,
                "path": meta.path.display().to_string(),
            });
            if let Some(category) = &meta.category {
                row.as_object_mut()
                    .unwrap()
                    .insert("category".into(), Value::String(category.clone()));
            }
            row
        })
        .collect();
    let count = rows.len();
    json!({ "success": true, "skills": rows, "count": count }).to_string()
}

/// Execute `skill_view` — the registry entry point (JSON string in, out).
pub fn execute_view(args: &Value) -> String {
    execute_view_in(&skills_root(), args)
}

/// `execute_view` against an explicit root (tests, embedding hosts).
pub fn execute_view_in(root: &Path, args: &Value) -> String {
    let Some(name) = args.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()) else {
        return crate::util::tool_error("skill_view: 'name' is required");
    };
    let file_path = args.get("file_path").and_then(Value::as_str);
    let Some(skill_md) = locate_skill(root, name) else {
        return crate::util::tool_error(&format!(
            "Skill '{name}' not found. Use skills_list to see the available skills."
        ));
    };
    let Some(skill_dir) = skill_md.parent().map(Path::to_path_buf) else {
        return crate::util::tool_error(&format!("Skill '{name}' has no parent directory."));
    };
    if let Some(file_path) = file_path.filter(|p| !p.trim().is_empty()) {
        return serve_linked_file(name, &skill_dir, file_path);
    }
    let content = match std::fs::read_to_string(&skill_md) {
        Ok(content) => content,
        Err(e) => return crate::util::tool_error(&format!("Failed to read skill '{name}': {e}")),
    };
    let (frontmatter, _body) = parse_frontmatter(&content);
    let description = frontmatter.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let rel_path = skill_md.strip_prefix(root).unwrap_or(&skill_md).display().to_string();
    let files = linked_files(&skill_dir);
    let linked: serde_json::Map<String, Value> = files
        .into_iter()
        .map(|(group, list)| (group, json!(list)))
        .collect();
    let has_linked = !linked.is_empty();
    let mut payload = json!({
        "success": true,
        "name": name,
        "description": description,
        "content": content,
        "path": rel_path,
        "skill_dir": skill_dir.display().to_string(),
        "linked_files": Value::Object(linked),
        "usage_hint": if has_linked {
            "To view linked files, call skill_view(name, file_path) where file_path is e.g. 'references/api.md' or 'assets/config.yaml'"
        } else {
            ""
        },
    });
    if !has_linked {
        payload
            .as_object_mut()
            .unwrap()
            .insert("linked_files".into(), Value::Null);
    }
    payload.to_string()
}

/// Serve one file inside the skill dir; relative paths only, and the resolved
/// path must stay within the skill dir (no `..` traversal).
fn serve_linked_file(name: &str, skill_dir: &Path, file_path: &str) -> String {
    let absolute = Path::new(file_path).is_absolute()
        || file_path.starts_with('/')
        || file_path.starts_with('\\')
        || (file_path.len() >= 2 && file_path.as_bytes()[1] == b':'); // C:\ style
    let escapes = Path::new(file_path)
        .components()
        .any(|c| c == std::path::Component::ParentDir);
    if absolute || escapes {
        return crate::util::tool_error(&format!(
            "Use a relative path within the skill directory for file_path ('{file_path}' was rejected)."
        ));
    }
    let resolved = skill_dir.join(file_path);
    if !resolved.starts_with(skill_dir) {
        return crate::util::tool_error(&format!(
            "Use a relative path within the skill directory for file_path ('{file_path}' was rejected)."
        ));
    }
    match std::fs::read_to_string(&resolved) {
        Ok(content) => json!({
            "success": true,
            "name": name,
            "file_path": file_path,
            "content": content,
            "skill_dir": skill_dir.display().to_string(),
        })
        .to_string(),
        Err(e) => {
            let hint = if e.kind() == std::io::ErrorKind::NotFound { " (check the linked_files listing)" } else { "" };
            crate::util::tool_error(&format!("Could not read '{file_path}' in skill '{name}': {e}{hint}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn tmp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-tools-skills-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_skill(root: &Path, category: Option<&str>, dir_name: &str, name: &str, description: &str) -> PathBuf {
        let base = if let Some(category) = category { root.join(category) } else { root.to_path_buf() };
        let dir = base.join(dir_name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nBody here.\n"),
        )
        .unwrap();
        dir
    }

    #[test]
    fn list_reports_name_description_category_and_filters() {
        let root = tmp_root();
        make_skill(&root, Some("devops"), "deploy", "deploy", "Ship things safely.");
        make_skill(&root, None, "writing", "humanizer", "Strip AI-isms from text.");
        let out = v(&execute_list_in(&root, &json!({})));
        assert_eq!(out["count"], 2);
        let names: Vec<&str> = out["skills"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"deploy") && names.contains(&"humanizer"));

        let out = v(&execute_list_in(&root, &json!({"category": "devops"})));
        assert_eq!(out["count"], 1);
        assert_eq!(out["skills"][0]["name"], "deploy");
        assert_eq!(out["skills"][0]["category"], "devops");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn view_returns_content_and_linked_files() {
        let root = tmp_root();
        let dir = make_skill(&root, None, "deploy", "deploy", "Ship things safely.");
        fs::create_dir_all(dir.join("references")).unwrap();
        fs::write(dir.join("references").join("api.md"), "## API\n\nEndpoints.").unwrap();
        let out = v(&execute_view_in(&root, &json!({"name": "deploy"})));
        assert_eq!(out["success"], true);
        assert_eq!(out["description"], "Ship things safely.");
        assert!(out["content"].as_str().unwrap().contains("Body here."));
        assert_eq!(
            out["linked_files"]["references"],
            json!(["references/api.md"]),
            "{}",
            out
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn view_without_linked_files_is_null() {
        let root = tmp_root();
        make_skill(&root, None, "bare", "bare", "No support dirs.");
        let out = v(&execute_view_in(&root, &json!({"name": "bare"})));
        assert_eq!(out["linked_files"], Value::Null);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn view_serves_linked_files_and_rejects_traversal() {
        let root = tmp_root();
        let dir = make_skill(&root, None, "deploy", "deploy", "Ship things safely.");
        fs::create_dir_all(dir.join("references")).unwrap();
        fs::write(dir.join("references").join("api.md"), "## API").unwrap();
        let out = v(&execute_view_in(&root, &json!({"name": "deploy", "file_path": "references/api.md"})));
        assert_eq!(out["content"], "## API");

        let secret = root.join("outside.md");
        fs::write(&secret, "secrets").unwrap();
        let out = v(&execute_view_in(&root, &json!({"name": "deploy", "file_path": "../../outside.md"})));
        assert!(out.get("error").is_some(), "{out}");
        let out = v(&execute_view_in(&root, &json!({"name": "deploy", "file_path": "missing.md"})));
        assert!(out["error"].as_str().unwrap().contains("Could not read"), "{out}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn view_requires_name_and_reports_not_found() {
        let root = tmp_root();
        let out = v(&execute_view_in(&root, &json!({})));
        assert!(out["error"].as_str().unwrap().contains("'name' is required"));
        let out = v(&execute_view_in(&root, &json!({"name": "nope"})));
        assert!(out["error"].as_str().unwrap().contains("Skill 'nope' not found"));
        let _ = fs::remove_dir_all(&root);
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }
}

// --- skill_manage -----------------------------------------------------------------
//
// Port of `tools/skill_manager_tool.py`: create / patch / full-rewrite /
// write_file / remove_file / delete, as an atomic operations array (a single
// edit is a list of one; any failure rolls every touched skill back). The
// legacy flat single-op shape is also accepted, unadvertised.
//
// Validation is the Python's, verbatim in effect: name regex
// `^[a-z0-9][a-z0-9._-]*$` (≤64), single-directory category, frontmatter must
// open and close with `---` and carry name+description, body non-empty,
// SKILL.md ≤ 100_000 chars, supporting files ≤ 1 MiB under
// references/|templates/|scripts/|assets/.

pub const MAX_NAME_LENGTH: usize = 64;
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;
pub const MAX_SKILL_CONTENT_CHARS: usize = 100_000;
pub const MAX_SKILL_FILE_BYTES: usize = 1_048_576;
pub const ALLOWED_SUBDIRS: &[&str] = &["references", "templates", "scripts", "assets"];

fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Skill name is required.".into());
    }
    if name.len() > MAX_NAME_LENGTH {
        return Err(format!("Skill name exceeds {MAX_NAME_LENGTH} characters."));
    }
    let first = name.chars().next().unwrap_or(' ');
    let ok = (first.is_ascii_lowercase() || first.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "Invalid skill name '{name}'. Use lowercase letters, numbers, hyphens, dots, and underscores. Must start with a letter or digit."
        ))
    }
}

fn validate_category(category: Option<&str>) -> Result<(), String> {
    let Some(category) = category.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(());
    };
    if category.contains('/') || category.contains('\\') {
        return Err(format!(
            "Invalid category '{category}'. Use lowercase letters, numbers, hyphens, dots, and underscores. Categories must be a single directory name."
        ));
    }
    valid_name(category).map_err(|_| {
        format!(
            "Invalid category '{category}'. Use lowercase letters, numbers, hyphens, dots, and underscores. Categories must be a single directory name."
        )
    })
}

/// Frontmatter checks: opens `---`, closes, has name+description, bounded
/// description, non-empty body. (The YAML subset parsed here is the
/// flat `key: value` frontmatter every SKILL.md uses.)
fn validate_frontmatter(content: &str, new_skill: bool) -> Result<(), String> {
    if content.trim().is_empty() {
        return Err("Content cannot be empty.".into());
    }
    let content = content.trim_start_matches('\u{feff}');
    if !content.starts_with("---") {
        return Err("SKILL.md must start with YAML frontmatter (---). See existing skills for format.".into());
    }
    let rest = &content[3..];
    let Some(end) = rest.find("\n---") else {
        return Err("SKILL.md frontmatter is not closed. Ensure you have a closing '---' line.".into());
    };
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    for line in rest[..end].lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        match key.trim() {
            "name" => name = Some(unquote(value.trim())),
            "description" => description = Some(unquote(value.trim())),
            _ => {}
        }
    }
    if name.is_none() || description.is_none() {
        return Err("Frontmatter must include 'name' and 'description' fields.".into());
    }
    let desc = description.unwrap_or_default();
    if desc.len() > MAX_DESCRIPTION_LENGTH {
        return Err(format!("Description exceeds {MAX_DESCRIPTION_LENGTH} characters."));
    }
    if new_skill && desc.trim().trim_matches(['\'', '"']).len() > 255 {
        return Err(format!(
            "Description is {} chars — new skills must fit the 255-char system-prompt budget (one sentence, trigger first, ends with a period). Move detail into the skill body.",
            desc.trim().len()
        ));
    }
    let body = &rest[end + 4..];
    if body.trim().is_empty() {
        return Err("SKILL.md must have content after the frontmatter (instructions, procedures, etc.).".into());
    }
    Ok(())
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && (value.starts_with('"') && value.ends_with('"') || value.starts_with('\'') && value.ends_with('\'')) {
        return value[1..value.len() - 1].to_string();
    }
    value.to_string()
}

fn validate_content_size(content: &str, label: &str) -> Result<(), String> {
    if content.len() > MAX_SKILL_CONTENT_CHARS {
        return Err(format!(
            "{label} content is {} characters (limit: {MAX_SKILL_CONTENT_CHARS}). Consider splitting into a smaller SKILL.md with supporting files in references/ or templates/.",
            content.len()
        ));
    }
    Ok(())
}

/// write_file/remove_file path rule: under an allowed subdir (SKILL.md at the
/// skill root accepted), no traversal.
pub fn validate_file_path(file_path: &str) -> Result<(), String> {
    if file_path.is_empty() {
        return Err("file_path is required.".into());
    }
    let path = Path::new(file_path);
    if path.components().any(|c| c == std::path::Component::ParentDir) {
        return Err("Path traversal ('..') is not allowed.".into());
    }
    if path.is_absolute() || file_path.starts_with('/') || file_path.starts_with('\\') || file_path.len() >= 2 && file_path.as_bytes()[1] == b':' {
        return Err(format!(
            "File must be under one of: {}. Got: '{file_path}'",
            ALLOWED_SUBDIRS.join(", ")
        ));
    }
    let parts: Vec<&str> = file_path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    if parts.last() == Some(&"SKILL.md") && (1..=2).contains(&parts.len()) {
        return Ok(());
    }
    if parts.first().map(|p| ALLOWED_SUBDIRS.contains(p)) != Some(true) {
        return Err(format!(
            "File must be under one of: {}. Got: '{file_path}'",
            ALLOWED_SUBDIRS.join(", ")
        ));
    }
    if parts.len() < 2 {
        return Err(format!(
            "Provide a file path, not just a directory. Example: '{}/myfile.md'",
            parts.first().unwrap_or(&"references")
        ));
    }
    Ok(())
}

/// Execute `skill_manage` — the registry entry point.
pub fn execute_manage(args: &Value) -> String {
    let skills_root = skills_root();
    execute_manage_in(&skills_root, args)
}

/// `execute_manage` against an explicit skills root (tests, embedding hosts).
pub fn execute_manage_in(root: &Path, args: &Value) -> String {
    // One advertised call shape: the call IS an operations array (a single
    // edit is a list of one); it applies atomically — any failure rolls every
    // touched skill back. The legacy flat shape is also accepted.
    let ops: Vec<Value> = match args.get("operations") {
        Some(Value::Array(ops)) => ops.clone(),
        _ if args.get("action").is_some() => vec![args.clone()],
        _ => return crate::util::tool_error("skill_manage requires an operations array (a single edit is a list of one)."),
    };
    if ops.is_empty() {
        return crate::util::tool_error("operations must contain at least one op.");
    }
    // delete is the sole op only.
    if ops.len() > 1 && ops.iter().any(|op| op.get("action").and_then(Value::as_str) == Some("delete")) {
        return crate::util::tool_error("delete must be the only operation in the batch.");
    }

    // Snapshot every touched skill dir up front so a mid-batch failure can
    // roll the whole batch back (the atomicity contract).
    let mut touched: Vec<(String, Option<PathBuf>)> = Vec::new();
    for op in &ops {
        let Some(name) = op.get("name").and_then(Value::as_str).map(str::trim) else {
            return crate::util::tool_error("each op names its target skill (name is required).");
        };
        let (name, category) = split_category(name);
        if let Err(e) = valid_name(name) {
            return crate::util::tool_error(e);
        }
        if let Err(e) = validate_category(category) {
            return crate::util::tool_error(e);
        }
        let dir = if let Some(skill_md) = locate_skill(root, name) {
            match skill_md.parent().map(Path::to_path_buf) {
                Some(dir) => Some(dir),
                None => return crate::util::tool_error(&format!("Skill '{name}' has no parent directory.")),
            }
        } else {
            None
        };
        if let Some(dir) = &dir {
            if touched.iter().any(|(n, _)| n == name) {
                continue;
            }
            touched.push((name.to_string(), Some(dir.clone())));
        } else {
            touched.push((name.to_string(), None));
        }
    }

    let mut snapshots: Vec<(PathBuf, Vec<(PathBuf, Vec<u8>)>)> = Vec::new();
    for (_name, dir) in &touched {
        if let Some(dir) = dir {
            let files = snapshot_dir(dir);
            snapshots.push((dir.clone(), files));
        }
    }

    let mut results: Vec<Value> = Vec::with_capacity(ops.len());
    for op in &ops {
        match apply_op(root, op) {
            Ok(result) => results.push(result),
            Err(e) => {
                rollback(&snapshots);
                return crate::util::tool_error(e);
            }
        }
    }
    let applied = results.len();
    json!({ "success": true, "applied": applied, "operations": results }).to_string()
}

/// `category/name` splits for ops that accept both (create takes a bare name).
fn split_category(name: &str) -> (&str, Option<&str>) {
    match name.split_once('/') {
        Some((category, rest)) if !rest.is_empty() => (rest, Some(category)),
        _ => (name, None),
    }
}

fn snapshot_dir(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.push((path, bytes));
            }
        }
    }
    out
}

fn rollback(snapshots: &[(PathBuf, Vec<(PathBuf, Vec<u8>)>)]) {
    for (dir, files) in snapshots {
        // Restore every snapshotted file; remove anything created since.
        for (path, bytes) in files {
            let _ = std::fs::write(path, bytes);
        }
        // Best-effort removal of files the batch created inside `dir`.
        let known: std::collections::HashSet<&Path> = files.iter().map(|(p, _)| p.as_path()).collect();
        strip_new_files(dir, &known);
    }
}

fn strip_new_files(dir: &Path, known: &std::collections::HashSet<&Path>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            strip_new_files(&path, known);
            let _ = std::fs::remove_dir(&path); // only removes if empty
        } else if !known.contains(path.as_path()) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn apply_op(root: &Path, op: &Value) -> Result<Value, String> {
    let action = op.get("action").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
    let raw_name = op.get("name").and_then(Value::as_str).unwrap_or("").trim();
    let (name, category) = split_category(raw_name);
    let content = op.get("content").and_then(Value::as_str);
    let file_path = op.get("file_path").and_then(Value::as_str).unwrap_or("");
    let file_content = op.get("file_content").and_then(Value::as_str).unwrap_or("");
    let old_string = op.get("old_string").and_then(Value::as_str);
    let new_string = op.get("new_string").and_then(Value::as_str);
    let replace_all = op.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
    let absorbed_into = op.get("absorbed_into").and_then(Value::as_str);

    match action.as_str() {
        "create" => {
            let category = category.map(str::to_string).or_else(|| {
                op.get("category").and_then(Value::as_str).map(|c| c.to_string()).filter(|c| !c.trim().is_empty())
            });
            if let Err(e) = validate_category(category.as_deref()) {
                return Err(e);
            }
            let Some(content) = content else { return Err("create requires the full SKILL.md 'content' (YAML frontmatter + markdown body).".into()) };
            if let Err(e) = validate_frontmatter(content, true) {
                return Err(e);
            }
            if let Err(e) = validate_content_size(content, "SKILL.md") {
                return Err(e);
            }
            if locate_skill(root, name).is_some() {
                return Err(format!("A skill named '{name}' already exists. Read it via skill_view first."));
            }
            let dir = if let Some(category) = &category {
                root.join(category).join(name)
            } else {
                root.join(name)
            };
            if dir.exists() {
                return Err(format!(
                    "Cannot create skill '{name}': {} already exists (not an empty directory, or unreadable). Choose another name, or move/remove that path and retry.",
                    dir.display()
                ));
            }
            std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create {display}: {e}", display = dir.display()))?;
            match std::fs::write(dir.join("SKILL.md"), content) {
                Ok(()) => Ok(json!({
                    "success": true,
                    "action": "create",
                    "name": name,
                    "message": format!("Skill '{name}' created."),
                    "path": dir.display().to_string(),
                    "hint": format!("To add reference files, templates, or scripts, use skill_manage(action='write_file', name='{name}', file_path='references/example.md', file_content='...')"),
                })),
                Err(e) => {
                    let _ = std::fs::remove_dir(&dir); // only an empty dir goes
                    Err(format!("failed to write SKILL.md: {e}"))
                }
            }
        }
        "patch" => {
            // Two shapes: content alone = full rewrite (the old 'edit');
            // old_string/new_string = targeted replacement.
            if let Some(content) = content {
                if old_string.is_some() || new_string.is_some() {
                    return Err("Pass either content (full rewrite) or old_string/new_string (targeted fix), not both.".into());
                }
                return rewrite(root, name, content);
            }
            let (Some(old), Some(new)) = (old_string, new_string) else {
                return Err("patch requires old_string and new_string (content alone = full rewrite).".into());
            };
            let dir = locate_dir(root, name)?;
            let target = if file_path.is_empty() {
                dir.join("SKILL.md")
            } else {
                if let Err(e) = validate_file_path(file_path) {
                    return Err(e);
                }
                dir.join(file_path)
            };
            let existing = std::fs::read(&target).map_err(|e| format!("File not found: {} ({e})", target.display()))?;
            let current = String::from_utf8_lossy(&existing).to_string();
            let count = current.matches(old).count();
            if count == 0 {
                return Err(format!("old_string not found in {}.", file_path.trim().is_empty().then(|| "SKILL.md").unwrap_or(file_path)));
            }
            if count > 1 && !replace_all {
                return Err(format!(
                    "old_string matches {count} times; pass replace_all=true to replace all occurrences, or make old_string unique."
                ));
            }
            let updated = if replace_all { current.replace(old, new) } else { current.replacen(old, new, 1) };
            if let Err(e) = validate_content_size(&updated, if file_path.is_empty() { "SKILL.md" } else { file_path }) {
                return Err(e);
            }
            if file_path.is_empty() {
                // Targeted patches are plain string replaces (Python's patch
                // op); only a full rewrite re-validates the frontmatter.
                if updated.starts_with("---") {
                    if let Err(e) = validate_frontmatter(&updated, false) {
                        return Err(format!("Patch would break SKILL.md structure: {e}"));
                    }
                }
            }
            std::fs::write(&target, &updated).map_err(|e| format!("failed to write {}: {e}", target.display()))?;
            let label = if file_path.is_empty() { "SKILL.md".to_string() } else { file_path.to_string() };
            Ok(json!({
                "success": true,
                "action": "patch",
                "name": name,
                "message": format!("Patched {label} in skill '{name}' ({count} replacement{}).", if count > 1 { "s" } else { "" }),
            }))
        }
        "write_file" => {
            if let Err(e) = validate_file_path(file_path) {
                return Err(e);
            }
            if file_content.is_empty() {
                return Err("write_file requires file_content (the full text of the supporting file).".into());
            }
            if file_content.len() > MAX_SKILL_FILE_BYTES {
                return Err(format!("file_content exceeds the 1 MiB per-file limit ({MAX_SKILL_FILE_BYTES} bytes)."));
            }
            let dir = locate_dir(root, name)?;
            let target = dir.join(file_path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("failed to create {display}: {e}", display = parent.display()))?;
            }
            std::fs::write(&target, file_content).map_err(|e| format!("failed to write {}: {e}", target.display()))?;
            Ok(json!({
                "success": true,
                "action": "write_file",
                "name": name,
                "file_path": file_path,
                "message": format!("Wrote {file_path} in skill '{name}'."),
            }))
        }
        "remove_file" => {
            if let Err(e) = validate_file_path(file_path) {
                return Err(e);
            }
            let dir = locate_dir(root, name)?;
            let target = dir.join(file_path);
            if !target.exists() {
                return Err(format!("File not found: {file_path}"));
            }
            std::fs::remove_file(&target).map_err(|e| format!("failed to remove {file_path}: {e}"))?;
            Ok(json!({
                "success": true,
                "action": "remove_file",
                "name": name,
                "file_path": file_path,
                "message": format!("Removed {file_path} from skill '{name}'."),
            }))
        }
        "delete" => {
            if let Some(absorbed) = absorbed_into.map(str::trim).filter(|a| !a.is_empty()) {
                if absorbed == name {
                    return Err(format!("absorbed_into='{absorbed}' cannot equal the skill being deleted."));
                }
                if locate_skill(root, absorbed).is_none() {
                    return Err(format!("absorbed_into='{absorbed}' does not exist. Create or patch the umbrella skill first, then retry the delete."));
                }
            }
            let dir = locate_dir(root, name)?;
            std::fs::remove_dir_all(&dir).map_err(|e| format!("failed to delete skill '{name}': {e}"))?;
            // Empty category dir goes, never the root.
            if let Some(parent) = dir.parent() {
                if parent != root {
                    if let Ok(mut entries) = std::fs::read_dir(parent) {
                        if entries.next().is_none() {
                            let _ = std::fs::remove_dir(parent);
                        }
                    }
                }
            }
            Ok(json!({
                "success": true,
                "action": "delete",
                "name": name,
                "message": format!("Skill '{name}' deleted."),
            }))
        }
        "edit" => {
            // Legacy alias for a full rewrite (not in the schema).
            let Some(content) = content else {
                return Err("edit (full rewrite) requires 'content' — use patch with old_string/new_string for a targeted fix.".into());
            };
            rewrite(root, name, content)
        }
        other => Err(format!(
            "Unknown skill_manage action '{other}'. Use: create, patch, delete, write_file, remove_file"
        )),
    }
}

fn rewrite(root: &Path, name: &str, content: &str) -> Result<Value, String> {
    if let Err(e) = validate_frontmatter(content, false) {
        return Err(e);
    }
    if let Err(e) = validate_content_size(content, "SKILL.md") {
        return Err(e);
    }
    let dir = locate_dir(root, name)?;
    std::fs::write(dir.join("SKILL.md"), content).map_err(|e| format!("failed to write SKILL.md: {e}"))?;
    Ok(json!({
        "success": true,
        "action": "patch",
        "name": name,
        "message": format!("Skill '{name}' updated (full rewrite)."),
    }))
}

fn locate_dir(root: &Path, name: &str) -> Result<PathBuf, String> {
    match locate_skill(root, name) {
        Some(skill_md) => skill_md
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("Skill '{name}' has no parent directory.")),
        None => Err(format!(
            "Skill '{name}' not found. Use skills_list to see the available skills."
        )),
    }
}

#[cfg(test)]
mod manage_tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn tmp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-skillmgr-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const SKILL_MD: &str = "---\nname: deploy\ndescription: Ship things safely.\n---\n\n# deploy\n\nBody here.\n";

    fn seed(root: &Path) -> PathBuf {
        let dir = root.join("deploy");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), SKILL_MD).unwrap();
        dir
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn create_writes_skill_md_and_requires_frontmatter() {
        let root = tmp_root();
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "new-skill", "action": "create", "category": "devops",
                             "content": "---\nname: new-skill\ndescription: Does the thing.\n---\n\n# new-skill\n\nBody." }],
        })));
        assert_eq!(out["applied"], 1, "{out}");
        let created = root.join("devops").join("new-skill").join("SKILL.md");
        assert!(created.exists(), "{}", created.display());
        // Duplicate create fails.
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "new-skill", "action": "create", "content": "---\nname: new-skill\ndescription: d\n---\n\nbody" }],
        })));
        assert!(out["error"].as_str().unwrap().contains("already exists"), "{out}");
        // Missing frontmatter fields fail.
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "bad", "action": "create", "content": "---\nnope: 1\n---\n\nbody" }],
        })));
        assert!(out["error"].as_str().unwrap().contains("'name' and 'description'"), "{out}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_names_and_categories_are_rejected_before_touching_disk() {
        let root = tmp_root();
        for bad in ["UPPER", "-lead", "has space", "a/b"] {
            let out = v(&execute_manage_in(&root, &json!({
                "operations": [{ "name": bad, "action": "create", "content": "x" }],
            })));
            assert!(out.get("error").is_some(), "{bad}: {out}");
        }
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "ok", "action": "create", "category": "deep/nested", "content": "x" }],
        })));
        assert!(out["error"].as_str().unwrap().contains("single directory"), "{out}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn patch_replaces_unique_match_and_requires_replace_all_for_many() {
        let root = tmp_root();
        seed(&root);
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "deploy", "action": "patch",
                             "old_string": "Body here.", "new_string": "Body rewritten." }],
        })));
        assert_eq!(out["success"], true, "{out}");
        let updated = fs::read_to_string(root.join("deploy").join("SKILL.md")).unwrap();
        assert!(updated.contains("Body rewritten."));

        // Many matches without replace_all fail with the count.
        fs::write(root.join("deploy").join("SKILL.md"), "dup dup dup").unwrap();
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "deploy", "action": "patch", "old_string": "dup", "new_string": "x" }],
        })));
        assert!(out["error"].as_str().unwrap().contains("3 times"), "{out}");
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "deploy", "action": "patch", "old_string": "dup", "new_string": "x", "replace_all": true }],
        })));
        assert_eq!(out["success"], true);
        assert_eq!(fs::read_to_string(root.join("deploy").join("SKILL.md")).unwrap(), "x x x");
        // A no-match is an error, not a silent no-op.
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "deploy", "action": "patch", "old_string": "absent", "new_string": "x" }],
        })));
        assert!(out["error"].as_str().unwrap().contains("not found"), "{out}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn content_alone_is_a_full_rewrite() {
        let root = tmp_root();
        seed(&root);
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "deploy", "action": "patch",
                             "content": "---\nname: deploy\ndescription: Rewritten.\n---\n\nNew body." }],
        })));
        assert_eq!(out["operations"][0]["message"], "Skill 'deploy' updated (full rewrite).");
        let updated = fs::read_to_string(root.join("deploy").join("SKILL.md")).unwrap();
        assert!(updated.contains("New body."));
        // Content + old_string together is rejected.
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "deploy", "action": "patch", "content": "y", "old_string": "a", "new_string": "b" }],
        })));
        assert!(out["error"].as_str().unwrap().contains("not both"), "{out}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_and_remove_supporting_files_within_allowed_subdirs() {
        let root = tmp_root();
        seed(&root);
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "deploy", "action": "write_file",
                             "file_path": "references/api.md", "file_content": "## API" }],
        })));
        assert_eq!(out["success"], true, "{out}");
        assert_eq!(fs::read_to_string(root.join("deploy").join("references").join("api.md")).unwrap(), "## API");
        // Path rules: traversal, absolute, non-allowed top dir.
        for bad in ["../../etc/passwd", "C:/x.md", "src/main.rs", "references"] {
            let out = v(&execute_manage_in(&root, &json!({
                "operations": [{ "name": "deploy", "action": "write_file", "file_path": bad, "file_content": "x" }],
            })));
            assert!(out.get("error").is_some(), "{bad}: {out}");
        }
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "deploy", "action": "remove_file", "file_path": "references/api.md" }],
        })));
        assert_eq!(out["success"], true);
        assert!(!root.join("deploy").join("references").join("api.md").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_requires_sole_op_and_validates_absorbed_into() {
        let root = tmp_root();
        seed(&root);
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [
                { "name": "deploy", "action": "delete" },
                { "name": "other", "action": "create", "content": "x" },
            ],
        })));
        assert!(out["error"].as_str().unwrap().contains("only operation"), "{out}");
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "deploy", "action": "delete", "absorbed_into": "deploy" }],
        })));
        assert!(out["error"].as_str().unwrap().contains("cannot equal"), "{out}");
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "deploy", "action": "delete", "absorbed_into": "ghost" }],
        })));
        assert!(out["error"].as_str().unwrap().contains("does not exist"), "{out}");
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [{ "name": "deploy", "action": "delete" }],
        })));
        assert_eq!(out["success"], true, "{out}");
        assert!(!root.join("deploy").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn batch_applies_atomically_and_rolls_back_on_failure() {
        let root = tmp_root();
        seed(&root);
        // op 2 patches deploy; op 3 creates a new skill whose content is
        // broken — the whole batch rolls back to the seeded state.
        let out = v(&execute_manage_in(&root, &json!({
            "operations": [
                { "name": "deploy", "action": "patch", "old_string": "Body here.", "new_string": "CHANGED" },
                { "name": "fresh", "action": "create", "content": "not valid frontmatter" },
            ],
        })));
        assert!(out.get("error").is_some(), "{out}");
        assert_eq!(
            fs::read_to_string(root.join("deploy").join("SKILL.md")).unwrap(),
            SKILL_MD,
            "the patch rolled back"
        );
        assert!(!root.join("fresh").exists(), "the half-created skill rolled back");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn flat_legacy_shape_is_accepted() {
        let root = tmp_root();
        seed(&root);
        let out = v(&execute_manage_in(&root, &json!({
            "action": "patch", "name": "deploy",
            "old_string": "Body here.", "new_string": "Flat-edited.",
        })));
        assert_eq!(out["success"], true, "{out}");
        assert!(fs::read_to_string(root.join("deploy").join("SKILL.md")).unwrap().contains("Flat-edited."));
        let _ = fs::remove_dir_all(&root);
    }
}
