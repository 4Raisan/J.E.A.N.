//! SQLite persistence: tasks, tool runs, and the append-only event log.
//!
//! ## Design constraints
//!
//! * **One file, no server.** D-008: SQLite only, no vector DB, no Postgres.
//! * **WAL mode** so a reader (a second `jean tasks` invocation) never blocks
//!   the writer (a running supervisor) and never sees a torn row.
//! * **Migrations by `PRAGMA user_version`.** A hand-rolled loop rather than a
//!   migration framework: seven tables do not justify the dependency, and the
//!   loop is small enough to read in one sitting.
//! * **Large blobs stay out of the database.** Captured stdout/stderr go to
//!   files; `tool_runs` stores the path and the byte count. A 10 MB output
//!   capture in a row would make every `tasks` query drag it along.
//! * **State changes go through the state machine.** [`Store::transition`] is the
//!   only way to move a task between states, and it validates against
//!   [`TaskState::transition`] inside the same transaction that writes the
//!   event. There is no `set_state`.
//!
//! ## What this crate deliberately does not do
//!
//! It does not decide whether a runner is alive. That needs `OpenProcess`, which
//! is Windows FFI and belongs in `jean-core`. [`Store::in_flight_tasks`] returns
//! candidates; the caller judges them. Keeping that split is what lets this
//! crate's tests run without touching the Win32 API.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// `Termination` and `StreamSummary` are defined in `jean-protocol` because
// `jean-process` produces them and this crate persists them. Re-exported so
// callers can keep saying `jean_store::Termination`.
pub use jean_protocol::{StreamSummary, Termination};

use jean_protocol::state::TaskState;
use jean_protocol::{Event, EventKind, PlanStepState, TransitionError};
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

pub mod memory;
pub mod migrations;

pub use memory::{
    FactAddOutcome, FactHit, HistoryHit, HistoryIndexReport, HistorySource, Preference, SavedFact,
    SearchOptions, DEFAULT_SEARCH_LIMIT, MAX_FACT_CHARS, MAX_LISTED_FACTS, MAX_QUERY_CHARS,
    MAX_SCOPE_CHARS, MAX_SEARCH_LIMIT, SNIPPET_TOKENS,
};
pub use migrations::{MIGRATIONS, SCHEMA_VERSION};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error at {context}: {source}")]
    Sqlite {
        context: &'static str,
        #[source]
        source: rusqlite::Error,
    },

    #[error("could not create the data directory {path}: {reason}")]
    Directory { path: String, reason: String },

    #[error("task `{0}` was not found")]
    NoSuchTask(String),

    #[error("task `{task_id}` is in state `{actual}`, not the expected `{expected}`")]
    UnexpectedState {
        task_id: String,
        actual: TaskState,
        expected: TaskState,
    },

    #[error("task state transition rejected: {0}")]
    Transition(#[from] TransitionError),

    #[error("database schema version {found} is newer than this build supports ({SCHEMA_VERSION}); refusing to read a database a future version wrote")]
    FromTheFuture { found: i64 },

    #[error("plan {plan_id:?} has no step {step_key:?}; the plan and its rows disagree")]
    NoSuchPlanStep { plan_id: String, step_key: String },

    #[error("could not serialize {what}: {reason}")]
    Serialization { what: &'static str, reason: String },

    #[error("the search cannot run: {reason}")]
    BadQuery { reason: String },

    #[error("the memory operation was refused: {reason}")]
    Memory { reason: String },
}

impl StoreError {
    fn sqlite(context: &'static str) -> impl FnOnce(rusqlite::Error) -> StoreError {
        move |source| StoreError::Sqlite { context, source }
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Milliseconds since the Unix epoch. Used for every timestamp so that ordering
/// is a plain integer comparison and no timezone library is needed.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        // A clock before 1970 is a broken machine; clamping to 0 keeps ordering
        // monotonic rather than producing negative timestamps that sort first.
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub id: String,
    /// What the user asked for. Free text; Phase 0 stores the tool invocation.
    pub goal: String,
    pub tool_id: Option<String>,
    pub state: TaskState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    /// Pid of the `jean run` process that owns this task, if any.
    pub runner_pid: Option<u32>,
    /// Creation time of that runner, in milliseconds since the epoch.
    ///
    /// Stored alongside the pid specifically to defeat pid reuse: a recycled pid
    /// will have a different creation time, so a dead runner is never mistaken
    /// for a live one that happens to share its number.
    pub runner_created_ms: Option<i64>,
    /// The original request, redacted before it was written.
    pub request_json: String,
    pub outcome: Option<String>,
    pub outcome_detail: Option<String>,
}

impl TaskRecord {
    pub fn new(id: String, goal: String, tool_id: Option<String>, request_json: String) -> Self {
        let now = now_ms();
        Self {
            id,
            goal,
            tool_id,
            state: TaskState::Queued,
            created_at_ms: now,
            updated_at_ms: now,
            started_at_ms: None,
            finished_at_ms: None,
            runner_pid: None,
            runner_created_ms: None,
            request_json,
            outcome: None,
            outcome_detail: None,
        }
    }

    pub fn is_in_flight(&self) -> bool {
        self.state.is_in_flight()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRunRecord {
    pub id: i64,
    pub task_id: String,
    pub tool_id: String,
    pub executable: String,
    /// Redacted. Never the raw argument vector.
    pub args_json: String,
    pub working_dir: Option<String>,
    /// Redacted. Always present: the column is `NOT NULL DEFAULT '{}'`, so "the
    /// tool saw no allowlisted variables" is recorded as `{}`, not as absence.
    pub env_json: String,
    pub child_pid: Option<u32>,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub exit_code: Option<i32>,
    pub termination: Option<Termination>,
    pub stdout_path: Option<String>,
    pub stderr_path: Option<String>,
    pub stdout_bytes: i64,
    pub stderr_bytes: i64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecord {
    pub seq: i64,
    pub task_id: String,
    pub at_ms: i64,
    pub kind: EventKind,
    pub detail: String,
}

/// What a caller may ask for when listing tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskFilter {
    All,
    InFlight,
    State(TaskState),
}

/// The recorded result of the last bounded health probe for one tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// Nothing has probed it yet on this database.
    Unknown,
    /// The last probe passed.
    Healthy,
    /// `consecutive_failures` reached the configured threshold.
    Unhealthy,
    /// `enabled = false` in `tools.toml`. Not a failure, and never overwritten by
    /// a probe: a disabled tool is not started to find out whether it works.
    Disabled,
}

impl HealthStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
            Self::Disabled => "disabled",
        }
    }

    /// Parse the persisted form. An inherent `from_str` rather than `FromStr`
    /// because failure here is expected — a row written by a newer build — and the
    /// caller wants `Option`, following the same convention as
    /// [`jean_protocol::Termination::from_str`].
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "unknown" => Some(Self::Unknown),
            "healthy" => Some(Self::Healthy),
            "unhealthy" => Some(Self::Unhealthy),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolHealthRecord {
    pub tool_id: String,
    pub status: HealthStatus,
    pub consecutive_failures: u32,
    pub last_check_ms: Option<i64>,
    pub last_ok_ms: Option<i64>,
    /// Already redacted before it was written; see [`Store::record_probe`].
    pub detail: Option<String>,
}

impl ToolHealthRecord {
    /// Whether this row is new enough to answer a health question without probing.
    ///
    /// `Unknown` is never fresh — nothing has been looked at yet. A row from the
    /// future is never fresh either: a clock that jumped backwards is
    /// indistinguishable from a very recent check, and re-probing is the safe
    /// reading.
    pub fn is_fresh(&self, interval: Duration, now_ms: i64) -> bool {
        let Some(at) = self.last_check_ms else {
            return false;
        };
        if self.status == HealthStatus::Unknown || at > now_ms {
            return false;
        }
        let age_ms = now_ms - at;
        // `as_millis()` is `u128`; an interval that will not fit an `i64` is a
        // duration of over 292 years, which is "never re-probe".
        age_ms < i64::try_from(interval.as_millis()).unwrap_or(i64::MAX)
    }
}

/// One remembered approval.
///
/// The row is matched on `scope_digest` alone, which is a digest of the operation
/// *and* its validated arguments and resolved target. The other descriptive fields
/// exist so `jean approvals` can tell a person what they already agreed to; they
/// are deliberately not part of the lookup, because matching on them would be
/// matching on the tool rather than on the act.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRecord {
    pub id: i64,
    pub scope_digest: String,
    pub operation: String,
    pub tool_id: String,
    pub tier: String,
    /// What was shown to the operator, redacted.
    pub summary: String,
    pub granted_at_ms: i64,
    pub expires_at_ms: i64,
    pub used_count: u32,
}

