//! Console signals: Ctrl-C, Ctrl-Break, and the window close button.
//!
//! Phase 0 requires the executable to exit cleanly on Ctrl-C and on window
//! close. "Cleanly" has a specific meaning here: the child tree is cancelled
//! through its Job Object, and the task record says `cancelled` rather than being
//! left in `running` for the next invocation to discover.
//!
//! ## What happens without a handler
//!
//! The default Ctrl-C behaviour terminates the process. The Job Object handle
//! closes with it, and `KILL_ON_JOB_CLOSE` does kill the tree — so nothing is
//! leaked. What is lost is the record: the task stays in `running` with a runner
//! pid that no longer exists, and only reconciliation on a later run turns it into
//! `needs_attention`. That is recoverable but it is a lie in the interim, and it
//! is why a handler is worth the FFI.
//!
//! ## Window close is different
//!
//! `CTRL_CLOSE_EVENT` gives the process a few seconds and then terminates it,
//! whatever the handler returns. So for close, logoff and shutdown the handler
//! signals cancellation and returns `FALSE` to let the system proceed; the job
//! kill-on-close is still the backstop that guarantees no orphaned descendants.
//!
//! ## Why a static, and why it is replaceable
//!
//! `PHANDLER_ROUTINE` is a plain function pointer with no context parameter, so the
//! token has to be reachable from a global. It is held in a `Mutex<Option<…>>` rather
//! than a `OnceLock` because one `jean` process can now supervise more than one task in
//! sequence — Phase 1's intents run back to back inside a single invocation, and the
//! second one cannot be given a Ctrl-C path that silently signals the *first* task's
//! token. `arm` therefore replaces the current token, and `disarm` clears it, so a token
//! is reachable only while its own task is running.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static OWNER_CANCELLED: AtomicBool = AtomicBool::new(false);

pub fn owner_token() -> Result<CancelToken> {
    let token = CancelToken::new()?;
    arm(token.clone())?;
    Ok(token)
}

pub fn cancel_owned_task() {
    OWNER_CANCELLED.store(true, Ordering::SeqCst);
    let guard = armed_tokens().lock().unwrap_or_else(|p| p.into_inner());
    for token in guard.iter() {
        let _ = token.cancel();
    }
}

use std::sync::{Mutex, OnceLock};

use windows::core::BOOL;
use windows::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT,
};

use jean_process::CancelToken;

use crate::error::{CoreError, Result};

/// All tokens signalled on a console event (parallel broadcast).
///
/// One entry per active task; `arm` pushes, `cancel_owned_task` cancels
/// every entry, `disarm` clears. Sequential tasks arm then disarm, so
/// they never see each other's tokens; parallel tasks each arm and a
/// single Ctrl-C cancels all of them.
static ARMED: OnceLock<Mutex<Vec<CancelToken>>> = OnceLock::new();

/// Whether `handle_console_event` is currently in the process's handler list.
///
/// `SetConsoleCtrlHandler` appends: adding the same routine twice installs it twice, so
/// the second removal would leave one copy armed. This makes install/remove balanced.
static INSTALLED: AtomicU32 = AtomicU32::new(0);

fn armed_tokens() -> &'static Mutex<Vec<CancelToken>> {
    ARMED.get_or_init(|| Mutex::new(Vec::new()))
}

/// Point the console handler at `token`, installing it if this is the first arm.
///
/// Re-arming is the normal case, not an error: it is what lets one invocation run
/// several supervised tasks in sequence, each cancellable while it runs and none of them
/// cancellable after it finished.
pub fn arm(token: CancelToken) -> Result<()> {
    {
        let mut guard = armed_tokens()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if OWNER_CANCELLED.load(Ordering::SeqCst) {
            token.cancel()?;
        }
        guard.push(token);
    }

    if INSTALLED.load(Ordering::SeqCst) == 0 {
        unsafe {
            SetConsoleCtrlHandler(Some(handle_console_event), true).map_err(|e| {
                // Undo the arm before returning: a token nothing will ever signal is
                // worse than no token, because `interrupt()` would still report a task
                // as in flight.
                armed_tokens()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .pop();
                CoreError::Environment {
                    reason: format!("SetConsoleCtrlHandler failed: {e}"),
                }
            })?;
        }
        INSTALLED.store(1, Ordering::SeqCst);
    }
    Ok(())
}

