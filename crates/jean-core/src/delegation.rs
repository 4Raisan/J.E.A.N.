//! # Delegation — an external worker does the work, J.E.A.N. decides and checks
//!
//! This is the Phase-3 objective workflow in one function: take the operator's goal, hand it to a
//! coding agent, watch what it does, look at what it changed, and then run the check *J.E.A.N.*
//! chose — not the one the agent proposed — and report what was observed.
//!
//! ## The two things this module refuses
//!
//! * **It does not take orders from the worker.** The verification command is resolved from
//!   `[delegation] verify_command` before the agent is started, through the same `resolve` path a
//!   typed command uses, and the argv the agent was given is never consulted again. A worker that
//!   finishes by saying "all tests pass" has made a *claim*; the row records the claim next to the
//!   verdict, and only the verdict decides the result.
//! * **It does not hold a second permission system.** An agent's `session/request_permission` arrives
//!   as a [`PermissionRequest`], is turned into an [`Operation`], and is decided by
//!   [`jean_policy::Policy`] — `auto_deny` first, elevation second, then the ledger, then the
//!   operator. The one shortcut is [`Authority::DelegationScope`]: an edit *inside the workspace the
//!   operator pointed at*, under *the capability the operator gave that worker*, is covered by the
//!   delegation they just approved and does not ask again. Everything else asks. An agent cannot
//!   widen either branch: a path outside the workspace is refused before the engine is consulted, and
//!   a command or a network target always goes through the ordinary gate.
//!
//! ## What is recorded, and where
//!
//! The `delegations` row (schema v4) holds the goal, the worker and its protocol, the workspace and
//! any worktree, the resolved verification command, the baseline verdict, the agent's stop reason and
//! claim, the changed-file summary, the verification verdict, and the result — each in its own column,
//! because the distinction between what the agent said and what J.E.A.N. observed is the whole point
//! and must survive being read out of the database a month later. Progress goes to `worker_events`
//! and to a redacted transcript under the task's artifact directory.
//!
//! ## Sequential on purpose
//!
//! `[delegation] max_parallel` is validated to `1`, because the cancellation design has exactly one
//! active task per process ([`crate::ipc`], and D-028's single signal handler). Two concurrent
//! delegations would need a cancellation router that does not exist yet, so this build runs one worker
//! at a time and says so rather than claiming otherwise.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jean_acp::contract::{
    Bounds, DelegateRequest as WorkerRequest, EventSink, PermissionAnswer, PermissionHost,
    PermissionRequest, RunSpec, WorkerAction, WorkerEvent, WorkerFailureKind, WorkerOutcome,
    STOP_CANCELLED,
};
use jean_policy::{
    AuditEvent, AuditWriter, Authority, Decision, DenyReason, Operation, OperationKind, Policy,
    Prompter, Scope,
};
use jean_process::{CancelToken, LaunchSpec, TerminalMode, Termination};
use jean_protocol::config::{DelegationConfig, WorkerConfig};
use jean_protocol::event::{Event, EventKind};
use jean_protocol::ipc::new_task_id;
use jean_protocol::tool::PolicyTier;
use jean_protocol::TaskState;
use jean_store::{NewDelegation, Store, TaskRecord};

use crate::context::Context;
use crate::env::build_child_env;
use crate::error::{CoreError, Result};
use crate::ipc::CancelListener;
use crate::verify::Verdict;

/// What the operator asked to have done, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateRequest {
    /// The operator's own words. Stored verbatim (redacted, not rewritten).
    pub goal: String,
    /// The directory the worker is pointed at. Resolved and required to exist.
    pub workspace: PathBuf,
    /// A worker by name, or `None` for `[delegation] default_worker`.
    pub worker: Option<String>,
}

/// How the delegation ended, as the `delegations.result` column checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationResult {
    /// The independent verification passed after the worker finished.
    Succeeded,
    /// The verification ran and did not pass, whatever the worker claimed.
    Failed,
    /// The operator, or `jean cancel`, ended the session.
    Cancelled,
    /// Something needs a person: a stalled worker, a refusal, a claim with no check behind it.
    NeedsAttention,
    /// Nothing was checked. Never reported as a success.
    Unverified,
}

impl DelegationResult {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::NeedsAttention => "needs_attention",
            Self::Unverified => "unverified",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "needs_attention" => Some(Self::NeedsAttention),
            "unverified" => Some(Self::Unverified),
            _ => None,
        }
    }

    /// The process exit status `jean delegate` reports.
    ///
    /// The same table the Phase-0 runner uses, because the meanings are the same and an operator who
    /// scripts one command should not have to learn a second dialect: 0 proven, 1 did not pass, 3
    /// nothing proved it, 4 a person should look, 130 cancelled.
    pub const fn exit_status(self) -> i32 {
        match self {
            Self::Succeeded => 0,
            Self::Failed => 1,
            Self::Unverified => 3,
            Self::NeedsAttention => 4,
            Self::Cancelled => 130,
        }
    }
}

/// What one delegation produced. Every field is an observation, except `claim`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationReport {
    pub task_id: String,
    pub delegation_id: String,
    pub worker: String,
    pub kind: String,
    pub workspace: PathBuf,
    /// The isolated worktree the worker actually ran in, when one was created.
    pub worktree: Option<PathBuf>,
    /// Why this workspace, or that worktree — including "isolation was asked for and refused".
    pub isolation: String,
    /// The verification's result *before* the worker ran. Always present, because a delegation
    /// without a check to run is refused before a worker is started.
    pub baseline: Verdict,
    pub stop_reason: String,
    /// The worker's own account of its result. Recorded, never believed.
    pub claim: Option<String>,
    /// Files the worker changed, relative to what was already dirty when it started.
    pub changed_files: Vec<String>,
    /// Anything that qualifies the summary above: "not a git repository", "truncated at N".
    pub changed_note: Option<String>,
    /// The verification's result *after* the worker ran.
    pub verification: Option<Verdict>,
    pub result: DelegationResult,
    pub events: usize,
    /// Asks answered from the delegation's own authority — no prompt, by design.
    pub covered_asks: usize,
    /// Asks that reached the engine, and so reached the operator.
    pub prompted_asks: usize,
    pub transcript: PathBuf,
    pub notes: Vec<String>,
}

#[derive(Debug)]
pub enum ResumeOutcome {
    Completed(DelegationReport),
    NeedsAttention {
        task_id: String,
        reason: String,
    },
    Blocked {
        task_id: String,
        status: String,
        reason: String,
    },
}

impl DelegationReport {
    /// The operator-facing summary, one line per fact.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "delegation {} → worker {} ({}) in {}",
            self.delegation_id,
            self.worker,
            self.kind,
            self.workspace.display()
        )];
        out.push(format!("task: {}", self.task_id));
        out.push(format!("isolation: {}", self.isolation));
        out.push(format!("baseline check: {}", verdict_text(&self.baseline)));
        out.push(format!(
            "worker: stop_reason={}{}",
            self.stop_reason,
            self.claim
                .as_ref()
                .map(|c| format!(", claimed {c:?}"))
                .unwrap_or_default()
        ));
        out.push(format!(
            "changed files: {}{}",
            self.changed_files.len(),
            self.changed_note
                .as_ref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default()
        ));
        out.push(format!(
            "verification: {}",
            self.verification
                .as_ref()
                .map(verdict_text)
                .unwrap_or_else(|| "not configured".to_owned())
        ));
        out.push(format!(
            "result: {} — {} covered, {} asked, {} event(s)",
            self.result.as_str(),
            self.covered_asks,
            self.prompted_asks,
            self.events
        ));
        for note in &self.notes {
            out.push(format!("note: {note}"));
        }
        out.push(format!("transcript: {}", self.transcript.display()));
        out
    }
}

fn verdict_text(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Proven(detail) => format!("passed ({detail})"),
        Verdict::NotProven(detail) => format!("did not pass ({detail})"),
        Verdict::Unverifiable(detail) => format!("unverifiable ({detail})"),
    }
}

/// The worker call, behind a seam so the whole flow is testable without an agent.
///
/// `jean-acp` is the production implementation. A test hands `delegate_with` a scripted runner and
/// still exercises the policy host, the store writes, the containment rules and the result rule,
/// which is what the tests are actually about.
pub trait WorkerRunner {
    fn run(
        &self,
        spec: &RunSpec<'_>,
        host: &mut dyn PermissionHost,
        sink: &mut dyn EventSink,
    ) -> std::result::Result<WorkerOutcome, jean_acp::WorkerError>;
}

/// The real thing: ACP or the structured CLI fallback, chosen by the worker's configured kind.
pub struct AcpRunner;

impl WorkerRunner for AcpRunner {
    fn run(
        &self,
        spec: &RunSpec<'_>,
        host: &mut dyn PermissionHost,
        sink: &mut dyn EventSink,
    ) -> std::result::Result<WorkerOutcome, jean_acp::WorkerError> {
        jean_acp::run(spec, host, sink)
    }
}

/// Delegate one goal to one worker, and report what was observed.
pub fn delegate(
    ctx: &mut Context,
    request: &DelegateRequest,
    prompter: &mut dyn Prompter,
) -> Result<DelegationReport> {
    delegate_with(ctx, request, prompter, &AcpRunner)
}

/// The same, with the worker call supplied.
pub fn delegate_with(
    ctx: &mut Context,
    request: &DelegateRequest,
    prompter: &mut dyn Prompter,
    runner: &dyn WorkerRunner,
) -> Result<DelegationReport> {
    delegate_with_mode(ctx, request, prompter, runner, false, true)
}

