//! Searchable history and operator-owned memory, on the same connection as the
//! records themselves.
//!
//! ## Why the index is derived, not authoritative
//!
//! `tasks_fts` and `events_fts` (version 5 of the schema) are a search key over
//! text the writer already redacted. Every hit is resolved by joining back to
//! `tasks` or `events`, so the state, outcome and timestamp a search reports are
//! the row's own — an index row that drifted would show a *stale snippet*, never a
//! stale fact about what happened. [`Store::rebuild_history_index`] is the recovery
//! path when an operator wants the derived copy rebuilt anyway, and it writes
//! nothing but the derived tables.
//!
//! ## What search is not
//!
//! Search is a read. It authorises nothing and grants nothing, and none of what it
//! returns is handed to a process. That is not only a convention here: the queries
//! run with `PRAGMA query_only` set, so a write attempted by this path fails rather
//! than happening, and the retrieved text arrives as a `String` in a struct with no
//! executable field. A fact whose text is `run powershell.exe -Command …` is a
//! sentence about something somebody once saved, and the only way it becomes an
//! action is an operator typing a command. `jean_core::memory` states the same rule
//! from the caller's side.

use rusqlite::{params, Connection, OptionalExtension};

use jean_protocol::preferences::PreferenceKey;

use crate::{now_ms, Result, Store, StoreError};

/// The longest query this build will run.
///
/// A limit on the query is a limit on the work: an FTS5 expression's cost grows
/// with the number of terms in it, and an unbounded paste from a log file would be
/// an operator paying for a search they did not mean to ask for.
pub const MAX_QUERY_CHARS: usize = 256;

/// Results returned when the caller does not choose.
pub const DEFAULT_SEARCH_LIMIT: usize = 20;

/// The most a caller may ask for, whichever way they ask it.
pub const MAX_SEARCH_LIMIT: usize = 100;

/// How many words of context `snippet()` may surround a match with.
pub const SNIPPET_TOKENS: usize = 12;

/// Markers around the matched word. Plain brackets, because a search result is read
/// in a terminal and may be pasted into a bug report.
const MARKER_OPEN: &str = "[";
const MARKER_CLOSE: &str = "]";

/// The longest text an operator may save as a fact. The schema states the same
/// bound (`CHECK (length(text) BETWEEN 1 AND 1000)`) so a hand-written row cannot
/// outgrow what the list is prepared to display.
pub const MAX_FACT_CHARS: usize = 1000;

/// The longest scope label, matching the schema's `CHECK (length(scope) <= 260)`.
/// 260 is the practical Windows path ceiling.
pub const MAX_SCOPE_CHARS: usize = 260;

/// The most facts a listing will return. Facts are few by design — a person typed
/// each one — so this is a ceiling against a database someone filled by hand, not
/// an expected page size.
pub const MAX_LISTED_FACTS: usize = 500;

/// Which record a history hit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistorySource {
    /// A task, matched on its goal or the detail of its outcome.
    Task,
    /// An event in the audit trail, matched on its detail.
    Event,
}

impl HistorySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Event => "event",
        }
    }
}

/// One history search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryHit {
    pub source: HistorySource,
    /// The task this hit belongs to — the identifier an operator acts on with
    /// `jean task <id>`, and nothing more than an identifier.
    pub task_id: String,
    /// When the matched record happened: creation time for a task, event time for
    /// an event.
    pub at_ms: i64,
    /// The task's recorded state, read live from `tasks`. `None` means the task row
    /// is gone — history outliving a removed row is deliberate (see the
    /// no-foreign-key note in version 1) and is reported rather than hidden.
    pub state: Option<String>,
    /// The task's recorded outcome, same provenance as `state`.
    pub outcome: Option<String>,
    /// The event's kind, when the hit is an event.
    pub event_kind: Option<String>,
    /// The event's `seq`, so an operator can point at one row of the trail.
    pub event_seq: Option<i64>,
    /// The matched text with the match marked, bounded by [`SNIPPET_TOKENS`].
    pub snippet: String,
}

/// How to run one search.
#[derive(Debug, Clone)]
pub struct SearchOptions<'a> {
    pub query: &'a str,
    /// Upper bound on hits. Clamped to [`MAX_SEARCH_LIMIT`] rather than refused: an
    /// operator who asks for 1000 results is asking for the newest ones.
    pub limit: usize,
    /// Read the query as an FTS5 expression instead of as literal text.
    ///
    /// Off unless the caller asked for it, and it changes nothing else: the
    /// expression is still bound as a parameter, so this enables `NEAR`/`OR`/prefix
    /// matching and cannot enable SQL.
    pub fts_syntax: bool,
}

impl<'a> Default for SearchOptions<'a> {
    fn default() -> Self {
        Self {
            query: "",
            limit: DEFAULT_SEARCH_LIMIT,
            fts_syntax: false,
        }
    }
}

/// A saved fact, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedFact {
    pub id: String,
    /// The operator's words, as saved.
    pub text: String,
    /// `""` is global; anything else is the project this fact was saved under.
    pub scope: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl SavedFact {
    /// `None` for a global fact, the project for a scoped one. Scope is data that
    /// narrows a listing; it never widens what an operation may do.
    pub fn project(&self) -> Option<&str> {
        (!self.scope.is_empty()).then_some(self.scope.as_str())
    }
}

