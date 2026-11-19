//! Episodic memory, self-updating consolidated memory notes, dreaming cycle logs,
//! and procedural playbooks for known/learned tasks.
//!
//! Every writer enforces the credential floor (`credential_floor()`) and text bounds
//! before writing to SQLite, and every search runs inside `QueryOnly` + `SearchBudget`.
//! Retrieved rows are data — never authority.

use rusqlite::{params, OptionalExtension};

use crate::memory::{
    classify_sqlite_error, collect, credential_floor, normalize_for_comparison, valid_fact_scope,
    valid_fact_text, QueryOnly, SearchBudget, MARKER_CLOSE, MARKER_OPEN,
};
use crate::{now_ms, Result, SearchOptions, Store, StoreError, MAX_SEARCH_LIMIT, SNIPPET_TOKENS};

pub const MAX_TOPIC_KEY_CHARS: usize = 160;
pub const MAX_TAGS_CHARS: usize = 260;
pub const MAX_PLAYBOOK_NAME_CHARS: usize = 80;
pub const MAX_PLAYBOOK_DISPLAY_CHARS: usize = 120;
pub const MAX_PLAYBOOK_DESC_CHARS: usize = 500;
pub const MAX_PLAYBOOK_JSON_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEpisode {
    pub id: String,
    pub kind: String,
    pub scope: String,
    pub summary: String,
    pub tags: String,
    pub salience: i64,
    pub task_id: Option<String>,
    pub consolidated: bool,
    pub created_at_ms: i64,
    pub last_recalled_ms: Option<i64>,
    pub recall_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMemoryEpisode {
    pub id: String,
    pub kind: String,
    pub scope: String,
    pub summary: String,
    pub tags: String,
    pub salience: i64,
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryNote {
    pub id: String,
    pub category: String,
    pub scope: String,
    pub topic_key: String,
    pub text: String,
    pub confidence: i64,
    pub salience: i64,
    pub origin: String,
    pub status: String,
    pub superseded_by: Option<String>,
    pub evidence_count: i64,
    pub recall_count: i64,
    pub last_recalled_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMemoryNote {
    pub id: String,
    pub category: String,
    pub scope: String,
    pub topic_key: String,
    pub text: String,
    pub confidence: i64,
    pub salience: i64,
    pub origin: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryNoteUpsert {
    Created(MemoryNote),
    Reinforced(MemoryNote),
    Superseded {
        previous_id: String,
        current: MemoryNote,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryNoteHit {
    pub note: MemoryNote,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamCycleRecord {
    pub id: String,
    pub mode: String,
    pub scope: String,
    pub episodes_scanned: i64,
    pub tasks_scanned: i64,
    pub notes_created: i64,
    pub notes_updated: i64,
    pub notes_superseded: i64,
    pub notes_archived: i64,
    pub playbooks_promoted: i64,
    pub summary: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybookRecord {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub triggers_json: String,
    pub scope: String,
    pub parameters_json: String,
    pub steps_json: String,
    pub version: i64,
    pub previous_steps_json: Option<String>,
    pub source: String,
    pub verified_successes: i64,
    pub failures: i64,
    pub last_used_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlaybook {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub triggers_json: String,
    pub scope: String,
    pub parameters_json: String,
    pub steps_json: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybookRunRecord {
    pub id: i64,
    pub playbook_id: String,
    pub task_id: Option<String>,
    pub outcome: String,
    pub duration_ms: i64,
    pub notes: String,
    pub at_ms: i64,
}

impl Store {
    // ── episodic memory ──────────────────────────────────────────────────

    pub fn add_episode(&self, episode: &NewMemoryEpisode) -> Result<MemoryEpisode> {
        let summary = valid_fact_text(&episode.summary)?;
        let scope = valid_fact_scope(&episode.scope)?;
        let tags = valid_bounded_plain(&episode.tags, MAX_TAGS_CHARS, "episode tags")?;
        let kind = valid_enum(
            &episode.kind,
            &[
                "conversation",
                "task_success",
                "task_failure",
                "correction",
                "preference_signal",
                "observation",
            ],
            "episode kind",
        )?;
        let salience = episode.salience.clamp(0, 100);
        let now = now_ms();

        self.conn
            .execute(
                "INSERT INTO memory_episodes
                 (id, kind, scope, summary, tags, salience, task_id, consolidated, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)",
                params![
                    episode.id,
                    kind,
                    scope,
                    summary,
                    tags,
                    salience,
                    episode.task_id,
                    now
                ],
            )
            .map_err(StoreError::sqlite("insert memory episode"))?;

        Ok(MemoryEpisode {
            id: episode.id.clone(),
            kind: kind.to_owned(),
            scope,
            summary,
            tags,
            salience,
            task_id: episode.task_id.clone(),
            consolidated: false,
            created_at_ms: now,
            last_recalled_ms: None,
            recall_count: 0,
        })
    }

    pub fn unconsolidated_episodes(
        &self,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEpisode>> {
        let limit = limit.clamp(1, 200) as i64;
        match scope.map(valid_fact_scope).transpose()? {
            Some(scope) => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, kind, scope, summary, tags, salience, task_id,
                                consolidated, created_at_ms, last_recalled_ms, recall_count
                         FROM memory_episodes
                         WHERE consolidated = 0 AND (scope = '' OR scope = ?1)
                         ORDER BY created_at_ms ASC LIMIT ?2",
                    )
                    .map_err(StoreError::sqlite("prepare unconsolidated episodes"))?;
                let rows = stmt
                    .query_map(params![scope, limit], row_to_episode)
                    .map_err(StoreError::sqlite("query unconsolidated episodes"))?;
                collect(rows, "read unconsolidated episode")
            }
            None => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, kind, scope, summary, tags, salience, task_id,
                                consolidated, created_at_ms, last_recalled_ms, recall_count
                         FROM memory_episodes
                         WHERE consolidated = 0
                         ORDER BY created_at_ms ASC LIMIT ?1",
                    )
                    .map_err(StoreError::sqlite("prepare unconsolidated episodes"))?;
                let rows = stmt
                    .query_map(params![limit], row_to_episode)
                    .map_err(StoreError::sqlite("query unconsolidated episodes"))?;
                collect(rows, "read unconsolidated episode")
            }
        }
    }

    pub fn recent_episodes(
        &self,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEpisode>> {
        let limit = limit.clamp(1, 200) as i64;
        match scope.map(valid_fact_scope).transpose()? {
            Some(scope) => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, kind, scope, summary, tags, salience, task_id,
                                consolidated, created_at_ms, last_recalled_ms, recall_count
                         FROM memory_episodes
                         WHERE scope = '' OR scope = ?1
                         ORDER BY created_at_ms DESC LIMIT ?2",
                    )
                    .map_err(StoreError::sqlite("prepare recent episodes"))?;
                let rows = stmt
                    .query_map(params![scope, limit], row_to_episode)
                    .map_err(StoreError::sqlite("query recent episodes"))?;
                collect(rows, "read recent episode")
            }
            None => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, kind, scope, summary, tags, salience, task_id,
                                consolidated, created_at_ms, last_recalled_ms, recall_count
                         FROM memory_episodes
                         WHERE scope = ''
                         ORDER BY created_at_ms DESC LIMIT ?1",
                    )
                    .map_err(StoreError::sqlite("prepare recent episodes"))?;
                let rows = stmt
                    .query_map(params![limit], row_to_episode)
                    .map_err(StoreError::sqlite("query recent episodes"))?;
                collect(rows, "read recent episode")
            }
        }
    }

    pub fn search_episodes(
        &self,
        query: &str,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEpisode>> {
        let expression = self.search_expression(&SearchOptions {
            query,
            limit,
            fts_syntax: false,
        })?;
        let limit = limit.clamp(1, MAX_SEARCH_LIMIT) as i64;
        let scope_filter = match scope {
            Some(_) => "AND (memory_episodes.scope = '' OR memory_episodes.scope = ?3)",
            None => "AND memory_episodes.scope = ''",
        };
        let _guard = QueryOnly::new(&self.conn)?;
        let _budget = SearchBudget::install(&self.conn)?;

        let sql = format!(
            "SELECT memory_episodes.id, memory_episodes.kind, memory_episodes.scope,
                    memory_episodes.summary, memory_episodes.tags, memory_episodes.salience,
                    memory_episodes.task_id, memory_episodes.consolidated,
                    memory_episodes.created_at_ms, memory_episodes.last_recalled_ms,
                    memory_episodes.recall_count
             FROM memory_episodes_fts
             JOIN memory_episodes ON memory_episodes.rowid = memory_episodes_fts.rowid
             WHERE memory_episodes_fts MATCH ?1 {scope_filter}
             ORDER BY memory_episodes.salience DESC, memory_episodes.created_at_ms DESC
             LIMIT ?2"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(StoreError::sqlite("prepare episode search"))?;
        let mut bindings: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(expression), Box::new(limit)];
        if let Some(s) = scope {
            bindings.push(Box::new(valid_fact_scope(s)?));
        }
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(bindings.iter()),
                row_to_episode,
            )
            .map_err(|e| classify_sqlite_error("episode search", e))?;
        collect(rows, "read episode search row")
    }

    pub fn mark_episodes_consolidated(&self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(StoreError::sqlite("begin mark consolidated"))?;
        let mut updated = 0usize;
        for id in ids {
            updated += tx
                .execute(
                    "UPDATE memory_episodes SET consolidated = 1 WHERE id = ?1 AND consolidated = 0",
                    params![id],
                )
                .map_err(StoreError::sqlite("mark episode consolidated"))?;
        }
        tx.commit()
            .map_err(StoreError::sqlite("commit mark consolidated"))?;
        Ok(updated)
    }

    // ── self-updating semantic memory notes ──────────────────────────────

    /// Create, reinforce, or supersede a semantic memory note.
    ///
    /// * If an active note with the same `(scope, norm)` exists, reinforces its
    ///   `evidence_count`, `confidence`, and `salience`.
    /// * If `topic_key` is non-empty and an active note with the same `(scope, topic_key)`
    ///   holds different text, marks that older note `status = 'superseded'` pointing at
    ///   the new note's id and inserts the new active note — preserving audit lineage
    ///   without leaving conflicting active facts.
    pub fn upsert_memory_note(&self, note: &NewMemoryNote) -> Result<MemoryNoteUpsert> {
        let text = valid_fact_text(&note.text)?;
        let scope = valid_fact_scope(&note.scope)?;
        let topic_key = valid_bounded_plain(&note.topic_key, MAX_TOPIC_KEY_CHARS, "topic key")?
            .to_ascii_lowercase();
        let category = valid_enum(
            &note.category,
            &[
                "fact",
                "preference",
                "project",
                "person",
                "lesson",
                "procedure",
                "environment",
                "summary",
            ],
            "memory note category",
        )?;
        let origin = valid_enum(
            &note.origin,
            &[
                "operator",
                "self_update",
                "dream_consolidation",
                "task_reflection",
            ],
            "memory note origin",
        )?;
        let norm = normalize_for_comparison(&text);
        let confidence = note.confidence.clamp(0, 100);
        let salience = note.salience.clamp(0, 100);
        let now = now_ms();

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(StoreError::sqlite("begin upsert memory note"))?;

        // 1) Exact match in active notes for this scope → reinforce instead of duplicating.
        let exact = tx
            .query_row(
                "SELECT id, category, scope, topic_key, text, confidence, salience, origin,
                        status, superseded_by, evidence_count, recall_count, last_recalled_ms,
                        created_at_ms, updated_at_ms
                 FROM memory_notes
                 WHERE scope = ?1 AND norm = ?2 AND status = 'active'
                 LIMIT 1",
                params![scope, norm],
                row_to_note,
            )
            .optional()
            .map_err(StoreError::sqlite("lookup active memory note"))?;

        if let Some(existing) = exact {
            let new_conf = existing.confidence.max(confidence).min(100);
            let new_salience = (existing.salience.max(salience) + 5).min(100);
            let new_evidence = existing.evidence_count.saturating_add(1);
            let effective_topic = if existing.topic_key.is_empty() && !topic_key.is_empty() {
                topic_key
            } else {
                existing.topic_key.clone()
            };
            tx.execute(
                "UPDATE memory_notes
                 SET confidence = ?2, salience = ?3, evidence_count = ?4,
                     topic_key = ?5, updated_at_ms = ?6
                 WHERE id = ?1",
                params![
                    existing.id,
                    new_conf,
                    new_salience,
                    new_evidence,
                    effective_topic,
                    now
                ],
            )
            .map_err(StoreError::sqlite("reinforce memory note"))?;
            tx.commit()
                .map_err(StoreError::sqlite("commit reinforce memory note"))?;
            return Ok(MemoryNoteUpsert::Reinforced(MemoryNote {
                confidence: new_conf,
                salience: new_salience,
                evidence_count: new_evidence,
                topic_key: effective_topic,
                updated_at_ms: now,
                ..existing
            }));
        }

        // 2) Same non-empty topic_key with different text → supersede prior active note(s).
        let previous = if !topic_key.is_empty() {
            tx.query_row(
                "SELECT id FROM memory_notes
                 WHERE scope = ?1 AND topic_key = ?2 AND status = 'active'
                 ORDER BY updated_at_ms DESC LIMIT 1",
                params![scope, topic_key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StoreError::sqlite("lookup topic memory note"))?
        } else {
            None
        };

        tx.execute(
            "INSERT INTO memory_notes
             (id, category, scope, topic_key, text, norm, confidence, salience, origin,
              status, superseded_by, evidence_count, recall_count, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active', NULL, 1, 0, ?10, ?10)",
            params![
                note.id, category, scope, topic_key, text, norm, confidence, salience, origin, now
            ],
        )
        .map_err(StoreError::sqlite("insert memory note"))?;

        if let Some(ref prev_id) = previous {
            tx.execute(
                "UPDATE memory_notes
                 SET status = 'superseded', superseded_by = ?2, updated_at_ms = ?3
                 WHERE scope = ?1 AND topic_key = ?4 AND status = 'active' AND id != ?2",
                params![scope, note.id, now, topic_key],
            )
            .map_err(StoreError::sqlite("supersede memory note"))?;
            let _ = prev_id;
        }

        tx.commit()
            .map_err(StoreError::sqlite("commit upsert memory note"))?;

        let created = MemoryNote {
            id: note.id.clone(),
            category: category.to_owned(),
            scope,
            topic_key,
            text,
            confidence,
            salience,
            origin: origin.to_owned(),
            status: "active".to_owned(),
            superseded_by: None,
            evidence_count: 1,
            recall_count: 0,
            last_recalled_ms: None,
            created_at_ms: now,
            updated_at_ms: now,
        };

        if let Some(previous_id) = previous {
            Ok(MemoryNoteUpsert::Superseded {
                previous_id,
                current: created,
            })
        } else {
            Ok(MemoryNoteUpsert::Created(created))
        }
    }

    pub fn active_memory_notes(
        &self,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryNote>> {
        self.list_memory_notes_filtered(scope, false, limit)
    }

    pub fn list_memory_notes(
        &self,
        scope: Option<&str>,
        include_inactive: bool,
        limit: usize,
    ) -> Result<Vec<MemoryNote>> {
        self.list_memory_notes_filtered(scope, include_inactive, limit)
    }

    fn list_memory_notes_filtered(
        &self,
        scope: Option<&str>,
        include_inactive: bool,
        limit: usize,
    ) -> Result<Vec<MemoryNote>> {
        let limit = limit.clamp(1, 200) as i64;
        let status_clause = if include_inactive {
            "1 = 1"
        } else {
            "status = 'active'"
        };
        match scope.map(valid_fact_scope).transpose()? {
            Some(scope) => {
                let sql = format!(
                    "SELECT id, category, scope, topic_key, text, confidence, salience, origin,
                            status, superseded_by, evidence_count, recall_count, last_recalled_ms,
                            created_at_ms, updated_at_ms
                     FROM memory_notes
                     WHERE {status_clause} AND (scope = '' OR scope = ?1)
                     ORDER BY salience DESC, updated_at_ms DESC LIMIT ?2"
                );
                let mut stmt = self
                    .conn
                    .prepare(&sql)
                    .map_err(StoreError::sqlite("prepare memory notes"))?;
                let rows = stmt
                    .query_map(params![scope, limit], row_to_note)
                    .map_err(StoreError::sqlite("query memory notes"))?;
                collect(rows, "read memory note")
            }
            None => {
                let sql = format!(
                    "SELECT id, category, scope, topic_key, text, confidence, salience, origin,
                            status, superseded_by, evidence_count, recall_count, last_recalled_ms,
                            created_at_ms, updated_at_ms
                     FROM memory_notes
                     WHERE {status_clause} AND scope = ''
                     ORDER BY salience DESC, updated_at_ms DESC LIMIT ?1"
                );
                let mut stmt = self
                    .conn
                    .prepare(&sql)
                    .map_err(StoreError::sqlite("prepare memory notes"))?;
                let rows = stmt
                    .query_map(params![limit], row_to_note)
                    .map_err(StoreError::sqlite("query memory notes"))?;
                collect(rows, "read memory note")
            }
        }
    }

    pub fn search_memory_notes(
        &self,
        query: &str,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryNoteHit>> {
        let expression = self.search_expression(&SearchOptions {
            query,
            limit,
            fts_syntax: false,
        })?;
        let limit = limit.clamp(1, MAX_SEARCH_LIMIT) as i64;
        let scope_filter = match scope {
            Some(_) => "AND (memory_notes.scope = '' OR memory_notes.scope = ?6)",
            None => "AND memory_notes.scope = ''",
        };
        let _guard = QueryOnly::new(&self.conn)?;
        let _budget = SearchBudget::install(&self.conn)?;

        let sql = format!(
            "SELECT memory_notes.id, memory_notes.category, memory_notes.scope,
                    memory_notes.topic_key, memory_notes.text, memory_notes.confidence,
                    memory_notes.salience, memory_notes.origin, memory_notes.status,
                    memory_notes.superseded_by, memory_notes.evidence_count,
                    memory_notes.recall_count, memory_notes.last_recalled_ms,
                    memory_notes.created_at_ms, memory_notes.updated_at_ms,
                    snippet(memory_notes_fts, 0, ?2, ?3, ' …', ?4)
             FROM memory_notes_fts
             JOIN memory_notes ON memory_notes.rowid = memory_notes_fts.rowid
             WHERE memory_notes_fts MATCH ?1 AND memory_notes.status = 'active' {scope_filter}
             ORDER BY memory_notes.salience DESC, memory_notes.updated_at_ms DESC
             LIMIT ?5"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(StoreError::sqlite("prepare memory note search"))?;
        let mut bindings: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
            Box::new(expression),
            Box::new(MARKER_OPEN.to_owned()),
            Box::new(MARKER_CLOSE.to_owned()),
            Box::new(SNIPPET_TOKENS as i64),
            Box::new(limit),
        ];
        if let Some(s) = scope {
            bindings.push(Box::new(valid_fact_scope(s)?));
        }
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bindings.iter()), |row| {
                Ok(MemoryNoteHit {
                    note: row_to_note(row)?,
                    snippet: row.get::<_, Option<String>>(15)?.unwrap_or_default(),
                })
            })
            .map_err(|e| classify_sqlite_error("memory note search", e))?;
        collect(rows, "read memory note search row")
    }

    pub fn touch_memory_note_recall(&self, id: &str) -> Result<()> {
        let now = now_ms();
        self.conn
            .execute(
                "UPDATE memory_notes
                 SET recall_count = recall_count + 1,
                     salience = MIN(100, salience + 2),
                     last_recalled_ms = ?2
                 WHERE id = ?1",
                params![id, now],
            )
            .map_err(StoreError::sqlite("touch memory note recall"))?;
        Ok(())
    }

    pub fn supersede_memory_note(&self, old_id: &str, new_id: &str) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "UPDATE memory_notes
                 SET status = 'superseded', superseded_by = ?2, updated_at_ms = ?3
                 WHERE id = ?1 AND status = 'active'",
                params![old_id, new_id, now_ms()],
            )
            .map_err(StoreError::sqlite("supersede memory note"))?;
        Ok(changed > 0)
    }

    /// Gently decay untouched low-salience notes older than `stale_before_ms`,
    /// archiving those whose salience drops below `archive_threshold`.
    pub fn decay_stale_memory_notes(
        &self,
        stale_before_ms: i64,
        decay_points: i64,
        archive_threshold: i64,
    ) -> Result<(usize, usize)> {
        let decay = decay_points.clamp(1, 25);
        let threshold = archive_threshold.clamp(0, 50);
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(StoreError::sqlite("begin decay memory notes"))?;
        let decayed = tx
            .execute(
                "UPDATE memory_notes
                 SET salience = MAX(0, salience - ?2)
                 WHERE status = 'active'
                   AND origin != 'operator'
                   AND updated_at_ms < ?1
                   AND (last_recalled_ms IS NULL OR last_recalled_ms < ?1)
                   AND recall_count = 0",
                params![stale_before_ms, decay],
            )
            .map_err(StoreError::sqlite("decay stale memory notes"))?;
        let archived = tx
            .execute(
                "UPDATE memory_notes
                 SET status = 'archived', updated_at_ms = ?2
                 WHERE status = 'active'
                   AND origin != 'operator'
                   AND salience <= ?1
                   AND recall_count = 0",
                params![threshold, now_ms()],
            )
            .map_err(StoreError::sqlite("archive low-salience memory notes"))?;
        tx.commit()
            .map_err(StoreError::sqlite("commit decay memory notes"))?;
        Ok((decayed, archived))
    }

    pub fn forget_memory_note(&self, id: &str) -> Result<Option<MemoryNote>> {
        let existing = self
            .conn
            .query_row(
                "SELECT id, category, scope, topic_key, text, confidence, salience, origin,
                        status, superseded_by, evidence_count, recall_count, last_recalled_ms,
                        created_at_ms, updated_at_ms
                 FROM memory_notes WHERE id = ?1",
                params![id],
                row_to_note,
            )
            .optional()
            .map_err(StoreError::sqlite("lookup memory note"))?;
        if existing.is_none() {
            return Ok(None);
        }
        self.conn
            .execute("DELETE FROM memory_notes WHERE id = ?1", params![id])
            .map_err(StoreError::sqlite("delete memory note"))?;
        Ok(existing)
    }

    // ── dream cycles ─────────────────────────────────────────────────────

    pub fn record_dream_cycle(&self, record: &DreamCycleRecord) -> Result<()> {
        let mode = valid_enum(
            &record.mode,
            &["deterministic", "model_assisted"],
            "dream cycle mode",
        )?;
        let scope = valid_fact_scope(&record.scope)?;
        let summary = valid_fact_text(&record.summary)?;
        self.conn
            .execute(
                "INSERT INTO dream_cycles
                 (id, mode, scope, episodes_scanned, tasks_scanned, notes_created,
                  notes_updated, notes_superseded, notes_archived, playbooks_promoted,
                  summary, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    record.id,
                    mode,
                    scope,
                    record.episodes_scanned,
                    record.tasks_scanned,
                    record.notes_created,
                    record.notes_updated,
                    record.notes_superseded,
                    record.notes_archived,
                    record.playbooks_promoted,
                    summary,
                    record.created_at_ms,
                ],
            )
            .map_err(StoreError::sqlite("insert dream cycle"))?;
        Ok(())
    }

    pub fn recent_dream_cycles(&self, limit: usize) -> Result<Vec<DreamCycleRecord>> {
        let limit = limit.clamp(1, 50) as i64;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, mode, scope, episodes_scanned, tasks_scanned, notes_created,
                        notes_updated, notes_superseded, notes_archived, playbooks_promoted,
                        summary, created_at_ms
                 FROM dream_cycles
                 ORDER BY created_at_ms DESC LIMIT ?1",
            )
            .map_err(StoreError::sqlite("prepare dream cycles"))?;
        let rows = stmt
            .query_map(params![limit], row_to_dream_cycle)
            .map_err(StoreError::sqlite("query dream cycles"))?;
        collect(rows, "read dream cycle")
    }

    pub fn last_dream_cycle(&self) -> Result<Option<DreamCycleRecord>> {
        Ok(self.recent_dream_cycles(1)?.into_iter().next())
    }

    // ── procedural playbooks (known tasks) ───────────────────────────────

    pub fn save_playbook(&self, pb: &NewPlaybook) -> Result<PlaybookRecord> {
        let name = valid_playbook_name(&pb.name)?;
        let display_name = valid_bounded_plain(
            &pb.display_name,
            MAX_PLAYBOOK_DISPLAY_CHARS,
            "playbook display_name",
        )?;
        let description = valid_bounded_plain(
            &pb.description,
            MAX_PLAYBOOK_DESC_CHARS,
            "playbook description",
        )?;
        let scope = valid_fact_scope(&pb.scope)?;
        let source = valid_enum(
            &pb.source,
            &["taught", "learned_from_task", "dream_promoted", "migrated"],
            "playbook source",
        )?;
        valid_json_payload(&pb.triggers_json, "playbook triggers")?;
        valid_json_payload(&pb.parameters_json, "playbook parameters")?;
        valid_json_payload(&pb.steps_json, "playbook steps")?;

        let now = now_ms();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(StoreError::sqlite("begin save playbook"))?;

        let existing = tx
            .query_row(
                "SELECT id, name, display_name, description, triggers_json, scope,
                        parameters_json, steps_json, version, previous_steps_json, source,
                        verified_successes, failures, last_used_ms, created_at_ms, updated_at_ms
                 FROM playbooks WHERE name = ?1",
                params![name],
                row_to_playbook,
            )
            .optional()
            .map_err(StoreError::sqlite("lookup playbook"))?;

        if let Some(prev) = existing {
            let changed = prev.steps_json != pb.steps_json;
            let next_version = if changed { prev.version + 1 } else { prev.version };
            let prev_steps = if changed {
                Some(prev.steps_json.clone())
            } else {
                prev.previous_steps_json.clone()
            };
            tx.execute(
                "UPDATE playbooks
                 SET display_name = ?2, description = ?3, triggers_json = ?4,
                     scope = ?5, parameters_json = ?6, steps_json = ?7,
                     version = ?8, previous_steps_json = ?9, source = ?10, updated_at_ms = ?11
                 WHERE id = ?1",
                params![
                    prev.id,
                    display_name,
                    description,
                    pb.triggers_json,
                    scope,
                    pb.parameters_json,
                    pb.steps_json,
                    next_version,
                    prev_steps,
                    source,
                    now
                ],
            )
            .map_err(StoreError::sqlite("update playbook"))?;
            tx.commit()
                .map_err(StoreError::sqlite("commit update playbook"))?;
            return Ok(PlaybookRecord {
                display_name,
                description,
                triggers_json: pb.triggers_json.clone(),
                scope,
                parameters_json: pb.parameters_json.clone(),
                steps_json: pb.steps_json.clone(),
                version: next_version,
                previous_steps_json: prev_steps,
                source: source.to_owned(),
                updated_at_ms: now,
                ..prev
            });
        }

        tx.execute(
            "INSERT INTO playbooks
             (id, name, display_name, description, triggers_json, scope, parameters_json,
              steps_json, version, previous_steps_json, source, verified_successes, failures,
              created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, NULL, ?9, 0, 0, ?10, ?10)",
            params![
                pb.id,
                name,
                display_name,
                description,
                pb.triggers_json,
                scope,
                pb.parameters_json,
                pb.steps_json,
                source,
                now
            ],
        )
        .map_err(StoreError::sqlite("insert playbook"))?;
        tx.commit()
            .map_err(StoreError::sqlite("commit insert playbook"))?;

        Ok(PlaybookRecord {
            id: pb.id.clone(),
            name,
            display_name,
            description,
            triggers_json: pb.triggers_json.clone(),
            scope,
            parameters_json: pb.parameters_json.clone(),
            steps_json: pb.steps_json.clone(),
            version: 1,
            previous_steps_json: None,
            source: source.to_owned(),
            verified_successes: 0,
            failures: 0,
            last_used_ms: None,
            created_at_ms: now,
            updated_at_ms: now,
        })
    }

    pub fn get_playbook(&self, name: &str) -> Result<Option<PlaybookRecord>> {
        let norm = name.trim().to_ascii_lowercase();
        self.conn
            .query_row(
                "SELECT id, name, display_name, description, triggers_json, scope,
                        parameters_json, steps_json, version, previous_steps_json, source,
                        verified_successes, failures, last_used_ms, created_at_ms, updated_at_ms
                 FROM playbooks WHERE name = ?1 OR id = ?1 LIMIT 1",
                params![norm],
                row_to_playbook,
            )
            .optional()
            .map_err(StoreError::sqlite("get playbook"))
    }

    pub fn list_playbooks(
        &self,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PlaybookRecord>> {
        let limit = limit.clamp(1, 200) as i64;
        match scope.map(valid_fact_scope).transpose()? {
            Some(scope) => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, name, display_name, description, triggers_json, scope,
                                parameters_json, steps_json, version, previous_steps_json, source,
                                verified_successes, failures, last_used_ms, created_at_ms, updated_at_ms
                         FROM playbooks
                         WHERE scope = '' OR scope = ?1
                         ORDER BY verified_successes DESC, updated_at_ms DESC LIMIT ?2",
                    )
                    .map_err(StoreError::sqlite("prepare list playbooks"))?;
                let rows = stmt
                    .query_map(params![scope, limit], row_to_playbook)
                    .map_err(StoreError::sqlite("query list playbooks"))?;
                collect(rows, "read playbook")
            }
            None => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, name, display_name, description, triggers_json, scope,
                                parameters_json, steps_json, version, previous_steps_json, source,
                                verified_successes, failures, last_used_ms, created_at_ms, updated_at_ms
                         FROM playbooks
                         ORDER BY verified_successes DESC, updated_at_ms DESC LIMIT ?1",
                    )
                    .map_err(StoreError::sqlite("prepare list playbooks"))?;
                let rows = stmt
                    .query_map(params![limit], row_to_playbook)
                    .map_err(StoreError::sqlite("query list playbooks"))?;
                collect(rows, "read playbook")
            }
        }
    }

    pub fn search_playbooks(
        &self,
        query: &str,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PlaybookRecord>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        // Build an OR query of safe alphanumeric tokens so natural utterances recall playbooks cleanly.
        let tokens: Vec<String> = trimmed
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() >= 2)
            .take(12)
            .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
            .collect();
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let expression = tokens.join(" OR ");
        let limit = limit.clamp(1, 50) as i64;
        let scope_filter = match scope {
            Some(_) => "AND (playbooks.scope = '' OR playbooks.scope = ?3)",
            None => "",
        };
        let _guard = QueryOnly::new(&self.conn)?;
        let _budget = SearchBudget::install(&self.conn)?;

        let sql = format!(
            "SELECT playbooks.id, playbooks.name, playbooks.display_name, playbooks.description,
                    playbooks.triggers_json, playbooks.scope, playbooks.parameters_json,
                    playbooks.steps_json, playbooks.version, playbooks.previous_steps_json,
                    playbooks.source, playbooks.verified_successes, playbooks.failures,
                    playbooks.last_used_ms, playbooks.created_at_ms, playbooks.updated_at_ms
             FROM playbooks_fts
             JOIN playbooks ON playbooks.rowid = playbooks_fts.rowid
             WHERE playbooks_fts MATCH ?1 {scope_filter}
             ORDER BY bm25(playbooks_fts), playbooks.verified_successes DESC
             LIMIT ?2"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(StoreError::sqlite("prepare playbook search"))?;
        let mut bindings: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(expression), Box::new(limit)];
        if let Some(s) = scope {
            bindings.push(Box::new(valid_fact_scope(s)?));
        }
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(bindings.iter()),
                row_to_playbook,
            )
            .map_err(|e| classify_sqlite_error("playbook search", e))?;
        collect(rows, "read playbook search row")
    }

    pub fn undo_playbook(&self, name: &str) -> Result<Option<PlaybookRecord>> {
        let Some(existing) = self.get_playbook(name)? else {
            return Ok(None);
        };
        let Some(prev_steps) = existing.previous_steps_json.clone() else {
            return Err(StoreError::Memory {
                reason: format!("playbook {:?} has no previous version to restore", existing.name),
            });
        };
        let now = now_ms();
        self.conn
            .execute(
                "UPDATE playbooks
                 SET steps_json = ?2, previous_steps_json = NULL,
                     version = MAX(1, version - 1), updated_at_ms = ?3
                 WHERE id = ?1",
                params![existing.id, prev_steps, now],
            )
            .map_err(StoreError::sqlite("undo playbook"))?;
        self.get_playbook(&existing.name)
    }

    pub fn forget_playbook(&self, name: &str) -> Result<Option<PlaybookRecord>> {
        let Some(existing) = self.get_playbook(name)? else {
            return Ok(None);
        };
        self.conn
            .execute("DELETE FROM playbooks WHERE id = ?1", params![existing.id])
            .map_err(StoreError::sqlite("delete playbook"))?;
        Ok(Some(existing))
    }

    pub fn record_playbook_run(
        &self,
        playbook_id: &str,
        task_id: Option<&str>,
        outcome: &str,
        duration_ms: i64,
        notes: &str,
    ) -> Result<()> {
        let outcome = valid_enum(
            outcome,
            &[
                "dispatched",
                "verified_success",
                "failed",
                "uncertain",
                "cancelled",
            ],
            "playbook run outcome",
        )?;
        let notes = credential_floor().redact(notes.trim());
        let notes: String = notes.chars().take(500).collect();
        let now = now_ms();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(StoreError::sqlite("begin record playbook run"))?;
        tx.execute(
            "INSERT INTO playbook_runs (playbook_id, task_id, outcome, duration_ms, notes, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![playbook_id, task_id, outcome, duration_ms.max(0), notes, now],
        )
        .map_err(StoreError::sqlite("insert playbook run"))?;
        match outcome {
            "verified_success" => {
                tx.execute(
                    "UPDATE playbooks
                     SET verified_successes = verified_successes + 1, last_used_ms = ?2, updated_at_ms = ?2
                     WHERE id = ?1",
                    params![playbook_id, now],
                )
                .map_err(StoreError::sqlite("increment playbook success"))?;
            }
            "failed" => {
                tx.execute(
                    "UPDATE playbooks
                     SET failures = failures + 1, last_used_ms = ?2, updated_at_ms = ?2
                     WHERE id = ?1",
                    params![playbook_id, now],
                )
                .map_err(StoreError::sqlite("increment playbook failure"))?;
            }
            _ => {
                tx.execute(
                    "UPDATE playbooks SET last_used_ms = ?2 WHERE id = ?1",
                    params![playbook_id, now],
                )
                .map_err(StoreError::sqlite("touch playbook last_used"))?;
            }
        }
        tx.commit()
            .map_err(StoreError::sqlite("commit record playbook run"))?;
        Ok(())
    }

    pub fn playbook_runs(
        &self,
        playbook_id: &str,
        limit: usize,
    ) -> Result<Vec<PlaybookRunRecord>> {
        let limit = limit.clamp(1, 50) as i64;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, playbook_id, task_id, outcome, duration_ms, notes, at_ms
                 FROM playbook_runs
                 WHERE playbook_id = ?1
                 ORDER BY at_ms DESC, id DESC LIMIT ?2",
            )
            .map_err(StoreError::sqlite("prepare playbook runs"))?;
        let rows = stmt
            .query_map(params![playbook_id, limit], |row| {
                Ok(PlaybookRunRecord {
                    id: row.get(0)?,
                    playbook_id: row.get(1)?,
                    task_id: row.get(2)?,
                    outcome: row.get(3)?,
                    duration_ms: row.get(4)?,
                    notes: row.get(5)?,
                    at_ms: row.get(6)?,
                })
            })
            .map_err(StoreError::sqlite("query playbook runs"))?;
        collect(rows, "read playbook run")
    }
}

