//! # jean-acp — external coding-agent supervision
//!
//! One contract, several protocol shapes. A delegation is handed to a worker — an ACP agent such as
//! `opencode acp`, or a structured CLI such as `codex exec --json` — and this crate's job is to make
//! the two agree on what J.E.A.N. sees: progress events, permission requests, a settlement, and a
//! process tree that dies when the operator says so.
//!
//! ## What this crate owns, and what it refuses to
//!
//! * It owns the **protocol**. It does not own the **decision**: every permission request goes to a
//!   [`contract::PermissionHost`] that `jean-core` implements over `jean-policy`, so the run-scoped
//!   authority of Phase 2 is the same authority that answers an agent. There is no second,
//!   uncontrolled execution path here.
//! * It owns the **bounds**. Frame size, event queue, session and idle deadlines, shutdown grace,
//!   output bytes — all from `[delegation]`, all enforced in this crate, because neither SDK enforces
//!   them and an agent that streams unbounded JSON is a denial of service against the operator.
//! * It does **not** decide whether a delegation should happen, what workspace it uses, or what
//!   command proves it worked. Those are resolved before a worker starts and are data the worker
//!   cannot rewrite.
//!
//! ## Async, deliberately
//!
//! The official ACP Rust SDK's connection entry point takes an `AsyncFnOnce`; there is no synchronous
//! form. Rather than hand-write a protocol with a conformance suite to avoid a runtime, the runtime is
//! adopted for this crate and `jean-mcp` only, on a current-thread executor that lives for one
//! session inside [`acp::run`] and is gone when it returns. Phases 0-2 keep their synchronous surface.
//!
//! ## Live-agent status
//!
//! The connectors are verified against deterministic fakes and against a scripted ACP peer in
//! `tests`. Whether `opencode acp` and `codex exec --json` accept these sessions is recorded as
//! **unverified** in `IMPLEMENTATION_STATUS.md`: a mock does not prove compatibility with a tool that
//! was not run.

pub mod acp;
pub mod cli;
pub mod contract;
pub mod error;
pub mod fake;
pub mod frames;
pub mod watchdog;

pub use contract::{
    Bounds, DelegateRequest, EventSink, PermissionAnswer, PermissionHost, PermissionOffer,
    PermissionRequest, RunSpec, VecSink, WorkerAction, WorkerEvent, WorkerOutcome,
};
pub use error::{Result, WorkerError};

/// Run one worker, in whatever protocol its configured `kind` speaks.
///
/// The kinds are the closed set `jean-protocol` validates (`WORKER_KINDS`), so a configuration that
/// names anything else has already failed at load; reaching [`WorkerError::Unsupported`] means a kind
/// was added to the config without a connector, and it is reported rather than guessed at.
pub fn run(
    spec: &RunSpec<'_>,
    host: &mut dyn PermissionHost,
    sink: &mut dyn EventSink,
) -> Result<WorkerOutcome> {
    match spec.worker.kind.as_str() {
        "acp" => acp::run(spec, host, sink),
        "codex-cli" => cli::run(spec, sink),
        other => Err(WorkerError::Unsupported {
            worker: spec.worker.name.clone(),
            kind: other.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_dispatch_covers_every_kind_the_config_accepts() {
        // The match arms above and `WORKER_KINDS` are the same list, kept in step by a test rather
        // than by a constant pattern: a kind added to the config without a connector must fail here,
        // not silently at runtime.
        for kind in jean_protocol::config::WORKER_KINDS {
            let known = matches!(*kind, "acp" | "codex-cli");
            assert!(known, "{kind} has no connector in jean-acp::run");
        }
        assert_eq!(
            jean_protocol::config::WORKER_KINDS,
            &["acp", "codex-cli"][..]
        );
    }
}
