//! SQLite storage backend for the Kanban board — port of the schema and
//! storage operations of the Python `hermes_cli/kanban_db.py` +
//! `hermes_cli/kanban_db_connect.py` (the previous board storage in this port
//! was an atomic-write JSON document; the Python contract is a SQLite DB, so
//! boards are now interchangeable).
//!
//! Schema: `kanban_db.SCHEMA_SQL` verbatim (tables `tasks`, `task_links`,
//! `task_comments`, `task_events`, `task_runs`, `task_attachments`,
//! `kanban_notify_subs` + their indexes), plus the additive-migration indexes
//! `kanban_db_connect._migrate_add_optional_columns` creates on every Python
//! connect (`idx_tasks_assignee_status`, `idx_tasks_tenant`,
//! `idx_tasks_idempotency`, `idx_tasks_session_id`, `idx_events_run`).
//!
//! Transactions: every save is one `BEGIN IMMEDIATE` … `COMMIT` — the Python
//! `write_txn` boundary — so a save either lands wholly or not at all, and a
//! concurrent writer is serialized the way SQLite serializes Python writers
//! (WAL + busy timeout). Column semantics (epoch INTEGERs, `skills` as a JSON
//! text array, `goal_mode` 0/1, JSON `payload`/`metadata` text or NULL)
//! follow the Python `_append_event` / `create_task` writers.

use crate::kanban_state::{Attachment, BoardFile, Comment, Counters, Event, Link, Run, Task};
use rusqlite::Connection;
use serde_json::Value as JsonValue;
use std::path::Path;

/// Python `kanban_db.SCHEMA_SQL` — verbatim, including its comments.
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id                   TEXT PRIMARY KEY,
    title                TEXT NOT NULL,
    body                 TEXT,
    assignee             TEXT,
    status               TEXT NOT NULL,
    priority             INTEGER DEFAULT 0,
    created_by           TEXT,
    created_at           INTEGER NOT NULL,
    started_at           INTEGER,
    completed_at         INTEGER,
    workspace_kind       TEXT NOT NULL DEFAULT 'scratch',
    workspace_path       TEXT,
    branch_name          TEXT,
    -- Optional link to a first-class Project (hermes_cli/projects_db). When set,
    -- the task's worktree is anchored under the project's primary repo with a
    -- deterministic branch name instead of a random wt/<task-id> fallback.
    project_id           TEXT,
    claim_lock           TEXT,
    claim_expires        INTEGER,
    tenant               TEXT,
    result               TEXT,
    idempotency_key      TEXT,
    -- Unified consecutive-failure counter. Incremented on spawn
    -- failure, timeout, or crash; reset only on successful completion.
    -- The circuit breaker in _record_task_failure trips when this
    -- exceeds DEFAULT_FAILURE_LIMIT consecutive non-successes.
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    worker_pid           INTEGER,
    -- Restart-stable fingerprint of worker_pid ("<boot/instantiation epoch>|<start time>",
    -- kanban_db_dispatch._process_fingerprint) recorded at spawn: liveness and kills require pid
    -- AND fingerprint to agree, so a PID recycled after a reboot is never read as our worker or
    -- signalled. NULL = legacy row (pre-fingerprint spawn); 'unverified' = capture failed at
    -- spawn (held while live, never signalled). Column keeps its INTEGER affinity for the
    -- start-time-only integer values older rows carry.
    worker_started_at    INTEGER,
    -- Short excerpt of the most recent failure's error text.
    last_failure_error   TEXT,
    max_runtime_seconds  INTEGER,
    last_heartbeat_at    INTEGER,
    -- Pointer into task_runs for the currently-active run (NULL if no
    -- run is in-flight). Denormalised for cheap reads.
    current_run_id       INTEGER,
    -- Forward-compat for v2 workflow routing. In v1 the kernel writes
    -- these when the task is opted into a template but otherwise ignores
    -- them; the dispatcher doesn't consult them for routing yet.
    workflow_template_id TEXT,
    current_step_key     TEXT,
    -- Force-loaded skills for the worker on this task, stored as JSON.
    -- Passed to the worker via `--skills`. NULL or empty array = no extras.
    skills               TEXT,
    -- Per-task model override. When set, the dispatcher passes -m <model>
    -- to the worker, overriding the profile's default model. NULL = use
    -- the profile default.
    model_override       TEXT,
    -- Provider the model override belongs to. When set (alongside
    -- model_override), the dispatcher passes --provider <name> so the
    -- worker resolves the model against the right backend instead of the
    -- profile's configured provider. NULL = profile provider.
    provider_override    TEXT,
    -- Per-task reasoning effort for the worker (minimal|low|medium|high|
    -- xhigh|max|ultra, or 'none' for thinking off). When set, the dispatcher
    -- passes --reasoning <level> so the worker runs at that depth regardless
    -- of the profile's agent.reasoning_effort. NULL = profile setting.
    reasoning_effort     TEXT,
    -- Per-task override for the consecutive-failure circuit breaker.
    -- The value is the failure count at which the breaker trips — e.g.
    -- ``max_retries=1`` blocks on the first failure. NULL (the common
    -- case) falls through to the dispatcher-level ``kanban.failure_limit``
    -- config and then ``DEFAULT_FAILURE_LIMIT``.
    max_retries          INTEGER,
    -- When 1, the dispatched worker runs in a Ralph-style goal loop: an
    -- auxiliary judge re-evaluates the worker's response against the card
    -- title/body after each turn and feeds a continuation prompt
    -- back into the SAME session until the judge agrees the work is done
    -- or ``goal_max_turns`` is exhausted. NULL/0 = classic single-shot
    -- worker (the default).
    goal_mode            INTEGER NOT NULL DEFAULT 0,
    -- Goal-loop turn budget for ``goal_mode`` workers. NULL = use the
    -- goals-engine default.
    goal_max_turns       INTEGER,
    -- Originating chat/agent session id when the task was created from
    -- inside an agent loop that propagated ``HERMES_SESSION_ID``. NULL
    -- for tasks created from the CLI, dashboard, or any path that doesn't
    -- set the env var, and for an id with no ``sessions`` row in this
    -- profile's state.db (kanban_create verifies before stamping). Indexed
    -- so per-session list queries stay cheap on larger boards.
    session_id           TEXT,
    -- Typed block reason set by ``block_task`` (one of VALID_BLOCK_KINDS, or
    -- NULL for legacy/un-typed blocks). Drives routing: ``dependency`` never
    -- sits in ``blocked`` (goes to ``todo`` for parent-gating); the others go
    -- to ``blocked`` for a human. Preserved across unblock so a re-block for
    -- the SAME kind can be recognised as a loop.
    block_kind           TEXT,
    -- Unblock-loop counter. Incremented each time a task is re-blocked for the
    -- same truly-blocked reason after having been unblocked. When it reaches
    -- BLOCK_RECURRENCE_LIMIT the task is routed to ``triage`` instead of
    -- ``blocked`` so a cron can't spin it forever. Reset to 0 only on a
    -- successful completion — NOT on unblock (resetting on unblock is exactly
    -- the amnesia that let the loop run unbounded).
    block_recurrences    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS task_links (
    parent_id  TEXT NOT NULL,
    child_id   TEXT NOT NULL,
    PRIMARY KEY (parent_id, child_id)
);

