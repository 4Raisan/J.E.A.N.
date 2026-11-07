//! Redaction of what J.E.A.N. *persists*, checked through the real run path.
//!
//! The unit tests in `jean-protocol::redact` prove the patterns work. This file
//! proves the wiring: that a secret arriving in a caller-supplied `--goal`, or in
//! a tool argument, does not reach the database in the clear. The distinction
//! matters because the goal is free text the user types, and a record written
//! before the redactor was applied cannot be corrected afterwards.
//!
//! The run below is the only one, and it covers both surfaces at once — not because a
//! second would fail: `jean_core::signals` re-arms per task since Phase 1, so one
//! process can supervise several tasks in sequence. It is single because redaction is
//! about what lands in the database, and one run writes both surfaces it checks.
//!
//! The tool it runs is `cmd.exe /c echo`, which writes one line and exits.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use jean_core::{run, Context, RunRequest};
use jean_protocol::redact::Redactor;

/// `JEAN_DATA_DIR` is process-global and cargo runs test targets in threads, so
/// anything that reads configuration has to hold this.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const SECRET_IN_GOAL: &str = "hunter2-GOALSECRET-4242";
const SECRET_IN_ARGS: &str = "sk-ARGSECRETabcdef01234";

fn temp_data_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jean-redact-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// A data directory holding only a `tools.toml` with one harmless echo tool, so a
/// secret has two places it could leak into: the caller's goal and the arguments.
///
/// `Paths` resolves an unset `tools.registry_path` to `<data_dir>/tools.toml`, so
/// the registry is found without writing a configuration file.
fn scenario() -> PathBuf {
    let dir = temp_data_dir("run");
    std::fs::create_dir_all(&dir).expect("the temp data directory must be creatable");

    std::fs::write(
        dir.join("tools.toml"),
        r#"config_version = 1

[[tool]]
id = "echo.tool"
kind = "shell"
command = "cmd.exe"
args = ["/c", "echo", "ran"]
verify = { method = "exit_code", expected = 0 }
"#,
    )
    .expect("the registry must be writable");

    dir
}

#[test]
fn persisted_records_never_contain_a_secret_in_the_clear() {
    let dir = scenario();

    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("JEAN_DATA_DIR", &dir);
    let ctx = Context::load(None).expect("a data directory with a valid registry must load");

    let report = run(
        &ctx,
        RunRequest {
            tool_id: "echo.tool".to_owned(),
            args: vec![format!("token={SECRET_IN_ARGS}")],
            goal: Some(format!("sign in with password={SECRET_IN_GOAL}")),
            interactive: None,
            intent: None,
        },
    )
    .expect("the harmless echo tool must run");

    let stored = ctx
        .store
        .require_task(&report.task_id)
        .expect("the task record must be readable");
    let runs = ctx
        .store
        .tool_runs(&report.task_id)
        .expect("the tool runs must be readable");

    std::env::remove_var("JEAN_DATA_DIR");
    drop(guard);

    let args_json = runs
        .last()
        .map(|run| run.args_json.clone())
        .unwrap_or_default();
    cleanup(&dir);

    assert_eq!(report.state.as_str(), "succeeded", "{report:?}");

    // The goal is free text, and it is the field the run path used to miss.
    assert!(
        !stored.goal.contains(SECRET_IN_GOAL),
        "the persisted goal leaked the secret: {:?}",
        stored.goal
    );
    assert!(
        stored.goal.contains("[REDACTED]"),
        "the persisted goal should show that something was removed: {:?}",
        stored.goal
    );
    assert!(
        stored.goal.contains("password="),
        "redaction must keep the key name so the record stays diagnosable: {:?}",
        stored.goal
    );

    // The argument vector reaches both the task row and the run row.
    assert!(
        !stored.request_json.contains(SECRET_IN_ARGS),
        "the request JSON leaked the secret: {:?}",
        stored.request_json
    );
    assert!(
        !args_json.contains(SECRET_IN_ARGS),
        "the tool run's arguments leaked the secret: {args_json}"
    );
    assert!(
        args_json.contains("[REDACTED]"),
        "the run row should record that redaction happened: {args_json}"
    );
}

/// The conventions in `default_redact_patterns` are what the assertions above
/// depend on: the value disappears and the key name stays readable.
#[test]
fn redaction_keeps_the_key_name_and_removes_the_value() {
    let redactor = Redactor::new(&jean_protocol::config::default_redact_patterns());
    let text = redactor.redact("connect api_key=ghp_AAAABBBBCCCCDDDD with bearer zzz-111-222");
    assert!(!text.contains("AAAABBBBCCCCDDDD"), "{text}");
    assert!(!text.contains("zzz-111-222"), "{text}");
    assert!(text.contains("api_key="), "{text}");
    assert!(text.contains("[REDACTED]"), "{text}");
}
