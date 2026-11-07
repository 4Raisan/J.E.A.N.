//! The one contract every external worker is written against.
//!
//! ## Why a contract instead of two connectors
//!
//! ACP and `codex exec --json` disagree about almost everything: ACP is a bidirectional JSON-RPC
//! session that asks for permission mid-turn, the CLI fallback is a one-way JSONL event stream that
//! cannot ask anything because nobody is listening. What must not differ is what JEAN does with them
//! — which event reaches the task board, who decides a permission request, what gets recorded when
//! the worker claims it finished, and what happens to its process tree on cancel. So the connectors
//! below produce **these** types and nothing else escapes to the caller.
//!
//! ## What the contract refuses to be
//!
//! * **Not an authority.** A worker's `PermissionRequest` carries what it wants to do, in JEAN's own
//!   operation vocabulary, and the answer comes from [`PermissionHost`] — which in `jean-core` is the
//!   existing policy engine, with the run's grant consulted in the same position it occupies for a
//!   plan step. A worker cannot widen what it was offered: options it did not list are not
//!   answerable, and an answer for a different action is not an answer for this one.
//! * **Not a shell.** The command a worker asks to run arrives as an argv, and is decided as one.
//!   Nothing here builds a shell string, and the verification command that decides whether the
//!   delegation worked was resolved *before* the delegation started, from configuration the worker
//!   never sees.
//! * **Not unbounded.** Every number in [`Bounds`] is enforced by the connector: frame size, event
//!   queue depth, session and idle deadlines, permission wait, output bytes, shutdown grace. The ACP
//!   SDK frames NDJSON lines with an unbounded reader, so [`Bounds::max_frame_bytes`] is the only
//!   ceiling between a chatty agent and this process's memory.
//!
//! Progress events go out through [`EventSink`] rather than a return value because a delegation that
//! produces fifty tool calls must not be summarised after the fact — the operator sees them as they
//! arrive, and a full queue stops the reader, which is back-pressure rather than loss.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jean_protocol::config::WorkerConfig;
use jean_protocol::event::EventKind;

/// What the worker's session produced, as observed by the connector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerOutcome {
    /// The worker's own reason for stopping, in its vocabulary, normalised to one of
    /// [`STOP_END_TURN`] / [`STOP_REFUSED`] / [`STOP_CANCELLED`] / [`STOP_FAILED`] /
    /// [`STOP_MAX_TOKENS`] / [`STOP_TIMEOUT`]. A CLI connector that only sees an exit code reports
    /// `failed` or `end_turn` — it is not permitted to invent a claim it did not observe.
    pub stop_reason: String,
    /// The worker's claim about its own result, if it made one. Recorded, never believed.
    pub claim: Option<String>,
    /// The process exit code, once the child is gone. `None` means the connector terminated it and
    /// the exit code was not recoverable.
    pub exit_code: Option<i32>,
    /// Events handed to the sink, including the settlement event.
    pub events: usize,
    /// Bytes of protocol traffic read from the worker's stdout.
    pub stdout_bytes: u64,
    /// Where the worker's stdout/stderr captures were written, when a capture was configured.
    pub output_dir: Option<PathBuf>,
    /// A connector-level failure: malformed frames, an unexpected exit, a stalled session. This is
    /// what separates "the agent did the work and it did not pass" from "we never got an answer".
    pub failure: Option<String>,
}

impl WorkerOutcome {
    /// Did the worker's session itself end cleanly? Says nothing about the task: that is the
    /// verification's question, and this build never merges the two.
    pub fn ended_cleanly(&self) -> bool {
        self.failure.is_none() && self.stop_reason == STOP_END_TURN
    }
}

/// A delegation, in the shape a connector needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateRequest {
    pub delegation_id: String,
    pub task_id: String,
    /// The operator's own words, verbatim.
    pub goal: String,
    /// The absolute directory the worker runs in — a worktree, or the workspace with a reason.
    pub workspace: PathBuf,
    /// The instruction handed to the worker: the goal, plus the check it must pass, stated as data.
    /// A worker can read this; it cannot edit it.
    pub prompt: String,
}