fn delegate_with_mode(
    ctx: &mut Context,
    request: &DelegateRequest,
    prompter: &mut dyn Prompter,
    runner: &dyn WorkerRunner,
    resume_in_place: bool,
    allow_recovery_resume: bool,
) -> Result<DelegationReport> {
    ctx.prepare_for_writing()?;
    let mut config = ctx.config.delegation.clone();
    if resume_in_place {
        config.isolate_in_worktree = false;
        config.require_clean_baseline = false;
    }
    if !config.enabled {
        return Err(CoreError::Intent {
            reason:
                "[delegation] enabled = false, so J.E.A.N. will not hand work to an external agent"
                    .to_owned(),
        });
    }

    let worker = select_worker(&config, request.worker.as_deref())?;
    let workspace = resolve_workspace(request.workspace.as_path())?;
    let verification = resolve_verification(&config)?;

    let task_id = new_task_id();
    let delegation_id = format!(
        "del-{}",
        task_id.strip_prefix("t-").unwrap_or(task_id.as_str())
    );
    let artifacts = ctx.paths.artifacts_for(&task_id);
    std::fs::create_dir_all(&artifacts).map_err(|e| CoreError::Directory {
        path: artifacts.clone(),
        reason: e.to_string(),
    })?;
    let transcript = artifacts.join("worker-transcript.log");

    let store = &ctx.store;
    store.insert_task(&TaskRecord::new(
        task_id.clone(),
        request.goal.clone(),
        Some(format!("delegate:{}", worker.name)),
        serde_json::json!({
            "goal": request.goal,
            "worker": worker.name,
            "kind": worker.kind,
            "workspace": workspace.display().to_string(),
            "verify_command": verification.describe(),
        })
        .to_string(),
    ))?;
    store.transition(
        &task_id,
        TaskState::Planning,
        Some("the goal was delegated"),
    )?;

    store
        .insert_delegation(&NewDelegation {
            id: delegation_id.clone(),
            task_id: task_id.clone(),
            worker: worker.name.clone(),
            kind: worker.kind.clone(),
            goal: ctx.redactor.redact(&request.goal),
            workspace: workspace.display().to_string(),
            worktree: None,
            worktree_note: None,
            // Written before the worker exists: the row states what had to pass, in the same words the
            // prompt uses, and nothing later can change it.
            verify_command: verification.executable.display().to_string(),
            verify_args_json: serde_json::to_string(&verification.args)
                .unwrap_or_else(|_| "[]".to_owned()),
        })
        .map_err(|e| CoreError::Intent {
            reason: format!("the delegation could not be recorded: {e}"),
        })?;

    // Cancellation, once, shared by every child this delegation can start.
    let token = CancelToken::new()?;
    let cancel = Arc::new(AtomicBool::new(false));
    crate::signals::arm(token.clone())?;
    let pipe_name = jean_protocol::ipc::pipe_name_for(&ctx.config.ipc.pipe_prefix, &task_id)
        .map_err(|e| CoreError::Ipc {
            task_id: task_id.clone(),
            reason: e.to_string(),
        })?;
    let listener = CancelListener::start(
        pipe_name,
        task_id.clone(),
        token.clone(),
        ctx.paths.database.clone(),
    )
    .map_err(|e| CoreError::Ipc {
        task_id: task_id.clone(),
        reason: e.to_string(),
    })?;
    let mut teardown = Teardown::new(listener, CancelBridge::start(token.clone(), cancel.clone()));

    let policy = Policy::new(ctx.config.policy.clone());
    let audit = AuditWriter::new(
        ctx.paths.audit.clone(),
        ctx.config.policy.audit_enabled,
        ctx.redactor.clone(),
    );
    let actor = actor_identity();

    // The gate the delegation itself passes through: starting an external agent with a shell in it is
    // an operation, not a formality. Nothing below runs if this says no.
    let launch_operation = delegation_operation(worker, &workspace, &verification);
    let granted = match policy
        .decide(&launch_operation, store, prompter)
        .map_err(CoreError::Policy)?
    {
        Decision::Allowed { authority } => {
            record(
                &audit,
                &launch_operation,
                &Decision::allowed(authority),
                &actor,
                &workspace,
            )?;
            authority
        }
        denied @ Decision::Denied(_) => {
            record(&audit, &launch_operation, &denied, &actor, &workspace)?;
            let text = AuditEvent::denial_text(denied.reason().expect("a denial has a reason"));
            teardown.stop();
            store
                .settle_delegation(&delegation_id, DelegationResult::NeedsAttention.as_str())
                .ok();
            store
                .transition(&task_id, TaskState::Failed, Some(&text))
                .ok();
            return Err(CoreError::Intent {
                reason: format!("the delegation was refused: {text}"),
            });
        }
    };
    store.transition(
        &task_id,
        TaskState::Running,
        Some("the worker was authorised"),
    )?;

    // Where the worker will actually work, and what was already dirty there.
    let isolation = prepare_isolation(ctx, &config, store, &delegation_id, &task_id, &workspace)?;
    let workdir = isolation.directory.clone();
    let pre_existing = dirty_files(ctx, workdir.as_path(), &task_id, "status-before")?;
    // What the operator had already modified in *their own* tree. Only worth asking when the worker
    // went somewhere else, because a worktree starts at HEAD and does not carry those changes, and
    // "the worker changed nothing" would otherwise read as "nothing needed changing".
    let operator_dirty = if isolation.worktree.is_some() {
        dirty_files(ctx, &workspace, &task_id, "status-operator-tree")?
    } else {
        BTreeSet::new()
    };

    // Baseline: the check before the agent touches anything. Its failure is not a reason to refuse
    // the work — fixing a failing check is a normal reason to delegate — but it has to be on the
    // record, because "the tests pass now" means something different when they passed before too.
    let verifier = Verifier {
        ctx,
        policy: &policy,
        audit: &audit,
        actor: &actor,
        store,
        task_id: &task_id,
        delegation_id: &delegation_id,
        token: &token,
    };
    // There is always a check: `resolve_verification` refused the delegation if the operator left
    // `[delegation] verify_command` blank.
    let baseline_run = verifier.run(
        prompter,
        &verification,
        workdir.as_path(),
        "the baseline check before any delegation",
        Phase::Baseline,
    )?;
    store
        .record_delegation_baseline(
            &delegation_id,
            baseline_run.verdict.label(),
            baseline_run.run_id,
        )
        .ok();
    let baseline = baseline_run.verdict;

    if config.require_clean_baseline && !pre_existing.is_empty() && isolation.worktree.is_none() {
        let note = format!(
            "{} file(s) were already modified in {} — refusing to mix them into a delegation summary",
            pre_existing.len(),
            workdir.display()
        );
        finish(
            store,
            &audit,
            &actor,
            &launch_operation,
            &delegation_id,
            &task_id,
            DelegationResult::NeedsAttention,
            &note,
        );
        teardown.stop();
        return Err(CoreError::Intent { reason: note });
    }

    // Durable before the worker can modify its isolated workspace. If this process is interrupted,
    // the uncertain marker forces reconciliation against the task record and workspace evidence.
    crate::owner::write_checkpoint(
        &ctx.paths.data_dir,
        &crate::owner::Checkpoint {
            task_id: task_id.clone(),
            goal: request.goal.clone(),
            resolved_scope: format!(
                "{}; verification: {}",
                workdir.display(),
                verification.describe()
            ),
            results: vec![format!("baseline verification: {}", baseline.label())],
            next_action: "worker may act; reconcile workspace and task evidence before retry"
                .to_owned(),
            uncertain: true,
            workspace: workdir.display().to_string(),
            remaining_work: request.goal.clone(),
            completed_effects: Vec::new(),
            eligible_free_workers: Vec::new(),
            attempted_workers: vec![worker.name.clone()],
            attempts: 0,
            max_attempts: 1,
            resumable: false,
            resumed_task_id: None,
            blocked_on: None,
        },
    )?;

    // The worker session.
    let bounds = Bounds {
        max_frame_bytes: config.max_frame_bytes,
        max_event_queue: config.max_event_queue,
        session_deadline: Instant::now() + Duration::from_secs(config.session_timeout_secs),
        idle_timeout: Duration::from_secs(config.idle_timeout_secs),
        permission_timeout: Duration::from_secs(config.permission_timeout_secs),
        shutdown_grace: Duration::from_millis(u64::from(config.shutdown_grace_ms)),
        max_output_bytes: config.max_output_bytes,
        output_dir: Some(artifacts.clone()),
    };
    let prompt = build_prompt(request, &verification);
    let launch = worker_launch(ctx, worker, workdir.as_path())?;
    let worker_request = WorkerRequest {
        delegation_id: delegation_id.clone(),
        task_id: task_id.clone(),
        goal: request.goal.clone(),
        workspace: workdir.clone(),
        prompt,
    };
    let spec = RunSpec {
        worker,
        launch: &launch,
        request: &worker_request,
        bounds: &bounds,
        cancel: cancel.clone(),
    };

    let mut sink = StoreSink {
        store,
        delegation_id: delegation_id.clone(),
        task_id: task_id.clone(),
        redactor: ctx.redactor.clone(),
        transcript: std::fs::File::options()
            .create(true)
            .append(true)
            .open(&transcript)
            .map_err(|e| CoreError::File {
                path: transcript.clone(),
                reason: e.to_string(),
            })?,
        emitted: 0,
        limit: config.max_event_queue,
        dropped: 0,
    };
    // Scoped, because the host holds the only mutable borrow of the operator's prompter and the
    // verification after the worker needs to ask that same person a question. The two tallies are
    // taken out before the borrow ends.
    let (outcome, covered_asks, prompted_asks) = {
        let mut host = WorkerAuthority::new(
            &policy,
            &audit,
            &actor,
            store,
            prompter,
            worker,
            workdir.clone(),
            bounds.session_deadline,
            bounds.permission_timeout,
            granted,
        );
        let outcome = runner.run(&spec, &mut host, &mut sink);
        (outcome, host.covered, host.asked)
    };

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            // The worker never produced a session. Recorded as an observation of the connector, not
            // as a claim about the task.
            let note = format!("the worker could not be run: {e}");
            store
                .record_worker_settled(&delegation_id, Some("connector_error"), None, None, None)
                .ok();
            finish(
                store,
                &audit,
                &actor,
                &launch_operation,
                &delegation_id,
                &task_id,
                DelegationResult::NeedsAttention,
                &note,
            );
            return Err(CoreError::Intent { reason: note });
        }
    };
    store
        .record_worker_settled(
            &delegation_id,
            Some(&outcome.stop_reason),
            outcome.claim.as_deref(),
            outcome.exit_code,
            outcome
                .output_dir
                .as_deref()
                .map(|d| d.to_str().unwrap_or_default()),
        )
        .ok();

    // What changed, as a diff against what was already dirty. `git status --porcelain` is read-only
    // and is the only summary this build claims to produce; outside a repository it says so.
    let after = dirty_files(ctx, workdir.as_path(), &task_id, "status-after")?;
    let mut changed: Vec<String> = after.difference(&pre_existing).cloned().collect();
    changed.sort();
    let changed_note = if !isolation.repository {
        Some(
            "the workspace is not a git repository, so no file-level summary is possible"
                .to_owned(),
        )
    } else if changed.len() > config.max_changed_files_reported {
        Some(format!(
            "showing the first {} of {} changed file(s)",
            config.max_changed_files_reported,
            changed.len()
        ))
    } else if pre_existing.is_empty() {
        None
    } else {
        Some(format!(
            "{} file(s) the operator had already modified are excluded",
            pre_existing.len()
        ))
    };
    let shown: Vec<String> = changed
        .iter()
        .take(config.max_changed_files_reported)
        .cloned()
        .collect();
    store
        .record_delegation_changes(
            &delegation_id,
            &serde_json::to_string(&shown).unwrap_or_else(|_| "[]".to_owned()),
            i64::try_from(changed.len()).unwrap_or(i64::MAX),
        )
        .ok();
    store
        .append_event(
            &task_id,
            &Event::new(
                EventKind::ChangesRecorded,
                ctx.redactor.redact(&format!(
                    "{} changed file(s) after {}: {}",
                    changed.len(),
                    worker.name,
                    shown.join(", ")
                )),
            ),
        )
        .ok();

    // The independent check, after the worker. Same resolved argv as the baseline, from the same
    // configuration the worker never saw.
    let verification_verdict = if cancel.load(Ordering::Relaxed) {
        // A cancelled delegation gets no second act: the operator asked for it to stop, and running
        // a check they may have watched time out would be J.E.A.N. deciding to keep working.
        None
    } else {
        let run = verifier.run(
            prompter,
            &verification,
            workdir.as_path(),
            "the verification after the delegation",
            Phase::After,
        )?;
        store
            .record_delegation_verification(&delegation_id, run.verdict.label(), run.run_id)
            .ok();
        Some(run.verdict)
    };
    // The cancellation machinery outlives the check itself, not the delegation: an operator who
    // cancels during the verification is entitled to have it stop.
    teardown.stop();

    let cancelled = cancel.load(Ordering::Relaxed)
        || outcome.stop_reason == STOP_CANCELLED
        || token.is_cancelled();
    let recovery_failure = outcome.failure_kind;
    let recognized_recovery = recovery_failure.is_some();
    let result = if recognized_recovery {
        DelegationResult::NeedsAttention
    } else {
        decide_result(cancelled, &outcome, verification_verdict.as_ref())
    };

    let mut checkpoint_results = vec![
        format!("baseline verification: {}", baseline.label()),
        format!("worker stopped: {}", outcome.stop_reason),
        format!(
            "independent verification: {}",
            verification_verdict
                .as_ref()
                .map(Verdict::label)
                .unwrap_or("not run")
        ),
        format!("JEAN task verdict: {}", result.as_str()),
    ];
    checkpoint_results.extend(shown.iter().map(|file| format!("changed: {file}")));
    let mut resumable = false;
    let remaining_work = outcome.remaining_work.clone().unwrap_or_default();
    let mut completed_effects = Vec::new();
    let mut eligible_free_workers = Vec::new();
    let mut attempted_workers = vec![worker.name.clone()];
    let mut blocked_on = match recovery_failure {
        Some(WorkerFailureKind::SignInExpired) => {
            Some(crate::owner::RecoveryBlocker::SignInExpired)
        }
        Some(WorkerFailureKind::NetworkUnavailable) => {
            Some(crate::owner::RecoveryBlocker::NetworkUnavailable)
        }
        _ => None,
    };
    let mut next_action = if result == DelegationResult::Succeeded {
        "review the verified diff; no further action is pending".to_owned()
    } else {
        "review task status and reconcile workspace evidence before any retry".to_owned()
    };
    if recognized_recovery && allow_recovery_resume {
        eligible_free_workers = config
            .workers
            .iter()
            .filter(|candidate| {
                candidate.enabled && candidate.free_fallback && candidate.name != worker.name
            })
            .map(|candidate| candidate.name.clone())
            .collect();
        let effect_snapshot = if pre_existing.is_empty() {
            if changed.is_empty() {
                Ok(Vec::new())
            } else {
                crate::owner::snapshot_workspace_effects(&workdir, &changed)
            }
        } else {
            Err(CoreError::Environment {
                reason: "the initial workspace already had uncommitted changes".to_owned(),
            })
        };
        match effect_snapshot {
            Ok(effects)
                if !remaining_work.trim().is_empty() && !eligible_free_workers.is_empty() =>
            {
                completed_effects = effects;
                resumable = true;
                next_action = match recovery_failure {
                    Some(WorkerFailureKind::QuotaExhausted) => format!(
                        "quota_exhausted: resume once with an eligible free worker: jean resume {task_id}"
                    ),
                    Some(WorkerFailureKind::ContextExhausted) => format!(
                        "context_exhausted: resume remaining work in a fresh session: jean resume {task_id}"
                    ),
                    Some(WorkerFailureKind::SignInExpired) => format!(
                        "{}: {}; then run jean resume {task_id} --condition-cleared",
                        crate::owner::RecoveryBlocker::SignInExpired.status(),
                        crate::owner::RecoveryBlocker::SignInExpired.action()
                    ),
                    Some(WorkerFailureKind::NetworkUnavailable) => format!(
                        "{}: {}; then run jean resume {task_id} --condition-cleared",
                        crate::owner::RecoveryBlocker::NetworkUnavailable.status(),
                        crate::owner::RecoveryBlocker::NetworkUnavailable.action()
                    ),
                    None => unreachable!(),
                };
            }
            Ok(_) => {
                next_action = "needs_attention: quota evidence lacked remaining work or an eligible free worker".to_owned();
            }
            Err(error) => {
                next_action = format!("needs_attention: quota fallback is unsafe: {error}");
            }
        }
        if eligible_free_workers.is_empty() {
            next_action = "needs_attention: no explicitly free-eligible fallback worker is enabled"
                .to_owned();
            resumable = false;
        }
    }
    if !allow_recovery_resume && recovery_failure.is_some() {
        resumable = false;
        if blocked_on.is_none() {
            next_action =
                "needs_attention: the one replacement attempt ended; no further retry is allowed"
                    .to_owned();
        }
    }
    let checkpoint_workspace = workdir.display().to_string();
    crate::owner::write_checkpoint(
        &ctx.paths.data_dir,
        &crate::owner::Checkpoint {
            task_id: task_id.clone(),
            goal: request.goal.clone(),
            resolved_scope: format!(
                "{}; verification: {}",
                workdir.display(),
                verification.describe()
            ),
            results: checkpoint_results,
            next_action,
            uncertain: false,
            workspace: checkpoint_workspace,
            remaining_work,
            completed_effects,
            eligible_free_workers,
            attempted_workers: std::mem::take(&mut attempted_workers),
            attempts: 0,
            max_attempts: if recognized_recovery { 1 } else { 0 },
            resumable,
            resumed_task_id: None,
            blocked_on: blocked_on.take(),
        },
    )?;

    let mut notes = Vec::new();
    if let Some(failure) = &outcome.failure {
        notes.push(format!("the worker's session reported: {failure}"));
    }
    if sink.dropped > 0 {
        notes.push(format!(
            "{} event(s) were dropped at the queue ceiling of {}",
            sink.dropped, sink.limit
        ));
    }
    if let Some(claim) = &outcome.claim {
        notes.push(format!(
            "the worker claimed {claim:?}; the result above came from the verification, not from it"
        ));
    }
    if let Some(path) = &isolation.worktree {
        notes.push(format!(
            "the worktree at {} was left in place; J.E.A.N. does not remove worktrees",
            path.display()
        ));
        if !operator_dirty.is_empty() {
            // Worth saying out loud: the operator's uncommitted work is safe in their own tree, but
            // the worker never saw it, because a worktree starts at HEAD.
            notes.push(format!(
                "{} file(s) the operator had already modified are not in the worktree, because it \
                 was created at HEAD",
                operator_dirty.len()
            ));
        }
    }

    store
        .set_delegation_notes(
            &delegation_id,
            &serde_json::to_string(&notes).unwrap_or_else(|_| "[]".to_owned()),
        )
        .ok();
    finish(
        store,
        &audit,
        &actor,
        &launch_operation,
        &delegation_id,
        &task_id,
        result,
        "",
    );

    Ok(DelegationReport {
        task_id,
        delegation_id,
        worker: worker.name.clone(),
        kind: worker.kind.clone(),
        workspace,
        worktree: isolation.worktree.clone(),
        isolation: isolation.explanation.clone(),
        baseline,
        stop_reason: outcome.stop_reason.clone(),
        claim: outcome.claim.clone(),
        changed_files: shown,
        changed_note,
        verification: verification_verdict,
        result,
        events: sink.emitted,
        covered_asks,
        prompted_asks,
        transcript,
        notes,
    })
}

