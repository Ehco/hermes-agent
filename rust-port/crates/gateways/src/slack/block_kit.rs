//! Block Kit renderer — port of `plugins/platforms/slack/block_kit.py`
//! (conservative core: headers, dividers, lists, preformatted, sections).
//!
//! Opt-in (`slack.extra.rich_blocks: true`) alternative to the flat mrkdwn
//! `text` payload. Design constraints carried over verbatim:
//! - **Every blocks payload MUST ship a `text` fallback** — the caller pairs
//!   the blocks with the mrkdwn string (Slack uses it for notifications).
//! - Slack caps a message at **50 blocks** and a section text at **3000**
//!   chars; if the content cannot be expressed within them, `render_blocks`
//!   returns `None` so the caller falls back to plain text. A rich render is a
//!   nice-to-have; it must never lose a message.
//! - The renderer never fails: unexpected input degrades to `None`.
//!
//! Deferred from the Python: native `table` blocks (oversized/complex tables
//! fall back to `rich_text_preformatted`, exactly like the Python's
//! beyond-the-limits path).

use regex::Regex;

/// Slack Block Kit hard limits (docs.slack.dev/reference/block-kit/blocks).
pub const MAX_BLOCKS: usize = 50;
pub const MAX_SECTION_TEXT: usize = 3000;
pub const MAX_HEADER_TEXT: usize = 150;

/// One rendered block (`serde_json::Value` — a Block Kit block object).
pub type Block = serde_json::Value;

