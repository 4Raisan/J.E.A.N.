//! The decision engine, end to end.
//!
//! These tests are the executable form of the authorization rules, and the one
//! that matters most is the one that asserts *no prompt happened*: an approval
//! engine that asks about routine work is the engine the roadmap's "no repeated
//! approval prompts" requirement exists to forbid. `ScriptedPrompt` errors when it
//! is asked a question nobody scripted, so "allowed with no prompt" cannot pass by
//! accident.

use jean_policy::basis::Authority;
use jean_policy::decision::{Decision, DenyReason};
use jean_policy::operation::{Operation, OperationKind, Scope};
use jean_policy::prompt::{Answer, PolicyError, ScriptedPrompt};
use jean_policy::{Policy, PolicyError as EngineError};
use jean_protocol::config::PolicyConfig;
use jean_protocol::tool::PolicyTier;
use jean_store::Store;

/// The configuration as shipped, so a test that passes here passes against the
/// real `require_approval` and `auto_deny` lists rather than a convenient subset.
fn config() -> PolicyConfig {
    PolicyConfig::default()
}

fn store() -> Store {
    Store::open_in_memory().expect("in-memory ledger")
}

fn op(kind: OperationKind, capabilities: &[&str], arguments: &[&str]) -> Operation {
    Operation {
        kind,
        tool_id: Some("powershell.echo".to_owned()),
        tier: PolicyTier::Normal,
        capabilities: capabilities.iter().map(|c| (*c).to_string()).collect(),
        summary: format!("{} {}", kind.as_str(), arguments.join(" ")),
        scope: Scope::new(
            "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
            arguments.iter().map(|a| (*a).to_string()).collect(),
        ),
    }
}

#[test]
fn a_routine_step_of_a_typed_command_is_allowed_without_a_prompt() {
    // The roadmap's four intents, with no capability that `require_approval`
    // mentions: the command the operator just typed is the authority.
    let policy = Policy::new(config());
    let ledger = store();
    // No scripted answers at all. A prompt here would return
    // `Err(PolicyError::ScriptExhausted)`, which is the assertion.
    let mut prompter = ScriptedPrompt::default();

    for kind in [
        OperationKind::LaunchApp,
        OperationKind::RunCommand,
        OperationKind::InspectProcess,
        OperationKind::LocateFile,
        OperationKind::ProbeHealth,
    ] {
        let decision = policy
            .decide(
                &op(kind, &[], &["-Command", "Get-Date"]),
                &ledger,
                &mut prompter,
            )
            .unwrap_or_else(|e| panic!("{kind:?} raised a prompt: {e}"));
        assert_eq!(
            decision,
            Decision::allowed(Authority::OwnerCommand),
            "{kind:?} should be allowed on the typed command alone"
        );
    }
    assert!(
        prompter.asked.is_empty(),
        "routine work prompted {} times: {:?}",
        prompter.asked.len(),
        prompter.asked
    );
}

#[test]
fn an_auto_denied_capability_is_refused_without_asking() {
    let policy = Policy::new(config());
    let ledger = store();

    // `format_volume` is on the documented auto-deny list. Even a prompter that
    // would have said yes — and said it permanently — must never be consulted.
    let mut prompter = ScriptedPrompt {
        answers: vec![Answer::ApproveAndRemember],
        asked: Vec::new(),
    };
    let decision = policy
        .decide(
            &op(OperationKind::RunCommand, &["format_volume"], &["C:"]),
            &ledger,
            &mut prompter,
        )
        .expect("a denial is a decision, not an error");

    assert_eq!(
        decision,
        Decision::Denied(DenyReason::AutoDenied {
            capability: "format_volume".to_owned()
        }),
        "{decision}"
    );
    assert!(
        prompter.asked.is_empty(),
        "an auto-denied capability must never reach a person: {:?}",
        prompter.asked
    );
    assert_eq!(
        decision.reason().map(DenyReason::kind),
        Some("auto_denied"),
        "the audit trail needs the category, not just the sentence"
    );
}

#[test]
fn auto_deny_wins_when_a_capability_is_in_both_lists() {
    // A config can contradict itself, and when it does the engine must fail closed.
    let mut config = config();
    config.auto_deny.push("process_kill".to_owned());
    // `process_kill` is already on require_approval by default.
    assert!(config.require_approval.iter().any(|c| c == "process_kill"));

    let policy = Policy::new(config);
    let ledger = store();
    let mut prompter = ScriptedPrompt::approving();

    let decision = policy
        .decide(
            &op(OperationKind::RunCommand, &["process_kill"], &["notepad"]),
            &ledger,
            &mut prompter,
        )
        .unwrap();
    assert!(
        matches!(decision, Decision::Denied(DenyReason::AutoDenied { .. })),
        "{decision}"
    );
    assert!(prompter.asked.is_empty(), "denied work must not prompt");
}

