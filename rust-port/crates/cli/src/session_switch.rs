//! Session listing and resume — Rust port of the `/resume` / `/sessions`
//! numbered-selection UX from `hermes_cli/cli_commands_mixin.py`
//! (`_handle_resume_command`, `_resolve_resume_target`, `_list_recent_sessions`)
//! adapted to the Rust JSONL store, where one session = one `*.jsonl` file in
//! the sessions directory.
//!
//! Parity points kept from the Python:
//! * the recent list is capped at 10 entries, newest activity first;
//! * a bare `/resume` shows the list AND arms a one-shot pending selection so
//!   the user can type just the number (`3`) on the next line;
//! * the numbered branch resolves against the same list the table showed;
//! * users who copy the help text's placeholder brackets/quotes verbatim
//!   (`/resume <abc123>`) get them stripped.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The Python `_list_recent_sessions(limit=10)` — the same limit everywhere.
pub const RESUME_LIST_LIMIT: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionEntry {
    /// File stem = the session id (the Python row `id`).
    pub id: String,
    pub path: PathBuf,
    pub modified: SystemTime,
    pub messages: usize,
}

/// Newest-activity-first listing of the sessions directory, excluding the
/// current session file, capped at `RESUME_LIST_LIMIT`.
pub fn list_sessions(dir: &Path, current: Option<&Path>) -> Vec<SessionEntry> {
    let mut out: Vec<SessionEntry> = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else { return out };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(cur) = current {
            if path == cur {
                continue;
            }
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let messages = count_messages(&path);
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push(SessionEntry { id, path, modified, messages });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.id.cmp(&b.id)));
    out.truncate(RESUME_LIST_LIMIT);
    out
}

fn count_messages(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// Strip copy-pasted placeholder wrappers (Python `_handle_resume_command`:
/// `<abc123>` / `[abc123]` / `"abc123"` / `'abc123'`).
pub fn strip_placeholder(target: &str) -> &str {
    let t = target.trim();
    if t.len() >= 2 {
        let bytes = t.as_bytes();
        let (first, last) = (bytes[0], bytes[t.len() - 1]);
        if matches!((first, last), (b'<', b'>') | (b'[', b']') | (b'"', b'"') | (b'\'', b'\'')) {
            return t[1..t.len() - 1].trim();
        }
    }
    t
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// A number outside 1..=len (Python `resume.out_of_range`).
    OutOfRange(usize),
    /// No session file with that id/stem (Python `resume.not_found`).
    NotFound(String),
}

/// Resolve a numbered selection, session id, or file stem against the recent
/// list — port of the Python `_resolve_resume_target` (digits index the
/// recent-10 table; free text resolves by name or id).
pub fn resolve_target<'a>(
    entries: &'a [SessionEntry],
    target: &str,
) -> Result<&'a SessionEntry, ResolveError> {
    let target = strip_placeholder(target);
    if target.chars().all(|c| c.is_ascii_digit()) && !target.is_empty() {
        let index: usize = target.parse().unwrap_or(0);
        if index < 1 || index > entries.len() {
            return Err(ResolveError::OutOfRange(index));
        }
        return Ok(&entries[index - 1]);
    }
    entries
        .iter()
        .find(|e| e.id == target)
        .ok_or_else(|| ResolveError::NotFound(target.to_string()))
}

/// The numbered table the Python `_show_recent_sessions` renders, including
/// the footer hint that drives the bare-number selection.
pub fn format_listing(entries: &[SessionEntry]) -> String {
    if entries.is_empty() {
        return "No previous sessions found. Sessions are saved next to the current one.\n\
                Use /title-named JSONL files in the sessions directory to name them."
            .into();
    }
    let mut out = String::from("Recent sessions:\n");
    for (i, e) in entries.iter().enumerate() {
        writeln!(
            &mut out,
            "\x20 {}. {} ({} messages)",
            i + 1,
            e.id,
            e.messages
        )
        .ok();
    }
    out.push_str("\nResume: /resume <number>, /resume <session name>, or just the number.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-sesswitch-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lists_jsonl_files_newest_first_excluding_current() {
        let dir = temp_dir("ordering");
        let older = dir.join("older.jsonl");
        let newer = dir.join("newer.jsonl");
        let current = dir.join("current.jsonl");
        std::fs::write(&older, "{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"a\"}]}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(120));
        std::fs::write(&newer, "{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"b\"}]}\n{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"c\"}]}\n").unwrap();
        std::fs::write(&current, "").unwrap();
        let stray = dir.join("notes.txt");
        std::fs::write(&stray, "not a session").unwrap();

        let sessions = list_sessions(&dir, Some(&current));
        assert_eq!(sessions.len(), 2, "current + non-jsonl excluded: {sessions:?}");
        assert_eq!(sessions[0].id, "newer");
        assert_eq!(sessions[1].id, "older");
        assert_eq!(sessions[0].messages, 2);
        assert_eq!(sessions[1].messages, 1);

        let listing = format_listing(&sessions);
        assert!(listing.contains("1. newer (2 messages)"));
        assert!(listing.contains("2. older (1 messages)"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolves_numbers_placeholders_and_stems() {
        let entries: Vec<SessionEntry> = vec![
            SessionEntry { id: "alpha".into(), path: PathBuf::from("alpha.jsonl"), modified: SystemTime::now(), messages: 3 },
            SessionEntry { id: "beta".into(), path: PathBuf::from("beta.jsonl"), modified: SystemTime::now(), messages: 1 },
        ];
        assert_eq!(resolve_target(&entries, "1").unwrap().id, "alpha");
        assert_eq!(resolve_target(&entries, "2").unwrap().id, "beta");
        assert_eq!(resolve_target(&entries, "beta").unwrap().id, "beta");
        // Placeholder brackets/quotes are stripped (Python parity).
        assert_eq!(resolve_target(&entries, "<beta>").unwrap().id, "beta");
        assert_eq!(resolve_target(&entries, "[alpha]").unwrap().id, "alpha");
        assert_eq!(resolve_target(&entries, "\"alpha\"").unwrap().id, "alpha");
        assert_eq!(resolve_target(&entries, "0"), Err(ResolveError::OutOfRange(0)));
        assert_eq!(resolve_target(&entries, "9"), Err(ResolveError::OutOfRange(9)));
        assert_eq!(resolve_target(&entries, "gamma"), Err(ResolveError::NotFound("gamma".into())));
    }
}
