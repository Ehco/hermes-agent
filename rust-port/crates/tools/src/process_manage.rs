//! process_manage — poll, wait on, or kill background terminal processes.
//!
//! Port of `tools/process_registry.py`'s `process_manage` tool surface: the
//! action enum (list/poll/log/wait/kill/write/submit/close/handoff), the
//! write-vs-submit trap (submit appends Enter; a lone `\n` on a Windows PTY is
//! not Enter) and the "completed results stay retrievable by session_id"
//! retention behavior, expressed with a metadata + rolling-log file per
//! session under `<hermes home>/cache/process_registry/`.
//!
//! Spawn happens through `terminal(background=true)` (Python's only spawn path
//! too); this module owns the registry half. The subagent-only `handoff`
//! action returns the Python error text — the Rust delegation intercept lives
//! in the agent runtime, not in this registry.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::util::tool_error;

/// Finished sessions are kept for this long (Python `FINISHED_TTL_SECONDS`
/// ≈ 7 days); the newest 64 results per profile are the practical cap.
const FINISHED_TTL_SECONDS: u64 = 7 * 24 * 3600;
/// Log lines returned by `log` by default (Python `read_log` default).
const DEFAULT_LOG_LIMIT: usize = 200;
/// `wait` polls at this cadence.
const WAIT_POLL_MS: u64 = 250;

/// A running child: the OS process plus its piped stdin (write/submit/close).
struct ChildHandle {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
}

fn running() -> &'static Mutex<HashMap<String, Arc<Mutex<ChildHandle>>>> {
    static RUNNING: OnceLock<Mutex<HashMap<String, Arc<Mutex<ChildHandle>>>>> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-session consumed output offset for `poll` ("new output since last poll").
fn read_offsets() -> &'static Mutex<HashMap<String, u64>> {
    static OFFSETS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    OFFSETS.get_or_init(|| Mutex::new(HashMap::new()))
}

static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

/// Sessions root: `$HERMES_DIR`, else `~/.hermes`, plus `cache/process_registry`.
pub fn sessions_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("HERMES_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir).join("cache").join("process_registry");
        }
    }
    let home = std::env::var("HERMES_HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".hermes")
        });
    home.join("cache").join("process_registry")
}

fn session_file(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.json"))
}

fn log_file(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.log"))
}

fn write_meta(dir: &Path, meta: &Value) {
    let id = meta["session_id"].as_str().unwrap_or_default();
    if id.is_empty() {
        return;
    }
    let tmp = dir.join(format!("{id}.json.tmp"));
    if std::fs::write(&tmp, meta.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, session_file(dir, id));
    }
}

fn read_meta(dir: &Path, session_id: &str) -> Option<Value> {
    let raw = std::fs::read(session_file(dir, session_id)).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn is_running(session_id: &str) -> bool {
    running().lock().map(|r| r.contains_key(session_id)).unwrap_or(false)
}

/// Execute `process_manage` — the registry entry point.
pub fn execute(args: &Value) -> String {
    execute_in(&sessions_dir(), args)
}

/// `execute` against an explicit sessions dir (tests, embedding hosts).
pub fn execute_in(dir: &Path, args: &Value) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
    // Coerce to string — some models send session_id as an integer (Python does str() too).
    let session_id = args
        .get("session_id")
        .map(|v| match v {
            Value::String(s) => s.trim().to_string(),
            other => other.to_string().trim_matches('"').to_string(),
        })
        .unwrap_or_default();

    match action.as_str() {
        "list" => Ok(list(dir)),
        "poll" => require_session(&session_id, &action).and_then(|sid| poll(dir, &sid)),
        "log" => require_session(&session_id, &action).and_then(|sid| log(dir, &sid, args)),
        "wait" => require_session(&session_id, &action).and_then(|sid| wait(dir, &sid, args)),
        "kill" => require_session(&session_id, &action).and_then(|sid| kill(dir, &sid)),
        "write" => require_session(&session_id, &action)
            .and_then(|sid| write_stdin(&sid, args, false)),
        "submit" => require_session(&session_id, &action)
            .and_then(|sid| write_stdin(&sid, args, true)),
        "close" => require_session(&session_id, &action).and_then(|sid| close_stdin(&sid)),
        "handoff" => {
            if session_id.is_empty() {
                Err(tool_error("session_id is required for handoff"))
            } else {
                // Subagent-only in Python too; the Rust delegation intercept
                // (agent runtime) would intercept this before the registry.
                Err(tool_error(
                    "handoff is only available to a running subagent with a live parent; you are not one.",
                ))
            }
        }
        other => Ok(tool_error(format!(
            "Unknown process action: {other}. Use: list, poll, log, wait, kill, write, submit, close, handoff"
        ))),
    }
    .unwrap_or_else(|e| e)
}