/// Stop signalling the current token, and remove the handler if it was the last arm.
///
/// Called before a normal exit so a long-lived process does not keep a token that
/// belongs to a finished task.
pub fn disarm() {
    armed_tokens()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    if INSTALLED.swap(0, Ordering::SeqCst) == 1 {
        unsafe {
            if let Err(e) = SetConsoleCtrlHandler(Some(handle_console_event), false) {
                tracing::debug!(error = %e, "could not remove the console handler");
            }
        }
    }
}

/// The last control type seen, plus one so 0 means "nothing yet".
///
/// `CTRL_C_EVENT` is 0, so the raw value cannot be stored directly without
/// conflating "Ctrl-C" and "no event".
static LAST_EVENT: AtomicU32 = AtomicU32::new(0);

/// The control type that interrupted this process, if any.
pub fn interrupt() -> Option<Interrupt> {
    if OWNER_CANCELLED.load(Ordering::SeqCst) {
        return Some(Interrupt::OwnerCancelled);
    }
    match LAST_EVENT.load(Ordering::SeqCst) {
        0 => None,
        n => Some(Interrupt::from_code(n - 1)),
    }
}

/// A console event, named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    OwnerCancelled,
    CtrlC,
    CtrlBreak,
    WindowClosed,
    Logoff,
    Shutdown,
    /// A control type this build does not name. Reported rather than dropped,
    /// because "something interrupted the task" is still the useful fact.
    Unknown(u32),
}

impl Interrupt {
    fn from_code(code: u32) -> Self {
        match code {
            CTRL_C_EVENT => Interrupt::CtrlC,
            CTRL_BREAK_EVENT => Interrupt::CtrlBreak,
            CTRL_CLOSE_EVENT => Interrupt::WindowClosed,
            CTRL_LOGOFF_EVENT => Interrupt::Logoff,
            CTRL_SHUTDOWN_EVENT => Interrupt::Shutdown,
            other => Interrupt::Unknown(other),
        }
    }

    /// A short label for logs and for the task record.
    pub fn as_str(self) -> &'static str {
        match self {
            Interrupt::OwnerCancelled => "the task owner requested cancellation",
            Interrupt::CtrlC => "Ctrl-C",
            Interrupt::CtrlBreak => "Ctrl-Break",
            Interrupt::WindowClosed => "the console window was closed",
            Interrupt::Logoff => "the user logged off",
            Interrupt::Shutdown => "the system is shutting down",
            Interrupt::Unknown(_) => "a console control event",
        }
    }

    /// True when the system will terminate this process shortly regardless of
    /// what the handler does.
    pub fn is_fatal(self) -> bool {
        matches!(
            self,
            Interrupt::WindowClosed | Interrupt::Logoff | Interrupt::Shutdown
        )
    }
}

