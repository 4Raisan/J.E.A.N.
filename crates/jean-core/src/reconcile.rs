//! Startup reconciliation: deciding which recorded tasks are orphans.
//!
//! ## The rule
//!
//! A task in a non-terminal state whose recorded runner is **provably dead** is
//! moved to `needs_attention`. Nothing is re-executed, and nothing is marked
//! failed on the runner's behalf — the process died before it could report, so
//! the truth about what it achieved is not in the database.
//!
//! ## Proving a runner is dead
//!
//! A pid alone cannot prove anything. Windows recycles pids, so after a crash the
//! recorded pid may belong to a completely unrelated program that started later;
//! trusting it would mean leaving an orphaned task in `running` forever. Comparing
//! the pid *and* the process creation time recorded alongside it settles the
//! question, because a recycled pid has a different creation time.
//!
//! Both checks go through [`jean_process::win32::is_same_process_alive`], which
//! also returns false when the process cannot be queried at all. Treating "cannot
//! tell" as dead is the safe direction: the cost is a task a human has to look at,
//! and the alternative is a task nobody ever will.
//!
//! ## The second-client rule
//!
//! A live runner is left completely alone. This is what makes `jean tasks` safe to
//! run while `jean run` is busy: the two are separate clients of the same
//! database, and only a dead runner indicates a crash.
//!
//! ## What is never done here
//!
//! * A command is never replayed. Resuming would repeat side effects — a file
//!   write, a deployment step — that may already have happened.
//! * `orchestrator.resume_on_restart` is deliberately **not** honoured in Phase 0.
//!   Replaying an interrupted command safely needs per-tool idempotence metadata
//!   that does not exist yet. The setting is loaded and then ignored, and that is
//!   recorded in `IMPLEMENTATION_STATUS.md` rather than quietly pretended away.

use jean_store::{Store, TaskRecord, Termination};

use crate::error::Result;

/// What one reconciliation pass found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciliation {
    /// Tasks moved to `needs_attention` because their runner is gone.
    pub orphaned: Vec<String>,
    /// Tasks left untouched because their runner is alive.
    pub still_owned: Vec<String>,
    /// In-flight tasks with no runner recorded at all: a crash between writing
    /// the row and recording ownership.
    pub never_owned: Vec<String>,
}

impl Reconciliation {
    pub fn is_empty(&self) -> bool {
        self.orphaned.is_empty() && self.still_owned.is_empty() && self.never_owned.is_empty()
    }

    /// A summary for the CLI.
    ///
    /// `None` when nothing needed attention, so an ordinary run stays quiet. A
    /// live runner is deliberately not worth reporting: warning about a task that
    /// is working correctly would train the operator to ignore this line.
    pub fn summary(&self) -> Option<String> {
        if self.orphaned.is_empty() && self.never_owned.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if !self.orphaned.is_empty() {
            parts.push(format!(
                "{} task(s) had a runner that no longer exists",
                self.orphaned.len()
            ));
        }
        if !self.never_owned.is_empty() {
            parts.push(format!(
                "{} task(s) never recorded a runner",
                self.never_owned.len()
            ));
        }
        let ids: Vec<&str> = self
            .orphaned
            .iter()
            .chain(self.never_owned.iter())
            .map(String::as_str)
            .collect();
        Some(format!(
            "reconciled {}: {}",
            parts.join("; "),
            ids.join(", ")
        ))
    }
}

/// Reconcile every in-flight task in the store.
///
/// Runs before a new task is created, so a crash is cleaned up at the next
/// opportunity instead of needing a daemon to notice it.
pub fn reconcile(store: &Store) -> Result<Reconciliation> {
    let candidates = store.in_flight_tasks()?;
    let mut result = Reconciliation::default();

    for task in candidates {
        match classify(&task) {
            Verdict::OwnedByLiveRunner => result.still_owned.push(task.id),
            Verdict::RunnerIsGone => {
                mark_orphaned(store, &task, "the recorded runner process no longer exists")?;
                result.orphaned.push(task.id);
            }
            Verdict::NoRunnerRecorded => {
                mark_orphaned(store, &task, "no runner was ever recorded for this task")?;
                result.never_owned.push(task.id);
            }
        }
    }

    if !result.is_empty() {
        tracing::info!(
            orphaned = result.orphaned.len(),
            never_owned = result.never_owned.len(),
            still_owned = result.still_owned.len(),
            "reconciled in-flight tasks from a previous run"
        );
    }
    Ok(result)
}

