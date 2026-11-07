//! Native Windows desktop mechanism: window identity, accessibility, capture, input, diagnostics.
//!
//! This crate is mechanism only. It cannot decide anything, record anything, or ask a person anything:
//! the policy engine, the task row, the audit line and the verdict all live above it, in
//! `jean_core::desktop`, exactly as they do for the browser and the delegation path. What it owns is the
//! five things a desktop operation is actually made of, each behind a trait so that a swap is a local
//! change rather than a rewrite:
//!
//! | Concern | Module | Why it is separate |
//! |---|---|---|
//! | which window, belonging to which process, at which integrity level, on which monitor, under which DPI | [`identity`] | "a window exists" is not an identity, and acting on the wrong window is the worst failure this crate can help cause |
//! | what the window exposes through UI Automation, and what can be done to it | [`accessibility`] | the semantic path (D-004), and the one that hangs when a provider does not answer |
//! | what the window looks like, on demand | [`capture`] | pixels are a *second* witness, never the verifier, and a blank frame is a diagnosis rather than a conclusion |
//! | keyboard and mouse, only when the target and the foreground have been re-checked | [`input`] | the fallback, deliberately last, deliberately boring |
//! | what this window was actually observed to support | [`capability`] | a desktop-wide `can_see` flag is how a protected app disables unrelated work; the observations belong per window and per operation |
//!
//! Two rules hold everywhere in this crate:
//!
//! * **Nothing that can hang, hangs here.** Every UIA and capture call runs in `jean-desktop-worker.exe`,
//!   a child started through the same supervision as every other J.E.A.N. process ([`worker`]). A provider
//!   that does not answer costs a *process*, which the parent terminates with its whole tree, so the
//!   long-running core never accumulates a thread parked inside another application's window procedure.
//!   The deadline is owned by the parent and the abandoned call stays uncertain — killing a worker does not
//!   undo an `Invoke` it had already delivered, and `jean_core::desktop` refuses to replay those (D-050).
//!   [`input`] is the exception, and the reason is measurable rather than convenient: `SendInput` posts to
//!   the system queue and touches no other process's window procedure, and an *action* performed in a
//!   process that can be killed mid-action is worse for the no-replay rule than the local call it replaces.
//! * **An observation is evidence, not a fact.** [`identity`] and [`accessibility`] both report what
//!   could not be determined, and the callers revalidate rather than caching. Handles are recycled,
//!   windows are recreated, and a bounding rectangle read before a layout change is a coordinate into
//!   somebody else's application.

pub mod accessibility;
pub mod bounded;
pub mod capability;
pub mod capture;
pub mod error;
pub mod identity;
pub mod input;
pub mod ocr;
mod uia;
pub mod worker;

pub use accessibility::{
    act as uia_act, probe as uia_probe, read_state as uia_read_state, read_tree as uia_read_tree,
    resolve as uia_resolve, AccessibilityBackend, Action, Control, ControlKind, ControlState,
    Effect, Locator, Pattern, Probe, Readback, Settings as AccessibilitySettings, StateRead, Tree,
    TreeExplanation, UiaBackend, MAX_NAME_CHARS,
};
pub use bounded::{in_flight, remaining, spawn_bounded, Abandoned, Outcome, WorkerBudget};
pub use capability::{
    awareness_capability, diagnose, session_capability, staleness, sweep, Capability,
    DesktopReport, Diagnosis, NextStep, Observation, Operation, Settings as CapabilitySettings,
    WindowReport,
};
pub use capture::{
    capture, capture_with_recovery, write_bmp, Attempt, Backend, Capture, CaptureBackend,
    FrameQuality, PrintWindowCapture, Recovery, RecoveryVerdict, ScreenCapture,
    Settings as CaptureSettings,
};
pub use error::{WindowsError, WindowsResult};
pub use identity::{
    awareness_of, class_name_of, display_affinity_of, ensure_dpi_awareness,
    enumerate as enumerate_windows, foreground, integrity_of, monitor_of, now_ms, own_integrity,
    read_identity, rect_of, session, virtual_desktop, Awareness, DisplayAffinity, Integrity,
    Interactive, Monitor, Reach, Rect, SessionObservation, WindowId, WindowIdentity,
};
pub use input::{
    send as send_input, Button as InputButton, Input as DesktopInput, InputBackend,
    Modifier as InputModifier, Scroll as InputScroll, SendInputBackend, Sent as InputSent,
    Settings as InputSettings, Target as InputTarget, VirtualKey,
};
pub use ocr::{
    BackendStatus, GroundedBox, GroundingRequest, NoOcr, OcrLine, OcrRequest, OcrResult,
    SidecarProtocol, TextExtractor, PROTOCOL_VERSION,
};
pub use worker::{
    live_workers, one as run_in_worker, Body as WorkerBody, FrameMeta, Reply as WorkerReply,
    ReplyBody as WorkerReplyBody, Request as WorkerRequest, Stop as WorkerStop, WireError,
    WORKER_PATH_ENV,
};
