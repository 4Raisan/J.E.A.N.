//! The capture store: who owns a kept frame, when it dies, and what bounds the pile.
//!
//! This module exists because `[desktop] retention = "keep_24h"` used to be a sentence about the future
//! that nothing in the build had the power to keep. Frames were written, a note said they were marked for
//! a 24-hour life, and no code removed them. A settings file that promises expiry the code does not perform
//! is worse than a settings file with no such option, so the option now comes with the mechanism, and if
//! the mechanism cannot run safely the setting is refused instead of quietly becoming `keep_until_manual`.
//!
//! ## Why there is a manifest, and why nothing else is ever deleted
//!
//! The dangerous version of a cleaner is the obvious one: walk the capture directory, match
//! `jean-*-*.bmp`, compare the name's embedded timestamp with the clock, delete what is old. That design
//! deletes files it did not create. Somebody else's screenshot, a file an operator copied in to keep, a
//! name that collides with the pattern, and an artifact this build wrote into a directory the operator has
//! since repurposed all look identical to it from the outside.
//!
//! So ownership is *recorded*, not inferred. [`claim`] appends one line to `jean-captures.jsonl` in the
//! capture root before the frame is written, and [`reconcile`] will only ever consider a file that the
//! manifest names. Every other file in that directory is counted and reported as unowned, and is otherwise
//! left alone forever. A frame whose claim could not be written is not written at all, which is the
//! ordering that makes "the manifest is the authority" true rather than merely stated.
//!
//! ## The refusals, which are the point rather than the fine print
//!
//! Each of these stops a deletion and says which one stopped it:
//!
//! * the capture root **is** a reparse point — a junction or symlink to somewhere else, where "delete the
//!   old frames" would mean "delete somebody's other directory";
//! * the file named by a manifest line is itself a reparse point;
//! * the name is not a bare file name (`..\`, an absolute path, `a/b`);
//! * the resolved path is not inside the canonical capture root;
//! * the manifest line does not parse — an unreadable claim proves nothing about ownership, and the
//!   conservative reading of an uncertain claim is that the file stays.
//!
//! ## What the two keeping policies promise, and what this delivers
//!
//! `keep_24h` expires at [`KEEP_HORIZON_MS`] and the kept set is held under [`MAX_STORE_BYTES`] by
//! removing the oldest `keep_24h` artifacts first. `keep_until_manual` expires nothing and is *excluded
//! from the budget pruning*, because "until the operator removes them" is the promise that setting makes
//! and a budget is not a licence to break it; when the manually-kept set alone exceeds the ceiling, the
//! sweep reports that rather than deleting. `discard_after_task` needs none of this: nothing is ever
//! written, and it is enforced in `desktop` rather than here.
//!
//! ## The one privacy sentence that belongs in the code
//!
//! Retained pixels are not redacted material. The text paths in this repository redact secrets before they
//! reach a log, a database, or a model request; a retained BMP is the screen as it looked, including
//! whatever was on it — a token in a password manager, an email nobody agreed to publish, the operator's
//! own desktop. `keep_24h` and `keep_until_manual` therefore both mean "this content now sits on disk in
//! the clear", and the note the CLI prints says so instead of leaving it to be inferred.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// The manifest's name, always directly inside the capture root.
pub const MANIFEST_NAME: &str = "jean-captures.jsonl";

/// How long a `keep_24h` artifact lives, measured from the moment its claim was written.
pub const KEEP_HORIZON_MS: u64 = 24 * 60 * 60 * 1_000;

/// The ceiling on the `keep_24h` set. Five hundred and twelve megabytes is roughly a thousand frames of a
/// 1080p window, which is more capture than a day of ordinary desktop work produces and small enough that
/// nobody discovers this limit by filling a drive.
pub const MAX_STORE_BYTES: u64 = 512 * 1_048_576;

/// The retention setting that expires.
pub const RETENTION_24H: &str = "keep_24h";

/// The retention setting that never expires on its own.
pub const RETENTION_MANUAL: &str = "keep_until_manual";

/// `FILE_ATTRIBUTE_REPARSE_POINT`: the bit that says "this entry is a door, not a room".
///
/// Measured, not remembered. The first version of this constant was `0x1000`, which is
/// `FILE_ATTRIBUTE_OFFLINE`, and every junction this module looked at reported itself as an ordinary
/// non-directory — a refusal with the wrong reason, which is worse than no refusal because it sends an
/// operator to look for a broken directory instead of the link that is there. A probe of a real `mklink /J`
/// returned `0x410`: `0x10` for the directory and `0x400` for the reparse point.
const REPARSE_ATTRIBUTE: u32 = 0x0000_0400;

/// A frame this build wrote, as the manifest records it.
///
/// `written_at_ms` is milliseconds since the Unix epoch — the same clock `jean_store` orders records by —
/// and not the mechanism's uptime counter, because an expiry has to survive a reboot and an uptime figure
/// does not mean anything across one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The bare file name, with no directory in it. A path here is a bug or an attack, and
    /// [`Entry::has_safe_name`] is what tells them apart from a legitimate claim.
    pub file: String,
    /// The window the pixels came from, kept because a report that says "three frames expired" should be
    /// able to say whose.
    pub window: isize,
    /// The mechanism's own uptime stamp for the frame, carried for traceability only.
    pub captured_at_ms: u64,
    pub written_at_ms: u64,
    /// Which policy asked for this artifact, so budget pruning can honour the promise of the other one.
    pub retention: String,
}