/// Why a task is or is not an orphan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A runner owns this task and is alive. Leave it completely alone.
    OwnedByLiveRunner,
    /// A runner was recorded and is gone.
    RunnerIsGone,
    /// The row was written but ownership was never recorded.
    NoRunnerRecorded,
}

/// Decide, without touching the database.
///
/// Split out from [`reconcile`] because this is the judgement that matters, and it
/// can be exercised against synthetic records with no store and no real child
/// process.
pub fn classify(task: &TaskRecord) -> Verdict {
    let Some(pid) = task.runner_pid else {
        return Verdict::NoRunnerRecorded;
    };
    if jean_process::win32::is_same_process_alive(pid, task.runner_created_ms) {
        Verdict::OwnedByLiveRunner
    } else {
        Verdict::RunnerIsGone
    }
}

/// Whether a recorded runner is still the same process.
///
/// Exposed for `jean task`, which reports liveness without changing anything. The
/// check is the same one reconciliation uses, so the CLI and the reconciler cannot
/// disagree about whether a task is abandoned.
pub fn runner_is_alive(pid: u32, created_ms: i64) -> bool {
    jean_process::win32::is_same_process_alive(pid, Some(created_ms))
}

/// Move one task to `needs_attention` and mark its unfinished runs orphaned.
fn mark_orphaned(store: &Store, task: &TaskRecord, reason: &str) -> Result<()> {
    // Every unfinished run of this task ended without an outcome being recorded.
    for run in store.tool_runs(&task.id)? {
        if run.ended_at_ms.is_none() {
            store.finish_tool_run(
                run.id,
                None,
                Termination::Orphaned,
                &jean_protocol::StreamSummary {
                    path: run.stdout_path.clone(),
                    bytes: run.stdout_bytes.max(0) as u64,
                    truncated: run.stdout_truncated,
                },
                &jean_protocol::StreamSummary {
                    path: run.stderr_path.clone(),
                    bytes: run.stderr_bytes.max(0) as u64,
                    truncated: run.stderr_truncated,
                },
                Some(reason),
            )?;
        }
    }

    store.transition(
        &task.id,
        jean_protocol::TaskState::NeedsAttention,
        Some(reason),
    )?;
    store.append_event(
        &task.id,
        &jean_protocol::Event::new(
            jean_protocol::EventKind::Reconciled,
            format!("{reason}; the command was not replayed and the outcome is unknown"),
        ),
    )?;

    // Clearing ownership means a second pass will not re-report the same task.
    store.clear_runner(&task.id)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jean_protocol::state::ALL_STATES;
    use jean_protocol::TaskState;
    use jean_store::{now_ms, TaskRecord};

    /// A pid that cannot belong to a live process on this machine.
    const DEAD_PID: u32 = u32::MAX;

    /// A record in an arbitrary state.
    ///
    /// Inserted directly rather than walked through transitions: `insert_task`
    /// writes whatever state the record carries, and the schema CHECK constraint
    /// keeps that honest. The transition rules themselves are covered by
    /// `jean_protocol::state` and by `jean_store`.
    fn record(id: &str, state: TaskState, pid: Option<u32>, created: Option<i64>) -> TaskRecord {
        let mut t = TaskRecord::new(
            id.to_owned(),
            format!("goal for {id}"),
            Some("powershell.echo".to_owned()),
            "{}".to_owned(),
        );
        t.state = state;
        t.runner_pid = pid;
        t.runner_created_ms = created;
        t
    }

    #[test]
    fn a_task_with_no_runner_was_never_owned() {
        let t = record("t-1", TaskState::Queued, None, None);
        assert_eq!(classify(&t), Verdict::NoRunnerRecorded);
    }

    #[test]
    fn a_task_owned_by_a_dead_pid_is_an_orphan() {
        let t = record("t-1", TaskState::Running, Some(DEAD_PID), Some(now_ms()));
        assert_eq!(classify(&t), Verdict::RunnerIsGone);
    }

    #[test]
    fn a_task_owned_by_this_process_is_left_alone() {
        let (pid, created) = jean_process::win32::current_process_identity();
        let t = record("t-1", TaskState::Running, Some(pid), created);
        assert_eq!(
            classify(&t),
            Verdict::OwnedByLiveRunner,
            "a live runner must never be reconciled: a second CLI client is not a crash"
        );
    }

    #[test]
    fn a_recycled_pid_with_the_wrong_creation_time_counts_as_dead() {
        // The core defence. This process holds the pid, but the recorded creation
        // time belongs to some other process that used to hold it.
        let (pid, created) = jean_process::win32::current_process_identity();
        let stale = created.map(|c| c - 3_600_000);
        let t = record("t-1", TaskState::Running, Some(pid), stale);
        assert_eq!(classify(&t), Verdict::RunnerIsGone);
    }

    #[test]
    fn a_runner_recorded_without_a_creation_time_falls_back_to_an_existence_check() {
        let (pid, _) = jean_process::win32::current_process_identity();
        let t = record("t-1", TaskState::Running, Some(pid), None);
        assert_eq!(
            classify(&t),
            Verdict::OwnedByLiveRunner,
            "a missing creation time weakens the check but must not invert it; \
             assuming the runner died would report against work still running"
        );
    }

    #[test]
    fn reconciling_an_empty_store_changes_nothing() {
        let store = Store::open_in_memory().unwrap();
        let result = reconcile(&store).unwrap();
        assert!(result.is_empty());
        assert!(result.summary().is_none());
    }

    #[test]
    fn an_orphaned_task_is_marked_needs_attention_not_failed() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_task(&record(
                "t-orphan",
                TaskState::Running,
                Some(DEAD_PID),
                Some(now_ms()),
            ))
            .unwrap();

        let result = reconcile(&store).unwrap();
        assert_eq!(result.orphaned, vec!["t-orphan"]);

        let after = store.require_task("t-orphan").unwrap();
        assert_eq!(
            after.state,
            TaskState::NeedsAttention,
            "`failed` would claim J.E.A.N. knows the command failed; it does not"
        );
        assert!(
            after.runner_pid.is_none(),
            "ownership must be cleared so the next pass does not re-report it"
        );
    }

    #[test]
    fn reconciliation_records_why_and_says_the_command_was_not_replayed() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_task(&record(
                "t-orphan",
                TaskState::Running,
                Some(DEAD_PID),
                Some(now_ms()),
            ))
            .unwrap();
        reconcile(&store).unwrap();

        let reconciled: Vec<String> = store
            .events("t-orphan")
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == jean_protocol::EventKind::Reconciled)
            .map(|e| e.detail)
            .collect();
        assert_eq!(reconciled.len(), 1);
        assert!(
            reconciled[0].contains("not replayed"),
            "the event must state that no re-execution happened: {}",
            reconciled[0]
        );
        assert!(
            reconciled[0].contains("no longer exists"),
            "{}",
            reconciled[0]
        );
    }

    #[test]
    fn an_unfinished_tool_run_is_marked_orphaned() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_task(&record(
                "t-run",
                TaskState::Running,
                Some(DEAD_PID),
                Some(now_ms()),
            ))
            .unwrap();
        let run_id = store
            .insert_tool_run(
                "t-run",
                "powershell.echo",
                "powershell.exe",
                "[]",
                None,
                "{}",
            )
            .unwrap();
        store.record_child_pid(run_id, 999_999).unwrap();

        reconcile(&store).unwrap();

        let run = &store.tool_runs("t-run").unwrap()[0];
        assert_eq!(run.termination, Some(Termination::Orphaned));
        assert!(run.ended_at_ms.is_some());
        assert_eq!(run.exit_code, None);
        assert!(run.error.as_deref().unwrap().contains("no longer exists"));
    }

    #[test]
    fn a_finished_tool_run_is_not_relabelled() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_task(&record(
                "t-done",
                TaskState::Verifying,
                Some(DEAD_PID),
                Some(now_ms()),
            ))
            .unwrap();
        let run_id = store
            .insert_tool_run(
                "t-done",
                "powershell.echo",
                "powershell.exe",
                "[]",
                None,
                "{}",
            )
            .unwrap();
        store
            .finish_tool_run(
                run_id,
                Some(0),
                Termination::Exited,
                &Default::default(),
                &Default::default(),
                None,
            )
            .unwrap();

        reconcile(&store).unwrap();

        let run = &store.tool_runs("t-done").unwrap()[0];
        assert_eq!(
            run.termination,
            Some(Termination::Exited),
            "a recorded outcome is evidence and must not be overwritten"
        );
        assert_eq!(run.exit_code, Some(0));
    }

    #[test]
    fn a_task_owned_by_a_live_runner_is_untouched() {
        let store = Store::open_in_memory().unwrap();
        let (pid, created) = jean_process::win32::current_process_identity();
        store
            .insert_task(&record("t-live", TaskState::Running, Some(pid), created))
            .unwrap();

        let result = reconcile(&store).unwrap();
        assert_eq!(result.still_owned, vec!["t-live"]);
        assert!(result.orphaned.is_empty());

        let after = store.require_task("t-live").unwrap();
        assert_eq!(after.state, TaskState::Running);
        assert_eq!(after.runner_pid, Some(pid));
        assert!(
            store
                .events("t-live")
                .unwrap()
                .iter()
                .all(|e| e.kind != jean_protocol::EventKind::Reconciled),
            "a live runner must not be reported on at all"
        );
    }

    #[test]
    fn terminal_tasks_are_never_candidates() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_task(&record(
                "t-done",
                TaskState::Succeeded,
                Some(DEAD_PID),
                Some(now_ms()),
            ))
            .unwrap();

        let result = reconcile(&store).unwrap();
        assert!(result.is_empty(), "a finished task is not in flight");
        assert_eq!(
            store.require_task("t-done").unwrap().state,
            TaskState::Succeeded
        );
    }

    #[test]
    fn every_in_flight_state_is_reconcilable() {
        // `needs_attention` must be reachable from all five unfinished states, or
        // a crash in one of them would leave a row the reconciler cannot fix.
        let in_flight: Vec<TaskState> = ALL_STATES
            .iter()
            .copied()
            .filter(|s| s.is_in_flight())
            .collect();
        assert_eq!(in_flight.len(), 5, "{in_flight:?}");
        for state in in_flight {
            assert!(
                state.can_transition_to(TaskState::NeedsAttention),
                "{state} cannot reach needs_attention"
            );
        }
    }

    #[test]
    fn the_liveness_probe_agrees_with_the_classifier() {
        let (pid, created) = jean_process::win32::current_process_identity();
        let created = created.unwrap();
        assert!(runner_is_alive(pid, created));
        assert!(!runner_is_alive(pid, created + 3_600_000));
        assert!(!runner_is_alive(u32::MAX, created));
        assert!(!runner_is_alive(0, created));

        let record = record("t-probe", TaskState::Running, Some(pid), Some(created));
        assert_eq!(classify(&record), Verdict::OwnedByLiveRunner);
    }

    #[test]
    fn reconciliation_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_task(&record(
                "t-twice",
                TaskState::Running,
                Some(DEAD_PID),
                Some(now_ms()),
            ))
            .unwrap();

        let first = reconcile(&store).unwrap();
        assert_eq!(first.orphaned.len(), 1);

        let second = reconcile(&store).unwrap();
        assert!(
            second.is_empty(),
            "a second pass must find nothing: the first cleared ownership and moved \
             the task out of the in-flight set, got {second:?}"
        );
        assert_eq!(
            store
                .events("t-twice")
                .unwrap()
                .iter()
                .filter(|e| e.kind == jean_protocol::EventKind::Reconciled)
                .count(),
            1
        );
    }

    #[test]
    fn the_summary_names_the_tasks_it_touched() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_task(&record(
                "t-a",
                TaskState::Running,
                Some(DEAD_PID),
                Some(now_ms()),
            ))
            .unwrap();
        store
            .insert_task(&record("t-b", TaskState::Queued, None, None))
            .unwrap();

        let result = reconcile(&store).unwrap();
        let summary = result.summary().expect("something was reconciled");
        assert!(summary.contains("t-a"), "{summary}");
        assert!(summary.contains("t-b"), "{summary}");
        assert!(summary.contains("never recorded a runner"), "{summary}");
        assert!(summary.contains("no longer exists"), "{summary}");
    }

    #[test]
    fn a_clean_store_produces_no_summary() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_task(&record("t-ok", TaskState::Succeeded, None, None))
            .unwrap();
        assert!(reconcile(&store).unwrap().summary().is_none());
    }

    #[test]
    fn a_live_runner_produces_no_summary() {
        let store = Store::open_in_memory().unwrap();
        let (pid, created) = jean_process::win32::current_process_identity();
        store
            .insert_task(&record("t-live", TaskState::Running, Some(pid), created))
            .unwrap();

        let result = reconcile(&store).unwrap();
        assert!(
            result.summary().is_none(),
            "reconciling a live task must be silent, or every `jean run` would warn \
             about the task that is currently working"
        );
    }
}
