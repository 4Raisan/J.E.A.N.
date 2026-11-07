//! Execution events: the append-only record of what happened to a task.
//!
//! Events are distinct from task state. State answers "where is this task now";
//! events answer "what happened, in order, and why". Reconciliation after a
//! crash reads events to explain a state, which is why every state change also
//! emits one.

use serde::{Deserialize, Serialize};

use crate::state::TaskState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Task row created.
    TaskCreated,
    /// A legal state transition occurred. The detail names both states.
    StateChanged,
    /// A child process was created. Detail carries the pid.
    ProcessSpawned,
    /// The child exited on its own. Detail carries the exit code.
    ProcessExited,
    /// The Job Object was terminated because the deadline elapsed.
    TimedOut,
    /// The Job Object was terminated because a cancel request arrived.
    CancelRequested,
    /// Output stream hit its cap and further bytes were discarded.
    OutputTruncated,
    /// A capture, transcript or artifact was written. Detail carries the path.
    ArtifactWritten,
    /// Something failed that is not the child's fault (spawn, job assignment…).
    SupervisorError,
    /// A reconciliation pass touched this task. Detail says what and why.
    Reconciled,
    /// A permission or policy decision. Phase 0 emits these only for refusals.
    PolicyDecision,
    /// A plan request was sent to a provider. Phase 2. The detail names the provider, the model
    /// and the prompt size — never a credential, and never the prompt itself.
    ModelRequested,
    /// The provider answered. Phase 2. The detail carries the attempt count and the byte size, so
    /// "the model answered twice because the first reply was unreadable" is on the record.
    ModelResponded,
    /// A plan arrived and was parsed, before validation. Phase 2.
    PlanProposed,
    /// A proposed plan passed structure and registry validation. Phase 2.
    PlanValidated,
    /// A proposed plan was refused. Phase 2. The detail carries the diagnostic, which is the only
    /// place a rejected plan is recorded at all.
    PlanRejected,
    /// A plan step began executing through the intent pipeline. Phase 2.
    StepStarted,
    /// A plan step finished, with its verification verdict. Phase 2.
    StepFinished,
    /// A plan step was not executed because something it depends on did not run. Phase 2.
    StepSkipped,
    /// A worker process was started for a delegation, inside the job object, with its pipes open.
    /// Phase 3. The detail names the connector and the pid.
    WorkerStarted,
    /// Progress from a worker: an agent message, a tool call it made, a plan update it published.
    /// Phase 3. Detail carries the summarised text, never the raw frame.
    WorkerProgress,
    /// The worker asked for permission to do something. Phase 3. The answer is a `PolicyDecision`,
    /// recorded next to this — a request with no recorded answer is a bug in this build.
    PermissionRequested,
    /// The worker's session ended: it reported a stop reason, or it died. Phase 3. The detail
    /// carries the stop reason and the exit code, because "the agent said done" and "the agent
    /// exited 0" are different claims and both belong on the record.
    WorkerSettled,
    /// A detached git worktree was prepared for a delegation, or the operator's checkout was used
    /// with the reason recorded. Phase 3.
    WorktreePrepared,
    /// A changed-file summary was captured for a delegation. Phase 3. Detail carries the count and
    /// the list up to the configured ceiling.
    ChangesRecorded,
    /// JEAN ran the configured verification command itself. Phase 3. Detail carries the verdict;
    /// this event is the difference between an agent's claim and an observation.
    VerificationRecorded,
    /// An MCP tool was called through the policy pipeline. Phase 3. Detail names the server, the
    /// tool and the byte size of the result — never the arguments, which can carry a credential.
    McpToolCalled,
    /// One step of a browser workflow: navigate, read, interact, verify. Phase 3. Detail names the
    /// action and the observed result.
    BrowserAction,
    /// One desktop observation or action against a window: enumerate, inspect, act, verify. Phase 4.
    ///
    /// The detail names the window by handle and pid, the operation, and the verdict — never a control's
    /// content and never a window title beyond the bounded label. A desktop event is the one kind whose
    /// detail could otherwise carry another application's document names and message text into the log.
    DesktopAction,
    /// Free-form diagnostic.
    Note,
}

