//! The operator's authority for one run, decided by the engine that already decides everything.
//!
//! These tests are the executable form of the four properties a run grant has to have to be worth
//! having: it answers the step it was minted for and nothing else, it cannot reach a refusal, it
//! cannot reach the elevation boundary, and it leaves no trace in the ledger for a later run to
//! pick up. `ScriptedPrompt::default()` errors when it is asked a question nobody scripted, so
//! "answered without a prompt" cannot pass by accident, and `approving()` makes "was asked" visible
//! as a count rather than as a silence.

use jean_policy::basis::Authority;
use jean_policy::decision::{Decision, DenyReason};
use jean_policy::grant::{GrantScope, RunGrant};
use jean_policy::operation::{Operation, OperationKind, Scope};
use jean_policy::prompt::ScriptedPrompt;
use jean_policy::{Policy, RunAuthority};
use jean_protocol::config::PolicyConfig;
use jean_protocol::tool::PolicyTier;
use jean_store::Store;

const PLAN: &str = "plan-1";
const GATED: &str = "network_egress_new_domain";

fn store() -> Store {
    Store::open_in_memory().expect("in-memory ledger")
}

fn config() -> PolicyConfig {
    // The shipped list, so the capability under test really is one a person is asked about.
    PolicyConfig::default()
}

/// A normal-tier operation that `require_approval` mentions.
fn gated(arguments: &[&str]) -> Operation {
    Operation {
        kind: OperationKind::RunCommand,
        tool_id: Some("report.send".to_owned()),
        tier: PolicyTier::Normal,
        capabilities: vec![GATED.to_owned()],
        summary: format!("send the report {}", arguments.join(" ")),
        scope: Scope::new(
            "C:\\tools\\report.exe",
            arguments.iter().map(|a| (*a).to_string()).collect(),
        ),
    }
}

/// A grant minted from the resolved scope of each of three steps, exactly as the plan runner does.
fn grant_for(steps: &[(&str, Operation)]) -> RunGrant {
    RunGrant::new(
        "task-run-1",
        PLAN,
        steps
            .iter()
            .map(|(key, operation)| (key.to_string(), operation.scope_digest()))
            .collect(),
    )
}

#[test]
fn a_run_grant_answers_the_step_it_was_minted_for_and_nothing_else() {
    let policy = Policy::new(config());
    let ledger = store();
    let first = gated(&["-to", "finance"]);
    let second = gated(&["-to", "legal"]);
    let grant = grant_for(&[("s1", first.clone()), ("s2", second.clone())]);

    // The step the operator was shown, at the scope it resolved to.
    let mut unanswered = ScriptedPrompt::default();
    let decision = policy
        .decide_granted(
            &first,
            &ledger,
            &mut unanswered,
            Some(GrantScope::new(&grant, PLAN, "s1")),
        )
        .expect("a covered step is decided without a question");
    assert_eq!(decision, Decision::allowed(Authority::RunScope));
    assert!(unanswered.asked.is_empty(), "a covered step asked a person");

    // Three ways the same grant must not answer, each tried against the same ledger. The step it
    // does cover is asserted separately, below.
    let cases: Vec<(&str, Operation, &str, &str)> = vec![
        (
            "a step that is not in the grant",
            gated(&["-to", "hr"]),
            PLAN,
            "s9",
        ),
        ("a different plan", first.clone(), "plan-2", "s1"),
        (
            "the same step at a drifted scope",
            gated(&["-to", "finance", "-cc", "board"]),
            PLAN,
            "s1",
        ),
    ];
    for (what, operation, plan_id, step_key) in cases {
        let mut asked = ScriptedPrompt::approving();
        let outcome = policy
            .decide_granted(
                &operation,
                &ledger,
                &mut asked,
                Some(GrantScope::new(&grant, plan_id, step_key)),
            )
            .expect("an uncovered step is decided by asking");
        assert_eq!(
            asked.asked.len(),
            1,
            "{what}: the grant answered for something it never covered"
        );
        assert_eq!(
            outcome,
            Decision::allowed(Authority::ApprovedThisTime),
            "{what}: a person was asked and their yes must be the authority recorded"
        );
    }

    // The "another step" case is worth stating on its own terms: s2 has its own entry and its own
    // digest, so consulting the grant for s2 answers s2 and not s1.
    let mut none = ScriptedPrompt::default();
    let decision = policy
        .decide_granted(
            &second,
            &ledger,
            &mut none,
            Some(GrantScope::new(&grant, PLAN, "s2")),
        )
        .expect("s2 has its own entry");
    assert_eq!(decision, Decision::allowed(Authority::RunScope));
}

