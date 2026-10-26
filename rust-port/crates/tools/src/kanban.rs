//! Kanban tools — port of `tools/kanban_tools.py` + `tools/kanban_tools_schemas.py`
//! from the Python hermes-agent: the 15 `kanban_*` tools with byte-frozen
//! schemas, the mode gating (`_check_kanban_mode` / `_check_kanban_orchestrator_mode`),
//! the worker-ownership guard chain, and the `_kanban_handler` error envelope.
//!
//! GATING (Python contract): the tools are registered only when the profile
//! enables the `kanban` toolset or the process is a dispatcher-spawned worker
//! (`HERMES_KANBAN_TASK` set). The Python registry gates via `check_fn`
//! visibility; this registry is static, so `kanban_toolset_enabled` /
//! `kanban_mode_visible` / `kanban_orchestrator_mode_visible` are exported and
//! TESTED here, and the orchestrator-only tools additionally reject workers at
//! dispatch time (`_require_orchestrator_tool`). Toolset enablement is pinned
//! with `HERMES_KANBAN_TOOLSET` (the Rust port has no profile config layer).

use serde_json::{json, Value};
use std::collections::BTreeSet;

use crate::kanban_state;
use crate::kanban_state::{
    is_valid_status, Board, KbError, ARCHIVED, KANBAN_ATTACHMENT_MAX_BYTES, VALID_BLOCK_KINDS,
};
use crate::util::tool_error;

/// Python `KANBAN_LIST_DEFAULT_LIMIT` / `KANBAN_LIST_MAX_LIMIT`.
const LIST_DEFAULT_LIMIT: i64 = 50;
const LIST_MAX_LIMIT: i64 = 200;

/// Python `_RUN_LIFECYCLE_TOOLS`: worker tools that terminate a run's ownership.
const RUN_LIFECYCLE_TOOLS: [&str; 5] = [
    "kanban_complete",
    "kanban_block",
    "kanban_schedule",
    "kanban_request_review",
    "kanban_request_changes",
];

/// Python `_ORCHESTRATOR_TOOLS`: board-routing tools hidden from task workers.
/// The dispatch arm uses this to apply the orchestrator-mode gate.
pub const ORCHESTRATOR_TOOLS: [&str; 2] = ["kanban_list", "kanban_unblock"];

/// Registration order = the order tools appear in the schema (Python `_TOOLS`).
pub const KANBAN_TOOL_NAMES: [&str; 15] = [
    "kanban_show",
    "kanban_list",
    "kanban_complete",
    "kanban_block",
    "kanban_schedule",
    "kanban_request_review",
    "kanban_request_changes",
    "kanban_heartbeat",
    "kanban_comment",
    "kanban_attach",
    "kanban_attach_url",
    "kanban_attachments",
    "kanban_create",
    "kanban_unblock",
    "kanban_link",
];

// --- Redaction (Python `agent.redact.redact_sensitive_text`) ---
// Subset of the Python prefix patterns: the well-known credential token
// shapes. (The full vendor list is a documented deferral.)

const SECRET_PATTERNS: &[&str] = &[
    r"sk-[A-Za-z0-9_\-]{20,}",
    r"ghp_[A-Za-z0-9_]{20,}",
    r"ghs_[A-Za-z0-9_]{20,}",
    r"github_pat_[A-Za-z0-9_]{20,}",
    r"glpat-[A-Za-z0-9_\-]{16,}",
    r"xoxb-[A-Za-z0-9_\-]{16,}",
    r"xoxp-[A-Za-z0-9_\-]{16,}",
    r"AKIA[0-9A-Z]{16}",
    r"Bearer [A-Za-z0-9._\-]{16,}",
    r"eyJhb[A-Za-z0-9._\-]{16,}",
];

/// Python `redact_sensitive_text(.., force=True)`: mask credential-shaped
/// tokens. Best-effort port (prefix subset).
pub fn redact_sensitive_text(text: &str) -> String {
    let mut out = text.to_string();
    for pattern in SECRET_PATTERNS {
        if let Ok(re) = regex::Regex::new(pattern) {
            out = re
                .replace_all(&out, "[REDACTED]")
                .to_string();
        }
    }
    out
}

fn redact(value: &Value) -> String {
    redact_sensitive_text(&value_to_str(value))
}

fn redact_opt(value: &Value) -> Value {
    if value.is_null() || value.as_bool() == Some(false) {
        return Value::Null;
    }
    if let Some(s) = value.as_str() {
        return json!(redact_sensitive_text(s));
    }
    value.clone()
}

/// Python `_redact_metadata`: JSON round-trip redaction.
fn redact_metadata(metadata: &Value) -> Option<Value> {
    serde_json::from_str(&redact_sensitive_text(&metadata.to_string())).ok()
}

/// Python type names for `type(value).__name__` in error strings.
fn python_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.is_f64() {
                "float"
            } else {
                "int"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn value_to_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// --- Gating (Python `_profile_has_kanban_toolset` / `_visible`) ---

fn is_delegated_child() -> bool {
    std::env::var("HERMES_DELEGATED_CHILD_CONTEXT")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Python `_is_dispatcher_owned_worker` (default True when no delegation
/// context is available): false only for delegate children here.
fn is_dispatcher_owned_worker() -> bool {
    !is_delegated_child()
}

/// Python `_profile_has_kanban_toolset`: the Rust port pins toolset
/// enablement with `HERMES_KANBAN_TOOLSET` (Python reads the profile's
/// `toolsets`/`platform_toolsets` config); legacy env words accepted.
pub fn kanban_toolset_enabled() -> bool {
    match std::env::var("HERMES_KANBAN_TOOLSET") {
        Ok(v) => matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes"),
        Err(_) => false,
    }
}

/// Python `_visible(to_env_worker=...)` core: never for delegate children;
/// dispatcher-spawned env workers per flag; else the profile toolset.
fn kanban_visible(to_env_worker: bool) -> bool {
    if is_delegated_child() {
        return false;
    }
    let env_task = std::env::var("HERMES_KANBAN_TASK").unwrap_or_default();
    if !env_task.trim().is_empty() && is_dispatcher_owned_worker() {
        return to_env_worker;
    }
    kanban_toolset_enabled()
}

/// Python `_check_kanban_mode`: lifecycle tools (dispatcher workers + profiles
/// with the `kanban` toolset).
pub fn kanban_mode_visible() -> bool {
    kanban_visible(true)
}

/// Python `_check_kanban_orchestrator_mode`: board-routing tools
/// (kanban_list, kanban_unblock) are hidden from task workers.
pub fn kanban_orchestrator_mode_visible() -> bool {
    kanban_visible(false)
}

// --- Handler plumbing (Python `_kanban_handler` / `_Reject`) ---

/// Carries a finished `tool_error` payload out of a validation helper.
struct Reject(String);

enum Fail {
    Reject(Reject),
    Other(String),
}

type HandlerResult = Result<String, Fail>;

fn reject(message: impl Into<String>) -> Fail {
    Fail::Reject(Reject(tool_error(message.into())))
}

fn check(cond: bool, message: impl Into<String>) -> Result<(), Fail> {
    if !cond {
        return Err(reject(message));
    }
    Ok(())
}

fn other_fail(e: impl std::fmt::Display) -> Fail {
    Fail::Other(e.to_string())
}

/// Python `_kanban_handler(tool_name)`: unknown-parameter check FIRST
/// (`_UNDECLARED_ARGS` honoured), then the handler, then the error envelope.
fn handler(tool_name: &str, args: &Value, body: impl FnOnce() -> HandlerResult) -> String {
    // Reject typos before a handoff can succeed without its artifacts.
    let properties: BTreeSet<String> = schema_for(tool_name)
        .get("properties")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let mut allowed = properties.clone();
    match tool_name {
        // Python `_UNDECLARED_ARGS`: session_id/project_id provenance, the
        // pre-`project` alias, and attach_url's `title` alias of `filename`.
        "kanban_create" => {
            allowed.insert("session_id".into());
            allowed.insert("project_id".into());
        }
        "kanban_attach_url" => {
            allowed.insert("title".into());
        }
        _ => {}
    }
    let unknown: Vec<String> = args
        .as_object()
        .map(|m| {
            m.keys()
                .filter(|k| !allowed.contains(*k))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if !unknown.is_empty() {
        return tool_error(format!(
            "{}: unknown parameter(s): {}. Valid parameters: {}. Nothing changed.",
            tool_name,
            unknown.join(", "),
            properties.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    match body() {
        Ok(out) => out,
        Err(Fail::Reject(Reject(payload))) => payload,
        Err(Fail::Other(message)) => tool_error(format!("{tool_name}: {message}")),
    }
}

/// Python `_persisted_identity`: the profile this call runs for, `"worker"`
/// when nothing names a profile. Never taken from tool args (#19713).
fn persisted_identity() -> String {
    match std::env::var("HERMES_PROFILE") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => "worker".into(),
    }
}

// --- Env-scoped worker helpers ---

fn env_task() -> Option<String> {
    std::env::var("HERMES_KANBAN_TASK").ok().filter(|v| !v.trim().is_empty())
}

/// Python `_own_task_env`: `$var` only when this worker is scoped to task_id.
fn own_task_env(task_id: &str, var: &str) -> Option<String> {
    if env_task().as_deref() == Some(task_id) {
        std::env::var(var).ok().filter(|v| !v.trim().is_empty())
    } else {
        None
    }
}

/// Python `_worker_run_id`: this worker's dispatcher run id when it is scoped
/// to task_id.
fn worker_run_id(task_id: &str) -> Option<i64> {
    own_task_env(task_id, "HERMES_KANBAN_RUN_ID")?.parse().ok()
}

/// Python `_stamp_worker_session_metadata`.
fn stamp_worker_session_metadata(task_id: &str, metadata: Option<Value>) -> Option<Value> {
    match own_task_env(task_id, "HERMES_SESSION_ID") {
        Some(session_id) => {
            let mut obj = metadata.unwrap_or_else(|| json!({}));
            if let Value::Object(map) = &mut obj {
                map.insert("worker_session_id".into(), json!(session_id));
            }
            Some(obj)
        }
        None => metadata,
    }
}

/// Python `_default_task_id`.
fn default_task_id(arg: Option<&Value>) -> Option<String> {
    if let Some(v) = arg {
        let val = value_to_str(v);
        let val = val.trim();
        if !val.is_empty() {
            return Some(val.to_string());
        }
    }
    if is_delegated_child() {
        return None;
    }
    if !is_dispatcher_owned_worker() {
        // A cron job fired in-process from a worker must never inherit the
        // worker's task id as an implicit default.
        return None;
    }
    env_task()
}

/// Python `_require_task_id`.
fn require_task_id(args: &Value) -> Result<String, Fail> {
    if let Some(tid) = default_task_id(args.get("task_id")) {
        return Ok(tid);
    }
    if env_task().is_some() {
        // Env task present but not usable here (delegate child / cron run
        // beside a worker): it is not this session's to default to.
        return Err(reject(
            "task_id is required: this session does not own the inherited \
             HERMES_KANBAN_TASK, so it is not a valid default. Pass an explicit \
             task_id.",
        ));
    }
    Err(reject(
        "task_id is required: this session has no dispatcher-assigned task to \
         default to. Pass an explicit task_id; discover task ids with kanban_list.",
    ))
}

/// Python `_reject_delegated_child_mutation`.
fn reject_delegated_child_mutation(tool_name: &str) -> Result<(), Fail> {
    if is_delegated_child() {
        return Err(reject(format!(
            "{tool_name} refused: delegate_task child agents are not Kanban run owners. \
             Return findings to the parent agent; the dispatcher worker or an explicitly \
             configured Kanban orchestrator must perform board mutations."
        )));
    }
    Ok(())
}

/// Python `_enforce_worker_task_ownership`: a dispatcher-spawned worker may
/// only mutate its own HERMES_KANBAN_TASK (#19534).
fn enforce_worker_task_ownership(tid: &str) -> Result<(), Fail> {
    if let Some(env_tid) = env_task() {
        if tid != env_tid {
            return Err(reject(format!(
                "worker is scoped to task {env_tid}; refusing to mutate {tid}. Use kanban_comment \
                 to hand off information to other tasks, or kanban_create to spawn follow-up work."
            )));
        }
    }
    Ok(())
}

/// Python `_worker_guard`: delegate-child rejection, task id resolution,
/// task-scope ownership, run-identity proof (run-lifecycle tools only).
fn worker_guard(tool_name: &str, args: &Value) -> Result<String, Fail> {
    reject_delegated_child_mutation(tool_name)?;
    let tid = require_task_id(args)?;
    enforce_worker_task_ownership(&tid)?;
    if RUN_LIFECYCLE_TOOLS.contains(&tool_name) && env_task().is_some() && worker_run_id(&tid).is_none()
    {
        return Err(reject(format!(
            "{tool_name} refused: this worker cannot resolve its \
             HERMES_KANBAN_RUN_ID, so it cannot prove ownership of the card's \
             current run. A stale or unbound worker must not terminate a run a \
             live successor owns. Re-run through the dispatcher so the run id is \
             pinned, or use an orchestrator/CLI path that passes an explicit \
             expected_run_id."
        )));
    }
    Ok(tid)
}

/// Python `_require_orchestrator_tool`: hidden from workers by the check_fn;
/// this catches a stale registration routing a worker here anyway.
fn require_orchestrator_tool(tool_name: &str) -> Result<(), Fail> {
    if !ORCHESTRATOR_TOOLS.contains(&tool_name) || env_task().is_none() {
        return Ok(());
    }
    return Err(reject(format!(
        "{tool_name} is orchestrator-only; dispatcher-spawned workers must use \
         kanban_complete, kanban_request_review, kanban_request_changes, kanban_block, \
         kanban_heartbeat, or kanban_comment for their assigned task."
    )));
}

// --- Argument coercion helpers (Python `_coerce_str_list` etc.) ---

/// Python `_coerce_str_list`: a single string (convenience) or a list/tuple;
/// anything else is a reject naming the Python type.
fn coerce_str_list(
    value: Option<&Value>,
    name: &str,
    what: &str,
    strip: bool,
) -> Result<Option<Vec<String>>, Fail> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(vec![s.clone()])),
        Some(Value::Array(items)) => {
            if strip {
                Ok(Some(
                    items
                        .iter()
                        .map(value_to_str)
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                ))
            } else {
                Ok(Some(items.iter().map(value_to_str).collect()))
            }
        }
        Some(other) => Err(reject(format!(
            "{name} must be a list of {what}, got {}",
            python_type_name(other)
        ))),
    }
}

fn require_dict_metadata(metadata: &Value) -> Result<(), Fail> {
    check(
        metadata.is_null() || metadata.is_object(),
        format!(
            "metadata must be an object/dict, got {}",
            python_type_name(metadata)
        ),
    )
}

/// Python `_merge_artifacts`: fold `artifacts` into `metadata["artifacts"]`
/// (merged with, never overwriting, a list the worker passed manually).
fn merge_artifacts(metadata: Option<Value>, artifacts: Vec<String>) -> Result<Value, Fail> {
    let metadata = metadata.unwrap_or_else(|| json!({}));
    require_dict_metadata(&metadata)?;
    let mut obj = metadata;
    let Value::Object(map) = &mut obj else {
        return Ok(obj);
    };
    let existing = map.get("artifacts").cloned();
    let merged: Vec<String> = match existing {
        Some(Value::Array(items)) => {
            let mut out: Vec<String> = items.iter().map(value_to_str).collect();
            out.extend(artifacts);
            out
        }
        _ => artifacts,
    };
    let deduped: Vec<String> = {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for s in merged {
            let s = s.trim().to_string();
            if !s.is_empty() && seen.insert(s.clone()) {
                out.push(s);
            }
        }
        out
    };
    map.insert("artifacts".into(), json!(deduped));
    Ok(obj)
}

fn require_text(args: &Value, name: &str, message: Option<&str>) -> Result<String, Fail> {
    let value = args.get(name);
    let ok = value
        .map(|v| {
            let s = value_to_str(v);
            !s.trim().is_empty()
        })
        .unwrap_or(false);
    if !ok {
        return Err(reject(
            message
                .map(str::to_string)
                .unwrap_or_else(|| format!("{name} is required")),
        ));
    }
    Ok(value_to_str(value.unwrap()))
}

/// Python `_parse_bool_arg` / `_BOOL_WORDS`.
fn parse_bool_arg(args: &Value, name: &str) -> Result<bool, Fail> {
    let value = args.get(name);
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => {
            let word = value_to_str(other).trim().to_lowercase();
            match word.as_str() {
                "true" | "1" | "yes" => Ok(true),
                "false" | "0" | "no" => Ok(false),
                _ => Err(reject(format!("{name} must be a boolean or 'true'/'false'"))),
            }
        }
    }
}

/// Python `_opt_int`.
fn opt_int(value: Option<&Value>, default: Option<i64>) -> Option<i64> {
    let v = value?;
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .or(default)
}

// --- Board / output helpers ---

fn open_board(board: Option<&Value>) -> Result<Board, Fail> {
    let slug = board.map(value_to_str);
    let path = kanban_state::board_file_path(slug.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()))
        .map_err(|e| match e {
            KbError::Value(msg) => Fail::Other(msg),
            _ => Fail::Other("board resolution failed".into()),
        })?;
    Board::load(path).map_err(|e| Fail::Other(format!("could not open board: {e}")))
}

/// Persist the board after a mutating tool call (Python's SQLite layer commits
/// inside each tool operation; the JSON port commits at the handler boundary).
fn persist(board: &Board) -> Result<(), Fail> {
    board
        .save()
        .map_err(|e| Fail::Other(format!("could not persist board: {e}")))
}

fn existing_task<'a>(board: &'a Board, tid: &str) -> Result<&'a kanban_state::Task, Fail> {
    board
        .get_task(tid)
        .ok_or_else(|| reject(format!("task {tid} not found")))
}

