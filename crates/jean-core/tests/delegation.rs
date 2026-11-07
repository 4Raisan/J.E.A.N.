//! Phase 3, driven by a scripted worker: a delegatee acts, J.E.A.N. decides, the records tell.
//!
//! Nothing here starts an agent. Every session is a [`ScriptedRunner`] that asks the permission host
//! the questions a real ACP worker asks and returns the outcome a real connector would return, so the
//! only variable is what **J.E.A.N.** did with it — which is what these tests are about. The policy
//! engine, the audit writer, the Job Object supervisor, the store and the verifier are the same code
//! Phase 0, 1 and 2 were tested with.
//!
//! ## What is asserted, and why that is the right place
//!
//! * A delegation with no check to run is refused **before a worker is started**, proved by counting
//!   rows and by the runner never being called — not by reading a message.
//! * A worker that ends its turn saying "all tests pass" while the configured check exits 1 is
//!   reported as `failed`, and the two facts are stored in different columns.
//! * An ask inside the workspace is answered without bothering the operator; an ask outside it is
//!   refused without bothering the operator; neither reaches the prompter.
//! * A worker is only ever answered with an option it offered itself, and never with `allow_always`
//!   when it also offered `allow_once`.
//! * Progress is bounded, redacted on the way in, and the events that were refused are reported.
//!
//! ## Configuration
//!
//! `JEAN_DATA_DIR` is process-global and cargo runs these tests in threads, so every scenario holds
//! `ENV_LOCK` for as long as its context lives, exactly as `tests/plans.rs` and `tests/intents.rs` do.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard};

use jean_acp::contract::{
    EventSink, PermissionAnswer, PermissionHost, PermissionOffer, PermissionRequest, RunSpec,
    WorkerAction, WorkerEvent, WorkerOutcome, STOP_END_TURN, STOP_FAILED,
};
use jean_acp::WorkerError;
use jean_core::delegation::{delegate_with, DelegateRequest, DelegationResult, WorkerRunner};
use jean_core::Context;
use jean_policy::{Answer, DenyReason, ScriptedPrompt};
use jean_protocol::{EventKind, TaskState};
use jean_store::DelegationRecord;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The `cmd.exe` every Windows install has, as an absolute path.
///
/// An absolute path rather than a bare name because these tests must not depend on the caller's
/// `PATH`: the verification command is resolved before the worker exists, and a scenario that failed
/// to find `cmd.exe` would fail for the wrong reason.
fn system32(program: &str) -> PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| String::from("C:\\Windows"));
    Path::new(&root).join("System32").join(program)
}

/// A TOML basic string's worth of a path.
fn toml_path(path: &Path) -> String {
    format!("\"{}\"", path.display().to_string().replace('\\', "\\\\"))
}

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jean-delegate-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock must be past the epoch")
            .as_nanos()
    ))
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// `[delegation]` with one scripted worker and a check that passes.
///
/// `extra` is one or more `key = value` lines that **replace** the setting of the same name below —
/// a scenario that redefined a key by appending would fail to load, because TOML refuses duplicates
/// and `[delegation]` has `deny_unknown_fields`.
fn delegation_section(verify_args: &[&str], extra: &str) -> String {
    let args = verify_args
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
        r#"[delegation]
enabled = true
default_worker = "fake"
verify_command = {}
verify_args = [{args}]
verify_timeout_secs = 60
session_timeout_secs = 120
idle_timeout_secs = 60
permission_timeout_secs = 10
shutdown_grace_ms = 200
max_event_queue = 64
max_output_bytes = 65536
isolate_in_worktree = false
"#,
        toml_path(&system32("cmd.exe"))
    );
    let mut lines: Vec<String> = body.lines().map(str::to_owned).collect();
    let extra = extra.trim();
    if !extra.is_empty() {
        let redefined: Vec<String> = extra
            .lines()
            .filter_map(|line| line.split('=').next())
            .map(|name| format!("{} =", name.trim()))
            .collect();
        lines.retain(|line| !redefined.iter().any(|prefix| line.starts_with(prefix)));
        lines.extend(extra.lines().map(str::to_owned));
    }
    // The worker belongs after the section's own keys, or it would inherit them.
    lines.extend([
        "[[delegation.workers]]".to_owned(),
        "name = \"fake\"".to_owned(),
        "kind = \"codex-cli\"".to_owned(),
        format!("command = {}", toml_path(&system32("cmd.exe"))),
        "args = [\"/c\", \"echo\", \"worker\"]".to_owned(),
        "capability = \"agent.delegate\"".to_owned(),
        "sandbox = \"workspace-write\"".to_owned(),
    ]);
    lines.join("\n") + "\n"
}

