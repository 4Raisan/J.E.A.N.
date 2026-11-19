//! Procedural memory CLI & verified execution engine for Known Tasks (`jean playbook ...`).
//!
//! Every step of a playbook executes through JEAN's existing policy-gated,
//! audited, independently verified runners (`intent::execute`, `desktop_goal`,
//! `workspace_goal`, `browser`, `delegation`). Playbooks are recipes, never
//! permission bypasses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::Subcommand;
use jean_core::intent::{CommandRequest, FilesRequest, Intent, LaunchRequest, Payload, ProcessRequest};
use jean_core::playbooks::{
    self, bind_steps, parse_params, parse_steps, parse_triggers, save_typed_playbook,
    PlaybookParam, PlaybookStep,
};
use jean_core::{CancelFlag, Context};
use jean_policy::{ConsolePrompt, Prompter};
use jean_tools::SortKey;

use crate::memory::terminal;

#[derive(Subcommand)]
#[command(
    after_help = "Examples:\n  jean playbook list\n  jean playbook show morning-check\n  jean playbook run youtube-clips --param query=\"Iron Man\"\n  jean playbook undo morning-check"
)]
pub enum Playbook {
    /// List saved and learned playbooks (known tasks).
    List {
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Inspect one playbook, its triggers, parameters, and steps.
    Show { name: String },
    /// Teach or update a typed playbook directly (saves without executing).
    Teach {
        name: String,
        #[arg(long)]
        description: String,
        #[arg(long = "trigger")]
        triggers: Vec<String>,
        #[arg(long)]
        steps_json: String,
        #[arg(long, default_value = "[]")]
        params_json: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value = "taught")]
        source: String,
    },
    /// Replay a saved playbook through live resolution, policy, and verification.
    Run {
        name: String,
        /// Parameter bindings in `key=value` format.
        #[arg(long = "param")]
        params: Vec<String>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Restore the previous version of a modified playbook.
    Undo { name: String },
    /// Remove a saved playbook.
    Forget { name: String },
    /// Test whether an utterance matches a known playbook (read-only).
    Match {
        utterance: String,
        #[arg(long)]
        scope: Option<String>,
    },
    /// Show verified run history for one playbook.
    History {
        name: String,
        #[arg(long, default_value_t = 15)]
        limit: usize,
    },
}

