//! GFM pipe-table rewrites shared by the platform formatters — ports of
//! `plugins/platforms/slack/adapter.py::_wrap_markdown_tables` (mrkdwn shows
//! pipe tables literally, so they are wrapped in monospace fences and cells
//! aligned to per-column display width, CJK-wide aware) and
//! `gateway/platforms/helpers.py::convert_table_to_bullets` (Discord renders
//! markdown but not pipe tables → bold-heading + bullet groups).

use regex::Regex;

/// `^\s*\|?\s*:?-+:?\s*(?:\|\s*:?-+:?\s*){1,}\|?\s*$` — the GFM divider row.
fn table_separator_re() -> Regex {
    Regex::new(r"^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*){1,}\|?\s*$").unwrap()
}

fn is_table_row(line: &str) -> bool {
    let stripped = line.trim();
    !stripped.is_empty() && stripped.contains('|')
}

/// Split a `` | a | b | c | `` row into trimmed cells (outer pipes optional).
fn split_table_row(line: &str) -> Vec<String> {
    let mut s = line.trim().to_string();
    if s.starts_with('|') {
        s.remove(0);
    }
    if s.ends_with('|') {
        s.truncate(s.len() - 1);
    }
    s.split('|').map(|c| c.trim().to_string()).collect()
}

/// Monospace display width: East-Asian Wide / Full-width chars count as 2
/// (`unicodedata.east_asian_width(c) in "WF"`).
fn disp_width(s: &str) -> usize {
    s.chars()
        .map(|c| if east_asian_wide(c) { 2 } else { 1 })
        .sum()
}

/// Approximation of Unicode EastAsianWidth W/F for the common ranges.
fn east_asian_wide(c: char) -> bool {
    let u = c as u32;
    matches!(u,
        0x1100..=0x115F       // Hangul Jamo
        | 0x2E80..=0x303E     // CJK radicals, Kangxi, CJK symbols & punctuation
        | 0x3041..=0x33FF     // Hiragana, Katakana, Bopomofo, CJK compat
        | 0x3400..=0x4DBF     // CJK Ext A
        | 0x4E00..=0x9FFF     // CJK Unified
        | 0xA000..=0xA4CF     // Yi
        | 0xAC00..=0xD7A3     // Hangul syllables
        | 0xF900..=0xFAFF     // CJK Compatibility Ideographs
        | 0xFE10..=0xFE19     // Vertical forms
        | 0xFE30..=0xFE6F     // CJK compat forms, small forms
        | 0xFF00..=0xFF60     // Full-width forms
        | 0xFFE0..=0xFFE6     // Half/full-width signs
        | 0x1F300..=0x1F64F   // emoji (W)
        | 0x1F900..=0x1F9FF   // supplemental symbols (W)
        | 0x20000..=0x3FFFD   // CJK Ext B+ (W)
    )
}

fn pad(cell: &str, width: usize) -> String {
    let w = disp_width(cell);
    let mut out = cell.to_string();
    if width > w {
        out.push_str(&" ".repeat(width - w));
    }
    out
}

