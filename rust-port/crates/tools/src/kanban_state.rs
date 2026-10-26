//! Kanban board state — port of the tool-reachable core of
//! `hermes_cli/kanban_db.py` (+ `kanban_db_dispatch.heartbeat_worker`,
//! `kanban_db_graph.initial_task_state`, `kanban_workflow`) from the Python
//! hermes-agent.
//!
//! STORAGE FORMAT DEVIATION (documented): the Python board is a SQLite
//! database (`kanban.db`) with tables `tasks`, `task_links`, `task_runs`,
//! `task_comments`, `task_events`, `task_attachments`. The Rust workspace has
//! no SQLite dependency available offline, so this port persists ONE JSON
//! document (`board.json`) whose objects carry the Python COLUMN NAMES
//! verbatim, so a board round-trips through this layer and the record shapes
//! match the Python row shapes field-for-field. The resolution chain is the
//! Python one: explicit `board` slug → `HERMES_KANBAN_DB` (a file path) →
//! `HERMES_KANBAN_BOARD` (a slug) → the `default` board under
//! `$HERMES_HOME/kanban/boards/<slug>/board.json` (`$HERMES_HOME` else
//! `~/.hermes`).
//!
//! Transition semantics ported faithfully:
//! - statuses: triage/todo/scheduled/ready/running/blocked/review/done(+archived)
//! - `create_task` initial state (`initial_task_state`): blocked > triage >
//!   todo (any non-done parent) > ready; unknown parents are a ValueError;
//!   `idempotency_key` returns the newest non-archived match.
//! - `complete_task`: parents satisfied; created-cards verification
//!   (HallucinatedCardsError); empty-completion gate (EmptyCompletionError);
//!   live-claim fence (LiveClaimError); CAS
//!   `running|ready|blocked|review -> done` with
//!   `AND current_run_id = ?` when `expected_run_id` is set; run close or
//!   zero-duration synthesis; `completed` event; recompute_ready.
//! - `block_task`: dependency → todo (re-kinded to needs_input when no parent
//!   is open), recurrence counting → triage at limit 2, others → blocked.
//! - `schedule_task`: todo/ready/running/blocked → scheduled.
//! - `request_review`: running/ready → review with implementer provenance.
//! - `request_changes`: closes a review-claimed running run back to the
//!   implementer from the latest review_requested event, re-gated.
//! - `unblock_task`: blocked/scheduled → ready/todo (review if it left off
//!   there), sticky blocks only exit via this verb.
//! - `link_tasks`: cycle/self-link/running-child rejection; ready child
//!   demotion to todo.
//! - `recompute_ready`: todo/blocked with all parents done/archived promote
//!   (resume_status from the newest lifecycle event), sticky blocks skipped.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

// --- Constants (Python kanban_db / kanban_workflow) ---

pub const ARCHIVED: &str = "archived";
pub const VALID_STATUSES: [&str; 9] = [
    "triage", "todo", "scheduled", "ready", "running", "blocked", "review", "done", ARCHIVED,
];
pub const VALID_BLOCK_KINDS: [&str; 4] = ["dependency", "needs_input", "capability", "transient"];
pub const VALID_WORKSPACE_KINDS: [&str; 3] = ["scratch", "worktree", "dir"];
pub const VALID_INITIAL_STATUSES: [&str; 2] = ["running", "blocked"];
pub const BLOCK_RECURRENCE_LIMIT: i64 = 2;
pub const KANBAN_ATTACHMENT_MAX_BYTES: usize = 25 * 1024 * 1024;
pub const DEFAULT_CLAIM_TTL_SECONDS: i64 = 15 * 60;
pub const DEFAULT_BOARD: &str = "default";

pub fn is_valid_status(s: &str) -> bool {
    VALID_STATUSES.contains(&s)
}

// --- Errors (Python exception classes; Display strings are byte-frozen) ---

#[derive(Debug)]
pub enum KbError {
    /// Python `ValueError` — rendered by `_kanban_handler` without a traceback.
    Value(String),
    /// `LiveClaimError`
    LiveClaim(String),
    /// `HallucinatedCardsError` — carries the phantom ids.
    Hallucinated(Vec<String>),
    /// `EmptyCompletionError`
    EmptyCompletion(String),
    /// `ArtifactPreservationError`
    Artifact(String),
}

impl KbError {
    /// The `{e}` text Python interpolates into `tool_error(f"{tool_name}: {e}")`.
    pub fn message(&self) -> String {
        match self {
            KbError::Value(m) | KbError::LiveClaim(m) | KbError::Artifact(m) => m.clone(),
            KbError::Hallucinated(phantom) => format!(
                "completion blocked: claimed created_cards that do not exist or were not created by this worker: {}",
                phantom.join(", ")
            ),
            KbError::EmptyCompletion(task_id) => {
                format!("completion blocked: {task_id} has no result or summary evidence")
            }
        }
    }
}

impl std::fmt::Display for KbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

