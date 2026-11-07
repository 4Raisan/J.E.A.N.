//! Crash-recovery integration tests.
//!
//! The Phase-0 exit criterion is: kill the runner mid-task, restart, and find the
//! task present in a state that tells the truth. These tests reproduce that without
//! needing a human to open Task Manager — the "crash" is a dropped `Store` and a
//! recorded runner pid that does not exist.
//!
//! All of them use a file-backed database in a temporary directory, because
//! recovery is a statement about what survives a process boundary and an in-memory
//! database cannot express it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use jean_core::reconcile::{classify, reconcile, Verdict};
use jean_protocol::{EventKind, StreamSummary, TaskState, Termination};
use jean_store::{now_ms, Store, TaskRecord};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A pid that cannot belong to a live process.
const DEAD_PID: u32 = u32::MAX;

fn temp_db(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "jean-recovery-{}-{}-{}.sqlite3",
        tag,
        std::process::id(),
        n
    ))
}

fn cleanup(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
}

fn open(path: &Path) -> Store {
    Store::open(path, "wal", Duration::from_secs(5)).expect("the store must open")
}

fn record(id: &str, state: TaskState, pid: Option<u32>, created: Option<i64>) -> TaskRecord {
    let mut task = TaskRecord::new(
        id.to_owned(),
        format!("run the fixture for {id}"),
        Some("powershell.echo".to_owned()),
        r#"{"tool":"powershell.echo","args":["-NoProfile"]}"#.to_owned(),
    );
    task.state = state;
    task.runner_pid = pid;
    task.runner_created_ms = created;
    task
}

/// Write a task that looks exactly like one interrupted mid-run: a row in
/// `running`, a recorded runner, and a tool run that never finished.
fn simulate_a_crashed_run(path: &Path, task_id: &str, runner_pid: u32) -> i64 {
    let store = open(path);
    store
        .insert_task(&record(
            task_id,
            TaskState::Running,
            Some(runner_pid),
            Some(now_ms()),
        ))
        .unwrap();

    let run_id = store
        .insert_tool_run(
            task_id,
            "powershell.echo",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            r#"["-NoProfile","-Command","Start-Sleep 300"]"#,
            None,
            r#"{"PATH":"C:\\Windows\\System32"}"#,
        )
        .unwrap();
    store.record_child_pid(run_id, 424_242).unwrap();
    store
        .append_event(
            task_id,
            &jean_protocol::Event::spawn(424_242, "powershell.exe"),
        )
        .unwrap();

    // Dropped without any shutdown sequence, which is what a kill looks like.
    drop(store);
    run_id
}

#[test]
fn a_task_interrupted_by_a_kill_is_recovered_on_the_next_start() {
    let path = temp_db("interrupted");
    cleanup(&path);
    let run_id = simulate_a_crashed_run(&path, "t-crashed", DEAD_PID);

    // "Restart": a new process opens the same database.
    let store = open(&path);
    let before = store.require_task("t-crashed").unwrap();
    assert_eq!(
        before.state,
        TaskState::Running,
        "the recorded state must survive the crash exactly as it was"
    );
    assert_eq!(before.runner_pid, Some(DEAD_PID));

    let result = reconcile(&store).unwrap();
    assert_eq!(result.orphaned, vec!["t-crashed"]);

    let after = store.require_task("t-crashed").unwrap();
    assert_eq!(
        after.state,
        TaskState::NeedsAttention,
        "an interrupted task must be flagged for a human, not resumed and not failed"
    );
    assert!(after.runner_pid.is_none(), "ownership must be released");

    let run = &store.tool_runs("t-crashed").unwrap()[0];
    assert_eq!(run.id, run_id);
    assert_eq!(run.termination, Some(Termination::Orphaned));
    assert_eq!(run.exit_code, None, "no exit code was ever observed");
    assert!(run.ended_at_ms.is_some(), "the run must be closed off");

    drop(store);
    cleanup(&path);
}

#[test]
fn recovery_never_replays_the_interrupted_command() {
    let path = temp_db("noreplay");
    cleanup(&path);
    simulate_a_crashed_run(&path, "t-crashed", DEAD_PID);

    let store = open(&path);
    let runs_before = store.tool_runs("t-crashed").unwrap().len();
    let events_before = store.events("t-crashed").unwrap().len();

    reconcile(&store).unwrap();

    assert_eq!(
        store.tool_runs("t-crashed").unwrap().len(),
        runs_before,
        "reconciliation must not create a new tool run"
    );
    let events = store.events("t-crashed").unwrap();
    assert_eq!(
        events.len(),
        events_before + 2,
        "exactly two events: the state change and the reconciliation note"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::Reconciled && e.detail.contains("not replayed")),
        "the record must state that nothing was re-executed: {events:?}"
    );

    drop(store);
    cleanup(&path);
}

