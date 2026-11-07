//! The four deterministic intents: what the operator typed, decided, run, and proved.
//!
//! ## Why these four, and why nothing else
//!
//! `PLANNING/05-ROADMAP.md` sets Phase 1's goal as "complete genuinely useful tasks with
//! **zero AI**". That is a constraint on the *input*, not a limitation of the plumbing: an
//! intent here is a typed command with typed arguments, resolved by code, and there is no
//! path from a sentence to a launch. The CLI's grammar is the understanding. When a
//! command is wrong, the failure is a parse error or a refusal, never a guess about what
//! the operator meant — a guess is the thing that later gets described as "the AI decided
//! to open that".
//!
//! | Intent | CLI | Acts? | Verified by |
//! |---|---|---|---|
//! | launch an application | `jean launch <app>` | yes — detached, on purpose | a window the operator can see belongs to that pid |
//! | run a registered command | `jean run <tool>` | yes — supervised in a Job Object | the tool's declared `verify` spec (Phase 0) |
//! | inspect a process | `jean ps` | no | the pid is alive and the fields it reported are real |
//! | locate a file | `jean find <root> <pattern>` | no | the reported path still exists, and is readable |
//!
//! ## Where each decision is made
//!
//! **Ambiguity is refused, never resolved.** `jean launch calc` on a machine with three
//! matches prints the three, with their provenance, and launches none. "First in the list"
//! is an enumeration order nobody agreed to, and making it a security property is how a
//! typo becomes a different program. `--pick N` is the operator choosing, which is a
//! decision with an author.
//!
//! **Approval happens before the act, and every attempt is audited — including the ones
//! that never ran.** [`jean_policy`] answers the question; this module asks it and writes
//! the answer down. A denial is a normal, complete result with a reason, and the audit
//! line for it is the interesting one, because that is the record that says the refusal
//! worked.
//!
//! **Completion and proof stay separate**, exactly as in Phase 0. `CreateProcess`
//! returning is not "the application opened". A child exiting 0 is not "the goal was met".
//! Every intent therefore ends in a [`Verdict`], and the CLI's exit status distinguishes
//! "ran and proved" (0) from "ran and could not prove" (3).
//!
//! ## The one deliberate asymmetry: `launch` is not supervised
//!
//! Everything else that starts a process puts it in a Job Object with
//! `KILL_ON_JOB_CLOSE`, because a command whose supervisor dies must not leak a tree. An
//! *application* is the opposite request: opening the calculator and having it vanish when
//! `jean` exits would be a broken feature, not a safety win. So `jean launch` uses
//! [`jean_process::launch_detached`], owns nothing after the spawn, and compensates by
//! verifying rather than by controlling — it can prove the window is there, and it never
//! claims it can close it.
//!
//! Single-instance applications (`PLANNING/01-SCOPE.md` calls this out specifically) are
//! the case that makes the verification honest rather than decorative: re-launching them
//! hands back a pid that exits within milliseconds while an already-running process's
//! window takes focus. "No window for your pid" and "your process is already gone" are
//! different facts, and collapsing them produces a wrong diagnosis every time. See
//! [`LaunchEvidence::HandedOff`] — that outcome reports the existing instance instead of
//! claiming the launched one opened anything.
//!
//! ## Task records
//!
//! `jean run` keeps Phase 0's lifecycle exactly as demonstrated (`jean_core::run` owns the
//! task row), so the ten Phase-0 behaviors stay covered by the same code path. The other
//! three intents drive the Phase-1 states themselves: `Queued → Planning →
//! (AwaitingApproval) → Running → Verifying → final`, with an approval transition that
//! happens only when the engine is actually about to ask somebody. The command intent's
//! approval is audited and then handed to `run`; its task row therefore begins at
//! `Queued`. That is a known, chosen shape rather than an oversight — duplicating `run`'s
//! lifecycle in order to insert one transition would fork the one code path Phase 0 was
//! demonstrated on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jean_policy::{
    AuditEvent, AuditWriter, Decision, DenyReason, GrantScope, Operation, OperationKind, Policy,
    Prompter, Scope,
};
use jean_process::{LaunchSpec, TerminalMode};
use jean_protocol::tool::PolicyTier;
use jean_protocol::{Event, TaskState, Termination};
use jean_store::{TaskRecord, ToolHealthRecord};
use jean_tools::{
    desktop, inspect_by_name, inspect_pid, inspect_self, list_processes, search_names,
    DiscoveryOutcome, InspectOutcome, NamePattern, ProcessReport, SearchLimits, SearchReport,
    SortKey,
};

use crate::context::Context;
use crate::env::build_child_env;
use crate::error::{CoreError, Result};
use crate::runner::{run, RunReport};
use crate::verify::Verdict;

/// How long to wait for a launched application to paint a window.
///
/// Five seconds, not thirty: a desktop application that has not shown anything after
/// five seconds is either doing something the operator should see reported (an installer,
/// a first-run dialog, a crash) or is not going to. A longer budget would make `jean
/// launch` look hung in exactly the case worth talking about, and the caller can raise it
/// with `--window-timeout`.
pub const DEFAULT_WINDOW_TIMEOUT: Duration = Duration::from_secs(5);

/// Floor for the window timeout.
///
/// A zero-second budget is a question that cannot be answered, and reporting "no window"
/// from it would be the failure mode this module exists to avoid. One second is enough
/// for a `cmd.exe`-class application and says loudly that a heavier one was cut off.
pub const MIN_WINDOW_TIMEOUT: Duration = Duration::from_secs(1);

/// The pause between window lookups while waiting for a launch.
///
/// Handed to [`desktop::wait_for_window`], which clamps it anyway; naming it here keeps
/// the intent's own cadence visible at the call site instead of buried in the probe.
pub const WINDOW_POLL: Duration = Duration::from_millis(100);

/// Capability strings the intents declare.
///
/// They are capabilities, not operation names: `policy.require_approval` and
/// `policy.auto_deny` match on what an act *does*, so an operator can decide that opening
/// a program needs a yes while reading a process table does not, without learning four
/// command names. Two of these are deliberately not in any shipped default list — they
/// exist so a configuration *can* name them.
pub mod capabilities {
    /// Start a program that keeps running after J.E.A.N. exits.
    pub const LAUNCH_APPLICATION: &str = "launch_application";
    /// Read the process table and per-process performance counters.
    pub const READ_PROCESS_INFORMATION: &str = "read_process_information";
    /// Walk directory entries under one root.
    pub const READ_FILESYSTEM: &str = "read_filesystem";
    /// The search root reaches outside the configured workspace root.
    pub const SEARCH_OUTSIDE_WORKSPACE: &str = "search_outside_workspace";
}

// ─────────────────────────────────────────────────────────────────────────────
// Requests
// ─────────────────────────────────────────────────────────────────────────────

