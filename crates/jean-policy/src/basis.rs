//! Why an allowed operation was allowed.
//!
//! The distinction matters more than the allow. `jean audit` answering "was this
//! run because somebody typed it, or because a year-old approval is still
//! standing?" is the question an operator asks after something went wrong, and a
//! boolean would have thrown that answer away.

use std::fmt;

/// The authority behind a decision to allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authority {
    /// The operator typed this command, and the work is routine for it.
    ///
    /// This is the ordinary path. There is no prompt, because asking a person to
    /// confirm the thing they just asked for is not consent, it is friction — and
    /// friction teaches people to answer yes without reading. The scope of this
    /// authority is one invocation of one command, and it cannot be widened by
    /// anything the command reads: see [`crate::Policy`]'s rule that tool output
    /// grants nothing.
    OwnerCommand,
    /// The operator authorised this run, and this step is one of the steps the validated plan
    /// resolved to — same plan, same step, same resolved scope digest.
    ///
    /// Distinct from [`Authority::OwnerCommand`] because a plan step was written by a model: the
    /// allow rests on the run the person asked for, not on a command they typed for this operation.
    /// Distinct from [`Authority::ApprovedThisTime`] because nobody was asked about this step, and
    /// from [`Authority::Remembered`] because it expires with the run and is never in the ledger.
    /// See [`crate::grant`].
    RunScope,
    /// An action an **external worker** asked for, inside the boundary the operator set when they
    /// delegated the task: this worker, this capability, inside this workspace.
    ///
    /// Its own variant rather than a reuse of [`Authority::RunScope`] because the two cover different
    /// things and must not be confused in the audit trail. A run grant says *a person read this step of
    /// this plan and said yes to its exact resolved digest*; a delegation grant says *a person handed
    /// this worker a directory and a capability class for one task*, and the individual file the agent
    /// wanted at 03:00 was never shown to anybody. The only defence for the second kind is that it is
    /// bounded — the workspace, the capability, and the sandbox the worker was started with — and
    /// `jean-core` checks all three before it reports this authority. Anything outside them is not a
    /// weaker yes: it is a fresh operation, and it goes through the ordinary gate with a prompt.
    DelegationScope,
    /// The operator was asked, and said yes to this operation with these arguments.
    ApprovedThisTime,
    /// A previous yes, bound to the same operation and the same validated arguments
    /// and target, is still inside its window.
    Remembered { id: i64, granted_at_ms: i64 },
}

impl Authority {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OwnerCommand => "owner_command",
            Self::RunScope => "run_scope",
            Self::DelegationScope => "delegation_scope",
            Self::ApprovedThisTime => "approved_this_time",
            Self::Remembered { .. } => "remembered_approval",
        }
    }

    /// The id to write into the audit trail, if any.
    pub const fn approval_id(self) -> Option<i64> {
        match self {
            Self::Remembered { id, .. } => Some(id),
            _ => None,
        }
    }
}

impl fmt::Display for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OwnerCommand => write!(f, "owner command"),
            Self::RunScope => write!(f, "the operator's authority for this run"),
            Self::DelegationScope => {
                write!(f, "the operator's delegation of this worker and workspace")
            }
            Self::ApprovedThisTime => write!(f, "approved this time"),
            Self::Remembered { id, .. } => write!(f, "remembered approval #{id}"),
        }
    }
}
