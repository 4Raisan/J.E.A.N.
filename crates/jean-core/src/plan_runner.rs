//! Execute a validated plan through the intents that already exist.
//!
//! There is no second process runner here and no second decision path. Every step becomes one
//! [`Intent`] and goes through [`crate::intent::execute`], so the policy engine, the audit writer,
//! the Job Object supervisor and the verifier are the same code Phase 0 and Phase 1 were tested
//! with. A plan step is not a new kind of execution; it is an ordinary intent with a row above it
//! saying which proposal it came from.
//!
//! ## What this module adds
//!
//! * A **parent task** for the operator's request, so `jean tasks` shows one line for "open notepad
//!   and check it" rather than three unrelated ones for its steps. The parent owns the plan rows and
//!   takes the final state; each step still keeps the id of the intent task that executed it.
//! * **Step rows written before the act.** [`Store::start_plan_step`](jean_store::Store) happens
//!   first, because that ordering is what makes a crash answerable: a step in `running` may have had
//!   its effect, and a step still `pending` certainly did not.
//! * **Dependents stopped, not replayed.** A step whose dependency did not run is `skipped`, and
//!   nothing in here re-issues a step left in `running`.
//! * **A summary built from the rows.** Titles are model wording and are labelled as such; verdicts,
//!   exit codes and evidence come from the record.
//!
//! ## Authority
//!
//! The operator's goal authorised this run; the plan did not authorise itself. A step still has to
//! pass the policy engine exactly as the same intent typed at the console would, so a plan of five
//! launches that need approval produces five decisions rather than a blanket consent. Nothing here
//! asks the model whether something is allowed, and the model's own words about permissions are
//! stored as text and never read back as authority.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use jean_model::{ChatRequest, ModelClient, ModelError};
use jean_policy::{GrantScope, Prompter, RunAuthority, RunGrant};
use jean_protocol::plan::{ArgValue, DeclaredVerification, PlanOperation, PlanStep, PlanStepState};
use jean_protocol::{Event, EventKind, Plan, TaskState};
use jean_store::{NewPlan, NewPlanStep, TaskRecord};
use jean_tools::SortKey;

use crate::context::Context;
use crate::error::{CoreError, Result};
use crate::intent::{
    self, CommandRequest, FilesRequest, Intent, IntentReport, LaunchRequest, Payload,
    ProcessRequest,
};
use crate::planner::{self, Planned};
use crate::verify::{self, Verdict};

/// The cancellation flag one plan run understands.
///
/// Wraps the same `AtomicBool` the provider client polls, and is raised from
/// [`crate::signals::interrupt()`] — the console handler Phase 0 already installed for one active
/// task. That is deliberate: a plan is a sequence of single tasks, so the existing handler covers it,
/// and a second signal path or per-step thread would be concurrency the cancellation infrastructure
/// cannot support (D-028).
#[derive(Debug, Clone, Default)]
pub struct CancelFlag {
    raised: Arc<AtomicBool>,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn raise(&self) {
        self.raised.store(true, Ordering::SeqCst);
    }

    pub fn raised(&self) -> bool {
        self.raised.load(Ordering::Relaxed)
    }

    /// The flag a provider call polls.
    pub fn atomic(&self) -> &AtomicBool {
        &self.raised
    }

    /// Adopt a console interrupt if this process has seen one, and report whether the run is now
    /// cancelled.
    ///
    /// Ctrl-C during a plan means "stop starting new steps", which is the same promise a supervised
    /// child already makes. The step in flight is cancelled by the pipeline that owns it, not by
    /// anything here.
    pub fn sync_with_console(&self) -> bool {
        if crate::signals::interrupt().is_some() {
            self.raise();
        }
        self.raised()
    }
}

/// What happened to one step.
#[derive(Debug, Clone)]
pub struct StepOutcome {
    pub step_key: String,
    /// Model wording, labelled as such wherever it is printed.
    pub title: String,
    pub state: PlanStepState,
    /// The tool's declaration and the step's proposal, combined; the stricter answer wins.
    pub verdict: Option<Verdict>,
    /// One sentence of evidence from the records, redacted.
    pub evidence: String,
    /// The intent's own task row, when the step got that far. Exit code and captured output live
    /// there; the plan row links to it instead of copying it.
    pub task_id: Option<String>,
    pub run_id: Option<i64>,
    /// Anything worth saying that is not a verdict: a deadline the registry overrides, a refusal the
    /// policy engine made.
    pub note: Option<String>,
}

impl StepOutcome {
    pub fn is_success(&self) -> bool {
        self.state == PlanStepState::Succeeded
    }

    /// A refusal is visible in the note, and the exit ladder depends on it.
    pub fn was_refused(&self) -> bool {
        self.note
            .as_deref()
            .is_some_and(|note| note.starts_with("refused by policy"))
    }
}

/// The whole run: the plan, its steps, and what became of each.
#[derive(Debug, Clone)]
pub struct PlanReport {
    /// The parent task.
    pub task_id: String,
    pub plan_id: String,
    /// The model's restatement of the goal. The operator's own words are on the parent task row.
    pub goal: String,
    pub provider: String,
    pub model: Option<String>,
    /// Proposals it took to reach a valid plan.
    pub attempts: u32,
    /// Why each earlier proposal was refused, kept so the report can explain the detour.
    pub rejected: Vec<String>,
    pub state: TaskState,
    pub steps: Vec<StepOutcome>,
    /// Who the authority for this run came from: `operator_run` or `prompt_each_step`. Recorded so a
    /// report can say whether a person was asked, and a later reader can tell that the answer to
    /// "why did nothing prompt?" is not "because the model answered it".
    pub run_authority: &'static str,
    /// How many steps the operator's own run covered without asking again. Not a count of runs:
    /// each covered step is one fewer prompt, and the rest were asked about normally.
    pub covered_steps: usize,
    /// Built from the rows, always.
    pub summary: String,
    pub notes: Vec<String>,
    pub elapsed_ms: u64,
}