#[test]
fn a_run_grant_cannot_reach_a_capability_the_configuration_refuses() {
    // The same capability on both lists, which is the case the engine resolves by failing closed.
    let mut config = config();
    config.auto_deny = vec![GATED.to_owned()];
    let policy = Policy::new(config);
    let ledger = store();
    let operation = gated(&["-to", "finance"]);
    let grant = grant_for(&[("s1", operation.clone())]);

    let mut prompter = ScriptedPrompt::approving();
    let decision = policy
        .decide_granted(
            &operation,
            &ledger,
            &mut prompter,
            Some(GrantScope::new(&grant, PLAN, "s1")),
        )
        .expect("a denial is a result, not an error");
    assert_eq!(
        decision,
        Decision::Denied(DenyReason::AutoDenied {
            capability: GATED.to_owned()
        }),
        "a grant must not turn a refusal into an approval"
    );
    assert!(
        prompter.asked.is_empty(),
        "and must not turn it into a question either: {:?}",
        prompter.asked
    );
}

#[test]
fn a_run_grant_cannot_reach_the_elevation_boundary() {
    // D-010: this build has no administrator token, so no authority — remembered, granted, or
    // offered — has ever reached this operation. A run grant is the first new kind of authority, and
    // this is the test that says it is not a way past that.
    let policy = Policy::new(config());
    let ledger = store();
    let mut operation = gated(&["-as-admin"]);
    operation.tier = PolicyTier::Elevated;
    let grant = grant_for(&[("s1", operation.clone())]);

    let mut prompter = ScriptedPrompt::approving();
    let decision = policy
        .decide_granted(
            &operation,
            &ledger,
            &mut prompter,
            Some(GrantScope::new(&grant, PLAN, "s1")),
        )
        .expect("a refusal at the boundary is a result");
    assert!(
        matches!(
            decision,
            Decision::Denied(DenyReason::ElevationRequired { .. })
        ),
        "the integrity level is decided before any grant is consulted, got {decision:?}"
    );
    assert!(prompter.asked.is_empty(), "{:?}", prompter.asked);
}

#[test]
fn a_run_grant_leaves_the_approval_ledger_empty() {
    // The ledger is how an answer outlives a run. A grant that wrote to it would be a remembered
    // approval the next process could match, which is the opposite of what it is for.
    let policy = Policy::new(config());
    let ledger = store();
    let operation = gated(&["-to", "finance"]);
    let grant = grant_for(&[("s1", operation.clone())]);
    let mut prompter = ScriptedPrompt::default();

    policy
        .decide_granted(
            &operation,
            &ledger,
            &mut prompter,
            Some(GrantScope::new(&grant, PLAN, "s1")),
        )
        .expect("covered by the run");

    assert!(
        ledger
            .find_approval(&operation.scope_digest())
            .expect("the ledger is readable")
            .is_none(),
        "a run grant must not be stored as an approval"
    );
    // A second run with no operator behind it asks again, from the same ledger.
    let mut asked = ScriptedPrompt::approving();
    let decision = policy
        .decide_granted(&operation, &ledger, &mut asked, None)
        .expect("the next run is decided normally");
    assert_eq!(
        asked.asked.len(),
        1,
        "the previous run's authority carried over"
    );
    assert_eq!(decision, Decision::allowed(Authority::ApprovedThisTime));
}

#[test]
fn authority_without_an_operator_mints_nothing_and_prompts_exactly_as_before() {
    // The enum is the only door, and `PromptEachStep` does not open it: a plan runner configured
    // this way behaves like Phase 1, which is what `jean do --prompt-each` promises.
    assert!(
        !RunAuthority::PromptEachStep.grants(),
        "a delegated run must not mint a grant"
    );
    assert!(RunAuthority::OperatorRun.grants());
    assert_eq!(RunAuthority::PromptEachStep.as_str(), "prompt_each_step");
    assert_eq!(RunAuthority::OperatorRun.as_str(), "operator_run");

    let policy = Policy::new(config());
    let ledger = store();
    let operation = gated(&["-to", "finance"]);
    let mut asked = ScriptedPrompt::approving();
    let decision = policy
        .decide_granted(&operation, &ledger, &mut asked, None)
        .expect("no grant, so the gate");
    assert_eq!(asked.asked.len(), 1);
    assert_eq!(decision, Decision::allowed(Authority::ApprovedThisTime));
}

#[test]
fn a_grant_names_the_run_and_plan_it_came_from_and_grows_no_more() {
    // What the operator is shown before the run starts has to be the same set the engine consults.
    let first = gated(&["-to", "finance"]);
    let second = gated(&["-to", "legal"]);
    let grant = grant_for(&[("s1", first), ("s2", second)]);
    assert_eq!(grant.covered_steps(), 2);
    assert_eq!(grant.run_id(), "task-run-1");
    assert_eq!(grant.plan_id(), PLAN);
    assert_eq!(
        grant.covered_step_keys(),
        vec!["s1", "s2"],
        "the covered set is what the plan said it was"
    );

    // Nothing in this type adds an entry after construction: a step that appears later in the run
    // (a replan, a tool that asked for one more thing) is not in the set and cannot be.
    let later = gated(&["-to", "board"]);
    assert!(
        !grant.covers(PLAN, "s3", &later),
        "an unminted step is not covered, whatever the run printed"
    );
}