/// The limits a connector enforces. Built from `[delegation]` by `jean-core`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bounds {
    pub max_frame_bytes: usize,
    pub max_event_queue: usize,
    pub session_deadline: Instant,
    pub idle_timeout: Duration,
    pub permission_timeout: Duration,
    pub shutdown_grace: Duration,
    pub max_output_bytes: u64,
    /// Where to write the worker's stdout/stderr captures. `None` means discard, which is a
    /// deliberate choice for a fake worker in a test.
    pub output_dir: Option<PathBuf>,
}

/// The set of options a worker offered for one permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOffer {
    pub option_id: String,
    pub name: String,
    /// `allow_once` | `allow_always` | `reject_once` | `reject_always`, the ACP vocabulary, used for
    /// CLI adapters too so a host never has to know two sets of words.
    pub kind: String,
}

/// What a worker wants to do, in terms the policy engine can decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerAction {
    /// Run this argv, in this directory if given. Never a shell string.
    RunCommand {
        argv: Vec<String>,
        cwd: Option<String>,
    },
    /// Write to this path.
    EditFile { path: String },
    /// Read this path.
    ReadFile { path: String },
    /// Reach this target.
    Network { target: String },
    /// Anything the connector could not classify. A worker's own sentence, kept as text so the
    /// operator sees what was asked, and decided under the worker's capability like any other action.
    Unclassified { text: String },
}

impl WorkerAction {
    pub fn describe(&self) -> String {
        match self {
            WorkerAction::RunCommand { argv, cwd } => match cwd {
                Some(dir) => format!("run {argv:?} in {dir}"),
                None => format!("run {argv:?}"),
            },
            WorkerAction::EditFile { path } => format!("write {path}"),
            WorkerAction::ReadFile { path } => format!("read {path}"),
            WorkerAction::Network { target } => format!("reach {target}"),
            WorkerAction::Unclassified { text } => text.clone(),
        }
    }

    /// The path this action touches, if it names one. Used to bind a permission request to the
    /// workspace the operator actually approved — a request to write outside it is not covered by
    /// anything a run grant could have said yes to.
    pub fn touched_path(&self) -> Option<&str> {
        match self {
            WorkerAction::EditFile { path } | WorkerAction::ReadFile { path } => Some(path),
            WorkerAction::RunCommand { cwd, .. } => cwd.as_deref(),
            _ => None,
        }
    }
}

/// One permission request from a worker, already translated into JEAN's vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub worker: String,
    pub tool_call_id: String,
    pub title: String,
    pub action: WorkerAction,
    /// What the worker is willing to accept. A host may only answer with one of these ids.
    pub offers: Vec<PermissionOffer>,
}

/// The host's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAnswer {
    /// One of the worker's own options, chosen by the policy engine.
    Selected { option_id: String },
    /// Refused, with the reason the policy gave. The connector turns this into the worker's reject
    /// option — or, when the worker offered no reject, into a failed session, because a silent no is
    /// how an agent hangs forever waiting for an answer that already came.
    Denied { reason: String },
    /// The session is going away: cancelled, timed out, or the operator left.
    Cancelled,
}

impl PermissionAnswer {
    pub fn is_yes(&self) -> bool {
        matches!(self, PermissionAnswer::Selected { .. })
    }
}

/// Something happened that the operator should see while the worker runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEvent {
    /// The worker process exists and its pipes are open.
    Started {
        worker: String,
        kind: String,
        pid: u32,
    },
    /// Progress. `source` is the worker's own category normalised: `agent_message`, `tool_call`,
    /// `plan`, `status`.
    Progress {
        seq: u64,
        source: String,
        text: String,
    },
    /// The worker asked. The answer is already decided — this event is the record of the question
    /// and the answer, so a transcript shows what was permitted without re-reading the audit log.
    PermissionAsked {
        request: PermissionRequest,
        answer: PermissionAnswer,
    },
    /// The session ended.
    Settled(WorkerOutcome),
    /// The connector gave up: malformed protocol, stalled, or the deadline.
    Failed { reason: String },
}

impl WorkerEvent {
    /// The audit vocabulary this event belongs to, so the transcript and `events` table use one set
    /// of names rather than two that drift.
    pub fn kind(&self) -> EventKind {
        match self {
            WorkerEvent::Started { .. } => EventKind::WorkerStarted,
            WorkerEvent::Progress { .. } => EventKind::WorkerProgress,
            WorkerEvent::PermissionAsked { .. } => EventKind::PermissionRequested,
            WorkerEvent::Settled(_) => EventKind::WorkerSettled,
            WorkerEvent::Failed { .. } => EventKind::SupervisorError,
        }
    }

