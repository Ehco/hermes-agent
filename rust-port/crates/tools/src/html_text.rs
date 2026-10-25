//! HTML → readable text (markdown-flavored) converter for `web_extract`.
//!
//! The Python agent's extract backends (Firecrawl/Tavily/...) return cleaned
//! markdown; this module is the Rust side's equivalent when fetching pages
//! directly with reqwest: strip scripts/styles/comments and non-content
//! elements, keep links and images as markdown refs, decode entities, and
//! collapse the whitespace runs the tag soup leaves behind.

use regex::Regex;

/// Named entities handled beyond the numeric forms (`&nbsp;` maps to a space).
const NAMED_ENTITIES: &[(&str, &str)] = &[
    ("&amp;", "&"),
    ("&lt;", "<"),
    ("&gt;", ">"),
    ("&quot;", "\""),
    ("&apos;", "'"),
    ("&nbsp;", " "),
    ("&copy;", "©"),
    ("&reg;", "®"),
    ("&trade;", "™"),
    ("&mdash;", "—"),
    ("&ndash;", "–"),
    ("&hellip;", "…"),
    ("&lsquo;", "‘"),
    ("&rsquo;", "’"),
    ("&ldquo;", "“"),
    ("&rdquo;", "”"),
];

/// Decode HTML entities: numeric (`&#62;`, `&#x3c;`) then a fixed named table.
pub fn decode_entities(text: &str) -> String {
    let mut out = text.to_string();
    let numeric = Regex::new("&#(x?)([0-9a-fA-F]+);").unwrap();
    out = numeric
        .replace_all(&out, |caps: &regex::Captures| {
            let hex = caps.get(1).map(|m| m.as_str() == "x").unwrap_or(false);
            let digits = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let code = if hex {
                u32::from_str_radix(digits, 16).ok()
            } else {
                digits.parse::<u32>().ok()
            };
            match code.and_then(char::from_u32) {
                Some(c) => c.to_string(),
                None => caps.get(0).unwrap().as_str().to_string(),
            }
        })
        .into_owned();
    for (ent, repl) in NAMED_ENTITIES {
        out = out.replace(ent, repl);
    }
    out
}

/// Extract the value of an attribute from a raw attribute blob.
fn attr(attrs: &str, name: &str) -> Option<String> {
    let re = Regex::new(&format!(
        "(?i){}[ \t\\r\\n]*=[ \t\\r\\n]*[\"']([^\"']*)[\"']",
        regex::escape(name)
    ))
    .unwrap();
    re.captures(attrs)
        .map(|c| decode_entities(c.get(1).map(|m| m.as_str()).unwrap_or("")))
}

/// ASCII `%XX`/`+` decoding used by the entity tests (kept local to the
/// module's test surface).
#[cfg(test)]
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The page `<title>`, if one is present.
pub fn extract_title(html: &str) -> Option<String> {
    let re = Regex::new("(?is)<title[^>]*>(.*?)</title>").unwrap();
    re.captures(html).map(|c| {
        decode_entities(c.get(1).map(|m| m.as_str()).unwrap_or("").trim())
    })
}

/// Convert an HTML document to readable text.
///
/// Links become `[text](href)` (hrefless anchors keep only their text),
/// images become `![alt](src)`, list items are prefixed `- `, headings get
/// `#` gutters, and block-level closers become newlines.
pub fn html_to_text(html: &str) -> String {
    // 1. Drop comments and non-content elements. An unclosed <script>/<style>
    //    (truncated page) must not leak its body into the text, so a missing
    //    closer eats to end-of-input.
    let mut out = strip_pair(html, "script");
    out = strip_pair(&out, "style");
    out = strip_pair(&out, "noscript");
    out = strip_pair(&out, "svg");
    out = strip_pair(&out, "head");
    out = Regex::new("(?s)<!--.*?-->")
        .unwrap()
        .replace_all(&out, "")
        .into_owned();

    // 2. Anchors → [text](href); plain anchors first so `href="#"` keeps text.
    let plain_anchor = Regex::new("(?is)<a\\s[^>]*href=[\"']#?[\"'][^>]*>(.*?)</a>").unwrap();
    out = plain_anchor.replace_all(&out, "$1").into_owned();
    let anchor = Regex::new("(?is)<a\\s[^>]*href=[\"']([^\"' ]+)[\"'][^>]*>(.*?)</a>").unwrap();
    out = anchor
        .replace_all(&out, |caps: &regex::Captures| {
            let href = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let text = caps.get(2).map(|m| m.as_str()).unwrap_or("").trim();
            if href.starts_with("javascript:") || text.is_empty() {
                text.to_string()
            } else {
                format!("[{text}]({href})")
            }
        })
        .into_owned();

    // 3. Images → ![alt](src).
    let img = Regex::new("(?i)<img\\b([^>]*)/?>").unwrap();
    out = img
        .replace_all(&out, |caps: &regex::Captures| {
            let attrs = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let src = attr(attrs, "src").unwrap_or_default();
            let alt = attr(attrs, "alt").unwrap_or_default();
            if src.is_empty() {
                String::new()
            } else if alt.is_empty() {
                format!("!({src})")
            } else {
                format!("![{alt}]({src})")
            }
        })
        .into_owned();

    // 4. Headings → `#` gutters, list items, and block-level newlines.
    // (regex has no backreferences, so open/close levels match independently;
    // a stray mismatched closer is harmless — it just becomes a newline.)
    let heading = Regex::new("(?is)<h([1-6])\\b[^>]*>(.*?)</h[1-6]>").unwrap();
    out = heading
        .replace_all(&out, |caps: &regex::Captures| {
            let level = caps.get(1).map(|m| m.as_str()).unwrap_or("1");
            let text = caps.get(2).map(|m| m.as_str()).unwrap_or("").trim();
            let hashes = "#".repeat(level.parse().unwrap_or(1).clamp(1, 6));
            format!("\n\n{hashes} {text}\n\n")
        })
        .into_owned();
    let li = Regex::new("(?is)<li\\b[^>]*>").unwrap();
    out = li.replace_all(&out, "\n- ").into_owned();
    let br = Regex::new("(?i)<br\\s*/?>").unwrap();
    out = br.replace_all(&out, "\n").into_owned();
    let blocks =
        Regex::new("(?i)</?(p|div|section|article|header|footer|main|ul|ol|table|tr|blockquote|pre|dl|dt|dd|figure|figcaption|form|nav|h[1-6]|td|th)\\b[^>]*>")
            .unwrap();
    out = blocks.replace_all(&out, "\n").into_owned();

    // 5. Strip every remaining tag, then decode entities in the text proper.
    let tag = Regex::new("<[a-zA-Z!/][^>]*>").unwrap();
    out = tag.replace_all(&out, "").into_owned();
    let mut out = decode_entities(&out);

    // 6. Collapse whitespace: trailing spaces per line, runs of blank lines,
    //    and leading/trailing blanks.
    let mut collapsed = String::with_capacity(out.len());
    let mut blanks = 0usize;
    for line in out.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blanks += 1;
            if blanks > 2 {
                continue;
            }
        } else {
            blanks = 0;
        }
        collapsed.push_str(line);
        collapsed.push('\n');
    }
    out = collapsed;
    out.trim().to_string()
}