impl PlanReport {
    /// Exit status, on the ladder Phase 0 and Phase 1 established.
    ///
    /// | Code | Meaning |
    /// |---|---|
    /// | 0 | Every step ran and every declared verification was found |
    /// | 1 | A step failed |
    /// | 3 | Steps ran, but the outcome is not proven |
    /// | 4 | A step's effect cannot be determined and needs a person |
    /// | 5 | A step was refused by policy |
    /// | 130 | Cancelled |
    pub fn exit_status(&self) -> i32 {
        match self.state {
            TaskState::Cancelled => 130,
            TaskState::NeedsAttention => 4,
            TaskState::Succeeded if self.steps.iter().all(StepOutcome::is_success) => 0,
            TaskState::Succeeded => 3,
            _ if self.steps.iter().any(StepOutcome::was_refused) => 5,
            _ => 1,
        }
    }

    pub fn settled(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| step.state.is_settled())
            .count()
    }

    pub fn succeeded(&self) -> usize {
        self.steps.iter().filter(|step| step.is_success()).count()
    }

    /// Lines for the CLI: facts first, one per step, with the evidence that produced them.
    pub fn report_lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "plan {} from {}{} — {} step(s), {} settled",
            self.plan_id,
            self.provider,
            self.model
                .as_deref()
                .map(|m| format!(" model {m}"))
                .unwrap_or_default(),
            self.steps.len(),
            self.settled()
        )];
        for step in &self.steps {
            let verdict = step
                .verdict
                .as_ref()
                .map(|v| v.label())
                .unwrap_or("not run");
            let task = step
                .task_id
                .as_deref()
                .map(|id| format!(" task {id}"))
                .unwrap_or_default();
            out.push(format!(
                "  {} [{}] {verdict}{task} — {}",
                step.step_key, step.state, step.title
            ));
            if !step.evidence.is_empty() {
                out.push(format!("      evidence: {}", step.evidence));
            }
            if let Some(note) = &step.note {
                out.push(format!("      note: {note}"));
            }
        }
        out.push(format!("outcome: {}", self.summary));
        for note in &self.notes {
            out.push(format!("  note: {note}"));
        }
        for rejected in &self.rejected {
            out.push(format!("  earlier proposal: {rejected}"));
        }
        out
    }
}

/// Plan and run, in that order, recording throughout.
///
/// Returns `Ok` for any run that reached a recorded end state, including a failed or cancelled one:
/// the caller needs the record either way. `Err` means the plan never became executable — the
/// provider failed, or every proposal was refused — in which case the parent task is closed out
/// rather than left in `planning` for reconciliation to guess about.
///
/// `authority` is the answer to *who asked for this run*, and only to that. A person typing the goal
/// into `jean do` authorises its validated routine steps once, for this run, at their resolved
/// scope; anything else — a future delegate, a `--prompt-each` invocation — keeps the per-step gate.
/// It changes who is asked, never what may be refused: the plan is validated first, and the two
/// deterministic refusals (`auto_deny`, elevation) are consulted before any grant is.
pub fn run(
    ctx: &mut Context,
    client: &dyn ModelClient,
    goal: &str,
    prompter: &mut dyn Prompter,
    cancel: &CancelFlag,
    progress: &mut dyn FnMut(&str),
    authority: RunAuthority,
) -> Result<PlanReport> {
    let task_id = jean_protocol::ipc::new_task_id();
    run_owned(
        ctx,
        client,
        (&task_id, goal),
        prompter,
        cancel,
        progress,
        authority,
    )
}

pub fn run_owned(
    ctx: &mut Context,
    client: &dyn ModelClient,
    submission: (&str, &str),
    prompter: &mut dyn Prompter,
    cancel: &CancelFlag,
    progress: &mut dyn FnMut(&str),
    authority: RunAuthority,
) -> Result<PlanReport> {
    let (task_id, goal) = submission;
    if !ctx.config.model.planning_enabled() {
        return Err(CoreError::Model(ModelError::ProviderDisabled));
    }
    ctx.prepare_for_writing()?;

    let parent = open_parent_task(ctx, task_id, goal, client)?;
    ctx.store
        .append_event(&parent, &model_requested_event(client, ctx))?;

    let planned = match planner::ask(ctx, client, goal, cancel.atomic()) {
        Ok(planned) => planned,
        Err(error) => {
            close_after_planning_failure(ctx, &parent, &error);
            return Err(error);
        }
    };

    ctx.store
        .append_event(&parent, &model_responded_event(&planned))?;
    ctx.store
        .append_event(&parent, &plan_validated_event(&planned.plan))?;

    let plan_id = new_plan_id();
    record_plan(ctx, &plan_id, &parent, &planned)?;
    ctx.store
        .append_event(&parent, &plan_proposed_event(ctx, &plan_id, &planned))?;
    for note in &planned.notes {
        ctx.store
            .append_event(&parent, &Event::new(EventKind::Note, note.clone()))?;
    }

    ctx.store.transition(
        &parent,
        TaskState::Running,
        Some("the plan was accepted and its steps recorded"),
    )?;

    // The operator's own run covers the steps it was told about, at the scope they resolve to.
    // Minted here — after validation, before any side effect — and dropped when this function
    // returns, so it expires with the run and never reaches the approval ledger.
    let mandate = RunMandate::mint(ctx, &plan_id, &parent, &planned, authority);

    // The plan, shown once, before any of it runs. A display is not a gate: the operator's goal
    // already authorised the run, and this is progress, not a second consent to obtain.
    progress("plan accepted");
    for (index, step) in planned.plan.steps.iter().enumerate() {
        progress(&format!(
            "  {}. {} — {} [{state}]",
            index + 1,
            step.id,
            step.title,
            state = describe(&step.operation)
        ));
    }
    progress(&mandate.describe_line(planned.plan.steps.len()));

    let rows = RunRows {
        plan_id: &plan_id,
        task_id: &parent,
    };
    let report = execute(ctx, &rows, &planned, prompter, cancel, progress, &mandate).inspect_err(
        |error| {
            // The parent row exists and the plan is recorded, so an unexpected failure still has to
            // be closed rather than left `running` for a later process to interpret.
            close_unfinished(ctx, &parent, &error.to_string());
        },
    )?;
    Ok(report)
}

