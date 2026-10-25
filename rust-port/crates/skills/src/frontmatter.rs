//! SKILL.md frontmatter parsing and validation.
//!
//! Port of `agent/skill_utils.py::parse_frontmatter`,
//! `tools/skills_tool_plugin.py` (MAX_NAME_LENGTH / MAX_DESCRIPTION_LENGTH /
//! `_truncate_description`) and the hard requirements of
//! `tools/skill_manager_tool.py::_validate_frontmatter` plus the
//! `tools/skill_linter.py` name-format rule.

use serde_yaml::Mapping;

/// Anthropic-recommended progressive-disclosure limits (`skills_tool_plugin.py`).
pub const MAX_NAME_LENGTH: usize = 64;
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;

/// Parsed SKILL.md: frontmatter mapping plus the markdown body.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillDoc {
    pub frontmatter: Mapping,
    pub body: String,
}

impl SkillDoc {
    /// Frontmatter `name`, or `""` when absent.
    pub fn name(&self) -> &str {
        self.frontmatter
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }

    /// Frontmatter `description`, or `""` when absent.
    pub fn description(&self) -> &str {
        self.frontmatter
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }
}

/// Parse YAML frontmatter from markdown; returns `(frontmatter, body)`.
/// Malformed YAML falls back to `key: value` line splitting (the Python
/// contract). A leading UTF-8 BOM (Windows editors) is stripped first or it
/// would defeat the `---` fence check. Content without a `---` fence yields
/// an empty mapping and the whole text as body.
pub fn parse_frontmatter(content: &str) -> (Mapping, String) {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let Some((yaml_content, body)) = split_fence(content) else {
        return (Mapping::new(), content.to_string());
    };
    let frontmatter = match serde_yaml::from_str::<serde_yaml::Value>(yaml_content) {
        Ok(serde_yaml::Value::Mapping(map)) => map,
        _ => fallback_key_value(yaml_content),
    };
    (frontmatter, body.to_string())
}

/// Parse a full SKILL.md document.
pub fn parse_skill_doc(content: &str) -> SkillDoc {
    let (frontmatter, body) = parse_frontmatter(content);
    SkillDoc { frontmatter, body }
}

/// `("yaml lines", "body")` when the content opens with `---` and closes the
/// frontmatter with a `\n---` fence, `None` otherwise. Mirrors the Python
/// regex `re.search(r"\n---\s*\n", content[3:])` on a `content.startswith("---")` guard.
fn split_fence(content: &str) -> Option<(&str, &str)> {
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    // Find "\n---" optionally followed by spaces/tabs, then a newline.
    let mut search_from = 0;
    while let Some(pos) = rest[search_from..].find("\n---") {
        let abs = search_from + pos;
        search_from = abs + 1;
        let mut cursor = abs + 4; // past "\n---"
        let bytes = rest.as_bytes();
        while cursor < bytes.len() && (bytes[cursor] == b' ' || bytes[cursor] == b'\t') {
            cursor += 1;
        }
        if cursor < bytes.len() && bytes[cursor] == b'\n' {
            let yaml = &rest[..abs];
            let body = &rest[cursor + 1..];
            return Some((yaml, body));
        }
        if cursor == bytes.len() {
            // Fence closed by EOF with trailing spaces only; Python's regex
            // requires a trailing \n, so treat as unterminated.
            return None;
        }
    }
    None
}

/// Malformed-YAML fallback: split every `key: value` line (values are kept as
/// raw strings, matching the Python `frontmatter[key.strip()] = value.strip()`).
fn fallback_key_value(yaml_content: &str) -> Mapping {
    let mut map = Mapping::new();
    for line in yaml_content.trim().split('\n') {
        if let Some((key, value)) = line.split_once(':') {
            map.insert(
                serde_yaml::Value::String(key.trim().to_string()),
                serde_yaml::Value::String(value.trim().to_string()),
            );
        }
    }
    map
}

/// Description over the 1024-char ceiling is hard-truncated with a `...` suffix
/// (`_truncate_description`).
pub fn truncate_description(description: &str) -> String {
    if description.len() <= MAX_DESCRIPTION_LENGTH {
        return description.to_string();
    }
    let mut end = MAX_DESCRIPTION_LENGTH - 3;
    while !description.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &description[..end])
}

/// Catalog name: frontmatter `name` (or fallback) truncated to 64 chars.
pub fn truncate_name(name: &str) -> String {
    let mut end = MAX_NAME_LENGTH.min(name.len());
    while end < name.len() && !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}