    /// The one-line detail for the record: redacted by the caller before it is written anywhere.
    pub fn detail(&self) -> String {
        match self {
            WorkerEvent::Started { worker, kind, pid } => {
                format!("worker {worker} ({kind}) started as pid {pid}")
            }
            WorkerEvent::Progress { seq, source, text } => format!("#{seq} {source}: {text}"),
            WorkerEvent::PermissionAsked { request, answer } => {
                let verdict = match answer {
                    PermissionAnswer::Selected { option_id } => {
                        let name = request
                            .offers
                            .iter()
                            .find(|o| &o.option_id == option_id)
                            .map(|o| o.kind.as_str())
                            .unwrap_or("unknown-option");
                        format!("answered {name} ({option_id})")
                    }
                    PermissionAnswer::Denied { reason } => format!("refused: {reason}"),
                    PermissionAnswer::Cancelled => "cancelled before an answer".to_owned(),
                };
                format!(
                    "{} asked to {} — {verdict}",
                    request.worker,
                    request.action.describe()
                )
            }
            WorkerEvent::Settled(outcome) => {
                let claim = outcome
                    .claim
                    .as_deref()
                    .map(|c| format!(", claimed {c:?}"))
                    .unwrap_or_default();
                let exit = outcome
                    .exit_code
                    .map(|c| format!(", exit {c}"))
                    .unwrap_or_default();
                let failure = outcome
                    .failure
                    .as_deref()
                    .map(|f| format!(", failure: {f}"))
                    .unwrap_or_default();
                format!(
                    "worker settled: stop_reason={}{exit}{claim}{failure}",
                    outcome.stop_reason
                )
            }
            WorkerEvent::Failed { reason } => reason.clone(),
        }
    }
}

/// The host's side of a permission request: decide, in JEAN's terms, with the operator's authority.
///
/// Implemented in `jean-core` over `jean-policy`, which is what keeps this crate from reaching the
/// store or the policy engine itself: the connector asks, and cannot see how the answer was produced.
///
/// ## Why there is no `Send` bound
///
/// Deliberately. The session runs inside `block_on` on a current-thread runtime, on the caller's own
/// thread, so a host may hold the things a decision actually needs — a `rusqlite` connection (which is
/// not `Sync`) and the operator's prompt reader. Requiring `Send` here would force `jean-core` to move
/// policy onto another thread and share a database it cannot share, and the alternative — a channel
/// between two threads, each half-deciding — is a second permission system wearing the first one's
/// clothes. A connector that wants its host on a worker thread must own a host that can go there.
pub trait PermissionHost {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionAnswer;
}

/// Where progress goes. `false` means the sink is gone and the connector should stop reading —
/// back-pressure made visible rather than a queue that grows without limit.
///
/// Same reasoning as [`PermissionHost`]: no `Send`, because the sink is the task's own database row
/// writer and it writes from the thread that owns the connection.
pub trait EventSink {
    fn emit(&mut self, event: WorkerEvent) -> bool;
}

/// A sink that keeps everything, for tests and for a caller that wants the whole transcript.
#[derive(Debug, Default)]
pub struct VecSink {
    pub events: Vec<WorkerEvent>,
}

impl EventSink for VecSink {
    fn emit(&mut self, event: WorkerEvent) -> bool {
        self.events.push(event);
        true
    }
}

/// Everything a connector needs to start one worker.
pub struct RunSpec<'a> {
    pub worker: &'a WorkerConfig,
    /// The already-resolved launch: executable, argv, working directory, environment.
    ///
    /// Built by `jean-core`, not here. Resolution goes through the tool registry and the
    /// allowlist-pinned environment the same way a Phase-1 command does, so a connector cannot spawn
    /// a binary the operator never approved, and cannot inherit a credential the config dropped.
    pub launch: &'a jean_process::spec::LaunchSpec,
    pub request: &'a DelegateRequest,
    pub bounds: &'a Bounds,
    /// The cancel flag the CLI's Ctrl-C handler and the IPC pipe both raise. A connector checks it
    /// between frames and terminates the job when it is set — the same shape as the model transport.
    pub cancel: Arc<AtomicBool>,
}

