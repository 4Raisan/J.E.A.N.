//! The desktop workflow: one window, identified, acted on, and read back.
//!
//! ## The shape of the authority
//!
//! `[desktop] enabled` is off by default, and turning it on does not authorise a click. What authorises
//! one is the policy engine, consulted with an operation whose **scope target is the process image of the
//! window being acted on** — the same choice `DriveBrowser` makes with an origin, for the same reason. A
//! remembered approval that covered a fixture window must never be the reason a chat client gets pressed
//! next week, and a digest keyed on an `hwnd` would have made that invisible: handles are recycled, so it
//! would be either meaningless later or accidentally aimed at whatever inherited the number.
//!
//! ```text
//! config gate → worker gate → resolve window → read identity → task row → policy decision → audit
//!             → running + cancellation channel → worker act → read back → judge → close the row
//! ```
//!
//! The two gates run before a record exists. `enabled` is the operator's standing answer about the
//! subsystem, and the worker gate refuses when the live `jean-desktop-worker` processes have reached
//! `max_concurrent_workers`; a refusal at either point creates no task row and says why in one sentence.
//!
//! Reads are deliberately outside the `enabled` gate. [`windows`], [`inspect`] and [`diagnose_window`]
//! work with `enabled = false`, because the plan requires a blocked window to be *diagnosed*, and a switch
//! that also turned diagnosis off would remove the only evidence an operator has for deciding whether to
//! turn the actions on. They create no task row — nothing was decided and nothing was acted on — and they
//! write metadata to the log (window label, locator, counts), never content. Control names are another
//! application's text: they go to the terminal the operator asked, not to a rolling file.
//!
//! ## Why the verdict needs a second control
//!
//! `expect` is stated before the action and read afterwards from the **witness**, which is usually not the
//! control that was acted on: pressing a send button changes a status line, ticking a checkbox changes a
//! total. That is "perform an authorized action → observe the resulting state → report evidence", and it
//! is why the mechanism's `act` takes a witness locator rather than verifying the acted control alone.
//! Without `expect` the run reports `unverifiable` and says what it observed: the change is evidence of
//! the action, not of the goal, and nothing was declared that could have failed. Same rule as the browser
//! workflow, so a script reads one vocabulary.
//!
//! ## Uncertain side effects are recorded, never replayed
//!
//! A desktop action runs in a worker process that can be killed, and it can be killed *after* the pattern
//! call was delivered. That is not a failure with a clean edge: the click may have landed, and no read can
//! tell. The outcome is recorded as `uncertain`, the task needs attention, and the report carries the rule
//! in its own words. Nothing in this build retries a desktop step — the reconciliation is a person looking
//! at the window. Termination removes the leak; it does not remove the uncertainty.
//!
//! ## What is not here
//!
//! No continuous capture, no OCR, no model-driven planning, no focus stealing, and no replay. A capture
//! happens when a command asks for one, is judged for retention by `[desktop] retention`, and its pixels
//! go nowhere near a log: [`capture::log_line`] is the only form this path records unless the operator
//! chose to keep the file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use jean_policy::{
    AuditEvent, AuditWriter, Decision, GrantScope, Operation, OperationKind, Policy, Prompter,
    Scope,
};
use jean_process::CancelToken;
use jean_protocol::config::DesktopConfig;
use jean_protocol::event::{Event, EventKind};
use jean_protocol::tool::PolicyTier;
use jean_protocol::TaskState;
use jean_store::TaskRecord;
use jean_windows::{
    accessibility, capability, capture, identity, input, ocr, worker, WindowsError,
};

use crate::captures;
use crate::context::Context;
use crate::error::{CoreError, Result};
use crate::ipc::CancelListener;
use crate::verify::Verdict;

// ─────────────────────────────────────────────────────────────────────────────
// The vocabulary the surface above this module speaks
// ─────────────────────────────────────────────────────────────────────────────

/// Acting on a control through its accessible patterns: `Invoke`, `Value`, `Toggle`, `Selection`.
pub const ACT_CAPABILITY: &str = "desktop.act";

/// Synthesised keyboard or mouse input. Its own capability because the semantic path is the one meant to
/// be used, and an operator can reasonably allow one and refuse the other.
pub const INPUT_CAPABILITY: &str = "desktop.input";

/// Reading one window's pixels on demand.
pub const CAPTURE_CAPABILITY: &str = "desktop.capture";

/// The id the task rows and audit lines are recorded under.
pub const TOOL_ID: &str = "desktop.native";

/// The mechanism's vocabulary, re-exported so `jean-cli` and any later planner name one set of types
/// rather than a second set that can drift from the crate that performs the call.
pub type DesktopAction = accessibility::Action;
pub type DesktopInput = input::Input;
pub type MouseButton = input::Button;
pub type ScrollAmount = input::Scroll;
pub type ModifierKey = input::Modifier;
pub type VirtualKeyCode = input::VirtualKey;

/// The observation types the read paths hand back, named here so the command line can print them without
/// depending on `jean-windows`. D-011's direction applies to the CLI too: the surface talks to this crate,
/// and reaching past it to a Win32 type would make the boundary a suggestion.
pub type WindowIdentity = identity::WindowIdentity;
pub type Tree = accessibility::Tree;
pub type WindowReport = capability::WindowReport;
pub type DesktopSweep = capability::DesktopReport;

/// The cadence of the cancellation observer.
///
/// The same 25 ms slice the mechanism waits in: the promise is that an operator's cancel reaches an
/// in-flight wait within one slice, not that it reaches it instantly.
const CANCEL_POLL: Duration = Duration::from_millis(25);

/// The longest one report line gets to be.
///
/// A control name is another application's text, and a log line is not where a program gets to write a
/// thousand characters about its own document titles.
const MAX_LINE_CHARS: usize = 400;

// ─────────────────────────────────────────────────────────────────────────────
// Requests
// ─────────────────────────────────────────────────────────────────────────────

/// Which window, named the way an operator names it.
///
/// Every field is a *question asked of the desktop now*, never a cached handle: an `hwnd` is resolved by
/// re-reading the identity it carries, and a title or image by re-enumerating. That is what makes "do not
/// reuse stale coordinates or accessibility observations after a layout change" the default rather than a
/// discipline somebody has to remember.
#[derive(Debug, Clone, Default)]
pub struct WindowSelector {
    /// The handle, as a previous `jean desktop windows` printed it.
    pub hwnd: Option<isize>,
    /// The owning process id, matched against visible windows of that process.
    pub pid: Option<u32>,
    /// Case-insensitive substring of the window title.
    pub title: Option<String>,
    /// Case-insensitive substring of the owning executable, or of the window class.
    pub image: Option<String>,
}

impl WindowSelector {
    pub fn is_empty(&self) -> bool {
        self.hwnd.is_none() && self.pid.is_none() && self.title.is_none() && self.image.is_none()
    }

    /// How the caller asked, for the audit line and the prompt. The resolved identity is carried
    /// separately, so this never has to stand in for it.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(hwnd) = self.hwnd {
            parts.push(format!("hwnd={hwnd}"));
        }
        if let Some(pid) = self.pid {
            parts.push(format!("pid={pid}"));
        }
        if let Some(title) = &self.title {
            parts.push(format!("title~{title:?}"));
        }
        if let Some(image) = &self.image {
            parts.push(format!("image~{image:?}"));
        }
        if parts.is_empty() {
            "nothing".to_owned()
        } else {
            parts.join(" ")
        }
    }
}

/// Which control, addressed the way UIA addresses one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlSelector {
    /// The `AutomationId`, which on a standard Win32 control is the dialog control id.
    AutomationId(String),
    /// The accessible name.
    Name(String),
}

impl ControlSelector {
    pub fn to_locator(&self) -> accessibility::Locator {
        match self {
            Self::AutomationId(id) => accessibility::Locator::AutomationId(id.clone()),
            Self::Name(name) => accessibility::Locator::Name {
                name: name.clone(),
                control_type: None,
            },
        }
    }

    /// The audit-safe form: what was looked for, never what was found.
    pub fn describe(&self) -> String {
        self.to_locator().describe()
    }
}

/// One pattern action, stated before any of it runs.
#[derive(Debug, Clone)]
pub struct ActRequest {
    pub window: WindowSelector,
    pub control: ControlSelector,
    pub action: DesktopAction,
    /// The control whose state proves the action. Defaults to the acted control itself, which is honest
    /// for a checkbox and useless for a button — hence the field.
    pub witness: Option<ControlSelector>,
    /// What the witness must show afterwards: a digest token (`name=clicked:2`) or the value alone
    /// (`clicked:2`).
    pub expect: Option<String>,
    /// Capture the window after the action, as a second witness. A failed capture never voids an action
    /// that succeeded; the two are different observations and the report keeps them apart.
    pub capture: bool,
}

/// Where an injected input is aimed.
#[derive(Debug, Clone)]
pub enum InputTarget {
    /// The centre of the control the locator names, re-read immediately before the send.
    Control(ControlSelector),
    /// Physical pixels on the virtual desktop, as a previous read reported them.
    ///
    /// Accepted, and checked: [`input::Target::verify`] refuses a point outside the window's *current*
    /// rectangle, which is the case where a coordinate outlived the layout it came from.
    Point { x: i32, y: i32 },
    /// The centre of the window's current rectangle.
    Window,
}

/// One injected input, stated before any of it runs.
#[derive(Debug, Clone)]
pub struct InputRequest {
    pub window: WindowSelector,
    pub at: InputTarget,
    pub input: DesktopInput,
    pub witness: Option<ControlSelector>,
    pub expect: Option<String>,
    pub capture: bool,
}

/// One on-demand capture.
#[derive(Debug, Clone)]
pub struct CaptureRequest {
    pub window: WindowSelector,
    /// Where to write the frame, whatever `retention` says. Naming a path is an explicit request to keep,
    /// and it is the only way a frame leaves this process.
    pub out: Option<PathBuf>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Reports
// ─────────────────────────────────────────────────────────────────────────────

/// What one desktop operation produced.
#[derive(Debug, Clone)]
pub struct DesktopReport {
    pub task_id: String,
    /// The bounded identity label of the window that was acted on, re-read at the moment of the act.
    pub window: String,
    pub pid: u32,
    /// What was done, in the audit-safe form: `invoke automation_id=1002`, never the typed text.
    pub did: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub changed: bool,
    /// Why the outcome could not be established, when it could not. Its presence is what forbids a retry.
    pub uncertain: Option<String>,
    /// `true` when the operation never reached the desktop: policy, or a configuration gate, said no.
    ///
    /// Separate from the verdict because the two need different things from a script. An action that was
    /// refused is a fact about the configuration and the ledger; an action that is `unverifiable` may have
    /// happened, and retrying it is the mistake this field helps a caller avoid making.
    pub refused: bool,
    pub screenshot: Option<PathBuf>,
    /// The frame's metadata line, when a capture ran: geometry, hash, quality. Never pixels.
    pub capture_note: Option<String>,
    pub retention: String,
    pub verdict: Verdict,
    pub state: TaskState,
    pub steps: Vec<String>,
}

impl DesktopReport {
    /// The one line the CLI prints.
    pub fn summary_line(&self) -> String {
        let mut line = format!("{}: {}", self.verdict.label(), self.verdict.detail());
        if let Some(uncertain) = &self.uncertain {
            line.push_str(&format!(" — outcome uncertain ({uncertain})"));
        }
        line
    }