impl ApprovalRecord {
    pub fn is_expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_at_ms
    }
}

/// A new remembered approval, before it has an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApproval {
    pub scope_digest: String,
    pub operation: String,
    pub tool_id: String,
    pub tier: String,
    pub summary: String,
    pub expires_at_ms: i64,
}

/// One step of a plan, before it has a row.
///
/// Every field here is the *validated* plan's own text: `step_key` and `position` come from the
/// plan document, `arguments_json` is the redacted argument vector that passed validation, and
/// `verify_method` is the method the plan declared. Nothing in this type can express an executable,
/// a working directory or an environment — those are resolved later by the intent pipeline, from the
/// registry, not from model output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlanStep {
    pub step_key: String,
    pub position: u32,
    pub title: String,
    pub operation_kind: String,
    pub tool_id: Option<String>,
    pub arguments_json: String,
    pub verify_method: String,
}

/// A validated plan, ready to persist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlan {
    pub id: String,
    pub task_id: String,
    /// The plan schema version that was validated, not the one this build prefers. A row written
    /// under version 1 is read as version 1 even after the schema grows.
    pub schema_version: u32,
    pub goal: String,
    /// Which provider proposed it. Provenance for the report; never an authority.
    pub provider: String,
    pub model: Option<String>,
    /// The plan document as canonical JSON, already redacted.
    pub steps_json: String,
    /// How many model round trips produced this plan. 1 means first time.
    pub attempts: u32,
    pub steps: Vec<NewPlanStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRecord {
    pub id: String,
    pub task_id: String,
    pub schema_version: u32,
    pub goal: String,
    pub provider: String,
    pub model: Option<String>,
    pub steps_json: String,
    pub attempts: u32,
    pub created_at_ms: i64,
    /// Set when a replacement plan took over. `None` means this is the live plan.
    pub superseded_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStepRecord {
    pub id: i64,
    pub plan_id: String,
    pub task_id: String,
    pub step_key: String,
    pub position: u32,
    pub title: String,
    pub operation_kind: String,
    pub tool_id: Option<String>,
    /// Redacted, like `ToolRunRecord::args_json`.
    pub arguments_json: String,
    pub verify_method: String,
    pub state: PlanStepState,
    /// The process run this step executed as, once there is one.
    pub tool_run_id: Option<i64>,
    /// One of `proven` / `not_proven` / `unverifiable`, or `None` if the step never ran.
    pub verdict: Option<String>,
    /// The evidence sentence, redacted. This is what `jean task` quotes.
    pub evidence: Option<String>,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
}

impl PlanStepRecord {
    /// Whether this step's outcome is settled enough that nothing further will change it.
    pub fn is_settled(&self) -> bool {
        self.state.is_settled()
    }
}

/// The redaction floor the search index applies to every value it copies.
///
/// Registered as SQLite scalars because the triggers and the backfill run inside
/// SQL: redaction that depended on each Rust writer remembering would leave a
/// hand-written row, or a row from an older build, indexed in the clear. The
/// pattern set is the shipped default; a caller's additional configured patterns
/// are applied by the writer before the row exists, so this is a floor under that,
/// not a replacement for it.
fn register_index_functions(conn: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    let patterns = jean_protocol::config::default_redact_patterns();

    let redactor = jean_protocol::Redactor::new(&patterns);
    conn.create_scalar_function("jean_redact", 1, flags, move |ctx| {
        Ok(ctx
            .get::<Option<String>>(0)?
            .map(|text| redactor.redact(&text)))
    })
    .map_err(StoreError::sqlite("register jean_redact"))?;

    // Two event kinds carry another process's or a model's own words rather than a
    // bounded description of what J.E.A.N. did. They are not indexed at all: the
    // audit row still exists, it simply is not a search target.
    let redactor = jean_protocol::Redactor::new(&patterns);
    conn.create_scalar_function("jean_event_text", 2, flags, move |ctx| {
        let kind = ctx.get::<Option<String>>(0)?;
        let detail = ctx.get::<Option<String>>(1)?;
        Ok(match (kind.as_deref(), detail) {
            (Some(kind), Some(detail))
                if !matches!(kind, "worker_progress" | "model_responded") =>
            {
                Some(redactor.redact(&detail))
            }
            _ => None,
        })
    })
    .map_err(StoreError::sqlite("register jean_event_text"))?;
    Ok(())
}

pub struct Store {
    conn: Connection,
}

/// `rusqlite::Connection` is not `Debug`, so report what actually identifies a
/// store: the schema it is on and the journal mode it ended up with. Both probes
/// are shown as their `Result` rather than unwrapped, because a pragma that
/// fails is precisely what an operator needs to see in a panic message.
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("schema_version", &self.schema_version())
            .field("journal_mode", &self.journal_mode())
            .finish()
    }
}

impl Store {
    /// Open (and if necessary create and migrate) the database at `path`, with
    /// `synchronous = NORMAL`.
    ///
    /// `journal_mode` is applied only for file-backed databases: SQLite reports
    /// `memory` for `:memory:` and the pragma is meaningless there.
    pub fn open(path: &Path, journal_mode: &str, busy_timeout: Duration) -> Result<Self> {
        Self::open_with(path, journal_mode, "NORMAL", busy_timeout)
    }

