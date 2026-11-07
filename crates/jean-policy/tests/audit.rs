//! The audit trail: what is written, and what is never written.
//!
//! The redaction tests here are the ones to keep. A secret reaching this file is
//! the failure mode an operator cannot see later — the log looks fine, and the
//! credential is in it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use jean_policy::audit::{AuditEvent, AuditWriter};
use jean_policy::decision::{Decision, DenyReason};
use jean_policy::operation::{Operation, OperationKind, Scope};
use jean_protocol::redact::Redactor;
use jean_protocol::tool::PolicyTier;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_path(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "jean-audit-{}-{}-{}.jsonl",
        tag,
        std::process::id(),
        n
    ))
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// The patterns as configured, so a test passing here passes against the filter the
/// product actually runs.
fn redactor() -> Redactor {
    Redactor::new(&jean_protocol::config::default_redact_patterns())
}

fn operation(arguments: &[&str]) -> Operation {
    Operation {
        kind: OperationKind::RunCommand,
        tool_id: Some("powershell.echo".to_owned()),
        tier: PolicyTier::Normal,
        capabilities: vec!["write_outside_workspace".to_owned()],
        summary: "run a command".to_owned(),
        scope: Scope::new(
            "powershell.exe",
            arguments.iter().map(|a| (*a).to_string()).collect(),
        ),
    }
}

fn event(path: &Path, arguments: &[&str]) -> AuditEvent {
    AuditEvent {
        at_ms: jean_store::now_ms(),
        actor: "test-actor".to_owned(),
        task_id: Some("task-1".to_owned()),
        operation: "run_command".to_owned(),
        tool_id: Some("powershell.echo".to_owned()),
        arguments: arguments.iter().map(|a| (*a).to_string()).collect(),
        target: Some("powershell.exe".to_owned()),
        working_dir: Some(path.parent().unwrap().display().to_string()),
        tier: "normal".to_owned(),
        capabilities: vec!["write_outside_workspace".to_owned()],
        decision: "allowed".to_owned(),
        authority: Some("owner_command".to_owned()),
        denial_reason: None,
        approval_id: None,
        outcome: Some("succeeded".to_owned()),
        exit_code: Some(0),
        termination: Some("exited".to_owned()),
        duration_ms: Some(12),
        notes: None,
    }
}

fn lines(path: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(path).expect("the log is readable");
    text.lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("line is not one JSON object: {e}\n{line}"))
        })
        .collect()
}

#[test]
fn a_record_is_one_json_object_per_line_with_the_documented_fields() {
    let path = temp_path("record");
    let writer = AuditWriter::new(path.clone(), true, redactor());

    writer
        .record(&event(&path, &["-Command", "Get-Process", "notepad"]))
        .expect("writing must succeed");

    let written = lines(&path);
    assert_eq!(written.len(), 1);
    let record = &written[0];
    for field in [
        "at_ms",
        "actor",
        "task_id",
        "operation",
        "tool_id",
        "arguments",
        "target",
        "working_dir",
        "tier",
        "capabilities",
        "decision",
        "authority",
        "outcome",
        "exit_code",
        "termination",
        "duration_ms",
    ] {
        assert!(
            record.get(field).is_some(),
            "the roadmap's audit fields must be present; {field} is missing from {record}"
        );
    }
    assert_eq!(record["decision"], "allowed");
    assert_eq!(record["authority"], "owner_command");
    assert_eq!(record["arguments"].as_array().unwrap().len(), 3);

    cleanup(&path);
}

#[test]
fn secrets_are_redacted_by_the_writer_not_by_the_caller() {
    // The property that matters: a caller that forgot to redact still cannot write a
    // secret. This is why redaction lives inside `record`.
    let path = temp_path("redact");
    let writer = AuditWriter::new(path.clone(), true, redactor());

    writer
        .record(&event(
            &path,
            &[
                "-Command",
                "Set-Content -Password hunter2-SECRET-9999",
                "token=sk-SECRETVALUE0123456",
            ],
        ))
        .expect("written");

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        !raw.contains("hunter2-SECRET-9999"),
        "a password in the argument vector reached the log: {raw}"
    );
    assert!(
        !raw.contains("sk-SECRETVALUE0123456"),
        "a prefixed token reached the log: {raw}"
    );
    assert!(raw.contains("[REDACTED]"), "nothing was redacted: {raw}");
    // The key name survives, so the log still says which argument was sensitive.
    assert!(
        raw.contains("Password"),
        "redaction should not erase the shape: {raw}"
    );

    cleanup(&path);
}

