//! Tool health: whether a registered tool is still where the registry says it is.
//!
//! ## The probe is a lookup, never a run
//!
//! A health check that *starts* the thing it is checking is not a check, it is a side
//! effect with a diagnostic label. `jean health` runs against every tool in the registry
//! on request, so a probe that launched an application would mean that asking "is this
//! installed?" opened windows, wrote the application's first-run files, and possibly
//! registered a single-instance lock — every one of which is a change to the machine the
//! operator did not ask for. Phase 1 therefore answers presence by resolving the tool's
//! `command` the same way a run would (`PATH`, `PATHEXT`, absolute path, no script
//! extensions) and stat'ing the result. That is the whole probe.
//!
//! What this does **not** claim: that a resolvable executable is a *working* one. A
//! `0-byte` binary, a DLL that cannot find its imports, a tool that needs a license file
//! and quits — all of them read as healthy here. `PLANNING/05-ROADMAP.md` calls for
//! "tool health checks", and this is the honest subset of that: *present*, not *correct*.
//! A command probe is a later decision, and when it comes it has to arrive with a policy
//! rule attached, because running arbitrary registered commands on a timer is exactly the
//! thing the approval flow exists to gate.
//!
//! ## Freshness, so the answer is not re-derived for nothing
//!
//! `tools.health_check_interval_secs` bounds how often one tool is looked at: an answer
//! newer than the interval is read back from the store rather than re-probed. There is no
//! daemon. `PLANNING/04-SETTINGS.md` describes an interval, and the cheapest way to
//! honour it without a background process is to treat the interval as a *cache lifetime*
//! checked at the moment somebody asks. `--force` (the CLI's `jean health --now`) ignores
//! it, because an operator who just installed the thing is entitled to a fresh answer.
//!
//! ## Who is unhealthy
//!
//! `tools.mark_unhealthy_after` is applied by the store on every recorded probe, so the
//! threshold in force is the one the row was written under. A tool that has never been
//! probed reads `unknown` — not healthy, not unhealthy. A tool with `enabled = false` is
//! recorded `disabled` and never probed at all: the absence is a fact about configuration,
//! and it would be dishonest to let a probe overwrite it.

use std::path::PathBuf;
use std::time::Duration;

use jean_protocol::tool::ToolDef;
use jean_store::{HealthStatus, ToolHealthRecord};

use crate::context::Context;
use crate::error::Result;
use crate::resolve::resolve_executable;

/// How one tool's health question was answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeAnswer {
    /// The executable resolved and exists. Carries the path that was found, so a
    /// report can show *which* copy won — the same name can appear in several `PATH`
    /// directories, and "healthy" without a path is not an actionable answer.
    Present(PathBuf),
    /// The executable could not be resolved. The string is the resolver's reason.
    Absent(String),
    /// `enabled = false`: nothing was looked at, and the reason says so.
    Disabled(String),
    /// The recorded answer was newer than the configured interval, so nothing was
    /// looked at this time.
    Cached(ToolHealthRecord),
}

impl ProbeAnswer {
    /// Whether this answer means the probe passed.
    ///
    /// `Disabled` is not a pass and not a failure: it is the absence of a question.
    /// Mapping it to `true` would let a registry of nothing-but-disabled-tools report
    /// all-healthy, which is a sentence with no content.
    pub fn passed(&self) -> Option<bool> {
        match self {
            ProbeAnswer::Present(_) => Some(true),
            ProbeAnswer::Absent(_) => Some(false),
            ProbeAnswer::Disabled(_) | ProbeAnswer::Cached(_) => None,
        }
    }

    pub fn is_cached(&self) -> bool {
        matches!(self, ProbeAnswer::Cached(_))
    }
}

/// One tool's health, as reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthOutcome {
    pub tool_id: String,
    pub status: HealthStatus,
    pub answer: ProbeAnswer,
    /// Consecutive failures behind `status`, straight from the row.
    pub consecutive_failures: u32,
    /// The already-redacted detail stored with the row, if any.
    pub detail: Option<String>,
}

impl HealthOutcome {
    /// Whether the tool should be considered runnable right now.
    ///
    /// `unknown` counts as runnable: a machine that has never been asked has no
    /// evidence either way, and refusing to run a tool because nobody probed it would
    /// make the probe a precondition rather than a diagnostic.
    pub fn runnable(&self) -> bool {
        self.status != HealthStatus::Unhealthy && self.status != HealthStatus::Disabled
    }
}

/// Probe every registered tool, honouring freshness unless forced.
///
/// Walks ids and re-reads each definition rather than the registry's `enabled`
/// iterator, because a disabled tool is part of the answer: `jean health` has to show it
/// as `disabled` rather than omit it, and an operator comparing the list against their
/// `tools.toml` would otherwise conclude the file was not read.
pub fn probe_all(ctx: &Context, force: bool) -> Result<Vec<HealthOutcome>> {
    let ids: Vec<String> = ctx.tools.ids().map(str::to_owned).collect();
    let mut outcomes = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(tool) = ctx.tools.get(&id) {
            outcomes.push(probe(ctx, tool, force)?);
        }
    }
    Ok(outcomes)
}