CREATE TABLE IF NOT EXISTS task_comments (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id    TEXT NOT NULL,
    author     TEXT NOT NULL,
    body       TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS task_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id    TEXT NOT NULL,
    run_id     INTEGER,
    kind       TEXT NOT NULL,
    payload    TEXT,
    created_at INTEGER NOT NULL
);

-- Historical attempt record. Each time the dispatcher claims a task, a
-- new row is created here; claim state, PID, heartbeat, runtime cap,
-- and structured summary all live on the run, not the task. Multiple
-- rows per task id when the task was retried after crash/timeout/block.
CREATE TABLE IF NOT EXISTS task_runs (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id             TEXT NOT NULL,
    profile             TEXT,
    step_key            TEXT,
    status              TEXT NOT NULL,
    -- status: running | done | blocked | crashed | timed_out | failed | released
    claim_lock          TEXT,
    claim_expires       INTEGER,
    worker_pid          INTEGER,
    -- Spawn-time start fingerprint of worker_pid (see tasks.worker_started_at).
    worker_started_at   INTEGER,
    max_runtime_seconds INTEGER,
    last_heartbeat_at   INTEGER,
    started_at          INTEGER NOT NULL,
    ended_at            INTEGER,
    outcome             TEXT,
    -- outcome: completed | blocked | crashed | timed_out | spawn_failed |
    --          gave_up | reclaimed | (null while still running)
    summary             TEXT,
    metadata            TEXT,
    error               TEXT
);