/// A data directory whose workspace is `dir/work`, already holding one file.
fn scenario(tag: &str, delegation: &str) -> (PathBuf, PathBuf, Context, MutexGuard<'static, ()>) {
    let dir = temp_dir(tag);
    let workspace = dir.join("work");
    std::fs::create_dir_all(&workspace).expect("the workspace must be creatable");
    std::fs::write(workspace.join("note.txt"), "one line\n").unwrap();
    std::fs::write(
        dir.join("tools.toml"),
        "config_version = 1\n\n[[tool]]\nid = \"echo.tool\"\nkind = \"shell\"\ncommand = \"cmd.exe\"\
         \nargs = [\"/c\", \"echo\", \"ran\"]\nverify = { method = \"exit_code\", expected = 0 }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("jean.toml"),
        format!(
            "[workspace]\nroot = {}\n{delegation}",
            toml_path(&workspace)
        ),
    )
    .expect("the configuration must be writable");

    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("JEAN_DATA_DIR", &dir);
    let ctx = Context::load(None).expect("a valid data directory must load");
    assert!(
        ctx.tools.get("echo.tool").is_some(),
        "the fixture registry must load: {:?}",
        ctx.warnings
    );
    (dir, workspace, ctx, guard)
}

// ─────────────────────────────────────────────────────────────────────────────
// The scripted worker
// ─────────────────────────────────────────────────────────────────────────────

fn outcome(stop_reason: &str, claim: Option<&str>) -> WorkerOutcome {
    WorkerOutcome {
        stop_reason: stop_reason.to_owned(),
        claim: claim.map(str::to_owned),
        exit_code: Some(if stop_reason == STOP_END_TURN { 0 } else { 1 }),
        events: 0,
        stdout_bytes: 128,
        output_dir: None,
        failure: None,
    }
}

fn offer(id: &str, kind: &str) -> PermissionOffer {
    PermissionOffer {
        option_id: id.to_owned(),
        name: kind.to_owned(),
        kind: kind.to_owned(),
    }
}

fn ask(action: WorkerAction, options: &[(&str, &str)]) -> PermissionRequest {
    PermissionRequest {
        worker: "fake".to_owned(),
        tool_call_id: "call-1".to_owned(),
        title: "a scripted request".to_owned(),
        action,
        offers: options.iter().map(|(id, kind)| offer(id, kind)).collect(),
    }
}

/// What the scripted runner saw, and what the host answered.
#[derive(Default)]
struct Observed {
    calls: usize,
    prompt: Option<String>,
    working_dir: Option<PathBuf>,
    env_names: Vec<String>,
    /// The verification command named in the prompt the worker was given.
    check_in_prompt: Option<String>,
    answers: Vec<PermissionAnswer>,
}

/// A worker whose whole behaviour is the script below.
///
/// It is a `WorkerRunner`, not a connector: the seam `delegate_with` calls, with the same types a
/// real ACP or CLI session produces. The questions it asks are genuine `PermissionRequest`s, so the
/// answers come from the same host that answers a live agent.
struct ScriptedRunner {
    events: Vec<WorkerEvent>,
    asks: Vec<PermissionRequest>,
    result: Result<WorkerOutcome, String>,
    /// Raise the cancel flag before returning, the way a real cancellation arrives.
    cancel: bool,
    /// Write a file into the workspace, so a changed-file summary has something to find.
    writes: Vec<String>,
    seen: RefCell<Observed>,
}

impl ScriptedRunner {
    fn new(result: Result<WorkerOutcome, String>) -> Self {
        Self {
            events: Vec::new(),
            asks: Vec::new(),
            result,
            cancel: false,
            writes: Vec::new(),
            seen: RefCell::new(Observed::default()),
        }
    }

    fn with_asks(mut self, asks: Vec<PermissionRequest>) -> Self {
        self.asks = asks;
        self
    }

    fn with_events(mut self, events: Vec<WorkerEvent>) -> Self {
        self.events = events;
        self
    }

    fn cancel(mut self) -> Self {
        self.cancel = true;
        self
    }

    fn writing(mut self, files: &[&str]) -> Self {
        self.writes = files.iter().map(|f| (*f).to_owned()).collect();
        self
    }

    fn answers(&self) -> Vec<PermissionAnswer> {
        self.seen.borrow().answers.clone()
    }

    fn observed(&self) -> Observed {
        let borrowed = self.seen.borrow();
        Observed {
            calls: borrowed.calls,
            prompt: borrowed.prompt.clone(),
            working_dir: borrowed.working_dir.clone(),
            env_names: borrowed.env_names.clone(),
            check_in_prompt: borrowed.check_in_prompt.clone(),
            answers: borrowed.answers.clone(),
        }
    }
}

impl WorkerRunner for ScriptedRunner {
    fn run(
        &self,
        spec: &RunSpec<'_>,
        host: &mut dyn PermissionHost,
        sink: &mut dyn EventSink,
    ) -> Result<WorkerOutcome, WorkerError> {
        {
            let mut seen = self.seen.borrow_mut();
            seen.calls += 1;
            seen.prompt = Some(spec.request.prompt.clone());
            seen.working_dir = spec.launch.working_dir.clone();
            seen.env_names = spec.launch.env.keys().cloned().collect();
        }
        for event in &self.events {
            if !sink.emit(event.clone()) {
                // The sink's ceiling was reached. A real connector stops reading for exactly this
                // reason, so the test exercises the same back-pressure.
                break;
            }
        }
        for request in &self.asks {
            let answer = host.decide(request);
            // A real connector records the question and the answer it produced; the host does not
            // write to the sink itself.
            sink.emit(WorkerEvent::PermissionAsked {
                request: request.clone(),
                answer: answer.clone(),
            });
            self.seen.borrow_mut().answers.push(answer);
        }
        if let Some(dir) = &spec.launch.working_dir {
            for name in &self.writes {
                let path = dir.join(name);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&path, "the worker wrote this\n");
            }
        }
        if self.cancel {
            spec.cancel.store(true, Ordering::Relaxed);
        }
        match &self.result {
            Ok(outcome) => Ok(outcome.clone()),
            Err(reason) => Err(WorkerError::Runtime {
                reason: reason.clone(),
            }),
        }
    }
}