/// Validate and render only. Writes nothing at all — no task, no plan row, no audit line.
///
/// This is `jean plan`: the operator sees the proposal, the bounds it was checked against and what
/// would run, without anything on the machine having been touched.
pub fn preview(
    ctx: &Context,
    client: &dyn ModelClient,
    goal: &str,
    cancel: &CancelFlag,
) -> Result<Planned> {
    if !ctx.config.model.planning_enabled() {
        return Err(CoreError::Model(ModelError::ProviderDisabled));
    }
    planner::ask(ctx, client, goal, cancel.atomic())
}

fn new_plan_id() -> String {
    format!("p-{}", jean_protocol::ipc::new_task_id())
}

fn model_requested_event(client: &dyn ModelClient, ctx: &Context) -> Event {
    Event::new(
        EventKind::ModelRequested,
        format!(
            "provider {} model {:?}; asking for a plan of at most {} steps within {} bytes",
            client.provider_name(),
            client.info().model,
            ctx.config.model.max_plan_steps,
            ctx.config.model.max_prompt_bytes
        ),
    )
}

fn model_responded_event(planned: &Planned) -> Event {
    Event::new(
        EventKind::ModelResponded,
        format!(
            "{} accepted after {} proposal(s) in {} ms{}",
            planned.provider,
            planned.attempts,
            planned.elapsed_ms,
            planned
                .tokens_used
                .map(|n| format!(", {n} tokens reported"))
                .unwrap_or_default()
        ),
    )
}

