//! MemoryStore — bounded, file-backed curated memory (MEMORY.md / USER.md).
//!
//! Port of `tools/memory_tool_store.py`: entries joined by `ENTRY_DELIMITER`,
//! budgets in chars, add/replace/patch/remove with the Python match semantics
//! (whole-entry exact match first, then typography-folded substring), patch
//! span rules, over-budget refusals, external-drift detection, atomic writes.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Entries are joined by this delimiter when persisted and counted for budget.
pub const ENTRY_DELIMITER: &str = "\n§\n";

pub const MEMORY_BLOCK_HEADER: &str = "MEMORY (your personal notes)";
pub const USER_BLOCK_HEADER: &str = "USER PROFILE (who the user is)";
pub const PATCH_DROP_HINT: &str = "To drop text inside an entry, patch a longer span that contains it; 'remove' deletes the whole entry.";

/// Which file a call targets: the agent notes or the user profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Memory,
    User,
}

impl Target {
    fn filename(self) -> &'static str {
        match self {
            Target::Memory => "MEMORY.md",
            Target::User => "USER.md",
        }
    }
}

/// Structured error, mirroring the Python `_error(message, failure_class, **extra)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryError {
    pub message: String,
    pub failure_class: &'static str,
    /// Live entries after a failed over-budget edit (`current_entries`).
    pub current_entries: Option<Vec<String>>,
    /// `"x/y chars"` usage string when present.
    pub usage: Option<String>,
    /// Closest entries on a zero-match (`no_match`) failure.
    pub closest_entries: Option<Vec<String>>,
    /// Drift backup path on a drift refusal.
    pub drift_backup: Option<String>,
}

impl MemoryError {
    fn new(message: impl Into<String>, failure_class: &'static str) -> Self {
        MemoryError {
            message: message.into(),
            failure_class,
            current_entries: None,
            usage: None,
            closest_entries: None,
            drift_backup: None,
        }
    }
}

/// Successful mutation result (`_success_response` minus the echo of entries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationOutcome {
    pub message: String,
    pub usage: String,
    pub entry_count: usize,
    /// Full entry text a replace/patch/remove overwrote (`replaced_entry` etc.).
    pub replaced_entry: Option<String>,
    /// The patched entry after the edit (`patched_entry_after`).
    pub patched_entry_after: Option<String>,
    /// The entry a remove deleted (`removed_entry`).
    pub removed_entry: Option<String>,
}

/// Bounded curated memory with file persistence. One instance per agent;
/// `memory_dir` is the profile `memories/` directory.
pub struct MemoryStore {
    pub memory_entries: Vec<String>,
    pub user_entries: Vec<String>,
    pub memory_char_limit: usize,
    pub user_char_limit: usize,
    memory_dir: PathBuf,
}

impl MemoryStore {
    /// Defaults mirror the Python constructor: 2200 chars for MEMORY.md,
    /// 1375 for USER.md.
    pub fn new(memory_dir: impl Into<PathBuf>) -> Self {
        MemoryStore {
            memory_entries: Vec::new(),
            user_entries: Vec::new(),
            memory_char_limit: 2200,
            user_char_limit: 1375,
            memory_dir: memory_dir.into(),
        }
    }

    pub fn with_limits(
        memory_dir: impl Into<PathBuf>,
        memory_char_limit: usize,
        user_char_limit: usize,
    ) -> Self {
        MemoryStore {
            memory_entries: Vec::new(),
            user_entries: Vec::new(),
            memory_char_limit,
            user_char_limit,
            memory_dir: memory_dir.into(),
        }
    }

    fn path_for(&self, target: Target) -> PathBuf {
        self.memory_dir.join(target.filename())
    }

    fn entries(&self, target: Target) -> &[String] {
        match target {
            Target::Memory => &self.memory_entries,
            Target::User => &self.user_entries,
        }
    }

    fn set_entries(&mut self, target: Target, entries: Vec<String>) {
        match target {
            Target::Memory => self.memory_entries = entries,
            Target::User => self.user_entries = entries,
        }
    }

    fn limit(&self, target: Target) -> usize {
        match target {
            Target::Memory => self.memory_char_limit,
            Target::User => self.user_char_limit,
        }
    }

