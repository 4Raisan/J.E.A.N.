//! Phase 4 CLI: `jean desktop`.
//!
//! This is the surface the desktop actuator is judged from, and the order of the verbs is the design:
//! `windows` says what is there, `inspect` says what a window exposes, `diagnose` says *why* something is
//! missing, and only then does `act` touch anything. The first three run with `[desktop] enabled = false`
//! — a switch that also turned diagnosis off would remove the evidence an operator needs to decide whether
//! to turn the actions on.
//!
//! ```text
//! jean desktop windows                     what an operator would call a window, and who owns it
//! jean desktop inspect  <window>           the accessible controls, with the reason if there are none
//! jean desktop diagnose <window> | --all   what each of the seven operations can actually do here
//! jean desktop act      <window> <control> one pattern action, then the read-back
//! jean desktop input    <window>           one injected input, behind its own switch
//! jean desktop capture  <window> [--out]   pixels, once, judged by [desktop] retention
//! jean desktop status                      the bounds, the awareness, the integrity, the session
//! ```
//!
//! ## What a status code means here
//!
//! | Code | Meaning |
//! |---|---|
//! | 0 | The check the operator stated is what the desktop now shows |
//! | 1 | The desktop does not show it: the action was refused by the mechanism, or nothing changed — and, as in every other command, a configuration gate that is off |
//! | 3 | Something was done and nothing proves the goal: no `--expect` was stated to prove it by |
//! | 4 | The outcome is **uncertain** — the action may have landed, and this build will not send it again |
//! | 5 | Refused by policy, or by an answer at the prompt |
//!
//! Code 4 is the one this phase added to the table. A provider that stops answering after accepting a
//! press leaves a click whose delivery nobody can confirm, and a script that retries on failure would
//! retry it. An uncertain outcome is therefore not a failure code: it is the hand-off to a person.
//!
//! ## What `--yes` does not do
//!
//! It grants nothing. What removes a repeated prompt is answering *remember* at the prompt — which writes
//! the approvals ledger against the digest of the resolved window's process image, so it does not cover a
//! different application — or running the step inside a plan whose grant a person already agreed to.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use jean_core::desktop::{
    self, ActRequest, CaptureRequest, ControlSelector, DesktopAction as PatternAction,
    DesktopInput as Injected, DesktopReport, InputRequest, InputTarget, ModifierKey, MouseButton,
    ScrollAmount, VirtualKeyCode, WindowSelector,
};
use jean_core::Context;
use jean_policy::ConsolePrompt;

use crate::phase1::{field, note};
use crate::phase3::quote;

/// `jean desktop …`, after clap.
pub enum DesktopVerb {
    /// The bounds, awareness, integrity and session this process sees. Touches nothing.
    Status,
    Windows {
        limit: usize,
        json: bool,
    },
    Inspect {
        window: WindowSelector,
        json: bool,
    },
    Diagnose {
        window: WindowSelector,
        all: bool,
        limit: usize,
        json: bool,
    },
    Act {
        window: WindowSelector,
        control: ControlSelector,
        action: PatternAction,
        witness: Option<ControlSelector>,
        expect: Option<String>,
        capture: bool,
        json: bool,
    },
    Input {
        window: WindowSelector,
        at: InputTarget,
        input: Injected,
        witness: Option<ControlSelector>,
        expect: Option<String>,
        capture: bool,
        json: bool,
    },
    Capture {
        window: WindowSelector,
        out: Option<PathBuf>,
        json: bool,
    },
}

