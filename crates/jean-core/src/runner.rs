//! Running one task: from a tool id and arguments to a recorded, verified outcome.
//!
//! This is the only place in Phase 0 where a task is executed. The sequence is
//! long, but every step is there for a reason that is easy to lose:
//!
//! 1. Reconcile leftovers first, so a previous crash is reported before new work
//!    starts and the two are never confused.
//! 2. Resolve the tool, its executable and its environment **before** creating the
//!    task row. A task that can never run should not exist.
//! 3. Record ownership (pid + creation time) before waiting, so a crash from that
//!    point on leaves something a later run can reason about.
//! 4. Arm cancellation — console handler, then the per-task pipe — before
//!    spawning, so there is no window in which a child exists and cannot be
//!    stopped.
//! 5. Supervise, persist, verify, and close the task on **every** path, including
//!    the error paths. That is what keeps the state machine honest.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use jean_process::{supervise, CancelToken, ChildOutcome, LaunchSpec, TerminalMode};
use jean_protocol::ipc::{new_task_id, pipe_name_for};
use jean_protocol::tool::ToolDef;
use jean_protocol::{Event, EventKind, TaskState, Termination};
use jean_store::{Store, TaskRecord};

use crate::context::Context;
use crate::env::{build_child_env, EnvReport};
use crate::error::{CoreError, Result};
use crate::ipc::{CancelListener, CLIENT_CONNECT_TIMEOUT};
use crate::reconcile::{self, Reconciliation};
use crate::resolve::resolve_for_tool;
use crate::signals;
use crate::verify::{self, Verdict};

/// What the CLI asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    /// A tool id from the registry, not a program name.
    pub tool_id: String,
    /// Arguments appended after the tool's own.
    pub args: Vec<String>,
    /// Free text describing the intent, for the task list. Redacted with the
    /// configured patterns before it is persisted.
    pub goal: Option<String>,
    /// Force or forbid the interactive (pseudo console) path.
    ///
    /// `None` means "do what the tool declares", which is the Phase-0 behavior and the
    /// one every existing caller wants. `Some(true)` is what `jean run --interactive`
    /// passes for a tool registered without the flag — an operator sitting at a terminal
    /// asking for a console is a stronger statement than a registry field. The final
    /// answer still has to survive `terminal.interactive_backend`, which is
    /// configuration, not preference.
    pub interactive: Option<bool>,
    /// Which Phase-1 intent is riding on this run, when one is.
    ///
    /// `jean exec` delegates its whole act to this function, so without the field a
    /// `run_command` row and a `jean run` row are byte-identical in the store, and
    /// `jean status` cannot tell the operator which door the operation came through.
    /// `jean run` passes `None`: it is the Phase-0 door, not an intent.
    pub intent: Option<&'static str>,
}

/// What a finished run reports.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub task_id: String,
    pub tool_id: String,
    pub executable: PathBuf,
    /// The task's final state.
    pub state: TaskState,
    /// What the child process did.
    pub outcome: ChildOutcome,
    /// What the declared verification concluded.
    pub verdict: Verdict,
    /// Anything worth telling the operator that is not already in the outcome.
    pub notes: Vec<String>,
    /// The reconciliation that ran before this task, if it found anything.
    pub reconciled: Reconciliation,
}

impl RunReport {
    /// True only when the child ran to completion *and* the declared verification
    /// proved the result.
    pub fn is_success(&self) -> bool {
        self.state == TaskState::Succeeded && self.verdict.is_proven()
    }

    /// A process exit status for the CLI: 0 only for a proven success.
    ///
    /// The codes are distinct so a script can tell "the command failed" from
    /// "J.E.A.N. could not prove anything", which have different remedies.
    pub fn exit_status(&self) -> i32 {
        match self.state {
            TaskState::Succeeded if self.verdict.is_proven() => 0,
            TaskState::Succeeded => 3,
            TaskState::Cancelled => 130,
            TaskState::NeedsAttention => 4,
            _ => 1,
        }
    }
}