#[test]
fn a_second_client_does_not_look_like_a_crash_to_a_live_runner() {
    let path = temp_db("live");
    cleanup(&path);
    let (pid, created) = jean_process_identity();

    {
        let runner = open(&path);
        runner
            .insert_task(&record("t-live", TaskState::Running, Some(pid), created))
            .unwrap();
        runner.set_runner("t-live", pid, created.unwrap()).unwrap();

        // A second client — `jean tasks`, or `jean cancel` — opens the same
        // database while the runner is still working.
        let client = open(&path);
        let in_flight = client.in_flight_tasks().unwrap();
        assert_eq!(in_flight.len(), 1);
        assert_eq!(
            classify(&in_flight[0]),
            Verdict::OwnedByLiveRunner,
            "a live runner must not be classified as an orphan"
        );

        let result = reconcile(&client).unwrap();
        assert!(result.orphaned.is_empty());
        assert!(result.never_owned.is_empty());
        assert_eq!(result.still_owned, vec!["t-live"]);

        // The runner's own view must be unchanged.
        let task = runner.require_task("t-live").unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert_eq!(task.runner_pid, Some(pid));
        assert_eq!(task.runner_created_ms, created);
    }

    cleanup(&path);
}

#[test]
fn a_reader_sees_a_consistent_state_while_a_writer_is_active() {
    // WAL is what makes this work: the reader must not block, and must not see a
    // half-written row.
    let path = temp_db("concurrent");
    cleanup(&path);

    let writer = open(&path);
    let reader = open(&path);
    assert_eq!(writer.journal_mode().unwrap().to_lowercase(), "wal");

    writer
        .insert_task(&record("t-1", TaskState::Queued, None, None))
        .unwrap();
    assert_eq!(
        reader
            .list_tasks(jean_store::TaskFilter::All, 10)
            .unwrap()
            .len(),
        1
    );

    writer
        .transition("t-1", TaskState::Running, Some("launched"))
        .unwrap();
    assert_eq!(
        reader.require_task("t-1").unwrap().state,
        TaskState::Running,
        "the reader must see committed writes"
    );

    // A rejected transition leaves nothing visible to the reader either.
    let rejected = writer.transition("t-1", TaskState::Succeeded, None);
    assert!(rejected.is_err(), "running -> succeeded must be illegal");
    assert_eq!(
        reader.require_task("t-1").unwrap().state,
        TaskState::Running
    );

    writer
        .transition("t-1", TaskState::Verifying, None)
        .unwrap();
    writer
        .transition("t-1", TaskState::Succeeded, None)
        .unwrap();
    let final_state = reader.require_task("t-1").unwrap();
    assert_eq!(final_state.state, TaskState::Succeeded);
    assert!(final_state.finished_at_ms.is_some());

    drop(reader);
    drop(writer);
    cleanup(&path);
}

