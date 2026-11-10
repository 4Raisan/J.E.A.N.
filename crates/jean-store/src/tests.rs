//! Unit tests for the persistence layer.
//!
//! These cover the Phase-0 deliverables that are observable without launching a
//! process: the migration runner, WAL, the state machine being the only writer
//! of `tasks.state`, and the crash-recovery primitives (`runner_pid` +
//! `runner_created_ms`, `in_flight_tasks`). Process behaviour is tested in
//! `jean-process`; the two are kept apart so this crate's tests never touch the
//! Win32 API.

use super::*;
use jean_protocol::state::ALL_STATES;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A throwaway on-disk database. File-backed rather than `:memory:` for the
/// tests that need to prove something survived a reopen — which is the whole
/// point of the crash-recovery requirement.
fn temp_db(tag: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "jean-store-test-{}-{}-{}.sqlite3",
        tag,
        std::process::id(),
        n
    ))
}

fn cleanup(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(p));
    }
}

fn task(id: &str) -> TaskRecord {
    TaskRecord::new(
        id.to_owned(),
        format!("run tool for {id}"),
        Some("powershell.echo".to_owned()),
        r#"{"args":["Write-Output","hi"]}"#.to_owned(),
    )
}

#[test]
fn a_fresh_database_is_migrated_to_the_current_version() {
    let store = Store::open_in_memory().expect("open in-memory store");
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION as i64);
    assert_eq!(store.count("tasks").unwrap(), 0);
    assert_eq!(store.count("tool_runs").unwrap(), 0);
    assert_eq!(store.count("events").unwrap(), 0);
}

#[test]
fn opening_twice_does_not_reapply_migrations() {
    let path = temp_db("reopen");
    {
        let store = Store::open(&path, "wal", Duration::from_secs(5)).expect("first open");
        store.insert_task(&task("t1")).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION as i64);
    }
    {
        let store = Store::open(&path, "wal", Duration::from_secs(5)).expect("second open");
        assert_eq!(
            store.schema_version().unwrap(),
            SCHEMA_VERSION as i64,
            "reopening must not bump user_version; if it did, migrations are \
             not idempotent and a restart would corrupt the database"
        );
        assert_eq!(store.count("tasks").unwrap(), 1, "the task survived");
        assert_eq!(store.require_task("t1").unwrap().state, TaskState::Queued);
    }
    cleanup(&path);
}

#[test]
fn concurrent_first_open_serializes_schema_migrations() {
    let path = temp_db("concurrent-first-open");
    cleanup(&path);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                Store::open(&path, "wal", Duration::from_secs(10)).expect(
                    "every simultaneous first opener must wait for the migration transaction",
                )
            })
        })
        .collect();
    for thread in threads {
        let store = thread.join().expect("opener thread must not panic");
        assert_eq!(store.count("tasks").unwrap(), 0);
    }
    drop(Store::open(&path, "wal", Duration::from_secs(5)).unwrap());
    cleanup(&path);
}

#[test]
fn a_file_backed_database_uses_wal() {
    let path = temp_db("wal");
    let store = Store::open(&path, "wal", Duration::from_secs(5)).expect("open");
    assert_eq!(
        store.journal_mode().unwrap().to_lowercase(),
        "wal",
        "WAL is required so a second `jean tasks` can read while a supervisor \
         writes; see PLANNING/05-ROADMAP.md Phase 0"
    );
    drop(store);
    cleanup(&path);
}

#[test]
fn a_database_from_the_future_is_refused() {
    let path = temp_db("future");
    {
        let store = Store::open(&path, "wal", Duration::from_secs(5)).unwrap();
        store.insert_task(&task("t1")).unwrap();
    }
    {
        // Simulate a newer build having migrated further.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", (SCHEMA_VERSION + 1) as i64)
            .unwrap();
    }
    let err =
        Store::open(&path, "wal", Duration::from_secs(5)).expect_err("must refuse a newer schema");
    // The refused version is stated, not pattern-matched as a literal: this test
    // has to keep passing when a migration is added, and the point of it is that
    // the refusal carries the number the file actually claimed.
    assert!(
        matches!(&err, StoreError::FromTheFuture { found } if *found == i64::from(SCHEMA_VERSION) + 1),
        "unexpected error: {err:?}"
    );
    assert!(
        err.to_string().contains("newer than this build"),
        "the message must explain why it refused: {err}"
    );
    cleanup(&path);
}

#[test]
fn a_task_round_trips_through_the_database() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t-abc")).unwrap();

    let got = store.require_task("t-abc").unwrap();
    assert_eq!(got.id, "t-abc");
    assert_eq!(got.goal, "run tool for t-abc");
    assert_eq!(got.tool_id.as_deref(), Some("powershell.echo"));
    assert_eq!(got.state, TaskState::Queued);
    assert!(got.runner_pid.is_none());
    assert!(got.finished_at_ms.is_none());
    assert_eq!(got.request_json, r#"{"args":["Write-Output","hi"]}"#);
    assert!(got.is_in_flight(), "Queued counts as unfinished");
}

#[test]
fn a_missing_task_is_a_named_error_not_a_panic() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.task("nope").unwrap().is_none());
    let err = store.require_task("nope").unwrap_err();
    assert!(matches!(err, StoreError::NoSuchTask(ref id) if id == "nope"));
    assert!(err.to_string().contains("nope"), "{err}");
}

#[test]
fn inserting_the_same_task_twice_is_rejected_by_the_primary_key() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("dup")).unwrap();
    let err = store.insert_task(&task("dup")).unwrap_err();
    assert!(matches!(err, StoreError::Sqlite { .. }), "{err:?}");
    assert_eq!(store.count("tasks").unwrap(), 1);
}

