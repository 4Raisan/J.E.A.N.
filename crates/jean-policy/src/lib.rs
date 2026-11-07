//! The deterministic permission engine.
//!
//! Everything in this crate is plain code: given an [`Operation`] and the
//! operator's configuration, it produces the same [`Decision`] every time. The
//! model never votes. This is D-011 — *the model may request an action but never
//! grant one* — expressed as a module boundary: this crate does not depend on any
//! provider, and `jean-core` calls into it rather than around it.
//!
//! Three things decide an operation, in this order:
//!
//! 1. **`auto_deny`** — refused with no prompt and no approval path. A prompt
//!    would invite a person to talk themselves into the thing the configuration
//!    already says never happens.
//! 2. **the elevation boundary** — refused because this build cannot hold an
//!    administrator token. D-010. A yes does not reach it either.
//! 3. **`require_approval`** — the operator is asked, unless the operator already
//!    authorised this exact step for this exact run: see [`crate::grant`], and note
//!    that a run grant is the *operator's* authority, extended by resolved scope,
//!    never by anything the model or a step's output said.
//! 4. **Everything else** — allowed, on the authority of the command the operator
//!    just typed. See [`crate::basis`].
//!
//! A denied operation is still audited. The audit trail is interesting precisely
//! when nothing ran.

pub mod audit;
pub mod basis;
pub mod decision;
pub mod grant;
pub mod operation;
pub mod prompt;

pub use audit::{AuditEvent, AuditWriter};
pub use basis::Authority;
pub use decision::{Decision, DenyReason};
pub use grant::{GrantScope, RunAuthority, RunGrant};
pub use operation::{Operation, OperationKind, Scope};
pub use prompt::{Answer, ConsolePrompt, PolicyError, Prompter, ScriptedPrompt};

use jean_protocol::config::PolicyConfig;

/// The rules, as configured.
///
/// Built once per CLI invocation from `policy.*`. It holds no state: the ledger of
/// remembered approvals lives in the database, and the prompt is whatever the
/// caller supplies, so a `Policy` is safe to share across threads and easy to
/// exercise in a test.
#[derive(Debug, Clone)]
pub struct Policy {
    config: PolicyConfig,
}

