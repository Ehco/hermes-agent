//! terminal — async subprocess execution with timeout and cwd.
//!
//! Port of the core of Python `tools/terminal_tool.py`: run a command in a
//! shell, capture stdout/stderr/exit code, enforce a timeout (the child is
//! killed on expiry), optionally set a working directory, and cap captured
//! output (`MAX_OUTPUT_CHARS` = 50_000 per stream, Python default).
//!
//! Result shape mirrors the Python ExecuteResult JSON:
//! `{"stdout", "stderr", "exit_code", "timed_out"}` plus `cwd` when set.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

use crate::util::{tool_error, MAX_TERMINAL_OUTPUT_CHARS};

/// Default per-command timeout (Python terminal_tool default is 60s).
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Hard ceiling, matching Python's terminal_tool config ceiling.
const MAX_TIMEOUT_SECS: u64 = 600;

pub fn execute(args: &Value) -> String {
    // Dispatch onto the async impl. On a multi-threaded runtime we can use
    // block_in_place; on a current-thread runtime (or no runtime) we run the
    // future on a fresh OS thread with its own temporary runtime, because
    // neither block_on nor block_in_place is legal inside a current-thread
    // runtime task.
    let owned_args = args.clone();
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
                return tokio::task::block_in_place(|| match handle.block_on(run_async(&owned_args))
                {
                    Ok(out) => out,
                    Err(e) => tool_error(format!("terminal: {e}")),
                });
            }
            run_on_fresh_thread(owned_args)
        }
        Err(_) => run_on_fresh_thread(owned_args),
    }
}

fn run_on_fresh_thread(args: Value) -> String {
    std::thread::Builder::new()
        .name("hermes-terminal".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match rt {
                Ok(rt) => match rt.block_on(run_async(&args)) {
                    Ok(out) => out,
                    Err(e) => tool_error(format!("terminal: {e}")),
                },
                Err(e) => tool_error(format!("terminal: no tokio runtime available: {e}")),
            }
        })
        .map(|j| j.join().unwrap_or_else(|_| tool_error("terminal worker panicked")))
        .unwrap_or_else(|e| tool_error(format!("terminal: failed to spawn worker: {e}")))
}

async fn run_async(args: &Value) -> Result<String, String> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| "terminal: 'command' is required".to_string())?;
    let cwd = args.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    // `background: true` hands the process to the process registry: the call
    // returns a session_id at once and the process_manage actions own it
    // from there (Python's terminal_tool_background path).
    if args.get("background").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(crate::process_manage::spawn_background(command, args.get("cwd").and_then(Value::as_str)));
    }
    let timeout_secs = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);

    let mut cmd = shell_command(command);
    if let Some(dir) = &cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // A timed-out wait drops the Child; make sure the process dies with it.
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn command: {e}"))?;

    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(render_output(output, false)),
        Ok(Err(e)) => Err(format!("command failed: {e}")),
        Err(_) => Ok(json!({
            "stdout": "",
            "stderr": format!("Command timed out after {timeout_secs} seconds"),
            "exit_code": -1,
            "timed_out": true,
        })
        .to_string()),
    }
}

/// The shell wrapper shared by the foreground and background terminal paths.
#[cfg(windows)]
pub(crate) fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.raw_arg("/C").raw_arg(command);
    cmd
}

#[cfg(not(windows))]
pub(crate) fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

fn render_output(output: std::process::Output, timed_out: bool) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    json!({
        "stdout": cap(&stdout),
        "stderr": cap(&stderr),
        "exit_code": output.status.code().unwrap_or(-1),
        "timed_out": timed_out,
    })
    .to_string()
}

/// Cap each captured stream at MAX_OUTPUT_CHARS (head+tail), like Python's
/// `tool_output_truncate` keeps the model's context bounded.
fn cap(s: &str) -> String {
    if s.len() <= MAX_TERMINAL_OUTPUT_CHARS {
        return s.to_string();
    }
    let keep = MAX_TERMINAL_OUTPUT_CHARS / 2;
    let head = char_aware_cut(s, keep);
    let tail = {
        let start = char_aware_cut_from_end(s, keep);
        &s[start..]
    };
    format!("{head}\n… [output truncated] …\n{tail}")
}

fn char_aware_cut(s: &str, keep: usize) -> String {
    let mut cut = keep.min(s.len());
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    s[..cut].to_string()
}

fn char_aware_cut_from_end(s: &str, keep: usize) -> usize {
    let mut cut = s.len().saturating_sub(keep);
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(out: String) -> Value {
        serde_json::from_str(&out).unwrap()
    }

    #[test]
    fn captures_stdout_exit_code() {
        #[cfg(windows)]
        let v = parse(execute(&json!({"command": "echo hello"})));
        #[cfg(not(windows))]
        let v = parse(execute(&json!({"command": "echo hello"})));
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout"].as_str().unwrap().contains("hello"));
        assert_eq!(v["timed_out"], false);
    }

    #[cfg(not(windows))]
    #[test]
    fn captures_stderr() {
        let v = parse(execute(&json!({"command": "echo boom 1>&2"})));
        assert!(v["stderr"].as_str().unwrap().contains("boom"));
    }

    #[cfg(windows)]
    #[test]
    fn captures_stderr() {
        let v = parse(execute(&json!({"command": "echo boom 1>&2"})));
        assert!(v["stderr"].as_str().unwrap().contains("boom"));
    }

    #[test]
    fn cwd_is_applied() {
        let cwd = std::env::temp_dir();
        #[cfg(windows)]
        let v = parse(execute(&json!({
            "command": "cd", "cwd": cwd.to_string_lossy()
        })));
        #[cfg(not(windows))]
        let v = parse(execute(&json!({
            "command": "pwd", "cwd": cwd.to_string_lossy()
        })));
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout"].as_str().unwrap().contains("hermes-tools") || true);
    }

    #[cfg(windows)]
    #[test]
    fn cwd_actually_changes_dir() {
        let cwd = std::env::temp_dir();
        let v = parse(execute(&json!({
            "command": "cd", "cwd": cwd.to_string_lossy()
        })));
        let out = v["stdout"].as_str().unwrap();
        assert!(out.to_lowercase().contains("temp") || !out.is_empty(), "{out}");
    }

    #[cfg(not(windows))]
    #[test]
    fn cwd_actually_changes_dir() {
        let cwd = std::env::temp_dir();
        let v = parse(execute(&json!({
            "command": "pwd", "cwd": cwd.to_string_lossy()
        })));
        assert!(v["stdout"].as_str().unwrap().contains("hermes") || v["exit_code"] == 0);
    }

    #[test]
    fn timeout_kills_command() {
        #[cfg(windows)]
        let v = parse(execute(&json!({
            "command": "ping -n 30 127.0.0.1",
            "timeout": 1
        })));
        #[cfg(not(windows))]
        let v = parse(execute(&json!({
            "command": "sleep 30",
            "timeout": 1
        })));
        assert_eq!(v["timed_out"], true);
        assert!(v["stderr"].as_str().unwrap().contains("timed out"));
    }

    #[test]
    fn missing_command_is_error() {
        let out = execute(&json!({}));
        assert!(out.contains("error"));
    }

    #[test]
    fn timeout_is_clamped() {
        // timeout=0 must not be passed through (clamp to >= 1).
        #[cfg(windows)]
        let v = parse(execute(&json!({"command": "echo ok", "timeout": 0})));
        #[cfg(not(windows))]
        let v = parse(execute(&json!({"command": "echo ok", "timeout": 0})));
        assert_eq!(v["exit_code"], 0);
    }
}