#[test]
fn a_legal_transition_moves_the_state_and_records_why() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t1")).unwrap();

    let after = store
        .transition("t1", TaskState::Running, Some("child spawned"))
        .expect("queued -> running is legal");
    assert_eq!(after.state, TaskState::Running);
    assert!(
        after.started_at_ms.is_some(),
        "entering Running must stamp started_at_ms"
    );

    let events = store.events("t1").unwrap();
    let state_events: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::StateChanged)
        .collect();
    assert_eq!(state_events.len(), 1);
    assert!(
        state_events[0].detail.contains("running"),
        "the event must name the state it moved to: {}",
        state_events[0].detail
    );
    assert!(
        state_events[0].detail.contains("child spawned"),
        "the event must carry the caller's reason: {}",
        state_events[0].detail
    );
}

#[test]
fn an_illegal_transition_is_refused_and_changes_nothing() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t1")).unwrap();

    // Queued -> Succeeded skips Running and Verifying.
    let err = store
        .transition("t1", TaskState::Succeeded, None)
        .expect_err("queued -> succeeded must be illegal");
    assert!(matches!(err, StoreError::Transition(_)), "{err:?}");

    let unchanged = store.require_task("t1").unwrap();
    assert_eq!(
        unchanged.state,
        TaskState::Queued,
        "a refused transition must not partially write"
    );
    assert!(unchanged.finished_at_ms.is_none());
    assert_eq!(
        store
            .events("t1")
            .unwrap()
            .iter()
            .filter(|e| e.kind == EventKind::StateChanged)
            .count(),
        0,
        "a refused transition must not write an event either"
    );
}

#[test]
fn a_running_task_cannot_claim_success_without_being_verified() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t1")).unwrap();
    store.transition("t1", TaskState::Running, None).unwrap();

    let err = store
        .transition("t1", TaskState::Succeeded, None)
        .expect_err("Running -> Succeeded must route through Verifying");
    assert!(matches!(err, StoreError::Transition(_)), "{err:?}");

    // The legal route, spelled out so the test fails loudly if Verifying is
    // ever dropped: this is the guard behind "an exit code of 0 does not prove
    // the user's goal was met" (IMPLEMENTATION_GUIDE.md §6).
    store.transition("t1", TaskState::Verifying, None).unwrap();
    store.transition("t1", TaskState::Succeeded, None).unwrap();
    assert_eq!(
        store.require_task("t1").unwrap().state,
        TaskState::Succeeded
    );
}

#[test]
fn transitioning_a_missing_task_reports_the_task_not_the_state() {
    let store = Store::open_in_memory().unwrap();
    let err = store
        .transition("ghost", TaskState::Running, None)
        .unwrap_err();
    assert!(matches!(err, StoreError::NoSuchTask(_)), "{err:?}");
}

#[test]
fn a_terminal_state_accepts_no_further_transitions() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t1")).unwrap();
    store.transition("t1", TaskState::Running, None).unwrap();
    store
        .transition("t1", TaskState::Cancelled, Some("user"))
        .unwrap();

    for next in ALL_STATES {
        let res = store.transition("t1", next, None);
        assert!(
            res.is_err(),
            "cancelled -> {next} must be refused; a finished task that can still \
             move is a task whose history cannot be trusted"
        );
    }
    assert_eq!(
        store.require_task("t1").unwrap().state,
        TaskState::Cancelled
    );
}

#[test]
fn in_flight_lists_exactly_the_unfinished_tasks() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("live")).unwrap();
    store.insert_task(&task("done")).unwrap();
    store.insert_task(&task("stuck")).unwrap();

    store.transition("live", TaskState::Running, None).unwrap();
    store.transition("done", TaskState::Running, None).unwrap();
    store
        .transition("done", TaskState::Verifying, None)
        .unwrap();
    store
        .transition("done", TaskState::Succeeded, None)
        .unwrap();
    store.transition("stuck", TaskState::Running, None).unwrap();
    store
        .transition("stuck", TaskState::Cancelled, None)
        .unwrap();

    let ids: Vec<String> = store
        .in_flight_tasks()
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, vec!["live"], "got {ids:?}");

    assert!(store
        .list_tasks(TaskFilter::State(TaskState::Queued), 10)
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .list_tasks(TaskFilter::State(TaskState::Succeeded), 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.list_tasks(TaskFilter::All, 10).unwrap().len(), 3);
}

#[test]
fn the_list_limit_is_honoured() {
    let store = Store::open_in_memory().unwrap();
    for i in 0..5 {
        store.insert_task(&task(&format!("t{i}"))).unwrap();
    }
    assert_eq!(store.list_tasks(TaskFilter::All, 2).unwrap().len(), 2);
    assert_eq!(store.list_tasks(TaskFilter::All, 100).unwrap().len(), 5);
    assert_eq!(store.list_tasks(TaskFilter::All, 0).unwrap().len(), 0);
}

#[test]
fn runner_ownership_can_be_recorded_and_cleared() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t1")).unwrap();

    store.set_runner("t1", 4242, 1_700_000_000_000).unwrap();
    let r = store.require_task("t1").unwrap();
    assert_eq!(r.runner_pid, Some(4242));
    assert_eq!(r.runner_created_ms, Some(1_700_000_000_000));

    store.clear_runner("t1").unwrap();
    let r = store.require_task("t1").unwrap();
    assert!(r.runner_pid.is_none());
    assert!(
        r.runner_created_ms.is_none(),
        "clearing the pid without clearing its creation time would leave a \
         half-truth that reconciliation could act on"
    );

    assert!(matches!(
        store.set_runner("ghost", 1, 1).unwrap_err(),
        StoreError::NoSuchTask(_)
    ));
}

