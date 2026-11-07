//! Errors the orchestration core reports to the CLI.
//!
//! The rule for every variant: the message must tell the operator what to do
//! next, or at least what to look at. "failed" is not an error message.

use std::path::PathBuf;

use thiserror::Error;

use jean_protocol::{ConfigError, ProtocolError, ToolValidationError};
use jean_store::StoreError;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),

    #[error("tool registry error: {0}")]
    ToolRegistry(#[from] ToolValidationError),

    #[error("task store error: {0}")]
    Store(#[from] StoreError),

    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("could not create {path}: {reason}")]
    Directory { path: PathBuf, reason: String },

    #[error("could not read or write {path}: {reason}")]
    File { path: PathBuf, reason: String },

    #[error("{reason}")]
    Environment { reason: String },

    /// The named tool is not in the registry, or is disabled.
    ///
    /// Carries the known ids because "did you mean" is the difference between a
    /// five-second fix and a hunt through the configuration.
    #[error("unknown tool `{tool_id}`{disabled}. Known tools: {known}")]
    UnknownTool {
        tool_id: String,
        disabled: &'static str,
        known: String,
    },

    /// The tool's executable could not be found on this machine.
    #[error("tool `{tool_id}` wants `{command}`, which could not be resolved: {reason}")]
    UnresolvedExecutable {
        tool_id: String,
        command: String,
        reason: String,
    },

    #[error("task `{task_id}` was not found")]
    NoSuchTask { task_id: String },

    /// A cancel arrived for a task that is not running.
    ///
    /// Reported separately from [`CoreError::NoSuchTask`] because the operator's
    /// next step differs: the task exists, it simply has no live runner, and
    /// treating that as a crash would be wrong.
    #[error("task `{task_id}` is in state `{state}` and has no runner to cancel")]
    NoRunner { task_id: String, state: String },

    /// The cancellation channel could not be created or reached.
    #[error("cancellation channel for task `{task_id}` failed: {reason}")]
    Ipc { task_id: String, reason: String },

    #[error("process supervision failed: {0}")]
    Process(#[from] jean_process::ProcessError),

    /// The tool ran and reported an exit code, but the declared verification did
    /// not pass. This is a distinct failure from "the command failed": the
    /// process was happy and J.E.A.N. is not.
    #[error("verification failed for task `{task_id}`: {reason}")]
    Verification { task_id: String, reason: String },

    /// Logging could not be initialised.
    ///
    /// Reported rather than ignored: J.E.A.N. without logs cannot be diagnosed,
    /// and silently continuing would make every later failure harder to explain.
    #[error("logging could not be initialised: {reason}")]
    Logging { reason: String },

    /// A read-only probe refused: the registry, the process table, or a directory.
    ///
    /// Kept distinct from [`CoreError::Process`] because nothing was supervised. A
    /// discovery failure and a child failure have opposite fixes — one is a
    /// configuration or permission question, the other is about the command.
    #[error("local probe failed: {0}")]
    Tool(#[from] jean_tools::ToolError),

    /// The policy engine could not reach a decision.
    ///
    /// Distinct from a decision of *no*: `Denied` is a normal, reported outcome, while
    /// this means the question itself could not be answered (the ledger was unreadable,
    /// or the prompt could not be written). Failing here rather than defaulting to
    /// allowed is the whole point of having an engine.
    #[error("the permission engine could not decide: {0}")]
    Policy(#[from] jean_policy::PolicyError),

    /// An intent was refused before anything ran, for a reason that belongs to no
    /// other category: an ambiguous application name, a search root outside the
    /// configured bounds, a pid that is not this user's to look at.
    #[error("{reason}")]
    Intent { reason: String },

    /// The provider could not produce an answer to plan from.
    ///
    /// Carried through rather than reinterpreted: `ModelError` already distinguishes a transport
    /// failure that may be asked for again from a response that arrived and could not be read, and
    /// flattening both into one core error would lose exactly the difference the CLI has to report.
    #[error("the model provider failed: {0}")]
    Model(#[from] jean_model::ModelError),

    /// A plan was refused, with every reason the validators found.
    ///
    /// A distinct variant because the operator's next step is different from any other failure: the
    /// goal, the catalog or the configuration is what needs changing, and the diagnostics say which.
    #[error("the plan was refused:\n{diagnostics}")]
    PlanRejected { diagnostics: String },

    /// An MCP session did not produce an answer.
    ///
    /// Carried as a sentence rather than as `jean_mcp::McpError` because the two kinds of failure —
    /// a peer that spoke nonsense, and a peer that said nothing in time — have already been turned
    /// into the operator's words by the crate that saw them, and a task record can only hold a string.
    #[error("the MCP session failed: {0}")]
    Mcp(String),

    /// A memory or preference command was refused before it touched the database.
    ///
    /// Its own variant because the next step is always the operator's own words: the
    /// sentence they meant to save, or the key and value they meant to set. Nothing
    /// ran, nothing was stored, and the reason names what to type instead.
    #[error("{reason}")]
    Memory { reason: String },
}

pub type Result<T> = std::result::Result<T, CoreError>;

impl CoreError {
    pub fn unknown_tool(tool_id: &str, known: Vec<&str>, disabled: bool) -> Self {
        CoreError::UnknownTool {
            tool_id: tool_id.to_owned(),
            disabled: if disabled {
                " (it is registered but disabled)"
            } else {
                ""
            },
            known: if known.is_empty() {
                "none are configured; see config/tools.example.toml".to_owned()
            } else {
                known.join(", ")
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_tool_lists_the_alternatives() {
        let e = CoreError::unknown_tool("gitt", vec!["git", "powershell.echo"], false);
        let text = e.to_string();
        assert!(text.contains("gitt"), "{text}");
        assert!(text.contains("git, powershell.echo"), "{text}");
        assert!(!text.contains("disabled"), "{text}");
    }

    #[test]
    fn a_disabled_tool_says_it_is_disabled() {
        let e = CoreError::unknown_tool("risky", vec!["git"], true);
        assert!(e.to_string().contains("registered but disabled"), "{e}");
    }

    #[test]
    fn an_empty_registry_points_at_the_example_file() {
        let e = CoreError::unknown_tool("anything", vec![], false);
        assert!(e.to_string().contains("tools.example.toml"), "{e}");
    }

    #[test]
    fn a_task_with_no_runner_is_not_reported_as_missing() {
        let missing = CoreError::NoSuchTask {
            task_id: "t-1".to_owned(),
        }
        .to_string();
        let idle = CoreError::NoRunner {
            task_id: "t-1".to_owned(),
            state: "succeeded".to_owned(),
        }
        .to_string();
        assert!(missing.contains("not found"), "{missing}");
        assert!(idle.contains("no runner"), "{idle}");
        assert!(idle.contains("succeeded"), "{idle}");
        assert_ne!(missing, idle);
    }

    #[test]
    fn a_verification_failure_is_distinct_from_a_command_failure() {
        let v = CoreError::Verification {
            task_id: "t-1".to_owned(),
            reason: "the expected output predicate did not match".to_owned(),
        };
        assert!(v.to_string().contains("verification"), "{v}");
        assert!(v.to_string().contains("t-1"), "{v}");
    }

    #[test]
    fn underlying_errors_convert_with_the_from_trait() {
        let store = StoreError::NoSuchTask("t-9".to_owned());
        let core: CoreError = store.into();
        assert!(core.to_string().contains("t-9"), "{core}");

        let process = jean_process::ProcessError::NoSuchExecutable {
            path: PathBuf::from("C:\\gone.exe"),
        };
        let core: CoreError = process.into();
        assert!(core.to_string().contains("gone.exe"), "{core}");
    }
}
