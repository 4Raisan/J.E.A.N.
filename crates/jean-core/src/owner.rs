//! Durable owner-workflow checkpoints for supervised handoffs.

use std::{
    fs::OpenOptions,
    io::Write,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{CoreError, Result};

const MAX_FIELD_BYTES: usize = 16 * 1024;
const MAX_RESULTS: usize = 32;
const MAX_RESULT_BYTES: usize = 2 * 1024;
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
const MAX_EFFECT_BYTES: usize = 16 * 1024;
const MAX_EFFECTS: usize = 16;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceEffect {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryBlocker {
    SignInExpired,
    NetworkUnavailable,
    LowDisk,
    DiskCheckUnavailable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryCause {
    Quota,
    Context,
    SignIn,
    Network,
}

impl RecoveryBlocker {
    pub const fn status(self) -> &'static str {
        match self {
            Self::SignInExpired => "blocked_sign_in_expired",
            Self::NetworkUnavailable => "blocked_network_unavailable",
            Self::LowDisk => "blocked_low_disk",
            Self::DiskCheckUnavailable => "blocked_disk_check_unavailable",
        }
    }

    pub const fn action(self) -> &'static str {
        match self {
            Self::SignInExpired => "sign in to the configured worker, then rerun with --condition-cleared",
            Self::NetworkUnavailable => "restore network access, then rerun with --condition-cleared",
            Self::LowDisk => "free disk space on the workspace and JEAN data volumes, then run jean resume again",
            Self::DiskCheckUnavailable => "make the workspace and JEAN data paths accessible to the disk-space check, then retry",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub task_id: String,
    pub goal: String,
    pub resolved_scope: String,
    pub results: Vec<String>,
    pub next_action: String,
    /// True from before a worker can act until its effects have been reconciled.
    pub uncertain: bool,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub remaining_work: String,
    #[serde(default)]
    pub completed_effects: Vec<WorkspaceEffect>,
    #[serde(default)]
    pub eligible_free_workers: Vec<String>,
    #[serde(default)]
    pub attempted_workers: Vec<String>,
    #[serde(default)]
    pub attempts: u8,
    #[serde(default)]
    pub max_attempts: u8,
    #[serde(default)]
    pub resumable: bool,
    #[serde(default)]
    pub resumed_task_id: Option<String>,
    #[serde(default)]
    pub blocked_on: Option<RecoveryBlocker>,
    #[serde(default)]
    pub recovery_cause: Option<RecoveryCause>,
}

fn checked_path(data_dir: &Path, task_id: &str) -> Result<PathBuf> {
    jean_protocol::ipc::validate_task_id(task_id).map_err(|e| CoreError::Environment {
        reason: format!("invalid checkpoint task id: {e}"),
    })?;
    Ok(data_dir.join("checkpoints").join(format!("{task_id}.json")))
}

pub fn checkpoint_path(data_dir: &Path, task_id: &str) -> Result<PathBuf> {
    checked_path(data_dir, task_id)
}

fn bounded(value: &str, max: usize, field: &str) -> Result<String> {
    if value.len() > max {
        return Err(CoreError::Environment {
            reason: format!("checkpoint {field} exceeds {max} bytes"),
        });
    }
    Ok(value.to_owned())
}

fn validate_and_redact(mut checkpoint: Checkpoint) -> Result<Checkpoint> {
    jean_protocol::ipc::validate_task_id(&checkpoint.task_id).map_err(|e| {
        CoreError::Environment {
            reason: format!("invalid checkpoint task id: {e}"),
        }
    })?;
    if checkpoint.results.len() > MAX_RESULTS {
        return Err(CoreError::Environment {
            reason: format!("checkpoint has more than {MAX_RESULTS} results"),
        });
    }
    let redactor = jean_protocol::Redactor::new(&jean_protocol::config::default_redact_patterns());
    checkpoint.goal = redactor.redact(&bounded(&checkpoint.goal, MAX_FIELD_BYTES, "goal")?);
    checkpoint.resolved_scope = redactor.redact(&bounded(
        &checkpoint.resolved_scope,
        MAX_FIELD_BYTES,
        "scope",
    )?);
    checkpoint.workspace = redactor.redact(&bounded(
        &checkpoint.workspace,
        MAX_FIELD_BYTES,
        "workspace",
    )?);
    checkpoint.next_action = redactor.redact(&bounded(
        &checkpoint.next_action,
        MAX_FIELD_BYTES,
        "next action",
    )?);
    checkpoint.results = checkpoint
        .results
        .iter()
        .map(|r| bounded(r, MAX_RESULT_BYTES, "result").map(|s| redactor.redact(&s)))
        .collect::<Result<Vec<_>>>()?;
    checkpoint.remaining_work = redactor.redact(&bounded(
        &checkpoint.remaining_work,
        MAX_FIELD_BYTES,
        "remaining work",
    )?);
    if checkpoint.completed_effects.len() > MAX_EFFECTS {
        return Err(CoreError::Environment {
            reason: format!("checkpoint has more than {MAX_EFFECTS} workspace effects"),
        });
    }
    for effect in &mut checkpoint.completed_effects {
        effect.path = redactor.redact(&bounded(&effect.path, MAX_FIELD_BYTES, "effect path")?);
        validate_relative_path(&effect.path)?;
        if effect.sha256.len() != 64 || !effect.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(CoreError::Environment {
                reason: "checkpoint has an invalid workspace digest".to_owned(),
            });
        }
        if effect.bytes as usize > MAX_EFFECT_BYTES {
            return Err(CoreError::Environment {
                reason: format!("checkpoint workspace effect exceeds {MAX_EFFECT_BYTES} bytes"),
            });
        }
    }
    if checkpoint.eligible_free_workers.len() > MAX_EFFECTS
        || checkpoint.attempted_workers.len() > MAX_EFFECTS
    {
        return Err(CoreError::Environment {
            reason: "checkpoint has too many worker choices or attempts".to_owned(),
        });
    }
    for name in checkpoint
        .eligible_free_workers
        .iter()
        .chain(checkpoint.attempted_workers.iter())
    {
        bounded(name, 128, "worker name")?;
    }
    Ok(checkpoint)
}

fn validate_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(CoreError::Environment {
            reason: "checkpoint effect path must be a normalized relative path".to_owned(),
        });
    }
    Ok(())
}

