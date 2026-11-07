//! Capability diagnostics: what this window was actually observed to support, and why not, per operation.
//!
//! This module exists because of one sentence in `docs/desktop-control-plan.md`: a blocked read is not
//! automatically a blocked launch, navigation, or action. The way that gets lost in practice is a single
//! desktop-wide `can_see` boolean, which one protected application then sets for everybody. So the unit of
//! reporting here is a *(window, operation)* pair, and there is no aggregate flag anywhere in the type.
//!
//! ## The states, and why each is its own
//!
//! [`Observation`] is the vocabulary the plan asks for — available, unsupported, permission denied,
//! explicit protection, temporarily unavailable, unknown. They are separate because the *responses* are
//! separate: an unsupported operation is a capability gap to record, a permission denial is a policy
//! question, an explicit protection is a boundary to respect, and a temporary unavailability is something
//! to try again later. Collapsing them is what produces an automation that retries a refusal forever or
//! gives up on a machine that was merely busy.
//!
//! [`Diagnosis`] is the second half: the specific conditions a report is asked to name. The mapping from
//! observation to diagnosis is computed first from evidence *about the window* — display affinity, the
//! iconic style bit, the token comparison, the desktop name — and from evidence about the attempt only
//! where no window-level fact exists. That ordering is the difference between "the capture was black so
//! the app is protected", a false statement waiting to be printed, and "the window declared an exclusion
//! and the capture was black", which is two true ones.
//!
//! ## What never happens here
//!
//! Nothing infers a capability from a failure to observe one, and nothing recommends a second backend
//! after an explicit exclusion. [`WindowReport::next_step`] is the only place an interpretation is
//! offered, and its hand-off arm is a hand-off, not a workaround.

use std::sync::atomic::AtomicBool;

use crate::accessibility::{self, Probe, TreeExplanation};
use crate::capture::{self, Capture, Recovery, RecoveryVerdict};
use crate::error::WindowsError;
use crate::identity::{
    self, DisplayAffinity, Integrity, Interactive, Reach, SessionObservation, WindowId,
    WindowIdentity,
};

/// The operations a capability report covers.
///
/// Seven, and they are deliberately not merged into "read" and "write": discovery, content reading and
/// action are answered by different APIs, so a Telegram-class window can genuinely be available for two of
/// them and explicitly protected for a third. A coarser split would make that state unrepresentable, and
/// that state is the reason this phase exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Can the window be found and identified.
    Discover,
    /// Can UI Automation enumerate controls for it.
    InspectAccessibility,
    /// Can a control pattern be acted on.
    ActByPattern,
    /// Can its pixels be captured.
    Capture,
    /// Can synthetic keyboard or mouse input reach it.
    InjectInput,
    /// Can the result of an action be observed.
    Verify,
    /// Is there an interactive session at all.
    Session,
}

impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Self::Discover => "discover",
            Self::InspectAccessibility => "inspect_accessibility",
            Self::ActByPattern => "act_by_pattern",
            Self::Capture => "capture",
            Self::InjectInput => "inject_input",
            Self::Verify => "verify",
            Self::Session => "session",
        }
    }

    /// Every operation, in the order a report lists them.
    pub const ALL: [Operation; 7] = [
        Operation::Discover,
        Operation::InspectAccessibility,
        Operation::ActByPattern,
        Operation::Capture,
        Operation::InjectInput,
        Operation::Verify,
        Operation::Session,
    ];
}

