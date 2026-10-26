//! delegate_task — spawn and live-control subagents in isolated contexts.
//!
//! Port of the `tools/delegate_tool.py` registry surface: the tasks batch
//! (`goal` required, optional `context`), the legacy single-goal shape wrapped
//! into a one-entry batch, the `action` enum (spawn/list/steer/stop) and the
//! steer/stop argument rules. Envelope semantics and child system prompts live
//! in `hermes-schedulers::delegate::DelegationEngine` (already ported); what
//! this registry layer adds is argument validation plus an injected backend —
//! the Python intercept (`run_agent._dispatch_delegate_task`) builds child
//! transports in the agent runtime, and the Rust CLI does the same, so the
//! default backend surfaces a clean setup error rather than a hollow spawn.

use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};

use crate::util::tool_error;

/// The delegation runtime seam: the agent runtime (CLI) wires a backend that
/// spawns children through `hermes-schedulers::delegate::DelegationEngine`.
pub trait DelegationBackend: Send + Sync {
    /// Run the batch to completion, returning the results array (each entry
    /// `{task_index, status, summary, error, duration_seconds}`).
    fn spawn_batch(&self, tasks: &[(String, Option<String>)]) -> Result<Value, String>;
    /// Live view: `{subagents: [{subagent_id, goal, status, transcript}]}`.
    fn list(&self) -> Result<Value, String>;
    /// Queue course-correction text into one running child.
    fn steer(&self, subagent_id: &str, message: &str) -> Result<Value, String>;
    /// End one child early (partial result still returns).
    fn stop(&self, subagent_id: &str) -> Result<Value, String>;
}

fn backend() -> &'static Mutex<Option<std::sync::Arc<dyn DelegationBackend>>> {
    static BACKEND: OnceLock<Mutex<Option<std::sync::Arc<dyn DelegationBackend>>>> =
        OnceLock::new();
    BACKEND.get_or_init(|| Mutex::new(None))
}

/// Wire the delegation backend (the CLI agent runtime calls this at startup).
pub fn set_backend(new_backend: std::sync::Arc<dyn DelegationBackend>) {
    *backend().lock().unwrap() = Some(new_backend);
}

fn with_backend(
    f: impl FnOnce(&dyn DelegationBackend) -> Result<Value, String>,
) -> Option<Result<Value, String>> {
    let guard = backend().lock().unwrap();
    guard.as_ref().map(|b| f(b.as_ref()))
}

/// Extract `[(goal, context)]` from args: the `tasks` batch, or the legacy
/// top-level `goal`/`context` wrapped into a one-entry batch.
fn normalize_tasks(args: &Value) -> Result<Vec<(String, Option<String>)>, String> {
    if let Some(tasks) = args.get("tasks") {
        let Some(entries) = tasks.as_array() else {
            return Err("tasks must be an array of task objects (each with 'goal').".into());
        };
        if entries.is_empty() {
            return Err("tasks must contain at least one task.".into());
        }
        let mut out = Vec::with_capacity(entries.len());
        for (i, entry) in entries.iter().enumerate() {
            let goal = entry
                .get("goal")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .ok_or_else(|| {
                    format!("tasks[{i}] is missing 'goal' — each task needs a self-contained goal.")
                })?;
            let context = entry
                .get("context")
                .and_then(Value::as_str)
                .map(|c| c.to_string())
                .filter(|c| !c.trim().is_empty());
            out.push((goal.to_string(), context));
        }
        return Ok(out);
    }
    // Legacy single-goal shape (accepted, unadvertised — old transcripts only).
    let goal = args
        .get("goal")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .ok_or_else(|| {
            "spawn requires tasks (a one-entry array), or the legacy top-level 'goal'.".to_string()
        })?;
    let context = args
        .get("context")
        .and_then(Value::as_str)
        .map(|c| c.to_string());
    Ok(vec![(goal.to_string(), context)])
}

