//! Verified Git patches and explicit disposition of isolated worker output.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    actor_identity, record, AuditWriter, Context, CoreError, Decision, Event, EventKind, Git,
    Operation, OperationKind, Policy, PolicyTier, Prompter, Repo, Result, Scope,
};

const MAX_METADATA: u64 = 64 * 1024;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Baseline {
    target: PathBuf,
    source: PathBuf,
    commit: String,
    target_ref: String,
}

#[derive(Deserialize, Serialize)]
struct Review {
    baseline: Baseline,
    tree: String,
    sha256: String,
}

pub struct Preview {
    pub workspace: PathBuf,
    pub baseline: String,
    pub sha256: String,
    pub state: &'static str,
    pub patch: Vec<u8>,
}

fn error(reason: impl Into<String>) -> CoreError {
    CoreError::Intent {
        reason: reason.into(),
    }
}

fn io(error_value: std::io::Error) -> CoreError {
    error(format!("review storage: {error_value}"))
}

fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(io)?;
    file.try_lock()
        .map_err(|e| error(format!("could not lock {}: {e}", path.display())))?;
    Ok(file)
}

pub(super) fn task_lock(ctx: &Context, id: &str) -> Result<File> {
    jean_protocol::ipc::validate_task_id(id)?;
    let dir = ctx.paths.artifacts_for(id);
    fs::create_dir_all(&dir).map_err(io)?;
    lock(&dir.join("owner-operation.lock"))
}

pub(super) fn workspace_lock(ctx: &Context, id: &str, dir: &Path) -> Result<File> {
    let git = git(ctx)?;
    let repo = repo(ctx, &git, id, dir);
    let git_dir = text(&repo, "owner-git-dir", &["rev-parse", "--absolute-git-dir"])?;
    lock(&Path::new(&git_dir).join("jean-review.lock"))
}

fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(io)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > limit {
        return Err(error(format!(
            "{} exceeds the review size limit",
            path.display()
        )));
    }
    Ok(bytes)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&read(path, MAX_METADATA)?)
        .map_err(|e| error(format!("invalid {}: {e}", path.display())))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(io)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|e| error(e.to_string()))?;
    if bytes.len() as u64 > MAX_METADATA {
        return Err(error("review metadata exceeds its size limit"));
    }
    write_new(path, &bytes)
}

fn git(ctx: &Context) -> Result<Git> {
    Git::find(ctx).ok_or_else(|| error("Git is unavailable"))
}

fn repo<'a>(ctx: &'a Context, git: &'a Git, id: &'a str, dir: &Path) -> Repo<'a> {
    Repo {
        ctx,
        git,
        dir: dir.to_path_buf(),
        task_id: id,
    }
}

fn run(repo: &Repo<'_>, label: &str, args: &[&str], index: Option<&Path>) -> Result<Vec<u8>> {
    let result = repo.run_with_index(label, args, index)?;
    if !result.complete || result.exit_code != Some(0) {
        return Err(error(format!(
            "Git {label} failed or exceeded its limits (exit {:?})",
            result.exit_code
        )));
    }
    Ok(result.stdout)
}

fn text(repo: &Repo<'_>, label: &str, args: &[&str]) -> Result<String> {
    String::from_utf8(run(repo, label, args, None)?)
        .map(|value| value.trim_end_matches(['\r', '\n']).to_owned())
        .map_err(|_| error("Git returned non-UTF-8 repository metadata"))
}

fn oid(value: String) -> Result<String> {
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(error("Git returned an invalid object ID"));
    }
    Ok(value)
}

fn head(repo: &Repo<'_>) -> Result<String> {
    oid(text(
        repo,
        "review-head",
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )?)
}

fn require_target_baseline(repo: &Repo<'_>, baseline: &Baseline) -> Result<()> {
    let clean = run(
        repo,
        "review-target-status",
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        None,
    )?
    .is_empty();
    if !clean
        || head(repo)? != baseline.commit
        || text(
            repo,
            "review-target-ref",
            &["rev-parse", "--symbolic-full-name", "HEAD"],
        )? != baseline.target_ref
    {
        return Err(error(
            "the target is dirty or its reviewed Git baseline has changed",
        ));
    }
    Ok(())
}