#[test]
fn elevation_is_a_blocker_and_says_which_sidecar_is_missing() {
    // D-010: nothing in this build can obtain a token, so an `elevated` operation
    // is refused rather than attempted. The refusal names the helper that would
    // have to exist, because "no" without a reason reads like a bug.
    let policy = Policy::new(config());
    let ledger = store();
    let mut prompter = ScriptedPrompt::default();

    let mut elevated = op(OperationKind::RunCommand, &[], &["net", "start", "spooler"]);
    elevated.tier = PolicyTier::Elevated;

    let decision = policy
        .decide(&elevated, &ledger, &mut prompter)
        .expect("a refusal is a decision");
    let reason = decision.reason().expect("denied");
    assert_eq!(reason.kind(), "elevation_required");
    assert_eq!(
        reason,
        &DenyReason::ElevationRequired {
            helper: "jean-elevated.exe".to_owned()
        }
    );
    let text = reason.to_string();
    assert!(text.contains("jean-elevated.exe"), "{text}");
    assert!(
        text.contains("D-010"),
        "the reason should cite the decision: {text}"
    );
    assert!(prompter.asked.is_empty(), "no prompt can change this");
}

#[test]
fn a_sensitive_capability_prompts_once_and_an_approval_authorises_that_one_call() {
    let policy = Policy::new(config());
    let ledger = store();
    let mut prompter = ScriptedPrompt::approving();

    let operation = op(
        OperationKind::RunCommand,
        &["write_outside_workspace"],
        &["Set-Content", "C:\\other\\file"],
    );
    let decision = policy.decide(&operation, &ledger, &mut prompter).unwrap();
    assert_eq!(decision, Decision::allowed(Authority::ApprovedThisTime));
    assert_eq!(
        prompter.asked,
        vec![operation.summary.clone()],
        "the operator was shown the operation they were asked about"
    );

    // Approving once does not leave a grant behind, so the next identical call asks
    // again. That is the difference between `y` and `a`.
    let mut again = ScriptedPrompt::denying();
    let second = policy.decide(&operation, &ledger, &mut again).unwrap();
    assert_eq!(second, Decision::Denied(DenyReason::Declined));
    assert_eq!(again.asked, vec![operation.summary.clone()]);
}

#[test]
fn a_declined_prompt_denies_and_leaves_nothing_remembered() {
    let policy = Policy::new(config());
    let ledger = store();
    let mut prompter = ScriptedPrompt::denying();

    let operation = op(
        OperationKind::InspectProcess,
        &["credential_read"],
        &["--pid", "4242"],
    );
    let decision = policy.decide(&operation, &ledger, &mut prompter).unwrap();
    assert_eq!(decision, Decision::Denied(DenyReason::Declined));
    assert_eq!(
        ledger.count("approvals").unwrap(),
        0,
        "a denial must not leave a grant behind"
    );
}

#[test]
fn a_remembered_approval_covers_that_operation_and_those_arguments_and_nothing_else() {
    // The requirement this whole path exists for: approving one PowerShell command
    // must not authorise every future PowerShell command.
    let policy = Policy::new(config());
    let ledger = store();
    let mut prompter = ScriptedPrompt::remembering();

    let push = op(
        OperationKind::RunCommand,
        &["write_outside_workspace"],
        &["git", "push", "origin", "main"],
    );
    let first = policy.decide(&push, &ledger, &mut prompter).unwrap();
    let Authority::Remembered { id, .. } = first.authority().expect("allowed") else {
        panic!("expected a remembered approval, got {first}");
    };
    assert_eq!(ledger.count("approvals").unwrap(), 1);

    // Exactly the same operation and arguments: allowed, and nobody is asked twice.
    let mut silent = ScriptedPrompt::default();
    let second = policy.decide(&push, &ledger, &mut silent).unwrap();
    assert!(
        second.is_allowed(),
        "same operation and arguments must reuse the grant: {second}"
    );
    match second {
        Decision::Allowed {
            authority: Authority::Remembered { id: reused, .. },
        } => assert_eq!(reused, id),
        other => panic!("expected the remembered approval #{id}, got {other}"),
    }
    assert!(
        silent.asked.is_empty(),
        "a remembered approval must not prompt again: {:?}",
        silent.asked
    );
    assert_eq!(
        ledger
            .find_approval(&push.scope_digest())
            .unwrap()
            .map(|a| a.used_count),
        Some(1),
        "the use is counted, so `jean approvals` can show what is carrying work"
    );

    // Same tool, same tier, one different argument: a different act, and it asks.
    let mut pull = op(
        OperationKind::RunCommand,
        &["write_outside_workspace"],
        &["git", "push", "origin", "main", "--force"],
    );
    pull.summary = "forced push".to_owned();
    let mut prompts = ScriptedPrompt::denying();
    let decision = policy.decide(&pull, &ledger, &mut prompts).unwrap();
    assert_eq!(decision, Decision::Denied(DenyReason::Declined));
    assert_eq!(prompts.asked.len(), 1, "the changed arguments re-prompted");
    assert_eq!(
        ledger.count("approvals").unwrap(),
        1,
        "declining the new act must not widen the old grant"
    );

    // A different operation kind over the same arguments is also a different act.
    let mut launch = push.clone();
    launch.kind = OperationKind::LaunchApp;
    let mut prompts = ScriptedPrompt::default();
    let error = policy
        .decide(&launch, &ledger, &mut prompts)
        .expect_err("nothing was scripted, so it must have asked");
    assert!(
        matches!(error, PolicyError::ScriptExhausted(_)),
        "a changed operation must prompt, got {error:?}"
    );
}

