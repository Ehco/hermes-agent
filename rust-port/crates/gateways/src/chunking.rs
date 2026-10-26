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
pub fn fence_state_after<'a>(text: &'a str, mut in_code: bool, mut lang: &'a str) -> (bool, String) {
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

/// Largest char-boundary offset `n` with `len_fn(s[..n]) <= budget` (binary
/// search over the char boundaries — flooring `mid` onto an already-tried
/// boundary would otherwise spin forever on multi-byte scripts/emoji).
fn custom_unit_to_cp(s: &str, budget: i64, len_fn: fn(&str) -> i64) -> usize {
    if len_fn(s) <= budget {
        return s.len();
    }
    // Walk the boundary list by index: every probe is a real boundary, so the
    // search window always shrinks.
    let bounds: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
    let (mut lo, mut hi) = (0usize, bounds.len()); // answer is some bounds[pos]
    let mut best = 0usize;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let offset = bounds[mid];
        if len_fn(&s[..offset]) <= budget {
            best = offset;
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    best
}

/// Unescaped-backtick parity of `s` (even = balanced inline spans / fences).
fn backtick_parity_odd(s: &str) -> bool {
    let unescaped = s.matches('`').count() - s.matches("\\`").count();
    unescaped % 2 == 1
}

/// Append a closing backtick when a chunk would end inside an inline code
/// span (an unpaired backtick breaks the markdown rendering downstream). The
/// fence closer (`\n```) is added before this runs, so fenced chunks are
/// already even.
fn balance_inline_spans(chunk: String) -> String {
    if backtick_parity_odd(&chunk) {
        format!("{}`", chunk)
    } else {
        chunk
    }
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
            let (in_code, _) = match &carry_lang {
                Some(lang) => fence_state_after(&remaining, true, lang),
                None => fence_state_after(&remaining, false, ""),
            };
            if in_code {
                // Close a reopened fence (carry) or a trailing lone ``` so the
                // chunk stands alone (balanced fences).
                final_chunk += fence_close;
            }
            chunks.push(balance_inline_spans(final_chunk));
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
            // No separator in the region: split at the full budget (Python:
            // ``split_at = max(1, _cp_limit)`` — a zero cp_limit (max_length
            // 0/1, or a surrogate pair wider than the utf16 budget) floors at
            // one codepoint so ``remaining`` always shrinks; overshooting
            // beats a hang).
            split_at = cp_limit.max(1);
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
        let full_chunk = balance_inline_spans(full_chunk);
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
        // Multi-chunk with sequential (N/M) indicators; every chunk within the
        // cap; no content lost. (The Python algorithm splits at the body
        // budget when no separator sits inside the split region, so the exact
        // count depends on the budget — pin the invariants, not the count.)
        assert!(chunks.len() >= 2);
        let n = chunks.len();
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.ends_with(&format!(" ({}/{})", i + 1, n)),
                "missing indicator on chunk {}: {}",
                i,
                c
            );
            assert!(
                c.chars().count() as i64 <= 24,
                "chunk too long: {} ({})",
                c,
                c.chars().count()
            );
        }
        // No content lost: every a and every b is delivered (chunk boundaries
        // consume the separator whitespace, so counts — not one joined string —
        // are the lossless check).
        let body_chars: Vec<char> = chunks
            .iter()
            .map(|c| c.trim_end_matches(|p: char| p == '(' || p == ')' || p.is_ascii_digit() || p == '/' || p == ' '))
            .flat_map(|s| s.chars())
            .collect();
        assert_eq!(body_chars.iter().filter(|&&c| c == 'a').count(), 30);
        assert_eq!(body_chars.iter().filter(|&&c| c == 'b').count(), 30);
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
        // Python semantics: every line starting with ``` toggles — the final
        // bare ``` after the closed fence OPENS a new block (which is exactly
        // why truncate_message re-closes a carried fence).
        let (in_code, _) = fence_state_after("```\ncode\n```", true, "rust");
        assert!(in_code);
    }

    #[test]
    fn astral_chars_never_panic() {
        let text = "\u{1F600}".repeat(50);
        let chunks = truncate_message(&text, 10, utf16_len);
        assert!(chunks.len() >= 9);
        for c in chunks {
            // The static indicator reserve bounds the overhang: body ≤ max,
            // (N/M) suffix ≤ INDICATOR_RESERVE.
            assert!(
                utf16_len(&c) <= 10 + 10,
                "chunk over utf16 budget: {}",
                utf16_len(&c)
            );
        }
    }
}