#[test]
fn a_denied_operation_is_written_because_that_is_the_interesting_case() {
    let path = temp_path("denied");
    let writer = AuditWriter::new(path.clone(), true, redactor());
    let op = operation(&["Format-Volume", "C:"]);

    let decision = Decision::Denied(DenyReason::AutoDenied {
        capability: "format_volume".to_owned(),
    });
    let record = AuditEvent::for_decision(&op, &decision, "test-actor".to_owned());
    assert_eq!(record.decision, "denied");
    assert_eq!(record.denial_reason.as_deref(), Some("auto_denied"));
    assert!(record.authority.is_none());

    writer.record(&record).expect("a denial is auditable");
    let written = lines(&path);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0]["decision"], "denied");
    assert_eq!(written[0]["denial_reason"], "auto_denied");
    assert_eq!(written[0]["operation"], "run_command");

    cleanup(&path);
}

#[test]
fn an_allowed_remembered_approval_records_which_row_covered_it() {
    let path = temp_path("remembered");
    let writer = AuditWriter::new(path.clone(), true, redactor());
    let op = operation(&["git", "push"]);

    let decision = Decision::allowed(jean_policy::basis::Authority::Remembered {
        id: 41,
        granted_at_ms: 1,
    });
    let record = AuditEvent::for_decision(&op, &decision, "test-actor".to_owned());
    writer.record(&record).unwrap();

    let written = lines(&path);
    assert_eq!(written[0]["authority"], "remembered_approval");
    assert_eq!(
        written[0]["approval_id"], 41,
        "the log must point at the ledger row that authorised the run"
    );

    cleanup(&path);
}

#[test]
fn the_log_is_append_only_in_the_way_this_build_can_promise() {
    let path = temp_path("append");
    let writer = AuditWriter::new(path.clone(), true, redactor());

    writer.record(&event(&path, &["first"])).unwrap();
    let after_one = std::fs::read_to_string(&path).unwrap();
    writer.record(&event(&path, &["second"])).unwrap();
    let after_two = std::fs::read_to_string(&path).unwrap();

    assert!(
        after_two.starts_with(&after_one),
        "an earlier record was rewritten: {after_two}"
    );
    assert!(after_two.len() > after_one.len(), "the log did not grow");
    let written = lines(&path);
    assert_eq!(written.len(), 2);
    assert_eq!(
        written[0]["arguments"][0], "first",
        "order must be preserved"
    );
    assert_eq!(written[1]["arguments"][0], "second");

    // A second writer on the same file appends rather than truncating, which is the
    // whole of what "append-only" claims here. It is a statement about how the file
    // is opened, not a tamper-resistance guarantee — see the module header.
    let other = AuditWriter::new(path.clone(), true, redactor());
    other.record(&event(&path, &["third"])).unwrap();
    assert_eq!(lines(&path).len(), 3);

    cleanup(&path);
}

#[test]
fn an_argument_containing_a_newline_still_produces_one_line() {
    // A multi-line quoted PowerShell command is legal input. If the writer emitted
    // it raw, the file would no longer be one JSON object per line and every later
    // reader would fail on a record that was never malformed to begin with.
    let path = temp_path("newline");
    let writer = AuditWriter::new(path.clone(), true, redactor());
    writer
        .record(&event(
            &path,
            &["-Command", "Write-Output 1\nWrite-Output 2"],
        ))
        .unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        text.lines().count(),
        1,
        "a newline in an argument broke the line-per-record contract"
    );
    let written = lines(&path);
    assert_eq!(written[0]["arguments"][1], "Write-Output 1\nWrite-Output 2");

    cleanup(&path);
}

#[test]
fn a_disabled_writer_writes_nothing_and_reports_success() {
    let path = temp_path("disabled");
    let _ = std::fs::remove_file(&path);
    let writer = AuditWriter::new(path.clone(), false, redactor());

    writer
        .record(&event(&path, &["-Command", "Get-Date"]))
        .unwrap();
    assert!(
        !path.exists(),
        "policy.audit_enabled = false was configured, and a file appeared anyway"
    );
    assert!(!writer.is_enabled());

    cleanup(&path);
}

#[test]
fn the_parent_directory_is_created_rather_than_the_record_dropped() {
    let mut path = temp_path("nested");
    path.pop();
    let path = path.join("jean-audit-nested-dir").join("audit.jsonl");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    let writer = AuditWriter::new(path.clone(), true, redactor());

    writer.record(&event(&path, &["first"])).unwrap();
    assert!(path.is_file(), "the record was silently lost");
    assert_eq!(lines(&path).len(), 1);

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