/// Python `_ok`: `{"ok": true, **fields}`.
fn ok(fields: Value) -> String {
    let mut obj = json!({ "ok": true });
    if let (Value::Object(base), Value::Object(extra)) = (&mut obj, &fields) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    obj.to_string()
}

/// Python `_ok_landed`: success payload reporting where the task actually landed.
fn ok_landed(board: &Board, tid: &str, default_status: &str, extra: Value) -> String {
    let run = board.latest_run(tid);
    let landed_status = board
        .get_task(tid)
        .map(|t| t.status.clone())
        .unwrap_or_else(|| default_status.to_string());
    let mut fields = json!({
        "task_id": tid,
        "run_id": run.map(|r| r.id),
        "status": landed_status,
    });
    if let (Value::Object(base), Value::Object(extra)) = (&mut fields, &extra) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    ok(fields)
}

/// Python `_goal_gate`: the auxiliary goal judge is unavailable in this port,
/// and an unavailable judge fails open (`_goal_judge_available` is false).
fn goal_gate(_tool: &str, _tid: &str, _evidence: &str) -> Result<(), Fail> {
    Ok(())
}

// --- Schemas (Python `kanban_tools_schemas.py`, byte-frozen) ---

const DESC_TASK_ID_DEFAULT: &str = "Task id. If omitted, defaults to HERMES_KANBAN_TASK from the env — the task the dispatcher spawned you to work on. That default only exists for a dispatcher-spawned worker; any other caller has no default and must pass an explicit task_id (use kanban_list to discover ids).";
const DESC_BOARD: &str = "Kanban board slug to target. When omitted, the call resolves the active board the usual way: HERMES_KANBAN_DB env → HERMES_KANBAN_BOARD env → the 'current' symlink under the kanban home → 'default'. Pass an explicit slug only when the caller (e.g. a Telegram routing layer) needs to override the env-pinned active board for this one call.";

fn prop(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn board_schema_prop() -> Value {
    prop(DESC_BOARD)
}

/// Every kanban tool takes an optional trailing `board`.
fn schema(_name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    let mut props = properties;
    let obj = props.as_object_mut().unwrap();
    obj.insert("board".into(), board_schema_prop());
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "__tool_description__": description,
    })
}

const STATUS_ENUM: [&str; 9] = [
    "triage", "todo", "scheduled", "ready", "running", "blocked", "review", "done", ARCHIVED,
];

