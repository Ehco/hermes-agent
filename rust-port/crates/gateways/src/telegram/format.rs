//! Telegram markdown → MarkdownV2 conversion — port of
//! `plugins/platforms/telegram/adapter.py::format_message` / `_escape_mdv2` /
//! `_strip_mdv2`.
//!
//! Contract: code is stashed behind placeholders first (never modified),
//! markdown constructs become MarkdownV2 syntax, everything else is escaped
//! with the MarkdownV2 escape table.

use regex::Regex;
use std::collections::HashMap;

/// Every char MarkdownV2 requires backslash-escaped outside code spans/fences:
/// `_*[]()~>#+\-=|{}.!` plus the backslash itself.
pub fn escape_mdv2(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        if "_*[]()~`>#+-=|{}.!\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Strip MarkdownV2 escapes and formatting markers for the plain-text fallback.
pub fn strip_mdv2(text: &str) -> String {
    let esc_re = Regex::new(r"\\([_\*\[\]()~`>#\+\-=|{}.!\\])").unwrap();
    let cleaned = esc_re.replace_all(text, "$1").to_string();
    let bold_re = Regex::new(r"\*\*([^*]+)\*\*").unwrap();
    let cleaned = bold_re.replace_all(&cleaned, "$1").to_string();
    let ital_re = Regex::new(r"\*([^*]+)\*").unwrap();
    let cleaned = ital_re.replace_all(&cleaned, "$1").to_string();
    // Italic underscore: word-bounded so snake_case survives
    // (Python: `(?<!\w)_([^_]+)_(?!\w)`).
    let und_re = Regex::new(r"_([^_]+)_").unwrap();
    let mut cleaned2 = String::with_capacity(cleaned.len());
    let mut last = 0usize;
    for m in und_re.find_iter(&cleaned) {
        let (s, e) = (m.start(), m.end());
        let wordy = |c: Option<char>| c.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false);
        let prev = cleaned[..s].chars().next_back();
        let next = cleaned[e..].chars().next();
        if wordy(prev) || wordy(next) {
            cleaned2.push_str(&cleaned[last..e]); // keep as-is
        } else {
            cleaned2.push_str(&cleaned[last..s]);
            cleaned2.push_str(&m[1]);
        }
        last = e;
    }
    cleaned2.push_str(&cleaned[last..]);
    let mut cleaned = cleaned2;
    let strike_re = Regex::new(r"~([^~]+)~").unwrap();
    cleaned = strike_re.replace_all(&cleaned, "$1").to_string();
    let spoiler_re = Regex::new(r"\|\|([^|]+)\|\|").unwrap();
    cleaned = spoiler_re.replace_all(&cleaned, "$1").to_string();
    cleaned
}

/// Stash `value` behind a placeholder token that survives escaping.
#[derive(Default)]
struct Placeholders {
    map: HashMap<String, String>,
    order: Vec<String>,
    counter: usize,
}

impl Placeholders {
    fn add(&mut self, value: String) -> String {
        let key = format!("\u{0}PH{}\u{0}", self.counter);
        self.counter += 1;
        self.map.insert(key.clone(), value);
        self.order.push(key.clone());
        key
    }
    /// Restore placeholders in reverse insertion order so nested placeholders resolve.
    fn restore(&self, mut text: String) -> String {
        for key in self.order.iter().rev() {
            if let Some(v) = self.map.get(key) {
                text = text.replace(key, v);
            }
        }
        text
    }
}

/// Protect fenced code blocks: a fence may carry leading prose/indent on its
/// opening line; `\` and `` ` `` inside the body are escaped per the MarkdownV2
/// spec. Returns (text-with-placeholder, None) or (input, Some(failure)).
fn protect_fenced(text: &str, ph: &mut Placeholders) -> String {
    // Match: prefix line(s) + opening fence line + body + closing fence line.
    // The opening ``` must be the first triple-backtick run on its line and end
    // it; the closing fence sits on its own line (any indent, optional \r).
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        // Find an opening fence: first ``` run on the line, with the rest of the
        // line backtick-free, followed (in the scan) by a closing fence later.
        if let Some(open_start) = line.find("```") {
            let after = &line[open_start + 3..];
            if !after.contains('`') {
                // Opening line = prefix + ```lang
                let prefix = &line[..open_start];
                let lang_and_nl = after;
                // Scan forward for the closing fence (own line, any indent).
                let mut j = i + 1;
                let mut body: Vec<&str> = Vec::new();
                let mut close_idx: Option<usize> = None;
                while j < lines.len() {
                    let l = lines[j];
                    let trimmed = l.trim_start();
                    if trimmed.starts_with("```") && trimmed[3..].trim_end().is_empty() {
                        close_idx = Some(j);
                        break;
                    }
                    body.push(l);
                    j += 1;
                }
                if let Some(close_idx) = close_idx {
                    let mut body = body.join("\n");
                    body = body.replace('\\', "\\\\").replace('`', "\\`");
                    let opening = format!("{}{}{}", prefix, "```", lang_and_nl);
                    let stash = if body.is_empty() {
                        format!("{}{}{}", opening, "\n", lines[close_idx])
                    } else {
                        format!("{}{}{}{}{}", opening, "\n", body, "\n", lines[close_idx])
                    };
                    out.push(ph.add(stash));
                    i = close_idx + 1;
                    continue;
                }
            }
        }
        out.push(line.to_string());
        i += 1;
    }
    out.join("\n")
}