/// Run one task to completion.
///
/// Returns `Ok` whenever the task reached a recorded final state, including a
/// failed or cancelled one: the CLI needs the record either way. `Err` means the
/// task could not be run at all, or could not be closed out.
pub fn run(ctx: &Context, request: RunRequest) -> Result<RunReport> {
    let reconciled = reconcile::reconcile(&ctx.store)?;
    if let Some(summary) = reconciled.summary() {
        tracing::warn!("{summary}");
    }

    let tool = lookup_tool(ctx, &request.tool_id)?;
    let plan = plan_invocation(ctx, tool, &request)?;

    let task_id = new_task_id();
    // `--goal` is free text typed by the caller and it lands in the task record
    // verbatim, so it gets the same treatment as the argument vector: a secret
    // inside a goal must not survive into the database unredacted.
    let goal = request
        .goal
        .as_deref()
        .map(|goal| ctx.redactor.redact(goal))
        .unwrap_or_else(|| plan.goal.clone());

    let record = TaskRecord::new(
        task_id.clone(),
        goal,
        Some(tool.id.clone()),
        plan.redacted_request_json.clone(),
    );
    ctx.store.insert_task(&record)?;

    // From here the task exists, so every path must close it.
    match execute(ctx, tool, &plan, &task_id, &reconciled) {
        Ok(report) => Ok(report),
        Err(e) => {
            close_as_failed(ctx, &task_id, &e.to_string());
            Err(e)
        }
    }
}

/// Everything decided before the child is created.
///
/// No task id and no artifact paths: those belong to the task, and keeping them out
/// is what makes planning testable on its own.
struct InvocationPlan {
    executable: PathBuf,
    args: Vec<String>,
    /// Redacted, for persistence and logging.
    redacted_args: Vec<String>,
    environment: BTreeMap<String, String>,
    /// Redacted, for persistence.
    redacted_environment: BTreeMap<String, String>,
    working_dir: Option<PathBuf>,
    timeout: Duration,
    output_cap: u64,
    /// How bytes travel between the child and J.E.A.N.: two pipes, or one merged
    /// pseudo console. Decided by the tool's declaration, the caller's request and
    /// `terminal.interactive_backend`, in that order of authority.
    terminal: TerminalMode,
    goal: String,
    redacted_request_json: String,
    env_notes: Vec<String>,
}

/// Choose the transport for a run, and say out loud when a request was not honoured.
///
/// A tool that declares itself interactive, or a caller who passes `--interactive`,
/// still ends up on pipes when `terminal.interactive_backend = "plain"` — that setting
/// exists for machines where a pseudo console is unavailable. Silently running with
/// stdin closed would let the operator believe the session was interactive, so the
/// degradation comes back as a note that reaches the task record and the CLI output.
fn terminal_mode(
    ctx: &Context,
    tool: &ToolDef,
    request: &RunRequest,
) -> (TerminalMode, Option<String>) {
    let wanted = request.interactive.unwrap_or(tool.interactive);
    if !wanted {
        return (TerminalMode::Pipes, None);
    }
    match ctx.config.terminal.interactive_backend.as_str() {
        "conpty" => (
            TerminalMode::Pty {
                rows: ctx.config.terminal.pty_rows,
                cols: ctx.config.terminal.pty_cols,
            },
            None,
        ),
        other => (
            TerminalMode::Pipes,
            Some(format!(
                "this run was asked to be interactive but `terminal.interactive_backend` is \
                 {other:?}, so the child got two pipes with no console; its stdin is closed \
                 and anything that waits for keystrokes will wait until its timeout"
            )),
        ),
    }
}

