//! The failures this crate can report, and what each one means for the operator.

use std::path::PathBuf;

/// Why an MCP exchange did not produce an answer.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The child could not be started, or its pipes could not be made.
    #[error(transparent)]
    Process(#[from] jean_process::ProcessError),

    /// The protocol layer reported a failure: a handshake refusal, a malformed frame, a peer that
    /// vanished mid-request. Carried as text because `rmcp`'s own error type is a moving target and
    /// the operator needs the sentence, not the enum.
    #[error("the MCP server {server} failed: {reason}")]
    Protocol { server: String, reason: String },

    /// A request outlived `[mcp] request_timeout_secs`. The server is still running; this call is not
    /// going to answer, and waiting longer is not a different outcome.
    #[error("the MCP server {server} did not answer {request} within {timeout:?}")]
    Timeout {
        server: String,
        request: String,
        timeout: std::time::Duration,
    },

    /// The server named a tool this build was not told to use. An allowlist that is enforced by
    /// refusing, rather than by not mentioning.
    #[error("{tool} is not in the allowed tools of MCP server {server}")]
    NotAllowed { server: String, tool: String },

    /// The tokio runtime this crate needs for `rmcp` could not be built on this thread.
    #[error("the MCP runtime could not be started: {reason}")]
    Runtime { reason: String },

    /// A capture path could not be written.
    #[error("could not write {path}: {reason}")]
    Output { path: PathBuf, reason: String },
}

pub type Result<T> = std::result::Result<T, McpError>;
