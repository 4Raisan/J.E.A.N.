//! The four Phase-1 intents, driven through the layer that decides and records.
//!
//! What this file is for: `jean-tools` proves the probes work, `jean-policy` proves the
//! rules decide correctly, and `jean-core/tests/config_files.rs` proves the schema. The
//! thing none of them can prove is the *seam* — that an intent actually asks the engine,
//! that a refusal stops the act, that an approval is remembered for exactly what it
//! covered, and that the audit trail is written even when nothing ran. That seam is where
//! a policy engine becomes decoration.
//!
//! ## What is driven live, and what is asserted as a rule
//!
//! `jean launch` resolves names against the machine's real registry, Start Menu and
//! `PATH`, which no test may depend on. Two consequences, both deliberate:
//!
//! * the ambiguity rule — "one launchable match or nothing" — is asserted through
//!   [`jean_core::intent::choose_candidate`] over a hand-built [`DiscoveryOutcome`], so
//!   the security-relevant decision is tested without installing anything;
//! * the full `execute` path is exercised for launch only with a query that cannot match
//!   anything, which proves the refusal creates no task row. Opening a real window here
//!   would make the suite depend on what happens to be installed.
//!
//! The other three intents are driven end to end, because their inputs the test does
//! control: this process's own pid, a temporary directory tree, and a `cmd.exe /c echo`
//! tool.
//!
//! ## Configuration
//!
//! `JEAN_DATA_DIR` is process-global and cargo runs the tests in this binary in threads,
//! so every scenario holds `ENV_LOCK`. Each test writes its own `tools.toml` and, where it
//! needs a policy list, its own `jean.toml`, because the capability lists are the thing
//! under test.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use jean_core::intent::{
    self, choose_candidate, CommandRequest, FilesRequest, Intent, LaunchRequest, Payload,
    ProcessRequest,
};
use jean_core::Context;
use jean_policy::{Decision, DenyReason, Prompter, ScriptedPrompt};
use jean_tools::{
    AppCandidate, DiscoveryBound, DiscoveryOutcome, DiscoverySource, MatchKind, Provenance,
};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jean-intent-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock must be past the epoch")
            .as_nanos()
    ))
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// A registry carrying the tools these tests need: an echo that verifies, one that
/// declares a capability on the default approval list, and one that needs elevation.
fn registry() -> String {
    r#"config_version = 1

[[tool]]
id = "echo.tool"
kind = "shell"
command = "cmd.exe"
args = ["/c", "echo", "ran"]
verify = { method = "exit_code", expected = 0 }

[[tool]]
id = "sensitive.tool"
kind = "shell"
command = "cmd.exe"
args = ["/c", "echo", "ran"]
capabilities = ["network_egress_new_domain"]
verify = { method = "exit_code", expected = 0 }

[[tool]]
id = "elevated.tool"
kind = "shell"
command = "cmd.exe"
args = ["/c", "echo", "ran"]
tier = "elevated"
verify = { method = "exit_code", expected = 0 }

"#
    .to_owned()
}

/// Load a context with `tools.toml` (and optionally `jean.toml`) in a private data dir.
///
/// The guard is returned, not dropped, so the caller holds `ENV_LOCK` for as long as the
/// context exists: `JEAN_DATA_DIR` is process-global and `Context::load` reads it.
fn scenario(
    tag: &str,
    jean_toml: Option<&str>,
) -> (PathBuf, Context, std::sync::MutexGuard<'static, ()>) {
    let dir = temp_dir(tag);
    std::fs::create_dir_all(&dir).expect("the temp data directory must be creatable");
    std::fs::write(dir.join("tools.toml"), registry()).expect("the registry must be writable");
    if let Some(config) = jean_toml {
        std::fs::write(dir.join("jean.toml"), config).expect("the configuration must be writable");
    }

    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("JEAN_DATA_DIR", &dir);
    let ctx = Context::load(None).expect("a data directory with a valid registry must load");

    // A rejected registry degrades to "no tools" with a warning rather than failing the
    // load, so a typo in the fixture above would otherwise surface as `UnknownTool` three
    // layers away from what caused it.
    assert!(
        ctx.tools.get("echo.tool").is_some(),
        "the fixture registry must load: {:?}",
        ctx.warnings
    );
    (dir, ctx, guard)
}