/// Resolve the tool into a concrete, validated invocation.
///
/// Nothing is written and no process is created here, so a failure at this stage
/// leaves no task row behind.
fn plan_invocation(ctx: &Context, tool: &ToolDef, request: &RunRequest) -> Result<InvocationPlan> {
    // Order matters: the tool's own arguments first, then its configured extras,
    // then whatever the caller typed. A caller argument therefore cannot be
    // shadowed by the registry, and a registry argument cannot be displaced by the
    // caller.
    let mut args: Vec<String> = Vec::new();
    args.extend(tool.args.iter().cloned());
    args.extend(tool.extra_args.iter().cloned());
    args.extend(request.args.iter().cloned());

    let parent_env: BTreeMap<String, String> = std::env::vars().collect();
    let (environment, env_report) =
        build_child_env(&ctx.config.process.env_allowlist, &tool.env, &parent_env);

    // Resolve against the environment the child will actually have, falling back
    // to the supervisor's only for the two lookup variables. Resolving against a
    // PATH the child will not see would produce a path that means something
    // different by the time it runs.
    let executable = resolve_for_tool(
        &tool.id,
        &tool.command,
        environment
            .get("PATH")
            .map(String::as_str)
            .or_else(|| parent_env.get("PATH").map(String::as_str)),
        environment
            .get("PATHEXT")
            .map(String::as_str)
            .or_else(|| parent_env.get("PATHEXT").map(String::as_str)),
    )?;

    let working_dir = if tool.working_dir.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(tool.working_dir.trim()))
    };

    let timeout = Duration::from_secs(
        tool.timeout_secs
            .unwrap_or(ctx.config.orchestrator.default_timeout_secs),
    );

    let redacted_args = ctx.redactor.redact_args(&args);
    let redacted_environment = ctx.redactor.redact_env(&environment);

    let (terminal, transport_note) = terminal_mode(ctx, tool, request);
    let mut env_notes = env_notes(&env_report);
    if let Some(note) = transport_note {
        env_notes.push(note);
    }

    let request_json = serde_json::to_string(&serde_json::json!({
        "tool": tool.id,
        // Present only for a Phase-1 intent riding on this runner, so `jean status`
        // can name the door the operation came through.
        "intent": request.intent,
        "command": tool.command,
        "args": redacted_args,
        "working_dir": working_dir.as_ref().map(|d| d.display().to_string()),
        "timeout_secs": timeout.as_secs(),
        // The transport, recorded as the thing that actually ran rather than as what
        // the registry or the caller asked for: a run whose console was refused by
        // configuration is distinguishable from one that was never asked for.
        "terminal": if terminal.is_pty() { "conpty" } else { "pipes" },
        "env": redacted_environment,
    }))
    .map_err(|e| CoreError::File {
        path: PathBuf::from("<request record>"),
        reason: e.to_string(),
    })?;

    let goal = format!("{} {}", tool.display_name, redacted_args.join(" "));

    Ok(InvocationPlan {
        executable,
        args,
        redacted_args,
        environment,
        redacted_environment,
        working_dir,
        timeout,
        output_cap: ctx.output_cap(),
        terminal,
        goal: goal.trim().to_owned(),
        redacted_request_json: request_json,
        env_notes,
    })
}

fn env_notes(report: &EnvReport) -> Vec<String> {
    let mut notes = Vec::new();
    if !report.rejected.is_empty() {
        notes.push(format!(
            "the tool asked for environment variables that are not on the allowlist and were \
             dropped: {}",
            report.rejected.join(", ")
        ));
    }
    if !report.absent.is_empty() {
        notes.push(format!(
            "allowlisted variables absent from this machine, so the child will not have them: {}",
            report.absent.join(", ")
        ));
    }
    notes
}

