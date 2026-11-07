//! # MCP — J.E.A.N. as the client, and J.E.A.N. as the server, over pipes it owns
//!
//! Both roles exist because the roadmap asks for both, and neither one gets a private path to the
//! machine:
//!
//! * **Client.** `jean mcp tools|call` starts a server JEAN was told about in `[[mcp.servers]]`,
//!   speaks to it over the pipes `jean-process` made, and stops it. The executable comes from
//!   configuration, is resolved through the same `resolve` path a typed command uses, and the call
//!   is decided by [`jean_policy::Policy`] before a byte reaches the peer.
//! * **Server.** `jean mcp serve` turns *this* process's stdio into an MCP server whose tool list is
//!   `[mcp] expose` intersected with the tool registry. An inbound call is executed by
//!   [`crate::intent::execute`] — the same function `jean run` goes through — so the rules, the
//!   approvals ledger, the audit line, the task row, the supervision and the declared verification
//!   are all the ones Phase 1 and 2 already ship. There is no second executor.
//!
//! ## Where the bounds are applied
//!
//! `[mcp] max_message_bytes`, `request_timeout_secs`, `max_result_bytes` and `shutdown_grace_ms`
//! become a [`jean_mcp::Bounds`] and are handed to the SDK, so the limits the operator configured
//! are the limits on the wire rather than comments in a file. `jean-mcp` never spawns a process and
//! never opens a listener: it is given pipes.
//!
//! ## What `serve` cannot do, and says so
//!
//! A server driven over stdio has no terminal of its own — stdin and stdout *are* the transport. So
//! a call that needs a person is refused, with the refusal naming what would make it allowed
//! (`jean approvals`, or the same command run in a terminal). A question nobody can answer is not
//! permission, and J.E.A.N. does not treat silence as a yes. The question is still written, to
//! stderr, which is diagnostic space the protocol never uses.
//!
//! ## Two roles in one process?
//!
//! No. Nothing here connects J.E.A.N. to itself: a client command talks to a configured peer and a
//! serve command answers one. That separation is what keeps "J.E.A.N. called a tool" and "someone
//! asked J.E.A.N. for a tool" two different audit sentences.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use jean_mcp::{Bounds, CallOutcome, ExposedTool, McpSession, ToolHost};
use jean_policy::{
    AuditEvent, AuditWriter, Decision, DenyReason, Operation, OperationKind, Policy, Prompter,
    Scope,
};
use jean_process::{LaunchSpec, TerminalMode, Termination};
use jean_protocol::config::{McpConfig, McpServerConfig};
use jean_protocol::event::{Event, EventKind};
use jean_protocol::tool::PolicyTier;
use jean_protocol::{StreamSummary, TaskState, ToolDef};
use jean_store::{Store, TaskRecord};

use crate::context::Context;
use crate::env::build_child_env;
use crate::error::{CoreError, Result};
use crate::intent::{self, CommandRequest, Intent};
use crate::verify::Verdict;