pub(super) fn record_baseline(
    ctx: &Context,
    id: &str,
    target: &Path,
    source: &Path,
) -> Result<Baseline> {
    let git = git(ctx)?;
    let target = target.canonicalize().map_err(io)?;
    let source = source.canonicalize().map_err(io)?;
    let target_repo = repo(ctx, &git, id, &target);
    if !text(
        &target_repo,
        "review-prefix",
        &["rev-parse", "--show-prefix"],
    )?
    .is_empty()
    {
        return Err(error("review requires the root of the target Git worktree"));
    }
    let commit = head(&repo(ctx, &git, id, &source))?;
    let target_ref = text(
        &target_repo,
        "review-ref",
        &["rev-parse", "--symbolic-full-name", "HEAD"],
    )?;
    if head(&target_repo)? != commit {
        return Err(error(
            "the target moved while the isolated baseline was created",
        ));
    }
    let baseline = Baseline {
        target,
        source,
        commit,
        target_ref,
    };
    write_json(
        &ctx.paths.artifacts_for(id).join("review-baseline.json"),
        &baseline,
    )?;
    Ok(baseline)
}

pub(super) fn resume_baseline(
    ctx: &Context,
    id: &str,
    worktree: &str,
    workspace: &str,
) -> Result<Baseline> {
    let baseline: Baseline = read_json(&ctx.paths.artifacts_for(id).join("review-baseline.json"))?;
    let git = git(ctx)?;
    let source = Path::new(worktree).canonicalize().map_err(io)?;
    if source != baseline.source
        || Path::new(workspace).canonicalize().map_err(io)? != source
        || head(&repo(ctx, &git, id, &source))? != baseline.commit
    {
        return Err(error(
            "the isolated recovery worktree no longer matches its recorded baseline",
        ));
    }
    Ok(baseline)
}

pub(super) fn validate_resume_workspace(
    ctx: &Context,
    checkpoint: &crate::owner::Checkpoint,
    baseline: Option<&Baseline>,
) -> Result<()> {
    reconcile_checkpoint(ctx, checkpoint)?;
    let id = &checkpoint.task_id;
    let source = Path::new(&checkpoint.workspace);
    if let Some(baseline) = baseline {
        let git = git(ctx)?;
        if source.canonicalize().map_err(io)? != baseline.source
            || head(&repo(ctx, &git, id, source))? != baseline.commit
        {
            return Err(error("the isolated recovery baseline changed"));
        }
        Ok(())
    } else {
        check_recovery_head(ctx, id, source)
    }
}

fn recovery_identity(ctx: &Context, id: &str, dir: &Path) -> Result<(PathBuf, String, String)> {
    let git = git(ctx)?;
    let dir = dir.canonicalize().map_err(io)?;
    let repo = repo(ctx, &git, id, &dir);
    let reference = text(
        &repo,
        "recovery-ref",
        &["rev-parse", "--symbolic-full-name", "HEAD"],
    )?;
    Ok((dir, head(&repo)?, reference))
}

pub(super) fn record_recovery_head(ctx: &Context, id: &str, dir: &Path) -> Result<()> {
    let identity = recovery_identity(ctx, id, dir)?;
    write_json(
        &ctx.paths.artifacts_for(id).join("recovery-head.json"),
        &identity,
    )
}

pub(super) fn check_recovery_head(ctx: &Context, id: &str, dir: &Path) -> Result<()> {
    let expected: (PathBuf, String, String) =
        read_json(&ctx.paths.artifacts_for(id).join("recovery-head.json"))?;
    if recovery_identity(ctx, id, dir)? != expected {
        return Err(error("the recovery workspace or Git baseline has changed"));
    }
    Ok(())
}