/// A saved fact that matched a search, with the words that matched marked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactHit {
    pub fact: SavedFact,
    pub snippet: String,
}

/// What `add_saved_fact` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactAddOutcome {
    Created(SavedFact),
    /// The same words were already saved in this scope, with the record that was
    /// already there. "Is this already saved?" is a question an operator asks out
    /// loud, and the existing id is the useful answer — so this is a result, not an
    /// error, and the table is left with exactly one row either way.
    Duplicate(SavedFact),
}

/// A stored preference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preference {
    pub key: String,
    pub value: String,
    pub set_at_ms: i64,
}

/// How many rows the index and its sources agree on, for `jean history status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryIndexReport {
    /// Whether this build's SQLite reports FTS5 as compiled in.
    pub fts5_compiled_in: bool,
    pub tasks_total: i64,
    pub tasks_indexed: i64,
    pub events_total: i64,
    pub events_indexed: i64,
    /// Source rows with no index row: records that cannot be found.
    pub tasks_missing_from_index: i64,
    pub events_missing_from_index: i64,
    /// Index rows pointing at nothing. Expected to be zero; a non-zero count is a
    /// report, not a search failure, and [`Store::rebuild_history_index`] clears it.
    pub index_rows_without_source: i64,
}

impl HistoryIndexReport {
    /// Whether a search can be trusted to see every record.
    pub fn is_consistent(&self) -> bool {
        self.fts5_compiled_in
            && self.tasks_missing_from_index == 0
            && self.events_missing_from_index == 0
            && self.index_rows_without_source == 0
    }
}

impl Store {
    // ── history search ───────────────────────────────────────────────────

    /// Search redacted task and event text, newest first.
    ///
    /// The work is bounded twice over: each source is asked for at most `limit`
    /// rows by `rowid DESC` — which for both tables is insertion order, so "the
    /// newest matches" — and the merged list is truncated to `limit`. That is what
    /// makes this a search rather than a scan: the FTS5 index answers the question,
    /// and no statement here walks the whole history.
    pub fn search_history(&self, options: &SearchOptions<'_>) -> Result<Vec<HistoryHit>> {
        let expression = self.search_expression(options)?;
        let limit = options.limit.clamp(1, MAX_SEARCH_LIMIT);
        // One read-only window over both halves: a search that could write is not a
        // search, whatever the code around it says.
        let guard = QueryOnly::new(&self.conn)?;
        let _budget = SearchBudget::install(&self.conn)?;

        let mut hits = self.task_hits(&expression, limit)?;
        hits.extend(self.event_hits(&expression, limit)?);
        drop(guard);

        hits.sort_by_key(|hit| std::cmp::Reverse(hit.at_ms));
        hits.truncate(limit);
        Ok(hits)
    }

