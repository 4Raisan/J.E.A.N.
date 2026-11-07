//! The verdict, and the reasons a verdict can be no.

use crate::basis::Authority;
use std::fmt;

/// The result of deciding one operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allowed { authority: Authority },
    Denied(DenyReason),
}

impl Decision {
    pub const fn allowed(authority: Authority) -> Self {
        Self::Allowed { authority }
    }

    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }

    pub const fn authority(&self) -> Option<Authority> {
        match self {
            Self::Allowed { authority } => Some(*authority),
            Self::Denied(_) => None,
        }
    }

    pub const fn reason(&self) -> Option<&DenyReason> {
        match self {
            Self::Denied(reason) => Some(reason),
            Self::Allowed { .. } => None,
        }
    }

    /// The value written to the audit trail's `decision` field.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Allowed { .. } => "allowed",
            Self::Denied(_) => "denied",
        }
    }
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allowed { authority } => write!(f, "allowed ({authority})"),
            Self::Denied(reason) => write!(f, "denied: {reason}"),
        }
    }
}

/// Why an operation did not run.
///
/// Every variant has to be sayable to the operator in one line, because a denial
/// that does not explain itself is indistinguishable from a bug — and the two need
/// opposite responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// The capability is on `auto_deny`. There was no prompt, and a prompt would
    /// not have helped: this is refused by configuration, not by a person.
    AutoDenied { capability: String },
    /// The operation needs an administrator token and this build does not have one,
    /// by design. D-010.
    ElevationRequired { helper: String },
    /// A person was asked and said no.
    Declined,
    /// A person was asked and did not answer — the timeout ran out, the stream
    /// closed, or the answer was not one of the offered words.
    Unanswerable(String),
    /// The operation falls outside a boundary the operator already drew, so asking
    /// would be dishonest.
    ///
    /// Phase 3 uses this for a delegated worker that reaches outside the workspace
    /// it was pointed at: the answer does not depend on the operator's mood, and a
    /// prompt would offer a `yes` that could not be honoured. It is deliberately not
    /// configurable — no approval record widens the directory a person handed over.
    OutsideScope { detail: String },
}

impl DenyReason {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::AutoDenied { .. } => "auto_denied",
            Self::ElevationRequired { .. } => "elevation_required",
            Self::Declined => "declined",
            Self::Unanswerable(_) => "unanswerable",
            Self::OutsideScope { .. } => "outside_scope",
        }
    }

    /// Whether anything could have changed this answer.
    ///
    /// The CLI uses it to decide whether to mention `jean approvals`: pointing
    /// somebody at a prompt for a capability the configuration refuses outright
    /// would send them to the wrong place.
    pub const fn is_configurable(&self) -> bool {
        matches!(self, Self::AutoDenied { .. })
    }
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AutoDenied { capability } => write!(
                f,
                "`{capability}` is on the auto-deny list; it is refused without a prompt, and no \
                 approval can unlock it"
            ),
            Self::ElevationRequired { helper } => write!(
                f,
                "this operation needs an administrator token and J.E.A.N. runs at your own \
                 integrity level by design (D-010); the sidecar that would perform a single \
                 scoped elevated operation is `{helper}`, and it is not part of this build"
            ),
            Self::Declined => write!(f, "the operator declined"),
            Self::Unanswerable(why) => write!(f, "the question was not answered: {why}"),
            Self::OutsideScope { detail } => write!(
                f,
                "it is outside a boundary the operator already set, so there was nothing to ask: \
                 {detail}"
            ),
        }
    }
}