#[test]
fn a_tool_run_records_its_outcome_and_output_summary() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t1")).unwrap();

    let run_id = store
        .insert_tool_run(
            "t1",
            "powershell.echo",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            r#"["-NoProfile","-Command","Write-Output hi"]"#,
            Some(r"C:\Users\test"),
            r#"{"PATH":"C:\\Windows\\System32"}"#,
        )
        .unwrap();
    assert!(run_id > 0);

    store.record_child_pid(run_id, 31337).unwrap();

    let stdout = StreamSummary {
        path: Some(r"C:\data\t1\stdout.log".to_owned()),
        bytes: 10_485_761,
        truncated: true,
    };
    let stderr = StreamSummary {
        path: Some(r"C:\data\t1\stderr.log".to_owned()),
        bytes: 12,
        truncated: false,
    };
    store
        .finish_tool_run(run_id, Some(0), Termination::Exited, &stdout, &stderr, None)
        .unwrap();

    let runs = store.tool_runs("t1").unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.id, run_id);
    assert_eq!(run.tool_id, "powershell.echo");
    assert_eq!(run.child_pid, Some(31337));
    assert_eq!(run.exit_code, Some(0));
    assert_eq!(run.termination, Some(Termination::Exited));
    assert_eq!(run.working_dir.as_deref(), Some(r"C:\Users\test"));
    assert_eq!(run.stdout_bytes, 10_485_761);
    assert!(run.stdout_truncated);
    assert_eq!(run.stderr_bytes, 12);
    assert!(!run.stderr_truncated);
    assert!(
        run.ended_at_ms.is_some() && run.started_at_ms.is_some(),
        "both timestamps must be present once finished"
    );
    assert!(run.ended_at_ms.unwrap() >= run.started_at_ms.unwrap());
    assert!(run.error.is_none());
}

#[test]
fn an_unfinished_tool_run_has_no_termination() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t1")).unwrap();
    let run_id = store
        .insert_tool_run("t1", "tool", "exe", "[]", None, "{}")
        .unwrap();
    let run = &store.tool_runs("t1").unwrap()[0];
    assert_eq!(run.id, run_id);
    assert!(run.termination.is_none());
    assert!(run.ended_at_ms.is_none());
    assert!(run.child_pid.is_none());
    assert_eq!(run.stdout_bytes, 0);
    assert_eq!(run.args_json, "[]");
}

#[test]
fn two_runs_of_one_task_are_kept_in_order() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t1")).unwrap();
    let first = store
        .insert_tool_run("t1", "tool", "exe-a", "[]", None, "{}")
        .unwrap();
    let second = store
        .insert_tool_run("t1", "tool", "exe-b", "[]", None, "{}")
        .unwrap();
    let ids: Vec<i64> = store
        .tool_runs("t1")
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![first, second]);
}

#[test]
fn a_tool_run_cannot_reference_a_task_that_does_not_exist() {
    let store = Store::open_in_memory().unwrap();
    let err = store
        .insert_tool_run("ghost", "tool", "exe", "[]", None, "{}")
        .unwrap_err();
    assert!(matches!(err, StoreError::Sqlite { .. }), "{err:?}");
    assert_eq!(store.count("tool_runs").unwrap(), 0);
}

#[test]
fn events_are_returned_in_insertion_order() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t1")).unwrap();
    store
        .append_event("t1", &Event::spawn(1234, "powershell.exe"))
        .unwrap();
    store.append_event("t1", &Event::note("halfway")).unwrap();
    store.append_event("t1", &Event::exit(0)).unwrap();

    let events = store.events("t1").unwrap();
    // One TaskCreated from insert_task, then the three above.
    assert_eq!(events.len(), 4);
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::TaskCreated,
            EventKind::ProcessSpawned,
            EventKind::Note,
            EventKind::ProcessExited
        ]
    );
    let seqs: Vec<i64> = events.iter().map(|e| e.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seq must be strictly increasing, got {seqs:?}"
    );
    assert!(store.events("other-task").unwrap().is_empty());
}

#[test]
fn an_event_kind_from_a_newer_build_reads_as_a_note() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t1")).unwrap();
    // Written behind the type system, the way a newer build would leave it.
    store
        .conn
        .execute(
            "INSERT INTO events (task_id, at_ms, kind, detail) VALUES (?1,?2,?3,?4)",
            params!["t1", now_ms(), "voice_transcript", "hello"],
        )
        .unwrap();

    let last = store.events("t1").unwrap().pop().unwrap();
    assert_eq!(last.kind, EventKind::Note);
    assert_eq!(last.detail, "hello");
}

#[test]
fn pruning_removes_only_events_older_than_the_retention_window() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t1")).unwrap();
    store.append_event("t1", &Event::note("recent")).unwrap();

    // Backdate one event, the way only a clock change or a hand edit could.
    let old = now_ms() - 40 * 86_400_000;
    store
        .conn
        .execute(
            "INSERT INTO events (task_id, at_ms, kind, detail) VALUES (?1,?2,?3,?4)",
            params!["t1", old, "note", "ancient"],
        )
        .unwrap();

    assert_eq!(store.count("events").unwrap(), 3);
    assert_eq!(store.prune_events(30).unwrap(), 1);
    assert_eq!(store.count("events").unwrap(), 2);

    let details: Vec<String> = store
        .events("t1")
        .unwrap()
        .into_iter()
        .map(|e| e.detail)
        .collect();
    assert!(!details.iter().any(|d| d == "ancient"), "{details:?}");
    assert!(details.iter().any(|d| d == "recent"));
}