    /// Open the database, choosing the SQLite `synchronous` pragma.
    ///
    /// `NORMAL` under WAL survives a process crash, which is the failure mode
    /// Phase 0 has to recover from. `FULL` also survives power loss, at a
    /// measurable cost per commit. The choice belongs to configuration, so it is a
    /// parameter rather than a constant buried here.
    pub fn open_with(
        path: &Path,
        journal_mode: &str,
        synchronous: &str,
        busy_timeout: Duration,
    ) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| StoreError::Directory {
                    path: parent.display().to_string(),
                    reason: e.to_string(),
                })?;
            }
        }

        let conn = Connection::open(path).map_err(StoreError::sqlite("open"))?;
        Self::from_connection(conn, Some(journal_mode), synchronous, busy_timeout)
    }

    /// An in-memory database for tests. Never WAL, never on disk.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(StoreError::sqlite("open_in_memory"))?;
        Self::from_connection(conn, None, "NORMAL", Duration::from_secs(5))
    }

    fn from_connection(
        conn: Connection,
        journal_mode: Option<&str>,
        synchronous: &str,
        busy_timeout: Duration,
    ) -> Result<Self> {
        conn.busy_timeout(busy_timeout)
            .map_err(StoreError::sqlite("busy_timeout"))?;

        if let Some(mode) = journal_mode {
            // Returns the mode actually in force. We log a mismatch rather than
            // failing: a read-only or network filesystem may refuse WAL, and a
            // J.E.A.N. that will not start because it could not get WAL is worse
            // than one that runs with a slower journal.
            let applied: String = conn
                .pragma_update_and_check(None, "journal_mode", mode, |row| row.get(0))
                .map_err(StoreError::sqlite("journal_mode"))?;
            if !applied.eq_ignore_ascii_case(mode) {
                tracing::warn!(
                    requested = mode,
                    applied = %applied,
                    "SQLite did not honour the requested journal mode; concurrent readers may block"
                );
            }
        }

        conn.pragma_update(None, "synchronous", synchronous)
            .map_err(StoreError::sqlite("synchronous"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(StoreError::sqlite("foreign_keys"))?;

        // The version-5 triggers and backfill call these scalars, so they have to
        // exist before the migration runs, not after it.
        register_index_functions(&conn)?;

        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Apply every migration above the current `user_version`.
    ///
    /// Each step runs in its own transaction and bumps `user_version` on
    /// success, so a crash partway through resumes at the right place.
    fn migrate(&mut self) -> Result<()> {
        // Serialize two processes starting on an empty data dir. A DEFERRED
        // transaction does not take a write lock until the first write, so
        // two processes both read user_version=0 and both try CREATE TABLE.
        // BEGIN IMMEDIATE acquires RESERVED before the read; the loser waits
        // on busy_timeout, then re-reads the winner's version and skips.
        // One outer transaction: a crash rolls back to the pre-migration
        // version and the next start resumes cleanly.
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(StoreError::sqlite("begin immediate"))?;

        let current: i64 = tx
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(StoreError::sqlite("read user_version"))?;

        if current > SCHEMA_VERSION as i64 {
            return Err(StoreError::FromTheFuture { found: current });
        }

        for m in MIGRATIONS {
            if (m.version as i64) <= current {
                continue;
            }
            tx.execute_batch(m.sql).map_err(|e| StoreError::Sqlite {
                context: "migration",
                source: e,
            })?;
            tx.execute_batch(&format!("PRAGMA user_version = {};", m.version))
                .map_err(StoreError::sqlite("set user_version"))?;
            tracing::debug!(version = m.version, name = m.name, "applied migration");
        }

        tx.commit().map_err(StoreError::sqlite("commit"))?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(StoreError::sqlite("read user_version"))
    }

    pub fn journal_mode(&self) -> Result<String> {
        self.conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(StoreError::sqlite("read journal_mode"))
    }

    /// The `synchronous` pragma actually in force, as SQLite reports it: 0 is OFF,
    /// 1 is NORMAL, 2 is FULL, 3 is EXTRA.
    pub fn synchronous(&self) -> Result<i64> {
        self.conn
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .map_err(StoreError::sqlite("read synchronous"))
    }

    // ── tasks ────────────────────────────────────────────────────────────

    pub fn insert_task(&self, task: &TaskRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO tasks (
                    id, goal, tool_id, state, created_at_ms, updated_at_ms,
                    started_at_ms, finished_at_ms, runner_pid, runner_created_ms,
                    request_json, outcome, outcome_detail
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                params![
                    task.id,
                    task.goal,
                    task.tool_id,
                    task.state.as_str(),
                    task.created_at_ms,
                    task.updated_at_ms,
                    task.started_at_ms,
                    task.finished_at_ms,
                    task.runner_pid.map(|p| p as i64),
                    task.runner_created_ms,
                    task.request_json,
                    task.outcome,
                    task.outcome_detail,
                ],
            )
            .map_err(StoreError::sqlite("insert task"))?;

        self.append_event(
            &task.id,
            &Event::new(
                EventKind::TaskCreated,
                format!("created in state {}", task.state),
            ),
        )?;
        Ok(())
    }

    pub fn task(&self, id: &str) -> Result<Option<TaskRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, goal, tool_id, state, created_at_ms, updated_at_ms,
                        started_at_ms, finished_at_ms, runner_pid, runner_created_ms,
                        request_json, outcome, outcome_detail
                 FROM tasks WHERE id = ?1",
            )
            .map_err(StoreError::sqlite("prepare task select"))?;

        stmt.query_row(params![id], row_to_task)
            .optional()
            .map_err(StoreError::sqlite("query task"))
    }

    pub fn require_task(&self, id: &str) -> Result<TaskRecord> {
        self.task(id)?
            .ok_or_else(|| StoreError::NoSuchTask(id.to_owned()))
    }

    pub fn list_tasks(&self, filter: TaskFilter, limit: usize) -> Result<Vec<TaskRecord>> {
        // The three filters are separate statements rather than a dynamic WHERE
        // clause: no string interpolation of a state name into SQL, ever.
        let (sql, state_param): (&str, Option<String>) = match filter {
            TaskFilter::All => (
                "SELECT id, goal, tool_id, state, created_at_ms, updated_at_ms,
                        started_at_ms, finished_at_ms, runner_pid, runner_created_ms,
                        request_json, outcome, outcome_detail
                 FROM tasks ORDER BY created_at_ms DESC, id DESC LIMIT ?1",
                None,
            ),
            TaskFilter::InFlight => (
                "SELECT id, goal, tool_id, state, created_at_ms, updated_at_ms,
                        started_at_ms, finished_at_ms, runner_pid, runner_created_ms,
                        request_json, outcome, outcome_detail
                 FROM tasks WHERE state IN ('queued','planning','awaiting_approval','running','verifying')
                 ORDER BY created_at_ms DESC, id DESC LIMIT ?1",
                None,
            ),
            TaskFilter::State(s) => (
                "SELECT id, goal, tool_id, state, created_at_ms, updated_at_ms,
                        started_at_ms, finished_at_ms, runner_pid, runner_created_ms,
                        request_json, outcome, outcome_detail
                 FROM tasks WHERE state = ?2 ORDER BY created_at_ms DESC, id DESC LIMIT ?1",
                Some(s.as_str().to_owned()),
            ),
        };

        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(StoreError::sqlite("prepare task list"))?;

        let rows = match state_param {
            Some(s) => stmt
                .query_map(params![limit as i64, s], row_to_task)
                .map_err(StoreError::sqlite("query task list"))?,
            None => stmt
                .query_map(params![limit as i64], row_to_task)
                .map_err(StoreError::sqlite("query task list"))?,
        };

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(StoreError::sqlite("read task row"))?);
        }
        Ok(out)
    }

    /// Every task in a non-terminal state. These are the reconciliation
    /// candidates; whether each is genuinely orphaned is the caller's call.
    pub fn in_flight_tasks(&self) -> Result<Vec<TaskRecord>> {
        self.list_tasks(TaskFilter::InFlight, usize::MAX)
    }

    /// Record which process owns a task, so a later run can tell an orphan from
    /// a live task.
    pub fn set_runner(&self, id: &str, pid: u32, created_ms: i64) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE tasks SET runner_pid = ?2, runner_created_ms = ?3, updated_at_ms = ?4
                 WHERE id = ?1",
                params![id, pid as i64, created_ms, now_ms()],
            )
            .map_err(StoreError::sqlite("set runner"))?;
        if n == 0 {
            return Err(StoreError::NoSuchTask(id.to_owned()));
        }
        Ok(())
    }

    /// Forget the runner association. Called after a task reaches a terminal
    /// state so a later reconciliation pass does not resurrect it.
    pub fn clear_runner(&self, id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE tasks SET runner_pid = NULL, runner_created_ms = NULL, updated_at_ms = ?2
                 WHERE id = ?1",
                params![id, now_ms()],
            )
            .map_err(StoreError::sqlite("clear runner"))?;
        Ok(())
    }

    /// Move a task to a new state, validated by the state machine, in one
    /// transaction with the event that explains it.
    ///
    /// This is the only path that changes `tasks.state`.
    pub fn transition(&self, id: &str, to: TaskState, reason: Option<&str>) -> Result<TaskRecord> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(StoreError::sqlite("begin transition"))?;

        let current: TaskRecord = tx
            .query_row(
                "SELECT id, goal, tool_id, state, created_at_ms, updated_at_ms,
                        started_at_ms, finished_at_ms, runner_pid, runner_created_ms,
                        request_json, outcome, outcome_detail
                 FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()
            .map_err(StoreError::sqlite("read task for transition"))?
            .ok_or_else(|| StoreError::NoSuchTask(id.to_owned()))?;

        // Reject before writing anything. The state machine, not SQL, is the
        // authority on what may follow what.
        current.state.transition(to)?;

        let now = now_ms();
        let started = match to {
            TaskState::Running if current.started_at_ms.is_none() => Some(now),
            _ => current.started_at_ms,
        };
        let finished = if to.is_terminal() { Some(now) } else { None };

        tx.execute(
            "UPDATE tasks SET state = ?2, updated_at_ms = ?3, started_at_ms = ?4,
                    finished_at_ms = COALESCE(?5, finished_at_ms)
             WHERE id = ?1 AND state = ?6",
            params![
                id,
                to.as_str(),
                now,
                started,
                finished,
                current.state.as_str()
            ],
        )
        .map_err(StoreError::sqlite("write transition"))?;

        tx.execute(
            "INSERT INTO events (task_id, at_ms, kind, detail) VALUES (?1,?2,?3,?4)",
            params![
                id,
                now,
                EventKind::StateChanged.as_str(),
                Event::state_changed(current.state, to, reason).detail,
            ],
        )
        .map_err(StoreError::sqlite("write transition event"))?;

        tx.commit()
            .map_err(StoreError::sqlite("commit transition"))?;

        self.require_task(id)
    }

    /// Record the final outcome. Called after the state has reached a terminal
    /// value; it does not itself change state.
    pub fn set_outcome(&self, id: &str, outcome: &str, detail: &str) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE tasks SET outcome = ?2, outcome_detail = ?3, updated_at_ms = ?4
                 WHERE id = ?1",
                params![id, outcome, detail, now_ms()],
            )
            .map_err(StoreError::sqlite("set outcome"))?;
        if n == 0 {
            return Err(StoreError::NoSuchTask(id.to_owned()));
        }
        Ok(())
    }

    // ── tool runs ────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn insert_tool_run(
        &self,
        task_id: &str,
        tool_id: &str,
        executable: &str,
        args_json: &str,
        working_dir: Option<&str>,
        env_json: &str,
    ) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO tool_runs (
                    task_id, tool_id, executable, args_json, working_dir, env_json, started_at_ms
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    task_id,
                    tool_id,
                    executable,
                    args_json,
                    working_dir,
                    env_json,
                    now_ms()
                ],
            )
            .map_err(StoreError::sqlite("insert tool run"))?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn record_child_pid(&self, run_id: i64, pid: u32) -> Result<()> {
        self.conn
            .execute(
                "UPDATE tool_runs SET child_pid = ?2 WHERE id = ?1",
                params![run_id, pid as i64],
            )
            .map_err(StoreError::sqlite("record child pid"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish_tool_run(
        &self,
        run_id: i64,
        exit_code: Option<i32>,
        termination: Termination,
        stdout: &StreamSummary,
        stderr: &StreamSummary,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE tool_runs SET
                    ended_at_ms = ?2, exit_code = ?3, termination = ?4,
                    stdout_path = ?5, stderr_path = ?6,
                    stdout_bytes = ?7, stderr_bytes = ?8,
                    stdout_truncated = ?9, stderr_truncated = ?10,
                    error = ?11
                 WHERE id = ?1",
                params![
                    run_id,
                    now_ms(),
                    exit_code,
                    termination.as_str(),
                    stdout.path.as_deref(),
                    stderr.path.as_deref(),
                    stdout.bytes as i64,
                    stderr.bytes as i64,
                    stdout.truncated as i64,
                    stderr.truncated as i64,
                    error,
                ],
            )
            .map_err(StoreError::sqlite("finish tool run"))?;
        Ok(())
    }

    pub fn tool_runs(&self, task_id: &str) -> Result<Vec<ToolRunRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, task_id, tool_id, executable, args_json, working_dir, env_json,
                        child_pid, started_at_ms, ended_at_ms, exit_code, termination,
                        stdout_path, stderr_path, stdout_bytes, stderr_bytes,
                        stdout_truncated, stderr_truncated, error
                 FROM tool_runs WHERE task_id = ?1 ORDER BY id ASC",
            )
            .map_err(StoreError::sqlite("prepare tool run select"))?;

        let rows = stmt
            .query_map(params![task_id], row_to_tool_run)
            .map_err(StoreError::sqlite("query tool runs"))?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(StoreError::sqlite("read tool run row"))?);
        }
        Ok(out)
    }

    // ── events ───────────────────────────────────────────────────────────

    pub fn append_event(&self, task_id: &str, event: &Event) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO events (task_id, at_ms, kind, detail) VALUES (?1,?2,?3,?4)",
                params![task_id, now_ms(), event.kind.as_str(), event.detail],
            )
            .map_err(StoreError::sqlite("append event"))?;
        Ok(())
    }

    pub fn events(&self, task_id: &str) -> Result<Vec<EventRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, task_id, at_ms, kind, detail FROM events WHERE task_id = ?1 ORDER BY seq ASC")
            .map_err(StoreError::sqlite("prepare event select"))?;

        let rows = stmt
            .query_map(params![task_id], |row| {
                let kind_str: String = row.get(3)?;
                Ok(EventRecord {
                    seq: row.get(0)?,
                    task_id: row.get(1)?,
                    at_ms: row.get(2)?,
                    // An unknown kind means a database written by a newer build.
                    // Surfacing it as Note keeps the history readable instead of
                    // failing the whole query.
                    kind: parse_event_kind(&kind_str),
                    detail: row.get(4)?,
                })
            })
            .map_err(StoreError::sqlite("query events"))?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(StoreError::sqlite("read event row"))?);
        }
        Ok(out)
    }

    /// Drop events older than `retain_days`. Bounded growth, per `store.retain_events_days`.
    pub fn prune_events(&self, retain_days: u32) -> Result<usize> {
        let cutoff = now_ms() - i64::from(retain_days) * 86_400_000;
        self.conn
            .execute("DELETE FROM events WHERE at_ms < ?1", params![cutoff])
            .map_err(StoreError::sqlite("prune events"))
    }

    /// Record the outcome of one bounded health probe.
    ///
    /// `unhealthy_after` is the caller's `tools.mark_unhealthy_after`, applied
    /// here so the counter and the status can never disagree: a reader that
    /// derived the status from a *different* threshold would report healthy for
    /// one config and unhealthy for the next.
    ///
    /// `detail` must already be redacted. This column is printed by `jean health`.
    pub fn record_probe(
        &self,
        tool_id: &str,
        ok: bool,
        unhealthy_after: u32,
        detail: Option<&str>,
    ) -> Result<ToolHealthRecord> {
        let at = now_ms();
        let previous = self.tool_health(tool_id)?;

        // A tool the operator switched off stays `disabled` however its probe
        // resolves: nothing should be started to test it, and if a probe arrived
        // anyway it is not evidence about a tool that was never run.
        let previous_status = previous.as_ref().map(|p| p.status);
        let disabled = previous_status == Some(HealthStatus::Disabled);

        let failures = match &previous {
            Some(p) if disabled => p.consecutive_failures,
            _ if ok => 0,
            Some(p) => p.consecutive_failures.saturating_add(1),
            None => 1,
        };
        let status = if disabled {
            HealthStatus::Disabled
        } else if ok {
            HealthStatus::Healthy
        } else if failures >= unhealthy_after.max(1) {
            HealthStatus::Unhealthy
        } else {
            // Below the threshold the tool is not yet unhealthy, and saying
            // `healthy` would hide a failure that just happened. The row keeps
            // whatever it said before and the counter carries the truth; `jean
            // health` prints both.
            previous_status.unwrap_or(HealthStatus::Unknown)
        };

        // `last_ok_ms` survives a later failure: "was working at T" is the fact
        // that makes an intermittent tool diagnosable, and it is not untrue
        // because the next probe failed.
        let last_ok_ms = if ok {
            Some(at)
        } else {
            previous.and_then(|p| p.last_ok_ms)
        };

        self.conn
            .execute(
                "INSERT INTO tool_health (
                     tool_id, status, consecutive_failures, last_check_ms, last_ok_ms, detail
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(tool_id) DO UPDATE SET
                     status               = excluded.status,
                     consecutive_failures = excluded.consecutive_failures,
                     last_check_ms        = excluded.last_check_ms,
                     last_ok_ms           = excluded.last_ok_ms,
                     detail               = excluded.detail",
                params![
                    tool_id,
                    status.as_str(),
                    i64::from(failures),
                    at,
                    last_ok_ms,
                    detail,
                ],
            )
            .map_err(StoreError::sqlite("record health probe"))?;

        // Read back rather than assemble, so what a caller gets is what a
        // concurrent writer left in the row.
        self.tool_health(tool_id)?
            .ok_or_else(|| StoreError::Serialization {
                what: "tool_health row",
                reason: format!("{tool_id} was written and then not found"),
            })
    }

    /// Mark a tool disabled without probing it.
    ///
    /// Called for `enabled = false` in `tools.toml`: the absence is a fact about
    /// configuration, and an unprobed tool must not read as an unhealthy one.
    pub fn set_disabled(&self, tool_id: &str, reason: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO tool_health (
                     tool_id, status, consecutive_failures, last_check_ms, last_ok_ms, detail
                 ) VALUES (?1, 'disabled', 0, NULL, NULL, ?2)
                 ON CONFLICT(tool_id) DO UPDATE SET status = 'disabled', detail = excluded.detail",
                params![tool_id, reason],
            )
            .map_err(StoreError::sqlite("mark tool disabled"))?;
        Ok(())
    }

    pub fn tool_health(&self, tool_id: &str) -> Result<Option<ToolHealthRecord>> {
        let mut rows = self
            .conn
            .prepare_cached(HEALTH_SELECT)
            .map_err(StoreError::sqlite("prepare tool_health read"))?;
        let mut rows = rows
            .query_map(params![tool_id], row_to_health)
            .map_err(StoreError::sqlite("read tool_health"))?;
        match rows.next() {
            Some(r) => r
                .map(Some)
                .map_err(StoreError::sqlite("read tool_health row")),
            None => Ok(None),
        }
    }

    /// Every recorded health row, ordered for a table the operator reads.
    pub fn all_tool_health(&self) -> Result<Vec<ToolHealthRecord>> {
        let mut rows = self
            .conn
            .prepare_cached("SELECT tool_id, status, consecutive_failures, last_check_ms, last_ok_ms, detail FROM tool_health ORDER BY tool_id")
            .map_err(StoreError::sqlite("prepare health list"))?;
        let rows = rows
            .query_map([], row_to_health)
            .map_err(StoreError::sqlite("list tool_health"))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::sqlite("read tool_health rows"))
    }

    /// Store a remembered approval.
    ///
    /// Re-presenting the same digest replaces the old row and its counter: the
    /// operator was asked again, which only happens because the earlier row had
    /// expired or been forgotten, and a fresh grant should not inherit an old
    /// usage history.
    pub fn remember_approval(&self, new: &NewApproval) -> Result<i64> {
        let now = now_ms();
        if new.expires_at_ms <= now {
            return Err(StoreError::Serialization {
                what: "approval",
                reason: format!(
                    "{} expires at {} which is not after now; remembering it would store a \
                     permission that cannot be used",
                    new.operation, new.expires_at_ms
                ),
            });
        }
        self.conn
            .execute(
                "INSERT INTO approvals (
                     scope_digest, operation, tool_id, tier, summary,
                     granted_at_ms, expires_at_ms, used_count
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)
                 ON CONFLICT(scope_digest) DO UPDATE SET
                     operation     = excluded.operation,
                     tool_id       = excluded.tool_id,
                     tier          = excluded.tier,
                     summary       = excluded.summary,
                     granted_at_ms = excluded.granted_at_ms,
                     expires_at_ms = excluded.expires_at_ms,
                     used_count    = 0",
                params![
                    new.scope_digest,
                    new.operation,
                    new.tool_id,
                    new.tier,
                    new.summary,
                    now,
                    new.expires_at_ms,
                ],
            )
            .map_err(StoreError::sqlite("remember approval"))?;
        self.approval_id_for_digest(&new.scope_digest)
    }

    fn approval_id_for_digest(&self, digest: &str) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT id FROM approvals WHERE scope_digest = ?1",
                params![digest],
                |row| row.get(0),
            )
            .map_err(StoreError::sqlite("read approval id"))
    }

    /// A usable approval for exactly this digest, if one exists.
    ///
    /// An expired row is not returned and not deleted here — `prune_approvals`
    /// does that — so the ledger keeps the record of a grant that ran out.
    pub fn find_approval(&self, scope_digest: &str) -> Result<Option<ApprovalRecord>> {
        let now = now_ms();
        let mut rows = self
            .conn
            .prepare_cached(APPROVAL_SELECT_BY_DIGEST)
            .map_err(StoreError::sqlite("prepare approval read"))?;
        let found = rows
            .query_map(params![scope_digest], row_to_approval)
            .map_err(StoreError::sqlite("read approval"))?
            .next()
            .transpose()
            .map_err(StoreError::sqlite("read approval row"))?;
        Ok(found.filter(|a| !a.is_expired(now)))
    }

    /// Count a use against the approval, so `jean approvals` can show which
    /// remembered grants are actually carrying work.
    pub fn touch_approval(&self, id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE approvals SET used_count = used_count + 1 WHERE id = ?1",
                params![id],
            )
            .map_err(StoreError::sqlite("count approval use"))?;
        Ok(())
    }

    pub fn list_approvals(&self) -> Result<Vec<ApprovalRecord>> {
        let mut rows = self
            .conn
            .prepare_cached(APPROVAL_SELECT_ALL)
            .map_err(StoreError::sqlite("prepare approval list"))?;
        let rows = rows
            .query_map([], row_to_approval)
            .map_err(StoreError::sqlite("list approvals"))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::sqlite("read approval rows"))
    }

    /// Forget one approval by id. Returns whether a row was actually removed, so
    /// `jean approvals forget` can say so rather than reporting silence.
    pub fn forget_approval(&self, id: i64) -> Result<bool> {
        let removed = self
            .conn
            .execute("DELETE FROM approvals WHERE id = ?1", params![id])
            .map_err(StoreError::sqlite("forget approval"))?;
        Ok(removed > 0)
    }

    /// Drop expired approvals, and trim the ledger to `max_rows` of the most
    /// recent grants. Returns how many rows went.
    ///
    /// The cap is what keeps a long-lived install from accumulating grants nobody
    /// can remember approving; when it bites, the oldest go.
    pub fn prune_approvals(&self, max_rows: u32) -> Result<usize> {
        let now = now_ms();
        let mut removed = self
            .conn
            .execute(
                "DELETE FROM approvals WHERE expires_at_ms <= ?1",
                params![now],
            )
            .map_err(StoreError::sqlite("prune expired approvals"))?;

        let max_rows = i64::from(max_rows);
        if max_rows > 0 {
            removed += self
                .conn
                .execute(
                    "DELETE FROM approvals WHERE id NOT IN (
                         SELECT id FROM approvals ORDER BY granted_at_ms DESC LIMIT ?1
                     )",
                    params![max_rows],
                )
                .map_err(StoreError::sqlite("trim approval ledger"))?;
        }
        Ok(removed)
    }

    // ── plans ────────────────────────────────────────────────────────────

    /// Write a validated plan and its steps, in one transaction.
    ///
    /// Both or neither: a plan row with no step rows would leave a reconciler holding an approved
    /// plan and no idea which effects were expected, and step rows without a plan would be effects
    /// with nothing behind them. `steps_json` is the plan as validated and is written alongside the
    /// rows — the rows record what happened, the document records what was agreed to.
    pub fn insert_plan(&self, plan: &NewPlan) -> Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(StoreError::sqlite("begin plan insert"))?;

        tx.execute(
            "INSERT INTO plans (
                id, task_id, schema_version, goal, provider, model, steps_json,
                attempts, created_at_ms
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                plan.id,
                plan.task_id,
                plan.schema_version as i64,
                plan.goal,
                plan.provider,
                plan.model,
                plan.steps_json,
                plan.attempts as i64,
                now_ms(),
            ],
        )
        .map_err(StoreError::sqlite("insert plan"))?;

        for step in &plan.steps {
            tx.execute(
                "INSERT INTO plan_steps (
                    plan_id, task_id, step_key, position, title, operation_kind,
                    tool_id, arguments_json, verify_method, state
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'pending')",
                params![
                    plan.id,
                    plan.task_id,
                    step.step_key,
                    step.position as i64,
                    step.title,
                    step.operation_kind,
                    step.tool_id,
                    step.arguments_json,
                    step.verify_method,
                ],
            )
            .map_err(StoreError::sqlite("insert plan step"))?;
        }

        tx.commit()
            .map_err(StoreError::sqlite("commit plan insert"))
    }

    /// The plan a task is working from: the newest one that has not been superseded.
    ///
    /// `None` for every deterministic Phase-0 and Phase-1 task — those have no plan, which is the
    /// point: a plan is a model artefact, and its absence is what distinguishes an instruction the
    /// operator gave from a proposal the model made.
    pub fn current_plan_for_task(&self, task_id: &str) -> Result<Option<PlanRecord>> {
        self.conn
            .query_row(PLAN_SELECT_FOR_TASK, params![task_id], row_to_plan)
            .optional()
            .map_err(StoreError::sqlite("query plan"))
    }

    /// Every plan the task was given, oldest first, replaced ones included.
    ///
    /// Replanning is bounded, and the bound is only auditable if the attempts are still there: this
    /// is how "the model took three goes and the first two were refused" survives to `jean task`.
    pub fn plans_for_task(&self, task_id: &str) -> Result<Vec<PlanRecord>> {
        let mut stmt = self
            .conn
            .prepare(PLAN_SELECT_ALL_FOR_TASK)
            .map_err(StoreError::sqlite("prepare plan list"))?;
        let rows = stmt
            .query_map(params![task_id], row_to_plan)
            .map_err(StoreError::sqlite("query plan list"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(StoreError::sqlite("read plan row"))?);
        }
        Ok(out)
    }

    /// The steps of one plan, in declaration order — which is also execution order.
    pub fn plan_steps(&self, plan_id: &str) -> Result<Vec<PlanStepRecord>> {
        let mut stmt = self
            .conn
            .prepare(STEP_SELECT_BY_PLAN)
            .map_err(StoreError::sqlite("prepare step list"))?;
        let rows = stmt
            .query_map(params![plan_id], row_to_plan_step)
            .map_err(StoreError::sqlite("query step list"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(StoreError::sqlite("read step row"))?);
        }
        Ok(out)
    }

    /// Mark a step as running, and attach the process run it became, once there is one.
    ///
    /// The state is written *before* the effect. That ordering is the whole reason a crash can be
    /// answered honestly: a step that is `running` with no `tool_run_id` never started, and a step
    /// that is `running` with one may have completed, so the reconciler must look rather than retry.
    pub fn start_plan_step(&self, plan_id: &str, step_key: &str) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE plan_steps SET state = 'running', started_at_ms = ?3
                 WHERE plan_id = ?1 AND step_key = ?2 AND state = 'pending'",
                params![plan_id, step_key, now_ms()],
            )
            .map_err(StoreError::sqlite("start plan step"))?;
        if n == 0 {
            return Err(StoreError::NoSuchPlanStep {
                plan_id: plan_id.to_owned(),
                step_key: step_key.to_owned(),
            });
        }
        Ok(())
    }

    /// Attach the process run to a step that has begun.
    pub fn set_plan_step_run(&self, plan_id: &str, step_key: &str, run_id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE plan_steps SET tool_run_id = ?3 WHERE plan_id = ?1 AND step_key = ?2",
                params![plan_id, step_key, run_id],
            )
            .map_err(StoreError::sqlite("attach step run"))?;
        Ok(())
    }

    /// Settle a step: its final state, the verdict its declared verification produced, and the
    /// evidence sentence that explains the verdict.
    ///
    /// `verdict` is the label of [`jean_core::verify::Verdict`] — `proven`, `not_proven`,
    /// `unverifiable` — and the schema's CHECK rejects anything else rather than storing a word that
    /// a later read would have to guess at. It is `None` for a step that never ran.
    pub fn finish_plan_step(
        &self,
        plan_id: &str,
        step_key: &str,
        state: PlanStepState,
        verdict: Option<&str>,
        evidence: &str,
    ) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE plan_steps SET state = ?3, verdict = ?4, evidence = ?5, ended_at_ms = ?6
                 WHERE plan_id = ?1 AND step_key = ?2",
                params![
                    plan_id,
                    step_key,
                    state.as_str(),
                    verdict,
                    evidence,
                    now_ms()
                ],
            )
            .map_err(StoreError::sqlite("finish plan step"))?;
        if n == 0 {
            return Err(StoreError::NoSuchPlanStep {
                plan_id: plan_id.to_owned(),
                step_key: step_key.to_owned(),
            });
        }
        Ok(())
    }

    /// Move a step that never ran to `skipped`, `cancelled` or `needs_attention`.
    ///
    /// Separate from [`Store::finish_plan_step`] because there is no verdict to record: a step that
    /// did not execute cannot be said to have been proven or unproven, and writing `not_proven` for
    /// it would blame the tool for the step's absence.
    pub fn stop_plan_step(
        &self,
        plan_id: &str,
        step_key: &str,
        state: PlanStepState,
        reason: &str,
    ) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE plan_steps SET state = ?3, evidence = ?4, ended_at_ms = ?5
                 WHERE plan_id = ?1 AND step_key = ?2 AND state = 'pending'",
                params![plan_id, step_key, state.as_str(), reason, now_ms()],
            )
            .map_err(StoreError::sqlite("stop plan step"))?;
        if n == 0 {
            return Err(StoreError::NoSuchPlanStep {
                plan_id: plan_id.to_owned(),
                step_key: step_key.to_owned(),
            });
        }
        Ok(())
    }

    /// Steps still marked `running` for a task: the effects whose outcome cannot be read from the
    /// record.
    ///
    /// This is what reconciliation asks. It returns candidates and changes nothing, because deciding
    /// whether the child actually ran needs the tool run and the verifier, and none of that belongs
    /// in a query.
    pub fn uncertain_plan_steps(&self, task_id: &str) -> Result<Vec<PlanStepRecord>> {
        let mut stmt = self
            .conn
            .prepare(STEP_SELECT_UNCERTAIN)
            .map_err(StoreError::sqlite("prepare uncertain steps"))?;
        let rows = stmt
            .query_map(params![task_id], row_to_plan_step)
            .map_err(StoreError::sqlite("query uncertain steps"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(StoreError::sqlite("read uncertain step"))?);
        }
        Ok(out)
    }

    /// Mark a plan replaced, so [`Store::current_plan_for_task`] stops returning it.
    pub fn supersede_plan(&self, plan_id: &str) -> Result<usize> {
        let n = self
            .conn
            .execute(
                "UPDATE plans SET superseded_at_ms = ?2 WHERE id = ?1 AND superseded_at_ms IS NULL",
                params![plan_id, now_ms()],
            )
            .map_err(StoreError::sqlite("supersede plan"))?;
        Ok(n)
    }

    pub fn count(&self, table: &'static str) -> Result<i64> {
        // Only the known table names are reachable; anything else is a programmer
        // error, and is rejected rather than interpolated. The three FTS tables are
        // here because `jean history status` asks how many index rows exist, and
        // naming them is the only way that question can be answered without
        // interpolating a caller's string into SQL.
        let allowed = matches!(
            table,
            "tasks"
                | "tool_runs"
                | "events"
                | "tool_health"
                | "approvals"
                | "plans"
                | "plan_steps"
                | "delegations"
                | "worker_events"
                | "saved_facts"
                | "preferences"
                | "tasks_fts"
                | "events_fts"
                | "saved_facts_fts"
                | "memory_changes"
        );
        if !allowed {
            return Err(StoreError::Serialization {
                what: "table name",
                reason: format!(
                    "{table:?} is not one of tasks|tool_runs|events|tool_health|approvals|plans|\
                     plan_steps|delegations|worker_events|saved_facts|preferences|tasks_fts|\
                     events_fts|saved_facts_fts|memory_changes"
                ),
            });
        }
        let sql = format!("SELECT COUNT(*) FROM {table}");
        self.conn
            .query_row(&sql, [], |row| row.get(0))
            .map_err(StoreError::sqlite("count rows"))
    }
}