impl EventKind {
    /// Every kind, in the order the enum declares them.
    ///
    /// This exists because a list written out twice drifts: the store's own reverse table missed
    /// the Phase-2 kinds, and every one of them read back as a generic note. The round-trip test
    /// below is built from this list so a new kind cannot join the enum and not the names.
    pub const ALL: &'static [EventKind] = &[
        EventKind::TaskCreated,
        EventKind::StateChanged,
        EventKind::ProcessSpawned,
        EventKind::ProcessExited,
        EventKind::TimedOut,
        EventKind::CancelRequested,
        EventKind::OutputTruncated,
        EventKind::ArtifactWritten,
        EventKind::SupervisorError,
        EventKind::Reconciled,
        EventKind::PolicyDecision,
        EventKind::ModelRequested,
        EventKind::ModelResponded,
        EventKind::PlanProposed,
        EventKind::PlanValidated,
        EventKind::PlanRejected,
        EventKind::StepStarted,
        EventKind::StepFinished,
        EventKind::StepSkipped,
        EventKind::WorkerStarted,
        EventKind::WorkerProgress,
        EventKind::PermissionRequested,
        EventKind::WorkerSettled,
        EventKind::WorktreePrepared,
        EventKind::ChangesRecorded,
        EventKind::VerificationRecorded,
        EventKind::McpToolCalled,
        EventKind::BrowserAction,
        EventKind::DesktopAction,
        EventKind::Note,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::TaskCreated => "task_created",
            EventKind::StateChanged => "state_changed",
            EventKind::ProcessSpawned => "process_spawned",
            EventKind::ProcessExited => "process_exited",
            EventKind::TimedOut => "timed_out",
            EventKind::CancelRequested => "cancel_requested",
            EventKind::OutputTruncated => "output_truncated",
            EventKind::ArtifactWritten => "artifact_written",
            EventKind::SupervisorError => "supervisor_error",
            EventKind::Reconciled => "reconciled",
            EventKind::PolicyDecision => "policy_decision",
            EventKind::ModelRequested => "model_requested",
            EventKind::ModelResponded => "model_responded",
            EventKind::PlanProposed => "plan_proposed",
            EventKind::PlanValidated => "plan_validated",
            EventKind::PlanRejected => "plan_rejected",
            EventKind::StepStarted => "step_started",
            EventKind::StepFinished => "step_finished",
            EventKind::StepSkipped => "step_skipped",
            EventKind::WorkerStarted => "worker_started",
            EventKind::WorkerProgress => "worker_progress",
            EventKind::PermissionRequested => "permission_requested",
            EventKind::WorkerSettled => "worker_settled",
            EventKind::WorktreePrepared => "worktree_prepared",
            EventKind::ChangesRecorded => "changes_recorded",
            EventKind::VerificationRecorded => "verification_recorded",
            EventKind::McpToolCalled => "mcp_tool_called",
            EventKind::BrowserAction => "browser_action",
            EventKind::DesktopAction => "desktop_action",
            EventKind::Note => "note",
        }
    }

    /// The kind a stored name reads back as, or `None` when this build does not know it.
    ///
    /// Derived from [`Self::as_str`] rather than a second table, which is the only way the two
    /// stay honest with each other.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.as_str() == name)
    }
}

impl std::str::FromStr for EventKind {
    type Err = ();

    /// The same lookup as [`EventKind::from_name`], in the shape a stored name should parse into.
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::from_name(name).ok_or(())
    }
}

/// An event as it travels between layers. Persistence adds `seq` and `at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub kind: EventKind,
    /// Human-readable, already redacted. Must never contain a raw secret.
    pub detail: String,
}

