//! The J.E.A.N. orchestration core.
//!
//! Phase 0's scope was: load typed configuration, resolve a registered tool, launch it under Job Object
//! supervision, record what happened in SQLite, verify the result against what the tool declared, and let
//! a second process cancel it. Every phase since has added a *kind* of act — an application, a model's
//! plan, an external worker, an MCP peer, a browser, a window — and none of them has replaced that
//! sequence. The record, the decision and the verification are still the ones Phase 0 built.
//!
//! ## What is deliberately absent
//!
//! No tray icon, no physical speech backend, no elevation, and no mechanism that decides for itself. [`desktop`] can drive
//! an application and [`browser`] can drive a page, and neither of them can reach a policy rule, an
//! approval, or the operator's prompt: the crates below this one speak protocols and Win32 and cannot see
//! this one, which is what makes that true rather than merely intended. That direction is D-011.
//!
//! ## Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`context`] | One-pass bootstrap: configuration, paths, store, registry |
//! | [`paths`] | Where state lives, and how empty settings are resolved |
//! | [`logging`] | Rolling file logs, with retention actually enforced |
//! | [`memory`] | Phase 5: history search that can only read, and the facts an operator chose to keep |
//! | [`error`] | Errors that say what to do next |
//! | [`env`] | The environment allowlist, applied |
//! | [`resolve`] | `command` → absolute executable path, without a shell |
//! | [`ipc`] | The per-task cancellation pipe and its DACL |
//! | [`intent`] | Phase 1's four intents: launch, exec, ps, find |
//! | [`runner`] | The whole sequence, in order |
//! | [`planner`] | Phase 2: ask a provider for a plan, and validate it |
//! | [`plan_runner`] | Phase 2: run the validated steps, with one grant per run |
//! | [`delegation`] | Phase 3: an external worker does the work; J.E.A.N. decides and checks |
//! | [`mcp`] | Phase 3: MCP both ways — a server J.E.A.N. starts, and J.E.A.N. on its own stdio |
//! | [`browser`] | Phase 3: a page J.E.A.N. starts, drives, reads, and judges |
//! | [`desktop`] | Phase 4: a window, its controls, its pixels, and what the desktop says afterwards |
//! | [`health`] | Is each registered tool still where the registry says? |
//! | [`notify`] | The toast channel, and the one registry value it writes |
//! | [`signals`] | Ctrl-C, Ctrl-Break and window close |
//! | [`reconcile`] | Orphan detection after a crash |
//! | [`verify`] | Proven / not proven / unverifiable |

pub mod browser;
pub mod captures;
pub mod context;
pub mod delegation;
pub mod desktop;
pub mod env;
pub mod error;
pub mod health;
pub mod intent;
pub mod interaction;
pub mod ipc;
pub mod logging;
pub mod mcp;
pub mod memory;
pub mod notify;
pub mod owner;
pub mod paths;
pub mod plan_runner;
pub mod planner;
pub mod reconcile;
pub mod resolve;
pub mod runner;
pub mod signals;
pub mod speech;
pub mod task_worker;
pub mod verify;

pub use context::Context;
pub use delegation::{
    delegate, delegate_with, AcpRunner, DelegateRequest, DelegationReport, DelegationResult,
    WorkerRunner,
};
pub use error::{CoreError, Result};
pub use health::{probe, probe_all, HealthOutcome, ProbeAnswer};
pub use intent::{
    CommandRequest, FilesRequest, Intent, IntentReport, LaunchEvidence, LaunchRequest, Payload,
    ProcessRequest,
};
pub use mcp::{
    bounds as mcp_bounds, call_tool, exposed as exposed_tools, list_tools, serve as serve_mcp,
    servers as mcp_servers, CallReport, ExposedView, ServeReport, ToolListing, ToolView,
};
pub use notify::{is_registered, register as register_notifications, Delivered};
pub use paths::Paths;
pub use plan_runner::{
    plan_lines, preview as preview_plan, render_preview, run as run_plan,
    wording as summary_wording, CancelFlag, PlanReport, StepOutcome,
};
pub use planner::{Planned, Validation, MAX_PLANNING_ATTEMPTS};
pub use reconcile::{reconcile, Reconciliation};
pub use runner::{run, RunReport, RunRequest};
pub use verify::{Verdict, VERIFY_READ_BYTES};

// Re-exported so the CLI does not need direct dependencies on the lower crates
// just to name the types that come back.
pub use jean_process::{ChildOutcome, Termination};
// The provider types the CLI has to name to ask for a plan. Re-exported rather than added to the
// CLI's dependency list, so `apps/jean-cli` keeps talking to this crate alone.
pub use jean_model::{
    build as build_model, ChatReply, ChatRequest, ModelClient, ModelError, ProviderInfo, Retryable,
    Secret,
};
pub use jean_protocol::TaskState;
pub use jean_store::{Store, TaskRecord, ToolRunRecord};

/// The version of this crate, for `jean --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The configuration schema version this build supports.
///
/// Surfaced here so the CLI can report it without reaching into `jean-protocol`.
pub const CONFIG_VERSION: u32 = jean_protocol::SUPPORTED_CONFIG_VERSION;