pub fn schema_for(tool: &str) -> Value {
    match tool {
        "kanban_show" => schema(
            "kanban_show",
            "Read a task's full state — title, body, assignee, parent task handoffs, your prior attempts on this task if any, comments, and recent events. Use this to (re)orient yourself before starting work, especially on retries. The response includes a pre-formatted ``worker_context`` string suitable for inclusion verbatim in your reasoning. Outside a dispatcher-spawned worker there is no default task: a bare call returns a pointer to ``kanban_list`` instead of task state.",
            json!({ "task_id": prop(DESC_TASK_ID_DEFAULT) }),
            &[],
        ),
        "kanban_list" => schema(
            "kanban_list",
            "List Kanban task summaries so an orchestrator profile can discover work to route. Supports the same core filters as the CLI: assignee, status, tenant, include_archived, and limit. Returns compact rows with ids, title, status, assignee, priority, parent/child ids, and counts. Bounded to 50 rows by default, 200 max, with truncation metadata. Also recomputes ready tasks before listing, matching the CLI. Orchestrator-only — dispatcher-spawned task workers never see this tool.",
            json!({
                "assignee": prop("Optional assignee/profile filter."),
                "status": {
                    "type": "string",
                    "enum": STATUS_ENUM,
                    "description": "Optional task status filter.",
                },
                "tenant": prop("Optional tenant/project namespace filter."),
                "include_archived": json!({
                    "type": "boolean",
                    "description": "Include archived tasks. Defaults to false.",
                }),
                "limit": json!({
                    "type": "integer",
                    "description": "Optional maximum rows to return (default 50, max 200).",
                }),
            }),
            &[],
        ),
        "kanban_complete" => schema(
            "kanban_complete",
            "Mark your current task done with a structured handoff for downstream workers and humans. Prefer ``summary`` for a human-readable 1-3 sentence description of what you did; put machine-readable facts in ``metadata`` (changed_files, tests_run, decisions, findings, etc). At least one of ``summary`` or ``result`` is required. If you created new tasks via ``kanban_create`` during this run, list their ids in ``created_cards`` — the kernel verifies them so phantom references are caught before they leak into downstream automation. If you produced deliverable files (charts, PDFs, spreadsheets, generated images), list their absolute paths in ``artifacts`` — the gateway notifier will upload them as native attachments to the human who subscribed to the task, so the deliverable lands in their chat alongside the summary instead of being a path they have to fetch by hand.",
            json!({
                "task_id": prop(DESC_TASK_ID_DEFAULT),
                "summary": prop("Human-readable handoff, 1-3 sentences. Appears in Run History on the dashboard and in downstream workers' context."),
                "metadata": json!({
                    "type": "object",
                    "description": "Free-form dict of structured facts about this attempt — {\"changed_files\": [...], \"tests_run\": 12, \"findings\": [...]}. Surfaced to downstream workers alongside ``summary``.",
                }),
                "result": prop("Short result log line (legacy field, maps to task.result). Use ``summary`` instead when possible; this exists for compatibility with callers that still set --result on the CLI."),
                "created_cards": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional structured manifest of task ids you created via ``kanban_create`` during this run. The kernel verifies each id exists and was created by this worker's profile; any phantom id blocks the completion with an error listing what went wrong (auditable in the task's events). Only list ids you got back from a successful ``kanban_create`` call — do not invent or remember ids from prose. Omit the field if you did not create any cards.",
                },
                "artifacts": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional list of absolute paths to deliverable files you produced during this run — generated charts, PDFs, spreadsheets, images, archives. Examples: [\"~/.hermes/cache/scratch/q3-revenue.png\", \"~/.hermes/cache/scratch/report.pdf\"]. The gateway notifier uploads each path as a native attachment to the subscribed chat (images embed inline, everything else uploads as a file) so the deliverable lands with the completion notification. Skip intermediate scratch files and references that are not the deliverable. The path must exist on disk at completion. Files inside a managed scratch workspace are copied to durable task attachments before cleanup; a missing declared scratch artifact keeps the task in-flight so you can fix the path and retry.",
                },
            }),
            &[],
        ),
        "kanban_block" => schema(
            "kanban_block",
            "Stop work on this task and route it according to WHY you're stuck. Set ``kind`` to say which: 'dependency' (waiting on another task — goes to todo and auto-resumes when that task finishes, no human needed), 'needs_input' (you need a human decision/answer), 'capability' (a hard wall: no access, missing credentials, an action no agent can do), or 'transient' (a flaky failure that may clear). ``reason`` is shown to the human on the board. If a task keeps getting unblocked and re-blocked for the same reason, it is auto-escalated to triage. Use for genuine blockers only — don't block on things you can resolve yourself.",
            json!({
                "task_id": prop(DESC_TASK_ID_DEFAULT),
                "reason": prop("What you need answered or what stopped you, in one or two sentences. Don't paste the whole conversation; the human has the board and can ask follow-ups via comments."),
                "kind": {
                    "type": "string",
                    "enum": VALID_BLOCK_KINDS,
                    "description": "Why you're blocked. 'dependency' waits in todo and resumes automatically when an incomplete parent finishes; if no parent is open it is recorded as needs_input instead. The others surface to a human. Omit only if none apply.",
                },
            }),
            &["reason"],
        ),
        "kanban_schedule" => schema(
            "kanban_schedule",
            "Park your current task in the 'scheduled' state while it waits for time or an external event. This ends the current run and makes the task non-dispatchable until an orchestrator unblocks it; it does not create a timer. Put any wake-up marker such as ``SCHEDULED_UNTIL=<ISO8601>`` in ``reason``.",
            json!({
                "task_id": prop(DESC_TASK_ID_DEFAULT),
                "reason": prop("Optional reason or machine-readable wake-up marker recorded on the completed run and scheduled event."),
            }),
            &[],
        ),
        "kanban_request_review" => schema(
            "kanban_request_review",
            "Hand the task off for review: implementation, self-review, and verification are complete and you want a human (or reviewer) to look before it is marked done. Moves the task to the 'review' column and notifies the subscriber. Unlike ``kanban_block`` this is NOT a blocker — it never counts toward unblock-loop detection, so a task can cycle through review across follow-ups without ever being falsely escalated to triage. Use this instead of blocking with a free-form 'review-required:' reason.",
            json!({
                "task_id": prop(DESC_TASK_ID_DEFAULT),
                "summary": prop("What was implemented and how it was verified, in one or two sentences — shown to the reviewer. Don't paste the whole diff; the reviewer has the board and the PR."),
                "reviewer": prop("Optional reviewer profile. When provided, the task is reassigned to that profile before review dispatch."),
                "metadata": {
                    "type": "object",
                    "description": "Optional structured handoff facts for the reviewer, such as changed_files, tests_run, commit, or decisions.",
                    "additionalProperties": true,
                },
                "artifacts": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional list of absolute paths to deliverable files this handoff names — generated charts, PDFs, spreadsheets, images, archives. Examples: ['~/.hermes/cache/scratch/q3-revenue.png', '~/.hermes/cache/scratch/report.pdf']. A review handoff is the last implementer transition, so the kernel copies these into the task's durable attachments before the reviewer's completion cleans the scratch workspace up, and the gateway notifier uploads them as native attachments to the subscribed chat. A missing declared scratch artifact keeps the task in place so you can fix the path and retry.",
                },
            }),
            &["summary"],
        ),
        "kanban_request_changes" => schema(
            "kanban_request_changes",
            "Reviewer verdict: return the current review run to the original implementer with concrete required changes. This closes the review run, reapplies parent dependency gating, and requeues the task without using block-loop accounting. Only use from a task claimed from the review column; use kanban_block only for a genuine external blocker.",
            json!({
                "task_id": prop(DESC_TASK_ID_DEFAULT),
                "reason": prop("Specific, actionable changes the implementer must make before requesting another review."),
            }),
            &["reason"],
        ),
        "kanban_heartbeat" => schema(
            "kanban_heartbeat",
            "Signal that you're still alive during a long operation (training, encoding, large crawls). Call every few minutes so humans see liveness separately from PID checks. Pure side effect — no work changes.",
            json!({
                "task_id": prop(DESC_TASK_ID_DEFAULT),
                "note": prop("Optional short note describing current progress. Shown in the event log."),
            }),
            &[],
        ),
        "kanban_comment" => schema(
            "kanban_comment",
            "Append a comment to a task's thread. Use for durable notes that should outlive this run (questions for the next worker, partial findings, rationale). Ephemeral reasoning doesn't belong here — use your normal response instead.",
            json!({
                "task_id": prop("Task id. Required (may be your own task or another's — comment threads are per-task). Outside a dispatcher-spawned worker there is no default; use kanban_list to discover ids."),
                "body": prop("Markdown-supported comment body."),
            }),
            &["task_id", "body"],
        ),
        "kanban_attach" => schema(
            "kanban_attach",
            "Attach a file to a task by passing its bytes inline (base64). Use for genuine file artifacts the next worker or a human should be able to download — generated reports, images, exports. The file is stored as a real attachment (not a comment link) under the task's attachments dir, capped at 25 MB. Prefer kanban_attach_url when you only have a URL.",
            json!({
                "task_id": prop(DESC_TASK_ID_DEFAULT),
                "filename": prop("File name to store it under (e.g. 'report.pdf'). Directory components are stripped; only the leaf is kept."),
                "content_base64": {
                    "type": "string",
                    "description": "The file contents, base64-encoded. Max 25 MB decoded.",
                },
                "content_type": prop("Optional MIME type (e.g. 'application/pdf')."),
            }),
            &["filename", "content_base64"],
        ),
        "kanban_attach_url" => schema(
            "kanban_attach_url",
            "Attach a file to a task by URL — Hermes downloads it server-side and stores it as a real attachment (capped at 25 MB). Use when you have a link rather than the bytes. Only http/https URLs are accepted.",
            json!({
                "task_id": prop(DESC_TASK_ID_DEFAULT),
                "url": prop("http(s) URL to fetch and store."),
                "filename": prop("Optional name to store it under. Defaults to the URL path's leaf component."),
                "content_type": prop("Optional MIME type override. Defaults to the Content-Type the server returns."),
            }),
            &["url"],
        ),
        "kanban_attachments" => schema(
            "kanban_attachments",
            "List the files attached to a task: id, filename, content_type, size, who uploaded it, and the absolute on-disk path you can read.",
            json!({ "task_id": prop(DESC_TASK_ID_DEFAULT) }),
            &[],
        ),
        "kanban_create" => schema(
            "kanban_create",
            "Create a new kanban task, optionally as a child of the current one (pass the current task id in ``parents``). Used by orchestrator workers to fan out — decompose work into child tasks with specific assignees, link them into a pipeline, then complete your own task. The dispatcher picks up the new tasks on its next tick and spawns the assigned profiles.",
            json!({
                "title": prop("Short task title (required)."),
                "assignee": prop("Profile name that should execute this task (e.g. 'researcher-a', 'reviewer', 'writer'). Required — tasks without an assignee are never dispatched."),
                "body": prop("Opening post: full spec, acceptance criteria, links. The assigned worker reads this as part of its context."),
                "parents": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Parent task ids. The new task stays in 'todo' until every parent reaches 'done'; then it auto-promotes to 'ready'. Typical fan-in: list all the researcher task ids when creating a synthesizer task.",
                },
                "tenant": prop("Optional namespace for multi-project isolation. Defaults to HERMES_TENANT env if set."),
                "priority": json!({
                    "type": "integer",
                    "description": "Dispatcher tiebreaker. Higher = picked sooner when multiple ready tasks share an assignee.",
                }),
                "workspace_kind": {
                    "type": "string",
                    "enum": ["scratch", "dir", "worktree"],
                    "description": "Workspace flavor: 'scratch' (fresh tmp dir, default), 'dir' (shared directory, requires absolute workspace_path), 'worktree' (git worktree).",
                },
                "workspace_path": prop("Absolute path for 'dir' or 'worktree' workspace. Relative paths are rejected at dispatch."),
                "project": prop("Optional project id or slug to link the task to. When set, the task becomes a git worktree under the project's primary repo with a deterministic branch (project slug + task id), instead of a random branch."),
                "triage": json!({
                    "type": "boolean",
                    "description": "If true, task lands in 'triage' instead of 'todo' — a specifier profile is expected to flesh out the body before work starts.",
                }),
                "idempotency_key": prop("If a non-archived task with this key already exists, return that task's id instead of creating a duplicate. Useful for retry-safe automation."),
                "max_runtime_seconds": json!({
                    "type": "integer",
                    "description": "Per-task runtime cap. When exceeded, the dispatcher SIGTERMs the worker and re-queues the task with outcome='timed_out'.",
                }),
                "initial_status": {
                    "type": "string",
                    "enum": ["running", "blocked"],
                    "description": "Initial card status. Use 'blocked' for tasks that require immediate human ops (R3 gate) to skip the brief running-to-blocked transition. Defaults to 'running', which preserves the usual dispatch path.",
                },
                "skills": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Skill names to force-load into the dispatched worker. The kanban lifecycle is already injected automatically; use this to pin a task to a specialist context — e.g. ['translation'] for a translation task, ['github-code-review'] for a reviewer task. The names must match skills installed on the assignee's profile.",
                },
                "goal_mode": json!({
                    "type": "boolean",
                    "description": "Run the dispatched worker in a goal loop. When true, after each turn an auxiliary judge checks the worker's response against this card's title/body; if the work isn't done and budget remains, the worker keeps going in the same session until the judge agrees it's complete (or the goal-turn budget is exhausted, which blocks the task for human review). Use this for open-ended cards where one shot rarely finishes the work. Defaults to false (classic single-shot worker).",
                }),
                "completion_contract": prop("Declare at creation: local-only (default), OWNER/REPO for PR publication, or an exact GitHub PR URL. PR tasks cannot complete until repository-required exact-head CI passes. On publication pass metadata.published_pr."),
                "goal_max_turns": json!({
                    "type": "integer",
                    "description": "Turn budget for goal_mode workers. Caps how many continuation turns the worker may take before the task is blocked for review. Ignored unless goal_mode is true. Defaults to the goal-engine default (20).",
                }),
                "model": prop("Pin the dispatched worker to this model instead of the assignee profile's configured model. Use the exact model name the target provider expects. Omit to use the profile default."),
                "provider": prop("Provider the 'model' belongs to (e.g. 'openrouter', 'anthropic', 'nous'). Set this whenever the model is not from the assignee profile's configured provider — a model name alone is resolved against the profile's provider and will fail if it belongs to a different one. Requires 'model'."),
            }),
            &["title", "assignee"],
        ),
        "kanban_unblock" => schema(
            "kanban_unblock",
            "Unblock a Kanban task. It moves to ready when all parents are done, or todo while any parent remains open. Orchestrator-only — only profiles with the kanban toolset can unblock routed work; dispatcher-spawned task workers never see this tool.",
            json!({ "task_id": prop("Blocked task id to move to ready or parent-gated todo.") }),
            &["task_id"],
        ),
        "kanban_link" => schema(
            "kanban_link",
            "Add a parent→child dependency edge after both tasks already exist. The child won't promote to 'ready' until all parents are 'done'. Cycles and self-links are rejected. A running child is rejected unless the active owning worker is linking its own card for a dependency handoff.",
            json!({
                "parent_id": {"type": "string", "description": "Parent task id."},
                "child_id": {"type": "string", "description": "Child task id."},
            }),
            &["parent_id", "child_id"],
        ),
        _ => json!({}),
    }
}

