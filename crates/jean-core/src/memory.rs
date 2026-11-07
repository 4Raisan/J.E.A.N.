//! Explicit operator memory; retrieved text is data, never authority.
//! Mutations are audited atomically by the store, without content or scope.

use jean_protocol::preferences::PreferenceKey;

use jean_store::{FactAddOutcome, FactHit, HistoryHit, Preference, SavedFact, SearchOptions};

use crate::context::Context;
use crate::error::{CoreError, Result};

pub const MAX_FACTS_IN_VIEW: usize = 20;

pub const MAX_VIEW_CHARS: usize = 4000;

pub struct MemoryView {
    facts: Vec<SavedFact>,
    truncated: bool,
}

impl MemoryView {
    pub fn read(ctx: &Context, scope: Option<&str>) -> Result<Self> {
        let stored = ctx.store.saved_facts(scope, MAX_FACTS_IN_VIEW + 1)?;
        let total = stored.len();
        let mut facts = Vec::new();
        let mut used = 0usize;
        for fact in stored.into_iter().take(MAX_FACTS_IN_VIEW) {
            let cost = fact.text.chars().count();
            if used + cost > MAX_VIEW_CHARS {
                break;
            }
            used += cost;
            facts.push(fact);
        }
        Ok(Self {
            truncated: facts.len() < total,
            facts,
        })
    }

    pub fn facts(&self) -> &[SavedFact] {
        &self.facts
    }

    pub fn was_truncated(&self) -> bool {
        self.truncated
    }

    pub fn chars(&self) -> usize {
        self.facts.iter().map(|f| f.text.chars().count()).sum()
    }
}

pub fn search_history(ctx: &Context, options: &SearchOptions<'_>) -> Result<Vec<HistoryHit>> {
    Ok(ctx.store.search_history(options)?)
}

pub fn add_fact(ctx: &mut Context, text: &str, scope: Option<&str>) -> Result<FactAddOutcome> {
    let prepared = prepare_fact_text(ctx, text, scope)?;

    let id = new_fact_id();
    let outcome = ctx
        .store
        .add_saved_fact(&id, &prepared.text, prepared.scope.as_deref())?;
    Ok(outcome)
}

pub fn update_fact(ctx: &mut Context, id: &str, text: &str) -> Result<Option<SavedFact>> {
    let prepared = prepare_fact_text(ctx, text, None)?;
    let updated = ctx.store.update_saved_fact(id, &prepared.text)?;
    Ok(updated)
}

pub fn forget_fact(ctx: &mut Context, id: &str) -> Result<Option<SavedFact>> {
    let forgotten = ctx.store.forget_saved_fact(id)?;
    Ok(forgotten)
}

pub fn list_facts(ctx: &Context, scope: Option<&str>, limit: usize) -> Result<Vec<SavedFact>> {
    Ok(ctx.store.saved_facts(scope, limit)?)
}

pub fn get_fact(ctx: &Context, id: &str) -> Result<Option<SavedFact>> {
    Ok(ctx.store.saved_fact(id)?)
}

pub fn search_facts(
    ctx: &Context,
    query: &str,
    scope: Option<&str>,
    limit: usize,
) -> Result<Vec<FactHit>> {
    Ok(ctx.store.search_saved_facts(query, scope, limit)?)
}

pub fn history_status(ctx: &Context) -> Result<jean_store::HistoryIndexReport> {
    Ok(ctx.store.history_index_report()?)
}

pub fn rebuild_history(ctx: &mut Context) -> Result<jean_store::HistoryIndexReport> {
    Ok(ctx.store.rebuild_history_index()?)
}

pub fn set_preference(ctx: &mut Context, key: &str, value: &str) -> Result<Preference> {
    let parsed = PreferenceKey::parse(key).ok_or_else(|| CoreError::Memory {
        reason: format!(
            "{key:?} is not a preference this build reads. Supported: {}",
            PreferenceKey::ALL
                .iter()
                .map(|k| format!("{} ({})", k.as_str(), k.meaning()))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    })?;
    parsed.validate(value).map_err(|reason| CoreError::Memory {
        reason: format!("{}: {reason}", parsed.as_str()),
    })?;

    let value = parsed.canonical_value(value);
    let mut rows = ctx.store.preference_rows()?;
    rows.retain(|(k, _)| k != parsed.as_str());
    rows.push((parsed.as_str().to_owned(), value.clone()));
    let candidate = ctx.preference_candidate(&rows)?;
    let stored = ctx.store.set_preference(parsed.as_str(), &value)?;
    ctx.install_preference_config(candidate);
    Ok(stored)
}

pub fn clear_preference(ctx: &mut Context, key: &str) -> Result<bool> {
    let cleared = ctx.store.clear_preference(key.trim())?;
    ctx.refresh_configuration_from_layers()?;
    Ok(cleared)
}

pub fn list_preferences(ctx: &Context) -> Result<Vec<Preference>> {
    Ok(ctx.store.list_preferences()?)
}

pub fn supported_preferences() -> Vec<(&'static str, &'static str, &'static [&'static str])> {
    PreferenceKey::ALL
        .iter()
        .map(|key| (key.as_str(), key.meaning(), key.accepted_values()))
        .collect()
}

struct Prepared {
    text: String,
    scope: Option<String>,
}

fn prepare_fact_text(ctx: &Context, text: &str, scope: Option<&str>) -> Result<Prepared> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(CoreError::Memory {
            reason: "there is nothing to save; `jean memory add` needs a sentence".to_owned(),
        });
    }
    let redacted = ctx.redactor.redact(trimmed);
    if !ctx.redactor.invalid_patterns.is_empty()
        || redacted != trimmed
        || scope.is_some_and(|s| ctx.redactor.redact(s) != s)
    {
        return Err(CoreError::Memory {
            reason: "credential-shaped text or invalid redaction configuration; nothing saved"
                .into(),
        });
    }
    Ok(Prepared {
        text: redacted,
        scope: scope.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()),
    })
}

fn new_fact_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("f-{nanos:x}-{:x}-{seq:x}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_view_is_bounded_in_rows_and_in_characters() {
        assert_eq!(MAX_FACTS_IN_VIEW, 20);
        const { assert!(MAX_VIEW_CHARS < MAX_FACTS_IN_VIEW * jean_store::MAX_FACT_CHARS) };
    }

    #[test]
    fn the_fact_id_is_safe_to_print_and_bounded() {
        let a = new_fact_id();
        let b = new_fact_id();
        assert!(a.starts_with("f-") && b.starts_with("f-"));
        assert_ne!(a, b, "two facts saved in the same call must not collide");
        assert!(a.len() < 64, "{a}");
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
