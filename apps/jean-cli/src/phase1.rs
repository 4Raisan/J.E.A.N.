//! The Phase-1 command surface: `apps`, `launch`, `exec`, `ps`, `find`, `health`,
//! `approvals`, `notify`, `status`.
//!
//! ## The one rule that shapes every function here
//!
//! Nothing in this module starts a process. The commands that act — `jean launch`,
//! `jean exec`, and the two read-only-but-recorded intents `jean ps` and `jean find` — all
//! reach [`jean_core::intent::execute`], which asks [`jean_policy`] and writes the audit
//! line. There is therefore no code path from `main` to a launcher that skips the
//! permission engine, and no flag in this file that can widen one.
//!
//! ## Read-only means read-only
//!
//! `apps`, `health`, `approvals` and `status` never call [`Context::prepare_for_writing`],
//! never create a task row and never write the audit file. `ps` and `find` *do* create
//! tasks, and that is deliberate: they are intents with verifiable outcomes, and an
//! outcome nobody recorded cannot be audited later. What none of them do is write outside
//! the configured data directory.
//!
//! `jean apps` calls [`jean_tools::discover_installed_apps`] rather than
//! [`jean_core::intent::discover`], because the intent-layer wrapper takes a
//! [`jean_core::intent::LaunchRequest`] — a launch request, not a question. It is the same
//! discovery function either way, and discovery resolves names without ever running what
//! it finds.
//!
//! ## What `--yes` does not do
//!
//! It is accepted, documented, and reported to the operator as reserved for Phase 2. It
//! does not answer an approval prompt: D-011 says a grant comes from a person or from the
//! configuration, and an auto-yes in the CLI would be neither.
//!
//! ## The voice
//!
//! Phase 0's, unchanged: aligned label/value lines in the same twelve-column shape
//! `jean run` uses (see [`field`]), no colour, no emoji, and every number carrying its
//! unit. Where the engine has no answer, the line says so instead of printing an empty
//! cell.

use std::fmt::Display;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use jean_core::health::{HealthOutcome, ProbeAnswer};
use jean_core::intent::{
    render_candidates, CommandRequest, FilesRequest, Intent, IntentReport, LaunchEvidence,
    LaunchRequest, Payload, ProcessRequest, DEFAULT_WINDOW_TIMEOUT,
};
use jean_core::notify::{self, Delivered};
use jean_core::Context;
use jean_policy::ConsolePrompt;
use jean_store::format_ms;
use jean_tools::SortKey;

use crate::print_run_report;

// ─────────────────────────────────────────────────────────────────────────────
// The column
// ─────────────────────────────────────────────────────────────────────────────

/// One label/value line, Phase 0's column: the label padded to eleven characters, then
/// one space, then the value.
///
/// `jean run` hand-writes that padding. Here the width is in one place, because these
/// commands have labels of every length — `key` and `directories` — and a hand-counted
/// space in each string literal is a column that drifts the first time a label changes.
/// Labels are kept at eleven characters or fewer for that reason; a longer one would not
/// panic, it would simply push its value two columns right.
pub(crate) fn field(out: &mut dyn Write, label: &str, value: impl Display) -> anyhow::Result<()> {
    writeln!(out, "{label:<11} {value}")?;
    Ok(())
}

/// The line under a [`field`], with no label of its own: twelve spaces, exactly as
/// `jean run` indents the second line of its verdict.
pub(crate) fn note(out: &mut dyn Write, text: impl Display) -> anyhow::Result<()> {
    writeln!(out, "            {text}")?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// The one path to a side effect
// ─────────────────────────────────────────────────────────────────────────────

/// What a command prints before the engine is reached, and what it prints after the
/// decision line.
///
/// A `&mut dyn FnMut`, not a function pointer, because `jean launch` reports the candidate
/// it resolved and `jean ps` echoes the selection it was given — both need to carry data
/// from the parsed arguments.
fn describe_nothing(_: &mut dyn Write) -> anyhow::Result<()> {
    Ok(())
}

/// Decide, act, verify, print. The single path from a CLI flag to a side effect.
fn run_intent(
    ctx: &mut Context,
    intent: &Intent,
    yes: bool,
    before: &mut dyn FnMut(&mut dyn Write) -> anyhow::Result<()>,
    after: &mut dyn FnMut(&mut dyn Write) -> anyhow::Result<()>,
    print_payload: fn(&mut dyn Write, &IntentReport) -> anyhow::Result<()>,
) -> anyhow::Result<ExitCode> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    before(&mut out)?;
    print_yes_flag(&mut out, yes)?;

    // The engine's own prompter: the CLI never answers a question it was not asked.
    let mut prompter = ConsolePrompt::new(&ctx.config.policy);
    let report = jean_core::intent::execute(ctx, intent, &mut prompter)?;

    // The command intent delegates the whole supervised run to `jean_core::run`, and its
    // payload printer is Phase 0's own body — which opens with the task id and closes with
    // the state, verdict, notes and result lines. Printing them around it as well would
    // say the same thing twice on one screen.
    let body_owns_the_run = matches!(&report.payload, Payload::Command(_));

    print_intent_header(&mut out, &report, body_owns_the_run)?;
    after(&mut out)?;
    print_payload(&mut out, &report)?;
    if !body_owns_the_run {
        print_intent_tail(&mut out, &report)?;
    }

    drop(out);
    report_denial(&report);
    Ok(exit_of(&report))
}

/// The lines every intent command opens with.
///
/// `task` is skipped when the payload body already prints it (see [`run_intent`]), and not
/// printed at all when the engine created no row: a denied command intent is a complete
/// result with no task behind it, and an empty id would read as a record nobody can look
/// up.
fn print_intent_header(
    out: &mut dyn Write,
    report: &IntentReport,
    body_owns_the_run: bool,
) -> anyhow::Result<()> {
    field(out, "intent", report.intent.as_str())?;
    if !report.task_id.is_empty() && !body_owns_the_run {
        field(out, "task", &report.task_id)?;
    }
    field(out, "decision", &report.decision)?;
    Ok(())
}