// --- Handlers ---

fn fields_task(t: &kanban_state::Task) -> Value {
    json!({
        "id": t.id, "title": t.title, "body": t.body, "assignee": t.assignee,
        "status": t.status, "tenant": t.tenant, "priority": t.priority,
        "workspace_kind": t.workspace_kind, "workspace_path": t.workspace_path,
        "created_by": t.created_by, "created_at": t.created_at,
        "started_at": t.started_at, "completed_at": t.completed_at,
        "result": t.result, "current_run_id": t.current_run_id,
        "model_override": t.model_override, "provider_override": t.provider_override,
        "completion_contract": t.completion_contract,
        "last_failure_error": t.last_failure_error,
    })
}

fn fields_run(r: &kanban_state::Run) -> Value {
    json!({
        "id": r.id, "profile": r.profile, "status": r.status, "outcome": r.outcome,
        "summary": r.summary, "error": r.error, "metadata": r.metadata,
        "started_at": r.started_at, "ended_at": r.ended_at,
    })
}

fn fields_comment(c: &kanban_state::Comment) -> Value {
    json!({ "author": c.author, "body": c.body, "created_at": c.created_at })
}

fn fields_event(e: &kanban_state::Event) -> Value {
    json!({ "kind": e.kind, "payload": e.payload, "created_at": e.created_at, "run_id": e.run_id })
}

fn fields_attachment(a: &kanban_state::Attachment) -> Value {
    json!({
        "id": a.id, "filename": a.filename, "content_type": a.content_type,
        "size": a.size, "uploaded_by": a.uploaded_by, "stored_path": a.stored_path,
        "created_at": a.created_at,
    })
}

fn handle_show(args: &Value) -> HandlerResult {
    let tid = default_task_id(args.get("task_id"));
    let Some(tid) = tid else {
        // No dispatcher task in scope and no explicit id: the caller asked
        // "what should I be looking at" (#91431) — answer instead of erroring.
        return Ok(json!({
            "current_task": null,
            "hint": "No dispatcher-assigned task in this session, so there is no task to show by default. Call kanban_list to see task ids on the board, then kanban_show(task_id=<id>) for full state.",
        })
        .to_string());
    };
    let board = open_board(args.get("board"))?;
    existing_task(&board, &tid)?;
    let task = board.get_task(&tid).unwrap().clone();
    let parents = board.parent_ids(&tid);
    let unsatisfied: Vec<Value> = board
        .unsatisfied_parents(&tid)
        .iter()
        .map(|(pid, status)| json!({ "id": pid, "status": status }))
        .collect();
    let children = board.child_ids(&tid);
    let comments: Vec<Value> = board.list_comments(&tid).iter().map(|c| fields_comment(c)).collect();
    // Capped; full log via CLI.
    let events: Vec<Value> = board
        .list_events(&tid)
        .iter()
        .rev()
        .take(50)
        .rev()
        .map(|e| fields_event(e))
        .collect();
    let runs: Vec<Value> = board.list_runs(&tid).iter().map(|r| fields_run(r)).collect();
    let worker_context = board.build_worker_context(&tid).map_err(other_fail)?;
    Ok(json!({
        "task": fields_task(&task),
        "parents": parents,
        "unsatisfied_parents": unsatisfied,
        "children": children,
        "comments": comments,
        "events": events,
        "runs": runs,
        "worker_context": worker_context,
    })
    .to_string())
}

fn task_summary(board: &Board, t: &kanban_state::Task) -> Value {
    let parents = board.parent_ids(&t.id);
    let children = board.child_ids(&t.id);
    json!({
        "id": t.id, "title": t.title, "assignee": t.assignee, "status": t.status,
        "priority": t.priority, "tenant": t.tenant, "workspace_kind": t.workspace_kind,
        "workspace_path": t.workspace_path, "project_id": t.project_id,
        "created_by": t.created_by, "created_at": t.created_at,
        "started_at": t.started_at, "completed_at": t.completed_at,
        "current_run_id": t.current_run_id, "model_override": t.model_override,
        "provider_override": t.provider_override,
        "parents": parents, "children": children,
        "parent_count": parents.len(), "child_count": children.len(),
    })
}

/// Python repr of a sorted list literal inside error strings: `['a', 'b']`.
fn py_list(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("'{s}'")).collect();
    format!("[{}]", quoted.join(", "))
}

/// Python `{value!r}` for the optional `kind` argument.
fn py_repr_opt(value: Option<&str>) -> String {
    match value {
        Some(s) => format!("'{s}'"),
        None => "None".into(),
    }
}

fn handle_list(args: &Value) -> HandlerResult {
    require_orchestrator_tool("kanban_list")?;
    if let Some(status) = args.get("status").and_then(Value::as_str) {
        check(
            is_valid_status(status),
            format!(
                "status must be one of {}",
                py_list(&[
                    "archived", "blocked", "done", "ready", "review", "running", "scheduled",
                    "todo", "triage",
                ])
            ),
        )?;
    }
    let include_archived = parse_bool_arg(args, "include_archived")?;
    let limit = match args.get("limit") {
        None | Some(Value::Null) => LIST_DEFAULT_LIMIT,
        Some(v) => v
            .as_i64()
            .or_else(|| v.as_f64().map(|f| f as i64))
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            .ok_or_else(|| Fail::Other("limit must be an integer".into()))?,
    };
    check(limit >= 1, "limit must be >= 1")?;
    check(limit <= LIST_MAX_LIMIT, format!("limit must be <= {LIST_MAX_LIMIT}"))?;
    let mut board = open_board(args.get("board"))?;
    // Match CLI list: dependencies cleared since the last dispatcher tick
    // should be visible to orchestrators immediately.
    let promoted = board.recompute_ready();
    // One extra row lets the output report truncation without dumping the board.
    let mut rows: Vec<&kanban_state::Task> = board
        .file
        .tasks
        .iter()
        .filter(|t| {
            let assignee_ok = args
                .get("assignee")
                .and_then(Value::as_str)
                .map(|a| t.assignee.as_deref() == Some(a))
                .unwrap_or(true);
            let status_ok = match args.get("status").and_then(Value::as_str) {
                Some(s) => t.status == s,
                None => include_archived || t.status != ARCHIVED,
            };
            let tenant_ok = args
                .get("tenant")
                .and_then(Value::as_str)
                .map(|tn| t.tenant.as_deref() == Some(tn))
                .unwrap_or(true);
            assignee_ok && status_ok && tenant_ok
        })
        .collect();
    rows.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.created_at.cmp(&b.created_at)));
    let truncated = rows.len() as i64 > limit;
    let tasks: Vec<Value> = rows
        .iter()
        .take(limit.max(0) as usize)
        .map(|t| task_summary(&board, t))
        .collect();
    let count = tasks.len() as i64;
    let next_limit = if truncated && limit < LIST_MAX_LIMIT {
        Some((limit * 2).min(LIST_MAX_LIMIT))
    } else {
        None
    };
    Ok(json!({
        "tasks": tasks,
        "count": count,
        "limit": limit,
        "truncated": truncated,
        "next_limit": next_limit,
        "promoted": promoted,
    })
    .to_string())
}

fn handle_complete(args: &Value) -> HandlerResult {
    reject_delegated_child_mutation("kanban_complete")?;
    let tid = require_task_id(args)?;
    let summary = redact_opt(args.get("summary").unwrap_or(&Value::Null));
    let result = redact_opt(args.get("result").unwrap_or(&Value::Null));
    let mut metadata = args.get("metadata").cloned();
    if let Some(md) = &metadata {
        if md.is_object() {
            // Keep the unredacted dict if the redacted JSON cannot be re-parsed.
            metadata = redact_metadata(md).or(metadata);
        }
    }
    let created_cards = coerce_str_list(args.get("created_cards"), "created_cards", "task ids", true)?;
    let artifacts = coerce_str_list(args.get("artifacts"), "artifacts", "file paths", true)?;
    if artifacts.as_ref().map(|a| !a.is_empty()).unwrap_or(false) {
        metadata = Some(merge_artifacts(metadata, artifacts.clone().unwrap())?);
    }
    let summary_text = summary.as_str().unwrap_or("");
    let result_text = result.as_str().unwrap_or("");
    // The empty-completion refusal precedes task/ownership resolution so a
    // bare retry with evidence added is never masked by a scoping error.
    check(
        !summary_text.is_empty() || !result_text.is_empty(),
        "provide at least one of: summary (preferred), result",
    )?;
    // Ownership and run-lifecycle checks come after payload validation.
    enforce_worker_task_ownership(&tid)?;
    if RUN_LIFECYCLE_TOOLS.contains(&"kanban_complete") && env_task().is_some() && worker_run_id(&tid).is_none() {
        return Err(reject(String::from(
            "kanban_complete refused: this worker cannot resolve its \
             HERMES_KANBAN_RUN_ID, so it cannot prove ownership of the card's \
             current run. A stale or unbound worker must not terminate a run a \
             live successor owns. Re-run through the dispatcher so the run id is \
             pinned, or use an orchestrator/CLI path that passes an explicit \
             expected_run_id.",
        )));
    }
    require_dict_metadata(metadata.as_ref().unwrap_or(&Value::Null))?;
    let metadata = stamp_worker_session_metadata(&tid, metadata);
    let mut board = open_board(args.get("board"))?;
    goal_gate("kanban_complete", &tid, if !summary_text.is_empty() { summary_text } else { result_text })?;
    let expected_run_id = worker_run_id(&tid);
    let summary_arg = summary.as_str().filter(|s| !s.is_empty());
    let result_arg = result.as_str().filter(|s| !s.is_empty());
    match board.complete_task(
        &tid,
        result_arg,
        summary_arg,
        metadata.as_ref(),
        created_cards.as_deref().unwrap_or(&[]),
        expected_run_id,
        false,
    ) {
        Ok(true) => {
            let attachments: Vec<Value> = board
                .list_attachments(&tid)
                .iter()
                .map(|a| fields_attachment(a))
                .collect();
            let run = board.latest_run(&tid);
            persist(&board)?;
            return Ok(ok(json!({
                "task_id": tid,
                "run_id": run.map(|r| r.id),
                "attachments": attachments,
            })));
        }
        Ok(false) => {
            // complete_task reports every refusal as bare False; a reopened or
            // never-finished parent is the actionable one.
            let blockers = board.unsatisfied_parents(&tid);
            if !blockers.is_empty() {
                let detail = blockers
                    .iter()
                    .map(|(pid, status)| format!("{pid} ({status})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(reject(format!(
                    "could not complete {tid}: unsatisfied parent dependencies: {detail}; \
                     complete the parents first (done or archived)"
                )));
            }
            let last_failure = board
                .get_task(&tid)
                .and_then(|t| t.last_failure_error.clone())
                .unwrap_or_default();
            return Err(reject(if last_failure.is_empty() {
                format!("could not complete {tid} (unknown id, stale run, or already terminal)")
            } else {
                last_failure
            }));
        }
        Err(KbError::Artifact(err)) => {
            return Ok(tool_error(format!(
                "kanban_complete could not preserve the declared artifacts: {err}. \
                 Your task is still in-flight and its scratch workspace was kept. Fix the \
                 artifact path or storage error, then retry kanban_complete with the same \
                 handoff."
            )));
        }
        Err(KbError::LiveClaim(claim_err)) => {
            return Ok(tool_error(format!(
                "kanban_complete refused: {claim_err}. Nothing changed. Wait for the worker \
                 to finish, or an operator can run `hermes kanban complete --force {tid}`."
            )));
        }
        Err(KbError::Hallucinated(phantom)) => {
            return Ok(tool_error(format!(
                "kanban_complete blocked: the following created_cards do not exist or were not \
                 created by this worker: {}. Your task is still in-flight (no state change). \
                 Retry kanban_complete with the same summary/metadata and either drop these ids \
                 from created_cards, or pass created_cards=[] to skip the card-claim check entirely.",
                phantom.join(", ")
            )));
        }
        Err(KbError::EmptyCompletion(empty_err)) => {
            return Ok(tool_error(format!(
                "kanban_complete blocked: {empty_err}. Your task is still in-flight (no state \
                 change). Retry kanban_complete with a non-empty summary or result describing \
                 what was done."
            )));
        }
        Err(e) => return Err(other_fail(e)),
    }
}