/// The Win32 handler.
///
/// Runs on a thread the system creates, at an arbitrary point in the process's
/// life. It must therefore do as little as possible: record the event, signal the
/// token, return. No allocation beyond what `cancel` needs, no logging that could
/// deadlock against a lock the interrupted thread holds.
///
/// # Safety
/// Called by Windows. The contract is that it returns quickly and does not
/// perform UI or blocking work; `CancelToken::cancel` is a single `SetEvent`.
unsafe extern "system" fn handle_console_event(control_type: u32) -> BOOL {
    LAST_EVENT.store(control_type.saturating_add(1), Ordering::SeqCst);

    // The lock is only ever held for the length of a pointer write in `arm`/`disarm`,
    // so taking it here cannot block meaningfully; the token is not cloned out because
    // nothing needs it after the one `SetEvent`.
    let guard = armed_tokens()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for token in guard.iter() {
        // Ignoring the error is deliberate: there is nothing to report it to, and
        // the handler must return.
        let _ = token.cancel();
    }

    let interrupt = Interrupt::from_code(control_type);
    if interrupt.is_fatal() {
        // Let the system proceed to termination. The job kill-on-close takes the
        // tree with it.
        BOOL(0)
    } else {
        // Handled: the supervisor will see the signalled event, terminate the job,
        // and record the task as cancelled before this process exits normally.
        BOOL(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_named_control_types_cover_the_documented_set() {
        assert_eq!(Interrupt::from_code(CTRL_C_EVENT), Interrupt::CtrlC);
        assert_eq!(Interrupt::from_code(CTRL_BREAK_EVENT), Interrupt::CtrlBreak);
        assert_eq!(
            Interrupt::from_code(CTRL_CLOSE_EVENT),
            Interrupt::WindowClosed
        );
        assert_eq!(Interrupt::from_code(CTRL_LOGOFF_EVENT), Interrupt::Logoff);
        assert_eq!(
            Interrupt::from_code(CTRL_SHUTDOWN_EVENT),
            Interrupt::Shutdown
        );
        assert_eq!(Interrupt::from_code(999), Interrupt::Unknown(999));
    }

    #[test]
    fn only_close_logoff_and_shutdown_are_fatal() {
        assert!(!Interrupt::CtrlC.is_fatal());
        assert!(!Interrupt::CtrlBreak.is_fatal());
        assert!(Interrupt::WindowClosed.is_fatal());
        assert!(Interrupt::Logoff.is_fatal());
        assert!(Interrupt::Shutdown.is_fatal());
    }

    #[test]
    fn ctrl_c_is_zero_so_the_stored_value_must_be_offset() {
        // The reason LAST_EVENT stores `code + 1`. If this ever changes the offset
        // becomes wrong and "no event" turns into "Ctrl-C".
        assert_eq!(CTRL_C_EVENT, 0);
        assert_ne!(Interrupt::from_code(0), Interrupt::Unknown(0));
    }

    #[test]
    fn every_interrupt_has_a_readable_label() {
        for interrupt in [
            Interrupt::CtrlC,
            Interrupt::CtrlBreak,
            Interrupt::WindowClosed,
            Interrupt::Logoff,
            Interrupt::Shutdown,
            Interrupt::Unknown(7),
        ] {
            assert!(!interrupt.as_str().is_empty(), "{interrupt:?}");
        }
    }

    #[test]
    fn nothing_is_interrupted_before_a_signal() {
        // Only meaningful while no handler has fired, which is the normal case in
        // a test process.
        if LAST_EVENT.load(Ordering::SeqCst) == 0 {
            assert_eq!(interrupt(), None);
        }
    }

    #[test]
    fn the_handler_records_what_it_was_told() {
        // Exercised through the offset arithmetic rather than by installing a
        // console handler: `arm` mutates process-global state that the test
        // harness shares, and a unit test should not do that. `arm` and `disarm`
        // are covered by the CLI integration test in `tests/`, where a real
        // Ctrl-C is delivered to a real supervised child.
        let before = LAST_EVENT.load(Ordering::SeqCst);
        if before != 0 {
            return;
        }
        LAST_EVENT.store(CTRL_C_EVENT.saturating_add(1), Ordering::SeqCst);
        assert_eq!(interrupt(), Some(Interrupt::CtrlC));
        LAST_EVENT.store(CTRL_CLOSE_EVENT.saturating_add(1), Ordering::SeqCst);
        assert_eq!(interrupt(), Some(Interrupt::WindowClosed));
        LAST_EVENT.store(0, Ordering::SeqCst);
    }

    #[test]
    fn a_fresh_token_can_replace_the_armed_one() {
        // This one does touch the process-global handler, and it has to: the
        // requirement is that a *second* `arm` in the same process succeeds, which is
        // not observable any other way. It never delivers a console event, so the only
        // shared state it changes is the handler list, which `disarm` takes back down
        // before the test returns.
        //
        // The Phase-1 reason: one `jean` invocation now runs several intents in
        // sequence, and each must be cancellable while it is the thing running. A
        // `OnceLock` made the second `arm` an error, so the second intent of an
        // invocation could be started and then never stopped by Ctrl-C.
        let first = CancelToken::new().expect("the cancel event must be creatable");
        arm(first.clone()).expect("the first arm must install the handler");
        disarm();

        let second = CancelToken::new().expect("a second cancel event must be creatable");
        arm(second.clone()).expect("re-arming after a disarm must succeed");
        assert!(
            !second.is_cancelled(),
            "a freshly armed token is not cancelled"
        );
        second
            .cancel()
            .expect("signalling the armed token must work");
        assert!(second.is_cancelled());
        disarm();
    }
}