#[test]
fn count_refuses_a_table_name_it_does_not_know() {
    let store = Store::open_in_memory().unwrap();
    // The parameter is `&'static str` precisely so callers cannot pass user
    // input, but the guard stays: interpolation into SQL should fail closed
    // even against a future refactor that loosens the signature.
    let err = store.count("tasks; DROP TABLE tasks").unwrap_err();
    assert!(matches!(err, StoreError::Serialization { .. }), "{err:?}");
    assert!(store.count("tasks").is_ok());
    assert!(store.count("tool_runs").is_ok());
    assert!(store.count("events").is_ok());
}

#[test]
fn the_schema_rejects_a_state_string_the_application_could_not_parse() {
    let store = Store::open_in_memory().unwrap();
    let err = store
        .conn
        .execute(
            "INSERT INTO tasks (id, goal, tool_id, state, created_at_ms, updated_at_ms, request_json)
             VALUES ('bad', 'g', 'tool', 'teleporting', 1, 1, '{}')",
            [],
        )
        .expect_err("the CHECK constraint must fire");
    assert!(matches!(err, rusqlite::Error::SqliteFailure(..)), "{err:?}");
    assert!(store.task("bad").unwrap().is_none());
}

#[test]
fn the_schema_rejects_an_unknown_termination_kind() {
    let store = Store::open_in_memory().unwrap();
    store.insert_task(&task("t1")).unwrap();
    let run_id = store
        .insert_tool_run("t1", "tool", "exe", "[]", None, "{}")
        .unwrap();
    let err = store
        .conn
        .execute(
            "UPDATE tool_runs SET termination = 'vaporised' WHERE id = ?1",
            params![run_id],
        )
        .expect_err("the CHECK constraint must fire");
    assert!(matches!(err, rusqlite::Error::SqliteFailure(..)), "{err:?}");
    assert!(store.tool_runs("t1").unwrap()[0].termination.is_none());
}

#[test]
fn the_synchronous_pragma_is_configurable() {
    // `store.synchronous` is a real setting, so it has to reach SQLite rather than
    // being read and dropped.
    let path = temp_db("sync");
    {
        let store =
            Store::open_with(&path, "wal", "FULL", Duration::from_secs(5)).expect("open FULL");
        assert_eq!(store.synchronous().unwrap(), 2, "FULL");
        assert_eq!(store.journal_mode().unwrap().to_lowercase(), "wal");
    }
    {
        let store = Store::open(&path, "wal", Duration::from_secs(5)).expect("open default");
        assert_eq!(store.synchronous().unwrap(), 1, "NORMAL is the default");
    }
    cleanup(&path);
}

#[test]
fn every_documented_synchronous_value_is_accepted() {
    // `AppConfig::validate` restricts the setting to exactly these four, so each
    // one must actually reach SQLite. The numbers are how SQLite reports them back.
    let path = temp_db("sync-all");
    for (name, expected) in [("OFF", 0), ("NORMAL", 1), ("FULL", 2), ("EXTRA", 3)] {
        let store = Store::open_with(&path, "wal", name, Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("{name} should be accepted: {e}"));
        assert_eq!(
            store.synchronous().unwrap(),
            expected,
            "{name} was not applied"
        );
    }
    cleanup(&path);
}

#[test]
fn a_new_task_starts_queued_with_no_timestamps_beyond_creation() {
    let t = task("fresh");
    assert_eq!(t.state, TaskState::Queued);
    assert!(t.created_at_ms > 0);
    assert_eq!(t.created_at_ms, t.updated_at_ms);
    assert!(t.started_at_ms.is_none());
    assert!(t.finished_at_ms.is_none());
    assert!(t.outcome.is_none());
}

#[test]
fn format_ms_renders_known_instants_correctly() {
    assert_eq!(format_ms(0), "1970-01-01 00:00:00.000Z");
    assert_eq!(format_ms(1_000), "1970-01-01 00:00:01.000Z");
    assert_eq!(format_ms(86_400_000), "1970-01-02 00:00:00.000Z");
    // A leap day, which is where a hand-rolled calendar is most likely to be wrong.
    assert_eq!(format_ms(1_709_209_996_789), "2024-02-29 12:33:16.789Z");
    // 2000 is divisible by 400, so it *is* a leap year and February had 29 days.
    assert_eq!(format_ms(951_868_800_000), "2000-03-01 00:00:00.000Z");
    // 2023-12-31 23:59:59.999 UTC — the last millisecond before a year rollover.
    assert_eq!(format_ms(1_704_067_199_999), "2023-12-31 23:59:59.999Z");
}

#[test]
fn format_ms_does_not_panic_on_a_clock_before_the_epoch() {
    // i64::MIN is deliberately excluded: `format_ms` walks years one at a time,
    // so an absurdly large magnitude would spin rather than return. Timestamps
    // come from `now_ms`, which clamps to 0, so that input is unreachable.
    let rendered = format_ms(-1);
    assert!(rendered.ends_with('Z'), "{rendered}");
    let rendered = format_ms(-86_400_000);
    assert!(rendered.ends_with('Z'), "{rendered}");
}

#[test]
fn now_ms_is_monotonic_enough_to_order_records() {
    let a = now_ms();
    let b = now_ms();
    assert!(b >= a);
    assert!(
        a > 1_700_000_000_000,
        "timestamps must be post-2023, got {a}"
    );
}

// ── Phase 1: recorded health and remembered approvals ──────────────────────

