//! Multiline editor state — the Rust port of the Python TUI's TextArea
//! behavior: a character-cursor buffer, submit/newline keybindings, backslash
//! continuation on Enter (reusing the classic CLI's `multiline` rules), and
//! Up/Down history browsing.

use hermes_cli::multiline;

/// What one Enter press produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to do (empty buffer or folded continuation).
    None,
    /// A full line was submitted (already cleared from the buffer).
    Sent(String),
}

#[derive(Debug, Default)]
pub struct Editor {
    buf: String,
    /// Byte offset of the cursor (always on a char boundary).
    cursor: usize,
    history: Vec<String>,
    /// None = editing live; Some(i) = browsing history at `history[i]`.
    hist: Option<usize>,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.buf
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Insert a character at the cursor (plain typing).
    pub fn insert(&mut self, ch: char) {
        self.buf.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// Delete the character before the cursor (Backspace).
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.buf[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.buf.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    /// Insert a newline without submitting (Ctrl+J / Alt+Enter).
    pub fn newline(&mut self) {
        self.buf.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    /// Replace the whole buffer (completion accept) and move the cursor to
    /// the end.
    pub fn replace(&mut self, text: &str) {
        self.buf.clear();
        self.buf.push_str(text);
        self.cursor = self.buf.len();
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.hist = None;
    }

    /// Enter pressed. A trailing `\[ \t]*` folds into a newline instead of
    /// submitting (port of `_apply_backslash_line_continuation` — the classic
    /// CLI's continuation rules hold inside the TUI too).
    pub fn enter(&mut self) -> Outcome {
        if multiline::is_backslash_line_continuation(&self.buf) {
            let folded = multiline::apply_backslash_line_continuation(&self.buf);
            self.buf.clear();
            self.buf.push_str(&folded);
            self.cursor = self.buf.len();
            return Outcome::None;
        }
        if self.buf.trim().is_empty() {
            return Outcome::None;
        }
        let line = std::mem::take(&mut self.buf);
        self.cursor = 0;
        self.hist = None;
        self.history.push(line.clone());
        Outcome::Sent(line)
    }

    /// Up: browse back through submitted history.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.hist {
            None => self.history.len() - 1,
            Some(i) => i.saturating_sub(1),
        };
        self.hist = Some(idx);
        self.replace(&self.history[idx].clone());
    }

    /// Down: forward through history, back to the live buffer at the end.
    pub fn history_next(&mut self) {
        let Some(i) = self.hist else { return };
        if i + 1 >= self.history.len() {
            self.hist = None;
            self.clear();
        } else {
            self.hist = Some(i + 1);
            self.replace(&self.history[i + 1].clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_lands_at_the_cursor_and_backspace_deletes_backwards() {
        let mut e = Editor::default();
        for ch in "héllo".chars() {
            e.insert(ch);
        }
        assert_eq!(e.cursor(), "héllo".len());
        e.backspace(); // removes 'o' (2 bytes)
        assert_eq!(e.text(), "héll");
        e.insert('o');
        assert_eq!(e.text(), "héllo");
        // Backspace on an empty buffer is a no-op.
        let mut e = Editor::default();
        e.backspace();
        assert_eq!(e.text(), "");
    }

    #[test]
    fn enter_submits_and_clears_but_not_when_empty() {
        let mut e = Editor::default();
        e.insert('h');
        e.insert('i');
        match e.enter() {
            Outcome::Sent(line) => assert_eq!(line, "hi"),
            Outcome::None => panic!("should have sent"),
        }
        assert!(e.is_empty());
        // Empty Enter is a no-op.
        assert_eq!(e.enter(), Outcome::None);
    }

    #[test]
    fn trailing_backslash_folds_enter_into_a_newline() {
        let mut e = Editor::default();
        for ch in "one \\".chars() {
            e.insert(ch);
        }
        // Enter does NOT submit; the marker becomes a literal newline
        // (Python `_apply_backslash_line_continuation`).
        assert_eq!(e.enter(), Outcome::None);
        assert_eq!(e.text(), "one \n");
        for ch in "two".chars() {
            e.insert(ch);
        }
        match e.enter() {
            Outcome::Sent(line) => assert_eq!(line, "one \ntwo"),
            Outcome::None => panic!("should have sent"),
        }
    }

    #[test]
    fn ctrl_j_newline_is_a_plain_newline_without_continuation_semantics() {
        let mut e = Editor::default();
        for ch in "ab".chars() {
            e.insert(ch);
        }
        e.newline();
        e.insert('c');
        match e.enter() {
            Outcome::Sent(line) => assert_eq!(line, "ab\nc"),
            Outcome::None => panic!("should have sent"),
        }
    }

    #[test]
    fn up_down_browses_history_and_returns_to_the_live_buffer() {
        let mut e = Editor::default();
        for line in ["first", "second"] {
            for ch in line.chars() {
                e.insert(ch);
            }
            e.enter();
        }
        e.history_prev();
        assert_eq!(e.text(), "second");
        e.history_prev();
        assert_eq!(e.text(), "first");
        // Clamped at the oldest entry.
        e.history_prev();
        assert_eq!(e.text(), "first");
        e.history_next();
        assert_eq!(e.text(), "second");
        // Past the newest → back to the (empty) live buffer.
        e.history_next();
        assert!(e.is_empty());
        // Down with no history position is a no-op.
        e.history_next();
        assert!(e.is_empty());
        // A fresh entry resets the history cursor.
        e.history_prev();
        e.insert('!');
        assert_eq!(e.text(), "second!");
    }

    #[test]
    fn completion_accept_replaces_the_buffer() {
        let mut e = Editor::default();
        for ch in "/comp".chars() {
            e.insert(ch);
        }
        e.replace("/compress ");
        assert_eq!(e.text(), "/compress ");
        assert_eq!(e.cursor(), e.text().len());
    }

    #[test]
    fn clear_resets_buffer_cursor_and_history_cursor() {
        let mut e = Editor::default();
        e.insert('x');
        e.history_prev(); // arms hist
        e.clear();
        assert!(e.is_empty());
        assert_eq!(e.cursor(), 0);
        // hist disarmed: next nav starts from the newest again.
        e.history_next();
        assert!(e.is_empty());
    }
}
