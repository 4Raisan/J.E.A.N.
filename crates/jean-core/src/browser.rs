//! The browser workflow: one page, opened by J.E.A.N., read by J.E.A.N., judged by J.E.A.N.
//!
//! ## The shape of the authority
//!
//! `[browser] enabled` is off by default, and turning it on does not authorize a page. What authorizes
//! one is the policy engine, consulted with an operation whose **scope is the origin being loaded** —
//! the same rule the MCP client follows, where the digest is what a remembered approval binds to. So an
//! approval given for a local fixture is not the reason a signed-in site gets driven next week, and the
//! audit line names the executable, the origin and the selectors, which is what a person reviewing the
//! log needs to see.
//!
//! ```text
//! config gate → origin gate → resolve executable → task row → policy decision → audit
//!             → start JEAN's own browser → navigate → read → act → read again → verify → close
//! ```
//!
//! Two gates run before a process exists, because both are things the operator decided in configuration
//! rather than things a prompt can undo: a disabled subsystem, and an origin outside the allowlist. A
//! refusal at either point produces no task row, no browser, and one sentence saying why.
//!
//! ## Why the verification is a second read
//!
//! `expect` is stated *before* the interaction — a selector and the text that element must show
//! afterwards — and is then read back through the DOM, with a screenshot of the rendered page kept
//! beside it as a second witness. The click's own return value proves only that an event was dispatched;
//! the page had to change for the check to match. This is the same distinction the delegation path draws
//! between an agent's claim and the check J.E.A.N. runs, and it produces the same three-way [`Verdict`],
//! which maps onto the task states the way every other Phase-3 result does: proven succeeded, not
//! proven failed, unverifiable needs attention — because a run that could not be judged is not a run
//! that passed.
//!
//! ## What is not here
//!
//! No attach path, no download, no persistent profile, no network listener, and no cancel channel: one
//! command drives one browser for one workflow, bounded by `action_timeout_secs` on every protocol call
//! and by the job object around the process.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jean_policy::{
    AuditEvent, AuditWriter, Decision, Operation, OperationKind, Policy, Prompter, Scope,
};
use jean_protocol::config::BrowserConfig;
use jean_protocol::event::{Event, EventKind};
use jean_protocol::tool::PolicyTier;
use jean_protocol::{StreamSummary, TaskState, Termination};
use jean_store::{Store, TaskRecord};

use crate::context::Context;
use crate::error::{CoreError, Result};
use crate::verify::Verdict;

/// The capability a browser workflow declares, and the name it is approved and audited under.
pub const BROWSER_CAPABILITY: &str = "browser.drive";

/// How long to wait for the page to settle after an interaction before reading the check.
///
/// A click that starts an animation or a `setTimeout` needs one beat, and a workflow that read the DOM
/// in the same tick would report `not_proven` for a page that was about to be right.
fn settle_delay(action_timeout: Duration) -> Duration {
    action_timeout / 4
}

/// One browser workflow, stated as data before any of it runs.
#[derive(Debug, Clone, Default)]
pub struct BrowserRequest {
    /// The page to open. Must be covered by `[browser] allowed_origins`.
    pub url: String,
    /// Read this element's text **before** acting, so "what changed" has a baseline.
    pub inspect: Option<String>,
    /// Focus this element and type into it.
    pub r#type: Option<(String, String)>,
    /// A real pointer click on this element.
    pub click: Option<String>,
    /// The check: this element must show exactly this text afterwards.
    pub expect: Option<(String, String)>,
}

/// What one workflow produced, in the order it happened.
#[derive(Debug, Clone)]
pub struct BrowserReport {
    pub task_id: String,
    pub url: String,
    pub pid: Option<u32>,
    pub title: String,
    /// The page's own markup, capped by `max_page_bytes`.
    pub excerpt: String,
    pub truncated: bool,
    /// What `inspect` showed before the interaction, when one was asked for.
    pub before: Option<String>,
    /// What `expect`'s selector showed after it.
    pub after: Option<String>,
    pub verdict: Verdict,
    pub screenshot: Option<PathBuf>,
    pub state: TaskState,
    pub exit_code: Option<i32>,
    pub steps: Vec<String>,
}