impl Entry {
    /// Whether this name can be joined to the root and still describe a file inside the root.
    ///
    /// `Path::file_name` is the whole test, and it is the right one: it rejects `..`, `a/b`, `C:\elsewhere`,
    /// an empty name and a trailing separator, and it accepts exactly the `jean-<hwnd>-<ms>.bmp` shape this
    /// build writes without restating that format and then disagreeing with itself.
    pub fn has_safe_name(&self) -> bool {
        Path::new(&self.file)
            .file_name()
            .and_then(|name| name.to_str())
            == Some(self.file.as_str())
    }
}

/// What the capture root turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Root {
    /// A real directory, not a reparse point, safe to delete inside of.
    Valid(PathBuf),
    /// A junction, symlink or mount point. Deleting through it would act on a directory nobody chose.
    Reparse(PathBuf),
    /// Not there. Nothing to expire, and nothing to create either — a sweep never builds the root.
    Missing(PathBuf),
    /// Something else — a file where a directory was configured.
    NotADirectory(PathBuf),
    /// The stat failed for a reason with no better name, which is reported rather than assumed benign.
    Unreadable { path: PathBuf, reason: String },
}

impl Root {
    /// Whether deletions may proceed under this path at all.
    pub fn is_writable_root(&self) -> bool {
        matches!(self, Self::Valid(_))
    }

    /// The sentence a report prints about it.
    pub fn line(&self) -> String {
        match self {
            Self::Valid(path) => format!("the capture root {} is a directory this build owns entries in", path.display()),
            Self::Reparse(path) => format!(
                "{} is a reparse point (a junction, symlink or mount point); expiry is refused because \
                 deleting through it would act on the directory it points at, not on this one",
                path.display()
            ),
            Self::Missing(path) => {
                format!("{} does not exist, so there is nothing to expire", path.display())
            }
            Self::NotADirectory(path) => {
                format!("{} is not a directory; nothing was deleted", path.display())
            }
            Self::Unreadable { path, reason } => format!(
                "{} could not be inspected ({reason}); nothing was deleted, because an unknown root is not \
                 a safe root",
                path.display()
            ),
        }
    }
}

/// One file the sweep declined to delete, and the reason that stopped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub file: String,
    pub reason: &'static str,
}

/// What one reconciliation did and refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sweep {
    pub root: Option<Root>,
    /// Expired `keep_24h` artifacts deleted.
    pub expired: Vec<String>,
    /// Oldest `keep_24h` artifacts deleted to hold the kept set under the byte ceiling.
    pub pruned: Vec<String>,
    /// Manifest claims whose file was already gone. The claim is dropped, which is the only action
    /// available for it, and the count is reported rather than hidden because a claim with no frame
    /// behind it is either a crash between the two writes or somebody else removing this build's evidence.
    pub stale_claims: usize,
    /// Manifest lines that could not be parsed. They authorise nothing and are dropped when the record is
    /// rewritten; a truncated or hand-edited manifest is a thing an operator should hear about.
    pub unreadable_claims: usize,
    /// Still-owned frames and their total size.
    pub kept: usize,
    pub bytes_kept: u64,
    /// Files in the root that no manifest line claims. Counted, reported, and never touched.
    pub unowned: Vec<String>,
    pub refused: Vec<Refusal>,
    /// Why the manifest could not be rewritten, if it could not be. The deletions that already happened
    /// are not undone by it — they were authorised by the claims they came from — but the report has to
    /// say that the record of what is left is now the old one.
    pub manifest_error: Option<String>,
}

impl Sweep {
    /// The line a status view prints, from a report that deleted nothing.
    ///
    /// Deliberately a second wording rather than the same one: [`Sweep::line`] is about what was removed,
    /// and a status view has to answer "what is in there now, and what is due" without implying that
    /// looking was the same thing as cleaning.
    pub fn overview_line(&self) -> String {
        match &self.root {
            Some(root) if !root.is_writable_root() => root.line(),
            _ => format!(
                "{} owned frame(s) totalling {} bytes, {} due to expire now, {} file(s) in the directory \
                 that this build does not own and will not delete{}",
                self.kept,
                self.bytes_kept,
                self.expired.len(),
                self.unowned.len(),
                if self.refused.is_empty() {
                    String::new()
                } else {
                    format!(", {} claim(s) refused", self.refused.len())
                }
            ),
        }
    }

    /// Whether anything was deleted.
    pub fn deleted_any(&self) -> bool {
        !self.expired.is_empty() || !self.pruned.is_empty()
    }

    /// The line a step list or a status view prints. One sentence per thing an operator would act on, and
    /// it names the refusals rather than folding them into a count.
    pub fn line(&self) -> String {
        let mut text = match &self.root {
            Some(root) if !root.is_writable_root() => format!("capture store: {}", root.line()),
            _ => format!(
                "capture store: {} expired, {} pruned for the byte ceiling, {} kept ({} bytes)",
                self.expired.len(),
                self.pruned.len(),
                self.kept,
                self.bytes_kept
            ),
        };
        if self.stale_claims > 0 {
            text.push_str(&format!(
                "; {} claims dropped for a file that was already gone",
                self.stale_claims
            ));
        }
        if self.unreadable_claims > 0 {
            text.push_str(&format!(
                "; {} manifest lines could not be read and were dropped — they claim nothing, so nothing                  was deleted on their account",
                self.unreadable_claims
            ));
        }
        if !self.unowned.is_empty() {
            text.push_str(&format!(
                "; {} files in the directory are not this build's to delete and were left alone",
                self.unowned.len()
            ));
        }
        if !self.refused.is_empty() {
            let reasons: Vec<String> = self
                .refused
                .iter()
                .map(|refusal| format!("{} ({})", refusal.file, refusal.reason))
                .collect();
            text.push_str(&format!("; refused to delete {}", reasons.join(", ")));
        }
        if let Some(error) = &self.manifest_error {
            text.push_str(&format!("; the manifest could not be rewritten: {error}"));
        }
        text
    }
}