/// What was observed about one operation on one window.
///
/// The six states the plan names, each with a note on what it does *not* mean, because the failure this
/// type guards against is an over-reading rather than a mis-coding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// The operation worked; the string is the evidence of that.
    Available(String),
    /// The mechanism does not exist here: no pattern, no provider, no API.
    ///
    /// Not a refusal and not a failure — a capability the platform does not offer for this window.
    Unsupported(String),
    /// The mechanism exists and something with authority over it said no.
    ///
    /// A policy question, so it is never retried by this crate and never routed around.
    PermissionDenied(String),
    /// The window itself declared that this operation is not permitted.
    ///
    /// The only state that is a *boundary*: the response is to respect it and continue elsewhere.
    ExplicitProtection(String),
    /// The operation did not complete this time, for a reason expected to change.
    ///
    /// A hung provider, a compositor behind, a window mid-recreation. Not "unavailable forever", and not
    /// a diagnosis of the application.
    TemporarilyUnavailable(String),
    /// Nothing was established.
    ///
    /// The honest default, and the one state that must never be reported as a failure of the application.
    Unknown(String),
}

impl Observation {
    pub fn state(&self) -> &'static str {
        match self {
            Self::Available(_) => "available",
            Self::Unsupported(_) => "unsupported",
            Self::PermissionDenied(_) => "permission_denied",
            Self::ExplicitProtection(_) => "explicit_protection",
            Self::TemporarilyUnavailable(_) => "temporarily_unavailable",
            Self::Unknown(_) => "unknown",
        }
    }

    pub fn evidence(&self) -> &str {
        match self {
            Self::Available(text)
            | Self::Unsupported(text)
            | Self::PermissionDenied(text)
            | Self::ExplicitProtection(text)
            | Self::TemporarilyUnavailable(text)
            | Self::Unknown(text) => text,
        }
    }

    /// Whether this state is a reason to stop working on *this* window.
    ///
    /// `TemporarilyUnavailable` and `Unknown` are not, which is the distinction that keeps a stalled
    /// third-party window from being reported as a broken desktop.
    pub fn blocks_this_window(&self) -> bool {
        matches!(
            self,
            Self::Unsupported(_) | Self::PermissionDenied(_) | Self::ExplicitProtection(_)
        )
    }

    /// Whether unrelated work may continue while this state holds. Always `false`, and always a method
    /// rather than a comment: the requirement is that one window's answer never becomes another window's
    /// stop signal, and the way that stays true is by there being no code path that could claim otherwise.
    pub fn blocks_other_work(&self) -> bool {
        false
    }
}

/// The specific condition a report names.
///
/// Separate from [`Observation`] because these are the *causes* and an observation is the effect: several
/// causes produce `PermissionDenied`, and an operator reading a diagnostic wants the cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diagnosis {
    /// The window's own display affinity excludes capture, and the capture agreed.
    ///
    /// The only capture case that is a fact about the application rather than about the attempt, which is
    /// why it is stated from `GetWindowDisplayAffinity` first and from the pixels only as agreement.
    ExplicitCaptureExclusion { affinity: String },
    /// The capture produced a frame that says nothing and no window-level fact explains it.
    ///
    /// Not guessed at. A black frame with no declared affinity is a graphics condition, a minimised
    /// surface, or a window that declined to paint, and which of the three it was is not knowable from
    /// the image.
    UnexplainedBlankCapture { quality: String },
    /// There was no surface to capture at all.
    SurfaceUnavailable { reason: String },
    /// A previous frame and the current one are indistinguishable, so the newest image may be stale.
    StaleFrame { previous_at_ms: u64 },
    /// UI Automation reported nothing actionable for this window.
    ///
    /// Not a protection. The wording is chosen to survive being read aloud: "no controls were exposed",
    /// never "the application is inaccessible".
    AccessibilityNotExposed { explanation: String },
    /// The provider was reachable and did not answer inside the bound.
    ProviderUnresponsive { waited_ms: u64, budget_secs: u64 },
    /// This process and the target are at integrity levels Windows will not bridge.
    IntegrityMismatch { ours: String, theirs: String },
    /// The interactive session cannot show or receive anything.
    SessionUnavailable { reason: String },
    /// Input would have landed somewhere other than the target, so it was refused before being sent.
    FocusNotOnTarget { expected: String, actual: String },
    /// Nothing that would explain the state was observed.
    NotDetermined,
}

