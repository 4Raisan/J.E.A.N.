//! Errors produced by the supervisor.
//!
//! Every variant carries enough context to answer the two questions an operator
//! asks next: *which* invocation failed, and *why*. A bare `io::Error` from
//! `CreateProcessW` says "The system cannot find the file specified" without
//! saying which file, which is not actionable.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProcessError {
    /// The launch request itself was malformed, before any Win32 call.
    #[error("{field} is not usable: {reason}")]
    Invalid { field: &'static str, reason: String },

    /// The executable does not exist or is not a file.
    ///
    /// Reported separately from [`ProcessError::CreateProcess`] because the fix
    /// is different: this one means the tool registry or PATH resolution is
    /// wrong, not that Windows refused.
    #[error("executable not found: {path}")]
    NoSuchExecutable { path: PathBuf },

    /// `CreateProcessW` failed.
    #[error("could not start {executable}: {reason}")]
    CreateProcess { executable: PathBuf, reason: String },

    /// The child was created but could not be placed in a Job Object, so it was
    /// terminated immediately. Supervision was never established.
    ///
    /// The most common real cause is UIPI: the target runs at a higher integrity
    /// level than J.E.A.N., and Windows fails the assignment rather than letting
    /// a medium-integrity process own a high-integrity one.
    #[error("child {pid} could not be assigned to a job object and was terminated: {reason}")]
    JobAssignment { pid: u32, reason: String },

    /// The process was created and resumed but something in the wait/collect
    /// path failed. The child is not leaked: the job's kill-on-close still
    /// applies, so dropping the job handle terminates the tree.
    #[error("supervision of {executable} failed: {reason}")]
    Supervision { executable: PathBuf, reason: String },

    /// Captured output could not be written to its artifact file.
    #[error("could not write {stream} output to {path}: {reason}")]
    Capture {
        stream: &'static str,
        path: PathBuf,
        reason: String,
    },

    /// A Win32 call outside process creation failed.
    #[error("{context} failed: {reason}")]
    Win32 {
        context: &'static str,
        reason: String,
    },

    /// The cancellation token could not be created, so the run was refused.
    ///
    /// Refusing is the correct response: starting a child that cannot be
    /// cancelled would break the one guarantee the supervisor exists to provide.
    #[error("cancellation could not be armed: {reason}")]
    CancelToken { reason: String },
}

impl ProcessError {
    /// Build a `Win32` variant from a rendered error.
    ///
    /// Takes a `String` rather than a `windows::core::Error` so this module
    /// stays free of the platform dependency; callers in `win32` render the
    /// error at the site where they know which call produced it.
    pub(crate) fn win32(context: &'static str, reason: impl std::fmt::Display) -> Self {
        ProcessError::Win32 {
            context,
            reason: reason.to_string(),
        }
    }

    /// True when the failure happened before the child existed.
    ///
    /// Lets the caller pick `launch_failed` rather than `job_assignment_failed`
    /// without string-matching the message.
    pub fn is_pre_spawn(&self) -> bool {
        matches!(
            self,
            ProcessError::Invalid { .. }
                | ProcessError::NoSuchExecutable { .. }
                | ProcessError::CreateProcess { .. }
                | ProcessError::CancelToken { .. }
        )
    }
}

pub type Result<T> = std::result::Result<T, ProcessError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_spawn_failures_are_identifiable_without_matching_text() {
        assert!(ProcessError::NoSuchExecutable {
            path: PathBuf::from("C:\\nope.exe"),
        }
        .is_pre_spawn());
        assert!(ProcessError::CreateProcess {
            executable: PathBuf::from("C:\\nope.exe"),
            reason: "not found".into(),
        }
        .is_pre_spawn());
        assert!(ProcessError::CancelToken { reason: "x".into() }.is_pre_spawn());
    }

    #[test]
    fn post_spawn_failures_are_not_reported_as_pre_spawn() {
        assert!(!ProcessError::JobAssignment {
            pid: 42,
            reason: "access denied".into(),
        }
        .is_pre_spawn());
        assert!(!ProcessError::Supervision {
            executable: PathBuf::from("C:\\x.exe"),
            reason: "wait failed".into(),
        }
        .is_pre_spawn());
    }

    #[test]
    fn messages_name_the_thing_that_failed() {
        let e = ProcessError::CreateProcess {
            executable: PathBuf::from("C:\\tools\\git.exe"),
            reason: "The system cannot find the file specified. (os error 2)".into(),
        };
        let text = e.to_string();
        assert!(text.contains("git.exe"), "{text}");
        assert!(text.contains("os error 2"), "{text}");
    }
}
