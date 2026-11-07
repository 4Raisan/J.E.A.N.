//! Properties added after review: scope isolation, metadata-only auditing, the
//! credential floor at the store writer, and redaction of rows that predate it.
//!
//! Temporary databases and text these tests wrote. `forget` is only ever pointed
//! at a record the test created.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jean_protocol::Event;
use jean_store::migrations::MIGRATIONS;
use jean_store::{SearchOptions, Store, TaskRecord};
use rusqlite::Connection;

fn temp_db(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jean-memory-review-{}-{}-{}.db",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn cleanup(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
}

fn on_disk(tag: &str) -> (Store, PathBuf) {
    let path = temp_db(tag);
    let store = Store::open(&path, "wal", Duration::from_secs(5)).expect("open");
    (store, path)
}

fn memory() -> Store {
    Store::open_in_memory().expect("in-memory store")
}

fn raw_column(path: &Path, sql: &str) -> Vec<String> {
    let conn = Connection::open(path).expect("second connection");
    let mut stmt = conn.prepare(sql).expect("prepare");
    stmt.query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .filter_map(|row| row.ok())
        .collect()
}

fn search(store: &Store, query: &str) -> Vec<jean_store::HistoryHit> {
    store
        .search_history(&SearchOptions {
            query,
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("literal search for {query:?}: {e}"))
}

#[test]
fn a_scope_search_cannot_reach_another_projects_facts() {
    let store = memory();
    store
        .add_saved_fact("f-global", "the shared build convention", None)
        .unwrap();
    store
        .add_saved_fact("f-a", "project alpha keeps invoices here", Some("alpha"))
        .unwrap();
    store
        .add_saved_fact("f-b", "project beta keeps invoices there", Some("beta"))
        .unwrap();

    // No scope means the global bucket only. A default that returned everything
    // would leak one project's notes into another's context, which is the opposite
    // of what a scope is for.
    assert!(
        store
            .search_saved_facts("invoices", None, 10)
            .unwrap()
            .is_empty(),
        "neither project fact is global"
    );
    assert_eq!(
        store.search_saved_facts("shared", None, 10).unwrap().len(),
        1
    );

    let alpha = store
        .search_saved_facts("invoices", Some("alpha"), 10)
        .unwrap();
    assert_eq!(alpha.len(), 1, "global plus alpha, never beta: {alpha:?}");
    assert_eq!(alpha[0].fact.id, "f-a");
    assert_eq!(store.saved_facts(Some("beta"), 10).unwrap().len(), 2);
    assert_eq!(store.saved_facts(None, 10).unwrap().len(), 1);
}

#[test]
fn memory_changes_are_recorded_without_retaining_the_content() {
    let (disk, path) = on_disk("audit");
    disk.add_saved_fact("f1", "a sentence worth keeping", Some("alpha"))
        .unwrap();
    disk.update_saved_fact("f1", "a revised sentence").unwrap();
    disk.forget_saved_fact("f1").unwrap();
    disk.set_preference("voice.enabled", "true").unwrap();
    disk.clear_preference("voice.enabled").unwrap();

    assert_eq!(
        raw_column(&path, "SELECT action FROM memory_changes ORDER BY seq"),
        vec![
            "add",
            "update",
            "forget",
            "preference_set",
            "preference_clear"
        ]
    );
    assert_eq!(
        raw_column(&path, "SELECT record_id FROM memory_changes ORDER BY seq"),
        vec!["f1", "f1", "f1", "voice.enabled", "voice.enabled"]
    );
    assert_eq!(
        raw_column(&path, "SELECT DISTINCT provenance FROM memory_changes"),
        vec!["operator_command"]
    );

    // The column list is the guarantee: there is nowhere for the forgotten words
    // to be kept. A log of a deletion that retained the deleted content would be a
    // second copy of the thing the operator asked to remove.
    assert_eq!(
        raw_column(
            &path,
            "SELECT name FROM pragma_table_info('memory_changes')"
        ),
        vec!["seq", "action", "record_id", "provenance", "at_ms"]
    );
    drop(disk);
    cleanup(&path);
}

#[test]
fn a_duplicate_add_records_no_change_because_nothing_changed() {
    let (disk, path) = on_disk("duplicate-audit");
    disk.add_saved_fact("f1", "the same words", None).unwrap();
    disk.add_saved_fact("f2", "THE SAME WORDS", None).unwrap();
    assert_eq!(
        raw_column(&path, "SELECT action FROM memory_changes"),
        vec!["add"],
        "an audit line for a write that did not happen would misreport when this \
         fact was saved"
    );
    drop(disk);
    cleanup(&path);
}

#[test]
fn the_store_writer_refuses_credential_shaped_text_itself() {
    let store = memory();
    // jean-core redacts before calling; this is the floor underneath it, so a
    // future writer that forgot cannot store a token as memory.
    let error = store
        .add_saved_fact(
            "f1",
            "use api_key=sk-live-ABCDEFGHIJKLMNOP for the build",
            None,
        )
        .expect_err("credential-shaped text is not memory");
    assert!(error.to_string().contains("credential"), "{error}");
    assert_eq!(store.count("saved_facts").unwrap(), 0);
    assert_eq!(
        store.count("memory_changes").unwrap(),
        0,
        "a refusal records no change"
    );
}

#[test]
fn a_legacy_row_written_in_the_clear_is_indexed_redacted() {
    let path = temp_db("legacy-secret");
    let token = "sk-live-LEGACYTOKEN123";
    {
        let conn = Connection::open(&path).unwrap();
        for migration in &MIGRATIONS[..4] {
            conn.execute_batch(migration.sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 4).unwrap();
        // A row from before goal redaction existed. The index must not widen it.
        conn.execute(
            "INSERT INTO tasks (id, goal, tool_id, state, created_at_ms, updated_at_ms, request_json)
             VALUES ('t-legacy', ?1, 'fixture', 'succeeded', 1, 1, '{}')",
            rusqlite::params![format!("print the report with {token}")],
        )
        .unwrap();
    }
    let disk = Store::open(&path, "wal", Duration::from_secs(5)).unwrap();
    assert!(
        search(&disk, token).is_empty(),
        "the clear token matches nothing"
    );
    let goals = raw_column(&path, "SELECT goal FROM tasks_fts");
    assert!(goals.iter().all(|g| !g.contains(token)), "{goals:?}");
    assert!(
        goals.iter().any(|g| g.contains("[REDACTED]")),
        "redacted rather than dropped, so the record is still findable: {goals:?}"
    );
    // The source row is untouched: this bounds the index, it does not rewrite
    // history the operator already has.
    assert!(disk.require_task("t-legacy").unwrap().goal.contains(token));
    drop(disk);
    cleanup(&path);
}

#[test]
fn worker_and_model_wording_is_not_a_search_target() {
    let store = memory();
    store
        .insert_task(&TaskRecord::new(
            "t1".into(),
            "delegate the refactor".into(),
            Some("fixture".into()),
            "{}".into(),
        ))
        .unwrap();
    store
        .append_event("t1", &Event::note("the bounded description"))
        .unwrap();
    // These two kinds carry another process's or a model's own words rather than a
    // bounded description of what J.E.A.N. did, so they are not search targets.
    store
        .append_event(
            "t1",
            &Event::new(
                jean_protocol::EventKind::WorkerProgress,
                "the worker said something secret",
            ),
        )
        .unwrap();
    store
        .append_event(
            "t1",
            &Event::new(
                jean_protocol::EventKind::ModelResponded,
                "the model said something else",
            ),
        )
        .unwrap();

    assert_eq!(search(&store, "bounded description").len(), 1);
    assert!(search(&store, "worker said").is_empty());
    assert!(search(&store, "model said").is_empty());
    assert_eq!(
        store.count("events").unwrap(),
        4,
        "the audit rows still exist; they are simply not indexed"
    );
}

/// The redaction scalars are registered per connection, so a writer that is not a
/// `Store` cannot satisfy the triggers. That is fail-closed: an unsupported writer
/// is refused rather than able to put unredacted text into the index.
#[test]
fn an_external_writer_is_refused_rather_than_able_to_index_in_the_clear() {
    let (disk, path) = on_disk("external-writer");
    disk.insert_task(&TaskRecord::new(
        "t1".into(),
        "a goal".into(),
        Some("fixture".into()),
        "{}".into(),
    ))
    .unwrap();
    let conn = Connection::open(&path).unwrap();
    let attempt = conn.execute(
        "INSERT INTO events (task_id, at_ms, kind, detail)
         VALUES ('t1', 5, 'note', 'api_key=sk-live-EXTERNALWRITE')",
        [],
    );
    assert!(
        attempt.is_err(),
        "a connection without the redaction scalars must not be able to write history"
    );
    assert_eq!(
        disk.count("events").unwrap(),
        1,
        "only the supported write landed"
    );
    drop(conn);
    drop(disk);
    cleanup(&path);
}

#[test]
fn a_refused_preference_write_records_no_change() {
    let (disk, path) = on_disk("preference-refusal");
    assert!(disk.set_preference("voice.enabled", "perhaps").is_err());
    assert!(disk.set_preference("ui.theme", "dark").is_err());
    assert_eq!(disk.count("preferences").unwrap(), 0);
    assert_eq!(disk.count("memory_changes").unwrap(), 0);
    drop(disk);
    cleanup(&path);
}