/// Restore a checkpoint, reconcile its exact workspace evidence, and make at most one
/// replacement delegation through the ordinary policy/verification path.
pub fn resume_checkpoint_with(
    ctx: &mut Context,
    task_id: &str,
    prompter: &mut dyn Prompter,
    runner: &dyn WorkerRunner,
) -> Result<ResumeOutcome> {
    resume_checkpoint_with_condition(ctx, task_id, prompter, runner, false)
}

fn resume_checkpoint_with_condition(
    ctx: &mut Context,
    task_id: &str,
    prompter: &mut dyn Prompter,
    runner: &dyn WorkerRunner,
    condition_cleared: bool,
) -> Result<ResumeOutcome> {
    let mut checkpoint = crate::owner::read_checkpoint(&ctx.paths.data_dir, task_id)?;
    let attention = |ctx: &Context, checkpoint: &mut crate::owner::Checkpoint, reason: String| {
        checkpoint.resumable = false;
        checkpoint.next_action = format!("needs_attention: {reason}");
        crate::owner::write_checkpoint(&ctx.paths.data_dir, checkpoint)?;
        Ok::<ResumeOutcome, CoreError>(ResumeOutcome::NeedsAttention {
            task_id: checkpoint.task_id.clone(),
            reason,
        })
    };

    if checkpoint.uncertain {
        return Ok(ResumeOutcome::NeedsAttention {
            task_id: checkpoint.task_id,
            reason: "a replacement may already have acted; preserve the reservation and reconcile manually before retrying".to_owned(),
        });
    }
    if !checkpoint.resumable {
        return attention(
            ctx,
            &mut checkpoint,
            "checkpoint is not marked resumable".to_owned(),
        );
    }
    if checkpoint.attempts >= checkpoint.max_attempts || checkpoint.max_attempts == 0 {
        return attention(
            ctx,
            &mut checkpoint,
            "the checkpoint's bounded retry count is exhausted".to_owned(),
        );
    }
    if let Err(error) = crate::owner::reconcile_for_resume(&checkpoint) {
        return attention(ctx, &mut checkpoint, error.to_string());
    }
    if let Some(
        blocker @ (crate::owner::RecoveryBlocker::SignInExpired
        | crate::owner::RecoveryBlocker::NetworkUnavailable),
    ) = checkpoint.blocked_on
    {
        if !condition_cleared {
            let reason = blocker.action().to_owned();
            checkpoint.next_action = format!(
                "{}: {reason}; then run jean resume {} --condition-cleared",
                blocker.status(),
                checkpoint.task_id
            );
            crate::owner::write_checkpoint(&ctx.paths.data_dir, &checkpoint)?;
            return Ok(ResumeOutcome::Blocked {
                task_id: checkpoint.task_id.clone(),
                status: blocker.status().to_owned(),
                reason,
            });
        }
    }

    let min_free = ctx.config.delegation.resume_min_free_disk_bytes;
    for path in [
        Path::new(&checkpoint.workspace),
        ctx.paths.data_dir.as_path(),
    ] {
        match available_disk_bytes(path) {
            Ok(bytes) if bytes >= min_free => {}
            Ok(bytes) => {
                let blocker = crate::owner::RecoveryBlocker::LowDisk;
                let reason = format!(
                    "{}; detected {bytes} available bytes, configured minimum is {min_free}",
                    blocker.action()
                );
                checkpoint.blocked_on = Some(blocker);
                checkpoint.next_action = format!("{}: {reason}", blocker.status());
                crate::owner::write_checkpoint(&ctx.paths.data_dir, &checkpoint)?;
                return Ok(ResumeOutcome::Blocked {
                    task_id: checkpoint.task_id.clone(),
                    status: blocker.status().to_owned(),
                    reason,
                });
            }
            Err(error) => {
                let blocker = crate::owner::RecoveryBlocker::DiskCheckUnavailable;
                let reason = format!("{}: {error}", blocker.action());
                checkpoint.blocked_on = Some(blocker);
                checkpoint.next_action = format!("{}: {reason}", blocker.status());
                crate::owner::write_checkpoint(&ctx.paths.data_dir, &checkpoint)?;
                return Ok(ResumeOutcome::Blocked {
                    task_id: checkpoint.task_id.clone(),
                    status: blocker.status().to_owned(),
                    reason,
                });
            }
        }
    }
    let candidate = checkpoint.eligible_free_workers.iter().find_map(|name| {
        ctx.config.delegation.workers.iter().find(|worker| {
            worker.name == *name
                && worker.enabled
                && worker.free_fallback
                && !checkpoint
                    .attempted_workers
                    .iter()
                    .any(|attempted| attempted == name)
        })
    });
    let Some(candidate) = candidate else {
        return attention(
            ctx,
            &mut checkpoint,
            "no enabled, explicitly free-eligible untried worker remains".to_owned(),
        );
    };
    if checkpoint.remaining_work.trim().is_empty() {
        return attention(
            ctx,
            &mut checkpoint,
            "checkpoint has no remaining work".to_owned(),
        );
    }

    let worker_name = candidate.name.clone();
    checkpoint.blocked_on = None;
    checkpoint.attempts += 1;
    checkpoint.attempted_workers.push(worker_name.clone());
    checkpoint.uncertain = true;
    checkpoint.resumable = false;
    checkpoint.next_action =
        format!("replacement worker {worker_name:?} may act; reconcile before any further retry");
    crate::owner::write_checkpoint(&ctx.paths.data_dir, &checkpoint)?;

    let request = DelegateRequest {
        goal: checkpoint.remaining_work.clone(),
        workspace: PathBuf::from(&checkpoint.workspace),
        worker: Some(worker_name.clone()),
    };
    let report = delegate_with_mode(ctx, &request, prompter, runner, true, false)?;

    checkpoint.uncertain = false;
    checkpoint.resumed_task_id = Some(report.task_id.clone());
    checkpoint
        .results
        .push(format!("replacement worker: {worker_name}"));
    checkpoint.results.push(format!(
        "replacement task verdict: {}",
        report.result.as_str()
    ));
    checkpoint.next_action = if report.result == DelegationResult::Succeeded {
        "resume verified; no further worker attempt is allowed".to_owned()
    } else {
        "needs_attention: replacement did not independently verify; no further worker attempt is allowed".to_owned()
    };
    crate::owner::write_checkpoint(&ctx.paths.data_dir, &checkpoint)?;
    Ok(ResumeOutcome::Completed(report))
}