    /// Char budget over the joined entries (Python `len()` counts chars, so
    /// the 3-char `"\n§\n"` delimiter is counted as chars, not bytes).
    fn char_count(&self, target: Target) -> usize {
        ENTRY_DELIMITER
            .chars()
            .count()
            .saturating_mul(self.entries(target).len().saturating_sub(1))
            + self
                .entries(target)
                .iter()
                .map(|e| e.chars().count())
                .sum::<usize>()
    }

    fn usage(&self, target: Target) -> String {
        format!(
            "{}/{} chars",
            format_usize(self.char_count(target)),
            format_usize(self.limit(target))
        )
    }

    /// Load MEMORY.md / USER.md from disk (dedup, order-preserving, first
    /// occurrence wins). Over-limit files load anyway: the limit only fires on
    /// add/replace/patch, never on load.
    pub fn load_from_disk(&mut self) -> std::io::Result<()> {
        for target in [Target::Memory, Target::User] {
            let path = self.path_for(target);
            let (raw, read_ok) = read_raw_checked(&path);
            if !read_ok {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} exists but could not be read", path.display()),
                ));
            }
            let entries = parse_entries(&raw);
            self.set_entries(target, entries);
        }
        Ok(())
    }

    /// Append a new entry; errors when it would exceed the char limit.
    /// Idempotent: an existing entry is a no-op success. Append-only, so the
    /// drift guard is skipped (appending never clobbers foreign content).
    pub fn add(&mut self, target: Target, content: &str) -> Result<MutationOutcome, MemoryError> {
        let content = content.trim();
        if content.is_empty() {
            return Err(MemoryError::new(
                "Content cannot be empty.",
                "missing_content",
            ));
        }
        // Lock, re-read from disk, mutate, persist (`_mutate`).
        let path = self.path_for(target);
        let (raw, read_ok) = read_raw_checked(&path);
        if !read_ok {
            return Err(read_failed_error(&path));
        }
        let disk = parse_entries(&raw);
        self.set_entries(target, disk);

        if self.entries(target).iter().any(|e| e == content) {
            return Ok(self.outcome(target, "Entry already exists (no duplicate added)."));
        }
        let mut candidate = self.entries(target).to_vec();
        candidate.push(content.to_string());
        let new_total = joined_len(&candidate);
        let limit = self.limit(target);
        if new_total > limit {
            let over = new_total - limit;
            return Err(self.over_budget_failure(
                target,
                format!(
                    "Memory at {}/{} chars; adding this entry ({} chars) would exceed the limit by {} chars.",
                    format_usize(self.char_count(target)),
                    format_usize(limit),
                    format_usize(content.len()),
                    format_usize(over)
                ),
            ));
        }
        let entries = candidate;
        if let Err(err) = persist(&path, &entries) {
            return Err(MemoryError::new(err, "write_failed"));
        }
        self.set_entries(target, entries);
        Ok(self.outcome(target, "Entry added."))
    }

    /// Replace the WHOLE entry containing *old_text* with *new_content*.
    pub fn replace(
        &mut self,
        target: Target,
        old_text: &str,
        new_content: &str,
    ) -> Result<MutationOutcome, MemoryError> {
        let new_content = new_content.trim();
        if old_text.trim().is_empty() {
            return Err(MemoryError::new(
                "old_text cannot be empty.",
                "missing_old_text",
            ));
        }
        if new_content.is_empty() {
            return Err(MemoryError::new(
                "new_content cannot be empty. Use 'remove' to delete entries.",
                "missing_content",
            ));
        }
        self.edit(target, old_text.trim(), Some(new_content))
    }

    /// Replace ONE span *old_text* inside the matched entry with *new_text*;
    /// the rest of the entry is preserved verbatim. A span occurring more than
    /// once is rejected rather than edited everywhere.
    pub fn patch(
        &mut self,
        target: Target,
        old_text: &str,
        new_text: &str,
    ) -> Result<MutationOutcome, MemoryError> {
        if old_text.trim().is_empty() {
            return Err(MemoryError::new(
                "old_text cannot be empty.",
                "missing_old_text",
            ));
        }
        if new_text.trim().is_empty() {
            return Err(MemoryError::new(
                format!("new_text cannot be empty. {PATCH_DROP_HINT}"),
                "missing_content",
            ));
        }
        // Lock + reload first so the span rules run against disk truth.
        let path = self.path_for(target);
        let (raw, read_ok) = read_raw_checked(&path);
        if !read_ok {
            return Err(read_failed_error(&path));
        }
        let disk = parse_entries(&raw);
        self.set_entries(target, disk);
        if let Some(bak) = self.detect_external_drift(target, &raw) {
            let mut err = MemoryError::new(drift_message(&path), "drift");
            err.drift_backup = Some(bak);
            return Err(err);
        }
        let entries = self.entries(target).to_vec();
        let idx = self.locate(&entries, old_text, "patch")?;

        let before = entries[idx].clone();
        let (after, patch_err) = patch_entry(&before, old_text, new_text);
        let Some(after) = after else {
            return Err(MemoryError::new(patch_err.unwrap_or_default(), "ambiguous"));
        };
        let mut patched = entries.clone();
        patched[idx] = after.clone();
        let limit = self.limit(target);
        if joined_len(&patched) > limit {
            let new_total = joined_len(&patched);
            return Err(self.over_budget_failure(
                target,
                format!(
                    "Patch would put memory at {}/{} chars. Shorten the new text, or 'remove' other stale or less important entries to make room, then retry.",
                    format_usize(new_total),
                    format_usize(limit)
                ),
            ));
        }
        if let Err(err) = persist(&path, &patched) {
            return Err(MemoryError::new(err, "write_failed"));
        }
        self.set_entries(target, patched);
        let mut outcome = self.outcome(target, "Entry patched.");
        outcome.replaced_entry = Some(before);
        outcome.patched_entry_after = Some(after);
        Ok(outcome)
    }

    /// Remove the entry containing *old_text*.
    pub fn remove(
        &mut self,
        target: Target,
        old_text: &str,
    ) -> Result<MutationOutcome, MemoryError> {
        if old_text.trim().is_empty() {
            return Err(MemoryError::new(
                "old_text cannot be empty.",
                "missing_old_text",
            ));
        }
        self.edit(target, old_text.trim(), None)
    }

    fn edit(
        &mut self,
        target: Target,
        old_text: &str,
        new_content: Option<&str>,
    ) -> Result<MutationOutcome, MemoryError> {
        let path = self.path_for(target);
        let (raw, read_ok) = read_raw_checked(&path);
        if !read_ok {
            return Err(read_failed_error(&path));
        }
        let disk = parse_entries(&raw);
        self.set_entries(target, disk);
        if let Some(bak) = self.detect_external_drift(target, &raw) {
            let mut err = MemoryError::new(drift_message(&path), "drift");
            err.drift_backup = Some(bak);
            return Err(err);
        }
        let entries = self.entries(target).to_vec();
        let verb = if new_content.is_some() {
            "replace"
        } else {
            "remove"
        };
        let idx = self.locate(&entries, old_text, verb)?;
        let previous = entries[idx].clone();
        let mut updated = entries.clone();
        match new_content {
            Some(content) => updated[idx] = content.to_string(),
            None => {
                updated.remove(idx);
            }
        }
        let limit = self.limit(target);
        if joined_len(&updated) > limit {
            let new_total = joined_len(&updated);
            return Err(self.over_budget_failure(
                target,
                format!(
                    "Replacement would put memory at {}/{} chars: free at least {} chars.",
                    format_usize(new_total),
                    format_usize(limit),
                    format_usize(new_total - limit)
                ),
            ));
        }
        if let Err(err) = persist(&path, &updated) {
            return Err(MemoryError::new(err, "write_failed"));
        }
        self.set_entries(target, updated);
        let verb_message = if new_content.is_some() {
            "Entry replaced."
        } else {
            "Entry removed."
        };
        let mut outcome = self.outcome(target, verb_message);
        if new_content.is_some() {
            outcome.replaced_entry = Some(previous);
        } else {
            outcome.removed_entry = Some(previous);
        }
        Ok(outcome)
    }

    /// Index of the entry *old_text* selects, or the error the edit returns.
    fn locate(&self, entries: &[String], old_text: &str, verb: &str) -> Result<usize, MemoryError> {
        let (idx, ambiguous) = find_unique_match(entries, old_text);
        if ambiguous {
            return Err(MemoryError::new(
                format!("Multiple entries matched '{old_text}'. Be more specific."),
                "ambiguous",
            ));
        }
        match idx {
            Some(i) => Ok(i),
            None => {
                let mut err = no_match_error(entries, old_text, verb);
                err.current_entries = Some(entries.to_vec());
                Err(err)
            }
        }
    }

    /// `.bak.<unix_ts>` snapshot when an entry exceeds the whole-file limit —
    /// free-form text appended without delimiters; flushing would discard it.
    fn detect_external_drift(&self, target: Target, raw: &str) -> Option<String> {
        let over = parse_entries(raw)
            .iter()
            .any(|e| e.len() > self.limit(target));
        if !over {
            return None;
        }
        let path = self.path_for(target);
        let bak_path = path.with_extension(format!(
            "{}.bak.{}",
            path.extension().and_then(|e| e.to_str()).unwrap_or("md"),
            unix_timestamp()
        ));
        match fs::write(&bak_path, raw) {
            Ok(_) => Some(bak_path.display().to_string()),
            Err(_) => Some(format!(
                "{} (BACKUP FAILED — file unchanged on disk)",
                bak_path.display()
            )),
        }
    }

    fn over_budget_failure(&self, target: Target, message: String) -> MemoryError {
        let mut err = MemoryError::new(message, "over_budget");
        err.current_entries = Some(self.entries(target).to_vec());
        err.usage = Some(self.usage(target));
        err
    }

    fn outcome(&self, target: Target, message: &str) -> MutationOutcome {
        MutationOutcome {
            message: message.to_string(),
            usage: self.usage(target),
            entry_count: self.entries(target).len(),
            replaced_entry: None,
            patched_entry_after: None,
            removed_entry: None,
        }
    }
}