// --- Records (Python column names, verbatim) ---

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Task {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub workspace_kind: Option<String>,
    #[serde(default)]
    pub workspace_path: Option<String>,
    #[serde(default)]
    pub branch_name: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub completed_at: Option<i64>,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub current_run_id: Option<i64>,
    #[serde(default)]
    pub model_override: Option<String>,
    #[serde(default)]
    pub provider_override: Option<String>,
    #[serde(default)]
    pub completion_contract: Option<String>,
    #[serde(default)]
    pub last_failure_error: Option<String>,
    // --- internal columns (tasks table, not all model-facing) ---
    #[serde(default)]
    pub block_kind: Option<String>,
    #[serde(default)]
    pub block_recurrences: i64,
    #[serde(default)]
    pub consecutive_failures: i64,
    #[serde(default)]
    pub goal_mode: bool,
    #[serde(default)]
    pub goal_max_turns: Option<i64>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub max_runtime_seconds: Option<i64>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub claim_lock: Option<String>,
    #[serde(default)]
    pub claim_expires: Option<i64>,
    #[serde(default)]
    pub worker_pid: Option<i64>,
    #[serde(default)]
    pub worker_started_at: Option<i64>,
    #[serde(default)]
    pub last_heartbeat_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Run {
    pub id: i64,
    pub task_id: String,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub ended_at: Option<i64>,
    #[serde(default)]
    pub claim_expires: Option<i64>,
    #[serde(default)]
    pub last_heartbeat_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Comment {
    pub id: i64,
    pub task_id: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Event {
    pub id: i64,
    pub task_id: String,
    #[serde(default)]
    pub run_id: Option<i64>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Attachment {
    pub id: i64,
    pub task_id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub stored_path: String,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub uploaded_by: Option<String>,
    #[serde(default)]
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Link {
    pub parent_id: String,
    pub child_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Counters {
    #[serde(default)]
    pub run: i64,
    #[serde(default)]
    pub comment: i64,
    #[serde(default)]
    pub event: i64,
    #[serde(default)]
    pub attachment: i64,
}

/// The on-disk board document. Field names mirror the SQLite table names.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BoardFile {
    #[serde(default)]
    pub schema: i64,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub runs: Vec<Run>,
    #[serde(default)]
    pub comments: Vec<Comment>,
    #[serde(default)]
    pub events: Vec<Event>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub counters: Counters,
}

// --- Board resolution ---

/// Python `_BOARD_SLUG_RE` + `_normalize_board_slug`.
pub fn normalize_board_slug(slug: Option<&str>) -> Result<Option<String>, KbError> {
    let s = match slug {
        Some(s) => s.trim().to_lowercase(),
        None => String::new(),
    };
    if s.is_empty() {
        return Ok(None);
    }
    let bytes: Vec<char> = s.chars().collect();
    let first_ok = bytes.first().map(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).unwrap_or(false);
    let ok = bytes.len() <= 64
        && first_ok
        && bytes.iter().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_'
        });
    if !ok {
        return Err(KbError::Value(format!(
            "invalid board slug {slug:?}: must be 1-64 chars, lowercase alphanumerics / hyphens / underscores, not starting with '-' or '_'"
        )));
    }
    Ok(Some(s))
}

/// Python `hermes_constants.get_hermes_home`: `HERMES_HOME` → `~/.hermes`.
pub fn hermes_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home);
        }
    }
    #[cfg(windows)]
    let base = std::env::var("USERPROFILE").unwrap_or_default();
    #[cfg(not(windows))]
    let base = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(base).join(".hermes")
}

/// Python `kanban_db.kanban_db_path(board)`: where this port stores a board.
pub fn board_file_path(board: Option<&str>) -> Result<PathBuf, KbError> {
    if let Ok(db) = std::env::var("HERMES_KANBAN_DB") {
        if !db.trim().is_empty() {
            // An explicit env path IS the board file (Python: the sqlite file).
            return Ok(PathBuf::from(db));
        }
    }
    let slug = if let Some(slug) = board {
        normalize_board_slug(Some(slug))?.unwrap_or_else(|| DEFAULT_BOARD.to_string())
    } else if let Ok(b) = std::env::var("HERMES_KANBAN_BOARD") {
        normalize_board_slug(Some(&b))?.unwrap_or_else(|| DEFAULT_BOARD.to_string())
    } else {
        DEFAULT_BOARD.to_string()
    };
    Ok(hermes_home()
        .join("kanban")
        .join("boards")
        .join(slug)
        .join("board.json"))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

static TASK_ID_SEQ: AtomicU64 = AtomicU64::new(0);

/// Python `_new_task_id`: `t_` + 8 hex chars.
fn new_task_id() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ TASK_ID_SEQ.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E3779B97F83C15B);
    let mixed = n ^ (n >> 29);
    format!("t_{:08x}", (mixed & 0xffff_ffff) as u32)
}

/// A board loaded from (and persisted back to) its `board.json`.
pub struct Board {
    pub path: PathBuf,
    pub file: BoardFile,
}

