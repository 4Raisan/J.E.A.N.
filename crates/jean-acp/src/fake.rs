//! Deterministic workers: the same contract, with no process and no network.
//!
//! §5 of the Phase-3 brief asks for coverage of cases a real agent cannot be made to produce on
//! demand — a permission request outside the run's scope, a completion claimed while the tests fail,
//! a stalled worker, a frame over the cap. A script is how those cases get tested *as contract
//! traffic*, through the same [`EventSink`] and [`PermissionHost`] the real connectors use, rather
//! than as assertions about a parser.
//!
//! This module is not reachable from a configuration: `jean-protocol` accepts only `acp` and
//! `codex-cli` as worker kinds, and `crate::run` refuses anything else. `jean-core`'s delegation runs
//! against an injected runner, and its tests hand it these scripts. Nothing here spawns, reads a
//! credential, or opens a socket.

use std::sync::atomic::Ordering;

use crate::contract::{
    EventSink, PermissionAnswer, PermissionHost, PermissionOffer, PermissionRequest, RunSpec,
    WorkerAction, WorkerEvent, WorkerOutcome, STOP_CANCELLED, STOP_END_TURN, STOP_FAILED,
    STOP_REFUSED, STOP_TIMEOUT,
};

/// One scripted thing a fake worker does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A progress event, in the same `source` vocabulary the connectors use.
    Say { source: String, text: String },
    /// A permission request the fake asks the host about, and records the answer for.
    Ask {
        title: String,
        action: WorkerAction,
        /// What the fake is willing to accept. A fake that offers nothing is testing the host's
        /// behaviour when an agent gives it no good answer to pick.
        offers: Vec<PermissionOffer>,
    },
    /// A cancellation request, checked against the run's cancel flag: a worker that ignores the
    /// operator is one of the cases the watchdog exists for.
    WaitForCancel,
    /// The script ends here.
    Stop {
        stop_reason: String,
        claim: Option<String>,
    },
}

/// A scripted worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub worker: String,
    pub steps: Vec<Step>,
    /// Bytes this worker claims to have produced, so a report test can assert on a number that is
    /// not "whatever the machine was doing".
    pub stdout_bytes: u64,
    /// A connector-level failure to report at settlement.
    pub failure: Option<String>,
}

impl Script {
    /// A worker that says one thing and finishes.
    pub fn simple(worker: &str, text: &str) -> Self {
        Self {
            worker: worker.to_owned(),
            stdout_bytes: text.len() as u64,
            steps: vec![
                Step::Say {
                    source: "agent_message".to_owned(),
                    text: text.to_owned(),
                },
                Step::Stop {
                    stop_reason: STOP_END_TURN.to_owned(),
                    claim: Some(text.to_owned()),
                },
            ],
            failure: None,
        }
    }

    pub fn with_steps(worker: &str, steps: Vec<Step>) -> Self {
        Self {
            worker: worker.to_owned(),
            steps,
            stdout_bytes: 0,
            failure: None,
        }
    }

    /// The answer the host gave to the nth request, for a test that has to assert on it.
    pub fn ask(title: &str, action: WorkerAction, offers: Vec<PermissionOffer>) -> Step {
        Step::Ask {
            title: title.to_owned(),
            action,
            offers,
        }
    }
}