/// Escape bare `( ) { }` that slipped through — never inside ``` or ` spans,
/// never already-escaped, never a link's `(` / URL-closing `)`.
fn escape_bare_brackets(text: &str) -> String {
    // Split into code / non-code segments first.
    let seg_re = Regex::new(r"```[\s\S]*?```|`[^`]+`").unwrap();
    let mut parts: Vec<String> = Vec::new();
    let mut last = 0usize;
    for m in seg_re.find_iter(text) {
        parts.push(text[last..m.start()].to_string());
        parts.push(m.as_str().to_string()); // inside code — untouched
        last = m.end();
    }
    parts.push(text[last..].to_string());
    parts.into_iter().enumerate()
        .map(|(idx, seg)| if idx % 2 == 1 { seg } else { escape_bare_brackets_seg(&seg) })
        .collect()
}

/// Port of `TelegramAdapter._escape_bare_bracket` over one non-code segment.
fn escape_bare_brackets_seg(seg: &str) -> String {
    let re = Regex::new(r"[(){}]").unwrap();
    let bytes = seg.as_bytes();
    let mut out = String::with_capacity(seg.len());
    let mut last = 0usize;
    for m in re.find_iter(seg) {
        let s = m.start();
        let ch = m.as_str().chars().next().unwrap();
        let prev = seg[..s].chars().next_back();
        // Already escaped → keep as-is.
        if prev == Some('\\') {
            out.push_str(&seg[last..=s]);
            last = m.end();
            continue;
        }
        // Opens a link [text](url) → keep.
        if ch == '(' && prev == Some(']') {
            out.push_str(&seg[last..=s]);
            last = m.end();
            continue;
        }
        // Closes a link URL? Walk back matching parens depth.
        if ch == ')' {
            let before = &seg[..s];
            if before.contains("](") {
                let mut depth = 0i64;
                let mut keep = false;
                let lo = s.saturating_sub(2000);
                let mut j = s;
                while j > lo {
                    j -= 1;
                    match bytes[j] {
                        b'(' => {
                            depth -= 1;
                            if depth < 0 {
                                keep = j > 0 && bytes[j - 1] == b']';
                                break;
                            }
                        }
                        b')' => depth += 1,
                        _ => {}
                    }
                }
                if keep {
                    out.push_str(&seg[last..=s]);
                    last = m.end();
                    continue;
                }
            }
        }
        // Escape it.
        out.push_str(&seg[last..s]);
        out.push('\\');
        out.push(ch);
        last = m.end();
    }
    out.push_str(&seg[last..]);
    out
}

