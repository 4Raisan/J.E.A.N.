//! What the engine decides about, and the identity of that thing.

use jean_protocol::tool::PolicyTier;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// The kind of work, from the roadmap's four deterministic intents plus the
/// plumbing the CLI needs around them.
///
/// These are names for *what is about to happen*, not for tools: `git push` and
/// `notepad.exe` are both `RunCommand`/`LaunchApp`, and what earns them a prompt
/// is the capability they declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    LaunchApp,
    RunCommand,
    InspectProcess,
    LocateFile,
    /// A bounded probe of whether a configured tool is present. Never starts the
    /// tool itself.
    ProbeHealth,
    /// Recording that the operator chose to remember an approval.
    RememberApproval,
    ForgetAllApprovals,
    ForgetOneApproval,
    /// Registering the notification AppUserModelId in `HKCU`.
    RegisterNotifications,
    DeliverNotification,
    /// An **external worker** asked for something that is not a command: a file it wants written or
    /// read, a network target, or a request this build could not classify.
    ///
    /// Its own kind rather than a reuse of `RunCommand`, because the digest is what a remembered
    /// approval is bound to: an approval for "run `cargo test`" must not silently cover "write
    /// `Cargo.toml`", and two kinds with one name would make them indistinguishable in the audit trail.
    DelegatedAction,
    /// Starting an MCP server and asking it for a list or a tool result.
    ///
    /// Separate from `DelegatedAction` for the same reason the digest exists at all: a person who
    /// agreed to let a worker edit a file did not agree to let a peer process answer tool calls, and
    /// the two have to be rememberable — and auditable — apart.
    McpCall,
    /// Driving a browser **J.E.A.N. itself started**, through one workflow: open a page, read it, act
    /// on it, and check what the page says afterwards.
    ///
    /// Its own kind because a page is a different kind of danger from a command: the target of the
    /// digest is the origin that will be loaded, so an approval given for a local fixture must not be
    /// the reason a signed-in session gets driven later.
    DriveBrowser,
    /// Acting on a window that already belongs to the operator's desktop.
    ///
    /// Its own kind, and its own digest target, for the reason `DriveBrowser` has one: the thing being
    /// agreed to is *this application*, addressed by process image and window, and an approval that
    /// covered a fixture window must never be the reason a chat client gets clicked. Reading the desktop
    /// is a separate kind again — `InspectProcess` and `LocateFile` are read-only probes and a UIA tree
    /// read is not the same category of nothing as a button press.
    OperateDesktop,
}

impl OperationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LaunchApp => "launch_app",
            Self::RunCommand => "run_command",
            Self::InspectProcess => "inspect_process",
            Self::LocateFile => "locate_file",
            Self::ProbeHealth => "probe_health",
            Self::RememberApproval => "remember_approval",
            Self::ForgetAllApprovals => "forget_all_approvals",
            Self::ForgetOneApproval => "forget_one_approval",
            Self::RegisterNotifications => "register_notifications",
            Self::DeliverNotification => "deliver_notification",
            Self::DelegatedAction => "delegated_action",
            Self::McpCall => "mcp_call",
            Self::DriveBrowser => "drive_browser",
            Self::OperateDesktop => "operate_desktop",
        }
    }
}

/// One operation, as the engine sees it.
///
/// The caller builds this *after* resolving and validating what the operator
/// typed: `scope.arguments` are the argument vector that would actually be passed,
/// not the raw command line, and `scope.target` is the resolved executable, search
/// root or pid. That is what makes a remembered approval mean something — a digest
/// over unresolved text would either be unmatchable or would match too much.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub kind: OperationKind,
    pub tool_id: Option<String>,
    pub tier: PolicyTier,
    /// Declared by the tool or the intent, matched against `require_approval` and
    /// `auto_deny` by name.
    pub capabilities: Vec<String>,
    /// What the operator is shown if a prompt is needed. Redacted before it gets
    /// here: the summary ends up in the approvals ledger and in the audit trail.
    pub summary: String,
    pub scope: Scope,
}

/// The part of an operation a remembered approval is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scope {
    /// The resolved thing the operation acts on: an executable path, a search
    /// root, a pid, a notification title.
    pub target: String,
    /// The validated argument vector, in the order it would be passed.
    pub arguments: Vec<String>,
}

impl Scope {
    pub fn new(target: impl Into<String>, arguments: Vec<String>) -> Self {
        Self {
            target: target.into(),
            arguments,
        }
    }

    /// A scope with no arguments, for the intents that act on a single resolved
    /// target.
    pub fn only(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            arguments: Vec::new(),
        }
    }
}

impl Operation {
    /// Build an operation for an intent that declares no capabilities.
    pub fn simple(
        kind: OperationKind,
        tool_id: Option<String>,
        tier: PolicyTier,
        summary: String,
        scope: Scope,
    ) -> Self {
        Self {
            kind,
            tool_id,
            tier,
            capabilities: Vec::new(),
            summary,
            scope,
        }
    }

    /// The identity of a remembered approval.
    ///
    /// Two different digests never share a grant, and the same digest always does.
    ///
    /// The canonical form is **length-prefixed**, not separator-joined, and that is
    /// the load-bearing detail. A separator character can appear inside an argument,
    /// and then two different scopes serialize identically: with `\u{1f}` as the
    /// separator, the argument vector `["a\u{1f}b"]` produces exactly the bytes of
    /// `["a", "b"]`. An approval for the one would cover the other. Each field here
    /// is written as `len#value`, so the field boundaries are facts and not
    /// guesses, and the argument *count* is written first for the same reason.
    ///
    /// Two recorded limits:
    ///
    /// * `DefaultHasher` is SipHash with keys the standard library does not promise
    ///   to keep across releases. If they ever change, every remembered approval
    ///   stops matching and the operator is asked again — the direction that fails
    ///   closed, and why this is documented rather than replaced with a hash crate.
    /// * A 64-bit digest collides by chance roughly once in 2^64 pairs. For a local
    ///   ledger of human approvals that is acceptable, and the consequence of the
    ///   impossible case is one operation covered by an approval meant for another.
    ///   A collision is not a way in for an outsider either: the digest is not a
    ///   secret, and granting requires writing to the ledger, which is the same
    ///   access as granting anything.
    pub fn scope_digest(&self) -> String {
        let mut canonical = String::with_capacity(
            self.kind.as_str().len()
                + self.scope.target.len()
                + self
                    .scope
                    .arguments
                    .iter()
                    .map(|a| a.len() + 4)
                    .sum::<usize>()
                + 32,
        );
        write_field(
            self.scope.arguments.len().to_string().as_str(),
            &mut canonical,
        );
        write_field(self.kind.as_str(), &mut canonical);
        write_field(self.tool_id.as_deref().unwrap_or(""), &mut canonical);
        write_field(&self.scope.target, &mut canonical);
        for argument in &self.scope.arguments {
            write_field(argument, &mut canonical);
        }

        let mut hasher = DefaultHasher::new();
        canonical.hash(&mut hasher);
        // Lowercase hex, fixed width, so the value survives being pasted into a
        // sqlite3 prompt unchanged.
        format!("{:016x}", hasher.finish())
    }
}

/// One length-prefixed field: `len#value`, where `len` is the byte count.
///
/// Byte count, not `chars().count()`, because the value that follows is exactly
/// that many bytes wide and the reader of this string never has to decide where a
/// multi-byte character ends.
fn write_field(value: &str, out: &mut String) {
    use std::fmt::Write as _;
    let _ = write!(out, "{}#", value.len());
    out.push_str(value);
}