#[test]
fn health_probes_accumulate_until_the_threshold() {
    let store = Store::open_in_memory().unwrap();

    // Never probed is not the same as broken.
    assert!(store.tool_health("git").unwrap().is_none());

    // One failure is recorded but does not make the tool unhealthy.
    let one = store
        .record_probe("git", false, 3, Some("path missing"))
        .unwrap();
    assert_eq!(one.status, HealthStatus::Unknown);
    assert_eq!(one.consecutive_failures, 1);
    assert_eq!(one.detail.as_deref(), Some("path missing"));

    // A success clears the counter and remembers when it last worked.
    let ok = store.record_probe("git", true, 3, None).unwrap();
    assert_eq!(ok.status, HealthStatus::Healthy);
    assert_eq!(ok.consecutive_failures, 0);
    assert!(ok.last_ok_ms.is_some());

    for expected in [1, 2] {
        let row = store
            .record_probe("git", false, 3, Some("exited 1"))
            .unwrap();
        assert_eq!(row.consecutive_failures, expected);
        // Below the threshold the previous verdict stands, so `jean health` can
        // say "healthy, 1 recent failure" rather than inventing a fourth status.
        assert_eq!(row.status, HealthStatus::Healthy);
        assert!(
            row.last_ok_ms.is_some(),
            "the last good probe is still a fact"
        );
    }

    let third = store
        .record_probe("git", false, 3, Some("exited 1"))
        .unwrap();
    assert_eq!(third.status, HealthStatus::Unhealthy);
    assert_eq!(third.consecutive_failures, 3);

    // And one good probe is enough to clear it again.
    let cleared = store.record_probe("git", true, 3, None).unwrap();
    assert_eq!(cleared.status, HealthStatus::Healthy);
    assert_eq!(store.all_tool_health().unwrap().len(), 1);
}

#[test]
fn a_disabled_tool_is_never_resurrected_by_a_probe() {
    let store = Store::open_in_memory().unwrap();
    store
        .set_disabled("brave", "disabled by configuration")
        .unwrap();
    let row = store.tool_health("brave").unwrap().expect("row");
    assert_eq!(row.status, HealthStatus::Disabled);
    assert_eq!(
        row.last_check_ms, None,
        "disabling a tool is not a check, and must not look like one"
    );

    // If a caller probes it anyway, the operator's switch still wins.
    let after = store.record_probe("brave", true, 3, None).unwrap();
    assert_eq!(after.status, HealthStatus::Disabled);
}

#[test]
fn freshness_is_an_age_rule_with_the_safe_direction_on_a_bad_clock() {
    let store = Store::open_in_memory().unwrap();
    let row = store.record_probe("git", true, 3, None).unwrap();

    let now = row.last_check_ms.expect("just checked");
    assert!(row.is_fresh(Duration::from_secs(300), now + 10));
    assert!(
        !row.is_fresh(Duration::from_secs(1), now + 2_000),
        "a row 2 s old with a 1 s interval is stale and must be re-probed"
    );

    let mut from_the_future = row.clone();
    from_the_future.last_check_ms = Some(now + 10_000);
    assert!(
        !from_the_future.is_fresh(Duration::from_secs(300), now),
        "a clock that jumped backwards must not make a stale row look fresh"
    );

    let mut unchecked = row.clone();
    unchecked.last_check_ms = None;
    assert!(!unchecked.is_fresh(Duration::from_secs(300), now));

    // Unknown means nothing has been looked at, however recent the row claims.
    let never = ToolHealthRecord {
        tool_id: "git".to_owned(),
        status: HealthStatus::Unknown,
        consecutive_failures: 0,
        last_check_ms: Some(now),
        last_ok_ms: None,
        detail: None,
    };
    assert!(!never.is_fresh(Duration::from_secs(300), now));
}

#[test]
fn a_remembered_approval_is_matched_only_by_its_own_digest() {
    let store = Store::open_in_memory().unwrap();
    let later = now_ms() + 60_000;

    let push = NewApproval {
        scope_digest: "digest-push".to_owned(),
        operation: "run_command".to_owned(),
        tool_id: "git".to_owned(),
        tier: "normal".to_owned(),
        summary: "git push origin main".to_owned(),
        expires_at_ms: later,
    };
    let id = store.remember_approval(&push).unwrap();

    // The exact act is covered.
    assert_eq!(
        store.find_approval("digest-push").unwrap().map(|a| a.id),
        Some(id)
    );
    // And nothing else that shares the tool, the tier or the operation. This is
    // the property `tool_and_tier` scoping did not have.
    assert_eq!(store.find_approval("digest-pull").unwrap(), None);
    assert_eq!(store.count("approvals").unwrap(), 1);

    // Re-presenting the same digest replaces rather than accumulates, and resets
    // the usage counter: the operator was asked again, so the history restarts.
    store.touch_approval(id).unwrap();
    store.remember_approval(&push).unwrap();
    let again = store
        .find_approval("digest-push")
        .unwrap()
        .expect("still there");
    assert_eq!(again.used_count, 0);
    assert_eq!(store.count("approvals").unwrap(), 1, "no duplicate row");

    // An approval already expired when offered is refused rather than stored as a
    // permission that can never be used.
    let mut stale = push.clone();
    stale.scope_digest = "digest-stale".to_owned();
    stale.expires_at_ms = now_ms() - 1;
    assert!(store.remember_approval(&stale).is_err());
}

#[test]
fn expiry_and_forgetting_are_reported_honestly() {
    let store = Store::open_in_memory().unwrap();
    let now = now_ms();

    let mut first = NewApproval {
        scope_digest: "a".to_owned(),
        operation: "run_command".to_owned(),
        tool_id: "git".to_owned(),
        tier: "normal".to_owned(),
        summary: "one".to_owned(),
        expires_at_ms: now + 60_000,
    };
    let id_a = store.remember_approval(&first).unwrap();
    first.scope_digest = "b".to_owned();
    first.summary = "two".to_owned();
    first.expires_at_ms = now + 3_600_000;
    store.remember_approval(&first).unwrap();

    assert_eq!(store.list_approvals().unwrap().len(), 2);
    assert!(
        !store.forget_approval(999_999).unwrap(),
        "forgetting a row that does not exist must not report success"
    );
    assert!(store.forget_approval(id_a).unwrap());
    assert_eq!(store.count("approvals").unwrap(), 1);

    assert_eq!(
        store.prune_approvals(0).unwrap(),
        0,
        "the surviving grant has not expired"
    );
    assert_eq!(store.list_approvals().unwrap().len(), 1);
}