fn require_session(session_id: &str, action: &str) -> Result<String, String> {
    if session_id.is_empty() {
        Err(tool_error(format!("session_id is required for {action}")))
    } else {
        Ok(session_id.to_string())
    }
}

fn err_json(msg: &str) -> String {
    tool_error(msg)
}

fn meta_with_live_status(dir: &Path, meta: &mut Value) {
    let id = meta["session_id"].as_str().unwrap_or_default().to_string();
    if is_running(&id) {
        return;
    }
    // Not in the live map: either finished (metadata has the truth) or the
    // host process restarted while the child was running (Python's in-memory
    // Popen dies the same way). Mark it so the model isn't told "running" for
    // something nobody owns anymore.
    if meta["status"] == "running" {
        meta["status"] = "exited".into();
        meta["exit_code"] = Value::Null;
        write_meta(dir, meta);
    }
}

fn list(dir: &Path) -> String {
    let mut rows: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(mut meta) =
                    serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap_or_default())
                {
                    meta_with_live_status(dir, &mut meta);
                    rows.push(meta);
                }
            }
        }
    }
    rows.sort_by_key(|m| m["started_at"].as_i64().unwrap_or(0));
    json!({ "processes": rows }).to_string()
}

/// New output since the last poll (`read_offsets` bookkeeping), with status.
fn poll(dir: &Path, session_id: &str) -> Result<String, String> {
    let Some(mut meta) = read_meta(dir, session_id) else {
        return Err(err_json(&format!(
            "Unknown session_id '{session_id}' (already pruned, or from another profile). Use action='list'."
        )));
    };
    meta_with_live_status(dir, &mut meta);
    let log_path = log_file(dir, session_id);
    let total_len = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
    let offset = read_offsets()
        .lock()
        .map(|o| o.get(session_id).copied().unwrap_or(0))
        .unwrap_or(0);
    let mut output = String::new();
    if offset < total_len {
        if let Ok(bytes) = std::fs::read(&log_path) {
            let start = (offset as usize).min(bytes.len());
            output = String::from_utf8_lossy(&bytes[start..]).to_string();
        }
    }
    if read_offsets().lock().is_ok() {
        read_offsets()
            .lock()
            .unwrap()
            .insert(session_id.to_string(), total_len);
    }
    json!({
        "session_id": session_id,
        "status": meta["status"],
        "exit_code": meta.get("exit_code").cloned().unwrap_or(Value::Null),
        "command": meta["command"],
        "output": output,
    })
    .to_string()
    .pipe_ok()
}

/// Full output, paged by line (Python `read_log(offset=…, limit=…)`).
fn log(dir: &Path, session_id: &str, args: &Value) -> Result<String, String> {
    let Some(meta) = read_meta(dir, session_id) else {
        return Err(err_json(&format!(
            "Unknown session_id '{session_id}' (already pruned, or from another profile). Use action='list'."
        )));
    };
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_LOG_LIMIT as u64)
        .clamp(1, 2000) as usize;
    let content = std::fs::read_to_string(log_file(dir, session_id)).unwrap_or_default();
    let lines: Vec<&str> = content.lines().collect();
    let window: Vec<&str> = lines
        .iter()
        .skip(offset)
        .take(limit)
        .copied()
        .collect();
    let total = lines.len();
    json!({
        "session_id": session_id,
        "status": meta["status"],
        "exit_code": meta.get("exit_code").cloned().unwrap_or(Value::Null),
        "offset": offset,
        "limit": limit,
        "total_lines": total,
        "output": window.join("\n"),
    })
    .to_string()
    .pipe_ok()
}