const HEALTH_SELECT: &str = "SELECT tool_id, status, consecutive_failures, last_check_ms, last_ok_ms, detail FROM tool_health WHERE tool_id = ?1";

const APPROVAL_SELECT_BY_DIGEST: &str = "SELECT id, scope_digest, operation, tool_id, tier, summary, granted_at_ms, expires_at_ms, used_count FROM approvals WHERE scope_digest = ?1";

const APPROVAL_SELECT_ALL: &str = "SELECT id, scope_digest, operation, tool_id, tier, summary, granted_at_ms, expires_at_ms, used_count FROM approvals ORDER BY granted_at_ms DESC";

fn row_to_health(row: &rusqlite::Row<'_>) -> rusqlite::Result<ToolHealthRecord> {
    let status: String = row.get(1)?;
    let failures: i64 = row.get(2)?;
    Ok(ToolHealthRecord {
        tool_id: row.get(0)?,
        // An unrecognised status is a row from a newer build. `Unknown` is the
        // honest reading: we do not know what it means, so we will go and look.
        status: HealthStatus::from_str(&status).unwrap_or(HealthStatus::Unknown),
        consecutive_failures: u32::try_from(failures.max(0)).unwrap_or(u32::MAX),
        last_check_ms: row.get(3)?,
        last_ok_ms: row.get(4)?,
        detail: row.get(5)?,
    })
}