fn request(goal: &str, workspace: &Path) -> DelegateRequest {
    DelegateRequest {
        goal: goal.to_owned(),
        workspace: workspace.to_path_buf(),
        worker: Some("fake".to_owned()),
    }
}

fn delegation(ctx: &Context, id: &str) -> DelegationRecord {
    ctx.store
        .delegation(id)
        .expect("the delegation must be readable")
        .expect("the delegation row must exist")
}

// ─────────────────────────────────────────────────────────────────────────────
// The check is decided before the worker exists
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_delegation_without_a_verification_command_is_refused_before_any_worker_runs() {
    let blank = delegation_section(&["/c", "exit", "0"], "verify_command = \"\"\n");
    let (dir, workspace, mut ctx, _guard) = scenario("no-check", &blank);
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, Some("done"))));
    let error = delegate_with(
        &mut ctx,
        &request("make the check pass", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect_err("an uncheckable delegation must be refused");
    assert!(
        error.to_string().contains("verify_command is empty"),
        "the refusal must name the setting: {error}"
    );
    assert_eq!(runner.observed().calls, 0, "no worker may run");
    assert_eq!(count(&ctx, "delegations"), 0, "nothing was recorded");
    assert_eq!(count(&ctx, "tasks"), 0, "not even a task row");
    cleanup(&dir);
}

#[test]
fn the_worker_is_told_the_check_and_never_gets_to_choose_it() {
    let (dir, workspace, mut ctx, _guard) = scenario(
        "prompt-carries-check",
        &delegation_section(&["/c", "exit", "0"], ""),
    );
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, Some("all tests pass"))));
    let report = delegate_with(
        &mut ctx,
        &request("add a note to note.txt", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");

    let observed = runner.observed();
    let prompt = observed.prompt.expect("a prompt");
    // Stated as data, in the same words the row records.
    assert!(prompt.contains("J.E.A.N. will decide"), "{prompt}");
    assert!(prompt.contains("Make that check pass"), "{prompt}");
    assert!(
        prompt.contains("Do not modify the check"),
        "the prompt must forbid editing the test harness: {prompt}"
    );
    let row = delegation(&ctx, &report.delegation_id);
    assert!(
        prompt.contains(&row.verify_command),
        "the prompt must name the command that will actually decide"
    );

    // The worker runs in the workspace the operator named — canonicalized, the same way a search
    // directory is — with the allowlist-pinned environment. A credential that is not on the list
    // cannot reach a delegatee any more than it reaches a registered tool.
    //
    // The canonical form is the plain drive spelling, not the verbatim `\\?\C:\…` that
    // `std::fs::canonicalize` returns: a child started in a verbatim directory is a child that is not
    // in the workspace at all (`cmd.exe` calls it a UNC path and starts in `C:\Windows`).
    let canonical = workspace
        .canonicalize()
        .expect("the workspace must resolve");
    let plain = canonical
        .to_string_lossy()
        .strip_prefix(r"\\?\")
        .map(PathBuf::from)
        .unwrap_or_else(|| canonical.clone());
    assert_eq!(observed.working_dir.as_deref(), Some(plain.as_path()));
    assert!(
        !observed
            .working_dir
            .as_deref()
            .map(|dir| dir.to_string_lossy().starts_with(r"\\?\"))
            .unwrap_or(true),
        "the worker's directory must be a path a child process can be started in"
    );
    assert!(
        observed
            .env_names
            .iter()
            .all(|name| !name.eq_ignore_ascii_case("aws_secret_access_key")),
        "an unlisted variable must not be forwarded: {:?}",
        observed.env_names
    );
    assert!(
        observed
            .env_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case("SystemRoot")),
        "cmd.exe needs its minimum environment: {:?}",
        observed.env_names
    );
    cleanup(&dir);
}

#[test]
fn a_claim_of_success_with_a_failing_check_is_reported_as_failed() {
    // The whole phase in one line: the agent said it worked, the check says it did not.
    let (dir, workspace, mut ctx, _guard) = scenario(
        "claim-vs-check",
        &delegation_section(&["/c", "exit", "1"], ""),
    );
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, Some("all tests pass"))));
    let report = delegate_with(
        &mut ctx,
        &request("fix the failing test", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");

    assert_eq!(report.result, DelegationResult::Failed);
    assert_eq!(report.claim.as_deref(), Some("all tests pass"));
    let row = delegation(&ctx, &report.delegation_id);
    assert_eq!(row.agent_claim.as_deref(), Some("all tests pass"));
    assert_eq!(row.verification_verdict.as_deref(), Some("not_proven"));
    assert_eq!(
        row.result.as_deref(),
        Some("failed"),
        "the verdict, not the claim"
    );
    assert_eq!(row.agent_stop_reason.as_deref(), Some(STOP_END_TURN));
    // Both facts, side by side, in the operator's own summary.
    let lines = report.lines().join("\n");
    assert!(lines.contains("claimed \"all tests pass\""), "{lines}");
    assert!(lines.contains("result: failed"), "{lines}");
    assert!(
        ctx.store
            .task(&report.task_id)
            .expect("the task must read")
            .expect("the task row must exist")
            .state
            == TaskState::Failed
    );
    cleanup(&dir);
}

#[test]
fn a_passing_check_on_a_cleanly_ended_session_is_the_only_way_to_succeed() {
    let (dir, workspace, mut ctx, _guard) =
        scenario("succeeds", &delegation_section(&["/c", "exit", "0"], ""));
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, Some("done"))));
    let report = delegate_with(
        &mut ctx,
        &request("do the thing", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");
    assert_eq!(report.result, DelegationResult::Succeeded);
    assert!(report.baseline.is_proven());
    assert_eq!(
        ctx.store
            .task(&report.task_id)
            .expect("read")
            .expect("row")
            .state,
        TaskState::Succeeded,
        "the state machine must pass through verifying to get here"
    );
    let kinds = kinds(&ctx, &report.task_id);
    assert!(kinds.contains(&EventKind::VerificationRecorded));
    assert!(kinds.contains(&EventKind::ChangesRecorded));
    assert!(!kinds.contains(&EventKind::PermissionRequested));
    cleanup(&dir);
}

#[test]
fn a_check_that_passed_on_a_session_that_did_not_end_cleanly_needs_a_person() {
    let (dir, workspace, mut ctx, _guard) =
        scenario("unclean", &delegation_section(&["/c", "exit", "0"], ""));
    let runner = ScriptedRunner::new(Ok(outcome(STOP_FAILED, None)));
    let report = delegate_with(
        &mut ctx,
        &request("do the thing", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");
    assert_eq!(report.result, DelegationResult::NeedsAttention);
    assert!(
        report.baseline.is_proven() && report.verification.as_ref().expect("check").is_proven(),
        "the check passed; the session is what did not finish"
    );
    assert_eq!(
        delegation(&ctx, &report.delegation_id).result.as_deref(),
        Some("needs_attention")
    );
    cleanup(&dir);
}

#[test]
fn a_check_that_could_not_run_is_not_attributed_to_the_workers_code() {
    // `delegation-verify` on the auto-deny list: the engine refuses the check itself. Nothing was
    // proven, and calling that `failed` would blame the worker for J.E.A.N.'s own configuration.
    let section = format!(
        "{}\n[policy]\nauto_deny = [\"delegation-verify\"]\n",
        delegation_section(&["/c", "exit", "0"], "")
    );
    let (dir, workspace, mut ctx, _guard) = scenario("check-refused", &section);
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, Some("all tests pass"))));
    let report = delegate_with(
        &mut ctx,
        &request("do the thing", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");
    assert_eq!(report.result, DelegationResult::Unverified);
    assert_eq!(report.claim.as_deref(), Some("all tests pass"));
    assert_eq!(
        delegation(&ctx, &report.delegation_id)
            .verification_verdict
            .as_deref(),
        Some("unverifiable"),
        "a refusal is recorded as an absence of evidence"
    );
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// Delegated permission requests, through the authority that already existed
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn an_edit_inside_the_workspace_is_answered_without_a_prompt_and_outside_it_is_refused_without_one()
{
    let (dir, workspace, mut ctx, _guard) =
        scenario("containment", &delegation_section(&["/c", "exit", "0"], ""));
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None))).with_asks(vec![
        ask(
            WorkerAction::EditFile {
                path: "note.txt".to_owned(),
            },
            &[("once", "allow_once"), ("always", "allow_always")],
        ),
        ask(
            WorkerAction::EditFile {
                path: "..\\outside.txt".to_owned(),
            },
            &[("once", "allow_once")],
        ),
        ask(
            WorkerAction::EditFile {
                path: system32("win.ini").display().to_string(),
            },
            &[("once", "allow_once")],
        ),
    ]);
    let mut prompter = ScriptedPrompt::default();
    let report = delegate_with(
        &mut ctx,
        &request("edit two files", &workspace),
        &mut prompter,
        &runner,
    )
    .expect("the delegation must run");

    let answers = runner.answers();
    assert_eq!(answers.len(), 3);
    assert!(
        matches!(&answers[0], PermissionAnswer::Selected { option_id } if option_id == "once"),
        "the covered edit takes the worker's own allow_once: {:?}",
        answers[0]
    );
    for answer in &answers[1..] {
        let PermissionAnswer::Denied { reason } = answer else {
            panic!("an edit outside the workspace must be refused, got {answer:?}");
        };
        assert!(
            reason.contains("outside the workspace this delegation covers"),
            "{reason}"
        );
    }
    // The point of the delegation grant: nobody was woken for either decision.
    assert!(
        prompter.asked.is_empty(),
        "neither a covered edit nor an escaped one may reach the operator: {:?}",
        prompter.asked
    );
    assert_eq!(report.covered_asks, 1);
    assert_eq!(report.prompted_asks, 2);

    // …and both are on the record, with the authority that answered the first one.
    let audit = std::fs::read_to_string(&ctx.paths.audit).expect("the audit log must exist");
    assert!(audit.contains("delegation_scope"), "{audit}");
    assert!(audit.contains("outside_scope"), "{audit}");
    assert!(
        ctx.store
            .events(&report.task_id)
            .expect("events")
            .iter()
            .any(|event| event.kind == EventKind::PermissionRequested),
        "the asks are task events, not only audit lines"
    );
    cleanup(&dir);
}