fn plan_validated_event(plan: &Plan) -> Event {
    Event::new(
        EventKind::PlanValidated,
        format!(
            "{} step(s) passed structure and registry validation: {}",
            plan.steps.len(),
            plan.steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
}

fn plan_proposed_event(ctx: &Context, plan_id: &str, planned: &Planned) -> Event {
    // The goal is free text the model wrote, and a model restates what the operator told it. An
    // audit line is stored, so it goes through the same redactor every other record does.
    Event::new(
        EventKind::PlanProposed,
        format!(
            "{plan_id}: {} step(s), goal {:?}",
            planned.plan.steps.len(),
            // Redacted before it is shortened: cutting first can amputate the end of a secret and
            // leave a fragment that no longer matches any pattern.
            shorten(&ctx.redactor.redact(&planned.plan.goal))
        ),
    )
}

/// The parent row: the operator's own words, which are the instruction, recorded apart from every
/// step title, which is model wording.
fn open_parent_task(
    ctx: &Context,
    task_id: &str,
    goal: &str,
    client: &dyn ModelClient,
) -> Result<String> {
    jean_protocol::ipc::validate_task_id(task_id)?;
    let task_id = task_id.to_owned();
    let redacted_goal = ctx.redactor.redact(goal.trim());
    let request_json = serde_json::to_string(&serde_json::json!({
        // Names and bounds only. No credential and no environment value: `jean task` prints this
        // text, so it has to be safe to read on a shared screen.
        "intent": "plan",
        "provider": client.provider_name(),
        "model": client.info().model,
        "max_plan_steps": ctx.config.model.max_plan_steps,
        "request_timeout_secs": ctx.config.model.request_timeout_secs,
        "workspace_root": ctx.config.workspace.root,
    }))
    .unwrap_or_else(|_| "{}".to_owned());

    let record = TaskRecord::new(
        task_id.clone(),
        redacted_goal,
        // `tasks.tool_id` is NOT NULL, and for a planned run the thing that ran *is* the plan.
        Some("plan".to_owned()),
        request_json,
    );
    ctx.store.insert_task(&record)?;
    // Ownership, through the same helper the supervised runner uses, so reconciliation leaves this
    // row alone while this process lives and adopts it when the process is gone. Without it, the
    // first command step's own `runner::run` would reconcile the live parent as an unowned orphan.
    crate::runner::record_ownership(&ctx.store, &task_id)?;
    ctx.store.transition(
        &task_id,
        TaskState::Planning,
        Some("a plan was requested from the configured provider"),
    )?;
    Ok(task_id)
}

/// Persist the plan and one `pending` row per step.
fn record_plan(ctx: &Context, plan_id: &str, task_id: &str, planned: &Planned) -> Result<()> {
    let steps = planned
        .plan
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| NewPlanStep {
            step_key: step.id.clone(),
            position: index as u32,
            title: ctx.redactor.redact(&step.title),
            operation_kind: step.operation.kind_name().to_owned(),
            tool_id: step.operation.tool_id().map(str::to_owned),
            arguments_json: arguments_json(ctx, step),
            verify_method: step.verify.as_str().to_owned(),
        })
        .collect();

    ctx.store.insert_plan(&NewPlan {
        id: plan_id.to_owned(),
        task_id: task_id.to_owned(),
        schema_version: planned.plan.schema_version,
        goal: ctx.redactor.redact(&planned.plan.goal),
        provider: planned.provider.clone(),
        model: planned.model.clone(),
        steps_json: planned.steps_json(&ctx.redactor),
        attempts: planned.attempts,
        steps,
    })?;
    Ok(())
}

/// The step's argument vector as validated, redacted — never the provider's raw text.
fn arguments_json(ctx: &Context, step: &PlanStep) -> String {
    let rendered: Vec<serde_json::Value> = step
        .operation
        .arguments()
        .iter()
        .filter_map(|argument| argument.to_value().ok())
        .map(|value| {
            serde_json::json!({
                "type": value.type_name(),
                "value": ctx.redactor.redact(&value.render()),
            })
        })
        .collect();
    serde_json::to_string(&rendered).unwrap_or_else(|_| "[]".to_owned())
}

/// The sequence, in declaration order, which is execution order.
///
/// `mandate` is the operator's authority for this run, or nothing when no operator stands behind
/// it. It is handed to each step as a *candidate* answer to the approval question — never as an
/// override of the policy engine, which is the one that decides whether it applies.
fn execute(
    ctx: &mut Context,
    run: &RunRows,
    planned: &Planned,
    prompter: &mut dyn Prompter,
    cancel: &CancelFlag,
    progress: &mut dyn FnMut(&str),
    mandate: &RunMandate,
) -> Result<PlanReport> {
    let RunRows { plan_id, task_id } = *run;
    let started = std::time::Instant::now();
    let plan = &planned.plan;
    let mut outcomes: Vec<StepOutcome> = Vec::with_capacity(plan.steps.len());
    let mut states: BTreeMap<String, PlanStepState> = BTreeMap::new();
    let mut stopped = None::<PlanStepState>;

    for step in &plan.steps {
        if cancel.sync_with_console() {
            stopped = Some(PlanStepState::Cancelled);
        }
        if let Some(reason_for_stop) = stopped {
            // The plan the operator was shown had this many steps, so the record says what became
            // of each instead of quietly shortening it. A cancellation means nothing further
            // starts; an uncertain step means nothing further starts *either*, but the honest word
            // for those rows is `skipped` — they were held back for a person, not called off.
            let (state, reason) = if reason_for_stop == PlanStepState::Cancelled {
                (
                    PlanStepState::Cancelled,
                    "the plan was cancelled before this step ran".to_owned(),
                )
            } else {
                (
                    PlanStepState::Skipped,
                    "an earlier step's outcome is uncertain, so nothing further was started"
                        .to_owned(),
                )
            };
            stop_step(ctx, plan_id, task_id, step, state, &reason)?;
            states.insert(step.id.clone(), state);
            progress(&format!("  {} [{}] {reason}", step.id, state));
            outcomes.push(quiet_outcome(
                step,
                state,
                if state == PlanStepState::Cancelled {
                    "cancelled before it started"
                } else {
                    &reason
                },
            ));
            continue;
        }

        // A dependency that did not run means this step's premise is absent. Skipping is the honest
        // answer; running anyway would execute a step the plan did not ask for.
        if let Some(blocked_by) = unmet_dependency(&step.depends_on, &states) {
            let reason =
                format!("step {blocked_by} did not succeed, so this step's premise is missing");
            stop_step(ctx, plan_id, task_id, step, PlanStepState::Skipped, &reason)?;
            states.insert(step.id.clone(), PlanStepState::Skipped);
            progress(&format!("  {} [skipped] {reason}", step.id));
            outcomes.push(quiet_outcome(step, PlanStepState::Skipped, &reason));
            continue;
        }

        // Written before the act. This row is what makes "no automatic replay of uncertain side
        // effects" answerable after a crash: `running` means it may have happened.
        ctx.store.start_plan_step(plan_id, &step.id)?;
        ctx.store.append_event(
            task_id,
            &Event::new(
                EventKind::StepStarted,
                format!(
                    "{}: {} ({})",
                    step.id,
                    step.title,
                    step.operation.kind_name()
                ),
            ),
        )?;

        let outcome = run_step(ctx, plan_id, task_id, step, prompter, mandate);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                // The step was opened and something went wrong on the way to an answer. Whether a
                // child ran is not knowable from here, so the step is reported as uncertain rather
                // than failed-and-safe-to-retry.
                let reason = ctx.redactor.redact(&error.to_string());
                ctx.store.finish_plan_step(
                    plan_id,
                    &step.id,
                    PlanStepState::NeedsAttention,
                    None,
                    &reason,
                )?;
                progress(&format!("  {} [needs_attention] {reason}", step.id));
                let outcome = quiet_outcome(step, PlanStepState::NeedsAttention, &reason);
                states.insert(step.id.clone(), outcome.state);
                outcomes.push(outcome);
                stopped = Some(PlanStepState::NeedsAttention);
                continue;
            }
        };

        states.insert(step.id.clone(), outcome.state);
        ctx.store.append_event(
            task_id,
            &Event::new(
                EventKind::StepFinished,
                format!(
                    "{}: {} verdict={}",
                    step.id,
                    outcome.state,
                    outcome
                        .verdict
                        .as_ref()
                        .map(|v| v.label())
                        .unwrap_or("none")
                ),
            ),
        )?;

        progress(&format!(
            "  {} [{}] {} — {}",
            step.id,
            outcome.state,
            outcome
                .verdict
                .as_ref()
                .map(|v| v.label())
                .unwrap_or("no verdict"),
            shorten(&outcome.evidence)
        ));

        if matches!(
            outcome.state,
            PlanStepState::Cancelled | PlanStepState::NeedsAttention
        ) {
            stopped = Some(outcome.state);
        }
        // A *failed* step does not stop the run here: it stops the steps that depend on it, through
        // `unmet_dependency` below, and leaves the ones the plan did not connect to it alone. That is
        // what "step failure with dependents stopped" asks of a sequential plan, and it keeps the
        // parent's state honest — a plan whose first probe failed and whose second step was
        // unrelated is reported as failed, not cancelled.
        outcomes.push(outcome);
    }

    Ok(finish(
        ctx,
        run,
        planned,
        outcomes,
        stopped,
        started.elapsed(),
        mandate,
    ))
}

/// Which rows this run owns: the plan it recorded and the parent task it reports to.
///
/// One argument rather than two, because every function that needs one needs the other, and the
/// lint that caps a readable signature is right that seven was already the limit.
#[derive(Debug, Clone, Copy)]
struct RunRows<'a> {
    plan_id: &'a str,
    task_id: &'a str,
}

/// Who authorised this run, and exactly which steps that reaches.
///
/// Built once, from the validated plan, after each step has been resolved the way [`run_step`] will
/// resolve it. The value is owned by [`run`] and dropped when it returns — that is the expiry, and
/// it is a lifetime rather than a timer. Nothing in it is written to the approval ledger, so a later
/// run cannot inherit an answer this run was given, and there is no method here that adds an entry
/// after the fact, so nothing a step prints can widen it.
struct RunMandate {
    authority: RunAuthority,
    grant: Option<RunGrant>,
}