fn handle_block(args: &Value) -> HandlerResult {
    let tid = worker_guard("kanban_block", args)?;
    let reason = redact(&json!(require_text(
        args,
        "reason",
        Some("reason is required — explain what input you need"),
    )?));
    let kind_raw = args.get("kind");
    let kind = kind_raw.and_then(Value::as_str);
    let mut board = open_board(args.get("board"))?;
    check(
        kind_raw.is_none()
            || kind_raw == Some(&Value::Null)
            || kind.is_some_and(|k| VALID_BLOCK_KINDS.contains(&k)),
        format!(
            "kind must be one of {} (or omit it)",
            py_list(&["capability", "dependency", "needs_input", "transient"])
        ),
    )?;
    // Goal-mode block gate (#38696): kanban_block is a second exit path out of
    // the goal loop, so goal_mode tasks may only block on genuine external
    // blockers.
    let (goal_mode, _) = board
        .get_task(&tid)
        .map(|t| (t.goal_mode, t.status.clone()))
        .unwrap_or((false, String::new()));
    check(
        !goal_mode
            || kind.is_some_and(|k| ["dependency", "needs_input"].contains(&k)),
        format!(
            "goal_mode tasks can only block with kind in {} (got {}). If the task is actually \
             finished or cannot proceed for another reason, call kanban_complete instead — \
             the completion judge will evaluate it.",
            py_list(&["dependency", "needs_input"]),
            py_repr_opt(kind),
        ),
    )?;
    let ok = board
        .block_task(&tid, Some(&reason), kind, worker_run_id(&tid))
        .map_err(other_fail)?;
    check(ok, format!("could not block {tid} (unknown id or not in running/ready)"))?;
    let landed_kind = board.get_task(&tid).and_then(|t| t.block_kind.clone());
    let mut extra = json!({ "block_kind": landed_kind });
    if kind == Some("dependency") {
        if let Some(k) = board.get_task(&tid).and_then(|t| t.block_kind.clone()) {
            if k != "dependency" {
                // block_task re-kinds a dependency wait that no open parent can satisfy.
                extra["requested_kind"] = json!("dependency");
                extra["note"] = json!(
                    "kind='dependency' only waits on an incomplete parent; no parent is open, \
                     so this was recorded as needs_input (sticky until a human unblocks) \
                     instead of parking in todo where the dispatcher would respawn it."
                );
            }
        }
    }
    persist(&board)?;
    Ok(ok_landed(&board, &tid, "blocked", extra))
}

fn handle_schedule(args: &Value) -> HandlerResult {
    let tid = worker_guard("kanban_schedule", args)?;
    let raw_reason = args.get("reason");
    check(
        raw_reason.is_none() || raw_reason.map(Value::is_string).unwrap_or(false),
        "reason must be a string",
    )?;
    let reason = raw_reason
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| redact(&json!(s)));
    let mut board = open_board(args.get("board"))?;
    // The goal loop treats `scheduled` as terminal like `blocked`, so parking
    // would bypass the completion judge (see kanban_block, #38696).
    let goal_mode = board.get_task(&tid).map(|t| t.goal_mode).unwrap_or(false);
    check(
        !goal_mode,
        "goal_mode tasks cannot be scheduled: use kanban_block with kind in \
         ['dependency', 'needs_input'] for a genuine external blocker, or kanban_complete \
         so the completion judge can evaluate it.",
    )?;
    let ok = board
        .schedule_task(&tid, reason.as_deref(), worker_run_id(&tid))
        .map_err(other_fail)?;
    check(
        ok,
        format!("could not schedule {tid} (unknown id or not in todo/ready/running/blocked)"),
    )?;
    persist(&board)?;
    Ok(ok_landed(&board, &tid, "scheduled", json!({ "reason": reason })))
}

fn handle_request_review(args: &Value) -> HandlerResult {
    // The reviewer-profile check precedes the summary requirement: a bad
    // profile is the caller's tooling error, reported before payload checks.
    let reviewer = redact_opt(args.get("reviewer").unwrap_or(&Value::Null));
    let reviewer = reviewer.as_str().filter(|s| !s.is_empty());
    if let Some(reviewer) = reviewer {
        // A non-profile reviewer would park the card in `review` on an assignee
        // the dispatcher can never spawn (#106163).
        if !profile_exists(reviewer) {
            return Err(reject(format!(
                "reviewer profile '{reviewer}' is not installed. Installed profiles: {}",
                list_profile_names().join(", ")
            )));
        }
    }
    let tid = worker_guard("kanban_request_review", args)?;
    let summary = redact(&json!(require_text(
        args,
        "summary",
        Some("summary is required — describe what was implemented and how it was verified so the reviewer has context"),
    )?));
    let metadata = args.get("metadata").cloned();
    require_dict_metadata(metadata.as_ref().unwrap_or(&Value::Null))?;
    let metadata = match metadata {
        Some(Value::Null) | None => None,
        Some(md) => {
            let redacted = redact_metadata(&md);
            check(
                redacted.is_some(),
                "metadata could not be safely serialized",
            )?;
            redacted
        }
    };
    let artifacts = coerce_str_list(args.get("artifacts"), "artifacts", "file paths", true)?;
    let metadata = if artifacts.as_ref().map(|a| !a.is_empty()).unwrap_or(false) {
        Some(merge_artifacts(metadata, artifacts.unwrap())?)
    } else {
        metadata
    };
    let metadata = stamp_worker_session_metadata(&tid, metadata);
    // Reviewer is model-supplied free text stored durably on the event payload.
    let reviewer = redact_opt(args.get("reviewer").unwrap_or(&Value::Null));
    let reviewer = reviewer.as_str().filter(|s| !s.is_empty());
    let mut board = open_board(args.get("board"))?;
    goal_gate("kanban_request_review", &tid, &summary)?;
    let (ok, fail_reason) = board
        .request_review(&tid, Some(&summary), metadata.as_ref(), reviewer, worker_run_id(&tid))
        .map_err(other_fail)?;
    check(
        ok,
        format!(
            "could not request review for {tid}: {}",
            fail_reason.unwrap_or_else(|| "unknown id or not in running/ready".into())
        ),
    )?;
    persist(&board)?;
    Ok(ok_landed(&board, &tid, "review", json!({})))
}

fn handle_request_changes(args: &Value) -> HandlerResult {
    let tid = worker_guard("kanban_request_changes", args)?;
    let reason = redact(&json!(require_text(
        args,
        "reason",
        Some("reason is required — describe the changes needed"),
    )?));
    let mut board = open_board(args.get("board"))?;
    let (ok, detail) = board
        .request_changes(&tid, &reason, worker_run_id(&tid))
        .map_err(other_fail)?;
    check(
        ok,
        format!(
            "could not request changes for {tid}: {}",
            detail.clone().unwrap_or_else(|| "invalid review state".into())
        ),
    )?;
    persist(&board)?;
    Ok(ok_landed(&board, &tid, "ready", json!({ "implementer": detail })))
}

fn handle_heartbeat(args: &Value) -> HandlerResult {
    let tid = worker_guard("kanban_heartbeat", args)?;
    let mut board = open_board(args.get("board"))?;
    // The dispatcher pins HERMES_KANBAN_CLAIM_LOCK at spawn; the default
    // claimer covers locally-driven workers that bypassed the dispatcher.
    let claimer = std::env::var("HERMES_KANBAN_CLAIM_LOCK").ok().filter(|v| !v.is_empty());
    board.heartbeat_claim(&tid, None, claimer.as_deref());
    let note = args.get("note").and_then(Value::as_str);
    let moved = board.heartbeat_worker(&tid, note, worker_run_id(&tid));
    check(moved, format!("could not heartbeat {tid} (unknown id or not running)"))?;
    persist(&board)?;
    Ok(ok(json!({ "task_id": tid })))
}

fn handle_comment(args: &Value) -> HandlerResult {
    reject_delegated_child_mutation("kanban_comment")?;
    let tid = match args.get("task_id").map(value_to_str).map(|s| s.trim().to_string()) {
        Some(tid) if !tid.is_empty() => tid,
        // A comment writes to the board, so a bare call must stay an error —
        // but point at kanban_list, not an env var a chat caller cannot set (#91431).
        _ => {
            return Err(reject(
                "task_id is required: comments are per-task and this session has no \
                 dispatcher-assigned task to default to. Discover task ids with \
                 kanban_list, or pass the task id you mean.",
            ))
        }
    };
    let body = redact(&json!(require_text(args, "body", None)?));
    // Author comes from the worker's runtime identity (`_persisted_identity`),
    // never caller args: comments are injected into future workers' prompts and
    // an args["author"] override could forge a directive (#19713).
    let author = persisted_identity();
    let mut board = open_board(args.get("board"))?;
    let cid = board.add_comment(&tid, &author, &body).map_err(other_fail)?;
    persist(&board)?;
    Ok(ok(json!({ "task_id": tid, "comment_id": cid })))
}

fn store_attachment(
    board_slug: Option<&Value>,
    tid: &str,
    filename: &str,
    data: Vec<u8>,
    content_type: Option<&str>,
) -> Result<String, Fail> {
    let mut board = open_board(board_slug)?;
    let att_id = board
        .store_attachment_bytes(tid, filename, &data, content_type, Some("agent"))
        .map_err(other_fail)?;
    persist(&board)?;
    Ok(ok(json!({ "task_id": tid, "attachment_id": att_id, "size": data.len() })))
}

