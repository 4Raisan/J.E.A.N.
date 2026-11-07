//! The failure vocabulary of the desktop mechanism.
//!
//! Every variant here is a *different sentence*, which is the whole point: the reason desktop automation
//! gets believed too easily is that "no controls", "no pixels", "no permission" and "the application did
//! not answer" all collapse into one error string, and a report that collapses them cannot tell an
//! operator which of the four to act on. `docs/desktop-control-plan.md` names the distinction as the
//! requirement; these are the types that make it impossible to lose.
//!
//! Nothing here says *why* a thing failed in the caller's terms — that is `jean-policy`'s and
//! `jean-core`'s business. This enum says what was observed.

use std::path::PathBuf;

/// A desktop operation, or the setup it needed, did not complete.
#[derive(Debug, thiserror::Error)]
pub enum WindowsError {
    /// The window handle is not a window any more.
    ///
    /// Reported *before* an action, never after: an action whose handle died mid-flight is an uncertain
    /// side effect, and the caller has to reconcile it rather than assume nothing happened.
    #[error("window {hwnd} is no longer a window")]
    StaleWindow { hwnd: isize },

    /// Nothing in this process could be found that matches the request.
    ///
    /// Distinct from [`WindowsError::AccessibilityUnavailable`]: a pid with no window is a fact about the
    /// application, while a window whose provider did not answer is a fact about the probe.
    #[error("no window matches {wanted}")]
    NoSuchWindow { wanted: String },

    /// UI Automation could not be reached at all — no COM, no automation core, no provider.
    #[error("UI Automation is not available here: {reason}")]
    AccessibilityUnavailable { reason: String },

    /// A provider was reachable but did not answer inside the bound the caller set.
    ///
    /// The worker process that was asking is terminated, with its job object, and nothing of it is left
    /// behind to accumulate (§49.1). Telling the operator "the provider did not answer, so this window is
    /// unassessed" is the honest version of a timeout; the thing that must not be claimed in addition is
    /// that the call was cancelled, because a cross-process send already in flight cannot be cancelled, and
    /// an action with side effects is therefore uncertain and never replayed (D-050).
    #[error("the accessibility provider for window {hwnd} did not answer within {budget_secs}s (waited {waited_ms}ms)")]
    ProviderTimeout {
        hwnd: isize,
        budget_secs: u64,
        waited_ms: u64,
    },

    /// The wait was ended because the run was cancelled.
    ///
    /// The worker process is terminated, so the *process* is certainly gone; the call is *not* cancelled —
    /// nothing can cancel a cross-process send that is already in flight — so an operation with side effects
    /// is uncertain by construction and the caller must reconcile it. This is the only variant whose meaning
    /// is "we stopped looking", not "the desktop refused".
    #[error("cancelled while the desktop worker was answering (budget {budget_secs}s, waited {waited_ms}ms); any side effect of that call is unverified")]
    Cancelled { budget_secs: u64, waited_ms: u64 },

    /// The desktop worker panicked inside our own code.
    ///
    /// Named separately from a timeout because the two imply opposite things about the provider: a panic
    /// is J.E.A.N.'s bug and the application may have answered perfectly, while a timeout is the
    /// application's and J.E.A.N.'s code never got an answer to fall over on.
    ///
    /// `call` is owned because the name can arrive from another process: a borrow with no lifetime to
    /// attach it to would have to be guessed back out of a closed list of names, and a guess about which
    /// call failed is a claim about a mechanism nobody observed.
    #[error("{call} panicked in the desktop worker")]
    Panicked { call: String },

    /// The locator matched more than one control.
    ///
    /// Refused rather than resolved. Picking the first match of an ambiguous locator is how an
    /// automation clicks "Continue" in the wrong dialog, and there is no ordering guarantee to fall back
    /// on — the enumeration order is whatever the provider felt like.
    #[error("{count} controls match {locator}; naming one is not a tie-break")]
    Ambiguous { locator: String, count: usize },

    /// The control is there and this build knows how to read it, but it exposes no pattern that can act
    /// on it.
    ///
    /// A read of a disabled control is still a read; only the action is refused.
    #[error("control {control} does not support the {pattern} pattern")]
    PatternUnsupported { control: String, pattern: String },

    /// The cached control is gone or has been replaced, so the observation it carries is out of date.
    #[error("the cached control {control} is stale; the tree has changed under it")]
    StaleElement { control: String },

    /// The action was refused because the window we were about to type into is no longer the one being
    /// typed into.
    ///
    /// This is the foreground lock's counterpart: focus can change between a verified read and the
    /// injected keystroke, and the only safe response is to stop.
    #[error("focus moved: expected {expected} and found {actual}")]
    FocusChanged { expected: String, actual: String },

    /// The target runs at a different integrity level, so the input or the read is refused by Windows.
    ///
    /// R-14: UIPI blocks both directions silently. The mitigation is to *say so*, because an automation
    /// that retries an integrity failure looks exactly like one that is being ignored.
    #[error("integrity mismatch with window {hwnd} ({reason})")]
    IntegrityBlocked { hwnd: isize, reason: String },

    /// The interactive session cannot show anything right now.
    #[error("the interactive session is unavailable: {reason}")]
    SessionUnavailable { reason: String },

    /// Capture produced an image and the image is not trustworthy for the purpose it was requested.
    ///
    /// Explicit protection and an unexplained blank are *not* the same variant: the first is
    /// detectable through the window's display affinity and the second is not, which is precisely the
    /// distinction the plan asks for.
    #[error("capture of window {hwnd} is unusable: {reason}")]
    CaptureUnusable { hwnd: isize, reason: String },

    /// A file the mechanism needed could not be written or read.
    #[error("could not use {path}: {reason}")]
    File { path: PathBuf, reason: String },

    /// A configured timeout or bound is impossible, reported before anything is attempted.
    #[error("invalid desktop setting: {reason}")]
    InvalidSetting { reason: String },

    /// A Win32 call failed for a reason with no more specific name here.
    ///
    /// Owned for the same reason as [`WindowsError::Panicked`]: the name survives crossing into and out of
    /// the desktop worker unchanged.
    #[error("{call} failed: {reason}")]
    Win32 { call: String, reason: String },
}

/// The crate's result type.
pub type WindowsResult<T> = std::result::Result<T, WindowsError>;
