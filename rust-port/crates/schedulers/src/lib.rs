//! hermes-schedulers: cron jobs and delegation (port of the Python cron
//! scheduler — `cron/jobs.py`, `cron/scheduler_tick.py` — and the delegation
//! registry — `tools/delegate_tool.py`).

pub mod delegate;
pub mod delivery;
pub mod engine;
pub mod jobs;
pub mod schedule;
pub mod timeutil;

pub use delegate::{DelegationEngine, DelegateTask, TaskResult, DEFAULT_MAX_CONCURRENT_CHILDREN};
pub use delivery::{DeliverySink, FileSink, StdoutSink};
pub use engine::{AgentJobRunner, JobRunner, SchedulerEngine};
pub use jobs::{CronJob, DeliveryTarget, JobStore};
pub use schedule::{compute_next_run, parse_schedule, Schedule};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_api_smoke() {
        // The crate's pieces compose: parse → job → store path → engine types.
        let sched = parse_schedule("every 30m", 0).unwrap();
        let job = CronJob::new("demo", "do it", sched, 0);
        assert_eq!(job.next_run_at, Some(1800));
        assert_eq!(job.delivery, DeliveryTarget::Stdout);
        let _: Box<dyn JobRunner> = Box::new(AgentJobRunner::from_provider(
            hermes_core::config::ProviderConfig::new(hermes_core::config::ProviderKind::ChatCompletions),
            "system",
        ));
        let engine = DelegationEngine::default();
        assert_eq!(engine.max_concurrent_children, DEFAULT_MAX_CONCURRENT_CHILDREN);
    }
}