fn handle_attach(args: &Value) -> HandlerResult {
    // Argument validation comes before the task guard: a malformed payload is
    // the caller's error, not a missing-task error.
    let filename = require_text(args, "filename", None)?;
    // A missing/None content_base64 is reported through the same decode-error
    // channel as a malformed payload.
    let data = match args.get("content_base64").and_then(Value::as_str) {
        Some(s) => base64_decode(s).map_err(|e| {
            Fail::Reject(Reject(tool_error(format!("content_base64 is not valid base64: {e}"))))
        })?,
        None => {
            return Err(reject(String::from(
                "content_base64 is not valid base64: expected a base64 string",
            )))
        }
    };
    let tid = worker_guard("kanban_attach", args)?;
    let content_type = args.get("content_type").and_then(Value::as_str);
    store_attachment(args.get("board"), &tid, &filename, data, content_type)
}

/// SSRF-guarded capped download (Python `_download_url_with_cap`): every hop
/// scheme+host checked, redirects followed manually, body capped mid-stream.
const MAX_ATTACH_URL_REDIRECTS: usize = 5;

fn download_url_with_cap(url: &str, max_bytes: usize) -> Result<(Vec<u8>, Option<String>), String> {
    use reqwest::blocking::Client;
    let client = Client::builder()
        .user_agent("hermes-kanban/attach")
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let mut current_url = url.to_string();
    for _ in 0..=MAX_ATTACH_URL_REDIRECTS {
        let scheme = current_url.split("://").next().unwrap_or("").to_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(format!("unsupported URL scheme {scheme:?}; only http/https are allowed"));
        }
        if !is_safe_url(&current_url) {
            return Err(format!(
                "URL blocked by SSRF protection (private/internal address): {current_url}"
            ));
        }
        let resp = client
            .get(&current_url)
            .send()
            .map_err(|e| format!("request to {current_url} failed: {e}"))?;
        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| format!("redirect without Location header from {current_url}"))?;
            current_url = join_urls(&current_url, &location);
            continue;
        }
        let status = resp.status();
        let headers = resp.headers().clone();
        let resp = resp.error_for_status().map_err(|e| {
            let status = e.status().unwrap_or(status);
            format!("{current_url} responded with {status}")
        })?;
        let content_type = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .and_then(|ct| ct.split(';').next())
            .map(str::trim)
            .filter(|ct| !ct.is_empty())
            .map(str::to_string);
        let body = resp.bytes().map_err(|e| format!("reading {current_url} failed: {e}"))?;
        if body.len() > max_bytes {
            return Err(format!(
                "attachment exceeds {} MB limit",
                max_bytes / (1024 * 1024)
            ));
        }
        return Ok((body.to_vec(), content_type));
    }
    Err(format!("too many redirects fetching {url}"))
}

/// Python `tools.url_safety.is_safe_url` subset: reject loopback/private/
/// cloud-metadata hosts.
fn is_safe_url(url: &str) -> bool {
    let host = crate::web_extract::url_host(url);
    let Some(host) = host else {
        return false;
    };
    let host = host.to_lowercase();
    if host == "169.254.169.254" || host == "metadata.google.internal" {
        return false;
    }
    if host == "localhost" || host == "::1" || host.starts_with("127.") || host.ends_with(".localhost") {
        return false;
    }
    if host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("fd")
        || host.starts_with("fe80")
    {
        return false;
    }
    if let Ok(rest) = host.parse::<std::net::IpAddr>() {
        let private = match rest {
            std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified(),
            std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || v6.is_unique_local() || v6.is_unicast_link_local(),
        };
        if private {
            return false;
        }
    }
    // 172.16.0.0/12
    if let Some(rest) = host.strip_prefix("172.") {
        if let Ok(second) = rest.split('.').next().unwrap_or("").parse::<u8>() {
            if (16..=31).contains(&second) {
                return false;
            }
        }
    }
    true
}

/// Minimal URL join for redirect resolution (Python `urljoin` subset):
/// absolute, protocol-relative, and absolute-path locations.
fn join_urls(base: &str, location: &str) -> String {
    if location.contains("://") {
        location.to_string()
    } else if let Some(rest) = location.strip_prefix("//") {
        let scheme = base.split("://").next().unwrap_or("https");
        format!("{scheme}:{rest}")
    } else if let Some(stripped) = location.strip_prefix('/') {
        let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
        let host_end = base[scheme_end..].find('/').map(|i| scheme_end + i).unwrap_or(base.len());
        format!("{}{}{}", &base[..host_end], "/", stripped)
    } else {
        location.to_string()
    }
}

fn handle_attach_url(args: &Value) -> HandlerResult {
    let tid = worker_guard("kanban_attach_url", args)?;
    let url = require_text(args, "url", None)?.trim().to_string();
    let filename = args
        .get("filename")
        .or(args.get("title"))
        .map(value_to_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            // Derive a name from the URL path's leaf component.
            let path = url.split("://").nth(1).unwrap_or(&url);
            let path = path.split(['?', '#']).next().unwrap_or(path);
            let leaf = path.rsplit('/').next().unwrap_or("");
            url_decode(leaf).trim().to_string()
        });
    let filename = if filename.is_empty() { "download".to_string() } else { filename };
    match download_url_with_cap(&url, KANBAN_ATTACHMENT_MAX_BYTES) {
        Ok((data, fetched_ct)) => {
            let content_type = args
                .get("content_type")
                .and_then(Value::as_str)
                .or(fetched_ct.as_deref());
            store_attachment(args.get("board"), &tid, &filename, data, content_type)
        }
        Err(e) => Ok(tool_error(format!("kanban_attach_url: {e}"))),
    }
}

fn handle_attachments(args: &Value) -> HandlerResult {
    // Read-only; no ownership restriction.
    let tid = require_task_id(args)?;
    let board = open_board(args.get("board"))?;
    existing_task(&board, &tid)?;
    let attachments: Vec<Value> = board
        .list_attachments(&tid)
        .iter()
        .map(|a| fields_attachment(a))
        .collect();
    Ok(json!({ "ok": true, "task_id": tid, "attachments": attachments }).to_string())
}

/// Python `hermes_cli.profiles.profile_exists` subset: a profile directory
/// under the Hermes home.
fn profile_exists(name: &str) -> bool {
    kanban_state::hermes_home().join("profiles").join(name).is_dir()
}

fn list_profile_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(kanban_state::hermes_home().join("profiles"))
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            entry.path().is_dir().then(|| entry.file_name().to_string_lossy().to_string())
        })
        .collect();
    names.sort();
    names
}

fn handle_create(args: &Value) -> HandlerResult {
    reject_delegated_child_mutation("kanban_create")?;
    let title = require_text(args, "title", None)?;
    let assignee = args.get("assignee").map(value_to_str);
    check(
        assignee.as_deref().map(|a| !a.trim().is_empty()).unwrap_or(false),
        "assignee is required — name the profile that should execute this task \
         (the dispatcher will only spawn tasks with an assignee)",
    )?;
    // Workspace sharing is always explicit: omitted fields mean a fresh scratch
    // workspace even for a dispatcher-spawned creator (#67567).
    let workspace_kind = args.get("workspace_kind").and_then(Value::as_str);
    let workspace_path = args.get("workspace_path").and_then(Value::as_str);
    // ``project=""`` is an explicit "no project" (no `or` collapse, #106342).
    let project_id = if args.as_object().map(|m| m.contains_key("project")).unwrap_or(false) {
        args.get("project").map(value_to_str)
    } else {
        args.get("project_id").map(value_to_str)
    };
    let triage = parse_bool_arg(args, "triage")?;
    let skills = coerce_str_list(args.get("skills"), "skills", "skill names", false)?;
    let goal_mode = parse_bool_arg(args, "goal_mode")?;
    let model_override = args.get("model").map(value_to_str);
    let provider_override = args.get("provider").map(value_to_str);
    check(
        model_override.is_some() || provider_override.is_none(),
        "'provider' requires 'model' to be set as well",
    )?;
    let parents = coerce_str_list(args.get("parents"), "parents", "task ids", false)?;
    let mut board = open_board(args.get("board"))?;
    let self_tid = if is_dispatcher_owned_worker() { env_task() } else { None };
    let self_task = self_tid.as_deref().and_then(|t| {
        board.get_task(t).cloned()
    });
    // The worker/API runtime may be transient; the owning task's origin is durable.
    // (Session provenance via state.db is a CLI-context no-op in this port.)
    let session_id = args
        .get("session_id")
        .map(value_to_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| self_task.as_ref().and_then(|t| t.session_id.clone()));
    let mut project_id = project_id;
    let mut project_source_task_id: Option<String> = None;
    if project_id.is_none() && workspace_kind.is_none() && workspace_path.is_none() {
        if let Some(self_task) = &self_task {
            if let Some(pid) = &self_task.project_id {
                project_id = Some(pid.clone());
                project_source_task_id = Some(self_task.id.clone());
            }
        }
    }
    let tenant = args
        .get("tenant")
        .map(value_to_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HERMES_TENANT").ok().filter(|t| !t.is_empty()));
    let priority = opt_int(args.get("priority"), Some(0)).unwrap_or(0);
    let initial_status = args
        .get("initial_status")
        .map(value_to_str)
        .unwrap_or_else(|| "running".into());
    let new_tid = board
        .create_task(
            &title,
            args.get("body").and_then(Value::as_str),
            assignee.unwrap_or_default().trim(),
            &persisted_identity(),
            parents.as_deref().unwrap_or(&[]),
            tenant.as_deref(),
            priority,
            workspace_kind,
            workspace_path,
            project_id.as_deref(),
            triage,
            args.get("idempotency_key").and_then(Value::as_str),
            opt_int(args.get("max_runtime_seconds"), None),
            skills,
            model_override.as_deref().filter(|m| !m.trim().is_empty()),
            provider_override.as_deref().filter(|p| !p.trim().is_empty()),
            goal_mode,
            opt_int(args.get("goal_max_turns"), None),
            args.get("completion_contract").and_then(Value::as_str),
            &initial_status,
            session_id.as_deref(),
        )
        .map_err(other_fail)?;
    let landed = board.get_task(&new_tid);
    let wait: Vec<&kanban_state::Event> = board
        .list_events(&new_tid)
        .into_iter()
        .filter(|e| e.kind == "dependency_wait")
        .collect();
    let gate = match wait.last() {
        Some(e) => json!({
            "gated": true,
            "gated_by": e.payload.as_ref().and_then(|p| p.get("parent")).cloned(),
        }),
        None => json!({ "gated": false }),
    };
    let subscribed = false; // CLI/test context: no persistent notify channel.
    persist(&board)?;
    Ok(ok(json!({
        "task_id": new_tid,
        "status": landed.map(|t| t.status.clone()),
        "workspace_kind": landed.and_then(|t| t.workspace_kind.clone()),
        "workspace_path": landed.and_then(|t| t.workspace_path.clone()),
        "project_id": landed.and_then(|t| t.project_id.clone()),
        "project_source_task_id": project_source_task_id,
        "gated": gate["gated"],
        "gated_by": gate.get("gated_by").cloned().unwrap_or(Value::Null),
        "subscribed": subscribed,
    })))
}

fn handle_unblock(args: &Value) -> HandlerResult {
    reject_delegated_child_mutation("kanban_unblock")?;
    require_orchestrator_tool("kanban_unblock")?;
    let tid = match args.get("task_id").map(value_to_str).map(|s| s.trim().to_string()) {
        Some(tid) if !tid.is_empty() => tid,
        _ => return Err(reject("task_id is required")),
    };
    enforce_worker_task_ownership(&tid)?;
    let mut board = open_board(args.get("board"))?;
    let moved = board.unblock_task(&tid).map_err(other_fail)?;
    check(moved, format!("could not unblock {tid} (not blocked or unknown)"))?;
    persist(&board)?;
    let status = board.get_task(&tid).map(|t| t.status.clone());
    Ok(ok(json!({ "task_id": tid, "status": status })))
}

fn handle_link(args: &Value) -> HandlerResult {
    reject_delegated_child_mutation("kanban_link")?;
    let parent_id = args.get("parent_id").map(value_to_str).filter(|s| !s.trim().is_empty());
    let child_id = args.get("child_id").map(value_to_str).filter(|s| !s.trim().is_empty());
    check(
        parent_id.is_some() && child_id.is_some(),
        "both parent_id and child_id are required",
    )?;
    let mut board = open_board(args.get("board"))?;
    // A worker linking its OWN running card proves ownership with its run id
    // so the dependency-block handoff still works.
    let gated = board
        .link_tasks(
            parent_id.as_deref().unwrap().trim(),
            child_id.as_deref().unwrap().trim(),
            worker_run_id(child_id.as_deref().unwrap().trim()),
        )
        .map_err(other_fail)?;
    persist(&board)?;
    let mut out = json!({ "parent_id": parent_id, "child_id": child_id, "gated": gated });
    if gated {
        out["gated_by"] = out["parent_id"].clone();
    }
    Ok(ok(out))
}

// --- Dispatch (the registry's single kanban arm) ---

pub fn execute(name: &str, args: &Value) -> String {
    match name {
        "kanban_show" => handler("kanban_show", args, || handle_show(args)),
        "kanban_list" => handler("kanban_list", args, || handle_list(args)),
        "kanban_complete" => handler("kanban_complete", args, || handle_complete(args)),
        "kanban_block" => handler("kanban_block", args, || handle_block(args)),
        "kanban_schedule" => handler("kanban_schedule", args, || handle_schedule(args)),
        "kanban_request_review" => handler("kanban_request_review", args, || handle_request_review(args)),
        "kanban_request_changes" => handler("kanban_request_changes", args, || handle_request_changes(args)),
        "kanban_heartbeat" => handler("kanban_heartbeat", args, || handle_heartbeat(args)),
        "kanban_comment" => handler("kanban_comment", args, || handle_comment(args)),
        "kanban_attach" => handler("kanban_attach", args, || handle_attach(args)),
        "kanban_attach_url" => handler("kanban_attach_url", args, || handle_attach_url(args)),
        "kanban_attachments" => handler("kanban_attachments", args, || handle_attachments(args)),
        "kanban_create" => handler("kanban_create", args, || handle_create(args)),
        "kanban_unblock" => handler("kanban_unblock", args, || handle_unblock(args)),
        "kanban_link" => handler("kanban_link", args, || handle_link(args)),
        other => tool_error(format!("Unknown tool: {other}")),
    }
}

// --- Small text helpers ---

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .map_err(|e| e.to_string())
}

