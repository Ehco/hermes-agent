//! Persisted cron job records (port of the Python cron store: a `cron/`
//! directory holding the job list as JSON, scheduler-owned state advanced by
//! the engine, authored fields owned by the creator).

use crate::schedule::{Schedule, compute_next_run};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where a fired job's agent output goes. The Python delivers to
/// Telegram/etc.; platform sinks implement the `DeliverySink` trait
/// (see `delivery.rs`), so new targets are additive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum DeliveryTarget {
    Stdout,
    /// Append each run's output to one file per job.
    File { path: PathBuf },
}

/// One persisted job. Scheduler-owned fields (`next_run_at`, `last_run_at`,
/// `last_status`, `runs_completed`) are advanced by the engine; everything
/// else is authored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    /// The prompt the agent turn runs for each fire.
    pub prompt: String,
    pub delivery: DeliveryTarget,
    pub enabled: bool,
    pub schedule: Schedule,
    /// Unix seconds of the next scheduled fire (scheduler-owned).
    pub next_run_at: Option<i64>,
    /// Unix seconds of the last completed fire (scheduler-owned).
    pub last_run_at: Option<i64>,
    /// "ok" | "error" (scheduler-owned, mirrors Python `last_status`).
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    /// Completed run count, for `max_runs` accounting.
    pub runs_completed: u32,
    /// Stop after N runs (Python `repeat.times`); None = forever.
    pub max_runs: Option<u32>,
    pub created_at: i64,
}

impl CronJob {
    pub fn new(name: &str, prompt: &str, schedule: Schedule, now: i64) -> Self {
        // Stable-ish unique id without a uuid dependency: name + timestamp.
        let id = format!("{}-{:08x}", sanitize_id(name), (now as u64) & 0xffff_ffff);
        let next_run_at = compute_next_run(&schedule, None, now);
        Self {
            id,
            name: name.to_string(),
            prompt: prompt.to_string(),
            delivery: DeliveryTarget::Stdout,
            enabled: true,
            schedule,
            next_run_at,
            last_run_at: None,
            last_status: None,
            last_error: None,
            runs_completed: 0,
            max_runs: None,
            created_at: now,
        }
    }

    /// Python `is_job_runnable`: enabled, has a computed next run, and has
    /// run budget left.
    pub fn is_runnable(&self, now: i64) -> bool {
        self.enabled
            && self
                .next_run_at
                .or_else(|| compute_next_run(&self.schedule, self.last_run_at, now))
                .is_some()
            && self.max_runs.map_or(true, |cap| self.runs_completed < cap)
    }
}

fn sanitize_id(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    cleaned.trim_matches('-').to_lowercase()
}

/// The job list store: one `jobs.json` under the cron directory, saved
/// atomically (tmp file + rename) like the Python `_secure_file` path.
#[derive(Debug, Clone)]
pub struct JobStore {
    jobs_path: PathBuf,
}

impl JobStore {
    /// `dir` is the cron directory (Python: `<home>/cron`).
    pub fn new(dir: &Path) -> Self {
        Self { jobs_path: dir.join("jobs.json") }
    }

    pub fn jobs_path(&self) -> &Path {
        &self.jobs_path
    }

    /// Load all jobs; a missing store is an empty list.
    pub fn load(&self) -> Result<Vec<CronJob>> {
        match std::fs::read_to_string(&self.jobs_path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", self.jobs_path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.jobs_path.display())),
        }
    }

    pub fn save(&self, jobs: &[CronJob]) -> Result<()> {
        if let Some(parent) = self.jobs_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let tmp = self.jobs_path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(jobs)?)?;
        std::fs::rename(&tmp, &self.jobs_path)
            .with_context(|| format!("renaming into {}", self.jobs_path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::Schedule;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hermes-schedulers-{tag}-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn job_store_roundtrips_every_schedule_kind() {
        let dir = temp_dir("roundtrip");
        let store = JobStore::new(&dir);
        let jobs = vec![
            CronJob::new("sweeper", "clean up", Schedule::Interval { minutes: 30 }, 1000),
            CronJob::new(
                "morning",
                "digest",
                Schedule::DailyAt { hour: 9, minute: 0 },
                1001,
            ),
            CronJob::new(
                "weekly",
                "report",
                Schedule::WeeklyAt { days: vec![1], hour: 8, minute: 30 },
                1002,
            ),
            CronJob::new("once", "ping", Schedule::Once { run_at: 9999 }, 1003),
            CronJob::new("cron", "tick", Schedule::Cron { expr: "*/5 * * * *".into() }, 1004),
        ];
        store.save(&jobs).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, jobs);
        assert!(loaded[0].is_runnable(1000));
        // The JSON uses the Python-compatible `kind` tag.
        let text = std::fs::read_to_string(store.jobs_path()).unwrap();
        assert!(text.contains("\"kind\": \"interval\"") || text.contains("\"kind\":\"interval\""));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_store_is_empty() {
        let dir = temp_dir("empty");
        assert_eq!(JobStore::new(&dir).load().unwrap().len(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn max_runs_disables_runnable() {
        let mut job = CronJob::new("once-only", "go", Schedule::Interval { minutes: 5 }, 100);
        job.max_runs = Some(1);
        assert!(job.is_runnable(100));
        job.runs_completed = 1;
        assert!(!job.is_runnable(200));
    }
}