impl RunMandate {
    /// Mint the operator's authority for a validated plan, step by resolved step.
    ///
    /// Only steps that would otherwise have asked a person are entered. That is not a shortcut: it
    /// is what makes the count the operator is shown mean "these are the prompts you are not going
    /// to see", and it keeps the grant away from steps whose refusal does not depend on being asked
    /// (`auto_deny`, elevation), which the policy engine decides before it ever looks here.
    fn mint(
        ctx: &Context,
        plan_id: &str,
        run_id: &str,
        planned: &Planned,
        authority: RunAuthority,
    ) -> Self {
        if !authority.grants() {
            return Self {
                authority,
                grant: None,
            };
        }
        let policy = jean_policy::Policy::new(ctx.config.policy.clone());
        let mut entries = Vec::new();
        for step in &planned.plan.steps {
            // The same two calls `run_step` makes, so the granted scope and the executed scope are
            // one computation. A step that does not resolve — an application name with two matches,
            // arguments that will not form a vector — has no entry, and the honest answer for a step
            // nobody can name is to ask.
            let Some(intent) = build_intent(ctx, step) else {
                continue;
            };
            let Ok(operation) = intent::operation_for(ctx, &intent) else {
                continue;
            };
            if operation.tier == jean_protocol::tool::PolicyTier::Elevated
                || operation
                    .capabilities
                    .iter()
                    .any(|capability| policy.is_auto_denied(capability))
            {
                continue;
            }
            let would_ask = operation
                .capabilities
                .iter()
                .any(|capability| policy.requires_approval(capability));
            if would_ask {
                entries.push((step.id.clone(), operation.scope_digest()));
            }
        }
        Self {
            authority,
            grant: Some(RunGrant::new(run_id, plan_id, entries)),
        }
    }

    /// The candidate answer for one step of one plan, if this run has one at all.
    fn scope_for<'a>(&'a self, plan_id: &'a str, step_key: &'a str) -> Option<GrantScope<'a>> {
        self.grant
            .as_ref()
            .map(|grant| GrantScope::new(grant, plan_id, step_key))
    }

    fn authority(&self) -> RunAuthority {
        self.authority
    }

    /// How many prompts this run is not going to make.
    fn covered(&self) -> usize {
        self.grant
            .as_ref()
            .map_or(0, jean_policy::RunGrant::covered_steps)
    }

    /// The sentence the operator sees before anything runs, naming the authority in play.
    fn describe_line(&self, steps: usize) -> String {
        match &self.grant {
            Some(grant) if grant.covered_steps() > 0 => format!(
                "authority: your own instruction covers {} of {} step(s) for this run only; every \
                 other step is asked about as before",
                grant.covered_steps(),
                steps
            ),
            Some(_) => {
                "authority: your own instruction; no step in this plan needed a prompt".to_owned()
            }
            None => "authority: each step passes the approval gate on its own".to_owned(),
        }
    }
}

/// One step through the intent pipeline.
///
/// The step is an intent, decided by the same engine a typed command goes through. `mandate` is
/// offered to that engine as this step's candidate answer, and whether it applies is the engine's
/// test — a step whose *resolved* scope differs from the one the operator was shown when the
/// mandate was minted is not covered by it, and is asked about like any other operation.
fn run_step(
    ctx: &mut Context,
    plan_id: &str,
    task_id: &str,
    step: &PlanStep,
    prompter: &mut dyn Prompter,
    mandate: &RunMandate,
) -> Result<StepOutcome> {
    let Some(intent) = build_intent(ctx, step) else {
        // An argument whose type did not survive translation. Nothing ran, so nothing is uncertain.
        let reason = "the step's arguments could not be turned into an argument vector";
        stop_step(ctx, plan_id, task_id, step, PlanStepState::Failed, reason)?;
        return Ok(quiet_outcome(step, PlanStepState::Failed, reason));
    };

    let scope = mandate.scope_for(plan_id, &step.id);
    let report = intent::execute_granted(ctx, &intent, prompter, scope)?;
    let (verdict, evidence) = combine_verdicts(ctx, &report, step);
    let state = state_for(&verdict, &report);

    let run_id = last_run_id(ctx, &report.task_id);
    ctx.store
        .finish_plan_step(plan_id, &step.id, state, Some(verdict.label()), &evidence)?;
    if let Some(run_id) = run_id {
        ctx.store.set_plan_step_run(plan_id, &step.id, run_id)?;
    }

    Ok(StepOutcome {
        step_key: step.id.clone(),
        title: step.title.clone(),
        state,
        note: policy_note(&report).or_else(|| timeout_note(ctx, step)),
        verdict: Some(verdict),
        evidence,
        task_id: Some(report.task_id.clone()),
        run_id,
    })
}

/// The intent this step *is*.
///
/// Only the plan's own typed values are read, and each becomes one field of one existing intent.
/// There is no path here that can produce a command string, a working directory or an environment:
/// the registry supplies the executable, the environment allowlist comes from configuration, and the
/// tool's declaration supplies the deadline — exactly as if the operator had typed the intent.
fn build_intent(ctx: &Context, step: &PlanStep) -> Option<Intent> {
    let arguments = |step: &PlanStep| -> Option<Vec<String>> {
        step.operation
            .arguments()
            .iter()
            .map(|argument| {
                let value = argument.to_value().ok()?;
                Some(match value {
                    ArgValue::Text(text) | ArgValue::Path(text) => text,
                    ArgValue::Integer(number) => number.to_string(),
                    ArgValue::Boolean(flag) => flag.to_string(),
                    ArgValue::Pid(pid) => pid.to_string(),
                })
            })
            .collect()
    };

    match &step.operation {
        PlanOperation::RunCommand {
            tool, interactive, ..
        } => Some(Intent::Command(CommandRequest {
            tool_id: tool.clone(),
            args: arguments(step)?,
            goal: Some(format!(
                "{} — {}",
                step.id,
                ctx.redactor.redact(&step.title)
            )),
            interactive: *interactive,
        })),
        PlanOperation::LaunchApp {
            app,
            pick,
            expect_window,
            window_timeout_secs,
            ..
        } => Some(Intent::Launch(LaunchRequest {
            query: app.clone(),
            args: arguments(step)?,
            pick: *pick,
            window_timeout: window_timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(intent::DEFAULT_WINDOW_TIMEOUT),
            expect_window: expect_window.unwrap_or(true),
        })),
        PlanOperation::InspectProcess {
            pid,
            name,
            this_process,
            list,
            sort,
            limit,
            sample_window_ms,
        } => Some(Intent::Process(ProcessRequest {
            pid: *pid,
            name: name.clone(),
            this_process: *this_process,
            list: *list,
            sort: sort.as_deref().and_then(parse_sort),
            limit: *limit,
            sample_window_ms: *sample_window_ms,
        })),
        PlanOperation::LocateFile {
            root,
            pattern,
            max_results,
        } => Some(Intent::Files(FilesRequest {
            root: PathBuf::from(root),
            pattern: pattern.clone(),
            max_results: *max_results,
        })),
    }
}