impl Policy {
    pub fn new(config: PolicyConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &PolicyConfig {
        &self.config
    }

    /// Whether `capability` is refused outright.
    pub fn is_auto_denied(&self, capability: &str) -> bool {
        self.config.auto_deny.iter().any(|c| c == capability)
    }

    /// Whether `capability` needs the operator's yes for this invocation.
    pub fn requires_approval(&self, capability: &str) -> bool {
        self.config.require_approval.iter().any(|c| c == capability)
    }

    /// Decide one operation.
    ///
    /// `store` is the ledger: consulted for a remembered approval and written to
    /// when the operator chooses to remember one. `prompter` is how a person is
    /// asked, which is a parameter rather than a direct stdin read so that the
    /// engine is testable and so a future tray UI can answer the same question.
    ///
    /// A capability in both lists is denied, not approved: `auto_deny` is consulted
    /// first, and a config that contradicts itself must fail closed.
    pub fn decide(
        &self,
        op: &Operation,
        store: &jean_store::Store,
        prompter: &mut dyn Prompter,
    ) -> Result<Decision, PolicyError> {
        self.decide_granted(op, store, prompter, None)
    }

    /// Decide one operation, with an optional run grant in scope.
    ///
    /// Where the grant is consulted is the whole design. It is asked about **after** `auto_deny`
    /// and **after** the elevation boundary, so no authority a person handed to a run can reach a
    /// thing the configuration refuses or a token this process does not have. It is asked about
    /// **before** the remembered-approval ledger and **before** the prompt, because it is the more
    /// specific and the fresher of the operator's two standing answers: it says "a person asked for
    /// this run and this step was in the plan they were shown", resolved to the byte.
    ///
    /// A grant that does not match — different plan, different step, different resolved digest — is
    /// not a weaker yes, it is no answer at all, and the step is asked about like any other.
    pub fn decide_granted(
        &self,
        op: &Operation,
        store: &jean_store::Store,
        prompter: &mut dyn Prompter,
        scope: Option<GrantScope<'_>>,
    ) -> Result<Decision, PolicyError> {
        // 1. Nothing that is denied may be approved, so this is checked before any
        //    lookup or prompt — and it produces no prompt at all.
        if let Some(capability) = op.capabilities.iter().find(|c| self.is_auto_denied(c)) {
            return Ok(Decision::Denied(DenyReason::AutoDenied {
                capability: capability.clone(),
            }));
        }

        // 2. Elevation. D-010: the orchestrator runs at the user's integrity level
        //    and this build ships no `jean-elevated.exe` sidecar, so there is no
        //    path from a yes to an administrator token. Refusing here, rather than
        //    attempting and failing, is what lets the operator see the real blocker.
        if op.tier == jean_protocol::tool::PolicyTier::Elevated {
            return Ok(Decision::Denied(DenyReason::ElevationRequired {
                helper: self.config.elevation_helper.clone(),
            }));
        }

        // 3. The capabilities that need asking. A tool with no declared
        //    capabilities has nothing in `require_approval` to match, and lands on
        //    the owner-command authority below.
        let sensitive: Vec<String> = op
            .capabilities
            .iter()
            .filter(|c| self.requires_approval(c))
            .cloned()
            .collect();

        if sensitive.is_empty() {
            return Ok(Decision::allowed(Authority::OwnerCommand));
        }

        // 4. The operator's own run, for exactly this step of exactly this plan at exactly this
        //    resolved scope. Consulted before the ledger and before the prompt; unable to reach
        //    anything the two checks above refused.
        if let Some(scope) = scope {
            if scope.grant.covers(scope.plan_id, scope.step_key, op) {
                return Ok(Decision::allowed(Authority::RunScope));
            }
        }

        // 5. Already agreed to — for exactly this operation with exactly these
        //    arguments. The digest is the whole lookup; nothing else matches.
        let digest = op.scope_digest();
        if let Some(found) = store.find_approval(&digest)? {
            store.touch_approval(found.id)?;
            return Ok(Decision::allowed(Authority::Remembered {
                id: found.id,
                granted_at_ms: found.granted_at_ms,
            }));
        }

        let answer = {
            // Built inside this block: `Prompt` borrows `op` and `sensitive`, and
            // `digest` is moved into the ledger below.
            let prompt = prompt::Prompt {
                summary: &op.summary,
                operation: op.kind.as_str(),
                tool_id: op.tool_id.as_deref(),
                tier: op.tier.as_str(),
                capabilities: &sensitive,
                allow_remember: self.config.allow_remember,
            };
            prompter.ask(&prompt)?
        };

        match answer {
            Answer::Denied(reason) => Ok(Decision::Denied(reason)),
            Answer::ApproveOnce => Ok(Decision::allowed(Authority::ApprovedThisTime)),
            Answer::ApproveAndRemember => {
                if !self.config.allow_remember {
                    // An answer that was not offered is a prompter bug, not an
                    // operator's wish. Refuse rather than silently widening it.
                    return Err(PolicyError::RememberNotAllowed);
                }
                // Saturating arithmetic below, so an absurd TTL lands on the end
                // of the clock rather than wrapping into the past, where the row
                // would be born expired and never usable.
                let ttl_secs =
                    i64::try_from(self.config.approval_remember_secs).unwrap_or(i64::MAX / 2);
                let approval = jean_store::NewApproval {
                    scope_digest: digest,
                    operation: op.kind.as_str().to_owned(),
                    tool_id: op.tool_id.clone().unwrap_or_default(),
                    tier: op.tier.as_str().to_owned(),
                    summary: op.summary.clone(),
                    expires_at_ms: jean_store::now_ms()
                        .saturating_add(ttl_secs.saturating_mul(1_000)),
                };
                let id = store.remember_approval(&approval)?;
                Ok(Decision::allowed(Authority::Remembered {
                    id,
                    granted_at_ms: jean_store::now_ms(),
                }))
            }
        }
    }
}