impl Diagnosis {
    pub fn name(&self) -> &'static str {
        match self {
            Self::ExplicitCaptureExclusion { .. } => "explicit_capture_exclusion",
            Self::UnexplainedBlankCapture { .. } => "unexplained_blank_capture",
            Self::SurfaceUnavailable { .. } => "surface_unavailable",
            Self::StaleFrame { .. } => "stale_frame",
            Self::AccessibilityNotExposed { .. } => "accessibility_not_exposed",
            Self::ProviderUnresponsive { .. } => "provider_unresponsive",
            Self::IntegrityMismatch { .. } => "integrity_mismatch",
            Self::SessionUnavailable { .. } => "session_unavailable",
            Self::FocusNotOnTarget { .. } => "focus_not_on_target",
            Self::NotDetermined => "not_determined",
        }
    }
}

/// One operation's entry in a report.
#[derive(Debug, Clone)]
pub struct Capability {
    pub operation: Operation,
    pub observation: Observation,
    pub diagnosis: Diagnosis,
    pub observed_at_ms: u64,
}

impl Capability {
    /// The line a diagnostic command prints.
    ///
    /// Note what is not in it: no control names, no titles beyond the identity's own bounded label, no
    /// pixels. A capability report is metadata about what could be done, which is the shape the plan
    /// allows into a log.
    pub fn line(&self) -> String {
        format!(
            "{}: {} [{}] — {}",
            self.operation.name(),
            self.observation.state(),
            self.diagnosis.name(),
            self.observation.evidence()
        )
    }

    /// The same entry as the three fields a machine consumer wants.
    pub fn to_json(&self) -> (String, String, String) {
        (
            self.operation.name().to_owned(),
            self.observation.state().to_owned(),
            self.diagnosis.name().to_owned(),
        )
    }
}

/// Everything observed about one window, plus what may be done next.
#[derive(Debug, Clone)]
pub struct WindowReport {
    pub identity: WindowIdentity,
    pub capabilities: Vec<Capability>,
    /// The session state this report was written under.
    ///
    /// Carried per report rather than asked once globally: a diagnostic that spans several windows has to
    /// be able to say which session each answer was true in, because that can change mid-report, and a
    /// report that hides the change is not reproducible.
    pub session: SessionObservation,
}

impl WindowReport {
    pub fn capability(&self, operation: Operation) -> Option<&Capability> {
        self.capabilities
            .iter()
            .find(|capability| capability.operation == operation)
    }

    /// Whether the window can be worked on through the semantic path.
    ///
    /// Deliberately narrower than "everything is fine": a window whose capture is excluded but whose
    /// controls respond *is* workable, and that is the case this phase must not lose. Only the
    /// accessibility half decides it.
    pub fn is_workable(&self) -> bool {
        self.capability(Operation::InspectAccessibility)
            .map(|capability| capability.observation.state() == "available")
            .unwrap_or(false)
    }

    /// The single next thing a caller should do with this window.
    ///
    /// Order matters and is D-004 restated as a branch: accessibility first, input second, hand-off last.
    /// A protected capture never reaches the input arm by falling through — it is checked explicitly,
    /// because the wrong reading of "capture failed" is "try clicking instead", and on a window that
    /// declared an exclusion that reading is the one the plan forbids.
    pub fn next_step(&self) -> NextStep {
        if !self.session.interactive().is_available() {
            return NextStep::PauseUntilInteractive;
        }
        if self.is_workable() {
            return NextStep::UseAccessibility;
        }
        let protected = self
            .capability(Operation::Capture)
            .map(|capability| {
                matches!(
                    capability.diagnosis,
                    Diagnosis::ExplicitCaptureExclusion { .. }
                )
            })
            .unwrap_or(false);
        if protected {
            return NextStep::NeedsHandoff;
        }
        if self
            .capability(Operation::InjectInput)
            .map(|capability| capability.observation.state() == "available")
            .unwrap_or(false)
        {
            return NextStep::UseInputWithVerification;
        }
        NextStep::NeedsHandoff
    }

