//! Schema definitions and the ordered migration list.
//!
//! Migrations are applied by `PRAGMA user_version` and are **append-only**.
//! Once a version has shipped it must never be edited, because a database on
//! disk only records "I am at version N" — it has no way to notice that the
//! version-N script it already ran has changed underneath it. To change the
//! schema, add version N+1.
//!
//! Column order matters: [`crate::row_to_task`] and [`crate::row_to_tool_run`]
//! read columns by position from explicit `SELECT` lists. Those lists, not the
//! DDL below, are the contract; this file only has to define every column they
//! name with a compatible type.

/// A single forward-only schema step.
pub struct Migration {
    /// Value written to `PRAGMA user_version` after `sql` succeeds.
    pub version: u32,
    /// Short human name, used in logs and in migration errors.
    pub name: &'static str,
    /// DDL/DML. Applied with `execute_batch`, so multiple statements are fine.
    /// Runs inside a transaction started by [`crate::Store::migrate`].
    pub sql: &'static str,
}

/// Highest version in [`MIGRATIONS`].
///
/// Hard-coded rather than derived so a truncated or reordered list is caught by
/// the `schema_version_matches_the_last_migration` test instead of silently
/// leaving databases half-migrated.
pub const SCHEMA_VERSION: u32 = 5;

/// Every migration, oldest first.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial",
        sql: V1_INITIAL,
    },
    Migration {
        version: 2,
        name: "phase_one_health_and_approvals",
        sql: V2_PHASE_ONE,
    },
    Migration {
        version: 3,
        name: "phase_two_plans_and_steps",
        sql: V3_PHASE_TWO,
    },
    Migration {
        version: 4,
        name: "phase_three_delegations",
        sql: V4_PHASE_THREE,
    },
    Migration {
        version: 5,
        name: "phase_five_history_search_and_memory",
        sql: V5_PHASE_FIVE,
    },
];

/// The v1 schema.
///
/// Two `CHECK` constraints duplicate knowledge that already lives in Rust:
/// the nine task states ([`jean_protocol::TaskState`]) and the termination
/// kinds ([`crate::Termination`]). That is deliberate. The Rust enums are the
/// runtime authority; the constraints exist so a hand-run `UPDATE` in
/// `sqlite3.exe` cannot leave a row the application would then fail to parse.
/// If the two ever disagree the enum wins, and widening the constraint is a
/// new migration. `the_state_check_lists_all_nine_states` fails loudly if a
/// state is added to the enum and forgotten here.
const V1_INITIAL: &str = "\
CREATE TABLE tasks (
    id                TEXT PRIMARY KEY NOT NULL,
    goal              TEXT NOT NULL,
    tool_id           TEXT NOT NULL,
    state             TEXT NOT NULL DEFAULT 'queued'
                      CHECK (state IN ('queued','planning','awaiting_approval',
                                       'running','verifying','succeeded','failed',
                                       'cancelled','needs_attention')),
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL,
    started_at_ms     INTEGER,
    finished_at_ms    INTEGER,
    -- The runner that owns this task, plus that process's creation time.
    -- A pid alone is not evidence: Windows reuses pids, so 'pid 4210 is
    -- alive' can be a different program that inherited the number. Comparing
    -- creation time as well makes a reused pid look dead, which is the safe
    -- direction for reconciliation.
    runner_pid        INTEGER,
    runner_created_ms INTEGER,
    -- The request as submitted, already passed through the redactor. See
    -- jean_protocol::redact for the boundary of what that catches.
    request_json      TEXT NOT NULL DEFAULT '{}',
    outcome           TEXT,
    outcome_detail    TEXT
);

CREATE INDEX idx_tasks_state ON tasks (state);
CREATE INDEX idx_tasks_created ON tasks (created_at_ms DESC);
CREATE INDEX idx_tasks_runner ON tasks (runner_pid) WHERE runner_pid IS NOT NULL;

CREATE TABLE tool_runs (
    id               INTEGER PRIMARY KEY,
    task_id          TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    tool_id          TEXT NOT NULL,
    executable       TEXT NOT NULL,
    args_json        TEXT NOT NULL DEFAULT '[]',
    working_dir      TEXT,
    env_json         TEXT NOT NULL DEFAULT '{}',
    child_pid        INTEGER,
    started_at_ms    INTEGER NOT NULL,
    ended_at_ms      INTEGER,
    exit_code        INTEGER,
    termination      TEXT
                     CHECK (termination IS NULL OR termination IN
                            ('exited','timeout','cancelled','launch_failed',
                             'job_assignment_failed','orphaned')),
    -- Output lives in files, not rows. A 10 MB capture per stream would
    -- otherwise be copied through SQLite on every read and would grow the
    -- database past any reasonable backup. The database keeps the pointer and
    -- the byte count so history stays summarisable without opening the file.
    stdout_path      TEXT,
    stderr_path      TEXT,
    stdout_bytes     INTEGER NOT NULL DEFAULT 0,
    stderr_bytes     INTEGER NOT NULL DEFAULT 0,
    -- 0/1 rather than a boolean type, because SQLite has no boolean type.
    -- Read with `row.get::<_, i64>(n) != 0`.
    stdout_truncated INTEGER NOT NULL DEFAULT 0,
    stderr_truncated INTEGER NOT NULL DEFAULT 0,
    error            TEXT
);