#[test]
fn forgetting_an_approval_makes_the_next_call_ask_again() {
    let policy = Policy::new(config());
    let ledger = store();
    let mut prompter = ScriptedPrompt::remembering();
    let operation = op(
        OperationKind::RunCommand,
        &["registry_write"],
        &["New-ItemProperty", "-Path", "HKCU:\\Software\\JEAN"],
    );
    let allowed = policy.decide(&operation, &ledger, &mut prompter).unwrap();
    let id = allowed
        .authority()
        .and_then(Authority::approval_id)
        .unwrap();

    assert!(ledger.forget_approval(id).unwrap());
    let mut prompts = ScriptedPrompt::denying();
    let decision = policy.decide(&operation, &ledger, &mut prompts).unwrap();
    assert_eq!(decision, Decision::Denied(DenyReason::Declined));
    assert_eq!(prompts.asked.len(), 1);
}

#[test]
fn an_expired_approval_is_not_an_approval() {
    // A one-second window, so the lapse is observed rather than assumed. The
    // ledger refuses to store an approval that is already expired, which is the
    // right rule, and it means the only way to reach this state is the real one:
    // time passing.
    let mut config = config();
    config.approval_remember_secs = 1;
    let policy = Policy::new(config);
    let ledger = store();
    let operation = op(
        OperationKind::RunCommand,
        &["delete_permanent"],
        &["Remove-Item", "-Recurse", "C:\\temp\\jean-fixture"],
    );

    let mut first = ScriptedPrompt::remembering();
    let allowed = policy.decide(&operation, &ledger, &mut first).unwrap();
    assert!(allowed.is_allowed(), "{allowed}");
    let granted = ledger
        .find_approval(&operation.scope_digest())
        .unwrap()
        .expect("just granted");
    assert!(
        !granted.is_expired(jean_store::now_ms()),
        "the window has not passed yet"
    );

    // Inside the window: allowed, and nobody is asked.
    let mut silent = ScriptedPrompt::default();
    let reused = policy.decide(&operation, &ledger, &mut silent).unwrap();
    assert!(reused.is_allowed(), "{reused}");
    assert!(silent.asked.is_empty(), "a live grant must not re-prompt");

    std::thread::sleep(std::time::Duration::from_millis(1_200));
    assert!(
        ledger
            .find_approval(&operation.scope_digest())
            .unwrap()
            .is_none(),
        "an expired row must not be returned as usable"
    );

    // A grant whose window has passed prompts rather than silently allowing.
    let mut prompts = ScriptedPrompt::denying();
    let decision = policy.decide(&operation, &ledger, &mut prompts).unwrap();
    assert_eq!(decision, Decision::Denied(DenyReason::Declined));
    assert_eq!(prompts.asked.len(), 1, "the expired grant prompted again");
}

#[test]
fn remembering_is_refused_when_the_configuration_says_no() {
    let mut config = config();
    config.allow_remember = false;
    let policy = Policy::new(config);
    let ledger = store();
    let mut prompter = ScriptedPrompt::remembering();

    let error = policy
        .decide(
            &op(
                OperationKind::RunCommand,
                &["service_control"],
                &["Stop-Service", "Spooler"],
            ),
            &ledger,
            &mut prompter,
        )
        .expect_err("an answer that was not offered cannot be honoured");
    assert!(
        matches!(error, EngineError::RememberNotAllowed),
        "{error:?}"
    );
    assert_eq!(
        ledger.count("approvals").unwrap(),
        0,
        "a refused remember must not write a row"
    );
}