impl Board {
    pub fn load(path: PathBuf) -> std::io::Result<Self> {
        let file = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<BoardFile>(&bytes).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("corrupt board {}: {e}", path.display()),
                )
            })?,
            Err(_) => BoardFile {
                schema: 1,
                ..Default::default()
            },
        };
        Ok(Board { path, file })
    }

    /// Atomic persist (temp file + rename), as SQLite's commit atomicity.
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&self.file).unwrap())?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    // --- Row accessors ---

    pub fn get_task(&self, task_id: &str) -> Option<&Task> {
        self.file.tasks.iter().find(|t| t.id == task_id)
    }

    pub fn task_status(&self, task_id: &str) -> Option<String> {
        self.get_task(task_id).map(|t| t.status.clone())
    }

    fn task_mut(&mut self, task_id: &str) -> Option<&mut Task> {
        self.file.tasks.iter_mut().find(|t| t.id == task_id)
    }

    pub fn current_run_id(&self, task_id: &str) -> Option<i64> {
        self.get_task(task_id)
            .and_then(|t| t.current_run_id.filter(|id| *id > 0))
    }

    pub fn parent_ids(&self, task_id: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .file
            .links
            .iter()
            .filter(|l| l.child_id == task_id)
            .map(|l| l.parent_id.clone())
            .collect();
        ids.sort();
        ids
    }

    pub fn child_ids(&self, task_id: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .file
            .links
            .iter()
            .filter(|l| l.parent_id == task_id)
            .map(|l| l.child_id.clone())
            .collect();
        ids.sort();
        ids
    }

    pub fn unsatisfied_parents(&self, task_id: &str) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = self
            .parent_ids(task_id)
            .into_iter()
            .filter_map(|pid| {
                self.get_task(&pid)
                    .filter(|p| !["done", "archived"].contains(&p.status.as_str()))
                    .map(|p| (p.id.clone(), p.status.clone()))
            })
            .collect();
        rows.sort();
        rows
    }

    fn parents_satisfied(&self, task_id: &str) -> bool {
        self.unsatisfied_parents(task_id).is_empty()
    }

    pub fn list_comments(&self, task_id: &str) -> Vec<&Comment> {
        self.file.comments.iter().filter(|c| c.task_id == task_id).collect()
    }

    pub fn list_comments_after(&self, task_id: &str, after_id: i64) -> Vec<&Comment> {
        self.file
            .comments
            .iter()
            .filter(|c| c.task_id == task_id && c.id > after_id)
            .collect()
    }

    pub fn list_events(&self, task_id: &str) -> Vec<&Event> {
        self.file.events.iter().filter(|e| e.task_id == task_id).collect()
    }

    pub fn list_runs(&self, task_id: &str) -> Vec<&Run> {
        self.file.runs.iter().filter(|r| r.task_id == task_id).collect()
    }

    pub fn latest_run(&self, task_id: &str) -> Option<&Run> {
        self.file
            .runs
            .iter()
            .filter(|r| r.task_id == task_id)
            .max_by_key(|r| r.id)
    }

    pub fn list_attachments(&self, task_id: &str) -> Vec<&Attachment> {
        self.file
            .attachments
            .iter()
            .filter(|a| a.task_id == task_id)
            .collect()
    }

    // --- Event / run primitives ---

    pub fn append_event(
        &mut self,
        task_id: &str,
        kind: &str,
        payload: Option<Value>,
        run_id: Option<i64>,
    ) -> i64 {
        self.file.counters.event += 1;
        let id = self.file.counters.event;
        self.file.events.push(Event {
            id,
            task_id: task_id.to_string(),
            run_id,
            kind: kind.to_string(),
            payload: payload.filter(|p| !as_null(p)),
            created_at: Some(now()),
        });
        id
    }

    fn latest_event(&self, task_id: &str, kinds: &[&str]) -> Option<&Event> {
        self.file
            .events
            .iter()
            .filter(|e| e.task_id == task_id && kinds.contains(&e.kind.as_str()))
            .max_by_key(|e| e.id)
    }

    /// Python `_end_run`: close the active run; None when none was open.
    fn end_run(
        &mut self,
        task_id: &str,
        outcome: &str,
        status: Option<&str>,
        summary: Option<&str>,
        error: Option<&str>,
        metadata: Option<&Value>,
    ) -> Option<i64> {
        let run_id = self.current_run_id(task_id)?;
        if let Some(run) = self.file.runs.iter_mut().find(|r| r.id == run_id && r.ended_at.is_none()) {
            run.status = status.unwrap_or(outcome).to_string();
            run.outcome = Some(outcome.to_string());
            run.summary = summary.map(str::to_string);
            run.error = error.map(str::to_string);
            run.metadata = metadata.cloned().filter(|m| !as_null(m));
            run.ended_at = Some(now());
            run.claim_expires = None;
        }
        if let Some(task) = self.task_mut(task_id) {
            task.current_run_id = None;
        }
        Some(run_id)
    }

    /// Python `_synthesize_ended_run`: zero-duration closed run on a
    /// never-claimed task so handoff fields survive.
    fn synthesize_ended_run(
        &mut self,
        task_id: &str,
        outcome: &str,
        summary: Option<&str>,
        metadata: Option<&Value>,
        profile_override: Option<&str>,
    ) -> i64 {
        self.file.counters.run += 1;
        let id = self.file.counters.run;
        let profile = profile_override
            .map(str::to_string)
            .or_else(|| self.get_task(task_id).and_then(|t| t.assignee.clone()));
        self.file.runs.push(Run {
            id,
            task_id: task_id.to_string(),
            profile,
            status: outcome.to_string(),
            outcome: Some(outcome.to_string()),
            summary: summary.map(str::to_string),
            error: None,
            metadata: metadata.cloned().filter(|m| !as_null(m)),
            started_at: Some(now()),
            ended_at: Some(now()),
            claim_expires: None,
            last_heartbeat_at: None,
        });
        id
    }

    fn end_or_synthesize_run(
        &mut self,
        task_id: &str,
        outcome: &str,
        status: &str,
        summary: Option<&str>,
        metadata: Option<&Value>,
        synthesize: bool,
        profile_override: Option<&str>,
    ) -> Option<i64> {
        if let Some(run_id) = self.end_run(task_id, outcome, Some(status), summary, None, metadata) {
            return Some(run_id);
        }
        if synthesize {
            return Some(self.synthesize_ended_run(task_id, outcome, summary, metadata, profile_override));
        }
        None
    }

    // --- Dependency machinery ---

    fn missing_task_ids(&self, ids: &[String]) -> Vec<String> {
        ids.iter()
            .filter(|id| !id.is_empty() && self.get_task(id).is_none())
            .cloned()
            .collect()
    }

    /// Python `_would_cycle`: true iff `parent_id` is already a descendant of `child_id`.
    fn would_cycle(&self, parent_id: &str, child_id: &str) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![child_id.to_string()];
        while let Some(node) = stack.pop() {
            if node == parent_id {
                return true;
            }
            if !seen.insert(node.clone()) {
                continue;
            }
            for link in &self.file.links {
                if link.parent_id == node {
                    stack.push(link.child_id.clone());
                }
            }
        }
        false
    }

    /// Python `_resume_status_from_events`: `review` when the newest lifecycle
    /// event carries a review resume/retry/source status, else `ready`.
    fn resume_status_from_events(&self, task_id: &str) -> String {
        let kinds = [
            "blocked", "block_loop_detected", "dependency_wait", "gave_up", "unblocked",
            "changes_requested", "review_reopened", "status", "reclaimed", "stale", "timed_out",
            "crashed", "spawn_failed", "rate_limited",
        ];
        if let Some(event) = self.latest_event(task_id, &kinds) {
            if let Some(payload) = &event.payload {
                for key in ["resume_status", "retry_status", "source_status"] {
                    if payload.get(key).and_then(Value::as_str) == Some("review") {
                        return "review".into();
                    }
                }
            }
        }
        "ready".into()
    }

    /// Python `_has_sticky_block`: newest blocked/unblocked event says the
    /// block waits for an operator (explicit kanban_block).
    fn has_sticky_block(&self, task_id: &str) -> bool {
        self.latest_event(task_id, &["blocked", "unblocked"])
            .map(|e| e.kind == "blocked")
            .unwrap_or(false)
    }

    /// Python `recompute_ready`: promote todo/blocked tasks whose parents are
    /// all done/archived; returns the promotion count.
    pub fn recompute_ready(&mut self) -> usize {
        let mut promoted = 0;
        let ids: Vec<String> = self
            .file
            .tasks
            .iter()
            .filter(|t| t.status == "todo" || t.status == "blocked")
            .map(|t| t.id.clone())
            .collect();
        for task_id in ids {
            let cur_status = self.task_status(&task_id).unwrap_or_default();
            if cur_status == "blocked" && self.has_sticky_block(&task_id) {
                continue;
            }
            let satisfied = self.parents_satisfied(&task_id);
            if !satisfied {
                continue;
            }
            let resume_status = self.resume_status_from_events(&task_id);
            if cur_status == "blocked" {
                let failures = self.get_task(&task_id).map(|t| t.consecutive_failures).unwrap_or(0);
                if failures >= BLOCK_RECURRENCE_LIMIT {
                    continue;
                }
            }
            let (updated, prior) = {
                let task = match self.task_mut(&task_id) {
                    Some(t) => t,
                    None => continue,
                };
                if task.status != cur_status {
                    continue;
                }
                task.status = resume_status.clone();
                (true, cur_status)
            };
            if updated {
                let payload = if resume_status == "ready" {
                    None
                } else {
                    Some(json!({ "status": resume_status }))
                };
                if prior == "blocked" {
                    // recompute_ready's blocked branch clears the breaker budget
                    // the way unblock does (fresh start for the retry budget).
                    if let Some(task) = self.task_mut(&task_id) {
                        task.consecutive_failures = 0;
                        task.last_failure_error = None;
                    }
                }
                self.append_event(&task_id, "promoted", payload, None);
                promoted += 1;
            }
        }
        promoted
    }

    // --- Task lifecycle verbs ---

    /// Python `create_task`. Returns the (new or idempotent) task id.
    #[allow(clippy::too_many_arguments)]
    pub fn create_task(
        &mut self,
        title: &str,
        body: Option<&str>,
        assignee: &str,
        created_by: &str,
        parents: &[String],
        tenant: Option<&str>,
        priority: i64,
        workspace_kind: Option<&str>,
        workspace_path: Option<&str>,
        project_id: Option<&str>,
        triage: bool,
        idempotency_key: Option<&str>,
        max_runtime_seconds: Option<i64>,
        skills: Option<Vec<String>>,
        model_override: Option<&str>,
        provider_override: Option<&str>,
        goal_mode: bool,
        goal_max_turns: Option<i64>,
        completion_contract: Option<&str>,
        initial_status: &str,
        session_id: Option<&str>,
    ) -> Result<String, KbError> {
        if title.trim().is_empty() {
            return Err(KbError::Value("title is required".into()));
        }
        if !VALID_INITIAL_STATUSES.contains(&initial_status) {
            return Err(KbError::Value(format!(
                "initial_status must be one of ['blocked', 'running']",
            )));
        }
        if let Some(kind) = workspace_kind {
            if !VALID_WORKSPACE_KINDS.contains(&kind) {
                return Err(KbError::Value(format!(
                    "workspace_kind must be one of ['dir', 'scratch', 'worktree'], got {kind:?}"
                )));
            }
        }
        if idempotency_key.is_some() {
            if let Some(existing) = self
                .file
                .tasks
                .iter()
                .filter(|t| t.idempotency_key.as_deref() == idempotency_key && t.status != ARCHIVED)
                .max_by_key(|t| t.created_at.unwrap_or(0))
                .map(|t| t.id.clone())
            {
                return Ok(existing);
            }
        }
        // Python `initial_task_state`: validate parents, resolve the tenant,
        // blocked > triage > todo (open parent) > ready.
        if !parents.is_empty() {
            let missing = self.missing_task_ids(parents);
            if !missing.is_empty() {
                return Err(KbError::Value(format!(
                    "unknown parent task(s): {}",
                    missing.join(", ")
                )));
            }
        }
        let tenant = parents
            .iter()
            .filter_map(|p| self.get_task(p).and_then(|t| t.tenant.clone()))
            .next()
            .or_else(|| tenant.map(str::to_string));
        let task_status = if initial_status == "blocked" {
            "blocked"
        } else if triage {
            "triage"
        } else if parents
            .iter()
            .any(|p| self.get_task(p).map(|t| t.status != "done").unwrap_or(false))
        {
            "todo"
        } else {
            "ready"
        };
        let task_id = new_task_id();
        let now_ts = now();
        let workspace_kind = workspace_kind.unwrap_or("scratch");
        let mut links = Vec::new();
        for pid in parents {
            links.push(Link {
                parent_id: pid.clone(),
                child_id: task_id.clone(),
            });
        }
        self.file.links.extend(links);
        let task = Task {
            id: task_id.clone(),
            title: title.trim().to_string(),
            body: body.map(str::to_string),
            assignee: Some(assignee.to_string()),
            status: task_status.to_string(),
            tenant,
            priority,
            workspace_kind: Some(workspace_kind.to_string()),
            workspace_path: workspace_path.map(str::to_string),
            project_id: project_id.map(str::to_string),
            created_by: Some(created_by.to_string()),
            created_at: Some(now_ts),
            goal_mode,
            goal_max_turns,
            idempotency_key: idempotency_key.map(str::to_string),
            skills,
            max_runtime_seconds,
            model_override: model_override.map(str::to_string),
            provider_override: provider_override.map(str::to_string),
            completion_contract: completion_contract.map(str::to_string),
            session_id: session_id.map(str::to_string),
            ..Default::default()
        };
        self.file.tasks.push(task);
        self.append_event(
            &task_id,
            "created",
            Some(json!({
                "assignee": assignee,
                "status": task_status,
                "parents": parents,
                "workspace_kind": workspace_kind,
                "model_override": model_override,
                "provider_override": provider_override,
                "goal_mode": goal_mode.then_some(true),
            })),
            None,
        );
        if task_status == "blocked" {
            self.append_event(
                &task_id,
                "blocked",
                Some(json!({ "reason": "initial_status", "status": "blocked", "actor": created_by })),
                None,
            );
        }
        if task_status == "todo" {
            let gating: Vec<String> = parents
                .iter()
                .filter(|p| {
                    self.task_status(p)
                        .map(|s| !["done", "archived"].contains(&s.as_str()))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            if let Some(parent) = gating.first() {
                self.append_event(
                    &task_id,
                    "dependency_wait",
                    Some(json!({ "reason": "parent_not_done", "parent": parent })),
                    None,
                );
            }
        }
        Ok(task_id)
    }

    /// Python `link_tasks`. Returns whether a ready child was demoted to todo.
    pub fn link_tasks(
        &mut self,
        parent_id: &str,
        child_id: &str,
        expected_child_run_id: Option<i64>,
    ) -> Result<bool, KbError> {
        if parent_id == child_id {
            return Err(KbError::Value("a task cannot depend on itself".into()));
        }
        let missing = self.missing_task_ids(&[parent_id.to_string(), child_id.to_string()]);
        if !missing.is_empty() {
            return Err(KbError::Value(format!(
                "unknown task(s): {}",
                missing.join(", ")
            )));
        }
        let child_status = self.get_task(child_id).map(|t| t.status.clone()).unwrap_or_default();
        if child_status == "running" {
            let owned = expected_child_run_id.is_some()
                && self.current_run_id(child_id) == expected_child_run_id;
            if !owned {
                return Err(KbError::Value(format!(
                    "cannot link {parent_id} -> {child_id}: child is already running"
                )));
            }
        }
        if self.would_cycle(parent_id, child_id) {
            return Err(KbError::Value(format!(
                "linking {parent_id} -> {child_id} would create a cycle"
            )));
        }
        if !self.file.links.iter().any(|l| l.parent_id == parent_id && l.child_id == child_id) {
            self.file.links.push(Link {
                parent_id: parent_id.to_string(),
                child_id: child_id.to_string(),
            });
        }
        let mut gated = false;
        if self.task_status(parent_id).map(|s| !["done", "archived"].contains(&s.as_str())).unwrap_or(true) {
            if let Some(task) = self.task_mut(child_id) {
                if task.status == "ready" {
                    task.status = "todo".into();
                    gated = true;
                }
            }
            if gated {
                self.append_event(
                    child_id,
                    "dependency_wait",
                    Some(json!({ "reason": "parent_not_done", "demoted": true, "parent": parent_id })),
                    None,
                );
            }
        }
        self.append_event(
            child_id,
            "linked",
            Some(json!({ "parent": parent_id, "child": child_id })),
            None,
        );
        Ok(gated)
    }

    /// Python `complete_task`. Ok(true) on a completed transition.
    #[allow(clippy::too_many_arguments)]
    pub fn complete_task(
        &mut self,
        task_id: &str,
        result: Option<&str>,
        summary: Option<&str>,
        metadata: Option<&Value>,
        created_cards: &[String],
        expected_run_id: Option<i64>,
        force: bool,
    ) -> Result<bool, KbError> {
        if !self.parents_satisfied(task_id) {
            return Ok(false);
        }
        let verified_cards = self.verify_created_cards(task_id, created_cards)?;
        self.gate_empty_completion(task_id, result, summary)?;
        let handoff_summary = summary.or(result);
        // Live-claim fence: refuse to close a live worker's run without proof
        // of ownership or an explicit operator override.
        if expected_run_id.is_none() && !force {
            let live = self.get_task(task_id).map(|t| {
                t.status == "running" && t.claim_lock.is_some() && t.worker_pid.is_some()
            });
            if live == Some(true) {
                return Err(KbError::LiveClaim(format!(
                    "{task_id} is running under a live worker claim; pass expected_run_id (worker ownership) or force=True (explicit operator override) instead of closing the live run"
                )));
            }
        }
        // Snapshot the board so an artifact-staging failure can roll the whole
        // transition back (Python does it with a transaction; the tool surface
        // only needs "no state change" on the structured rejection).
        let snapshot = serde_json::to_vec(&self.file).unwrap_or_default();
        // CAS: running|ready|blocked|review -> done (+ run ownership guard).
        {
            let task = match self.task_mut(task_id) {
                Some(t) => t,
                None => return Ok(false),
            };
            if !["running", "ready", "blocked", "review"].contains(&task.status.as_str()) {
                return Ok(false);
            }
            if expected_run_id.is_some() && task.current_run_id != expected_run_id {
                return Ok(false);
            }
            task.status = "done".into();
            task.result = result.map(str::to_string);
            task.completed_at = Some(now());
            task.claim_lock = None;
            task.claim_expires = None;
            task.worker_pid = None;
            task.block_kind = None;
            task.block_recurrences = 0;
        }
        // Stage declared artifacts into the durable attachments dir
        // (Python `_stage_completion_artifacts`): a missing/non-regular
        // declared artifact raises ArtifactPreservationError and the
        // transition rolls back — the task is untouched and retryable.
        if let Err(err) = self.stage_artifacts(task_id, metadata, "kanban_complete") {
            if let KbError::Artifact(problem) = &err {
                if let Ok(restored) = serde_json::from_slice::<BoardFile>(&snapshot) {
                    self.file = restored;
                }
                return Err(KbError::Artifact(problem.clone()));
            }
            return Err(err);
        }
        let run_id = self.end_or_synthesize_run(
            task_id,
            "completed",
            "done",
            handoff_summary,
            metadata,
            summary.is_some() || metadata.is_some() || result.is_some(),
            None,
        );
        let event_summary = first_line(handoff_summary, 400);
        let payload = {
            let mut payload = json!({
                "result_len": result.map(str::len).unwrap_or(0),
                "summary": if event_summary.is_empty() { Value::Null } else { Value::String(event_summary) },
            });
            if !verified_cards.is_empty() {
                payload["verified_cards"] = json!(verified_cards);
            }
            if let Some(cleaned) = metadata.and_then(|m| cleaned_artifact_paths(m)) {
                if !cleaned.is_empty() {
                    payload["artifacts"] = json!(cleaned);
                }
            }
            payload
        };
        self.append_event(task_id, "completed", Some(payload), run_id);
        if let Some(task) = self.task_mut(task_id) {
            task.consecutive_failures = 0;
        }
        self.recompute_ready();
        Ok(true)
    }

    /// Python `_verify_created_cards` + `_gate_created_cards`.
    fn verify_created_cards(
        &self,
        task_id: &str,
        created_cards: &[String],
    ) -> Result<Vec<String>, KbError> {
        let ordered: Vec<String> = {
            let mut out = Vec::new();
            for c in created_cards {
                let c = c.trim();
                if !c.is_empty() && !out.iter().any(|x| x == c) {
                    out.push(c.to_string());
                }
            }
            out
        };
        if ordered.is_empty() {
            return Ok(Vec::new());
        }
        let completing_assignee = self.get_task(task_id).and_then(|t| t.assignee.clone());
        let children = self.child_ids(task_id);
        let linked_children: HashSet<&str> = children.iter().map(String::as_str).collect();
        let mut verified = Vec::new();
        let mut phantom = Vec::new();
        for cid in ordered {
            let found = self.get_task(&cid);
            let created_by = found.and_then(|t| t.created_by.clone());
            let trusted = created_by.is_some()
                && (completing_assignee
                    .as_ref()
                    .map(|a| created_by.as_deref() == Some(a.as_str()))
                    .unwrap_or(false)
                    || created_by.as_deref() == Some(task_id)
                    || linked_children.contains(cid.as_str()));
            if trusted {
                verified.push(cid);
            } else {
                phantom.push(cid);
            }
        }
        if !phantom.is_empty() {
            return Err(KbError::Hallucinated(phantom));
        }
        Ok(verified)
    }

    /// Python `_gate_empty_completion`.
    fn gate_empty_completion(
        &self,
        task_id: &str,
        result: Option<&str>,
        summary: Option<&str>,
    ) -> Result<(), KbError> {
        if let Some(task) = self.get_task(task_id) {
            if task.status == "review" {
                return Ok(());
            }
            let stored = task.result.as_deref();
            if substantive_text(result) || substantive_text(summary) || substantive_text(stored) {
                return Ok(());
            }
            return Err(KbError::EmptyCompletion(task_id.to_string()));
        }
        Ok(())
    }

    /// Python `_stage_completion_artifacts` — copy each declared, existing
    /// artifact into the task's durable attachments dir with an attachment row.
    fn stage_artifacts(
        &mut self,
        task_id: &str,
        metadata: Option<&Value>,
        uploaded_by: &str,
    ) -> Result<(), KbError> {
        let paths = metadata.and_then(|m| cleaned_artifact_paths(m)).unwrap_or_default();
        if paths.is_empty() {
            return Ok(());
        }
        for path in paths {
            let src = PathBuf::from(&path);
            if !src.is_file() {
                return Err(KbError::Artifact(format!(
                    "declared scratch artifact is unavailable or not a regular file: {path}"
                )));
            }
            let size = std::fs::metadata(&src).map(|m| m.len() as i64).unwrap_or(0);
            if size > KANBAN_ATTACHMENT_MAX_BYTES as i64 {
                return Err(KbError::Artifact(format!(
                    "declared scratch artifact exceeds the {KANBAN_ATTACHMENT_MAX_BYTES}-byte limit: {path}"
                )));
            }
            let bytes = std::fs::read(&src)
                .map_err(|e| KbError::Artifact(format!("could not read declared artifact {path}: {e}")))?;
            self.store_attachment_bytes(task_id, &src_file_name(&path), bytes.as_slice(), None, Some(uploaded_by))?;
        }
        Ok(())
    }

    /// Python `block_task`. Ok(true) on any transition.
    pub fn block_task(
        &mut self,
        task_id: &str,
        reason: Option<&str>,
        kind: Option<&str>,
        expected_run_id: Option<i64>,
    ) -> Result<bool, KbError> {
        if let Some(kind) = kind {
            if !VALID_BLOCK_KINDS.contains(&kind) {
                return Err(KbError::Value(format!(
                    "block kind must be one of ['capability', 'dependency', 'needs_input', 'transient'] or None"
                )));
            }
        }
        let cur = match self.get_task(task_id) {
            Some(t) => (t.status.clone(), t.block_kind.clone(), t.block_recurrences),
            None => return Ok(false),
        };
        let (status, prev_kind, prev_recurrences) = cur;
        // Already blocked: classify in place only when untyped and unowned.
        if status == "blocked" {
            if kind.is_none() || expected_run_id.is_some() || prev_kind.is_some() {
                return Ok(false);
            }
            {
                let task = self.task_mut(task_id).unwrap();
                task.block_kind = kind.map(str::to_string);
                task.block_recurrences = 1;
            }
            self.append_event(
                task_id,
                "blocked",
                Some(json!({ "kind": kind, "reason": reason, "classified_in_place": true })),
                None,
            );
            return Ok(true);
        }
        let requested_kind = kind;
        let mut rekind_reason: Option<&str> = None;
        let mut kind = kind;
        if kind == Some("dependency") && self.parents_satisfied(task_id) {
            kind = Some("needs_input");
            rekind_reason = Some("no_open_parent");
        }
        // Python `_route_block`.
        let (new_status, event_kind, block_kind, recurrences) = if kind == Some("dependency") {
            ("todo", "dependency_wait", kind.map(str::to_string), prev_recurrences)
        } else {
            let recurrences = if prev_kind.as_deref() == kind { prev_recurrences + 1 } else { 1 };
            if recurrences >= BLOCK_RECURRENCE_LIMIT {
                ("triage", "block_loop_detected", kind.map(str::to_string), recurrences)
            } else {
                ("blocked", "blocked", kind.map(str::to_string), recurrences)
            }
        };
        {
            let task = match self.task_mut(task_id) {
                Some(t) => t,
                None => return Ok(false),
            };
            if !["running", "ready"].contains(&task.status.as_str()) {
                return Ok(false);
            }
            if expected_run_id.is_some() && task.current_run_id != expected_run_id {
                return Ok(false);
            }
            task.status = new_status.into();
            task.claim_lock = None;
            task.claim_expires = None;
            task.worker_pid = None;
            task.block_kind = block_kind.clone();
            task.block_recurrences = recurrences;
        }
        let run_id = self.end_or_synthesize_run(
            task_id,
            "blocked",
            "blocked",
            reason,
            None,
            reason.is_some(),
            None,
        );
        let mut payload = json!({
            "reason": reason,
            "kind": kind,
            "source_status": status,
        });
        if new_status != "dependency_wait" && new_status != "todo" {
            payload["recurrences"] = recurrences.into();
        }
        if new_status == "triage" {
            payload["limit"] = BLOCK_RECURRENCE_LIMIT.into();
        }
        if let Some(reason) = rekind_reason {
            payload["requested_kind"] = requested_kind.into();
            payload["rekind_reason"] = reason.into();
        }
        self.append_event(task_id, event_kind, Some(payload), run_id);
        Ok(true)
    }

    /// Python `schedule_task`.
    pub fn schedule_task(
        &mut self,
        task_id: &str,
        reason: Option<&str>,
        expected_run_id: Option<i64>,
    ) -> Result<bool, KbError> {
        {
            let task = match self.task_mut(task_id) {
                Some(t) => t,
                None => return Ok(false),
            };
            if !["todo", "ready", "running", "blocked"].contains(&task.status.as_str()) {
                return Ok(false);
            }
            if expected_run_id.is_some() && task.current_run_id != expected_run_id {
                return Ok(false);
            }
            task.status = "scheduled".into();
            task.claim_lock = None;
            task.claim_expires = None;
            task.worker_pid = None;
        }
        let run_id = self.end_or_synthesize_run(
            task_id,
            "scheduled",
            "scheduled",
            reason,
            None,
            reason.is_some(),
            None,
        );
        self.append_event(task_id, "scheduled", Some(json!({ "reason": reason })), run_id);
        Ok(true)
    }

    /// Python `request_review(with_reason=True)`. Ok((ok, fail_reason)).
    pub fn request_review(
        &mut self,
        task_id: &str,
        summary: Option<&str>,
        metadata: Option<&Value>,
        reviewer: Option<&str>,
        expected_run_id: Option<i64>,
    ) -> Result<(bool, Option<String>), KbError> {
        if !self.parents_satisfied(task_id) {
            return Ok((false, Some("parent dependencies are not satisfied".into())));
        }
        let (_status, assignee, current_run_id) = match self.get_task(task_id) {
            Some(t) => (t.status.clone(), t.assignee.clone(), t.current_run_id),
            None => return Ok((false, Some("task not found".into()))),
        };
        let live = self.get_task(task_id).map(|t| {
            t.status == "running" && t.claim_lock.is_some() && t.worker_pid.is_some()
        });
        if expected_run_id.is_none() && live == Some(true) {
            return Ok((
                false,
                Some("task is running under a live claim; pass expected_run_id (worker ownership) or force=True (explicit operator override) instead of clearing the live run's claim".into()),
            ));
        }
        // Implementer provenance: the profile on the run that did the work.
        let implementer = current_run_id
            .and_then(|rid| self.file.runs.iter().find(|r| r.id == rid))
            .and_then(|r| r.profile.clone())
            .or_else(|| {
                if assignee != reviewer.map(str::to_string) {
                    assignee.clone()
                } else {
                    None
                }
            });
        // Snapshot for the artifact-rollback contract (Python: the transition
        // rolls back inside the txn when staging fails).
        let snapshot = serde_json::to_vec(&self.file).unwrap_or_default();
        {
            let task = match self.task_mut(task_id) {
                Some(t) => t,
                None => return Ok((false, Some("task not found".into()))),
            };
            if !["running", "ready"].contains(&task.status.as_str()) {
                return Ok((
                    false,
                    Some("task is not in running/ready (or expected_run_id did not match the current run)".into()),
                ));
            }
            if expected_run_id.is_some() && task.current_run_id != expected_run_id {
                return Ok((
                    false,
                    Some("task is not in running/ready (or expected_run_id did not match the current run)".into()),
                ));
            }
            task.status = "review".into();
            task.claim_lock = None;
            task.claim_expires = None;
            task.worker_pid = None;
            if reviewer.is_some() {
                task.assignee = reviewer.map(str::to_string);
            }
        }
        if let Err(err) = self.stage_artifacts(task_id, metadata, "kanban_request_review") {
            if let KbError::Artifact(problem) = err {
                // Python: ArtifactPreservationError propagates out of the txn
                // (rollback) to the tool handler's structured rejection.
                if let Ok(restored) = serde_json::from_slice::<BoardFile>(&snapshot) {
                    self.file = restored;
                }
                return Err(KbError::Artifact(problem));
            }
            return Err(err);
        }
        let run_id = self.end_or_synthesize_run(
            task_id,
            "review_requested",
            "review",
            summary,
            metadata,
            summary.is_some() || metadata.is_some(),
            implementer.as_deref(),
        );
        let mut payload = json!({
            "summary": first_line(summary, 400),
            "implementer": implementer,
            "reviewer": reviewer,
        });
        if let Some(staged) = metadata.and_then(|m| cleaned_artifact_paths(m)) {
            if !staged.is_empty() {
                payload["artifacts"] = json!(staged);
            }
        }
        self.append_event(task_id, "review_requested", Some(payload), run_id);
        Ok((true, None))
    }

    /// Python `request_changes`. Ok((ok, implementer | reason)).
    pub fn request_changes(
        &mut self,
        task_id: &str,
        reason: &str,
        expected_run_id: Option<i64>,
    ) -> Result<(bool, Option<String>), KbError> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Ok((false, Some("reason is required".into())));
        }
        let (status, assignee, current_run_id) = match self.get_task(task_id) {
            Some(t) => (t.status.clone(), t.assignee.clone(), t.current_run_id),
            None => return Ok((false, Some("task not found".into()))),
        };
        if status != "running" || current_run_id.is_none() {
            return Ok((false, Some("task is not in an active review run".into())));
        }
        if expected_run_id.is_some() && current_run_id != expected_run_id {
            return Ok((false, Some("run_id mismatch".into())));
        }
        let claimed_from_review = self
            .file
            .events
            .iter()
            .filter(|e| e.task_id == task_id && e.kind == "claimed" && e.run_id == current_run_id)
            .any(|e| {
                e.payload
                    .as_ref()
                    .and_then(|p| p.get("source_status"))
                    .and_then(Value::as_str)
                    == Some("review")
            });
        if !claimed_from_review {
            return Ok((false, Some("active run was not claimed from review".into())));
        }
        let implementer = self
            .latest_event(task_id, &["review_requested"])
            .and_then(|e| e.payload.as_ref())
            .and_then(|p| p.get("implementer"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(implementer) = implementer.filter(|i| !i.trim().is_empty()) else {
            return Ok((
                false,
                Some("review handoff has no valid implementer provenance".into()),
            ));
        };
        // Parent re-gate: ready when every parent is terminal else todo.
        let new_status = if self.parents_satisfied(task_id) { "ready" } else { "todo" };
        {
            let task = self.task_mut(task_id).unwrap();
            task.status = new_status.into();
            task.assignee = Some(implementer.clone());
            task.claim_lock = None;
            task.claim_expires = None;
            task.worker_pid = None;
            task.worker_started_at = None;
        }
        let run_id = self.end_run(task_id, "changes_requested", Some(new_status), Some(reason), None, None);
        self.append_event(
            task_id,
            "changes_requested",
            Some(json!({
                "reason": reason,
                "implementer": implementer,
                "reviewer": assignee,
                "status": new_status,
            })),
            run_id,
        );
        Ok((true, Some(implementer)))
    }

    /// Python `unblock_task`: blocked/scheduled -> resumable phase.
    pub fn unblock_task(&mut self, task_id: &str) -> Result<bool, KbError> {
        let status = match self.task_status(task_id) {
            Some(s) => s,
            None => return Ok(false),
        };
        let resume_status = if status == "blocked" {
            self.resume_status_from_events(task_id)
        } else {
            "ready".into()
        };
        let landing = if self.parents_satisfied(task_id) { "ready" } else { "todo" };
        let new_status = if landing == "ready" && resume_status == "review" {
            "review"
        } else {
            &landing
        }
        .to_string();
        {
            let task = match self.task_mut(task_id) {
                Some(t) => t,
                None => return Ok(false),
            };
            if !["blocked", "scheduled"].contains(&task.status.as_str()) {
                return Ok(false);
            }
            task.status = new_status.clone();
            task.current_run_id = None;
            task.consecutive_failures = 0;
            task.last_failure_error = None;
        }
        let payload = if new_status == "ready" && resume_status == "ready" {
            None
        } else {
            Some(json!({ "status": new_status, "resume_status": resume_status }))
        };
        self.append_event(task_id, "unblocked", payload, None);
        Ok(true)
    }

    // --- Comments / attachments / heartbeats ---

    /// Python `add_comment`. Returns the new comment id.
    pub fn add_comment(&mut self, task_id: &str, author: &str, body: &str) -> Result<i64, KbError> {
        if !matches!(self.task_status(task_id), Some(_)) {
            return Err(KbError::Value(format!("task {task_id} not found")));
        }
        self.file.counters.comment += 1;
        let id = self.file.counters.comment;
        self.file.comments.push(Comment {
            id,
            task_id: task_id.to_string(),
            author: Some(author.to_string()),
            body: body.to_string(),
            created_at: Some(now()),
        });
        Ok(id)
    }

    /// Python `store_attachment_bytes` + `add_attachment`: size cap, safe
    /// basename, blob under the task attachments dir, metadata row + event.
    pub fn store_attachment_bytes(
        &mut self,
        task_id: &str,
        filename: &str,
        data: &[u8],
        content_type: Option<&str>,
        uploaded_by: Option<&str>,
    ) -> Result<i64, KbError> {
        if data.len() > KANBAN_ATTACHMENT_MAX_BYTES {
            return Err(KbError::Value(format!(
                "attachment exceeds {} MB limit",
                KANBAN_ATTACHMENT_MAX_BYTES / (1024 * 1024)
            )));
        }
        if !matches!(self.task_status(task_id), Some(_)) {
            return Err(KbError::Value(format!("task {task_id} not found")));
        }
        let safe_name = safe_attachment_name(filename);
        let dest_dir = self.attachments_dir(task_id);
        std::fs::create_dir_all(&dest_dir)
            .map_err(|e| KbError::Value(format!("could not create attachments dir: {e}")))?;
        let dest_path = collision_free_path(&dest_dir, &safe_name);
        std::fs::write(&dest_path, data)
            .map_err(|e| KbError::Value(format!("could not write attachment: {e}")))?;
        self.file.counters.attachment += 1;
        let id = self.file.counters.attachment;
        self.file.attachments.push(Attachment {
            id,
            task_id: task_id.to_string(),
            filename: dest_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(safe_name),
            stored_path: dest_path.to_string_lossy().to_string(),
            content_type: content_type.map(str::to_string),
            size: data.len() as i64,
            uploaded_by: uploaded_by.map(str::to_string),
            created_at: Some(now()),
        });
        self.append_event(
            task_id,
            "attached",
            Some(json!({ "filename": filename.trim(), "size": data.len(), "by": uploaded_by })),
            None,
        );
        Ok(id)
    }

    /// Python `task_attachments_dir`.
    pub fn attachments_dir(&self, task_id: &str) -> PathBuf {
        self.path
            .parent()
            .unwrap_or(Path::new("."))
            .join("attachments")
            .join(task_id)
    }

    /// Python `heartbeat_claim` (kanban_db): extend the running claim.
    pub fn heartbeat_claim(&mut self, task_id: &str, ttl_seconds: Option<i64>, claimer: Option<&str>) -> bool {
        let expires = now() + ttl_seconds.unwrap_or(DEFAULT_CLAIM_TTL_SECONDS);
        let lock = claimer
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}:{}", hostname(), std::process::id()));
        let matched = self
            .task_mut(task_id)
            .map(|t| t.status == "running" && t.claim_lock == Some(lock.clone()))
            .unwrap_or(false);
        if !matched {
            return false;
        }
        let task = self.task_mut(task_id).unwrap();
        task.claim_expires = Some(expires);
        if let Some(run_id) = task.current_run_id {
            if let Some(run) = self.file.runs.iter_mut().find(|r| r.id == run_id) {
                run.claim_expires = Some(expires);
            }
        }
        true
    }

    /// Python `heartbeat_worker` (kanban_db_dispatch): liveness + event.
    pub fn heartbeat_worker(&mut self, task_id: &str, note: Option<&str>, expected_run_id: Option<i64>) -> bool {
        {
            let task = match self.task_mut(task_id) {
                Some(t) => t,
                None => return false,
            };
            if task.status != "running" {
                return false;
            }
            if expected_run_id.is_some() && task.current_run_id != expected_run_id {
                return false;
            }
            task.last_heartbeat_at = Some(now());
        }
        let run_id = expected_run_id.or_else(|| self.current_run_id(task_id));
        if let Some(run_id) = run_id {
            if let Some(run) = self.file.runs.iter_mut().find(|r| r.id == run_id) {
                run.last_heartbeat_at = Some(now());
            }
        }
        self.append_event(task_id, "heartbeat", note.map(|n| json!({ "note": n })), run_id);
        true
    }

    /// Python `build_worker_context` — compact rendering (the full
    /// kanban_db_context layout is a documented deferral).
    pub fn build_worker_context(&self, task_id: &str) -> Result<String, KbError> {
        let Some(task) = self.get_task(task_id) else {
            return Err(KbError::Value(format!("unknown task {task_id}")));
        };
        let mut lines = vec![format!("# Task {}: {}", task.id, task.title)];
        if let Some(assignee) = &task.assignee {
            lines.push(format!("Assignee: {assignee}"));
        }
        lines.push(format!("Status: {}", task.status));
        if let Some(body) = task.body.as_deref().filter(|b| !b.is_empty()) {
            lines.push(String::new());
            lines.push(body.to_string());
        }
        let done_parents: Vec<String> = self
            .parent_ids(task_id)
            .into_iter()
            .filter(|pid| {
                self.get_task(pid)
                    .map(|p| ["done", "archived"].contains(&p.status.as_str()))
                    .unwrap_or(false)
            })
            .collect();
        for pid in done_parents {
            if let Some(parent) = self.get_task(&pid) {
                lines.push(String::new());
                lines.push(format!("## Parent {} ({}) handoff", parent.id, parent.status));
                if let Some(result) = parent.result.as_deref() {
                    lines.push(result.to_string());
                }
            }
        }
        let comments = self.list_comments(task_id);
        if !comments.is_empty() {
            lines.push(String::new());
            lines.push("## Comments".into());
            for c in comments {
                lines.push(format!(
                    "**{}**: {}",
                    c.author.as_deref().unwrap_or("operator"),
                    c.body
                ));
            }
        }
        let mut out = lines.join("\n");
        while out.ends_with('\n') {
            out.pop();
        }
        out.push('\n');
        Ok(out)
    }
}