#[test]
fn a_command_a_worker_asks_for_reaches_the_ordinary_gate_and_never_the_delegation_grant() {
    let section = format!(
        "{}\n[policy]\nrequire_approval = [\"agent-command\"]\n",
        delegation_section(&["/c", "exit", "0"], "")
    );
    let (dir, workspace, mut ctx, _guard) = scenario("command-ask", &section);
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None))).with_asks(vec![ask(
        WorkerAction::RunCommand {
            argv: vec!["cargo".to_owned(), "test".to_owned()],
            cwd: None,
        },
        &[("once", "allow_once"), ("always", "allow_always")],
    )]);
    let mut prompter = ScriptedPrompt {
        answers: vec![Answer::ApproveOnce],
        asked: Vec::new(),
    };
    let report = delegate_with(
        &mut ctx,
        &request("run the tests", &workspace),
        &mut prompter,
        &runner,
    )
    .expect("the delegation must run");

    assert_eq!(report.covered_asks, 0, "a command is never covered");
    assert_eq!(report.prompted_asks, 1);
    assert_eq!(
        prompter.asked.len(),
        1,
        "the operator was asked exactly once, about the command"
    );
    assert!(
        matches!(
            runner.answers().first(),
            Some(PermissionAnswer::Selected { option_id }) if option_id == "once"
        ),
        "an approval must answer allow_once, never allow_always"
    );
    cleanup(&dir);
}