/// Block until exit or timeout (partial output on timeout, like Python).
fn wait(dir: &Path, session_id: &str, args: &Value) -> Result<String, String> {
    let Some(_meta) = read_meta(dir, session_id) else {
        return Err(err_json(&format!(
            "Unknown session_id '{session_id}' (already pruned, or from another profile). Use action='list'."
        )));
    };
    let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(0);
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout);
    loop {
        if !is_running(session_id) {
            break;
        }
        if timeout > 0 && std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(WAIT_POLL_MS));
    }
    let Some(mut meta) = read_meta(dir, session_id) else {
        return Err(err_json("session vanished while waiting"));
    };
    meta_with_live_status(dir, &mut meta);
    // Log tail (bounded, like Python's rolling output tail).
    let content = std::fs::read_to_string(log_file(dir, session_id)).unwrap_or_default();
    let tail: String = {
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(DEFAULT_LOG_LIMIT);
        lines[start..].join("\n")
    };
    json!({
        "session_id": session_id,
        "status": meta["status"],
        "exit_code": meta.get("exit_code").cloned().unwrap_or(Value::Null),
        "timed_out": is_running(session_id),
        "output": tail,
    })
    .to_string()
    .pipe_ok()
}

fn kill(dir: &Path, session_id: &str) -> Result<String, String> {
    let Some(meta) = read_meta(dir, session_id) else {
        return Err(err_json(&format!(
            "Unknown session_id '{session_id}' (already pruned, or from another profile). Use action='list'."
        )));
    };
    let handle = running().lock().unwrap().remove(session_id);
    match handle {
        Some(handle) => {
            let mut child = handle.lock().unwrap();
            let _ = child.child.kill();
            let _ = child.child.wait();
            drop(child);
            let mut meta = meta;
            meta["status"] = "killed".into();
            write_meta(dir, &meta);
            json!({ "session_id": session_id, "status": "killed" }).to_string().pipe_ok()
        }
        None => {
            let status = meta["status"].as_str().unwrap_or("exited");
            Err(err_json(&format!(
                "cannot kill {session_id}: already exited (status {status}); read its result with poll/log."
            )))
        }
    }
}

/// `write` sends raw bytes with no newline; `submit` appends Enter — the one
/// real trap: a lone `\n` on a Windows PTY is not delivered as a line
/// terminator, so submit writes `\r\n` on Windows (Python #95681 note).
fn write_stdin(session_id: &str, args: &Value, submit: bool) -> Result<String, String> {
    let data = args.get("data").and_then(Value::as_str).unwrap_or("");
    let handle = running().lock().unwrap().get(session_id).cloned();
    let Some(handle) = handle else {
        return Err(err_json(&format!(
            "cannot write to {session_id}: not a running process (already exited? use poll/log)."
        )));
    };
    let mut child = handle.lock().unwrap();
    let Some(stdin) = child.stdin.as_mut() else {
        return Err(err_json(&format!(
            "cannot write to {session_id}: stdin is closed (action='close' was used)."
        )));
    };
    if stdin.write_all(data.as_bytes()).is_err() {
        return Err(err_json(&format!("failed to write to {session_id}: broken pipe")));
    }
    if submit {
        #[cfg(windows)]
        let terminator = "\r\n";
        #[cfg(not(windows))]
        let terminator = "\n";
        let _ = stdin.write_all(terminator.as_bytes());
    }
    let _ = stdin.flush();
    json!({ "session_id": session_id, "status": if submit { "submitted" } else { "written" } })
        .to_string()
        .pipe_ok()
}

/// EOF stdin: the child keeps running but no longer accepts input.
fn close_stdin(session_id: &str) -> Result<String, String> {
    let handle = running().lock().unwrap().get(session_id).cloned();
    let Some(handle) = handle else {
        return Err(err_json(&format!(
            "cannot close {session_id}: not a running process (already exited? use poll/log)."
        )));
    };
    let mut child = handle.lock().unwrap();
    child.stdin = None; // drop -> EOF
    json!({ "session_id": session_id, "status": "stdin_closed" }).to_string().pipe_ok()
}

/// Spawn a detached background process (the `terminal(background=true)` path).
/// The child's output appends to `<session_id>.log`; a monitor thread records
/// the exit status in the metadata file. Returns the Python spawn envelope.
pub fn spawn_background(command: &str, cwd: Option<&str>) -> String {
    spawn_background_in(&sessions_dir(), command, cwd)
}

