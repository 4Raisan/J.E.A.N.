//! Authority the operator gave to **one run** of **one resolved plan**.
//!
//! ## The problem this solves
//!
//! A plan of three steps that each touch a capability on `policy.require_approval` used to ask the
//! operator three questions about the thing they had just asked for once. That is not consent, it is
//! friction, and friction trains people to answer yes without reading — the same reason
//! [`Authority::OwnerCommand`] exists for a typed command. The difference is that a plan is written
//! by a model, so the answer cannot simply be "the operator typed it": it has to be "the operator
//! typed *this run*, and this step is one of the steps that run was validated to contain".
//!
//! ## What the grant is bound to
//!
//! Three things, all of which must match for it to cover a step:
//!
//! * **the plan** — `plan_id`, so a replacement plan adopted later cannot inherit the yes given to
//!   the plan the operator looked at;
//! * **the step** — `step_key`, so one entry cannot answer for a different position in the run;
//! * **the resolved scope** — [`Operation::scope_digest`], i.e. the operation kind, the tool id,
//!   the resolved target and the argument vector. Not the step's title, not its `summary`, and not
//!   the model's description of it. If what the run is about to do differs from what was resolved
//!   when the grant was minted, the digests differ and the step is asked about like any other.
//!
//! ## What it cannot do
//!
//! It cannot reach an explicit deny or an elevation boundary, because [`crate::Policy`] consults
//! both before it looks at a grant. It cannot be widened by anything a step prints: the entries are
//! fixed when it is built, and there is no method here that adds one. It cannot outlive the run:
//! it is a value owned by `plan_runner::run`, never written to the `approvals` ledger, never
//! serialised, and dropped when the run returns — so the next `jean do` needs its own operator, and
//! `jean audit` will say `run_scope` rather than pretending a person said yes to that step.
//! It cannot be created by a model: the only way to get one is
//! [`RunAuthority::OperatorRun`], which the CLI passes when a person typed the goal.

use std::collections::BTreeMap;

use crate::operation::Operation;

/// Whether this run may carry an operator's authority for its routine steps.
///
/// A parameter rather than a configuration key on purpose: the question is *who asked for this run*,
/// and that is answered by the code path, not by a file. `PromptEachStep` is the Phase-1 shape, kept
/// reachable through `jean do --prompt-each` for an operator who wants to see every step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunAuthority {
    /// A person typed the goal into this command. Routine steps of the validated plan are covered
    /// for this run only, without a prompt each.
    OperatorRun,
    /// Nothing in this process stands behind the run as a whole: every step passes the same gate a
    /// typed intent does.
    PromptEachStep,
}

impl RunAuthority {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OperatorRun => "operator_run",
            Self::PromptEachStep => "prompt_each_step",
        }
    }

    /// Whether a grant should be minted for a plan at all.
    pub const fn grants(self) -> bool {
        matches!(self, Self::OperatorRun)
    }
}

/// The set of steps one operator authorised, and the exact scope each was authorised for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunGrant {
    /// The parent task this run opened. Diagnostics only — matching is on plan, step and digest.
    run_id: String,
    /// The plan whose steps this covers. A superseded plan's grant covers nothing in its replacement.
    plan_id: String,
    /// `step key → resolved scope digest`. Immutable after construction.
    steps: BTreeMap<String, String>,
}

impl RunGrant {
    /// Bind a grant to a run and a plan, from the resolved scope of each of its steps.
    ///
    /// Callers pass entries computed by the *same* code that later runs the step: a grant minted
    /// from a declared-but-unresolved scope would either fail to match, or — worse — match a
    /// different resolved operation than the operator read.
    pub fn new(
        run_id: impl Into<String>,
        plan_id: impl Into<String>,
        steps: Vec<(String, String)>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            plan_id: plan_id.into(),
            steps: steps.into_iter().collect(),
        }
    }

    /// Whether this grant already covers `step_key` of `plan_id` for exactly this operation.
    pub fn covers(&self, plan_id: &str, step_key: &str, operation: &Operation) -> bool {
        if self.plan_id != plan_id {
            return false;
        }
        self.steps
            .get(step_key)
            .is_some_and(|digest| *digest == operation.scope_digest())
    }

    /// How many steps are covered — the number an operator should see before the run starts.
    pub fn covered_steps(&self) -> usize {
        self.steps.len()
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn plan_id(&self) -> &str {
        &self.plan_id
    }

    /// The step keys covered, sorted. For the plan display, never for a decision.
    pub fn covered_step_keys(&self) -> Vec<&str> {
        self.steps.keys().map(String::as_str).collect()
    }
}

/// Which step of which plan a decision is being made for, alongside the grant that may cover it.
///
/// A struct rather than two loose parameters so that a grant can never be consulted without saying
/// what it is being matched against.
#[derive(Debug, Clone, Copy)]
pub struct GrantScope<'a> {
    pub grant: &'a RunGrant,
    pub plan_id: &'a str,
    pub step_key: &'a str,
}

impl<'a> GrantScope<'a> {
    pub fn new(grant: &'a RunGrant, plan_id: &'a str, step_key: &'a str) -> Self {
        Self {
            grant,
            plan_id,
            step_key,
        }
    }
}
