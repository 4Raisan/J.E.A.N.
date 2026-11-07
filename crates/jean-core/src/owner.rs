//! Milestone 5.5: owner workflow — checkpointed handoffs.
//!
//! A checkpoint is `<data_dir>/checkpoints/<task_id>.json` holding the
//! goal, resolved scope, results, and next action, written BEFORE any
//! session switch. Resume reads it back, reconciles uncertain effects
//! (task must be `needs_attention` if a step was `running`), then continues.
//! Profiles are per `owner` + `workspace` today; learned candidates are
//! separate rows in `saved_facts`/`preferences` (D-053), not inferred.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub task_id: String,
    pub goal: String,
    pub resolved_scope: String,
    pub results: Vec<String>,
    pub next_action: String,
    pub uncertain: bool,
}

pub fn checkpoint_path(data_dir: &std::path::Path, task_id: &str) -> PathBuf {
    let safe: String = task_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    data_dir.join("checkpoints").join(format!("{safe}.json"))
}

pub fn write_checkpoint(data_dir: &std::path::Path, checkpoint: &Checkpoint) -> Result<()> {
    let p = checkpoint_path(data_dir, &checkpoint.task_id);
    if let Some(parent) = p.parent() { std::fs::create_dir_all(parent).map_err(|e| CoreError::Environment { reason: format!("checkpoint dir: {e}") })?; }
    let json = serde_json::to_string_pretty(checkpoint).map_err(|e| CoreError::Environment { reason: e.to_string() })?;
    std::fs::write(&p, json).map_err(|e| CoreError::Environment { reason: format!("checkpoint write: {e}") })?;
    Ok(())
}

pub fn read_checkpoint(data_dir: &std::path::Path, task_id: &str) -> Result<Checkpoint> {
    let p = checkpoint_path(data_dir, task_id);
    let raw = std::fs::read_to_string(&p).map_err(|e| CoreError::Environment { reason: format!("checkpoint read: {e}") })?;
    serde_json::from_str(&raw).map_err(|e| CoreError::Environment { reason: format!("checkpoint parse: {e}") })
}

pub fn reconcile_for_resume(checkpoint: &Checkpoint) -> Result<()> {
    if checkpoint.uncertain {
        return Err(CoreError::Environment { reason: "checkpoint has uncertain effects; reconcile before retrying".to_owned() });
    }
    Ok(())
}