    /// A plain-text report, one line per capability.
    pub fn render(&self) -> String {
        let dpi = self
            .identity
            .dpi
            .map(|dpi| format!("{dpi} ({}%)", dpi * 100 / 96))
            .unwrap_or_else(|| "unknown".to_owned());
        let integrity = self
            .identity
            .integrity
            .as_ref()
            .map(|value| format!("{} ({})", value.label, value.sid))
            .unwrap_or_else(|| "unknown".to_owned());
        let mut out = format!(
            "{}\n  interactive: {:?}\n  integrity: {integrity}\n  dpi: {dpi}\n  affinity: {:?}",
            self.identity.audit_label(),
            self.session.interactive(),
            self.identity.display_affinity
        );
        for capability in &self.capabilities {
            out.push_str(&format!("\n  {}", capability.line()));
        }
        out
    }
}

/// What a caller should do next, derived rather than guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextStep {
    /// The accessibility path works; use it and do not ask for pixels.
    UseAccessibility,
    /// Only input is available, so every action needs a verification read afterwards.
    UseInputWithVerification,
    /// Nothing is possible until the session is interactive again.
    PauseUntilInteractive,
    /// No supported path exists for this window: dependent steps pause, independent work continues.
    NeedsHandoff,
}

/// The settings a diagnostic run uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct Settings {
    pub accessibility: accessibility::Settings,
    pub capture: capture::Settings,
}

/// A complete capability report for one window.
///
/// Every probe is independent and every failure becomes an entry rather than an abort, which is what makes
/// the report safe to run against a hung application: the worst it can do is report that one operation as
/// [`Observation::TemporarilyUnavailable`] after its own bound.
pub fn diagnose(
    window: WindowId,
    settings: &Settings,
    cancel: Option<&AtomicBool>,
) -> WindowReport {
    let identity = identity::read_identity(window);
    let session = identity::session();
    let mut capabilities = vec![
        discover(&identity, &session),
        inspect_accessibility(window, &settings.accessibility, cancel),
        // Acting is reported separately from inspecting on purpose. A window can expose a perfect tree
        // and no single pattern that can change anything — a read-only document is the ordinary case —
        // and a report that said only "accessible" would be answering a question nobody asked.
        act_by_pattern(window, &settings.accessibility, cancel),
        capture_capability(window, &settings.capture, cancel),
        inject_input(&identity),
        verify(window, &settings.accessibility, cancel),
    ];
    capabilities.push(session_capability(&session));

    WindowReport {
        identity,
        capabilities,
        session,
    }
}

fn discover(found: &WindowIdentity, session: &SessionObservation) -> Capability {
    let interactive = session.interactive();
    let (observation, diagnosis) = if !interactive.is_available() {
        (
            Observation::TemporarilyUnavailable(format!(
                "the interactive session is {interactive:?}; the window itself is still enumerable"
            )),
            Diagnosis::SessionUnavailable {
                reason: format!("{interactive:?}"),
            },
        )
    } else if !found.is_visible {
        (
            Observation::TemporarilyUnavailable(format!(
                "hwnd {} exists but is not visible",
                found.id.hwnd
            )),
            Diagnosis::SurfaceUnavailable {
                reason: "not visible".to_owned(),
            },
        )
    } else if found.pid == 0 {
        (
            Observation::Unknown("no owning process could be attributed".to_owned()),
            Diagnosis::NotDetermined,
        )
    } else {
        (
            Observation::Available(format!(
                "pid {}, {}x{}@{},{} on {}",
                found.pid,
                found.rect.map(|rect| rect.width()).unwrap_or(0),
                found.rect.map(|rect| rect.height()).unwrap_or(0),
                found.rect.map(|rect| rect.left).unwrap_or(0),
                found.rect.map(|rect| rect.top).unwrap_or(0),
                found
                    .monitor
                    .map(|monitor| format!(
                        "monitor {}x{} at {}% dpi",
                        monitor.width,
                        monitor.height,
                        monitor.scale_percent()
                    ))
                    .unwrap_or_else(|| "an unreadable monitor".to_owned())
            )),
            Diagnosis::NotDetermined,
        )
    };
    Capability {
        operation: Operation::Discover,
        observation,
        diagnosis,
        observed_at_ms: found.observed_at_ms,
    }
}

