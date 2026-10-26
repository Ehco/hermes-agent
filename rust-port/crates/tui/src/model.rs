//! Transcript view-model — the Rust port of the Python TUI's conversation
//! rendering (`cli_stream_mixin.py` / `cli_render.py`): user turns, assistant
//! turns with live-streaming deltas, tool cards rendered in place, and system
//! notices.
//!
//! Everything here is plain data + pure rendering to `ratatui::text::Line`s,
//! so it is unit-testable without a TTY; `ui.rs` stays a thin renderer over
//! these types.

use hermes_core::model::{Content, Message, Role};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Fallback wrap width for tests / headless rendering.
pub const DEFAULT_WIDTH: usize = 80;

/// Lifecycle of a tool card (Python tool-card states).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// Call issued, result not in yet (⏺ spinner state).
    Running,
    /// Result arrived.
    Done,
    /// Result was an error (kept distinct for styling).
    Failed,
}

/// One conversation card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    User {
        text: String,
    },
    /// Assistant turn; `streaming` is true while deltas are still arriving
    /// (the live card the Python TUI appends to in place).
    Assistant {
        text: String,
        streaming: bool,
    },
    Tool {
        name: String,
        arguments: String,
        output: String,
        status: ToolStatus,
    },
    /// REPL notices: /help output, "agent is running", "(new session)"…
    Notice {
        text: String,
    },
}

impl Block {
    /// The plain text body of the card (used by tests and wrapping).
    pub fn body(&self) -> &str {
        match self {
            Block::User { text }
            | Block::Assistant { text, .. }
            | Block::Notice { text } => text,
            Block::Tool { output, .. } => output,
        }
    }
}

/// Bottom-anchored transcript with a scrollback offset.
#[derive(Debug, Default, Clone)]
pub struct Transcript {
    blocks: Vec<Block>,
    /// How many rendered lines the viewport is shifted up from the bottom
    /// (0 = follow the live tail, Python autoscroll).
    scroll: usize,
}

impl Transcript {
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Scroll by delta lines (negative = toward the tail); clamped so the
    /// viewport never leaves the transcript.
    pub fn scroll_by(&mut self, delta: isize, height: usize) {
        let total = self.rendered_len(DEFAULT_WIDTH);
        let max = total.saturating_sub(height);
        let next = (self.scroll as isize + delta).clamp(0, max as isize);
        self.scroll = next as usize;
    }