/// Probe one tool and record the answer.
pub fn probe(ctx: &Context, tool: &ToolDef, force: bool) -> Result<HealthOutcome> {
    let recorded = ctx.store.tool_health(&tool.id)?;

    // A disabled tool is recorded once and then left alone. Probing it would mean
    // resolving a command the operator has explicitly switched off, and the row must
    // keep saying `disabled` rather than `unhealthy`, which are different sentences.
    if !tool.enabled {
        let reason = format!("`enabled = false` in the registry for `{}`", tool.id);
        ctx.store.set_disabled(&tool.id, &reason)?;
        return Ok(HealthOutcome {
            tool_id: tool.id.clone(),
            status: HealthStatus::Disabled,
            answer: ProbeAnswer::Disabled(reason),
            consecutive_failures: recorded
                .as_ref()
                .map(|r| r.consecutive_failures)
                .unwrap_or(0),
            detail: recorded.and_then(|r| r.detail),
        });
    }

    let interval = Duration::from_secs(ctx.config.tools.health_check_interval_secs);
    if !force {
        if let Some(record) = recorded.filter(|r| r.is_fresh(interval, jean_store::now_ms())) {
            return Ok(HealthOutcome {
                tool_id: tool.id.clone(),
                status: record.status,
                answer: ProbeAnswer::Cached(record.clone()),
                consecutive_failures: record.consecutive_failures,
                detail: record.detail,
            });
        }
    }

    let answer = resolve_tool(tool);
    let passed = answer.passed().unwrap_or(false);
    let detail = match &answer {
        ProbeAnswer::Present(path) => Some(format!("resolved to {}", path.display())),
        ProbeAnswer::Absent(reason) => Some(format!(
            "could not be resolved: {reason}; the probe is a path lookup, so a tool that \
             exists but fails to start still reads healthy"
        )),
        _ => None,
    };
    // The detail column is printed by `jean health` and lives in the same database as
    // everything else the operator types, so it goes through the redactor like every
    // other persisted string.
    let detail = detail.map(|d| ctx.redactor.redact(&d));

    let record = ctx.store.record_probe(
        &tool.id,
        passed,
        ctx.config.tools.mark_unhealthy_after,
        detail.as_deref(),
    )?;

    Ok(HealthOutcome {
        tool_id: tool.id.clone(),
        status: record.status,
        answer,
        consecutive_failures: record.consecutive_failures,
        detail: record.detail,
    })
}

/// Resolve a tool's command against the environment a child of this tool would see.
///
/// Deliberately the *child's* `PATH`, not the supervisor's: `tool.env` can replace it,
/// and a probe that resolved against a different `PATH` than the run uses would report
/// healthy for a tool that then fails to launch, which is the exact failure mode a health
/// check is supposed to prevent.
fn resolve_tool(tool: &ToolDef) -> ProbeAnswer {
    let parent_path = std::env::var("PATH").ok();
    let parent_pathext = std::env::var("PATHEXT").ok();
    let path = tool.env.get("PATH").cloned().or(parent_path);
    let pathext = tool.env.get("PATHEXT").cloned().or(parent_pathext);

    match resolve_executable(&tool.command, path.as_deref(), pathext.as_deref()) {
        Ok(found) => ProbeAnswer::Present(found),
        Err(first) => match &tool.fallback_command {
            // The fallback is part of the tool's own definition of how to start, so a
            // tool that resolves only through it is not unhealthy.
            Some(fallback) => {
                match resolve_executable(fallback, path.as_deref(), pathext.as_deref()) {
                    Ok(found) => ProbeAnswer::Present(found),
                    Err(second) => ProbeAnswer::Absent(format!(
                        "{command}: {first}; fallback {fallback}: {second}",
                        command = tool.command
                    )),
                }
            }
            None => ProbeAnswer::Absent(first.reason),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_answer_is_neither_a_pass_nor_a_failure() {
        let disabled = ProbeAnswer::Disabled("enabled = false".to_owned());
        assert_eq!(disabled.passed(), None);
        assert!(!disabled.is_cached());
        let cached = ProbeAnswer::Cached(ToolHealthRecord {
            tool_id: "x".to_owned(),
            status: HealthStatus::Unknown,
            consecutive_failures: 0,
            last_check_ms: None,
            last_ok_ms: None,
            detail: None,
        });
        assert!(cached.is_cached());
        assert_eq!(cached.passed(), None);
    }

    #[test]
    fn unknown_and_healthy_tools_are_runnable_but_unhealthy_ones_are_not() {
        for status in [HealthStatus::Unknown, HealthStatus::Healthy] {
            let outcome = outcome_for(status);
            assert!(
                outcome.runnable(),
                "{} must still be runnable",
                status.as_str()
            );
        }
        for status in [HealthStatus::Unhealthy, HealthStatus::Disabled] {
            assert!(
                !outcome_for(status).runnable(),
                "{} must not be offered as runnable",
                status.as_str()
            );
        }
    }

    fn outcome_for(status: HealthStatus) -> HealthOutcome {
        HealthOutcome {
            tool_id: "demo".to_owned(),
            status,
            answer: ProbeAnswer::Absent("test fixture".to_owned()),
            consecutive_failures: 0,
            detail: None,
        }
    }
}
