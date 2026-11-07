//! The task state machine.
//!
//! All nine states from `PLANNING/05-ROADMAP.md` Phase 0 exist here, including
//! the ones Phase 0 does not yet drive (`Planning`, `AwaitingApproval`,
//! `Verifying`). They are present because the transition table is the contract
//! later phases build on, and because an illegal transition must be rejected
//! *now* rather than discovered when a model-driven planner arrives.
//!
//! Terminal states admit no outgoing transitions. `NeedsAttention` is the one
//! non-terminal resting state: it is where reconciled orphans land, and a human
//! (not the model) resolves it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Planning,
    AwaitingApproval,
    Running,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
    NeedsAttention,
}

/// Every state, in declaration order. Used by tests to exhaustively walk the
/// transition matrix so an added state cannot silently skip coverage.
pub const ALL_STATES: [TaskState; 9] = [
    TaskState::Queued,
    TaskState::Planning,
    TaskState::AwaitingApproval,
    TaskState::Running,
    TaskState::Verifying,
    TaskState::Succeeded,
    TaskState::Failed,
    TaskState::Cancelled,
    TaskState::NeedsAttention,
];

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Planning => "planning",
            TaskState::AwaitingApproval => "awaiting_approval",
            TaskState::Running => "running",
            TaskState::Verifying => "verifying",
            TaskState::Succeeded => "succeeded",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
            TaskState::NeedsAttention => "needs_attention",
        }
    }

    /// A terminal state never changes again without human intervention.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
        )
    }

    /// States that mean "some process may still be alive and owned by a runner".
    ///
    /// Reconciliation only ever considers these. A task in `Queued` has no
    /// process yet, so an orphaned `Queued` row is simply re-queueable rather
    /// than suspicious — but it is still included here because a crash between
    /// "row written" and "process spawned" leaves it stranded and a human must
    /// decide.
    pub fn is_in_flight(self) -> bool {
        matches!(
            self,
            TaskState::Queued
                | TaskState::Planning
                | TaskState::AwaitingApproval
                | TaskState::Running
                | TaskState::Verifying
        )
    }

    /// The explicit legal-transition table.
    ///
    /// Anything not listed here is illegal, including self-transitions. A
    /// self-transition would let a caller "refresh" a terminal state and is
    /// therefore rejected: state changes must be real changes.
    pub fn allowed_transitions(self) -> &'static [TaskState] {
        match self {
            TaskState::Queued => &[
                TaskState::Planning,
                TaskState::Running,
                TaskState::Cancelled,
                TaskState::Failed,
                TaskState::NeedsAttention,
            ],
            TaskState::Planning => &[
                TaskState::AwaitingApproval,
                TaskState::Running,
                TaskState::Failed,
                TaskState::Cancelled,
                TaskState::NeedsAttention,
            ],
            TaskState::AwaitingApproval => &[
                TaskState::Planning,
                TaskState::Running,
                TaskState::Failed,
                TaskState::Cancelled,
                TaskState::NeedsAttention,
            ],
            TaskState::Running => &[
                TaskState::Verifying,
                TaskState::Failed,
                TaskState::Cancelled,
                TaskState::NeedsAttention,
            ],
            TaskState::Verifying => &[
                TaskState::Succeeded,
                TaskState::Failed,
                TaskState::Cancelled,
                TaskState::NeedsAttention,
            ],
            // Terminal: no way out.
            TaskState::Succeeded => &[],
            TaskState::Failed => &[],
            TaskState::Cancelled => &[],
            // Human-resolvable. Returning to Queued is an explicit retry.
            TaskState::NeedsAttention => &[
                TaskState::Queued,
                TaskState::Planning,
                TaskState::Succeeded,
                TaskState::Failed,
                TaskState::Cancelled,
            ],
        }
    }

    pub fn can_transition_to(self, next: TaskState) -> bool {
        self.allowed_transitions().contains(&next)
    }

    /// Checked transition. Returns `Ok(next)` or a `TransitionError` naming both
    /// states and the legal set, so a log line explains itself.
    pub fn transition(self, next: TaskState) -> Result<TaskState, TransitionError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(TransitionError::Illegal {
                from: self,
                to: next,
                allowed: self.allowed_transitions().to_vec(),
            })
        }
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskState {
    type Err = TransitionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ALL_STATES
            .into_iter()
            .find(|st| st.as_str() == s)
            .ok_or_else(|| TransitionError::UnknownState(s.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransitionError {
    #[error(
        "illegal task state transition {from} -> {to}; legal targets from {from}: {allowed:?}"
    )]
    Illegal {
        from: TaskState,
        to: TaskState,
        allowed: Vec<TaskState>,
    },

    #[error("unknown task state {0:?}")]
    UnknownState(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every one of the 81 ordered pairs is classified, and the classification
    /// matches the table. This is the exhaustive check the roadmap asks for.
    #[test]
    fn full_transition_matrix_is_explicit() {
        let mut legal = 0usize;
        let mut illegal = 0usize;

        for from in ALL_STATES {
            for to in ALL_STATES {
                let allowed = from.allowed_transitions().contains(&to);
                assert_eq!(
                    allowed,
                    from.can_transition_to(to),
                    "can_transition_to disagrees with the table for {from} -> {to}"
                );

                if allowed {
                    assert_eq!(from.transition(to), Ok(to));
                    legal += 1;
                } else {
                    let err = from.transition(to).expect_err("should be illegal");
                    assert!(matches!(err, TransitionError::Illegal { .. }));
                    illegal += 1;
                }
            }
        }

        assert_eq!(legal + illegal, 81, "matrix must cover all 9x9 pairs");
        // Documented expectations: 5+5+5+4+4+0+0+0+5 = 28 legal edges.
        assert_eq!(legal, 28, "unexpected number of legal transitions");
        assert_eq!(illegal, 53);
    }

    #[test]
    fn self_transitions_are_always_illegal() {
        for st in ALL_STATES {
            assert!(
                !st.can_transition_to(st),
                "{st} must not self-transition; that would let a terminal state be refreshed"
            );
        }
    }

    #[test]
    fn terminal_states_have_no_exits() {
        for st in [
            TaskState::Succeeded,
            TaskState::Failed,
            TaskState::Cancelled,
        ] {
            assert!(st.is_terminal());
            assert!(
                st.allowed_transitions().is_empty(),
                "{st} is terminal and must admit no transitions"
            );
            for other in ALL_STATES {
                assert!(!st.can_transition_to(other));
            }
        }
    }

    #[test]
    fn needs_attention_is_resolvable_by_a_human() {
        let na = TaskState::NeedsAttention;
        assert!(!na.is_terminal(), "needs_attention must remain actionable");
        assert!(na.can_transition_to(TaskState::Queued), "retry path");
        assert!(
            na.can_transition_to(TaskState::Succeeded),
            "human marks it done"
        );
        assert!(
            na.can_transition_to(TaskState::Failed),
            "human marks it failed"
        );
        assert!(na.can_transition_to(TaskState::Cancelled), "human drops it");
        // It must NOT be able to jump straight back into Running: a reconciled
        // orphan is never silently re-executed.
        assert!(
            !na.can_transition_to(TaskState::Running),
            "needs_attention must not resume execution without going through Queued"
        );
    }

    #[test]
    fn the_phase0_happy_path_is_legal() {
        let path = [
            TaskState::Queued,
            TaskState::Running,
            TaskState::Verifying,
            TaskState::Succeeded,
        ];
        for w in path.windows(2) {
            assert_eq!(w[0].transition(w[1]), Ok(w[1]), "{} -> {}", w[0], w[1]);
        }
    }

    #[test]
    fn skipping_states_is_rejected() {
        // Queued cannot jump to Succeeded: nothing ran, so nothing was verified.
        assert!(matches!(
            TaskState::Queued.transition(TaskState::Succeeded),
            Err(TransitionError::Illegal { .. })
        ));
        // Running cannot claim Succeeded directly; it must pass through Verifying.
        assert!(matches!(
            TaskState::Running.transition(TaskState::Succeeded),
            Err(TransitionError::Illegal { .. })
        ));
    }

    #[test]
    fn in_flight_states_are_disjoint_from_terminal_states() {
        let mut neither = Vec::new();
        for st in ALL_STATES {
            assert!(
                !(st.is_in_flight() && st.is_terminal()),
                "{st} is both in-flight and terminal; a task with a live runner \
                 cannot also be finished"
            );
            if !st.is_in_flight() && !st.is_terminal() {
                neither.push(st);
            }
        }
        // `needs_attention` is the one state deliberately neither: no runner owns
        // it, so `jean tasks --in-flight` must not list it as work still going,
        // and it is not final, because a human can still move it on. Pinning the
        // set means a second such state has to be justified here rather than
        // slipping in unnoticed.
        assert_eq!(neither, vec![TaskState::NeedsAttention], "{neither:?}");
    }

    #[test]
    fn state_strings_round_trip() {
        for st in ALL_STATES {
            assert_eq!(st.as_str().parse::<TaskState>(), Ok(st));
            assert_eq!(st.to_string(), st.as_str());
        }
        assert!(matches!(
            "not_a_state".parse::<TaskState>(),
            Err(TransitionError::UnknownState(_))
        ));
    }

    #[test]
    fn serde_uses_snake_case_strings() {
        // The database stores these strings; changing them is a migration.
        assert_eq!(
            serde_json::to_string(&TaskState::AwaitingApproval).unwrap(),
            "\"awaiting_approval\""
        );
        assert_eq!(
            serde_json::to_string(&TaskState::NeedsAttention).unwrap(),
            "\"needs_attention\""
        );
        let back: TaskState = serde_json::from_str("\"needs_attention\"").unwrap();
        assert_eq!(back, TaskState::NeedsAttention);
    }

    #[test]
    fn illegal_transition_error_names_both_states_and_the_legal_set() {
        let err = TaskState::Succeeded
            .transition(TaskState::Running)
            .expect_err("terminal state");
        let msg = err.to_string();
        assert!(msg.contains("succeeded"), "message was: {msg}");
        assert!(msg.contains("running"), "message was: {msg}");
        assert!(
            msg.contains("legal targets"),
            "message must state what IS allowed: {msg}"
        );
    }
}