pub fn resume_checkpoint(
    ctx: &mut Context,
    task_id: &str,
    prompter: &mut dyn Prompter,
) -> Result<ResumeOutcome> {
    resume_checkpoint_with(ctx, task_id, prompter, &AcpRunner)
}

pub fn resume_checkpoint_after_condition_cleared(
    ctx: &mut Context,
    task_id: &str,
    prompter: &mut dyn Prompter,
) -> Result<ResumeOutcome> {
    resume_checkpoint_with_condition(ctx, task_id, prompter, &AcpRunner, true)
}

fn available_disk_bytes(path: &Path) -> std::result::Result<u64, String> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        let resolved = path
            .canonicalize()
            .map_err(|error| format!("could not resolve {}: {error}", path.display()))?;
        let wide: Vec<u16> = resolved.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut available = 0_u64;
        // SAFETY: `wide` is NUL-terminated and remains alive for the call. The output pointer
        // points to a valid u64 for the duration of the synchronous Windows API call.
        unsafe {
            windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                PCWSTR(wide.as_ptr()),
                Some(&mut available),
                None,
                None,
            )
        }
        .map_err(|error| format!("free-space query failed: {error}"))?;
        Ok(available)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("free-space checking is implemented only on Windows".to_owned())
    }
}

/// The result rule, in one place because it is the sentence the whole phase is judged on.
fn decide_result(
    cancelled: bool,
    outcome: &WorkerOutcome,
    verification: Option<&Verdict>,
) -> DelegationResult {
    if cancelled {
        return DelegationResult::Cancelled;
    }
    match verification {
        None => DelegationResult::Unverified,
        Some(verdict) => match (verdict, outcome.ended_cleanly()) {
            (Verdict::Proven(_), true) => DelegationResult::Succeeded,
            // The check passes but the session did not end the way a finished job ends: the operator
            // decides what that means, this build does not.
            (Verdict::Proven(_), false) => DelegationResult::NeedsAttention,
            // The check itself never produced an answer — refused by policy, cancelled, never
            // started. Attributing that to the worker's code would blame the wrong thing for the
            // wrong reason.
            (Verdict::Unverifiable(_), _) => DelegationResult::Unverified,
            (Verdict::NotProven(_), _) => DelegationResult::Failed,
        },
    }
}

/// Write the settlement down: the delegation row, the task row, and the audit line.
#[allow(clippy::too_many_arguments)]
fn finish(
    store: &Store,
    audit: &AuditWriter,
    actor: &str,
    operation: &Operation,
    delegation_id: &str,
    task_id: &str,
    result: DelegationResult,
    note: &str,
) {
    // `settle_delegation` refuses to overwrite a row that already has a result, so a delegation that
    // was settled by an earlier failure keeps the first answer.
    match store.settle_delegation(delegation_id, result.as_str()) {
        Ok(true) => {}
        Ok(false) => tracing::warn!(delegation_id, "the delegation had already been settled"),
        Err(e) => tracing::warn!(error = %e, "the delegation could not be settled"),
    }
    let state = match result {
        DelegationResult::Succeeded => TaskState::Succeeded,
        DelegationResult::Cancelled => TaskState::Cancelled,
        DelegationResult::NeedsAttention | DelegationResult::Unverified => {
            TaskState::NeedsAttention
        }
        DelegationResult::Failed => TaskState::Failed,
    };
    let reason = match (result, note) {
        (_, text) if !text.is_empty() => text.to_owned(),
        (DelegationResult::Succeeded, _) => {
            "the verification passed after the delegation".to_owned()
        }
        (DelegationResult::Failed, _) => "the verification did not pass".to_owned(),
        (DelegationResult::Cancelled, _) => "the delegation was cancelled".to_owned(),
        (DelegationResult::Unverified, _) => {
            "no verification command was configured, so nothing was proven".to_owned()
        }
        (DelegationResult::NeedsAttention, _) => {
            "the delegation needs the operator's attention".to_owned()
        }
    };
    // The state machine requires `running → verifying → succeeded`, and a delegation is exactly the
    // kind of task that earns the middle step. Nothing else needs the hop: `failed`, `cancelled`
    // and `needs_attention` are reachable from `running` directly.
    if state == TaskState::Succeeded {
        store
            .transition(
                task_id,
                TaskState::Verifying,
                Some("the worker finished, so the check ran"),
            )
            .ok();
    }
    if store.transition(task_id, state, Some(&reason)).is_err() {
        tracing::warn!(delegation_id, "the task could not be transitioned");
    }
    store.set_outcome(task_id, result.as_str(), &reason).ok();
    let event = AuditEvent::for_decision(
        operation,
        &Decision::allowed(Authority::OwnerCommand),
        actor.to_owned(),
    )
    .with_task(task_id)
    .with_notes(Some(format!(
        "delegation {delegation_id} settled: {reason}"
    )));
    if let Err(e) = audit.record(&event) {
        tracing::warn!(error = %e, "the settlement could not be audited");
    }
}