pub(super) fn reconcile_checkpoint(
    ctx: &Context,
    checkpoint: &crate::owner::Checkpoint,
) -> Result<()> {
    let git = git(ctx)?;
    let dir = Path::new(&checkpoint.workspace)
        .canonicalize()
        .map_err(io)?;
    let repo = repo(ctx, &git, &checkpoint.task_id, &dir);
    let status = run(
        &repo,
        "recovery-status",
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--ignore-submodules=none",
        ],
        None,
    )?;
    crate::owner::reconcile_status(checkpoint, &dir, &status)?;
    let expected: String = read_json(
        &ctx.paths
            .artifacts_for(&checkpoint.task_id)
            .join("recovery-index.json"),
    )?;
    if index_digest(&repo)? != expected {
        return Err(error(
            "the staged changes differ from the recovery checkpoint",
        ));
    }
    Ok(())
}

fn index_digest(repo: &Repo<'_>) -> Result<String> {
    let index_patch = run(
        repo,
        "recovery-index",
        &[
            "diff",
            "--cached",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            "HEAD",
            "--",
        ],
        None,
    )?;
    Ok(digest(&index_patch))
}

pub(super) fn record_recovery_index(ctx: &Context, id: &str, dir: &Path) -> Result<()> {
    let git = git(ctx)?;
    let repo = repo(ctx, &git, id, dir);
    let hash = index_digest(&repo)?;
    write_json(
        &ctx.paths.artifacts_for(id).join("recovery-index.json"),
        &hash,
    )
}

struct TemporaryIndex(PathBuf);
impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_file(self.0.with_extension("lock"));
    }
}

pub(super) fn snapshot(
    ctx: &Context,
    id: &str,
    source: &Path,
    baseline: &Baseline,
) -> Result<String> {
    let git = git(ctx)?;
    if source.canonicalize().map_err(io)? != baseline.source {
        return Err(error("the isolated worktree identity changed"));
    }
    let repo = repo(ctx, &git, id, source);
    if head(&repo)? != baseline.commit {
        return Err(error(
            "the worker changed HEAD; review requires the recorded detached baseline",
        ));
    }
    reject_unsupported_modes(&run(
        &repo,
        "review-worktree-modes",
        &[
            "diff",
            "--raw",
            "-z",
            "--no-renames",
            "--ignore-submodules=none",
            &baseline.commit,
            "--",
        ],
        None,
    )?)?;
    let index = TemporaryIndex(
        ctx.paths
            .artifacts_for(id)
            .join(jean_protocol::ipc::new_task_id()),
    );
    let path = Some(index.0.as_path());
    run(
        &repo,
        "review-read-tree",
        &["read-tree", &baseline.commit],
        path,
    )?;
    run(&repo, "review-stage", &["add", "-A", "--", "."], path)?;
    let tree = run(&repo, "review-write-tree", &["write-tree"], path)?;
    oid(String::from_utf8_lossy(&tree).trim().to_owned())
}

pub(super) fn publish(
    ctx: &Context,
    id: &str,
    source: &Path,
    baseline: &Baseline,
    verified_tree: &str,
) -> Result<bool> {
    if snapshot(ctx, id, source, baseline)? != verified_tree {
        return Err(error("workspace files changed during verification"));
    }
    let git = git(ctx)?;
    let repo = repo(ctx, &git, id, source);
    let raw = run(
        &repo,
        "review-modes",
        &[
            "diff",
            "--raw",
            "-z",
            "--no-renames",
            &baseline.commit,
            verified_tree,
            "--",
        ],
        None,
    )?;
    reject_unsupported_modes(&raw)?;
    let patch = run(
        &repo,
        "review-diff",
        &[
            "diff",
            "--binary",
            "--full-index",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            &baseline.commit,
            verified_tree,
            "--",
        ],
        None,
    )?;
    if patch.is_empty() {
        return Ok(false);
    }
    let dir = ctx.paths.artifacts_for(id);
    let review = Review {
        baseline: baseline.clone(),
        tree: verified_tree.into(),
        sha256: digest(&patch),
    };
    write_new(&dir.join("verified.patch"), &patch)?;
    write_json(&dir.join("review.json"), &review)?;
    Ok(true)
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn reject_unsupported_modes(raw: &[u8]) -> Result<()> {
    if raw.split(|byte| *byte == 0).step_by(2).any(|entry| {
        entry
            .split(|byte| *byte == b' ')
            .take(2)
            .any(|mode| [b":160000".as_slice(), b"160000", b":120000", b"120000"].contains(&mode))
    }) {
        return Err(error(
            "review does not support changed symlinks or submodules",
        ));
    }
    Ok(())
}

fn state(dir: &Path) -> Result<&'static str> {
    for (file, label) in [
        ("review-applied", "applied"),
        ("review-discarded", "discarded"),
        ("review-applying", "needs_attention"),
    ] {
        if dir.join(file).try_exists().map_err(io)? {
            return Ok(label);
        }
    }
    Ok("pending")
}

