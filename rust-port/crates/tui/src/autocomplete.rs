//! Slash-command autocomplete — Rust port of the Python
//! `SlashCommandCompleter` (`hermes_cli/commands_completion.py`) popup
//! behavior, driven by the exact same command table the REPL dispatches
//! (`hermes_cli::slash::COMMAND_REGISTRY`).
//!
//! Python semantics kept:
//! - case-folded *prefix* match over command names AND aliases
//!   (`_prefix_completions`);
//! - an exact match completes to nothing (`skip_exact=True`) so the popup
//!   closes once the full command is typed;
//! - the completion replaces just the typed `/word`
//!   (`Completion(text, start_position=-len(word))` → full canonical name
//!   plus a trailing space);
//! - once args start (first space typed), the popup closes.

use hermes_cli::slash::COMMAND_REGISTRY;

/// One popup row: the command row plus the name that matched (canonical or
/// alias — accepting always completes to the canonical name).
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Index into COMMAND_REGISTRY.
    pub def: &'static hermes_cli::slash::CommandDef,
    /// The matched name (canonical or an alias).
    pub matched: &'static str,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.matched == other.matched && std::ptr::eq(self.def, other.def)
    }
}

impl Eq for Candidate {}

impl Candidate {
    /// The canonical command name without slash.
    pub fn canonical(&self) -> &'static str {
        self.def.name
    }
}

#[derive(Debug, Default, Clone)]
pub struct Autocomplete {
    open: bool,
    selected: usize,
    matches: Vec<Candidate>,
}