/// Validation errors mirroring the validator/linter hard requirements.
pub fn validate_skill_doc(doc: &SkillDoc) -> Vec<String> {
    let mut errors = Vec::new();
    let name = doc.name();
    if name.is_empty() {
        errors.push("frontmatter must declare a `name` field".to_string());
    } else {
        let name_len = name.chars().count();
        if name_len > MAX_NAME_LENGTH {
            errors.push(format!(
                "name '{name}' exceeds {MAX_NAME_LENGTH} chars (MAX_NAME_LENGTH)"
            ));
        }
        if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
            || !name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        {
            errors.push(format!(
                "name '{name}' must be lowercase letters, digits, hyphens, or underscores"
            ));
        }
    }
    let description = doc.description();
    if description.is_empty() {
        errors.push("frontmatter must declare a `description` field".to_string());
    } else if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        errors.push(format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} chars"
        ));
    }
    if doc.body.trim().is_empty() {
        errors.push("SKILL.md body is empty after the closing `---`".to_string());
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_fixture_frontmatter() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/gif-search-SKILL.md"
        ))
        .unwrap();
        let doc = parse_skill_doc(&raw);
        assert_eq!(doc.name(), "gif-search");
        assert_eq!(doc.description(), "Search/download GIFs from Tenor via curl + jq.");
        assert!(doc.body.trim_start().starts_with("# GIF Search"));
        assert!(validate_skill_doc(&doc).is_empty());
        // nested metadata survives YAML parsing
        let meta = doc.frontmatter.get("metadata").unwrap();
        let meta = meta.as_mapping().unwrap();
        let hermes = meta.get(serde_yaml::Value::String("hermes".into())).unwrap();
        let hermes = hermes.as_mapping().unwrap();
        let tags = hermes
            .get(serde_yaml::Value::String("tags".into()))
            .unwrap()
            .as_sequence()
            .unwrap();
        assert!(!tags.is_empty());
    }

    #[test]
    fn no_fence_yields_empty_frontmatter_and_full_body() {
        let (fm, body) = parse_frontmatter("just some text\n");
        assert!(fm.is_empty());
        assert_eq!(body, "just some text\n");
    }

    #[test]
    fn bom_is_stripped_before_fence_check() {
        let raw = "\u{feff}---\nname: test-skill\n---\n\nBody here.\n";
        let (fm, body) = parse_frontmatter(raw);
        assert_eq!(
            fm.get("name").and_then(|v| v.as_str()),
            Some("test-skill")
        );
        assert_eq!(body, "\nBody here.\n");
    }

    #[test]
    fn malformed_yaml_falls_back_to_key_value_splitting() {
        let raw = "---\nname: broken-skill\ndescription: no closing quote \"\n---\n\nBody.\n";
        let (fm, _) = parse_frontmatter(raw);
        assert_eq!(fm.get("name").and_then(|v| v.as_str()), Some("broken-skill"));
        assert_eq!(
            fm.get("description").and_then(|v| v.as_str()),
            Some("no closing quote \"")
        );
    }

    #[test]
    fn description_is_truncated_at_1024_with_ellipsis() {
        let long = "a".repeat(2000);
        let t = truncate_description(&long);
        assert_eq!(t.len(), 1024);
        assert!(t.ends_with("..."));
        let short = "x".repeat(1024);
        assert_eq!(truncate_description(&short), short);
    }

    #[test]
    fn name_is_truncated_at_64_chars() {
        assert_eq!(truncate_name(&"a".repeat(80)).len(), 64);
        assert_eq!(truncate_name("short"), "short");
    }

    #[test]
    fn validation_reports_every_hard_requirement() {
        let doc = parse_skill_doc("---\nname: Bad_Name\n---\n\nbody\n");
        let errs = validate_skill_doc(&doc);
        assert!(errs.iter().any(|e| e.contains("description")));
        assert!(errs.iter().any(|e| e.contains("lowercase")));

        let doc = parse_skill_doc(&format!(
            "---\nname: {}\ndescription: d\n---\n\nbody\n",
            "a".repeat(80)
        ));
        assert!(validate_skill_doc(&doc).iter().any(|e| e.contains("64 chars")));

        let doc = parse_skill_doc("---\nname: ok\ndescription: d\n---\n");
        assert!(validate_skill_doc(&doc).iter().any(|e| e.contains("body")));
    }
}
