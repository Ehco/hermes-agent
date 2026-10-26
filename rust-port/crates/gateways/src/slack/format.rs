//! Standard markdown → Slack mrkdwn — port of
//! `plugins/platforms/slack/adapter.py::format_message`.
//!
//! Contract carried over from the Python:
//! - tables are fenced first (and column-aligned) — `crate::tables`,
//! - code is stashed behind placeholders and never modified (Slack renders the
//!   fence language tag literally, so line-start fences lose the tag; a mid-line
//!   ``` is real content and keeps its text),
//! - broadcast mentions (`<!everyone|channel|here>`) are escaped so bot output
//!   can't ping @channel,
//! - HTML entities decode first, then `&<>` escape — in ONE pass (sequential
//!   replaces would decode `&amp;lt;` twice),
//! - emphasis converts in order: `***x***` → `*_x_*`, `**x**` → `*x*` (with a
//!   zero-width space guarding a trailing non-word char Slack would eat),
//!   `*x*` → `_x_`, `~~x~~` → `~x~`,
//! - links `[label](url)` → `<url|label>`; pre-existing `<…>` autolinks and
//!   mentions pass through untouched.

use regex::Regex;
use std::collections::HashMap;

use crate::tables::wrap_markdown_tables;

/// Stash `value` behind a placeholder token that survives all later passes
/// (the Python `\x00SL{n}\x00` tokens).
struct Placeholders {
    map: HashMap<String, String>,
    order: Vec<String>,
    counter: usize,
}

impl Placeholders {
    fn add(&mut self, value: String) -> String {
        let key = format!("\u{0}SL{}\u{0}", self.counter);
        self.counter += 1;
        self.map.insert(key.clone(), value);
        self.order.push(key.clone());
        key
    }
    /// Restore placeholders in reverse insertion order (the Python iterates
    /// `reversed(placeholders)` over its insertion-ordered dict).
    fn restore(&self, mut text: String) -> String {
        for key in self.order.iter().rev() {
            if let Some(v) = self.map.get(key) {
                text = text.replace(key, v);
            }
        }
        text
    }
}