/// `(patched entry, None)` or `(None, error)` when the span occurs more than
/// once or the edit is a no-op (`_patch_entry`).
fn patch_entry(entry: &str, old_text: &str, new_text: &str) -> (Option<String>, Option<String>) {
    let count = span_occurrences(entry, old_text);
    if count > 1 {
        return (
            None,
            Some(format!(
                "'{old_text}' appears {count} times in the matched entry, and patch edits one span. Include more surrounding text so old_text occurs once."
            )),
        );
    }
    let patched = entry.replacen(old_text, new_text, 1);
    if patched == entry {
        return (
            None,
            Some(format!(
                "Patch is a no-op: '{old_text}' -> '{new_text}' leaves the entry unchanged."
            )),
        );
    }
    (Some(patched), None)
}

/// Occurrences of *span* in *text*, overlapping ones included ('00' twice in '1000').
fn span_occurrences(text: &str, span: &str) -> usize {
    if span.is_empty() || span.len() > text.len() {
        return 0;
    }
    let text = text.as_bytes();
    let span = span.as_bytes();
    (0..=text.len() - span.len())
        .filter(|&i| &text[i..i + span.len()] == span)
        .count()
}

/// Typography a model re-types differently: every quote/backtick -> `'`,
/// every dash/minus -> `-`, whitespace runs (re-wrapped lines, NBSP) -> one
/// space, literal backslash-n -> space (`_normalize_for_match`).
fn normalize_for_match(text: &str) -> String {
    let folded: String = text
        .replace("\\n", " ")
        .chars()
        .map(|c| match c {
            '"' | '`' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' | '\u{201c}'
            | '\u{201d}' | '\u{201e}' | '\u{201f}' => '\'',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            _ => c,
        })
        .collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Indices of entries containing *old_text*: byte-exact first, else folded.
fn substring_matches(entries: &[String], old_text: &str) -> Vec<usize> {
    let exact: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.contains(old_text))
        .map(|(i, _)| i)
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let needle = normalize_for_match(old_text);
    if !needle.chars().any(|c| c.is_alphanumeric()) {
        // punctuation/space alone would select a whole entry
        return Vec::new();
    }
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| normalize_for_match(e).contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

/// `(index, ambiguous)`: a whole-entry EXACT match takes absolute priority —
/// substring matches are only considered when no entry equals *old_text*.
/// Distinct matches -> `(None, true)`.
fn find_unique_match(entries: &[String], old_text: &str) -> (Option<usize>, bool) {
    let exact: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.as_str() == old_text)
        .map(|(i, _)| i)
        .collect();
    let matches = if !exact.is_empty() {
        exact
    } else {
        substring_matches(entries, old_text)
    };
    let mut distinct: Vec<&String> = matches.iter().map(|&i| &entries[i]).collect();
    distinct.sort();
    distinct.dedup();
    if distinct.len() > 1 {
        return (None, true);
    }
    (matches.first().copied(), false)
}

