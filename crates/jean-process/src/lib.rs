//! Supervised child processes for J.E.A.N.
//!
//! This crate exists to make one promise reliable: **when J.E.A.N. stops a task,
//! everything it started stops too.** Everything else here — bounded output
//! capture, exit-code discrimination, timeout handling — is in service of being
//! able to say what actually happened.
//!
//! ## Why not `std::process::Command`
//!
//! `Command::kill` terminates one process. A tool that spawns a build that
//! spawns a test runner that spawns three compilers leaves four survivors, and
//! the user sees a "cancelled" task whose processes are still eating the CPU.
//! The Rust standard library offers no process-group or job mechanism on
//! Windows, so this crate drives `CreateProcessW` directly and assigns every
//! child to a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
//!
//! It also needs `CREATE_SUSPENDED`, because assigning a process to a job
//! *after* it has started leaves a window where a descendant can escape. See
//! [`runner`] for the full sequence and why the order cannot change.
//!
//! ## Layering
//!
//! | Module | Concern | Windows FFI |
//! |---|---|---|
//! | [`error`] | Actionable error types | no |
//! | [`cmdline`] | argv → command-line encoding | no |
//! | [`env`] | environment block encoding | no |
//! | [`spec`] | the launch request and the outcome | no |
//! | [`output`] | bounded capture with truncation markers | no |
//! | [`win32`] | every `unsafe` block in the crate | yes |
//! | [`conpty`] | the pseudo console, and the console-mode guard | yes |
//! | [`runner`] | the supervised-execution sequence | via `win32`, `conpty` |
//!
//! The unsafe surface is confined to the two platform modules so the sequencing
//! logic can be read, and reviewed, as safe Rust.
//!
//! ## What this crate does not do
//!
//! * **Policy.** It never decides whether a tool may run, what tier it needs, or
//!   whether approval is required. `jean-core` builds a [`spec::LaunchSpec`] that
//!   has already been through the policy engine and the environment allowlist.
//! * **Persistence.** It returns a [`spec::ChildOutcome`]; writing it to SQLite
//!   is `jean-store`'s job.
//! * **Verification.** A zero exit code is reported, not interpreted. Whether the
//!   user's goal was achieved is a separate step, and conflating the two is how
//!   an orchestrator ends up reporting success for work that did not happen.
//! * **Terminal emulation.** A run asked for with [`spec::TerminalMode::Pty`] gets a
//!   real pseudo console and its keystrokes forwarded, but nothing here interprets
//!   the screen: resizing it to follow a window, or turning the ANSI stream back
//!   into something a model should read, belongs above this crate.

#[cfg(not(windows))]
compile_error!(
    "jean-process is Windows-only: process-tree containment depends on Job \
     Objects, and there is no portable equivalent worth abstracting over in \
     Phase 0. The supported target is stable-x86_64-pc-windows-msvc."
);

pub mod channel;
pub mod cmdline;
pub mod conpty;
pub mod env;
pub mod error;
pub mod output;
pub mod piped;
pub mod runner;
pub mod spec;
pub mod win32;

pub use error::{ProcessError, Result};
pub use piped::{spawn as spawn_piped, Control as PipedControl, PipedChild};
pub use runner::{launch_detached, supervise, CancelToken, DetachedOutcome};
pub use spec::{ChildOutcome, LaunchSpec, TerminalMode};

// Re-exported so callers do not have to depend on `jean-protocol` just to name
// the types that come back from `supervise`.
pub use jean_protocol::{StreamSummary, Termination};

/// Convenience for tests and fixtures: the exit code J.E.A.N. writes into a job
/// it terminated because the deadline elapsed.
pub use win32::{CANCEL_EXIT_CODE, TIMEOUT_EXIT_CODE};