/// Re-emit a markdown table padded to per-column display width; rows[1] is the
/// GFM separator (regenerated); short rows are padded to a uniform column count.
fn align_table(rows: &[String]) -> Vec<String> {
    if rows.len() < 2 {
        return rows.to_vec();
    }
    let parsed: Vec<Vec<String>> = rows.iter().map(|r| split_table_row(r)).collect();
    let n_cols = parsed.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut padded: Vec<Vec<String>> = parsed
        .into_iter()
        .map(|mut r| {
            r.resize(n_cols, String::new());
            r
        })
        .collect();
    if padded.len() >= 2 {
        padded[1] = vec!["---".to_string(); n_cols]; // placeholder; regenerated below
    }
    let widths: Vec<usize> = (0..n_cols)
        .map(|c| {
            padded
                .iter()
                .map(|r| disp_width(r.get(c).map(String::as_str).unwrap_or("")))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = Vec::new();
    for (idx, row) in padded.iter().enumerate() {
        let cells: Vec<String> = (0..n_cols)
            .map(|c| {
                let w = widths[c];
                if idx == 1 {
                    "-".repeat(w)
                } else {
                    pad(row.get(c).map(String::as_str).unwrap_or(""), w)
                }
            })
            .collect();
        out.push(format!("| {} |", cells.join(" | ")));
    }
    out
}

/// Wrap GFM pipe tables in ``` fences and align columns; tables already in
/// fences are left alone.
pub fn wrap_markdown_tables(text: &str) -> String {
    if !text.contains('|') || !text.contains('-') {
        return text.to_string();
    }
    let sep_re = table_separator_re();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence
            && line.contains('|')
            && i + 1 < lines.len()
            && sep_re.is_match(lines[i + 1])
        {
            let mut block: Vec<String> = vec![line.to_string(), lines[i + 1].to_string()];
            let mut j = i + 2;
            while j < lines.len() && is_table_row(lines[j]) {
                block.push(lines[j].to_string());
                j += 1;
            }
            out.push("```".to_string());
            out.extend(align_table(&block));
            out.push("```".to_string());
            i = j;
            continue;
        }
        out.push(line.to_string());
        i += 1;
    }
    out.join("\n")
}

/// Render a GFM table as bold-heading + bullet groups (same alignment logic as
/// Telegram's renderer: without a row-label column the full row is data and the
/// heading bullet is skipped).
fn render_table_block(table_block: &[String]) -> String {
    let headers = if table_block.len() >= 3 {
        split_table_row(&table_block[0])
    } else {
        Vec::new()
    };
    if headers.len() < 2 {
        return table_block.join("\n");
    }
    let has_row_label_col = if table_block.len() > 2 {
        split_table_row(&table_block[2]).len() == headers.len() + 1
    } else {
        false
    };
    let mut rendered_groups: Vec<String> = Vec::new();
    for (index, row) in table_block.iter().skip(2).enumerate() {
        let index = index + 1;
        let cells = split_table_row(row);
        let (heading, data_cells): (String, Vec<String>) = if has_row_label_col {
            let h = if cells
                .first()
                .map(String::as_str)
                .map(str::is_empty)
                .unwrap_or(true)
            {
                format!("Row {}", index)
            } else {
                cells[0].clone()
            };
            (h, cells.iter().skip(1).cloned().collect())
        } else {
            let h = cells
                .iter()
                .find(|c| !c.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Row {}", index));
            (h, cells.clone())
        };
        let mut data = data_cells.clone();
        data.resize(headers.len(), String::new());
        let bullets: Vec<String> = headers
            .iter()
            .zip(data.iter())
            .filter(|(_, v)| has_row_label_col || v.as_str() != heading)
            .map(|(h, v)| format!("\u{2022} {}: {}", h, v))
            .collect();
        let mut group = vec![format!("**{}**", heading)];
        group.extend(bullets);
        rendered_groups.push(group.join("\n"));
    }
    rendered_groups.join("\n\n")
}

/// Rewrite GFM pipe tables into bold-heading + bullet groups; fenced code is
/// left alone. (Port of `helpers.py::convert_table_to_bullets`.)
pub fn convert_table_to_bullets(text: &str) -> String {
    if !text.contains('|') || !text.contains('-') {
        return text.to_string();
    }
    let sep_re = table_separator_re();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let is_fence_line = line.trim_start().starts_with("```");
        in_fence ^= is_fence_line;
        if !in_fence
            && !is_fence_line
            && line.contains('|')
            && i + 1 < lines.len()
            && sep_re.is_match(lines[i + 1])
        {
            let mut j = i + 2;
            while j < lines.len() && is_table_row(lines[j]) {
                j += 1;
            }
            let block: Vec<String> = lines[i..j].iter().map(|s| s.to_string()).collect();
            out.push(render_table_block(&block));
            i = j;
        } else {
            out.push(line.to_string());
            i += 1;
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_table_row_trims_and_strips_outer_pipes() {
        assert_eq!(split_table_row("| a | b |"), vec!["a", "b"]);
        assert_eq!(split_table_row("a | b"), vec!["a", "b"]);
        assert_eq!(split_table_row("| x |"), vec!["x"]);
    }

    #[test]
    fn slack_variant_fences_and_aligns() {
        let table = "| A | BB |\n| --- | --- |\n| x | y |\n| longer | z |";
        let out = wrap_markdown_tables(table);
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines[0], "```");
        assert_eq!(lines[1], "| A      | BB  |");
        assert_eq!(lines[2], "| ------ | --- |");
        assert_eq!(lines[3], "| x      | y   |");
        assert_eq!(lines[4], "| longer | z   |");
        assert_eq!(lines[5], "```");
        // CJK-wide cells count double.
        let out2 = wrap_markdown_tables("| A | B |\n| --- | --- |\n| \u{65E5}\u{672C} | x |");
        assert!(out2.contains("| \u{65E5}\u{672C} | x   |"), "{}", out2);
    }

    #[test]
    fn slack_variant_leaves_fenced_tables_alone() {
        let text = "```rust\n| a | b |\n```\npre | post\n| --- | --- |\n| 1 | 2 |";
        let out = wrap_markdown_tables(text);
        // The rust-fenced table is untouched; the second table is wrapped.
        assert!(out.contains("```rust\n| a | b |"));
        assert!(out.starts_with("```rust\n| a | b |\n```"));
    }

    #[test]
    fn plain_text_without_tables_passes_through() {
        assert_eq!(wrap_markdown_tables("no pipes here"), "no pipes here");
        assert_eq!(convert_table_to_bullets("just a | pipe"), "just a | pipe");
    }

    #[test]
    fn discord_variant_renders_bullet_groups_with_row_labels() {
        let table = "| name | hp |\n| --- | --- |\n| goblin | 7 |";
        let out = convert_table_to_bullets(table);
        assert_eq!(out, "**goblin**\n\u{2022} hp: 7");
    }

    #[test]
    fn discord_variant_without_label_column_keeps_all_cells() {
        let table = "| one | two |\n| --- | --- |\n| a | b |";
        let out = convert_table_to_bullets(table);
        assert!(out.starts_with("**a**\n"));
        assert!(out.contains("\u{2022} one: a") || out.contains("\u{2022} two: b"));
    }

    #[test]
    fn discord_variant_preserves_surrounding_text() {
        let text = "before\n| a | b |\n| --- | --- |\n| x | y |\nafter";
        let out = convert_table_to_bullets(text);
        assert!(out.starts_with("before\n"));
        assert!(out.ends_with("after"));
        assert!(out.contains("**x**"));
    }
}
