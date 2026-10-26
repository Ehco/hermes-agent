//! Multiline input — port of the Python classic-CLI line-continuation
//! mechanism (`hermes_cli/cli_terminal_input.py`):
//!
//! ```text
//! _BACKSLASH_LINE_CONTINUATION_RE = re.compile(r"\\[ \t]*$")
//! def _is_backslash_line_continuation(text): ...
//! def _apply_backslash_line_continuation(text): ...
//! ```
//!
//! A trailing `\` (optionally followed by spaces/tabs) turns Enter into a
//! newline and keeps the REPL reading lines until a line arrives without the
//! marker. (The Python TUI also binds Ctrl+J to newline; that keybinding lives
//! in prompt_toolkit, which the line-based Rust REPL does not use.)

/// True when Enter should turn a trailing backslash into a newline
/// (port of `_is_backslash_line_continuation` / `\\[ \t]*$`).
pub fn is_backslash_line_continuation(text: &str) -> bool {
    let trimmed = text.trim_end_matches([' ', '\t']);
    trimmed.ends_with('\\')
}

/// Replace a trailing `\[ \t]*` marker with an actual newline (port of
/// `_apply_backslash_line_continuation`).
pub fn apply_backslash_line_continuation(text: &str) -> String {
    let trimmed = text.trim_end_matches([' ', '\t']);
    let end = trimmed.len().saturating_sub(1); // drop the trailing backslash
    format!("{}\n", &trimmed[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_trailing_backslash_with_optional_whitespace() {
        assert!(is_backslash_line_continuation("first \\"));
        assert!(is_backslash_line_continuation("first \\\t  "));
        assert!(is_backslash_line_continuation("\\"));
        assert!(!is_backslash_line_continuation("first\\mid")); // not at end
        assert!(!is_backslash_line_continuation("plain line"));
        assert!(!is_backslash_line_continuation("ends with spaces   "));
    }

    #[test]
    fn strips_marker_and_appends_newline() {
        assert_eq!(apply_backslash_line_continuation("first \\"), "first \n");
        assert_eq!(apply_backslash_line_continuation("first \\\t "), "first \n");
        assert_eq!(apply_backslash_line_continuation("\\"), "\n");
    }

    #[test]
    fn joins_a_full_multiline_entry() {
        // The sequence the REPL sees for: "one \
        // two \
        // three"
        let joined = format!(
            "{}{}{}",
            apply_backslash_line_continuation("one \\"),
            apply_backslash_line_continuation("two \\"),
            "three",
        );
        assert_eq!(joined, "one \ntwo \nthree");
    }
}