/// Spawn, supervise, persist and verify. The task row already exists.
fn execute(
    ctx: &Context,
    tool: &ToolDef,
    plan: &InvocationPlan,
    task_id: &str,
    reconciled: &Reconciliation,
) -> Result<RunReport> {
    let store = &ctx.store;

    let artifacts = ctx.paths.artifacts_for(task_id);
    std::fs::create_dir_all(&artifacts).map_err(|e| CoreError::Directory {
        path: artifacts.clone(),
        reason: e.to_string(),
    })?;

    // A pseudo console has one merged stream, and `LaunchSpec::validate` refuses a
    // stderr artifact alongside it: an empty file somebody asked for is worse than no
    // file, because the reader cannot tell "nothing went to stderr" from "there was no
    // stderr to write". The note the supervisor adds says which of the two it was.
    let console = plan.terminal.is_pty();
    let spec = LaunchSpec {
        executable: plan.executable.clone(),
        args: plan.args.clone(),
        working_dir: plan.working_dir.clone(),
        env: plan.environment.clone(),
        timeout: Some(plan.timeout),
        output_cap_bytes: plan.output_cap,
        stdout_path: Some(artifacts.join(if console { "console.log" } else { "stdout.log" })),
        stderr_path: if console {
            None
        } else {
            Some(artifacts.join("stderr.log"))
        },
        terminal: plan.terminal,
    };

    // Arm cancellation before the child exists. If arming fails the run is
    // refused: starting a child that cannot be stopped would break the one
    // guarantee the supervisor exists to provide.
    let token = CancelToken::new()?;
    signals::arm(token.clone())?;

    let pipe_name = pipe_name_for(&ctx.config.ipc.pipe_prefix, task_id)?;
    let listener = match CancelListener::start(
        pipe_name,
        task_id.to_owned(),
        token.clone(),
        ctx.paths.database.clone(),
    ) {
        Ok(listener) => listener,
        Err(e) => {
            // A task with no cancellation channel can only be stopped by killing
            // the CLI. That is not acceptable, so the run is refused rather than
            // quietly degraded.
            signals::disarm();
            return Err(e);
        }
    };

    store.transition(task_id, TaskState::Running, Some("the child was launched"))?;
    record_ownership(store, task_id)?;
    (ctx.task_started)(task_id);

    for note in &plan.env_notes {
        store.append_event(task_id, &Event::note(note.clone()))?;
    }

    let run_id = store.insert_tool_run(
        task_id,
        &tool.id,
        &plan.executable.display().to_string(),
        &to_json_array(&plan.redacted_args),
        plan.working_dir
            .as_ref()
            .map(|d| d.display().to_string())
            .as_deref(),
        &to_json_map(&plan.redacted_environment),
    )?;

    let executable_label = plan.executable.display().to_string();
    let outcome = supervise(&spec, &token, |child_pid| {
        // Recorded before any waiting, so a crash from here on names the process.
        if let Err(e) = store.record_child_pid(run_id, child_pid) {
            tracing::error!(error = %e, "could not record the child pid");
            return;
        }
        if let Err(e) = store.append_event(task_id, &Event::spawn(child_pid, &executable_label)) {
            tracing::error!(error = %e, "could not record the spawn event");
        }
    })?;

    listener.stop();
    signals::disarm();

    persist_outcome(store, task_id, run_id, plan.output_cap, &outcome)?;

    store.transition(task_id, TaskState::Verifying, Some("the child stopped"))?;
    let verdict = verify::verify(&tool.verify, &outcome);

    let final_state = decide_state(
        ctx.config.orchestrator.verification_required,
        &outcome,
        &verdict,
    );
    let reason = format!("{}: {}", verdict.label(), verdict.detail());
    store.transition(task_id, final_state, Some(&reason))?;
    store.set_outcome(task_id, verdict.label(), verdict.detail())?;
    store.clear_runner(task_id)?;

    let mut notes = plan.env_notes.clone();
    if let Some(detail) = &outcome.error {
        notes.push(detail.clone());
    }
    if let Some(interrupt) = signals::interrupt() {
        notes.push(format!("interrupted by {}", interrupt.as_str()));
    }
    notes.push(match &verdict {
        Verdict::Proven(_) => "the declared verification passed".to_owned(),
        Verdict::NotProven(_) => "the declared verification did not pass".to_owned(),
        Verdict::Unverifiable(_) => {
            "the command completed, but nothing proves the broader goal was met".to_owned()
        }
    });

    Ok(RunReport {
        task_id: task_id.to_owned(),
        tool_id: tool.id.clone(),
        executable: plan.executable.clone(),
        state: final_state,
        outcome,
        verdict,
        notes,
        reconciled: reconciled.clone(),
    })
}