/// The limits from `[mcp]`, as the transport understands them.
pub fn bounds(config: &McpConfig) -> Bounds {
    Bounds {
        max_message_bytes: config.max_message_bytes,
        request_timeout: Duration::from_secs(config.request_timeout_secs.max(1)),
        max_result_bytes: config.max_result_bytes,
        shutdown_grace: Duration::from_millis(u64::from(config.shutdown_grace_ms.max(1))),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Reading the configuration
// ─────────────────────────────────────────────────────────────────────────────

/// Why MCP is not available, if it is not.
fn require_enabled(config: &McpConfig) -> Result<()> {
    if config.enabled {
        Ok(())
    } else {
        Err(CoreError::Intent {
            reason:
                "`[mcp] enabled = false`, so J.E.A.N. will not start or act as a server; set it \
                     in the configuration when you want one"
                    .to_owned(),
        })
    }
}

fn find_server<'a>(config: &'a McpConfig, name: &str) -> Result<&'a McpServerConfig> {
    config
        .servers
        .iter()
        .find(|server| server.name == name)
        .ok_or_else(|| CoreError::Intent {
            reason: format!(
                "no `[[mcp.servers]]` entry is named {name:?}; the names this build knows are {}",
                if config.servers.is_empty() {
                    "none".to_owned()
                } else {
                    config
                        .servers
                        .iter()
                        .map(|s| format!("`{}`", s.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
        })
}

/// Whether this server is willing to have this tool called.
///
/// The allowlist is checked before the process starts: a refusal that costs a spawn is a refusal
/// that also gave the peer a session.
fn check_allowlist(server: &McpServerConfig, tool: &str) -> Result<()> {
    if server.allow_tools.iter().any(|t| t == tool) {
        return Ok(());
    }
    Err(CoreError::Intent {
        reason: if server.allow_tools.is_empty() {
            format!(
                "server {:?} declares no `allow_tools`, which means J.E.A.N. calls nothing on it; \
                 add `{tool}` to the list to allow this one",
                server.name
            )
        } else {
            format!(
                "{tool:?} is not in server {:?}'s `allow_tools` ({})",
                server.name,
                server
                    .allow_tools
                    .iter()
                    .map(|t| format!("`{t}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    })
}

/// What one configured server is, as far as this machine can tell without starting it.
#[derive(Debug, Clone)]
pub struct ServerView {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub enabled: bool,
    pub capability: String,
    /// `None` means the executable was not found. Nothing was started to discover that.
    pub resolved: Option<PathBuf>,
    pub allowed_tools: Vec<String>,
    pub env_names: Vec<String>,
    pub notes: Vec<String>,
}

/// Every `[[mcp.servers]]` entry, resolved but never launched.
pub fn servers(ctx: &Context) -> Vec<ServerView> {
    let config = &ctx.config.mcp;
    config
        .servers
        .iter()
        .map(|server| {
            let mut notes = Vec::new();
            if !config.enabled {
                notes.push("MCP is disabled by `[mcp] enabled`".to_owned());
            }
            if !server.enabled {
                notes.push("this server is disabled".to_owned());
            }
            if server.allow_tools.is_empty() {
                notes.push(
                    "`allow_tools` is empty, which means J.E.A.N. calls nothing on this server"
                        .to_owned(),
                );
            }
            let resolved = crate::resolve::resolve_executable(
                &server.command,
                std::env::var("PATH").ok().as_deref(),
                std::env::var("PATHEXT").ok().as_deref(),
            )
            .ok();
            if resolved.is_none() {
                notes.push(format!(
                    "`{}` was not found on PATH, so this server could not be started",
                    server.command
                ));
            }
            ServerView {
                name: server.name.clone(),
                command: server.command.clone(),
                args: server.args.clone(),
                enabled: server.enabled && config.enabled,
                capability: server.capability.clone(),
                resolved,
                allowed_tools: server.allow_tools.clone(),
                env_names: server.env_passthrough.clone(),
                notes,
            }
        })
        .collect()
}

/// One tool J.E.A.N. would publish as a server, and whether the registry can back it.
#[derive(Debug, Clone)]
pub struct ExposedView {
    pub id: String,
    pub in_registry: bool,
    pub enabled: bool,
    pub capability: String,
    pub tier: String,
}

/// `[mcp] expose`, checked against the registry.
pub fn exposed(ctx: &Context) -> Vec<ExposedView> {
    ctx.config
        .mcp
        .expose
        .iter()
        .map(|id| match ctx.tools.get(id) {
            Some(tool) => ExposedView {
                id: id.clone(),
                in_registry: true,
                enabled: tool.enabled,
                capability: tool.capabilities.join(", "),
                tier: tool.tier.as_str().to_owned(),
            },
            None => ExposedView {
                id: id.clone(),
                in_registry: false,
                enabled: false,
                capability: String::new(),
                tier: String::new(),
            },
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// The client role
// ─────────────────────────────────────────────────────────────────────────────

/// One MCP peer, whether it is a process J.E.A.N. started or a scripted answer in a test.
///
/// This is the same seam [`crate::delegation`] puts in front of a worker: everything around it — the
/// policy decision, the task row, the run row, the events, the audit line — is the production code,
/// and only the bytes on the other end of the pipe are substituted. A test that re-implemented the
/// accounting would prove nothing about the accounting.
pub trait Peer {
    fn list(&mut self) -> std::result::Result<Vec<jean_mcp::ToolInfo>, jean_mcp::McpError>;
    fn call(
        &mut self,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> std::result::Result<CallOutcome, jean_mcp::McpError>;
    /// The pid of the process behind the session, for the run row.
    fn pid(&self) -> u32;
    /// End the session and report the child's exit code, if it had one.
    fn close(&mut self) -> Option<i32>;
}

impl Peer for McpSession {
    fn list(&mut self) -> std::result::Result<Vec<jean_mcp::ToolInfo>, jean_mcp::McpError> {
        McpSession::list_tools(self)
    }

    fn call(
        &mut self,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> std::result::Result<CallOutcome, jean_mcp::McpError> {
        McpSession::call(self, tool, arguments.clone())
    }

    fn pid(&self) -> u32 {
        McpSession::pid(self)
    }

    fn close(&mut self) -> Option<i32> {
        McpSession::close(self)
    }
}

/// How a session gets started: the production opener, or a scripted one.
pub type Opener = dyn Fn(&str, &LaunchSpec, &Bounds) -> Result<Box<dyn Peer>>;

/// The real opener — start the process, complete the handshake.
pub fn open_session(server: &str, launch: &LaunchSpec, bounds: &Bounds) -> Result<Box<dyn Peer>> {
    McpSession::open(server, launch, bounds)
        .map(|session| Box::new(session) as Box<dyn Peer>)
        .map_err(|e| CoreError::Mcp(e.to_string()))
}

/// The `LaunchSpec` for one configured server.
///
/// Built here and not in `jean-mcp`, because a protocol library does not get to choose what binary
/// runs on this machine. The working directory and the environment follow the same rules the registry
/// tools follow: the workspace root when one is configured, the caller's directory otherwise, and
/// only allowlisted variables plus this server's `env_passthrough`.
fn server_launch(ctx: &Context, server: &McpServerConfig) -> Result<LaunchSpec> {
    let executable = crate::resolve::resolve_executable(
        &server.command,
        std::env::var("PATH").ok().as_deref(),
        std::env::var("PATHEXT").ok().as_deref(),
    )
    .map_err(|e| CoreError::UnresolvedExecutable {
        tool_id: format!("mcp.server:{}", server.name),
        command: server.command.clone(),
        reason: e.reason,
    })?;

    let mut allowlist = ctx.config.process.env_allowlist.clone();
    allowlist.extend(server.env_passthrough.iter().cloned());
    let parent: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let (env, _) = build_child_env(&allowlist, &Default::default(), &parent);

    let working_dir = intent::workspace_root(ctx)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    Ok(LaunchSpec {
        executable,
        args: server.args.clone(),
        working_dir: Some(working_dir),
        env,
        // This is the *lifetime* of the child, not the deadline for one request: a session is a
        // handshake, a list or a call, and a close. The per-request bound lives in the runtime.
        timeout: Some(
            Duration::from_secs(ctx.config.mcp.request_timeout_secs.max(1).saturating_mul(3))
                .saturating_add(Duration::from_secs(30)),
        ),
        output_cap_bytes: ctx.output_cap(),
        stdout_path: None,
        stderr_path: None,
        // The protocol is stdout. Diagnostics stay in the pipe `jean-process` keeps, and are shown
        // only in `jean mcp`' own notes — never appended to a result.
        terminal: TerminalMode::Pipes,
    })
}

/// One tool as this server publishes it, and whether J.E.A.N. is allowed to call it.
#[derive(Debug, Clone)]
pub struct ToolView {
    pub name: String,
    pub description: String,
    pub allowed: bool,
}

/// What `jean mcp tools` found, and how it was decided.
#[derive(Debug, Clone)]
pub struct ToolListing {
    pub task_id: String,
    pub server: String,
    pub tools: Vec<ToolView>,
    pub decision: Decision,
    pub state: TaskState,
    /// The refusal, when there was one — in the operator's words, never a stack frame.
    pub notes: Vec<String>,
}

/// Ask one server what it offers.
///
/// Starting a server is an action, so it is decided like one: the same capability a call uses, the
/// same rules, the same audit line. `jean mcp status` answers the same question from configuration
/// alone and starts nothing.
pub fn list_tools(
    ctx: &mut Context,
    server_name: &str,
    prompter: &mut dyn Prompter,
) -> Result<ToolListing> {
    list_tools_with(ctx, server_name, prompter, &open_session)
}

/// [`list_tools`], with the peer behind a seam, so a test can script the other end of the pipe without
/// reimplementing the accounting around it.
pub fn list_tools_with(
    ctx: &mut Context,
    server_name: &str,
    prompter: &mut dyn Prompter,
    opener: &Opener,
) -> Result<ToolListing> {
    ctx.prepare_for_writing()?;
    require_enabled(&ctx.config.mcp)?;
    let server = find_server(&ctx.config.mcp, server_name)?.clone();
    if !server.enabled {
        return Err(CoreError::Intent {
            reason: format!("server {:?} is disabled", server.name),
        });
    }
    let launch = server_launch(ctx, &server)?;
    let outcome = decided_session(
        ctx,
        &server,
        REQUEST_LIST,
        &serde_json::json!({}),
        &launch,
        prompter,
        opener,
    )?;
    match outcome {
        SessionOutcome::Listed {
            task_id,
            tools,
            decision,
        } => Ok(ToolListing {
            task_id,
            server: server.name,
            tools,
            decision,
            state: TaskState::Succeeded,
            notes: Vec::new(),
        }),
        SessionOutcome::Denied(report) => Ok(ToolListing {
            task_id: report.task_id,
            server: report.server,
            tools: Vec::new(),
            decision: report.decision,
            state: report.state,
            notes: vec![report.text],
        }),
        SessionOutcome::Called(_) => Err(CoreError::Intent {
            reason: "tools/list cannot return a call result".to_owned(),
        }),
    }
}

/// What one `tools/call` returned, as observed.
#[derive(Debug, Clone)]
pub struct CallReport {
    pub task_id: String,
    pub server: String,
    pub tool: String,
    pub decision: Decision,
    pub state: TaskState,
    /// The result text, capped at `[mcp] max_result_bytes`.
    pub text: String,
    /// Bytes the server actually returned, before the cap.
    pub bytes: u64,
    pub truncated: bool,
    /// The server's own tool-level failure: an answer, not a transport fault.
    pub tool_error: bool,
    pub exit_code: Option<i32>,
    pub notes: Vec<String>,
}

impl CallReport {
    /// A process exit status for the CLI: 0 only when the call came back clean.
    pub fn exit_status(&self) -> i32 {
        match self.state {
            TaskState::Succeeded if !self.tool_error => 0,
            TaskState::Succeeded | TaskState::Failed => 1,
            TaskState::Cancelled => 130,
            _ => 4,
        }
    }
}

/// The result of `tools/list` or `tools/call`, once the decision is known.
enum SessionOutcome {
    /// The engine said no, and the reason is already recorded.
    Denied(CallReport),
    Listed {
        task_id: String,
        tools: Vec<ToolView>,
        decision: Decision,
    },
    Called(CallReport),
}

/// The one request that is not a tool name.
const REQUEST_LIST: &str = "tools/list";

/// Call one tool on one server.
pub fn call_tool(
    ctx: &mut Context,
    server_name: &str,
    tool: &str,
    arguments: serde_json::Value,
    prompter: &mut dyn Prompter,
) -> Result<CallReport> {
    call_tool_with(ctx, server_name, tool, arguments, prompter, &open_session)
}

/// [`call_tool`], with the peer behind a seam — see [`list_tools_with`] for why the seam exists.
pub fn call_tool_with(
    ctx: &mut Context,
    server_name: &str,
    tool: &str,
    arguments: serde_json::Value,
    prompter: &mut dyn Prompter,
    opener: &Opener,
) -> Result<CallReport> {
    ctx.prepare_for_writing()?;
    require_enabled(&ctx.config.mcp)?;
    let server = find_server(&ctx.config.mcp, server_name)?.clone();
    if !server.enabled {
        return Err(CoreError::Intent {
            reason: format!("server {:?} is disabled", server.name),
        });
    }
    // Refused before the executable is even resolved: the allowlist is the operator's boundary, and
    // crossing it is not something a prompt can authorise.
    check_allowlist(&server, tool)?;
    if !arguments.is_object() {
        return Err(CoreError::Intent {
            reason: "the arguments of a tool call must be a JSON object; J.E.A.N. does not invent \
                     the shape a peer expects"
                .to_owned(),
        });
    }
    let launch = server_launch(ctx, &server)?;
    match decided_session(ctx, &server, tool, &arguments, &launch, prompter, opener)? {
        SessionOutcome::Called(report) | SessionOutcome::Denied(report) => Ok(report),
        SessionOutcome::Listed { .. } => Err(CoreError::Intent {
            reason: "unreachable: a call was asked for".to_owned(),
        }),
    }
}

/// Decide, and only on an allow, open a session and run the request.
///
/// The session is created *after* the decision, so a refusal never starts a process, and the task row
/// exists before it, so the refusal is a record and not a whisper.
fn decided_session(
    ctx: &mut Context,
    server: &McpServerConfig,
    request: &str,
    arguments: &serde_json::Value,
    launch: &LaunchSpec,
    prompter: &mut dyn Prompter,
    opener: &Opener,
) -> Result<SessionOutcome> {
    let operation = call_operation(ctx, server, request, arguments, launch);
    let policy = Policy::new(ctx.config.policy.clone());
    let audit = AuditWriter::new(
        ctx.paths.audit.clone(),
        ctx.config.policy.audit_enabled,
        ctx.redactor.clone(),
    );
    let task_id = open_task(ctx, server, &operation, request)?;
    let actor = crate::intent::actor();

    if would_prompt(&policy, &ctx.store, &operation)? {
        ctx.store.transition(
            &task_id,
            TaskState::AwaitingApproval,
            Some(
                "a capability on the list \
             needs a yes",
            ),
        )?;
    }
    let decision = policy
        .decide(&operation, &ctx.store, prompter)
        .map_err(CoreError::Policy)?;
    audit
        .record(
            &AuditEvent::for_decision(&operation, &decision, actor.clone())
                .with_task(&task_id)
                .with_working_dir(current_directory()),
        )
        .map_err(CoreError::Policy)?;

    if let Decision::Denied(reason) = decision.clone() {
        let text = AuditEvent::denial_text(&reason);
        ctx.store
            .transition(&task_id, TaskState::Failed, Some(&text))?;
        ctx.store.set_outcome(&task_id, "denied", &text)?;
        ctx.store
            .append_event(&task_id, &Event::note(text.clone()))?;
        let _ = ctx.store.clear_runner(&task_id);
        return Ok(SessionOutcome::Denied(CallReport {
            task_id,
            server: server.name.clone(),
            tool: request.to_owned(),
            decision,
            state: TaskState::Failed,
            text,
            bytes: 0,
            truncated: false,
            tool_error: false,
            exit_code: None,
            notes: vec![
                "nothing was started: the process that would have served the protocol was created \
                 only after the decision allowed it"
                    .to_owned(),
            ],
        }));
    }

    let recording = Recording {
        operation: &operation,
        decision: &decision,
        audit: &audit,
        actor: &actor,
    };
    run_session(
        ctx, server, request, launch, &task_id, &recording, arguments, opener,
    )
    .inspect_err(|e| {
        close_as_failed(ctx, &task_id, &e.to_string());
    })
}

/// The decision, the thing it was about, and where the record of it goes.
///
/// Grouped because a session needs all four at three different points, and because — as in
/// [`crate::intent`] — bundling them is what makes "a decision is never recorded without its audit
/// line" a property of one type instead of a convention repeated at each call site.
struct Recording<'a> {
    operation: &'a Operation,
    decision: &'a Decision,
    audit: &'a AuditWriter,
    actor: &'a str,
}

/// Open the session, do the work, close the session, and write down every step.
#[allow(clippy::too_many_arguments)]
fn run_session(
    ctx: &mut Context,
    server: &McpServerConfig,
    request: &str,
    launch: &LaunchSpec,
    task_id: &str,
    recording: &Recording<'_>,
    arguments: &serde_json::Value,
    opener: &Opener,
) -> Result<SessionOutcome> {
    let bounds = bounds(&ctx.config.mcp);
    ctx.store.transition(
        task_id,
        TaskState::Running,
        Some("the decision allowed the act"),
    )?;
    crate::runner::record_ownership(&ctx.store, task_id)?;
    let run_id = ctx.store.insert_tool_run(
        task_id,
        &format!("mcp:{}", server.name),
        &launch.executable.display().to_string(),
        &crate::runner::to_json_array(&launch.args),
        launch
            .working_dir
            .as_deref()
            .map(|d| d.display().to_string())
            .as_deref(),
        &crate::runner::to_json_map(&launch.env),
    )?;

    let started = Instant::now();
    // The row exists before the child does, and the pid is written the moment it can be: a server
    // that dies mid-handshake still has a record that it was started.
    let mut session = match opener(&server.name, launch, &bounds) {
        Ok(session) => {
            let _ = ctx.store.record_child_pid(run_id, session.pid());
            session
        }
        Err(e) => {
            return Err(fail_session(
                ctx,
                task_id,
                run_id,
                None,
                Termination::LaunchFailed,
                &e.to_string(),
            ))
        }
    };

    let requested = if request == REQUEST_LIST {
        session.list().map(Requested::List)
    } else {
        session.call(request, arguments).map(Requested::Call)
    };

    // Whether the work succeeded or not, the session ends here: `close` shuts the protocol down and
    // takes the job object with it, so a server cannot outlive the command that started it.
    let exit_code = session.close();
    let termination = if exit_code.is_some() {
        Termination::Exited
    } else {
        // It was still running when the grace period expired, so J.E.A.N. ended it. `Timeout` is the
        // honest label — the child did not exit, it was made to.
        Termination::Timeout
    };
    drop(session);

    let requested = match requested {
        Ok(value) => value,
        Err(e) => {
            return Err(fail_session(
                ctx,
                task_id,
                run_id,
                exit_code,
                termination,
                &e.to_string(),
            ))
        }
    };

    let summary = match &requested {
        Requested::List(tools) => StreamSummary {
            path: None,
            bytes: tools.len() as u64,
            truncated: false,
        },
        Requested::Call(call) => StreamSummary {
            path: None,
            bytes: call.bytes,
            truncated: call.truncated,
        },
    };
    ctx.store.finish_tool_run(
        run_id,
        exit_code,
        termination,
        &summary,
        &Default::default(),
        None,
    )?;

    settle(
        ctx,
        server,
        request,
        task_id,
        recording,
        requested,
        exit_code,
        termination,
        started.elapsed(),
    )
}

/// What the session was asked for, as the peer answered it.
enum Requested {
    List(Vec<jean_mcp::ToolInfo>),
    Call(CallOutcome),
}

/// Record what came back, and close the task out.
///
/// The child is already gone by the time this runs, so the exit code and the termination are the
/// supervisor's facts rather than an assumption.
#[allow(clippy::too_many_arguments)]
fn settle(
    ctx: &mut Context,
    server: &McpServerConfig,
    request: &str,
    task_id: &str,
    recording: &Recording<'_>,
    requested: Requested,
    exit_code: Option<i32>,
    termination: Termination,
    elapsed: Duration,
) -> Result<SessionOutcome> {
    let (verdict, outcome) = match &requested {
        // A list is proven by the peer having published it: there is no check to run against a
        // `tools/list`, and pretending there were would be a verdict about nothing.
        Requested::List(tools) => (
            Verdict::Proven(format!(
                "server {} published {} tool(s) over stdio",
                server.name,
                tools.len()
            )),
            SessionOutcome::Listed {
                task_id: task_id.to_owned(),
                tools: describe_tools(server, tools.clone()),
                decision: recording.decision.clone(),
            },
        ),
        Requested::Call(call) => {
            // The distinction is the whole reason this module keeps `is_error` separate from `Err`:
            // "the server refused to do it" is evidence about the tool, "the server did not answer"
            // is a fault in the session.
            let verdict = if call.is_error {
                Verdict::NotProven(format!(
                    "{}/{} reported a tool error ({} bytes returned)",
                    server.name, request, call.bytes
                ))
            } else {
                Verdict::Proven(format!(
                    "{}/{} returned {} bytes{}",
                    server.name,
                    request,
                    call.bytes,
                    if call.truncated {
                        ", capped at the configured result limit"
                    } else {
                        ""
                    }
                ))
            };
            (
                verdict,
                SessionOutcome::Called(CallReport {
                    task_id: task_id.to_owned(),
                    server: server.name.clone(),
                    tool: request.to_owned(),
                    decision: recording.decision.clone(),
                    // Set below, from the verdict.
                    state: TaskState::Running,
                    text: call.text.clone(),
                    bytes: call.bytes,
                    truncated: call.truncated,
                    tool_error: call.is_error,
                    exit_code,
                    notes: Vec::new(),
                }),
            )
        }
    };

    let state = match &outcome {
        // A server that answered with a tool-level failure did the thing wrong, and the task says
        // so. `Verifying → Failed` is the only legal edge out of here that is not a success.
        SessionOutcome::Called(report) if report.tool_error => TaskState::Failed,
        SessionOutcome::Called(_) | SessionOutcome::Listed { .. } => TaskState::Succeeded,
        SessionOutcome::Denied(_) => TaskState::Failed,
    };
    ctx.store
        .transition(task_id, TaskState::Verifying, Some("the peer answered"))?;
    let reason = format!("{}: {}", verdict.label(), verdict.detail());
    ctx.store.transition(task_id, state, Some(&reason))?;
    ctx.store
        .set_outcome(task_id, verdict.label(), verdict.detail())?;
    ctx.store.clear_runner(task_id)?;

    // Never the arguments: the event says what was called and how big the answer was, because this
    // row is read out of the database long after a credential in an argument should have been.
    ctx.store.append_event(
        task_id,
        &Event::new(
            EventKind::McpToolCalled,
            ctx.redactor.redact(&format!(
                "server `{}` {} `{}` → {} ({})",
                server.name,
                if request == REQUEST_LIST {
                    "listed via"
                } else {
                    "tool"
                },
                request,
                verdict.label(),
                termination,
            )),
        ),
    )?;

    recording
        .audit
        .record(
            &AuditEvent::for_decision(
                recording.operation,
                recording.decision,
                recording.actor.to_owned(),
            )
            .with_task(task_id)
            .with_working_dir(current_directory())
            .with_outcome(
                verdict.label(),
                exit_code,
                Some(termination.to_string()),
                Some(elapsed.as_millis() as u64),
            ),
        )
        .map_err(CoreError::Policy)?;

    match outcome {
        SessionOutcome::Called(mut report) => {
            if report.truncated {
                report.notes.push(format!(
                    "the result was cut at `[mcp] max_result_bytes` = {}; the server sent {} bytes",
                    ctx.config.mcp.max_result_bytes, report.bytes
                ));
            }
            report.state = state;
            Ok(SessionOutcome::Called(report))
        }
        other => Ok(other),
    }
}

/// A session that did not produce an answer: the run row and the task both say why.
fn fail_session(
    ctx: &mut Context,
    task_id: &str,
    run_id: i64,
    exit_code: Option<i32>,
    termination: Termination,
    reason: &str,
) -> CoreError {
    // The run row is closed even when nothing was proven, or the database would say a server is
    // still running that this process already took down.
    if let Err(e) = ctx.store.finish_tool_run(
        run_id,
        exit_code,
        termination,
        &Default::default(),
        &Default::default(),
        Some(reason),
    ) {
        tracing::warn!(error = %e, run_id, "could not close an MCP run row");
    }
    if ctx
        .store
        .transition(task_id, TaskState::Failed, Some(reason))
        .is_ok()
    {
        let _ = ctx.store.set_outcome(task_id, "error", reason);
    }
    let _ = ctx.store.append_event(
        task_id,
        &Event::new(
            EventKind::McpToolCalled,
            ctx.redactor
                .redact(&format!("the MCP session failed: {reason}")),
        ),
    );
    let _ = ctx.store.clear_runner(task_id);
    CoreError::Mcp(reason.to_owned())
}

/// Which tools this server published, marked against its own allowlist.
fn describe_tools(server: &McpServerConfig, tools: Vec<jean_mcp::ToolInfo>) -> Vec<ToolView> {
    tools
        .into_iter()
        .map(|tool| ToolView {
            allowed: server.allow_tools.contains(&tool.name),
            name: tool.name,
            description: tool.description,
        })
        .collect()
}

/// The operation an MCP request becomes.
///
/// The digest covers the server, the tool, and — for a call — the exact arguments. That is the same
/// rule Phase 1 uses for a command: an approval for one argument vector does not cover another, and
/// `allow_always` means "this precisely", never "this shape of thing".
fn call_operation(
    ctx: &Context,
    server: &McpServerConfig,
    request: &str,
    arguments: &serde_json::Value,
    launch: &LaunchSpec,
) -> Operation {
    let canonical = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_owned());
    let keys = arguments
        .as_object()
        .map(|map| map.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    // The summary is shown in a prompt and stored in the approvals ledger, so it names the argument
    // *keys* and not their values: a person approving a call should see what the call touches, and
    // the values stay in the digest, where they are one-way.
    let summary = ctx.redactor.redact(&format!(
        "call MCP tool `{request}` on server `{}` (arguments: {})",
        server.name,
        if keys.is_empty() {
            "none".to_owned()
        } else {
            keys.join(", ")
        }
    ));
    Operation {
        kind: OperationKind::McpCall,
        tool_id: Some(format!("mcp.{}", server.name)),
        tier: PolicyTier::Normal,
        capabilities: vec![server.capability.clone()],
        summary,
        scope: Scope::new(
            launch.executable.display().to_string(),
            vec![server.name.clone(), request.to_owned(), canonical],
        ),
    }
}

fn open_task(
    ctx: &Context,
    server: &McpServerConfig,
    operation: &Operation,
    request: &str,
) -> Result<String> {
    let task_id = jean_protocol::ipc::new_task_id();
    let request_json = serde_json::to_string(&serde_json::json!({
        "server": server.name,
        "request": request,
        "capability": server.capability,
        "tier": operation.tier.as_str(),
    }))
    .map_err(|e| CoreError::File {
        path: PathBuf::from("<request record>"),
        reason: e.to_string(),
    })?;
    ctx.store.insert_task(&TaskRecord::new(
        task_id.clone(),
        operation.summary.clone(),
        Some(format!("mcp:{}", server.name)),
        request_json,
    ))?;
    ctx.store.transition(
        &task_id,
        TaskState::Planning,
        Some("the server was resolved"),
    )?;
    Ok(task_id)
}

/// Whether the engine is about to ask a person.
fn would_prompt(policy: &Policy, store: &Store, operation: &Operation) -> Result<bool> {
    let sensitive = operation
        .capabilities
        .iter()
        .any(|c| policy.requires_approval(c));
    if !sensitive {
        return Ok(false);
    }
    Ok(store
        .find_approval(&operation.scope_digest())
        .map_err(CoreError::Store)?
        .is_none())
}

fn current_directory() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|dir| dir.display().to_string())
}

fn close_as_failed(ctx: &Context, task_id: &str, reason: &str) {
    if ctx
        .store
        .transition(task_id, TaskState::Failed, Some(reason))
        .is_err()
    {
        // The task may already be terminal. Either way the outcome is what a reader needs.
        let _ = ctx.store.set_outcome(task_id, "error", reason);
    } else {
        let _ = ctx.store.set_outcome(task_id, "error", reason);
    }
    let _ = ctx.store.clear_runner(task_id);
}

// ─────────────────────────────────────────────────────────────────────────────
// The server role
// ─────────────────────────────────────────────────────────────────────────────

/// One inbound request, and the channel its answer goes back on.
struct Forwarded {
    tool: String,
    arguments: serde_json::Value,
    reply: mpsc::Sender<std::result::Result<String, String>>,
}

/// The transport thread's only reach into J.E.A.N.: a queue.
///
/// This is the whole reason the bridge exists. `rmcp`'s handler must be `Send + Sync`, and
/// [`crate::Context`] is neither — it owns the single SQLite connection. So the thread that speaks
/// the protocol cannot touch the policy engine, the ledger or a process at all; it hands the request
/// to the thread that owns them and waits. A protocol peer therefore has no code path to the machine
/// except the one the operator's main thread runs, which is the same claim `jean-acp` makes about a
/// worker.
struct Bridge {
    sender: Mutex<mpsc::Sender<Forwarded>>,
}

impl ToolHost for Bridge {
    fn invoke(
        &self,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> std::result::Result<String, String> {
        let (reply, receiver) = mpsc::channel();
        let request = Forwarded {
            tool: tool.to_owned(),
            arguments: arguments.clone(),
            reply,
        };
        {
            // A poisoned lock means the main thread panicked while holding it; the request would then
            // never be answered, so the honest failure is this one.
            let sender = self
                .sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sender
                .send(request)
                .map_err(|_| "J.E.A.N. is no longer taking requests".to_owned())?;
        }
        match receiver.recv() {
            Ok(answer) => answer,
            Err(_) => Err("the request was not answered".to_owned()),
        }
    }
}

/// A prompter with nobody to ask.
///
/// `jean mcp serve` has no terminal: stdin and stdout carry the protocol, and reading them for a
/// word like `y` would corrupt the session it is part of. So a capability that needs a person is
/// refused, the question is written to stderr where the operator can see it, and the answer the
/// client gets names what would have made it allowed. Silence is not consent, and this type is the
/// place that rule is written down.
struct NoTerminalPrompter;

impl Prompter for NoTerminalPrompter {
    fn ask(
        &mut self,
        prompt: &jean_policy::prompt::Prompt<'_>,
    ) -> std::result::Result<jean_policy::Answer, jean_policy::PolicyError> {
        eprintln!("[jean mcp serve] an approval was needed and there is no terminal to ask in:");
        for line in prompt.render().lines() {
            eprintln!("  {line}");
        }
        Ok(jean_policy::Answer::Denied(DenyReason::Unanswerable(
            "J.E.A.N. is serving MCP over its own stdio, so no one was asked. Run the tool from a \
             terminal, or approve this exact call first (`jean approvals`), or remove the \
             capability from `policy.require_approval`."
                .to_owned(),
        )))
    }
}

/// What `jean mcp serve` did, once the client disconnected.
#[derive(Debug, Clone)]
pub struct ServeReport {
    pub exposed: Vec<String>,
    pub requests: usize,
}

/// Become an MCP server on this process's stdio until the client closes it.
///
/// There is no listener and nothing to configure a port for: the only client is the one attached to
/// the pipes this command already owns.
pub fn serve(ctx: &mut Context) -> Result<ServeReport> {
    ctx.prepare_for_writing()?;
    require_enabled(&ctx.config.mcp)?;
    let (tools, ids) = exposed_tools(ctx)?;
    if tools.is_empty() {
        return Err(CoreError::Intent {
            reason: "`[mcp] expose` lists no tools, so there is nothing to publish. A server that \
                     advertises an empty list and refuses every call is not a service, it is a \
                     puzzle."
                .to_owned(),
        });
    }

    let (sender, receiver) = mpsc::channel::<Forwarded>();
    let host = Bridge {
        sender: Mutex::new(sender),
    };
    let bounds = bounds(&ctx.config.mcp);
    // One thread drives the protocol; this one owns the database, the policy engine and the children.
    let transport = std::thread::Builder::new()
        .name("jean-mcp-serve".to_owned())
        .spawn(move || jean_mcp::serve("jean", tools, host, &bounds))
        .map_err(|e| CoreError::Environment {
            reason: format!("the MCP transport thread could not start: {e}"),
        })?;

    let mut prompter = NoTerminalPrompter;
    let mut requests = 0usize;
    while let Ok(request) = receiver.recv() {
        requests += 1;
        let answer = answer_request(ctx, &request, &mut prompter);
        if let Err(text) = &answer {
            // stderr again: the client gets the refusal as a protocol result, the operator gets to
            // see that something was asked for and what stopped it.
            eprintln!("[jean mcp serve] refused `{}`: {text}", request.tool);
        }
        // A closed channel means the client went away mid-request. Nothing to do but stop answering.
        let _ = request.reply.send(answer);
    }

    let served = transport.join().map_err(|_| CoreError::Intent {
        reason: "the MCP transport thread panicked".to_owned(),
    })?;
    served.map_err(|e| CoreError::Mcp(e.to_string()))?;
    Ok(ServeReport {
        exposed: ids,
        requests,
    })
}

/// `[mcp] expose`, turned into wire tools — and refused if any name is not a registry tool.
fn exposed_tools(ctx: &Context) -> Result<(Vec<ExposedTool>, Vec<String>)> {
    let mut tools = Vec::new();
    let mut ids = Vec::new();
    for id in &ctx.config.mcp.expose {
        let tool = ctx.tools.get(id).ok_or_else(|| CoreError::Intent {
            reason: format!(
                "`[mcp] expose` names {id:?}, which is not in the tool registry at {}; J.E.A.N. \
                 will not publish a tool it cannot run",
                ctx.paths.tools.display()
            ),
        })?;
        if !tool.enabled {
            return Err(CoreError::Intent {
                reason: format!("exposed tool {id:?} is disabled in the registry"),
            });
        }
        ids.push(id.clone());
        tools.push(ExposedTool {
            id: id.clone(),
            description: description_for(tool),
            input_schema: arguments_schema(),
        });
    }
    Ok((tools, ids))
}

fn description_for(tool: &ToolDef) -> String {
    let name = if tool.display_name.trim().is_empty() {
        tool.id.as_str()
    } else {
        tool.display_name.trim()
    };
    format!(
        "{name} — J.E.A.N. registry tool `{}`. Runs under J.E.A.N.'s policy, supervision and \
         verification; the answer is what J.E.A.N. observed, not what the tool claimed.",
        tool.id
    )
}

/// The schema every exposed tool advertises.
///
/// One shape for all of them, deliberately: a registry entry declares a fixed argv, not a typed
/// parameter object, and inventing a per-tool schema from nothing would promise a structure J.E.A.N.
/// cannot check. `args` is appended to that fixed argv exactly as `jean run --arg` does.
fn arguments_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "args": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Arguments appended after the tool's configured argv."
            }
        },
        "additionalProperties": false
    })
}

/// One client's request, executed through the pipeline every typed command uses.
fn answer_request(
    ctx: &mut Context,
    request: &Forwarded,
    prompter: &mut dyn Prompter,
) -> std::result::Result<String, String> {
    let args = client_arguments(&request.arguments)?;
    let intent = Intent::Command(CommandRequest {
        tool_id: request.tool.clone(),
        args,
        goal: Some(format!(
            "an MCP client asked J.E.A.N. to run `{}`",
            request.tool
        )),
        interactive: None,
    });
    let report = intent::execute(ctx, &intent, prompter).map_err(|e| e.to_string())?;
    if !report.decision.is_allowed() {
        return Err(match report.decision.reason() {
            Some(reason) => AuditEvent::denial_text(reason),
            None => "the policy engine refused this call".to_owned(),
        });
    }
    let mut answer = format!(
        "{}: {}",
        report.verdict.label(),
        ctx.redactor.redact(report.verdict.detail())
    );
    if let Some(body) = observed_output(ctx, &report.task_id) {
        answer.push_str("\n\n");
        answer.push_str(&body);
    }
    for note in &report.notes {
        answer.push_str(&format!("\nnote: {}", ctx.redactor.redact(note)));
    }
    Ok(answer)
}

/// Validate what a client passed, or refuse it with a sentence the client can act on.
fn client_arguments(arguments: &serde_json::Value) -> std::result::Result<Vec<String>, String> {
    if arguments.is_null() {
        return Ok(Vec::new());
    }
    let map = arguments
        .as_object()
        .ok_or_else(|| "the arguments must be a JSON object".to_owned())?;
    for key in map.keys() {
        if key != "args" {
            return Err(format!(
                "unknown argument key {key:?}; this tool takes only `args`, an array of strings"
            ));
        }
    }
    let Some(value) = map.get("args") else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| "`args` must be an array of strings".to_owned())?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let text = item
            .as_str()
            .ok_or_else(|| "`args` must contain only strings".to_owned())?;
        // The same characters the registry refuses in its own entries: NUL truncates the wide string
        // that becomes a Windows command line, and CR/LF can split one. A peer must not get to
        // inject what the operator's own configuration is not allowed to contain.
        if let Some(c) = text
            .chars()
            .find(|c| *c == '\0' || *c == '\r' || *c == '\n')
        {
            return Err(format!(
                "`args` contains {c:?}, which would corrupt or split the command line"
            ));
        }
        out.push(text.to_owned());
    }
    Ok(out)
}

/// The retained stdout of the run, as the record points at it.
///
/// The child's output is evidence J.E.A.N. captured, and an MCP client asking for a tool wants that
/// body, not only a verdict about it. It is read through the same cap the client role uses, so
/// publishing a tool does not turn out into an unbounded string.
fn observed_output(ctx: &Context, task_id: &str) -> Option<String> {
    let runs = ctx.store.tool_runs(task_id).ok()?;
    let path = runs.last()?.stdout_path.clone()?;
    let bytes = std::fs::read(Path::new(&path)).ok()?;
    let cap = ctx.config.mcp.max_result_bytes;
    let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(cap)]).to_string();
    if bytes.len() > cap {
        text.push_str("\n[output truncated to the configured result cap]");
    }
    if text.trim().is_empty() {
        return None;
    }
    Some(ctx.redactor.redact(&text))
}