impl BrowserReport {
    /// The one line the CLI prints: the verdict, and where to look.
    pub fn summary_line(&self) -> String {
        format!(
            "{}: {} — screenshot {}",
            self.verdict.label(),
            self.verdict.detail(),
            self.screenshot
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "not taken".to_owned())
        )
    }
}

/// Run one workflow. The browser exists only between the policy decision and the return.
pub fn run(
    ctx: &mut Context,
    request: &BrowserRequest,
    prompter: &mut dyn Prompter,
) -> Result<BrowserReport> {
    ctx.prepare_for_writing()?;
    let config = ctx.config.browser.clone();
    require_enabled(&config)?;

    // The origin gate is first, before the executable is even chosen: `allowed_origins` is the
    // operator's boundary, and crossing it is not a thing a yes at a prompt can authorise.
    if !jean_browser::origin_permitted(&config, &request.url) {
        return Err(CoreError::Intent {
            reason: format!(
                "{} is outside [browser] allowed_origins ({:?}); J.E.A.N. will not open a page it has \
                 no rule for",
                request.url, config.allowed_origins
            ),
        });
    }
    let executable = resolve_browser(&config)?;

    let operation = browser_operation(ctx, &executable, request);
    let policy = Policy::new(ctx.config.policy.clone());
    let audit = AuditWriter::new(
        ctx.paths.audit.clone(),
        ctx.config.policy.audit_enabled,
        ctx.redactor.clone(),
    );
    let task_id = open_task(ctx, &operation, request)?;
    let actor = crate::intent::actor();

    if would_prompt(&policy, &ctx.store, &operation)? {
        ctx.store.transition(
            &task_id,
            TaskState::AwaitingApproval,
            Some("driving a browser needs a yes"),
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

    if let Decision::Denied(reason) = &decision {
        let text = AuditEvent::denial_text(reason);
        ctx.store
            .transition(&task_id, TaskState::Failed, Some(&text))?;
        ctx.store.set_outcome(&task_id, "denied", &text)?;
        ctx.store.append_event(&task_id, &Event::note(&text))?;
        let _ = ctx.store.clear_runner(&task_id);
        return Ok(BrowserReport {
            task_id,
            url: request.url.clone(),
            pid: None,
            title: String::new(),
            excerpt: String::new(),
            truncated: false,
            before: None,
            after: None,
            verdict: Verdict::Unverifiable(text),
            screenshot: None,
            state: TaskState::Failed,
            exit_code: None,
            steps: vec![
                "nothing was started: the browser process is created only after the decision allows it"
                    .to_owned(),
            ],
        });
    }

    drive(ctx, &config, &task_id, &executable, request).inspect_err(|e| {
        let reason = e.to_string();
        ctx.store
            .transition(&task_id, TaskState::Failed, Some(&reason))
            .ok();
        ctx.store.set_outcome(&task_id, "error", &reason).ok();
    })
}

/// Start, act, verify, stop, and write down every step.
fn drive(
    ctx: &mut Context,
    config: &BrowserConfig,
    task_id: &str,
    executable: &Path,
    request: &BrowserRequest,
) -> Result<BrowserReport> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default();
    let artifact_dir = ctx
        .paths
        .artifacts
        .join(format!("browser-{task_id}-{stamp}"));
    make_dir(&artifact_dir)?;
    // A profile J.E.A.N. made, in a directory J.E.A.N. chose, which `jean-browser` refuses to share: it
    // will not start on a profile that already holds something, so no setting can point this at a
    // person's own.
    let profile = artifact_dir.join("profile");
    make_dir(&profile)?;

    ctx.store.transition(
        task_id,
        TaskState::Running,
        Some("the decision allowed the page"),
    )?;
    crate::runner::record_ownership(&ctx.store, task_id)?;
    let run_id = ctx.store.insert_tool_run(
        task_id,
        "browser.local",
        &executable.display().to_string(),
        &crate::runner::to_json_array(std::slice::from_ref(&request.url)),
        Some(&profile.display().to_string()),
        "{}",
    )?;

    let mut browser = match jean_browser::OwnedBrowser::start(config, executable, &profile, &[]) {
        Ok(browser) => browser,
        Err(e) => {
            let reason = e.to_string();
            abandon(ctx, task_id, run_id, &reason);
            return Err(CoreError::Intent { reason });
        }
    };
    let pid = browser.pid();
    let _ = ctx.store.record_child_pid(run_id, pid);

    let mut steps = vec![format!("started pid {pid} on {}", browser.endpoint())];
    ctx.store.append_event(
        task_id,
        &Event::new(
            EventKind::BrowserAction,
            format!("started pid {pid}, a browser this process created"),
        ),
    )?;

    let mut report = workflow(
        ctx,
        config,
        task_id,
        request,
        &mut browser,
        &mut steps,
        &artifact_dir,
    )
    .into_report(task_id);

    let exit_code = match browser.close() {
        Ok(code) => code,
        Err(e) => {
            report
                .steps
                .push(format!("the browser did not close cleanly: {e}"));
            None
        }
    };
    report.exit_code = exit_code;
    // The profile is disposable because J.E.A.N. made it, in this run, under its own artifact
    // directory. Chrome's helper processes may hold a file briefly after the
    // main pid exits, so retry instead of a single remove_dir_all that
    // leaves the directory behind (§60.9). Bounded: 20 x 100ms = 2s.
    let mut last_err: Option<std::io::Error> = None;
    for _ in 0..20 {
        match std::fs::remove_dir_all(&profile) {
            Ok(()) => { last_err = None; break; }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    if let Some(e) = last_err {
        tracing::warn!(profile = %profile.display(), error = %e, "disposable browser profile could not be removed; it will be cleaned by the next run");
    }

    let verdict = report.verdict.clone();
    let state = match verdict {
        Verdict::Proven(_) => TaskState::Succeeded,
        Verdict::NotProven(_) => TaskState::Failed,
        Verdict::Unverifiable(_) => TaskState::NeedsAttention,
    };
    report.state = state;
    // The state machine asks for the judgement to be a step of its own, and it is right: the page was
    // acted on, and what happens next is J.E.A.N. reading it back.
    ctx.store.transition(
        task_id,
        TaskState::Verifying,
        Some("the page was read back"),
    )?;
    ctx.store.finish_tool_run(
        run_id,
        exit_code,
        termination_of(exit_code),
        &StreamSummary {
            path: report
                .screenshot
                .as_deref()
                .map(|p| p.display().to_string()),
            bytes: report.excerpt.len() as u64,
            truncated: report.truncated,
        },
        &Default::default(),
        None,
    )?;
    let detail = report.verdict.detail().to_owned();
    ctx.store.transition(task_id, state, Some(&detail))?;
    ctx.store
        .set_outcome(task_id, report.verdict.label(), &detail)?;
    let _ = ctx.store.clear_runner(task_id);
    Ok(report)
}

/// The loop: navigate, read, act, read again, judge.
///
/// A page that refuses any step produces a report rather than an error — the browser still has to be
/// closed and the run still has to be recorded — so only a failure to *write the record* escapes.
fn workflow(
    ctx: &mut Context,
    config: &BrowserConfig,
    task_id: &str,
    request: &BrowserRequest,
    browser: &mut jean_browser::OwnedBrowser,
    steps: &mut Vec<String>,
    artifact_dir: &Path,
) -> Unsettled {
    let stalled = |steps: &mut Vec<String>, reason: String| Unsettled {
        url: request.url.clone(),
        pid: None,
        title: String::new(),
        excerpt: String::new(),
        truncated: false,
        before: None,
        after: None,
        verdict: Verdict::Unverifiable(reason.clone()),
        screenshot: None,
        steps: {
            steps.push(reason);
            std::mem::take(steps)
        },
    };

    if let Err(e) = browser.navigate(config, &request.url) {
        return stalled(steps, format!("navigation failed: {e}"));
    }
    note(ctx, task_id, steps, format!("navigated to {}", request.url));

    let title = browser.title().unwrap_or_default();
    let (excerpt, truncated) = match browser.content(config.max_page_bytes) {
        Ok(pair) => pair,
        Err(e) => return stalled(steps, format!("the page could not be read: {e}")),
    };
    note(
        ctx,
        task_id,
        steps,
        format!("read {} bytes of page markup", excerpt.len()),
    );

    let before = match &request.inspect {
        Some(selector) => match browser.text_of(selector) {
            Ok(text) => {
                note(
                    ctx,
                    task_id,
                    steps,
                    format!("{selector} showed {text:?} before anything was done to it"),
                );
                Some(text)
            }
            Err(e) => return stalled(steps, format!("the inspection failed: {e}")),
        },
        None => None,
    };

    if let Some((selector, text)) = &request.r#type {
        if let Err(e) = browser.fill(selector, text) {
            return stalled(steps, format!("typing into {selector} failed: {e}"));
        }
        note(ctx, task_id, steps, format!("typed into {selector}"));
    }
    if let Some(selector) = &request.click {
        if let Err(e) = browser.click(selector) {
            return stalled(steps, format!("the click on {selector} failed: {e}"));
        }
        note(
            ctx,
            task_id,
            steps,
            format!("clicked {selector} with a real pointer event"),
        );
        std::thread::sleep(settle_delay(Duration::from_secs(
            config.action_timeout_secs,
        )));
    }

    let screenshot = artifact_dir.join("page.png");
    let shot = match browser.screenshot(&screenshot) {
        Ok(bytes) => {
            note(
                ctx,
                task_id,
                steps,
                format!("captured the rendered page ({bytes} bytes)"),
            );
            Some(screenshot)
        }
        Err(e) => {
            steps.push(format!("the screenshot could not be taken: {e}"));
            None
        }
    };

    // The check runs against a fresh read, after the interaction. Nothing the click reported is used:
    // a dispatched event says the browser heard, not that the page changed.
    let (verdict, after) = match &request.expect {
        Some((selector, expected)) => match browser.text_of(selector) {
            Ok(text) => {
                let shown = text.trim().to_owned();
                let verdict = if shown == expected.trim() {
                    Verdict::Proven(format!("{selector} shows {shown:?} after the interaction"))
                } else {
                    Verdict::NotProven(format!(
                        "{selector} shows {shown:?}, not the {expected:?} the check asked for"
                    ))
                };
                (verdict, Some(shown))
            }
            Err(e) => (
                Verdict::Unverifiable(format!("{selector} could not be read afterwards: {e}")),
                None,
            ),
        },
        None => (
            Verdict::Unverifiable(
                "no expectation was given, so the page was read and nothing was proven".to_owned(),
            ),
            None,
        ),
    };
    ctx.store
        .append_event(
            task_id,
            &Event::new(
                EventKind::VerificationRecorded,
                format!("{}: {}", verdict.label(), verdict.detail()),
            ),
        )
        .ok();
    steps.push(format!("verification: {}", verdict.detail()));

    Unsettled {
        url: request.url.clone(),
        pid: Some(browser.pid()),
        title,
        excerpt,
        truncated,
        before,
        after,
        verdict,
        screenshot: shot,
        steps: std::mem::take(steps),
    }
}

/// What the workflow observed, before the run row and the task row are closed out.
struct Unsettled {
    url: String,
    pid: Option<u32>,
    title: String,
    excerpt: String,
    truncated: bool,
    before: Option<String>,
    after: Option<String>,
    verdict: Verdict,
    screenshot: Option<PathBuf>,
    steps: Vec<String>,
}

impl Unsettled {
    fn into_report(self, task_id: &str) -> BrowserReport {
        BrowserReport {
            task_id: task_id.to_owned(),
            url: self.url,
            pid: self.pid,
            title: self.title,
            excerpt: self.excerpt,
            truncated: self.truncated,
            before: self.before,
            after: self.after,
            verdict: self.verdict,
            screenshot: self.screenshot,
            state: TaskState::Running,
            exit_code: None,
            steps: self.steps,
        }
    }
}

/// The row is closed even when nothing was proven, or the database would say a browser is still open
/// that this process already took down.
fn abandon(ctx: &mut Context, task_id: &str, run_id: i64, reason: &str) {
    if let Err(e) = ctx.store.finish_tool_run(
        run_id,
        None,
        Termination::LaunchFailed,
        &Default::default(),
        &Default::default(),
        Some(reason),
    ) {
        tracing::warn!(error = %e, run_id, "the browser run row could not be closed");
    }
    let _ = ctx.store.clear_runner(task_id);
}

fn note(ctx: &mut Context, task_id: &str, steps: &mut Vec<String>, detail: String) {
    steps.push(detail.clone());
    if let Err(e) = ctx.store.append_event(
        task_id,
        &Event::new(EventKind::BrowserAction, ctx.redactor.redact(&detail)),
    ) {
        tracing::warn!(error = %e, "a browser step could not be recorded");
    }
}

fn make_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| CoreError::File {
        path: dir.to_path_buf(),
        reason: e.to_string(),
    })
}