fn row_to_approval(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalRecord> {
    let used_count: i64 = row.get(8)?;
    Ok(ApprovalRecord {
        id: row.get(0)?,
        scope_digest: row.get(1)?,
        operation: row.get(2)?,
        tool_id: row.get(3)?,
        tier: row.get(4)?,
        summary: row.get(5)?,
        granted_at_ms: row.get(6)?,
        expires_at_ms: row.get(7)?,
        used_count: u32::try_from(used_count.max(0)).unwrap_or(u32::MAX),
    })
}

fn parse_event_kind(s: &str) -> EventKind {
    // One table, owned by the enum it describes. The hand-written match that used to live here
    // listed the Phase-0 and Phase-1 names only, so every Phase-2 event — a step starting, a
    // plan being refused — read back as a generic note.
    s.parse().unwrap_or(EventKind::Note)
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    let state_str: String = row.get(3)?;
    let runner_pid: Option<i64> = row.get(8)?;

    Ok(TaskRecord {
        id: row.get(0)?,
        goal: row.get(1)?,
        tool_id: row.get(2)?,
        // A state string this build does not know means the database came from a
        // newer version. `needs_attention` is the honest classification: it is
        // the state that means "a human must look at this".
        state: state_str
            .parse::<TaskState>()
            .unwrap_or(TaskState::NeedsAttention),
        created_at_ms: row.get(4)?,
        updated_at_ms: row.get(5)?,
        started_at_ms: row.get(6)?,
        finished_at_ms: row.get(7)?,
        runner_pid: runner_pid.map(|p| p as u32),
        runner_created_ms: row.get(9)?,
        request_json: row.get(10)?,
        outcome: row.get(11)?,
        outcome_detail: row.get(12)?,
    })
}

fn row_to_tool_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<ToolRunRecord> {
    let child_pid: Option<i64> = row.get(7)?;
    let termination: Option<String> = row.get(11)?;

    Ok(ToolRunRecord {
        id: row.get(0)?,
        task_id: row.get(1)?,
        tool_id: row.get(2)?,
        executable: row.get(3)?,
        args_json: row.get(4)?,
        working_dir: row.get(5)?,
        env_json: row.get(6)?,
        child_pid: child_pid.map(|p| p as u32),
        started_at_ms: row.get(8)?,
        ended_at_ms: row.get(9)?,
        exit_code: row.get(10)?,
        termination: termination.as_deref().and_then(Termination::from_str),
        stdout_path: row.get(12)?,
        stderr_path: row.get(13)?,
        stdout_bytes: row.get(14)?,
        stderr_bytes: row.get(15)?,
        stdout_truncated: row.get::<_, i64>(16)? != 0,
        stderr_truncated: row.get::<_, i64>(17)? != 0,
        error: row.get(18)?,
    })
}

/// The live plan for one task: the one that was never superseded, most recent first.
const PLAN_SELECT_FOR_TASK: &str =
    "SELECT id, task_id, schema_version, goal, provider, model, steps_json, attempts,
            created_at_ms, superseded_at_ms
     FROM plans WHERE task_id = ?1 AND superseded_at_ms IS NULL
     ORDER BY created_at_ms DESC, id DESC LIMIT 1";

const PLAN_SELECT_ALL_FOR_TASK: &str =
    "SELECT id, task_id, schema_version, goal, provider, model, steps_json, attempts,
            created_at_ms, superseded_at_ms
     FROM plans WHERE task_id = ?1 ORDER BY created_at_ms ASC, id ASC";

const STEP_SELECT_BY_PLAN: &str =
    "SELECT id, plan_id, task_id, step_key, position, title, operation_kind, tool_id,
            arguments_json, verify_method, state, tool_run_id, verdict, evidence,
            started_at_ms, ended_at_ms
     FROM plan_steps WHERE plan_id = ?1 ORDER BY position ASC";

const STEP_SELECT_UNCERTAIN: &str =
    "SELECT id, plan_id, task_id, step_key, position, title, operation_kind, tool_id,
            arguments_json, verify_method, state, tool_run_id, verdict, evidence,
            started_at_ms, ended_at_ms
     FROM plan_steps WHERE task_id = ?1 AND state = 'running' ORDER BY position ASC";

fn row_to_plan(row: &rusqlite::Row<'_>) -> rusqlite::Result<PlanRecord> {
    let schema_version: i64 = row.get(2)?;
    let attempts: i64 = row.get(7)?;
    Ok(PlanRecord {
        id: row.get(0)?,
        task_id: row.get(1)?,
        schema_version: u32::try_from(schema_version.max(0)).unwrap_or(u32::MAX),
        goal: row.get(3)?,
        provider: row.get(4)?,
        model: row.get(5)?,
        steps_json: row.get(6)?,
        attempts: u32::try_from(attempts.max(0)).unwrap_or(u32::MAX),
        created_at_ms: row.get(8)?,
        superseded_at_ms: row.get(9)?,
    })
}

fn row_to_plan_step(row: &rusqlite::Row<'_>) -> rusqlite::Result<PlanStepRecord> {
    let state: String = row.get(10)?;
    let position: i64 = row.get(4)?;
    Ok(PlanStepRecord {
        id: row.get(0)?,
        plan_id: row.get(1)?,
        task_id: row.get(2)?,
        step_key: row.get(3)?,
        position: u32::try_from(position.max(0)).unwrap_or(u32::MAX),
        title: row.get(5)?,
        operation_kind: row.get(6)?,
        tool_id: row.get(7)?,
        arguments_json: row.get(8)?,
        verify_method: row.get(9)?,
        // The CHECK constraint limits this column to the eight names, so an unparseable state can
        // only come from a build newer than this one — which `FromTheFuture` already refuses at
        // open. `NeedsAttention` is the reading that neither retries nor reports success.
        state: PlanStepState::from_str(&state).unwrap_or(PlanStepState::NeedsAttention),
        tool_run_id: row.get(11)?,
        verdict: row.get(12)?,
        evidence: row.get(13)?,
        started_at_ms: row.get(14)?,
        ended_at_ms: row.get(15)?,
    })
}

/// A delegation ready to persist: one outcome a person asked for, one worker, one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDelegation {
    pub id: String,
    pub task_id: String,
    /// The connector name, as configured (`opencode`, `codex`).
    pub worker: String,
    /// The protocol shape it speaks (`acp`, `codex-cli`). Recorded, not inferred later.
    pub kind: String,
    /// The operator's own words. Never the worker's restatement of them.
    pub goal: String,
    pub workspace: String,
    pub worktree: Option<String>,
    pub worktree_note: Option<String>,
    /// Resolved before delegation, so the row states what had to pass.
    pub verify_command: String,
    pub verify_args_json: String,
}

