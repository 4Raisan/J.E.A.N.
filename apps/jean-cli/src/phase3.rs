//! Phase 3 CLI: `jean delegate`, `jean agent`, `jean mcp`.
//!
//! `jean delegate` is the workflow the phase exists for, in one command: the goal goes to an external
//! coding agent, its progress is recorded while it runs, what it changed is listed, and then J.E.A.N.
//! runs the check *it* resolved from `[delegation] verify_command` and reports what it observed. The
//! agent's own account of its success is printed next to that verdict and never replaces it.
//!
//! `jean agent` reads. It says which workers are configured, which binary each name resolves to,
//! which protocol this build can actually speak, and — the part an operator cannot get from a README —
//! that live compatibility with a real agent is a separate claim from support in this build.
//!
//! `jean mcp status` reads in the same spirit; `tools` and `call` start a configured server and talk
//! to it; `serve` makes this process the server on its own stdio. Nothing in the module opens a port.
//!
//! Nothing here grants anything. `jean delegate` passes through the same policy gate as a typed
//! command, and every permission request the agent makes in mid-session is decided by that same
//! engine, so the exit status of a delegation means exactly what the exit status of `jean exec` means.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::owner_preferences;
use jean_core::{
    Context, DelegateRequest, DelegationReport, call_tool, delegate, exposed_tools, list_tools,
    mcp_bounds, mcp_servers, serve_mcp,
};
use jean_policy::{ConsolePrompt, Decision};
use jean_protocol::config::{DelegationConfig, WORKER_KINDS};

use crate::phase1::{field, note};

pub struct DelegateOptions {
    pub goal: String,
    pub profile: Option<String>,
    pub instruction: Option<String>,
    /// The directory the worker is pointed at. `None` uses `[workspace] root`.
    pub workspace: Option<PathBuf>,
    /// `None` uses `[delegation] default_worker`.
    pub worker: Option<String>,
    /// Print the report as JSON instead of lines.
    pub json: bool,
}

pub fn cmd_delegate(ctx: &mut Context, options: DelegateOptions) -> anyhow::Result<ExitCode> {
    let workspace = match (&options.workspace, ctx.config.workspace.root.trim()) {
        (Some(path), _) => path.clone(),
        (None, root) if !root.is_empty() => PathBuf::from(root),
        (None, _) => {
            return Err(anyhow::anyhow!(
                "no workspace was given and [workspace] root is empty — pass `--workspace \
                 <directory>` or set `[workspace] root`"
            ));
        }
    };
    let request = DelegateRequest {
        goal: owner_profile_goal(
            &options.goal,
            options.profile.as_deref(),
            options.instruction.as_deref(),
            owner_preferences::State::load(&ctx.paths.data_dir)?
                .applies(&workspace.canonicalize()?.display().to_string()),
        )?,
        workspace,
        worker: options.worker.clone(),
    };

    let mut prompter = ConsolePrompt::new(&ctx.config.policy);
    let report = delegate(ctx, &request, &mut prompter)?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if options.json {
        writeln!(out, "{}", report_json(&report))?;
    } else {
        for line in report.lines() {
            writeln!(out, "{line}")?;
        }
    }
    Ok(ExitCode::from(report.result.exit_status() as u16 as u8))
}

