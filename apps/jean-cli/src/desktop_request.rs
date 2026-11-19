//! The overlay's task entry point. Routing selects an existing engine path and
//! leaves tool resolution, approvals, supervision and verification to that path.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};

use clap::Args;
use jean_core::intent::{CommandRequest, Intent, Payload};
use jean_core::{delegation, CancelFlag, Context};
use jean_policy::{prompt::Prompt, Answer, DenyReason, PolicyError, Prompter};
use serde::Deserialize;
use serde_json::json;

#[derive(Args)]
pub(crate) struct Options {
    #[arg(long)]
    channel: String,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    worker: Option<String>,
    #[arg(long, value_parser = ["quick-prototype", "polished-ui"])]
    profile: Option<String>,
    #[arg(long)]
    instruction: Option<String>,
    goal: String,
}

static CHANNEL: OnceLock<String> = OnceLock::new();

fn event(body: serde_json::Value) {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(
        stdout,
        "JEAN:{} {body}",
        CHANNEL.get().expect("desktop channel set")
    );
    let _ = stdout.flush();
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Input {
    Approve { id: u64, allow: bool },
    Cancel,
}

struct DesktopPrompt {
    answers: mpsc::Receiver<Input>,
    cancel: CancelFlag,
    timeout: Duration,
    next_id: u64,
}

impl DesktopPrompt {
    fn new(timeout: Duration, cancel: CancelFlag) -> Self {
        let (tx, answers) = mpsc::sync_channel(8);
        let cancellation = cancel.clone();
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines().map_while(Result::ok) {
                match serde_json::from_str::<Input>(&line) {
                    Ok(Input::Cancel) => break,
                    Ok(answer) => {
                        let _ = tx.try_send(answer);
                    }
                    Err(_) => break,
                }
            }
            cancellation.raise();
            jean_core::signals::cancel_owned_task();
        });
        Self {
            answers,
            cancel,
            timeout,
            next_id: 0,
        }
    }
}