/// The browser is closed by J.E.A.N. — that is the promise of owning it — so an observed code is the
/// end of a process this process ended, and the absence of one means it was still going when the
/// grace ran out.
fn termination_of(exit_code: Option<i32>) -> Termination {
    match exit_code {
        Some(_) => Termination::Exited,
        None => Termination::Cancelled,
    }
}

/// Where an installed browser was found, for `jean browser status`. Detection only: nothing here
/// starts, downloads or attaches to anything.
pub fn detected() -> Option<PathBuf> {
    jean_browser::detect_browser()
}

/// The browser to start: the configured path, or an installed one, or a blocker that says which.
fn resolve_browser(config: &BrowserConfig) -> Result<PathBuf> {
    if !config.browser_path.trim().is_empty() {
        let path = PathBuf::from(config.browser_path.trim());
        if path.is_file() {
            return Ok(path);
        }
        return Err(CoreError::Intent {
            reason: format!("browser.browser_path ({}) is not a file", path.display()),
        });
    }
    jean_browser::detect_browser().ok_or_else(|| CoreError::Intent {
        reason:
            "no installed browser was found, and J.E.A.N. does not download one; set [browser] \
                 browser_path to the executable to drive"
                .to_owned(),
    })
}

fn require_enabled(config: &BrowserConfig) -> Result<()> {
    if config.enabled {
        Ok(())
    } else {
        Err(CoreError::Intent {
            reason: "[browser] enabled = false: J.E.A.N. does not start a browser unless the \
                     configuration says it may"
                .to_owned(),
        })
    }
}