    /// Whether this build's SQLite has FTS5.
    ///
    /// Checked by asking SQLite, not by reading the dependency list: `bundled`
    /// compiles SQLite inside this build, and which optional modules that
    /// compilation enabled is a property of the resulting binary. A missing FTS5 is
    /// reported here and by the migration that fails to run — never papered over
    /// with a `LIKE '%…%'` walk of every row, which would answer correctly and cost
    /// without bound.
    pub fn fts5_compiled_in(&self) -> Result<bool> {
        let mut stmt = self
            .conn
            .prepare("PRAGMA compile_options")
            .map_err(StoreError::sqlite("prepare compile options"))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(StoreError::sqlite("read compile options"))?;
        for option in rows {
            // SQLite prints each option without the `SQLITE_` prefix it carries as a
            // C define, so compare against what the pragma actually reports.
            if option
                .map_err(StoreError::sqlite("read compile option"))?
                .eq_ignore_ascii_case("ENABLE_FTS5")
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Do the index and its sources still agree?
    pub fn history_index_report(&self) -> Result<HistoryIndexReport> {
        let count = |sql: &str| -> Result<i64> {
            self.conn
                .query_row(sql, [], |row| row.get(0))
                .map_err(StoreError::sqlite("history index report"))
        };
        Ok(HistoryIndexReport {
            fts5_compiled_in: self.fts5_compiled_in()?,
            tasks_total: count("SELECT COUNT(*) FROM tasks")?,
            tasks_indexed: count("SELECT COUNT(*) FROM tasks_fts")?,
            events_total: count("SELECT COUNT(*) FROM events")?,
            events_indexed: count("SELECT COUNT(*) FROM events_fts")?,
            tasks_missing_from_index: count(
                "SELECT COUNT(*) FROM tasks WHERE NOT EXISTS \
                 (SELECT 1 FROM tasks_fts WHERE tasks_fts.rowid = tasks.rowid)",
            )?,
            events_missing_from_index: count(
                "SELECT COUNT(*) FROM events WHERE NOT EXISTS \
                 (SELECT 1 FROM events_fts WHERE events_fts.rowid = events.seq)",
            )?,
            index_rows_without_source: count(
                "SELECT (SELECT COUNT(*) FROM tasks_fts WHERE NOT EXISTS \
                    (SELECT 1 FROM tasks WHERE tasks.rowid = tasks_fts.rowid)) \
                  + (SELECT COUNT(*) FROM events_fts WHERE NOT EXISTS \
                    (SELECT 1 FROM events WHERE events.seq = events_fts.rowid))",
            )?,
        })
    }

    /// Rebuild the derived index from the rows it describes.
    ///
    /// Writes only the three FTS tables. The records themselves are untouched,
    /// which is what makes this safe against a live history: the worst it can do is
    /// fail partway and leave an index derived from the same rows it already was.
    pub fn rebuild_history_index(&self) -> Result<HistoryIndexReport> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(StoreError::sqlite("begin rebuild"))?;
        tx.execute_batch(
            "DELETE FROM tasks_fts;
             DELETE FROM events_fts;
             DELETE FROM saved_facts_fts;
             INSERT INTO tasks_fts(rowid, goal, outcome_detail)
                 SELECT rowid, substr(goal, 1, 512),
                        substr(COALESCE(outcome_detail, ''), 1, 512) FROM tasks;
             INSERT INTO events_fts(rowid, detail)
                 SELECT seq, substr(detail, 1, 512) FROM events;
             INSERT INTO saved_facts_fts(rowid, text)
                 SELECT rowid, substr(text, 1, 512) FROM saved_facts;",
        )
        .map_err(StoreError::sqlite("rebuild history index"))?;
        tx.commit().map_err(StoreError::sqlite("commit rebuild"))?;
        self.history_index_report()
    }

    // ── saved facts ──────────────────────────────────────────────────────

    /// Save a fact the operator chose to keep.
    ///
    /// `id` is supplied by the caller because identity is not a storage detail: an
    /// id has to be stable for as long as the record is, and `add` reporting the id
    /// it used is what lets `jean memory add` be scripted against its own output.
    ///
    /// Uniqueness is (scope, normalised text). The same words in a different scope
    /// are deliberately two records: a preference saved globally and the same
    /// preference saved for one project are things an operator may want to forget
    /// separately.
    pub fn add_saved_fact(
        &self,
        id: &str,
        text: &str,
        scope: Option<&str>,
    ) -> Result<FactAddOutcome> {
        let text = valid_fact_text(text)?;
        let scope = valid_fact_scope(scope.unwrap_or(""))?;
        let norm = normalize_for_comparison(&text);
        let now = now_ms();

        // `DO NOTHING` rather than a caught constraint error: a duplicate is an
        // expected answer, and matching on SQLite's message text to tell it from a
        // real failure would break on the next SQLite release.
        let written = self.mutate_with_audit(
            "INSERT INTO saved_facts (id, text, scope, norm, source, created_at_ms, updated_at_ms)
             VALUES (?1,?2,?3,?4,'operator_command',?5,?5)
             ON CONFLICT (scope, norm) DO NOTHING",
            &[&id, &text, &scope, &norm, &now],
            "add",
            id,
        )?;
        if written == 0 {
            let existing = self
                .conn
                .query_row(
                    "SELECT id, text, scope, created_at_ms, updated_at_ms FROM saved_facts
                     WHERE scope = ?1 AND norm = ?2",
                    params![scope, norm],
                    row_to_fact,
                )
                .optional()
                .map_err(StoreError::sqlite("read existing fact"))?
                .ok_or_else(|| StoreError::Memory {
                    reason:
                        "the words are already saved, but the row that says so cannot be read; \
                             the index and the table disagree"
                            .to_owned(),
                })?;
            return Ok(FactAddOutcome::Duplicate(existing));
        }
        Ok(FactAddOutcome::Created(self.require_fact(id)?))
    }

    /// One fact by id.
    pub fn saved_fact(&self, id: &str) -> Result<Option<SavedFact>> {
        self.conn
            .query_row(
                "SELECT id, text, scope, created_at_ms, updated_at_ms FROM saved_facts WHERE id = ?1",
                params![id],
                row_to_fact,
            )
            .optional()
            .map_err(StoreError::sqlite("read saved fact"))
    }

    /// Saved facts, newest first, optionally narrowed to one scope.
    ///
    /// A scoped listing means "global facts plus this project's", because a listing
    /// that hid the global ones would be answering a different question than the
    /// operator asked.
    pub fn saved_facts(&self, scope: Option<&str>, limit: usize) -> Result<Vec<SavedFact>> {
        let limit = limit.clamp(1, MAX_LISTED_FACTS) as i64;
        let mut out = match scope.map(valid_scope_or_global).transpose()? {
            Some(scope) => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, text, scope, created_at_ms, updated_at_ms FROM saved_facts
                         WHERE scope = ?1 OR scope = '' ORDER BY created_at_ms DESC, id DESC LIMIT ?2",
                    )
                    .map_err(StoreError::sqlite("prepare scoped facts"))?;
                let rows = stmt
                    .query_map(params![scope, limit], row_to_fact)
                    .map_err(StoreError::sqlite("query scoped facts"))?;
                collect(rows, "read saved fact")?
            }
            None => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, text, scope, created_at_ms, updated_at_ms FROM saved_facts
                         WHERE scope = '' ORDER BY created_at_ms DESC, id DESC LIMIT ?1",
                    )
                    .map_err(StoreError::sqlite("prepare facts"))?;
                let rows = stmt
                    .query_map(params![limit], row_to_fact)
                    .map_err(StoreError::sqlite("query facts"))?;
                collect(rows, "read saved fact")?
            }
        };
        out.truncate(limit as usize);
        Ok(out)
    }

    /// Rewrite an existing fact's text, keeping its identity and its creation time.
    ///
    /// `Ok(None)` means no fact has that id. The caller has to be able to say that
    /// plainly: "I edited the fact you meant" is the failure this is designed to
    /// make impossible.
    pub fn update_saved_fact(&self, id: &str, text: &str) -> Result<Option<SavedFact>> {
        let text = valid_fact_text(text)?;
        let existing = match self.saved_fact(id)? {
            Some(fact) => fact,
            None => return Ok(None),
        };
        let norm = normalize_for_comparison(&text);
        if let Some(other) = self.fact_with_norm_except(&existing.scope, &norm, id)? {
            return Err(StoreError::Memory {
                reason: format!(
                    "those words are already saved as {} in scope {:?}; edit that one instead",
                    other.id,
                    if other.scope.is_empty() {
                        "global"
                    } else {
                        &other.scope
                    }
                ),
            });
        }
        let n = self.mutate_with_audit(
            "UPDATE saved_facts SET text = ?2, norm = ?3, updated_at_ms = ?4 WHERE id = ?1",
            &[&id, &text, &norm, &now_ms()],
            "update",
            id,
        )?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(self.require_fact(id)?))
    }

    /// Remove one fact by id, returning the record that was forgotten.
    ///
    /// The text comes back to the caller so the CLI can name what it removed —
    /// "did you forget the right thing?" needs the words, not a digest — and that
    /// print is the last the build will have of them: no audit field keeps a copy.
    /// The row and its index entry go together, via the delete trigger.
    pub fn forget_saved_fact(&self, id: &str) -> Result<Option<SavedFact>> {
        let existing = self.saved_fact(id)?;
        if existing.is_none() {
            return Ok(None);
        }
        let n = self.mutate_with_audit(
            "DELETE FROM saved_facts WHERE id = ?1",
            &[&id],
            "forget",
            id,
        )?;
        if n == 0 {
            return Ok(None);
        }
        Ok(existing)
    }

    /// Search saved facts.
    ///
    /// Literal text only: facts are few, and an operator searching their own notes
    /// wants the words they typed, not a ranking function. Same bounds and same
    /// read-only window as history search.
    pub fn search_saved_facts(
        &self,
        query: &str,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FactHit>> {
        let options = SearchOptions {
            query,
            limit,
            fts_syntax: false,
        };
        let expression = self.search_expression(&options)?;
        let limit = options.limit.clamp(1, MAX_SEARCH_LIMIT);
        // `None` means the global bucket only. A search that silently returned another
        // project's facts would leak one project's notes into another's context, which
        // is the opposite of what a scope is for.
        let scope_filter = match scope {
            Some(_) => "AND (saved_facts.scope = '' OR saved_facts.scope = ?6)".to_owned(),
            None => "AND saved_facts.scope = ''".to_owned(),
        };
        let _guard = QueryOnly::new(&self.conn)?;
        let _budget = SearchBudget::install(&self.conn)?;

        let sql = format!(
            "SELECT saved_facts.id, saved_facts.text, saved_facts.scope,
                    saved_facts.created_at_ms, saved_facts.updated_at_ms,
                    snippet(saved_facts_fts, -1, ?2, ?3, ' …', ?4)
             FROM saved_facts_fts
             JOIN saved_facts ON saved_facts.rowid = saved_facts_fts.rowid
             WHERE saved_facts_fts MATCH ?1 {scope_filter}
             ORDER BY saved_facts_fts.rowid DESC
             LIMIT ?5"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(StoreError::sqlite("prepare fact search"))?;
        let mut bindings: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
            Box::new(expression),
            Box::new(MARKER_OPEN.to_owned()),
            Box::new(MARKER_CLOSE.to_owned()),
            Box::new(SNIPPET_TOKENS as i64),
            Box::new(limit as i64),
        ];
        if let Some(s) = scope {
            bindings.push(Box::new(valid_fact_scope(s)?));
        }
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bindings.iter()), |row| {
                Ok(FactHit {
                    fact: SavedFact {
                        id: row.get(0)?,
                        text: row.get(1)?,
                        scope: row.get(2)?,
                        created_at_ms: row.get(3)?,
                        updated_at_ms: row.get(4)?,
                    },
                    snippet: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                })
            })
            .map_err(|e| classify_sqlite_error("fact search", e))?;
        let mut hits = collect(rows, "read fact search row")?;
        hits.truncate(limit);
        Ok(hits)
    }

    // ── preferences ──────────────────────────────────────────────────────

    /// Store one supported preference, in the value's canonical spelling.
    ///
    /// The key has to be one [`PreferenceKey`] knows and the value one it accepts:
    /// a row nothing reads is the thing this table exists to avoid, and the schema's
    /// `CHECK` on `key` says the same thing at a lower layer.
    pub fn set_preference(&self, key: &str, value: &str) -> Result<Preference> {
        let parsed = self.supported_key(key)?;
        parsed
            .validate(value)
            .map_err(|reason| StoreError::Memory {
                reason: format!("preference {}: {reason}", parsed.as_str()),
            })?;
        let value = parsed.canonical_value(value);
        let now = now_ms();
        self.mutate_with_audit(
            "INSERT INTO preferences (key, value, set_at_ms) VALUES (?1,?2,?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                                             set_at_ms = excluded.set_at_ms",
            &[&parsed.as_str(), &value, &now],
            "preference_set",
            parsed.as_str(),
        )?;
        Ok(Preference {
            key: parsed.as_str().to_owned(),
            value,
            set_at_ms: now,
        })
    }

    /// The key and value currently stored for one supported preference.
    pub fn preference(&self, key: &str) -> Result<Option<Preference>> {
        let parsed = self.supported_key(key)?;
        self.conn
            .query_row(
                "SELECT key, value, set_at_ms FROM preferences WHERE key = ?1",
                params![parsed.as_str()],
                |row| {
                    Ok(Preference {
                        key: row.get(0)?,
                        value: row.get(1)?,
                        set_at_ms: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::sqlite("read preference"))
    }

    /// Every stored preference, in supported-key order rather than insertion
    /// order, so `jean preferences list` reads the same tomorrow.
    pub fn list_preferences(&self) -> Result<Vec<Preference>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value, set_at_ms FROM preferences")
            .map_err(StoreError::sqlite("prepare preferences"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Preference {
                    key: row.get(0)?,
                    value: row.get(1)?,
                    set_at_ms: row.get(2)?,
                })
            })
            .map_err(StoreError::sqlite("query preferences"))?;
        let mut out = collect(rows, "read preference")?;
        out.sort_by_key(|p| {
            jean_protocol::preferences::SUPPORTED_KEYS
                .iter()
                .position(|name| *name == p.key)
                .unwrap_or(usize::MAX)
        });
        Ok(out)
    }

    /// Drop one preference so the configuration file's value stands again.
    pub fn clear_preference(&self, key: &str) -> Result<bool> {
        let parsed = self.supported_key(key)?;
        let n = self.mutate_with_audit(
            "DELETE FROM preferences WHERE key = ?1",
            &[&parsed.as_str()],
            "preference_clear",
            parsed.as_str(),
        )?;
        Ok(n > 0)
    }

    /// `(key, value)` rows, for the configuration overlay in
    /// `jean_protocol::preferences::overlay`.
    pub fn preference_rows(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .list_preferences()?
            .into_iter()
            .map(|p| (p.key, p.value))
            .collect())
    }

    // ── shared plumbing ──────────────────────────────────────────────────

    /// Run one memory mutation and its metadata-only audit row in one transaction.
    ///
    /// A crash between the change and the record would leave a database that says a
    /// fact was forgotten and no record of who asked, so the two commit together or
    /// not at all. The audit row carries the action and the id, never the content.
    fn mutate_with_audit(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::types::ToSql],
        action: &str,
        record_id: &str,
    ) -> Result<usize> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(StoreError::sqlite("begin memory change"))?;
        let changed = tx
            .execute(sql, params)
            .map_err(StoreError::sqlite("memory change"))?;
        if changed > 0 {
            tx.execute(
                "INSERT INTO memory_changes (action, record_id, provenance, at_ms)
                 VALUES (?1, ?2, 'operator_command', ?3)",
                rusqlite::params![action, record_id, now_ms()],
            )
            .map_err(StoreError::sqlite("record memory change"))?;
        }
        tx.commit()
            .map_err(StoreError::sqlite("commit memory change"))?;
        Ok(changed)
    }

    fn supported_key(&self, key: &str) -> std::result::Result<PreferenceKey, StoreError> {
        PreferenceKey::parse(key).ok_or_else(|| StoreError::Memory {
            reason: format!(
                "{key:?} is not a preference this build reads; supported keys are {}",
                jean_protocol::preferences::SUPPORTED_KEYS.join(", ")
            ),
        })
    }

    fn require_fact(&self, id: &str) -> Result<SavedFact> {
        self.saved_fact(id)?.ok_or_else(|| StoreError::Memory {
            reason: format!(
                "the fact was written but cannot be read back as {id}; the database and this \
                 build disagree about what it just stored"
            ),
        })
    }

    fn fact_with_norm_except(
        &self,
        scope: &str,
        norm: &str,
        except_id: &str,
    ) -> Result<Option<SavedFact>> {
        self.conn
            .query_row(
                "SELECT id, text, scope, created_at_ms, updated_at_ms FROM saved_facts
                 WHERE scope = ?1 AND norm = ?2 AND id != ?3",
                params![scope, norm, except_id],
                row_to_fact,
            )
            .optional()
            .map_err(StoreError::sqlite("read duplicate fact"))
    }

    fn task_hits(&self, expression: &str, limit: usize) -> Result<Vec<HistoryHit>> {
        // `tasks_fts` is in the `FROM`, not buried in a subquery, because
        // `snippet()` is a function *of the FTS table* and SQLite cannot see a
        // table that only appears inside one. The rowid ordering is what bounds the
        // returned set, and for `tasks` a rowid is creation order — so the newest
        // matches are the ones that come back, and the join supplies each row's live
        // metadata rather than a copy of it.
        const SQL: &str = "SELECT tasks.id, tasks.created_at_ms, tasks.state, tasks.outcome,
                                  snippet(tasks_fts, 0, ?2, ?3, ' …', ?4),
                                  snippet(tasks_fts, 1, ?2, ?3, ' …', ?4)
                           FROM tasks_fts
                           JOIN tasks ON tasks.rowid = tasks_fts.rowid
                           WHERE tasks_fts MATCH ?1
                           ORDER BY tasks_fts.rowid DESC
                           LIMIT ?5";
        let mut stmt = self
            .conn
            .prepare(SQL)
            .map_err(StoreError::sqlite("prepare task search"))?;
        let rows = stmt
            .query_map(
                params![
                    expression,
                    MARKER_OPEN,
                    MARKER_CLOSE,
                    SNIPPET_TOKENS as i64,
                    limit as i64
                ],
                |row| {
                    Ok(HistoryHit {
                        source: HistorySource::Task,
                        task_id: row.get(0)?,
                        at_ms: row.get(1)?,
                        state: row.get(2)?,
                        outcome: row.get(3)?,
                        event_kind: None,
                        event_seq: None,
                        snippet: choose_snippet(
                            row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                            row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                        ),
                    })
                },
            )
            .map_err(|e| classify_sqlite_error("task search", e))?;
        collect(rows, "read task search row")
    }

    fn event_hits(&self, expression: &str, limit: usize) -> Result<Vec<HistoryHit>> {
        // `LEFT JOIN tasks`: events outlive a task row by design, and a hit on
        // orphaned history is reported with `state: None` rather than dropped —
        // silently losing results is the failure that makes an operator stop
        // trusting the search. `events.seq` is the rowid, so `rowid DESC` here is
        // the same "newest matches first" bound [`Self::task_hits`] gets from
        // insertion order.
        const SQL: &str = "SELECT events.task_id, events.at_ms, tasks.state, tasks.outcome,
                                  events.kind, events.seq,
                                  snippet(events_fts, 0, ?2, ?3, ' …', ?4)
                           FROM events_fts
                           JOIN events ON events.seq = events_fts.rowid
                           LEFT JOIN tasks ON tasks.id = events.task_id
                           WHERE events_fts MATCH ?1
                           ORDER BY events_fts.rowid DESC
                           LIMIT ?5";
        let mut stmt = self
            .conn
            .prepare(SQL)
            .map_err(StoreError::sqlite("prepare event search"))?;
        let rows = stmt
            .query_map(
                params![
                    expression,
                    MARKER_OPEN,
                    MARKER_CLOSE,
                    SNIPPET_TOKENS as i64,
                    limit as i64
                ],
                |row| {
                    Ok(HistoryHit {
                        source: HistorySource::Event,
                        task_id: row.get(0)?,
                        at_ms: row.get(1)?,
                        state: row.get(2)?,
                        outcome: row.get(3)?,
                        event_kind: Some(row.get(4)?),
                        event_seq: Some(row.get(5)?),
                        snippet: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    })
                },
            )
            .map_err(|e| classify_sqlite_error("event search", e))?;
        collect(rows, "read event search row")
    }

    /// Turn what the operator typed into the string bound to `MATCH`.
    fn search_expression(
        &self,
        options: &SearchOptions<'_>,
    ) -> std::result::Result<String, StoreError> {
        let query = options.query.trim();
        if query.is_empty() {
            return Err(StoreError::BadQuery {
                reason: "the search query is empty; give at least one word, as in \
                         `jean history search report`"
                    .to_owned(),
            });
        }
        let chars = query.chars().count();
        if chars > MAX_QUERY_CHARS {
            return Err(StoreError::BadQuery {
                reason: format!(
                    "the query is {chars} characters, over the {MAX_QUERY_CHARS}-character limit; \
                     search for a shorter phrase"
                ),
            });
        }
        if query.chars().any(|c| c.is_control()) {
            return Err(StoreError::BadQuery {
                reason: "the query contains a control character; paste plain text rather than a \
                        terminal escape sequence"
                    .to_owned(),
            });
        }

        if options.fts_syntax {
            // Explicitly opted in, so the words are handed to FTS5 as written.
            return Ok(query.to_owned());
        }
        // Literal mode: one quoted phrase. FTS5's string syntax takes `""` for an
        // embedded quote, and nothing inside a quoted string is read as an operator —
        // which is what stops a search for `NEAR` from becoming a request to rank by
        // proximity.
        Ok(format!("\"{}\"", query.replace('"', "\"\"")))
    }
}