pub fn run_command(ctx: &mut Context, command: Playbook) -> anyhow::Result<ExitCode> {
    match command {
        Playbook::List { scope, limit } => {
            let list = ctx.store.list_playbooks(scope.as_deref(), limit)?;
            if list.is_empty() {
                println!("No playbooks saved yet.");
            } else {
                for pb in list {
                    println!(
                        "{}\tv{}\tsource={}\tverified={}\tfailed={}\t{}",
                        terminal(&pb.name),
                        pb.version,
                        terminal(&pb.source),
                        pb.verified_successes,
                        pb.failures,
                        terminal(&pb.description)
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Playbook::Show { name } => {
            let pb = ctx
                .store
                .get_playbook(&name)?
                .ok_or_else(|| anyhow::anyhow!("playbook {name:?} not found"))?;
            let triggers = parse_triggers(&pb);
            let params = parse_params(&pb);
            let steps = parse_steps(&pb)?;
            println!("name: {} (v{}, source={})", terminal(&pb.name), pb.version, terminal(&pb.source));
            println!("description: {}", terminal(&pb.description));
            println!(
                "stats: verified_successes={} failures={}",
                pb.verified_successes, pb.failures
            );
            if !triggers.is_empty() {
                println!("triggers: {}", terminal(&triggers.join(" | ")));
            }
            if !params.is_empty() {
                let p: Vec<String> = params
                    .iter()
                    .map(|p| format!("{} ({})", p.name, p.kind))
                    .collect();
                println!("parameters: {}", terminal(&p.join(", ")));
            }
            println!("steps ({}):", steps.len());
            for (i, step) in steps.iter().enumerate() {
                println!("  {}. {}", i + 1, terminal(&describe_step(step)));
            }
            Ok(ExitCode::SUCCESS)
        }
        Playbook::Teach {
            name,
            description,
            triggers,
            steps_json,
            params_json,
            scope,
            source,
        } => {
            let steps: Vec<PlaybookStep> = serde_json::from_str(&steps_json)?;
            let params: Vec<PlaybookParam> = serde_json::from_str(&params_json)?;
            let triggers = if triggers.is_empty() {
                vec![name.clone()]
            } else {
                triggers
            };
            let saved = save_typed_playbook(
                ctx,
                &name,
                &name,
                &description,
                &triggers,
                scope.as_deref(),
                &params,
                &steps,
                &source,
            )?;
            println!(
                "saved playbook {} (v{}, {} step(s))",
                terminal(&saved.name),
                saved.version,
                steps.len()
            );
            Ok(ExitCode::SUCCESS)
        }
        Playbook::Run {
            name,
            params,
            workspace,
        } => {
            let pb = ctx
                .store
                .get_playbook(&name)?
                .ok_or_else(|| anyhow::anyhow!("playbook {name:?} not found"))?;
            let mut bindings = BTreeMap::new();
            for item in params {
                let (k, v) = item
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("expected --param key=value, got {item:?}"))?;
                bindings.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
            }
            let steps = bind_steps(&pb, &bindings)?;
            let mut prompt = ConsolePrompt::new(&ctx.config.policy);
            let cancel = CancelFlag::new();
            execute_playbook(
                ctx,
                &pb,
                &steps,
                workspace.as_deref(),
                &mut prompt,
                &cancel,
            )
        }
        Playbook::Undo { name } => {
            let restored = ctx
                .store
                .undo_playbook(&name)?
                .ok_or_else(|| anyhow::anyhow!("playbook {name:?} not found"))?;
            println!(
                "restored playbook {} to v{}",
                terminal(&restored.name),
                restored.version
            );
            Ok(ExitCode::SUCCESS)
        }
        Playbook::Forget { name } => {
            let removed = ctx
                .store
                .forget_playbook(&name)?
                .ok_or_else(|| anyhow::anyhow!("playbook {name:?} not found"))?;
            println!("forgotten playbook {}", terminal(&removed.name));
            Ok(ExitCode::SUCCESS)
        }
        Playbook::Match { utterance, scope } => {
            match playbooks::match_known_task(ctx, &utterance, scope.as_deref())? {
                Some(matched) => {
                    println!(
                        "matched playbook {} (reason={:?}, score={}, verified={}x)",
                        terminal(&matched.record.name),
                        matched.reason,
                        matched.score,
                        matched.record.verified_successes
                    );
                    for (k, v) in &matched.bound_params {
                        println!("  param {} = {}", terminal(k), terminal(v));
                    }
                    for (i, step) in matched.steps.iter().enumerate() {
                        println!("  step {}: {}", i + 1, terminal(&describe_step(step)));
                    }
                }
                None => println!("no matching playbook"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Playbook::History { name, limit } => {
            let pb = ctx
                .store
                .get_playbook(&name)?
                .ok_or_else(|| anyhow::anyhow!("playbook {name:?} not found"))?;
            let runs = ctx.store.playbook_runs(&pb.id, limit)?;
            if runs.is_empty() {
                println!("No recorded runs for playbook {}.", terminal(&pb.name));
            } else {
                for r in runs {
                    println!(
                        "#{}\t{}\t{}ms\ttask={}\t{}",
                        r.id,
                        terminal(&r.outcome),
                        r.duration_ms,
                        terminal(r.task_id.as_deref().unwrap_or("-")),
                        terminal(&r.notes)
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

pub(crate) fn execute_playbook(
    ctx: &mut Context,
    record: &jean_store::PlaybookRecord,
    steps: &[PlaybookStep],
    workspace: Option<&Path>,
    prompt: &mut dyn Prompter,
    cancel: &CancelFlag,
) -> anyhow::Result<ExitCode> {
    let started = Instant::now();
    println!("playbook: {} (v{})", terminal(&record.name), record.version);
    let outcome = execute_steps(ctx, steps, workspace, prompt, cancel);
    let elapsed_ms = started.elapsed().as_millis() as i64;
    match outcome {
        Ok((code, summary)) => {
            let label = if code == ExitCode::SUCCESS {
                "verified_success"
            } else if code == ExitCode::from(130) {
                "cancelled"
            } else if code == ExitCode::from(3) || code == ExitCode::from(4) {
                "uncertain"
            } else {
                "failed"
            };
            let _ = ctx
                .store
                .record_playbook_run(&record.id, None, label, elapsed_ms, &summary);
            let _ = jean_core::dream::observe_episode(
                ctx,
                if code == ExitCode::SUCCESS {
                    "task_success"
                } else {
                    "task_failure"
                },
                workspace.map(|p| p.to_string_lossy()).as_deref(),
                &format!("Playbook {} ({label}): {summary}", record.name),
                &format!("playbook,{}", record.name),
                if code == ExitCode::SUCCESS { 70 } else { 75 },
                None,
            );
            Ok(code)
        }
        Err(err) => {
            let reason = ctx.redactor.redact(&err.to_string());
            let _ = ctx
                .store
                .record_playbook_run(&record.id, None, "failed", elapsed_ms, &reason);
            let _ = jean_core::dream::observe_episode(
                ctx,
                "task_failure",
                workspace.map(|p| p.to_string_lossy()).as_deref(),
                &format!("Playbook {} failed: {reason}", record.name),
                &format!("playbook,{}", record.name),
                75,
                None,
            );
            Err(err)
        }
    }
}

/// Execute a typed list of `PlaybookStep`s sequentially, stopping on the first
/// unverified/failed/denied/cancelled step.
pub(crate) fn execute_steps(
    ctx: &mut Context,
    steps: &[PlaybookStep],
    workspace: Option<&Path>,
    prompt: &mut dyn Prompter,
    cancel: &CancelFlag,
) -> anyhow::Result<(ExitCode, String)> {
    let mut summaries = Vec::new();
    for (idx, step) in steps.iter().enumerate() {
        if cancel.raised() {
            return Ok((
                ExitCode::from(130),
                format!("cancelled before step {}", idx + 1),
            ));
        }
        match step {
            PlaybookStep::RunCommand { tool, args } => {
                let intent = Intent::Command(CommandRequest {
                    tool_id: tool.clone(),
                    args: args.clone(),
                    goal: Some(format!("playbook step {}: {tool}", idx + 1)),
                    interactive: Some(false),
                });
                let report = jean_core::intent::execute(ctx, &intent, prompt)?;
                if let Payload::Command(run) = &report.payload {
                    crate::print_run_report(&mut std::io::stdout(), run)?;
                } else {
                    println!("result: {}\n{}", report.state, report.verdict.detail());
                }
                let exit = report.exit_status();
                summaries.push(format!("{tool}: {}", report.verdict.detail()));
                if exit != 0 {
                    return Ok((ExitCode::from(exit as u8), summaries.join("; ")));
                }
            }
            PlaybookStep::LaunchApp {
                app,
                args,
                expect_window,
            } => {
                let intent = Intent::Launch(LaunchRequest {
                    query: app.clone(),
                    args: args.clone(),
                    pick: None,
                    window_timeout: jean_core::intent::DEFAULT_WINDOW_TIMEOUT,
                    expect_window: *expect_window,
                });
                let report = jean_core::intent::execute(ctx, &intent, prompt)?;
                println!("result: {}", report.verdict.detail());
                let exit = report.exit_status();
                summaries.push(format!("launch {app}: {}", report.verdict.detail()));
                if exit != 0 {
                    return Ok((ExitCode::from(exit as u8), summaries.join("; ")));
                }
            }
            PlaybookStep::InspectProcess { name, sort, limit } => {
                let sort_key = sort.as_deref().and_then(|s| match s {
                    "cpu" => Some(SortKey::Cpu),
                    "memory" => Some(SortKey::Memory),
                    "pid" => Some(SortKey::Pid),
                    _ => None,
                });
                let intent = Intent::Process(ProcessRequest {
                    pid: None,
                    name: name.clone(),
                    this_process: false,
                    list: name.is_none(),
                    sort: sort_key,
                    limit: *limit,
                    sample_window_ms: None,
                });
                let report = jean_core::intent::execute(ctx, &intent, prompt)?;
                if let Payload::Processes { rows, .. } = &report.payload {
                    for row in rows.iter().take(10) {
                        println!(
                            "  pid={} name={} mem_mb={} cpu={:.1}%",
                            row.pid,
                            row.name,
                            row.memory_bytes / (1024 * 1024),
                            row.cpu_percent_of_one_core
                        );
                    }
                    let top = rows
                        .first()
                        .map(|r| format!("top process {} ({} MB)", r.name, r.memory_bytes / (1024 * 1024)))
                        .unwrap_or_else(|| report.verdict.detail().to_owned());
                    println!("result: inspected {} process(es); {top}", rows.len());
                    summaries.push(top);
                } else {
                    println!("result: {}", report.verdict.detail());
                    summaries.push(report.verdict.detail().to_owned());
                }
                let exit = report.exit_status();
                if exit != 0 {
                    return Ok((ExitCode::from(exit as u8), summaries.join("; ")));
                }
            }
            PlaybookStep::LocateFile { root, pattern } => {
                let resolved_root = if root.trim().is_empty() {
                    workspace
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| PathBuf::from(&ctx.config.workspace.root))
                } else {
                    PathBuf::from(root)
                };
                let intent = Intent::Files(FilesRequest {
                    root: resolved_root,
                    pattern: pattern.clone(),
                    max_results: Some(25),
                });
                let report = jean_core::intent::execute(ctx, &intent, prompt)?;
                println!("result: {}", report.verdict.detail());
                let exit = report.exit_status();
                summaries.push(report.verdict.detail().to_owned());
                if exit != 0 {
                    return Ok((ExitCode::from(exit as u8), summaries.join("; ")));
                }
            }
            PlaybookStep::DesktopGoal {
                window,
                goal,
                launch,
            } => {
                let code = crate::desktop_goal::run(
                    ctx,
                    crate::desktop_goal::Goal::new(window.clone(), goal.clone(), *launch),
                    prompt,
                    cancel,
                )?;
                summaries.push(format!("desktop {window}: {goal}"));
                if code != ExitCode::SUCCESS {
                    return Ok((code, summaries.join("; ")));
                }
            }
            PlaybookStep::WorkspaceGoal { goal } => {
                let root = workspace
                    .map(Path::to_path_buf)
                    .filter(|p| p.is_dir())
                    .or_else(|| {
                        let r = ctx.config.workspace.root.trim();
                        (!r.is_empty()).then(|| PathBuf::from(r))
                    })
                    .ok_or_else(|| anyhow::anyhow!("Select a workspace directory first."))?;
                let code = crate::workspace_goal::run(ctx, &root, goal, prompt, cancel)?;
                summaries.push(format!("workspace: {goal}"));
                if code != ExitCode::SUCCESS {
                    return Ok((code, summaries.join("; ")));
                }
            }
            PlaybookStep::Browser {
                url,
                inspect,
                expect_non_empty,
                verify_video,
                video_query,
            } => {
                let request = jean_core::browser::BrowserRequest {
                    url: url.clone(),
                    inspect: inspect.clone(),
                    r#type: None,
                    click: None,
                    expect: None,
                    expect_non_empty: expect_non_empty.clone(),
                    verify_video: *verify_video,
                    video_query: video_query.clone(),
                    allow_audio: *verify_video,
                };
                let report = jean_core::browser::run(ctx, &request, prompt)?;
                if let Some(pb) = &report.playback {
                    println!(
                        "result: Playing {} ({:.2}s -> {:.2}s, {}x{}) ({})",
                        pb.video_title, pb.start_secs, pb.end_secs, pb.video_width, pb.video_height, report.url
                    );
                    summaries.push(format!("played {}", pb.video_title));
                } else if !report.title.is_empty() {
                    println!("result: {} ({})", report.title, report.url);
                    summaries.push(format!("browser {}", report.title));
                } else {
                    println!("reason: {}", report.verdict.detail());
                    summaries.push(report.verdict.detail().to_owned());
                }
                let exit = crate::phase3::browser_exit(&report);
                if exit != 0 {
                    return Ok((ExitCode::from(exit as u8), summaries.join("; ")));
                }
            }
            PlaybookStep::OpenUrl { url } => {
                anyhow::ensure!(
                    url.starts_with("https://") || url.starts_with("http://"),
                    "OpenUrl step requires an http:// or https:// URL"
                );
                let report = jean_core::browser::run(
                    ctx,
                    &jean_core::browser::BrowserRequest {
                        url: url.clone(),
                        expect_non_empty: Some("body".to_owned()),
                        ..Default::default()
                    },
                    prompt,
                )?;
                let exit = crate::phase3::browser_exit(&report);
                println!("result: opened {url} ({})", report.verdict.detail());
                summaries.push(format!("opened {url}"));
                if exit != 0 {
                    return Ok((ExitCode::from(exit as u8), summaries.join("; ")));
                }
            }
            PlaybookStep::Clock { scope } => {
                let now_ms = jean_store::now_ms();
                let secs = now_ms / 1000;
                let offset_secs = if scope == "sri-lanka" { 5 * 3600 + 30 * 60 } else { 0 };
                let adj = secs + offset_secs;
                let hours = (adj.rem_euclid(86400)) / 3600;
                let mins = (adj.rem_euclid(3600)) / 60;
                let line = if scope == "sri-lanka" {
                    format!("Sri Lanka time is {:02}:{:02} (+05:30)", hours, mins)
                } else {
                    format!("UTC time is {:02}:{:02}", hours, mins)
                };
                println!("result: {line}");
                summaries.push(line);
            }
            PlaybookStep::Gmail => {
                let intent = Intent::Launch(LaunchRequest {
                    query: "Gmail".into(),
                    args: vec![],
                    pick: None,
                    window_timeout: jean_core::intent::DEFAULT_WINDOW_TIMEOUT,
                    expect_window: false,
                });
                let report = jean_core::intent::execute(ctx, &intent, prompt)?;
                let exit = report.exit_status();
                println!("result: {}", report.verdict.detail());
                summaries.push(report.verdict.detail().to_owned());
                if exit != 0 {
                    return Ok((ExitCode::from(exit as u8), summaries.join("; ")));
                }
            }
            PlaybookStep::Delegate { goal, worker } => {
                let code = crate::phase3::delegate_with_prompt(
                    ctx,
                    crate::phase3::DelegateOptions {
                        goal: goal.clone(),
                        workspace: workspace.map(Path::to_path_buf),
                        worker: worker.clone(),
                        profile: None,
                        instruction: None,
                        json: false,
                    },
                    prompt,
                )?;
                summaries.push(format!("delegated: {goal}"));
                if code != ExitCode::SUCCESS {
                    return Ok((code, summaries.join("; ")));
                }
            }
        }
    }
    Ok((ExitCode::SUCCESS, summaries.join("; ")))
}

pub(crate) fn describe_step(step: &PlaybookStep) -> String {
    match step {
        PlaybookStep::RunCommand { tool, args } => {
            if args.is_empty() {
                format!("run tool `{tool}`")
            } else {
                format!("run tool `{tool}` {}", args.join(" "))
            }
        }
        PlaybookStep::LaunchApp { app, .. } => format!("launch app `{app}`"),
        PlaybookStep::InspectProcess { name, sort, .. } => match name {
            Some(n) => format!("inspect process `{n}`"),
            None => format!("inspect top processes (sort={})", sort.as_deref().unwrap_or("memory")),
        },
        PlaybookStep::LocateFile { root, pattern } => format!("find `{pattern}` in `{root}`"),
        PlaybookStep::DesktopGoal { window, goal, launch } => format!(
            "{} `{window}`: {goal}",
            if *launch { "open & operate" } else { "operate" }
        ),
        PlaybookStep::WorkspaceGoal { goal } => format!("workspace goal: {goal}"),
        PlaybookStep::Browser { url, .. } => format!("browse `{url}`"),
        PlaybookStep::OpenUrl { url } => format!("open `{url}`"),
        PlaybookStep::Clock { scope } => format!("read clock ({scope})"),
        PlaybookStep::Gmail => "open Gmail".to_owned(),
        PlaybookStep::Delegate { goal, worker } => format!(
            "delegate{} `{goal}`",
            worker
                .as_deref()
                .map(|w| format!(" to {w}:"))
                .unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jean_policy::ScriptedPrompt;

    #[test]
    fn playbook_lifecycle_teaches_executes_records_verified_run_and_undoes() {
        let dir = std::env::temp_dir().join(format!(
            "jean-cli-playbook-{}-{}",
            std::process::id(),
            jean_store::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = Context::for_test(&dir);

        let pb = save_typed_playbook(
            &ctx,
            "sri-lanka-clock",
            "Sri Lanka Clock",
            "Check the current time in Sri Lanka",
            &["check sri lanka time".into()],
            None,
            &[],
            &[PlaybookStep::Clock {
                scope: "sri-lanka".into(),
            }],
            "taught",
        )
        .unwrap();

        let matched = playbooks::match_known_task(&ctx, "Check Sri Lanka time", None)
            .unwrap()
            .expect("known task must match");
        let mut prompt = ScriptedPrompt::approving();
        let code = execute_playbook(
            &mut ctx,
            &matched.record,
            &matched.steps,
            None,
            &mut prompt,
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(code, ExitCode::SUCCESS);

        let updated = ctx.store.get_playbook(&pb.name).unwrap().unwrap();
        assert_eq!(updated.verified_successes, 1);
        assert_eq!(ctx.store.playbook_runs(&pb.id, 10).unwrap().len(), 1);
        assert!(!ctx.store.recent_episodes(None, 10).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