fn browser_operation(ctx: &Context, executable: &Path, request: &BrowserRequest) -> Operation {
    // The summary is redacted because a URL can carry a session token in its query string, and the
    // summary is what a prompt shows and what the approvals ledger keeps.
    let summary = ctx.redactor.redact(&format!(
        "drive {} to open {}",
        executable.file_name().unwrap_or_default().to_string_lossy(),
        request.url
    ));
    let mut arguments = vec![executable.display().to_string(), origin_of(&request.url)];
    if let Some(selector) = &request.inspect {
        arguments.push(format!("read:{selector}"));
    }
    if let Some((selector, _)) = &request.r#type {
        arguments.push(format!("type:{selector}"));
    }
    if let Some(selector) = &request.click {
        arguments.push(format!("click:{selector}"));
    }
    if let Some((selector, expected)) = &request.expect {
        arguments.push(format!("expect:{selector}={expected}"));
    }
    Operation {
        kind: OperationKind::DriveBrowser,
        tool_id: Some("browser.local".to_owned()),
        tier: PolicyTier::Normal,
        capabilities: vec![BROWSER_CAPABILITY.to_owned()],
        summary,
        // The origin, not the whole URL: the digest is what a remembered approval binds to, and a token
        // in a query string would make every approval single-use by accident.
        scope: Scope::new(origin_of(&request.url), arguments),
    }
}

fn origin_of(url: &str) -> String {
    jean_browser::Origin::of(url).map_or_else(
        || url.to_owned(),
        |origin| match origin.port {
            Some(port) => format!("{}://{}:{}", origin.scheme, origin.host, port),
            None => format!("{}://{}", origin.scheme, origin.host),
        },
    )
}

fn open_task(ctx: &mut Context, operation: &Operation, request: &BrowserRequest) -> Result<String> {
    let task_id = jean_protocol::ipc::new_task_id();
    let request_json = serde_json::to_string(&serde_json::json!({
        "url": request.url,
        "inspect": request.inspect,
        "click": request.click,
        "expect": request.expect,
        "capability": BROWSER_CAPABILITY,
        "tier": operation.tier.as_str(),
    }))
    .map_err(|e| CoreError::File {
        path: PathBuf::from("<request record>"),
        reason: e.to_string(),
    })?;
    ctx.store.insert_task(&TaskRecord::new(
        task_id.clone(),
        operation.summary.clone(),
        Some("browser.local".to_owned()),
        request_json,
    ))?;
    ctx.store.transition(
        &task_id,
        TaskState::Planning,
        Some("the page and the browser were resolved"),
    )?;
    Ok(task_id)
}

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