/// A wall-clock ceiling on one search, enforced by interrupting the query.
///
/// `LIMIT` bounds what comes back, not what is examined: a broad term can match
/// most of a large history before the limit is reached. A watchdog thread holds
/// SQLite's interrupt handle and fires it if the search outlives the budget, so
/// the answer is an error rather than an unbounded wait — and never a fallback
/// scan. The thread is joined on drop, so a search that finishes in time cannot
/// leave a watchdog armed over the next statement.
struct SearchBudget {
    done: std::sync::mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// The most wall time one search may take before it is interrupted.
const MAX_SEARCH_MS: u64 = 2_000;

impl SearchBudget {
    fn install(conn: &Connection) -> Result<Self> {
        let interrupt = conn.get_interrupt_handle();
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::Builder::new()
            .name("jean-search-budget".to_owned())
            .spawn(move || {
                if wait
                    .recv_timeout(std::time::Duration::from_millis(MAX_SEARCH_MS))
                    .is_err()
                {
                    interrupt.interrupt();
                }
            })
            .map_err(|e| StoreError::Memory {
                reason: format!("could not start the search watchdog: {e}"),
            })?;
        Ok(Self {
            done,
            worker: Some(worker),
        })
    }
}

impl Drop for SearchBudget {
    fn drop(&mut self) {
        let _ = self.done.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// `PRAGMA query_only` for the lifetime of a search.
///
/// Restored on `Drop` rather than on the success path, because the point is that a
/// query that errors halfway cannot leave the connection read-only and make every
/// later write in this process fail for a reason nobody can see.
struct QueryOnly<'a> {
    conn: &'a Connection,
}

impl<'a> QueryOnly<'a> {
    fn new(conn: &'a Connection) -> Result<Self> {
        conn.pragma_update(None, "query_only", "ON")
            .map_err(StoreError::sqlite("set query_only"))?;
        Ok(Self { conn })
    }
}

impl Drop for QueryOnly<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.conn.pragma_update(None, "query_only", "OFF") {
            // Loud, because a connection left read-only would misreport every later
            // write in this process as a bug in the code that made them.
            tracing::error!(error = %e, "could not restore query_only; writes on this connection may fail");
        }
    }
}