fn inspect_accessibility(
    window: WindowId,
    settings: &accessibility::Settings,
    cancel: Option<&AtomicBool>,
) -> Capability {
    let now = identity::now_ms();
    match accessibility::probe(window, *settings, cancel) {
        Ok(probe) => from_probe(&probe, now),
        Err(error) => {
            from_accessibility_error(&error, window, Operation::InspectAccessibility, now)
        }
    }
}

/// The interpretation of a successful probe.
///
/// Split out because it is testable without a desktop: the mapping from a UIA answer to a capability claim
/// is a policy about words, and a policy about words is the thing most likely to go wrong in exactly the
/// way the brief warns about.
fn from_probe(probe: &Probe, now: u64) -> Capability {
    let evidence = format!(
        "{} controls; patterns {}",
        probe.control_count,
        probe
            .patterns
            .iter()
            .map(|pattern| pattern.name())
            .collect::<Vec<_>>()
            .join("/")
    );
    let (observation, diagnosis) = match &probe.explanation {
        TreeExplanation::Populated => (Observation::Available(evidence), Diagnosis::NotDetermined),
        TreeExplanation::NoElements => (
            Observation::Unsupported(format!(
                "no element was exposed for window {} (frame class {:?}) — what the tree reported, not a claim about the application",
                probe.window.hwnd, probe.frame_class
            )),
            Diagnosis::AccessibilityNotExposed {
                explanation: "no element for the handle".to_owned(),
            },
        ),
        TreeExplanation::NoInteractiveContent {
            frames,
            class_name,
            looks_like_web_content,
        } => {
            let hint = if *looks_like_web_content {
                " — the frame class is one an embedded browser surface uses, where the content tree is exposed only when the application is told to expose it"
            } else {
                ""
            };
            (
                Observation::Unsupported(format!(
                    "{frames} frame element(s) and nothing actionable (class {class_name:?}){hint}"
                )),
                Diagnosis::AccessibilityNotExposed {
                    explanation: format!("frames only, class {class_name}"),
                },
            )
        }
        TreeExplanation::Capped => (
            Observation::Available(format!("{evidence} (truncated at the node cap)")),
            Diagnosis::NotDetermined,
        ),
        TreeExplanation::Unreachable(reason) => (
            Observation::Unknown(format!("the automation pipeline did not answer: {reason}")),
            Diagnosis::NotDetermined,
        ),
    };
    Capability {
        operation: Operation::InspectAccessibility,
        observation,
        diagnosis,
        observed_at_ms: now,
    }
}

fn from_accessibility_error(
    error: &WindowsError,
    window: WindowId,
    operation: Operation,
    now: u64,
) -> Capability {
    let (observation, diagnosis) = match error {
        WindowsError::ProviderTimeout {
            budget_secs,
            waited_ms,
            ..
        } => (
            Observation::TemporarilyUnavailable(format!(
                "window {} did not answer within {budget_secs}s; its worker process was terminated, and \
                 the call was not cancelled",
                window.hwnd
            )),
            Diagnosis::ProviderUnresponsive {
                waited_ms: *waited_ms,
                budget_secs: *budget_secs,
            },
        ),
        WindowsError::Cancelled { .. } => (
            Observation::TemporarilyUnavailable("the probe was cancelled by the run".to_owned()),
            Diagnosis::NotDetermined,
        ),
        WindowsError::IntegrityBlocked { reason, .. } => (
            Observation::PermissionDenied(reason.clone()),
            Diagnosis::IntegrityMismatch {
                ours: Integrity::owned_by_this_process()
                    .map(|value| format!("{} ({})", value.label, value.sid))
                    .unwrap_or_else(|| "unknown".to_owned()),
                theirs: reason.clone(),
            },
        ),
        WindowsError::AccessibilityUnavailable { reason } => (
            Observation::Unknown(format!(
                "UI Automation could not be reached for window {}: {reason}",
                window.hwnd
            )),
            Diagnosis::NotDetermined,
        ),
        other => (
            Observation::Unknown(other.to_string()),
            Diagnosis::NotDetermined,
        ),
    };
    Capability {
        operation,
        observation,
        diagnosis,
        observed_at_ms: now,
    }
}