pub fn snapshot_workspace_effects(
    workspace: &Path,
    paths: &[String],
) -> Result<Vec<WorkspaceEffect>> {
    if paths.is_empty() || paths.len() > MAX_EFFECTS {
        return Err(CoreError::Environment {
            reason: format!("resumable handoff requires 1 to {MAX_EFFECTS} changed files"),
        });
    }
    let root = workspace
        .canonicalize()
        .map_err(|e| CoreError::Environment {
            reason: format!("workspace cannot be resolved for checkpoint: {e}"),
        })?;
    let mut effects = Vec::with_capacity(paths.len());
    for value in paths {
        validate_relative_path(value)?;
        let path = root.join(value);
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| CoreError::Environment {
            reason: format!("changed workspace file {value:?} cannot be inspected: {e}"),
        })?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() as usize > MAX_EFFECT_BYTES
        {
            return Err(CoreError::Environment {
                reason: format!("changed workspace file {value:?} is not a bounded regular file"),
            });
        }
        let canonical = path.canonicalize().map_err(|e| CoreError::Environment {
            reason: format!("changed workspace file {value:?} cannot be resolved: {e}"),
        })?;
        if !canonical.starts_with(&root) {
            return Err(CoreError::Environment {
                reason: format!("changed workspace file {value:?} escapes the workspace"),
            });
        }
        let bytes = std::fs::read(&canonical).map_err(|e| CoreError::Environment {
            reason: format!("changed workspace file {value:?} cannot be read: {e}"),
        })?;
        let sha256 = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        effects.push(WorkspaceEffect {
            path: value.clone(),
            bytes: bytes.len() as u64,
            sha256,
        });
    }
    Ok(effects)
}