/// Prefer the snippet that actually contains a marked match.
///
/// `tasks_fts` has two indexed columns, and `snippet()` returns plain text for a
/// column that did not match. Printing the goal when the match was in the outcome
/// detail would send an operator looking in the wrong place.
fn choose_snippet(goal: String, outcome: String) -> String {
    if outcome.contains(MARKER_OPEN) {
        outcome
    } else {
        goal
    }
}

fn row_to_fact(row: &rusqlite::Row<'_>) -> rusqlite::Result<SavedFact> {
    Ok(SavedFact {
        id: row.get(0)?,
        text: row.get(1)?,
        scope: row.get(2)?,
        created_at_ms: row.get(3)?,
        updated_at_ms: row.get(4)?,
    })
}

fn collect<T>(
    rows: impl Iterator<Item = std::result::Result<T, rusqlite::Error>>,
    context: &'static str,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        // Row-iteration errors go through the same classifier: FTS5 can raise its
        // syntax error on the first step rather than when the query starts.
        out.push(row.map_err(|e| classify_sqlite_error(context, e))?);
    }
    Ok(out)
}

/// Sort an FTS5 syntax failure from an ordinary SQLite one.
///
/// FTS5 reports a bad expression when it **starts** the query, which in rusqlite
/// can surface either from `query_map` or from pulling the first row — so both
/// paths go through here. Without it a malformed query looks exactly like a search
/// that found nothing, the most confusing possible answer, because "no results"
/// teaches an operator to change the words rather than the flag.
fn classify_sqlite_error(context: &'static str, error: rusqlite::Error) -> StoreError {
    let message = error.to_string();
    if message.contains("fts5: syntax error") {
        StoreError::BadQuery {
            reason: format!(
                "the query is not valid FTS5 syntax ({message}); search without \
                 `--fts-syntax` to look for the words themselves"
            ),
        }
    } else {
        StoreError::sqlite(context)(error)
    }
}

