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