// --- Small helpers shared with the tool layer ---

fn as_null(v: &Value) -> bool {
    v.is_null()
}

fn substantive_text(value: Option<&str>) -> bool {
    value.map(|v| !v.trim().is_empty()).unwrap_or(false)
}

/// Python `_first_line`: first stripped line capped at `limit` chars.
pub fn first_line(text: Option<&str>, limit: usize) -> String {
    let stripped = text.unwrap_or("").trim();
    match stripped.lines().next() {
        Some(line) => line.chars().take(limit).collect(),
        None => String::new(),
    }
}

/// Python `_cleaned_artifact_paths`: non-blank string paths in metadata["artifacts"].
pub fn cleaned_artifact_paths(metadata: &Value) -> Option<Vec<String>> {
    metadata
        .get("artifacts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
}

/// Python `_safe_attachment_name`: keep the leaf component only.
fn safe_attachment_name(filename: &str) -> String {
    let name = Path::new(filename.trim())
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    name.replace(['\\', '/', ':', '\0'], "_")
}

fn src_file_name(path: &str) -> String {
    safe_attachment_name(path)
}

fn collision_free_path(dir: &Path, name: &str) -> PathBuf {
    let mut candidate = dir.join(&name);
    let mut i = 1;
    while candidate.exists() {
        let stem = Path::new(&name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let ext = Path::new(&name).extension().map(|e| e.to_string_lossy().to_string());
        candidate = dir.join(match &ext {
            Some(e) => format!("{stem}_{i}.{e}"),
            None => format!("{stem}_{i}"),
        });
        i += 1;
    }
    candidate
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into())
}