#[test]
fn the_approval_cap_drops_the_oldest_grants() {
    let store = Store::open_in_memory().unwrap();
    let later = now_ms() + 60_000;
    for digest in ["d1", "d2", "d3", "d4"] {
        store
            .remember_approval(&NewApproval {
                scope_digest: digest.to_owned(),
                operation: "run_command".to_owned(),
                tool_id: "git".to_owned(),
                tier: "normal".to_owned(),
                summary: digest.to_owned(),
                expires_at_ms: later,
            })
            .unwrap();
    }
    assert_eq!(store.count("approvals").unwrap(), 4);

    // All four rows share a `granted_at_ms` in a test this fast, so the tie is
    // broken by rowid and the newest insertions are the ones that survive.
    assert_eq!(store.prune_approvals(2).unwrap(), 2);
    let remaining = store.list_approvals().unwrap();
    assert_eq!(remaining.len(), 2);
    assert!(
        remaining.iter().all(|a| a.expires_at_ms > now_ms()),
        "pruning to the cap must not delete live grants"
    );
}

#[test]
fn the_count_helper_still_refuses_an_unknown_table() {
    let store = Store::open_in_memory().unwrap();
    // Widened for v2, and still an allowlist: the argument is interpolated into
    // SQL, so anything not named here must not reach it.
    assert_eq!(store.count("tool_health").unwrap(), 0);
    assert_eq!(store.count("approvals").unwrap(), 0);
    assert!(store.count("sqlite_master").is_err());
    assert!(store.count("tasks; DROP TABLE tasks").is_err());
}

// ── Phase 2: plans, steps and the states a reconciler reads ──────────────────

/// A two-step plan of the shape `jean_core::planner` writes: sequential, registered tools only,
/// each step declaring how it will be verified.
fn plan(task_id: &str, plan_id: &str, attempts: u32) -> NewPlan {
    NewPlan {
        id: plan_id.to_owned(),
        task_id: task_id.to_owned(),
        schema_version: jean_protocol::PLAN_SCHEMA_VERSION,
        goal: "open notepad and check it is on screen".to_owned(),
        provider: "ollama".to_owned(),
        model: Some("test-model:latest".to_owned()),
        steps_json: r#"{"schema_version":1,"steps":[]}"#.to_owned(),
        attempts,
        steps: vec![
            NewPlanStep {
                step_key: "s1".to_owned(),
                position: 0,
                title: "start notepad".to_owned(),
                operation_kind: "launch_app".to_owned(),
                tool_id: Some("app.notepad".to_owned()),
                arguments_json: r#"[{"type":"text","value":"notepad"}]"#.to_owned(),
                verify_method: "window_for_pid".to_owned(),
            },
            NewPlanStep {
                step_key: "s2".to_owned(),
                position: 1,
                title: "list its windows".to_owned(),
                operation_kind: "inspect_process".to_owned(),
                tool_id: Some("system.process_table".to_owned()),
                arguments_json: r#"[{"type":"text","value":"notepad"}]"#.to_owned(),
                verify_method: "fields_available".to_owned(),
            },
        ],
    }
}

fn store_with_task(id: &str) -> Store {
    let store = Store::open_in_memory().expect("open store");
    store.insert_task(&task(id)).expect("insert task");
    store
}

#[test]
fn a_task_with_no_plan_reports_none_rather_than_an_empty_one() {
    // Phase 0 and Phase 1 tasks must keep looking exactly as they did: no plan, no steps, and no
    // code path that treats "no plan" as a failure.
    let store = store_with_task("t1");
    assert!(store.current_plan_for_task("t1").unwrap().is_none());
    assert!(store.plans_for_task("t1").unwrap().is_empty());
    assert!(store.uncertain_plan_steps("t1").unwrap().is_empty());
}

#[test]
fn a_plan_and_its_steps_round_trip_in_declaration_order() {
    let store = store_with_task("t1");
    store
        .insert_plan(&plan("t1", "p1", 1))
        .expect("insert plan");

    let read = store.current_plan_for_task("t1").unwrap().expect("a plan");
    assert_eq!(read.id, "p1");
    assert_eq!(read.provider, "ollama");
    assert_eq!(read.model.as_deref(), Some("test-model:latest"));
    assert_eq!(read.attempts, 1);
    assert_eq!(read.schema_version, jean_protocol::PLAN_SCHEMA_VERSION);
    assert!(read.superseded_at_ms.is_none());
    assert!(read.created_at_ms > 0);

    let steps = store.plan_steps("p1").unwrap();
    assert_eq!(steps.len(), 2, "every step of the validated plan is a row");
    assert_eq!(steps[0].step_key, "s1");
    assert_eq!(steps[1].step_key, "s2");
    // Position, not the step id's text, decides order — the runner walks this list as-is.
    assert_eq!(steps[0].position, 0);
    assert_eq!(steps[1].position, 1);
    assert_eq!(steps[0].state, PlanStepState::Pending);
    assert_eq!(steps[0].verify_method, "window_for_pid");
    assert_eq!(steps[0].tool_id.as_deref(), Some("app.notepad"));
    assert!(steps[0].tool_run_id.is_none());
    assert!(steps[0].verdict.is_none(), "nothing has run yet");
}