fn record(
    audit: &AuditWriter,
    operation: &Operation,
    decision: &Decision,
    actor: &str,
    workspace: &Path,
) -> Result<()> {
    let event = AuditEvent::for_decision(operation, decision, actor.to_owned())
        .with_working_dir(Some(workspace.display().to_string()));
    audit.record(&event).map_err(|e| CoreError::Intent {
        reason: e.to_string(),
    })
}

/// The worker, as configured. A disabled worker is not silently replaced by the default.
fn select_worker<'a>(
    config: &'a DelegationConfig,
    named: Option<&str>,
) -> Result<&'a WorkerConfig> {
    let wanted = named.unwrap_or(config.default_worker.as_str());
    config
        .workers
        .iter()
        .find(|w| w.name == wanted)
        .ok_or_else(|| CoreError::Intent {
            reason: format!(
                "no worker named {wanted:?} is in [delegation] workers (known: {})",
                config
                    .workers
                    .iter()
                    .map(|w| w.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
        .and_then(|worker| {
            if worker.enabled {
                Ok(worker)
            } else {
                Err(CoreError::Intent {
                    reason: format!("worker {:?} is disabled", worker.name),
                })
            }
        })
}

fn resolve_workspace(path: &Path) -> Result<PathBuf> {
    let meta = std::fs::metadata(path).map_err(|e| CoreError::Directory {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    if !meta.is_dir() {
        return Err(CoreError::Directory {
            path: path.to_path_buf(),
            reason: "it is a file, not a directory".to_owned(),
        });
    }
    path.canonicalize()
        .map(plain_windows_path)
        .map_err(|e| CoreError::Directory {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })
}

/// The form of a path that another program can be *started* in.
///
/// `std::fs::canonicalize` returns a **verbatim** path on Windows — `\\?\C:\Users\…` — which is exact
/// but is a string that ordinary programs do not understand: `cmd.exe` reads it as a UNC path, says so
/// on stderr, and silently starts in `C:\Windows`, which turns a verification command into a check run
/// in the wrong tree. `git` handed one as its working directory takes minutes where it should take
/// milliseconds. The prefix is therefore removed for a drive path, and a real share keeps its `\\host`
/// spelling. Nothing else changes: containment tests re-canonicalize on both sides, so they are
/// unaffected, and a path that has to be *given to a child* is the one that has to look like a path.
fn plain_windows_path(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(share) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{share}"));
    }
    if let Some(drive) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(drive);
    }
    path
}

/// The check, resolved before the worker exists.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Verification {
    executable: PathBuf,
    args: Vec<String>,
    timeout: Duration,
}

impl Verification {
    fn describe(&self) -> String {
        format!(
            "{}{}",
            self.executable.display(),
            if self.args.is_empty() {
                String::new()
            } else {
                format!(" {}", self.args.join(" "))
            }
        )
    }
}

fn resolve_verification(config: &DelegationConfig) -> Result<Verification> {
    let command = config.verify_command.trim();
    if command.is_empty() {
        // Refused here rather than reported later as unverified: without a check the operator's
        // report can only say "nothing was proven", and by then a worker's session and the tokens it
        // spent are already gone. `[delegation] verify_command` is documented as required, and an
        // external agent with nothing to satisfy is not a delegation, it is a guess.
        return Err(CoreError::Intent {
            reason:
                "[delegation] verify_command is empty, so there is nothing J.E.A.N. could check \
                     afterwards. Set it (for example `cargo`) with `verify_args` (for example \
                     `[\"test\"]`) and delegate again."
                    .to_owned(),
        });
    }
    let path = crate::resolve::resolve_executable(
        command,
        std::env::var("PATH").ok().as_deref(),
        std::env::var("PATHEXT").ok().as_deref(),
    )
    .map_err(|e| CoreError::UnresolvedExecutable {
        tool_id: "delegation.verify_command".to_owned(),
        command: command.to_owned(),
        reason: e.reason,
    })?;
    Ok(Verification {
        executable: path,
        args: config.verify_args.clone(),
        timeout: Duration::from_secs(config.verify_timeout_secs),
    })
}

/// The prompt the worker is given: the operator's goal, plus the check it must satisfy.
///
/// Stating the check as data rather than as an instruction the agent can argue with is what makes
/// "the agent was told what had to pass" a fact in the transcript. The worker is *told* the command
/// and never *asked* for one: it has no say in what decides its own success.
fn build_prompt(request: &DelegateRequest, check: &Verification) -> String {
    format!(
        "Goal (from the operator, verbatim):\n{}\n\nJ.E.A.N. will decide whether this task succeeded \
         by running `{}` in the workspace after you finish. Make that check pass. Do not modify the \
         check, its configuration, or the test harness to make it pass.\n\nWork only inside the \
         workspace you were given. Report what you changed when you are done.",
        request.goal.trim(),
        check.describe()
    )
}

/// The launch spec for one worker, built the same way an intent builds one.
fn worker_launch(ctx: &Context, worker: &WorkerConfig, workdir: &Path) -> Result<LaunchSpec> {
    let path = crate::resolve::resolve_executable(
        &worker.command,
        std::env::var("PATH").ok().as_deref(),
        std::env::var("PATHEXT").ok().as_deref(),
    )
    .map_err(|e| CoreError::UnresolvedExecutable {
        tool_id: format!("delegation.worker:{}", worker.name),
        command: worker.command.clone(),
        reason: e.reason,
    })?;
    Ok(LaunchSpec {
        executable: path,
        args: worker.args.clone(),
        // The *worktree* when there is one: a worker pointed at a tree it can write in, not at the
        // operator's checkout. `resolve_workspace` required the directory to exist before any of
        // this ran, and a tree that vanished since is refused by `LaunchSpec::validate`.
        working_dir: Some(workdir.to_path_buf()),
        env: worker_environment(ctx, worker),
        // The supervisor's own timeout is the session deadline plus a margin, so a worker cannot
        // outlive its job even if the watchdog thread is starved.
        timeout: Some(Duration::from_secs(
            ctx.config
                .delegation
                .session_timeout_secs
                .saturating_add(30),
        )),
        output_cap_bytes: ctx.config.delegation.max_output_bytes,
        stdout_path: None,
        stderr_path: None,
        terminal: TerminalMode::Pipes,
    })
}

/// The allowlist, plus the names this worker was given.
fn worker_environment(
    ctx: &Context,
    worker: &WorkerConfig,
) -> std::collections::BTreeMap<String, String> {
    let parent: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let mut allowlist = ctx.config.process.env_allowlist.clone();
    // `[delegation] workers.env_passthrough` is a per-worker extension of the same allowlist, and it
    // goes through the same filter: the worker does not choose its own environment, and a name that
    // is not listed here never reaches the child.
    allowlist.extend(worker.env_passthrough.iter().cloned());
    let (child, report) = build_child_env(&allowlist, &Default::default(), &parent);
    if !report.is_empty() {
        tracing::debug!(
            inherited = ?report.inherited,
            rejected = ?report.rejected,
            "the worker's environment was narrowed to the allowlist"
        );
    }
    child
}

/// Which of a delegation's two checks this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Before the worker exists.
    Baseline,
    /// After the worker's session ended.
    After,
}

impl Phase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::After => "verification",
        }
    }
}

/// One check, and the `tool_runs` row that shows it was supervised rather than asserted.
struct CheckRun {
    verdict: Verdict,
    run_id: Option<i64>,
}

/// The delegation's own verifier: the same resolved argv, run twice, recorded as an observation.
///
/// Everything it needs is fixed when it is built — the command, its timeout, the supervision token,
/// the task the rows belong to — so the baseline and the final check cannot drift apart, and neither
/// one consults the worker. This is the mechanism behind the phase's central rule: an agent's claim
/// of success has no path into the verdict.
struct Verifier<'a> {
    ctx: &'a Context,
    policy: &'a Policy,
    audit: &'a AuditWriter,
    actor: &'a str,
    store: &'a Store,
    task_id: &'a str,
    delegation_id: &'a str,
    token: &'a CancelToken,
}