CREATE INDEX idx_tool_runs_task ON tool_runs (task_id);

CREATE TABLE events (
    seq     INTEGER PRIMARY KEY,
    -- No foreign key on purpose. Events are the audit trail; if a task row is
    -- ever removed by hand, the history explaining why should outlive it.
    task_id TEXT NOT NULL,
    at_ms   INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    detail  TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_events_task ON events (task_id, seq);
";

/// Phase 1: what a bounded health probe last saw, and what the operator has
/// already agreed to.
///
/// Neither table was in v1 because neither had a reader. Phase 1 adds both:
/// `jean health` answers from `tool_health` instead of re-probing, and a
/// remembered approval is a row, not process memory.
const V2_PHASE_ONE: &str = "\
-- Health is a record of the last probe, not a live view. The CLI re-probes only
-- when a row is older than `tools.health_check_interval_secs`, which is what
-- keeps an unhealthy tool from hanging every later invocation.
CREATE TABLE tool_health (
    tool_id              TEXT PRIMARY KEY NOT NULL,
    status               TEXT NOT NULL DEFAULT 'unknown'
                         CHECK (status IN ('unknown','healthy','unhealthy','disabled')),
    -- Reset to zero by a successful probe. `tools.mark_unhealthy_after` is
    -- compared against this, so the threshold stays a config value instead of
    -- being baked into the schema.
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    last_check_ms        INTEGER,
    last_ok_ms           INTEGER,
    -- What was checked, or why it failed. Written already redacted: this column
    -- ends up in `jean health` output on a shared screen.
    detail               TEXT
);

-- A remembered approval, keyed by a digest of the operation **and** its validated
-- arguments and resolved target. That binding is the whole point of the table:
-- `operation`, `tool_id` and `tier` are stored so `jean approvals` can explain a
-- row to the operator, and are deliberately not what a lookup matches on. An
-- approval for one PowerShell command must not authorise the next one.
CREATE TABLE approvals (
    id             INTEGER PRIMARY KEY,
    scope_digest   TEXT NOT NULL UNIQUE,
    operation      TEXT NOT NULL,
    tool_id        TEXT NOT NULL,
    tier           TEXT NOT NULL,
    summary        TEXT NOT NULL,
    granted_at_ms  INTEGER NOT NULL,
    -- An approval that never expires is a permission grant. A later command with
    -- the same arguments after this instant is refused and prompts again.
    expires_at_ms  INTEGER NOT NULL,
    used_count     INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_approvals_expiry ON approvals (expires_at_ms);
";

/// Phase 2: what the model proposed, what was validated, and how each step ended.
///
/// Two tables rather than one JSON blob in `tasks`, because the questions the operator and the
/// reconciler ask are per step. "Did step 3 run?" and "which process was step 3?" must be answered
/// from rows that were written *before* the effect happened, not from a document that only exists
/// once the whole plan is known. `plans` keeps the validated plan verbatim so a later read can show
/// exactly what was agreed to; `plan_steps` keeps what actually happened to each of its steps.
const V3_PHASE_TWO: &str = "\
-- One row per plan a task was given. A task can have several: a rejected proposal leaves no row,
-- but a replacement plan from bounded replanning does, with `attempts` counting the round trips
-- that produced it and `superseded_at_ms` marking the one that was replaced.
CREATE TABLE plans (
    id               TEXT PRIMARY KEY NOT NULL,
    task_id          TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    -- The plan schema version this row was written under. Validation and reading
    -- back have to agree, and a row from a future schema must not be reinterpreted.
    schema_version   INTEGER NOT NULL,
    goal             TEXT NOT NULL,
    -- Provenance, not authority: which provider and which model produced this plan.
    provider         TEXT NOT NULL,
    model            TEXT,
    -- The validated plan as canonical JSON, redacted before the write. Steps are
    -- duplicated here and in `plan_steps` on purpose: this column is the record of
    -- what was approved, the step rows are the record of what happened.
    steps_json       TEXT NOT NULL,
    attempts         INTEGER NOT NULL DEFAULT 1,
    created_at_ms    INTEGER NOT NULL,
    superseded_at_ms INTEGER
);

CREATE INDEX idx_plans_task ON plans (task_id, created_at_ms);

-- One row per step, written when the plan is accepted and updated as it runs.
-- `state` is step-level, not task-level: a plan step never enters
-- `awaiting_approval`, and the task's nine states would misdescribe a step.
CREATE TABLE plan_steps (
    id             INTEGER PRIMARY KEY,
    plan_id        TEXT NOT NULL REFERENCES plans (id) ON DELETE CASCADE,
    -- Denormalised so a step can be listed for a task without joining plans.
    task_id        TEXT NOT NULL,
    -- The step's own id from the plan document, e.g. 's1'. Stable across a
    -- replacement plan only within its own plan, hence the composite unique key.
    step_key       TEXT NOT NULL,
    -- Declaration order. Execution follows it exactly: Phase 2 is sequential.
    position       INTEGER NOT NULL,
    title          TEXT NOT NULL,
    operation_kind TEXT NOT NULL,
    tool_id        TEXT,
    -- The validated argument vector, redacted. Same rule as `tool_runs.args_json`.
    arguments_json TEXT NOT NULL DEFAULT '[]',
    verify_method  TEXT NOT NULL,
    state          TEXT NOT NULL DEFAULT 'pending'
                   CHECK (state IN ('pending','running','succeeded','failed','skipped',
                                    'cancelled','unverified','needs_attention')),
    -- The process run this step executed as, once there is one. Nullable because
    -- the row exists before the step runs.
    tool_run_id    INTEGER REFERENCES tool_runs (id) ON DELETE SET NULL,
    verdict        TEXT CHECK (verdict IS NULL OR verdict IN
                      ('proven','not_proven','unverifiable')),
    evidence       TEXT,
    started_at_ms  INTEGER,
    ended_at_ms    INTEGER,
    UNIQUE (plan_id, step_key)
);

CREATE INDEX idx_plan_steps_plan ON plan_steps (plan_id, position);
CREATE INDEX idx_plan_steps_task ON plan_steps (task_id);
";

const V4_PHASE_THREE: &str =
    "-- One row per delegation: one outcome a person asked for, handed to one external worker.
--
-- The shape of this table is the whole point of Phase 3. What the agent claimed and what JEAN
-- observed are **separate columns**, and neither one is derived from the other. A delegation whose
-- agent said `done` while the verification command failed is a row with both facts in it, and the
-- `result` column takes the verification side — an agent's wording can never overwrite an
-- observation.
CREATE TABLE delegations (
    id             TEXT PRIMARY KEY NOT NULL,
    task_id        TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    -- The connector name the operator chose (or the configured default), and the protocol shape
    -- it speaks. Recorded, not inferred: `acp` and `codex-cli` produce different evidence.
    worker         TEXT NOT NULL,
    kind           TEXT NOT NULL,
    -- The operator's own words. Never a plan the worker wrote.
    goal           TEXT NOT NULL,
    -- Resolved absolute workspace path the worker was pointed at, and the worktree it actually ran
    -- in when isolation happened. `worktree` is NULL when the operator's checkout was used, and
    -- `worktree_note` then says why.
    workspace      TEXT NOT NULL,
    worktree       TEXT,
    worktree_note  TEXT,
    -- The verification command, resolved BEFORE the delegation. Storing it here is what makes a
    -- later read of this row able to say what was actually run, and it is the reason an
    -- agent-suggested command cannot become the check: the check is already written down.
    verify_command TEXT NOT NULL,
    verify_args_json TEXT NOT NULL DEFAULT '[]',
    -- Baseline: the same command, against the same workspace, before the worker touched anything.
    -- NULL means no baseline ran, which is a reported condition, not a silent pass.
    baseline_verdict TEXT CHECK (baseline_verdict IS NULL OR baseline_verdict IN
                      ('proven','not_proven','unverifiable')),
    baseline_run_id  INTEGER REFERENCES tool_runs (id) ON DELETE SET NULL,
    -- What the worker said, in its own terms, plus how its session actually ended.
    agent_stop_reason TEXT,
    agent_claim       TEXT,
    worker_exit_code  INTEGER,
    -- What JEAN observed afterwards.
    verification_verdict TEXT CHECK (verification_verdict IS NULL OR verification_verdict IN
                         ('proven','not_proven','unverifiable')),
    verification_run_id  INTEGER REFERENCES tool_runs (id) ON DELETE SET NULL,
    -- The changed-file summary, captured from the workspace rather than from the agent's report.
    changed_files_json   TEXT NOT NULL DEFAULT '[]',
    changed_file_count   INTEGER NOT NULL DEFAULT 0,
    -- The verdict JEAN settled on. Derived from verification and process facts, never from
    -- `agent_claim`.
    result TEXT CHECK (result IS NULL OR result IN
             ('succeeded','failed','cancelled','needs_attention','unverified')),
    -- Where the worker's stdout/stderr captures live, so the evidence is reachable from the row.
    output_dir TEXT,
    created_at_ms INTEGER NOT NULL,
    settled_at_ms INTEGER,
    notes_json    TEXT NOT NULL DEFAULT '[]'
);

CREATE INDEX idx_delegations_task ON delegations (task_id, created_at_ms);

-- Worker progress, one row per event the worker produced, in the order it arrived.
--
-- Kept separate from `events` because the two have different lifetimes and different meanings:
-- `events` is the task audit trail, this is the transcript of what the worker did. A `seq` per
-- delegation is what lets a reader notice that progress rows went missing, which a timestamp alone
-- cannot do on a machine with a coarse clock.
CREATE TABLE worker_events (
    id            INTEGER PRIMARY KEY,
    delegation_id TEXT NOT NULL REFERENCES delegations (id) ON DELETE CASCADE,
    seq           INTEGER NOT NULL,
    kind          TEXT NOT NULL,
    -- Already redacted by the caller: a worker's tool output is the most likely place on this
    -- machine for a token to appear.
    detail        TEXT NOT NULL,
    at_ms         INTEGER NOT NULL,
    UNIQUE (delegation_id, seq)
);

CREATE INDEX idx_worker_events_delegation ON worker_events (delegation_id, seq);
";

/// How many characters of a source column reach the search index.
///
/// The same number is written into the DDL below as a literal (`substr(..., 1,
/// 512)`), because a migration script is a shipped artifact and cannot read a
/// Rust constant. `the_index_bound_in_the_ddl_matches_the_rust_constant` fails if
/// the two ever disagree.
pub const MAX_INDEXED_CHARS: usize = 512;

/// Phase 5: searchable history, operator-approved saved facts, and the small set
/// of settings that are toggled at runtime rather than edited in a TOML file.
///
/// Three things in this script are deliberate and worth stating before the DDL:
///
/// * **The index is a search key, not a second copy of the record.** A hit is
///   resolved by joining back to `tasks` / `events`, so the state, the outcome
///   and the timestamps a search reports are the row's current ones. Only text
///   the writer already redacted is indexed, and only the first
///   [`MAX_INDEXED_CHARS`] characters of it. Captured stdout and stderr,
///   screenshot pixels and environment values never entered those columns in the
///   first place (see version 1), so they cannot reach the index either.
/// * **Synchronisation lives in triggers, not in the callers.** Every write path
///   in this crate — `insert_task`, `set_outcome`, `append_event`, the inline
///   event insert in `transition`, and `prune_events` — then keeps the index in
///   step without knowing it exists, and so does a hand-run `UPDATE` in
///   `sqlite3.exe`. That is the same argument the audit writer makes for
///   redacting in the writer: a guarantee that depends on nobody forgetting is a
///   habit, not a property of the code.
/// * **Nothing here is reachable from a model.** `saved_facts.source` and
///   `preferences.key` are `CHECK`-constrained to the shapes an operator command
///   writes. Retrieval is a read; it authorises nothing (see
///   `jean_core::memory`, whose whole doc is that separation).
const V5_PHASE_FIVE: &str = "\
-- One FTS5 table per source table, not one combined table. An FTS5 rowid has to
-- name exactly one source row, and `tasks` and `events` already have id spaces
-- that do not correspond: `tasks` is keyed by a TEXT id with an implicit rowid,
-- while `events.seq` *is* the rowid (INTEGER PRIMARY KEY). Two tables let each
-- index use its source's own rowid as its key, so a hit joins straight back with
-- no mapping to keep in step. A combined table would have needed a third id
-- space and a translation table, and the translation is exactly the kind of
-- state that drifts.
CREATE VIRTUAL TABLE tasks_fts USING fts5(
    goal,
    outcome_detail,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- `goal` is written once and never changed, and `outcome_detail` is written by
-- `set_outcome`; `state` and the timestamps are excluded on purpose — they are
-- mutable metadata that every read takes from `tasks`, and indexing them would
-- make every state change rewrite an index row to no purpose.
CREATE TRIGGER tasks_fts_after_insert AFTER INSERT ON tasks BEGIN
    INSERT INTO tasks_fts(rowid, goal, outcome_detail)
    VALUES (new.rowid, substr(jean_redact(new.goal), 1, 512),
            substr(jean_redact(new.outcome_detail), 1, 512));
END;

-- Delete-then-insert rather than UPDATE: `UPDATE ... WHERE rowid = ?` silently
-- affects nothing if the index row is missing, which would remove a task from
-- search results without saying so.
CREATE TRIGGER tasks_fts_after_update AFTER UPDATE OF goal, outcome_detail ON tasks BEGIN
    DELETE FROM tasks_fts WHERE rowid = new.rowid;
    INSERT INTO tasks_fts(rowid, goal, outcome_detail)
    VALUES (new.rowid, substr(jean_redact(new.goal), 1, 512),
            substr(jean_redact(new.outcome_detail), 1, 512));
END;

CREATE TRIGGER tasks_fts_after_delete AFTER DELETE ON tasks BEGIN
    DELETE FROM tasks_fts WHERE rowid = old.rowid;
END;

CREATE VIRTUAL TABLE events_fts USING fts5(
    detail,
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER events_fts_after_insert AFTER INSERT ON events BEGIN
    INSERT INTO events_fts(rowid, detail) VALUES (new.seq, substr(jean_event_text(new.kind, new.detail), 1, 512));
END;

-- `events` has no updater today — it is append-only. The trigger costs nothing
-- and is here so that if one is ever added, the index cannot be the thing that
-- quietly disagrees with the row.
CREATE TRIGGER events_fts_after_update AFTER UPDATE OF kind, detail ON events BEGIN
    DELETE FROM events_fts WHERE rowid = new.seq;
    INSERT INTO events_fts(rowid, detail) VALUES (new.seq, substr(jean_event_text(new.kind, new.detail), 1, 512));
END;

-- This is the retention path: `prune_events` deletes by `at_ms`, and the delete
-- trigger is what stops pruned history from staying searchable forever.
CREATE TRIGGER events_fts_after_delete AFTER DELETE ON events BEGIN
    DELETE FROM events_fts WHERE rowid = old.seq;
END;

-- Backfill. Existing rows are eligible for the same reason new ones are: only
-- columns the writer already redacted are copied. Rows whose text is empty still
-- get an index entry, because an entry is what makes a row findable at all.
--
-- Inside the migration transaction, so a database with a great deal of history
-- either comes back fully searchable or stays at version 4 — never half-built.
INSERT INTO tasks_fts(rowid, goal, outcome_detail)
SELECT rowid, substr(jean_redact(goal), 1, 512), substr(jean_redact(outcome_detail), 1, 512) FROM tasks;

INSERT INTO events_fts(rowid, detail)
SELECT seq, substr(jean_event_text(kind, detail), 1, 512) FROM events;

-- ── saved facts ───────────────────────────────────────────────────────
--
-- A separate table rather than rows in a `preferences` key/value table, because
-- the two have different identities: a preference is one current value per key,
-- and a fact is one of many free-text entries a person chose to keep, each with
-- its own lifetime. Forcing facts into key/value rows would mean inventing
-- synthetic keys, and a synthetic key is where a collision and a silent
-- overwrite come from.
--
-- Identity is (scope, norm). `norm` is the text as normalisation sees it and
-- `text` is what the operator typed; both are stored so a duplicate is detected
-- without mangling the record, and so the list can show the original words.
-- `norm` rather than a digest, because a digest made by `DefaultHasher` is only
-- stable for as long as the standard library's hash stays the same, and a fact
-- that outlives a Rust upgrade would come back as a duplicate of itself.
CREATE TABLE saved_facts (
    id             TEXT PRIMARY KEY NOT NULL,
    -- The operator's own words, redacted before the write by the caller. Bounded
    -- here as well as in Rust so a hand-written row cannot outgrow the display.
    text           TEXT NOT NULL CHECK (length(text) BETWEEN 1 AND 1000),
    -- '' means global. Otherwise the project path or name the fact was saved
    -- under; it is data, never a filter that authorises anything.
    scope          TEXT NOT NULL DEFAULT '' CHECK (length(scope) <= 260),
    norm           TEXT NOT NULL,
    -- The only source this column may hold. A model, a transcript, a tool result
    -- or a worker cannot be the provenance of a saved fact, and the schema says
    -- so: an automatic promotion would fail the CHECK rather than sneak in.
    source         TEXT NOT NULL DEFAULT 'operator_command'
                   CHECK (source IN ('operator_command')),
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL,
    UNIQUE (scope, norm)
);

CREATE INDEX idx_saved_facts_created ON saved_facts (created_at_ms DESC);

CREATE VIRTUAL TABLE saved_facts_fts USING fts5(
    text,
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER saved_facts_fts_after_insert AFTER INSERT ON saved_facts BEGIN
    INSERT INTO saved_facts_fts(rowid, text) VALUES (new.rowid, jean_redact(new.text));
END;

CREATE TRIGGER saved_facts_fts_after_update AFTER UPDATE OF text ON saved_facts BEGIN
    DELETE FROM saved_facts_fts WHERE rowid = new.rowid;
    INSERT INTO saved_facts_fts(rowid, text) VALUES (new.rowid, jean_redact(new.text));
END;

CREATE TRIGGER saved_facts_fts_after_delete AFTER DELETE ON saved_facts BEGIN
    DELETE FROM saved_facts_fts WHERE rowid = old.rowid;
END;

-- ── runtime preferences ───────────────────────────────────────────────
--
-- Only keys this build actually reads are allowed, and that is enforced by the
-- CHECK rather than by a list a caller might forget. A setting nothing consumes
-- would be a lie in a table, which is the failure mode `keep_24h` had before
-- version 4 gave it a cleaner: an operator would set it, see nothing happen, and
-- have no way to tell that nothing was ever going to happen.
--
-- `jean_protocol::preferences::PreferenceKey` is the Rust side of the same list;
-- `the_preference_check_lists_every_key_the_protocol_reads` keeps them together.
CREATE TABLE preferences (
    key        TEXT PRIMARY KEY NOT NULL
               CHECK (key IN ('voice.enabled', 'voice.mode',
                              'desktop.enabled', 'desktop.retention')),
    -- Stored as text because SQLite has no typed column for a value a person
    -- types; parsed by the protocol, which is what refuses a value it cannot
    -- apply before the row is ever written.
    value      TEXT NOT NULL CHECK (length(value) BETWEEN 1 AND 260),
    set_at_ms  INTEGER NOT NULL
);

-- The record that an explicit memory command changed something, written in the
-- same transaction as the change. Metadata only: an action, the id it touched and
-- when. No text, no scope, no value and no hash of any of them — a log of a
-- deletion that kept the deleted words would be a second copy of the thing the
-- operator asked to remove, in the file designed to be kept longest.
CREATE TABLE memory_changes (
    seq        INTEGER PRIMARY KEY,
    action     TEXT NOT NULL CHECK (action IN
                 ('add','update','forget','preference_set','preference_clear')),
    record_id  TEXT NOT NULL CHECK (length(record_id) BETWEEN 1 AND 260),
    provenance TEXT NOT NULL DEFAULT 'operator_command'
               CHECK (provenance IN ('operator_command')),
    at_ms      INTEGER NOT NULL
);
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HealthStatus;

    #[test]
    fn migrations_are_contiguous_and_start_at_one() {
        for (index, m) in MIGRATIONS.iter().enumerate() {
            let expected = index as u32 + 1;
            assert_eq!(
                m.version, expected,
                "migration versions must be contiguous starting at 1; \
                 a gap means a database could skip a schema step"
            );
            assert!(
                !m.sql.trim().is_empty(),
                "migration {} ({}) has an empty script",
                m.version,
                m.name
            );
        }
    }

    #[test]
    fn schema_version_matches_the_last_migration() {
        let last = MIGRATIONS
            .last()
            .expect("MIGRATIONS must not be empty")
            .version;
        assert_eq!(
            SCHEMA_VERSION, last,
            "SCHEMA_VERSION is hard-coded on purpose; if you added a migration \
             you must also raise it"
        );
    }

    #[test]
    fn the_state_check_lists_all_nine_states() {
        let sql = MIGRATIONS[0].sql;
        for state in jean_protocol::state::ALL_STATES {
            assert!(
                sql.contains(&format!("'{}'", state.as_str())),
                "the tasks.state CHECK constraint is missing '{}'; a row \
                 written by a newer build would be rejected by this schema",
                state.as_str()
            );
        }
    }

    #[test]
    fn the_termination_check_lists_every_termination_kind() {
        let sql = MIGRATIONS[0].sql;
        let kinds = [
            crate::Termination::Exited,
            crate::Termination::Timeout,
            crate::Termination::Cancelled,
            crate::Termination::LaunchFailed,
            crate::Termination::JobAssignmentFailed,
            crate::Termination::Orphaned,
        ];
        for kind in kinds {
            assert!(
                sql.contains(&format!("'{}'", kind.as_str())),
                "the tool_runs.termination CHECK constraint is missing '{}'",
                kind.as_str()
            );
        }
    }

    #[test]
    fn every_column_read_by_position_exists_in_the_schema() {
        // Guards the real contract: row_to_task reads indices 0..=12 and
        // row_to_tool_run reads 0..=18 from explicit SELECT lists. Rather than
        // re-typing those lists here, assert the schema defines each named
        // column. A rename in one place and not the other is the failure this
        // catches.
        let sql = MIGRATIONS[0].sql;
        for col in [
            "id",
            "goal",
            "tool_id",
            "state",
            "created_at_ms",
            "updated_at_ms",
            "started_at_ms",
            "finished_at_ms",
            "runner_pid",
            "runner_created_ms",
            "request_json",
            "outcome",
            "outcome_detail",
        ] {
            assert!(sql.contains(col), "tasks is missing column {col}");
        }
        for col in [
            "executable",
            "args_json",
            "working_dir",
            "env_json",
            "child_pid",
            "ended_at_ms",
            "exit_code",
            "termination",
            "stdout_path",
            "stderr_path",
            "stdout_bytes",
            "stderr_bytes",
            "stdout_truncated",
            "stderr_truncated",
            "error",
        ] {
            assert!(sql.contains(col), "tool_runs is missing column {col}");
        }
        for col in ["seq", "task_id", "at_ms", "kind", "detail"] {
            assert!(sql.contains(col), "events is missing column {col}");
        }
    }

    /// The v2 tables, checked the same way v1 is: by the columns the readers name.
    #[test]
    fn version_two_defines_the_phase_one_tables() {
        let sql = MIGRATIONS[1].sql;
        for col in [
            "tool_id",
            "status",
            "consecutive_failures",
            "last_check_ms",
            "last_ok_ms",
            "detail",
        ] {
            assert!(sql.contains(col), "tool_health is missing column {col}");
        }
        for col in [
            "scope_digest",
            "operation",
            "tool_id",
            "tier",
            "summary",
            "granted_at_ms",
            "expires_at_ms",
            "used_count",
        ] {
            assert!(sql.contains(col), "approvals is missing column {col}");
        }
        for status in [
            HealthStatus::Unknown.as_str(),
            HealthStatus::Healthy.as_str(),
            HealthStatus::Unhealthy.as_str(),
            HealthStatus::Disabled.as_str(),
        ] {
            assert!(
                sql.contains(&format!("'{status}'")),
                "the tool_health.status CHECK is missing '{status}'"
            );
        }
        // The digest is UNIQUE because a second grant of the same operation and
        // arguments is meant to replace the first, not to add a row that a lookup
        // would then have to arbitrate between.
        assert!(
            sql.contains("scope_digest   TEXT NOT NULL UNIQUE"),
            "approvals.scope_digest must be UNIQUE"
        );
    }

    #[test]
    fn v1_is_unchanged_by_the_phase_one_migration() {
        // Migrations are append-only: editing a shipped script means a database
        // that already ran it has no way to notice. If this test fails, the fix is
        // a new version, not a change to version 1.
        let v1 = MIGRATIONS[0].sql;
        assert!(
            v1.starts_with("CREATE TABLE tasks ("),
            "v1 tasks DDL changed"
        );
        assert!(v1.contains("CREATE TABLE tool_runs ("));
        assert!(v1.contains("CREATE TABLE events ("));
        assert!(
            !v1.contains("tool_health") && !v1.contains("approvals"),
            "phase-one tables must live in version 2, not be folded into version 1"
        );
    }

    #[test]
    fn phase_two_tables_arrive_in_version_three_and_touch_nothing_earlier() {
        let v3 = MIGRATIONS[2].sql;
        assert!(
            v3.contains("CREATE TABLE plans ("),
            "v3 must introduce plans"
        );
        assert!(
            v3.contains("CREATE TABLE plan_steps ("),
            "v3 must introduce plan_steps"
        );
        // No ALTER, no DROP, no re-creation of a shipped table: an added column to
        // `tasks` would be its own statement in its own version, and a phase-two
        // migration that edits v1 or v2 would silently fail on an existing database.
        for forbidden in [
            "ALTER TABLE",
            "DROP TABLE",
            "CREATE TABLE tasks",
            "CREATE TABLE tool_runs",
        ] {
            assert!(
                !v3.contains(forbidden),
                "v3 must not contain {forbidden:?}; it may only add the two new tables"
            );
        }
        assert!(
            !MIGRATIONS[0].sql.contains("plans") && !MIGRATIONS[1].sql.contains("plan_steps"),
            "the plan tables belong to version 3"
        );
    }

    /// The v3 tables, checked the way v1 and v2 are: by the columns their readers name.
    #[test]
    fn version_three_defines_the_phase_two_tables() {
        let sql = MIGRATIONS[2].sql;
        for col in [
            "task_id",
            "schema_version",
            "goal",
            "provider",
            "model",
            "steps_json",
            "attempts",
            "created_at_ms",
            "superseded_at_ms",
        ] {
            assert!(sql.contains(col), "plans is missing column {col}");
        }
        for col in [
            "plan_id",
            "step_key",
            "position",
            "title",
            "operation_kind",
            "tool_id",
            "arguments_json",
            "verify_method",
            "state",
            "tool_run_id",
            "verdict",
            "evidence",
            "started_at_ms",
            "ended_at_ms",
        ] {
            assert!(sql.contains(col), "plan_steps is missing column {col}");
        }
        // A step id is only stable inside the plan that proposed it, so the key is
        // composite. A replacement plan may reuse `s1` without colliding.
        assert!(
            sql.contains("UNIQUE (plan_id, step_key)"),
            "plan_steps must be unique per (plan, step)"
        );
        for state in jean_protocol::PlanStepState::ALL {
            assert!(
                sql.contains(&format!("'{}'", state.as_str())),
                "the plan_steps.state CHECK constraint is missing '{}'; a row written by a \
                 newer build would be rejected by this schema",
                state.as_str()
            );
        }
        // The three verdicts are the ones `jean_core::verify::Verdict` produces. A
        // fourth label in that enum must reach this constraint too, or a real run
        // would fail to record its own outcome.
        for verdict in ["proven", "not_proven", "unverifiable"] {
            assert!(
                sql.contains(&format!("'{verdict}'")),
                "the plan_steps.verdict CHECK is missing '{verdict}'"
            );
        }
    }

    #[test]
    fn v2_is_unchanged_by_the_phase_two_migration() {
        let v2 = MIGRATIONS[1].sql;
        assert!(v2.starts_with("-- Health is a record"));
        assert!(v2.contains("CREATE TABLE tool_health ("));
        assert!(v2.contains("CREATE TABLE approvals ("));
        assert!(
            !v2.contains("CREATE TABLE plans") && !v2.contains("CREATE TABLE plan_steps"),
            "the plan tables must arrive in version 3, not be folded into version 2"
        );
    }

    /// The `substr(..., 1, N)` bound in the DDL and [`MAX_INDEXED_CHARS`] are the
    /// same number written in two languages. If they drift, the Rust side would
    /// describe an index the schema does not build.
    #[test]
    fn the_index_bound_in_the_ddl_matches_the_rust_constant() {
        let v5 = MIGRATIONS[4].sql;
        let literal = format!(", 1, {})", MAX_INDEXED_CHARS);
        assert!(
            v5.contains(&literal),
            "the DDL must bound indexed text at {MAX_INDEXED_CHARS} characters"
        );
        assert!(
            !v5.contains(", 1, 0)") && !v5.contains(", 1, -1)"),
            "an unbounded or zero-length substr would index everything or nothing"
        );
    }

    #[test]
    fn phase_five_adds_search_and_memory_without_rewriting_an_earlier_table() {
        let v5 = MIGRATIONS[4].sql;
        for required in [
            "CREATE VIRTUAL TABLE tasks_fts USING fts5(",
            "CREATE VIRTUAL TABLE events_fts USING fts5(",
            "CREATE VIRTUAL TABLE saved_facts_fts USING fts5(",
            "CREATE TABLE saved_facts (",
            "CREATE TABLE preferences (",
        ] {
            assert!(v5.contains(required), "v5 is missing {required:?}");
        }
        // Append-only again, and this time it matters more than usual: v1 defines
        // `tasks` and `events`, and a version-5 script that edited either would
        // not run on a database that already has them.
        for forbidden in ["ALTER TABLE", "DROP TABLE", "DROP INDEX"] {
            assert!(!v5.contains(forbidden), "v5 must not contain {forbidden:?}");
        }
        for (index, earlier) in MIGRATIONS[..4].iter().enumerate() {
            assert!(
                !earlier.sql.contains("fts5") && !earlier.sql.contains("saved_facts"),
                "the search and memory schema belongs to version 5, not {index}"
            );
        }
    }

    /// Synchronisation is schema-level, so every table the index covers has to
    /// carry all three triggers. A missing delete trigger leaves pruned history
    /// searchable; a missing insert trigger makes a row invisible forever.
    #[test]
    fn every_indexed_table_has_insert_update_and_delete_triggers() {
        let v5 = MIGRATIONS[4].sql;
        for source in ["tasks", "events", "saved_facts"] {
            for event in ["insert", "update", "delete"] {
                let needle = format!("CREATE TRIGGER {source}_fts_after_{event}");
                assert!(
                    v5.contains(&needle),
                    "{source} is indexed but has no AFTER {event} trigger; the index \
                     would drift from the rows it claims to describe"
                );
            }
        }
    }

    #[test]
    fn the_backfill_covers_both_history_tables_and_nothing_else() {
        let v5 = MIGRATIONS[4].sql;
        assert!(v5.contains(
            "INSERT INTO tasks_fts(rowid, goal, outcome_detail)\nSELECT rowid, \
             substr(jean_redact(goal), 1, 512), substr(jean_redact(outcome_detail), 1, 512) FROM tasks;"
        ));
        assert!(
            v5.contains(
                "INSERT INTO events_fts(rowid, detail)\nSELECT seq, substr(jean_event_text(kind, detail), 1, 512) FROM events;"
            ),
            "existing events must become searchable too, not only new ones"
        );
    }

    /// The `preferences.key` CHECK is the schema's copy of the list the protocol
    /// reads. A key in one and not the other is a setting that either cannot be
    /// stored or is stored and never consumed.
    #[test]
    fn the_preference_check_lists_every_key_the_protocol_reads() {
        let v5 = MIGRATIONS[4].sql;
        for key in jean_protocol::preferences::SUPPORTED_KEYS {
            assert!(
                v5.contains(&format!("'{key}'")),
                "the preferences.key CHECK is missing {key:?}; `jean preferences set {key}` \
                 would be refused by the schema"
            );
        }
        assert!(
            v5.matches("CHECK (key IN (").count() == 1,
            "the key list must appear once, or the two copies could disagree"
        );
    }

    /// Saved facts are the one piece of stored memory a model must never be able
    /// to author. The CHECK is what makes that a schema property.
    #[test]
    fn the_fact_source_check_admits_only_an_operator_command() {
        let v5 = MIGRATIONS[4].sql;
        assert!(v5.contains("CHECK (source IN ('operator_command'))"));
        for forbidden in ["'model'", "'transcript'", "'tool_result'", "'worker'"] {
            assert!(
                !v5.contains(&format!("source IN ('operator_command', {forbidden}")),
                "a second provenance in the source CHECK would let automatic \
                 promotion of {forbidden} output into saved memory"
            );
        }
    }

    #[test]
    fn a_fact_is_unique_per_scope_and_has_bounded_text() {
        let v5 = MIGRATIONS[4].sql;
        assert!(
            v5.contains("UNIQUE (scope, norm)"),
            "duplicate detection is (scope, normalised text); without the constraint \
             two identical facts in one scope would both be listed and neither would \
             be the answer to \"is this already saved?\""
        );
        assert!(v5.contains("CHECK (length(text) BETWEEN 1 AND 1000)"));
        assert!(v5.contains("CHECK (length(scope) <= 260)"));
    }
}