fn act_by_pattern(
    window: WindowId,
    settings: &accessibility::Settings,
    cancel: Option<&AtomicBool>,
) -> Capability {
    let now = identity::now_ms();
    // The pattern list from a probe is the evidence. No action is performed in order to find out whether
    // action works: a diagnostic that pressed something to test pressing is not a diagnostic.
    match accessibility::probe(window, *settings, cancel) {
        Ok(probe) => {
            let actionable: Vec<&str> = probe
                .patterns
                .iter()
                .map(|pattern| pattern.name())
                .filter(|name| *name != "Transform" && *name != "Selection")
                .collect();
            let (observation, diagnosis) = if actionable.is_empty() {
                (
                    Observation::Unsupported(format!(
                        "the tree exposed {} controls but no pattern that changes state",
                        probe.control_count
                    )),
                    Diagnosis::AccessibilityNotExposed {
                        explanation: "no state-changing pattern".to_owned(),
                    },
                )
            } else {
                (
                    Observation::Available(actionable.join(", ")),
                    Diagnosis::NotDetermined,
                )
            };
            Capability {
                operation: Operation::ActByPattern,
                observation,
                diagnosis,
                observed_at_ms: now,
            }
        }
        Err(error) => from_accessibility_error(&error, window, Operation::ActByPattern, now),
    }
}

fn capture_capability(
    window: WindowId,
    settings: &capture::Settings,
    cancel: Option<&AtomicBool>,
) -> Capability {
    let now = identity::now_ms();
    let affinity = identity::display_affinity_of(window);

    // The window's own declaration is read first, and an exclusion stops the attempt. The order *is* the
    // requirement: asking an application to paint into our surface after it said it would not is the
    // backend-cycling the plan forbids, and it also manufactures the black frame a lazier implementation
    // would then have to guess the meaning of.
    if affinity.is_explicit_protection() {
        return Capability {
            operation: Operation::Capture,
            observation: Observation::ExplicitProtection(format!(
                "the window set its own display affinity to {affinity:?}"
            )),
            diagnosis: Diagnosis::ExplicitCaptureExclusion {
                affinity: format!("{affinity:?}"),
            },
            observed_at_ms: now,
        };
    }

    match capture::capture_with_recovery(window, *settings, cancel) {
        Ok(recovery) => from_recovery(&recovery, affinity, now),
        Err(error) => Capability {
            operation: Operation::Capture,
            observation: Observation::Unknown(error.to_string()),
            diagnosis: Diagnosis::NotDetermined,
            observed_at_ms: now,
        },
    }
}