#[test]
fn a_declined_command_is_refused_with_the_operators_own_reason() {
    let section = format!(
        "{}\n[policy]\nrequire_approval = [\"agent-command\"]\n",
        delegation_section(&["/c", "exit", "0"], "")
    );
    let (dir, workspace, mut ctx, _guard) = scenario("declined-command", &section);
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None))).with_asks(vec![ask(
        WorkerAction::RunCommand {
            argv: vec!["del".to_owned(), "C:\\Windows".to_owned()],
            cwd: None,
        },
        &[("once", "allow_once"), ("no", "reject_once")],
    )]);
    let mut prompter = ScriptedPrompt {
        answers: vec![Answer::Denied(DenyReason::Declined)],
        asked: Vec::new(),
    };
    delegate_with(
        &mut ctx,
        &request("delete something", &workspace),
        &mut prompter,
        &runner,
    )
    .expect("a refusal inside the session is a reportable outcome, not an error");
    let answer = runner.answers().remove(0);
    // The host answers in JEAN's vocabulary — the connector's job is to turn that into the worker's
    // own `reject_once`, which `jean-acp` does and its tests assert. Saying "no" in a language the
    // agent cannot parse is how a session hangs.
    let PermissionAnswer::Denied { reason } = &answer else {
        panic!("a declined ask must be a refusal, got {answer:?}");
    };
    assert!(reason.contains("declined"), "{reason}");
    assert!(
        !runner.answers().iter().any(PermissionAnswer::is_yes),
        "and nothing about this ask became an approval"
    );
    cleanup(&dir);
}