pub fn spawn_background_in(dir: &Path, command: &str, cwd: Option<&str>) -> String {
    let _ = std::fs::create_dir_all(dir);
    let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let id = format!(
        "proc_{}{:x}",
        crate::web_common::sha256_hex10(format!("{command}{nanos}").as_bytes()),
        seq & 0xffff
    );

    let mut cmd = bg_shell_command(command);
    if let Some(dir_cwd) = cwd.filter(|c| !c.trim().is_empty()) {
        cmd.current_dir(dir_cwd);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window popping up on the user's desktop.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return tool_error(format!("failed to spawn background command: {e}")),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdin = child.stdin.take();

    let log_path = log_file(dir, &id);
    let log_for_stdout = log_path.clone();
    if let Some(out) = stdout {
        std::thread::spawn(move || pump_to_log(out, &log_for_stdout));
    }
    if let Some(err_stream) = stderr {
        std::thread::spawn(move || pump_to_log(err_stream, &log_path));
    }

    let meta = json!({
        "session_id": id,
        "command": command,
        "cwd": cwd.unwrap_or(""),
        "started_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0),
        "status": "running",
        "exit_code": Value::Null,
        "ttl_seconds": FINISHED_TTL_SECONDS,
    });
    write_meta(dir, &meta);

    let handle = Arc::new(Mutex::new(ChildHandle { child, stdin }));
    running().lock().unwrap().insert(id.clone(), handle.clone());

    // Monitor: mark exit in metadata, release the OS handles, drop from the
    // live map (Python `_move_to_finished`).
    let meta_path_dir = dir.to_path_buf();
    let monitor_id = id.clone();
    std::thread::spawn(move || {
        loop {
            let exited = {
                let live = running().lock().unwrap();
                match live.get(&monitor_id) {
                    Some(h) => {
                        let mut child = h.lock().unwrap();
                        match child.child.try_wait() {
                            Ok(Some(status)) => status.code(),
                            Ok(None) => None,
                            Err(_) => Some(-1),
                        }
                    }
                    None => break, // killed by the tool: metadata already final
                }
            };
            match exited {
                None => std::thread::sleep(Duration::from_millis(WAIT_POLL_MS)),
                Some(code) => {
                    running().lock().unwrap().remove(&monitor_id);
                    if let Some(mut meta) = read_meta(&meta_path_dir, &monitor_id) {
                        meta["status"] = "exited".into();
                        meta["exit_code"] = json!(code);
                        write_meta(&meta_path_dir, &meta);
                    }
                    break;
                }
            }
        }
    });

    json!({
        "session_id": id,
        "status": "running",
        "command": command,
        "message": "Running in the background. Poll, read the log, wait, or kill it with process_manage; the session_id retrieves completed results later.",
    })
    .to_string()
}

/// Append everything a stream produces to the session log file.
fn pump_to_log<R: Read + Send>(mut stream: R, log_path: &Path) {
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
                    let _ = f.write_all(&buf[..n]);
                }
            }
        }
    }
}