fn run_intent(
    ctx: &mut Context,
    intent: &Intent,
    prompter: &mut dyn Prompter,
) -> jean_core::Result<intent::IntentReport> {
    intent::execute(ctx, intent, prompter)
}

fn find_intent(root: &Path, pattern: &str) -> Intent {
    Intent::Files(FilesRequest {
        root: root.to_path_buf(),
        pattern: pattern.to_owned(),
        max_results: None,
    })
}

fn launched_or_refused(report: &intent::IntentReport) -> bool {
    !matches!(report.payload, Payload::Nothing)
}

#[test]
fn an_auto_denied_capability_is_refused_without_asking() {
    let (dir, mut ctx, _guard) = scenario(
        "auto-deny",
        Some("[policy]\nauto_deny = [\"read_filesystem\"]\n"),
    );
    let root = dir.join("searched");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("note.txt"), "x").unwrap();

    let mut prompter = ScriptedPrompt::approving();
    let report = run_intent(&mut ctx, &find_intent(&root, "note"), &mut prompter)
        .expect("a refusal is a result, not an error");

    assert!(
        prompter.asked.is_empty(),
        "auto-deny must produce no prompt; a prompt would invite a yes for something the \
         configuration already forbids. Asked: {:?}",
        prompter.asked
    );
    match report.decision {
        Decision::Denied(DenyReason::AutoDenied { ref capability }) => {
            assert_eq!(capability, "read_filesystem");
        }
        other => panic!("expected an auto-deny, got {other:?}"),
    }
    assert_eq!(
        report.exit_status(),
        5,
        "a policy refusal has its own exit status"
    );
    assert!(
        !launched_or_refused(&report),
        "nothing may run after a denial"
    );
    // The denial is recorded, because "it was refused" is a fact an operator needs later.
    assert_eq!(report.state, jean_core::TaskState::Failed);
    assert!(
        report.notes[0].contains("auto-deny list"),
        "the refusal must explain itself: {:?}",
        report.notes
    );
    cleanup(&dir);
}

/// An elevated operation is refused because no sidecar exists in this build (D-010), and
/// refusing it in the engine is what lets the CLI name the real blocker instead of
/// reporting a launch failure.
#[test]
fn an_elevated_command_is_refused_because_there_is_no_helper() {
    let (dir, mut ctx, _guard) = scenario("elevated", None);
    let mut prompter = ScriptedPrompt::approving();
    let intent = Intent::Command(CommandRequest {
        tool_id: "elevated.tool".to_owned(),
        args: Vec::new(),
        goal: None,
        interactive: None,
    });

    let report = run_intent(&mut ctx, &intent, &mut prompter).expect("a refusal is a result");
    assert!(
        prompter.asked.is_empty(),
        "elevation is refused before any question could be asked"
    );
    let reason = report.denial().expect("the decision must be a denial");
    assert!(
        matches!(reason, DenyReason::ElevationRequired { .. }),
        "expected the elevation refusal, got {reason:?}"
    );
    assert!(
        reason.to_string().contains("jean-elevated.exe"),
        "the refusal must name the helper the operator would have to provide: {reason}"
    );
    assert_eq!(report.exit_status(), 5);
    cleanup(&dir);
}

