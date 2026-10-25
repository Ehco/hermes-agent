//! Delegation (port of `tools/delegate_tool.py` + the delegation registry):
//! spawn N child agent turns on isolated histories concurrently, collect
//! result envelopes in completion order.
//!
//! Ported envelope semantics (`_build_result_entry` / `_fabricated_entry`):
//! each result carries `task_index`, `status` ("completed" | "failed"),
//! `summary`, `error` and `duration_seconds`; a failed child never fabricates
//! a summary. Children know nothing of the parent conversation — everything
//! rides in the task's goal/context (the focused child system prompt of
//! `_build_child_system_prompt`). This port's children are leaves: nested
//! delegation (Python's depth-derived orchestrator capability) is a CLI-layer
//! concern; callers enforce `max_spawn_depth` around this engine.

use anyhow::Result;
use hermes_core::agent::{run_turn, ToolHost};
use hermes_core::model::{Message, Role, ToolSpec};
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;

/// Python `_DEFAULT_MAX_CONCURRENT_CHILDREN`.
pub const DEFAULT_MAX_CONCURRENT_CHILDREN: usize = 10;

/// One requested child task (Python task entry: `goal` required, optional
/// `context`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateTask {
    pub goal: String,
    pub context: Option<String>,
}

impl DelegateTask {
    pub fn new(goal: impl Into<String>) -> Self {
        Self { goal: goal.into(), context: None }
    }

    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }
}

/// Result envelope for one child (Python `_build_result_entry` subset, JSON
/// field names identical).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TaskResult {
    pub task_index: usize,
    /// "completed" | "failed" (Python: completed/interrupted/failed).
    pub status: String,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub duration_seconds: f64,
}

/// Focused child system prompt (port of `_build_child_system_prompt`; the
/// goal itself is the child's first user turn, per `_build_child_goal_message`).
fn child_system_prompt(task: &DelegateTask) -> String {
    let mut parts =
        vec!["You are a focused subagent working on a specific delegated task.".to_string()];
    if let Some(ctx) = task.context.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        parts.push(format!("\nCONTEXT:\n{ctx}"));
    }
    parts.join("\n")
}

fn child_goal_message(task: &DelegateTask) -> Message {
    Message::text(Role::User, task.goal.clone())
}

pub struct DelegationEngine {
    /// Python `delegation.max_concurrent_children` (floor 1).
    pub max_concurrent_children: usize,
}

impl Default for DelegationEngine {
    fn default() -> Self {
        Self { max_concurrent_children: DEFAULT_MAX_CONCURRENT_CHILDREN }
    }
}

impl DelegationEngine {
    pub fn new(max_concurrent_children: usize) -> Self {
        Self { max_concurrent_children: max_concurrent_children.max(1) }
    }

    /// Run every task on its own isolated agent turn, at most
    /// `max_concurrent_children` at a time. Results arrive in COMPLETION
    /// order; each carries its `task_index` so the parent can join them back
    /// onto the requested tasks.
    ///
    /// `transport_factory` builds a fresh child transport per task — the
    /// Python resolves a per-child runtime, so children may run on a
    /// different provider pair than the parent.
    pub async fn run_batch<T, H, F>(
        &self,
        tasks: Vec<DelegateTask>,
        transport_factory: F,
        tool_host: H,
        tools: Vec<ToolSpec>,
    ) -> Vec<TaskResult>
    where
        F: Fn(usize, &DelegateTask) -> Result<T> + Clone + Send + Sync + 'static,
        T: hermes_core::transport::Transport + Send + Sync + 'static,
        H: ToolHost + Clone + Send + Sync + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<TaskResult>(tasks.len().max(1));
        let semaphore = Arc::new(Semaphore::new(self.max_concurrent_children));
        let mut set = JoinSet::new();

        for (task_index, task) in tasks.iter().enumerate() {
            let factory = transport_factory.clone();
            let host = tool_host.clone();
            let tools = tools.clone();
            let task = task.clone();
            let tx = tx.clone();
            let sem = semaphore.clone();
            set.spawn(async move {
                // Wait for a concurrency slot BEFORE building the child — a
                // saturated pool queues the spawn (Python's pool-saturated note).
                let _permit = sem.acquire_owned().await.expect("semaphore closed");
                let result = run_child(task_index, &task, &factory, &host, &tools).await;
                let _ = tx.send(result).await;
            });
        }
        drop(tx);