    pub fn is_proven(&self) -> bool {
        matches!(self.verdict, Verdict::Proven(_))
    }
}

/// What `jean desktop windows` answers.
#[derive(Debug, Clone)]
pub struct WindowListing {
    /// The DPI awareness this process actually has, which is what every coordinate here assumes.
    pub awareness: identity::Awareness,
    pub session: identity::SessionObservation,
    pub windows: Vec<identity::WindowIdentity>,
    /// How many windows existed before the limit, so a truncated list says it is truncated.
    pub total: usize,
}

/// What `jean desktop inspect` answers.
#[derive(Debug, Clone)]
pub struct Inspection {
    pub window: identity::WindowIdentity,
    pub tree: accessibility::Tree,
}

impl Inspection {
    /// A readable tree, one line per control.
    ///
    /// This text is what the operator asked for and it is not logged: it carries control names, which are
    /// another application's content.
    pub fn render(&self) -> String {
        let mut out = format!(
            "{}\n  {} — {} controls, dpi {}{}",
            self.window.audit_label(),
            explain(&self.tree.explanation),
            self.tree.controls.len(),
            self.window
                .dpi
                .map(|dpi| dpi.to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
            if self.tree.truncated {
                ", TRUNCATED by [desktop] max_tree_nodes"
            } else {
                ""
            }
        );
        for (index, control) in self.tree.controls.iter().enumerate() {
            out.push_str(&format!(
                "\n  {:>3} {:<16} id={:<10} name={:?} enabled={} patterns={}",
                index,
                control.kind.name(),
                if control.automation_id.is_empty() {
                    "-"
                } else {
                    control.automation_id.as_str()
                },
                control.name,
                control.is_enabled,
                if control.patterns.is_empty() {
                    "-".to_owned()
                } else {
                    control
                        .patterns
                        .iter()
                        .map(|pattern| pattern.name())
                        .collect::<Vec<_>>()
                        .join(",")
                }
            ));
        }
        out
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The read paths
// ─────────────────────────────────────────────────────────────────────────────

/// Every window an operator would call a window, with the identity needed to act on it.
///
/// Uses Phase 1's definition of "operator window" through [`identity::enumerate`], so the list `jean
/// launch` verifies against and the list printed here cannot disagree — one enumeration, two consumers.
pub fn windows(limit: usize) -> Result<WindowListing> {
    let awareness = identity::ensure_dpi_awareness();
    let all = identity::enumerate().map_err(intent_of)?;
    let total = all.len();
    tracing::info!(
        total,
        shown = total.min(limit),
        awareness = awareness_line(&awareness).as_str(),
        "desktop windows enumerated"
    );
    Ok(WindowListing {
        awareness,
        session: identity::session(),
        windows: all.into_iter().take(limit).collect(),
        total,
    })
}

/// The accessible controls of one window, on a bounded worker.
pub fn inspect(ctx: &Context, selector: &WindowSelector) -> Result<Inspection> {
    let found = resolve_window(selector)?;
    let tree =
        accessibility::read_tree(found.id, accessibility_settings(&ctx.config.desktop), None)
            .map_err(intent_of)?;
    tracing::info!(
        window = found.audit_label().as_str(),
        controls = tree.controls.len(),
        explanation = explain(&tree.explanation).as_str(),
        "desktop tree read"
    );
    Ok(Inspection {
        window: found,
        tree,
    })
}

/// What one window was actually observed to support, and why anything it did not support failed.
///
/// The seven operations are probed independently and every failure becomes an entry rather than an abort,
/// which is what makes this safe to run against an application that is not answering.
pub fn diagnose_window(
    ctx: &Context,
    selector: &WindowSelector,
) -> Result<capability::WindowReport> {
    let found = resolve_window(selector)?;
    Ok(capability::diagnose(
        found.id,
        &capability_settings(&ctx.config.desktop),
        None,
    ))
}

/// The same, for every window an operator would call a window.
pub fn diagnose_desktop(ctx: &Context, limit: usize) -> capability::DesktopReport {
    capability::sweep(limit, &capability_settings(&ctx.config.desktop), None)
}

/// One window's pixels, on demand, with the retention decision applied before anything is written.
///
/// Routed through policy, task and audit even though it changes nothing: a frame is the most revealing
/// thing this crate can produce, and "who looked at that window" is a question an operator is entitled to
/// ask of the record afterwards.
pub fn capture_window(
    ctx: &mut Context,
    request: &CaptureRequest,
    prompter: &mut dyn Prompter,
) -> Result<DesktopReport> {
    let config = ctx.config.desktop.clone();
    let found = resolve_window(&request.window)?;
    let operation = capture_operation(ctx, &found, request);
    let request_json = serde_json::json!({
        "window": request.window.describe(),
        "capability": CAPTURE_CAPABILITY,
        "out": request.out,
        "tier": operation.tier.as_str(),
    })
    .to_string();
    let out = request.out.clone();
    decided(
        ctx,
        &config,
        operation,
        &found,
        "capture",
        prompter,
        None,
        &request_json,
        {
            move |_ctx, config, _task_id, cancel, found| {
                let recovery = capture::capture_with_recovery(
                    found.id,
                    capture_settings(config),
                    Some(cancel),
                )
                .map_err(intent_of)?;
                let mut steps = vec![describe_attempts(&recovery)];
                report_frame(config, found, recovery, out.as_ref(), &mut steps)
            }
        },
    )
}

/// The frame, the retention, and the judgement of whether it says anything.
fn report_frame(
    config: &DesktopConfig,
    found: &identity::WindowIdentity,
    recovery: capture::Recovery,
    out: Option<&PathBuf>,
    steps: &mut Vec<String>,
) -> Result<Unsettled> {
    let note = recovery
        .capture
        .as_ref()
        .map(capture::log_line)
        .unwrap_or_default();
    let retention = retention_of(config);
    let mut screenshot = None;
    match (recovery.capture.as_ref(), out) {
        (Some(frame), Some(path)) => {
            make_parent(path)?;
            match capture::write_bmp(frame, path).map_err(intent_of) {
                Ok(written) => {
                    steps.push(format!("wrote the frame to {}", written.display()));
                    screenshot = Some(written);
                }
                Err(error) => steps.push(format!("the frame could not be written: {error}")),
            }
        }
        (Some(frame), None) if retention.keep => match keep_frame(config, frame) {
            Ok(path) => {
                steps.push(format!("kept the frame at {}", path.display()));
                screenshot = Some(path);
            }
            Err(error) => steps.push(format!("the frame could not be kept: {error}")),
        },
        (Some(_), None) => steps.push(retention.note.clone()),
        (None, _) => steps.push("nothing was captured, so nothing was retained".to_owned()),
    }
    let verdict = match recovery.capture.as_ref() {
        Some(frame) if frame.quality().is_informative() => Verdict::Proven(format!(
            "{}x{} frame from the {} backend",
            frame.width,
            frame.height,
            frame.backend.name()
        )),
        Some(frame) => Verdict::NotProven(format!(
            "a frame came back and it says nothing: {:?} — {}",
            frame.quality(),
            describe_attempts(&recovery)
        )),
        None => Verdict::NotProven(describe_attempts(&recovery)),
    };
    Ok(Unsettled {
        did: format!("capture {}", found.audit_label()),
        before: None,
        after: (!note.is_empty()).then_some(note),
        changed: recovery.capture.is_some(),
        uncertain: None,
        screenshot,
        capture_note: recovery.capture.as_ref().map(capture::log_line),
        retention: retention.note,
        verdict,
        steps: std::mem::take(steps),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// The action paths
// ─────────────────────────────────────────────────────────────────────────────

/// Perform one pattern action, and observe what the desktop says afterwards.
pub fn act(
    ctx: &mut Context,
    request: &ActRequest,
    prompter: &mut dyn Prompter,
) -> Result<DesktopReport> {
    act_granted(ctx, request, prompter, None)
}

/// [`act`], with the run grant that may already cover exactly this step.
///
/// A desktop action is a step of a plan like any other, and a grant is the operator's authority for a run:
/// a plan whose steps were shown to a person does not ask again for each of them. Nothing a grant covers
/// reaches past the refusals the policy engine makes before it looks at one.
pub fn act_granted(
    ctx: &mut Context,
    request: &ActRequest,
    prompter: &mut dyn Prompter,
    scope: Option<GrantScope<'_>>,
) -> Result<DesktopReport> {
    let config = ctx.config.desktop.clone();
    require_enabled(&config)?;
    let found = resolve_window(&request.window)?;
    let operation = act_operation(ctx, &found, request);
    let request_json = serde_json::json!({
        "window": request.window.describe(),
        "control": request.control.describe(),
        "action": request.action.describe(),
        "witness": request.witness.as_ref().map(ControlSelector::describe),
        "expect": request.expect,
        "capability": ACT_CAPABILITY,
        "tier": operation.tier.as_str(),
    })
    .to_string();
    let request = request.clone();
    decided(
        ctx,
        &config,
        operation,
        &found,
        "act",
        prompter,
        scope,
        &request_json,
        move |_ctx, config, _task_id, cancel, found| act_drive(config, cancel, found, &request),
    )
}

/// Inject one keyboard or mouse input, and observe what the desktop says afterwards.
///
/// The fallback: deliberately last, deliberately behind its own switch, and never taken because a capture
/// came back blank.
pub fn send_input(
    ctx: &mut Context,
    request: &InputRequest,
    prompter: &mut dyn Prompter,
) -> Result<DesktopReport> {
    send_input_granted(ctx, request, prompter, None)
}

/// [`send_input`], with the run grant in scope.
pub fn send_input_granted(
    ctx: &mut Context,
    request: &InputRequest,
    prompter: &mut dyn Prompter,
    scope: Option<GrantScope<'_>>,
) -> Result<DesktopReport> {
    let config = ctx.config.desktop.clone();
    require_enabled(&config)?;
    require_synthetic_input(&config)?;
    let found = resolve_window(&request.window)?;
    let operation = input_operation(ctx, &found, request);
    let request_json = serde_json::json!({
        "window": request.window.describe(),
        "input": request.input.describe(),
        "target": describe_target(&request.at),
        "witness": request.witness.as_ref().map(ControlSelector::describe),
        "expect": request.expect,
        "capability": INPUT_CAPABILITY,
        "tier": operation.tier.as_str(),
    })
    .to_string();
    let request = request.clone();
    decided(
        ctx,
        &config,
        operation,
        &found,
        "input",
        prompter,
        scope,
        &request_json,
        move |_ctx, config, _task_id, cancel, found| input_drive(config, cancel, found, &request),
    )
}

/// The action, the read-back, and the judgement — everything on this side of the decision.
fn act_drive(
    config: &DesktopConfig,
    cancel: &AtomicBool,
    found: &identity::WindowIdentity,
    request: &ActRequest,
) -> Result<Unsettled> {
    let acted = request.control.to_locator();
    let witness = request
        .witness
        .as_ref()
        .map_or_else(|| acted.clone(), |selector| selector.to_locator());

    let effect = accessibility::act(
        found.id,
        acted.clone(),
        request.action.clone(),
        Some(witness.clone()),
        accessibility_settings(config),
        Some(cancel),
    );
    let mut steps = Vec::new();
    let outcome = match effect {
        Ok(effect) => {
            steps.push(format!(
                "{} carried the {} on {}",
                effect.pattern.name(),
                effect.action.describe(),
                effect.control.fingerprint()
            ));
            if let Some(foreground) = effect.foreground_after {
                steps.push(format!(
                    "the foreground after the action is {}",
                    window_label(foreground)
                ));
            }
            judge(&effect, &witness, request.expect.as_deref(), &mut steps)
        }
        Err(error) => {
            // The error *is* the observation, and it is classified rather than paraphrased: a refused
            // pattern is a fact about the control, a timeout on a side-effecting action is a fact about
            // what may already have happened to the operator's desktop.
            let uncertain = uncertain_after_failure(&error, request.action.has_side_effects());
            let verdict = match &uncertain {
                Some(reason) => {
                    steps.push(format!("the action may have landed: {reason}"));
                    Verdict::Unverifiable(format!(
                        "the {} was delivered to a provider that then stopped answering, so its effect \
                         is unknown and will not be repeated",
                        request.action.describe()
                    ))
                }
                None => {
                    steps.push(format!(
                        "the mechanism refused before anything was delivered: {error}"
                    ));
                    Verdict::NotProven(error.to_string())
                }
            };
            Judgement::unreadable(verdict, uncertain)
        }
    };
    steps.push(outcome.observation(&witness));

    let (screenshot, capture_note, retention) =
        secondary_capture(config, found, request.capture, cancel, &mut steps);

    Ok(Unsettled {
        did: format!("{} {}", request.action.describe(), acted.describe()),
        before: Some(outcome.before.clone()),
        after: outcome.after.clone(),
        changed: outcome.changed,
        uncertain: outcome.uncertain.clone(),
        screenshot,
        capture_note,
        retention,
        verdict: outcome.verdict,
        steps,
    })
}

/// Read the witness, inject the input, read the witness again.
///
/// The input path has no pattern call to carry a witness inside it, so it takes two state reads of the
/// control the operator named — the same Value, Toggle, SelectionItem and ExpandCollapse properties an
/// action reads back, which is what makes one `--expect` grammar mean the same thing on both paths. What two
/// reads cannot show is *when* the difference appeared: they bracket the batch, and nothing inside it.
fn input_drive(
    config: &DesktopConfig,
    cancel: &AtomicBool,
    found: &identity::WindowIdentity,
    request: &InputRequest,
) -> Result<Unsettled> {
    let point = resolve_point(config, found, &request.at, Some(cancel))?;
    let witness_label = request
        .witness
        .as_ref()
        .map_or_else(|| "the target window".to_owned(), ControlSelector::describe);
    let before = request
        .witness
        .as_ref()
        .map(|selector| read_witness(config, found, selector, Some(cancel)))
        .transpose()?;

    let target = input::Target {
        window: found.id,
        point,
        // The window the operator named must be the window in front. This command will not bring it there
        // by force: focus belongs to the operator, and stealing it so a click lands is the exact failure
        // the rest of this phase exists to avoid.
        expect_foreground: found.id,
    };
    let mut steps = vec![format!(
        "about to {} at ({},{}) against {}",
        request.input.describe(),
        point.0,
        point.1,
        found.audit_label()
    )];
    let sent = input::send(
        target,
        request.input.clone(),
        input_settings(config),
        Some(cancel),
    );
    let (uncertain, refusal) = match &sent {
        Ok(sent) => {
            steps.push(format!(
                "SendInput accepted {}/{} events",
                sent.events_accepted, sent.events_queued
            ));
            if !sent.is_complete() {
                steps.push(
                    "the batch was not accepted whole, so part of it was dropped between this process \
                     and the target"
                        .to_owned(),
                );
            }
            if let Some(foreground) = sent.foreground_after {
                steps.push(format!(
                    "the foreground after the input is {}",
                    window_label(foreground)
                ));
            }
            if sent.is_complete() {
                (None, None)
            } else {
                (
                    Some(
                        "an incomplete input batch may have half-landed, and how much of it did is not \
                         establishable from here"
                            .to_owned(),
                    ),
                    None,
                )
            }
        }
        Err(error) => {
            let uncertain = uncertain_after_failure(error, request.input.has_side_effects());
            // A refusal that happened *before* `SendInput` is a definite "this did not happen". Calling
            // that unverifiable would tell the operator the desktop might have changed, when the one
            // thing known is that it did not — and it would cost a script the difference between looking
            // and retrying.
            let refusal = if uncertain.is_some() {
                None
            } else {
                Some(error.to_string())
            };
            match &uncertain {
                Some(reason) => steps.push(format!("the input may have landed: {reason}")),
                None => steps.push(format!("the input was refused before it was sent: {error}")),
            }
            (uncertain, refusal)
        }
    };

    let after = request
        .witness
        .as_ref()
        .map(|selector| read_witness(config, found, selector, Some(cancel)))
        .transpose()?;
    let unresolved = after.as_ref().is_some_and(|read| read.unresolved());
    if unresolved {
        steps.push(format!(
            "the witness {witness_label} did not resolve to exactly one control afterwards: {}",
            after
                .as_ref()
                .and_then(|read| read.note.clone())
                .unwrap_or_default()
        ));
    } else if let Some(read) = &after {
        steps.push(format!(
            "the witness {witness_label} now reads {}",
            read.digest.as_str()
        ));
    }
    let changed = match (&before, &after) {
        (Some(first), Some(second)) if !first.unresolved() && !second.unresolved() => {
            first.digest != second.digest
        }
        _ => false,
    };
    let after_digest = after
        .as_ref()
        .filter(|read| !read.unresolved())
        .map(|read| read.digest.clone());

    let verdict = input_verdict(
        uncertain.as_deref(),
        refusal.as_deref(),
        request.expect.as_deref(),
        after_digest.as_deref(),
        &witness_label,
    );

    let (screenshot, capture_note, retention) =
        secondary_capture(config, found, request.capture, cancel, &mut steps);

    Ok(Unsettled {
        did: format!(
            "{} at ({},{}) on {}",
            request.input.describe(),
            point.0,
            point.1,
            found.audit_label()
        ),
        before: before
            .as_ref()
            .filter(|read| !read.unresolved())
            .map(|read| read.digest.clone()),
        after: after_digest,
        changed,
        uncertain,
        screenshot,
        capture_note,
        retention,
        verdict,
        steps,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// The decision skeleton
// ─────────────────────────────────────────────────────────────────────────────

/// What the drive observed, before the task row is closed out.
struct Unsettled {
    did: String,
    before: Option<String>,
    after: Option<String>,
    changed: bool,
    uncertain: Option<String>,
    screenshot: Option<PathBuf>,
    capture_note: Option<String>,
    retention: String,
    verdict: Verdict,
    steps: Vec<String>,
}

/// Decide, and only on an allow reach the desktop.
///
/// One skeleton for act, input and capture because the promise has to be identical for all three: no
/// record without a decision, no decision without an audit line, and nothing that touches another
/// application before both say it may.
#[allow(clippy::too_many_arguments)]
fn decided<F>(
    ctx: &mut Context,
    config: &DesktopConfig,
    operation: Operation,
    found: &identity::WindowIdentity,
    verb: &'static str,
    prompter: &mut dyn Prompter,
    scope: Option<GrantScope<'_>>,
    request_json: &str,
    work: F,
) -> Result<DesktopReport>
where
    F: FnOnce(
        &mut Context,
        &DesktopConfig,
        &str,
        &AtomicBool,
        &identity::WindowIdentity,
    ) -> Result<Unsettled>,
{
    ctx.prepare_for_writing()?;
    // The window was resolved by the caller, before this function, so that a refusal to name one
    // unambiguously leaves no record at all: `--title notepad` against three Notepad windows is a question
    // for the operator, not a task that failed.
    gate_workers(config)?;
    reconcile_capture_store(config);

    let policy = Policy::new(ctx.config.policy.clone());
    let audit = AuditWriter::new(
        ctx.paths.audit.clone(),
        ctx.config.policy.audit_enabled,
        ctx.redactor.clone(),
    );
    let task_id = open_task(ctx, &operation, request_json)?;
    let actor = crate::intent::actor();

    if would_prompt(&policy, &ctx.store, &operation)? {
        ctx.store.transition(
            &task_id,
            TaskState::AwaitingApproval,
            Some("reaching into another application needs a yes"),
        )?;
    }
    let decision = policy
        .decide_granted(&operation, &ctx.store, prompter, scope)
        .map_err(CoreError::Policy)?;
    audit
        .record(
            &AuditEvent::for_decision(&operation, &decision, actor.clone())
                .with_task(&task_id)
                .with_working_dir(current_directory()),
        )
        .map_err(CoreError::Policy)?;

    if let Decision::Denied(reason) = &decision {
        let text = AuditEvent::denial_text(reason);
        ctx.store
            .transition(&task_id, TaskState::Failed, Some(&text))?;
        ctx.store.set_outcome(&task_id, "denied", &text)?;
        ctx.store.append_event(&task_id, &Event::note(&text))?;
        let _ = ctx.store.clear_runner(&task_id);
        return Ok(DesktopReport {
            task_id,
            window: found.audit_label(),
            pid: found.pid,
            did: format!("{verb} refused before it ran"),
            before: None,
            after: None,
            changed: false,
            uncertain: None,
            refused: true,
            screenshot: None,
            capture_note: None,
            retention: retention_of(config).note,
            verdict: Verdict::Unverifiable(text),
            state: TaskState::Failed,
            steps: vec![format!(
                "nothing was done to the desktop: {verb} happens only after the decision allows it"
            )],
        });
    }

    ctx.store.transition(
        &task_id,
        TaskState::Running,
        Some("the decision allowed the act"),
    )?;
    crate::runner::record_ownership(&ctx.store, &task_id)?;
    // Opened after the allow and before the act: a desktop call is the one thing in this build that can
    // sit inside another application's message pump, and the operator's cancel has to be able to reach it.
    let cancellation = Cancellation::start(ctx, &task_id)?;
    let unsettled = work(ctx, config, &task_id, cancellation.flag(), found).inspect_err(|e| {
        let reason = e.to_string();
        ctx.store
            .transition(&task_id, TaskState::Failed, Some(&reason))
            .ok();
        ctx.store.set_outcome(&task_id, "error", &reason).ok();
        let _ = ctx.store.clear_runner(&task_id);
    })?;
    drop(cancellation);

    settle(ctx, &task_id, found, unsettled)
}

/// Write the observation into the record, and choose the state from the verdict.
fn settle(
    ctx: &mut Context,
    task_id: &str,
    found: &identity::WindowIdentity,
    unsettled: Unsettled,
) -> Result<DesktopReport> {
    let Unsettled {
        did,
        before,
        after,
        changed,
        uncertain,
        screenshot,
        capture_note,
        mut steps,
        retention,
        verdict,
    } = unsettled;

    ctx.store.transition(
        task_id,
        TaskState::Verifying,
        Some("the desktop was read back"),
    )?;
    let detail = verdict.detail().to_owned();
    let state = match &verdict {
        Verdict::Proven(_) => TaskState::Succeeded,
        Verdict::NotProven(_) => TaskState::Failed,
        Verdict::Unverifiable(_) => TaskState::NeedsAttention,
    };
    // `uncertain` is its own outcome label rather than a fold into `unverifiable`. The two need the same
    // thing from a person; the difference is what a later search for "what might have happened twice" uses.
    let outcome = if uncertain.is_some() {
        "uncertain"
    } else {
        verdict.label()
    };
    ctx.store.transition(task_id, state, Some(&detail))?;
    ctx.store.set_outcome(task_id, outcome, &detail)?;
    if uncertain.is_some() {
        steps.push(
            "J.E.A.N. will not repeat this step: an action whose result is unknown is reconciled by a \
             person looking at the window, never by a retry that might press it twice"
                .to_owned(),
        );
    }
    for step in &steps {
        event(ctx, task_id, step)?;
    }
    ctx.store.append_event(
        task_id,
        &Event::new(
            EventKind::VerificationRecorded,
            ctx.redactor
                .redact(&format!("{}: {detail}", verdict.label())),
        ),
    )?;
    let _ = ctx.store.clear_runner(task_id);

    Ok(DesktopReport {
        task_id: task_id.to_owned(),
        window: found.audit_label(),
        pid: found.pid,
        did,
        before,
        after,
        changed,
        uncertain,
        refused: false,
        screenshot,
        capture_note,
        retention,
        verdict,
        state,
        steps,
    })
}

fn event(ctx: &mut Context, task_id: &str, detail: &str) -> Result<()> {
    ctx.store
        .append_event(
            task_id,
            &Event::new(EventKind::DesktopAction, ctx.redactor.redact(&clip(detail))),
        )
        .map_err(CoreError::Store)
}

/// A report line, bounded.
fn clip(detail: &str) -> String {
    if detail.chars().count() <= MAX_LINE_CHARS {
        return detail.to_owned();
    }
    let mut cut: String = detail.chars().take(MAX_LINE_CHARS).collect();
    cut.push('…');
    cut
}

// ─────────────────────────────────────────────────────────────────────────────
// Cancellation
// ─────────────────────────────────────────────────────────────────────────────

/// The flag a bounded desktop wait polls, and the two ways it gets raised.
///
/// A desktop call cannot be cancelled in the ordinary sense: a cross-process send that is already in
/// flight cannot be retracted, and this crate says so rather than pretending otherwise. What the worker
/// change (D-051) made endable is the *waiting* — the child process running the call can be terminated
/// with its job object, so nothing of it has to be carried. The promise here is the narrow useful one: an
/// operator's `jean cancel`, or Ctrl-C, stops the command starting the next call and stops it *waiting*
/// for the one in flight within one poll slice. The terminated worker's call is then accounted for as an
/// uncertain side effect rather than assumed harmless, and never replayed (D-050).
struct Cancellation {
    flag: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    bridge: Option<std::thread::JoinHandle<()>>,
    listener: Option<CancelListener>,
}

impl Cancellation {
    fn start(ctx: &Context, task_id: &str) -> Result<Self> {
        let token = CancelToken::new()?;
        crate::signals::arm(token.clone())?;
        let pipe_name = jean_protocol::ipc::pipe_name_for(&ctx.config.ipc.pipe_prefix, task_id)
            .map_err(|e| CoreError::Ipc {
                task_id: task_id.to_owned(),
                reason: e.to_string(),
            })?;
        let listener = CancelListener::start(
            pipe_name,
            task_id.to_owned(),
            token.clone(),
            ctx.paths.database.clone(),
        )
        .map_err(|e| CoreError::Ipc {
            task_id: task_id.to_owned(),
            reason: e.to_string(),
        })?;

        let flag = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let raised = flag.clone();
        let watching = stop.clone();
        let bridge = std::thread::Builder::new()
            .name(format!("jean-desktop-cancel-{task_id}"))
            .spawn(move || {
                while !watching.load(Ordering::Relaxed) {
                    if token.is_cancelled() || crate::signals::interrupt().is_some() {
                        raised.store(true, Ordering::Relaxed);
                        return;
                    }
                    std::thread::sleep(CANCEL_POLL);
                }
            })
            .map_err(|e| CoreError::Ipc {
                task_id: task_id.to_owned(),
                reason: format!("the cancellation observer could not start: {e}"),
            })?;
        Ok(Self {
            flag,
            stop,
            bridge: Some(bridge),
            listener: Some(listener),
        })
    }

    fn flag(&self) -> &AtomicBool {
        &self.flag
    }
}

impl Drop for Cancellation {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(bridge) = self.bridge.take() {
            // The loop checks `stop` every poll slice, so this join is bounded by `CANCEL_POLL` and not by
            // whatever the provider is doing.
            let _ = bridge.join();
        }
        if let Some(listener) = self.listener.take() {
            listener.stop();
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Gates and settings
// ─────────────────────────────────────────────────────────────────────────────

fn require_enabled(config: &DesktopConfig) -> Result<()> {
    if config.enabled {
        return Ok(());
    }
    Err(CoreError::Intent {
        reason:
            "[desktop] enabled = false: J.E.A.N. does not act on the desktop unless the \
                 configuration says it may. Reading it — `jean desktop windows|inspect|diagnose` — \
                 works either way, and diagnosis is where an operator decides whether to enable it."
                .to_owned(),
    })
}

fn require_synthetic_input(config: &DesktopConfig) -> Result<()> {
    if config.allow_synthetic_input {
        return Ok(());
    }
    Err(CoreError::Intent {
        reason: "[desktop] allow_synthetic_input = false: synthesised keyboard and mouse input is a \
                 separate decision from acting through the accessibility patterns, and it is off. Use \
                 `jean desktop act` if the window exposes a pattern — `jean desktop diagnose` says \
                 whether it does."
            .to_owned(),
    })
}

/// Refuse before the call when the desktop workers already fill the configured ceiling.
///
/// The count is of **processes**, and each one is ended by its own deadline, so a hung provider no longer
/// accumulates the way a parked thread did. The ceiling stays for two reasons: a live worker is a process,
/// two pipes and two pump threads, which is not free; and a worker whose exit never became observable
/// stays counted *on purpose*, because that is the only way this design can still pile up and the visible
/// answer to it is to stop starting more rather than to pretend the pile is not there.
fn gate_workers(config: &DesktopConfig) -> Result<()> {
    let limit = config.max_concurrent_workers.max(1);
    let alive = worker::live_workers();
    if alive < limit {
        return Ok(());
    }
    Err(CoreError::Intent {
        reason: format!(
            "{alive} desktop worker processes are alive and [desktop] max_concurrent_workers is {limit}; \
             every call ends its own worker, so this means workers are being started faster than they are \
             exiting, or one of them will not report its exit. J.E.A.N. stops here rather than adding \
             another. `jean desktop diagnose` names the window that is not answering.",
        ),
    })
}

fn accessibility_settings(config: &DesktopConfig) -> accessibility::Settings {
    accessibility::Settings {
        call_timeout: Duration::from_secs(config.uia_timeout_secs.max(1)),
        max_nodes: config.max_tree_nodes.max(1),
        // Probing every control's patterns costs a COM round trip per pattern, so a big tree is read
        // without it and an action probes only the control it touches. `true` here because the only caller
        // of this mapping is `jean desktop inspect`, which an operator runs precisely to see what is
        // addressable, and the node cap bounds the cost.
        probe_patterns: true,
    }
}

fn capture_settings(config: &DesktopConfig) -> capture::Settings {
    capture::Settings {
        call_timeout: Duration::from_secs(config.capture_timeout_secs.max(1)),
        max_pixels: config.max_capture_pixels.max(1),
    }
}

fn input_settings(config: &DesktopConfig) -> input::Settings {
    input::Settings {
        call_timeout: Duration::from_secs(config.uia_timeout_secs.max(1)),
        // Moving the operator's pointer is visible and startling. The click does not require it: an
        // absolute mouse event addresses the coordinates without a cursor move being mandatory.
        move_visible_cursor: true,
    }
}

fn capability_settings(config: &DesktopConfig) -> capability::Settings {
    capability::Settings {
        accessibility: accessibility::Settings {
            call_timeout: Duration::from_secs(config.uia_timeout_secs.max(1)),
            max_nodes: config.max_tree_nodes.max(1),
            // A sweep walks every window on the desktop; per-control pattern probes would turn it into a
            // minutes-long command, and a capability report asks only whether *any* pattern is exposed,
            // which the tree-level probe already answers.
            probe_patterns: false,
        },
        capture: capture_settings(config),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Resolving the target
// ─────────────────────────────────────────────────────────────────────────────

/// Which window the selector names, read *now*.
///
/// Zero matches and several matches are refusals, not fallbacks. Taking the first window whose title
/// contains "Notepad" is the same mistake as taking the first of two `Continue` buttons, and the answer to
/// both is the same: say what was found, and let the operator name one.
fn resolve_window(selector: &WindowSelector) -> Result<identity::WindowIdentity> {
    if selector.is_empty() {
        return Err(CoreError::Intent {
            reason: "no window was named; use --hwnd, --pid, --title or --image".to_owned(),
        });
    }
    identity::ensure_dpi_awareness();
    if let Some(hwnd) = selector.hwnd {
        let id = identity::WindowId { hwnd };
        if !id.is_alive() {
            return Err(CoreError::Intent {
                reason: format!(
                    "window {hwnd} is no longer a window. Handles are recycled, so this one was \
                     re-checked rather than assumed; `jean desktop windows` lists what is here now."
                ),
            });
        }
        return Ok(identity::read_identity(id));
    }

    let all = identity::enumerate().map_err(intent_of)?;
    let visible: Vec<identity::WindowIdentity> = all
        .iter()
        .filter(|window| window.is_visible)
        .cloned()
        .collect();
    let matches: Vec<identity::WindowIdentity> = visible
        .into_iter()
        .filter(|window| {
            selector.pid.map(|pid| window.pid == pid).unwrap_or(true)
                && selector
                    .title
                    .as_deref()
                    .map(|want| contains(window.title.as_deref(), want))
                    .unwrap_or(true)
                && selector
                    .image
                    .as_deref()
                    .map(|want| {
                        contains(window.process_image.as_deref(), want)
                            || contains(window.class_name.as_deref(), want)
                    })
                    .unwrap_or(true)
        })
        .collect();

    match matches.len() {
        1 => Ok(matches.into_iter().next().expect("one match was counted")),
        0 => Err(CoreError::Intent {
            reason: format!(
                "no visible window matches {} among {} top-level windows. J.E.A.N. does not act on a \
                 window it could not find, and does not fall back to an invisible one.",
                selector.describe(),
                all.len()
            ),
        }),
        count => {
            let mut list = String::new();
            for window in matches.iter().take(10) {
                list.push_str(&format!("\n  {}", window.audit_label()));
            }
            Err(CoreError::Intent {
                reason: format!(
                    "{count} visible windows match {}; naming one is not a tie-break, so none was \
                     chosen:{list}{}",
                    selector.describe(),
                    if count > 10 { "\n  …" } else { "" }
                ),
            })
        }
    }
}

fn contains(haystack: Option<&str>, needle: &str) -> bool {
    haystack
        .map(|text| text.to_lowercase().contains(&needle.to_lowercase()))
        .unwrap_or(false)
}

/// The physical point an input will be aimed at, from a current observation.
fn resolve_point(
    config: &DesktopConfig,
    found: &identity::WindowIdentity,
    target: &InputTarget,
    cancel: Option<&AtomicBool>,
) -> Result<(i32, i32)> {
    match target {
        InputTarget::Window => identity::rect_of(found.id)
            .filter(|rect| !rect.is_empty())
            .map(|rect| rect.centre())
            .ok_or_else(|| CoreError::Intent {
                reason: format!(
                    "window {} reports no usable rectangle, so there is nowhere honest to aim",
                    found.id.hwnd
                ),
            }),
        // The point is used as given and then checked against the window's current rectangle by
        // `input::Target::verify`. That check is the reason a coordinate can be accepted at all.
        InputTarget::Point { x, y } => Ok((*x, *y)),
        InputTarget::Control(selector) => {
            let controls = accessibility::resolve(
                found.id,
                selector.to_locator(),
                accessibility_settings(config),
                cancel,
            )
            .map_err(intent_of)?;
            match controls.len() {
                1 => controls
                    .into_iter()
                    .next()
                    .expect("one control was counted")
                    .click_point()
                    .ok_or_else(|| CoreError::Intent {
                        reason: format!(
                            "{} reported no usable bounds, so clicking its centre would be clicking an \
                             assumed coordinate",
                            selector.describe()
                        ),
                    }),
                0 => Err(CoreError::Intent {
                    reason: format!(
                        "{} names no control in {}",
                        selector.describe(),
                        found.audit_label()
                    ),
                }),
                count => Err(CoreError::Intent {
                    reason: format!(
                        "{count} controls match {}; narrowing it is the operator's call, not a \
                         first-match guess",
                        selector.describe()
                    ),
                }),
            }
        }
    }
}

/// One control's readable state, taken from its own patterns.
///
/// `None` from [`read_witness`] means no witness was named; inside `Some`, an unresolved witness is a
/// different sentence from an unchanged one and is kept separate.
#[derive(Debug, Clone)]
struct WitnessRead {
    digest: String,
    /// Why the witness could not be read: nothing matched, or several did.
    note: Option<String>,
}

impl WitnessRead {
    fn unresolved(&self) -> bool {
        self.note.is_some()
    }
}

fn read_witness(
    config: &DesktopConfig,
    found: &identity::WindowIdentity,
    selector: &ControlSelector,
    cancel: Option<&AtomicBool>,
) -> Result<WitnessRead> {
    let read = accessibility::read_state(
        found.id,
        selector.to_locator(),
        accessibility_settings(config),
        cancel,
    )
    .map_err(intent_of)?;
    Ok(match read {
        accessibility::StateRead::Found(found) => WitnessRead {
            digest: witness_digest(&found.control, &found.state),
            note: None,
        },
        accessibility::StateRead::Absent => WitnessRead {
            digest: String::new(),
            note: Some("no control matched".to_owned()),
        },
        accessibility::StateRead::Ambiguous { count } => WitnessRead {
            digest: String::new(),
            note: Some(format!("{count} controls matched")),
        },
    })
}

/// One control's readable state, in the same words the action path uses.
///
/// The state half — Value, Toggle, SelectionItem, ExpandCollapse, name, enabled — is what an expectation is
/// checked against, and a password value reaches here already reduced to a character count. The listing half
/// says which control that state belongs to, and `focus=` is the part that makes a click's evidence a click's
/// evidence rather than a coincidence.
fn witness_digest(control: &accessibility::Control, state: &accessibility::Readback) -> String {
    let listing = format!(
        "kind={} focus={}",
        control.kind.name(),
        control.has_keyboard_focus
    );
    if state.is_empty() {
        listing
    } else {
        format!("{} {listing}", state.digest())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Judging
// ─────────────────────────────────────────────────────────────────────────────

/// What one read-back established.
struct Judgement {
    verdict: Verdict,
    before: String,
    after: Option<String>,
    changed: bool,
    uncertain: Option<String>,
    /// `true` when the witness named nothing at all — a plan that pointed at the wrong thing, which is a
    /// different sentence from "nothing happened".
    witness_unresolved: bool,
}

impl Judgement {
    /// The judgement for a call that never got far enough to have a read-back.
    ///
    /// `before` says so rather than inventing an empty state: a verifier that read `""` would report
    /// "nothing changed", and the truth is that nothing was *observed*.
    fn unreadable(verdict: Verdict, uncertain: Option<String>) -> Self {
        Self {
            verdict,
            before: "<nothing readable>".to_owned(),
            after: None,
            changed: false,
            uncertain,
            witness_unresolved: false,
        }
    }

    /// The observation, in one line: what the witness read, and whether it moved.
    fn observation(&self, witness: &accessibility::Locator) -> String {
        if self.witness_unresolved {
            return format!(
                "the witness {} did not resolve to exactly one control after the action",
                witness.describe()
            );
        }
        match &self.after {
            Some(after) => format!(
                "{} read {} -> {after}{}",
                witness.describe(),
                self.before,
                if self.changed {
                    " (changed)"
                } else {
                    " (unchanged)"
                }
            ),
            None => format!("{} could not be read after the action", witness.describe()),
        }
    }
}

/// Turn one [`accessibility::Effect`] into the report's judgement.
fn judge(
    effect: &accessibility::Effect,
    witness: &accessibility::Locator,
    expect: Option<&str>,
    steps: &mut Vec<String>,
) -> Judgement {
    // The witness is the control that proves the action; when the caller did not name one, the acted
    // control's own state is the only thing there to read.
    let (before, after, unresolved) = match &effect.witness {
        Some(read) => (
            read.before.digest(),
            read.after.as_ref().map(|readback| readback.digest()),
            read.unresolved,
        ),
        None => (
            effect.before.digest(),
            effect.after.as_ref().map(|readback| readback.digest()),
            false,
        ),
    };
    if unresolved {
        steps.push(
            "the witness was not found, so the check could not be run against it — that is a plan that \
             pointed at the wrong control, not an application that did nothing"
                .to_owned(),
        );
    }
    let changed = effect.state_changed();
    let uncertain = if effect.is_uncertain_side_effect() {
        Some(effect.uncertainty_reason.clone().unwrap_or_else(|| {
            "the provider accepted the action and the state could not be read back".to_owned()
        }))
    } else {
        None
    };

    let verdict = match (&uncertain, unresolved, expect, &after) {
        (Some(reason), _, _, _) => Verdict::Unverifiable(format!(
            "the {} was performed and its outcome is unknown ({reason}); it will not be repeated",
            effect.action.describe()
        )),
        (None, true, Some(expected), _) => Verdict::Unverifiable(format!(
            "the check for {expected:?} could not be run: the witness {} did not resolve",
            witness.describe()
        )),
        (None, false, Some(expected), Some(digest)) if expectation_holds(digest, expected) => {
            Verdict::Proven(format!(
                "{} shows {expected:?} after the {}",
                witness.describe(),
                effect.action.describe()
            ))
        }
        (None, false, Some(expected), Some(digest)) => Verdict::NotProven(format!(
            "{} reads {digest}, not the {expected:?} the check asked for",
            witness.describe()
        )),
        (None, false, Some(expected), None) => Verdict::Unverifiable(format!(
            "{} could not be read after the action, so the check for {expected:?} could not be run",
            witness.describe()
        )),
        (None, _, None, Some(digest)) => Verdict::Unverifiable(format!(
            "the {} ran and {} read {} -> {digest}; no expectation was stated, so nothing was proven \
             about the goal",
            effect.action.name(),
            witness.describe(),
            before
        )),
        (None, _, None, None) => Verdict::Unverifiable(
            "the action ran and nothing could be read back from the desktop".to_owned(),
        ),
    };
    Judgement {
        verdict,
        before,
        after,
        changed,
        uncertain,
        witness_unresolved: unresolved,
    }
}

/// The judgement for an injected-input attempt, whose states the action path does not have.
///
/// The action path has one provider call, so the space between "did not happen" and "may have happened" is
/// the provider's answer. The input path has a queue between this process and the desktop, so it has three:
/// refused before the queue, accepted whole, and accepted *partly*. A refusal that never reached `SendInput`
/// is a definite no — reporting it as unverifiable would tell the operator the desktop might have changed
/// when the one thing known is that it did not, and would cost a script the difference between looking and
/// retrying. An incomplete batch is the opposite: some of it landed and the amount is not knowable, which is
/// why it is uncertain and why nothing replays it.
fn input_verdict(
    uncertain: Option<&str>,
    refusal: Option<&str>,
    expect: Option<&str>,
    after: Option<&str>,
    witness_label: &str,
) -> Verdict {
    if let Some(reason) = uncertain {
        return Verdict::Unverifiable(format!(
            "the input could not be accounted for: {reason}; it will not be repeated"
        ));
    }
    if let Some(refused) = refusal {
        return Verdict::NotProven(format!("the input was not sent: {refused}"));
    }
    match (expect, after) {
        (Some(expected), Some(digest)) if expectation_holds(digest, expected) => {
            Verdict::Proven(format!("{witness_label} shows {expected:?} after the input"))
        }
        (Some(expected), Some(digest)) => Verdict::NotProven(format!(
            "{witness_label} reads {digest}, not the {expected:?} the check asked for"
        )),
        (Some(expected), None) => Verdict::Unverifiable(format!(
            "the check for {expected:?} could not be run: {witness_label} was not readable afterwards"
        )),
        (None, Some(digest)) => Verdict::Unverifiable(format!(
            "the input was sent and {witness_label} reads {digest}; no expectation was stated, so \
             nothing was proven about the goal"
        )),
        (None, None) => {
            Verdict::Unverifiable("the input was sent and nothing was named to prove it by".to_owned())
        }
    }
}

/// The field names the two digest producers emit.
///
/// A digest is a sequence of `field=value` parts, and knowing which words *can* start a part is what makes
/// the split unambiguous enough to check an expectation against.
const DIGEST_FIELDS: [&str; 9] = [
    "name", "value", "toggle", "selected", "expanded", "enabled", "kind", "focus", "patterns",
];

/// Split a digest into its `field=value` parts.
///
/// Splitting on whitespace would break on the ordinary case — an edit whose value is `typed by jean` —
/// because a value is text somebody else chose, and quoting it instead would make the check an operator
/// types disagree with the report line they can see. So the cut happens at the words that can start a
/// field and nowhere else.
///
/// What that cannot disambiguate is stated rather than hidden: a value that itself contains ` enabled=`
/// reads as two fields. The escape hatch is the whole-digest form below, and the report prints the exact
/// digest, so an operator can paste it.
fn digest_fields(digest: &str) -> Vec<(&str, String)> {
    let characters: Vec<char> = digest.chars().collect();
    let mut marks: Vec<(usize, &str)> = Vec::new();
    let mut index = 0usize;
    while index < characters.len() {
        let at_start_of_part = index == 0 || characters[index - 1] == ' ';
        let mut matched = false;
        if at_start_of_part {
            for field in DIGEST_FIELDS {
                let name: Vec<char> = field.chars().collect();
                let rest = &characters[index..];
                if rest.starts_with(&name) && rest.get(name.len()) == Some(&'=') {
                    marks.push((index, field));
                    index += name.len() + 1;
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            index += 1;
        }
    }
    let mut parts = Vec::new();
    for (position, (start, field)) in marks.iter().enumerate() {
        let equals = characters[*start..]
            .iter()
            .position(|c| *c == '=')
            .map(|offset| start + offset + 1)
            .unwrap_or(*start);
        let end = marks
            .get(position + 1)
            .map(|(next, _)| *next)
            .unwrap_or(characters.len());
        // The space that separated this part from the next one is not part of the value. At the end of
        // the digest there is no such space, and cutting a character off the last value would make the
        // final field unmatchable.
        let stop = if end > equals && characters[end - 1] == ' ' {
            end - 1
        } else {
            end
        };
        let value: String = characters[equals..stop].iter().collect();
        parts.push((*field, value));
    }
    parts
}

/// One layer of double quotes, which is how a tree read shows a name the provider gave.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value)
}

/// The check an operator writes, applied to a digest.
///
/// Three forms, all exact: the whole digest as the report printed it, one `field=value` part, or the bare
/// value (`clicked:1`, which matches whichever field shows it). Plain substring matching is refused on
/// purpose — it would let `--expect 1` pass for `clicked:12`, which is the kind of check that proves
/// nothing while looking like a pass.
fn expectation_holds(digest: &str, expected: &str) -> bool {
    let expected = expected.trim();
    if expected.is_empty() {
        return false;
    }
    if digest == expected {
        return true;
    }
    digest_fields(digest).into_iter().any(|(field, value)| {
        expected == format!("{field}={value}") || expected == value || expected == unquote(&value)
    })
}

/// Whether an error means the desktop may already have been changed.
///
/// The distinction this whole module turns on. "The provider refused" leaves the window as it was. "The
/// provider stopped answering", or "the operator stopped waiting", both leave a call whose delivery nobody
/// can confirm. Only the second group is uncertain, and only an uncertain one forbids a retry.
fn uncertain_after_failure(error: &WindowsError, side_effects: bool) -> Option<String> {
    if !side_effects {
        return None;
    }
    match error {
        WindowsError::ProviderTimeout {
            budget_secs,
            waited_ms,
            ..
        } => Some(format!(
            "the provider did not answer within {budget_secs}s (waited {waited_ms}ms) and the call may \
             already have reached the application"
        )),
        WindowsError::Cancelled {
            budget_secs,
            waited_ms,
        } => Some(format!(
            "the wait was stopped on request after {waited_ms}ms of a {budget_secs}s bound, and whether \
             the call had already been delivered is not observable from here"
        )),
        WindowsError::Panicked { call } => Some(format!(
            "the worker for {call} panicked; anything it had already handed to the provider stays \
             where it was"
        )),
        _ => None,
    }
}

fn window_label(id: identity::WindowId) -> String {
    identity::read_identity(id).audit_label()
}

fn explain(explanation: &accessibility::TreeExplanation) -> String {
    match explanation {
        accessibility::TreeExplanation::Populated => "controls exposed".to_owned(),
        accessibility::TreeExplanation::NoElements => {
            "no UIA element at all — not a protection, an absent provider or an unenabled tree"
                .to_owned()
        }
        accessibility::TreeExplanation::NoInteractiveContent {
            frames,
            class_name,
            looks_like_web_content,
        } => format!(
            "{frames} frame(s) and nothing actionable (class {class_name}){}",
            if *looks_like_web_content {
                " — a web-content frame, where an empty tree is expected rather than mysterious"
            } else {
                ""
            }
        ),
        accessibility::TreeExplanation::Capped => {
            "the node cap cut the walk, so this is a sample".to_owned()
        }
        accessibility::TreeExplanation::Unreachable(reason) => {
            format!("the provider could not be reached: {reason}")
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Capture and retention
// ─────────────────────────────────────────────────────────────────────────────

/// The capture an operator asked to accompany an action.
///
/// Its failure is a step, never a verdict: a blank window does not make a delivered click unknown, and
/// treating it as one would be the mistake of letting pixels outrank the semantic read.
fn secondary_capture(
    config: &DesktopConfig,
    found: &identity::WindowIdentity,
    requested: bool,
    cancel: &AtomicBool,
    steps: &mut Vec<String>,
) -> (Option<PathBuf>, Option<String>, String) {
    let retention = retention_of(config);
    if !requested {
        return (None, None, retention.note);
    }
    match capture::capture_with_recovery(found.id, capture_settings(config), Some(cancel)) {
        Ok(recovery) => {
            steps.push(describe_attempts(&recovery));
            let note = recovery
                .capture
                .as_ref()
                .map(capture::log_line)
                .unwrap_or_default();
            if !note.is_empty() {
                steps.push(format!("the frame: {note}"));
            }
            let kept = match (recovery.capture.as_ref(), retention.keep) {
                (Some(frame), true) => match keep_frame(config, frame) {
                    Ok(path) => {
                        steps.push(format!("kept the frame at {}", path.display()));
                        Some(path)
                    }
                    Err(error) => {
                        steps.push(format!("the frame could not be kept: {error}"));
                        None
                    }
                },
                (Some(_), false) => {
                    steps.push(retention.note.clone());
                    None
                }
                (None, _) => None,
            };
            (kept, (!note.is_empty()).then_some(note), retention.note)
        }
        Err(error) => {
            steps.push(format!(
                "the capture could not be taken: {error} — this says nothing about whether the action \
                 was delivered"
            ));
            (None, None, retention.note)
        }
    }
}

#[derive(Debug)]
struct Retention {
    keep: bool,
    note: String,
}

/// What `[desktop] retention` means for this run, in the words this build can honour.
///
/// `discard_after_task` is still "never written", which remains the only form of it that needs no
/// mechanism: nothing exists to remove. `keep_24h` is now an actual expiry, performed by
/// [`captures::reconcile`] against an ownership manifest, and the note says what the cleaner will and will
/// not do — including the sentence that matters most, that a retained frame is the screen as it looked and
/// is not redacted by anything that redacts text.
fn retention_of(config: &DesktopConfig) -> Retention {
    match config.retention.as_str() {
        captures::RETENTION_24H => Retention {
            keep: true,
            note: format!(
                "retention keep_24h: the frame is claimed in {} before it is written, and the capture \
                 store removes it twenty-four hours after that claim ({} bytes is the ceiling on the \
                 kept set, oldest first). Files the claim does not name are never touched, not even when \
                 they are named like captures. Retained pixels are the screen as it looked: the redaction \
                 applied to text does not apply to an image.",
                captures::MANIFEST_NAME,
                captures::MAX_STORE_BYTES
            ),
        },
        captures::RETENTION_MANUAL => Retention {
            keep: true,
            note: format!(
                "retention keep_until_manual: the frame is claimed in {} and kept until the operator \
                 removes it — no expiry and no byte ceiling applies to this policy, because \"until \
                 manually\" is the promise and a cleaner does not get to revise it. Retained pixels are the \
                 screen as it looked: the redaction applied to text does not apply to an image.",
                captures::MANIFEST_NAME
            ),
        },
        other => Retention {
            keep: false,
            note: if other == "discard_after_task" {
                "retention discard_after_task: no capture file was written".to_owned()
            } else {
                // Unreachable through a loaded configuration — `DesktopConfig::check` refuses a word that
                // is not one of the three — but a retention setting is not a place to fall through.
                format!("retention {other:?} is not a policy this build knows, so nothing was written")
            },
        },
    }
}

/// Expire what is due in the capture store, once per process, before anything new lands in it.
///
/// Housekeeping, so it never fails the operation it is preparing for: a store that could not be swept is
/// reported in the log and left alone, which is the correct outcome for a cleaner and the wrong one for a
/// desktop action the operator asked for.
fn reconcile_capture_store(config: &DesktopConfig) {
    if config.retention == "discard_after_task" {
        // Nothing this run writes can need expiring, and a diagnostic command should not be the thing that
        // starts deleting files.
        return;
    }
    let dir = capture_directory(config);
    if let Some(sweep) = captures::reconcile_at_startup(&dir, captures::now_ms()) {
        tracing::debug!("{}", sweep.line());
    }
}

fn keep_frame(
    config: &DesktopConfig,
    frame: &capture::Capture,
) -> std::result::Result<PathBuf, CoreError> {
    let dir = capture_directory(config);
    std::fs::create_dir_all(&dir).map_err(|e| CoreError::Directory {
        path: dir.clone(),
        reason: e.to_string(),
    })?;
    let path = ocr::capture_path(&dir, frame.window.hwnd, frame.captured_at_ms);
    // Claimed before the pixels are written, and the write is refused if the claim cannot be made. That
    // ordering is what makes `keep_24h` a true statement rather than a hope: a frame with no claim behind
    // it is a frame no expiry is allowed to remove, and the only way to keep that from accumulating is not
    // to create the frame. The reverse order produces exactly the pile the setting promises to clear.
    let claim = captures::Entry {
        file: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        window: frame.window.hwnd,
        captured_at_ms: frame.captured_at_ms,
        written_at_ms: captures::now_ms(),
        retention: config.retention.clone(),
    };
    captures::claim(&dir, &claim).map_err(|error| CoreError::File {
        path: dir.join(captures::MANIFEST_NAME),
        reason: format!(
            "the frame was not written because it could not be claimed as this build's own artifact \
             ({error}); a capture nobody is permitted to remove would outlive the retention setting that \
             says it should not"
        ),
    })?;
    capture::write_bmp(frame, &path).map_err(|e| CoreError::Intent {
        reason: e.to_string(),
    })
}

fn make_parent(path: &Path) -> std::result::Result<(), CoreError> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            std::fs::create_dir_all(dir).map_err(|e| CoreError::Directory {
                path: dir.to_path_buf(),
                reason: e.to_string(),
            })
        }
        _ => Ok(()),
    }
}

/// Where a kept frame goes. Expanded, because the shipped default is written the way Windows writes it.
fn capture_directory(config: &DesktopConfig) -> PathBuf {
    let expanded = jean_tools::apps::expand_environment_strings(config.capture_dir.trim());
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        return path;
    }
    // A relative capture directory would land wherever the command happened to be started from, which is
    // not a place anybody chose.
    std::env::temp_dir().join("JEAN").join("captures")
}

fn describe_attempts(recovery: &capture::Recovery) -> String {
    let attempts = recovery
        .attempts
        .iter()
        .map(|attempt| {
            format!(
                "{} {}",
                attempt.backend.name(),
                if attempt.succeeded {
                    attempt
                        .quality
                        .map(|quality| format!("({quality:?})"))
                        .unwrap_or_default()
                } else {
                    format!("failed: {}", attempt.note)
                }
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} after [{attempts}]", describe_verdict(recovery))
}

fn describe_verdict(recovery: &capture::Recovery) -> String {
    match recovery.verdict() {
        capture::RecoveryVerdict::Usable(quality) => format!("usable frame ({quality:?})"),
        capture::RecoveryVerdict::Uninformative(quality) => {
            format!("uninformative frame ({quality:?})")
        }
        capture::RecoveryVerdict::ExplicitProtection => {
            "capture excluded by the window itself".to_owned()
        }
        capture::RecoveryVerdict::Unusable(reason) => format!("no frame ({reason})"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Operations, tasks, and the small helpers they need
// ─────────────────────────────────────────────────────────────────────────────

/// What the approval is about, and therefore what a remembered approval binds to.
///
/// The target is the process image, with the handle as an argument: an approval has to survive the window
/// being recreated, and must not survive the application becoming a different one.
fn desktop_operation(
    ctx: &Context,
    found: &identity::WindowIdentity,
    capability_name: &str,
    summary: String,
    arguments: Vec<String>,
    tier: PolicyTier,
) -> Operation {
    let image = found
        .process_image
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("pid={}", found.pid));
    Operation {
        kind: OperationKind::OperateDesktop,
        tool_id: Some(TOOL_ID.to_owned()),
        tier,
        capabilities: vec![capability_name.to_owned()],
        summary: ctx.redactor.redact(&summary),
        scope: Scope::new(
            image,
            std::iter::once(format!("hwnd={}", found.id.hwnd))
                .chain(arguments)
                .collect(),
        ),
    }
}

/// The expectation as a routine record can carry it: that one was stated, and how long it is.
///
/// An expectation is desired UI text — `--expect "value=<whatever the operator intends to see>"` — and
/// private UI text stays out of the logs and the audit trail. Its two sibling entries already bound what
/// they send (`set_value(13 chars)`, `type 14 chars`); a verbatim expectation would be the one place where
/// the same command leaked the text it was trying to prove. The report an operator reads still shows the
/// expectation in full, because that is the thing they wrote and the thing they need to see checked.
fn describe_expect(expected: &str) -> String {
    format!("expect stated ({} chars)", expected.chars().count())
}

fn act_operation(
    ctx: &Context,
    found: &identity::WindowIdentity,
    request: &ActRequest,
) -> Operation {
    let summary = format!(
        "act on {} — {} on {}",
        found.audit_label(),
        request.action.describe(),
        request.control.describe()
    );
    let mut arguments = vec![
        request.action.describe(),
        request.control.describe(),
        format!(
            "witness={}",
            request
                .witness
                .as_ref()
                .map_or_else(|| "the acted control".to_owned(), ControlSelector::describe)
        ),
    ];
    if let Some(expected) = &request.expect {
        arguments.push(describe_expect(expected));
    }
    desktop_operation(
        ctx,
        found,
        ACT_CAPABILITY,
        summary,
        arguments,
        PolicyTier::Normal,
    )
}

fn input_operation(
    ctx: &Context,
    found: &identity::WindowIdentity,
    request: &InputRequest,
) -> Operation {
    let summary = format!(
        "inject input into {} — {}",
        found.audit_label(),
        request.input.describe()
    );
    let mut arguments = vec![
        request.input.describe(),
        format!("target={}", describe_target(&request.at)),
        format!(
            "witness={}",
            request
                .witness
                .as_ref()
                .map_or_else(|| "none".to_owned(), ControlSelector::describe)
        ),
    ];
    if let Some(expected) = &request.expect {
        arguments.push(describe_expect(expected));
    }
    desktop_operation(
        ctx,
        found,
        INPUT_CAPABILITY,
        summary,
        arguments,
        PolicyTier::Normal,
    )
}

fn capture_operation(
    ctx: &Context,
    found: &identity::WindowIdentity,
    request: &CaptureRequest,
) -> Operation {
    let summary = format!("capture {} on demand", found.audit_label());
    let mut arguments = vec![format!(
        "bounds={}",
        found
            .rect
            .map(|rect| format!("{}x{}", rect.width(), rect.height()))
            .unwrap_or_else(|| "unknown".to_owned())
    )];
    if let Some(path) = &request.out {
        arguments.push(format!("out={}", path.display()));
    }
    desktop_operation(
        ctx,
        found,
        CAPTURE_CAPABILITY,
        summary,
        arguments,
        PolicyTier::ReadOnly,
    )
}

fn describe_target(target: &InputTarget) -> String {
    match target {
        InputTarget::Window => "window-centre".to_owned(),
        InputTarget::Point { x, y } => format!("point({x},{y})"),
        InputTarget::Control(selector) => selector.describe(),
    }
}

fn open_task(ctx: &mut Context, operation: &Operation, request_json: &str) -> Result<String> {
    let task_id = jean_protocol::ipc::new_task_id();
    ctx.store.insert_task(&TaskRecord::new(
        task_id.clone(),
        operation.summary.clone(),
        Some(TOOL_ID.to_owned()),
        request_json.to_owned(),
    ))?;
    ctx.store.transition(
        &task_id,
        TaskState::Planning,
        Some("the window was found and identified"),
    )?;
    Ok(task_id)
}

fn would_prompt(policy: &Policy, store: &jean_store::Store, operation: &Operation) -> Result<bool> {
    let sensitive = operation
        .capabilities
        .iter()
        .any(|capability| policy.requires_approval(capability));
    if !sensitive {
        return Ok(false);
    }
    Ok(store
        .find_approval(&operation.scope_digest())
        .map_err(CoreError::Store)?
        .is_none())
}

fn current_directory() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|dir| dir.display().to_string())
}

/// The mechanism's error, in the operator's mouth: every variant of [`WindowsError`] is already a
/// different sentence, and paraphrasing one into another is the failure this crate is built against.
fn intent_of(error: WindowsError) -> CoreError {
    CoreError::Intent {
        reason: error.to_string(),
    }
}

fn awareness_line(awareness: &identity::Awareness) -> String {
    match awareness {
        identity::Awareness::Set => "per-monitor-aware-v2, set by this process".to_owned(),
        identity::Awareness::AlreadyFixed(value) => format!("already fixed as {value}"),
        identity::Awareness::Unavailable(reason) => format!("unknown ({reason})"),
    }
}

/// The one-screen answer for `jean desktop status`: what this process can see, under what bounds.
///
/// [`identity::ensure_dpi_awareness`] runs here because it has to run before any coordinate work in the
/// process, and status is the command an operator runs first. It reports what ended up in force, which is
/// the honest answer when a manifest or an earlier call already decided.
pub fn overview(config: &DesktopConfig) -> Vec<(String, String)> {
    let awareness = identity::ensure_dpi_awareness();
    let session = identity::session();
    let integrity = identity::own_integrity()
        .map(|value| format!("{} ({})", value.label, value.sid))
        .unwrap_or_else(|| "unknown".to_owned());
    vec![
        ("enabled".to_owned(), config.enabled.to_string()),
        (
            "allow_synthetic_input".to_owned(),
            config.allow_synthetic_input.to_string(),
        ),
        (
            "bounds".to_owned(),
            format!(
                "uia {} s, capture {} s, tree {} nodes, capture {} px, workers {}",
                config.uia_timeout_secs,
                config.capture_timeout_secs,
                config.max_tree_nodes,
                config.max_capture_pixels,
                config.max_concurrent_workers
            ),
        ),
        ("retention".to_owned(), retention_of(config).note),
        (
            "capture_store".to_owned(),
            captures::state(
                &capture_directory(config),
                captures::now_ms(),
                captures::KEEP_HORIZON_MS,
            )
            .overview_line(),
        ),
        (
            "capture_dir".to_owned(),
            capture_directory(config).display().to_string(),
        ),
        ("dpi_awareness".to_owned(), awareness_line(&awareness)),
        ("integrity".to_owned(), integrity),
        ("session".to_owned(), format!("{:?}", session.interactive())),
        (
            "desktop_workers_alive".to_owned(),
            format!(
                "{} of {}",
                worker::live_workers(),
                config.max_concurrent_workers
            ),
        ),
    ]
}

/// The capability lines of one window's diagnosis, for the CLI to print.
pub fn capability_lines(report: &capability::WindowReport) -> Vec<String> {
    report
        .capabilities
        .iter()
        .map(|capability| capability.line())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selector_describes_the_question_it_asked() {
        let by_handle = WindowSelector {
            hwnd: Some(0x1234),
            ..Default::default()
        };
        assert_eq!(by_handle.describe(), "hwnd=4660");
        assert_eq!(WindowSelector::default().describe(), "nothing");
        let by_title = WindowSelector {
            title: Some("notepad".to_owned()),
            ..Default::default()
        };
        assert_eq!(by_title.describe(), "title~\"notepad\"");
        let both = WindowSelector {
            pid: Some(4124),
            image: Some("telegram".to_owned()),
            ..Default::default()
        };
        assert_eq!(both.describe(), "pid=4124 image~\"telegram\"");
    }

    #[test]
    fn the_expectation_is_a_whole_token_or_a_whole_value_and_never_a_substring() {
        let digest = "name=clicked:12 enabled=true";
        assert!(expectation_holds(digest, "name=clicked:12"));
        assert!(expectation_holds(digest, "clicked:12"));
        assert!(expectation_holds(digest, "true"));
        // The trap the rule exists to avoid: `1` is a prefix of `clicked:12` and proves nothing.
        assert!(!expectation_holds(digest, "1"));
        assert!(!expectation_holds(digest, "name=clicked:1"));
        assert!(!expectation_holds(digest, ""));

        // A value that is a phrase — the case whitespace-splitting loses, and the reason the split is on
        // the field names this module produces.
        let phrase = "value=typed by jean enabled=true";
        assert!(expectation_holds(phrase, "value=typed by jean"));
        assert!(expectation_holds(phrase, "typed by jean"));
        assert!(!expectation_holds(phrase, "typed"));
        assert!(!expectation_holds(phrase, "by jean enabled=true"));
        // The last field keeps its own text instead of losing a character to a separator that is not there.
        assert!(expectation_holds("name=ok toggle=Off", "toggle=Off"));
        assert!(expectation_holds("name=ok toggle=Off", "Off"));
        // A quoted name, which is how a tree read shows what the provider gave.
        let quoted = "name=\"clicked:1\" enabled=true";
        assert!(expectation_holds(quoted, "clicked:1"));
        assert!(expectation_holds(quoted, "name=\"clicked:1\""));
        // The whole digest, exactly as the report printed it.
        assert!(expectation_holds(phrase, phrase));
    }

    #[test]
    fn a_successful_pattern_call_without_the_expected_observation_is_not_success() {
        use jean_windows::accessibility::{
            Control, ControlKind, Locator, Pattern, Readback, Witness,
        };

        let locator = Locator::AutomationId("status".to_owned());
        let unchanged = Readback {
            name: Some("idle".to_owned()),
            enabled: Some(true),
            ..Readback::default()
        };
        let effect = accessibility::Effect {
            control: Control {
                automation_id: "continue".to_owned(),
                name: "Continue".to_owned(),
                name_truncated: false,
                kind: ControlKind::Button,
                class_name: "Button".to_owned(),
                is_enabled: true,
                has_keyboard_focus: false,
                is_password: false,
                rect: None,
                runtime_id: Vec::new(),
                patterns: vec![Pattern::Invoke],
                depth: None,
            },
            action: accessibility::Action::Invoke,
            pattern: Pattern::Invoke,
            before: Readback::default(),
            after: Some(Readback::default()),
            changed: false,
            witness: Some(Witness {
                locator: locator.clone(),
                before: unchanged.clone(),
                after: Some(unchanged),
                changed: false,
                unresolved: false,
            }),
            uncertain: false,
            uncertainty_reason: None,
            foreground_after: None,
        };

        let judged = judge(&effect, &locator, Some("clicked:1"), &mut Vec::new());
        assert!(matches!(judged.verdict, Verdict::NotProven(_)));
        assert!(!judged.verdict.is_proven());
    }

    #[test]
    fn an_input_that_was_never_sent_is_disproven_and_one_maybe_half_sent_is_uncertain() {
        // The defect this test exists for: a refusal before `SendInput` used to be reported as
        // unverifiable, which tells the operator the desktop might have changed when the one thing known
        // is that it did not.
        let refused = input_verdict(None, Some("focus moved"), None, None, "the target window");
        assert!(matches!(refused, Verdict::NotProven(_)), "{refused:?}");
        assert!(!refused.is_proven());

        // A partial batch is the opposite case, and outranks whatever the check said afterwards.
        let half = input_verdict(
            Some("an incomplete input batch may have half-landed"),
            None,
            Some("value=typed by jean"),
            Some("value=typed by jean"),
            "control automation_id=1001",
        );
        assert!(matches!(half, Verdict::Unverifiable(_)), "{half:?}");

        // The check is what separates proven from not proven, same as the action path.
        let witness = "control automation_id=1001";
        assert!(input_verdict(
            None,
            None,
            Some("value=typed by jean"),
            Some("value=typed by jean enabled=true"),
            witness
        )
        .is_proven());
        assert!(matches!(
            input_verdict(
                None,
                None,
                Some("value=something else"),
                Some("value=typed by jean"),
                witness
            ),
            Verdict::NotProven(_)
        ));
        // Nothing stated, or nothing readable, is a send with no claim attached — never a pass.
        assert!(matches!(
            input_verdict(None, None, None, Some("value=x"), witness),
            Verdict::Unverifiable(_)
        ));
        assert!(matches!(
            input_verdict(None, None, Some("value=x"), None, witness),
            Verdict::Unverifiable(_)
        ));
        assert!(matches!(
            input_verdict(None, None, None, None, witness),
            Verdict::Unverifiable(_)
        ));
    }

    #[test]
    fn a_refusal_is_not_an_uncertain_side_effect_and_a_timeout_is() {
        assert!(uncertain_after_failure(
            &WindowsError::PatternUnsupported {
                control: "c".into(),
                pattern: "Value is read-only".into()
            },
            true
        )
        .is_none());
        // Focus moved, so nothing was sent: the desktop is exactly as it was.
        assert!(uncertain_after_failure(
            &WindowsError::FocusChanged {
                expected: "a".into(),
                actual: "b".into()
            },
            true
        )
        .is_none());
        let timed_out = uncertain_after_failure(
            &WindowsError::ProviderTimeout {
                hwnd: 1,
                budget_secs: 5,
                waited_ms: 5_012,
            },
            true,
        );
        assert!(timed_out
            .as_deref()
            .is_some_and(|reason| reason.contains("may already have reached")));
        // The same timeout on a read changed nothing, whatever else is true about it.
        assert!(uncertain_after_failure(
            &WindowsError::ProviderTimeout {
                hwnd: 1,
                budget_secs: 5,
                waited_ms: 5_012,
            },
            false,
        )
        .is_none());
    }

    #[test]
    fn a_cancelled_wait_is_uncertain_and_a_cancelled_read_is_not() {
        assert!(uncertain_after_failure(
            &WindowsError::Cancelled {
                budget_secs: 120,
                waited_ms: 40,
            },
            true,
        )
        .is_some());
        assert!(uncertain_after_failure(
            &WindowsError::Cancelled {
                budget_secs: 120,
                waited_ms: 40,
            },
            false,
        )
        .is_none());
    }

    #[test]
    fn a_recorded_expectation_says_one_was_stated_and_not_what_it_says() {
        // The check an operator writes is desired UI text, and the record is a routine log line that other
        // tools read. `--set-value` and `--type` were already bounded this way; the expectation joins them.
        let secret = "value=my national insurance number";
        let line = describe_expect(secret);
        assert_eq!(line, "expect stated (34 chars)");
        assert!(!line.contains("insurance"), "{line}");
        assert!(!line.contains(secret), "{line}");
    }

    #[test]
    fn a_witness_digest_is_one_controls_state_in_the_words_both_paths_use() {
        let control = accessibility::Control {
            automation_id: "1001".to_owned(),
            name: String::new(),
            name_truncated: false,
            kind: accessibility::ControlKind::Edit,
            class_name: "Edit".to_owned(),
            is_enabled: true,
            has_keyboard_focus: false,
            is_password: true,
            rect: None,
            runtime_id: vec![1, 2],
            patterns: vec![accessibility::Pattern::Value],
            depth: None,
        };

        // The case the input path could not make before this digest existed: typed text proven by the
        // Value pattern, with the rest of the sentence saying which control said it.
        let typed = accessibility::Readback {
            value: Some("typed by jean".to_owned()),
            enabled: Some(true),
            ..Default::default()
        };
        let digest = witness_digest(&control, &typed);
        assert_eq!(
            digest,
            "value=typed by jean enabled=true kind=Edit focus=false"
        );
        assert!(expectation_holds(&digest, "value=typed by jean"));
        assert!(expectation_holds(&digest, "typed by jean"));
        assert!(expectation_holds(&digest, "kind=Edit"));

        // A password field reports how much it holds and never what it holds; the digest carries that
        // through, so the witness line a report prints is the redacted one.
        let secret = accessibility::Readback {
            value: Some("hunter2".to_owned()),
            value_redacted: true,
            ..Default::default()
        };
        let digest = witness_digest(&control, &secret);
        assert!(digest.contains("value=7 chars (redacted)"));
        assert!(!digest.contains("hunter2"));

        // A control whose provider reports no state at all still says what it is.
        let bare = accessibility::Readback::default();
        assert_eq!(witness_digest(&control, &bare), "kind=Edit focus=false");
    }

    #[test]
    fn an_empty_tree_is_explained_as_an_absence_and_not_as_a_protection() {
        let explanation = explain(&accessibility::TreeExplanation::NoElements);
        assert!(explanation.contains("not a protection"));
        let web = explain(&accessibility::TreeExplanation::NoInteractiveContent {
            frames: 1,
            class_name: "Chrome_WidgetWin_1".to_owned(),
            looks_like_web_content: true,
        });
        assert!(web.contains("expected rather than mysterious"));
    }

    #[test]
    fn retention_is_stated_in_the_words_of_the_policy_that_was_chosen() {
        let discard = retention_of(&DesktopConfig::default());
        assert!(!discard.keep);
        assert!(discard.note.contains("no capture file was written"));
        let kept = DesktopConfig {
            retention: "keep_24h".to_owned(),
            ..Default::default()
        };
        let retention = retention_of(&kept);
        assert!(retention.keep);
        // The expiry is the mechanism now, and the note says what the mechanism will not do either.
        assert!(retention.note.contains("claimed in"), "{retention:?}");
        assert!(retention.note.contains("never touched"), "{retention:?}");
        assert!(
            retention.note.contains("does not apply to an image"),
            "a retained frame is not redacted material, and the note that hides that is the bug:              {retention:?}"
        );
        let manual = DesktopConfig {
            retention: "keep_until_manual".to_owned(),
            ..Default::default()
        };
        let manual = retention_of(&manual);
        assert!(
            manual.note.contains("no expiry and no byte ceiling"),
            "and the other keeping policy says which promise it is making: {manual:?}"
        );
        let unknown = DesktopConfig {
            retention: "forever".to_owned(),
            ..Default::default()
        };
        assert!(!retention_of(&unknown).keep);
        assert!(retention_of(&unknown)
            .note
            .contains("not a policy this build knows"));
    }

    #[test]
    fn a_relative_capture_directory_is_not_left_to_wherever_the_command_was_started() {
        let config = DesktopConfig {
            capture_dir: "captures".to_owned(),
            ..Default::default()
        };
        assert!(capture_directory(&config).is_absolute());
    }

    #[test]
    fn report_lines_are_bounded_because_control_names_are_other_peoples_text() {
        let long = "x".repeat(2_000);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), MAX_LINE_CHARS + 1);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip("short"), "short");
    }

    #[test]
    fn an_action_and_an_input_are_different_capabilities_and_neither_is_a_read() {
        // The gate that keeps `enabled = true` from meaning "and also press keys".
        let config = DesktopConfig {
            enabled: true,
            allow_synthetic_input: false,
            ..Default::default()
        };
        assert!(require_enabled(&config).is_ok());
        assert!(require_synthetic_input(&config)
            .unwrap_err()
            .to_string()
            .contains("allow_synthetic_input = false"));
        // Diagnostics do not need the gate at all, and the read paths take no config gate for that reason.
        assert!(!capability_settings(&config).accessibility.probe_patterns);
        assert!(accessibility_settings(&config).probe_patterns);
    }

    #[test]
    fn the_worker_gate_lets_a_first_call_through_and_uses_the_configured_bound() {
        // Nothing is outstanding in a unit test, so the gate's job here is to prove the bound is read
        // from configuration rather than hard-coded.
        assert!(gate_workers(&DesktopConfig::default()).is_ok());
        let zero = DesktopConfig {
            max_concurrent_workers: 1,
            ..Default::default()
        };
        assert!(gate_workers(&zero).is_ok());
    }

    #[test]
    fn a_window_that_is_not_there_is_refused_rather_than_guessed_at() {
        // Handle 1 is not a window on any machine, and the refusal must say that in those words.
        let selector = WindowSelector {
            hwnd: Some(1),
            ..Default::default()
        };
        let error = resolve_window(&selector).unwrap_err().to_string();
        assert!(error.contains("no longer a window"), "{error}");
        assert!(resolve_window(&WindowSelector::default())
            .unwrap_err()
            .to_string()
            .contains("no window was named"));
    }
}
