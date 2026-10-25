//! Rendering of MCP tool-result content blocks into the model-facing
//! tool-result string (port of `tools/mcp_tool_content.py` +
//! `tools/mcp_tool_handlers.py::_render_content_blocks`).
//!
//! Success: text blocks joined with "\n". `isError` results and transport
//! failures render as the registry's error JSON `{"error": ..., "tool": ...}`
//! — the same shape the Python server-side dispatch produces for a raised
//! tool (`json.dumps({"error": str(exc), "tool": tool_name})`), which the
//! agent loop treats opaquely.

use anyhow::{anyhow, Result};
use serde_json::json;
use serde_json::Value;

use crate::protocol::CallToolResult;

/// Hard cap for one MCP text payload (Python `_MCP_HARD_RESULT_CAP_CHARS`:
/// 500_000, head+tail truncation guards against pathological servers).
pub const MCP_HARD_RESULT_CAP_CHARS: usize = 500_000;

/// Render a completed `tools/call` into the tool-result string the agent
/// loop expects. `tool` is the unprefixed MCP tool name (diagnostics only).
pub fn render_call_result(result: &CallToolResult, tool: &str) -> String {
    let text = render_content_blocks(result);
    if result.is_error {
        error_result(&text, tool)
    } else {
        text
    }
}

/// Wrap an MCP failure (isError flag OR transport error) as the registry's
/// error payload.
pub fn error_result(message: &str, tool: &str) -> String {
    json!({"error": message, "tool": tool}).to_string()
}

/// Text blocks joined with "\n"; non-text blocks become a short `[...]`
/// marker (the Python renderer drops unsupported blocks with a warning and
/// only text contributes to the model-facing string).
pub fn render_content_blocks(result: &CallToolResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in &result.content {
        if block.kind == "text" {
            if let Some(t) = &block.text {
                parts.push(strip_unicode_tags(t));
            }
        } else {
            parts.push(format!("[{} content block]", block.kind));
        }
    }
    truncate_head_tail(&parts.join("\n"), MCP_HARD_RESULT_CAP_CHARS)
}

/// Python `strip_unicode_tags` (tools/ansi_strip.py): removes the invisible
/// plane-14 Unicode TAG characters (U+E0000–U+E007F, a prompt-injection
/// smuggling channel in untrusted tool output).
fn strip_unicode_tags(text: &str) -> String {
    text.chars()
        .filter(|c| !(*c as u32 >= 0xE0000 && *c as u32 <= 0xE007F))
        .collect()
}

/// Head+tail truncation (Python `truncate_head_tail`): keep the budget but
/// preserve both ends, eliding the middle.
fn truncate_head_tail(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    let label = format!("\n... [{} chars elided] ...\n", chars.len() - max_chars);
    let head = max_chars * 3 / 4;
    let tail = max_chars - head;
    let mut out: String = chars[..head].iter().collect();
    out.push_str(&label);
    out.extend(chars[chars.len() - tail..].iter());
    out
}

/// Parse the registry's error payload back out (test/binding convenience).
pub fn parse_error_payload(payload: &str) -> Result<(String, String)> {
    let v: Value = serde_json::from_str(payload).map_err(|_| anyhow!("not an error payload"))?;
    let err = v
        .get("error")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing error key"))?
        .to_string();
    let tool = v
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok((err, tool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn result(content: Value, is_error: bool) -> CallToolResult {
        serde_json::from_value(json!({"content": content, "isError": is_error})).unwrap()
    }

    #[test]
    fn joins_text_blocks_with_newlines() {
        let r = result(json!([
            {"type": "text", "text": "line one"},
            {"type": "text", "text": "line two"},
        ]), false);
        assert_eq!(render_call_result(&r, "t"), "line one\nline two");
    }

    #[test]
    fn non_text_blocks_become_markers() {
        let r = result(json!([
            {"type": "image", "data": "...", "mimeType": "image/png"},
            {"type": "text", "text": "after"},
        ]), false);
        assert_eq!(render_call_result(&r, "t"), "[image content block]\nafter");
    }

    #[test]
    fn is_error_flag_becomes_error_json() {
        let r = result(json!([{"type": "text", "text": "boom"}]), true);
        let rendered = render_call_result(&r, "mytool");
        let (err, tool) = parse_error_payload(&rendered).unwrap();
        assert_eq!(err, "boom");
        assert_eq!(tool, "mytool");
    }

    #[test]
    fn transport_error_shape() {
        let rendered = error_result("server closed the stdio transport", "t");
        let (err, tool) = parse_error_payload(&rendered).unwrap();
        assert!(err.contains("stdio"));
        assert_eq!(tool, "t");
    }

    #[test]
    fn oversized_text_is_elided_not_dropped() {
        let big = "x".repeat(MCP_HARD_RESULT_CAP_CHARS + 10);
        let r = result(json!([{"type": "text", "text": big}]), false);
        let rendered = render_call_result(&r, "t");
        assert!(rendered.contains("chars elided"));
        assert!(rendered.len() < MCP_HARD_RESULT_CAP_CHARS + 100);
    }

    #[test]
    fn strips_unicode_tag_chars() {
        // Plane-14 TAG chars (U+E0000–U+E007F) are stripped; other content stays.
        let tagged = "ok\u{E0041}\u{E0042}!";
        let r = result(json!([{"type": "text", "text": tagged}]), false);
        assert_eq!(render_call_result(&r, "t"), "ok!");
        let emoji = "👍\u{FE0F}";
        let r = result(json!([{"type": "text", "text": emoji}]), false);
        assert_eq!(render_call_result(&r, "t"), emoji);
    }
}