/// A delegation as recorded. The columns that hold a claim and the columns that hold an
/// observation are deliberately not merged: reading this row must show both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationRecord {
    pub id: String,
    pub task_id: String,
    pub worker: String,
    pub kind: String,
    pub goal: String,
    pub workspace: String,
    pub worktree: Option<String>,
    pub worktree_note: Option<String>,
    pub verify_command: String,
    pub verify_args_json: String,
    pub baseline_verdict: Option<String>,
    pub baseline_run_id: Option<i64>,
    pub agent_stop_reason: Option<String>,
    pub agent_claim: Option<String>,
    pub worker_exit_code: Option<i32>,
    pub verification_verdict: Option<String>,
    pub verification_run_id: Option<i64>,
    pub changed_files_json: String,
    pub changed_file_count: i64,
    pub result: Option<String>,
    pub output_dir: Option<String>,
    pub created_at_ms: i64,
    pub settled_at_ms: Option<i64>,
    pub notes_json: String,
}

/// One progress row from a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerEventRecord {
    pub delegation_id: String,
    pub seq: i64,
    /// An [`crate::EventKind`] name, so a transcript and the audit trail use one vocabulary.
    pub kind: String,
    /// Redacted before the write: worker output is where a token is most likely to appear.
    pub detail: String,
    pub at_ms: i64,
}

