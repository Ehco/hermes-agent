//! Skill discovery and full-content loading.
//!
//! Port of the catalog walk in `tools/skills_tool.py::_skill_catalog` /
//! `find_all_skills` / `sort_skills` (with the `agent/skill_utils.py`
//! EXCLUDED_SKILL_DIRS / SKILL_SUPPORT_DIRS rules), `_skill_linked_files`
//! (the references/templates/assets/scripts linked-files listing) and the
//! by-name lookup behind `skill_view`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::frontmatter::{parse_frontmatter, truncate_description, truncate_name};

/// VCS/dependency/cache directories skill scanners always skip
/// (`agent/skill_utils.py::EXCLUDED_SKILL_DIRS`).
pub const EXCLUDED_SKILL_DIRS: &[&str] = &[
    ".git", ".github", ".hub", ".archive", ".curator_backups", ".locks", ".venv", "venv",
    "node_modules", "site-packages", "__pycache__", ".tox", ".nox", ".pytest_cache",
    ".mypy_cache", ".ruff_cache",
];

/// Progressive-disclosure support dirs inside a skill package: loaded
/// explicitly via linked files, never scanned as standalone skills.
pub const SKILL_SUPPORT_DIRS: &[&str] = &["references", "templates", "assets", "scripts"];

/// One row of the skill catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    /// Declared frontmatter name (fallback: directory name), ≤ 64 chars.
    pub name: String,
    /// Truncated description (1024-char budget, `...` suffix).
    pub description: String,
    /// First path component below the root for a `root/<category>/<skill>/SKILL.md` layout.
    pub category: Option<String>,
    /// Path to the SKILL.md file.
    pub path: PathBuf,
}

/// `(support dir, globs, recursive, files only)` — order is the linked_files
/// key order (`tools/skills_tool.py::_LINKED_FILE_SPECS`).
const LINKED_FILE_SPECS: &[(&str, &[&str], bool, bool)] = &[
    ("references", &["*.md"], false, false),
    (
        "templates",
        &["*.md", "*.py", "*.yaml", "*.yml", "*.json", "*.tex", "*.sh"],
        true,
        false,
    ),
    ("assets", &["*"], true, true),
    (
        "scripts",
        &["*.py", "*.sh", "*.bash", "*.js", "*.ts", "*.rb"],
        false,
        false,
    ),
];

fn is_excluded_dir_name(name: &str) -> bool {
    EXCLUDED_SKILL_DIRS.contains(&name)
}

/// Every SKILL.md under *root* (recursively), skipping excluded directories at
/// any path component and skipping a skill's own support dirs — the
/// `iter_skill_index_files` contract. Deterministic order: lexical by path.
fn iter_skill_index_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut names: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        names.sort();
        let has_skill_md = names.iter().any(|p| p.file_name().map_or(false, |n| n == "SKILL.md"));
        for path in names {
            if path.is_dir() {
                let name = path.file_name().map(|n| n.to_string_lossy().to_string());
                let name = match name {
                    Some(n) => n,
                    None => continue,
                };
                if is_excluded_dir_name(&name) {
                    continue;
                }
                // Inside a skill dir, its support dirs are loaded via
                // linked files and never scanned as standalone skills.
                if has_skill_md && SKILL_SUPPORT_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push(path);
            } else if path.file_name().map_or(false, |n| n == "SKILL.md") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Category of a skill path relative to *root*: the first component when the
/// layout is `root/<category>/<skill>/SKILL.md` (`_get_category_from_path`).
fn category_from_path(root: &Path, skill_md: &Path) -> Option<String> {
    let rel = skill_md.strip_prefix(root).ok()?;
    let parts: Vec<_> = rel.components().collect();
    if parts.len() >= 3 {
        parts[0].as_os_str().to_str().map(str::to_string)
    } else {
        None
    }
}

/// Scan a skills root for `*/SKILL.md` (with optional category subdirs) and
/// return sorted metadata: `(category or "", name)` order, mirroring
/// `sort_skills`. Parse failures are skipped (never fatal), as in Python.
pub fn discover_skills(root: &Path) -> Vec<SkillMeta> {
    let mut skills = Vec::new();
    for skill_md in iter_skill_index_files(root) {
        let raw = match fs::read_to_string(&skill_md) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        // The catalog parses only the first 4000 chars (`_skill_catalog`).
        let raw_trunc: String = raw.chars().take(4000).collect();
        let (frontmatter, body) = parse_frontmatter(&raw_trunc);
        let declared_name = frontmatter
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let name = if declared_name.is_empty() {
            skill_md
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            declared_name.to_string()
        };
        let mut description = frontmatter
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if description.is_empty() {
            // First non-heading body line (a null value stays null).
            description = body
                .lines()
                .map(str::trim)
                .find(|ln| !ln.is_empty() && !ln.starts_with('#'))
                .unwrap_or("")
                .to_string();
        }
        skills.push(SkillMeta {
            name: truncate_name(&name),
            description: truncate_description(&description),
            category: category_from_path(root, &skill_md),
            path: skill_md,
        });
    }
    sort_skills(skills)
}

/// Keep every skill listing ordered the same way: `(category or "", name)`.
pub fn sort_skills(skills: Vec<SkillMeta>) -> Vec<SkillMeta> {
    let mut skills = skills;
    skills.sort_by(|a, b| {
        a.category
            .clone()
            .unwrap_or_default()
            .cmp(&b.category.clone().unwrap_or_default())
            .then(a.name.cmp(&b.name))
    });
    skills
}

/// Find the SKILL.md whose frontmatter `name` or parent-directory name equals
/// *name* (the `skill_view` lookup over the same walk the catalog uses).
/// Ties keep the lexically first path, so the result is deterministic.
pub fn locate_skill(root: &Path, name: &str) -> Option<PathBuf> {
    iter_skill_index_files(root)
        .into_iter()
        .find(|skill_md| {
            let raw = fs::read_to_string(skill_md).unwrap_or_default();
            let (frontmatter, _body) =
                parse_frontmatter(raw.chars().take(4000).collect::<String>().as_str());
            let declared = frontmatter.get("name").and_then(|v| v.as_str()).unwrap_or("");
            declared == name
                || skill_md
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n == name)
                    .unwrap_or(false)
        })
}