/// Text bound to a fact, checked the same way the schema checks it.
/// The shipped pattern floor, applied at the writer so a caller that skipped
/// redaction still cannot store a credential. Patterns recognise shapes: a secret
/// in an unknown shape passes, which is why this is a floor and not a guarantee.
fn credential_floor() -> jean_protocol::Redactor {
    jean_protocol::Redactor::new(&jean_protocol::config::default_redact_patterns())
}

fn valid_fact_text(text: &str) -> std::result::Result<String, StoreError> {
    let trimmed = text.trim();
    if credential_floor().redact(trimmed) != trimmed {
        return Err(StoreError::Memory {
            reason: "that sentence contains credential-shaped text; nothing was saved.                      Credentials are not memory — save the fact without the value."
                .to_owned(),
        });
    }
    if trimmed.is_empty() {
        return Err(StoreError::Memory {
            reason: "a saved fact needs some words; `jean memory add` with an empty sentence \
                     would save nothing to read back"
                .to_owned(),
        });
    }
    let chars = trimmed.chars().count();
    if chars > MAX_FACT_CHARS {
        return Err(StoreError::Memory {
            reason: format!(
                "the fact is {chars} characters, over the {MAX_FACT_CHARS}-character limit; save \
                 a shorter sentence, or put the long version in a file and save the path"
            ),
        });
    }
    if trimmed.chars().any(|c| c.is_control() && c != '\t') {
        return Err(StoreError::Memory {
            reason: "a saved fact may not contain control characters; save plain text".to_owned(),
        });
    }
    Ok(trimmed.to_owned())
}