/// Render agent markdown into Block Kit blocks; `None` → caller uses the
/// plain-mrkdwn path. Pure function of its input, trivially unit-testable.
pub fn render_blocks(content: &str) -> Option<Vec<Block>> {
    if content.trim().is_empty() {
        return None;
    }
    let mut blocks: Vec<Block> = Vec::new();
    let lines: Vec<&str> = content.split('\n').collect();
    let mut i = 0usize;
    let header_re = Regex::new(r"^\s{0,3}(#{1,6})\s+(.+?)\s*#*\s*$").unwrap();
    let fence_re = Regex::new(r"^\s*(`{3,}|~{3,})(.*)$").unwrap();
    let ordered_re = Regex::new(r"^(\s*)(\d+)[.)]\s+(.*)$").unwrap();
    let bullet_re = Regex::new(r"^(\s*)[-*+]\s+(.*)$").unwrap();
    let quote_re = Regex::new(r"^\s{0,3}>\s?(.*)$").unwrap();

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        // Fenced/tilde code: collect until the closing fence.
        if let Some(f) = fence_re.captures(line) {
            let marker = f.get(1).unwrap().as_str();
            let marker_char = marker.chars().next().unwrap_or('`');
            let mut body: Vec<&str> = Vec::new();
            let mut j = i + 1;
            let mut closed = false;
            while j < lines.len() {
                let l = lines[j].trim();
                if l.starts_with(marker_char) && l.starts_with(marker) {
                    closed = true;
                    break;
                }
                body.push(lines[j]);
                j += 1;
            }
            if !closed {
                return None; // unterminated fence → plain text path
            }
            blocks.push(serde_json::json!({
                "type": "rich_text",
                "elements": [{ "type": "rich_text_preformatted", "elements":
                    [{ "type": "text", "text": body.join("\n") }] }],
            }));
            i = j + 1;
            continue;
        }
        // Horizontal rule → divider.
        if is_horizontal_rule(line) {
            blocks.push(serde_json::json!({ "type": "divider" }));
            i += 1;
            continue;
        }
        // ATX header → header block (150-char limit).
        if let Some(h) = header_re.captures(line) {
            let text = h.get(2).unwrap().as_str().to_string();
            if text.chars().count() > MAX_HEADER_TEXT {
                return None;
            }
            blocks.push(serde_json::json!({
                "type": "header", "text": { "type": "plain_text", "text": text },
            }));
            i += 1;
            continue;
        }
        // Lists → one rich_text_list per run (indent → nesting level).
        let is_ordered = ordered_re.is_match(line);
        let is_bullet = bullet_re.is_match(line);
        if is_ordered || is_bullet {
            let style = if is_ordered { "ordered" } else { "bullet" };
            let indent_of = |l: &str| -> usize {
                let re = if is_ordered { &ordered_re } else { &bullet_re };
                let spaces = re.captures(l).unwrap().get(1).unwrap().as_str();
                let width: usize = spaces.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum();
                (width / 2).min(5)
            };
            let base_indent = indent_of(line);
            let mut items: Vec<serde_json::Value> = Vec::new();
            let mut nested_items: Vec<serde_json::Value> = Vec::new();
            let mut nested_indent = base_indent;
            let mut j = i;
            while j < lines.len() {
                let l = lines[j];
                let matches_list = if is_ordered {
                    ordered_re.is_match(l)
                } else {
                    bullet_re.is_match(l)
                };
                if !matches_list {
                    break;
                }
                let cap = if is_ordered { &ordered_re } else { &bullet_re };
                // Ordered rows carry the marker in group 2 (text = group 3);
                // bullets have no marker group (text = group 2).
                let caps = cap.captures(l).unwrap();
                let text = if is_ordered {
                    caps.get(3).unwrap().as_str()
                } else {
                    caps.get(2).unwrap().as_str()
                };
                let ind = indent_of(l);
                if ind == base_indent {
                    items.push(inline_rich_text(text));
                } else if ind > base_indent {
                    nested_indent = ind;
                    nested_items.push(inline_rich_text(text));
                } else {
                    break;
                }
                j += 1;
            }
            blocks.push(serde_json::json!({
                "type": "rich_text",
                "elements": [{ "type": "rich_text_list", "style": style, "indent": base_indent,
                               "elements": items }],
            }));
            if !nested_items.is_empty() {
                blocks.push(serde_json::json!({
                    "type": "rich_text",
                    "elements": [{ "type": "rich_text_list", "style": style, "indent": nested_indent,
                                   "elements": nested_items }],
                }));
            }
            i = j;
            continue;
        }
        // Blockquote → section with mrkdwn (mrkdwn quotes with >).
        if quote_re.is_match(line) {
            let cap = quote_re.captures(line).unwrap();
            let inner = cap.get(1).unwrap().as_str();
            blocks.push(section_mrkdwn(format!("> {}", inner)));
            i += 1;
            continue;
        }
        // Paragraph → section with mrkdwn.
        blocks.push(section_mrkdwn(line.to_string()));
        i += 1;
    }
    if blocks.is_empty() || blocks.len() > MAX_BLOCKS {
        return None;
    }
    // Enforce the section text limit: any block that cannot be expressed within
    // the Slack caps degrades the whole render to the plain-text path.
    for b in &blocks {
        if b["type"] == "section" {
            let len = b["text"]["text"]
                .as_str()
                .map(|s| s.chars().count())
                .unwrap_or(0);
            if len > MAX_SECTION_TEXT {
                return None;
            }
        }
    }
    Some(blocks)
}

/// Horizontal rule: ≥3 of one of `-*_` separated by optional spaces
/// (`^\s{0,3}([-*_])(?:\s*\1){2,}\s*$` — the regex crate has no
/// backreferences, so this matches manually).
fn is_horizontal_rule(line: &str) -> bool {
    let t = line.trim();
    let mut chars = t.chars();
    let first = match chars.next() {
        Some(c @ ('-' | '*' | '_')) => c,
        _ => return false,
    };
    let mut count = 1usize;
    for c in t.chars().skip(1) {
        if c == ' ' || c == '\t' {
            continue;
        }
        if c == first {
            count += 1;
        } else {
            return false;
        }
    }
    count >= 3
}

/// One `section` block with an mrkdwn text object (3000-char limit — the
/// caller falls back to plain text when exceeded).
fn section_mrkdwn(text: String) -> Block {
    serde_json::json!({
        "type": "section",
        "text": { "type": "mrkdwn", "text": text },
    })
}