/// Full converter (port of `TelegramAdapter.format_message`).
pub fn format_message(content: &str) -> String {
    if content.is_empty() {
        return content.to_string();
    }
    let mut ph = Placeholders::default();

    // 0) Rewrite GFM-style pipe tables into Telegram-friendly row groups.
    let mut text = crate::tables::wrap_markdown_tables(content);

    // 1) Protect fenced code blocks.
    text = protect_fenced(&text, &mut ph);

    // 2) Protect inline code — escape `\` inside inline code per the spec.
    let inline_re = Regex::new(r"(`[^`]+`)").unwrap();
    text = inline_re
        .replace_all(&text, |m: &regex::Captures| {
            ph.add(m[0].replace('\\', "\\\\"))
        })
        .to_string();

    // 3) Convert links — escape display text; in the URL only `)` and `\` need it.
    let link_re = Regex::new(r"\[([^\]]+)\]\(([^()]*(?:\([^()]*\)[^()]*)*)\)").unwrap();
    text = link_re
        .replace_all(&text, |m: &regex::Captures| {
            let url = m[2].replace('\\', "\\\\").replace(')', "\\)");
            ph.add(format!("[{}]({})", escape_mdv2(&m[1]), url))
        })
        .to_string();

    // 4) Headers (## Title) → bold *Title*, stripping redundant ** inside.
    let header_re = Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap();
    text = header_re
        .replace_all(&text, |m: &regex::Captures| {
            let inner = Regex::new(r"\*\*(.+?)\*\*").unwrap().replace_all(m[1].trim(), "$1").to_string();
            ph.add(format!("*{}*", escape_mdv2(&inner)))
        })
        .to_string();

    // 5) Bold **text** → *text*; 6) Italic *text* → _text_; 7) ~~strike~~ → ~text~;
    // 8) ||spoiler|| kept as-is.
    let bold_re = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    text = bold_re
        .replace_all(&text, |m: &regex::Captures| ph.add(format!("*{}*", escape_mdv2(&m[1]))))
        .to_string();
    let ital_re = Regex::new(r"\*([^*\n]+)\*").unwrap();
    text = ital_re
        .replace_all(&text, |m: &regex::Captures| ph.add(format!("_{}_", escape_mdv2(&m[1]))))
        .to_string();
    let strike_re = Regex::new(r"~~(.+?)~~").unwrap();
    text = strike_re
        .replace_all(&text, |m: &regex::Captures| ph.add(format!("~{}~", escape_mdv2(&m[1]))))
        .to_string();
    let spoiler_re = Regex::new(r"\|\|(.+?)\|\|").unwrap();
    text = spoiler_re
        .replace_all(&text, |m: &regex::Captures| {
            ph.add(format!("||{}||", escape_mdv2(&m[1])))
        })
        .to_string();

    // 9) Blockquotes: protect leading > from escaping; expandable quotes
    // (**> starts, trailing || ends).
    let quote_re = Regex::new(r"(?m)^((?:\*\*)?>{1,3}) (.+)$").unwrap();
    text = quote_re
        .replace_all(&text, |m: &regex::Captures| {
            let prefix = &m[1];
            let content = &m[2];
            if prefix.starts_with("**") && content.ends_with("||") {
                ph.add(format!("{} {}||", prefix, escape_mdv2(&content[..content.len() - 2])))
            } else {
                ph.add(format!("{} {}", prefix, escape_mdv2(content)))
            }
        })
        .to_string();

    // 10) Escape remaining special characters in plain text.
    text = escape_mdv2(&text);

    // 11) Restore placeholders in reverse insertion order.
    let mut text = ph.restore(text);

    // 12) Safety net: escape bare ( ) { } that slipped through, but never
    // inside ``` or ` spans.
    text = escape_bare_brackets(&text);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_table_matches_python_set() {
        assert_eq!(escape_mdv2("a_b*c[d]e(f)g~h`i>j#k+l-m=n|o{p}q.r!s\\t"),
            "a\\_b\\*c\\[d\\]e\\(f\\)g\\~h\\`i\\>j\\#k\\+l\\-m\\=n\\|o\\{p\\}q\\.r\\!s\\\\t");
        assert_eq!(escape_mdv2("plain text 123"), "plain text 123");
        assert_eq!(escape_mdv2(""), "");
    }

    #[test]
    fn bold_italic_strike_spoiler() {
        assert_eq!(format_message("**bold**"), "*bold*");
        assert_eq!(format_message("*ital*"), "_ital_");
        assert_eq!(format_message("~~gone~~"), "~gone~");
        assert_eq!(format_message("||shh||"), "||shh||");
        assert_eq!(format_message("snake_case survives"), "snake\\_case survives");
    }

    #[test]
    fn headers_become_bold() {
        assert_eq!(format_message("## Title"), "*Title*");
        assert_eq!(format_message("# Head **line**"), "*Head line*");
    }

    #[test]
    fn links_escape_url_parens_only() {
        assert_eq!(format_message("[docs](https://x.io/a(b))"), "[docs](https://x.io/a(b\\))");
        // Display text is escaped.
        assert_eq!(format_message("[a_b](https://x.io)"), "[a\\_b](https://x.io)");
    }

    #[test]
    fn code_is_protected_from_escaping() {
        assert_eq!(format_message("use `fn main()` now"),
            "use `fn main()` now");
        assert_eq!(format_message("```\nlet x = a_b;\n```"),
            "```\nlet x = a\\_b;\n```");
        assert_eq!(format_message("```rust\nlet s = \"`\";\n```"),
            "```rust\nlet s = \"\\`\";\n```");
    }

    #[test]
    fn bare_brackets_escaped_outside_code() {
        assert_eq!(format_message("call f(x) {y}"), "call f\\(x\\) \\{y\\}");
        assert_eq!(format_message("`f(x)` stays"), "`f(x)` stays");
        assert_eq!(format_message("```\n(x)\n```"), "```\n(x)\n```");
    }

    #[test]
    fn blockquotes_protect_angle() {
        assert_eq!(format_message("> quoted"), "> quoted");
        assert_eq!(format_message("> a_b"), "> a\\_b");
        assert_eq!(format_message("**> expandable**"), "**> expandable**");
    }

    #[test]
    fn tables_become_fenced_rows() {
        let out = format_message("| a | b |\n| --- | --- |\n| 1 | 2 |");
        assert!(out.starts_with("```\n| "), "{}", out);
        assert!(out.contains("| 1    | 2  |") || out.contains("| 1 "), "{}", out);
    }

    #[test]
    fn plain_text_escapes_everything_special() {
        assert_eq!(format_message("1 + 1 = 2!"), "1 \\+ 1 \\= 2\\!");
        assert_eq!(format_message("100% sure."), "100% sure\\.");
    }

    #[test]
    fn strip_mdv2_is_the_plain_fallback() {
        assert_eq!(strip_mdv2("*bold* _ital_ \\_x\\_"), "bold ital x");
        assert_eq!(strip_mdv2("snake_case"), "snake_case");
    }
}