impl Verifier<'_> {
    /// Decide, run, record, classify.
    ///
    /// `prompter` is taken per call: both checks ask the same person the same question, and the
    /// borrow must not outlive either answer.
    fn run(
        &self,
        prompter: &mut dyn Prompter,
        check: &Verification,
        workdir: &Path,
        purpose: &str,
        phase: Phase,
    ) -> Result<CheckRun> {
        let operation = Operation {
            kind: OperationKind::RunCommand,
            tool_id: None,
            tier: PolicyTier::Normal,
            // Its own capability, so an operator who wants to gate the check separately from the worker
            // can name `delegation-verify` in `[policy] require_approval`.
            capabilities: vec!["delegation-verify".to_owned()],
            summary: format!("{purpose}: {}", check.describe()),
            scope: Scope::new(check.executable.display().to_string(), check.args.clone()),
        };
        // The engine decides, with no grant: the check is not covered by the operator having typed
        // `jean delegate`, and a capability the operator put on the list gets asked about every time.
        let decision = self.policy.decide(&operation, self.store, prompter)?;
        record(self.audit, &operation, &decision, self.actor, workdir)?;
        if let Decision::Denied(reason) = decision {
            // Not a failure of the code under test: a refusal by this build, recorded as such so the
            // delegation cannot be reported as verified.
            return Ok(CheckRun {
                verdict: Verdict::Unverifiable(format!(
                    "the check was not allowed to run: {}",
                    AuditEvent::denial_text(&reason)
                )),
                run_id: None,
            });
        }

        let artifacts = self.ctx.paths.artifacts_for(self.task_id);
        let name = phase.as_str();
        let stdout_path = artifacts.join(format!("{name}-stdout.log"));
        let spec = LaunchSpec {
            executable: check.executable.clone(),
            args: check.args.clone(),
            working_dir: Some(workdir.to_path_buf()),
            env: build_child_env(
                &self.ctx.config.process.env_allowlist,
                &Default::default(),
                &std::env::vars().collect(),
            )
            .0,
            timeout: Some(check.timeout),
            output_cap_bytes: self.ctx.output_cap(),
            stdout_path: Some(stdout_path.clone()),
            stderr_path: Some(artifacts.join(format!("{name}-stderr.log"))),
            terminal: TerminalMode::Pipes,
        };
        // The run row exists before the child does, so a check that was killed mid-flight still has
        // a record that it was started.
        let run_id = self
            .store
            .insert_tool_run(
                self.task_id,
                &format!("delegation-{name}"),
                &check.executable.display().to_string(),
                &serde_json::to_string(&check.args).unwrap_or_else(|_| "[]".to_owned()),
                Some(&workdir.display().to_string()),
                "{}",
            )
            .ok();
        let outcome = jean_process::supervise(&spec, self.token, |pid| {
            if let Some(id) = run_id {
                self.store.record_child_pid(id, pid).ok();
            }
        })?;
        self.store.append_event(
            self.task_id,
            &Event::new(
                EventKind::VerificationRecorded,
                self.ctx.redactor.redact(&format!(
                    "delegation {}: {name} check {} → {}",
                    self.delegation_id,
                    check.describe(),
                    outcome
                        .exit_code
                        .map(|c| format!("exit {c}"))
                        .unwrap_or_else(|| "no exit code".to_owned())
                )),
            ),
        )?;

        Ok(CheckRun {
            verdict: classify(&outcome, check, &stdout_path),
            run_id,
        })
    }
}

/// Exit code and termination, turned into the same three-way verdict a tool run gets.
///
/// Only `Exited` with code 0 is a proof. A timeout is *not* proven, which is the honest reading: the
/// check did not pass inside the budget. Anything that prevented the check from running at all is
/// unverifiable, never a failure attributed to the worker's code.
fn classify(
    outcome: &jean_process::ChildOutcome,
    check: &Verification,
    evidence: &Path,
) -> Verdict {
    let described = check.describe();
    match (outcome.exit_code, &outcome.termination) {
        (Some(0), Termination::Exited) => Verdict::Proven(format!(
            "{described} exited 0; output kept at {}",
            evidence.display()
        )),
        (Some(code), Termination::Exited) => {
            Verdict::NotProven(format!("{described} exited {code}"))
        }
        (_, Termination::Cancelled) => {
            Verdict::Unverifiable(format!("{described} was cancelled before it finished"))
        }
        (_, Termination::Timeout) => Verdict::NotProven(format!(
            "{described} hit its {} s deadline",
            check.timeout.as_secs()
        )),
        (_, Termination::LaunchFailed) => Verdict::Unverifiable(format!(
            "{described} could not be started: {}",
            outcome.error.as_deref().unwrap_or("no reason reported")
        )),
        (_, Termination::JobAssignmentFailed) => Verdict::Unverifiable(format!(
            "{described} was never inside a supervised job: {}",
            outcome.error.as_deref().unwrap_or("no reason reported")
        )),
        (_, Termination::Orphaned) => Verdict::Unverifiable(format!(
            "{described} outlived its supervisor, so its exit code is not known"
        )),
        (None, Termination::Exited) => {
            Verdict::Unverifiable(format!("{described} exited without reporting a code"))
        }
    }
}

/// Where the worker will work, and why.
#[derive(Debug, Clone)]
struct Isolation {
    directory: PathBuf,
    worktree: Option<PathBuf>,
    repository: bool,
    explanation: String,
}

fn prepare_isolation(
    ctx: &Context,
    config: &DelegationConfig,
    store: &Store,
    delegation_id: &str,
    task_id: &str,
    workspace: &Path,
) -> Result<Isolation> {
    let Some(git) = Git::find(ctx) else {
        // No `git` on this machine. That is only a refusal when the operator asked for isolation,
        // and it is never a reason to quietly write into their tree instead.
        return refusal_if_isolation_asked(
            config,
            workspace,
            "`git` is not installed or not on PATH",
        );
    };
    let repo = Repo {
        ctx,
        git: &git,
        dir: workspace.to_path_buf(),
        task_id,
    };
    let repository = repo.is_work_tree()?;
    if !config.isolate_in_worktree {
        return Ok(Isolation {
            directory: workspace.to_path_buf(),
            worktree: None,
            repository,
            explanation: if repository {
                "the worker works directly in the operator's tree (isolate_in_worktree = false)"
                    .to_owned()
            } else {
                "not a git repository, so isolation was not possible".to_owned()
            },
        });
    }
    if !repository {
        // Refusing, rather than falling back to the operator's tree: the operator asked for
        // isolation, and the one thing that must not happen is their own checkout being written into
        // when they asked that it not be.
        return refusal_if_isolation_asked(
            config,
            workspace,
            "it is not a git working tree, so no worktree can be created",
        );
    }
    let root = if config.worktree_root.trim().is_empty() {
        ctx.paths.artifacts.clone()
    } else {
        PathBuf::from(config.worktree_root.trim())
    };
    // One directory per delegation, and never inside the operator's tree.
    let target = root.join(delegation_id);
    if target.starts_with(workspace) {
        return Err(CoreError::Intent {
            reason: format!(
                "[delegation] worktree_root {:?} points inside the workspace, which would put a \
                 second working tree under the operator's own directory",
                config.worktree_root
            ),
        });
    }
    let parent = target.parent().unwrap_or(target.as_path());
    std::fs::create_dir_all(parent).map_err(|e| CoreError::Directory {
        path: target.parent().unwrap_or(target.as_path()).to_path_buf(),
        reason: e.to_string(),
    })?;
    // `git worktree add --detach` creates a new working tree at HEAD without touching any existing
    // branch, worktree, or uncommitted change. Nothing here removes it afterwards: an operator's
    // repository is not this build's to tidy.
    repo.add_worktree(&target)?;
    store
        .set_delegation_worktree(
            delegation_id,
            Some(&target.display().to_string()),
            Some("created by `git worktree add --detach` at HEAD"),
        )
        .ok();
    store
        .append_event(
            task_id,
            &Event::new(
                EventKind::WorktreePrepared,
                ctx.redactor.redact(&format!(
                    "detached worktree for {delegation_id} at {}",
                    target.display()
                )),
            ),
        )
        .ok();
    Ok(Isolation {
        directory: target.clone(),
        worktree: Some(target),
        repository: true,
        explanation: "the worker was given an isolated detached worktree at HEAD".to_owned(),
    })
}

/// The one refusal message isolation can produce, phrased for whatever asked for it.
fn refusal_if_isolation_asked(
    config: &DelegationConfig,
    workspace: &Path,
    why: &str,
) -> Result<Isolation> {
    if config.isolate_in_worktree {
        return Err(CoreError::Intent {
            reason: format!(
                "isolation was asked for and {} is {why}; J.E.A.N. will not write into the tree \
                 instead",
                workspace.display()
            ),
        });
    }
    Ok(Isolation {
        directory: workspace.to_path_buf(),
        worktree: None,
        repository: false,
        explanation: format!("no repository here ({why}); the worker uses the workspace directly"),
    })
}

/// The `git` binary, resolved once per process the same way a tool command is resolved.
struct Git {
    path: PathBuf,
}

impl Git {
    fn find(ctx: &Context) -> Option<Git> {
        static PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        let resolved = PATH.get_or_init(|| {
            crate::resolve::resolve_executable(
                "git",
                std::env::var("PATH").ok().as_deref(),
                std::env::var("PATHEXT").ok().as_deref(),
            )
            .ok()
        });
        let _ = ctx;
        resolved.clone().map(|path| Git { path })
    }
}

/// The repository facts one delegation needs, read through `git` with its output captured to the
/// task's own artifact directory.
///
/// ## What it cannot do
///
/// `run` refuses any argv whose first element is not in [`GIT_VERBS`], and that list holds only
/// read-only inspection and `worktree add`. There is no route from here to `commit`, `push`,
/// `reset`, `clean`, `checkout`, `branch`, `merge` or `worktree remove` — not because a caller might
/// forget, but because the strings are not in the list and `run` will not accept one that is not.
/// The operator's repository is not this build's to tidy.
struct Repo<'a> {
    ctx: &'a Context,
    git: &'a Git,
    dir: PathBuf,
    task_id: &'a str,
}

/// The verbs [`Repo::run`] accepts.
const GIT_VERBS: [&str; 3] = ["rev-parse", "status", "worktree"];

struct GitRun {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
}