impl Store {
    /// Record a delegation at the moment it is accepted, before any worker exists.
    pub fn insert_delegation(&self, row: &NewDelegation) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO delegations (
                    id, task_id, worker, kind, goal, workspace, worktree, worktree_note,
                    verify_command, verify_args_json, created_at_ms
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    row.id,
                    row.task_id,
                    row.worker,
                    row.kind,
                    row.goal,
                    row.workspace,
                    row.worktree,
                    row.worktree_note,
                    row.verify_command,
                    row.verify_args_json,
                    now_ms(),
                ],
            )
            .map_err(StoreError::sqlite("insert delegation"))?;
        Ok(())
    }

    pub fn delegation(&self, id: &str) -> Result<Option<DelegationRecord>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, task_id, worker, kind, goal, workspace, worktree, worktree_note,
                        verify_command, verify_args_json, baseline_verdict, baseline_run_id,
                        agent_stop_reason, agent_claim, worker_exit_code, verification_verdict,
                        verification_run_id, changed_files_json, changed_file_count, result,
                        output_dir, created_at_ms, settled_at_ms, notes_json
                 FROM delegations WHERE id = ?1",
                params![id],
                delegation_from_row,
            )
            .optional()
            .map_err(StoreError::sqlite("read delegation"))?;
        Ok(row)
    }

    /// Delegations belonging to one task, oldest first.
    pub fn delegations_for_task(&self, task_id: &str) -> Result<Vec<DelegationRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, task_id, worker, kind, goal, workspace, worktree, worktree_note,
                        verify_command, verify_args_json, baseline_verdict, baseline_run_id,
                        agent_stop_reason, agent_claim, worker_exit_code, verification_verdict,
                        verification_run_id, changed_files_json, changed_file_count, result,
                        output_dir, created_at_ms, settled_at_ms, notes_json
                 FROM delegations WHERE task_id = ?1 ORDER BY created_at_ms, id",
            )
            .map_err(StoreError::sqlite("prepare delegations"))?;
        let rows = stmt
            .query_map(params![task_id], delegation_from_row)
            .map_err(StoreError::sqlite("list delegations"))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::sqlite("read delegation rows"))
    }

    /// Note where the worker actually ran, once isolation is known.
    ///
    /// Returns `false` when no row matches, which is how a caller learns the delegation id was
    /// never written, rather than silently updating nothing.
    pub fn set_delegation_worktree(
        &self,
        id: &str,
        worktree: Option<&str>,
        note: Option<&str>,
    ) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE delegations SET worktree = ?2, worktree_note = ?3 WHERE id = ?1",
                params![id, worktree, note],
            )
            .map_err(StoreError::sqlite("record worktree"))?;
        Ok(n == 1)
    }

    /// Store the baseline verification verdict and the run that produced it.
    pub fn record_delegation_baseline(
        &self,
        id: &str,
        verdict: &str,
        run_id: Option<i64>,
    ) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE delegations SET baseline_verdict = ?2, baseline_run_id = ?3 WHERE id = ?1",
                params![id, verdict, run_id],
            )
            .map_err(StoreError::sqlite("record baseline"))?;
        Ok(n == 1)
    }

    /// How the worker's session ended: its own stop reason and claim, its exit code, and where its
    /// captured output lives.
    pub fn record_worker_settled(
        &self,
        id: &str,
        stop_reason: Option<&str>,
        claim: Option<&str>,
        exit_code: Option<i32>,
        output_dir: Option<&str>,
    ) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE delegations SET agent_stop_reason = ?2, agent_claim = ?3,
                        worker_exit_code = ?4, output_dir = COALESCE(?5, output_dir)
                 WHERE id = ?1",
                params![id, stop_reason, claim, exit_code, output_dir],
            )
            .map_err(StoreError::sqlite("record worker settled"))?;
        Ok(n == 1)
    }

    /// The changed-file summary, taken from the workspace.
    pub fn record_delegation_changes(
        &self,
        id: &str,
        files_json: &str,
        count: i64,
    ) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE delegations SET changed_files_json = ?2, changed_file_count = ?3 WHERE id = ?1",
                params![id, files_json, count],
            )
            .map_err(StoreError::sqlite("record changes"))?;
        Ok(n == 1)
    }

    /// The verdict of the verification JEAN ran itself.
    pub fn record_delegation_verification(
        &self,
        id: &str,
        verdict: &str,
        run_id: Option<i64>,
    ) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE delegations SET verification_verdict = ?2, verification_run_id = ?3
                 WHERE id = ?1",
                params![id, verdict, run_id],
            )
            .map_err(StoreError::sqlite("record verification"))?;
        Ok(n == 1)
    }

    /// Close a delegation with JEAN's own result.
    ///
    /// A delegation already settled is not overwritten: two reports of the same delegation would
    /// mean a second settle path, and the first one's facts are the record.
    pub fn settle_delegation(&self, id: &str, result: &str) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE delegations SET result = ?2, settled_at_ms = ?3
                 WHERE id = ?1 AND result IS NULL",
                params![id, result, now_ms()],
            )
            .map_err(StoreError::sqlite("settle delegation"))?;
        Ok(n == 1)
    }

    /// Free-form notes, redacted, as JSON. Overwrites the previous value: callers append in memory
    /// so the row always holds one complete list rather than a merge of partial ones.
    pub fn set_delegation_notes(&self, id: &str, notes_json: &str) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE delegations SET notes_json = ?2 WHERE id = ?1",
                params![id, notes_json],
            )
            .map_err(StoreError::sqlite("set delegation notes"))?;
        Ok(n == 1)
    }

    /// Append one worker progress row.
    pub fn append_worker_event(
        &self,
        delegation_id: &str,
        seq: i64,
        kind: &str,
        detail: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO worker_events (delegation_id, seq, kind, detail, at_ms)
                 VALUES (?1,?2,?3,?4,?5)",
                params![delegation_id, seq, kind, detail, now_ms()],
            )
            .map_err(StoreError::sqlite("append worker event"))?;
        Ok(())
    }

    /// The transcript, in arrival order, up to `limit`.
    pub fn worker_events(
        &self,
        delegation_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkerEventRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT delegation_id, seq, kind, detail, at_ms FROM worker_events
                 WHERE delegation_id = ?1 ORDER BY seq LIMIT ?2",
            )
            .map_err(StoreError::sqlite("prepare worker events"))?;
        let rows = stmt
            .query_map(params![delegation_id, limit as i64], |row| {
                Ok(WorkerEventRecord {
                    delegation_id: row.get(0)?,
                    seq: row.get(1)?,
                    kind: row.get(2)?,
                    detail: row.get(3)?,
                    at_ms: row.get(4)?,
                })
            })
            .map_err(StoreError::sqlite("list worker events"))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::sqlite("read worker event rows"))
    }

    /// Delegations that were still running when this process started, for reconciliation.
    pub fn unsettled_delegations(&self) -> Result<Vec<DelegationRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, task_id, worker, kind, goal, workspace, worktree, worktree_note,
                        verify_command, verify_args_json, baseline_verdict, baseline_run_id,
                        agent_stop_reason, agent_claim, worker_exit_code, verification_verdict,
                        verification_run_id, changed_files_json, changed_file_count, result,
                        output_dir, created_at_ms, settled_at_ms, notes_json
                 FROM delegations WHERE result IS NULL ORDER BY created_at_ms, id",
            )
            .map_err(StoreError::sqlite("prepare unsettled delegations"))?;
        let rows = stmt
            .query_map([], delegation_from_row)
            .map_err(StoreError::sqlite("list unsettled delegations"))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::sqlite("read unsettled delegation rows"))
    }
}