/// Record which process owns this task, and prove the record is usable.
///
/// If the creation time cannot be read, ownership is **not** recorded at all.
/// Writing a pid with a creation time of 0 would make a later reconciliation
/// compare against 0, find a mismatch, and declare a live runner dead — which is
/// the exact failure the creation time exists to prevent. A task with no recorded
/// owner is reconciled as "never owned", which is true and safe.
pub(crate) fn record_ownership(store: &Store, task_id: &str) -> Result<()> {
    let (pid, created) = jean_process::win32::current_process_identity();
    match created {
        Some(created) => {
            store.set_runner(task_id, pid, created)?;
            Ok(())
        }
        None => {
            let reason = "this process's creation time could not be read, so ownership was not \
                          recorded; if this run is interrupted the task will be reported as never \
                          owned rather than misattributed to another process"
                .to_owned();
            tracing::warn!(task_id, "{reason}");
            store.append_event(task_id, &Event::note(reason))?;
            Ok(())
        }
    }
}

/// Map an outcome and a verdict onto a final task state.
///
/// The mapping is the policy, so it is written out explicitly rather than derived:
///
/// | Termination | Verdict | State |
/// |---|---|---|
/// | Cancelled | any | `cancelled` |
/// | Timeout / LaunchFailed / JobAssignmentFailed | any | `failed` |
/// | Orphaned | any | `needs_attention` |
/// | Exited | Proven | `succeeded` |
/// | Exited | NotProven | `failed` |
/// | Exited | Unverifiable | `succeeded`, or `needs_attention` when verification is required |
///
/// The last row is the one that matters most. A command that ran and returned 0 is
/// not the same as a goal that was achieved, and `orchestrator.verification_required`
/// is how the operator says which of those a task must reach before it may be
/// called done.
pub fn decide_state(
    verification_required: bool,
    outcome: &ChildOutcome,
    verdict: &Verdict,
) -> TaskState {
    match outcome.termination {
        Termination::Cancelled => TaskState::Cancelled,
        Termination::Timeout | Termination::LaunchFailed | Termination::JobAssignmentFailed => {
            TaskState::Failed
        }
        Termination::Orphaned => TaskState::NeedsAttention,
        Termination::Exited => match verdict {
            Verdict::Proven(_) => TaskState::Succeeded,
            Verdict::NotProven(_) => TaskState::Failed,
            Verdict::Unverifiable(_) => {
                if verification_required {
                    TaskState::NeedsAttention
                } else {
                    TaskState::Succeeded
                }
            }
        },
    }
}

/// Write the run's outcome and the events that explain it.
fn persist_outcome(
    store: &Store,
    task_id: &str,
    run_id: i64,
    output_cap: u64,
    outcome: &ChildOutcome,
) -> Result<()> {
    store.finish_tool_run(
        run_id,
        outcome.exit_code,
        outcome.termination,
        &outcome.stdout,
        &outcome.stderr,
        outcome.error.as_deref(),
    )?;

    match outcome.exit_code {
        Some(code) => store.append_event(task_id, &Event::exit(code))?,
        None => store.append_event(
            task_id,
            &Event::new(EventKind::ProcessExited, "no exit code was recorded"),
        )?,
    }

    if outcome.stdout.truncated {
        store.append_event(
            task_id,
            &Event::truncated("stdout", output_cap, outcome.stdout.bytes),
        )?;
    }
    if outcome.stderr.truncated {
        store.append_event(
            task_id,
            &Event::truncated("stderr", output_cap, outcome.stderr.bytes),
        )?;
    }
    Ok(())
}

/// Close a task that could not be run, on the error path.
///
/// Best effort: the original error is what the caller needs, so a failure here is
/// logged rather than replacing it. The state machine is still respected, because a
/// row left in `running` would be indistinguishable from a crash.
fn close_as_failed(ctx: &Context, task_id: &str, reason: &str) {
    let store = &ctx.store;
    let result = (|| -> Result<()> {
        store.append_event(
            task_id,
            &Event::new(
                EventKind::SupervisorError,
                format!("the run could not be completed: {reason}"),
            ),
        )?;
        let current = store.require_task(task_id)?;
        if current.state != TaskState::Failed {
            if current.state.can_transition_to(TaskState::Failed) {
                store.transition(task_id, TaskState::Failed, Some(reason))?;
            } else {
                // A task already in a terminal state cannot move, and forcing it
                // would rewrite history. `needs_attention` is only reachable from
                // an unfinished state, so if neither applies the row is left as it
                // is and the event above is the record.
                if current.state.can_transition_to(TaskState::NeedsAttention) {
                    store.transition(
                        task_id,
                        TaskState::NeedsAttention,
                        Some(&format!(
                            "the run failed ({reason}) and `{}` cannot move to failed directly",
                            current.state
                        )),
                    )?;
                } else {
                    tracing::warn!(
                        task_id,
                        state = %current.state,
                        "the task is already in a state that cannot record this failure"
                    );
                }
            }
        }
        store.set_outcome(task_id, "not_proven", reason)?;
        store.clear_runner(task_id)?;
        Ok(())
    })();

    if let Err(e) = result {
        tracing::error!(task_id, error = %e, "could not close out a failed task");
    }
}