/// Is this path a directory this build may delete *entries* in?
///
/// `symlink_metadata` rather than `metadata`: the difference between the two is whether a reparse point is
/// followed, and following the operator's configured root to some other volume is exactly the failure this
/// module is written to avoid.
pub fn inspect_root(dir: &Path) -> Root {
    use std::os::windows::fs::MetadataExt;
    match std::fs::symlink_metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Root::Missing(dir.to_path_buf())
        }
        Err(error) => Root::Unreadable {
            path: dir.to_path_buf(),
            reason: error.to_string(),
        },
        Ok(meta) => {
            if meta.file_attributes() & REPARSE_ATTRIBUTE != 0 {
                return Root::Reparse(dir.to_path_buf());
            }
            if !meta.is_dir() {
                return Root::NotADirectory(dir.to_path_buf());
            }
            Root::Valid(dir.to_path_buf())
        }
    }
}

/// Whether a path is a file the sweep may act on, or the reason it is not.
enum Candidate {
    /// A real file, inside the root, claimed by this build: the only case in which a
    /// deletion is permitted.
    Deletable(PathBuf),
    /// The claim is sound and the frame is already gone, so there is nothing to remove and the
    /// record should stop describing it.
    AlreadyGone,
    /// Something about this claim does not authorise touching the name it carries.
    Refused(&'static str),
}

/// Decide, without touching anything, whether one manifest claim authorises a deletion.
fn classify(root: &Path, entry: &Entry) -> Candidate {
    use std::os::windows::fs::MetadataExt;
    if !entry.has_safe_name() {
        return Candidate::Refused("the claim names a path, not a file in the capture root");
    }
    let path = root.join(&entry.file);
    let meta = match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Candidate::AlreadyGone
        }
        Err(_) => {
            return Candidate::Refused(
                "the file could not be inspected, and an unknown file is not a deleted file",
            )
        }
        Ok(meta) => meta,
    };
    if meta.file_attributes() & REPARSE_ATTRIBUTE != 0 {
        return Candidate::Refused("the file is a reparse point");
    }
    if !meta.is_file() {
        return Candidate::Refused("the claim points at something that is not a file");
    }
    // The name test above already makes containment arithmetic, but arithmetic is not the point of this
    // check: canonicalising both sides catches a root that moved underneath the claim — a directory renamed
    // or replaced between the claim and the sweep — which no amount of name hygiene would notice.
    match (path.parent().map(Path::canonicalize), root.canonicalize()) {
        (Some(Ok(parent)), Ok(rooted)) if parent == rooted => Candidate::Deletable(path),
        (Some(Ok(_)), Ok(_)) => {
            Candidate::Refused("the file resolves outside the canonical capture root")
        }
        // Both sides fail for the same reason on a root that cannot be canonicalised, which is a root the
        // caller has already been told about; anything weirder is reported as the refusal it is.
        (Some(Err(_)), _) | (_, Err(_)) => Candidate::Refused(
            "the capture root could not be canonicalised, so containment is unknown",
        ),
        (None, _) => Candidate::Refused("the path has no parent to compare"),
    }
}

/// How many bytes a file holds, or 0 when it cannot be asked.
fn size_of(path: &Path) -> u64 {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.len())
        .unwrap_or(0)
}

/// Record that this build is about to write this frame.
///
/// Appends before the caller writes the pixels, so there is no instant at which a frame exists with no
/// claim on it. The reverse order is the one that produces an unowned file in a directory the cleaner is
/// allowed to delete in — and an unowned file is one this module must never touch, which means it would
/// live forever under a setting that says twenty-four hours.
pub fn claim(dir: &Path, entry: &Entry) -> std::io::Result<()> {
    if !entry.has_safe_name() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{:?} is not a file name, so it cannot be claimed in the capture root",
                entry.file
            ),
        ));
    }
    let manifest = dir.join(MANIFEST_NAME);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&manifest)?;
    let line = serde_json::to_string(entry).map_err(|error| {
        std::io::Error::other(format!("the claim could not be written: {error}"))
    })?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()
}

/// The claims of one manifest, and the lines that could not be read.
///
/// A line that does not parse is neither silently dropped nor fatal. It arrives as a name in
/// [`Claims::unreadable`], which the sweep reports as a refusal and the rewrite leaves out: an unreadable
/// line cannot claim anything, so it can authorise no deletion, and it is not an `Entry` pretending to be
/// one. Separate fields are what keep "I could not read this" from being confused with "this one is safe
/// to remove" — the first is a fact about the record, the second a fact about a file.
#[derive(Debug, Default)]
struct Claims {
    entries: Vec<Entry>,
    unreadable: Vec<String>,
}

