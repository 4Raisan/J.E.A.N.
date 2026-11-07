//! Local capability probes for J.E.A.N. — read-only, bounded, and never the thing
//! that acts.
//!
//! ## The contract
//!
//! **This crate never launches an application, never kills a process, and never writes
//! outside `%TEMP%`.** It answers questions: what is installed and where it lives, what
//! files match this name, what is a process doing right now. Every capability here is
//! inspection of state somebody else owns.
//!
//! That is not a modest claim, it is the reason the crate can be called early in a task
//! pipeline. Discovery runs against directories and registry hives that any program the
//! user has ever run was allowed to write into. If resolving a name could also *start*
//! the thing it found, then a typo would be a code-execution primitive, and the policy
//! engine downstream would have nothing left to decide about. So:
//!
//! * [`apps`] resolves names to paths and stops. Handing a path to a launcher is
//!   `jean-core`'s job, after approval.
//! * [`process`] reports. There is no terminate, suspend, signal or resume function in
//!   this crate, and cancellation of work J.E.A.N. started belongs to `jean-process`,
//!   where it goes through a Job Object so the whole tree dies together.
//! * [`search`] reads directory entries and file *names*. It never opens a file for
//!   reading, which is why a personal directory can be searched without indexing what
//!   is inside it.
//! * [`desktop`] looks at the window station and says which process owns a window a
//!   person can see. It never sets, restores, minimises, closes or foregrounds one —
//!   the foreground-lock trap that the Phase-4 actuator has to solve is refused here
//!   outright, so no probe result can quietly become an action.
//!
//! ## Two habits that run through every module
//!
//! **Ambiguity is a result, not an error.** Several candidates for one query, several
//! processes for one name — they are all returned, ranked, with a flag saying the match
//! is ambiguous. The caller refuses rather than picks, because "first" depends on
//! enumeration order and nobody agreed to that being a security property.
//!
//! **Bounds are reported, not hidden.** Candidate counts, directories opened, entries
//! examined, deadlines, row limits: each one has a named stop condition, and a run says
//! which bound ended it. "No matches" and "I stopped after 200 000 entries" are answers
//! to different questions, and printing the first when the second is true is how an
//! orchestrator quietly becomes unreliable.
//!
//! ## Module map
//!
//! | Module | Responsibility | Windows FFI |
//! |---|---|---|
//! | [`error`] | Errors that say which source refused and with what code | no |
//! | [`apps`] | Installed-application discovery: PATH, Start Menu, registry | yes (COM + registry) |
//! | [`search`] | Bounded recursive file location by name | attribute bit only |
//! | [`process`] | Process CPU/memory/identity via `sysinfo` | via `sysinfo` |
//! | [`desktop`] | Which process owns a window an operator can see | yes — but see below |
//!
//! [`desktop`]'s FFI column needs the qualifier the crate's own rules require: the
//! bindings it uses (`EnumWindows` and friends) live behind the
//! `Win32_UI_WindowsAndMessaging` feature of the `windows` crate, which is **not**
//! declared in the workspace dependency set. The module compiles and its filters,
//! bounds and wait logic are fully implemented and tested, but the live walk reports
//! that it could not look rather than claiming there was nothing to see. Read
//! [`desktop`]'s status section before wiring it into a launch verification.
//!
//! ## What is deliberately absent
//!
//! No persistence (`jean-store`), no supervision (`jean-process`), no policy decisions
//! (`jean-policy`), no external search tools. No window mutation either: no foreground,
//! no restore, no close — the window station is read and nothing else. This crate does
//! not shell out to `fd`, `where.exe` or `Get-Process`: those produce output shaped for
//! a terminal, fail in ways that cannot be distinguished from empty results, and — for
//! `where.exe` — resolve `.bat` and `.cmd` files, which this project refuses to treat as
//! launchable.

#[cfg(not(windows))]
compile_error!(
    "jean-tools is Windows-only: application discovery reads the Start Menu through COM and \
     the registry, and the reparse-point rule that keeps a file search inside its authorised \
     root depends on Windows file attributes. There is no portable equivalent worth faking. \
     The supported target is stable-x86_64-pc-windows-msvc."
);

pub mod apps;
pub mod desktop;
pub mod error;
pub mod process;
pub mod search;

pub use error::{Result, ToolError};

// Re-exported so the CLI does not need to reach into submodules just to name what comes
// back, mirroring how `jean-core` re-exports `jean-process`'s outcome types.
pub use apps::{
    discover_apps, discover_installed_apps, AppCandidate, DiscoveryBound, DiscoveryEnv,
    DiscoveryLimits, DiscoveryOutcome, DiscoverySource, MatchKind, Provenance,
};
pub use desktop::{
    any_window_for_pid, clamp_poll, counts_as_operator_window, pid_is_running, wait_for_window,
    wait_for_window_with, windows_for_pid, windows_for_pid_capped, windows_for_pid_with, RawWindow,
    SystemWindowStation, WindowInfo, WindowReport, WindowStation, WindowWait, MAX_POLL,
    MAX_TITLE_CHARS, MAX_WINDOWS_REPORTED, MIN_POLL,
};
pub use process::{
    inspect_by_name, inspect_pid, inspect_self, list_processes, InspectOutcome, MachineSummary,
    ProcessListing, ProcessReport, SortKey,
};
pub use search::{
    search_names, FileHit, NamePattern, PatternKind, SearchLimits, SearchReport, SearchStop,
};

/// The version of this crate, for `jean --version` and for audit records that want to
/// say which build produced a discovery result.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