/// The mapping from a bounded recovery to a capability claim, with the four kinds of nothing kept apart.
///
/// The affinity argument is the reason this function exists: a uniform-black frame plus a declared
/// exclusion is protection, and a uniform-black frame plus no declaration is not, and the only way those
/// two sentences stay different in the code is by passing the declaration in.
fn from_recovery(recovery: &Recovery, affinity: DisplayAffinity, now: u64) -> Capability {
    let evidence: String = recovery
        .attempts
        .iter()
        .map(|attempt| format!("{}: {}", attempt.backend.name(), attempt.note))
        .collect::<Vec<_>>()
        .join("; ");

    let (observation, diagnosis) = match recovery.verdict() {
        RecoveryVerdict::Usable(_) => (Observation::Available(evidence), Diagnosis::NotDetermined),
        RecoveryVerdict::ExplicitProtection => (
            Observation::ExplicitProtection(evidence),
            Diagnosis::ExplicitCaptureExclusion {
                affinity: format!("{affinity:?}"),
            },
        ),
        RecoveryVerdict::Unusable(reason) => (
            Observation::TemporarilyUnavailable(evidence),
            Diagnosis::SurfaceUnavailable { reason },
        ),
        RecoveryVerdict::Uninformative(quality) => {
            if affinity.is_explicit_protection() {
                (
                    Observation::ExplicitProtection(evidence),
                    Diagnosis::ExplicitCaptureExclusion {
                        affinity: format!("{affinity:?}"),
                    },
                )
            } else {
                (
                    // The frame was produced, it is blank, and nothing about the window explains why. This
                    // is the case that must not be labelled a protection, and `quality` is the whole of
                    // what can honestly be said about it.
                    Observation::Unknown(format!("{evidence} (quality {quality:?})")),
                    Diagnosis::UnexplainedBlankCapture {
                        quality: format!("{quality:?}"),
                    },
                )
            }
        }
    };
    Capability {
        operation: Operation::Capture,
        observation,
        diagnosis,
        observed_at_ms: now,
    }
}

/// Whether the frame agrees with the one before it.
///
/// A separate function rather than part of [`diagnose`], because staleness is only answerable against a
/// previous frame and only the caller knows whether it kept one — and keeping captures is a retention
/// decision, not a diagnostic one.
pub fn staleness(previous: Option<&Capture>, current: &Capture) -> Option<Capability> {
    let previous = previous?;
    if current.same_content_as(previous) {
        Some(Capability {
            operation: Operation::Capture,
            observation: Observation::Unknown(format!(
                "identical to the capture at {} ms; the surface may not have repainted",
                previous.captured_at_ms
            )),
            diagnosis: Diagnosis::StaleFrame {
                previous_at_ms: previous.captured_at_ms,
            },
            observed_at_ms: current.captured_at_ms,
        })
    } else {
        None
    }
}

fn inject_input(found: &WindowIdentity) -> Capability {
    let ours = Integrity::owned_by_this_process();
    let reach = Integrity::input_reach(ours.as_ref(), found.integrity.as_ref());
    let (observation, diagnosis) = match reach {
        Reach::Allowed => (
            Observation::Available(format!(
                "this process is at {}, the target is at {}",
                ours.map(|value| value.label).unwrap_or_default(),
                found
                    .integrity
                    .as_ref()
                    .map(|value| value.label.clone())
                    .unwrap_or_else(|| "unknown".to_owned())
            )),
            Diagnosis::NotDetermined,
        ),
        Reach::Blocked { ours, theirs } => (
            Observation::PermissionDenied(format!(
                "Windows would drop the input without reporting it: this process is {ours}, the target is {theirs}"
            )),
            Diagnosis::IntegrityMismatch { ours, theirs },
        ),
        Reach::Unknown(reason) => (Observation::Unknown(reason), Diagnosis::NotDetermined),
    };
    // A window that is not in the foreground is still input-*reachable*. Whether input would land there is
    // answered at send time, not here: recording it as a refusal would make a purely read-only diagnostic
    // report a capability gap that does not exist.
    Capability {
        operation: Operation::InjectInput,
        observation,
        diagnosis,
        observed_at_ms: found.observed_at_ms,
    }
}

fn verify(
    window: WindowId,
    settings: &accessibility::Settings,
    cancel: Option<&AtomicBool>,
) -> Capability {
    let now = identity::now_ms();
    // Verification means re-reading a control property after an action, and the only mechanism that does
    // that without pixels is the accessibility path. The restatement is worth having: a window whose tree
    // reads but whose controls report nothing observable cannot be verified, and that is a claim a report
    // should be able to make on its own line.
    let observation = match accessibility::probe(window, *settings, cancel) {
        Ok(probe) if probe.control_count == 0 => {
            Observation::Unsupported("no controls to re-read".to_owned())
        }
        Ok(probe) => Observation::Available(format!(
            "{} controls; observable state comes from {}",
            probe.control_count,
            probe
                .patterns
                .iter()
                .map(|pattern| pattern.name())
                .collect::<Vec<_>>()
                .join("/")
        )),
        Err(error) => Observation::Unknown(error.to_string()),
    };
    Capability {
        operation: Operation::Verify,
        observation,
        diagnosis: Diagnosis::NotDetermined,
        observed_at_ms: now,
    }
}