#[test]
fn a_capability_on_the_approval_list_asks_and_a_decline_stops_the_run() {
    let (dir, mut ctx, _guard) = scenario("decline", None);
    let before = ctx
        .store
        .count("tool_runs")
        .expect("the row count must be readable");

    let mut prompter = ScriptedPrompt::denying();
    let intent = Intent::Command(CommandRequest {
        tool_id: "sensitive.tool".to_owned(),
        args: Vec::new(),
        goal: Some("prove declining is quiet".to_owned()),
        interactive: None,
    });
    let report = run_intent(&mut ctx, &intent, &mut prompter).expect("a decline is a result");

    assert_eq!(
        prompter.asked.len(),
        1,
        "exactly one question, for the capability the tool declares: {:?}",
        prompter.asked
    );
    assert!(
        prompter.asked[0].contains("sensitive.tool"),
        "the question the ledger keeps must name the tool: {:?}",
        prompter.asked[0]
    );
    // The prompt *rendered for the operator* names the capability; `ScriptedPrompt`
    // records only the summary, so the capability has to be asserted where it is shown.
    let capabilities = vec!["network_egress_new_domain".to_owned()];
    let shown = jean_policy::prompt::Prompt {
        summary: &prompter.asked[0],
        operation: "run_command",
        tool_id: Some("sensitive.tool"),
        tier: "normal",
        capabilities: &capabilities,
        allow_remember: true,
    }
    .render();
    assert!(
        shown.contains("network_egress_new_domain") && shown.contains("[y] yes, this once"),
        "the question must show what is being agreed to: {shown}"
    );
    assert!(
        matches!(report.decision, Decision::Denied(DenyReason::Declined)),
        "{:?}",
        report.decision
    );
    assert_eq!(
        ctx.store.count("tool_runs").expect("count"),
        before,
        "a declined command must not leave a tool run behind — nothing ran"
    );
    assert_eq!(report.exit_status(), 5);
    cleanup(&dir);
}

/// A remembered approval is bound to the operation *and* the resolved target and the
/// argument vector. This is the test that catches a digest that matches too much.
#[test]
fn a_remembered_approval_covers_those_arguments_and_not_others() {
    let (dir, mut ctx, _guard) = scenario("remember", None);
    let intent = || {
        Intent::Command(CommandRequest {
            tool_id: "sensitive.tool".to_owned(),
            args: vec!["first".to_owned()],
            goal: None,
            interactive: None,
        })
    };
    let different = Intent::Command(CommandRequest {
        tool_id: "sensitive.tool".to_owned(),
        args: vec!["second".to_owned()],
        goal: None,
        interactive: None,
    });

    let mut first = ScriptedPrompt::remembering();
    let report =
        run_intent(&mut ctx, &intent(), &mut first).expect("the approval must allow the run");
    assert!(
        first.asked.len() == 1,
        "the first attempt asks: {:?}",
        first.asked
    );
    assert!(
        report.is_success(),
        "an approved run must complete and verify: {:?} {:?}",
        report.state,
        report.verdict
    );

    // Same tool, same arguments: the ledger answers, and no person is asked twice.
    let mut second = ScriptedPrompt::approving();
    let report =
        run_intent(&mut ctx, &intent(), &mut second).expect("the remembered run must proceed");
    assert!(
        second.asked.is_empty(),
        "a remembered approval must cover the identical operation: {:?}",
        second.asked
    );
    match report.decision {
        Decision::Allowed { authority } => {
            assert_eq!(authority.as_str(), "remembered_approval");
            assert!(
                authority.approval_id().is_some(),
                "the record must say which approval authorised it"
            );
        }
        ref other => panic!("expected an allowed decision, got {other:?}"),
    }

    // Different arguments: the grant does not stretch.
    let mut third = ScriptedPrompt::denying();
    let report = run_intent(&mut ctx, &different, &mut third).expect("the decline is a result");
    assert_eq!(
        third.asked.len(),
        1,
        "changed arguments must ask again — a broader grant would be the blanket approval \
         `policy.approval_scope` refuses"
    );
    assert!(
        matches!(report.decision, Decision::Denied(DenyReason::Declined)),
        "{:?}",
        report.decision
    );
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// The audit trail
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn every_attempt_is_audited_including_the_ones_that_never_ran() {
    let (dir, mut ctx, _guard) = scenario(
        "audit",
        Some("[policy]\naudit_enabled = true\nauto_deny = [\"read_filesystem\"]\n"),
    );
    let root = dir.join("searched");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("note.txt"), "x").unwrap();

    let mut prompter = ScriptedPrompt::approving();
    // Denied, then allowed: one line for each decision, plus the outcome line for the run
    // that actually happened.
    run_intent(&mut ctx, &find_intent(&root, "note"), &mut prompter).expect("denied is a result");
    let allowed = Intent::Process(ProcessRequest {
        this_process: true,
        ..ProcessRequest::default()
    });
    run_intent(&mut ctx, &allowed, &mut prompter).expect("the probe must run");

    let text = std::fs::read_to_string(&ctx.paths.audit).unwrap_or_else(|e| {
        panic!(
            "{} must exist and be readable: {e}",
            ctx.paths.audit.display()
        )
    });
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        lines.len() >= 3,
        "expected at least three records (a denial, an allow, an outcome), got {}: {lines:?}",
        lines.len()
    );

    let denied = lines
        .iter()
        .find(|l| l.contains("\"decision\":\"denied\""))
        .expect("a denied attempt must be in the trail");
    assert!(
        denied.contains("locate_file"),
        "the operation must be named: {denied}"
    );
    assert!(
        denied.contains("\"auto_denied\""),
        "the reason kind must be recorded: {denied}"
    );
    let allowed_line = lines
        .iter()
        .filter(|l| l.contains("inspect_process"))
        .find(|l| l.contains("\"outcome\""))
        .unwrap_or_else(|| panic!("an act that ran must have an outcome record: {lines:?}"));
    assert!(
        allowed_line.contains("\"outcome\""),
        "the outcome line must say what happened: {allowed_line}"
    );
    // Every line parses as JSON on its own: append-only JSONL is the format the roadmap
    // specifies, and a line that breaks the parser breaks every later reader.
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("audit line is not JSON: {e}\n{line}"));
    }
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// Launch: the ambiguity rule
// ─────────────────────────────────────────────────────────────────────────────