impl Prompter for DesktopPrompt {
    fn ask(&mut self, prompt: &Prompt<'_>) -> Result<Answer, PolicyError> {
        self.next_id += 1;
        event(json!({
            "kind": "approval", "id": self.next_id,
            "summary": prompt.summary, "operation": prompt.operation,
            "tool": prompt.tool_id, "tier": prompt.tier,
            "capabilities": prompt.capabilities,
            "timeout_ms": self.timeout.as_millis(),
        }));
        let start = Instant::now();
        while !self.cancel.raised() {
            let remaining = self.timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                break;
            }
            match self.answers.recv_timeout(remaining) {
                Ok(Input::Approve { id, allow }) if id == self.next_id && !self.cancel.raised() => {
                    return Ok(if allow {
                        Answer::ApproveOnce
                    } else {
                        Answer::Denied(DenyReason::Declined)
                    });
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        Ok(Answer::Denied(DenyReason::Unanswerable(
            "desktop approval expired or the request was cancelled".into(),
        )))
    }
}

pub(crate) fn run(ctx: &mut Context, options: Options) -> anyhow::Result<ExitCode> {
    let goal = options.goal.trim();
    anyhow::ensure!(!goal.is_empty(), "enter a goal");
    anyhow::ensure!(
        options.channel.len() == 32 && options.channel.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid desktop channel"
    );
    CHANNEL
        .set(options.channel.clone())
        .expect("one desktop request per process");
    ctx.task_started = |id| event(json!({ "kind": "task", "id": id }));

    let cancel = CancelFlag::new();
    let mut prompt = DesktopPrompt::new(
        Duration::from_secs(ctx.config.policy.approval_timeout_secs.max(1)),
        cancel.clone(),
    );
    match route(ctx, options, &mut prompt, &cancel) {
        Err(error) if cancel.raised() => {
            println!("result: cancelled");
            println!("reason: {}", ctx.redactor.redact(&error.to_string()));
            Ok(ExitCode::from(130))
        }
        result => result,
    }
}

fn route(
    ctx: &mut Context,
    options: Options,
    prompt: &mut DesktopPrompt,
    cancel: &CancelFlag,
) -> anyhow::Result<ExitCode> {
    let goal = options.goal.trim();
    let (command, arguments) = split_first(goal);
    if command == "/dream" {
        let deep = arguments.split_whitespace().any(|tok| tok == "--deep");
        let scope = options
            .workspace
            .as_deref()
            .map(|p| p.display().to_string());
        let model = if deep && ctx.config.model.planning_enabled() {
            Some(jean_core::build_model(&ctx.config.model)?)
        } else {
            None
        };
        let report = jean_core::dream::dream(
            ctx,
            model.as_deref(),
            scope.as_deref(),
            cancel.atomic(),
        )?;
        println!("result: {}", report.cycle.summary);
        for pb in &report.promoted_playbooks {
            println!("promoted playbook: {pb}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if command == "/recall" {
        let scope = options
            .workspace
            .as_deref()
            .map(|p| p.display().to_string());
        let recalled = jean_core::dream::recall_context(
            ctx,
            arguments,
            scope.as_deref(),
            jean_core::dream::DEFAULT_RECALL_CHARS,
        )?;
        println!("result: {}", recalled.render_spoken_summary());
        return Ok(ExitCode::SUCCESS);
    }
    if command == "/playbook" {
        let (sub, rest) = split_first(arguments);
        match sub {
            "" | "list" => {
                let scope = options
                    .workspace
                    .as_deref()
                    .map(|p| p.display().to_string());
                let list = ctx.store.list_playbooks(scope.as_deref(), 50)?;
                if list.is_empty() {
                    println!("result: No playbooks saved yet.");
                } else {
                    let names: Vec<String> = list
                        .iter()
                        .map(|pb| format!("{} (v{}, verified {}x)", pb.name, pb.version, pb.verified_successes))
                        .collect();
                    println!("result: {}", names.join(", "));
                }
                return Ok(ExitCode::SUCCESS);
            }
            "run" => {
                let Some(matched) = jean_core::playbooks::match_known_task(
                    ctx,
                    &format!("run playbook {rest}"),
                    options
                        .workspace
                        .as_deref()
                        .map(|p| p.to_string_lossy())
                        .as_deref(),
                )?
                else {
                    anyhow::bail!("No saved playbook matched {rest:?}.");
                };
                return crate::playbook::execute_playbook(
                    ctx,
                    &matched.record,
                    &matched.steps,
                    options.workspace.as_deref(),
                    prompt,
                    cancel,
                );
            }
            "undo" => {
                let restored = ctx
                    .store
                    .undo_playbook(rest)?
                    .ok_or_else(|| anyhow::anyhow!("Playbook {rest:?} not found."))?;
                println!("result: Restored playbook {} to v{}.", restored.name, restored.version);
                return Ok(ExitCode::SUCCESS);
            }
            "forget" => {
                let removed = ctx
                    .store
                    .forget_playbook(rest)?
                    .ok_or_else(|| anyhow::anyhow!("Playbook {rest:?} not found."))?;
                println!("result: Removed playbook {}.", removed.name);
                return Ok(ExitCode::SUCCESS);
            }
            _ => anyhow::bail!("Use /playbook list, /playbook run <name>, /playbook undo <name>, or /playbook forget <name>."),
        }
    }
    if command == "/workspace" {
        let root = options.workspace.as_deref()
            .ok_or_else(|| anyhow::anyhow!("Select a workspace with Browse first."))?;
        return crate::workspace_goal::run(ctx, root, arguments, prompt, cancel);
    }
    if command == "/desktop" {
        let task = serde_json::from_str(arguments)?;
        return crate::desktop_goal::run(ctx, task, prompt, cancel);
    }
    if command == "/browse" {
        let (verify_video, url) = if let Some(rest) = arguments.strip_prefix("--verify-video ") {
            (true, rest.trim())
        } else {
            (false, arguments.trim())
        };
        anyhow::ensure!(
            url.starts_with("https://") || url.starts_with("http://"),
            "Enter a browser URL (example: /browse https://example.com/)."
        );
        let expect_non_empty = (!verify_video && url.to_ascii_lowercase().contains("bing.com/search"))
            .then(|| "#b_results".to_owned());
        let request = jean_core::browser::BrowserRequest {
            url: url.to_owned(),
            inspect: expect_non_empty.clone(),
            r#type: None,
            click: None,
            expect: None,
            expect_non_empty,
            verify_video,
            video_query: None,
            allow_audio: verify_video,
        };
        let report = jean_core::browser::run(ctx, &request, prompt)?;
        if let Some(pb) = &report.playback {
            println!(
                "result: Playing {} ({:.2}s -> {:.2}s, {}x{})\n{}",
                pb.video_title, pb.start_secs, pb.end_secs, pb.video_width, pb.video_height, report.url
            );
            if let Some(path) = &report.screenshot {
                println!("Screenshot: {}", path.display());
            }
        } else if !report.title.is_empty() {
            println!("result: {}\n{}", report.title, report.url);
            if let Some(path) = &report.screenshot {
                println!("Screenshot: {}", path.display());
            }
        } else {
            println!("reason: {}", report.verdict.detail());
        }
        return Ok(ExitCode::from(crate::phase3::browser_exit(&report) as u8));
    }
    if command == "/windows" {
        let report = jean_core::mcp::call_tool(ctx, "cua", "list_windows", json!({}), prompt)?;
        if matches!(report.decision, jean_policy::Decision::Denied(_)) || report.tool_error {
            let reason = report
                .notes
                .last()
                .cloned()
                .unwrap_or_else(|| report.decision.to_string());
            println!("reason: {reason}");
        } else {
            println!("result: {}", report.text.trim());
        }
        return Ok(ExitCode::from(report.exit_status() as u8));
    }
    if command == "/click" {
        let (pid, window_id, label, before, after): (u32, u64, String, String, String) =
            serde_json::from_str(arguments).map_err(|_| {
                anyhow::anyhow!("Use /click [pid, window_id, \"control\", \"before\", \"after\"]")
            })?;
        let report = jean_core::mcp::click_cua_control(
            ctx,
            (pid, window_id),
            &label,
            &before,
            &after,
            prompt,
            cancel,
        )?;
        println!("result: {}", report.text);
        return Ok(ExitCode::from(report.exit_status() as u8));
    }
    match command {
        "/review" => {
            let preview = delegation::review::preview(ctx, arguments)?;
            crate::print_review(ctx, arguments, &preview)?;
            event(json!({
                "kind": "review", "id": arguments, "sha256": preview.sha256,
                "baseline": preview.baseline, "workspace": preview.workspace,
                "state": preview.state, "patch": crate::review_patch_text(&preview.patch),
            }));
            return Ok(ExitCode::SUCCESS);
        }
        "/apply" => {
            let (id, sha256) = split_first(arguments);
            delegation::review::apply(ctx, id, sha256, prompt)?;
            println!("result: applied to the working tree and index");
            return Ok(ExitCode::SUCCESS);
        }
        "/discard" => {
            delegation::review::discard(ctx, arguments)?;
            println!("result: discarded; isolated evidence retained");
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }
    if let Some(id) = goal.strip_prefix("/resume ") {
        let (id, flag) = split_first(id.trim());
        anyhow::ensure!(
            flag.is_empty() || flag == "--condition-cleared",
            "usage: /resume <task-id> [--condition-cleared]"
        );
        jean_protocol::ipc::validate_task_id(id)?;
        let outcome = if flag == "--condition-cleared" {
            delegation::resume_checkpoint_after_condition_cleared(ctx, id, prompt)?
        } else {
            delegation::resume_checkpoint(ctx, id, prompt)?
        };
        return crate::print_resume(outcome);
    }

    let (first, rest) = split_first(goal);
    let (tool, args) = if first == "/tool" {
        let (tool, arguments) = split_first(rest);
        anyhow::ensure!(!tool.is_empty(), "usage: /tool <id> [JSON argument array]");
        let args = if arguments.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(arguments)?
        };
        (tool, args)
    } else {
        (first, rest.split_whitespace().map(str::to_owned).collect())
    };
    if first == "/tool" || ctx.tools.get(tool).is_some() {
        let intent = Intent::Command(CommandRequest {
            tool_id: tool.into(),
            args,
            goal: Some(goal.into()),
            interactive: Some(false),
        });
        let report = jean_core::intent::execute(ctx, &intent, prompt)?;
        if let Payload::Command(run) = &report.payload {
            crate::print_run_report(&mut io::stdout(), run)?;
        } else {
            println!("result: {}\n{}", report.state, report.verdict.detail());
        }
        return Ok(ExitCode::from(report.exit_status() as u8));
    }

    let (requested_worker, goal) = starting_worker(goal)?;
    let explicit_delegate = goal == "/delegate" || goal.starts_with("/delegate ");
    let goal = if explicit_delegate {
        split_first(goal).1
    } else {
        goal
    };
    anyhow::ensure!(!goal.is_empty(), "enter the goal for the worker");
    anyhow::ensure!(
        !goal.starts_with('/'),
        "unknown task command; use /desktop, /tool, /delegate, /resume, /review, /apply, or /discard"
    );
    if explicit_delegate
        || requested_worker.is_some()
        || options.worker.is_some()
        || options.profile.is_some()
        || options.instruction.is_some()
    {
        let worker = requested_worker.or(options.worker.as_deref());
        let worker = worker
            .map(|name| configured_worker_name(ctx, name))
            .transpose()?;
        return crate::phase3::delegate_with_prompt(
            ctx,
            crate::phase3::DelegateOptions {
                goal: goal.into(),
                workspace: options.workspace,
                worker,
                profile: options.profile,
                instruction: options.instruction,
                json: false,
            },
            prompt,
        );
    }
    if let Some(workspace) = options.workspace {
        let workspace = workspace.canonicalize()?;
        anyhow::ensure!(
            workspace.is_dir(),
            "the selected workspace is not a directory"
        );
        ctx.config.workspace.root = workspace.display().to_string();
    }
    if let Some(task) = crate::desktop_goal::opening_request(goal) {
        return crate::desktop_goal::run(ctx, task, prompt, cancel);
    }
    let result = crate::phase2::do_with_prompt(
        ctx,
        crate::phase2::DoOptions {
            goal: goal.into(),
            yes: false,
            prompt_each: true,
        },
        prompt,
        cancel,
    );
    if result.as_ref().err().is_some_and(|error| {
        matches!(
            error.downcast_ref::<jean_core::CoreError>(),
            Some(jean_core::CoreError::PlanRejected { .. })
        )
    }) {
        println!("next: For an open app, choose More → Work in an app. For a code change, select a workspace and use /delegate followed by the goal.");
    }
    result
}

fn split_first(text: &str) -> (&str, &str) {
    text.split_once(char::is_whitespace)
        .map_or((text, ""), |(first, rest)| (first, rest.trim_start()))
}

fn starting_worker(goal: &str) -> anyhow::Result<(Option<&str>, &str)> {
    const PREFIX: &str = "start with ";
    let Some(rest) = goal
        .get(PREFIX.len()..)
        .filter(|_| goal[..PREFIX.len()].eq_ignore_ascii_case(PREFIX))
    else {
        return Ok((None, goal));
    };
    let Some((separator, _)) = rest.char_indices().find(|&(index, character)| {
        matches!(character, '.' | ',' | ':') && rest[index + 1..].starts_with(char::is_whitespace)
    }) else {
        anyhow::bail!("use ‘Start with <worker>. <goal>’");
    };
    let worker = rest[..separator].trim();
    anyhow::ensure!(!worker.is_empty(), "enter a configured worker name");
    Ok((Some(worker), rest[separator + 1..].trim()))
}

fn configured_worker_name(ctx: &Context, name: &str) -> anyhow::Result<String> {
    let workers = &ctx.config.delegation.workers;
    if workers.iter().any(|worker| worker.name == name) {
        return Ok(name.to_owned());
    }
    let mut matches = workers
        .iter()
        .filter(|worker| worker.name.eq_ignore_ascii_case(name));
    let Some(worker) = matches.next() else {
        return Ok(name.to_owned());
    };
    anyhow::ensure!(
        matches.next().is_none(),
        "worker name {name:?} is ambiguous; use the exact configured name"
    );
    Ok(worker.name.clone())
}