fn url_decode(input: &str) -> String {
    crate::web_extract::percent_decode_url(input)
}

/// The `(description, parameters-schema)` pair for a kanban tool's ToolSpec,
/// extracted from the byte-frozen schema built above.
pub fn spec_parts(name: &str) -> Option<(String, Value)> {
    let schema = schema_for(name);
    let description = schema.get("__tool_description__")?.as_str()?.to_string();
    let mut parameters = schema.clone();
    if let Some(obj) = parameters.as_object_mut() {
        obj.remove("__tool_description__");
    }
    Some((description, parameters))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Env-mutating tests run under one lock (registry tests share the process).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Fresh `HERMES_HOME` in a tempdir + cleared kanban worker envs, restored
    /// on drop. Holds the env lock for the test's lifetime.
    struct Env {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        home: PathBuf,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl Env {
        fn fresh() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir_all(&home).unwrap();
            let mut saved = Vec::new();
            std::env::set_var("HERMES_HOME", &home);
            for key in [
                "HERMES_KANBAN_TASK",
                "HERMES_KANBAN_RUN_ID",
                "HERMES_KANBAN_TOOLSET",
                "HERMES_KANBAN_DB",
                "HERMES_KANBAN_BOARD",
                "HERMES_DELEGATED_CHILD_CONTEXT",
                "HERMES_KANBAN_CLAIM_LOCK",
                "HERMES_SESSION_ID",
            ] {
                saved.push((key, std::env::var(key).ok()));
                std::env::remove_var(key);
            }
            Env { _lock: lock, _dir: dir, home, saved }
        }

        fn board_path(&self) -> PathBuf {
            self.home.join("kanban/boards/default/board.json")
        }

        fn board(&self) -> Board {
            Board::load(self.board_path()).unwrap()
        }

        /// Force a task into a live running claim (dispatcher spawn state).
        fn task_running(&self, tid: &str, run_id: i64) {
            let mut board = self.board();
            {
                let task = board.file.tasks.iter_mut().find(|t| t.id == tid).unwrap();
                task.status = "running".into();
                task.current_run_id = Some(run_id);
                task.claim_lock = Some("host:1".into());
                board.file.counters.run = run_id;
                board
                    .file
                    .runs
                    .push(kanban_state::Run {
                        id: run_id,
                        task_id: tid.into(),
                        status: "running".into(),
                        ..Default::default()
                    });
            }
            board.save().unwrap();
        }

        fn set(key: &str, value: &str) {
            std::env::set_var(key, value);
        }

        fn remove(key: &str) {
            std::env::remove_var(key);
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn parse(out: &str) -> Value {
        serde_json::from_str(out).unwrap_or_else(|e| panic!("non-JSON output: {e}: {out}"))
    }

    #[test]
    fn board_state_round_trips_via_tempfile() {
        let env = Env::fresh();
        let out = execute("kanban_create", &json!({ "title": "Port it", "assignee": "dev" }));
        let v = parse(&out);
        assert_eq!(v["ok"], true, "{out}");
        let tid = v["task_id"].as_str().unwrap().to_string();
        assert!(tid.starts_with("t_"));
        // The board document persisted to the default board under HERMES_HOME.
        let board = env.board();
        let task = board.get_task(&tid).unwrap();
        assert_eq!(task.title, "Port it");
        assert_eq!(task.assignee.as_deref(), Some("dev"));
        assert_eq!(task.status, "ready");
        assert_eq!(task.created_by.as_deref(), Some("worker"));
        // kanban_list sees it, with the summary shape.
        let out = execute("kanban_list", &json!({}));
        let v = parse(&out);
        assert_eq!(v["count"], 1, "{out}");
        assert_eq!(v["tasks"][0]["id"], json!(tid));
        assert_eq!(v["tasks"][0]["parent_count"], 0);
        assert_eq!(v["truncated"], false);
    }

    #[test]
    fn worker_ownership_is_enforced() {
        let _env = Env::fresh();
        // A dispatcher worker scoped to t_other may not mutate a sibling task.
        Env::set("HERMES_KANBAN_TASK", "t_other");
        let out = execute(
            "kanban_complete",
            &json!({ "task_id": "t_sibling", "summary": "done" }),
        );
        assert!(
            out.contains("worker is scoped to task t_other; refusing to mutate t_sibling"),
            "{out}"
        );
        assert!(out.contains("kanban_comment"), "{out}");
        // Run-lifecycle tools require a resolvable run id (fail closed).
        let out = execute("kanban_complete", &json!({ "summary": "done" }));
        assert!(
            out.contains("kanban_complete refused: this worker cannot resolve its HERMES_KANBAN_RUN_ID"),
            "{out}"
        );
        // Non-lifecycle tools are NOT run-id gated.
        let out = execute("kanban_heartbeat", &json!({}));
        assert!(out.contains("could not heartbeat t_other"), "{out}");
    }

    #[test]
    fn task_id_defaulting() {
        let _env = Env::fresh();
        // No dispatcher task and no explicit id: kanban_show answers instead of erroring.
        let out = execute("kanban_show", &json!({}));
        let v = parse(&out);
        assert_eq!(v["current_task"], serde_json::Value::Null);
        assert!(v["hint"].as_str().unwrap().contains("kanban_list"));
        // With the env task set, the default resolves.
        Env::set("HERMES_KANBAN_TASK", "t_12345678");
        let out = execute("kanban_show", &json!({}));
        assert!(out.contains("task t_12345678 not found"), "{out}");
    }

    #[test]
    fn kanban_complete_contract() {
        let env = Env::fresh();
        // Unknown-parameter check comes first.
        let out = execute("kanban_complete", &json!({ "bogus": 1 }));
        assert!(out.contains("kanban_complete: unknown parameter(s): bogus"), "{out}");
        let out = execute("kanban_complete", &json!({}));
        assert!(out.contains("task_id is required"), "{out}");
        // Create + run a card, then complete it with structured evidence.
        let out = execute("kanban_create", &json!({ "title": "Work", "assignee": "dev" }));
        let tid = parse(&out)["task_id"].as_str().unwrap().to_string();
        env.task_running(&tid, 7);
        // The dispatcher pins these at spawn; pin them here so the defaulting
        // path resolves to this card.
        Env::set("HERMES_KANBAN_TASK", &tid);
        Env::set("HERMES_KANBAN_RUN_ID", "7");
        let out = execute(
            "kanban_complete",
            &json!({ "summary": "did the thing", "metadata": { "tests_run": 3 }, "created_cards": ["t_phantom1"] }),
        );
        assert!(
            out.contains("kanban_complete blocked: the following created_cards do not exist"),
            "{out}"
        );
        let out = execute(
            "kanban_complete",
            &json!({ "summary": "did the thing", "metadata": { "tests_run": 3 } }),
        );
        let v = parse(&out);
        assert_eq!(v["ok"], true, "{out}");
        assert_eq!(v["run_id"], 7);
        assert_eq!(env.board().get_task(&tid).unwrap().status, "done");
        // A completion with no evidence is refused (summary/result requirement).
        let out = execute("kanban_create", &json!({ "title": "Two", "assignee": "dev" }));
        let tid2 = parse(&out)["task_id"].as_str().unwrap().to_string();
        let out = execute("kanban_complete", &json!({ "task_id": tid2, "metadata": {} }));
        assert!(
            out.contains("provide at least one of: summary (preferred), result"),
            "{out}"
        );
    }

    #[test]
    fn dependency_gating_and_promotion() {
        let env = Env::fresh();
        let parent = parse(&execute("kanban_create", &json!({ "title": "P", "assignee": "dev" })))
            ["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        let out = execute(
            "kanban_create",
            &json!({ "title": "C", "assignee": "dev", "parents": [parent] }),
        );
        let v = parse(&out);
        let child = v["task_id"].as_str().unwrap().to_string();
        assert_eq!(v["gated"], true);
        assert_eq!(v["gated_by"], json!(parent));
        assert_eq!(env.board().get_task(&child).unwrap().status, "todo");
        // The child cannot complete before its parent is done.
        let out = execute("kanban_complete", &json!({ "task_id": child, "summary": "s" }));
        assert!(out.contains("unsatisfied parent dependencies"), "{out}");
        // Completing the parent promotes the child to ready (recompute_ready).
        env.task_running(&parent, 5);
        let out = execute("kanban_complete", &json!({ "task_id": parent, "summary": "parent done" }));
        assert_eq!(parse(&out)["ok"], true, "{out}");
        assert_eq!(env.board().get_task(&child).unwrap().status, "ready");
        // kanban_link: self-links and cycles rejected.
        let out = execute("kanban_link", &json!({ "parent_id": child, "child_id": child }));
        assert!(out.contains("a task cannot depend on itself"), "{out}");
        let out = execute("kanban_link", &json!({ "parent_id": child, "child_id": parent }));
        assert!(out.contains("would create a cycle"), "{out}");
    }

    #[test]
    fn kanban_block_routing() {
        let env = Env::fresh();
        let out = execute("kanban_block", &json!({ "task_id": "t_missing" }));
        assert!(out.contains("reason is required — explain what input you need"), "{out}");
        let out = execute("kanban_create", &json!({ "title": "B", "assignee": "dev" }));
        let tid = parse(&out)["task_id"].as_str().unwrap().to_string();
        // kind validation.
        let out = execute("kanban_block", &json!({ "task_id": tid, "reason": "r", "kind": "bogus" }));
        assert!(
            out.contains(
                "kind must be one of ['capability', 'dependency', 'needs_input', 'transient'] (or omit it)"
            ),
            "{out}"
        );
        // dependency with no open parent re-kinds to needs_input (sticky).
        let out = execute("kanban_block", &json!({ "task_id": tid, "reason": "waiting", "kind": "dependency" }));
        let v = parse(&out);
        assert_eq!(v["status"], "blocked", "{out}");
        assert_eq!(v["block_kind"], "needs_input");
        assert_eq!(v["requested_kind"], "dependency");
        assert!(v["note"].as_str().unwrap().contains("no parent is open"));
        // A sticky block leaves only via kanban_unblock.
        let out = execute("kanban_unblock", &json!({ "task_id": tid }));
        assert_eq!(parse(&out)["status"], "ready");
    }

    #[test]
    fn kanban_schedule_and_landed_status() {
        let _env = Env::fresh();
        let out = execute("kanban_schedule", &json!({ "task_id": "t_missing" }));
        assert!(
            out.contains("could not schedule t_missing (unknown id or not in todo/ready/running/blocked)"),
            "{out}"
        );
        let out = execute("kanban_create", &json!({ "title": "S", "assignee": "dev" }));
        let tid = parse(&out)["task_id"].as_str().unwrap().to_string();
        let out = execute(
            "kanban_schedule",
            &json!({ "task_id": tid, "reason": "SCHEDULED_UNTIL=2026-01-01" }),
        );
        let v = parse(&out);
        assert_eq!(v["status"], "scheduled", "{out}");
        assert_eq!(v["reason"], "SCHEDULED_UNTIL=2026-01-01");
    }

    #[test]
    fn review_handoff_round_trip() {
        let env = Env::fresh();
        let out = execute("kanban_request_review", &json!({ "task_id": "t_missing" }));
        assert!(out.contains("summary is required — describe what was implemented"), "{out}");
        let out = execute("kanban_create", &json!({ "title": "R", "assignee": "dev" }));
        let tid = parse(&out)["task_id"].as_str().unwrap().to_string();
        env.task_running(&tid, 11);
        Env::set("HERMES_KANBAN_TASK", &tid);
        Env::set("HERMES_KANBAN_RUN_ID", "11");
        let out = execute("kanban_request_review", &json!({ "summary": "implemented + verified" }));
        let v = parse(&out);
        assert_eq!(v["status"], "review", "{out}");
        assert_eq!(v["run_id"], 11);
        // Reviewer-profile check.
        let out = execute("kanban_request_review", &json!({ "reviewer": "ghost" }));
        assert!(out.contains("reviewer profile 'ghost' is not installed"), "{out}");
    }

    #[test]
    fn kanban_comment_contract() {
        let env = Env::fresh();
        let out = execute("kanban_comment", &json!({}));
        assert!(out.contains("task_id is required: comments are per-task"), "{out}");
        let out = execute("kanban_comment", &json!({ "task_id": "t_1" }));
        assert!(out.contains("body is required"), "{out}");
        let out = execute("kanban_create", &json!({ "title": "C", "assignee": "dev" }));
        let tid = parse(&out)["task_id"].as_str().unwrap().to_string();
        let out = execute("kanban_comment", &json!({ "task_id": tid, "body": "handoff note" }));
        let v = parse(&out);
        assert_eq!(v["ok"], true, "{out}");
        let board = env.board();
        let comments = board.list_comments(&tid);
        assert_eq!(comments.len(), 1);
        // The author is the persisted identity, never caller-supplied (#19713).
        assert_eq!(comments[0].author.as_deref(), Some("worker"));
        // kanban_show renders the full state incl. comments and worker context.
        let out = execute("kanban_show", &json!({ "task_id": tid }));
        let v = parse(&out);
        assert_eq!(v["task"]["id"], json!(tid));
        assert_eq!(v["comments"][0]["body"], "handoff note");
        assert!(v["worker_context"].as_str().unwrap().contains("handoff note"));
    }

    #[test]
    fn attach_and_attachments() {
        let env = Env::fresh();
        let out = execute("kanban_attach", &json!({ "filename": "a.txt" }));
        assert!(out.contains("content_base64 is not valid base64"), "{out}");
        let out = execute("kanban_create", &json!({ "title": "A", "assignee": "dev" }));
        let tid = parse(&out)["task_id"].as_str().unwrap().to_string();
        let out = execute(
            "kanban_attach",
            &json!({ "task_id": tid, "filename": "dir/report.txt", "content_base64": "aGVsbG8=" }),
        );
        let v = parse(&out);
        assert_eq!(v["size"], 5, "{out}");
        // The blob landed in the board's per-task attachments dir; leaf name kept.
        let board = env.board();
        let stored = board.list_attachments(&tid)[0];
        assert!(stored.filename.ends_with("report.txt"), "{}", stored.filename);
        assert!(std::path::Path::new(&stored.stored_path).exists());
        // kanban_attachments lists them read-only.
        let out = execute("kanban_attachments", &json!({ "task_id": tid }));
        assert_eq!(parse(&out)["attachments"].as_array().unwrap().len(), 1);
        // attach_url: scheme validation is offline-testable.
        let out = execute("kanban_attach_url", &json!({ "task_id": tid, "url": "file:///tmp/x" }));
        assert!(out.contains("kanban_attach_url: unsupported URL scheme"), "{out}");
    }

    #[test]
    fn list_filters_and_limits() {
        let _env = Env::fresh();
        let out = execute("kanban_list", &json!({ "limit": 0 }));
        assert!(out.contains("limit must be >= 1"), "{out}");
        let out = execute("kanban_list", &json!({ "limit": 201 }));
        assert!(out.contains("limit must be <= 200"), "{out}");
        let out = execute("kanban_list", &json!({ "limit": "many" }));
        assert!(out.contains("limit must be an integer"), "{out}");
        let out = execute("kanban_list", &json!({ "status": "bogus" }));
        assert!(
            out.contains(
                "status must be one of ['archived', 'blocked', 'done', 'ready', 'review', 'running', 'scheduled', 'todo', 'triage']"
            ),
            "{out}"
        );
        // Truncation metadata: 3 tasks with limit 2 → truncated with next_limit.
        for i in 0..3 {
            execute("kanban_create", &json!({ "title": format!("t{i}"), "assignee": "dev" }));
        }
        let out = execute("kanban_list", &json!({ "limit": 2 }));
        let v = parse(&out);
        assert_eq!(v["count"], 2);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["next_limit"], 4);
        // Assignee filter excludes.
        let out = execute("kanban_list", &json!({ "assignee": "nobody" }));
        assert_eq!(parse(&out)["count"], 0);
    }

    #[test]
    fn create_contract() {
        let _env = Env::fresh();
        let out = execute("kanban_create", &json!({ "assignee": "dev" }));
        assert!(out.contains("title is required"), "{out}");
        let out = execute("kanban_create", &json!({ "title": "x" }));
        assert!(
            out.contains("assignee is required — name the profile that should execute this task"),
            "{out}"
        );
        let out = execute("kanban_create", &json!({ "title": "x", "assignee": "dev", "provider": "nous" }));
        assert!(out.contains("'provider' requires 'model' to be set as well"), "{out}");
        let out = execute("kanban_create", &json!({ "title": "x", "assignee": "dev", "parents": ["t_ffffffff"] }));
        assert!(out.contains("unknown parent task(s): t_ffffffff"), "{out}");
        // Idempotency key returns the original id.
        let first = parse(&execute(
            "kanban_create",
            &json!({ "title": "i", "assignee": "dev", "idempotency_key": "k1" }),
        ))["task_id"].as_str().unwrap().to_string();
        let second = parse(&execute(
            "kanban_create",
            &json!({ "title": "i2", "assignee": "dev", "idempotency_key": "k1" }),
        ))["task_id"].as_str().unwrap().to_string();
        assert_eq!(first, second);
    }

    #[test]
    fn orchestrator_tools_refuse_workers() {
        let _env = Env::fresh();
        Env::set("HERMES_KANBAN_TASK", "t_worker");
        for tool in ["kanban_list", "kanban_unblock"] {
            let out = execute(tool, &json!({}));
            assert!(
                out.contains("is orchestrator-only; dispatcher-spawned workers must use"),
                "{tool}: {out}"
            );
        }
    }

    #[test]
    fn mode_gating_on_and_off() {
        let _env = Env::fresh();
        // Default: no toolset, no dispatcher task → nothing visible.
        assert!(!kanban_mode_visible());
        assert!(!kanban_orchestrator_mode_visible());
        // Profile toolset opt-in exposes both.
        Env::set("HERMES_KANBAN_TOOLSET", "1");
        assert!(kanban_mode_visible());
        assert!(kanban_orchestrator_mode_visible());
        Env::remove("HERMES_KANBAN_TOOLSET");
        // Dispatcher worker: lifecycle yes, board-routing no.
        Env::set("HERMES_KANBAN_TASK", "t_w");
        assert!(kanban_mode_visible());
        assert!(!kanban_orchestrator_mode_visible());
        Env::remove("HERMES_KANBAN_TASK");
        // Delegate children never see kanban tools.
        Env::set("HERMES_DELEGATED_CHILD_CONTEXT", "1");
        assert!(!kanban_mode_visible());
        Env::remove("HERMES_DELEGATED_CHILD_CONTEXT");
    }

    #[test]
    fn inter_tool_schema_consistency() {
        // The 15 tools, in registration order, each with an optional trailing
        // `board` param and required ⊆ properties.
        assert_eq!(KANBAN_TOOL_NAMES.len(), 15);
        let expected_required: &[(&str, &[&str])] = &[
            ("kanban_show", &[]),
            ("kanban_list", &[]),
            ("kanban_complete", &[]),
            ("kanban_block", &["reason"]),
            ("kanban_schedule", &[]),
            ("kanban_request_review", &["summary"]),
            ("kanban_request_changes", &["reason"]),
            ("kanban_heartbeat", &[]),
            ("kanban_comment", &["task_id", "body"]),
            ("kanban_attach", &["filename", "content_base64"]),
            ("kanban_attach_url", &["url"]),
            ("kanban_attachments", &[]),
            ("kanban_create", &["title", "assignee"]),
            ("kanban_unblock", &["task_id"]),
            ("kanban_link", &["parent_id", "child_id"]),
        ];
        for (name, required) in expected_required {
            let (description, schema) = spec_parts(name).unwrap_or_else(|| panic!("{name} schema"));
            assert!(!description.is_empty());
            let props = schema["properties"].as_object().unwrap();
            assert!(props.contains_key("board"), "{name}: trailing board param");
            let req: Vec<&str> = schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_str().unwrap())
                .collect();
            assert_eq!(req, required.to_vec(), "{name} required");
            for r in req {
                assert!(props.contains_key(r), "{name}: required {r} not declared");
            }
        }
    }
}