impl Event {
    pub fn new(kind: EventKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    pub fn note(detail: impl Into<String>) -> Self {
        Self::new(EventKind::Note, detail)
    }

    /// The standard event for a state transition. Both states are named so a log
    /// reader never has to look up the previous row.
    pub fn state_changed(from: TaskState, to: TaskState, reason: Option<&str>) -> Self {
        let detail = match reason {
            Some(r) if !r.is_empty() => format!("{from} -> {to}: {r}"),
            _ => format!("{from} -> {to}"),
        };
        Self::new(EventKind::StateChanged, detail)
    }

    pub fn spawn(pid: u32, executable: &str) -> Self {
        Self::new(
            EventKind::ProcessSpawned,
            format!("pid {pid}: {executable}"),
        )
    }

    pub fn exit(code: i32) -> Self {
        Self::new(EventKind::ProcessExited, format!("exit code {code}"))
    }

    pub fn truncated(stream: &str, cap_bytes: u64, produced_bytes: u64) -> Self {
        Self::new(
            EventKind::OutputTruncated,
            format!(
                "{stream} exceeded its {cap_bytes}-byte cap; the child produced {produced_bytes} \
                 bytes and the excess was drained and discarded"
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_changed_names_both_states() {
        let e = Event::state_changed(TaskState::Queued, TaskState::Running, None);
        assert!(e.detail.contains("queued"), "{}", e.detail);
        assert!(e.detail.contains("running"), "{}", e.detail);
        assert_eq!(e.kind, EventKind::StateChanged);
    }

    #[test]
    fn state_changed_includes_the_reason_when_given() {
        let e = Event::state_changed(
            TaskState::Running,
            TaskState::NeedsAttention,
            Some("runner pid 4242 is no longer alive"),
        );
        assert!(e.detail.contains("runner pid 4242"), "{}", e.detail);

        // An empty reason must not leave a dangling colon.
        let e = Event::state_changed(TaskState::Queued, TaskState::Running, Some(""));
        assert!(!e.detail.ends_with(':'), "{}", e.detail);
    }

    #[test]
    fn truncation_event_explains_that_draining_continued() {
        let e = Event::truncated("stdout", 1024, 4096);
        assert!(e.detail.contains("stdout"));
        assert!(e.detail.contains("1024"));
        assert!(e.detail.contains("4096"));
        assert!(
            e.detail.contains("drained"),
            "the event must say the child was not blocked: {}",
            e.detail
        );
    }

    #[test]
    fn event_kinds_round_trip_through_serde() {
        for k in EventKind::ALL {
            let json = serde_json::to_string(k).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()));
            let back: EventKind = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, k);
        }
    }

    #[test]
    fn no_two_kinds_share_a_string() {
        // The database column is the string, so a collision would silently merge
        // two different happenings into one history entry.
        let mut seen: Vec<&str> = Vec::new();
        for kind in EventKind::ALL {
            assert!(
                !seen.contains(&kind.as_str()),
                "{} repeats an earlier kind",
                kind.as_str()
            );
            seen.push(kind.as_str());
        }
        assert_eq!(seen.len(), EventKind::ALL.len());
    }

    #[test]
    fn every_kind_reads_back_from_its_stored_name() {
        // The store persists `as_str` and has to turn it back into a kind. A name that does not
        // round-trip is not a crash: the row reads as a generic note, and a plan's step history
        // silently loses what it was. This test is the reason there is one list, not two.
        for kind in EventKind::ALL {
            assert_eq!(
                kind.as_str().parse::<EventKind>().ok(),
                Some(*kind),
                "{} must read back as itself",
                kind.as_str()
            );
        }
        assert!("written_by_a_newer_build".parse::<EventKind>().is_err());
        assert!("".parse::<EventKind>().is_err());
    }

    #[test]
    fn event_serialises_with_both_fields() {
        let json = serde_json::to_string(&Event::exit(3)).unwrap();
        assert!(json.contains("\"kind\":\"process_exited\""), "{json}");
        assert!(json.contains("exit code 3"), "{json}");
    }
}