/// Zero-match error naming the entries sharing the most words with *old_text*
/// (`_no_match_error`).
fn no_match_error(entries: &[String], old_text: &str, verb: &str) -> MemoryError {
    let words = word_set(old_text);
    let mut scored: Vec<(f64, usize)> = Vec::new();
    if !words.is_empty() {
        for (i, e) in entries.iter().enumerate() {
            let shared = words.intersection(&word_set(e)).count();
            let score = shared as f64 / words.len() as f64;
            if score >= 0.3 {
                scored.push((score, i));
            }
        }
    }
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    let closest: Vec<String> = scored
        .into_iter()
        .take(3)
        .map(|(_, i)| entries[i].clone())
        .collect();
    let hint = if old_text.contains('§') {
        " old_text must come from ONE entry ('§' separates entries); use one op per entry."
    } else {
        ""
    };
    let mut message = format!(
        "No entry matched '{old_text}'.{hint} Retry the {verb} with old_text copied verbatim from the intended entry"
    );
    if !closest.is_empty() {
        message.push_str(" (closest_entries below).");
    } else {
        message.push('.');
    }
    let mut err = MemoryError::new(message, "no_match");
    if !closest.is_empty() {
        err.closest_entries = Some(closest);
    }
    err
}

fn word_set(text: &str) -> std::collections::HashSet<String> {
    text.chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_lowercase)
        .collect()
}