#[test]
fn writing_a_plan_and_its_steps_is_one_transaction() {
    // A step row that names a plan which does not exist would leave the reconciler with an effect
    // and nothing that authorised it, so the insert must fail as a unit.
    let store = Store::open_in_memory().expect("open store");
    let mut broken = plan("ghost-task", "p9", 1);
    broken.task_id = "no-such-task".to_owned();
    let error = store.insert_plan(&broken).unwrap_err();
    assert!(matches!(error, StoreError::Sqlite { .. }), "{error:?}");
    assert_eq!(
        store.count("plans").unwrap(),
        0,
        "the plan row must not survive"
    );
    assert_eq!(
        store.count("plan_steps").unwrap(),
        0,
        "nor may any step row, because the task row they name was never inserted"
    );
}

#[test]
fn a_step_becomes_running_before_it_can_be_settled() {
    let store = store_with_task("t1");
    store.insert_plan(&plan("t1", "p1", 1)).unwrap();
    store.start_plan_step("p1", "s1").unwrap();

    let steps = store.plan_steps("p1").unwrap();
    assert_eq!(steps[0].state, PlanStepState::Running);
    assert!(steps[0].started_at_ms.is_some());
    assert!(steps[0].ended_at_ms.is_none());
    assert!(!steps[0].is_settled());
    // The second step is untouched: a sequential plan does not open two steps at once.
    assert_eq!(steps[1].state, PlanStepState::Pending);
}

#[test]
fn starting_a_step_twice_does_not_reopen_it() {
    let store = store_with_task("t1");
    store.insert_plan(&plan("t1", "p1", 1)).unwrap();
    store.start_plan_step("p1", "s1").unwrap();
    // `WHERE ... AND state = 'pending'` matched nothing the second time, and that is reported as
    // the anomaly it is rather than silently resetting `started_at_ms`.
    let error = store.start_plan_step("p1", "s1").unwrap_err();
    assert!(
        matches!(&error, StoreError::NoSuchPlanStep { step_key, .. } if step_key == "s1"),
        "{error:?}"
    );
}

#[test]
fn a_step_records_the_run_it_executed_as() {
    let store = store_with_task("t1");
    store.insert_plan(&plan("t1", "p1", 1)).unwrap();
    let run_id = store
        .insert_tool_run("t1", "app.notepad", "notepad.exe", "[]", None, "{}")
        .unwrap();
    store.start_plan_step("p1", "s1").unwrap();
    store.set_plan_step_run("p1", "s1", run_id).unwrap();

    let steps = store.plan_steps("p1").unwrap();
    assert_eq!(steps[0].tool_run_id, Some(run_id));
    // The link is what lets `jean task` show the exit code and captured output behind a step
    // without duplicating either into the step row.
    let runs = store.tool_runs("t1").unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run_id);
}

#[test]
fn settling_a_step_keeps_the_verdict_and_the_evidence_together() {
    let store = store_with_task("t1");
    store.insert_plan(&plan("t1", "p1", 1)).unwrap();
    store.start_plan_step("p1", "s1").unwrap();
    store
        .finish_plan_step(
            "p1",
            "s1",
            PlanStepState::Succeeded,
            Some("proven"),
            "a visible window titled Untitled belongs to pid 4242",
        )
        .unwrap();

    let steps = store.plan_steps("p1").unwrap();
    assert_eq!(steps[0].state, PlanStepState::Succeeded);
    assert_eq!(steps[0].verdict.as_deref(), Some("proven"));
    assert!(steps[0].evidence.as_deref().unwrap().contains("pid 4242"));
    assert!(steps[0].ended_at_ms.is_some());
    assert!(steps[0].is_settled());
}

#[test]
fn a_step_that_ran_but_proved_nothing_is_unverified_not_failed() {
    // D-012: exit code 0 is not success. The state has to be able to say "completed, unproven",
    // because flattening that into `failed` would hide that the effect probably did happen.
    let store = store_with_task("t1");
    store.insert_plan(&plan("t1", "p1", 1)).unwrap();
    store.start_plan_step("p1", "s2").unwrap();
    store
        .finish_plan_step(
            "p1",
            "s2",
            PlanStepState::Unverified,
            Some("unverifiable"),
            "the tool declares no verification; the command completed",
        )
        .unwrap();
    let steps = store.plan_steps("p1").unwrap();
    assert_eq!(steps[1].state, PlanStepState::Unverified);
    assert_eq!(steps[1].verdict.as_deref(), Some("unverifiable"));
}

#[test]
fn an_unrecognised_verdict_is_rejected_by_the_schema() {
    let store = store_with_task("t1");
    store.insert_plan(&plan("t1", "p1", 1)).unwrap();
    store.start_plan_step("p1", "s1").unwrap();
    let error = store
        .finish_plan_step("p1", "s1", PlanStepState::Succeeded, Some("probably"), "")
        .unwrap_err();
    assert!(matches!(error, StoreError::Sqlite { .. }), "{error:?}");
    // And the failed write left the step where it was, rather than half-updated.
    let steps = store.plan_steps("p1").unwrap();
    assert_eq!(steps[0].state, PlanStepState::Running);
}

#[test]
fn a_step_that_never_ran_is_stopped_without_a_verdict() {
    let store = store_with_task("t1");
    store.insert_plan(&plan("t1", "p1", 1)).unwrap();
    store
        .stop_plan_step("p1", "s2", PlanStepState::Skipped, "step s1 failed")
        .unwrap();
    let steps = store.plan_steps("p1").unwrap();
    assert_eq!(steps[1].state, PlanStepState::Skipped);
    assert_eq!(
        steps[1].verdict, None,
        "a step that did not run proves nothing"
    );
    assert_eq!(steps[1].evidence.as_deref(), Some("step s1 failed"));

    // Refusing to skip a step that is already running is the same guard as `start_plan_step`:
    // 'pending' is the only state a step can leave without having run.
    store.start_plan_step("p1", "s1").unwrap();
    assert!(store
        .stop_plan_step("p1", "s1", PlanStepState::Cancelled, "operator cancelled")
        .is_err());
}