pub fn cmd_desktop(ctx: &mut Context, verb: DesktopVerb) -> anyhow::Result<ExitCode> {
    match verb {
        DesktopVerb::Status => cmd_status(&ctx.config.desktop),
        DesktopVerb::Windows { limit, json } => cmd_windows(limit, json),
        DesktopVerb::Inspect { window, json } => cmd_inspect(ctx, &window, json),
        DesktopVerb::Diagnose {
            window,
            all,
            limit,
            json,
        } => cmd_diagnose(ctx, &window, all, limit, json),
        DesktopVerb::Act {
            window,
            control,
            action,
            witness,
            expect,
            capture,
            json,
        } => {
            let request = ActRequest {
                window,
                control,
                action,
                witness,
                expect,
                capture,
            };
            let mut prompter = ConsolePrompt::new(&ctx.config.policy);
            let report = desktop::act(ctx, &request, &mut prompter)?;
            print_report(&report, json)
        }
        DesktopVerb::Input {
            window,
            at,
            input,
            witness,
            expect,
            capture,
            json,
        } => {
            let request = InputRequest {
                window,
                at,
                input,
                witness,
                expect,
                capture,
            };
            let mut prompter = ConsolePrompt::new(&ctx.config.policy);
            let report = desktop::send_input(ctx, &request, &mut prompter)?;
            print_report(&report, json)
        }
        DesktopVerb::Capture { window, out, json } => {
            let request = CaptureRequest { window, out };
            let mut prompter = ConsolePrompt::new(&ctx.config.policy);
            let report = desktop::capture_window(ctx, &request, &mut prompter)?;
            print_report(&report, json)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// status
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_status(config: &jean_protocol::config::DesktopConfig) -> anyhow::Result<ExitCode> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (label, value) in desktop::overview(config) {
        field(&mut out, &label, value.as_str())?;
    }
    writeln!(out)?;
    note(
        &mut out,
        "`enabled` gates the actions. `windows`, `inspect` and `diagnose` run either way, because a \
         window that cannot be worked on still has to be diagnosed.",
    )?;
    note(
        &mut out,
        "`allow_synthetic_input` is a separate switch from `enabled`: turning the actuator on is \
         agreeing to pattern actions, not to synthesised keystrokes.",
    )?;
    note(
        &mut out,
        "the DPI awareness and integrity lines describe *this* process. A Medium-integrity J.E.A.N. \
         cannot reach an elevated window, and Windows says so silently — which is why the check runs \
         before the input rather than after.",
    )?;
    Ok(ExitCode::SUCCESS)
}

// ─────────────────────────────────────────────────────────────────────────────
// windows / inspect / diagnose — the reads
// ─────────────────────────────────────────────────────────────────────────────

fn cmd_windows(limit: usize, json: bool) -> anyhow::Result<ExitCode> {
    let listing = desktop::windows(limit)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if json {
        // The title is in this output because the operator asked for the list. It is not in the audit
        // trail, which carries the bounded label instead, and it is not in the log.
        let rows = listing
            .windows
            .iter()
            .map(|window| {
                format!(
                    "{{\"hwnd\":{},\"pid\":{},\"process\":{},\"title\":{},\"class\":{},\"visible\":{},\
                     \"foreground\":{},\"dpi\":{},\"integrity\":{},\"affinity\":{},\"bounds\":{}}}",
                    quote(&format!("{:#x}", window.id.hwnd)),
                    window.pid,
                    quote(
                        window
                            .process_image
                            .as_deref()
                            .and_then(|path| path.rsplit(['\\', '/']).next())
                            .unwrap_or("unknown")
                    ),
                    quote(window.title.as_deref().unwrap_or("")),
                    quote(window.class_name.as_deref().unwrap_or("")),
                    window.is_visible,
                    window.is_foreground,
                    window
                        .dpi
                        .map(|dpi| dpi.to_string())
                        .unwrap_or_else(|| "null".to_owned()),
                    quote(
                        window
                            .integrity
                            .as_ref()
                            .map(|value| value.label.clone())
                            .unwrap_or_else(|| "unknown".to_owned())
                            .as_str()
                    ),
                    quote(&format!("{:?}", window.display_affinity)),
                    window
                        .rect
                        .map(|rect| {
                            format!(
                                "\"{}x{}+{}+{}\"",
                                rect.width(),
                                rect.height(),
                                rect.left,
                                rect.top
                            )
                        })
                        .unwrap_or_else(|| "null".to_owned())
                )
            })
            .collect::<Vec<_>>()
            .join(",\n    ");
        writeln!(
            out,
            "{{\n  \"awareness\": {},\n  \"session\": {},\n  \"total\": {},\n  \"shown\": {},\n  \
             \"windows\": [\n    {}\n  ]\n}}",
            quote(&format!("{:?}", listing.awareness)),
            quote(&format!("{:?}", listing.session.interactive())),
            listing.total,
            listing.windows.len(),
            rows
        )?;
        return Ok(ExitCode::SUCCESS);
    }

    field(&mut out, "total", listing.total)?;
    field(&mut out, "shown", listing.windows.len())?;
    if listing.total > listing.windows.len() {
        note(
            &mut out,
            "the list was cut by --limit; the count above is the whole desktop",
        )?;
    }
    field(&mut out, "awareness", format!("{:?}", listing.awareness))?;
    field(
        &mut out,
        "session",
        format!("{:?}", listing.session.interactive()),
    )?;
    writeln!(out)?;
    for window in &listing.windows {
        let process = window
            .process_image
            .as_deref()
            .and_then(|path| path.rsplit(['\\', '/']).next())
            .unwrap_or("unknown")
            .chars()
            .take(26)
            .collect::<String>();
        writeln!(
            out,
            "{:#x}  pid {:>7}  {:<26}  {:>6}  {:<10}  {:<22}  {}",
            window.id.hwnd,
            window.pid,
            process,
            window
                .dpi
                .map(|dpi| format!("{dpi}"))
                .unwrap_or_else(|| "-".to_owned()),
            window
                .integrity
                .as_ref()
                .map(|value| value.label.clone())
                .unwrap_or_else(|| "unknown".to_owned()),
            format!("{:?}", window.display_affinity),
            window
                .title
                .as_deref()
                .map(|title| {
                    let cut: String = title.chars().take(60).collect();
                    format!("{cut:?}")
                })
                .unwrap_or_default()
        )?;
        if let Some(reason) = &window.identity_note {
            note(&mut out, format!("  identity: {reason}"))?;
        }
        if let Some(monitor) = &window.monitor {
            note(
                &mut out,
                format!(
                    "  monitor: {}x{}+{}+{} at {}% {}",
                    monitor.width,
                    monitor.height,
                    monitor.x,
                    monitor.y,
                    monitor.scale_percent(),
                    if monitor.is_primary { "(primary)" } else { "" }
                ),
            )?;
        }
    }
    writeln!(out)?;
    note(
        &mut out,
        "`ExcludeFromCapture` is the window's own declaration about pixels. It does not mean the \
         window cannot be worked on, and `jean desktop diagnose` is what says whether it can.",
    )?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_inspect(
    ctx: &mut Context,
    selector: &WindowSelector,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let inspection = desktop::inspect(ctx, selector)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if json {
        let controls = inspection
            .tree
            .controls
            .iter()
            .map(|control| {
                format!(
                    "{{\"automation_id\":{},\"name\":{},\"kind\":{},\"enabled\":{},\"focus\":{},\
                     \"password\":{},\"patterns\":[{}],\"bounds\":{}}}",
                    quote(&control.automation_id),
                    quote(&control.name),
                    quote(&control.kind.name()),
                    control.is_enabled,
                    control.has_keyboard_focus,
                    control.is_password,
                    control
                        .patterns
                        .iter()
                        .map(|pattern| quote(pattern.name()))
                        .collect::<Vec<_>>()
                        .join(","),
                    control
                        .rect
                        .map(|rect| format!(
                            "\"{}x{}+{}+{}\"",
                            rect.width(),
                            rect.height(),
                            rect.left,
                            rect.top
                        ))
                        .unwrap_or_else(|| "null".to_owned())
                )
            })
            .collect::<Vec<_>>()
            .join(",\n    ");
        writeln!(
            out,
            "{{\n  \"window\": {},\n  \"explanation\": {},\n  \"truncated\": {},\n  \
             \"observed_at_ms\": {},\n  \"controls\": [\n    {}\n  ]\n}}",
            quote(&inspection.window.audit_label()),
            quote(&format!("{:?}", inspection.tree.explanation)),
            inspection.tree.truncated,
            inspection.tree.observed_at_ms,
            controls
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    writeln!(out, "{}", inspection.render())?;
    writeln!(out)?;
    note(
        &mut out,
        "this text is what you asked for, and it is not written to the log or the database: control \
         names are another application's content.",
    )?;
    if inspection.tree.truncated {
        note(
            &mut out,
            "the tree was cut by [desktop] max_tree_nodes, so this is a sample of the window and not \
             the whole of it",
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_diagnose(
    ctx: &Context,
    window: &WindowSelector,
    all: bool,
    limit: usize,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if all {
        let sweep = desktop::diagnose_desktop(ctx, limit);
        if json {
            let rows = sweep
                .windows
                .iter()
                .map(|report| {
                    format!(
                        "{{\"window\":{},\"workable\":{},\"next\":{},\"capabilities\":[{}]}}",
                        quote(&report.identity.audit_label()),
                        report.is_workable(),
                        quote(&format!("{:?}", report.next_step())),
                        desktop::capability_lines(report)
                            .iter()
                            .map(|line| quote(line))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                })
                .collect::<Vec<_>>()
                .join(",\n    ");
            writeln!(
                out,
                "{{\n  \"session\": {},\n  \"awareness\": {},\n  \"elapsed_ms\": {},\n  \
                 \"truncated\": {},\n  \"windows\": [\n    {}\n  ]\n}}",
                quote(sweep.session.line().as_str()),
                quote(sweep.awareness.line().as_str()),
                sweep.elapsed_ms,
                sweep.truncated,
                rows
            )?;
            return Ok(ExitCode::SUCCESS);
        }
        field(&mut out, "session", sweep.session.line())?;
        field(&mut out, "awareness", sweep.awareness.line())?;
        field(&mut out, "elapsed", format!("{} ms", sweep.elapsed_ms))?;
        if sweep.truncated {
            note(
                &mut out,
                format!("the sweep stopped at --limit {limit}; the desktop has more windows"),
            )?;
        }
        writeln!(out)?;
        for report in &sweep.windows {
            writeln!(out, "{}", report.identity.audit_label())?;
            for line in desktop::capability_lines(report) {
                writeln!(out, "  {line}")?;
            }
            writeln!(out, "  next: {:?}\n", report.next_step())?;
        }
        return Ok(ExitCode::SUCCESS);
    }

    if window.is_empty() {
        return Err(anyhow::anyhow!(
            "name a window with --hwnd, --pid, --title or --image, or pass --all to sweep the desktop"
        ));
    }
    let report = desktop::diagnose_window(ctx, window)?;
    if json {
        writeln!(
            out,
            "{{\n  \"window\": {},\n  \"workable\": {},\n  \"next\": {},\n  \"capabilities\": \
             [\n    {}\n  ]\n}}",
            quote(&report.identity.audit_label()),
            report.is_workable(),
            quote(&format!("{:?}", report.next_step())),
            desktop::capability_lines(&report)
                .iter()
                .map(|line| quote(line))
                .collect::<Vec<_>>()
                .join(",\n    ")
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    writeln!(out, "{}", report.render())?;
    writeln!(out)?;
    note(&mut out, format!("next step: {:?}", report.next_step()))?;
    note(
        &mut out,
        "`explicit_protection` is the window's own declaration and is respected, not worked around. \
         `temporarily_unavailable` is a provider that did not answer inside the bound and says nothing \
         about the application. Neither of them disables any other window.",
    )?;
    if !report.is_workable() {
        note(
            &mut out,
            "this window has no accessible controls to act on. That is the case the OCR and visual \
             fallback exists for, and it is reported as unverified in this build: see \
             `jean desktop status` and IMPLEMENTATION_STATUS.md.",
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

// ─────────────────────────────────────────────────────────────────────────────
// The report every action path prints
// ─────────────────────────────────────────────────────────────────────────────

fn print_report(report: &DesktopReport, json: bool) -> anyhow::Result<ExitCode> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let code = desktop_exit(report);
    if json {
        writeln!(
            out,
            "{{\n  \"task_id\": {},\n  \"window\": {},\n  \"pid\": {},\n  \"did\": {},\n  \
             \"before\": {},\n  \"after\": {},\n  \"changed\": {},\n  \"uncertain\": {},\n  \
             \"refused\": {},\n  \"verdict\": {},\n  \"detail\": {},\n  \"state\": {},\n  \
             \"exit_status\": {},\n  \"screenshot\": {},\n  \"retention\": {},\n  \"steps\": [{}]\n}}",
            quote(&report.task_id),
            quote(&report.window),
            report.pid,
            quote(&report.did),
            optional(report.before.as_deref()),
            optional(report.after.as_deref()),
            report.changed,
            optional(report.uncertain.as_deref()),
            report.refused,
            quote(report.verdict.label()),
            quote(report.verdict.detail()),
            quote(report.state.as_str()),
            code,
            optional(
                report
                    .screenshot
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .as_deref()
            ),
            quote(&report.retention),
            report
                .steps
                .iter()
                .map(|step| quote(step))
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        return Ok(ExitCode::from(code as u16 as u8));
    }

    field(&mut out, "task", &report.task_id)?;
    field(&mut out, "window", &report.window)?;
    field(&mut out, "did", &report.did)?;
    if let Some(before) = &report.before {
        field(&mut out, "before", before)?;
    }
    if let Some(after) = &report.after {
        field(&mut out, "after", after)?;
    }
    field(&mut out, "changed", report.changed)?;
    field(&mut out, "verdict", report.summary_line())?;
    if let Some(path) = &report.screenshot {
        field(&mut out, "frame", path.display())?;
    }
    field(&mut out, "retention", report.retention.as_str())?;
    writeln!(out)?;
    for step in &report.steps {
        note(&mut out, step)?;
    }
    if report.uncertain.is_some() {
        note(
            &mut out,
            "look at the window before deciding what to do next. This was recorded as uncertain, and \
             J.E.A.N. will not send the same action again on its own.",
        )?;
    }
    Ok(ExitCode::from(code as u16 as u8))
}

fn optional(value: Option<&str>) -> String {
    value.map(quote).unwrap_or_else(|| "null".to_owned())
}

/// The exit table from this module's header.
///
/// 4 is chosen for an uncertain outcome rather than a generic failure code because a script that retried
/// on failure would retry a click that may already have landed. 5 is only a *policy* refusal: the
/// configuration gates are errors, and an error leaves `main` at 1 the way every other command's gate
/// does — a difference between a switch an operator can flip and an answer a person gave, kept on purpose.
fn desktop_exit(report: &DesktopReport) -> i32 {
    if report.verdict.is_proven() {
        return 0;
    }
    if report.refused {
        return 5;
    }
    if report.uncertain.is_some() {
        return 4;
    }
    match report.verdict.label() {
        "not_proven" => 1,
        _ => 3,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Argument translation
// ─────────────────────────────────────────────────────────────────────────────

/// A window handle, as `jean desktop windows` printed it: `0x…` or decimal.
pub fn parse_hwnd(text: &str) -> Result<isize, String> {
    let trimmed = text.trim();
    let value = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        isize::from_str_radix(hex, 16)
    } else {
        trimmed.parse::<isize>()
    }
    .map_err(|e| format!("`{text}` is not a window handle: {e}"))?;
    if value <= 0 {
        return Err(format!(
            "`{text}` is not a window handle: handles are positive, and a zero or negative one is how \
             a stale or invented number usually arrives"
        ));
    }
    Ok(value)
}

/// The four selector flags, translated into the question they ask.
pub fn selector(
    hwnd: Option<isize>,
    pid: Option<u32>,
    title: Option<String>,
    image: Option<String>,
) -> WindowSelector {
    WindowSelector {
        hwnd,
        pid,
        title,
        image,
    }
}

/// Exactly one action, or a stated error. clap's `conflicts_with_all` would need fifteen pairs; the
/// message here is the point, and it says which one to keep.
///
/// The arguments are the flags, one per flag: a struct for them would rename what the operator typed
/// without reducing what has to be checked.
#[allow(clippy::too_many_arguments)]
pub fn chosen_action(
    invoke: bool,
    set_value: Option<String>,
    toggle: bool,
    select: bool,
    expand: bool,
    collapse: bool,
    scroll_into_view: bool,
    set_focus: bool,
) -> anyhow::Result<PatternAction> {
    let chosen = [
        (invoke, PatternAction::Invoke, "--invoke"),
        (
            set_value.is_some(),
            PatternAction::SetValue {
                value: set_value.clone().unwrap_or_default(),
            },
            "--set-value",
        ),
        (toggle, PatternAction::Toggle, "--toggle"),
        (select, PatternAction::Select, "--select"),
        (expand, PatternAction::Expand, "--expand"),
        (collapse, PatternAction::Collapse, "--collapse"),
        (
            scroll_into_view,
            PatternAction::ScrollIntoView,
            "--scroll-into-view",
        ),
        (set_focus, PatternAction::SetFocus, "--set-focus"),
    ];
    let names: Vec<&str> = chosen
        .iter()
        .filter(|(given, _, _)| *given)
        .map(|(_, _, name)| *name)
        .collect();
    match names.len() {
        0 => Err(anyhow::anyhow!(
            "no action was named; pass one of --invoke, --set-value, --toggle, --select, --expand, \
             --collapse, --scroll-into-view, --set-focus"
        )),
        1 => Ok(chosen
            .into_iter()
            .find(|(given, _, _)| *given)
            .map(|(_, action, _)| action)
            .expect("one action was counted")),
        count => Err(anyhow::anyhow!(
            "{count} actions were named ({names}); one call does one thing, so the read-back says \
             which action it is proving",
            names = names.join(", ")
        )),
    }
}

/// Exactly one input, or a stated error.
#[allow(clippy::too_many_arguments)]
pub fn chosen_input(
    click: bool,
    right_click: bool,
    double_click: bool,
    move_cursor: bool,
    scroll: Option<i32>,
    text: Option<String>,
    key: Option<String>,
    modifiers: &[String],
) -> anyhow::Result<Injected> {
    let mut names = Vec::new();
    if click {
        names.push("--click");
    }
    if right_click {
        names.push("--right-click");
    }
    if double_click {
        names.push("--double-click");
    }
    if move_cursor {
        names.push("--move");
    }
    if scroll.is_some() {
        names.push("--scroll");
    }
    if text.is_some() {
        names.push("--type");
    }
    if key.is_some() {
        names.push("--key");
    }
    if names.len() != 1 {
        return Err(if names.is_empty() {
            anyhow::anyhow!(
                "no input was named; pass one of --click, --right-click, --double-click, --move, \
                 --scroll, --type, --key"
            )
        } else {
            anyhow::anyhow!(
                "{} inputs were named ({}); one call sends one input",
                names.len(),
                names.join(", ")
            )
        });
    }
    let held = parse_modifiers(modifiers)?;
    if !held.is_empty() && key.is_none() {
        return Err(anyhow::anyhow!(
            "--modifier only means something with --key: a modifier is not a character, and holding \
             one over a click is a different input this build does not claim to express"
        ));
    }
    Ok(if click {
        Injected::Click {
            button: MouseButton::Left,
        }
    } else if right_click {
        Injected::Click {
            button: MouseButton::Right,
        }
    } else if double_click {
        Injected::DoubleClick {
            button: MouseButton::Left,
        }
    } else if move_cursor {
        Injected::Move
    } else if let Some(ticks) = scroll {
        Injected::Scroll {
            amount: ScrollAmount::Vertical(ticks),
        }
    } else if let Some(text) = text {
        Injected::Text { text }
    } else {
        Injected::Key {
            key: parse_key(key.as_deref().unwrap_or_default())?,
            modifiers: held,
        }
    })
}

fn parse_modifiers(modifiers: &[String]) -> anyhow::Result<Vec<ModifierKey>> {
    modifiers
        .iter()
        .map(|text| {
            Ok(match text.to_lowercase().as_str() {
                "ctrl" | "control" => ModifierKey::Control,
                "shift" => ModifierKey::Shift,
                "alt" => ModifierKey::Alt,
                "win" | "meta" | "super" => ModifierKey::Windows,
                other => {
                    return Err(anyhow::anyhow!(
                        "`{other}` is not a modifier this build holds; try ctrl, shift, alt, win"
                    ))
                }
            })
        })
        .collect()
}

/// A named key, or a raw virtual-key code as `0x1e` / `30`.
///
/// The named half is deliberately small and closed: a typo in a plan becomes a readable failure rather
/// than a keystroke nobody asked for. `--key` with a code is the escape hatch the mechanism already
/// offers, and it is reported as a number, because naming an arbitrary code would be a claim about a
/// keyboard layout this machine may not be using.
fn parse_key(text: &str) -> anyhow::Result<VirtualKeyCode> {
    Ok(match text.to_lowercase().as_str() {
        "enter" | "return" => VirtualKeyCode::Enter,
        "escape" | "esc" => VirtualKeyCode::Escape,
        "tab" => VirtualKeyCode::Tab,
        "space" | "spacebar" => VirtualKeyCode::Space,
        "backspace" => VirtualKeyCode::Backspace,
        "delete" | "del" => VirtualKeyCode::Delete,
        "up" => VirtualKeyCode::Up,
        "down" => VirtualKeyCode::Down,
        "left" => VirtualKeyCode::Left,
        "right" => VirtualKeyCode::Right,
        "home" => VirtualKeyCode::Home,
        "end" => VirtualKeyCode::End,
        "pageup" | "pgup" | "prior" => VirtualKeyCode::PageUp,
        "pagedown" | "pgdn" | "next" => VirtualKeyCode::PageDown,
        other => {
            let code = if let Some(hex) = other.strip_prefix("0x") {
                u16::from_str_radix(hex, 16).ok()
            } else if let Some(raw) = other.strip_prefix("raw:") {
                u16::from_str_radix(raw, 16).ok()
            } else {
                other.parse::<u16>().ok()
            };
            match code {
                Some(value) => VirtualKeyCode::Raw(value),
                None => {
                    return Err(anyhow::anyhow!(
                        "`{other}` is not a key this build names. Enter, Escape, Tab, Space, \
                         Backspace, Delete, the arrows, Home, End, PageUp, PageDown — or a virtual-key \
                         code like `0x1e`. Text belongs in --type, which is layout-independent."
                    ))
                }
            }
        }
    })
}

/// Where the input lands.
pub fn chosen_target(
    at: Option<String>,
    control: Option<ControlSelector>,
) -> anyhow::Result<InputTarget> {
    match (at.as_deref(), control) {
        (Some(spec), None) => {
            let (x, y) = spec
                .split_once(',')
                .map(|(x, y)| (x.trim().parse::<i32>(), y.trim().parse::<i32>()))
                .ok_or_else(|| {
                    anyhow::anyhow!("--at wants a point as `x,y`, for example `--at 1500,420`")
                })?;
            Ok(InputTarget::Point {
                x: x.map_err(|e| anyhow::anyhow!("`{spec}` has no readable x: {e}"))?,
                y: y.map_err(|e| anyhow::anyhow!("`{spec}` has no readable y: {e}"))?,
            })
        }
        (None, Some(selector)) => Ok(InputTarget::Control(selector)),
        (None, None) => Ok(InputTarget::Window),
        (Some(_), Some(_)) => Err(anyhow::anyhow!(
            "both --at and a control were named; one target, so the read-back knows what was aimed at"
        )),
    }
}

/// A control, from the two flags that can name one.
pub fn control(
    automation_id: Option<String>,
    name: Option<String>,
) -> anyhow::Result<ControlSelector> {
    match (automation_id, name) {
        (Some(id), None) => Ok(ControlSelector::AutomationId(id)),
        (None, Some(name)) => Ok(ControlSelector::Name(name)),
        (None, None) => Err(anyhow::anyhow!(
            "no control was named; pass --automation-id or --control. A coordinate is what \
             `jean desktop input --at` is for, and the accessible path is what the semantic action is \
             for."
        )),
        (Some(_), Some(_)) => Err(anyhow::anyhow!(
            "both --automation-id and --control were given; one locator, so the ambiguity is reported \
             rather than resolved by whichever flag came first"
        )),
    }
}

/// A witness, which may be absent.
pub fn witness(
    automation_id: Option<String>,
    name: Option<String>,
) -> anyhow::Result<Option<ControlSelector>> {
    if automation_id.is_none() && name.is_none() {
        return Ok(None);
    }
    control(automation_id, name).map(Some)
}

// ─────────────────────────────────────────────────────────────────────────────
// clap → verb
// ─────────────────────────────────────────────────────────────────────────────

/// Translate the arguments into the operation, or refuse with the sentence that says what is missing.
///
/// Fallible rather than a `From` impl, because the arguments are a question and some ways of asking them
/// are not one: two actions in a single call, a control and a coordinate, a locator that was never
/// named. Those are answered here, before a task row exists, so a malformed command leaves no record.
pub fn translate(command: Option<crate::DesktopCommand>) -> anyhow::Result<DesktopVerb> {
    use crate::DesktopCommand as C;
    Ok(match command {
        None | Some(C::Status) => DesktopVerb::Status,
        Some(C::Windows { limit, json }) => DesktopVerb::Windows { limit, json },
        Some(C::Inspect { window, json }) => DesktopVerb::Inspect {
            window: window.into_selector(),
            json,
        },
        Some(C::Diagnose {
            window,
            all,
            limit,
            json,
        }) => DesktopVerb::Diagnose {
            window: window.into_selector(),
            all,
            limit,
            json,
        },
        Some(C::Act {
            window,
            automation_id,
            // Renamed, because the translation function is `control` and a local binding called
            // `control` would shadow it — which is how `--control` ends up being "called" as a function.
            control: control_name,
            invoke,
            set_value,
            toggle,
            select,
            expand,
            collapse,
            scroll_into_view,
            set_focus,
            witness_id,
            witness_name,
            expect,
            capture,
            json,
        }) => DesktopVerb::Act {
            window: window.into_selector(),
            control: control(automation_id, control_name)?,
            action: chosen_action(
                invoke,
                set_value,
                toggle,
                select,
                expand,
                collapse,
                scroll_into_view,
                set_focus,
            )?,
            witness: witness(witness_id, witness_name)?,
            expect,
            capture,
            json,
        },
        Some(C::Input {
            window,
            at,
            automation_id,
            control: aim_name,
            click,
            right_click,
            double_click,
            move_cursor,
            scroll,
            r#type: text,
            key,
            modifiers,
            witness_id,
            witness_name,
            expect,
            capture,
            json,
        }) => {
            let aim = match (automation_id, aim_name) {
                (Some(id), None) => Some(ControlSelector::AutomationId(id)),
                (None, Some(name)) => Some(ControlSelector::Name(name)),
                (None, None) => None,
                (Some(_), Some(_)) => {
                    return Err(anyhow::anyhow!(
                        "both --automation-id and --control were given as the target of the input; \
                         one locator"
                    ))
                }
            };
            DesktopVerb::Input {
                window: window.into_selector(),
                at: chosen_target(at, aim)?,
                input: chosen_input(
                    click,
                    right_click,
                    double_click,
                    move_cursor,
                    scroll,
                    text,
                    key,
                    &modifiers,
                )?,
                witness: witness(witness_id, witness_name)?,
                expect,
                capture,
                json,
            }
        }
        Some(C::Capture { window, out, json }) => DesktopVerb::Capture {
            window: window.into_selector(),
            out,
            json,
        },
    })
}

/// A window handle, as an operator writes it.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_is_read_as_hex_or_decimal_and_never_as_something_else() {
        assert_eq!(parse_hwnd("0x2a1f3").unwrap(), 0x2a1f3);
        assert_eq!(parse_hwnd("175603").unwrap(), 175603);
        // A non-positive number is how an invented or already-freed handle usually arrives, and the
        // mechanism would then be acting on whatever recycled the value.
        for bad in ["0", "-1", "0x0", "not a handle", ""] {
            assert!(
                parse_hwnd(bad).is_err(),
                "`{bad}` was accepted as a window handle"
            );
        }
    }

    #[test]
    fn one_call_does_one_action() {
        assert!(matches!(
            chosen_action(true, None, false, false, false, false, false, false),
            Ok(PatternAction::Invoke)
        ));
        let typed = chosen_action(
            false,
            Some("hello".to_owned()),
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            typed,
            PatternAction::SetValue {
                value: "hello".to_owned()
            }
        );
        // And the description that reaches the audit line never carries the text itself.
        assert_eq!(typed.describe(), "set_value(5 chars)");
        assert!(
            chosen_action(false, None, false, false, false, false, false, false)
                .unwrap_err()
                .to_string()
                .contains("no action was named")
        );
        let two = chosen_action(true, None, true, false, false, false, false, false).unwrap_err();
        assert!(two.to_string().contains("2 actions were named"), "{two}");
    }

    #[test]
    fn one_call_sends_one_input_and_a_modifier_needs_a_key() {
        assert!(matches!(
            chosen_input(true, false, false, false, None, None, None, &[]),
            Ok(Injected::Click {
                button: MouseButton::Left
            })
        ));
        let text = chosen_input(
            false,
            false,
            false,
            false,
            None,
            Some("héllo".to_owned()),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(text.describe(), "type 5 chars");
        assert!(
            chosen_input(true, false, false, false, None, Some("x".into()), None, &[])
                .unwrap_err()
                .to_string()
                .contains("2 inputs were named")
        );
        // A modifier has to be counted as the *second* thing in the call, not the only one: with no input
        // named at all the refusal is "no input was named", which is true and is not this assertion. The
        // shape that reaches the modifier check is one input plus a held key, so the input is named here.
        assert!(
            chosen_input(
                true,
                false,
                false,
                false,
                None,
                None,
                None,
                &["ctrl".to_owned()]
            )
            .unwrap_err()
            .to_string()
            .contains("--modifier only means something with --key"),
            "a modifier held over a click is a chord this build does not claim to express"
        );
        assert!(
            chosen_input(
                false,
                false,
                false,
                false,
                None,
                None,
                None,
                &["ctrl".to_owned()]
            )
            .unwrap_err()
            .to_string()
            .contains("no input was named"),
            "a bare modifier names no input at all, and says so"
        );
        let chord = chosen_input(
            false,
            false,
            false,
            false,
            None,
            None,
            Some("enter".to_owned()),
            &["ctrl".to_owned(), "shift".to_owned()],
        )
        .unwrap();
        assert_eq!(chord.describe(), "key ctrl+shift+Enter");
    }

    #[test]
    fn a_key_is_named_or_a_code_and_never_a_guessed_layout() {
        assert_eq!(parse_key("escape").unwrap(), VirtualKeyCode::Escape);
        assert_eq!(parse_key("PageUp").unwrap(), VirtualKeyCode::PageUp);
        assert_eq!(parse_key("0x1e").unwrap(), VirtualKeyCode::Raw(0x1e));
        assert_eq!(parse_key("30").unwrap(), VirtualKeyCode::Raw(30));
        assert!(parse_key("a")
            .unwrap_err()
            .to_string()
            .contains("layout-independent"));
    }

    #[test]
    fn a_target_is_a_coordinate_or_a_control_and_not_both() {
        assert!(matches!(
            chosen_target(Some("1500,420".to_owned()), None).unwrap(),
            InputTarget::Point { x: 1500, y: 420 }
        ));
        assert!(matches!(
            chosen_target(None, None).unwrap(),
            InputTarget::Window
        ));
        assert!(chosen_target(Some("1500".to_owned()), None)
            .unwrap_err()
            .to_string()
            .contains("x,y"));
        assert!(chosen_target(
            Some("1,2".to_owned()),
            Some(ControlSelector::Name("Send".to_owned()))
        )
        .unwrap_err()
        .to_string()
        .contains("one target"));
    }

    #[test]
    fn a_control_has_to_be_named_and_naming_it_two_ways_is_refused() {
        assert_eq!(
            control(Some("1002".to_owned()), None).unwrap(),
            ControlSelector::AutomationId("1002".to_owned())
        );
        assert_eq!(
            control(None, Some("Send".to_owned())).unwrap(),
            ControlSelector::Name("Send".to_owned())
        );
        assert!(control(None, None)
            .unwrap_err()
            .to_string()
            .contains("no control was named"));
        assert!(control(Some("1002".into()), Some("Send".into()))
            .unwrap_err()
            .to_string()
            .contains("both --automation-id and --control"));
        // A witness is optional; absent it, the acted control is the thing that gets read back.
        assert!(witness(None, None).unwrap().is_none());
        assert!(witness(Some("1006".into()), None).unwrap().is_some());
    }

    #[test]
    fn the_exit_codes_separate_a_refusal_from_an_action_that_may_have_landed() {
        let base = DesktopReport {
            task_id: "t".to_owned(),
            window: "hwnd=1 pid=1 x \"w\"".to_owned(),
            pid: 1,
            did: "invoke automation_id=1002".to_owned(),
            before: None,
            after: None,
            changed: false,
            uncertain: None,
            refused: false,
            screenshot: None,
            capture_note: None,
            retention: "retention discard_after_task".to_owned(),
            verdict: jean_core::Verdict::Proven("done".to_owned()),
            state: jean_core::TaskState::Succeeded,
            steps: vec![],
        };
        assert_eq!(desktop_exit(&base), 0);
        let denied = DesktopReport {
            refused: true,
            verdict: jean_core::Verdict::Unverifiable("no".to_owned()),
            ..base.clone()
        };
        assert_eq!(desktop_exit(&denied), 5);
        let uncertain = DesktopReport {
            uncertain: Some("the provider did not answer".to_owned()),
            verdict: jean_core::Verdict::Unverifiable("unknown".to_owned()),
            state: jean_core::TaskState::NeedsAttention,
            ..base.clone()
        };
        assert_eq!(desktop_exit(&uncertain), 4);
        let unproven = DesktopReport {
            verdict: jean_core::Verdict::NotProven("it did not change".to_owned()),
            state: jean_core::TaskState::Failed,
            ..base.clone()
        };
        assert_eq!(desktop_exit(&unproven), 1);
        let nothing_declared = DesktopReport {
            verdict: jean_core::Verdict::Unverifiable("no expectation was stated".to_owned()),
            state: jean_core::TaskState::NeedsAttention,
            ..base.clone()
        };
        assert_eq!(desktop_exit(&nothing_declared), 3);
    }
}