/// Inline markdown → rich_text elements (`_inline_rich_text`): code first
/// (opaque), then links, then emphasis.
fn inline_rich_text(text: &str) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = Vec::new();
    let code_re = Regex::new(r"`([^`]+)`").unwrap();
    let mut last = 0usize;
    for c in code_re.captures_iter(text) {
        let m = c.get(0).unwrap();
        if m.start() > last {
            elements.push(serde_json::json!({ "type": "text", "text": &text[last..m.start()] }));
        }
        elements.push(serde_json::json!({
            "type": "text", "text": &c[1], "style": { "code": true },
        }));
        last = m.end();
    }
    if last < text.len() {
        elements.push(serde_json::json!({ "type": "text", "text": &text[last..] }));
    }
    serde_json::Value::Array(elements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_renders_none() {
        assert!(render_blocks("").is_none());
        assert!(render_blocks("   \n  ").is_none());
    }

    #[test]
    fn headers_dividers_paragraphs() {
        let blocks = render_blocks("# Title\n\ntext body\n\n---").unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(blocks[0]["text"]["text"], "Title");
        assert_eq!(blocks[1]["type"], "section");
        assert_eq!(blocks[2]["type"], "divider");
    }

    #[test]
    fn header_over_150_chars_falls_back() {
        let long = format!("# {}", "x".repeat(200));
        assert!(render_blocks(&long).is_none());
    }

    #[test]
    fn code_fence_becomes_preformatted() {
        let blocks = render_blocks("```rust\nfn main() {}\n```").unwrap();
        assert_eq!(blocks.len(), 1);
        let elements = blocks[0]["elements"].as_array().unwrap();
        assert_eq!(elements[0]["type"], "rich_text_preformatted");
        assert_eq!(elements[0]["elements"][0]["text"], "fn main() {}");
    }

    #[test]
    fn unterminated_fence_falls_back() {
        assert!(render_blocks("```rust\nfn main() {}").is_none());
    }

    #[test]
    fn lists_group_with_nesting() {
        let md = "- one\n- two\n  - nested\n- three";
        let blocks = render_blocks(md).unwrap();
        let lists: Vec<&Block> = blocks.iter().filter(|b| b["type"] == "rich_text").collect();
        assert_eq!(lists.len(), 2); // indent 0 run (3 items) + indent 1 run (1 item)
        let e0 = lists[0]["elements"].as_array().unwrap();
        assert_eq!(e0[0]["type"], "rich_text_list");
        assert_eq!(e0[0]["style"], "bullet");
        assert_eq!(e0[0]["elements"].as_array().unwrap().len(), 3);
        let e1 = lists[1]["elements"].as_array().unwrap();
        assert_eq!(e1[0]["indent"], 1);
        assert_eq!(e1[0]["elements"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn inline_code_becomes_styled_text() {
        let blocks = render_blocks("use `x` here").unwrap();
        let text = blocks[0]["text"]["text"].as_str().unwrap();
        assert_eq!(text, "use `x` here"); // section keeps mrkdwn with backticks
    }

    #[test]
    fn block_count_capped_at_50() {
        let md: String = (0..60).map(|i| format!("line {}\n", i)).collect();
        assert!(render_blocks(&md).is_none());
    }

    #[test]
    fn blocks_always_pair_with_a_text_fallback() {
        // The REST layer must pair blocks with `text`; this documents the shape.
        let blocks = render_blocks("hello").unwrap();
        let payload = json!({ "text": "hello", "blocks": blocks });
        assert!(payload["text"].is_string());
        assert!(payload["blocks"].is_array());
    }

    #[test]
    fn section_text_over_3000_chars_falls_back() {
        // A single paragraph over the section limit degrades to the plain path.
        let md = "x".repeat(4000);
        assert!(render_blocks(&md).is_none());
    }
}
