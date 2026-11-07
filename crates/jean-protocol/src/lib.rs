//! Typed contracts shared by every J.E.A.N. component.
//!
//! This crate contains no I/O beyond reading configuration text, no Windows
//! FFI, and no process management. It exists so that the state machine, the
//! configuration schema and the redaction rules can be tested in isolation.

pub mod config;
pub mod error;
pub mod event;
pub mod exec;
pub mod interaction;
pub mod ipc;
pub mod plan;
pub mod preferences;
pub mod redact;
pub mod state;
pub mod tool;

pub use config::{AppConfig, ConfigError, ConfigSource, SUPPORTED_CONFIG_VERSION};
pub use error::{ProtocolError, Result};
pub use event::{Event, EventKind};
pub use exec::{StreamSummary, Termination};
pub use ipc::{ClientRequest, ServerResponse};
pub use plan::{
    ArgValue, DeclaredVerification, Plan, PlanArgument, PlanOperation, PlanStep, PlanStepState,
    PlanStructureError, StructureReport, MAX_PLAN_STEPS, PLAN_SCHEMA_VERSION,
};
pub use preferences::{PreferenceKey, SUPPORTED_KEYS as SUPPORTED_PREFERENCE_KEYS};
pub use redact::Redactor;
pub use state::{TaskState, TransitionError};
pub use tool::{PolicyTier, ToolDef, ToolKind, ToolRegistry, ToolValidationError};

/// Identifier for a task. Opaque, stable, safe to embed in a pipe name.
pub type TaskId = String;