/// The session entry, computed from a session observation the caller already has.
///
/// Takes the observation rather than reading it, because a sweep reads the session once and every window
/// report in that sweep must describe the *same* session state — not whatever it happened to be when each
/// per-window read got around to asking again.
pub fn session_capability(session: &SessionObservation) -> Capability {
    let interactive = session.interactive();
    let (observation, diagnosis) = match &interactive {
        Interactive::Available => (
            Observation::Available(format!(
                "input desktop {input:?}, console session {console:?}, this process in {ours:?}",
                input = session.input_desktop,
                console = session.console_session,
                ours = session.our_session
            )),
            Diagnosis::NotDetermined,
        ),
        other => (
            Observation::TemporarilyUnavailable(format!("{other:?}")),
            Diagnosis::SessionUnavailable {
                reason: format!("{other:?}"),
            },
        ),
    };
    Capability {
        operation: Operation::Session,
        observation,
        diagnosis,
        observed_at_ms: identity::now_ms(),
    }
}

/// The DPI posture of this process, which is a capability in its own right on a mixed-DPI machine.
///
/// Reads the awareness Windows says is in force; it does not *set* it. A diagnostic that mutates the
/// process's coordinate space would make the thing it is reporting about the cause of it.
pub fn awareness_capability() -> Capability {
    let in_force = identity::awareness_of(WindowId { hwnd: 0 });
    let (observation, diagnosis) = if in_force == "per-monitor-v2" {
        (
            Observation::Available(format!("awareness is {in_force}")),
            Diagnosis::NotDetermined,
        )
    } else {
        (
            Observation::Unknown(format!(
                "awareness is {in_force}; coordinates and captures are computed in that space"
            )),
            Diagnosis::NotDetermined,
        )
    };
    Capability {
        operation: Operation::Discover,
        observation,
        diagnosis,
        observed_at_ms: identity::now_ms(),
    }
}

/// A whole-desktop diagnostic: the session, the awareness, and one report per window.
#[derive(Debug, Clone)]
pub struct DesktopReport {
    pub session: Capability,
    pub awareness: Capability,
    pub windows: Vec<WindowReport>,
    /// `true` when more windows existed than the limit allowed.
    pub truncated: bool,
    /// How long the sweep took, so a slow third-party window is visible as a cause.
    pub elapsed_ms: u128,
}

/// Run the sweep.
///
/// `limit` is applied *after* enumeration so the count of windows an operator can see stays part of what
/// the report says, and the session is read once for the whole sweep for the reason on
/// [`session_capability`].
pub fn sweep(limit: usize, settings: &Settings, cancel: Option<&AtomicBool>) -> DesktopReport {
    let started = std::time::Instant::now();
    let session = identity::session();
    let all = identity::enumerate().unwrap_or_default();
    let truncated = all.len() > limit;
    let windows: Vec<WindowReport> = all
        .into_iter()
        .take(limit)
        .map(|found| WindowReport {
            capabilities: vec![
                discover(&found, &session),
                inspect_accessibility(found.id, &settings.accessibility, cancel),
                act_by_pattern(found.id, &settings.accessibility, cancel),
                capture_capability(found.id, &settings.capture, cancel),
                inject_input(&found),
                verify(found.id, &settings.accessibility, cancel),
                session_capability(&session),
            ],
            identity: found,
            session: session.clone(),
        })
        .collect();
    DesktopReport {
        session: session_capability(&session),
        awareness: awareness_capability(),
        windows,
        truncated,
        elapsed_ms: started.elapsed().as_millis(),
    }
}