/// Launch a discovered application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    /// The name as typed. Resolved through `tools.discovery`, never run directly:
    /// an unresolvable name produces candidates-and-refusal, not a `PATH` guess.
    pub query: String,
    /// Arguments for the application, kept as a vector.
    pub args: Vec<String>,
    /// Which ambiguous candidate to use, 1-based, after the operator has seen the list.
    pub pick: Option<usize>,
    pub window_timeout: Duration,
    /// Skip the window check for an application that legitimately has no window
    /// (a background helper, a `--version` invocation that prints and exits).
    ///
    /// Opt-in, because a launch with no verification is an unproven launch by
    /// definition: the verdict becomes `Unverifiable` and says so rather than defaulting
    /// to success.
    pub expect_window: bool,
}

impl Default for LaunchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            args: Vec::new(),
            pick: None,
            window_timeout: DEFAULT_WINDOW_TIMEOUT,
            expect_window: true,
        }
    }
}

/// Run a registered command through the Phase-0 supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    pub tool_id: String,
    pub args: Vec<String>,
    pub goal: Option<String>,
    /// Force or forbid the pseudo console. `None` follows the tool's declaration.
    pub interactive: Option<bool>,
}

/// Inspect running processes. Terminates nothing — see [`jean_tools::process`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessRequest {
    /// One process by id.
    pub pid: Option<u32>,
    /// One or more processes by executable name.
    pub name: Option<String>,
    /// Inspect this `jean` process, which is how the sampling semantics get
    /// demonstrated without depending on what else is running.
    pub this_process: bool,
    /// No target: a bounded top-N listing.
    pub list: bool,
    pub sort: Option<SortKey>,
    pub limit: Option<usize>,
    pub sample_window_ms: Option<u64>,
}

/// Locate files by name under one explicit root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesRequest {
    pub root: PathBuf,
    pub pattern: String,
    /// Cut the reported hits further than `search.max_results`; never raise it.
    pub max_results: Option<usize>,
}

impl Default for FilesRequest {
    fn default() -> Self {
        Self {
            root: PathBuf::new(),
            pattern: String::new(),
            max_results: None,
        }
    }
}

/// One Phase-1 intent, already parsed by the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Launch(LaunchRequest),
    Command(CommandRequest),
    Process(ProcessRequest),
    Files(FilesRequest),
}

impl Intent {
    pub fn kind(&self) -> OperationKind {
        match self {
            Intent::Launch(_) => OperationKind::LaunchApp,
            Intent::Command(_) => OperationKind::RunCommand,
            Intent::Process(_) => OperationKind::InspectProcess,
            Intent::Files(_) => OperationKind::LocateFile,
        }
    }

