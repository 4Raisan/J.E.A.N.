//! Errors for the read-only capability probes.
//!
//! Every variant answers the question an operator actually asks next: *which*
//! source, key, root or pid, and *what* to do about it. Two rules shape this enum
//! more than the others do.
//!
//! **A bound that was hit is not a failure of the probe.** Discovery stopping at
//! 24 candidate directories, and a search stopping at 200 results, are both
//! successful runs with incomplete coverage — so they carry the numbers that let
//! the caller say so. `jean find C:\` reporting "stopped after 200_000 entries" is
//! a better outcome than reporting nothing, and worse than reporting everything;
//! the message has to make clear which of those happened.
//!
//! **Refused data is named, never invented.** Where Windows would not hand over a
//! command line, or a registry key could not be opened, the error says which one
//! and with what code. A zero-filled report would look like an idle process or an
//! empty key, and that resemblance is the whole danger.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    /// A registry key could not be opened, or a value inside it could not be read.
    ///
    /// Reported separately from "the key does not exist": the difference decides
    /// whether the operator should grant rights or fix a path. `code` is the raw
    /// Win32 error so it can be looked up rather than paraphrased.
    #[error("registry access failed for {key} ({context}): win32 code {code}")]
    Registry {
        key: String,
        context: &'static str,
        code: u32,
    },

    /// A registry subtree could not be enumerated because it is protected.
    ///
    /// This is a *partial result*, not a dead end: discovery keeps going and the
    /// caller reports which sources were actually seen. Silently skipping an
    /// inaccessible hive is how "no such application" gets said about an
    /// application that is installed.
    #[error("registry subtree {key} is not readable by this account (win32 code {code}); its applications are absent from this result")]
    RegistryInaccessible { key: String, code: u32 },

    /// COM could not be initialised on this thread, so `.lnk` targets cannot be
    /// resolved at all.
    #[error("COM initialisation failed ({context}): HRESULT 0x{code:08X}")]
    ComInit { context: &'static str, code: u32 },

    /// The shell-link factory could not be created or asked for its target.
    ///
    /// Carries the link path because the failure is usually per-link (a corrupt or
    /// foreign `.lnk`), not global, and the operator needs the file name to tell
    /// those two apart.
    #[error("could not resolve the target of {link}: {reason}")]
    ShellLink { link: PathBuf, reason: String },

    /// Application discovery stopped because a configured bound was reached.
    ///
    /// The candidate list gathered so far is still valid and is returned by the
    /// caller; this error exists to say the list is known-incomplete.
    #[error("application discovery stopped at the {bound} bound ({value}; {detail})")]
    DiscoveryBound {
        bound: &'static str,
        value: usize,
        detail: String,
    },

    /// The directory handed to the search is not usable as a search root.
    ///
    /// Distinguished from "no matches": a typo in the root must not be reported as
    /// an empty result set, because the reader would conclude the file does not
    /// exist.
    #[error("invalid search root {path}: {reason}")]
    InvalidSearchRoot { path: PathBuf, reason: String },

    /// Process inspection is not available on this build.
    ///
    /// Either the platform is not supported by the backend (`System::IS_SUPPORTED`
    /// false) or the constructor itself refused. Returning zeros here would be a
    /// lie about an idle machine, so the caller is told to stop.
    #[error("process inspection is unavailable here: {reason}")]
    SysinfoUnavailable { reason: String },

    /// No process with that id existed at the moment of sampling.
    ///
    /// Not an error condition in the operator's sense — see [`crate::process`],
    /// which returns the typed `Gone` outcome instead of this variant whenever the
    /// process merely exited mid-sample. This variant is reserved for the call that
    /// could not be made at all.
    #[error("could not inspect process {pid}: {reason}")]
    ProcessQuery { pid: u32, reason: String },

    /// An I/O operation outside the probes' own tolerances failed.
    ///
    /// Directory walks and re-verifications swallow their per-entry errors into
    /// bounded lists instead of raising this; it survives for the operations where
    /// there is no partial answer, like creating a fixture directory.
    #[error("{context} failed: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: std::io::Error,
    },
}

impl ToolError {
    /// Build the I/O variant at the site that knows what was being attempted.
    ///
    /// `std::io::Error`'s own message names a syscall, not an intent; prefixing
    /// the intent here is what makes the error readable without a backtrace.
    pub fn io(context: &'static str, source: std::io::Error) -> Self {
        ToolError::Io { context, source }
    }

    /// True when the run produced a usable-but-incomplete answer rather than none.
    ///
    /// Callers use this to decide whether to print partial results. Bound hits and
    /// inaccessible registry hives qualify; a bad search root does not.
    pub fn is_partial(&self) -> bool {
        matches!(
            self,
            ToolError::DiscoveryBound { .. } | ToolError::RegistryInaccessible { .. }
        )
    }
}

/// Convenience alias mirroring the other crates in the workspace.
pub type Result<T> = std::result::Result<T, ToolError>;