fn read_manifest(dir: &Path) -> Claims {
    let manifest = dir.join(MANIFEST_NAME);
    let text = match std::fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Claims::default(),
        Err(error) => {
            return Claims {
                entries: Vec::new(),
                unreadable: vec![format!("the manifest itself could not be read: {error}")],
            }
        }
    };
    let mut claims = Claims::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(entry) => claims.entries.push(entry),
            Err(_) => {
                tracing::debug!("a capture manifest line could not be read: {line}");
                claims.unreadable.push(line.to_owned());
            }
        }
    }
    claims
}

/// Expire what is due, hold the kept set under the ceiling, and report everything else that was found.
///
/// `now_ms` is a parameter for the same reason any bound that is tested is a parameter: waiting a day to
/// test a day-old expiry is not a test.
pub fn reconcile(dir: &Path, now_ms: u64, horizon_ms: u64, budget_bytes: u64) -> Sweep {
    let mut sweep = Sweep::default();
    let root = inspect_root(dir);
    sweep.root = Some(root.clone());
    if !root.is_writable_root() {
        return sweep;
    }
    let claims = read_manifest(dir);
    let entries = claims.entries;
    sweep.unreadable_claims = claims.unreadable.len();
    for (index, line) in claims.unreadable.iter().enumerate() {
        // Reported as a refusal under a name that cannot be joined to anything, because the honest
        // statement about an unparseable line is "this proved nothing" — not "this file is safe to remove"
        // and not "this file belongs to somebody else, leave it out of the report".
        let _ = line;
        sweep.refused.push(Refusal {
            file: format!("<unreadable manifest claim {}>", index + 1),
            reason: "an unreadable claim authorises nothing",
        });
    }

    // Pass one: sort every claim into deleted-already, refused, and live.
    struct Live {
        entry: Entry,
        path: PathBuf,
        bytes: u64,
    }
    let mut live: Vec<Live> = Vec::new();
    let mut survivors: Vec<Entry> = Vec::new();
    for entry in &entries {
        match classify(dir, entry) {
            Candidate::Deletable(path) => live.push(Live {
                bytes: size_of(&path),
                entry: entry.clone(),
                path,
            }),
            Candidate::AlreadyGone => {
                // Nothing to delete, and nothing to keep claiming: a claim for a file that is not there can
                // only ever have meant a frame that was already removed by something other than this build.
                sweep.stale_claims += 1;
            }
            Candidate::Refused(reason) => {
                sweep.refused.push(Refusal {
                    file: entry.file.clone(),
                    reason,
                });
                survivors.push(entry.clone());
            }
        }
    }

    // Pass two: expiry. Only `keep_24h` claims expire; the manual policy made a different promise and this
    // module does not get to revise it.
    live.sort_by_key(|item| item.entry.written_at_ms);
    let mut kept: Vec<Live> = Vec::new();
    for item in live {
        let expires = item.entry.retention == RETENTION_24H
            && item.entry.written_at_ms.saturating_add(horizon_ms) <= now_ms;
        if expires {
            match std::fs::remove_file(&item.path) {
                Ok(()) => sweep.expired.push(item.entry.file),
                Err(error) => {
                    // The claim stands and so does the file: a deletion that failed is reported as a
                    // refusal, never as an expiry that happened.
                    sweep.refused.push(Refusal {
                        file: item.entry.file.clone(),
                        reason: "the file is claimed and expired but Windows would not remove it",
                    });
                    tracing::debug!(%error, "an expired capture could not be removed");
                    kept.push(item);
                }
            }
        } else {
            kept.push(item);
        }
    }

    // Pass three: the byte ceiling, oldest first, and never a manually-kept artifact.
    let mut bytes: u64 = kept.iter().map(|item| item.bytes).sum();
    let mut index = 0usize;
    while bytes > budget_bytes && index < kept.len() {
        if kept[index].entry.retention != RETENTION_24H {
            index += 1;
            continue;
        }
        match std::fs::remove_file(&kept[index].path) {
            Ok(()) => {
                bytes -= kept[index].bytes;
                sweep.pruned.push(kept[index].entry.file.clone());
                kept.remove(index);
            }
            Err(_) => {
                sweep.refused.push(Refusal {
                    file: kept[index].entry.file.clone(),
                    reason:
                        "the file is claimed and over the byte ceiling but would not be removed",
                });
                index += 1;
            }
        }
    }

    sweep.kept = kept.len();
    sweep.bytes_kept = bytes;
    survivors.extend(kept.into_iter().map(|item| item.entry));

    // Everything in the directory that the record does not name. Reported, and left exactly where it is.
    let owned: std::collections::BTreeSet<&str> =
        survivors.iter().map(|entry| entry.file.as_str()).collect();
    if let Ok(read) = std::fs::read_dir(dir) {
        for dirent in read.flatten() {
            let name = dirent.file_name().to_string_lossy().into_owned();
            if name == MANIFEST_NAME || owned.contains(name.as_str()) {
                continue;
            }
            let Ok(meta) = dirent.metadata() else {
                continue;
            };
            if meta.is_file() {
                sweep.unowned.push(name);
            }
        }
    }

    let record_changed = survivors.len() != entries.len() || !claims.unreadable.is_empty();
    if record_changed {
        if let Err(error) = write_manifest(dir, &survivors) {
            sweep.manifest_error = Some(error.to_string());
        }
    }
    sweep
}