fn parse_sort(key: &str) -> Option<SortKey> {
    match key {
        "cpu" => Some(SortKey::Cpu),
        "memory" => Some(SortKey::Memory),
        "pid" => Some(SortKey::Pid),
        _ => None,
    }
}

/// The tool's declaration *and* the step's proposal, with the stricter of the two winning.
///
/// A plan cannot weaken a tool: if the registry says "the output must contain X" and the step only
/// claims exit code 0, X is still required. The converse matters equally — a step that promises more
/// than the tool declares is checked for the extra promise, so a plan cannot make an unproven run
/// look proven by verifying less.
fn combine_verdicts(ctx: &Context, report: &IntentReport, step: &PlanStep) -> (Verdict, String) {
    let pipeline = report.verdict.clone();
    let outcome = match &report.payload {
        Payload::Command(run) => Some(&run.outcome),
        _ => None,
    };

    match step.verify.to_verify_spec() {
        Some(spec) if outcome.is_some() => {
            let extra = verify::verify(&spec, outcome.expect("checked above"));
            let worse = worse_of(&pipeline, &extra).clone();
            let detail = ctx.redactor.redact(&format!(
                "{} | as the step declared: {}",
                pipeline.detail(),
                extra.detail()
            ));
            (relabel(&worse, detail.clone()), detail)
        }
        // A step whose evidence is a window, a field or a directory entry has nothing extra to
        // check: the pipeline already looked, and re-deriving it here would be a second verifier.
        _ => {
            let detail = ctx.redactor.redact(pipeline.detail());
            (relabel(&pipeline, detail.clone()), detail)
        }
    }
}

fn worse_of<'a>(a: &'a Verdict, b: &'a Verdict) -> &'a Verdict {
    // `NotProven` outranks everything: evidence was looked for and found absent. `Unverifiable` is
    // next: there was nothing to look at. `Proven` survives only when both say so.
    match (a, b) {
        (Verdict::Proven(_), Verdict::Proven(_)) => a,
        (Verdict::NotProven(_), _) | (_, Verdict::NotProven(_)) => {
            if matches!(a, Verdict::NotProven(_)) {
                a
            } else {
                b
            }
        }
        (Verdict::Unverifiable(_), _) => a,
        (_, Verdict::Unverifiable(_)) => b,
    }
}

fn relabel(verdict: &Verdict, detail: String) -> Verdict {
    match verdict {
        Verdict::Proven(_) => Verdict::Proven(detail),
        Verdict::NotProven(_) => Verdict::NotProven(detail),
        Verdict::Unverifiable(_) => Verdict::Unverifiable(detail),
    }
}

/// The step's state, from its verdict and from how the intent itself ended.
fn state_for(verdict: &Verdict, report: &IntentReport) -> PlanStepState {
    match report.state {
        TaskState::Cancelled => PlanStepState::Cancelled,
        // A task left needing attention is a step whose effect cannot be determined. That is the
        // one reading a later run must not turn into a retry.
        TaskState::NeedsAttention => PlanStepState::NeedsAttention,
        TaskState::Failed => PlanStepState::Failed,
        _ => match verdict {
            Verdict::Proven(_) => PlanStepState::Succeeded,
            Verdict::NotProven(_) => PlanStepState::Failed,
            Verdict::Unverifiable(_) => PlanStepState::Unverified,
        },
    }
}

fn policy_note(report: &IntentReport) -> Option<String> {
    let denial = report.denial()?;
    Some(format!(
        "refused by policy ({}): {} — the plan proposed it, the rule decided it",
        denial.kind(),
        denial
    ))
}

/// A plan may tighten a deadline; the registry is what applies. Saying so is the difference between
/// an expectation and a silent substitution.
fn timeout_note(ctx: &Context, step: &PlanStep) -> Option<String> {
    let requested = step.operation.timeout_secs()?;
    let definition = ctx.tools.get(step.operation.tool_id()?)?;
    let effective = definition
        .timeout_secs
        .unwrap_or(ctx.config.orchestrator.default_timeout_secs);
    if requested == effective {
        return None;
    }
    Some(format!(
        "the step asked for {requested}s; the tool is configured with {effective}s and that is \
         what applies — a plan cannot change a registry deadline"
    ))
}

fn last_run_id(ctx: &Context, task_id: &str) -> Option<i64> {
    let runs = ctx.store.tool_runs(task_id).ok()?;
    runs.last().map(|run| run.id)
}

fn quiet_outcome(step: &PlanStep, state: PlanStepState, note: &str) -> StepOutcome {
    StepOutcome {
        step_key: step.id.clone(),
        title: step.title.clone(),
        state,
        verdict: None,
        evidence: String::new(),
        task_id: None,
        run_id: None,
        note: Some(note.to_owned()),
    }
}

/// Settle a step that never executed, and event it on the parent.
fn stop_step(
    ctx: &Context,
    plan_id: &str,
    task_id: &str,
    step: &PlanStep,
    state: PlanStepState,
    reason: &str,
) -> Result<()> {
    ctx.store.stop_plan_step(plan_id, &step.id, state, reason)?;
    ctx.store.append_event(
        task_id,
        &Event::new(
            if state == PlanStepState::Skipped {
                EventKind::StepSkipped
            } else {
                EventKind::StepFinished
            },
            format!("{}: {state} — {reason}", step.id),
        ),
    )?;
    Ok(())
}