fn owner_profile_goal(
    goal: &str,
    profile: Option<&str>,
    instruction: Option<&str>,
    learned_no_animation: bool,
) -> anyhow::Result<String> {
    let (name, criteria) = match profile {
        None => ("workspace preference", ""),
        Some("quick-prototype") => (
            "quick prototype",
            "Keep the change minimal; prioritize a working responsive page, reuse existing components, and avoid polish work. Acceptance guidance: render the main flow and verify the key interaction.",
        ),
        Some("polished-ui") => (
            "polished UI",
            "Deliver a refined responsive interface with consistent spacing, typography, loading/empty/error states, keyboard accessibility, subtle motion and cohesive interaction details. Acceptance guidance: inspect responsive states and accessibility-sensitive controls.",
        ),
        Some(other) => anyhow::bail!("unsupported owner profile {other:?}"),
    };
    if profile.is_none() && !learned_no_animation && instruction.is_none() {
        return Ok(goal.to_owned());
    }
    let mut effective = if learned_no_animation {
        let replaced = criteria.replace(
            "subtle motion",
            "no animation (accepted workspace preference)",
        );
        if replaced == criteria {
            format!("{criteria}\nAccepted workspace preference: no animation.")
        } else {
            replaced
        }
    } else {
        criteria.to_owned()
    };
    if profile.is_none() && learned_no_animation {
        effective = "Respect the accepted workspace preference: no animation.".to_owned();
    }
    let override_text = instruction.unwrap_or("").trim();
    let mut override_note = "none";
    if learned_no_animation {
        override_note = "accepted workspace preference: no animation";
    }
    if !override_text.is_empty() {
        // An explicit instruction replaces overlapping profile guidance in the prompt. JEAN's
        // configured verification remains authoritative and is never synthesized from this text.
        effective = if override_text.to_ascii_lowercase().contains("no animation") {
            if profile.is_none() {
                "No animation (explicit task instruction).".to_owned()
            } else {
                let replaced =
                    criteria.replace("subtle motion", "no animation (explicit override)");
                if replaced == criteria {
                    format!("{criteria}\nNo animation (explicit override).")
                } else {
                    replaced
                }
            }
        } else {
            format!(
                "{criteria}\nExplicit task instruction takes precedence where it conflicts: {override_text}"
            )
        };
        override_note = override_text;
    }
    Ok(format!(
        "{goal}\n\nJEAN owner profile: {name}\nEffective acceptance criteria (worker guidance only): {effective}\nExplicit instruction override: {override_note}\nOnly JEAN's independently configured verification determines the verdict."
    ))
}