fn valid_fact_scope(scope: &str) -> std::result::Result<String, StoreError> {
    let trimmed = scope.trim();
    if credential_floor().redact(trimmed) != trimmed {
        return Err(StoreError::Memory {
            reason: "the scope contains credential-shaped text; nothing was saved".to_owned(),
        });
    }
    let chars = trimmed.chars().count();
    if chars > MAX_SCOPE_CHARS {
        return Err(StoreError::Memory {
            reason: format!(
                "the scope is {chars} characters, over the {MAX_SCOPE_CHARS}-character limit"
            ),
        });
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(StoreError::Memory {
            reason: "a scope may not contain control characters".to_owned(),
        });
    }
    Ok(trimmed.to_owned())
}

/// A scope argument as SQL wants it: anything non-empty, or `''` for global.
fn valid_scope_or_global(scope: &str) -> std::result::Result<String, StoreError> {
    valid_fact_scope(scope)
}

/// The comparison form of a fact's text.
///
/// Trimmed, whitespace runs collapsed to one space, and case-folded. The questions
/// this answers are "did I already save this?" and "is this the same fact?", and
/// nobody means those to turn on how they happened to type the spacing. The stored
/// display text is untouched.
///
/// Not a normalisation of *meaning*: `prefer dark` and `prefers dark` are two
/// records, because deciding otherwise would be the storage layer inventing
/// semantics nobody specified.
pub fn normalize_for_comparison(text: &str) -> String {
    text.split_whitespace()
        .map(|word| word.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation_ignores_spacing_and_case_but_not_words() {
        assert_eq!(
            normalize_for_comparison("  Prefers   the DARK   theme \n"),
            "prefers the dark theme"
        );
        assert_eq!(
            normalize_for_comparison("prefers the dark theme"),
            normalize_for_comparison("PREFERS   The Dark Theme")
        );
        assert_ne!(
            normalize_for_comparison("prefers dark"),
            normalize_for_comparison("prefer dark"),
            "the storage layer does not decide that two different sentences mean one thing"
        );
    }

    #[test]
    fn a_fact_bound_is_checked_before_it_reaches_sql() {
        assert!(valid_fact_text("   ").is_err());
        assert!(valid_fact_text(&"x".repeat(MAX_FACT_CHARS + 1)).is_err());
        assert!(valid_fact_text("has a\0nul").is_err());
        assert!(valid_fact_scope(&"s".repeat(MAX_SCOPE_CHARS + 1)).is_err());
        assert_eq!(valid_fact_text(" keep me  ").unwrap(), "keep me");
    }

    #[test]
    fn a_literal_query_becomes_one_quoted_phrase() {
        let store = Store::open_in_memory().unwrap();
        let options = SearchOptions {
            query: "NEAR report",
            ..Default::default()
        };
        assert_eq!(
            store.search_expression(&options).unwrap(),
            "\"NEAR report\"",
            "the words the operator typed, as a phrase"
        );

        let escaped = SearchOptions {
            query: "say \"hello\" now",
            ..Default::default()
        };
        assert_eq!(
            store.search_expression(&escaped).unwrap(),
            "\"say \"\"hello\"\" now\"",
            "an embedded quote is doubled, which is FTS5's string escape"
        );

        let explicit = SearchOptions {
            query: "report NEAR invoice",
            limit: 5,
            fts_syntax: true,
        };
        assert_eq!(
            store.search_expression(&explicit).unwrap(),
            "report NEAR invoice"
        );
    }

    #[test]
    fn an_empty_or_overlong_query_is_refused_before_the_index_is_walked() {
        let store = Store::open_in_memory().unwrap();
        let reason = store
            .search_history(&SearchOptions {
                query: "   ",
                ..Default::default()
            })
            .expect_err("no query is not a search");
        assert!(reason.to_string().contains("empty"), "{reason}");

        let long = "w ".repeat(MAX_QUERY_CHARS + 1);
        let reason = store
            .search_history(&SearchOptions {
                query: &long,
                ..Default::default()
            })
            .expect_err("an unbounded query is unbounded work");
        let text = reason.to_string();
        assert!(text.contains(&MAX_QUERY_CHARS.to_string()), "{text}");
        assert!(text.contains("shorter"), "{text}");
    }

    #[test]
    fn the_read_only_window_closes_whatever_the_search_did() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_task(&crate::TaskRecord::new(
                "t-probe".into(),
                "probe goal".into(),
                Some("probe".into()),
                "{}".into(),
            ))
            .unwrap();
        let hits = store
            .search_history(&SearchOptions {
                query: "probe",
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1, "the trigger indexed the row it inserted");
        // Writing afterwards proves `query_only` came back off.
        store
            .add_saved_fact("f-after", "writes still work", None)
            .expect("a search must leave the connection writable");
    }
}