#[test]
fn everything_written_before_a_kill_is_still_there_after_a_restart() {
    let path = temp_db("durable");
    cleanup(&path);

    {
        let store = open(&path);
        store
            .insert_task(&record(
                "t-done",
                TaskState::Queued,
                Some(1234),
                Some(now_ms()),
            ))
            .unwrap();
        store
            .transition("t-done", TaskState::Running, Some("launched"))
            .unwrap();
        let run_id = store
            .insert_tool_run(
                "t-done",
                "powershell.echo",
                "powershell.exe",
                "[]",
                None,
                "{}",
            )
            .unwrap();
        store.record_child_pid(run_id, 4321).unwrap();
        // Written explicitly, because these events come from the runner rather
        // than from `Store`: this test is about whether rows survive a restart, so
        // it has to put one of every kind on disk itself.
        store
            .append_event(
                "t-done",
                &jean_protocol::Event::spawn(4321, "powershell.exe"),
            )
            .unwrap();
        store
            .finish_tool_run(
                run_id,
                Some(0),
                Termination::Exited,
                &StreamSummary {
                    path: Some("C:\\data\\stdout.log".to_owned()),
                    bytes: 12,
                    truncated: false,
                },
                &StreamSummary::default(),
                None,
            )
            .unwrap();
        store
            .append_event("t-done", &jean_protocol::Event::exit(0))
            .unwrap();
        store
            .transition("t-done", TaskState::Verifying, None)
            .unwrap();
        store
            .transition("t-done", TaskState::Succeeded, Some("proven: exit 0"))
            .unwrap();
        store
            .set_outcome("t-done", "proven", "the child exited with 0")
            .unwrap();
        store.clear_runner("t-done").unwrap();
    }

    let store = open(&path);
    let task = store.require_task("t-done").unwrap();
    assert_eq!(task.state, TaskState::Succeeded);
    assert_eq!(task.outcome.as_deref(), Some("proven"));
    assert_eq!(
        task.outcome_detail.as_deref(),
        Some("the child exited with 0")
    );
    assert!(task.runner_pid.is_none());
    assert!(task.started_at_ms.is_some());
    assert!(task.finished_at_ms.is_some());

    let run = &store.tool_runs("t-done").unwrap()[0];
    assert_eq!(run.exit_code, Some(0));
    assert_eq!(run.termination, Some(Termination::Exited));
    assert_eq!(run.child_pid, Some(4321));
    assert_eq!(run.stdout_bytes, 12);

    let kinds: Vec<EventKind> = store
        .events("t-done")
        .unwrap()
        .iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&EventKind::TaskCreated));
    assert!(kinds.contains(&EventKind::ProcessSpawned));
    assert!(kinds.contains(&EventKind::ProcessExited));
    // Four states visited, three transitions recorded.
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == EventKind::StateChanged)
            .count(),
        3,
        "queued->running->verifying->succeeded: {kinds:?}"
    );

    // A finished task is not a reconciliation candidate.
    let result = reconcile(&store).unwrap();
    assert!(result.is_empty(), "{result:?}");

    drop(store);
    cleanup(&path);
}

#[test]
fn a_task_that_never_recorded_a_runner_is_also_flagged() {
    // A crash between "row written" and "ownership recorded" leaves a task that
    // looks queued forever. It has to be reported, because nobody will ever
    // advance it.
    let path = temp_db("neverowned");
    cleanup(&path);
    {
        let store = open(&path);
        store
            .insert_task(&record("t-stranded", TaskState::Queued, None, None))
            .unwrap();
    }

    let store = open(&path);
    let result = reconcile(&store).unwrap();
    assert_eq!(result.never_owned, vec!["t-stranded"]);
    assert_eq!(
        store.require_task("t-stranded").unwrap().state,
        TaskState::NeedsAttention
    );
    drop(store);
    cleanup(&path);
}

#[test]
fn a_recycled_pid_is_not_mistaken_for_the_original_runner() {
    // The scenario the creation time exists for. This process holds the pid that
    // the crashed runner recorded, but the creation time belongs to the process
    // that used to hold it.
    let path = temp_db("recycled");
    cleanup(&path);
    let (pid, created) = jean_process_identity();
    let stale = created.map(|c| c - 86_400_000);

    {
        let store = open(&path);
        store
            .insert_task(&record("t-recycled", TaskState::Running, Some(pid), stale))
            .unwrap();
    }

    let store = open(&path);
    let result = reconcile(&store).unwrap();
    assert_eq!(
        result.orphaned,
        vec!["t-recycled"],
        "a matching pid with a non-matching creation time is a dead runner"
    );
    drop(store);
    cleanup(&path);
}

#[test]
fn a_database_written_by_a_newer_build_is_refused_rather_than_misread() {
    let path = temp_db("future");
    cleanup(&path);
    {
        let store = open(&path);
        store
            .insert_task(&record("t-1", TaskState::Queued, None, None))
            .unwrap();
        drop(store);

        let conn = rusqlite_connection(&path);
        conn.pragma_update(None, "user_version", 99).unwrap();
    }

    let err = Store::open(&path, "wal", Duration::from_secs(5))
        .expect_err("a newer schema must not be read by an older build");
    assert!(err.to_string().contains("newer than this build"), "{err}");
    cleanup(&path);
}

/// Open a raw connection, the way an operator using `sqlite3.exe` would.
fn rusqlite_connection(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).expect("the database must be openable directly")
}

/// This process's identity, via the supervisor's own probe.
///
/// Reached through `jean-core` rather than `jean-process` so the test exercises
/// the same call path reconciliation uses.
fn jean_process_identity() -> (u32, Option<i64>) {
    jean_process::win32::current_process_identity()
}