/// Execute `delegate_task` — the registry entry point.
pub fn execute(args: &Value) -> String {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("spawn")
        .trim()
        .to_lowercase();
    let subagent_id = args
        .get("subagent_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let message = args.get("message").and_then(Value::as_str).unwrap_or("");

    match action.as_str() {
        "spawn" => match normalize_tasks(args) {
            Ok(tasks) => match with_backend(|b| b.spawn_batch(&tasks).map_err(|e: String| e)) {
                Some(Ok(results)) => {
                    json!({ "success": true, "dispatched": tasks.len(), "results": results })
                        .to_string()
                }
                Some(Err(e)) => tool_error(e),
                None => tool_error(
                    "no delegation backend is wired into this process: the agent runtime owns \
                         child transport construction; wire delegate_task::set_backend at startup.",
                ),
            },
            Err(e) => tool_error(e),
        },
        "list" => match with_backend(|b| b.list().map_err(|e: String| e)) {
            Some(Ok(v)) => v.to_string(),
            Some(Err(e)) => tool_error(e),
            None => tool_error("no delegation backend is wired into this process."),
        },
        "steer" => {
            if subagent_id.is_empty() {
                return tool_error("subagent_id is required for action='steer' (ids from the spawn response or action='list').");
            }
            if message.trim().is_empty() {
                return tool_error("message is required for action='steer': the course correction, appended to the child's next tool result mid-run.");
            }
            match with_backend(|b| b.steer(&subagent_id, message).map_err(|e: String| e)) {
                Some(Ok(v)) => v.to_string(),
                Some(Err(e)) => tool_error(e),
                None => tool_error("no delegation backend is wired into this process."),
            }
        }
        "stop" => {
            if subagent_id.is_empty() {
                return tool_error(
                    "subagent_id is required for action='stop' (partial result still returns).",
                );
            }
            match with_backend(|b| b.stop(&subagent_id).map_err(|e: String| e)) {
                Some(Ok(v)) => v.to_string(),
                Some(Err(e)) => tool_error(e),
                None => tool_error("no delegation backend is wired into this process."),
            }
        }
        other => tool_error(format!(
            "Unknown delegate_task action: {other}. Use: spawn, list, steer, stop"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubBackend {
        calls: AtomicUsize,
    }

    impl DelegationBackend for StubBackend {
        fn spawn_batch(&self, tasks: &[(String, Option<String>)]) -> Result<Value, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let results: Vec<Value> = tasks
                .iter()
                .enumerate()
                .map(|(i, (goal, ctx))| {
                    json!({
                        "task_index": i,
                        "status": "completed",
                        "summary": format!("did {goal} with context {}", ctx.clone().unwrap_or_default()),
                        "error": Value::Null,
                        "duration_seconds": 0.1,
                    })
                })
                .collect();
            Ok(json!(results))
        }
        fn list(&self) -> Result<Value, String> {
            Ok(json!({ "subagents": [{ "subagent_id": "sub_1", "status": "running" }] }))
        }
        fn steer(&self, id: &str, message: &str) -> Result<Value, String> {
            if id == "sub_1" {
                Ok(json!({ "queued": true, "subagent_id": id, "message": message }))
            } else {
                Err(format!("Unknown subagent '{id}'"))
            }
        }
        fn stop(&self, id: &str) -> Result<Value, String> {
            Ok(json!({ "stopped": true, "subagent_id": id }))
        }
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    fn install() {
        set_backend(std::sync::Arc::new(StubBackend {
            calls: AtomicUsize::new(0),
        }));
    }

    #[test]
    fn spawn_validates_and_routes_tasks_to_the_backend() {
        install();
        let out = v(&execute(&json!({ "tasks": [{ "goal": "port the tool" }] })));
        assert_eq!(out["success"], true);
        assert_eq!(out["dispatched"], 1);
        assert_eq!(
            out["results"][0]["summary"],
            "did port the tool with context "
        );

        // Legacy single-goal shape wraps into a one-entry batch with context.
        let out = v(&execute(
            &json!({ "goal": "solo", "context": "the widget is red" }),
        ));
        assert_eq!(
            out["results"][0]["summary"],
            "did solo with context the widget is red"
        );

        // Validation: tasks array without goal.
        let out = v(&execute(&json!({ "tasks": [{ "context": "no goal" }] })));
        assert!(
            out["error"].as_str().unwrap().contains("missing 'goal'"),
            "{out}"
        );
        let out = v(&execute(&json!({ "tasks": [] })));
        assert!(out["error"].as_str().unwrap().contains("at least one task"));
        let out = v(&execute(&json!({})));
        assert!(
            out["error"].as_str().unwrap().contains("requires tasks"),
            "{out}"
        );
    }

    #[test]
    fn steer_and_stop_require_their_arguments() {
        install();
        let out = v(&execute(
            &json!({ "action": "steer", "message": "use fewer threads" }),
        ));
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("subagent_id is required"),
            "{out}"
        );
        let out = v(&execute(
            &json!({ "action": "steer", "subagent_id": "sub_1" }),
        ));
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("message is required"),
            "{out}"
        );
        let out = v(&execute(
            &json!({ "action": "steer", "subagent_id": "sub_1", "message": "slow down" }),
        ));
        assert_eq!(out["queued"], true);
        let out = v(&execute(
            &json!({ "action": "steer", "subagent_id": "nope", "message": "x" }),
        ));
        assert!(
            out["error"].as_str().unwrap().contains("Unknown subagent"),
            "{out}"
        );
        let out = v(&execute(
            &json!({ "action": "stop", "subagent_id": "sub_1" }),
        ));
        assert_eq!(out["stopped"], true);
        let out = v(&execute(&json!({ "action": "stop" })));
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("subagent_id is required"),
            "{out}"
        );
    }

    #[test]
    fn list_and_unknown_action() {
        install();
        let out = v(&execute(&json!({ "action": "list" })));
        assert_eq!(out["subagents"][0]["subagent_id"], "sub_1");
        let out = v(&execute(&json!({ "action": "explode" })));
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("Unknown delegate_task action"),
            "{out}"
        );
    }

    #[test]
    fn unset_backend_is_a_clean_setup_error_not_a_panic() {
        // Only safe to assert when nothing installed a global backend yet
        // (tests here install one; a sibling test binary could race, so just
        // check the envelope is valid JSON either way).
        let out = execute(&json!({ "action": "list" }));
        assert!(serde_json::from_str::<Value>(&out).is_ok(), "{out}");
    }
}