pub fn reconcile_workspace(checkpoint: &Checkpoint) -> Result<()> {
    let workspace = PathBuf::from(&checkpoint.workspace)
        .canonicalize()
        .map_err(|e| CoreError::Environment {
            reason: format!("checkpoint workspace is unavailable: {e}"),
        })?;
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(&workspace)
        .output()
        .map_err(|e| CoreError::Environment {
            reason: format!("workspace reconciliation could not run git status: {e}"),
        })?;
    if !output.status.success() {
        return Err(CoreError::Environment {
            reason: "workspace reconciliation could not read Git status".to_owned(),
        });
    }
    reconcile_status(checkpoint, &workspace, &output.stdout)
}

pub(crate) fn reconcile_status(
    checkpoint: &Checkpoint,
    workspace: &Path,
    status: &[u8],
) -> Result<()> {
    let expected: std::collections::BTreeSet<_> = checkpoint
        .completed_effects
        .iter()
        .map(|effect| effect.path.clone())
        .collect();
    let mut actual = std::collections::BTreeSet::new();
    for record in status.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        if record.len() < 4 || record[2] != b' ' {
            return Err(CoreError::Environment {
                reason:
                    "workspace has an unsupported Git status record; reconciliation is ambiguous"
                        .to_owned(),
            });
        }
        let path = std::str::from_utf8(&record[3..])
            .map_err(|_| CoreError::Environment {
                reason: "workspace contains a non-UTF-8 path; reconciliation is ambiguous"
                    .to_owned(),
            })?
            .to_owned();
        if !actual.insert(path) {
            return Err(CoreError::Environment {
                reason: "workspace Git status contains duplicate paths".to_owned(),
            });
        }
    }
    if actual != expected {
        return Err(CoreError::Environment {
            reason: format!(
                "workspace changed since checkpoint (expected {:?}, found {:?})",
                expected, actual
            ),
        });
    }
    let effects = if expected.is_empty() {
        Vec::new()
    } else {
        snapshot_workspace_effects(workspace, &expected.iter().cloned().collect::<Vec<_>>())?
    };
    let observed: std::collections::BTreeMap<_, _> =
        effects.into_iter().map(|e| (e.path.clone(), e)).collect();
    for expected_effect in &checkpoint.completed_effects {
        if observed.get(&expected_effect.path) != Some(expected_effect) {
            return Err(CoreError::Environment {
                reason: format!(
                    "workspace effect {:?} no longer matches the checkpoint",
                    expected_effect.path
                ),
            });
        }
    }
    Ok(())
}

pub fn write_checkpoint(data_dir: &Path, checkpoint: &Checkpoint) -> Result<()> {
    let checkpoint = validate_and_redact(checkpoint.clone())?;
    let path = checked_path(data_dir, &checkpoint.task_id)?;
    let parent = path.parent().expect("checkpoint path has parent");
    std::fs::create_dir_all(parent).map_err(|e| CoreError::Environment {
        reason: format!("checkpoint dir: {e}"),
    })?;
    let json = serde_json::to_vec_pretty(&checkpoint).map_err(|e| CoreError::Environment {
        reason: e.to_string(),
    })?;
    if json.len() > MAX_CHECKPOINT_BYTES {
        return Err(CoreError::Environment {
            reason: format!("checkpoint exceeds {MAX_CHECKPOINT_BYTES} bytes"),
        });
    }
    let mut last_error = None;
    for _ in 0..8 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".checkpoint-{}-{sequence}.tmp", std::process::id()));
        let opened = OpenOptions::new().write(true).create_new(true).open(&temp);
        let mut file = match opened {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(CoreError::Environment {
                    reason: format!("checkpoint temp: {e}"),
                });
            }
        };
        let result = (|| {
            file.write_all(&json)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temp, &path)
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                last_error = Some(e);
            }
        }
    }
    Err(CoreError::Environment {
        reason: format!(
            "atomic checkpoint replace failed: {}",
            last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "temporary name collisions".to_owned())
        ),
    })
}