/// State, verdict, notes, and the one line that separates "ran" from "proven".
///
/// The two `result` sentences are Phase 0's, word for word, because a script that sees
/// exit status 3 needs the same explanation here that `jean run` gives it.
fn print_intent_tail(out: &mut dyn Write, report: &IntentReport) -> anyhow::Result<()> {
    field(out, "state", report.state)?;
    field(out, "verdict", report.verdict.label())?;
    note(out, report.verdict.detail())?;

    // On the denied path the same text reaches stderr from `report_denial`, verbatim;
    // printing it twice would make one refusal look like two facts.
    if report.denial().is_none() && !report.notes.is_empty() {
        writeln!(out, "notes")?;
        for note in &report.notes {
            writeln!(out, "  - {note}")?;
        }
    }

    if report.is_success() {
        writeln!(out)?;
        field(out, "result", "completed and verified")?;
    } else if report.denial().is_some() {
        writeln!(out)?;
        field(out, "result", "refused by policy; nothing was attempted")?;
    } else {
        writeln!(out)?;
        field(
            out,
            "result",
            "command completed; the requested outcome is NOT proven",
        )?;
    }
    Ok(())
}

/// The denial, on stderr, exactly as `DenyReason` words it.
///
/// No paraphrase: this is the sentence that tells the operator whether to change a
/// configuration file, answer a prompt, or accept that the act was refused on purpose.
fn report_denial(report: &IntentReport) {
    if let Some(reason) = report.denial() {
        eprintln!("jean: {reason}");
    }
}

/// The engine's exit status, widened to the type `main` hands back.
fn exit_of(report: &IntentReport) -> ExitCode {
    // `IntentReport::exit_status` returns 0, 3, 4, 5, 130 or 1; all fit in a `u8`. The
    // `unwrap_or` guards against a future code, and is not a path an argument can reach.
    ExitCode::from(u8::try_from(report.exit_status()).unwrap_or(1))
}