/// The report as one JSON object.
///
/// Handwritten rather than derived because the fields an operator pipes somewhere are a contract: the
/// worker's claim and J.E.A.N.'s observation are separate keys, and nothing here merges them.
fn report_json(report: &DelegationReport) -> String {
    let text = |value: Option<&str>| quote(value.unwrap_or(""));
    format!(
        "{{\n  \"task_id\": {},\n  \"delegation_id\": {},\n  \"worker\": {},\n  \"kind\": {},\n  \
         \"workspace\": {},\n  \"worktree\": {},\n  \"isolation\": {},\n  \
         \"baseline\": {},\n  \"stop_reason\": {},\n  \"claim\": {},\n  \"changed_files\": {},\n  \
         \"changed_note\": {},\n  \"verification\": {},\n  \"result\": {},\n  \"exit_status\": {},\n  \
         \"events\": {},\n  \"covered_asks\": {},\n  \"prompted_asks\": {},\n  \"transcript\": {},\n  \
         \"notes\": {}\n}}",
        quote(&report.task_id),
        quote(&report.delegation_id),
        quote(&report.worker),
        quote(&report.kind),
        quote(&report.workspace.display().to_string()),
        report
            .worktree
            .as_ref()
            .map(|path| quote(&path.display().to_string()))
            .unwrap_or_else(|| "null".to_owned()),
        quote(&report.isolation),
        quote(report.baseline.label()),
        quote(&report.stop_reason),
        text(report.claim.as_deref()),
        report
            .changed_files
            .iter()
            .map(|file| quote(file))
            .collect::<Vec<_>>()
            .join(", "),
        text(report.changed_note.as_deref()),
        text(report.verification.as_ref().map(|verdict| verdict.label())),
        quote(report.result.as_str()),
        report.result.exit_status(),
        report.events,
        report.covered_asks,
        report.prompted_asks,
        quote(&report.transcript.display().to_string()),
        report
            .notes
            .iter()
            .map(|note| quote(note))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// A JSON string literal, escaped for every character that can break one — including the
/// newlines and tabs that multiline tool output and notes carry.
pub(crate) fn quote(value: &str) -> String {
    serde_json::Value::String(value.to_owned()).to_string()
}

/// A JSON array of note strings. Always bracketed, so an empty note list is `[]`, not a bare value.
pub(crate) fn notes_array(notes: &[String]) -> String {
    format!(
        "[{}]",
        notes.iter().map(|note| quote(note)).collect::<Vec<_>>().join(", ")
    )
}

/// `jean agent status` — what the configuration says, what resolves, and what this build speaks.
pub fn cmd_agent(config: &DelegationConfig) -> anyhow::Result<ExitCode> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    field(&mut out, "enabled", config.enabled)?;
    field(&mut out, "default", config.default_worker.as_str())?;
    field(
        &mut out,
        "parallel",
        format!(
            "{} — this build runs one worker at a time, because cancellation has one active task \
             per process",
            config.max_parallel
        ),
    )?;
    // Resolved here rather than printed raw, so a name that does not exist is visible now, and not
    // after a worker has already been paid for.
    let check = if config.verify_command.trim().is_empty() {
        "not configured — `jean delegate` will refuse to run".to_owned()
    } else {
        match resolve(&config.verify_command) {
            Some(path) => format!("{} {}", path.display(), config.verify_args.join(" ")),
            None => format!("{:?} could not be resolved", config.verify_command),
        }
    };
    field(&mut out, "check", check)?;
    field(
        &mut out,
        "budgets",
        format!(
            "session {} s, idle {} s, permission {} s, shutdown grace {} ms, frame {} B, queue {}, \
             output {} B",
            config.session_timeout_secs,
            config.idle_timeout_secs,
            config.permission_timeout_secs,
            config.shutdown_grace_ms,
            config.max_frame_bytes,
            config.max_event_queue,
            config.max_output_bytes
        ),
    )?;
    writeln!(out)?;

    if config.workers.is_empty() {
        note(
            &mut out,
            "no workers are configured under [delegation.workers]",
        )?;
        return Ok(ExitCode::SUCCESS);
    }

    for worker in &config.workers {
        field(&mut out, "worker", worker.name.as_str())?;
        note(
            &mut out,
            format!("protocol  {} — {}", worker.kind, support_for(&worker.kind)),
        )?;
        match resolve(&worker.command) {
            Some(path) => note(&mut out, format!("binary    {}", path.display()))?,
            None => note(
                &mut out,
                format!(
                    "binary    {:?} was not found on PATH — this worker cannot be started",
                    worker.command
                ),
            )?,
        }
        if !worker.enabled {
            note(&mut out, "state     disabled by configuration")?;
        }
        note(
            &mut out,
            format!(
                "authority capability {}, sandbox {}",
                worker.capability, worker.sandbox
            ),
        )?;
        if !worker.env_passthrough.is_empty() {
            note(
                &mut out,
                format!(
                    "env       {} name(s) forwarded beyond [process].env_allowlist: {}",
                    worker.env_passthrough.len(),
                    worker.env_passthrough.join(", ")
                ),
            )?;
        }
        writeln!(out)?;
    }

    note(
        &mut out,
        "supported above means this build speaks the protocol; it does not mean a session with a \
         live agent was run here. That is recorded separately, and is unverified unless \
         IMPLEMENTATION_STATUS.md says otherwise.",
    )?;
    Ok(ExitCode::SUCCESS)
}

/// The first existing file `command` names, the way a tool command is resolved.
fn resolve(command: &str) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.is_absolute() {
        return path.exists().then(|| path.to_path_buf());
    }
    let extensions = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
        .to_ascii_lowercase();
    std::env::var("PATH")
        .ok()?
        .split(';')
        .filter(|dir| !dir.trim().is_empty())
        .flat_map(|dir| {
            let dir = PathBuf::from(dir);
            std::iter::once(dir.join(command)).chain(
                extensions
                    .split(';')
                    .map(move |ext| dir.join(format!("{command}{ext}"))),
            )
        })
        .find(|candidate| candidate.is_file())
}