impl Autocomplete {
    pub fn is_open(&self) -> bool {
        self.open && !self.matches.is_empty()
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.matches
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_candidate(&self) -> Option<&Candidate> {
        self.matches.get(self.selected)
    }

    /// Recompute the popup for the current editor text. Called after every
    /// editor change (Python recomputes per keystroke).
    pub fn update(&mut self, input: &str) {
        self.matches.clear();
        self.selected = 0;
        let Some(rest) = input.trim_start().strip_prefix('/') else {
            self.open = false;
            return;
        };
        // Only the leading word is completed; args close the popup.
        if rest.contains(char::is_whitespace) {
            self.open = false;
            return;
        }
        let partial = rest.to_ascii_lowercase();
        for def in COMMAND_REGISTRY {
            let names = std::iter::once(def.name).chain(def.aliases.iter().copied());
            for name in names {
                let lowered = name.to_ascii_lowercase();
                // skip_exact: a fully-typed command offers nothing.
                if lowered.starts_with(&partial) && lowered != partial {
                    self.matches.push(Candidate { def, matched: name });
                }
            }
        }
        self.open = true;
    }

    /// Force the popup closed (Escape — Python closes the completion menu).
    pub fn close(&mut self) {
        self.open = false;
        self.matches.clear();
        self.selected = 0;
    }

    /// Move the selection down (wraps, Python menu-cycling).
    pub fn move_down(&mut self) {
        if self.is_open() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    pub fn move_up(&mut self) {
        if self.is_open() {
            self.selected = (self.selected + self.matches.len() - 1) % self.matches.len();
        }
    }

    /// Tab/Enter accept: replace the typed `/partial` with the selected
    /// candidate's canonical command plus a trailing space. Returns the new
    /// editor text, or None when there is nothing to accept.
    pub fn accept(&self, input: &str) -> Option<String> {
        let candidate = self.selected_candidate()?;
        // Re-anchor to the leading word: keep any leading whitespace.
        let trimmed = input.trim_start();
        let lead = input.len() - trimmed.len();
        let lead_ws = &input[..lead];
        let _ = trimmed;
        Some(format!("{lead_ws}/{} ", candidate.canonical()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_cli::slash::COMMAND_REGISTRY;

    fn names(ac: &Autocomplete) -> Vec<&'static str> {
        ac.candidates().iter().map(|c| c.matched).collect()
    }

    #[test]
    fn empty_input_starts_closed() {
        let mut ac = Autocomplete::default();
        ac.update("");
        assert!(!ac.is_open());
        ac.update("hello");
        assert!(!ac.is_open());
    }

    #[test]
    fn bare_slash_lists_every_command_and_alias() {
        let mut ac = Autocomplete::default();
        ac.update("/");
        assert!(ac.is_open());
        // Aliases are offered as their own rows, like the Python completer.
        let expected: usize = COMMAND_REGISTRY.iter().map(|d| 1 + d.aliases.len()).sum();
        assert_eq!(ac.candidates().len(), expected);
    }

    #[test]
    fn filters_by_case_folded_prefix() {
        let mut ac = Autocomplete::default();
        ac.update("/SE");
        assert!(ac.is_open());
        assert_eq!(names(&ac), vec!["sessions"]);
    }

    #[test]
    fn aliases_match_alongside_canonical_names() {
        // Canonical-name prefix...
        let mut ac = Autocomplete::default();
        ac.update("/comp");
        assert!(ac.is_open());
        assert!(names(&ac).contains(&"compress"));
        // ...and the alias row sits alongside it (both start with "comp").
        assert!(names(&ac).contains(&"compact"));
        // An alias prefix offers the alias row, whose canonical command is
        // what accepting completes to.
        let mut ac = Autocomplete::default();
        ac.update("/compac");
        assert_eq!(names(&ac), vec!["compact"]);
        assert_eq!(ac.selected_candidate().unwrap().canonical(), "compress");
    }

    #[test]
    fn exact_match_closes_the_popup() {
        let mut ac = Autocomplete::default();
        ac.update("/new");
        // "new" itself is skipped (skip_exact) and has no other matches.
        assert!(!ac.is_open());
        // Same for a fully-typed alias: the exact row is skipped and no
        // other name carries that prefix.
        let mut ac = Autocomplete::default();
        ac.update("/compact");
        assert!(!ac.is_open());
        // But a *prefix* of an alias still offers the canonical name.
        let mut ac = Autocomplete::default();
        ac.update("/compac");
        assert!(ac.is_open());
        assert_eq!(ac.selected_candidate().unwrap().canonical(), "compress");
    }

    #[test]
    fn args_and_whitespace_close_the_popup() {
        let mut ac = Autocomplete::default();
        ac.update("/model gpt");
        assert!(!ac.is_open());
        let mut ac = Autocomplete::default();
        ac.update("/resume ");
        assert!(!ac.is_open());
    }

    #[test]
    fn selection_moves_and_wraps() {
        let mut ac = Autocomplete::default();
        ac.update("/");
        let n = ac.candidates().len();
        assert_eq!(ac.selected(), 0);
        ac.move_up();
        assert_eq!(ac.selected(), n - 1, "up from the first wraps to last");
        ac.move_down();
        assert_eq!(ac.selected(), 0);
        ac.move_down();
        assert_eq!(ac.selected(), 1);
    }

    #[test]
    fn accept_completes_to_canonical_name_with_trailing_space() {
        // An alias-typed partial completes to the canonical "/compress ".
        let mut ac = Autocomplete::default();
        ac.update("/compac");
        assert_eq!(ac.accept("/compac").unwrap(), "/compress ");
        // Accepting a canonical partial keeps the canonical name.
        let mut ac = Autocomplete::default();
        ac.update("/compre");
        assert_eq!(ac.accept("/compre").unwrap(), "/compress ");
        // Nothing open → nothing to accept.
        let mut ac = Autocomplete::default();
        ac.update("plain text");
        assert!(ac.accept("plain text").is_none());
    }

    #[test]
    fn close_keeps_state_consistent() {
        let mut ac = Autocomplete::default();
        ac.update("/re");
        ac.close();
        assert!(!ac.is_open());
        assert!(ac.candidates().is_empty());
        assert_eq!(ac.selected(), 0);
    }

    #[test]
    fn every_registry_row_is_reachable_through_the_popup() {
        // Drift guard: the popup is driven by the same table as the REPL.
        // (First-letter prefixes are unique across the registry, and an
        // exact full name would be skipped by skip_exact.)
        for def in COMMAND_REGISTRY {
            let mut ac = Autocomplete::default();
            ac.update(&format!("/{}", &def.name[..1]));
            assert!(
                ac.candidates().iter().any(|c| c.canonical() == def.name),
                "popup missed /{}",
                def.name
            );
        }
    }
}