/// A dependency that is absent, failed, skipped or cancelled.
///
/// `Unverified` counts as met: the step ran, and this one's premise was that it *ran*. Whether the
/// goal was proven is a different question, reported on the step that answered it.
fn unmet_dependency(
    dependencies: &[String],
    states: &BTreeMap<String, PlanStepState>,
) -> Option<String> {
    dependencies
        .iter()
        .find_map(|needed| match states.get(needed) {
            Some(PlanStepState::Succeeded) | Some(PlanStepState::Unverified) => None,
            Some(state) => Some(format!("{needed} ({state})")),
            None => Some(format!("{needed} (did not run)")),
        })
}

/// Decide the parent's state from the step rows, write it, and build the report.
fn finish(
    ctx: &Context,
    run: &RunRows,
    planned: &Planned,
    outcomes: Vec<StepOutcome>,
    stopped: Option<PlanStepState>,
    elapsed: std::time::Duration,
    mandate: &RunMandate,
) -> PlanReport {
    let RunRows { plan_id, task_id } = *run;
    let state = parent_state(&outcomes, stopped, planned.plan.steps.len());
    let summary = summarize(&outcomes, state);

    // Running -> Verifying -> final. The verification stage is real: the verdicts above were produced
    // before this transition, not after it.
    if let Err(e) = ctx.store.transition(
        task_id,
        TaskState::Verifying,
        Some("every recorded step had an outcome"),
    ) {
        tracing::warn!(error = %e, "could not move the plan task to verifying");
    }
    if let Err(e) = ctx.store.transition(task_id, state, Some(&summary)) {
        tracing::warn!(error = %e, "could not close the plan task");
    }
    let _ = ctx.store.set_outcome(task_id, state.as_str(), &summary);
    let _ = ctx.store.clear_runner(task_id);
    let _ = ctx.store.append_event(
        task_id,
        &Event::new(
            EventKind::Note,
            format!(
                "plan {plan_id} finished as {state}: {} of {} step(s) proven",
                outcomes.iter().filter(|o| o.is_success()).count(),
                outcomes.len()
            ),
        ),
    );

    PlanReport {
        task_id: task_id.to_owned(),
        plan_id: plan_id.to_owned(),
        goal: planned.plan.goal.clone(),
        provider: planned.provider.clone(),
        model: planned.model.clone(),
        attempts: planned.attempts,
        rejected: planned.rejected.clone(),
        state,
        steps: outcomes,
        run_authority: mandate.authority().as_str(),
        covered_steps: mandate.covered(),
        summary,
        notes: planned.notes.clone(),
        elapsed_ms: elapsed.as_millis() as u64,
    }
}

fn parent_state(
    outcomes: &[StepOutcome],
    stopped: Option<PlanStepState>,
    planned_steps: usize,
) -> TaskState {
    if stopped == Some(PlanStepState::Cancelled)
        || outcomes.iter().any(|o| o.state == PlanStepState::Cancelled)
    {
        return TaskState::Cancelled;
    }
    if outcomes
        .iter()
        .any(|o| o.state == PlanStepState::NeedsAttention)
    {
        return TaskState::NeedsAttention;
    }
    if outcomes.iter().any(|o| o.state == PlanStepState::Failed) {
        return TaskState::Failed;
    }
    // A plan that recorded fewer settled steps than it had was not completed, whatever it says.
    if outcomes.len() == planned_steps && outcomes.iter().all(|o| o.state.is_settled()) {
        return TaskState::Succeeded;
    }
    TaskState::Failed
}

/// Built from what happened. Nothing here is the model's claim about what happened.
fn summarize(outcomes: &[StepOutcome], state: TaskState) -> String {
    let count = |wanted: PlanStepState| outcomes.iter().filter(|o| o.state == wanted).count();
    let mut parts = vec![format!(
        "{} of {} step(s) proven",
        count(PlanStepState::Succeeded),
        outcomes.len()
    )];
    for (label, state_name) in [
        ("failed", PlanStepState::Failed),
        ("unverified", PlanStepState::Unverified),
        ("skipped", PlanStepState::Skipped),
        ("cancelled", PlanStepState::Cancelled),
        ("uncertain", PlanStepState::NeedsAttention),
    ] {
        let n = count(state_name);
        if n > 0 {
            parts.push(format!("{n} {label}"));
        }
    }
    format!("{state}: {}", parts.join(", "))
}

/// A refused or unobtainable plan is a reported outcome, not a crash: `Failed` for a refusal or a
/// provider fault, `Cancelled` for an operator who stopped it.
fn refusal_state(error: &CoreError) -> Option<TaskState> {
    match error {
        CoreError::PlanRejected { .. } => Some(TaskState::Failed),
        CoreError::Model(ModelError::Cancelled) => Some(TaskState::Cancelled),
        CoreError::Model(_) => Some(TaskState::Failed),
        _ => None,
    }
}

fn close_after_planning_failure(ctx: &Context, task_id: &str, error: &CoreError) {
    let Some(state) = refusal_state(error) else {
        return;
    };
    let reason = ctx.redactor.redact(&error.to_string());
    let kind = if state == TaskState::Cancelled {
        EventKind::CancelRequested
    } else if matches!(error, CoreError::PlanRejected { .. }) {
        EventKind::PlanRejected
    } else {
        EventKind::SupervisorError
    };
    if ctx.store.transition(task_id, state, Some(&reason)).is_ok() {
        let _ = ctx.store.set_outcome(task_id, state.as_str(), &reason);
        let _ = ctx.store.clear_runner(task_id);
        let _ = ctx.store.append_event(task_id, &Event::new(kind, &reason));
    }
}

fn close_unfinished(ctx: &Context, task_id: &str, reason: &str) {
    if ctx
        .store
        .transition(task_id, TaskState::Failed, Some(reason))
        .is_ok()
    {
        let _ = ctx.store.set_outcome(task_id, "failed", reason);
        let _ = ctx.store.clear_runner(task_id);
    }
}