/// Convert standard markdown to Slack mrkdwn (`SlackAdapter.format_message`).
pub fn format_message(content: &str) -> String {
    if content.is_empty() {
        return content.to_string();
    }
    let mut ph = Placeholders { map: HashMap::new(), order: Vec::new(), counter: 0 };

    // Tables → fenced + aligned (mrkdwn shows pipe tables literally).
    let content = wrap_markdown_tables(content);

    // Broadcast mentions ping even from bots; escape the leading `<` keeping
    // the rest of the tag verbatim (`_SLACK_SPECIAL_MENTION_RE`).
    let mention_re = Regex::new(r"(?i)<!((?:everyone|channel|here))(?:\|[^>\n]*)?>").unwrap();
    let content = mention_re
        .replace_all(&content, |c: &regex::Captures| {
            // Python: m.group(0).replace("<", "&lt;", 1) — the whole tag stays,
            // only the leading `<` escapes.
            format!("&lt;{}", &c[0][1..])
        })
        .to_string();

    // Fences: Slack renders the language tag literally, so drop it — only for a
    // line-start fence; a mid-line ``` is real content (`_protect_fence`).
    let fence_tag_re = Regex::new(r"(?m)^```[^\s`]+[ \t]*(\r?\n)").unwrap();
    let content = fence_tag_re
        .replace_all(&content, |c: &regex::Captures| format!("```{}", &c[1]))
        .to_string();

    // Ordered passes: protect code/links/entities/quotes, escape, then convert
    // emphasis. Code first (opaque), then links.
    let fence_re = Regex::new(r"(?s)```[\s\S]*?```").unwrap();
    let content = fence_re
        .replace_all(&content, |c: &regex::Captures| ph.add(c[0].to_string()))
        .to_string();

    let inline_re = Regex::new(r"`[^`]+`").unwrap();
    let content = inline_re
        .replace_all(&content, |c: &regex::Captures| ph.add(c[0].to_string()))
        .to_string();

    // Markdown links → `<url|label>`; the regex crate has no lookbehind for the
    // Python's `(?<!!)`, so capture the optional `!` and skip the image form.
    let link_re = Regex::new(r"(!?)\[([^\]]+)\]\(([^()]*(?:\([^()]*\)[^()]*)*)\)").unwrap();
    let content = link_re
        .replace_all(&content, |c: &regex::Captures| {
            if &c[1] == "!" {
                return c[0].to_string();
            }
            let mut url = c[3].trim().to_string();
            if url.starts_with('<') && url.ends_with('>') {
                url = url[1..url.len() - 1].trim().to_string();
            }
            ph.add(format!("<{}|{}>", url, &c[2]))
        })
        .to_string();

    // Existing slack autolinks/mentions `<@U…>` `<#C…>` `<!here>` `<https…>` —
    // protected from the escape step.
    let entity_re = Regex::new(r"<(?:[@#!]|(?:https?|mailto|tel):)[^>\n]+>").unwrap();
    let content = entity_re
        .replace_all(&content, |c: &regex::Captures| ph.add(c[0].to_string()))
        .to_string();

    // Blockquote prefixes (protected so the escape step can't break them).
    let quote_re = Regex::new(r"(?m)^>+\s").unwrap();
    let mut text = quote_re
        .replace_all(&content, |c: &regex::Captures| ph.add(c[0].to_string()))
        .to_string();

    // The escape step, in ONE pass: decode HTML entities first (sequential
    // replaces would decode `&amp;lt;` twice), then escape `& < >`.
    for (ent, ch) in [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
    ] {
        text = text.replace(ent, ch);
    }
    let text = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");

    // Headers: `# Title` → `*Title*`; a trailing `###` closer is stripped and
    // inner `**` flattened.
    let header_re = Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap();
    let inner_bold = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    let text = header_re
        .replace_all(&text, |c: &regex::Captures| {
            let inner = inner_bold.replace_all(c[1].trim().trim_end_matches('#'), "$1");
            ph.add(format!("*{}*", inner))
        })
        .to_string();

    // Emphasis, outermost first.
    let bold_italic_re = Regex::new(r"\*\*\*(.+?)\*\*\*").unwrap();
    let text = bold_italic_re
        .replace_all(&text, |c: &regex::Captures| ph.add(format!("*_{}_*", &c[1])))
        .to_string();

    let bold_re = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    let text = bold_re
        .replace_all(&text, |c: &regex::Captures| {
            // Slack misses a closing * after a non-word char and silently
            // truncates the message; insert U+200B before it.
            let zw = match c[1].chars().next_back() {
                Some(ch) if !(ch.is_alphanumeric() || ch == '_') => "\u{200b}",
                _ => "",
            };
            ph.add(format!("*{}{}*", &c[1], zw))
        })
        .to_string();

    let text = convert_italic(&text, &mut ph);
    let strike_re = Regex::new(r"~~(.+?)~~").unwrap();
    let text = strike_re
        .replace_all(&text, |c: &regex::Captures| ph.add(format!("~{}~", &c[1])))
        .to_string();

    ph.restore(text)
}