#[test]
fn the_scope_digest_separates_fields_no_argument_can_smuggle() {
    // `["a\u{1f}b"]` and `["a", "b"]` are different operations. Joined on any
    // separator character they serialize to the same bytes, and one approval would
    // then cover the other — which is how a prompt bypass would be written into a
    // tool's argument vector. The digest is length-prefixed for this reason.
    let base = op(OperationKind::RunCommand, &["elevated"], &[]);
    let mut smuggled = base.clone();
    smuggled.scope = Scope::new("exe", vec!["a\u{1f}b".to_owned()]);
    let mut split = base.clone();
    split.scope = Scope::new("exe", vec!["a".to_owned(), "b".to_owned()]);
    assert_ne!(
        smuggled.scope_digest(),
        split.scope_digest(),
        "a separator character inside an argument must not blur two scopes"
    );

    // The same trap between the target and the first argument.
    let mut target_joined = base.clone();
    target_joined.scope = Scope::new("exe\u{1f}arg", vec!["arg".to_owned()]);
    let mut target_plain = base.clone();
    target_plain.scope = Scope::new("exe", vec!["arg".to_owned()]);
    assert_ne!(
        target_joined.scope_digest(),
        target_plain.scope_digest(),
        "a target ending in the separator must not look like a shorter target plus an argument"
    );

    // One argument versus two, where the second is empty: the argument count is part
    // of the digest, so these are not the same act.
    let mut one = base.clone();
    one.scope = Scope::new("exe", vec!["a".to_owned()]);
    let mut two = base.clone();
    two.scope = Scope::new("exe", vec!["a".to_owned(), String::new()]);
    assert_ne!(one.scope_digest(), two.scope_digest());

    // Multibyte arguments: the prefix is a byte count, so a value full of emoji is
    // framed by its bytes and not by a character count that would cut it mid-value.
    let mut wide = base.clone();
    wide.scope = Scope::new("exe", vec!["🧪 test".to_owned()]);
    let mut wide_split = base.clone();
    wide_split.scope = Scope::new("exe", vec!["🧪".to_owned(), " test".to_owned()]);
    assert_ne!(wide.scope_digest(), wide_split.scope_digest());

    // Same scope, same digest — and stable across clones of the operation.
    assert_eq!(
        base.scope_digest(),
        op(OperationKind::RunCommand, &["elevated"], &[]).scope_digest()
    );
    // Different operation kind, different digest.
    let mut other_kind = base.clone();
    other_kind.kind = OperationKind::LaunchApp;
    assert_ne!(base.scope_digest(), other_kind.scope_digest());
    // Different tool, different digest.
    let mut other_tool = base.clone();
    other_tool.tool_id = Some("other".to_owned());
    assert_ne!(base.scope_digest(), other_tool.scope_digest());
    // A digest is a fixed-width lowercase hex string, so it can be pasted into a
    // `sqlite3` prompt without quoting surprises.
    let digest = base.scope_digest();
    assert_eq!(digest.len(), 16, "{digest}");
    assert!(
        digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
        "{digest}"
    );
}

#[test]
fn an_operation_with_nothing_sensitive_about_it_is_allowed_even_when_the_ledger_is_empty() {
    // The inverse of the auto-deny test: the default lists must not accidentally
    // cover the ordinary case. If `read_only` work ever matched `require_approval`,
    // the CLI would prompt on every command and the requirement would be dead.
    let policy = Policy::new(config());
    let ledger = store();
    let mut prompter = ScriptedPrompt::default();

    let mut inspect = op(OperationKind::InspectProcess, &[], &["--pid", "1"]);
    inspect.tier = PolicyTier::ReadOnly;
    assert_eq!(
        policy.decide(&inspect, &ledger, &mut prompter).unwrap(),
        Decision::allowed(Authority::OwnerCommand)
    );
    assert!(prompter.asked.is_empty());
}

#[test]
fn the_prompt_shows_why_it_is_being_asked() {
    let policy = Policy::new(config());
    let ledger = store();
    let mut prompter = ScriptedPrompt::approving();

    let operation = op(
        OperationKind::RunCommand,
        &["write_outside_workspace", "credential_read"],
        &["Set-Content", "C:\\Users\\other"],
    );
    policy.decide(&operation, &ledger, &mut prompter).unwrap();

    // Re-render the question that was asked, to check it carries the two
    // capabilities and not just the first.
    let sensitive: Vec<String> = operation
        .capabilities
        .iter()
        .filter(|c| policy.requires_approval(c))
        .cloned()
        .collect();
    let shown = jean_policy::prompt::Prompt {
        summary: &operation.summary,
        operation: operation.kind.as_str(),
        tool_id: operation.tool_id.as_deref(),
        tier: operation.tier.as_str(),
        capabilities: &sensitive,
        allow_remember: true,
    }
    .render();

    assert!(shown.contains(&operation.summary), "{shown}");
    assert!(shown.contains("write_outside_workspace"), "{shown}");
    assert!(shown.contains("credential_read"), "{shown}");
    assert!(shown.contains("[a] yes, and remember"), "{shown}");
    assert_eq!(
        sensitive,
        vec![
            "write_outside_workspace".to_owned(),
            "credential_read".to_owned()
        ],
        "only the capabilities that matched are shown"
    );
}
