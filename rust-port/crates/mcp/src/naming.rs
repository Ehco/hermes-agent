//! Tool-name namespacing — a faithful port of the Python
//! `tools/mcp_tool_schema.py::mcp_prefixed_tool_name`:
//!
//! Registry/wire name `mcp__<sanitizedServer>__<sanitizedTool>` (the
//! convention shared by Claude Code, Codex and OpenCode), clamped to 64
//! chars with a deterministic sha256-derived suffix when the natural name is
//! longer (OpenAI-compatible providers reject names over 64 chars).

use sha2::{Digest, Sha256};

pub const MCP_TOOL_NAME_PREFIX: &str = "mcp__";

const MCP_TOOL_NAME_MAX_LENGTH: usize = 64;
const MCP_TOOL_NAME_HASH_LENGTH: usize = 8;

/// Python `sanitize_mcp_name_component`: `re.sub(r"[^A-Za-z0-9_]", "_", value)`.
pub fn sanitize_name_component(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

/// `mcp__<sanitizedServer>__<sanitizedTool>`, clamped to 64 chars with a
/// stable hash suffix when the natural name is longer.
pub fn prefixed_tool_name(server_name: &str, tool_name: &str) -> String {
    let full_name = format!(
        "{}{}__{}",
        MCP_TOOL_NAME_PREFIX,
        sanitize_name_component(server_name),
        sanitize_name_component(tool_name)
    );
    let bytes = full_name.as_bytes();
    if bytes.len() <= MCP_TOOL_NAME_MAX_LENGTH {
        return full_name;
    }
    let digest = Sha256::digest(bytes);
    let suffix = format!(
        "_{}",
        hex(&digest)[..MCP_TOOL_NAME_HASH_LENGTH].to_string()
    );
    let keep = MCP_TOOL_NAME_MAX_LENGTH - suffix.len();
    // Slice on char boundaries (sanitized names are ASCII, but stay safe).
    let mut end = keep;
    while !full_name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &full_name[..end], suffix)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_namespacing() {
        assert_eq!(prefixed_tool_name("github", "create_issue"), "mcp__github__create_issue");
    }

    #[test]
    fn sanitizes_non_identifier_chars() {
        // Dashes and dots fold to underscores, exactly like the Python regex.
        assert_eq!(
            prefixed_tool_name("my-server.io", "tool-1"),
            "mcp__my_server_io__tool_1"
        );
    }

    #[test]
    fn short_names_untouched_by_clamp() {
        let name = prefixed_tool_name("s", "t");
        assert_eq!(name.len() as usize, "mcp__s__t".len());
        assert_eq!(name, "mcp__s__t");
    }

    #[test]
    fn long_names_clamped_with_hash_suffix() {
        let long_server = "a".repeat(40);
        let long_tool = "b".repeat(40);
        let name = prefixed_tool_name(&long_server, &long_tool);
        assert_eq!(name.len(), 64);
        // Deterministic: same input, same suffix.
        assert_eq!(prefixed_tool_name(&long_server, &long_tool), name);
        // The suffix matches the sha256 of the full name, hex[:8], after "_".
        let full = format!(
            "mcp__{}__{}",
            sanitize_name_component(&long_server),
            sanitize_name_component(&long_tool)
        );
        let digest = Sha256::digest(full.as_bytes());
        let expected_suffix = format!("_{}", &hex(&digest)[..8]);
        assert!(name.ends_with(&expected_suffix));
        assert!(name.starts_with(&full[..64 - expected_suffix.len()]));
    }

    #[test]
    fn matches_python_reference_vector() {
        // Cross-checked against Python:
        //   sanitize_mcp_name_component("my server!") == "my_server_"
        //   mcp_prefixed_tool_name("my server!", "do thing") == "mcp__my_server___do_thing"
        assert_eq!(sanitize_name_component("my server!"), "my_server_");
        assert_eq!(
            prefixed_tool_name("my server!", "do thing"),
            "mcp__my_server___do_thing"
        );
    }
}