/// What `jean-acp` can do with a worker of this kind, in the operator's words.
fn support_for(kind: &str) -> String {
    match kind {
        "acp" => "supported: a bidirectional ACP session, with permission requests answered by \
                 J.E.A.N.'s policy engine"
            .to_owned(),
        "codex-cli" => {
            "supported: the structured JSONL CLI fallback — progress and a final claim, \
                        but no mid-session permission channel"
                .to_owned()
        }
        other => format!(
            "not supported by this build ({other:?} is not a kind it speaks); the kinds are {}",
            WORKER_KINDS.join(" | ")
        ),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// jean mcp
// ─────────────────────────────────────────────────────────────────────────────

/// What `jean mcp` was asked to do.
pub enum McpAction {
    /// Configuration and resolution only. Starts nothing.
    Status,
    /// Start a configured server and ask it what it publishes.
    Tools { server: String },
    /// Start a configured server and call one allowlisted tool on it.
    Call {
        server: String,
        tool: String,
        /// A JSON object as typed; `None` means `{}`.
        arguments: Option<String>,
        json: bool,
    },
    /// This process becomes the server, on its own stdio.
    Serve,
}

pub fn cmd_mcp(ctx: &mut Context, action: McpAction) -> anyhow::Result<ExitCode> {
    match action {
        McpAction::Status => cmd_mcp_status(ctx),
        McpAction::Tools { server } => cmd_mcp_tools(ctx, &server),
        McpAction::Call {
            server,
            tool,
            arguments,
            json,
        } => cmd_mcp_call(ctx, &server, &tool, arguments.as_deref(), json),
        McpAction::Serve => cmd_mcp_serve(ctx),
    }
}

fn cmd_mcp_status(ctx: &Context) -> anyhow::Result<ExitCode> {
    let config = &ctx.config.mcp;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    field(&mut out, "enabled", config.enabled)?;
    let bounds = mcp_bounds(config);
    field(
        &mut out,
        "bounds",
        format!(
            "frame {} B, request {} s, result {} B, shutdown grace {} ms",
            bounds.max_message_bytes,
            bounds.request_timeout.as_secs(),
            bounds.max_result_bytes,
            bounds.shutdown_grace.as_millis()
        ),
    )?;
    writeln!(out)?;

    let servers = mcp_servers(ctx);
    if servers.is_empty() {
        note(&mut out, "no [[mcp.servers]] entries are configured")?;
    }
    for server in servers {
        field(&mut out, "server", &server.name)?;
        note(
            &mut out,
            format!(
                "command   {} {}",
                server
                    .resolved
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| format!("{:?} (not on PATH)", server.command)),
                server.args.join(" ")
            ),
        )?;
        note(
            &mut out,
            format!("authority capability {}", server.capability),
        )?;
        note(
            &mut out,
            format!(
                "allowlist {}",
                if server.allowed_tools.is_empty() {
                    "none — J.E.A.N. will call nothing on this server".to_owned()
                } else {
                    server.allowed_tools.join(", ")
                }
            ),
        )?;
        if !server.env_names.is_empty() {
            note(
                &mut out,
                format!(
                    "env       {} name(s) beyond [process].env_allowlist: {}",
                    server.env_names.len(),
                    server.env_names.join(", ")
                ),
            )?;
        }
        for observation in server.notes {
            note(&mut out, observation)?;
        }
        writeln!(out)?;
    }

    field(&mut out, "expose", {
        let exposed = exposed_tools(ctx);
        if exposed.is_empty() {
            "nothing — `jean mcp serve` would publish no tools".to_owned()
        } else {
            exposed
                .iter()
                .map(|tool| {
                    if !tool.in_registry {
                        format!("{} (not in the registry)", tool.id)
                    } else if !tool.enabled {
                        format!("{} (disabled)", tool.id)
                    } else {
                        tool.id.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
    })?;
    note(
        &mut out,
        "both roles are stdio: `jean mcp call` starts a server J.E.A.N. owns, and `jean mcp serve` \
         answers a client already attached to this process's pipes. There is no listener and no \
         port, and every call in either direction goes through the same policy engine as `jean run`.",
    )?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_mcp_tools(ctx: &mut Context, server: &str) -> anyhow::Result<ExitCode> {
    let mut prompter = ConsolePrompt::new(&ctx.config.policy);
    let listing = list_tools(ctx, server, &mut prompter)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    field(&mut out, "server", &listing.server)?;
    field(&mut out, "task", &listing.task_id)?;
    field(&mut out, "decision", listing.decision.to_string())?;
    if listing.tools.is_empty() {
        for line in &listing.notes {
            note(&mut out, line)?;
        }
        note(
            &mut out,
            "the server published no tools, or the call was refused",
        )?;
        return Ok(ExitCode::from(1));
    }
    writeln!(out)?;
    for tool in listing.tools {
        // The marker is the local allowlist, not the server's claim: what it advertises and what
        // J.E.A.N. will call are two different facts, and conflating them is how an allowlist that
        // looks like a list of names stops being a boundary.
        writeln!(
            out,
            "{} {:30} {}",
            if tool.allowed { "+" } else { " " },
            tool.name,
            tool.description.replace('\n', " ")
        )?;
    }
    writeln!(out)?;
    note(
        &mut out,
        "`+` marks a tool in this server's `allow_tools`, which is the only list `jean mcp call` \
         accepts.",
    )?;
    Ok(match listing.decision {
        Decision::Allowed { .. } => ExitCode::SUCCESS,
        Decision::Denied(_) => ExitCode::from(1),
    })
}

fn cmd_mcp_call(
    ctx: &mut Context,
    server: &str,
    tool: &str,
    arguments: Option<&str>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let arguments: serde_json::Value = match arguments {
        None => serde_json::json!({}),
        Some(text) => serde_json::from_str(text).map_err(|e| {
            anyhow::anyhow!(
                "`--args` must be one JSON object, because that is what a tool call is: {e}"
            )
        })?,
    };
    if !arguments.is_object() {
        return Err(anyhow::anyhow!(
            "`--args` has to be a JSON object ({{\"path\":\"src/main.rs\"}}); a bare value is not \
             an argument set"
        ));
    }

    let mut prompter = ConsolePrompt::new(&ctx.config.policy);
    let report = call_tool(ctx, server, tool, arguments, &mut prompter)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if json {
        write!(
            out,
            "{{\n  \"task_id\": {},\n  \"server\": {},\n  \"tool\": {},\n  \"decision\": {},\n  \
             \"state\": {},\n  \"text\": {},\n  \"bytes\": {},\n  \"truncated\": {},\n  \
             \"tool_error\": {},\n  \"exit_code\": {},\n  \"notes\": {}\n}}\n",
            quote(&report.task_id),
            quote(&report.server),
            quote(&report.tool),
            quote(&report.decision.to_string()),
            quote(report.state.as_str()),
            quote(&report.text),
            report.bytes,
            report.truncated,
            report.tool_error,
            report
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "null".to_owned()),
            notes_array(&report.notes)
        )?;
    } else {
        field(
            &mut out,
            "server",
            format!("{} → {}", report.server, report.tool),
        )?;
        field(&mut out, "task", report.task_id.as_str())?;
        field(&mut out, "decision", report.decision.to_string())?;
        field(&mut out, "state", report.state.as_str())?;
        if report.tool_error {
            note(
                &mut out,
                "the server answered with a tool-level failure — an answer about the tool, not a \
                 broken session",
            )?;
        }
        writeln!(out, "result   ({} bytes{})", report.bytes, {
            if report.truncated {
                ", capped"
            } else {
                ""
            }
        })?;
        writeln!(out, "{}", report.text)?;
        for line in &report.notes {
            note(&mut out, line)?;
        }
    }
    Ok(ExitCode::from(report.exit_status() as u16 as u8))
}

fn cmd_mcp_serve(ctx: &mut Context) -> anyhow::Result<ExitCode> {
    // Nothing may be written to stdout here: those bytes are the transport, and a stray log line
    // makes J.E.A.N. the peer that "speaks plain text instead of JSON-RPC". Diagnostics go to stderr.
    let report = serve_mcp(ctx)?;
    eprintln!(
        "[jean mcp serve] session closed after {} request(s); published {}",
        report.requests,
        report.exposed.join(", ")
    );
    Ok(ExitCode::SUCCESS)
}

// ─────────────────────────────────────────────────────────────────────────────
// `jean browser`
// ─────────────────────────────────────────────────────────────────────────────

/// What `jean browser` was asked to do.
pub enum BrowserAction {
    /// Configuration and detection only. Starts nothing, connects to nothing.
    Status,
    /// Open a page, read it, act on it, and check what it says afterwards.
    Run {
        url: String,
        inspect: Option<String>,
        r#type: Option<String>,
        click: Option<String>,
        expect: Option<String>,
        json: bool,
    },
}

/// One selector-and-value pair, as typed: `--expect "#status=clicked"`.
///
/// Split on the *first* `=`, because the value is page text and page text is allowed to contain one.
fn pair(spec: &str, flag: &str) -> anyhow::Result<(String, String)> {
    let (selector, value) = spec.split_once('=').ok_or_else(|| {
        anyhow::anyhow!("`{flag} {spec}` needs the form `selector=value` (quote it in a shell)")
    })?;
    if selector.trim().is_empty() || value.trim().is_empty() {
        anyhow::bail!("`{flag} {spec}` has an empty selector or value");
    }
    Ok((selector.trim().to_owned(), value.trim().to_owned()))
}

pub fn cmd_browser(ctx: &mut Context, action: BrowserAction) -> anyhow::Result<ExitCode> {
    match action {
        BrowserAction::Status => cmd_browser_status(ctx),
        BrowserAction::Run {
            url,
            inspect,
            r#type,
            click,
            expect,
            json,
        } => {
            let request = jean_core::browser::BrowserRequest {
                url,
                inspect,
                r#type: r#type.as_deref().map(|s| pair(s, "--type")).transpose()?,
                click,
                expect: expect.as_deref().map(|s| pair(s, "--expect")).transpose()?,
            };
            cmd_browser_run(ctx, &request, json)
        }
    }
}

fn cmd_browser_status(ctx: &Context) -> anyhow::Result<ExitCode> {
    let config = &ctx.config.browser;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    field(&mut out, "enabled", config.enabled)?;
    field(
        &mut out,
        "headless",
        if config.headless { "yes" } else { "no" },
    )?;
    field(
        &mut out,
        "bounds",
        format!(
            "startup {} s, action {} s, page {} B",
            config.startup_timeout_secs, config.action_timeout_secs, config.max_page_bytes
        ),
    )?;
    field(
        &mut out,
        "allowed_origins",
        config.allowed_origins.join(", "),
    )?;
    writeln!(out)?;

    let resolved = if !config.browser_path.trim().is_empty() {
        Some(PathBuf::from(config.browser_path.trim()))
    } else {
        jean_core::browser::detected()
    };
    match resolved {
        Some(path) => {
            field(&mut out, "browser", path.display())?;
            note(
                &mut out,
                "this is what `jean browser run` would start, as its own child, with its own empty \
                 profile and a loopback debugging endpoint. Nothing is started by `status`, and \
                 J.E.A.N. never connects to a browser it did not create.",
            )?;
        }
        None => note(
            &mut out,
            "no installed browser was found, and J.E.A.N. does not download one — set [browser] \
             browser_path to the executable to drive",
        )?,
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_browser_run(
    ctx: &mut Context,
    request: &jean_core::browser::BrowserRequest,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let mut prompter = ConsolePrompt::new(&ctx.config.policy);
    let report = jean_core::browser::run(ctx, request, &mut prompter)?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if json {
        writeln!(out, "{}", browser_json(&report))?;
    } else {
        field(&mut out, "task", &report.task_id)?;
        field(&mut out, "url", &report.url)?;
        field(
            &mut out,
            "pid",
            report.pid.map(|p| p.to_string()).unwrap_or_default(),
        )?;
        field(&mut out, "title", &report.title)?;
        if let Some(before) = &report.before {
            field(&mut out, "before", before)?;
        }
        if let Some(after) = &report.after {
            field(&mut out, "after", after)?;
        }
        field(&mut out, "verdict", report.summary_line())?;
        if report.truncated {
            note(
                &mut out,
                "the page markup was capped by [browser] max_page_bytes",
            )?;
        }
        writeln!(out)?;
        for step in &report.steps {
            note(&mut out, step)?;
        }
    }
    Ok(ExitCode::from(browser_exit(&report) as u16 as u8))
}

/// Proven is 0, an expectation that did not hold is 1, and a run that could not be judged is 3 — the
/// same vocabulary `jean delegate` uses, so a script does not have to learn a second one. A refusal is
/// 4: nothing about the page was wrong, the operator said no.
fn browser_exit(report: &jean_core::browser::BrowserReport) -> i32 {
    match (report.state, report.verdict.label()) {
        (_, "proven") => 0,
        (jean_protocol::TaskState::NeedsAttention, _) => 3,
        (_, "not_proven") => 1,
        _ => 4,
    }
}

fn browser_json(report: &jean_core::browser::BrowserReport) -> String {
    let text = |value: Option<&str>| quote(value.unwrap_or(""));
    let steps = report
        .steps
        .iter()
        .map(|s| quote(s))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{{\n  \"task_id\": {},\n  \"url\": {},\n  \"pid\": {},\n  \"title\": {},\n  \
         \"before\": {},\n  \"after\": {},\n  \"verdict\": {},\n  \"detail\": {},\n  \
         \"state\": {},\n  \"exit_status\": {},\n  \"screenshot\": {},\n  \"steps\": [{}]\n}}",
        quote(&report.task_id),
        quote(&report.url),
        report
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "null".to_owned()),
        quote(&report.title),
        text(report.before.as_deref()),
        text(report.after.as_deref()),
        quote(report.verdict.label()),
        quote(report.verdict.detail()),
        quote(report.state.as_str()),
        browser_exit(report),
        text(
            report
                .screenshot
                .as_deref()
                .map(|p| p.display().to_string())
                .as_deref()
        ),
        steps
    )
}

#[cfg(test)]
mod tests {
    use super::{notes_array, quote};

    /// The `mcp call --json` report must stay parseable when a tool returns multiline text and
    /// several notes. The old escaper handled only `\` and `"`, so a raw newline in `text` produced
    /// invalid JSON (a standard parser rejected it with "invalid control character").
    #[test]
    fn mcp_call_json_survives_multiline_output_and_multiple_notes() {
        let text = "line one\nline two\ttabbed\r\n\"quoted\" ✅ final";
        let notes = ["first note\nwith a newline", "second note\twith a tab"];
        let report = format!(
            "{{\n  \"text\": {},\n  \"notes\": [{}]\n}}\n",
            quote(text),
            notes.iter().map(|note| quote(note)).collect::<Vec<_>>().join(", ")
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&report).expect("a standard JSON parser must read the report");
        assert_eq!(parsed["text"].as_str(), Some(text));
        let parsed_notes = parsed["notes"].as_array().expect("notes is an array");
        assert_eq!(parsed_notes.len(), notes.len());
        for (parsed, original) in parsed_notes.iter().zip(notes) {
            assert_eq!(parsed.as_str(), Some(original));
        }
    }

    #[test]
    fn quote_escapes_every_control_character() {
        let raw = "\u{0}\u{1}\n\r\t\u{8}\u{c}\u{1f}";
        let parsed: serde_json::Value =
            serde_json::from_str(&quote(raw)).expect("control characters must be escaped");
        assert_eq!(parsed.as_str(), Some(raw));
    }

    #[test]
    fn quote_escapes_backslash() {
        let raw = r"back\slash";
        let parsed: serde_json::Value =
            serde_json::from_str(&quote(raw)).expect("backslash must be escaped");
        assert_eq!(parsed.as_str(), Some(raw));
    }

    #[test]
    fn notes_array_is_always_a_bracketed_json_array() {
        let empty: Vec<String> = Vec::new();
        assert_eq!(notes_array(&empty), "[]");
        let parsed: serde_json::Value =
            serde_json::from_str(&notes_array(&empty)).expect("empty notes must be a valid array");
        assert_eq!(parsed.as_array().map(Vec::len), Some(0));

        let notes = vec![
            "first note\nwith a newline".to_owned(),
            "second note\twith a tab".to_owned(),
        ];
        let parsed: serde_json::Value = serde_json::from_str(&notes_array(&notes))
            .expect("multiple multiline notes must be a valid array");
        let array = parsed.as_array().expect("notes is an array");
        assert_eq!(array.len(), 2);
        assert_eq!(array[0].as_str(), Some(notes[0].as_str()));
        assert_eq!(array[1].as_str(), Some(notes[1].as_str()));
    }
}