fn candidate(name: &str, exe: &str, broken: bool) -> AppCandidate {
    AppCandidate {
        display_name: name.to_owned(),
        exe_path: Some(PathBuf::from(exe)),
        sources: vec![Provenance {
            source: DiscoverySource::StartMenu,
            seen_at: format!(r"C:\Start Menu\{name}.lnk"),
            app_path_value: None,
        }],
        match_kind: MatchKind::Exact,
        is_broken: broken,
        install_location: None,
    }
}

fn outcome_with(candidates: Vec<AppCandidate>) -> DiscoveryOutcome {
    let ambiguous = candidates.len() > 1;
    DiscoveryOutcome {
        query: "calc".to_owned(),
        bound: DiscoveryBound::None,
        inaccessible_keys: Vec::new(),
        sources_used: vec![DiscoverySource::StartMenu],
        directories_scanned: 1,
        entries_examined: 3,
        elapsed_ms: 4,
        notes: Vec::new(),
        ambiguous,
        candidates,
    }
}

#[test]
fn several_matches_launch_nothing_and_name_every_one_of_them() {
    let outcome = outcome_with(vec![
        candidate("Calculator", r"C:\Windows\System32\calc.exe", false),
        candidate("Calc Bridge", r"C:\Tools\calc.exe", false),
    ]);

    let err = choose_candidate(&outcome, None).unwrap_err();
    assert!(err.contains("matches 2 applications"), "{err}");

    let rendered = intent::render_candidates(&outcome);
    assert!(rendered.contains("1) Calculator"), "{rendered}");
    assert!(rendered.contains("2) Calc Bridge"), "{rendered}");
    assert!(
        rendered.contains(r"C:\Windows\System32\calc.exe"),
        "the list must show what would run: {rendered}"
    );
}

#[test]
fn one_launchable_match_is_the_answer_and_a_broken_shortcut_is_not() {
    let good = outcome_with(vec![candidate(
        "Calculator",
        r"C:\Windows\System32\calc.exe",
        false,
    )]);
    let chosen = choose_candidate(&good, None).expect("a single launchable match is chosen");
    assert_eq!(chosen.display_name, "Calculator");

    let broken = outcome_with(vec![candidate("Gone", r"C:\Old\app.exe", true)]);
    let err = choose_candidate(&broken, None).unwrap_err();
    assert!(
        err.contains("NOT LAUNCHABLE") || err.contains("nothing launchable"),
        "{err}"
    );
    let rendered = intent::render_candidates(&broken);
    assert!(
        rendered.contains("NOT LAUNCHABLE"),
        "the candidate stays listed with its defect stated, rather than being dropped: \
         {rendered}"
    );
}