fn drift_message(path: &Path) -> String {
    format!(
        "Refusing to write {}: it holds an entry larger than the whole store's char limit, i.e. free-form text written without § delimiters (the patch tool, a shell append, a manual edit). Rewriting it would lose that text. A snapshot was saved. Resolve the drift first — either rewrite the file as a clean §-delimited list of entries, or move the extra content out — then retry.",
        path.display()
    )
}

fn read_failed_error(path: &Path) -> MemoryError {
    MemoryError::new(
        format!(
            "Refusing to write {}: the file exists on disk but could not be read right now. Treating an unreadable file as empty and saving would wipe existing memory, so the write is refused. Nothing was changed — retry in a moment.",
            path.display()
        ),
        "read_failed",
    )
}

/// `(raw, read_ok)`; `read_ok` is false ONLY when the file exists but can't be
/// read. A leading UTF-8 BOM (Notepad-edited files on Windows) is stripped.
fn read_raw_checked(path: &Path) -> (String, bool) {
    match fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), true),
        Err(_) => (String::new(), false),
        Ok(bytes) => {
            let bytes = bytes
                .strip_prefix(&[0xEF, 0xBB, 0xBF])
                .map_or(bytes.as_slice(), |b| b);
            match std::str::from_utf8(bytes) {
                Ok(text) => (text.to_string(), true),
                Err(_) => (String::new(), false),
            }
        }
    }
}

/// Stripped, non-empty entries; splits on the FULL delimiter so a bare `§` survives.
pub fn parse_entries(raw: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    raw.split(ENTRY_DELIMITER)
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .filter(|e| seen.insert(e.to_string()))
        .map(str::to_string)
        .collect()
}

fn joined_len(entries: &[String]) -> usize {
    ENTRY_DELIMITER.chars().count() * entries.len().saturating_sub(1)
        + entries.iter().map(|e| e.chars().count()).sum::<usize>()
}