impl Repo<'_> {
    fn run(&self, label: &str, args: &[&str]) -> Result<GitRun> {
        let verb = args.first().copied().unwrap_or("");
        if !GIT_VERBS.contains(&verb) {
            return Err(CoreError::Intent {
                reason: format!("`git {verb}` is not a verb this build is willing to run"),
            });
        }
        let artifacts = self.ctx.paths.artifacts_for(self.task_id);
        let stdout_path = artifacts.join(format!("git-{label}-stdout"));
        let spec = LaunchSpec {
            executable: self.git.path.clone(),
            args: {
                let mut argv = vec![
                    "-C".to_owned(),
                    self.dir.display().to_string(),
                    "--no-pager".to_owned(),
                ];
                argv.extend(args.iter().map(|a| (*a).to_owned()));
                argv
            },
            working_dir: Some(self.dir.clone()),
            env: build_child_env(
                &self.ctx.config.process.env_allowlist,
                &Default::default(),
                &std::env::vars().collect(),
            )
            .0,
            timeout: Some(Duration::from_secs(30)),
            output_cap_bytes: self.ctx.output_cap(),
            stdout_path: Some(stdout_path.clone()),
            stderr_path: Some(artifacts.join(format!("git-{label}-stderr"))),
            terminal: TerminalMode::Pipes,
        };
        // A token of its own: `git status` is not part of the worker's cancellation, and a delegation
        // that was cancelled must still be able to ask the repository what it left behind.
        let token = CancelToken::new()?;
        let outcome = jean_process::supervise(&spec, &token, |_| {})?;
        Ok(GitRun {
            exit_code: outcome.exit_code,
            stdout: std::fs::read(&stdout_path).unwrap_or_default(),
        })
    }

    /// Whether `dir` is inside a git working tree — false, not an error, when it is not.
    fn is_work_tree(&self) -> Result<bool> {
        let run = self.run("probe", &["rev-parse", "--is-inside-work-tree"])?;
        Ok(run.exit_code == Some(0)
            && std::str::from_utf8(&run.stdout)
                .unwrap_or_default()
                .contains("true"))
    }

    /// The tracked and untracked changes in this tree, as repository-relative paths.
    fn dirty(&self, label: &str) -> Result<BTreeSet<String>> {
        if !self.is_work_tree()? {
            return Ok(BTreeSet::new());
        }
        let run = self.run(label, &["status", "--porcelain=v1", "-z"])?;
        if run.exit_code != Some(0) {
            return Err(CoreError::Intent {
                reason: format!(
                    "`git status` in {} exited {:?}, so no file summary is available",
                    self.dir.display(),
                    run.exit_code
                ),
            });
        }
        Ok(parse_porcelain(&run.stdout).into_iter().collect())
    }

    fn add_worktree(&self, target: &Path) -> Result<()> {
        let run = self.run(
            "worktree-add",
            &["worktree", "add", "--detach", &target.display().to_string()],
        )?;
        if run.exit_code == Some(0) {
            return Ok(());
        }
        Err(CoreError::Intent {
            reason: format!(
                "`git worktree add --detach` for {} failed (exit {:?})",
                target.display(),
                run.exit_code
            ),
        })
    }
}

/// The files a directory holds changed, or an empty set when there is no repository to ask.
fn dirty_files(ctx: &Context, dir: &Path, task_id: &str, label: &str) -> Result<BTreeSet<String>> {
    let Some(git) = Git::find(ctx) else {
        return Ok(BTreeSet::new());
    };
    let repo = Repo {
        ctx,
        git: &git,
        dir: dir.to_path_buf(),
        task_id,
    };
    repo.dirty(label)
}

/// `-z` records: each NUL-terminated entry starts with two status columns and a path.
fn parse_porcelain(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .filter_map(|entry| {
            if entry.len() < 4 {
                return None;
            }
            let path = std::str::from_utf8(&entry[3..]).ok()?;
            let path = path.trim_matches(|c| c == ' ' || c == '"').to_owned();
            if path.is_empty() {
                None
            } else {
                Some(path)
            }
        })
        .collect()
}

/// The actor name written into every audit line, as `intent` writes it.
fn actor_identity() -> String {
    for name in ["USERNAME", "USER", "LOGON_USER"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return value;
            }
        }
    }
    "<unknown: no username in the environment>".to_owned()
}

/// The operation the operator approves when they run `jean delegate`.
fn delegation_operation(
    worker: &WorkerConfig,
    workspace: &Path,
    verification: &Verification,
) -> Operation {
    Operation {
        kind: OperationKind::DelegatedAction,
        tool_id: Some(format!("worker:{}", worker.name)),
        tier: PolicyTier::Normal,
        capabilities: vec![
            "delegate-work".to_owned(),
            worker.capability.clone(),
            format!("worker-kind:{}", worker.kind),
        ],
        summary: format!(
            "delegate work to {} ({}, sandbox {}) in {}",
            worker.name,
            worker.kind,
            worker.sandbox,
            workspace.display()
        ),
        // The scope is the durable part: worker, sandbox and workspace. A remembered approval for
        // one directory does not cover another, and the check that will decide the result is named.
        scope: Scope::new(
            workspace.display().to_string(),
            vec![
                worker.name.clone(),
                worker.kind.clone(),
                worker.sandbox.clone(),
                verification.describe(),
            ],
        ),
    }
}

/// The host that answers a worker's questions with the operator's authority.
///
/// ## The two doors, and why only one of them is quiet
///
/// * **Covered** — an edit or a read of a path that resolves inside the workspace the operator
///   pointed at, under the capability the operator gave this worker, in a sandbox that is not
///   `read-only`. Answered from [`Authority::DelegationScope`] with no prompt, because the operator
///   already said yes to exactly this when they ran `jean delegate`. This is what keeps a worker that
///   touches thirty files from asking thirty times.
/// * **Everything else** — a command, a network target, anything unclassifiable, or a path outside
///   the tree. The second door is the ordinary one: [`Policy::decide_granted`] with no run grant, so
///   `auto_deny`, elevation, the ledger and the operator apply exactly as they do to a typed command.
///
/// A worker cannot widen either door. It is never consulted about its own containment: a path
/// outside the workspace is refused *before* the engine runs, so no prompt can talk the build into
/// writing there. And the answer it gets must be one of the options it offered itself — JEAN does
/// not invent a `yes` the agent's protocol would not understand, and never picks `allow_always` when
/// the agent also offered `allow_once`, because extending one approval across a session is the
/// operator's choice to make.
pub struct WorkerAuthority<'a> {
    policy: &'a Policy,
    audit: &'a AuditWriter,
    actor: String,
    store: &'a Store,
    prompter: &'a mut dyn Prompter,
    worker: String,
    capability: String,
    sandbox: String,
    workspace: PathBuf,
    /// When this delegation's session is over, from [`Bounds::session_deadline`].
    session_deadline: Instant,
    /// How long an answer is worth waiting for. Used to refuse an ask that could not be answered
    /// inside what is left of the session, rather than parked on.
    permission_window: Duration,
    /// The authority behind the delegation itself, so the audit trail can name the door a covered
    /// edit came through.
    #[allow(dead_code)]
    granted: Authority,
    pub covered: usize,
    pub asked: usize,
}

impl<'a> WorkerAuthority<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        policy: &'a Policy,
        audit: &'a AuditWriter,
        actor: &str,
        store: &'a Store,
        prompter: &'a mut dyn Prompter,
        worker: &WorkerConfig,
        workspace: PathBuf,
        session_deadline: Instant,
        permission_window: Duration,
        granted: Authority,
    ) -> Self {
        Self {
            policy,
            audit,
            actor: actor.to_owned(),
            store,
            prompter,
            worker: worker.name.clone(),
            capability: worker.capability.clone(),
            sandbox: worker.sandbox.clone(),
            workspace,
            session_deadline,
            permission_window,
            granted,
            covered: 0,
            asked: 0,
        }
    }

    /// The operation an agent's request becomes.
    ///
    /// `RunCommand` keeps the existing kind — an agent asking to run `cargo test` is the same class of
    /// thing as the operator asking, and must be digestible against the same approvals. Its cwd goes
    /// into the scope arguments, so a `cd` is not free. Everything else is `DelegatedAction`, with the
    /// file or target as the scope: a remembered approval for one path never covers another.
    fn operation(&self, request: &PermissionRequest) -> Operation {
        let (kind, target, arguments) = match &request.action {
            WorkerAction::RunCommand { argv, cwd } => {
                let mut arguments: Vec<String> = argv.iter().skip(1).cloned().collect();
                if let Some(dir) = cwd {
                    arguments.push(format!("cwd={dir}"));
                }
                (
                    OperationKind::RunCommand,
                    argv.first().cloned().unwrap_or_default(),
                    arguments,
                )
            }
            WorkerAction::EditFile { path } => (
                OperationKind::DelegatedAction,
                path.clone(),
                vec!["write".to_owned()],
            ),
            WorkerAction::ReadFile { path } => (
                OperationKind::DelegatedAction,
                path.clone(),
                vec!["read".to_owned()],
            ),
            WorkerAction::Network { target } => (
                OperationKind::DelegatedAction,
                target.clone(),
                vec!["network".to_owned()],
            ),
            WorkerAction::Unclassified { text } => (
                OperationKind::DelegatedAction,
                text.clone(),
                vec!["unclassified".to_owned()],
            ),
        };
        Operation {
            kind,
            tool_id: Some(format!("worker:{}", self.worker)),
            tier: PolicyTier::Normal,
            // The worker's own capability first, then the class of action: an operator who gated
            // `agent-command` gets asked about commands and not about reads.
            capabilities: vec![
                self.capability.clone(),
                action_capability(&request.action).to_owned(),
            ],
            summary: format!("{} asked to {}", self.worker, request.action.describe()),
            scope: Scope::new(target, arguments),
        }
    }

    /// Whether this action is inside what the operator's delegation approval covers.
    ///
    /// Two conditions, both required: the worker's own capability, and an action class that only
    /// touches the tree — a path inside the workspace is checked by the caller before this runs. A
    /// command, a network target or anything unclassifiable is never covered here: it reaches the
    /// engine, and the engine reaches the operator. The `read-only` sandbox is not tested here either;
    /// [`decide`](Self::decide) refuses a write outright, because a limit the engine would happily
    /// approve is not a limit.
    fn covered(&self, request: &PermissionRequest, operation: &Operation) -> bool {
        let inside = match request.action.touched_path() {
            Some(path) => resolves_inside(&self.workspace, Path::new(path)),
            None => false,
        };
        if !inside {
            return false;
        }
        let editing = matches!(
            request.action,
            WorkerAction::EditFile { .. } | WorkerAction::ReadFile { .. }
        );
        let capability_matches = operation.capabilities.iter().any(|c| c == &self.capability)
            && !self.policy.is_auto_denied(&self.capability);
        editing && capability_matches
    }
}

