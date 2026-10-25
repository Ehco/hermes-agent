//! Fence-aware message chunking — direct port of
//! `gateway/platforms/base.py::truncate_message` + `utf16_len` and
//! `gateway/platforms/helpers.py::fence_state_after`.

/// UTF-16 code units in `s` — Telegram's 4096 limit counts those, so astral
/// chars (emoji, CJK Ext B) cost **two** units although `chars().count()`
/// counts one (see the Python docstring: nearai/ironclaw#2304).
pub fn utf16_len(s: &str) -> i64 {
    s.encode_utf16().count() as i64
}

/// Largest char-boundary index ≤ `i`.
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Walk `text` line by line toggling on ``` lines; return the final `(in_code, lang)`.
pub fn fence_state_after(text: &str, mut in_code: bool, mut lang: &str) -> (bool, String) {
    for line in text.split('\n') {
        let stripped = line.trim();
        if stripped.starts_with("```") {
            let tag = stripped[3..].split_whitespace().next().unwrap_or("");
            if in_code {
                in_code = false;
                lang = "";
            } else {
                in_code = true;
                lang = tag;
            }
        }
    }
    (in_code, lang.to_string())
}

/// Largest char-boundary offset `n` with `len_fn(s[..n]) <= budget` (binary search).
fn custom_unit_to_cp(s: &str, budget: i64, len_fn: fn(&str) -> i64) -> usize {
    if len_fn(s) <= budget {
        return s.len();
    }
    let (mut lo, mut hi) = (0usize, s.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let mid = floor_char_boundary(s, mid);
        if mid == 0 {
            lo = 0;
            break;
        }
        if len_fn(&s[..mid]) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// Split a long message into chunks preserving code blocks: a split inside a
/// fence closes it at the chunk end and reopens it (same language tag) in the
/// next; multi-chunk output gets `(1/3)` indicators. `len_fn` overrides `len`
/// (`utf16_len` for Telegram, plain char count for Discord/Slack).
pub fn truncate_message(content: &str, max_length: i64, len_fn: fn(&str) -> i64) -> Vec<String> {
    if len_fn(content) <= max_length {
        return vec![content.to_string()];
    }
    const INDICATOR_RESERVE: i64 = 10; // room for " (XX/XX)"
    let fence_close = "\n```";
    let mut chunks: Vec<String> = Vec::new();
    let mut remaining = content.to_string();
    let mut carry_lang: Option<String> = None; // language tag ("" ok) when previous chunk ended mid-fence
    while !remaining.is_empty() {
        let prefix = match &carry_lang {
            Some(l) => format!("```{}\n", l),
            None => String::new(),
        };
        // Body budget after prefix/fence/indicator; floored so a tiny max_length can't stall.
        let mut headroom =
            max_length - INDICATOR_RESERVE - len_fn(&prefix) - fence_close.len() as i64;
        if headroom < 1 {
            headroom = (max_length / 2).max(1);
        }
        // Remainder fits in one final chunk; close a reopened fence if still open.
        if len_fn(&prefix) + len_fn(&remaining) <= max_length - INDICATOR_RESERVE {
            let mut final_chunk = format!("{}{}", prefix, remaining);
            if let Some(lang) = &carry_lang {
                let (in_code, _) = fence_state_after(&remaining, true, lang);
                if in_code {
                    final_chunk += fence_close;
                }
            }
            chunks.push(final_chunk);
            break;
        }
        // Natural split (newline, then space); a custom len budget maps to a codepoint offset.
        let cp_limit = custom_unit_to_cp(&remaining, headroom, len_fn);
        let region = &remaining[..cp_limit];
        let mut split_at = region.rfind('\n').unwrap_or(0);
        if split_at < cp_limit / 2 {
            split_at = region.rfind(' ').unwrap_or(0);
        }
        if split_at < 1 {
            // Floor at one codepoint: a zero cp_limit (max_length 0/1, or a surrogate
            // pair wider than the utf16 budget) would never shrink `remaining`;
            // overshooting beats a hang.
            split_at = remaining
                .char_indices()
                .nth(1)
                .map(|(i, _)| i)
                .unwrap_or(remaining.len());
        }
        // Don't split inside an inline code span: an unpaired backtick breaks MarkdownV2.
        let candidate = &remaining[..split_at];
        let backtick_count = candidate.matches('`').count() - candidate.matches("\\`").count();
        if backtick_count % 2 == 1 {
            let mut last_bt = candidate.rfind('`').unwrap_or(0);
            while last_bt > 0 {
                let before = &candidate[..last_bt];
                if before.ends_with('\\') {
                    last_bt = before.rfind('`').unwrap_or(0);
                } else {
                    break;
                }
            }
            if last_bt > 0 {
                let a = candidate[..last_bt].rfind(' ').map(|i| i + 1).unwrap_or(0);
                let b = candidate[..last_bt].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let safe_split = a.max(b);
                if safe_split > cp_limit / 4 {
                    split_at = safe_split;
                }
            }
        }
        // Snap to a char boundary so slicing never panics (astral chars in utf16 budgets).
        split_at = floor_char_boundary(&remaining, split_at.max(1));
        let chunk_body = remaining[..split_at].to_string();
        remaining = remaining[split_at..].trim_start().to_string();
        let mut full_chunk = format!("{}{}", prefix, chunk_body);
        let lang_carry = carry_lang.clone().unwrap_or_default();
        let (in_code, lang) = fence_state_after(&chunk_body, carry_lang.is_some(), &lang_carry);
        carry_lang = if in_code { Some(lang) } else { None };
        // Close the orphaned fence so the chunk stands alone.
        if in_code {
            full_chunk += fence_close;
        }
        chunks.push(full_chunk);
    }
    if chunks.len() > 1 {
        let n = chunks.len();
        chunks = chunks
            .into_iter()
            .enumerate()
            .map(|(i, c)| format!("{} ({}/{})", c, i + 1, n))
            .collect();
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_counts_astral_pairs_twice() {
        assert_eq!(utf16_len("hello"), 5);
        assert_eq!(utf16_len("\u{1F600}"), 2); // U+1F600: astral, 2 UTF-16 units
        assert_eq!(utf16_len("a\u{1F600}b"), 4);
        assert_eq!(utf16_len("日本"), 2);
    }

    #[test]
    fn short_message_is_not_split() {
        assert_eq!(truncate_message("hi there", 4096, utf16_len), vec!["hi there"]);
    }

    #[test]
    fn splits_at_newline_and_appends_indicators() {
        let text = format!("{}\n{}", "a".repeat(30), "b".repeat(30));
        let chunks = truncate_message(&text, 24, |s| s.chars().count() as i64);
        assert!(chunks.len() >= 3);
        assert!(chunks[0].ends_with(" (1/3)"));
        assert!(chunks.last().unwrap().ends_with(&format!(" ({}/3)", chunks.len())));
        for c in &chunks {
            assert!(c.chars().count() as i64 <= 24, "chunk too long: {} ({})", c, c.chars().count());
        }
        // No content lost.
        let joined = chunks.join("\n");
        assert!(joined.contains(&"a".repeat(30)));
        assert!(joined.contains(&"b".repeat(30)));
    }

    #[test]
    fn fence_split_closes_and_reopens_with_language() {
        let body = "x".repeat(40);
        let text = format!("```rust\n{}\nmore\n```\n", body);
        let chunks = truncate_message(&text, 30, |s| s.chars().count() as i64);
        assert!(chunks.len() >= 2);
        // Every chunk must stand alone: balanced fences.
        for c in &chunks {
            let fences = c.matches("```").count();
            assert_eq!(fences % 2, 0, "unbalanced fence in {:?}: {}", c, c);
        }
        // The continuation reopens with the carried language tag.
        assert!(chunks.iter().any(|c| c.starts_with("```rust\n")));
        assert!(chunks.iter().any(|c| c.contains("more")));
    }

    #[test]
    fn inline_code_span_is_not_broken() {
        let text = format!("`code {}` tail", "y".repeat(20));
        let chunks = truncate_message(&text, 20, |s| s.chars().count() as i64);
        assert!(chunks.len() >= 2);
        // No chunk may end inside a code span (odd unescaped backtick count).
        for c in &chunks {
            let bt = c.matches('`').count() - c.matches("\\`").count();
            assert_eq!(bt % 2, 0, "chunk splits an inline code span: {:?}", c);
        }
    }

    #[test]
    fn fence_state_walks_lines() {
        let (in_code, lang) = fence_state_after("text\n```python\nprint(1)\n```\ntext", false, "");
        assert!(!in_code);
        assert_eq!(lang, "");
        let (in_code, lang) = fence_state_after("```python\nprint(1)", false, "");
        assert!(in_code);
        assert_eq!(lang, "python");
        let (in_code, _) = fence_state_after("```\ncode\n```", true, "rust");
        assert!(!in_code);
    }

    #[test]
    fn astral_chars_never_panic() {
        let text = "\u{1F600}".repeat(50);
        let chunks = truncate_message(&text, 10, utf16_len);
        assert!(chunks.len() >= 9);
        for c in chunks {
            assert!(utf16_len(&c) <= 10, "chunk over utf16 budget: {}", utf16_len(&c));
        }
    }
}