#[test]
fn a_pick_selects_by_position_and_out_of_range_refuses() {
    let outcome = outcome_with(vec![
        candidate("Calculator", r"C:\Windows\System32\calc.exe", false),
        candidate("Calc Bridge", r"C:\Tools\calc.exe", false),
    ]);
    let second = choose_candidate(&outcome, Some(2)).expect("an explicit pick is honoured");
    assert_eq!(second.display_name, "Calc Bridge");

    let too_far = choose_candidate(&outcome, Some(9)).unwrap_err();
    assert!(too_far.contains("no launchable candidate #9"), "{too_far}");
    let zero = choose_candidate(&outcome, Some(0)).unwrap_err();
    assert!(zero.contains("1-based"), "{zero}");
    // The refusal has to name the application the operator asked for. It once shipped
    // an unformatted `{}` in that slot, which is the failure this line pins.
    assert!(
        zero.contains("jean launch calc"),
        "the query is missing from: {zero}"
    );
    assert!(
        !zero.contains("{}"),
        "unformatted placeholder reached the operator: {zero}"
    );
}

#[test]
fn an_unresolvable_application_creates_no_record_and_launches_nothing() {
    let (dir, mut ctx, _guard) = scenario("launch-missing", None);
    let tasks_before = ctx.store.count("tasks").expect("count");

    let mut prompter = ScriptedPrompt::approving();
    let attempt = Intent::Launch(LaunchRequest {
        query: "jean-does-not-exist-anywhere-9f3a".to_owned(),
        ..LaunchRequest::default()
    });
    let err = intent::execute(&mut ctx, &attempt, &mut prompter).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("no installed application matches"), "{text}");
    assert!(
        prompter.asked.is_empty(),
        "nothing can be approved for an application that was never found: {:?}",
        prompter.asked
    );
    assert_eq!(
        ctx.store.count("tasks").expect("count"),
        tasks_before,
        "a refusal at resolution time must leave no task row"
    );
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// Inspect a process
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn inspecting_this_process_proves_the_pid_is_alive() {
    let (dir, mut ctx, _guard) = scenario("ps-self", None);
    let mut prompter = ScriptedPrompt::approving();
    let report = run_intent(
        &mut ctx,
        &Intent::Process(ProcessRequest {
            this_process: true,
            ..ProcessRequest::default()
        }),
        &mut prompter,
    )
    .expect("the probe must run");

    assert_eq!(report.state, jean_core::TaskState::Succeeded);
    match &report.verdict {
        jean_core::Verdict::Proven(detail) => {
            assert!(
                detail.contains(&format!("pid {}", std::process::id())),
                "the verdict must name the pid it proved: {detail}"
            );
        }
        other => panic!(
            "a live pid with obtainable fields must be proven; got {:?} — note that a \
             platform-absent field must not be treated as a refusal",
            other
        ),
    }
    assert_eq!(report.exit_status(), 0, "proven is exit 0");
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("thread_count") && n.contains("platform absence")),
        "the never-available field has to be stated as what it is: {:?}",
        report.notes
    );
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("cpu percentages are of ONE core")),
        "the sampling semantics must travel with the numbers: {:?}",
        report.notes
    );
    cleanup(&dir);
}

#[test]
fn a_pid_that_does_not_exist_is_answered_not_treated_as_a_failure() {
    let (dir, mut ctx, _guard) = scenario("ps-gone", None);
    let missing = jean_tools::process::unused_pid().expect("an unused pid must be findable");
    let mut prompter = ScriptedPrompt::approving();
    let report = run_intent(
        &mut ctx,
        &Intent::Process(ProcessRequest {
            pid: Some(missing),
            ..ProcessRequest::default()
        }),
        &mut prompter,
    )
    .expect("a negative answer is still an answer");

    assert_eq!(
        report.state,
        jean_core::TaskState::Succeeded,
        "the probe ran and reported honestly; the *verdict* is where absence is said"
    );
    match &report.verdict {
        jean_core::Verdict::NotProven(detail) => {
            assert!(detail.contains(&format!("pid {missing}")), "{detail}")
        }
        other => panic!("an absent pid must be NotProven, got {other:?}"),
    }
    assert_eq!(
        report.exit_status(),
        3,
        "nothing proved the goal, which is exit 3 rather than an error"
    );
    match &report.payload {
        Payload::Processes { gone, rows, .. } => {
            assert_eq!(*gone, Some(missing));
            assert!(rows.is_empty(), "no rows for an absent pid");
        }
        other => panic!("expected a process payload, got {other:?}"),
    }
    cleanup(&dir);
}