/// Italic `*x*` → `_x_` — port of the Python's
/// `(?<!\*)\*(\S(?:[^*\n]*?\S)?)\*(?!\*)` (the regex crate has no lookaround,
/// so this scans manually). Only spans with non-whitespace touching both
/// delimiters convert ("a * b * c" stays).
fn convert_italic(text: &str, ph: &mut Placeholders) -> String {
    let b = text.as_bytes();
    let n = b.len();
    let mut spans: Vec<(usize, usize)> = Vec::new(); // (open, close) * positions
    let mut i = 0usize;
    while i < n {
        if b[i] == b'*' && (i == 0 || b[i - 1] != b'*') {
            let inner_start = i + 1;
            // Inner: non-space first char, no `*`/newline, non-space last char.
            let mut j = inner_start;
            let mut close: Option<usize> = None;
            let mut valid = inner_start < n
                && b[inner_start] != b'\n'
                && !(b[inner_start] as char).is_whitespace();
            while valid && j < n {
                match b[j] {
                    b'*' => {
                        let next_star = j + 1 < n && b[j + 1] == b'*';
                        let last_space = (b[j - 1] as char).is_whitespace();
                        if !next_star && !last_space {
                            close = Some(j);
                        }
                        break;
                    }
                    b'\n' => valid = false,
                    _ => j += 1,
                }
            }
            if let (true, Some(close)) = (valid, close) {
                spans.push((i, close));
                i = close + 1;
                continue;
            }
        }
        i += 1;
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for (open, close) in spans {
        out.push_str(&text[last..open]);
        out.push_str(&ph.add(format!("_{}_", &text[open + 1..close])));
        last = close + 1;
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_passes_through() {
        assert_eq!(format_message(""), "");
    }

    #[test]
    fn emphasis_chain() {
        assert_eq!(format_message("**bold**"), "*bold*");
        assert_eq!(format_message("***both***"), "*_both_*");
        assert_eq!(format_message("~~gone~~"), "~gone~");
        assert_eq!(format_message("plain"), "plain");
    }

    #[test]
    fn italic_needs_word_touching_delimiters() {
        assert_eq!(format_message("a * b * c"), "a * b * c");
        assert_eq!(format_message("*it*"), "_it_");
    }

    #[test]
    fn bold_before_non_word_gets_zero_width_guard() {
        let out = format_message("**done!**");
        assert!(out.starts_with("*done!"));
        assert!(out.contains('\u{200b}'));
        assert!(out.ends_with('*'));
    }

    #[test]
    fn headers_to_bold() {
        assert_eq!(format_message("# Title"), "*Title*");
        assert_eq!(format_message("### Deep **bold**"), "*Deep bold*");
        assert_eq!(format_message("no header\n## Sub"), "no header\n*Sub*");
    }

    #[test]
    fn links_to_slack_autolink() {
        assert_eq!(format_message("[docs](https://e.com/x)"), "<https://e.com/x|docs>");
        assert_eq!(format_message("[label](<https://e.com/y>)"), "<https://e.com/y|label>");
        // !-prefixed (image) links pass through.
        assert_eq!(format_message("![alt](https://e.com/i.png)"), "![alt](https://e.com/i.png)");
    }

    #[test]
    fn inline_code_and_fences_are_protected() {
        assert_eq!(format_message("`**not bold**`"), "`**not bold**`");
        let fenced = "```rust\n**keep**\n```";
        assert_eq!(format_message(fenced), "```\n**keep**\n```");
        // Mid-line ``` is real content: kept verbatim.
        assert_eq!(format_message("a ```x b"), "a ```x b");
    }

    #[test]
    fn backticks_inside_fence_survive() {
        let fenced = "```\nuse `x` here\n```";
        assert_eq!(format_message(fenced), "```\nuse `x` here\n```");
    }

    #[test]
    fn special_mentions_escaped() {
        // Escape, then the global entity round-trip re-escapes the trailing >:
        // "&lt;!channel>" decodes to "<!channel>" and escapes to "&lt;!channel&gt;".
        assert_eq!(format_message("<!channel>"), "&lt;!channel&gt;");
        assert_eq!(format_message("<!here>"), "&lt;!here&gt;");
        assert_eq!(format_message("<!everyone|x>"), "&lt;!everyone|x&gt;");
    }

    #[test]
    fn user_mentions_kept() {
        let out = format_message("hi <@U123>");
        assert!(out.contains("<@U123>"), "{}", out);
    }

    #[test]
    fn entities_decode_then_escape_once() {
        assert_eq!(format_message("a &amp; b"), "a &amp; b");
        assert_eq!(format_message("a &lt; b"), "a &lt; b");
        assert_eq!(format_message("raw < & >"), "raw &lt; &amp; &gt;");
    }

    #[test]
    fn tables_fenced() {
        let table = "| a | b |\n| --- | --- |\n| 1 | 2 |";
        let out = format_message(table);
        assert!(out.starts_with("```"), "{}", out);
        assert!(out.ends_with("```"));
    }

    #[test]
    fn nested_construction_round_trips() {
        let out = format_message("**bold with `code` inside**");
        assert!(out.contains("*bold with `code` inside*"), "{}", out);
    }

    #[test]
    fn blockquotes_survive_escaping() {
        let out = format_message("> quoted <tag>");
        assert!(out.contains("> quoted &lt;tag&gt;"), "{}", out);
    }
}