/// What `--yes` is, printed whenever it is passed.
fn print_yes_flag(out: &mut dyn Write, yes: bool) -> anyhow::Result<()> {
    if yes {
        field(
            out,
            "--yes",
            "accepted and not acted on: it is reserved for Phase 2, when a plan can carry \
             an approval.",
        )?;
        note(
            out,
            "It does not bypass `policy.require_approval`. In this build the question still \
             goes to `ConsolePrompt`, and a prompt nobody answers is a denial (D-011).",
        )?;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// apps — discovery, and nothing else
// ─────────────────────────────────────────────────────────────────────────────

/// `jean apps <query>` — what would `jean launch <query>` offer?
///
/// Read-only: no task row, no audit line, no `prepare_for_writing`.
pub fn cmd_apps(ctx: &Context, query: String) -> anyhow::Result<ExitCode> {
    let outcome = jean_tools::discover_installed_apps(query.trim(), &ctx.config.tools.discovery)?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let launchable = outcome
        .candidates
        .iter()
        .filter(|candidate| candidate.is_launchable())
        .count();

    field(&mut out, "query", &outcome.query)?;
    field(
        &mut out,
        "sources",
        if outcome.sources_used.is_empty() {
            "none consulted — every entry in `tools.discovery` was refused or unknown".to_owned()
        } else {
            outcome
                .sources_used
                .iter()
                .map(|source| source.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        },
    )?;
    field(
        &mut out,
        "candidates",
        format!(
            "{} found, {} launchable",
            outcome.candidates.len(),
            launchable
        ),
    )?;
    field(
        &mut out,
        "ambiguous",
        if outcome.ambiguous {
            "yes — `jean launch` refuses to choose between them; pass --pick"
        } else {
            "no"
        },
    )?;
    field(
        &mut out,
        "bound",
        if outcome.sources_used.is_empty() {
            "none — nothing was consulted, so nothing was bounded".to_owned()
        } else {
            format!(
                "{}{}",
                outcome.bound.as_str(),
                if outcome.bound.was_hit() {
                    " — the search stopped at this bound, so the list may be incomplete"
                } else {
                    " — every consulted source was exhausted"
                }
            )
        },
    )?;
    field(
        &mut out,
        "directories",
        format!("{} opened", outcome.directories_scanned),
    )?;
    field(
        &mut out,
        "entries",
        format!("{} directory entries examined", outcome.entries_examined),
    )?;
    field(&mut out, "elapsed", format!("{} ms", outcome.elapsed_ms))?;
    field(
        &mut out,
        "unreadable",
        format!(
            "{} registry subtree(s) not readable by this account",
            outcome.inaccessible_keys.len()
        ),
    )?;
    for (key, code) in &outcome.inaccessible_keys {
        note(&mut out, format!("{key}: win32 error {code}"))?;
    }
    if outcome.notes.is_empty() {
        field(&mut out, "notes", "none")?;
    } else {
        field(
            &mut out,
            "notes",
            format!(
                "{} (listed under `matches`, in the engine's own words)",
                outcome.notes.len()
            ),
        )?;
    }

    writeln!(out)?;
    field(
        &mut out,
        "matches",
        format!(
            "the list `jean launch {} --pick N` chooses from",
            outcome.query
        ),
    )?;
    write!(out, "{}", render_candidates(&outcome))?;

    writeln!(
        out,
        "\nthis command started nothing: discovery resolves names and never runs them"
    )?;
    if outcome.candidates.is_empty() {
        writeln!(
            out,
            "nothing matched in the sources listed above, which is not the same claim as \
             \"not installed\""
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

// ─────────────────────────────────────────────────────────────────────────────
// launch
// ─────────────────────────────────────────────────────────────────────────────

pub struct LaunchOptions {
    pub app: String,
    pub args: Vec<String>,
    pub pick: Option<usize>,
    pub window_timeout_secs: u64,
    pub no_window: bool,
    pub yes: bool,
}

/// `jean launch <app> [args…]` — start it, then prove that a window the operator can see
/// belongs to the process that was started.
pub fn cmd_launch(ctx: &mut Context, options: LaunchOptions) -> anyhow::Result<ExitCode> {
    let LaunchOptions {
        app,
        args,
        pick,
        window_timeout_secs,
        no_window,
        yes,
    } = options;

    let intent = Intent::Launch(LaunchRequest {
        query: app.clone(),
        args,
        pick,
        // The engine clamps this up to its own one-second floor: a budget of zero is a
        // question that cannot be answered, and "no window" out of it would be a lie.
        window_timeout: Duration::from_secs(window_timeout_secs),
        expect_window: !no_window,
    });

    // Describe the candidate the operator is about to get *before* the act, by re-running
    // the same read-only discovery the engine runs. The configured sources are cloned so
    // this report holds no borrow across the mutable call below.
    let sources = ctx.config.tools.discovery.clone();
    let mut before =
        |out: &mut dyn Write| -> anyhow::Result<()> { print_resolution(out, &sources, &app, pick) };

    run_intent(
        ctx,
        &intent,
        yes,
        &mut before,
        &mut describe_nothing,
        print_launch_payload,
    )
}

/// The candidate and its provenance, from a fresh read-only discovery pass.
fn print_resolution(
    out: &mut dyn Write,
    sources: &[String],
    query: &str,
    pick: Option<usize>,
) -> anyhow::Result<()> {
    let Ok(outcome) = jean_tools::discover_installed_apps(query, sources) else {
        return Ok(());
    };
    let candidate = match pick {
        None => outcome.sole_launchable(),
        Some(index) => index
            .checked_sub(1)
            .and_then(|position| outcome.candidates.get(position))
            .filter(|candidate| candidate.is_launchable()),
    };
    let Some(candidate) = candidate else {
        // Ambiguous or unresolvable: say which, and let the engine's own refusal print the
        // whole candidate list.
        field(
            out,
            "resolved",
            format!("(no single launchable candidate for `{query}`)"),
        )?;
        note(
            out,
            format!(
                "`jean apps {query}` shows what was found; `jean launch {query} --pick N` \
                 chooses one of the numbers it prints"
            ),
        )?;
        return Ok(());
    };

    field(
        out,
        "resolved",
        format!(
            "{} [{}] — a second read-only discovery pass, for reporting; the engine's own \
             answer is the task record below",
            candidate.display_name,
            candidate.match_kind.as_str()
        ),
    )?;
    match &candidate.exe_path {
        Some(path) => note(out, format!("executable {}", path.display()))?,
        None => note(out, "executable (none resolved)")?,
    }
    for provenance in &candidate.sources {
        note(
            out,
            format!(
                "seen at {} ({})",
                provenance.seen_at,
                provenance.source.as_str()
            ),
        )?;
    }
    if let Some(location) = &candidate.install_location {
        note(out, format!("install location {}", location.display()))?;
    }
    Ok(())
}

fn print_launch_payload(out: &mut dyn Write, report: &IntentReport) -> anyhow::Result<()> {
    match &report.payload {
        Payload::Launched(evidence) => print_launch_evidence(out, evidence),
        // Denied, or refused before anything ran.
        Payload::Nothing => field(out, "processes", "none started"),
        other => field(out, "payload", format!("(not a launch payload: {other:?})")),
    }
}

fn print_launch_evidence(out: &mut dyn Write, evidence: &LaunchEvidence) -> anyhow::Result<()> {
    match evidence {
        LaunchEvidence::WindowOpened {
            pid,
            created_ms,
            windows,
            waited_ms,
        } => {
            field(out, "launched", format!("pid {pid}"))?;
            print_identity(out, *created_ms)?;
            field(
                out,
                "waited",
                format!("{waited_ms} ms for the window check"),
            )?;
            print_window_report(out, windows)?;
        }
        LaunchEvidence::HandedOff {
            pid,
            existing,
            windows,
            note: explanation,
        } => {
            field(out, "launched", format!("pid {pid}"))?;
            field(
                out,
                "handoff",
                format!(
                    "that process is gone; {} process(es) with the same image name are \
                     running",
                    existing.len()
                ),
            )?;
            for process in existing {
                note(out, format!("pid {} `{}`", process.pid, process.name))?;
            }
            for window_report in windows {
                print_window_report(out, window_report)?;
            }
            note(out, explanation)?;
        }
        LaunchEvidence::NoWindowInTime {
            pid,
            created_ms,
            waited_ms,
            reason,
        } => {
            field(out, "launched", format!("pid {pid}"))?;
            print_identity(out, *created_ms)?;
            field(out, "waited", format!("{waited_ms} ms"))?;
            field(out, "reason", reason)?;
        }
        LaunchEvidence::ExitedWithoutWindow { pid, exit_claim } => {
            field(out, "launched", format!("pid {pid}"))?;
            field(out, "process", "the pid you started has exited")?;
            note(out, exit_claim)?;
        }
        LaunchEvidence::NotChecked(why) => {
            field(out, "verified", format!("not attempted: {why}"))?;
            note(
                out,
                "no evidence was gathered, so nothing here can say the application opened",
            )?;
        }
        LaunchEvidence::Refused { candidates, reason } => {
            field(out, "refused", reason)?;
            write!(out, "{}", render_candidates(candidates))?;
        }
    }
    Ok(())
}

/// The process identity, or the honest absence of one.
///
/// `created_ms` is what makes "pid 1234 is alive" a real answer, because Windows recycles
/// pids; when it was not recorded, the pid alone proves nothing and is labelled as such.
fn print_identity(out: &mut dyn Write, created_ms: Option<i64>) -> anyhow::Result<()> {
    match created_ms {
        Some(ms) => field(out, "created_ms", format!("{ms} ({})", format_ms(ms))),
        None => field(out, "created_ms", "(identity not recorded)"),
    }
}

fn print_window_report(
    out: &mut dyn Write,
    report: &jean_tools::WindowReport,
) -> anyhow::Result<()> {
    field(
        out,
        "windows",
        format!(
            "{} matched, {} top-level window(s) examined on the station",
            report.windows_matched, report.windows_examined
        ),
    )?;
    for window in &report.windows {
        let state = if window.is_minimised {
            "minimised"
        } else if window.is_visible {
            "visible"
        } else {
            "not visible"
        };
        note(out, format!("pid {} [{state}]", window.pid))?;
        note(out, format!("title: {}", window.title))?;
        if window.title_truncated {
            note(
                out,
                "the title was cut at the probe's character bound, so this is not the whole \
                 title",
            )?;
        }
    }
    if report.truncated {
        note(
            out,
            format!(
                "the row bound of {} window(s) cut this list; more matched",
                report.max_rows
            ),
        )?;
    }
    if report.walk_capped {
        note(
            out,
            "the walk stopped at the probe's own bound, so a window may exist that was never \
             looked at",
        )?;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// exec
// ─────────────────────────────────────────────────────────────────────────────

pub struct ExecOptions {
    pub tool: String,
    pub args: Vec<String>,
    pub goal: Option<String>,
    pub interactive: bool,
    pub yes: bool,
}

/// `jean exec <tool> [args…]` — Phase 0's supervised run, reached through the intent layer
/// so that the permission engine and the audit line are on the way there.
pub fn cmd_exec(ctx: &mut Context, options: ExecOptions) -> anyhow::Result<ExitCode> {
    let ExecOptions {
        tool,
        args,
        goal,
        interactive,
        yes,
    } = options;

    let intent = Intent::Command(CommandRequest {
        tool_id: tool,
        args,
        goal,
        // Only an explicit `--interactive` asks for the pseudo console. Without the flag
        // the registry decides, exactly as it does for `jean run`.
        interactive: if interactive { Some(true) } else { None },
    });

    run_intent(
        ctx,
        &intent,
        yes,
        &mut describe_nothing,
        &mut describe_nothing,
        print_exec_payload,
    )
}

fn print_exec_payload(out: &mut dyn Write, report: &IntentReport) -> anyhow::Result<()> {
    if let Payload::Command(run) = &report.payload {
        // Anything the intent layer noticed that the run itself did not say — the health
        // note is the usual one — goes first, so the Phase-0 body stays byte-identical.
        for extra in report.notes.iter().filter(|note| !run.notes.contains(note)) {
            field(out, "engine note", extra)?;
        }
        print_run_report(out, run)?;
        return Ok(());
    }

    field(out, "child", "never started")?;
    if matches!(report.payload, Payload::Nothing) {
        note(
            out,
            "the decision refused the act, so nothing was supervised and no child was \
             created for it",
        )?;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// ps
// ─────────────────────────────────────────────────────────────────────────────

pub struct PsOptions {
    pub pid: Option<u32>,
    pub name: Option<String>,
    pub this_process: bool,
    pub list: bool,
    pub sort: Option<SortKey>,
    pub limit: Option<usize>,
    pub sample_ms: Option<u64>,
    pub yes: bool,
}

/// `jean ps` — read the process table. Nothing is started, stopped or signalled: there is
/// no code path from this command to a termination call, in this file or in `jean-tools`.
pub fn cmd_ps(ctx: &mut Context, options: PsOptions) -> anyhow::Result<ExitCode> {
    let PsOptions {
        pid,
        name,
        this_process,
        list,
        sort,
        limit,
        sample_ms,
        yes,
    } = options;

    let has_target = pid.is_some() || name.is_some() || this_process;
    let intent = Intent::Process(ProcessRequest {
        pid,
        name,
        this_process,
        // No target at all is the bounded listing, which is what a bare `jean ps` means.
        list: list || !has_target,
        sort,
        limit,
        sample_window_ms: sample_ms,
    });

    // The selection, echoed with its units and with the defaults named, so a second run
    // can be compared against the first on purpose rather than by accident.
    let requested_sort = sort.unwrap_or(SortKey::Cpu);
    let requested_limit = limit.unwrap_or(jean_tools::process::DEFAULT_MAX_ROWS);
    let requested_window = sample_ms.unwrap_or(jean_tools::process::DEFAULT_SAMPLE_WINDOW_MS);
    let target = if has_target {
        "named process(es), not a listing"
    } else {
        "the bounded top-N listing"
    };
    let mut before = move |out: &mut dyn Write| -> anyhow::Result<()> {
        field(out, "request", target)?;
        note(
            out,
            format!(
                "sorted by {} (the default is {}), at most {} row(s), sampled over {} ms",
                requested_sort.as_str(),
                SortKey::Cpu.as_str(),
                requested_limit,
                requested_window
            ),
        )?;
        if limit.is_none() {
            note(
                out,
                format!(
                    "{} rows is the probe's own default (`DEFAULT_MAX_ROWS`); raise it with \
                     --limit",
                    requested_limit
                ),
            )?;
        }
        if sample_ms.is_none() {
            note(
                out,
                format!(
                    "{} ms is `DEFAULT_SAMPLE_WINDOW_MS`; two runs are comparable only if \
                     the window matches",
                    requested_window
                ),
            )?;
        }
        Ok(())
    };

    run_intent(
        ctx,
        &intent,
        yes,
        &mut before,
        &mut describe_nothing,
        print_ps_payload,
    )
}

fn print_ps_payload(out: &mut dyn Write, report: &IntentReport) -> anyhow::Result<()> {
    let Payload::Processes {
        machine,
        rows,
        truncated,
        sample_window_ms,
        gone,
    } = &report.payload
    else {
        field(out, "processes", "none reported")?;
        return Ok(());
    };

    field(
        out,
        "sampled",
        format!(
            "over {sample_window_ms} ms — CPU percentages are of ONE core across that window, \
             and can exceed 100"
        ),
    )?;
    field(out, "rows", format!("{} returned", rows.len()))?;
    if *truncated {
        field(
            out,
            "truncated",
            "yes — the row bound cut the listing; raise it with --limit",
        )?;
    }

    match machine {
        Some(summary) => {
            field(
                out,
                "mem total",
                format!(
                    "{:.2} MiB ({} bytes)",
                    as_mib(summary.total_memory_bytes),
                    summary.total_memory_bytes
                ),
            )?;
            field(
                out,
                "mem used",
                format!(
                    "{:.2} MiB ({} bytes)",
                    as_mib(summary.used_memory_bytes),
                    summary.used_memory_bytes
                ),
            )?;
            field(
                out,
                "cores",
                format!(
                    "{} physical, {} logical",
                    summary.physical_core_count, summary.logical_core_count
                ),
            )?;
            field(out, "uptime", format!("{} s", summary.uptime_secs))?;
            field(
                out,
                "processes",
                format!(
                    "{} visible to this account at sample time",
                    summary.process_count
                ),
            )?;
            if !summary.unavailable_fields.is_empty() {
                note(
                    out,
                    format!(
                        "field(s) the OS refused for the machine summary: {}",
                        summary.unavailable_fields.join(", ")
                    ),
                )?;
            }
        }
        None => field(
            out,
            "machine",
            "(no summary: this request inspected named processes, not the whole table)",
        )?,
    }

    if let Some(pid) = gone {
        // Not an error. A process that is gone is a fact about the machine, and short-lived
        // processes are the ordinary case on a working desktop. The verdict below says which
        // kind of gone this is — never there, or there when the sample began — so this line
        // does not guess.
        writeln!(out)?;
        field(
            out,
            "process",
            format!("pid {pid} is gone; there is no report to print for it"),
        )?;
    }

    if rows.is_empty() {
        field(
            out,
            "rows",
            "none — the verdict below says what was looked for",
        )?;
    } else {
        writeln!(out)?;
        // The column names are what the numbers actually are, checked against
        // `Get-Process` rather than assumed from the sampler's field names: on Windows
        // `sysinfo`'s `virtual_memory` is the process's *private* (page-file backed)
        // usage — for a `cmd.exe` sleeping in a loop it reported 6.54 MiB where
        // `Get-Process VirtualMemorySize64` reported ~2 TiB of address space. Labelling
        // that column "VIRT" would be a claim about the virtual address space that the
        // number does not support.
        writeln!(
            out,
            "{:<9} {:<26} {:>10} {:>11} {:>13} {:>9}",
            "PID", "NAME", "MEM(MiB)", "PRIV(MiB)", "CPU%(1core)", "RUN(s)"
        )?;
        for row in rows {
            writeln!(
                out,
                "{:<9} {:<26} {:>10.2} {:>11.2} {:>13.2} {:>9}",
                row.pid,
                row.name,
                as_mib(row.memory_bytes),
                as_mib(row.virtual_memory_bytes),
                row.cpu_percent_of_one_core,
                row.run_time_secs
            )?;
            // A refused field is named, not blanked: an empty cell reads as "zero", and
            // "zero" and "the OS would not say" are opposite answers.
            if !row.unavailable_fields.is_empty() {
                writeln!(
                    out,
                    "            field(s) the OS refused for pid {}: {} — the cells above \
                     carry the sampler's own value for those fields, not a measurement",
                    row.pid,
                    row.unavailable_fields.join(", ")
                )?;
            }
            if row.sample_window_ms != *sample_window_ms {
                writeln!(
                    out,
                    "            pid {} was sampled over {} ms, not the {sample_window_ms} ms \
                     stated above",
                    row.pid, row.sample_window_ms
                )?;
            }
        }
        writeln!(out)?;
        field(
            out,
            "columns",
            "MEM = working set; PRIV = private (page-file backed) usage as the sampler \
             reports it on Windows — not the virtual address space, which for a sleeping \
             `cmd.exe` is around 2 TiB and says nothing about cost; CPU% is of ONE core \
             over the stated window and can exceed 100 on a multi-core machine; RUN is the \
             process's wall-clock age in seconds",
        )?;
    }

    Ok(())
}

/// Bytes to MiB, for a cell whose header names the unit.
fn as_mib(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

// ─────────────────────────────────────────────────────────────────────────────
// find
// ─────────────────────────────────────────────────────────────────────────────

pub struct FindOptions {
    pub root: PathBuf,
    pub pattern: String,
    pub max_results: Option<usize>,
    pub yes: bool,
}

/// `jean find <root> <pattern>` — locate names under one explicit root.
///
/// The root is required rather than defaulted: the choice of root is this intent's privacy
/// boundary, and picking one on the operator's behalf would be the decision their
/// configuration is supposed to own.
pub fn cmd_find(ctx: &mut Context, options: FindOptions) -> anyhow::Result<ExitCode> {
    let FindOptions {
        root,
        pattern,
        max_results,
        yes,
    } = options;

    let configured = ctx.config.search.max_results as usize;
    // `--max-results` is a further cut, never a widening: `search.max_results` is the
    // ceiling the operator agreed to, and the walk applies it whatever this number says.
    let requested = max_results.unwrap_or(configured);
    let capped = requested.min(configured);
    let mut before = move |out: &mut dyn Write| -> anyhow::Result<()> {
        if requested > configured {
            field(
                out,
                "max results",
                format!(
                    "--max-results {requested} is above `search.max_results`, which is \
                     {configured} rows; the search is capped at {configured} rows"
                ),
            )?;
        }
        Ok(())
    };

    let intent = Intent::Files(FilesRequest {
        root,
        pattern,
        max_results: Some(capped),
    });

    run_intent(
        ctx,
        &intent,
        yes,
        &mut before,
        &mut describe_nothing,
        print_find_payload,
    )
}

fn print_find_payload(out: &mut dyn Write, report: &IntentReport) -> anyhow::Result<()> {
    let Payload::Files(found) = &report.payload else {
        field(out, "paths", "none reported")?;
        return Ok(());
    };

    field(out, "root", &found.display_root)?;
    field(out, "canonical", found.canonical_root.display())?;
    field(out, "pattern", &found.pattern)?;
    field(out, "hits", format!("{} reported", found.hits.len()))?;

    for hit in &found.hits {
        let size = match hit.size_bytes {
            Some(bytes) => format!("{bytes} bytes"),
            None => "(size refused by the OS)".to_owned(),
        };
        let modified = match hit.modified_ms {
            Some(ms) => format_ms(ms),
            None => "(modified not recorded)".to_owned(),
        };
        writeln!(
            out,
            "  {} [{}] {} {}",
            hit.path.display(),
            if hit.is_dir { "dir" } else { "file" },
            size,
            modified
        )?;
    }

    writeln!(out)?;
    field(
        out,
        "stop",
        format!(
            "{}{}",
            found.stop.as_str(),
            if found.stop.is_complete() {
                " — the authorised root was walked to its bounds, so this list is complete"
            } else {
                " — a bound ended the walk, so this list is NOT exhaustive"
            }
        ),
    )?;
    field(
        out,
        "entries",
        format!("{} directory entries examined", found.entries_examined),
    )?;
    field(out, "elapsed", format!("{} ms", found.elapsed_ms))?;
    field(
        out,
        "vanished",
        format!(
            "{} hit(s) disappeared between being found and being reported",
            found.vanished
        ),
    )?;
    field(
        out,
        "hidden",
        format!(
            "{} entry name(s) withheld, because a hidden entry is not searched unless \
             `search.include_hidden` is true",
            found.hidden_withheld
        ),
    )?;
    field(
        out,
        "reparse",
        format!(
            "{} reparse point(s) seen and not followed",
            found.reparse_skipped.len()
        ),
    )?;
    for skipped in &found.reparse_skipped {
        note(out, skipped.path.display())?;
    }
    field(
        out,
        "access",
        format!(
            "{} location(s) could not be read{}",
            found.access_errors.len(),
            if found.access_errors_truncated {
                ", and further failures were folded into that count"
            } else {
                ""
            }
        ),
    )?;
    for failure in &found.access_errors {
        note(
            out,
            format!("{}: {}", failure.path.display(), failure.reason),
        )?;
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// health
// ─────────────────────────────────────────────────────────────────────────────

/// `jean health [--now]` — is each registered tool still where the registry says it is?
///
/// Read-only for the CLI: no task row, no audit line, no `prepare_for_writing`. The probe
/// itself records health rows, which is this command's entire subject.
pub fn cmd_health(ctx: &Context, now: bool) -> anyhow::Result<ExitCode> {
    let outcomes = jean_core::health::probe_all(ctx, now)?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    field(&mut out, "tools", format!("{} registered", outcomes.len()))?;
    field(
        &mut out,
        "forced",
        if now {
            "yes — every tool was looked up again, whatever the record said"
        } else {
            "no — an answer newer than `tools.health_check_interval_secs` was read back"
        },
    )?;
    field(
        &mut out,
        "interval",
        format!("{} s", ctx.config.tools.health_check_interval_secs),
    )?;
    field(
        &mut out,
        "threshold",
        format!(
            "unhealthy after {} consecutive failed probe(s)",
            ctx.config.tools.mark_unhealthy_after
        ),
    )?;

    if outcomes.is_empty() {
        writeln!(
            out,
            "\nno tool is registered, so there was nothing to look up; see \
             config/tools.example.toml"
        )?;
        return Ok(ExitCode::SUCCESS);
    }

    writeln!(out)?;
    #[allow(clippy::write_literal)]
    writeln!(
        out,
        "{:<24} {:<11} {:>12} {:<26} {}",
        "TOOL", "STATUS", "FAILURES", "LAST CHECK", "DETAIL"
    )?;
    for outcome in &outcomes {
        let last_check = last_check_ms(ctx, outcome)?;
        writeln!(
            out,
            "{:<24} {:<11} {:>12} {:<26} {}",
            outcome.tool_id,
            outcome.status.as_str(),
            format!("{} failure(s)", outcome.consecutive_failures),
            last_check,
            describe_answer(outcome),
        )?;
    }

    let blocked: Vec<String> = outcomes
        .iter()
        .filter(|outcome| !outcome.runnable())
        .map(|outcome| format!("{} ({})", outcome.tool_id, outcome.status.as_str()))
        .collect();
    if !blocked.is_empty() {
        writeln!(out, "\nnot offered as runnable: {}", blocked.join(", "))?;
        writeln!(
            out,
            "`unknown` counts as runnable: nothing has been looked at, which is not evidence \
             that a tool is broken"
        )?;
    }

    writeln!(
        out,
        "\na probe is a path lookup, not a run: `jean health` never starts the tool it \
         checks, because starting it would be a side effect wearing a diagnostic label"
    )?;
    Ok(ExitCode::SUCCESS)
}

/// The last check time, from the row the probe returned or the row it just wrote.
///
/// [`ProbeAnswer`] carries a record only when the answer was read from the cache, so for a
/// fresh lookup the same row is re-read — a read of the database the probe wrote.
fn last_check_ms(ctx: &Context, outcome: &HealthOutcome) -> anyhow::Result<String> {
    let recorded = match &outcome.answer {
        ProbeAnswer::Cached(record) => record.last_check_ms,
        _ => ctx
            .store
            .tool_health(&outcome.tool_id)?
            .and_then(|record| record.last_check_ms),
    };
    Ok(match recorded {
        Some(ms) => format_ms(ms),
        None => "(never recorded)".to_owned(),
    })
}

/// What the answer actually was, including whether anything was looked at.
///
/// For a fresh lookup the row's `detail` is the same sentence the probe built, so printing
/// both would repeat the path twice on one line; it is printed on its own there, and
/// combined with a head only where the head says something the row cannot — that nothing
/// was looked at this time, or that the tool is switched off.
fn describe_answer(outcome: &HealthOutcome) -> String {
    match &outcome.answer {
        ProbeAnswer::Present(_) | ProbeAnswer::Absent(_) => {
            outcome
                .detail
                .clone()
                .unwrap_or_else(|| match &outcome.answer {
                    ProbeAnswer::Present(path) => format!("resolved to {}", path.display()),
                    ProbeAnswer::Absent(reason) => format!("could not be resolved: {reason}"),
                    ProbeAnswer::Disabled(reason) => reason.clone(),
                    ProbeAnswer::Cached(_) => "(no detail recorded)".to_owned(),
                })
        }
        ProbeAnswer::Disabled(reason) => match &outcome.detail {
            Some(detail) => format!("{reason}; last recorded detail: {detail}"),
            None => reason.clone(),
        },
        ProbeAnswer::Cached(_) => match &outcome.detail {
            Some(detail) => {
                format!("read from the database, not looked up again; {detail}")
            }
            None => "read from the database, not looked up again".to_owned(),
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// approvals
// ─────────────────────────────────────────────────────────────────────────────

pub enum ApprovalAction {
    List,
    Forget { id: i64 },
    Clear,
}

/// `jean approvals [list|forget ID|clear]` — the ledger of remembered yeses.
///
/// The only writes are deletes from the approvals table: no task row, no audit line, and
/// no directory prepared.
pub fn cmd_approvals(ctx: &Context, action: ApprovalAction) -> anyhow::Result<ExitCode> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    match action {
        ApprovalAction::List => {
            let approvals = ctx.store.list_approvals()?;
            let now = jean_store::now_ms();
            field(
                &mut out,
                "approvals",
                format!("{} remembered", approvals.len()),
            )?;
            field(
                &mut out,
                "window",
                format!(
                    "a grant is valid for {} s from the moment [a] was answered \
                     (`policy.approval_remember_secs`)",
                    ctx.config.policy.approval_remember_secs
                ),
            )?;

            if approvals.is_empty() {
                writeln!(
                    out,
                    "\nnothing is remembered, so every approval in this ledger was answered \
                     one operation at a time"
                )?;
                return Ok(ExitCode::SUCCESS);
            }

            writeln!(out)?;
            #[allow(clippy::write_literal)]
            writeln!(
                out,
                "{:<5} {:<20} {:<20} {:<9} {:>10} {:<26} {:<26} {}",
                "ID", "OPERATION", "TOOL", "TIER", "USED", "GRANTED", "EXPIRES", "SUMMARY"
            )?;
            let mut expired = 0usize;
            for approval in &approvals {
                let is_expired = approval.is_expired(now);
                if is_expired {
                    expired += 1;
                }
                writeln!(
                    out,
                    "{:<5} {:<20} {:<20} {:<9} {:>10} {:<26} {:<26} {}{}",
                    approval.id,
                    approval.operation,
                    if approval.tool_id.is_empty() {
                        "-"
                    } else {
                        approval.tool_id.as_str()
                    },
                    approval.tier,
                    format!("{} time(s)", approval.used_count),
                    format_ms(approval.granted_at_ms),
                    format_ms(approval.expires_at_ms),
                    approval.summary,
                    if is_expired { "  (EXPIRED)" } else { "" }
                )?;
                // The digest is what a remembered grant is matched on, so an operator
                // reviewing the ledger can see that it is not the summary text.
                writeln!(out, "      scope digest {}", approval.scope_digest)?;
            }
            writeln!(
                out,
                "\n{} approval(s) listed, {} of which are already expired and authorise \
                 nothing until the store prunes them",
                approvals.len(),
                expired
            )?;
            writeln!(
                out,
                "`jean approvals forget <id>` removes one; a grant covers exactly the \
                 operation and the arguments shown above, nothing nearby"
            )?;
        }
        ApprovalAction::Forget { id } => {
            let removed = ctx.store.forget_approval(id)?;
            if removed {
                field(&mut out, "approval", format!("#{id} was forgotten"))?;
                note(
                    &mut out,
                    "the next matching operation is decided on its own merits: a prompt, or \
                     the command you typed",
                )?;
            } else {
                // Stated plainly and not as an error: the state the caller wanted — that
                // row gone — is already true.
                writeln!(out, "no approval with id {id} exists; nothing was removed")?;
            }
        }
        ApprovalAction::Clear => {
            let approvals = ctx.store.list_approvals()?;
            let total = approvals.len();
            let mut removed = 0usize;
            for approval in &approvals {
                if ctx.store.forget_approval(approval.id)? {
                    removed += 1;
                }
            }
            field(
                &mut out,
                "approvals",
                format!("{removed} of {total} row(s) removed"),
            )?;
            if removed != total {
                note(
                    &mut out,
                    "the difference is a row that went between the listing and the delete; \
                     nothing else was touched",
                )?;
            }
            if total == 0 {
                writeln!(out, "there was nothing to forget")?;
            } else {
                writeln!(
                    out,
                    "every remembered yes is gone: later operations are decided by the \
                     configuration and by whoever is at the terminal"
                )?;
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

// ─────────────────────────────────────────────────────────────────────────────
// notify
// ─────────────────────────────────────────────────────────────────────────────

pub struct NotifyOptions {
    pub register: bool,
    pub send: Option<(String, String)>,
}

/// `jean notify <status|register> [--send TITLE BODY]`.
///
/// Not an intent: no process is started and no task row is created, so there is no outcome
/// to verify. `register` writes one `DisplayName` value under `HKCU`, which is what the
/// roadmap's "AUMID registered" deliverable is, and the account it returns is printed
/// verbatim so the operator can see exactly which key changed.
pub fn cmd_notify(ctx: &Context, options: NotifyOptions) -> anyhow::Result<ExitCode> {
    let NotifyOptions { register, send } = options;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let aumid = ctx.config.ui.app_user_model_id.clone();
    let mode_is_none = ctx.config.ui.notifications == "none";
    field(&mut out, "identity", &aumid)?;
    field(
        &mut out,
        "key",
        format!("HKCU\\Software\\Classes\\AppUserModelId\\{aumid}"),
    )?;
    field(&mut out, "mode", &ctx.config.ui.notifications)?;
    field(
        &mut out,
        "registered",
        if notify::is_registered(&aumid) {
            "yes — a readable DisplayName value is present at that key"
        } else {
            "no — no readable DisplayName value is present at that key"
        },
    )?;
    field(
        &mut out,
        "deliver",
        if mode_is_none {
            "\"none\": a send performs no registry read and no WinRT call at all"
        } else {
            "\"toast\": the raw WinRT path is configured"
        },
    )?;

    if register {
        writeln!(out)?;
        // The identity is its own display name: there is no configuration key for a
        // separate label, and inventing one would be inventing a setting.
        let account = notify::register(&aumid, &aumid)
            .map_err(|e| anyhow::anyhow!("could not register the notification identity: {e}"))?;
        writeln!(out, "{account}")?;
        field(
            &mut out,
            "registered",
            if notify::is_registered(&aumid) {
                "yes — confirmed by reading the value back"
            } else {
                "still no — the write was reported and the read disagrees"
            },
        )?;
        if mode_is_none {
            note(
                &mut out,
                "the identity is registered while `ui.notifications` is still \"none\", so \
                 nothing will be shown until that is set to \"toast\"",
            )?;
        }
    }

    if let Some((title, body)) = send {
        writeln!(out)?;
        field(
            &mut out,
            "sending",
            "a display-only toast: no click action and no activation argument",
        )?;
        let delivered = notify::deliver(&ctx.config.ui, &title, &body)
            .map_err(|e| anyhow::anyhow!("could not deliver the notification: {e}"))?;
        match delivered {
            Delivered::Disabled => {
                field(&mut out, "delivered", "Disabled — nothing was attempted")?;
                note(
                    &mut out,
                    "`ui.notifications` is \"none\"; set it to \"toast\" and run `jean notify \
                     register` if this is meant to appear",
                )?;
            }
            Delivered::Shown => {
                field(&mut out, "delivered", "Shown")?;
                // Load-bearing: `Shown` is the strongest claim a display-only path can
                // make, and it is not a claim about the operator's screen.
                note(
                    &mut out,
                    "`Shown` means the WinRT API accepted the toast, not that it appeared on \
                     your screen.",
                )?;
                note(
                    &mut out,
                    "for an executable that is not packaged as MSIX the OS may accept a toast \
                     and render nothing, and this build does not read the desktop back, so it \
                     cannot tell the two apart",
                )?;
            }
            Delivered::Failed(reason) => {
                field(&mut out, "delivered", format!("Failed — {reason}"))?;
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

// ─────────────────────────────────────────────────────────────────────────────
// status
// ─────────────────────────────────────────────────────────────────────────────

/// `jean status [<id>]` — the compact view of what the engine recorded.
///
/// Read-only: the store is queried and nothing else happens. A live [`IntentReport`] only
/// exists inside the invocation that produced it, so this reads the rows that report was
/// written into — which is why the authority line says what the database does not keep.
pub fn cmd_status(ctx: &Context, id: Option<String>) -> anyhow::Result<ExitCode> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    match id {
        None => print_status_table(&mut out, ctx)?,
        Some(id) => {
            jean_protocol::ipc::validate_task_id(&id)?;
            print_status_one(&mut out, ctx, &id)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_status_table(out: &mut dyn Write, ctx: &Context) -> anyhow::Result<()> {
    let tasks = ctx.store.list_tasks(jean_store::TaskFilter::All, 50)?;
    if tasks.is_empty() {
        writeln!(out, "no tasks recorded")?;
        writeln!(
            out,
            "nothing has run in this database: {}",
            ctx.paths.database.display()
        )?;
        return Ok(());
    }

    field(out, "database", ctx.paths.database.display())?;
    writeln!(out)?;
    #[allow(clippy::write_literal)]
    writeln!(
        out,
        "{:<26} {:<16} {:<22} {:<26} {}",
        "ID", "STATE", "INTENT/TOOL", "CREATED", "VERDICT"
    )?;
    for task in &tasks {
        let intent = request_field(&task.request_json, "intent");
        // For the three intents with no registry entry behind them the engine records the
        // intent name in `tasks.tool_id`, because that column is `NOT NULL`. Printing
        // `inspect_process/inspect_process` would be two copies of one fact.
        let subject = match (&task.tool_id, intent) {
            (Some(tool), Some(kind)) if tool == &kind => kind,
            (Some(tool), Some(kind)) => format!("{kind}/{tool}"),
            (Some(tool), None) => tool.clone(),
            (None, Some(kind)) => kind,
            (None, None) => "-".to_owned(),
        };
        writeln!(
            out,
            "{:<26} {:<16} {:<22} {:<26} {}",
            task.id,
            task.state.as_str(),
            subject,
            format_ms(task.created_at_ms),
            task.outcome.as_deref().unwrap_or("(not recorded)")
        )?;
    }
    writeln!(out, "\n{} task(s)", tasks.len())?;
    writeln!(
        out,
        "`jean status <id>` for the compact record of one; `jean task <id>` for everything \
         the store holds"
    )?;
    Ok(())
}

fn print_status_one(out: &mut dyn Write, ctx: &Context, id: &str) -> anyhow::Result<()> {
    let task = ctx.store.require_task(id)?;
    let runs = ctx.store.tool_runs(id)?;
    let events = ctx.store.events(id)?;

    field(out, "task", &task.id)?;
    field(out, "goal", &task.goal)?;
    field(out, "state", task.state.as_str())?;
    let intent = request_field(&task.request_json, "intent");
    field(
        out,
        "intent",
        intent.clone().unwrap_or_else(|| {
            "run_command (the Phase-0 path: the stored request carries no `intent` field)"
                .to_owned()
        }),
    )?;
    // For the three intents with no registry entry behind them the engine keeps the intent
    // name in `tasks.tool_id`, because that column is `NOT NULL`. Saying so is better than
    // printing the same word twice on two lines and letting the operator work it out.
    let recorded_tool = task.tool_id.as_deref().unwrap_or("-");
    field(
        out,
        "tool",
        match &intent {
            Some(kind) if kind == recorded_tool => {
                format!(
                    "{recorded_tool} (the intent itself: no registry tool stands behind \
                        this row)"
                )
            }
            _ => recorded_tool.to_owned(),
        },
    )?;
    field(
        out,
        "target",
        request_field(&task.request_json, "target")
            .or_else(|| request_field(&task.request_json, "command"))
            .unwrap_or_else(|| "(not recorded)".to_owned()),
    )?;
    if let Some(capabilities) = request_array(&task.request_json, "capabilities") {
        field(out, "capability", capabilities)?;
    }
    field(out, "created", format_ms(task.created_at_ms))?;
    field(out, "updated", format_ms(task.updated_at_ms))?;
    if let Some(finished) = task.finished_at_ms {
        field(out, "finished", format_ms(finished))?;
    }

    // The decision, as far as the rows record it. `jean_core::intent` writes the denial
    // text into `outcome_detail`, so a refusal is readable here; what no column keeps is
    // which authority stood behind an allowed operation, and that is said rather than
    // guessed at.
    let denied = task.outcome.as_deref() == Some("denied");
    field(out, "decision", if denied { "denied" } else { "allowed" })?;
    if denied {
        field(out, "authority", "(none — the act was refused)")?;
    } else {
        let prompted = events.iter().any(|event| {
            event
                .detail
                .to_ascii_lowercase()
                .contains("awaiting_approval")
        });
        field(
            out,
            "authority",
            format!(
                "{} the database does not record which authority applied; the audit line at \
                 {} names it",
                if prompted {
                    "an approval prompt was raised for this task;"
                } else {
                    "the command you typed, or a remembered approval;"
                },
                ctx.paths.audit.display()
            ),
        )?;
    }

    field(
        out,
        "verdict",
        task.outcome.as_deref().unwrap_or("(not recorded)"),
    )?;
    if let Some(detail) = &task.outcome_detail {
        note(out, detail)?;
    }

    let notes: Vec<&str> = events
        .iter()
        .filter(|event| event.kind.as_str() == "note")
        .map(|event| event.detail.as_str())
        .collect();
    if notes.is_empty() {
        field(out, "notes", "none in the event log")?;
    } else {
        writeln!(out, "notes")?;
        for line in &notes {
            writeln!(out, "  - {line}")?;
        }
    }

    if runs.is_empty() {
        field(out, "child", "no tool run was recorded for this task")?;
    } else {
        field(out, "runs", runs.len())?;
        for run in &runs {
            note(out, format!("#{} {}", run.id, run.tool_id))?;
            match run.child_pid {
                Some(pid) => {
                    note(out, format!("child pid   {pid}"))?;
                }
                None => {
                    note(out, "child pid   (never started)")?;
                }
            }
            match run.exit_code {
                Some(code) => {
                    note(out, format!("exit code   {code}"))?;
                }
                None => {
                    note(out, "exit code   (none recorded)")?;
                }
            }
            match run.termination {
                Some(t) => {
                    note(out, format!("termination {t}"))?;
                }
                None => {
                    note(out, "termination (unfinished)")?;
                }
            }
        }
    }

    if task.state.is_in_flight() {
        writeln!(
            out,
            "\nthis task is still in flight; `jean cancel {}` asks it to stop",
            task.id
        )?;
    }
    Ok(())
}

/// One string field out of the persisted request record.
fn request_field(json: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()?
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

/// One array field, joined for printing.
fn request_array(json: &str, key: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
    let items = value.get(key)?.as_array()?;
    let joined = items
        .iter()
        .filter_map(|item| item.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Some(if joined.is_empty() {
        "(none declared)".to_owned()
    } else {
        joined
    })
}

/// The window timeout the CLI offers as its default, taken from the engine so the two can
/// never disagree.
pub const DEFAULT_WINDOW_TIMEOUT_SECS: u64 = DEFAULT_WINDOW_TIMEOUT.as_secs();