#[test]
fn a_listing_reports_the_machine_header_and_honours_its_bound() {
    let (dir, mut ctx, _guard) = scenario("ps-list", None);
    let mut prompter = ScriptedPrompt::approving();
    let report = run_intent(
        &mut ctx,
        &Intent::Process(ProcessRequest {
            list: true,
            limit: Some(3),
            sort: Some(jean_tools::SortKey::Pid),
            // The floor the sampler documents: below this the delta is noise.
            sample_window_ms: Some(20),
            ..ProcessRequest::default()
        }),
        &mut prompter,
    )
    .expect("the listing must run");

    match &report.payload {
        Payload::Processes {
            machine,
            rows,
            truncated,
            sample_window_ms,
            ..
        } => {
            let machine = machine
                .as_ref()
                .expect("a listing carries the machine header");
            assert!(machine.total_memory_bytes > 0, "the header must be real");
            assert!(machine.logical_core_count > 0);
            assert_eq!(*sample_window_ms, 20);
            assert!(rows.len() <= 3, "the bound is a bound: {}", rows.len());
            assert!(
                *truncated,
                "a three-row cut on a real machine must say it cut"
            );
            // Sorted by pid, so the rows must be ascending — that is what makes two
            // runs comparable, which is why `SortKey::Pid` exists.
            assert!(
                rows.windows(2).all(|w| w[0].pid <= w[1].pid),
                "pid order broken: {:?}",
                rows.iter().map(|r| r.pid).collect::<Vec<_>>()
            );
        }
        other => panic!("expected a listing, got {other:?}"),
    }
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// Locate a file
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_found_file_is_proven_to_exist_and_be_readable() {
    let (dir, mut ctx, _guard) = scenario("find-hit", None);
    let root = dir.join("searched");
    std::fs::create_dir_all(root.join("nested")).unwrap();
    std::fs::write(root.join("nested").join("invoice-2026.txt"), "x").unwrap();

    let mut prompter = ScriptedPrompt::approving();
    let report = run_intent(&mut ctx, &find_intent(&root, "invoice"), &mut prompter)
        .expect("the walk must run");

    match &report.payload {
        Payload::Files(search) => {
            assert_eq!(search.hits.len(), 1, "{:?}", search.hits);
            assert!(search.fully_covered(), "a complete walk must say so");
        }
        other => panic!("expected a file payload, got {other:?}"),
    }
    assert!(
        report.verdict.is_proven(),
        "the roadmap's verification is that the path exists and opens: {:?}",
        report.verdict
    );
    assert_eq!(report.exit_status(), 0);
    cleanup(&dir);
}

#[test]
fn a_root_that_is_not_there_is_refused_before_any_record_exists() {
    let (dir, mut ctx, _guard) = scenario("find-root", None);
    let tasks_before = ctx.store.count("tasks").expect("count");
    let mut prompter = ScriptedPrompt::approving();

    let err = run_intent(
        &mut ctx,
        &find_intent(&dir.join("nowhere-at-all"), "anything"),
        &mut prompter,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("cannot be used as a search root"),
        "{err}"
    );
    assert_eq!(
        ctx.store.count("tasks").expect("count"),
        tasks_before,
        "refusing to search must not leave a task behind"
    );
    assert!(
        prompter.asked.is_empty(),
        "an impossible search is refused before it is approved"
    );
    cleanup(&dir);
}

#[test]
fn no_match_in_a_fully_searched_root_says_so_instead_of_failing() {
    let (dir, mut ctx, _guard) = scenario("find-miss", None);
    let root = dir.join("searched");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("other.md"), "x").unwrap();

    let mut prompter = ScriptedPrompt::approving();
    let report = run_intent(&mut ctx, &find_intent(&root, "invoice"), &mut prompter)
        .expect("a negative answer is a result");

    match &report.verdict {
        jean_core::Verdict::NotProven(detail) => {
            assert!(
                detail.contains("was walked to its bounds"),
                "a complete search must say it was complete: {detail}"
            );
        }
        other => panic!("expected NotProven, got {other:?}"),
    }
    assert_eq!(report.state, jean_core::TaskState::Succeeded);
    assert_eq!(
        report.exit_status(),
        3,
        "searching was successful; locating was not — those are different sentences"
    );
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// The command intent keeps Phase 0 intact
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_command_runs_under_the_phase_zero_pipeline_and_is_verified() {
    let (dir, mut ctx, _guard) = scenario("command", None);
    let mut prompter = ScriptedPrompt::approving();
    let report = run_intent(
        &mut ctx,
        &Intent::Command(CommandRequest {
            tool_id: "echo.tool".to_owned(),
            args: Vec::new(),
            goal: Some("prove the seam".to_owned()),
            interactive: None,
        }),
        &mut prompter,
    )
    .expect("the command must run");

    assert!(
        prompter.asked.is_empty(),
        "a tool with no listed capability is authorised by the command the operator typed, \
         not by a prompt: {:?}",
        prompter.asked
    );
    match &report.payload {
        Payload::Command(run) => {
            assert_eq!(run.outcome.termination, jean_core::Termination::Exited);
            assert_eq!(run.outcome.exit_code, Some(0));
            assert!(
                run.outcome.stdout.bytes > 0,
                "the echo must have been captured: {:?}",
                run.outcome.stdout
            );
            assert_eq!(
                report.task_id, run.task_id,
                "the report names Phase 0's task row"
            );
        }
        other => panic!("expected a command payload, got {other:?}"),
    }
    assert!(report.is_success(), "{:?}", report.verdict);
    assert_eq!(report.exit_status(), 0);
    cleanup(&dir);
}

/// The console handler used to be armable once per process, which meant a second
/// supervised task in one invocation could not be cancelled. Phase 1 runs several intents
/// per process, so both of these runs happen in the same binary.
#[test]
fn two_supervised_tasks_in_one_invocation_are_each_cancellable() {
    let (dir, mut ctx, _guard) = scenario("rearm", None);
    let mut prompter = ScriptedPrompt::approving();
    let intent = || {
        Intent::Command(CommandRequest {
            tool_id: "echo.tool".to_owned(),
            args: Vec::new(),
            goal: None,
            interactive: None,
        })
    };

    let first = run_intent(&mut ctx, &intent(), &mut prompter).expect("the first run must work");
    let second = run_intent(&mut ctx, &intent(), &mut prompter)
        .expect("a second supervised run in the same process must also work");
    assert_ne!(
        first.task_id, second.task_id,
        "each run owns its own record"
    );
    assert!(second.is_success(), "{:?}", second.verdict);
    cleanup(&dir);
}

#[test]
fn an_answer_the_prompter_never_offers_cannot_make_a_grant() {
    // `allow_remember = false` means the prompt never offers "remember"; an answer of
    // `ApproveAndRemember` therefore comes from a broken prompter, not from an operator.
    // The engine must fail rather than widen the grant, and nothing may run either.
    let (dir, mut ctx, _guard) = scenario(
        "bad-answer",
        Some(
            "[policy]
allow_remember = false
require_approval = [\"network_egress_new_domain\"]
",
        ),
    );
    let mut prompter = ScriptedPrompt::remembering();
    let intent = Intent::Command(CommandRequest {
        tool_id: "sensitive.tool".to_owned(),
        args: Vec::new(),
        goal: None,
        interactive: None,
    });
    let outcome = intent::execute(&mut ctx, &intent, &mut prompter);
    let err = match outcome {
        Ok(report) => panic!(
            "a remembered grant must be refused when the configuration forbids remembering: {:?}",
            report.decision
        ),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("could not decide"),
        "the failure must name the rule it hit: {err}"
    );
    let rows = ctx
        .store
        .list_approvals()
        .expect("the ledger must be readable");
    assert!(rows.is_empty(), "no approval row may exist: {rows:?}");
    assert_eq!(
        ctx.store.count("tool_runs").expect("count"),
        0,
        "an impossible answer runs nothing"
    );
    cleanup(&dir);
}