/// A worker's own reason for ending a turn, in one vocabulary.
pub const STOP_END_TURN: &str = "end_turn";
/// The worker refused to continue.
pub const STOP_REFUSED: &str = "refused";
/// The session was cancelled by JEAN or by the operator.
pub const STOP_CANCELLED: &str = "cancelled";
/// The worker stopped because it could not go on.
pub const STOP_FAILED: &str = "failed";
/// The token budget was reached.
pub const STOP_MAX_TOKENS: &str = "max_tokens";
/// The turn hit its own limit, not this build's deadline.
pub const STOP_MAX_TURN_REQUESTS: &str = "max_turn_requests";
/// This build's deadline elapsed and the job was terminated. Distinct from every reason a worker
/// can report, because no worker reports being killed.
pub const STOP_TIMEOUT: &str = "jean_timeout";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_worker_event_kind_is_distinct_and_named() {
        // A transcript and the audit trail share one vocabulary only if both read this enum, so the
        // mapping is asserted rather than assumed.
        let started = WorkerEvent::Started {
            worker: "opencode".to_owned(),
            kind: "acp".to_owned(),
            pid: 4242,
        };
        assert_eq!(started.kind(), EventKind::WorkerStarted);
        assert_eq!(
            started.detail(),
            "worker opencode (acp) started as pid 4242"
        );

        let asked = WorkerEvent::PermissionAsked {
            request: PermissionRequest {
                worker: "codex".to_owned(),
                tool_call_id: "t1".to_owned(),
                title: "run tests".to_owned(),
                action: WorkerAction::RunCommand {
                    argv: vec!["cargo".to_owned(), "test".to_owned()],
                    cwd: Some("C:\\ws".to_owned()),
                },
                offers: vec![PermissionOffer {
                    option_id: "once".to_owned(),
                    name: "Allow once".to_owned(),
                    kind: "allow_once".to_owned(),
                }],
            },
            answer: PermissionAnswer::Selected {
                option_id: "once".to_owned(),
            },
        };
        let text = asked.detail();
        assert!(text.contains("allow_once"), "{text}");
        assert!(
            text.contains("run [\"cargo\", \"test\"] in C:\\ws"),
            "{text}"
        );
    }

    #[test]
    fn a_refusal_and_a_cancellation_read_differently_in_the_record() {
        // "the operator said no" and "nobody was there to say anything" are different failures, and
        // the report has to be able to tell them apart.
        let request = PermissionRequest {
            worker: "codex".to_owned(),
            tool_call_id: "t2".to_owned(),
            title: String::new(),
            action: WorkerAction::EditFile {
                path: "src/main.rs".to_owned(),
            },
            offers: Vec::new(),
        };
        let denied = WorkerEvent::PermissionAsked {
            request: request.clone(),
            answer: PermissionAnswer::Denied {
                reason: "auto_deny covers write.outside_root".to_owned(),
            },
        };
        let gone = WorkerEvent::PermissionAsked {
            request,
            answer: PermissionAnswer::Cancelled,
        };
        assert!(denied.detail().contains("refused: auto_deny"));
        assert!(gone.detail().contains("cancelled before an answer"));
    }

    #[test]
    fn an_action_reports_the_path_it_touches_or_says_it_does_not() {
        // The workspace binding is what stops a run grant from covering an edit outside the tree the
        // operator pointed at.
        assert_eq!(
            WorkerAction::EditFile {
                path: "a\\b.txt".to_owned()
            }
            .touched_path(),
            Some("a\\b.txt")
        );
        assert_eq!(
            WorkerAction::RunCommand {
                argv: vec!["x".to_owned()],
                cwd: None
            }
            .touched_path(),
            None
        );
    }

    #[test]
    fn a_clean_ending_is_not_a_successful_task() {
        let outcome = WorkerOutcome {
            stop_reason: STOP_END_TURN.to_owned(),
            claim: Some("all tests pass".to_owned()),
            exit_code: Some(0),
            events: 4,
            stdout_bytes: 900,
            output_dir: None,
            failure: None,
        };
        assert!(outcome.ended_cleanly());
        // The claim is still only a claim; nothing here decides the task's result.
        assert_eq!(outcome.claim.as_deref(), Some("all tests pass"));

        let timed_out = WorkerOutcome {
            stop_reason: STOP_TIMEOUT.to_owned(),
            ..outcome
        };
        assert!(!timed_out.ended_cleanly());
    }
}