fn load(ctx: &Context, id: &str) -> Result<(PathBuf, Review, Vec<u8>)> {
    let task = ctx.store.require_task(id)?;
    if task.state != jean_protocol::TaskState::Succeeded {
        return Err(error("review requires a successfully verified delegation"));
    }
    if !ctx
        .store
        .delegations_for_task(id)?
        .iter()
        .any(|row| row.verification_verdict.as_deref() == Some("proven"))
    {
        return Err(error(
            "no independent verification evidence was recorded for this patch",
        ));
    }
    let dir = ctx.paths.artifacts_for(id);
    let review: Review = read_json(&dir.join("review.json"))?;
    let patch = read(&dir.join("verified.patch"), ctx.output_cap())?;
    if digest(&patch) != review.sha256 {
        return Err(error("the saved patch no longer matches the review record"));
    }
    Ok((dir, review, patch))
}

pub fn preview(ctx: &Context, id: &str) -> Result<Preview> {
    let _lock = task_lock(ctx, id)?;
    let (dir, review, patch) = load(ctx, id)?;
    Ok(Preview {
        workspace: review.baseline.target,
        baseline: review.baseline.commit,
        sha256: review.sha256,
        state: state(&dir)?,
        patch,
    })
}

pub fn discard(ctx: &Context, id: &str) -> Result<()> {
    let _lock = task_lock(ctx, id)?;
    let (dir, _, _) = load(ctx, id)?;
    match state(&dir)? {
        "discarded" => return Ok(()),
        "pending" => {}
        other => return Err(error(format!("cannot discard a review in state {other}"))),
    }
    write_new(&dir.join("review-discarded"), b"discarded\n")?;
    ctx.store.append_event(
        id,
        &Event::note("the operator discarded the isolated patch"),
    )?;
    Ok(())
}