    /// Return to the live tail (any key activity that isn't scrolling does
    /// this in the Python TUI).
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
    }

    pub fn push_user(&mut self, text: &str) {
        self.blocks.push(Block::User { text: text.into() });
        self.scroll_to_bottom();
    }

    pub fn push_notice(&mut self, text: &str) {
        // Notices arrive with the REPL's trailing newline; drop it.
        let text = text.trim_end_matches('\n');
        self.blocks.push(Block::Notice { text: text.into() });
    }

    /// Append a streaming delta to the live assistant card, creating it on
    /// the first delta (Python: the streaming card is appended to in place).
    pub fn apply_stream_delta(&mut self, delta: &str) {
        match self.blocks.last_mut() {
            Some(Block::Assistant { text, streaming: true }) => text.push_str(delta),
            _ => self.blocks.push(Block::Assistant {
                text: delta.into(),
                streaming: true,
            }),
        }
        self.scroll_to_bottom();
    }

    /// Close the streaming card (turn done or interrupted).
    pub fn finish_streaming(&mut self) {
        if let Some(Block::Assistant { streaming, .. }) = self.blocks.last_mut() {
            *streaming = false;
        }
    }

    /// A tool call starts — a fresh Running card (rendered in place: the
    /// result later updates this same card).
    pub fn start_tool(&mut self, name: &str, arguments: &str) {
        self.blocks.push(Block::Tool {
            name: name.into(),
            arguments: arguments.into(),
            output: String::new(),
            status: ToolStatus::Running,
        });
        self.scroll_to_bottom();
    }

    /// The running card's result arrived. `ok=false` styles it as failed.
    pub fn finish_tool(&mut self, output: &str, ok: bool) {
        for block in self.blocks.iter_mut().rev() {
            if let Block::Tool { status, output: out, .. } = block {
                if *status == ToolStatus::Running {
                    *status = if ok { ToolStatus::Done } else { ToolStatus::Failed };
                    out.clear();
                    out.push_str(output);
                    return;
                }
            }
        }
    }

    /// Seed the transcript from a resumed session's messages.
    pub fn seed_from_history(&mut self, history: &[Message]) {
        for msg in history {
            match msg.role {
                Role::User => {
                    for c in &msg.content {
                        if let Content::Text { text } = c {
                            self.blocks.push(Block::User { text: text.clone() });
                        }
                    }
                }
                Role::Assistant => {
                    let mut text = String::new();
                    for c in &msg.content {
                        match c {
                            Content::Text { text: t } => {
                                text.push_str(t);
                            }
                            Content::ToolUse { call } => {
                                self.blocks.push(Block::Tool {
                                    name: call.name.clone(),
                                    arguments: call.arguments.clone(),
                                    output: String::new(),
                                    status: ToolStatus::Done,
                                });
                            }
                            Content::ToolResult { .. } => {}
                        }
                    }
                    if !text.is_empty() {
                        self.blocks.push(Block::Assistant { text, streaming: false });
                    }
                }
                Role::Tool => {
                    for c in &msg.content {
                        if let Content::ToolResult { output, .. } = c {
                            // Resumed results have no matching live card.
                            self.blocks.push(Block::Tool {
                                name: "tool".into(),
                                arguments: String::new(),
                                output: output.clone(),
                                status: ToolStatus::Done,
                            });
                        }
                    }
                }
                Role::System => {}
            }
        }
    }

    /// Total rendered line count at `width` (what the scrollbar math uses).
    pub fn rendered_len(&self, width: usize) -> usize {
        self.to_lines(width).len()
    }

    /// Visible window over `total` rendered lines for a `height`-tall
    /// viewport with the current scroll offset: `(start, end)` line indexes.
    pub fn visible_range(&self, total: usize, height: usize) -> (usize, usize) {
        let max = total.saturating_sub(height);
        let off = self.scroll.min(max);
        let end = total - off;
        let start = end.saturating_sub(height);
        (start, end)
    }

    /// Render to styled ratatui lines, wrapping every card to `width`.
    /// Labels and styles follow the Python TUI: bold `❯` user turns, plain
    /// assistant text, `⏺` tool cards with dimmed output, dim notices.
    pub fn to_lines(&self, width: usize) -> Vec<Line<'static>> {
        let width = width.max(1);
        let mut lines: Vec<Line> = Vec::new();
        for block in &self.blocks {
            match block {
                Block::User { text } => {
                    for (i, l) in wrap(text, width).into_iter().enumerate() {
                        let prefix = if i == 0 { "❯ ".to_string() } else { "  ".to_string() };
                        lines.push(Line::from(Span::styled(
                            format!("{prefix}{l}"),
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )));
                    }
                }
                Block::Assistant { text, streaming } => {
                    for l in wrap(text, width) {
                        let mut spans = vec![Span::raw(l)];
                        if *streaming {
                            spans.push(Span::styled(" ▍", Style::default().fg(Color::Yellow)));
                        }
                        lines.push(Line::from(spans));
                    }
                }
                Block::Tool { name, arguments, output, status } => {
                    let (marker, color) = match status {
                        ToolStatus::Running => ("⏺", Color::Yellow),
                        ToolStatus::Done => ("⏺", Color::Green),
                        ToolStatus::Failed => ("⏺", Color::Red),
                    };
                    let header = if arguments.is_empty() {
                        format!("{marker} {name}")
                    } else {
                        format!("{marker} {name} {arguments}")
                    };
                    lines.push(Line::from(Span::styled(
                        header,
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    )));
                    for l in wrap(output, width.saturating_sub(4).max(1)) {
                        lines.push(Line::from(Span::styled(
                            format!("  └ {l}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
                Block::Notice { text } => {
                    for l in wrap(text, width) {
                        lines.push(Line::from(Span::styled(
                            l,
                            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                        )));
                    }
                }
            }
            // Blank separator between cards (Python separates cards too).
            lines.push(Line::from(""));
        }
        // Drop the trailing separator.
        lines.pop();
        lines
    }
}

/// Greedy word wrap to at most `width` columns per line (Python wraps via
/// prompt_toolkit; the TUI needs the same visual result). Words longer than
/// `width` are hard-split.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let mut word = word;
            loop {
                let room = if line.is_empty() {
                    width
                } else {
                    width.saturating_sub(line.chars().count() + 1)
                };
                let wc = word.chars().count();
                if wc <= room {
                    if !line.is_empty() {
                        line.push(' ');
                    }
                    line.push_str(word);
                    break;
                }
                // Doesn't fit: flush the current line and retry on a fresh
                // one; a lone over-long word is hard-split at the width.
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                    continue;
                }
                let take = word
                    .char_indices()
                    .take(width)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(word.len());
                out.push(word[..take].to_string());
                word = &word[take..];
            }
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_deltas_append_to_one_live_card() {
        let mut t = Transcript::default();
        t.push_user("hi");
        t.apply_stream_delta("hel");
        t.apply_stream_delta("lo there");
        // One assistant card holding the joined deltas.
        let mut found = 0;
        for b in t.blocks() {
            if let Block::Assistant { text, streaming: true } = b {
                assert_eq!(text, "hello there");
                found += 1;
            }
        }
        assert_eq!(found, 1);
        t.finish_streaming();
        assert!(matches!(
            t.blocks().last(),
            Some(Block::Assistant { streaming: false, .. })
        ));
    }

    #[test]
    fn a_new_card_starts_after_a_user_push_mid_stream() {
        let mut t = Transcript::default();
        t.apply_stream_delta("answer");
        t.push_user("next question");
        t.apply_stream_delta("part");
        // The delta after the user push starts a NEW card, not the old one.
        assert_eq!(t.blocks()[0].body(), "answer");
        assert!(matches!(t.blocks()[1], Block::User { .. }));
        assert_eq!(t.blocks()[2].body(), "part");
    }

    #[test]
    fn tool_cards_update_in_place() {
        let mut t = Transcript::default();
        t.start_tool("terminal", r#"{"command":"ls"}"#);
        assert!(matches!(
            t.blocks().last(),
            Some(Block::Tool { status: ToolStatus::Running, .. })
        ));
        t.finish_tool("file-a\nfile-b", true);
        match t.blocks().last() {
            Some(Block::Tool { output, status, .. }) => {
                assert_eq!(output, "file-a\nfile-b");
                assert_eq!(*status, ToolStatus::Done);
            }
            other => panic!("wrong block: {other:?}"),
        }
    }

    #[test]
    fn failed_tools_are_marked() {
        let mut t = Transcript::default();
        t.start_tool("terminal", "{}");
        t.finish_tool("boom", false);
        assert!(matches!(
            t.blocks().last(),
            Some(Block::Tool { status: ToolStatus::Failed, .. })
        ));
    }

    #[test]
    fn wrapping_fits_the_width_and_splits_long_words() {
        assert_eq!(wrap("one two three", 5), vec!["one", "two", "three"]);
        assert_eq!(wrap("abc def", 8), vec!["abc def".to_string()]);
        // Long word is hard-split to width.
        let wrapped = wrap("abcdef", 4);
        assert_eq!(wrapped, vec!["abcd".to_string(), "ef".to_string()]);
        // Empty text still renders one (empty) line.
        assert_eq!(wrap("", 10), vec![String::new()]);
        // Newlines force breaks.
        let multi = wrap("a\nb", 10);
        assert_eq!(multi.len(), 2);
    }

    #[test]
    fn to_lines_renders_labels_and_wraps() {
        let mut t = Transcript::default();
        t.push_user("short question");
        t.apply_stream_delta("an answer");
        t.finish_streaming();
        t.push_notice("(new session)");
        let lines = t.to_lines(DEFAULT_WIDTH);
        let plain: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans.iter().map(|s| s.content.clone()).collect::<String>()
            })
            .collect();
        assert!(plain.iter().any(|l| l.contains("❯ short question")), "{plain:?}");
        assert!(plain.iter().any(|l| l.contains("an answer")));
        assert!(plain.iter().any(|l| l.contains("(new session)")));
        assert_eq!(plain.last().unwrap(), "(new session)", "no trailing separator");
    }

    #[test]
    fn long_lines_are_wrapped_to_width() {
        let mut t = Transcript::default();
        t.push_notice(&"x".repeat(200));
        let lines = t.to_lines(80);
        assert!(lines.len() >= 3);
        for l in &lines {
            let len: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(len <= 80, "line too long: {len}");
        }
    }

    #[test]
    fn visible_range_is_bottom_anchored_with_clamped_scroll() {
        let mut t = Transcript::default();
        t.push_notice("one");
        t.push_notice("two");
        t.push_notice("three");
        let total = t.rendered_len(DEFAULT_WIDTH);
        assert!(total >= 3);
        // At the tail: the last `height` lines.
        let (s, e) = t.visible_range(total, 3);
        assert_eq!((s, e), (total - 3, total));
        // Scroll up one page-step shifts the window toward the transcript
        // start, clamped at the top.
        t.scroll_by(1, 3);
        let (s, e) = t.visible_range(total, 3);
        assert_eq!(e, total - 1);
        assert_eq!(s, total - 4);
        // Scrolling past the top clamps.
        t.scroll_by(100, 3);
        let (s, e) = t.visible_range(total, 3);
        assert_eq!((s, e), (0, 3));
        // Back to the tail (negative = toward the tail, clamped at 0).
        t.scroll_by(-100, 3);
        let (s, e) = t.visible_range(total, 3);
        assert_eq!(e, total);
    }

    #[test]
    fn history_seeding_maps_roles() {
        let mut t = Transcript::default();
        let history = vec![
            Message::text(Role::System, "sys"),
            Message::text(Role::User, "question"),
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::ToolUse { call: hermes_core::ToolCall {
                        id: "1".into(), name: "terminal".into(), arguments: "{}".into(),
                    } },
                    Content::Text { text: "done".into() },
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult { call_id: "1".into(), output: "ok".into() }],
            },
        ];
        t.seed_from_history(&history);
        let blocks = t.blocks();
        assert!(matches!(blocks[0], Block::User { .. }));
        assert!(matches!(blocks[1], Block::Tool { .. }));
        assert!(matches!(blocks[2], Block::Assistant { .. }));
        assert!(matches!(blocks[3], Block::Tool { .. }));
        assert_eq!(blocks[3].body(), "ok");
    }
}