pub fn read_checkpoint(data_dir: &Path, task_id: &str) -> Result<Checkpoint> {
    let path = checked_path(data_dir, task_id)?;
    let metadata = std::fs::metadata(&path).map_err(|e| CoreError::Environment {
        reason: format!("checkpoint read: {e}"),
    })?;
    if metadata.len() as usize > MAX_CHECKPOINT_BYTES {
        return Err(CoreError::Environment {
            reason: format!("checkpoint exceeds {MAX_CHECKPOINT_BYTES} bytes"),
        });
    }
    let raw = std::fs::read(&path).map_err(|e| CoreError::Environment {
        reason: format!("checkpoint read: {e}"),
    })?;
    let checkpoint: Checkpoint =
        serde_json::from_slice(&raw).map_err(|e| CoreError::Environment {
            reason: format!("checkpoint parse: {e}"),
        })?;
    if checkpoint.task_id != task_id {
        return Err(CoreError::Environment {
            reason: "checkpoint identity does not match requested task id".to_owned(),
        });
    }
    validate_and_redact(checkpoint)
}

pub fn reconcile_for_resume(checkpoint: &Checkpoint) -> Result<()> {
    if checkpoint.uncertain {
        return Err(CoreError::Environment { reason: "checkpoint has uncertain effects; reconcile workspace and task evidence before retrying".to_owned() });
    }
    if checkpoint.workspace.is_empty() {
        return Err(CoreError::Environment {
            reason: "checkpoint has no workspace evidence to resume".to_owned(),
        });
    }
    reconcile_workspace(checkpoint)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "jean-checkpoint-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn sample(uncertain: bool) -> Checkpoint {
        Checkpoint {
            task_id: "t-checkpoint-1".into(),
            goal: "ship token=private".into(),
            resolved_scope: "scratch workspace".into(),
            results: vec!["partial edit".into()],
            next_action: "verify".into(),
            uncertain,
            workspace: String::new(),
            remaining_work: "finish token=private".into(),
            completed_effects: Vec::new(),
            eligible_free_workers: Vec::new(),
            attempted_workers: Vec::new(),
            attempts: 0,
            max_attempts: 0,
            resumable: false,
            resumed_task_id: None,
            blocked_on: None,
            recovery_cause: None,
        }
    }

    #[test]
    fn checkpoint_is_bounded_redacted_atomic_and_restorable() {
        let dir = scratch();
        write_checkpoint(&dir, &sample(false)).unwrap();
        let loaded = read_checkpoint(&dir, "t-checkpoint-1").unwrap();
        assert!(loaded.goal.contains("[REDACTED]"));
        assert!(loaded.remaining_work.contains("[REDACTED]"));
        assert_eq!(loaded.results, ["partial edit"]);
        write_checkpoint(&dir, &sample(true)).unwrap();
        assert!(read_checkpoint(&dir, "t-checkpoint-1").unwrap().uncertain);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn checkpoint_rejects_ambiguous_ids_and_uncertain_replay() {
        let dir = scratch();
        assert!(checkpoint_path(&dir, "t/a").is_err());
        assert!(write_checkpoint(
            &dir,
            &Checkpoint {
                goal: "x".repeat(MAX_FIELD_BYTES + 1),
                ..sample(false)
            }
        )
        .is_err());
        assert!(reconcile_for_resume(&sample(true)).is_err());
        assert!(reconcile_for_resume(&sample(false)).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn checkpoint_is_restored_in_a_fresh_process() {
        if let Some(dir) = std::env::var_os("JEAN_CHECKPOINT_CHILD") {
            let checkpoint = read_checkpoint(Path::new(&dir), "t-checkpoint-1").unwrap();
            assert_eq!(checkpoint.results, ["partial edit"]);
            assert!(!checkpoint.uncertain);
            return;
        }

        let dir = scratch();
        write_checkpoint(&dir, &sample(false)).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "owner::tests::checkpoint_is_restored_in_a_fresh_process",
            ])
            .env("JEAN_CHECKPOINT_CHILD", &dir)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "fresh test process must restore the checkpoint"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