/// Atomic temp-file + rename: readers never see a truncated file.
fn persist(path: &Path, entries: &[String]) -> Result<(), String> {
    let joined = entries.join(ENTRY_DELIMITER);
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return Err(format!("Failed to create memory dir {parent:?}: {e}"));
        }
    }
    let tmp = path.with_extension(format!("mem_{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(joined.as_bytes())?;
        f.sync_all()?;
        Ok(())
    };
    match write() {
        Ok(()) => {
            if let Err(e) = fs::rename(&tmp, path) {
                let _ = fs::remove_file(&tmp);
                Err(format!(
                    "Failed to write memory file {}: {e}",
                    path.display()
                ))
            } else {
                Ok(())
            }
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(format!(
                "Failed to write memory file {}: {e}",
                path.display()
            ))
        }
    }
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Thousands-separated rendering of `_over_limit` / `_usage` numbers.
fn format_usize(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

    fn tmp_store(memory_limit: usize, user_limit: usize) -> (MemoryStore, PathBuf) {
        let id = NEXT_TMP.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("hermes-skills-mem-{}-{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        (
            MemoryStore::with_limits(&dir, memory_limit, user_limit),
            dir,
        )
    }

    #[test]
    fn add_append_dedupes_and_persists() {
        let (mut store, dir) = tmp_store(2200, 1375);
        assert!(store.add(Target::Memory, "likes tea").is_ok());
        assert!(store.add(Target::Memory, "likes coffee").is_ok());
        // duplicate add is a no-op success
        let dup = store.add(Target::Memory, "likes tea").unwrap();
        assert_eq!(dup.entry_count, 2);
        assert!(dup.message.contains("already exists"));

        let raw = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert_eq!(parse_entries(&raw), vec!["likes tea", "likes coffee"]);
        assert_eq!(store.memory_entries, vec!["likes tea", "likes coffee"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_refuses_empty_and_over_budget() {
        let (mut store, dir) = tmp_store(20, 1375);
        assert_eq!(
            store.add(Target::Memory, "   ").unwrap_err().failure_class,
            "missing_content"
        );
        // 20-char limit: "a, b" joined = "a" + 3 + "b" = 5 chars fits; next entry doesn't
        assert!(store.add(Target::Memory, "aaaaa").is_ok()); // joined 5
        let err = store.add(Target::Memory, "bbbbbbbbbbbbbbbbbb").unwrap_err(); // 5+3+18=26 > 20
        assert_eq!(err.failure_class, "over_budget");
        assert!(err.current_entries.as_deref() == Some(&["aaaaa".to_string()][..]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn budget_counts_delimiter_chars() {
        let (mut store, dir) = tmp_store(2200, 1375);
        store.add(Target::Memory, "one").unwrap();
        store.add(Target::Memory, "two").unwrap();
        // "one" + "\n§\n" + "two" = 3 + 3 + 3
        assert_eq!(store.char_count(Target::Memory), 9);
        assert_eq!(store.usage(Target::Memory), "9/2,200 chars");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_deletes_whole_entry_and_reports_it() {
        let (mut store, dir) = tmp_store(2200, 1375);
        store.add(Target::Memory, "first entry").unwrap();
        store.add(Target::Memory, "second entry").unwrap();
        let out = store.remove(Target::Memory, "second entry").unwrap();
        assert_eq!(out.removed_entry.as_deref(), Some("second entry"));
        assert_eq!(store.memory_entries, vec!["first entry"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_substring_matches_entry_and_ambiguous_requires_specificity() {
        let (mut store, dir) = tmp_store(2200, 1375);
        store.add(Target::Memory, "tests pass quickly").unwrap();
        store.add(Target::Memory, "tests").unwrap();
        // "tests" is contained in the first entry, but the WHOLE-entry exact
        // match has absolute priority: the short entry stays addressable.
        let out = store.remove(Target::Memory, "tests").unwrap();
        assert_eq!(out.removed_entry.as_deref(), Some("tests"));
        // now two entries both contain "entry"? no — exercise ambiguity:
        store.add(Target::Memory, "alpha beta").unwrap();
        store.add(Target::Memory, "alpha gamma").unwrap();
        let err = store.remove(Target::Memory, "alpha").unwrap_err();
        assert_eq!(err.failure_class, "ambiguous");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_match_error_suggests_closest_entries() {
        let (mut store, dir) = tmp_store(2200, 1375);
        store
            .add(Target::Memory, "deploy steps: build, test, ship")
            .unwrap();
        let err = store
            .remove(Target::Memory, "deploy steps: build, test, send")
            .unwrap_err();
        assert_eq!(err.failure_class, "no_match");
        assert!(err.closest_entries.is_some());
        assert!(err.current_entries.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn patch_splices_only_the_span_and_preserves_the_rest() {
        let (mut store, dir) = tmp_store(2200, 1375);
        store
            .add(Target::Memory, "deploys with cargo ship")
            .unwrap();
        let out = store
            .patch(Target::Memory, "cargo ship", "cargo ship --release")
            .unwrap();
        assert_eq!(
            store.memory_entries,
            vec!["deploys with cargo ship --release"]
        );
        assert_eq!(
            out.patched_entry_after.as_deref(),
            Some("deploys with cargo ship --release")
        );
        assert_eq!(
            out.replaced_entry.as_deref(),
            Some("deploys with cargo ship")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn patch_rejects_repeated_span_and_no_ops() {
        let (mut store, dir) = tmp_store(2200, 1375);
        store.add(Target::Memory, "aa bb aa").unwrap();
        let err = store.patch(Target::Memory, "aa", "x").unwrap_err();
        assert_eq!(err.failure_class, "ambiguous");
        assert!(err.message.contains("appears 2 times"));
        let err = store.patch(Target::Memory, "bb", "bb").unwrap_err();
        assert!(err.message.contains("no-op"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn folded_typography_matches_for_remove() {
        let (mut store, dir) = tmp_store(2200, 1375);
        store
            .add(Target::Memory, "says \u{201c}hello\u{201d} \u{2014} always")
            .unwrap();
        // Model re-types the same entry with straight quotes and a plain dash;
        // the folded substring match still locates (and removes) the entry.
        let out = store.remove(Target::Memory, "\"hello\" - always").unwrap();
        assert_eq!(
            out.removed_entry.as_deref(),
            Some("says \u{201c}hello\u{201d} \u{2014} always")
        );
        assert!(store.memory_entries.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_overwrites_whole_entry_with_budget_check() {
        let (mut store, dir) = tmp_store(30, 1375);
        store.add(Target::Memory, "short").unwrap();
        let err = store
            .replace(Target::Memory, "short", "this replacement is far too long")
            .unwrap_err();
        assert_eq!(err.failure_class, "over_budget");
        let out = store.replace(Target::Memory, "short", "replaced!").unwrap();
        assert_eq!(out.replaced_entry.as_deref(), Some("short"));
        assert_eq!(store.memory_entries, vec!["replaced!"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_target_is_independent_and_persists_to_user_md() {
        let (mut store, dir) = tmp_store(2200, 1375);
        store.add(Target::Memory, "agent note").unwrap();
        store.add(Target::User, "user profile note").unwrap();
        assert!(dir.join("USER.md").exists());
        assert_eq!(store.user_entries, vec!["user profile note"]);
        assert_eq!(store.memory_entries, vec!["agent note"]);

        // fresh store reloads both files
        let mut reloaded = MemoryStore::new(&dir);
        reloaded.load_from_disk().unwrap();
        assert_eq!(reloaded.user_entries, vec!["user profile note"]);
        assert_eq!(reloaded.memory_entries, vec!["agent note"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_drift_snapshot_and_refusal() {
        let (mut store, dir) = tmp_store(20, 1375);
        let path = dir.join("MEMORY.md");
        fs::create_dir_all(&dir).unwrap();
        // one entry far over the whole-file limit: hand-written drift
        fs::write(&path, "x".repeat(50)).unwrap();
        let err = store.patch(Target::Memory, "x", "y").unwrap_err();
        assert_eq!(err.failure_class, "drift");
        let bak = err.drift_backup.unwrap();
        assert!(fs::read_to_string(&bak).unwrap().contains("xxxxx"));
        // the live entries ARE loaded from disk (matching the Python store);
        // only the WRITE is refused until the drift is resolved
        assert_eq!(store.memory_entries, vec!["x".repeat(50)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_entries_strips_and_dedupes() {
        assert_eq!(parse_entries("\n a \n§\nb\n§\n a \n"), vec!["a", "b"]);
        assert!(parse_entries("").is_empty());
    }

    #[test]
    fn span_occurrences_counts_overlaps() {
        assert_eq!(span_occurrences("1000", "00"), 2);
        assert_eq!(span_occurrences("abc", "abc"), 1);
        assert_eq!(span_occurrences("abc", ""), 0);
    }

    #[test]
    fn load_from_disk_reports_unreadable_file() {
        let (mut store, dir) = tmp_store(2200, 1375);
        let _ = fs::create_dir_all(&dir);
        // a directory named MEMORY.md exists but can't be read as a file
        let _ = fs::create_dir_all(dir.join("MEMORY.md"));
        assert!(store.load_from_disk().is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