pub fn apply(ctx: &Context, id: &str, expected: &str, prompter: &mut dyn Prompter) -> Result<()> {
    let _lock = task_lock(ctx, id)?;
    let (dir, review, patch) = load(ctx, id)?;
    if expected != review.sha256 {
        return Err(error(
            "patch digest differs; inspect the exact patch with jean review",
        ));
    }
    match state(&dir)? {
        "applied" => return Ok(()),
        "pending" => {}
        other => {
            return Err(error(format!(
                "cannot apply a review in state {other}; inspect the target and index"
            )));
        }
    }
    let baseline = &review.baseline;
    if baseline.target.canonicalize().map_err(io)? != baseline.target {
        return Err(error(
            "the recorded target workspace now resolves to a different location",
        ));
    }
    let git = git(ctx)?;
    let repo = repo(ctx, &git, id, &baseline.target);
    let operation = Operation {
        kind: OperationKind::DelegatedAction,
        tool_id: Some("review.apply".into()),
        tier: PolicyTier::Normal,
        capabilities: vec!["bulk_file_change".into()],
        summary: format!(
            "apply verified patch {expected} to {}",
            baseline.target.display()
        ),
        scope: Scope::new(
            baseline.target.display().to_string(),
            vec![id.into(), expected.into()],
        ),
    };
    let audit = AuditWriter::new(
        ctx.paths.audit.clone(),
        ctx.config.policy.audit_enabled,
        ctx.redactor.clone(),
    );
    let decision = Policy::new(ctx.config.policy.clone())
        .decide(&operation, &ctx.store, prompter)
        .map_err(CoreError::Policy)?;
    record(
        &audit,
        &operation,
        &decision,
        &actor_identity(),
        &baseline.target,
    )?;
    if let Decision::Denied(reason) = decision {
        return Err(error(format!("patch application refused: {reason:?}")));
    }
    let _workspace_lock = workspace_lock(ctx, id, &baseline.target)?;
    let entries = run(&repo, "review-index-flags", &["ls-files", "-v", "-z"], None)?;
    if entries.split(|byte| *byte == 0).any(|entry| {
        entry
            .first()
            .is_some_and(|flag| *flag == b'S' || flag.is_ascii_lowercase())
    }) {
        return Err(error(
            "the target index has sparse or assume-unchanged entries; apply requires a fully visible worktree",
        ));
    }
    require_target_baseline(&repo, baseline)?;
    let input = dir.join(format!("apply-{}.patch", jean_protocol::ipc::new_task_id()));
    write_new(&input, &patch)?;
    let input_name = input.to_string_lossy();
    let apply_result = (|| -> Result<()> {
        // Reconstruct the candidate tree in a disposable index before the
        // operator's index or worktree is eligible for mutation.
        let scratch = TemporaryIndex(dir.join(jean_protocol::ipc::new_task_id()));
        let index = Some(scratch.0.as_path());
        run(
            &repo,
            "review-apply-baseline",
            &["read-tree", &baseline.commit],
            index,
        )?;
        run(
            &repo,
            "review-apply-candidate",
            &[
                "apply",
                "--cached",
                "--binary",
                "--whitespace=nowarn",
                "--",
                &input_name,
            ],
            index,
        )?;
        let candidate = run(&repo, "review-candidate-tree", &["write-tree"], index)?;
        if String::from_utf8_lossy(&candidate).trim() != review.tree {
            return Err(error(
                "the saved patch does not reconstruct the independently verified tree",
            ));
        }
        run(
            &repo,
            "review-apply-check",
            &[
                "apply",
                "--check",
                "--index",
                "--binary",
                "--whitespace=nowarn",
                "--",
                &input_name,
            ],
            None,
        )?;
        require_target_baseline(&repo, baseline)?;
        if digest(&read(&input, ctx.output_cap())?) != expected {
            return Err(error("the application input changed after validation"));
        }
        if crate::signals::interrupt().is_some() {
            return Err(error("application cancelled before writing the target"));
        }
        write_new(&dir.join("review-applying"), expected.as_bytes())?;
        run(
            &repo,
            "review-apply",
            &[
                "apply",
                "--index",
                "--binary",
                "--whitespace=nowarn",
                "--",
                &input_name,
            ],
            None,
        )?;
        let applied = text(&repo, "review-applied-tree", &["write-tree"])?;
        run(
            &repo,
            "review-applied-worktree",
            &[
                "diff",
                "--quiet",
                "--no-ext-diff",
                "--ignore-submodules=none",
                "--",
            ],
            None,
        )?;
        if applied != review.tree
            || head(&repo)? != baseline.commit
            || text(
                &repo,
                "review-applied-ref",
                &["rev-parse", "--symbolic-full-name", "HEAD"],
            )? != baseline.target_ref
        {
            return Err(error(
                "the target or index changed during application; inspect it before retrying",
            ));
        }
        write_new(&dir.join("review-applied"), expected.as_bytes())?;
        ctx.store.append_event(
            id,
            &Event::new(
                EventKind::Note,
                "the operator applied the verified patch to the working tree and index",
            ),
        )?;
        Ok(())
    })();
    let _ = fs::remove_file(input);
    apply_result
}