#[test]
fn a_worker_that_offers_no_way_to_accept_is_refused_even_when_jean_would_allow() {
    let (dir, workspace, mut ctx, _guard) =
        scenario("no-option", &delegation_section(&["/c", "exit", "0"], ""));
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None))).with_asks(vec![ask(
        WorkerAction::EditFile {
            path: "note.txt".to_owned(),
        },
        &[("cancel", "reject_once")],
    )]);
    delegate_with(
        &mut ctx,
        &request("edit a file the agent will not let be written", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");
    let answer = runner.answers().remove(0);
    let PermissionAnswer::Denied { reason } = &answer else {
        panic!("an agent that offers only \"no\" cannot be answered \"yes\": {answer:?}");
    };
    assert!(reason.contains("offered no way to accept it"), "{reason}");
    cleanup(&dir);
}

#[test]
fn a_read_only_worker_is_not_covered_for_writes() {
    let section = delegation_section(&["/c", "exit", "0"], "")
        .replace("sandbox = \"workspace-write\"", "sandbox = \"read-only\"");
    let (dir, workspace, mut ctx, _guard) = scenario("read-only", &section);
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None))).with_asks(vec![
        ask(
            WorkerAction::EditFile {
                path: "note.txt".to_owned(),
            },
            &[("once", "allow_once")],
        ),
        ask(
            WorkerAction::ReadFile {
                path: "note.txt".to_owned(),
            },
            &[("once", "allow_once")],
        ),
    ]);
    let report = delegate_with(
        &mut ctx,
        &request("read, do not write", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");
    let answers = runner.answers();
    // A read is covered: it cannot change the tree the operator handed over.
    assert!(!answers[0].is_yes(), "a write must not be covered");
    assert!(answers[1].is_yes(), "a read inside the tree is covered");
    assert_eq!(report.covered_asks, 1);
    cleanup(&dir);
}

#[test]
fn an_ask_arriving_after_the_session_has_too_little_time_left_is_cancelled_not_asked() {
    // The scenario's whole session budget is shorter than the time an answer is worth waiting for,
    // which is the case this branch exists for: asking would park the worker on a reply the
    // supervisor is about to cut off.
    let section = delegation_section(
        &["/c", "exit", "0"],
        "session_timeout_secs = 20\nidle_timeout_secs = 5\npermission_timeout_secs = 30\n",
    );
    let (dir, workspace, mut ctx, _guard) = scenario("stale-ask", &section);
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None))).with_asks(vec![ask(
        WorkerAction::EditFile {
            path: "note.txt".to_owned(),
        },
        &[("once", "allow_once")],
    )]);
    let report = delegate_with(
        &mut ctx,
        &request("ask late", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");
    assert!(
        matches!(runner.answers().first(), Some(PermissionAnswer::Cancelled)),
        "an answer nobody has time to use must not be waited on: {:?}",
        runner.answers()
    );
    assert_eq!(report.covered_asks, 0);
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// Progress, bounds, redaction, and the shape of the record
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn progress_is_written_to_the_database_and_the_transcript_and_redacted_on_the_way_in() {
    let (dir, workspace, mut ctx, _guard) =
        scenario("progress", &delegation_section(&["/c", "exit", "0"], ""));
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None))).with_events(vec![
        WorkerEvent::Started {
            worker: "fake".to_owned(),
            kind: "codex-cli".to_owned(),
            pid: 4242,
        },
        WorkerEvent::Progress {
            seq: 1,
            source: "agent_message".to_owned(),
            text: "applying patch with token=AAAABBBBCCCCDDDDEEEE inside".to_owned(),
        },
        WorkerEvent::Settled(outcome(STOP_END_TURN, None)),
    ]);
    let report = delegate_with(
        &mut ctx,
        &request("do the thing", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");

    let transcript =
        std::fs::read_to_string(&report.transcript).expect("the transcript must exist");
    assert!(
        !transcript.contains("AAAABBBBCCCCDDDDEEEE"),
        "worker output is where a token is most likely to appear, and it must not survive"
    );
    assert!(transcript.contains("worker settled"), "{transcript}");
    let rows = ctx
        .store
        .worker_events(&report.delegation_id, 100)
        .expect("worker events must read");
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| !row.detail.contains("AAAA")));
    assert_eq!(report.events, 3);
    // The task board sees the same progress, in the same order.
    let kinds = kinds(&ctx, &report.task_id);
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == EventKind::WorkerProgress)
            .count(),
        1
    );
    cleanup(&dir);
}

#[test]
fn a_worker_that_floods_the_sink_is_stopped_at_the_ceiling_and_the_refusal_is_reported() {
    let section = delegation_section(&["/c", "exit", "0"], "max_event_queue = 3\n");
    let (dir, workspace, mut ctx, _guard) = scenario("flood", &section);
    let flood: Vec<WorkerEvent> = (0..10)
        .map(|i| WorkerEvent::Progress {
            seq: i,
            source: "agent_message".to_owned(),
            text: format!("event {i}"),
        })
        .collect();
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None))).with_events(flood);
    let report = delegate_with(
        &mut ctx,
        &request("announce itself forever", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");
    assert_eq!(report.events, 3, "the ceiling is the ceiling");
    assert_eq!(
        ctx.store
            .worker_events(&report.delegation_id, 100)
            .expect("read")
            .len(),
        3
    );
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("dropped") && note.contains("ceiling of 3")),
        "the dropped events must be said out loud: {:?}",
        report.notes
    );
    cleanup(&dir);
}

