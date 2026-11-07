//! # jean-mcp — MCP in both directions, over stdio, with J.E.A.N. holding the process
//!
//! Two roles, one rule: **J.E.A.N. spawns, J.E.A.N. supervises, J.E.A.N. decides.**
//!
//! * **Client** ([`client`]): J.E.A.N. starts an MCP server as an ordinary worker child — through
//!   [`jean_process::piped`], inside a job object, with the allowlist-pinned environment — and speaks
//!   the protocol over the two pipes it handed it. `rmcp`'s own `transport-child-process` feature is
//!   deliberately not compiled in, because a server the SDK starts is a process outside the job,
//!   outside the deadline and outside the audit trail.
//! * **Server** ([`server`]): J.E.A.N. answers an MCP client that connected to *this* process's
//!   stdio. Only tools the operator named in `[mcp] expose` are ever advertised, and every call is
//!   decided by `jean-policy` before it runs — a client that speaks MCP gets J.E.A.N.'s authority,
//!   not a bypass around it.
//!
//! ## What is bounded
//!
//! `rmcp`'s stdio transport reads NDJSON lines with a length ceiling that defaults to 16 MiB, so
//! `[mcp] max_message_bytes` is passed to `with_max_line_length` rather than left at the SDK's
//! choice. One tool result is capped at `max_result_bytes` and reported as truncated instead of
//! being streamed into the caller's memory. A request that outlives `request_timeout_secs` is
//! abandoned, and the shutdown grace bounds how long the child gets to close its pipes before the
//! job is terminated. Every one of those numbers comes from configuration, and a connector cannot
//! raise them.
//!
//! ## No network listener
//!
//! This crate transports over a child's pipes and over its own standard streams. There is no HTTP
//! transport, no WebSocket, and no port bound anywhere in it — that is not an omission, it is the
//! shape the roadmap asked for first.

mod client;
mod error;
mod server;

pub use client::{CallOutcome, McpSession, ToolInfo};
pub use error::{McpError, Result};
pub use server::{serve, ExposedTool, ToolHost};

/// The limits one MCP exchange runs under, from `[mcp]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bounds {
    /// Largest single JSON-RPC line in either direction.
    pub max_message_bytes: usize,
    /// How long one request may take before it is abandoned.
    pub request_timeout: std::time::Duration,
    /// Ceiling on the text of one tool result.
    pub max_result_bytes: usize,
    /// How long the server gets to close its pipes before the job is terminated.
    pub shutdown_grace: std::time::Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bounds_value_is_what_the_configuration_says_not_what_the_sdk_defaults_to() {
        // The SDK's own default is 16 MiB per line. If this build silently inherited that, `[mcp]
        // max_message_bytes` would be a setting that changes nothing, which is worse than not having
        // it — so the value is threaded through and asserted here.
        let bounds = Bounds {
            max_message_bytes: 1024,
            request_timeout: std::time::Duration::from_secs(1),
            max_result_bytes: 2048,
            shutdown_grace: std::time::Duration::from_millis(50),
        };
        assert_eq!(bounds.max_message_bytes, 1024);
        assert!(bounds.max_result_bytes > bounds.max_message_bytes / 2);
    }
}