/// The action class an ask belongs to, as a capability name.
fn action_capability(action: &WorkerAction) -> &'static str {
    match action {
        WorkerAction::RunCommand { .. } => "agent-command",
        WorkerAction::EditFile { .. } => "agent-edit",
        WorkerAction::ReadFile { .. } => "agent-read",
        WorkerAction::Network { .. } => "agent-network",
        WorkerAction::Unclassified { .. } => "agent-request",
    }
}

impl PermissionHost for WorkerAuthority<'_> {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionAnswer {
        let left = self
            .session_deadline
            .saturating_duration_since(Instant::now());
        if left <= self.permission_window {
            // Asking now would park the worker on an answer the session will not live to use. The
            // worker's own protocol gets `cancelled`, which it can end its turn on, rather than a
            // refusal it might try to argue with.
            let operation = self.operation(request);
            self.audit_decision(
                &operation,
                &Decision::Denied(DenyReason::Unanswerable(format!(
                    "the delegation's session has {} s left and an answer needs {} s",
                    left.as_secs(),
                    self.permission_window.as_secs()
                ))),
            );
            self.asked += 1;
            return PermissionAnswer::Cancelled;
        }

        let operation = self.operation(request);

        // Refuse outside the workspace *before* the engine, so the operator is never prompted about a
        // thing they cannot approve by saying yes: the boundary is the delegation, not the answer.
        if let Some(path) = request.action.touched_path() {
            if !resolves_inside(&self.workspace, Path::new(path)) {
                let denied = Decision::Denied(DenyReason::OutsideScope {
                    detail: format!(
                        "{path} is outside the workspace this delegation covers ({})",
                        self.workspace.display()
                    ),
                });
                self.audit_decision(&operation, &denied);
                self.asked += 1;
                let text = AuditEvent::denial_text(denied.reason().expect("a denial has a reason"));
                return PermissionAnswer::Denied { reason: text };
            }
        }

        // A `read-only` sandbox is a boundary the operator configured, and the engine does not know
        // about it: left to `decide_granted`, a write would be allowed on the worker's plain
        // capability. Refused here, so the setting means what it says.
        if self.sandbox == "read-only" && matches!(request.action, WorkerAction::EditFile { .. }) {
            let denied = Decision::Denied(DenyReason::OutsideScope {
                detail: format!(
                    "{} is configured with sandbox = \"read-only\", which is not a limit the agent \
                     can lift by asking",
                    self.worker
                ),
            });
            self.audit_decision(&operation, &denied);
            self.asked += 1;
            let text = AuditEvent::denial_text(denied.reason().expect("a denial has a reason"));
            return PermissionAnswer::Denied { reason: text };
        }

        if self.covered(request, &operation) {
            let allowed = Decision::allowed(Authority::DelegationScope);
            self.audit_decision(&operation, &allowed);
            self.covered += 1;
            return self.offer_for(request, Authority::DelegationScope);
        }

        let decision = match self
            .policy
            .decide_granted(&operation, self.store, self.prompter, None)
        {
            Ok(decision) => decision,
            Err(e) => {
                let text = format!("the policy engine could not decide: {e}");
                self.audit_decision(
                    &operation,
                    &Decision::Denied(DenyReason::Unanswerable(text.clone())),
                );
                self.asked += 1;
                return PermissionAnswer::Denied { reason: text };
            }
        };
        self.audit_decision(&operation, &decision);
        self.asked += 1;
        match decision {
            Decision::Allowed { authority } => self.offer_for(request, authority),
            denied @ Decision::Denied(_) => PermissionAnswer::Denied {
                reason: AuditEvent::denial_text(denied.reason().expect("a denial has a reason")),
            },
        }
    }
}

impl WorkerAuthority<'_> {
    fn audit_decision(&self, operation: &Operation, decision: &Decision) {
        let event = AuditEvent::for_decision(operation, decision, self.actor.clone())
            .with_working_dir(Some(self.workspace.display().to_string()))
            .with_notes(Some(format!("delegated ask by {}", self.worker)));
        if let Err(e) = self.audit.record(&event) {
            tracing::warn!(error = %e, "a worker's permission request could not be audited");
        }
    }

    /// Turn the engine's verdict into one of the options the worker offered.
    ///
    /// `allow_once` is preferred whenever it exists, including when the engine's authority is a
    /// remembered approval: choosing `allow_always` on an agent's behalf would let the worker extend
    /// one yes across its whole session, and that widening is the operator's choice to make, not
    /// this code's.
    fn offer_for(&self, request: &PermissionRequest, authority: Authority) -> PermissionAnswer {
        let pick = |kind: &str| {
            request
                .offers
                .iter()
                .find(|offer| offer.kind == kind)
                .map(|offer| offer.option_id.clone())
        };
        if let Some(option) = pick("allow_once").or_else(|| pick("allow_always")) {
            tracing::debug!(
                worker = %self.worker,
                authority = authority.as_str(),
                "a worker's request was answered from J.E.A.N.'s decision"
            );
            return PermissionAnswer::Selected { option_id: option };
        }
        PermissionAnswer::Denied {
            reason: format!(
                "J.E.A.N. would allow this ({}), but {} offered no way to accept it",
                authority.as_str(),
                self.worker
            ),
        }
    }
}

/// The path containment test, in the one place both branches of the host use it.
fn resolves_inside(root: &Path, target: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let candidate = if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.join(target)
    };
    // Resolve the deepest existing ancestor so a brand-new file still has a location, and so a
    // symlinked directory is followed before the comparison rather than after.
    let resolved = existing_ancestor(&candidate).unwrap_or(candidate);
    resolved.starts_with(&root)
}

fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        if let Ok(existing) = cursor.canonicalize() {
            let mut out = existing;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return Some(out);
        }
        let name = cursor.file_name()?.to_owned();
        tail.push(name);
        cursor = cursor.parent()?.to_path_buf();
    }
}

/// Stops a delegation's cancellation machinery, whichever way the delegation ended.
///
/// Both halves are process-wide resources with one slot each: the console handler holds a single
/// armed [`CancelToken`] and the pipe listener owns one pipe name. A `?` on the way to the worker —
/// an isolation refusal, a baseline check that could not be recorded — would otherwise leave a
/// signal handler aimed at a task that no longer exists and a name squatted by a listener nobody
/// will stop, which is the kind of bug that only shows up on the *next* command in the same process.
struct Teardown {
    listener: Option<CancelListener>,
    bridge: Option<CancelBridge>,
}

impl Teardown {
    fn new(listener: CancelListener, bridge: CancelBridge) -> Self {
        Self {
            listener: Some(listener),
            bridge: Some(bridge),
        }
    }

    fn stop(&mut self) {
        if let Some(bridge) = self.bridge.take() {
            bridge.stop();
        }
        if let Some(listener) = self.listener.take() {
            listener.stop();
        }
        crate::signals::disarm();
    }
}

impl Drop for Teardown {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The bridge that turns a `CancelToken` into the flag the worker contract waits on.
///
/// The token is what the signal handler and the IPC listener raise; the flag is what
/// [`jean_acp`] polls and what the watchdog reads. Polling at 50 ms is not an elegance problem, it is
/// that the token's event is a Win32 handle this crate does not own a wait on, and a fifty-millisecond
/// latency on an operator's cancel is a smaller cost than a second cancellation mechanism.
struct CancelBridge {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CancelBridge {
    fn start(token: CancelToken, flag: Arc<AtomicBool>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if token.is_cancelled() {
                        flag.store(true, Ordering::Relaxed);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Where a worker's progress goes: the database, the transcript, and a ceiling.
struct StoreSink<'a> {
    store: &'a Store,
    delegation_id: String,
    task_id: String,
    redactor: jean_protocol::Redactor,
    transcript: std::fs::File,
    emitted: usize,
    limit: usize,
    dropped: usize,
}

impl EventSink for StoreSink<'_> {
    /// `false` means the ceiling was reached and the connector must stop reading.
    ///
    /// A worker that emits twenty thousand events is either looping or announcing itself, and a
    /// queue that grows to meet it is the operator's disk. The events already written stay written,
    /// the count of what was refused is reported, and the session is ended rather than truncated in
    /// silence.
    fn emit(&mut self, event: WorkerEvent) -> bool {
        if self.emitted >= self.limit {
            self.dropped += 1;
            return false;
        }
        use std::io::Write as _;
        let detail = self.redactor.redact(&event.detail());
        self.emitted += 1;
        let kind = event.kind();
        let _ = self.store.append_worker_event(
            &self.delegation_id,
            i64::try_from(self.emitted).unwrap_or(i64::MAX),
            kind.as_str(),
            &detail,
        );
        let _ = self
            .store
            .append_event(&self.task_id, &Event::new(kind, detail.clone()));
        let line = format!("[{:>6}] {}: {detail}\n", self.emitted, kind.as_str());
        if self.transcript.write_all(line.as_bytes()).is_err() {
            return false;
        }
        let _ = self.transcript.flush();
        true
    }
}