#[test]
fn a_connector_that_never_reached_a_worker_is_reported_and_recorded_as_needing_attention() {
    let (dir, workspace, mut ctx, _guard) = scenario(
        "connector-error",
        &delegation_section(&["/c", "exit", "0"], ""),
    );
    let runner = ScriptedRunner::new(Err("the worker exited before answering".to_owned()));
    let error = delegate_with(
        &mut ctx,
        &request("do the thing", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect_err("a session that never started cannot be reported as a result");
    assert!(error.to_string().contains("could not be run"), "{error}");
    let row = only_delegation(&ctx);
    assert_eq!(row.result.as_deref(), Some("needs_attention"));
    assert_eq!(row.agent_stop_reason.as_deref(), Some("connector_error"));
    assert_eq!(
        ctx.store
            .tool_runs(&row.task_id)
            .expect("runs")
            .iter()
            .filter(|run| run.tool_id == "delegation-verification")
            .count(),
        0,
        "the check after the worker never ran, because the delegation ended before it"
    );
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// Cancellation: one act, settled once, never replayed
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_cancelled_delegation_stops_before_the_check_and_settles_exactly_once() {
    let (dir, workspace, mut ctx, _guard) =
        scenario("cancel", &delegation_section(&["/c", "exit", "0"], ""));
    let runner = ScriptedRunner::new(Ok(WorkerOutcome {
        stop_reason: jean_acp::contract::STOP_CANCELLED.to_owned(),
        claim: None,
        exit_code: None,
        events: 1,
        stdout_bytes: 16,
        output_dir: None,
        failure: None,
    }))
    .cancel();
    let report = delegate_with(
        &mut ctx,
        &request("stop halfway", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("a cancellation is a result, not an error");
    assert_eq!(report.result, DelegationResult::Cancelled);
    assert!(
        report.verification.is_none(),
        "a check the operator did not ask to keep running must not start"
    );
    assert_eq!(report.claim, None);
    let row = delegation(&ctx, &report.delegation_id);
    assert_eq!(row.result.as_deref(), Some("cancelled"));
    assert_eq!(
        ctx.store
            .tool_runs(&report.task_id)
            .expect("runs")
            .iter()
            .filter(|run| run.tool_id == "delegation-verification")
            .count(),
        0,
        "no replay of the check after a cancel"
    );
    assert_eq!(
        ctx.store
            .tool_runs(&report.task_id)
            .expect("runs")
            .iter()
            .filter(|run| run.tool_id == "delegation-baseline")
            .count(),
        1,
        "the baseline ran before the worker, as recorded"
    );
    // A second settle cannot rewrite the first answer.
    assert!(
        !ctx.store
            .settle_delegation(&report.delegation_id, "succeeded")
            .expect("settle must not error"),
        "the row is closed"
    );
    assert_eq!(
        delegation(&ctx, &report.delegation_id).result.as_deref(),
        Some("cancelled")
    );
    assert_eq!(
        ctx.store
            .task(&report.task_id)
            .expect("read")
            .expect("row")
            .state,
        TaskState::Cancelled
    );
    cleanup(&dir);
}

#[test]
fn a_delegation_the_operator_declines_never_starts_a_worker_and_says_who_refused_it() {
    let section = format!(
        "{}\n[policy]\nrequire_approval = [\"delegate-work\"]\n",
        delegation_section(&["/c", "exit", "0"], "")
    );
    let (dir, workspace, mut ctx, _guard) = scenario("declined-delegation", &section);
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None)));
    let mut prompter = ScriptedPrompt {
        answers: vec![Answer::Denied(DenyReason::Declined)],
        asked: Vec::new(),
    };
    let error = delegate_with(
        &mut ctx,
        &request("do the thing", &workspace),
        &mut prompter,
        &runner,
    )
    .expect_err("the operator's no is the end of it");
    assert!(error.to_string().contains("refused"), "{error}");
    assert_eq!(
        runner.observed().calls,
        0,
        "no worker, no prompt for its asks"
    );
    assert_eq!(
        prompter.asked.len(),
        1,
        "asked once, about the delegation itself"
    );
    assert_eq!(
        only_delegation(&ctx).result.as_deref(),
        Some("needs_attention")
    );
    let audit = std::fs::read_to_string(&ctx.paths.audit).expect("audit");
    assert!(audit.contains("\"decision\":\"denied\""), "{audit}");
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// A real repository: the operator's changes, isolation, and the file summary
// ─────────────────────────────────────────────────────────────────────────────

/// A data directory whose workspace is a git repository with one commit and one file the operator
/// modified without committing.
///
/// `None` when `git` is not on this machine — the tests below then say so rather than pretending a
/// working tree exists where there is none.
fn git_scenario(
    tag: &str,
    extra: &str,
) -> Option<(PathBuf, PathBuf, Context, MutexGuard<'static, ()>)> {
    let git = which("git")?;
    let dir = temp_dir(tag);
    let workspace = dir.join("repo");
    std::fs::create_dir_all(&workspace).expect("the repository directory must be creatable");
    std::fs::write(
        dir.join("tools.toml"),
        "config_version = 1\n\n[[tool]]\nid = \"echo.tool\"\nkind = \"shell\"\ncommand = \"cmd.exe\"\
         \nargs = [\"/c\", \"echo\", \"ran\"]\nverify = { method = \"exit_code\", expected = 0 }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("jean.toml"),
        format!(
            "[workspace]\nroot = {}\n{}",
            toml_path(&workspace),
            delegation_section(&["/c", "exit", "0"], extra)
        ),
    )
    .expect("the configuration must be writable");

    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("JEAN_DATA_DIR", &dir);
    let ctx = Context::load(None).expect("a valid data directory must load");

    let commit = |args: &[&str]| {
        std::process::Command::new(&git)
            .args(args)
            .current_dir(&workspace)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    };
    std::fs::write(workspace.join("tracked.txt"), "committed\n").unwrap();
    std::fs::write(workspace.join("already-dirty.txt"), "committed once\n").unwrap();
    let seeded = commit(&["init", "-q", "-b", "main", "."])
        && commit(&["config", "user.email", "jean@example.invalid"])
        && commit(&["config", "user.name", "JEAN test"])
        && commit(&["add", "tracked.txt", "already-dirty.txt"])
        && commit(&["commit", "-q", "-m", "seed"]);
    if !seeded {
        drop(guard);
        cleanup(&dir);
        return None;
    }
    // The operator's own uncommitted change, which is the thing a summary must not steal.
    std::fs::write(
        workspace.join("already-dirty.txt"),
        "the operator own change\n",
    )
    .unwrap();
    Some((dir, workspace, ctx, guard))
}

fn skip_without_git(why: &str) {
    eprintln!("skipping: {why}");
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var("PATH")
        .ok()?
        .split(';')
        .map(PathBuf::from)
        .map(|dir| dir.join(format!("{program}.exe")))
        .find(|path| path.exists())
}

/// The single delegation row this scenario's data directory holds.
fn only_delegation(ctx: &Context) -> jean_store::DelegationRecord {
    let tasks = ctx
        .store
        .list_tasks(jean_store::TaskFilter::All, 10)
        .expect("the tasks must read");
    let task = tasks.first().expect("one task row");
    ctx.store
        .delegations_for_task(&task.id)
        .expect("the delegations must read")
        .remove(0)
}

#[test]
fn a_dirty_workspace_is_refused_rather_than_mixed_into_a_delegation_summary() {
    let Some((dir, workspace, mut ctx, _guard)) = git_scenario(
        "dirty-refused",
        "isolate_in_worktree = false\nrequire_clean_baseline = true\n",
    ) else {
        skip_without_git("this scenario needs git on PATH");
        return;
    };
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None)));
    let error = delegate_with(
        &mut ctx,
        &request("touch the repository", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect_err("uncommitted work must not be folded into a delegation");
    assert!(
        error.to_string().contains("already modified"),
        "the refusal must name what it found: {error}"
    );
    assert_eq!(runner.observed().calls, 0, "no worker runs");
    assert_eq!(
        only_delegation(&ctx).result.as_deref(),
        Some("needs_attention"),
        "and the row says a person should look"
    );
    assert!(workspace.join("already-dirty.txt").exists());
    cleanup(&dir);
}

#[test]
fn the_changed_file_summary_excludes_what_the_operator_had_already_modified() {
    let Some((dir, workspace, mut ctx, _guard)) = git_scenario(
        "summary",
        "isolate_in_worktree = false\nrequire_clean_baseline = false\n",
    ) else {
        skip_without_git("this scenario needs git on PATH");
        return;
    };
    let runner =
        ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None))).writing(&["added-by-worker.txt"]);
    let report = delegate_with(
        &mut ctx,
        &request("add one file", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");

    assert_eq!(
        report.changed_files,
        vec!["added-by-worker.txt".to_owned()],
        "the worker's own file, and only it"
    );
    assert!(
        report
            .changed_note
            .as_deref()
            .unwrap_or_default()
            .contains("already modified"),
        "and it must be said what was left out: {:?}",
        report.changed_note
    );
    assert_eq!(
        delegation(&ctx, &report.delegation_id).changed_file_count,
        1
    );
    assert_eq!(report.result, DelegationResult::Succeeded);
    cleanup(&dir);
}

#[test]
fn a_worktree_is_created_at_head_used_by_the_worker_and_left_behind() {
    let Some((dir, workspace, mut ctx, _guard)) =
        git_scenario("worktree", "isolate_in_worktree = true\n")
    else {
        skip_without_git("this scenario needs git on PATH");
        return;
    };
    let runner =
        ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None))).writing(&["in-worktree.txt"]);
    let report = delegate_with(
        &mut ctx,
        &request("change the copy, not my checkout", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");

    let worktree = report.worktree.clone().expect("a worktree was asked for");
    assert_ne!(worktree, workspace);
    assert!(
        worktree.join("tracked.txt").exists(),
        "it is a checkout of HEAD"
    );
    assert!(
        worktree.join("in-worktree.txt").exists(),
        "the worker wrote there"
    );
    assert!(
        !workspace.join("in-worktree.txt").exists(),
        "and nowhere in the operator's tree"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("already-dirty.txt")).expect("the operator file"),
        "the operator own change\n",
        "untouched, exactly as they left it"
    );
    assert_eq!(
        report.changed_files,
        vec!["in-worktree.txt".to_owned()],
        "the worktree started clean, so the summary is the worker's alone"
    );
    let row = delegation(&ctx, &report.delegation_id);
    assert_eq!(
        row.worktree.as_deref(),
        Some(worktree.display().to_string().as_str())
    );
    assert!(row.worktree_note.expect("note").contains("--detach"));
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("left in place")),
        "the build does not tidy an operator's repository: {:?}",
        report.notes
    );
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("are not in the worktree")),
        "nor pretend their uncommitted work went with the worker: {:?}",
        report.notes
    );
    cleanup(&dir);
}

#[test]
fn a_workspace_that_is_not_a_repository_says_so_in_the_summary() {
    let (dir, workspace, mut ctx, _guard) = scenario(
        "no-repository",
        &delegation_section(&["/c", "exit", "0"], ""),
    );
    let runner = ScriptedRunner::new(Ok(outcome(STOP_END_TURN, None))).writing(&["new.txt"]);
    let report = delegate_with(
        &mut ctx,
        &request("add a file outside any repository", &workspace),
        &mut ScriptedPrompt::default(),
        &runner,
    )
    .expect("the delegation must run");
    assert!(report.changed_files.is_empty());
    assert_eq!(
        report.changed_note.as_deref(),
        Some("the workspace is not a git repository, so no file-level summary is possible")
    );
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// Reading the records back
// ─────────────────────────────────────────────────────────────────────────────

fn count(ctx: &Context, table: &'static str) -> i64 {
    ctx.store
        .count(table)
        .expect("the row count must be readable")
}

fn kinds(ctx: &Context, task_id: &str) -> Vec<EventKind> {
    ctx.store
        .events(task_id)
        .expect("the events must be readable")
        .into_iter()
        .map(|event| event.kind)
        .collect()
}