-- Files attached to a task (PDFs, images, source documents). The blob
-- lives on disk under ``attachments_root(board)/<task_id>/<stored_name>``;
-- this row carries metadata + the absolute ``stored_path``.
CREATE TABLE IF NOT EXISTS task_attachments (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id      TEXT NOT NULL,
    filename     TEXT NOT NULL,
    stored_path  TEXT NOT NULL,
    content_type TEXT,
    size         INTEGER NOT NULL DEFAULT 0,
    uploaded_by  TEXT,
    created_at   INTEGER NOT NULL
);

-- Subscription from a gateway source (platform + chat + thread) to a
-- task. The gateway's kanban-notifier watcher tails task_events.
CREATE TABLE IF NOT EXISTS kanban_notify_subs (
    task_id       TEXT NOT NULL,
    platform      TEXT NOT NULL,
    chat_id       TEXT NOT NULL,
    thread_id     TEXT NOT NULL DEFAULT '',
    user_id       TEXT,
    user_id_alt   TEXT,
    chat_type     TEXT,
    notifier_profile TEXT,
    delivery_mode TEXT NOT NULL DEFAULT 'notify',
    delivery_metadata TEXT,
    created_at    INTEGER NOT NULL,
    last_event_id INTEGER NOT NULL DEFAULT 0,
    last_ping_event_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (task_id, platform, chat_id, thread_id)
);

CREATE INDEX IF NOT EXISTS idx_tasks_status          ON tasks(status);
CREATE INDEX IF NOT EXISTS idx_links_child           ON task_links(child_id);
CREATE INDEX IF NOT EXISTS idx_links_parent          ON task_links(parent_id);
CREATE INDEX IF NOT EXISTS idx_comments_task         ON task_comments(task_id, created_at);
CREATE INDEX IF NOT EXISTS idx_events_task           ON task_events(task_id, created_at);
CREATE INDEX IF NOT EXISTS idx_runs_task             ON task_runs(task_id, started_at);
CREATE INDEX IF NOT EXISTS idx_runs_status           ON task_runs(status);
CREATE INDEX IF NOT EXISTS idx_attachments_task      ON task_attachments(task_id, created_at);
CREATE INDEX IF NOT EXISTS idx_notify_task           ON kanban_notify_subs(task_id);

-- Additive-migration indexes (Python `kanban_db_connect.py`
-- `_migrate_add_optional_columns`): created by every real Python connect.
CREATE INDEX IF NOT EXISTS idx_tasks_assignee_status ON tasks(assignee, status);
CREATE INDEX IF NOT EXISTS idx_tasks_tenant          ON tasks(tenant);
CREATE INDEX IF NOT EXISTS idx_tasks_idempotency     ON tasks(idempotency_key);
CREATE INDEX IF NOT EXISTS idx_tasks_session_id      ON tasks(session_id);
CREATE INDEX IF NOT EXISTS idx_events_run            ON task_events(run_id, id);
"#;

pub fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
}

/// Python `kanban_db_connect._sqlite_connect` + `_open_configured` subset:
/// open with a long busy timeout, WAL (with silent fallback on filesystems
/// that cannot support it), FULL synchronous, FK enforcement.
pub fn open_connection(path: &Path) -> std::io::Result<Connection> {
    let conn = Connection::open(path).map_err(io_err)?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(120_000)); // Python DEFAULT_BUSY_TIMEOUT_MS
    let _ = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0));
    let _ = conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;");
    Ok(conn)
}

/// Idempotent schema init (Python `executescript(SCHEMA_SQL)` +
/// `_migrate_add_optional_columns` — every CREATE here is `IF NOT EXISTS`,
/// and the additive `ALTER TABLE` pass adds the columns the v1 CREATE TABLE
/// predates; Python-created boards carry them via the same migration).
pub fn init_schema(conn: &Connection) -> std::io::Result<()> {
    conn.execute_batch(SCHEMA_SQL).map_err(io_err)?;
    // Python `_LATER_TASK_COLUMNS`: the only v1-CREATE column the migration
    // still adds (everything else is already in SCHEMA_SQL).
    let tasks_cols: Vec<String> = {
        let mut stmt = conn
            .prepare("PRAGMA table_info(tasks)")
            .map_err(io_err)?;
        let mut rows = stmt.query([]).map_err(io_err)?;
        let mut names = Vec::new();
        while let Some(row) = rows.next().map_err(io_err)? {
            let name: String = row.get(1).map_err(|e| io_err(e))?;
            names.push(name);
        }
        names
    };
    if !tasks_cols.iter().any(|c| c == "completion_contract") {
        conn.execute("ALTER TABLE tasks ADD COLUMN completion_contract TEXT", [])
            .map_err(io_err)?;
    }
    Ok(())
}

