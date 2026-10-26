//! Keybindings — Rust port of the portable subset of the Python TUI's
//! `_tui_build_key_bindings` (prompt_toolkit KeyBindings):
//!
//! ```text
//! Enter                  → submit          (escape-enter / c-j → newline)
//! Alt+Enter / Ctrl+J     → insert newline  (Claude Code / Codex / OpenCode)
//! Tab                    → accept completion (popup open)
//! Up/Down                → history (popup closed) / popup navigation (open)
//! Escape                 → interrupt the running turn (Python double-escape)
//! Ctrl+C                 → clear input, quit when already empty
//! Ctrl+D                 → quit (EOF)
//! PageUp/PageDown        → transcript scrollback
//! ```
//!
//! Plain characters, Backspace, Delete, Home/End are the editor's defaults
//! and are handled there, not in this mapping.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Ctrl+J / Alt+Enter: newline without submitting.
    InsertNewline,
    /// Enter with the completion popup closed: submit the editor buffer.
    Send,
    /// Tab / Enter with the completion popup open: accept the selection.
    AcceptCompletion,
    AutocompleteNext,
    AutocompletePrev,
    HistoryPrev,
    HistoryNext,
    /// Escape: send /stop (interrupt the in-flight turn).
    Interrupt,
    /// Ctrl+C with text in the editor: clear it.
    ClearInput,
    /// Ctrl+C on an empty editor / Ctrl+D: quit.
    Quit,
    ScrollUp,
    ScrollDown,
    Ignored,
}

pub fn map_key(key: KeyEvent, popup_open: bool, editor_empty: bool) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('c') if ctrl => {
            if editor_empty {
                Action::Quit
            } else {
                Action::ClearInput
            }
        }
        KeyCode::Char('d') if ctrl => Action::Quit,
        KeyCode::Char('j') if ctrl => Action::InsertNewline,
        KeyCode::Enter if alt => Action::InsertNewline,
        KeyCode::Enter => {
            if popup_open {
                Action::AcceptCompletion
            } else {
                Action::Send
            }
        }
        KeyCode::Tab => {
            if popup_open {
                Action::AcceptCompletion
            } else {
                Action::Ignored
            }
        }
        KeyCode::Up => {
            if popup_open {
                Action::AutocompletePrev
            } else {
                Action::HistoryPrev
            }
        }
        KeyCode::Down => {
            if popup_open {
                Action::AutocompleteNext
            } else {
                Action::HistoryNext
            }
        }
        KeyCode::Esc => Action::Interrupt,
        KeyCode::PageUp => Action::ScrollUp,
        KeyCode::PageDown => Action::ScrollDown,
        _ => Action::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn enter_sends_and_ctrl_j_alt_enter_insert_newlines() {
        assert_eq!(map_key(key(KeyCode::Enter, KeyModifiers::NONE), false, false), Action::Send);
        assert_eq!(
            map_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL), false, false),
            Action::InsertNewline
        );
        assert_eq!(
            map_key(key(KeyCode::Enter, KeyModifiers::ALT), false, false),
            Action::InsertNewline
        );
    }

    #[test]
    fn popup_redirects_enter_tab_and_up_down() {
        let open = true;
        assert_eq!(map_key(key(KeyCode::Enter, KeyModifiers::NONE), open, false), Action::AcceptCompletion);
        assert_eq!(map_key(key(KeyCode::Tab, KeyModifiers::NONE), open, false), Action::AcceptCompletion);
        assert_eq!(map_key(key(KeyCode::Tab, KeyModifiers::NONE), false, false), Action::Ignored);
        assert_eq!(map_key(key(KeyCode::Up, KeyModifiers::NONE), open, false), Action::AutocompletePrev);
        assert_eq!(map_key(key(KeyCode::Down, KeyModifiers::NONE), open, false), Action::AutocompleteNext);
        assert_eq!(map_key(key(KeyCode::Up, KeyModifiers::NONE), false, false), Action::HistoryPrev);
        assert_eq!(map_key(key(KeyCode::Down, KeyModifiers::NONE), false, false), Action::HistoryNext);
    }

    #[test]
    fn escape_interrupts_and_ctrl_c_clears_then_quits() {
        assert_eq!(map_key(key(KeyCode::Esc, KeyModifiers::NONE), false, false), Action::Interrupt);
        assert_eq!(map_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), false, true), Action::Quit);
        assert_eq!(map_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), false, false), Action::ClearInput);
        assert_eq!(map_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL), false, true), Action::Quit);
    }

    #[test]
    fn page_keys_scroll_the_transcript() {
        assert_eq!(map_key(key(KeyCode::PageUp, KeyModifiers::NONE), false, false), Action::ScrollUp);
        assert_eq!(map_key(key(KeyCode::PageDown, KeyModifiers::NONE), false, false), Action::ScrollDown);
    }

    #[test]
    fn plain_characters_are_left_to_the_editor() {
        assert_eq!(map_key(key(KeyCode::Char('x'), KeyModifiers::NONE), false, false), Action::Ignored);
        assert_eq!(map_key(key(KeyCode::Backspace, KeyModifiers::NONE), false, false), Action::Ignored);
    }
}