/// Ask the provider to phrase the summary it was given. Its wording decorates the facts; it cannot
/// change them.
///
/// Only reachable with `model.summary_wording = "model"`. Any failure returns `None` and the
/// deterministic summary stands alone, which is the same answer as with no provider at all — so this
/// path cannot be the difference between "the plan worked" and "the plan did not".
pub fn wording(ctx: &Context, client: &dyn ModelClient, report: &PlanReport) -> Option<String> {
    if ctx.config.model.summary_wording != "model" {
        return None;
    }
    let request = ChatRequest {
        system:
            "Restate these recorded facts in one or two plain sentences. Do not add an outcome \
                 that is not in them, do not soften a failure, and do not describe anything as \
                 verified that is recorded as unverified. You have no authority over this task."
                .to_owned(),
        user: format!(
            "Recorded: {}\nSteps:\n{}",
            report.summary,
            report
                .steps
                .iter()
                .map(|step| format!(
                    "- {}: {} verdict={}",
                    step.step_key,
                    step.state,
                    step.verdict.as_ref().map(|v| v.label()).unwrap_or("none")
                ))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        prefer_json: false,
    };
    let cancel = AtomicBool::new(false);
    let reply = client.generate(&request, &cancel).ok()?;
    if reply.truncated() {
        return None;
    }
    let text = reply.content.trim().to_owned();
    if text.is_empty() {
        return None;
    }
    Some(ctx.redactor.redact(&text))
}

fn describe(operation: &PlanOperation) -> String {
    match operation {
        PlanOperation::RunCommand { tool, .. } => format!("run registered tool `{tool}`"),
        PlanOperation::LaunchApp { app, .. } => format!("launch application `{app}`"),
        PlanOperation::InspectProcess { .. } => "read the process table".to_owned(),
        PlanOperation::LocateFile { root, pattern, .. } => {
            format!("find `{pattern}` under `{root}`")
        }
    }
}

fn describe_verification(verify: &DeclaredVerification) -> String {
    match verify {
        DeclaredVerification::ExitCode { expected } => format!("exit code {expected}"),
        DeclaredVerification::OutputContains { expected, needle } => {
            format!("exit code {expected} and output containing {needle:?}")
        }
        DeclaredVerification::WindowForPid => "a window owned by the started pid".to_owned(),
        DeclaredVerification::FieldsAvailable => "the reported fields are present".to_owned(),
        DeclaredVerification::PathReadable => "the path opened for reading".to_owned(),
    }
}

/// The plan section of `jean task`, as lines of text, read back from the rows.
///
/// This lives beside the executor rather than in the CLI so that what an operator reads about an
/// interrupted plan is covered by the same tests as the rows it is built from. `clock` formats a
/// millisecond stamp; only the CLI knows how it wants times to look.
///
/// A Phase-0 or Phase-1 task has no plan rows and produces no lines, so the record an operator
/// read before Phase 2 is exactly the record they read after.
pub fn plan_lines(
    ctx: &Context,
    task_id: &str,
    clock: &dyn Fn(i64) -> String,
) -> Result<Vec<String>> {
    let Some(plan) = ctx.store.current_plan_for_task(task_id)? else {
        return Ok(Vec::new());
    };
    let steps = ctx.store.plan_steps(&plan.id)?;
    let superseded = ctx
        .store
        .plans_for_task(task_id)?
        .into_iter()
        .filter(|record| record.superseded_at_ms.is_some())
        .count();

    let mut out = vec![
        format!("plan        {}", plan.id),
        format!(
            "      provider     {}{}",
            plan.provider,
            plan.model
                .as_deref()
                .map(|model| format!(" model {model}"))
                .unwrap_or_default()
        ),
        format!("      schema       v{}", plan.schema_version),
        format!("      proposed     {}", clock(plan.created_at_ms)),
        format!("      plan goal    {}", plan.goal),
    ];
    if plan.attempts > 1 {
        out.push(format!(
            "      proposals    {} to reach the plan that ran",
            plan.attempts
        ));
    }
    if superseded > 0 {
        out.push(format!(
            "      superseded   {superseded} earlier plan(s) for this task, kept in the database"
        ));
    }

    out.push("".to_owned());
    out.push("plan steps".to_owned());
    for step in &steps {
        out.push(format!(
            "  {} [{}] {} — {}",
            step.step_key, step.state, step.title, step.operation_kind
        ));
        out.push(format!(
            "      tool         {}",
            step.tool_id.as_deref().unwrap_or("(none: a local probe)")
        ));
        out.push(format!("      verify       {}", step.verify_method));
        out.push(format!("      arguments    {}", step.arguments_json));
        if let Some(run_id) = step.tool_run_id {
            out.push(format!("      run          #{run_id}"));
        }
        if let Some(verdict) = &step.verdict {
            out.push(format!("      verdict      {verdict}"));
        }
        if let Some(evidence) = &step.evidence {
            out.push(format!("      evidence     {evidence}"));
        }
        if step.state == PlanStepState::Running {
            // The one line that matters most: an effect that may already have happened is named,
            // not retried.
            out.push(
                "      UNCERTAIN    recorded as running when this task was lost. Its effect may \
                 already have happened; JEAN will not replay it to find out."
                    .to_owned(),
            );
        }
    }
    Ok(out)
}

/// What a validated plan looks like before anything runs.
pub fn render_preview(planned: &Planned) -> Vec<String> {
    let mut out = vec![format!(
        "goal (restated by the model): {}",
        planned.plan.goal
    )];
    out.push(format!(
        "provider {}{} — {} earlier proposal(s) refused",
        planned.provider,
        planned
            .model
            .as_deref()
            .map(|m| format!(" model {m}"))
            .unwrap_or_default(),
        planned.attempts.saturating_sub(1)
    ));
    for (index, step) in planned.plan.steps.iter().enumerate() {
        out.push(format!("  {}. {} — {}", index + 1, step.id, step.title));
        out.push(format!("     does: {}", describe(&step.operation)));
        out.push(format!(
            "     proves: {}",
            describe_verification(&step.verify)
        ));
        if !step.depends_on.is_empty() {
            out.push(format!("     needs: {}", step.depends_on.join(", ")));
        }
    }
    out.push("steps run in this order, one at a time; nothing here has been executed".to_owned());
    for note in &planned.notes {
        out.push(format!("  note: {note}"));
    }
    for rejected in &planned.rejected {
        out.push(format!("  earlier proposal: {rejected}"));
    }
    out
}

fn shorten(text: &str) -> String {
    let mut out: String = text.chars().take(120).collect();
    if text.chars().count() > 120 {
        out.push('…');
    }
    out
}