/// Rewrite the record so it names exactly what is still owned.
///
/// Written to a sibling and renamed over: a manifest truncated by a crash mid-write would turn every
/// unreadable line into a refusal and every real claim into nothing, which is a loss of ownership rather
/// than a loss of one frame.
fn write_manifest(dir: &Path, entries: &[Entry]) -> std::io::Result<()> {
    let temp = dir.join(format!("{MANIFEST_NAME}.{}.tmp", std::process::id()));
    let mut body = String::new();
    for entry in entries {
        body.push_str(
            &serde_json::to_string(entry)
                .map_err(|error| std::io::Error::other(error.to_string()))?,
        );
        body.push('\n');
    }
    std::fs::write(&temp, body)?;
    match std::fs::rename(&temp, dir.join(MANIFEST_NAME)) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

/// What the store holds right now, computed without deleting anything.
///
/// The status view needs this: a read path that prunes the operator's evidence is the asymmetry every
/// other module in this crate keeps, and "how much is due in an hour" is a question a diagnostic should be
/// able to ask.
pub fn state(dir: &Path, now_ms: u64, horizon_ms: u64) -> Sweep {
    let mut view = Sweep::default();
    let root = inspect_root(dir);
    view.root = Some(root.clone());
    if !root.is_writable_root() {
        return view;
    }
    let claims = read_manifest(dir);
    let entries = claims.entries;
    view.unreadable_claims = claims.unreadable.len();
    let mut owned: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in &entries {
        match classify(dir, entry) {
            Candidate::Deletable(path) => {
                owned.insert(entry.file.clone());
                let due = entry.retention == RETENTION_24H
                    && entry.written_at_ms.saturating_add(horizon_ms) <= now_ms;
                if due {
                    view.expired.push(entry.file.clone());
                } else {
                    view.kept += 1;
                    view.bytes_kept = view.bytes_kept.saturating_add(size_of(&path));
                }
            }
            Candidate::AlreadyGone => view.stale_claims += 1,
            Candidate::Refused(reason) => view.refused.push(Refusal {
                file: entry.file.clone(),
                reason,
            }),
        }
    }
    if let Ok(read) = std::fs::read_dir(dir) {
        for dirent in read.flatten() {
            let name = dirent.file_name().to_string_lossy().into_owned();
            if name == MANIFEST_NAME || owned.contains(&name) {
                continue;
            }
            if dirent
                .metadata()
                .map(|meta| meta.is_file())
                .unwrap_or(false)
            {
                view.unowned.push(name);
            }
        }
    }
    view
}

/// The clock the shipped paths use, named once so a test does not have to reach for a crate to fake it.
pub fn now_ms() -> u64 {
    jean_store::now_ms().max(0) as u64
}

static RECONCILED: AtomicU64 = AtomicU64::new(0);

/// Reconcile the store once per process, before anything new is written into it.
///
/// The `Once` is not a micro-optimisation: expiring on every capture would make the cost of keeping a frame
/// grow with everything already kept, and an operator watching a slow `jean desktop capture` would have no
/// way to know that the slowness was the cleaner. Once per run is what "startup reconciliation" means for
/// a command-line tool that runs one command per process, and the sweep result is logged so the run that
/// did delete something says so in the log it wrote.
pub fn reconcile_at_startup(dir: &Path, now_ms: u64) -> Option<Sweep> {
    if RECONCILED.swap(1, Ordering::SeqCst) == 1 {
        return None;
    }
    let sweep = reconcile(dir, now_ms, KEEP_HORIZON_MS, MAX_STORE_BYTES);
    if sweep.deleted_any() || !sweep.refused.is_empty() || sweep.stale_claims > 0 {
        tracing::info!("{}", sweep.line());
    }
    Some(sweep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    /// A directory this test owns, removed when the test ends.
    ///
    /// Every case here is about deletion, so every case runs in a directory it made. Sharing one temp
    /// folder between tests would mean a bug in one test deleting another one's evidence.
    struct Sandbox {
        dir: PathBuf,
    }

    impl Sandbox {
        fn new(tag: &str) -> Sandbox {
            let dir = std::env::temp_dir().join(format!(
                "jean-capture-store-{}-{tag}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).expect("a test directory");
            Sandbox { dir }
        }

        fn dir(&self) -> &Path {
            &self.dir
        }

        /// Write a frame and claim it, the way `keep_frame` does.
        fn keep(&self, window: isize, written_at_ms: u64, bytes: usize, retention: &str) -> String {
            let file = format!("jean-{window}-{written_at_ms}.bmp");
            let entry = Entry {
                file: file.clone(),
                window,
                captured_at_ms: written_at_ms,
                written_at_ms,
                retention: retention.to_owned(),
            };
            claim(self.dir(), &entry).expect("the claim is written");
            std::fs::write(self.dir().join(&file), vec![0u8; bytes]).expect("the frame is written");
            file
        }

        fn exists(&self, name: &str) -> bool {
            self.dir().join(name).exists()
        }

        fn entries(&self) -> Vec<Entry> {
            read_manifest(self.dir()).entries
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const DAY: u64 = KEEP_HORIZON_MS;
    const HOUR: u64 = 60 * 60 * 1_000;

    #[test]
    fn a_kept_frame_survives_until_its_day_is_up() {
        let box_ = Sandbox::new("expiry");
        let written = 1_000_000_000_000;
        let file = box_.keep(7, written, 4096, RETENTION_24H);

        // Twenty-three hours and fifty-nine minutes is not a day, and a cleaner that treats "about a day"
        // as the bound is a cleaner that deletes evidence early.
        let early = reconcile(box_.dir(), written + DAY - 60_000, DAY, MAX_STORE_BYTES);
        assert!(early.expired.is_empty(), "{:?}", early.expired);
        assert_eq!(early.kept, 1);
        assert!(box_.exists(&file));

        let due = reconcile(box_.dir(), written + DAY, DAY, MAX_STORE_BYTES);
        assert_eq!(
            due.expired,
            vec![file.clone()],
            "exactly the file that is due"
        );
        assert_eq!(due.kept, 0);
        assert!(!box_.exists(&file), "the expired frame is gone from disk");
        assert!(
            box_.entries().is_empty(),
            "the record no longer claims a file that is not there"
        );
    }

    #[test]
    fn a_file_the_manifest_does_not_name_is_never_touched() {
        let box_ = Sandbox::new("unowned");
        let old = 1_000_000_000_000;
        let mine = box_.keep(7, old, 1024, RETENTION_24H);
        // The two shapes a naive cleaner would delete: an identical name this build never claimed, and a
        // file the operator put there under a name that has nothing to do with captures at all.
        let lookalike = "jean-999-999999999999.bmp";
        let someone_elses = "holiday.bmp";
        std::fs::write(box_.dir().join(lookalike), [1u8; 1024]).expect("a planted lookalike");
        std::fs::write(box_.dir().join(someone_elses), [2u8; 1024]).expect("a planted file");

        let sweep = reconcile(box_.dir(), old + DAY, DAY, MAX_STORE_BYTES);
        assert_eq!(sweep.expired, vec![mine], "only the claimed frame expired");
        assert!(
            box_.exists(lookalike),
            "an unclaimed frame that looks owned is left alone"
        );
        assert!(
            box_.exists(someone_elses),
            "and so is anything else in the directory"
        );
        let unowned: Vec<&str> = sweep.unowned.iter().map(String::as_str).collect();
        assert!(
            unowned.contains(&lookalike) && unowned.contains(&someone_elses),
            "both are reported rather than silently kept: {:?}",
            sweep.unowned
        );
    }

    #[test]
    fn the_budget_prunes_the_oldest_kept_frame_and_never_a_manual_one() {
        let box_ = Sandbox::new("budget");
        let base = 1_000_000_000_000;
        // Nothing is due: every frame is an hour old. The only reason to remove one is the ceiling.
        let oldest = box_.keep(1, base, 8_192, RETENTION_24H);
        let middle = box_.keep(2, base + HOUR, 8_192, RETENTION_24H);
        let manual = box_.keep(3, base, 8_192, RETENTION_MANUAL);

        // 20 KiB of frames against a 16 KiB ceiling: the oldest `keep_24h` frame goes, and the
        // manually-kept one stays even though it is the same age and the same size.
        let sweep = reconcile(box_.dir(), base + HOUR, DAY, 16_384);
        assert_eq!(
            sweep.expired,
            Vec::<String>::new(),
            "nothing is past its horizon"
        );
        assert_eq!(
            sweep.pruned,
            vec![oldest],
            "the oldest keep_24h frame pays for the ceiling"
        );
        assert!(box_.exists(&middle) && box_.exists(&manual));
        assert!(
            sweep.bytes_kept <= 16_384,
            "the kept set is inside the ceiling it was given: {}",
            sweep.bytes_kept
        );
        assert!(
            !sweep.line().contains("refused"),
            "a budget pruning is a decision, not a refusal: {}",
            sweep.line()
        );
    }

    #[test]
    fn a_manual_set_alone_over_the_budget_is_reported_and_kept() {
        let box_ = Sandbox::new("manual-over-budget");
        let base = 1_000_000_000_000;
        let manual = box_.keep(1, base, 32_768, RETENTION_MANUAL);
        let sweep = reconcile(box_.dir(), base + HOUR, DAY, 1_024);
        assert!(
            sweep.pruned.is_empty(),
            "keep_until_manual is not this module's to prune"
        );
        assert!(box_.exists(&manual));
        assert_eq!(sweep.bytes_kept, 32_768);
        assert_eq!(
            sweep.kept, 1,
            "and the report says it is still there, which is the operator's business now"
        );
    }

    #[test]
    fn a_claim_that_names_a_path_proves_nothing_and_deletes_nothing() {
        let box_ = Sandbox::new("traversal");
        // The claim's file is written where a malicious or buggy claim would point: outside the root, in a
        // directory the sweep has no business touching.
        let outside = std::env::temp_dir().join(format!(
            "jean-capture-outside-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&outside).expect("a directory outside the root");
        let victim = "ancient.bmp";
        std::fs::write(outside.join(victim), [0u8; 512]).expect("a file outside the root");
        let relative = format!(
            "{}\\{victim}",
            outside.file_name().expect("a name").to_string_lossy()
        );
        let claim_of_a_path = Entry {
            // `..\\` out of the root, which is the shape the requirement names explicitly.
            file: format!("..\\{relative}"),
            window: 1,
            captured_at_ms: 0,
            written_at_ms: 1,
            retention: RETENTION_24H.to_owned(),
        };
        claim(box_.dir(), &claim_of_a_path)
            .expect_err("a path is not a file name, so it cannot be claimed at all");

        // A claim that got into the manifest by some other route still has to be refused at deletion time.
        let sneaked = Entry {
            file: format!("..\\{relative}"),
            ..claim_of_a_path
        };
        let mut line = serde_json::to_string(&sneaked).expect("serialisable");
        line.push('\n');
        std::fs::write(box_.dir().join(MANIFEST_NAME), line)
            .expect("a manifest with one bad claim");
        let sweep = reconcile(box_.dir(), DAY * 2, DAY, MAX_STORE_BYTES);
        assert!(sweep.expired.is_empty() && sweep.pruned.is_empty());
        assert_eq!(sweep.refused.len(), 1, "{:?}", sweep.refused);
        assert!(
            outside.join(victim).exists(),
            "the file the claim pointed at is exactly where it was"
        );
        assert!(
            sweep.refused[0].reason.contains("names a path, not a file"),
            "and the report says which rule stopped it: {:?}",
            sweep.refused[0].reason
        );
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn a_root_that_is_a_junction_is_refused_rather_than_followed() {
        let outer = Sandbox::new("junction-target");
        let base = 1_000_000_000_000;
        let victim = outer.keep(1, base, 4_096, RETENTION_24H);

        let parent = std::env::temp_dir().join(format!(
            "jean-capture-junction-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&parent).expect("a parent directory");
        let link = parent.join("captures");
        // A junction, not a symlink: it needs no developer mode and it is the shape that actually occurs
        // when somebody moves their capture directory to another drive and leaves the old path behind.
        let made = std::process::Command::new("cmd")
            .args([
                "/c",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &outer.dir().to_string_lossy(),
            ])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !made {
            eprintln!(
                "NOTE: this machine would not create a junction; the refusal path is untested here"
            );
            let _ = std::fs::remove_dir_all(&parent);
            return;
        }

        let sweep = reconcile(&link, base + DAY, DAY, MAX_STORE_BYTES);
        assert!(
            !sweep.deleted_any(),
            "a junction root is not a root: {:?}",
            sweep
        );
        assert!(
            matches!(sweep.root, Some(Root::Reparse(_))),
            "and it is named as what it is: {:?}",
            sweep.root
        );
        assert!(
            sweep
                .line()
                .contains("would act on the directory it points at"),
            "{}",
            sweep.line()
        );
        assert!(
            outer.exists(&victim),
            "the frames behind the junction are untouched"
        );

        // Removing the link removes the link only. This is the assertion that proves the test set up the
        // thing it claims to have tested.
        std::fs::remove_dir(&link).expect("the junction itself is removable");
        assert!(
            outer.exists(&victim),
            "and removing it did not take the target directory with it"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn a_claim_for_a_frame_that_is_not_there_drops_the_claim_and_deletes_nothing() {
        let box_ = Sandbox::new("gone-frame");
        let entry = Entry {
            file: "jean-5-5.bmp".to_owned(),
            window: 5,
            captured_at_ms: 5,
            written_at_ms: 1,
            retention: RETENTION_24H.to_owned(),
        };
        claim(box_.dir(), &entry).expect("the frame is claimed");
        // and then the frame disappears — an operator removed it, or a crash happened between the claim
        // and the write. Either way there is nothing to delete and the record has to stop claiming it.
        let sweep = reconcile(box_.dir(), DAY * 2, DAY, MAX_STORE_BYTES);
        assert_eq!(sweep.stale_claims, 1, "{sweep:?}");
        assert!(sweep.expired.is_empty() && sweep.pruned.is_empty());
        assert!(
            box_.entries().is_empty(),
            "and the claim itself is gone: {:?}",
            box_.entries()
        );
    }

    #[test]
    fn a_reparse_point_inside_the_root_is_refused_and_its_target_survives() {
        let box_ = Sandbox::new("in-root-reparse");
        // A real frame in a real directory somewhere else, and a claim for the name of a junction that
        // points at it. This is the shape that turns a naive cleaner into a data-loss bug: the name is
        // inside the root, the bytes are not.
        let target = Sandbox::new("reparse-target");
        let base = 1_000_000_000_000;
        let payload = target.keep(9, base, 4_096, RETENTION_MANUAL);

        let link = box_.dir().join("jean-9-9.bmp");
        let made = std::process::Command::new("cmd")
            .args([
                "/c",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &target.dir().to_string_lossy(),
            ])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !made {
            eprintln!(
                "NOTE: this machine would not create the junction this test needs; the in-root reparse \
                 refusal is untested here"
            );
            return;
        }
        let entry = Entry {
            file: "jean-9-9.bmp".to_owned(),
            window: 9,
            captured_at_ms: 9,
            written_at_ms: base,
            retention: RETENTION_24H.to_owned(),
        };
        claim(box_.dir(), &entry).expect("the claim is written");
        let sweep = reconcile(box_.dir(), base + DAY, DAY, MAX_STORE_BYTES);
        assert!(
            sweep.expired.is_empty() && sweep.pruned.is_empty(),
            "{sweep:?}"
        );
        assert_eq!(
            sweep.refused.len(),
            1,
            "the one thing in the root that was not deleted: {:?}",
            sweep.refused
        );
        assert!(
            sweep.refused[0].reason.contains("reparse point"),
            "named as the reparse point it is: {:?}",
            sweep.refused[0].reason
        );
        assert!(
            target.exists(&payload),
            "and the frame it pointed at is where its owner left it"
        );
    }

    #[test]
    fn an_unreadable_claim_is_refused_and_says_so() {
        let box_ = Sandbox::new("garbage-line");
        let base = 1_000_000_000_000;
        let mine = box_.keep(7, base, 1_024, RETENTION_24H);
        let existing = box_.dir().join(MANIFEST_NAME);
        let mut text = std::fs::read_to_string(&existing).expect("the manifest the claim wrote");
        text.push_str("this is not a claim at all\n");
        std::fs::write(&existing, text).expect("a manifest with a junk line");

        let sweep = reconcile(box_.dir(), base + DAY, DAY, MAX_STORE_BYTES);
        assert_eq!(
            sweep.expired,
            vec![mine],
            "the readable claim still expires"
        );
        assert_eq!(
            sweep.unreadable_claims, 1,
            "and the unreadable one is counted rather than ignored: {sweep:?}"
        );
        assert_eq!(sweep.stale_claims, 0, "a junk line is not a missing frame");
        assert!(
            sweep.line().contains("could not be read") || sweep.line().contains("unreadable"),
            "the report says a line was unreadable: {}",
            sweep.line()
        );
        assert!(
            sweep
                .refused
                .iter()
                .any(|refusal| refusal.reason.contains("authorises nothing")),
            "and it is counted as a refusal rather than a deletion: {:?}",
            sweep.refused
        );
        assert!(
            !box_
                .entries()
                .iter()
                .any(|entry| entry.file.contains("unreadable")),
            "and the rewritten record holds only real claims: {:?}",
            box_.entries()
        );
    }

    #[test]
    fn a_missing_root_is_reported_and_never_created_by_a_sweep() {
        let absent = std::env::temp_dir().join(format!(
            "jean-capture-absent-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        let sweep = reconcile(&absent, DAY * 2, DAY, MAX_STORE_BYTES);
        assert!(
            matches!(sweep.root, Some(Root::Missing(_))),
            "{:?}",
            sweep.root
        );
        assert!(!sweep.deleted_any());
        assert!(
            !absent.exists(),
            "a sweep reports a directory rather than making one behind the operator's back"
        );
        assert!(
            sweep.line().contains("nothing to expire"),
            "{}",
            sweep.line()
        );
    }

    #[test]
    fn a_claim_is_written_before_the_frame_exists() {
        let box_ = Sandbox::new("ordering");
        let entry = Entry {
            file: "jean-3-3.bmp".to_owned(),
            window: 3,
            captured_at_ms: 3,
            written_at_ms: 1,
            retention: RETENTION_24H.to_owned(),
        };
        claim(box_.dir(), &entry).expect("the claim is written");
        assert!(
            !box_.exists("jean-3-3.bmp"),
            "the claim precedes the pixels — that ordering is the whole of the ownership guarantee"
        );
        // A crash between the two leaves a claim with no file, which the next sweep drops without
        // inventing a deletion.
        let sweep = reconcile(box_.dir(), DAY * 2, DAY, MAX_STORE_BYTES);
        assert_eq!(sweep.stale_claims, 1, "{sweep:?}");
        assert!(sweep.expired.is_empty() && sweep.pruned.is_empty());
        assert!(
            box_.entries().is_empty(),
            "and the dangling claim is gone from the record"
        );
    }

    #[test]
    fn a_state_report_deletes_nothing() {
        let box_ = Sandbox::new("state");
        let base = 1_000_000_000_000;
        let mine = box_.keep(7, base, 1_024, RETENTION_24H);
        let view = state(box_.dir(), base + DAY * 2, DAY);
        assert_eq!(
            view.expired,
            vec![mine.clone()],
            "the report says what is due"
        );
        assert!(
            box_.exists(&mine),
            "and a read path does not remove it — that is `reconcile`'s job"
        );
    }

    #[test]
    fn the_manifest_is_rewritten_with_only_what_is_still_owned() {
        let box_ = Sandbox::new("rewrite");
        let base = 1_000_000_000_000;
        let due = box_.keep(1, base, 1_024, RETENTION_24H);
        let staying = box_.keep(2, base + HOUR, 1_024, RETENTION_24H);
        let sweep = reconcile(box_.dir(), base + DAY, DAY, MAX_STORE_BYTES);
        assert_eq!(sweep.expired, vec![due]);
        assert!(sweep.manifest_error.is_none(), "{:?}", sweep.manifest_error);
        let entries = box_.entries();
        assert_eq!(entries.len(), 1, "one claim left: {entries:?}");
        assert_eq!(entries[0].file, staying);
        assert_eq!(entries[0].retention, RETENTION_24H);
        assert!(
            !box_.exists(&format!("{MANIFEST_NAME}.{}", std::process::id())),
            "the rename left no temporary behind"
        );
    }

    #[test]
    fn a_name_is_only_a_name_when_it_cannot_escape() {
        let safe = |name: &str| Entry {
            file: name.to_owned(),
            window: 1,
            captured_at_ms: 0,
            written_at_ms: 0,
            retention: RETENTION_24H.to_owned(),
        };
        assert!(safe("jean-1-1.bmp").has_safe_name());
        for escape in [
            "..\\evil.bmp",
            "../evil.bmp",
            "C:\\Windows\\evil.bmp",
            "\\evil.bmp",
            "sub\\dir\\evil.bmp",
            "",
            ".",
            "..",
            "evil.bmp\\",
        ] {
            assert!(
                !safe(escape).has_safe_name(),
                "{escape:?} is not a bare file name and must not be claimable"
            );
        }
    }
}