fn valid_enum<'a>(value: &str, allowed: &[&'a str], label: &str) -> Result<&'a str> {
    allowed
        .iter()
        .copied()
        .find(|candidate| *candidate == value)
        .ok_or_else(|| StoreError::Memory {
            reason: format!(
                "invalid {label} {value:?}; expected one of {}",
                allowed.join(", ")
            ),
        })
}

fn valid_bounded_plain(text: &str, max_chars: usize, label: &str) -> Result<String> {
    let trimmed = text.trim();
    if credential_floor().redact(trimmed) != trimmed {
        return Err(StoreError::Memory {
            reason: format!("{label} contains credential-shaped text; nothing was saved"),
        });
    }
    if trimmed.chars().count() > max_chars {
        return Err(StoreError::Memory {
            reason: format!("{label} exceeds {max_chars} characters"),
        });
    }
    if trimmed.chars().any(|c| c.is_control() && c != '\t') {
        return Err(StoreError::Memory {
            reason: format!("{label} may not contain control characters"),
        });
    }
    Ok(trimmed.to_owned())
}

fn valid_playbook_name(name: &str) -> Result<String> {
    let norm = name
        .trim()
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-");
    if norm.is_empty() || norm.chars().count() > MAX_PLAYBOOK_NAME_CHARS {
        return Err(StoreError::Memory {
            reason: format!("playbook name must be 1..={MAX_PLAYBOOK_NAME_CHARS} characters"),
        });
    }
    if !norm
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(StoreError::Memory {
            reason: "playbook name may only contain letters, digits, spaces, '-', '_', or '.'"
                .to_owned(),
        });
    }
    Ok(norm)
}