/// Linked files of a directory skill, keyed references → templates → assets →
/// scripts, each a sorted list of posix paths relative to the skill dir.
/// Empty groups are dropped (`_skill_linked_files`).
pub fn linked_files(skill_dir: &Path) -> BTreeMap<String, Vec<String>> {
    let mut files: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (sub, globs, recursive, files_only) in LINKED_FILE_SPECS {
        let base = skill_dir.join(sub);
        if !base.is_dir() {
            continue;
        }
        let mut found = Vec::new();
        // paths are relative to the skill dir, so the support dir is part of the prefix
        collect_matches(&base, sub, globs, *recursive, *files_only, &mut found);
        found.sort();
        found.dedup();
        if !found.is_empty() {
            files.insert(sub.to_string(), found);
        }
    }
    files
}

/// Limited glob support without extra dependencies: *globs* are simple
/// `*.ext` suffix patterns or `*` (everything), matched against the file name;
/// *recursive* walks subdirectories.
fn collect_matches(
    dir: &Path,
    prefix: &str,
    globs: &[&str],
    recursive: bool,
    files_only: bool,
    out: &mut Vec<String>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string());
        let Some(name) = name else { continue };
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            if recursive {
                collect_matches(&path, &rel, globs, recursive, files_only, out);
            }
            continue;
        }
        let matched = globs.iter().any(|g| match *g {
            "*" => true,
            ext if ext.starts_with("*.") => name.ends_with(&ext[1..]),
            _ => false,
        });
        if matched && (!files_only || path.is_file()) {
            out.push(rel);
        }
    }
}

/// Load a skill's full content plus its linked-files listing by name — the
/// `skill_view` happy path. Returns `(SKILL.md text, linked_files)`.
pub fn load_skill(root: &Path, name: &str) -> anyhow::Result<(String, BTreeMap<String, Vec<String>>)> {
    let skill_md = locate_skill(root, name)
        .ok_or_else(|| anyhow::anyhow!("Skill '{name}' not found."))?;
    let content = fs::read_to_string(&skill_md)
        .map_err(|e| anyhow::anyhow!("Failed to read skill '{name}': {e}"))?;
    let dir = skill_md
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Skill '{name}' has no parent directory."))?;
    Ok((content, linked_files(dir)))
}

/// Metadata the tests can assert on: the catalog name cap equals the Python
/// `MAX_NAME_LENGTH` exactly.
#[cfg(test)]
mod catalog_contract {
    #[test]
    fn name_cap_matches_python() {
        use crate::frontmatter::MAX_NAME_LENGTH;
        assert_eq!(MAX_NAME_LENGTH, 64);
    }
}