fn json_text(value: &Option<JsonValue>) -> Option<String> {
    // Python `_json_or_null`: JSON text for a payload/metadata column; falsy -> NULL.
    match value {
        Some(v) if !v.is_null() => Some(v.to_string()),
        _ => None,
    }
}

fn json_from_row(value: Option<String>) -> Option<JsonValue> {
    // Python `_json_or`: any decode failure or empty value yields None.
    let text = value?;
    if text.is_empty() {
        return None;
    }
    serde_json::from_str(&text).ok()
}

fn opt_string(row: &rusqlite::Row, col: &str) -> rusqlite::Result<Option<String>> {
    row.get(col)
}

fn opt_i64(row: &rusqlite::Row, col: &str) -> rusqlite::Result<Option<i64>> {
    row.get(col)
}

fn bool_i64(row: &rusqlite::Row, col: &str) -> rusqlite::Result<bool> {
    Ok(row.get::<_, Option<i64>>(col)?.unwrap_or(0) != 0)
}

fn task_from_row(row: &rusqlite::Row) -> rusqlite::Result<Task> {
    let skills: Option<String> = row.get("skills")?;
    let skills: Option<Vec<String>> = match json_from_row(skills) {
        Some(JsonValue::Array(items)) => Some(
            items
                .iter()
                .filter_map(JsonValue::as_str)
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    };
    Ok(Task {
        id: row.get("id")?,
        title: row.get("title")?,
        body: opt_string(row, "body")?,
        assignee: opt_string(row, "assignee")?,
        status: row.get("status")?,
        tenant: opt_string(row, "tenant")?,
        priority: row.get::<_, Option<i64>>("priority")?.unwrap_or(0),
        workspace_kind: opt_string(row, "workspace_kind")?,
        workspace_path: opt_string(row, "workspace_path")?,
        branch_name: opt_string(row, "branch_name")?,
        project_id: opt_string(row, "project_id")?,
        created_by: opt_string(row, "created_by")?,
        created_at: opt_i64(row, "created_at")?,
        started_at: opt_i64(row, "started_at")?,
        completed_at: opt_i64(row, "completed_at")?,
        result: opt_string(row, "result")?,
        current_run_id: opt_i64(row, "current_run_id")?,
        model_override: opt_string(row, "model_override")?,
        provider_override: opt_string(row, "provider_override")?,
        completion_contract: opt_string(row, "completion_contract")?,
        last_failure_error: opt_string(row, "last_failure_error")?,
        block_kind: opt_string(row, "block_kind")?,
        block_recurrences: row.get::<_, Option<i64>>("block_recurrences")?.unwrap_or(0),
        consecutive_failures: row.get::<_, Option<i64>>("consecutive_failures")?.unwrap_or(0),
        goal_mode: bool_i64(row, "goal_mode")?,
        goal_max_turns: opt_i64(row, "goal_max_turns")?,
        idempotency_key: opt_string(row, "idempotency_key")?,
        skills,
        max_runtime_seconds: opt_i64(row, "max_runtime_seconds")?,
        session_id: opt_string(row, "session_id")?,
        claim_lock: opt_string(row, "claim_lock")?,
        claim_expires: opt_i64(row, "claim_expires")?,
        worker_pid: opt_i64(row, "worker_pid")?,
        worker_started_at: opt_i64(row, "worker_started_at")?,
        last_heartbeat_at: opt_i64(row, "last_heartbeat_at")?,
        workflow_template_id: opt_string(row, "workflow_template_id")?,
        current_step_key: opt_string(row, "current_step_key")?,
        reasoning_effort: opt_string(row, "reasoning_effort")?,
        max_retries: opt_i64(row, "max_retries")?,
    })
}

fn run_from_row(row: &rusqlite::Row) -> rusqlite::Result<Run> {
    let metadata: Option<String> = row.get("metadata")?;
    Ok(Run {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        profile: opt_string(row, "profile")?,
        step_key: opt_string(row, "step_key")?,
        status: row.get("status")?,
        outcome: opt_string(row, "outcome")?,
        summary: opt_string(row, "summary")?,
        error: opt_string(row, "error")?,
        metadata: json_from_row(metadata),
        started_at: opt_i64(row, "started_at")?,
        ended_at: opt_i64(row, "ended_at")?,
        claim_expires: opt_i64(row, "claim_expires")?,
        last_heartbeat_at: opt_i64(row, "last_heartbeat_at")?,
        claim_lock: opt_string(row, "claim_lock")?,
        worker_pid: opt_i64(row, "worker_pid")?,
        worker_started_at: opt_i64(row, "worker_started_at")?,
        max_runtime_seconds: opt_i64(row, "max_runtime_seconds")?,
    })
}

fn comment_from_row(row: &rusqlite::Row) -> rusqlite::Result<Comment> {
    Ok(Comment {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        author: opt_string(row, "author")?,
        body: row.get("body")?,
        created_at: opt_i64(row, "created_at")?,
    })
}

fn event_from_row(row: &rusqlite::Row) -> rusqlite::Result<Event> {
    let payload: Option<String> = row.get("payload")?;
    Ok(Event {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        run_id: opt_i64(row, "run_id")?,
        kind: row.get("kind")?,
        payload: json_from_row(payload),
        created_at: opt_i64(row, "created_at")?,
    })
}

fn attachment_from_row(row: &rusqlite::Row) -> rusqlite::Result<Attachment> {
    Ok(Attachment {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        filename: row.get("filename")?,
        stored_path: row.get("stored_path")?,
        content_type: opt_string(row, "content_type")?,
        size: row.get::<_, Option<i64>>("size")?.unwrap_or(0),
        uploaded_by: opt_string(row, "uploaded_by")?,
        created_at: opt_i64(row, "created_at")?,
    })
}

fn rows_of<T, F>(conn: &Connection, sql: &str, mut map: F) -> std::io::Result<Vec<T>>
where
    F: FnMut(&rusqlite::Row) -> rusqlite::Result<T>,
{
    let mut stmt = conn.prepare(sql).map_err(io_err)?;
    let mut rows = stmt.query([]).map_err(io_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(io_err)? {
        out.push(map(row).map_err(io_err)?);
    }
    Ok(out)
}

fn max_id(conn: &Connection, table: &str) -> std::io::Result<i64> {
    let sql = format!("SELECT COALESCE(MAX(id), 0) FROM {table}");
    conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map_err(io_err)
}

/// Load the whole board from the SQLite tables into the in-memory document.
pub fn load_board_file(conn: &Connection) -> std::io::Result<BoardFile> {
    let tasks = rows_of(conn, "SELECT * FROM tasks ORDER BY rowid", task_from_row)?;
    let links = rows_of(
        conn,
        "SELECT parent_id, child_id FROM task_links ORDER BY rowid",
        |row| {
            Ok(Link {
                parent_id: row.get("parent_id")?,
                child_id: row.get("child_id")?,
            })
        },
    )?;
    let runs = rows_of(conn, "SELECT * FROM task_runs ORDER BY id", run_from_row)?;
    let comments = rows_of(conn, "SELECT * FROM task_comments ORDER BY id", comment_from_row)?;
    let events = rows_of(conn, "SELECT * FROM task_events ORDER BY id", event_from_row)?;
    let attachments = rows_of(
        conn,
        "SELECT * FROM task_attachments ORDER BY id",
        attachment_from_row,
    )?;
    // New ids must never collide with an existing (or ever-used) row id:
    // Python's AUTOINCREMENT guarantees monotonic ids; mirror it from MAX(id).
    let counters = Counters {
        run: max_id(conn, "task_runs")?,
        comment: max_id(conn, "task_comments")?,
        event: max_id(conn, "task_events")?,
        attachment: max_id(conn, "task_attachments")?,
    };
    Ok(BoardFile { tasks, links, runs, comments, events, attachments, counters, schema: 1 })
}

const INSERT_TASK: &str = "INSERT INTO tasks (
    id, title, body, assignee, status, priority, created_by, created_at,
    started_at, completed_at, workspace_kind, workspace_path, branch_name,
    project_id, claim_lock, claim_expires, tenant, result, idempotency_key,
    consecutive_failures, worker_pid, worker_started_at, last_failure_error,
    max_runtime_seconds, last_heartbeat_at, current_run_id,
    workflow_template_id, current_step_key, skills, model_override,
    provider_override, reasoning_effort, max_retries, goal_mode,
    goal_max_turns, session_id, block_kind, block_recurrences,
    completion_contract
) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)";