    pub fn as_str(&self) -> &'static str {
        self.kind().as_str()
    }

    /// The one-line summary the operator is shown if a prompt is needed, and what the
    /// task record keeps as its goal.
    ///
    /// Built from the *resolved* values, not the raw text, so the words a person
    /// approves and the arguments that run cannot drift apart.
    fn summary(&self) -> String {
        match self {
            Intent::Launch(r) => format!("launch application `{}`", r.query.trim()),
            Intent::Command(r) => format!("run registered tool `{}`", r.tool_id),
            Intent::Process(r) => match (&r.pid, &r.name, r.this_process, r.list) {
                (Some(pid), _, _, _) => format!("inspect process {pid}"),
                (_, Some(name), _, _) => format!("inspect processes named `{name}`"),
                (_, _, true, _) => format!("inspect this process ({})", std::process::id()),
                _ => "list the busiest processes".to_owned(),
            },
            Intent::Files(r) => format!("find `{}` under `{}`", r.pattern, r.root.display()),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Reports
// ─────────────────────────────────────────────────────────────────────────────

/// What a launch produced, as far as it is known.
#[derive(Debug, Clone)]
pub enum LaunchEvidence {
    /// A window the operator can see belongs to the pid that was started.
    WindowOpened {
        pid: u32,
        created_ms: Option<i64>,
        windows: desktop::WindowReport,
        waited_ms: u64,
    },
    /// The started process is gone and another process of the same executable owns
    /// windows — the single-instance handoff.
    ///
    /// `windows` is the *existing* instance's report. It is not proof that the request
    /// did something: the handoff is why an existing window gained focus, most likely,
    /// and "most likely" is why the verdict for this case is not `Proven`.
    HandedOff {
        pid: u32,
        existing: Vec<ProcessReport>,
        windows: Vec<desktop::WindowReport>,
        note: String,
    },
    /// The process is alive and no window appeared within the budget.
    NoWindowInTime {
        pid: u32,
        created_ms: Option<i64>,
        waited_ms: u64,
        reason: String,
    },
    /// The process exited and nothing else with the same name owns a window.
    ExitedWithoutWindow { pid: u32, exit_claim: String },
    /// The window check was declined by configuration or by `--no-window`.
    NotChecked(String),
    /// Nothing was started: ambiguous, unresolvable, or refused.
    Refused {
        candidates: Box<DiscoveryOutcome>,
        reason: String,
    },
}

/// What an intent produced, alongside its verdict.
#[derive(Debug, Clone)]
pub enum Payload {
    Launched(LaunchEvidence),
    /// The command intent delegates to Phase 0's pipeline and keeps its report.
    Command(Box<RunReport>),
    Processes {
        machine: Option<jean_tools::MachineSummary>,
        rows: Vec<ProcessReport>,
        truncated: bool,
        sample_window_ms: u64,
        gone: Option<u32>,
    },
    Files(SearchReport),
    /// Denied, or refused before anything could run.
    Nothing,
}

/// The complete result of one intent.
#[derive(Debug, Clone)]
pub struct IntentReport {
    pub task_id: String,
    pub intent: OperationKind,
    pub decision: Decision,
    pub state: TaskState,
    pub verdict: Verdict,
    pub payload: Payload,
    pub notes: Vec<String>,
}

impl IntentReport {
    /// True only when the act completed *and* its declared evidence was found.
    pub fn is_success(&self) -> bool {
        self.state == TaskState::Succeeded && self.verdict.is_proven()
    }

    /// A process exit status for scripts.
    ///
    /// Phase 0's codes are unchanged, with two additions that Phase 1 needs:
    ///
    /// | Code | Meaning |
    /// |---|---|
    /// | 0 | Ran, and the declared evidence was found |
    /// | 3 | Ran, and nothing proves the requested outcome |
    /// | 4 | Needs a human to look at it |
    /// | 5 | Refused by policy — by configuration, by the operator, or by a timeout |
    /// | 130 | Cancelled |
    /// | 1 | Anything else |
    ///
    /// 5 is separate from 1 on purpose. "The command failed" and "J.E.A.N. would not run
    /// it" look identical in a log and need opposite fixes.
    pub fn exit_status(&self) -> i32 {
        if matches!(self.decision, Decision::Denied(_)) {
            return 5;
        }
        match self.state {
            TaskState::Succeeded if self.verdict.is_proven() => 0,
            TaskState::Succeeded => 3,
            TaskState::Cancelled => 130,
            TaskState::NeedsAttention => 4,
            _ => 1,
        }
    }

    /// The denial, if there was one.
    pub fn denial(&self) -> Option<&DenyReason> {
        self.decision.reason()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Execution
// ─────────────────────────────────────────────────────────────────────────────

/// Decide, act, verify, record.
///
/// `prompter` is a parameter rather than a stdin read so that the tray UI of a later
/// phase can answer the same question, and so a test can hand back a scripted answer
/// without owning a console.
pub fn execute(
    ctx: &mut Context,
    intent: &Intent,
    prompter: &mut dyn Prompter,
) -> Result<IntentReport> {
    execute_granted(ctx, intent, prompter, None)
}

/// [`execute`], with the run grant that may already cover exactly this operation.
///
/// Only the plan runner passes a scope: a typed intent is not a step of any plan, and a grant is
/// the operator's authority for a run, never something this function can invent. What the grant can
/// change is whether a person is asked, and nothing it covers is outside the two refusals
/// [`jean_policy::Policy`] makes before it looks at one.
pub fn execute_granted(
    ctx: &mut Context,
    intent: &Intent,
    prompter: &mut dyn Prompter,
    scope: Option<GrantScope<'_>>,
) -> Result<IntentReport> {
    ctx.prepare_for_writing()?;
    let policy = Policy::new(ctx.config.policy.clone());
    let audit = AuditWriter::new(
        ctx.paths.audit.clone(),
        ctx.config.policy.audit_enabled,
        ctx.redactor.clone(),
    );
    let actor = actor();

    // The operation is built *after* resolution, so a remembered approval is bound to
    // the executable, root or pid that will actually be used.
    let (operation, resolved) = prepare(ctx, intent)?;
    let enforcement = Enforcement {
        policy: &policy,
        audit: &audit,
        actor,
        grant: scope,
    };
    let report = match &resolved {
        Resolved::Command { request, .. } => {
            decide_and_run_command(ctx, intent, request, &operation, &enforcement, prompter)
        }
        Resolved::Launch { .. } | Resolved::Process { .. } | Resolved::Files { .. } => {
            let task_id = open_task(ctx, intent, &operation)?;
            // The row exists from `open_task` onward, so a failure here still has to be
            // closed out rather than left in `running` for reconciliation to find.
            decide_and_act(
                ctx,
                &task_id,
                intent,
                &operation,
                &resolved,
                &enforcement,
                prompter,
            )
            .inspect_err(|e| close_as_failed(ctx, &task_id, &e.to_string()))
        }
    }?;

    Ok(report)
}

/// Everything an intent needs before it can be decided or run.
enum Resolved {
    Launch {
        candidate: jean_tools::AppCandidate,
        request: LaunchRequest,
    },
    Command {
        request: CommandRequest,
    },
    Process(ProcessRequest),
    Files {
        request: FilesRequest,
        canonical_root: PathBuf,
    },
}

/// Validate and resolve the operator's request into the target the approval is about.
///
/// No process is started and no record is written here, so a refusal at this stage —
/// an ambiguous application name, a root that does not exist — leaves the database
/// untouched, which is what makes "jean find /nonexistent" a cheap command.
fn prepare(ctx: &Context, intent: &Intent) -> Result<(Operation, Resolved)> {
    match intent {
        Intent::Launch(request) => prepare_launch(ctx, request).map(|(op, r)| {
            (
                op,
                Resolved::Launch {
                    candidate: r,
                    request: request.clone(),
                },
            )
        }),
        Intent::Command(request) => prepare_command(ctx, request),
        Intent::Process(request) => {
            let target = process_target(request);
            let operation = Operation {
                kind: intent.kind(),
                tool_id: None,
                tier: PolicyTier::ReadOnly,
                capabilities: vec![capabilities::READ_PROCESS_INFORMATION.to_owned()],
                summary: ctx.redactor.redact(&intent.summary()),
                scope: Scope::only(target),
            };
            Ok((operation, Resolved::Process(request.clone())))
        }
        Intent::Files(request) => {
            let canonical_root = canonical_root(&request.root)?;
            let mut capabilities = vec![capabilities::READ_FILESYSTEM.to_owned()];
            if let Some(root) = workspace_root(ctx) {
                if !canonical_root.starts_with(&root) {
                    capabilities.push(capabilities::SEARCH_OUTSIDE_WORKSPACE.to_owned());
                }
            }
            let operation = Operation {
                kind: intent.kind(),
                tool_id: None,
                tier: PolicyTier::ReadOnly,
                capabilities,
                summary: ctx.redactor.redact(&intent.summary()),
                scope: Scope::new(
                    canonical_root.display().to_string(),
                    vec![request.pattern.clone()],
                ),
            };
            Ok((
                operation,
                Resolved::Files {
                    request: request.clone(),
                    canonical_root,
                },
            ))
        }
    }
}

fn prepare_command(ctx: &Context, request: &CommandRequest) -> Result<(Operation, Resolved)> {
    // The same lookup Phase 0 uses, so a disabled tool is refused identically in both
    // entry points rather than having two definitions of "unknown".
    let tool = crate::runner::lookup_tool(ctx, &request.tool_id)?;
    let mut capabilities = tool.capabilities.clone();
    if request.interactive.unwrap_or(false) {
        // An interactive child receives keystrokes from this terminal and its output is
        // one merged screen: the operator should be able to name that difference in
        // configuration without editing the registry.
        capabilities.push("interactive_terminal".to_owned());
    }
    let operation = Operation {
        kind: OperationKind::RunCommand,
        tool_id: Some(tool.id.clone()),
        tier: tool.tier,
        capabilities,
        summary: ctx.redactor.redact(&format!(
            "{} {}",
            request_summary(request),
            request
                .args
                .iter()
                .map(|a| ctx.redactor.redact(a))
                .collect::<Vec<_>>()
                .join(" ")
        )),
        scope: Scope::new(tool.command.clone(), request.args.clone()),
    };
    Ok((
        operation,
        Resolved::Command {
            request: request.clone(),
        },
    ))
}

fn request_summary(request: &CommandRequest) -> String {
    format!("run registered tool `{}`", request.tool_id)
}

/// Resolve an application name into exactly one launchable candidate, or refuse.
fn prepare_launch(
    ctx: &Context,
    request: &LaunchRequest,
) -> Result<(Operation, jean_tools::AppCandidate)> {
    let outcome = discover(ctx, request)?;
    let candidate =
        choose_candidate(&outcome, request.pick).map_err(|reason| CoreError::Intent {
            reason: format!("{reason}\n{}", render_candidates(&outcome)),
        })?;

    let exe = candidate.exe_path.clone().unwrap_or_default();
    let operation = Operation {
        kind: OperationKind::LaunchApp,
        tool_id: None,
        tier: PolicyTier::Normal,
        capabilities: vec![capabilities::LAUNCH_APPLICATION.to_owned()],
        summary: ctx.redactor.redact(&format!(
            "launch `{}` from {} as `{}`{}",
            candidate.display_name,
            candidate
                .sources
                .first()
                .map(|p| p.source.as_str())
                .unwrap_or("an unknown source"),
            exe.display(),
            if request.args.is_empty() {
                String::new()
            } else {
                format!(
                    " with {}",
                    request
                        .args
                        .iter()
                        .map(|a| ctx.redactor.redact(a))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            }
        )),
        scope: Scope::new(exe.display().to_string(), request.args.clone()),
    };
    Ok((operation, candidate))
}

/// Run discovery through the configured sources.
pub fn discover(ctx: &Context, request: &LaunchRequest) -> Result<DiscoveryOutcome> {
    jean_tools::discover_installed_apps(request.query.trim(), &ctx.config.tools.discovery)
        .map_err(CoreError::Tool)
}

/// The candidate to launch, or the reason there is not one.
///
/// Public because the rule it encodes — one launchable match or nothing — is the
/// security-relevant decision of the launch intent, and it has to be assertable without
/// installing applications to steer live discovery.
pub fn choose_candidate(
    outcome: &DiscoveryOutcome,
    pick: Option<usize>,
) -> std::result::Result<jean_tools::AppCandidate, String> {
    match pick {
        None => outcome
            .sole_launchable()
            .cloned()
            .ok_or_else(|| describe_ambiguity(outcome)),
        Some(index) => {
            let position = index.checked_sub(1).ok_or_else(|| {
                format!(
                    "`--pick` is 1-based, and 0 is not a candidate. Say `jean launch {}` to \
                     see the list, then pick one of the numbers shown",
                    outcome.query
                )
            })?;
            outcome
                .candidates
                .get(position)
                .cloned()
                .filter(|c| c.is_launchable())
                .ok_or_else(|| {
                    format!(
                        "there is no launchable candidate #{index} ({} candidate(s) were \
                         found{}); nothing was launched",
                        outcome.candidates.len(),
                        if outcome.bound.was_hit() {
                            format!(
                                ", and the search stopped at the {} bound",
                                outcome.bound.as_str()
                            )
                        } else {
                            String::new()
                        }
                    )
                })
        }
    }
}

fn describe_ambiguity(outcome: &DiscoveryOutcome) -> String {
    if outcome.candidates.is_empty() {
        return format!(
            "no installed application matches `{}`{}",
            outcome.query,
            if outcome.bound.was_hit() {
                format!(
                    " in what was searched (the run stopped at the {} bound)",
                    outcome.bound.as_str()
                )
            } else {
                " in the configured sources".to_owned()
            }
        );
    }
    let broken: Vec<&str> = outcome
        .candidates
        .iter()
        .filter(|c| !c.is_launchable())
        .map(|c| c.display_name.as_str())
        .collect();
    if outcome.candidates.len() == 1 && !outcome.candidates[0].is_launchable() {
        return format!(
            "the only match, `{}`, resolves to nothing launchable{}",
            outcome.candidates[0].display_name,
            if broken.is_empty() {
                String::new()
            } else {
                " (its shortcut target does not exist)".to_owned()
            }
        );
    }
    format!(
        "`{}` matches {} applications, and J.E.A.N. does not choose between them",
        outcome.query,
        outcome.candidates.len()
    )
}

/// The candidate list, formatted for the operator who has to pick from it.
pub fn render_candidates(outcome: &DiscoveryOutcome) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    if outcome.candidates.is_empty() {
        let _ = writeln!(text, "  (no candidates)");
    }
    for (index, candidate) in outcome.candidates.iter().enumerate() {
        let _ = writeln!(
            text,
            "  {}) {:<28} {} [{}]{}",
            index + 1,
            candidate.display_name,
            candidate
                .exe_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(no executable found)".to_owned()),
            candidate.match_kind.as_str(),
            if candidate.is_launchable() {
                String::new()
            } else {
                " — NOT LAUNCHABLE".to_owned()
            }
        );
        for provenance in &candidate.sources {
            let _ = writeln!(
                text,
                "      seen at: {} ({})",
                provenance.seen_at,
                provenance.source.as_str()
            );
        }
    }
    if !outcome.notes.is_empty() {
        for note in &outcome.notes {
            let _ = writeln!(text, "  note: {note}");
        }
    }
    if outcome.bound.was_hit() {
        let _ = writeln!(
            text,
            "  the search stopped at the {} bound, so this list may be incomplete",
            outcome.bound.as_str()
        );
    }
    if !outcome.inaccessible_keys.is_empty() {
        let _ = writeln!(
            text,
            "  {} registry subtree(s) were not readable by this account",
            outcome.inaccessible_keys.len()
        );
    }
    text
}

// ─────────────────────────────────────────────────────────────────────────────
// The two lifecycles
// ─────────────────────────────────────────────────────────────────────────────

/// Create the task row in `Queued`, then move it to `Planning`.
fn open_task(ctx: &Context, intent: &Intent, operation: &Operation) -> Result<String> {
    let task_id = jean_protocol::ipc::new_task_id();
    let request_json = serde_json::to_string(&serde_json::json!({
        "intent": intent.as_str(),
        "target": operation.scope.target,
        "arguments": operation.scope.arguments,
        "tier": operation.tier.as_str(),
        "capabilities": operation.capabilities,
    }))
    .map_err(|e| CoreError::File {
        path: PathBuf::from("<request record>"),
        reason: e.to_string(),
    })?;

    let record = TaskRecord::new(
        task_id.clone(),
        operation.summary.clone(),
        // The schema's `tool_id` is `NOT NULL`, and for these three intents the thing
        // that ran *is* the intent: there is no registry entry behind `jean find`.
        // Naming the intent here is what makes `jean tasks` able to say "this row came
        // from a probe" rather than showing a dash and leaving the operator to guess.
        operation
            .tool_id
            .clone()
            .or_else(|| Some(intent.as_str().to_owned())),
        request_json,
    );
    ctx.store.insert_task(&record)?;
    ctx.store.transition(
        &task_id,
        TaskState::Planning,
        Some("the request was resolved"),
    )?;
    Ok(task_id)
}

/// The permission engine, its audit trail, and whose name goes on the record.
///
/// Bundled because both decision paths need all three, and passing them as three
/// arguments each was what pushed `decide_and_act` past a readable signature. Keeping
/// them together is also what makes the rule "a decision is never recorded without its
/// audit line" a property of one type rather than a convention repeated at two sites.
struct Enforcement<'a> {
    policy: &'a Policy,
    audit: &'a AuditWriter,
    actor: String,
    /// The operator's authority for this run, already matched to the plan and step being decided.
    /// `None` for every typed intent, which is what keeps Phase 1's gate exactly where it was.
    grant: Option<GrantScope<'a>>,
}

impl Enforcement<'_> {
    /// Ask the engine and write the answer down before anything acts on it.
    fn decide(
        &self,
        store: &jean_store::Store,
        operation: &Operation,
        prompter: &mut dyn Prompter,
    ) -> Result<Decision> {
        let decision = self
            .policy
            .decide_granted(operation, store, prompter, self.grant)
            .map_err(CoreError::Policy)?;
        let event = AuditEvent::for_decision(operation, &decision, self.actor.clone())
            .with_working_dir(current_directory());
        self.audit.record(&event).map_err(CoreError::Policy)?;
        Ok(decision)
    }

    /// Whether the engine is about to ask a person, counting the run grant.
    ///
    /// Computed rather than observed, because the transition has to be written *before* the
    /// question: a task that is being waited on is in `awaiting_approval` while the operator
    /// is deciding, which is the state that makes `jean tasks` useful during a long prompt.
    /// The condition is the engine's own: a capability on `require_approval` with neither a
    /// run grant nor a remembered grant for this exact scope.
    fn would_prompt(&self, store: &jean_store::Store, operation: &Operation) -> Result<bool> {
        let sensitive = operation
            .capabilities
            .iter()
            .any(|capability| self.policy.requires_approval(capability));
        if !sensitive || operation.tier == PolicyTier::Elevated {
            return Ok(false);
        }
        if let Some(scope) = self.grant {
            if scope.grant.covers(scope.plan_id, scope.step_key, operation) {
                return Ok(false);
            }
        }
        Ok(store
            .find_approval(&operation.scope_digest())
            .map_err(CoreError::Store)?
            .is_none())
    }

    fn record(&self, event: AuditEvent) -> Result<()> {
        self.audit.record(&event).map_err(CoreError::Policy)
    }
}

/// Ask the engine, write the answer down, and act on it.
fn decide_and_act(
    ctx: &Context,
    task_id: &str,
    intent: &Intent,
    operation: &Operation,
    resolved: &Resolved,
    enforcement: &Enforcement<'_>,
    prompter: &mut dyn Prompter,
) -> Result<IntentReport> {
    if enforcement.would_prompt(&ctx.store, operation)? {
        ctx.store.transition(
            task_id,
            TaskState::AwaitingApproval,
            Some("a capability on the list needs a yes"),
        )?;
    }

    let decision = enforcement.decide(&ctx.store, operation, prompter)?;

    if let Decision::Denied(reason) = decision.clone() {
        let text = AuditEvent::denial_text(&reason);
        ctx.store
            .transition(task_id, TaskState::Failed, Some(&text))?;
        ctx.store.set_outcome(task_id, "denied", &text)?;
        ctx.store
            .append_event(task_id, &Event::note(text.clone()))?;
        return Ok(IntentReport {
            task_id: task_id.to_owned(),
            intent: intent.kind(),
            decision,
            state: TaskState::Failed,
            verdict: Verdict::NotProven(format!("nothing was attempted: {text}")),
            payload: Payload::Nothing,
            notes: vec![text],
        });
    }

    // One transition covers both the prompted and the unprompted path: the state
    // table makes `Planning → Running` and `AwaitingApproval → Running` legal, and
    // nothing else is allowed out of `AwaitingApproval` except the failure states.
    ctx.store.transition(
        task_id,
        TaskState::Running,
        Some("the decision allowed the act"),
    )?;
    crate::runner::record_ownership(&ctx.store, task_id)?;
    let run_id = ctx.store.insert_tool_run(
        task_id,
        intent.as_str(),
        &operation.scope.target,
        &crate::runner::to_json_array(&operation.scope.arguments),
        current_directory().as_deref(),
        "{}",
    )?;

    let started = std::time::Instant::now();
    let (payload, verdict, mut notes) = match resolved {
        Resolved::Launch { candidate, request } => act_launch(ctx, candidate, request)?,
        Resolved::Process(request) => act_process(request)?,
        Resolved::Files {
            request,
            canonical_root,
        } => act_files(ctx, request, canonical_root)?,
        Resolved::Command { .. } => {
            return Err(CoreError::Intent {
                reason: "the command intent has its own lifecycle".to_owned(),
            });
        }
    };
    notes.extend(env_notes_hint(ctx));

    ctx.store.finish_tool_run(
        run_id,
        // No child exit code exists for these three intents: nothing was supervised,
        // and inventing `Some(0)` would read as "a command succeeded".
        None,
        // `Exited` is the only honest label for "this completed and there was never a
        // job to kill": `LaunchFailed` would say the act failed, which the verdict says
        // better.
        Termination::Exited,
        &jean_protocol::StreamSummary::default(),
        &jean_protocol::StreamSummary::default(),
        None,
    )?;

    ctx.store
        .transition(task_id, TaskState::Verifying, Some("the act finished"))?;
    let state = final_state(&verdict);
    let reason = format!("{}: {}", verdict.label(), verdict.detail());
    ctx.store.transition(task_id, state, Some(&reason))?;
    ctx.store
        .set_outcome(task_id, verdict.label(), verdict.detail())?;
    ctx.store.clear_runner(task_id)?;
    ctx.store.append_event(
        task_id,
        &Event::note(format!("completed in {}ms", started.elapsed().as_millis())),
    )?;

    // The second line is the one that matters for an audit review: the first says a
    // decision was reached, this one says what happened after it. `decision` is
    // `Allowed` here — the denied path returned above — so no authority is re-derived.
    let outcome_event = AuditEvent::for_decision(operation, &decision, enforcement.actor.clone())
        .with_task(task_id)
        .with_working_dir(current_directory())
        .with_outcome(
            verdict.label(),
            None,
            Some(Termination::Exited.to_string()),
            Some(started.elapsed().as_millis() as u64),
        )
        .with_notes(if notes.is_empty() {
            None
        } else {
            Some(notes.join("; "))
        });
    enforcement.record(outcome_event)?;

    Ok(IntentReport {
        task_id: task_id.to_owned(),
        intent: intent.kind(),
        decision,
        state,
        verdict,
        payload,
        notes,
    })
}

/// Close a task that failed before it could be verified.
fn close_as_failed(ctx: &Context, task_id: &str, reason: &str) {
    if let Err(e) = ctx
        .store
        .transition(task_id, TaskState::Failed, Some(reason))
    {
        tracing::error!(error = %e, task_id, "could not record a failed intent");
    }
    if let Err(e) = ctx.store.set_outcome(task_id, "error", reason) {
        tracing::error!(error = %e, task_id, "could not record the outcome of a failed intent");
    }
    let _ = ctx.store.clear_runner(task_id);
}

/// The command intent: decide, audit, then hand the whole run to Phase 0's pipeline.
///
/// `jean_core::run` owns the task row, the cancellation channel and the verification,
/// exactly as it did in Phase 0, and this function deliberately does not reach inside
/// it. The cost is that the command's task begins at `Queued` rather than `Planning` —
/// recorded in the module docs, not hidden.
fn decide_and_run_command(
    ctx: &Context,
    intent: &Intent,
    request: &CommandRequest,
    operation: &Operation,
    enforcement: &Enforcement<'_>,
    prompter: &mut dyn Prompter,
) -> Result<IntentReport> {
    let decision = enforcement.decide(&ctx.store, operation, prompter)?;

    if let Decision::Denied(reason) = decision.clone() {
        let text = AuditEvent::denial_text(&reason);
        return Ok(IntentReport {
            task_id: String::new(),
            intent: intent.kind(),
            decision,
            state: TaskState::Failed,
            verdict: Verdict::NotProven(format!("nothing was attempted: {text}")),
            payload: Payload::Nothing,
            notes: vec![text],
        });
    }

    let report = run(
        ctx,
        crate::runner::RunRequest {
            tool_id: request.tool_id.clone(),
            args: request.args.clone(),
            goal: request.goal.clone(),
            interactive: request.interactive,
            intent: Some(intent.as_str()),
        },
    )?;

    let mut notes: Vec<String> = health_note(ctx, &request.tool_id).into_iter().collect();
    let outcome_event = AuditEvent::for_decision(operation, &decision, enforcement.actor.clone())
        .with_task(report.task_id.clone())
        .with_working_dir(current_directory())
        .with_outcome(
            report.verdict.label(),
            report.outcome.exit_code,
            Some(report.outcome.termination.to_string()),
            Some(report.outcome.duration.as_millis() as u64),
        )
        .with_notes(if report.notes.is_empty() {
            None
        } else {
            Some(report.notes.join("; "))
        });
    enforcement.record(outcome_event)?;
    notes.extend(report.notes.iter().cloned());

    Ok(IntentReport {
        task_id: report.task_id.clone(),
        intent: OperationKind::RunCommand,
        decision,
        state: report.state,
        verdict: report.verdict.clone(),
        payload: Payload::Command(Box::new(report)),
        notes,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Acting
// ─────────────────────────────────────────────────────────────────────────────

/// Start the application, then prove it opened.
fn act_launch(
    ctx: &Context,
    candidate: &jean_tools::AppCandidate,
    request: &LaunchRequest,
) -> Result<(Payload, Verdict, Vec<String>)> {
    let executable = candidate
        .exe_path
        .clone()
        .ok_or_else(|| CoreError::Intent {
            reason: format!(
                "`{}` has no resolved executable, so there is nothing to launch",
                candidate.display_name
            ),
        })?;

    let spec = LaunchSpec {
        executable: executable.clone(),
        args: request.args.clone(),
        working_dir: None,
        env: launch_environment(ctx),
        timeout: None,
        output_cap_bytes: ctx.output_cap(),
        stdout_path: None,
        stderr_path: None,
        terminal: TerminalMode::Pipes,
    };

    let started = jean_process::launch_detached(&spec).map_err(|e| CoreError::Intent {
        reason: format!(
            "Windows refused to start {}: {e}. The path came from {}, so the file was found \
             and the refusal is about running it, not locating it",
            executable.display(),
            candidate
                .sources
                .first()
                .map(|p| p.seen_at.as_str())
                .unwrap_or("discovery")
        ),
    })?;

    if !request.expect_window {
        return Ok((
            Payload::Launched(LaunchEvidence::NotChecked(format!(
                "the window check was declined; process {} was started and nothing proves \
                 what it did",
                started.pid
            ))),
            Verdict::Unverifiable(format!(
                "process {} started with no verification requested, so completion is \
                 asserted, not proven",
                started.pid
            )),
            vec!["`--no-window` declines the only evidence this intent can gather".to_owned()],
        ));
    }

    let timeout = request.window_timeout.max(MIN_WINDOW_TIMEOUT);
    let waiting = std::time::Instant::now();
    let wait = desktop::wait_for_window(started.pid, timeout, WINDOW_POLL);

    match wait {
        desktop::WindowWait::Found(report) => Ok((
            Payload::Launched(LaunchEvidence::WindowOpened {
                pid: started.pid,
                created_ms: started.created_ms,
                waited_ms: waiting.elapsed().as_millis() as u64,
                windows: report.clone(),
            }),
            Verdict::Proven(format!(
                "{} window(s) an operator can see belong to pid {} ({})",
                report.windows_matched,
                started.pid,
                report
                    .windows
                    .first()
                    .map(|w| w.title.as_str())
                    .unwrap_or("untitled")
            )),
            if report.all_minimised() {
                vec![
                    "every window it owns is minimised, so nothing appeared in front of you"
                        .to_owned(),
                ]
            } else {
                Vec::new()
            },
        )),
        desktop::WindowWait::ProbeUnavailable { reason, .. } => Ok((
            Payload::Launched(LaunchEvidence::NotChecked(reason.clone())),
            Verdict::Unverifiable(format!(
                "the window probe could not look: {reason}. Nothing is claimed about pid {}",
                started.pid
            )),
            Vec::new(),
        )),
        desktop::WindowWait::PidGone {
            waited, attempts, ..
        } => handoff_or_exit(&started, &executable, waited.as_millis() as u64, attempts),
        desktop::WindowWait::DeadlineReached {
            waited,
            poll,
            attempts,
            ..
        } => Ok((
            Payload::Launched(LaunchEvidence::NoWindowInTime {
                pid: started.pid,
                created_ms: started.created_ms,
                waited_ms: waited.as_millis() as u64,
                reason: format!(
                    "no window an operator can see appeared for pid {} within {}ms \
                     ({} looks at {}ms apart)",
                    started.pid,
                    waited.as_millis(),
                    attempts,
                    poll.as_millis()
                ),
            }),
            Verdict::NotProven(format!(
                "process {} is still running but opened no visible window in {}ms; it may be \
                 a background helper, a dialog the probe does not count, or still starting",
                started.pid,
                waited.as_millis()
            )),
            Vec::new(),
        )),
    }
}

/// The single-instance case: the launched pid is gone, so find out whose window moved.
fn handoff_or_exit(
    started: &jean_process::DetachedOutcome,
    executable: &Path,
    waited_ms: u64,
    attempts: usize,
) -> Result<(Payload, Verdict, Vec<String>)> {
    let name = executable
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Matching by process name is exactly the identity a handoff leaves behind: the
    // launched pid is gone, but the surviving instance has the same image name.
    let same_name: Vec<ProcessReport> =
        inspect_by_name(&name, jean_tools::process::DEFAULT_SAMPLE_WINDOW_MS)
            .map_err(CoreError::Tool)?;

    let mut reports = Vec::new();
    for process in &same_name {
        if let Ok(report) = desktop::windows_for_pid(process.pid) {
            if report.has_window() {
                reports.push(report);
            }
        }
    }

    if reports.is_empty() {
        return Ok((
            Payload::Launched(LaunchEvidence::ExitedWithoutWindow {
                pid: started.pid,
                exit_claim: format!(
                    "process {} was gone {}ms after the start ({} looks were made); {} \
                     process(es) named `{name}` are running and none owns a visible window",
                    started.pid,
                    waited_ms,
                    attempts,
                    same_name.len()
                ),
            }),
            Verdict::NotProven(format!(
                "the launched process {} is gone and no window belonging to `{name}` was \
                 found, so nothing shows that the request did anything",
                started.pid
            )),
            vec![format!(
                "`{name}` may need arguments this launch did not supply, or it may have \
                 finished the work and exited"
            )],
        ));
    }

    let windows: usize = reports.iter().map(|r| r.windows_matched).sum();
    Ok((
        Payload::Launched(LaunchEvidence::HandedOff {
            pid: started.pid,
            existing: same_name,
            note: format!(
                "the launched process exited within {}ms; {windows} window(s) belong to \
                 already-running `{name}` process(es), which is what a single-instance \
                 application does",
                waited_ms
            ),
            windows: reports,
        }),
        Verdict::Unverifiable(format!(
            "process {} handed off to an existing instance owning {windows} window(s); that \
             window may be the result of this launch or may already have been on screen, and \
             nothing here can tell the two apart",
            started.pid
        )),
        vec!["single-instance handoff: the launched pid did not survive".to_owned()],
    ))
}

/// Read the process table. No process is touched.
fn act_process(request: &ProcessRequest) -> Result<(Payload, Verdict, Vec<String>)> {
    let window = request
        .sample_window_ms
        .unwrap_or(jean_tools::process::DEFAULT_SAMPLE_WINDOW_MS);
    let mut notes = vec![format!(
        "cpu percentages are of ONE core, measured over a {}ms window; two runs are \
         comparable only if the window matches",
        window
    )];

    if let Some(pid) = request.pid {
        let outcome = inspect_pid(pid, window).map_err(CoreError::Tool)?;
        return Ok(describe_inspection(outcome, window, notes));
    }
    if let Some(name) = &request.name {
        let rows = inspect_by_name(name, window).map_err(CoreError::Tool)?;
        let verdict = if rows.is_empty() {
            Verdict::NotProven(format!(
                "no process named `{name}` is running. The match is on the image name, not \
                 the path, so a tool that runs under a different file name will not appear"
            ))
        } else if rows.len() == 1 {
            Verdict::Proven(format!(
                "`{name}` is running as pid {}; its fields are reported below",
                rows[0].pid
            ))
        } else {
            Verdict::Proven(format!(
                "{} processes match `{name}`; all are reported rather than one being picked",
                rows.len()
            ))
        };
        if rows.len() > 1 {
            notes.push(format!(
                "more than one process matches `{name}` — the intent reports all of them"
            ));
        }
        return Ok((
            Payload::Processes {
                machine: None,
                rows,
                truncated: false,
                sample_window_ms: window,
                gone: None,
            },
            verdict,
            notes,
        ));
    }
    if request.this_process {
        let outcome = inspect_self(window).map_err(CoreError::Tool)?;
        return Ok(describe_inspection(outcome, window, notes));
    }

    let sort = request.sort.unwrap_or(SortKey::Cpu);
    let limit = request
        .limit
        .unwrap_or(jean_tools::process::DEFAULT_MAX_ROWS);
    let listing = list_processes(sort, limit, window).map_err(CoreError::Tool)?;

    let verdict = if listing.rows.is_empty() {
        // An empty process table is not a plausible observation on a running Windows
        // session, so it is reported as "could not tell" rather than "nothing is
        // running", which would be a false statement about the machine.
        Verdict::Unverifiable(
            "the process table came back empty, which no live Windows session does; the \
             sampler, not the machine, is what failed"
                .to_owned(),
        )
    } else {
        Verdict::Proven(format!(
            "{} process(es) visible to this account were sampled over {}ms; the {} most \
             significant by {} are reported",
            listing.machine.process_count,
            listing.sample_window_ms,
            listing.rows.len(),
            sort.as_str()
        ))
    };
    if listing.truncated {
        notes.push(format!(
            "the listing is cut to {} rows by `--limit`; {} process(es) existed beyond it",
            listing.rows.len(),
            listing
                .machine
                .process_count
                .saturating_sub(listing.rows.len())
        ));
    }
    Ok((
        Payload::Processes {
            machine: Some(listing.machine),
            rows: listing.rows,
            truncated: listing.truncated,
            sample_window_ms: listing.sample_window_ms,
            gone: None,
        },
        verdict,
        notes,
    ))
}

/// One pid's report, with the fields the OS refused named instead of hidden.
///
/// `thread_count` is excluded from the verdict, and that exclusion is the difference
/// between a useful command and a useless one: `sysinfo` exposes per-process thread lists
/// only on Linux/Android, so on Windows *every* process reports it unavailable. Folding a
/// documented platform absence into the verdict would make `jean ps` exit "not proven" for
/// every process on every machine, which trains an operator to ignore the verdict. The
/// field is still reported, in the notes, as what it is.
fn describe_inspection(
    outcome: InspectOutcome,
    window: u64,
    mut notes: Vec<String>,
) -> (Payload, Verdict, Vec<String>) {
    match outcome {
        InspectOutcome::Found(report) => {
            let refused: Vec<&'static str> = report
                .unavailable_fields
                .iter()
                .copied()
                .filter(|field| *field != jean_tools::process::fields::THREAD_COUNT)
                .collect();
            let rows = vec![report.clone()];
            let verdict = if refused.is_empty() {
                Verdict::Proven(format!(
                    "pid {} is running and every field this build can report was obtained",
                    report.pid
                ))
            } else {
                // A partially-refused report is still a report: the pid's existence is
                // proven, and the withheld fields are the honest remainder — which is
                // why this is `Unverifiable` and not `Proven` or `NotProven`.
                Verdict::Unverifiable(format!(
                    "pid {} is running, but {} field(s) were refused by the OS: {}",
                    report.pid,
                    refused.len(),
                    refused.join(", ")
                ))
            };
            if report.unavailable_fields.len() > refused.len() {
                notes.push(
                    "`thread_count` is never available on Windows through this sampler \
                     (`sysinfo` exposes per-process thread lists on Linux only); it is a \
                     platform absence, not a refusal, and does not change the verdict"
                        .to_owned(),
                );
            }
            if !refused.is_empty() {
                notes.push(
                    "fields are usually refused for a process owned by another user; J.E.A.N. \
                     runs at your own integrity level and will not escalate to read them \
                     (D-010)"
                        .to_owned(),
                );
            }
            (
                Payload::Processes {
                    machine: None,
                    rows,
                    truncated: false,
                    sample_window_ms: window,
                    gone: None,
                },
                verdict,
                notes,
            )
        }
        InspectOutcome::Gone { pid, seen_at_ms } => (
            Payload::Processes {
                machine: None,
                rows: Vec::new(),
                truncated: false,
                sample_window_ms: window,
                gone: Some(pid),
            },
            Verdict::NotProven(if seen_at_ms == 0 {
                format!("no process with pid {pid} exists")
            } else {
                format!(
                    "pid {pid} was present when the sample began and had exited by {}",
                    jean_store::format_ms(i64::try_from(seen_at_ms).unwrap_or(i64::MAX))
                )
            }),
            notes,
        ),
    }
}

/// Walk one root for names. Contents are never read.
fn act_files(
    ctx: &Context,
    request: &FilesRequest,
    canonical_root: &Path,
) -> Result<(Payload, Verdict, Vec<String>)> {
    let pattern = NamePattern::new(&request.pattern, ctx.config.search.ignore_case);
    let limits = SearchLimits {
        max_results: request
            .max_results
            .unwrap_or(ctx.config.search.max_results as usize),
        ..SearchLimits::default()
    };
    let report = search_names(canonical_root, &pattern, &ctx.config.search, limits)
        .map_err(CoreError::Tool)?;

    let mut notes = Vec::new();
    if !report.fully_covered() {
        notes.push(format!(
            "this is not a complete answer: the walk stopped at {} and {} hit(s) vanished \
             between being found and being reported",
            report.stop.as_str(),
            report.vanished
        ));
    }
    if !report.reparse_skipped.is_empty() {
        notes.push(format!(
            "{} reparse point(s) were seen and not followed, which is how the walk stays \
             inside {}",
            report.reparse_skipped.len(),
            canonical_root.display()
        ));
    }
    if !report.access_errors.is_empty() {
        notes.push(format!(
            "{} location(s) could not be read{}: {}",
            report.access_errors.len(),
            if report.access_errors_truncated {
                " (further failures were folded into the count)"
            } else {
                ""
            },
            report
                .access_errors
                .iter()
                .map(|f| format!("{}: {}", f.path.display(), f.reason))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    if report.hidden_withheld > 0 {
        notes.push(format!(
            "{} entry name(s) were withheld because they are hidden and \
             `search.include_hidden` is false",
            report.hidden_withheld
        ));
    }

    let verdict = verify_hits(&report);
    Ok((Payload::Files(report), verdict, notes))
}

/// "The path exists and is readable", per the roadmap's verification table.
fn verify_hits(report: &SearchReport) -> Verdict {
    if report.hits.is_empty() {
        return Verdict::NotProven(if report.fully_covered() {
            format!(
                "nothing named `{}` exists under {} — the authorised root was walked to its \
                 bounds",
                report.pattern, report.display_root
            )
        } else {
            format!(
                "nothing named `{}` was found in what was searched; the walk stopped at {}, \
                 so the file may be somewhere it did not reach",
                report.pattern,
                report.stop.as_str()
            )
        });
    }

    // Re-checked at reporting time. The walk already verified its own candidates, and a
    // file deleted in between must not be reported as found; `report.vanished` covers
    // the ones the walk itself noticed.
    let mut readable = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for hit in &report.hits {
        let path = &hit.path;
        match std::fs::File::open(path) {
            Ok(file) => {
                readable += 1;
                drop(file);
            }
            Err(e) => failures.push(format!("{}: {e}", path.display())),
        }
    }

    if readable == 0 {
        return Verdict::NotProven(format!(
            "{} name(s) matched but none could be opened for reading: {}",
            report.hits.len(),
            failures.join("; ")
        ));
    }
    if failures.is_empty() {
        Verdict::Proven(format!(
            "{readable} of {} reported path(s) exist and opened for reading",
            report.hits.len()
        ))
    } else {
        // The claim is proven for what opened, and the rest is stated separately rather
        // than folded into a single "found". A directory hit on Windows answers
        // `File::open` with "access denied" — that is a real report, not a wrong one.
        Verdict::Proven(format!(
            "{readable} of {} reported path(s) exist and opened for reading; the others did \
             not ({}), which includes any reported directory",
            report.hits.len(),
            failures.join("; ")
        ))
    }
}

/// A search root the walk is allowed to use, canonicalised once.
fn canonical_root(root: &Path) -> Result<PathBuf> {
    if root.as_os_str().is_empty() {
        return Err(CoreError::Intent {
            reason: "a search needs an explicit root; `jean find` will not pick one for you, \
                     because the choice of root is the privacy boundary of this intent"
                .to_owned(),
        });
    }
    let canonical = root.canonicalize().map_err(|e| CoreError::Intent {
        reason: format!(
            "{} cannot be used as a search root: {e}. The root must exist before the walk, \
             so a typo is reported rather than searched for",
            root.display()
        ),
    })?;
    Ok(canonical)
}

/// The configured workspace root, canonicalised, if one is set.
///
/// `pub(crate)` because the planner asks the same question before a plan is accepted: a step whose
/// search root falls outside the workspace must be refused at validation, and there has to be one
/// definition of where the boundary is.
pub(crate) fn workspace_root(ctx: &Context) -> Option<PathBuf> {
    if ctx.config.workspace.root.trim().is_empty() {
        return None;
    }
    Path::new(ctx.config.workspace.root.trim())
        .canonicalize()
        .ok()
}

/// The environment a launched application gets.
///
/// The same allowlist a supervised child gets, for the same reason: an application
/// started by J.E.A.N. should not see more of the operator's environment than a command
/// started by J.E.A.N. does. What it additionally must not see is this process's own
/// secrets, which the allowlist is the only copy of.
fn launch_environment(ctx: &Context) -> BTreeMap<String, String> {
    let parent: BTreeMap<String, String> = std::env::vars().collect();
    let (child, report) =
        build_child_env(&ctx.config.process.env_allowlist, &BTreeMap::new(), &parent);
    if !report.is_empty() {
        tracing::debug!(
            rejected = ?report.rejected,
            absent = ?report.absent,
            "the launched application's environment was narrowed to the allowlist"
        );
    }
    child
}

/// The operation this intent resolves to, without deciding or running it.
///
/// The plan runner needs this to mint a run grant: the digest an operator authorises must be
/// computed by exactly the code that later executes the step — after resolution — or the grant
/// would match a different operation than the one the plan describes. A step that does not resolve
/// (an application name with two matches, say) has no operation and so no entry, and the step is
/// asked about like any other.
pub fn operation_for(ctx: &Context, intent: &Intent) -> Result<Operation> {
    prepare(ctx, intent).map(|(operation, _resolved)| operation)
}

/// The final state for a verdict.
fn final_state(verdict: &Verdict) -> TaskState {
    match verdict {
        // A launched application that opened a window is a success even though the
        // broader goal ("the user is now working in it") is out of scope for a probe.
        Verdict::Proven(_) => TaskState::Succeeded,
        Verdict::NotProven(_) | Verdict::Unverifiable(_) => TaskState::Succeeded,
    }
}

/// Anything worth saying about the environment that a probe's own notes cannot carry.
fn env_notes_hint(ctx: &Context) -> Vec<String> {
    // The workspace root is the one configuration item whose absence changes what a
    // `jean find` refusal means, so it is reported once per invocation rather than
    // silently disabling the `search_outside_workspace` capability.
    if ctx.config.workspace.root.trim().is_empty() {
        return vec![
            "`workspace.root` is not set, so `jean find` cannot tell whether a search left \
             the workspace and `policy.require_approval` has nothing to match on for \
             `search_outside_workspace`"
                .to_owned(),
        ];
    }
    Vec::new()
}

fn process_target(request: &ProcessRequest) -> String {
    match (
        &request.pid,
        &request.name,
        request.this_process,
        request.list,
    ) {
        (Some(pid), _, _, _) => format!("pid:{pid}"),
        (_, Some(name), _, _) => format!("name:{}", name.trim().to_ascii_lowercase()),
        (_, _, true, _) => format!("pid:{}", std::process::id()),
        _ => "process-table".to_owned(),
    }
}

/// Who is asking.
///
/// The username, from the environment, with a fallback that says the lookup failed
/// rather than inventing `unknown`: the audit line is the record of who authorised an
/// act, and a made-up author is worse than a missing one.
pub(crate) fn actor() -> String {
    for name in ["USERNAME", "USER", "LOGON_USER"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return value;
            }
        }
    }
    "<unknown: no username in the environment>".to_owned()
}

fn current_directory() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
}

/// The health question for the intent layer: is the thing being run known-broken?
///
/// Reported as a note, never as a refusal. A tool whose executable moved is worth
/// telling the operator about before the child fails with a confusing message, but
/// refusing to run it would make a cache of probes into a permission gate, which is
/// `jean-policy`'s job.
pub fn health_note(ctx: &Context, tool_id: &str) -> Option<String> {
    let record: ToolHealthRecord = ctx
        .store
        .tool_health(tool_id)
        .ok()
        .flatten()
        .filter(|r| r.status == jean_store::HealthStatus::Unhealthy)?;
    Some(format!(
        "the last health probe of `{tool_id}` said unhealthy after {} consecutive failures{}",
        record.consecutive_failures,
        record
            .detail
            .as_ref()
            .map(|d| format!(": {d}"))
            .unwrap_or_default()
    ))
}