#[test]
fn an_interrupted_plan_leaves_exactly_the_steps_that_must_not_be_replayed() {
    // The reconciler's whole question is "which effects are uncertain", and `running` is the only
    // state that means that. A crash mid-step leaves one row here; pending rows are simply not
    // reached and settled rows need nothing.
    let store = store_with_task("t1");
    store.insert_plan(&plan("t1", "p1", 1)).unwrap();
    store
        .finish_plan_step(
            "p1",
            "s1",
            PlanStepState::Succeeded,
            Some("proven"),
            "window found",
        )
        .unwrap();
    // The runner set s1 settled, started s2, and died before recording anything about it.
    store.start_plan_step("p1", "s2").unwrap();

    let uncertain = store.uncertain_plan_steps("t1").unwrap();
    assert_eq!(uncertain.len(), 1, "{uncertain:?}");
    assert_eq!(uncertain[0].step_key, "s2");
    assert_eq!(uncertain[0].state, PlanStepState::Running);
    assert!(
        uncertain[0].tool_run_id.is_none(),
        "no run was ever attached"
    );
    assert!(!uncertain[0].is_settled());
}

#[test]
fn a_replacement_plan_supersedes_the_first_without_erasing_it() {
    let store = store_with_task("t1");
    store.insert_plan(&plan("t1", "p1", 1)).unwrap();
    // Bounded replanning: the second plan is the live one, and the first stays readable so the
    // report can say how many attempts it took and what was wrong with the earlier one.
    assert_eq!(store.supersede_plan("p1").unwrap(), 1);
    store.insert_plan(&plan("t1", "p2", 2)).unwrap();

    let live = store.current_plan_for_task("t1").unwrap().expect("p2");
    assert_eq!(live.id, "p2");
    assert_eq!(live.attempts, 2);
    let all = store.plans_for_task("t1").unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(
        all[0].id, "p1",
        "oldest first, so the attempt order is readable"
    );
    assert!(all[0].superseded_at_ms.is_some());
    assert!(all[1].superseded_at_ms.is_none());
    // Superseding twice is a no-op, not a rewritten timestamp.
    assert_eq!(store.supersede_plan("p1").unwrap(), 0);

    // Each plan owns its step keys: 's1' exists twice, once under each plan, and the runner reads
    // them through `plan_steps(plan_id)` so there is no ambiguity about which is which.
    assert_eq!(store.plan_steps("p1").unwrap().len(), 2);
    assert_eq!(store.plan_steps("p2").unwrap().len(), 2);
    store
        .finish_plan_step(
            "p1",
            "s1",
            PlanStepState::Failed,
            Some("not_proven"),
            "no window",
        )
        .unwrap();
    let after = store.plan_steps("p1").unwrap();
    assert_eq!(after[0].state, PlanStepState::Failed);
    assert_eq!(
        store.plan_steps("p2").unwrap()[0].state,
        PlanStepState::Pending,
        "settling the replaced plan's step must not touch the live plan"
    );
}

#[test]
fn a_reopened_database_still_knows_which_step_was_left_running() {
    // Crash recovery is about a *different process* reading what the dead one wrote, so this test
    // has to reopen rather than trust an in-memory handle.
    let path = temp_db("plans-reopen");
    {
        let store = Store::open(&path, "wal", Duration::from_secs(5)).expect("first open");
        store.insert_task(&task("t1")).unwrap();
        store.insert_plan(&plan("t1", "p1", 1)).unwrap();
        store.start_plan_step("p1", "s1").unwrap();
    }
    {
        let store = Store::open(&path, "wal", Duration::from_secs(5)).expect("reopen");
        let uncertain = store.uncertain_plan_steps("t1").unwrap();
        assert_eq!(uncertain.len(), 1);
        assert_eq!(uncertain[0].step_key, "s1");
        assert_eq!(store.count("plans").unwrap(), 1);
        assert_eq!(store.count("plan_steps").unwrap(), 2);
    }
    cleanup(&path);
}

#[test]
fn the_new_tables_are_reachable_only_through_the_known_names() {
    let store = Store::open_in_memory().expect("open store");
    assert_eq!(store.count("plans").unwrap(), 0);
    assert_eq!(store.count("plan_steps").unwrap(), 0);
    // `count` refuses anything it has not been told about rather than interpolating it.
    assert!(store.count("plan_steps; DROP TABLE tasks").is_err());
    assert!(store.count("tasks").is_ok());
}

#[test]
fn plan_step_states_round_trip_through_their_names() {
    for state in PlanStepState::ALL {
        assert_eq!(
            PlanStepState::from_str(state.as_str()),
            Some(*state),
            "{} must parse back",
            state.as_str()
        );
    }
    assert_eq!(PlanStepState::from_str("awaiting_approval"), None);
    assert_eq!(PlanStepState::from_str(""), None);

    // `pending` is the only state a runner will ever reach again, and `running` is deliberately
    // not: that is the no-replay rule expressed as a predicate rather than as a comment.
    assert!(PlanStepState::Pending.is_reachable());
    for state in [
        PlanStepState::Running,
        PlanStepState::Succeeded,
        PlanStepState::Failed,
        PlanStepState::Skipped,
        PlanStepState::Cancelled,
        PlanStepState::Unverified,
        PlanStepState::NeedsAttention,
    ] {
        assert!(
            !state.is_reachable(),
            "{state} must not be re-run automatically"
        );
    }
    assert!(!PlanStepState::Running.is_settled());
    assert!(PlanStepState::NeedsAttention.is_settled());
    // A step never borrows the task states: these are different questions.
    assert!(!PlanStepState::ALL
        .iter()
        .any(|s| s.as_str() == jean_protocol::TaskState::AwaitingApproval.as_str()));
}