/// The shell wrapper for background spawns. Same argv/args mapping as the
/// terminal tool's `shell_command`, but returns a `std::process::Command` so
/// the registry's plain-`Read` pumping threads own the pipes.
fn bg_shell_command(command: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("cmd.exe");
        cmd.arg("/C").arg(command);
        // No console window popping up on the user's desktop.
        cmd.creation_flags(0x0800_0000);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

/// `Ok(json!(…).to_string())` — every action returns a JSON string result.
trait PipeOk {
    fn pipe_ok(self) -> Result<String, String>;
}

impl PipeOk for String {
    fn pipe_ok(self) -> Result<String, String> {
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-procmgr-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn unknown_action_and_missing_session_id() {
        let dir = tmp_dir();
        let out = v(&execute_in(&dir, &json!({"action": "frobnicate"})));
        assert!(out["error"].as_str().unwrap().contains("Unknown process action"));
        let out = v(&execute_in(&dir, &json!({"action": "poll"})));
        assert!(out["error"].as_str().unwrap().contains("session_id is required for poll"));
        let out = v(&execute_in(&dir, &json!({"action": "handoff"})));
        assert!(out["error"].as_str().unwrap().contains("session_id is required for handoff"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_session_is_an_error_not_a_panic() {
        let dir = tmp_dir();
        let out = v(&execute_in(&dir, &json!({"action": "poll", "session_id": "proc_nope"})));
        assert!(out["error"].as_str().unwrap().contains("Unknown session_id"));
        let out = v(&execute_in(&dir, &json!({"action": "kill", "session_id": "proc_nope"})));
        assert!(out["error"].as_str().unwrap().contains("Unknown session_id"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handoff_is_subagent_only() {
        let dir = tmp_dir();
        let out = v(&execute_in(&dir, &json!({"action": "handoff", "session_id": "proc_x"})));
        assert!(out["error"].as_str().unwrap().contains("only available to a running subagent"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_wait_and_collect_completed_result() {
        let dir = tmp_dir();
        #[cfg(windows)]
        let spawned = spawn_background_in(&dir, "echo hello-bg", None);
        #[cfg(not(windows))]
        let spawned = spawn_background_in(&dir, "echo hello-bg", None);
        let spawned = v(&spawned);
        assert_eq!(spawned["status"], "running");
        let sid = spawned["session_id"].as_str().unwrap().to_string();

        let waited = v(&execute_in(&dir, &json!({"action": "wait", "session_id": sid, "timeout": 15})));
        assert_eq!(waited["status"], "exited", "{waited}");
        assert_eq!(waited["exit_code"], 0);
        assert_eq!(waited["timed_out"], false);
        assert!(waited["output"].as_str().unwrap().contains("hello-bg"), "{waited}");

        // Completed results stay retrievable by session_id (the retention contract).
        let log = v(&execute_in(&dir, &json!({"action": "log", "session_id": sid})));
        assert!(log["output"].as_str().unwrap().contains("hello-bg"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_paging_is_line_offset_and_limit() {
        let dir = tmp_dir();
        // Three echos = three log lines, in order.
        #[cfg(windows)]
        let spawned = v(&spawn_background_in(&dir, "(echo a)&(echo b)&(echo c)", None));
        #[cfg(not(windows))]
        let spawned = v(&spawn_background_in(&dir, "echo a; echo b; echo c", None));
        let sid = spawned["session_id"].as_str().unwrap().to_string();
        let _ = execute_in(&dir, &json!({"action": "wait", "session_id": sid, "timeout": 15}));
        let page = v(&execute_in(&dir, &json!({"action": "log", "session_id": sid, "offset": 1, "limit": 1})));
        let out = page["output"].as_str().unwrap().trim();
        assert_eq!(out, "b", "{page}");
        assert!(page["total_lines"].as_u64().unwrap() >= 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_reports_sessions() {
        let dir = tmp_dir();
        let spawned = v(&spawn_background_in(&dir, "echo listed", None));
        let sid = spawned["session_id"].as_str().unwrap().to_string();
        let listed = v(&execute_in(&dir, &json!({"action": "list"})));
        let procs = listed["processes"].as_array().unwrap();
        assert!(procs.iter().any(|p| p["session_id"].as_str() == Some(sid.as_str())), "{listed}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn write_submit_and_kill_live_session() {
        let dir = tmp_dir();
        // `more` consumes stdin — a prompt-answering stand-in.
        let spawned = v(&spawn_background_in(&dir, "cmd /C more", None));
        let sid = spawned["session_id"].as_str().unwrap().to_string();
        let out = v(&execute_in(&dir, &json!({"action": "write", "session_id": sid, "data": "hi"})));
        assert_eq!(out["status"], "written", "{out}");
        let out = v(&execute_in(&dir, &json!({"action": "submit", "session_id": sid, "data": "hi"})));
        assert_eq!(out["status"], "submitted");
        let killed = v(&execute_in(&dir, &json!({"action": "kill", "session_id": sid})));
        assert_eq!(killed["status"], "killed", "{killed}");
        // Killing twice is a faithful "already exited" error, not a panic.
        let again = v(&execute_in(&dir, &json!({"action": "kill", "session_id": sid})));
        assert!(again["error"].as_str().unwrap().contains("already exited"), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn close_stdin_then_write_errors() {
        let dir = tmp_dir();
        let spawned = v(&spawn_background_in(&dir, "cmd /C more", None));
        let sid = spawned["session_id"].as_str().unwrap().to_string();
        let out = v(&execute_in(&dir, &json!({"action": "close", "session_id": sid})));
        assert_eq!(out["status"], "stdin_closed");
        let out = v(&execute_in(&dir, &json!({"action": "write", "session_id": sid, "data": "x"})));
        assert!(out["error"].as_str().unwrap().contains("stdin is closed"), "{out}");
        let _ = execute_in(&dir, &json!({"action": "kill", "session_id": sid}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_returns_new_output_incrementally() {
        let dir = tmp_dir();
        let spawned = v(&spawn_background_in(&dir, "echo poll-me", None));
        let sid = spawned["session_id"].as_str().unwrap().to_string();
        let _ = execute_in(&dir, &json!({"action": "wait", "session_id": sid, "timeout": 15}));
        let first = v(&execute_in(&dir, &json!({"action": "poll", "session_id": sid})));
        assert!(first["output"].as_str().unwrap().contains("poll-me"), "{first}");
        // Second poll: consumed — new output is empty even though the log has content.
        let second = v(&execute_in(&dir, &json!({"action": "poll", "session_id": sid})));
        assert_eq!(second["output"], "", "{second}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