/// Look a tool up, distinguishing "unknown" from "disabled".
pub(crate) fn lookup_tool<'a>(ctx: &'a Context, tool_id: &str) -> Result<&'a ToolDef> {
    if let Some(tool) = ctx.tools.get(tool_id) {
        if !tool.enabled {
            return Err(CoreError::unknown_tool(tool_id, known_ids(ctx), true));
        }
        return Ok(tool);
    }
    Err(CoreError::unknown_tool(tool_id, known_ids(ctx), false))
}

fn known_ids(ctx: &Context) -> Vec<&str> {
    ctx.tools.ids().collect()
}

/// Serialize for the persisted record, falling back to a literal rather than
/// failing the run.
///
/// These values are plain strings and maps, so serialization cannot realistically
/// fail; if it somehow does, losing the argument list is better than losing the
/// task.
pub(crate) fn to_json_array(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_owned())
}

pub(crate) fn to_json_map(values: &BTreeMap<String, String>) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "{}".to_owned())
}

/// How long a cancel client waits. Re-exported so the CLI has one place to change.
pub fn cancel_timeout() -> Duration {
    CLIENT_CONNECT_TIMEOUT
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(termination: Termination, exit_code: Option<i32>) -> ChildOutcome {
        ChildOutcome {
            pid: 1,
            exit_code,
            termination,
            stdout: jean_protocol::StreamSummary::default(),
            stderr: jean_protocol::StreamSummary::default(),
            duration: Duration::from_millis(1),
            error: None,
        }
    }

    fn report(state: TaskState, verdict: Verdict) -> RunReport {
        RunReport {
            task_id: "t-1".to_owned(),
            tool_id: "echo".to_owned(),
            executable: PathBuf::from("powershell.exe"),
            state,
            outcome: outcome(Termination::Exited, Some(0)),
            verdict,
            notes: vec![],
            reconciled: Reconciliation::default(),
        }
    }

    #[test]
    fn a_proven_success_exits_zero() {
        let r = report(TaskState::Succeeded, Verdict::Proven("ok".to_owned()));
        assert!(r.is_success());
        assert_eq!(r.exit_status(), 0);
    }

    #[test]
    fn an_unverified_completion_does_not_exit_zero() {
        // The distinction the whole verification step exists for: the command ran,
        // and that is not the same as the goal being met.
        let r = report(
            TaskState::Succeeded,
            Verdict::Unverifiable("no verification declared".to_owned()),
        );
        assert!(!r.is_success());
        assert_eq!(r.exit_status(), 3);
    }

    #[test]
    fn cancelled_failed_and_needs_attention_have_distinct_status_codes() {
        assert_eq!(
            report(TaskState::Failed, Verdict::NotProven("bad".to_owned())).exit_status(),
            1
        );
        assert_eq!(
            report(
                TaskState::Cancelled,
                Verdict::NotProven("killed".to_owned())
            )
            .exit_status(),
            130
        );
        assert_eq!(
            report(
                TaskState::NeedsAttention,
                Verdict::Unverifiable("unknown".to_owned())
            )
            .exit_status(),
            4
        );
    }

    #[test]
    fn the_state_decision_follows_the_documented_table() {
        let cases: Vec<(Termination, Option<i32>, Verdict, TaskState)> = vec![
            (
                Termination::Cancelled,
                Some(130),
                Verdict::NotProven("killed".into()),
                TaskState::Cancelled,
            ),
            (
                Termination::Timeout,
                Some(124),
                Verdict::NotProven("timed out".into()),
                TaskState::Failed,
            ),
            (
                Termination::LaunchFailed,
                None,
                Verdict::NotProven("never started".into()),
                TaskState::Failed,
            ),
            (
                Termination::JobAssignmentFailed,
                Some(130),
                Verdict::NotProven("not supervised".into()),
                TaskState::Failed,
            ),
            (
                Termination::Orphaned,
                None,
                Verdict::Unverifiable("unknown".into()),
                TaskState::NeedsAttention,
            ),
            (
                Termination::Exited,
                Some(0),
                Verdict::Proven("ok".into()),
                TaskState::Succeeded,
            ),
            (
                Termination::Exited,
                Some(1),
                Verdict::NotProven("bad".into()),
                TaskState::Failed,
            ),
        ];

        for (termination, exit_code, verdict, expected) in cases {
            let o = outcome(termination, exit_code);
            // verification_required must not change any row above, so both values
            // are checked for each.
            for required in [true, false] {
                assert_eq!(
                    decide_state(required, &o, &verdict),
                    expected,
                    "{termination:?} + {} with verification_required={required} should give {expected}",
                    verdict.label()
                );
            }
        }
    }

    #[test]
    fn verification_required_decides_the_unverifiable_case() {
        let o = outcome(Termination::Exited, Some(0));
        let verdict = Verdict::Unverifiable("nothing declared".into());

        assert_eq!(
            decide_state(true, &o, &verdict),
            TaskState::NeedsAttention,
            "with verification required, an unproven completion must not be called success"
        );
        assert_eq!(
            decide_state(false, &o, &verdict),
            TaskState::Succeeded,
            "with verification off, completion alone is accepted"
        );
    }

    #[test]
    fn env_notes_report_rejected_and_absent_variables_without_values() {
        let report = EnvReport {
            inherited: vec!["PATH".to_owned()],
            from_tool: vec![],
            rejected: vec!["AWS_SECRET_ACCESS_KEY".to_owned()],
            absent: vec!["JAVA_HOME".to_owned()],
        };
        let notes = env_notes(&report);
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("AWS_SECRET_ACCESS_KEY"), "{notes:?}");
        assert!(notes[0].contains("not on the allowlist"), "{notes:?}");
        assert!(notes[1].contains("JAVA_HOME"), "{notes:?}");
    }

    #[test]
    fn a_clean_environment_produces_no_notes() {
        let report = EnvReport {
            inherited: vec!["PATH".to_owned()],
            from_tool: vec![],
            rejected: vec![],
            absent: vec![],
        };
        assert!(env_notes(&report).is_empty());
        assert!(report.is_empty());
    }

    #[test]
    fn the_default_configuration_requires_verification() {
        let config = jean_protocol::AppConfig::default();
        assert!(
            config.orchestrator.verification_required,
            "the default must not silently accept an unverified completion"
        );
        assert!(config.orchestrator.reconcile_on_start);
        assert!(!config.process.inherit_environment);
    }

    #[test]
    fn json_fallbacks_are_valid_json() {
        assert_eq!(
            to_json_array(&["a".to_owned(), "b".to_owned()]),
            r#"["a","b"]"#
        );
        assert_eq!(to_json_array(&[]), "[]");
        let mut map = BTreeMap::new();
        map.insert("K".to_owned(), "V".to_owned());
        assert_eq!(to_json_map(&map), r#"{"K":"V"}"#);
        assert_eq!(to_json_map(&BTreeMap::new()), "{}");
    }

    #[test]
    fn a_real_task_id_is_distinguishable_from_a_placeholder() {
        // Task ids end up in pipe names and directory names, so their shape is part
        // of the contract.
        let id = new_task_id();
        assert!(id.starts_with("t-"), "{id}");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{id} must be safe in a path and a pipe name"
        );
        assert_ne!(new_task_id(), new_task_id(), "ids must not repeat");
    }
}
