//! Execution facts shared by the supervisor and the store.
//!
//! These two types are the seam between "what happened to the child process"
//! ([`crate::Termination`]) and "what we kept of its output"
//! ([`crate::StreamSummary`]). They live in `jean-protocol` rather than in
//! either consumer because both need them and neither should depend on the
//! other: `jean-process` produces them without knowing about SQLite, and
//! `jean-store` persists them without knowing about Win32.

/// How a tool run ended.
///
/// Distinct from the task state, and deliberately more granular than
/// success/failure. A task can be `Failed` *because* its tool `TimedOut`, and
/// collapsing the two is what produces unactionable error messages like
/// "the task failed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    /// The child exited by itself. `exit_code` is then meaningful.
    Exited,
    /// The Job Object was terminated because the deadline elapsed.
    Timeout,
    /// The Job Object was terminated because a cancel request arrived.
    Cancelled,
    /// The child could not be created at all (bad path, access denied, not an
    /// executable). No pid exists.
    LaunchFailed,
    /// The child was created but could not be placed in a Job Object, and was
    /// therefore terminated immediately.
    ///
    /// Reported separately from `LaunchFailed` because it means the process
    /// *did* briefly exist outside supervision. See `PLANNING/06-RISKS.md` R-14.
    JobAssignmentFailed,
    /// The runner died before recording an outcome. Set only by reconciliation,
    /// never by the supervisor.
    Orphaned,
}

impl Termination {
    pub fn as_str(self) -> &'static str {
        match self {
            Termination::Exited => "exited",
            Termination::Timeout => "timeout",
            Termination::Cancelled => "cancelled",
            Termination::LaunchFailed => "launch_failed",
            Termination::JobAssignmentFailed => "job_assignment_failed",
            Termination::Orphaned => "orphaned",
        }
    }

    /// Parse the persisted form. An inherent `from_str` rather than `FromStr`
    /// because failure here is expected (a database written by a newer build)
    /// and callers want `Option`, not a `Result` they must unwrap.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "exited" => Termination::Exited,
            "timeout" => Termination::Timeout,
            "cancelled" => Termination::Cancelled,
            "launch_failed" => Termination::LaunchFailed,
            "job_assignment_failed" => Termination::JobAssignmentFailed,
            "orphaned" => Termination::Orphaned,
            _ => return None,
        })
    }

    /// True when J.E.A.N. ended the process rather than the process ending
    /// itself. Used to decide whether an exit code is worth reporting at all:
    /// after a job kill the code is the one we chose, not the child's opinion.
    pub fn was_forced(self) -> bool {
        matches!(
            self,
            Termination::Timeout
                | Termination::Cancelled
                | Termination::LaunchFailed
                | Termination::JobAssignmentFailed
                | Termination::Orphaned
        )
    }
}

impl std::fmt::Display for Termination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one captured stream produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamSummary {
    /// Where the retained prefix was written. `None` if capture was disabled.
    pub path: Option<String>,
    /// Bytes the child actually produced, **including the discarded excess**.
    /// Reporting only the retained bytes would hide the fact that output was
    /// dropped, which is exactly what a truncation marker exists to prevent.
    pub bytes: u64,
    /// True when `bytes` exceeded the cap and output was dropped.
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Termination; 6] = [
        Termination::Exited,
        Termination::Timeout,
        Termination::Cancelled,
        Termination::LaunchFailed,
        Termination::JobAssignmentFailed,
        Termination::Orphaned,
    ];

    #[test]
    fn every_termination_round_trips_through_its_string() {
        for t in ALL {
            assert_eq!(Termination::from_str(t.as_str()), Some(t));
        }
    }

    #[test]
    fn the_strings_are_distinct() {
        let mut seen = std::collections::BTreeSet::new();
        for t in ALL {
            assert!(seen.insert(t.as_str()), "{} is duplicated", t.as_str());
        }
    }

    #[test]
    fn unknown_strings_do_not_parse() {
        for s in ["", "EXITED", "vaporised", "exited ", "orphan"] {
            assert_eq!(Termination::from_str(s), None, "{s:?} should not parse");
        }
    }

    #[test]
    fn only_a_natural_exit_counts_as_unforced() {
        assert!(!Termination::Exited.was_forced());
        for t in ALL {
            if t == Termination::Exited {
                continue;
            }
            assert!(t.was_forced(), "{t} should be reported as forced");
        }
    }

    #[test]
    fn display_matches_as_str() {
        for t in ALL {
            assert_eq!(t.to_string(), t.as_str());
        }
    }

    #[test]
    fn a_default_stream_summary_describes_no_output() {
        let s = StreamSummary::default();
        assert!(s.path.is_none());
        assert_eq!(s.bytes, 0);
        assert!(!s.truncated);
    }
}
