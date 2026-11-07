//! The audit trail: one JSON object per line, written after redaction.
//!
//! ## What "append-only" means here, precisely
//!
//! The roadmap calls this an append-only JSONL log, and the word has two possible
//! meanings. This implementation has only the first one:
//!
//! * **Application behaviour** — the writer opens the file with `append(true)` and
//!   never with `write(true)`/`truncate`, never rewrites a line, and exposes no
//!   read-modify-write path. Nothing inside J.E.A.N. edits history, so a record
//!   this process wrote stays as it was written.
//! * **Tamper resistance** — *not implemented, and not claimed.* The file lives in
//!   the operator's own data directory under the operator's own ACL, so anything
//!   running as that user can rewrite or delete it. Making that provable would need
//!   a per-line hash chain (each record carrying the digest of the previous one) and
//!   a key the attacker does not have, which a local single-user file does not.
//!
//! `config/defaults.toml` and `IMPLEMENTATION_STATUS.md` say the same thing. If a
//! future batch implements the chain, that sentence changes and this one documents
//! the change.

use std::io::Write;
use std::path::{Path, PathBuf};

use jean_protocol::redact::Redactor;
use serde::Serialize;

use crate::decision::DenyReason;
use crate::prompt::PolicyError;

/// One record.
///
/// `Default` exists because a denial that happens before a task is created — an
/// unknown tool id, a config refusal — still has to be written, and the fields it
/// does not have stay absent rather than being filled with a plausible zero.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AuditEvent {
    pub at_ms: i64,
    /// Who ran J.E.A.N.: the account the process is running as. Not a model name
    /// and not `root` — the acting identity is the one that owns the files the
    /// operation touched.
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    /// The argument vector, redacted by the writer.
    pub arguments: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    pub tier: String,
    pub capabilities: Vec<String>,
    pub decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denial_reason: Option<String>,
    /// Which remembered approval covered it, if any. The id is the link back to
    /// `jean approvals`, and the row carries the arguments it was granted for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl AuditEvent {
    /// The record for a decision, before anything ran.
    pub fn for_decision(
        operation: &crate::Operation,
        decision: &crate::Decision,
        actor: String,
    ) -> Self {
        Self {
            at_ms: jean_store::now_ms(),
            actor,
            task_id: None,
            operation: operation.kind.as_str().to_owned(),
            tool_id: operation.tool_id.clone(),
            arguments: operation.scope.arguments.clone(),
            target: Some(operation.scope.target.clone()),
            working_dir: None,
            tier: operation.tier.as_str().to_owned(),
            capabilities: operation.capabilities.clone(),
            decision: decision.as_str().to_owned(),
            authority: decision.authority().map(|a| a.as_str().to_owned()),
            denial_reason: decision.reason().map(DenyReason::kind).map(str::to_owned),
            approval_id: decision.authority().and_then(|a| a.approval_id()),
            outcome: None,
            exit_code: None,
            termination: None,
            duration_ms: None,
            notes: None,
        }
    }

    /// The operator-visible sentence for a denial, or `None` for an allow.
    pub fn denial_text(reason: &DenyReason) -> String {
        reason.to_string()
    }

    pub fn with_task(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    pub fn with_working_dir(mut self, dir: Option<String>) -> Self {
        self.working_dir = dir;
        self
    }

    pub fn with_notes(mut self, notes: Option<String>) -> Self {
        self.notes = notes;
        self
    }

    /// Attach what actually happened. Called once the child is done, or once the
    /// outcome is known.
    pub fn with_outcome(
        mut self,
        outcome: impl Into<String>,
        exit_code: Option<i32>,
        termination: Option<String>,
        duration_ms: Option<u64>,
    ) -> Self {
        self.outcome = Some(outcome.into());
        self.exit_code = exit_code;
        self.termination = termination;
        self.duration_ms = duration_ms;
        self
    }
}

/// Writes records.
///
/// One instance per CLI invocation, cheap to clone-less share: the file is opened
/// and closed per record rather than held open, so a crash between two decisions
/// loses neither the earlier line nor the lock on the database.
pub struct AuditWriter {
    path: PathBuf,
    enabled: bool,
    redactor: Redactor,
}

impl AuditWriter {
    pub fn new(path: PathBuf, enabled: bool, redactor: Redactor) -> Self {
        Self {
            path,
            enabled,
            redactor,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Append one record.
    ///
    /// Redaction happens *here*, on a local copy, before serialization. Putting it
    /// in the writer rather than in every caller is the only way to make "secrets
    /// are never persisted" a property of the code rather than a habit: a new
    /// intent that forgets to redact still cannot write one.
    ///
    /// A disabled writer returns `Ok` and writes nothing. `policy.audit_enabled`
    /// is the operator's own switch, which is different from `disable_audit_logging`
    /// on the auto-deny list: that one is about a *tool* or a *model* turning the
    /// trail off, which stays refused.
    pub fn record(&self, event: &AuditEvent) -> Result<(), PolicyError> {
        if !self.enabled {
            tracing::warn!(
                path = %self.path.display(),
                "audit is disabled by configuration; this record was not written"
            );
            return Ok(());
        }

        let mut redacted = event.clone();
        redacted.arguments = redacted
            .arguments
            .iter()
            .map(|a| self.redactor.redact(a))
            .collect();
        redacted.target = redacted
            .target
            .map(|t| self.redactor.redact(&t))
            .filter(|t| !t.is_empty());
        redacted.working_dir = redacted.working_dir.map(|d| self.redactor.redact(&d));
        redacted.notes = redacted.notes.map(|n| self.redactor.redact(&n));

        // One line. A record that could contain a newline would corrupt the
        // one-object-per-line contract, and an argument can contain one: a quoted
        // PowerShell string with a literal newline in it is legal. serde_json
        // escapes it inside the string, so the line itself stays a line — but
        // `to_string_pretty` must never be used here, and the assertion below is
        // why this writes with the compact form.
        let mut line =
            serde_json::to_string(&redacted).map_err(|e| PolicyError::Audit(e.to_string()))?;
        debug_assert!(
            !line.contains('\n'),
            "an audit record must serialize to a single line"
        );
        line.push('\n');

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| PolicyError::Audit(format!("{}: {e}", parent.display())))?;
            }
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            // Append only. There is no `truncate(true)` anywhere in this file, and
            // adding one would silently unmake the promise in the module header.
            .append(true)
            .open(&self.path)
            .map_err(|e| PolicyError::Audit(format!("{}: {e}", self.path.display())))?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|e| PolicyError::Audit(e.to_string()))?;

        tracing::debug!(
            path = %self.path.display(),
            decision = %event.decision,
            operation = %event.operation,
            "audit record written"
        );
        Ok(())
    }
}