const INSERT_RUN: &str = "INSERT INTO task_runs (
    id, task_id, profile, step_key, status, claim_lock, claim_expires,
    worker_pid, worker_started_at, max_runtime_seconds, last_heartbeat_at,
    started_at, ended_at, outcome, summary, metadata, error
) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)";

const INSERT_EVENT: &str =
    "INSERT INTO task_events (id, task_id, run_id, kind, payload, created_at) VALUES (?,?,?,?,?,?)";
const INSERT_COMMENT: &str =
    "INSERT INTO task_comments (id, task_id, author, body, created_at) VALUES (?,?,?,?,?)";
const INSERT_ATTACHMENT: &str =
    "INSERT INTO task_attachments (id, task_id, filename, stored_path, content_type, size, uploaded_by, created_at) VALUES (?,?,?,?,?,?,?,?)";
const INSERT_LINK: &str =
    "INSERT INTO task_links (parent_id, child_id) VALUES (?,?)";

/// Replace the stored board with `file` inside one transaction. The caller
/// owns the `BEGIN IMMEDIATE` boundary; a failure must be rolled back by it.
pub fn write_board_file(conn: &Connection, file: &BoardFile) -> std::io::Result<()> {
    for table in [
        "task_links",
        "task_comments",
        "task_events",
        "task_runs",
        "task_attachments",
        "tasks",
    ] {
        conn.execute(&format!("DELETE FROM {table}"), []).map_err(io_err)?;
    }
    for task in &file.tasks {
        conn.execute(
            INSERT_TASK,
            rusqlite::params![
                task.id,
                task.title,
                task.body,
                task.assignee,
                task.status,
                task.priority,
                task.created_by,
                // NOT NULL columns the in-memory documents may leave unset:
                // Python readers coerce NULL epoch/stamp cells the same way
                // (`_to_epoch(..) or 0`, workspace_kind DEFAULT 'scratch').
                task.created_at.unwrap_or(0),
                task.started_at,
                task.completed_at,
                Some(task.workspace_kind.clone().unwrap_or_else(|| "scratch".into())),
                task.workspace_path,
                task.branch_name,
                task.project_id,
                task.claim_lock,
                task.claim_expires,
                task.tenant,
                task.result,
                task.idempotency_key,
                task.consecutive_failures,
                task.worker_pid,
                task.worker_started_at,
                task.last_failure_error,
                task.max_runtime_seconds,
                task.last_heartbeat_at,
                task.current_run_id,
                task.workflow_template_id,
                task.current_step_key,
                task.skills.as_ref().map(|s| serde_json::to_string(s).unwrap_or_default()),
                task.model_override,
                task.provider_override,
                task.reasoning_effort,
                task.max_retries,
                if task.goal_mode { 1 } else { 0 },
                task.goal_max_turns,
                task.session_id,
                task.block_kind,
                task.block_recurrences,
                task.completion_contract,
            ],
        )
        .map_err(io_err)?;
    }
    for link in &file.links {
        conn.execute(INSERT_LINK, rusqlite::params![link.parent_id, link.child_id])
            .map_err(io_err)?;
    }
    for run in &file.runs {
        conn.execute(
            INSERT_RUN,
            rusqlite::params![
                run.id,
                run.task_id,
                run.profile,
                run.step_key,
                run.status,
                run.claim_lock,
                run.claim_expires,
                run.worker_pid,
                run.worker_started_at,
                run.max_runtime_seconds,
                run.last_heartbeat_at,
                run.started_at.unwrap_or(0),
                run.ended_at,
                run.outcome,
                run.summary,
                json_text(&run.metadata),
                run.error,
            ],
        )
        .map_err(io_err)?;
    }
    for comment in &file.comments {
        conn.execute(
            INSERT_COMMENT,
            rusqlite::params![
                comment.id,
                comment.task_id,
                Some(comment.author.clone().unwrap_or_default()),
                comment.body,
                comment.created_at.unwrap_or(0),
            ],
        )
        .map_err(io_err)?;
    }
    for event in &file.events {
        conn.execute(
            INSERT_EVENT,
            rusqlite::params![
                event.id,
                event.task_id,
                event.run_id,
                event.kind,
                json_text(&event.payload),
                event.created_at,
            ],
        )
        .map_err(io_err)?;
    }
    for att in &file.attachments {
        conn.execute(
            INSERT_ATTACHMENT,
            rusqlite::params![
                att.id,
                att.task_id,
                att.filename,
                att.stored_path,
                att.content_type,
                att.size,
                att.uploaded_by,
                att.created_at,
            ],
        )
        .map_err(io_err)?;
    }
    // kanban_notify_subs rows are gateway-channel data this tool surface never
    // writes (Python sets `subscribed` only through the notifier); any rows a
    // Python-created board carries are preserved untouched on save.
    Ok(())
}