fn delegation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DelegationRecord> {
    Ok(DelegationRecord {
        id: row.get(0)?,
        task_id: row.get(1)?,
        worker: row.get(2)?,
        kind: row.get(3)?,
        goal: row.get(4)?,
        workspace: row.get(5)?,
        worktree: row.get(6)?,
        worktree_note: row.get(7)?,
        verify_command: row.get(8)?,
        verify_args_json: row.get(9)?,
        baseline_verdict: row.get(10)?,
        baseline_run_id: row.get(11)?,
        agent_stop_reason: row.get(12)?,
        agent_claim: row.get(13)?,
        worker_exit_code: row.get::<_, Option<i32>>(14)?,
        verification_verdict: row.get(15)?,
        verification_run_id: row.get(16)?,
        changed_files_json: row.get(17)?,
        changed_file_count: row.get(18)?,
        result: row.get(19)?,
        output_dir: row.get(20)?,
        created_at_ms: row.get(21)?,
        settled_at_ms: row.get(22)?,
        notes_json: row.get(23)?,
    })
}

pub fn format_ms(ms: i64) -> String {
    const fn is_leap(y: i64) -> bool {
        (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
    }
    const MDAYS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);

    let mut days = secs.div_euclid(86_400);
    let day_secs = secs.rem_euclid(86_400);

    let mut year = 1970i64;
    loop {
        let len = if is_leap(year) { 366 } else { 365 };
        if days < len {
            break;
        }
        days -= len;
        year += 1;
    }

    let mut month = 0usize;
    while month < 12 {
        let mut len = MDAYS[month];
        if month == 1 && is_leap(year) {
            len += 1;
        }
        if days < len {
            break;
        }
        days -= len;
        month += 1;
    }

    format!(
        "{year:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}.{ms:03}Z",
        m = month + 1,
        d = days + 1,
        h = day_secs / 3600,
        mi = (day_secs % 3600) / 60,
        s = day_secs % 60,
        ms = millis
    )
}

#[cfg(test)]
mod tests;