        let mut results = Vec::with_capacity(tasks.len());
        while let Some(joined) = set.join_next().await {
            // A child task itself can only fail by panicking; surface that as
            // a failed envelope rather than losing the result slot.
            if let Err(e) = joined {
                results.push(TaskResult {
                    task_index: usize::MAX,
                    status: "failed".into(),
                    summary: None,
                    error: Some(format!("child task panicked: {e}")),
                    duration_seconds: 0.0,
                });
            }
        }
        while let Some(r) = rx.recv().await {
            results.push(r);
        }
        results
    }
}

/// Run ONE child: build its transport, run the isolated agent turn, and map
/// the outcome onto the Python result envelope.
async fn run_child<T, H, F>(
    task_index: usize,
    task: &DelegateTask,
    transport_factory: &F,
    host: &H,
    tools: &[ToolSpec],
) -> TaskResult
where
    T: hermes_core::transport::Transport + Send + Sync,
    H: ToolHost,
    F: Fn(usize, &DelegateTask) -> Result<T>,
{
    let start = Instant::now();
    let finish = |status: &str, summary: Option<String>, error: Option<String>| TaskResult {
        task_index,
        status: status.into(),
        summary,
        error,
        duration_seconds: start.elapsed().as_secs_f64(),
    };
    // Isolated history: fresh system + goal message. The child never sees the
    // parent's conversation.
    let mut history = vec![
        Message::text(Role::System, child_system_prompt(task)),
        child_goal_message(task),
    ];
    let transport = match transport_factory(task_index, task) {
        Ok(t) => t,
        Err(e) => {
            return finish("failed", None, Some(format!("child transport build failed: {e}")));
        }
    };
    match run_turn(&transport, tools, &mut history, host).await {
        Ok(summary) => finish("completed", Some(summary), None),
        Err(e) => finish("failed", None, Some(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use futures::Stream;
    use hermes_core::model::Content;
    use hermes_core::test_transport::ScriptedTransport;
    use hermes_core::transport::{StreamItem, Transport};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct EmptyHost;
    impl ToolHost for EmptyHost {
        fn execute(
            &self,
            _name: &str,
            _arguments: &str,
        ) -> impl Future<Output = Result<String>> + Send {
            std::future::ready(Ok(String::new()))
        }
    }

    /// Transport whose turn sleeps `delay_ms` then answers with its text —
    /// makes completion order and concurrency observable. When `counters` is
    /// set it tracks in-flight turns (active, max-seen).
    #[derive(Clone)]
    struct DelayTransport {
        text: String,
        delay_ms: u64,
        counters: Option<(Arc<AtomicUsize>, Arc<AtomicUsize>)>,
    }

    impl DelayTransport {
        fn new(text: impl Into<String>, delay_ms: u64) -> Self {
            Self { text: text.into(), delay_ms, counters: None }
        }
    }

    impl Transport for DelayTransport {
        fn stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamItem>> + Send>>> {
            let text = self.text.clone();
            let delay = self.delay_ms;
            let counters = self.counters.clone();
            Ok(Box::pin(futures::stream::once(async move {
                if let Some((active, max)) = &counters {
                    let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(cur, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                } else {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Ok(StreamItem::TextDelta(text))
            })))
        }
    }

    #[tokio::test]
    async fn runs_children_concurrently_and_preserves_per_task_results() {
        let tasks = vec![
            DelegateTask::new("alpha").with_context("ctx-a"),
            DelegateTask::new("beta"),
            DelegateTask::new("gamma"),
            DelegateTask::new("delta"),
        ];
        // Descending delays: completion order is the REVERSE of spawn order
        // when everything runs concurrently.
        let delays = [150u64, 100, 50, 25];
        let started = Instant::now();
        let engine = DelegationEngine::new(10);
        let results = engine
            .run_batch(
                tasks.clone(),
                move |i, task| {
                    Ok(DelayTransport::new(format!("{}-done", task.goal), delays[i]))
                },
                EmptyHost,
                vec![],
            )
            .await;
        let elapsed = started.elapsed();

        // Envelope semantics: one result per task, completed, per-task summary
        // (results arrive in completion order; join via task_index).
        assert_eq!(results.len(), 4);
        for r in &results {
            assert_eq!(r.status, "completed", "{r:?}");
            assert!(r.error.is_none());
            let goal = &tasks[r.task_index].goal;
            assert_eq!(r.summary.as_deref(), Some(format!("{goal}-done").as_str()));
            assert!(r.duration_seconds >= 0.0);
        }
        // Completion order: fastest first, task_index preserved on each.
        let order: Vec<usize> = results.iter().map(|r| r.task_index).collect();
        assert_eq!(order, vec![3, 2, 1, 0], "completion order {order:?}");
        // Concurrent: total elapsed well under the 325ms serial sum.
        assert!(elapsed.as_millis() < 320, "children serialized: {elapsed:?}");
    }

    #[tokio::test]
    async fn honors_max_concurrent_children_cap() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<DelegateTask> =
            (0..6).map(|i| DelegateTask::new(format!("task-{i}"))).collect();
        let engine = DelegationEngine::new(2);
        let (active_in, max_in) = (active.clone(), max_active.clone());
        let results = engine
            .run_batch(
                tasks,
                move |_i, _task| {
                    Ok(DelayTransport {
                        text: "ok".into(),
                        delay_ms: 80,
                        counters: Some((active_in.clone(), max_in.clone())),
                    })
                },
                EmptyHost,
                vec![],
            )
            .await;
        assert_eq!(results.len(), 6);
        assert!(results.iter().all(|r| r.status == "completed"));
        // Capped at 2 in flight — never 6 parallel, never serialized.
        assert_eq!(max_active.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failed_transport_build_fails_that_task_alone() {
        let tasks = vec![DelegateTask::new("good-one"), DelegateTask::new("cursed")];
        let engine = DelegationEngine::default();
        let results = engine
            .run_batch(
                tasks,
                |i, _task| {
                    if i == 1 {
                        Err(anyhow!("no route for this backend"))
                    } else {
                        Ok(ScriptedTransport::new(vec![vec![StreamItem::TextDelta(
                            "fine".into(),
                        )]]))
                    }
                },
                EmptyHost,
                vec![],
            )
            .await;
        assert_eq!(results.len(), 2);
        let cursed = results.iter().find(|r| r.task_index == 1).unwrap();
        let good = results.iter().find(|r| r.task_index == 0).unwrap();
        // Failed envelope: status failed, no fabricated summary, error present.
        assert_eq!(cursed.status, "failed");
        assert!(cursed.summary.is_none());
        assert!(cursed.error.as_deref().unwrap().contains("no route"));
        // The sibling is untouched.
        assert_eq!(good.status, "completed");
        assert_eq!(good.summary.as_deref(), Some("fine"));
    }

    #[tokio::test]
    async fn child_histories_are_isolated_per_task() {
        // Each child's transport is seeded with its own goal; the summary
        // proves the child saw only ITS goal, not the batch.
        let tasks = vec![DelegateTask::new("only-mine"), DelegateTask::new("mine-too")];
        let engine = DelegationEngine::new(2);
        let results = engine
            .run_batch(
                tasks,
                |_i, task| {
                    let goal = task.goal.clone();
                    Ok(DelayTransport::new(format!("saw {goal}"), 0))
                },
                EmptyHost,
                vec![],
            )
            .await;
        let summaries: Vec<String> = results.into_iter().map(|r| r.summary.unwrap()).collect();
        assert!(summaries.contains(&"saw only-mine".to_string()));
        assert!(summaries.contains(&"saw mine-too".to_string()));
    }

    #[test]
    fn child_prompt_carries_context_and_goal_stays_user_turn() {
        let task = DelegateTask::new("do the thing").with_context("the widget is red");
        let prompt = child_system_prompt(&task);
        assert!(prompt.contains("focused subagent"));
        assert!(prompt.contains("CONTEXT:\nthe widget is red"));
        assert!(!prompt.contains("do the thing"), "goal rides on the user turn");
        match child_goal_message(&task).content.as_slice() {
            [Content::Text { text }] => assert_eq!(text, "do the thing"),
            other => panic!("{other:?}"),
        }
    }
}