fn valid_json_payload(payload: &str, label: &str) -> Result<()> {
    if payload.len() > MAX_PLAYBOOK_JSON_BYTES {
        return Err(StoreError::Memory {
            reason: format!("{label} exceeds {MAX_PLAYBOOK_JSON_BYTES} bytes"),
        });
    }
    if credential_floor().redact(payload) != payload {
        return Err(StoreError::Memory {
            reason: format!("{label} contains credential-shaped text; nothing was saved"),
        });
    }
    serde_json::from_str::<serde_json::Value>(payload).map_err(|e| StoreError::Memory {
        reason: format!("{label} is not valid JSON: {e}"),
    })?;
    Ok(())
}

fn row_to_episode(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEpisode> {
    Ok(MemoryEpisode {
        id: row.get(0)?,
        kind: row.get(1)?,
        scope: row.get(2)?,
        summary: row.get(3)?,
        tags: row.get(4)?,
        salience: row.get(5)?,
        task_id: row.get(6)?,
        consolidated: row.get::<_, i64>(7)? != 0,
        created_at_ms: row.get(8)?,
        last_recalled_ms: row.get(9)?,
        recall_count: row.get(10)?,
    })
}

fn row_to_note(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryNote> {
    Ok(MemoryNote {
        id: row.get(0)?,
        category: row.get(1)?,
        scope: row.get(2)?,
        topic_key: row.get(3)?,
        text: row.get(4)?,
        confidence: row.get(5)?,
        salience: row.get(6)?,
        origin: row.get(7)?,
        status: row.get(8)?,
        superseded_by: row.get(9)?,
        evidence_count: row.get(10)?,
        recall_count: row.get(11)?,
        last_recalled_ms: row.get(12)?,
        created_at_ms: row.get(13)?,
        updated_at_ms: row.get(14)?,
    })
}

fn row_to_dream_cycle(row: &rusqlite::Row<'_>) -> rusqlite::Result<DreamCycleRecord> {
    Ok(DreamCycleRecord {
        id: row.get(0)?,
        mode: row.get(1)?,
        scope: row.get(2)?,
        episodes_scanned: row.get(3)?,
        tasks_scanned: row.get(4)?,
        notes_created: row.get(5)?,
        notes_updated: row.get(6)?,
        notes_superseded: row.get(7)?,
        notes_archived: row.get(8)?,
        playbooks_promoted: row.get(9)?,
        summary: row.get(10)?,
        created_at_ms: row.get(11)?,
    })
}

fn row_to_playbook(row: &rusqlite::Row<'_>) -> rusqlite::Result<PlaybookRecord> {
    Ok(PlaybookRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        display_name: row.get(2)?,
        description: row.get(3)?,
        triggers_json: row.get(4)?,
        scope: row.get(5)?,
        parameters_json: row.get(6)?,
        steps_json: row.get(7)?,
        version: row.get(8)?,
        previous_steps_json: row.get(9)?,
        source: row.get(10)?,
        verified_successes: row.get(11)?,
        failures: row.get(12)?,
        last_used_ms: row.get(13)?,
        created_at_ms: row.get(14)?,
        updated_at_ms: row.get(15)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_notes_self_update_reinforce_and_supersede_cleanly() {
        let store = Store::open_in_memory().unwrap();

        let first = store
            .upsert_memory_note(&NewMemoryNote {
                id: "mn-1".into(),
                category: "preference".into(),
                scope: String::new(),
                topic_key: "preferred_editor".into(),
                text: "Operator prefers VS Code for Rust projects".into(),
                confidence: 85,
                salience: 70,
                origin: "self_update".into(),
            })
            .unwrap();
        assert!(matches!(first, MemoryNoteUpsert::Created(_)));

        // Identical text reinforces evidence and salience rather than duplicating.
        let reinforced = store
            .upsert_memory_note(&NewMemoryNote {
                id: "mn-2".into(),
                category: "preference".into(),
                scope: String::new(),
                topic_key: "preferred_editor".into(),
                text: "operator prefers vs code for rust projects".into(),
                confidence: 90,
                salience: 75,
                origin: "dream_consolidation".into(),
            })
            .unwrap();
        match reinforced {
            MemoryNoteUpsert::Reinforced(note) => {
                assert_eq!(note.id, "mn-1");
                assert_eq!(note.evidence_count, 2);
                assert!(note.salience >= 80);
            }
            other => panic!("expected reinforcement, got {other:?}"),
        }

        // Contradictory update on the same topic_key supersedes the older note.
        let superseded = store
            .upsert_memory_note(&NewMemoryNote {
                id: "mn-3".into(),
                category: "preference".into(),
                scope: String::new(),
                topic_key: "preferred_editor".into(),
                text: "Operator prefers Cursor for Rust projects".into(),
                confidence: 95,
                salience: 85,
                origin: "self_update".into(),
            })
            .unwrap();
        match superseded {
            MemoryNoteUpsert::Superseded {
                previous_id,
                current,
            } => {
                assert_eq!(previous_id, "mn-1");
                assert_eq!(current.id, "mn-3");
            }
            other => panic!("expected superseding, got {other:?}"),
        }

        let active = store.active_memory_notes(None, 10).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "mn-3");
        assert!(active[0].text.contains("Cursor"));

        let hits = store.search_memory_notes("Cursor", None, 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].note.id, "mn-3");
        // Superseded note is excluded from active search.
        assert!(store.search_memory_notes("VS Code", None, 5).unwrap().is_empty());
    }

    #[test]
    fn cognitive_writers_refuse_credentials_and_playbooks_support_versioning_and_undo() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .add_episode(&NewMemoryEpisode {
                id: "ep-secret".into(),
                kind: "conversation".into(),
                scope: String::new(),
                summary: "my key is sk-live-SYNTHETICSECRET12345".into(),
                tags: "chat".into(),
                salience: 50,
                task_id: None,
            })
            .expect_err("credentials must be refused in episodes");
        assert!(err.to_string().contains("credential"));

        let saved = store
            .save_playbook(&NewPlaybook {
                id: "pb-1".into(),
                name: "Morning Check".into(),
                display_name: "Morning Check".into(),
                description: "Check Sri Lanka time and open Gmail".into(),
                triggers_json: r#"["morning check","start my morning"]"#.into(),
                scope: String::new(),
                parameters_json: "[]".into(),
                steps_json: r#"[{"kind":"clock","scope":"sri-lanka"}]"#.into(),
                source: "taught".into(),
            })
            .unwrap();
        assert_eq!(saved.name, "morning-check");
        assert_eq!(saved.version, 1);

        let updated = store
            .save_playbook(&NewPlaybook {
                id: "pb-2".into(),
                name: "Morning Check".into(),
                display_name: "Morning Check".into(),
                description: "Check Sri Lanka time and open Gmail".into(),
                triggers_json: r#"["morning check","start my morning"]"#.into(),
                scope: String::new(),
                parameters_json: "[]".into(),
                steps_json: r#"[{"kind":"clock","scope":"sri-lanka"},{"kind":"gmail"}]"#.into(),
                source: "taught".into(),
            })
            .unwrap();
        assert_eq!(updated.version, 2);
        assert!(updated.previous_steps_json.is_some());

        store
            .record_playbook_run(&updated.id, Some("t-1"), "verified_success", 42, "all good")
            .unwrap();
        let found = store.search_playbooks("start my morning", None, 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].verified_successes, 1);

        let restored = store.undo_playbook("morning-check").unwrap().unwrap();
        assert_eq!(restored.version, 1);
        assert!(restored.steps_json.contains("sri-lanka"));
        assert!(!restored.steps_json.contains("gmail"));
    }
}