/// Remove `<tag ...>...</tag>` including the body; unclosed eats to the end.
fn strip_pair(html: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let lower = html.to_lowercase();
    // The open tag must not be a longer tag's prefix (e.g. <span vs <s).
    let Some(open_start) = lower.find(&open).filter(|_| true).and_then(|pos| {
        let next = html[pos + open.len()..].chars().next()?;
        (!next.is_ascii_alphanumeric()).then_some(pos)
    }) else {
        return html.to_string();
    };
    // Find the matching close (case-insensitive), skipping the open tag body.
    let after_open = &html[open_start..];
    let close = format!("</{tag}");
    let close_pos = after_open
        .to_lowercase()
        .find(&close.to_lowercase())
        .map(|i| open_start + i);
    let (head, tail) = match close_pos {
        Some(cp) => {
            // Advance past the closing tag's '>'.
            let rest = &html[cp..];
            let end = rest.find('>').map(|i| cp + i + 1).unwrap_or(rest.len() + cp);
            (&html[..open_start], &html[end..])
        }
        None => (&html[..open_start], ""),
    };
    format!("{head}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_scripts_and_styles() {
        let html = "<html><head><title>T</title><style>b{}</style></head>\
<body><script>var x = 1 < 2;</script><p>hello</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("hello"));
        assert!(!text.contains("var x"), "{text}");
        assert!(!text.contains("b{}"));
    }

    #[test]
    fn unclosed_script_is_swallowed() {
        let text = html_to_text("<p>keep</p><script>if (a<b) { dump");
        assert_eq!(text, "keep");
    }

    #[test]
    fn links_and_images_become_markdown() {
        let html = r##"<a href="https://e.com">Site</a> <a href="#">plain</a> \
<img src="https://e.com/i.png" alt="pic"> <img src="https://e.com/j.png">"##;
        let text = html_to_text(html);
        assert!(text.contains("[Site](https://e.com)"), "{text}");
        assert!(text.contains("plain"));
        assert!(text.contains("![pic](https://e.com/i.png)"));
        assert!(text.contains("!(https://e.com/j.png)"));
    }

    #[test]
    fn javascript_links_keep_text_only() {
        let text = html_to_text(r#"<a href="javascript:void(0)">go</a>"#);
        assert_eq!(text, "go");
    }

    #[test]
    fn headings_lists_and_blocks() {
        let html = "<h1>Big</h1><ul><li>one</li><li>two</li></ul><p>para</p>";
        let text = html_to_text(html);
        assert!(text.contains("# Big"), "{text}");
        assert!(text.contains("- one"));
        assert!(text.contains("- two"));
        assert!(text.contains("para"));
    }

    #[test]
    fn entities_decode() {
        assert_eq!(decode_entities("a &amp; b &#x27; &#62; &nbsp;end"), "a & b ' >  end");
        assert_eq!(decode_entities("&#65;"), "A");
    }

    #[test]
    fn whitespace_collapses() {
        let text = html_to_text("<div>a</div>\n\n\n\n\n<div>b</div>");
        assert_eq!(text, "a\n\n\nb");
    }

    #[test]
    fn title_extracts() {
        assert_eq!(
            extract_title("<html><head><title> Hello &amp; world </title>"),
            Some("Hello & world".into())
        );
        assert_eq!(extract_title("<body>no title</body>"), None);
    }

    #[test]
    fn uddg_style_percent_sequences_survive_text_conversion() {
        // The converter must not eat % or decode hrefs into the body text.
        let text = html_to_text(r#"<p>50% off &amp; more</p>"#);
        assert_eq!(text, "50% off & more");
        assert_eq!(percent_decode("a%20b+c"), "a b c");
    }
}