/// Run a script through the same host and sink the real connectors use.
///
/// Returns the outcome the delegation layer would record, so a test of "the agent claimed success and
/// the verification failed" is a test of the recording, not of a mock's opinion.
pub fn play(
    spec: &RunSpec<'_>,
    script: &Script,
    host: &mut dyn PermissionHost,
    sink: &mut dyn EventSink,
) -> (WorkerOutcome, Vec<PermissionAnswer>) {
    let mut events = 0usize;
    let mut answers = Vec::new();
    let mut stop_reason = STOP_FAILED.to_owned();
    let mut claim = None;
    let mut failure = script.failure.clone();

    if !sink.emit(WorkerEvent::Started {
        worker: script.worker.clone(),
        kind: "script".to_owned(),
        pid: 0,
    }) {
        failure = Some("the event sink closed before the worker started".to_owned());
    }
    events += 1;

    for step in &script.steps {
        if failure.is_some() {
            break;
        }
        match step {
            Step::Say { source, text } => {
                events += 1;
                if !sink.emit(WorkerEvent::Progress {
                    seq: events as u64,
                    source: source.clone(),
                    text: text.clone(),
                }) {
                    failure = Some("the event queue was full".to_owned());
                }
            }
            Step::Ask {
                title,
                action,
                offers,
            } => {
                let request = PermissionRequest {
                    worker: script.worker.clone(),
                    tool_call_id: format!("script-{}", events + 1),
                    title: title.clone(),
                    action: action.clone(),
                    offers: offers.clone(),
                };
                let answer = host.decide(&request);
                // The same rule the ACP connector enforces at the wire: an answer naming an option
                // that was not offered is not an answer. A fake is not exempt, because the point of
                // the rule is that the host cannot be trusted to remember it.
                let answer = match &answer {
                    PermissionAnswer::Selected { option_id }
                        if !offers.iter().any(|o| &o.option_id == option_id) =>
                    {
                        PermissionAnswer::Denied {
                            reason: format!("{option_id} was not one of the offered options"),
                        }
                    }
                    other => other.clone(),
                };
                events += 1;
                let _ = sink.emit(WorkerEvent::PermissionAsked {
                    request,
                    answer: answer.clone(),
                });
                answers.push(answer);
            }
            Step::WaitForCancel => {
                // A script cannot be killed, so it waits for the operator the way a real worker
                // would: until the flag is raised, or the session deadline says otherwise.
                let until = spec.bounds.session_deadline;
                while !spec.cancel.load(Ordering::Relaxed) && std::time::Instant::now() < until {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                if spec.cancel.load(Ordering::Relaxed) {
                    stop_reason = STOP_CANCELLED.to_owned();
                } else {
                    stop_reason = STOP_TIMEOUT.to_owned();
                    failure = Some("the script outlived its session deadline".to_owned());
                }
                break;
            }
            Step::Stop {
                stop_reason: reason,
                claim: text,
            } => {
                stop_reason = reason.clone();
                claim = text.clone();
            }
        }
    }

    if stop_reason == STOP_REFUSED && failure.is_none() {
        failure = Some("the agent refused to continue".to_owned());
    }

    let outcome = WorkerOutcome {
        stop_reason,
        claim,
        exit_code: Some(0),
        events,
        stdout_bytes: script.stdout_bytes,
        output_dir: spec.bounds.output_dir.clone(),
        failure,
        failure_kind: None,
        remaining_work: None,
    };
    let _ = sink.emit(WorkerEvent::Settled(outcome.clone()));
    (outcome, answers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Bounds, DelegateRequest, VecSink};
    use jean_protocol::config::WorkerConfig;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    struct AlwaysDeny;

    impl PermissionHost for AlwaysDeny {
        fn decide(&mut self, _request: &PermissionRequest) -> PermissionAnswer {
            PermissionAnswer::Denied {
                reason: "auto_deny says so".to_owned(),
            }
        }
    }

    /// A host that always says yes to the first option, which is exactly the mistake the guard below
    /// exists to catch.
    struct NaiveYes;

    impl PermissionHost for NaiveYes {
        fn decide(&mut self, _request: &PermissionRequest) -> PermissionAnswer {
            PermissionAnswer::Selected {
                option_id: "grant-everything".to_owned(),
            }
        }
    }

    fn spec() -> (
        WorkerConfig,
        jean_process::spec::LaunchSpec,
        DelegateRequest,
        Bounds,
        Arc<AtomicBool>,
    ) {
        let worker = WorkerConfig {
            name: "fake".to_owned(),
            kind: "acp".to_owned(),
            command: "unused".to_owned(),
            args: Vec::new(),
            enabled: true,
            model: String::new(),
            free_fallback: false,
            env_passthrough: Vec::new(),
            capability: "code-edit".to_owned(),
            sandbox: "workspace-write".to_owned(),
        };
        let launch = jean_process::spec::LaunchSpec {
            executable: PathBuf::from("unused"),
            args: Vec::new(),
            working_dir: None,
            env: Default::default(),
            timeout: None,
            output_cap_bytes: 1024,
            stdout_path: None,
            stderr_path: None,
            terminal: jean_process::spec::TerminalMode::Pipes,
        };
        let request = DelegateRequest {
            delegation_id: "d-1".to_owned(),
            task_id: "t-1".to_owned(),
            goal: "make the test pass".to_owned(),
            workspace: PathBuf::from("."),
            prompt: "make the test pass".to_owned(),
        };
        let bounds = Bounds {
            max_frame_bytes: 4096,
            max_event_queue: 8,
            session_deadline: Instant::now() + Duration::from_secs(30),
            idle_timeout: Duration::from_secs(5),
            permission_timeout: Duration::from_secs(2),
            shutdown_grace: Duration::from_millis(50),
            max_output_bytes: 4096,
            output_dir: None,
        };
        (
            worker,
            launch,
            request,
            bounds,
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn a_script_produces_started_progress_and_a_settlement_in_that_order() {
        let (worker, launch, request, bounds, cancel) = spec();
        let run = RunSpec {
            worker: &worker,
            launch: &launch,
            request: &request,
            bounds: &bounds,
            cancel,
        };
        let mut sink = VecSink::default();
        let (outcome, _answers) = play(
            &run,
            &Script::simple("fake", "wrote the patch"),
            &mut AlwaysDeny,
            &mut sink,
        );
        assert!(matches!(sink.events[0], WorkerEvent::Started { .. }));
        assert!(matches!(sink.events[1], WorkerEvent::Progress { .. }));
        assert!(matches!(sink.events[2], WorkerEvent::Settled(_)));
        assert_eq!(outcome.stop_reason, STOP_END_TURN);
        assert_eq!(outcome.claim.as_deref(), Some("wrote the patch"));
        assert!(outcome.ended_cleanly());
    }

    #[test]
    fn an_answer_naming_an_option_that_was_never_offered_is_not_an_answer() {
        let (worker, launch, request, bounds, cancel) = spec();
        let run = RunSpec {
            worker: &worker,
            launch: &launch,
            request: &request,
            bounds: &bounds,
            cancel,
        };
        let mut sink = VecSink::default();
        let script = Script::with_steps(
            "fake",
            vec![Script::ask(
                "touch a file",
                WorkerAction::EditFile {
                    path: "src/lib.rs".to_owned(),
                },
                vec![PermissionOffer {
                    option_id: "once".to_owned(),
                    name: "Allow once".to_owned(),
                    kind: "allow_once".to_owned(),
                }],
            )],
        );
        let (_outcome, answers) = play(&run, &script, &mut NaiveYes, &mut sink);
        assert!(
            matches!(&answers[0], PermissionAnswer::Denied { reason } if reason.contains("not one of the offered")),
            "{:?}",
            answers[0]
        );
    }
}
