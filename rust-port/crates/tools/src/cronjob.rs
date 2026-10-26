//! cronjob_manage — create/inspect/manage persisted scheduled jobs. Port of
//! `tools/cronjob_tools.py::cronjob` as a thin dispatcher over
//! `hermes_schedulers::jobs::JobStore` (the port of the Python cron store) —
//! job records, the JSON layout, atomic saves and schedule parsing all live
//! in the schedulers crate, not here.
//!
//! Actions: `create` (schedule + prompt required; `paused` persists disabled;
//! `repeat` caps the run count), `list`, and `update` / `pause` / `resume` /
//! `remove` by `job_id` (always list first — never guess job IDs). `run`
//! fires a job through the scheduler engine, which owns the agent runtime;
//! this tool-level port returns an explicit error for it.
//!
//! The cron directory is `$HERMES_HOME/cron` (`HERMES_HOME` env, else
//! `~/.hermes`).

use serde_json::{json, Value};
use std::path::PathBuf;

use hermes_schedulers::jobs::{CronJob, JobStore};
use hermes_schedulers::schedule::{compute_next_run, parse_schedule, Schedule};

pub fn cron_dir() -> PathBuf {
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
    home.join("cron")
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Execute `cronjob_manage` — the registry entry point (JSON string in, out).
pub fn execute(args: &Value) -> String {
    execute_in(&cron_dir(), args)
}

/// `execute` against an explicit cron directory (tests, embedding hosts).
pub fn execute_in(dir: &PathBuf, args: &Value) -> String {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    match action.as_str() {
        "create" => create(dir, args),
        "list" => list(dir, args),
        "update" => update(dir, args),
        "pause" | "resume" => set_enabled(dir, args, &action),
        "remove" => remove(dir, args),
        "run" => crate::util::tool_error(
            "action 'run' fires through the scheduler engine (it owns the agent runtime); \
             the job stays scheduled — use 'list' to confirm its next_run_at.",
        ),
        "" => crate::util::tool_error(
            "Missing required parameter: 'action' (create, list, update, pause, resume, remove).",
        ),
        other => crate::util::tool_error(&format!(
            "Unknown action '{other}'. Use: create, list, update, pause, resume, remove"
        )),
    }
}

fn job_to_json(job: &CronJob) -> Value {
    let mut obj = json!({
        "id": job.id,
        "name": job.name,
        "prompt": job.prompt,
        "enabled": job.enabled,
        "schedule": serde_json::to_value(&job.schedule).unwrap_or(Value::Null),
        "next_run_at": job.next_run_at,
        "last_run_at": job.last_run_at,
        "last_status": job.last_status,
        "runs_completed": job.runs_completed,
        "max_runs": job.max_runs,
        "created_at": job.created_at,
    });
    if let Some(err) = &job.last_error {
        obj.as_object_mut()
            .unwrap()
            .insert("last_error".into(), Value::String(err.clone()));
    }
    obj
}

fn create(dir: &PathBuf, args: &Value) -> String {
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let schedule_text = args
        .get("schedule")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if prompt.is_empty() || schedule_text.is_empty() {
        return crate::util::tool_error("action='create' requires both 'schedule' and 'prompt' (the prompt must be self-contained: jobs run in a fresh session with no current-chat context).");
    }
    let now = now_unix();
    let schedule = match parse_schedule(&schedule_text, now) {
        Ok(schedule) => schedule,
        Err(e) => return crate::util::tool_error(&e.to_string()),
    };
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("cron-job");
    let mut job = CronJob::new(name, &prompt, schedule, now);
    job.max_runs = args
        .get("repeat")
        .and_then(Value::as_u64)
        .filter(|r| *r >= 1)
        .map(|r| r as u32);
    if args.get("paused").and_then(Value::as_bool).unwrap_or(false) {
        job.enabled = false;
        if let Some(reason) = args
            .get("paused_reason")
            .and_then(Value::as_str)
            .filter(|r| !r.trim().is_empty())
        {
            job.last_error = Some(format!("paused at create: {reason}"));
        }
    }
    let id = job.id.clone();
    let next_run_at = job.next_run_at;
    let paused = !job.enabled;
    let store = JobStore::new(dir);
    let mut jobs = store.load().unwrap_or_default();
    jobs.push(job);
    if let Err(e) = store.save(&jobs) {
        return crate::util::tool_error(&format!("Failed to persist the job list: {e}"));
    }
    json!({
        "success": true,
        "message": format!(
            "Job '{id}' created ({}).",
            if paused { "persisted disabled; 'resume' schedules it, explicit run stays available" } else { "scheduled" }
        ),
        "job_id": id,
        "next_run_at": next_run_at,
        "paused": paused,
    })
    .to_string()
}

fn list(dir: &PathBuf, args: &Value) -> String {
    let include_disabled = args
        .get("include_disabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let store = JobStore::new(dir);
    let jobs = match store.load() {
        Ok(jobs) => jobs,
        Err(e) => return crate::util::tool_error(&format!("Failed to read the job list: {e}")),
    };
    let rows: Vec<Value> = jobs
        .iter()
        .filter(|job| include_disabled || job.enabled)
        .map(job_to_json)
        .collect();
    let count = rows.len();
    json!({ "success": true, "jobs": rows, "count": count }).to_string()
}

fn mutate_job(
    dir: &PathBuf,
    args: &Value,
    f: impl FnOnce(&mut CronJob, i64) -> Result<String, String>,
) -> String {
    let job_id = args
        .get("job_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if job_id.is_empty() {
        return crate::util::tool_error(
            "'job_id' is required (always list first — never guess job IDs).",
        );
    }
    let store = JobStore::new(dir);
    let mut jobs = match store.load() {
        Ok(jobs) => jobs,
        Err(e) => return crate::util::tool_error(&format!("Failed to read the job list: {e}")),
    };
    let Some(idx) = jobs.iter().position(|job| job.id == job_id) else {
        return crate::util::tool_error(&format!(
            "Job '{job_id}' not found. List jobs to get valid IDs."
        ));
    };
    let now = now_unix();
    let mut job = jobs.remove(idx);
    let message = match f(&mut job, now) {
        Ok(message) => message,
        Err(e) => return crate::util::tool_error(e),
    };
    jobs.insert(idx, job);
    if let Err(e) = store.save(&jobs) {
        return crate::util::tool_error(&format!("Failed to persist the job list: {e}"));
    }
    let job_json = job_to_json(&jobs[idx]);
    json!({ "success": true, "message": message, "job": job_json }).to_string()
}

fn update(dir: &PathBuf, args: &Value) -> String {
    mutate_job(dir, args, |job, now| {
        if let Some(name) = args.get("name").and_then(Value::as_str) {
            let name = name.trim();
            if !name.is_empty() {
                job.name = name.to_string();
            }
        }
        if let Some(prompt) = args.get("prompt").and_then(Value::as_str) {
            let prompt = prompt.trim();
            if !prompt.is_empty() {
                job.prompt = prompt.to_string();
            }
        }
        if let Some(repeat) = args.get("repeat") {
            // '' / null clears; an integer ≥1 caps the run count.
            job.max_runs = match repeat {
                Value::Number(n) => n.as_u64().filter(|r| *r >= 1).map(|r| r as u32),
                _ => None,
            };
        }
        if let Some(schedule_text) = args.get("schedule").and_then(Value::as_str) {
            let schedule_text = schedule_text.trim();
            if !schedule_text.is_empty() {
                let schedule: Schedule =
                    parse_schedule(schedule_text, now).map_err(|e| e.to_string())?;
                job.schedule = schedule;
                job.next_run_at = compute_next_run(&job.schedule, job.last_run_at, now);
            }
        }
        Ok(format!("Job '{}' updated.", job.id))
    })
}

fn set_enabled(dir: &PathBuf, args: &Value, action: &str) -> String {
    mutate_job(dir, args, |job, now| {
        let target_enabled = action == "resume";
        if job.enabled == target_enabled {
            return Err(format!(
                "Job '{}' is already {}.",
                job.id,
                if target_enabled { "enabled" } else { "paused" }
            ));
        }
        job.enabled = target_enabled;
        if target_enabled && job.next_run_at.is_none() {
            job.next_run_at = compute_next_run(&job.schedule, job.last_run_at, now);
        }
        Ok(format!(
            "Job '{}' {}.",
            job.id,
            if target_enabled { "resumed" } else { "paused" }
        ))
    })
}

fn remove(dir: &PathBuf, args: &Value) -> String {
    let job_id = args
        .get("job_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if job_id.is_empty() {
        return crate::util::tool_error(
            "'job_id' is required (always list first — never guess job IDs).",
        );
    }
    let store = JobStore::new(dir);
    let mut jobs = match store.load() {
        Ok(jobs) => jobs,
        Err(e) => return crate::util::tool_error(&format!("Failed to read the job list: {e}")),
    };
    let Some(idx) = jobs.iter().position(|job| job.id == job_id) else {
        return crate::util::tool_error(&format!(
            "Job '{job_id}' not found. List jobs to get valid IDs."
        ));
    };
    let removed = jobs.remove(idx);
    if let Err(e) = store.save(&jobs) {
        return crate::util::tool_error(&format!("Failed to persist the job list: {e}"));
    }
    json!({
        "success": true,
        "message": format!("Job '{}' removed (was: {}).", removed.id, removed.name),
        "job_id": removed.id,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn tmp_cron_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-tools-cron-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn create_list_roundtrip_and_schedules_parse() {
        let dir = tmp_cron_dir();
        let out = v(&execute_in(
            &dir,
            &json!({
                "action": "create",
                "schedule": "every 30m",
                "prompt": "sweep the inbox",
                "name": "sweeper",
            }),
        ));
        assert_eq!(out["success"], true, "{}", out);
        let id = out["job_id"].as_str().unwrap().to_string();
        assert!(id.starts_with("sweeper-"));
        assert!(out["next_run_at"].is_i64());

        // The persisted jobs.json is the schedulers crate's layout.
        let raw = fs::read_to_string(dir.join("jobs.json")).unwrap();
        assert!(raw.contains("\"interval\""));

        let out = v(&execute_in(&dir, &json!({"action": "list"})));
        assert_eq!(out["count"], 1);
        let job = &out["jobs"][0];
        assert_eq!(job["id"], id);
        assert_eq!(job["prompt"], "sweep the inbox");
        assert_eq!(job["enabled"], true);
        assert_eq!(job["schedule"]["kind"], "interval");
        assert_eq!(job["schedule"]["minutes"], 30);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_requires_schedule_and_prompt_and_rejects_bad_schedules() {
        let dir = tmp_cron_dir();
        let out = v(&execute_in(
            &dir,
            &json!({"action": "create", "prompt": "x"}),
        ));
        assert!(out["error"].as_str().unwrap().contains("requires both"));
        let out = v(&execute_in(
            &dir,
            &json!({"action": "create", "schedule": "garbage thing", "prompt": "x"}),
        ));
        assert!(
            out["error"].as_str().unwrap().contains("Invalid schedule"),
            "{}",
            out
        );
        // A valid cron expression and one-shot forms parse through the same path.
        let out = v(&execute_in(
            &dir,
            &json!({"action": "create", "schedule": "0 9 * * *", "prompt": "digest"}),
        ));
        assert_eq!(out["success"], true);
        let out = v(&execute_in(
            &dir,
            &json!({"action": "create", "schedule": "in 30m", "prompt": "remind"}),
        ));
        assert_eq!(out["success"], true);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_pause_resume_remove_by_id() {
        let dir = tmp_cron_dir();
        let id = v(&execute_in(
            &dir,
            &json!({
                "action": "create", "schedule": "every 1h", "prompt": "old prompt", "name": "job-a",
            }),
        ))["job_id"]
            .as_str()
            .unwrap()
            .to_string();

        let out = v(&execute_in(
            &dir,
            &json!({"action": "update", "job_id": id, "prompt": "new prompt", "repeat": 3}),
        ));
        assert_eq!(out["success"], true);
        assert_eq!(out["job"]["prompt"], "new prompt");
        assert_eq!(out["job"]["max_runs"], 3);

        let out = v(&execute_in(&dir, &json!({"action": "pause", "job_id": id})));
        assert_eq!(out["job"]["enabled"], false);
        // Pausing twice is an explicit no-op error.
        let out = v(&execute_in(&dir, &json!({"action": "pause", "job_id": id})));
        assert!(out["error"].as_str().unwrap().contains("already paused"));
        let out = v(&execute_in(
            &dir,
            &json!({"action": "resume", "job_id": id}),
        ));
        assert_eq!(out["job"]["enabled"], true);
        let out = v(&execute_in(&dir, &json!({"action": "list"})));
        assert_eq!(out["jobs"][0]["prompt"], "new prompt");

        let out = v(&execute_in(
            &dir,
            &json!({"action": "remove", "job_id": id}),
        ));
        assert_eq!(out["success"], true);
        let out = v(&execute_in(&dir, &json!({"action": "list"})));
        assert_eq!(out["count"], 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn job_id_is_required_and_unknown_ids_error() {
        let dir = tmp_cron_dir();
        execute_in(
            &dir,
            &json!({"action": "create", "schedule": "every 1h", "prompt": "p"}),
        );
        let out = v(&execute_in(&dir, &json!({"action": "remove"})));
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("'job_id' is required"));
        let out = v(&execute_in(
            &dir,
            &json!({"action": "pause", "job_id": "nope-1"}),
        ));
        assert!(
            out["error"].as_str().unwrap().contains("not found"),
            "{}",
            out
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn action_is_required_and_unknown_actions_error() {
        let dir = tmp_cron_dir();
        let out = v(&execute_in(&dir, &json!({})));
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("Missing required parameter"));
        let out = v(&execute_in(&dir, &json!({"action": "frobnicate"})));
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("Unknown action 'frobnicate'"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn paused_create_and_disabled_list_filter() {
        let dir = tmp_cron_dir();
        let out = v(&execute_in(
            &dir,
            &json!({
                "action": "create", "schedule": "every 1h", "prompt": "later",
                "paused": true, "paused_reason": "awaiting review",
            }),
        ));
        assert_eq!(out["paused"], true);
        let out = v(&execute_in(
            &dir,
            &json!({"action": "list", "include_disabled": false}),
        ));
        assert_eq!(out["count"], 0);
        let out = v(&execute_in(&dir, &json!({"action": "list"})));
        assert_eq!(out["count"], 1);
        assert_eq!(out["jobs"][0]["enabled"], false);
        let _ = fs::remove_dir_all(&dir);
    }
}
