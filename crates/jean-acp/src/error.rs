//! Why a worker session could not run at all.
//!
//! Deliberately narrow. A worker that ran and produced a broken protocol stream, a refused task, or a
//! wrong answer is **not** an error here — those are [`crate::contract::WorkerOutcome`] fields,
//! because the delegation still has to be recorded and the operator still has to see what happened.
//! This enum is for the cases where there is no session to report on: the process never started, the
//! connector was asked for a protocol it does not speak, or the spawn configuration was rejected
//! before a child existed.

use thiserror::Error;

/// Everything this crate returns through `Result`.
pub type Result<T> = std::result::Result<T, WorkerError>;

#[derive(Debug, Error)]
pub enum WorkerError {
    /// The worker's `kind` is not one this build can speak.
    ///
    /// `jean-protocol` validates the closed set at config load, so reaching this means a connector
    /// was named that the crate does not implement — reported rather than guessed at.
    #[error("worker {worker} has kind {kind}, which this build has no connector for")]
    Unsupported { worker: String, kind: String },

    /// The child could not be created or contained.
    #[error(transparent)]
    Process(#[from] jean_process::ProcessError),

    /// A capture file could not be opened, so the session would run unrecorded.
    #[error("could not open {path} for the worker transcript: {reason}")]
    Output { path: String, reason: String },

    /// The async runtime this crate's SDK needs could not be started.
    #[error("could not start the protocol runtime: {reason}")]
    Runtime { reason: String },
}
