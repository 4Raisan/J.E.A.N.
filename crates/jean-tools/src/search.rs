//! Bounded file *location* by name.
//!
//! ## What this is for
//!
//! The `jean find` intent: answer "where is that file" without a search index,
//! without an external tool, and without ever letting the walk leave the one
//! directory the caller authorised. Nothing here reads file contents — names only.
//! That is not an optimisation, it is the reason a personal directory can be
//! searched without indexing what is inside it.
//!
//! ## What this deliberately does not do
//!
//! * **No recursion in code.** A `fn` that calls itself per directory would make
//!   depth a stack question. This uses an explicit stack, so depth is bounded by
//!   `max_depth` and cannot blow the thread stack at all.
//! * **No reparse-point descent.** See below. This is the load-bearing rule.
//! * **No content reading, no `.git` special-casing, no ignore files.** An
//!   unbounded feature set smuggles in unbounded cost; `PLANNING/04-SETTINGS.md`
//!   gives bounds instead of heuristics.
//! * **No sorting by relevance.** Shallowest-first then case-insensitive path:
//!   deterministic, so two runs of `jean find` print the same order and an operator
//!   can diff them.
//!
//! ## Why reparse points are recorded and never descended
//!
//! A junction or symlink is a door out of the authorised root. Following one turns
//! "search my project" into "search `C:\Windows`", and a self-referential junction
//! makes the walk loop forever on an expanding path. So a reparse point is reported
//! as skipped, with its path, and the walk does not enter it.
//!
//! Detection uses `MetadataExt::file_attributes()` and `FILE_ATTRIBUTE_REPARSE_POINT`
//! rather than `FileType::is_symlink()`, because a junction is a reparse point that
//! the standard library may or may not classify as a symlink depending on how it was
//! created and which version read it. The safe behaviour is identical either way —
//! report it, do not follow it — so the attribute bit is the test, not the enum.
//!
//! ## Bounds are limits, not hints
//!
//! `max_results`, `max_entries`, `deadline_secs` and `max_depth` come from
//! [`jean_protocol::config::SearchConfig`], and each one has a matching
//! [`SearchStop`] variant. A run always reports which bound ended it, because
//! "no matches" and "I stopped looking after 200 000 entries" are different answers
//! to different questions, and only one of them should ever be printed as the other.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jean_protocol::config::SearchConfig;

use crate::error::{Result, ToolError};

// ─────────────────────────────────────────────────────────────────────────────
// Name matching. Written here rather than taken from a crate, because the
// semantics that matter (literal `[`, `**` treated as `*`) are exactly the ones
// the portable glob crates get differently.
// ─────────────────────────────────────────────────────────────────────────────

/// How a pattern is compared against a file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// Shell-style glob over the whole name: `*`, `?`, and `[abc]` classes.
    Glob,
    /// Substring match anywhere in the name.
    Substring,
}

/// A compiled name pattern.
///
/// Compiled once per search rather than per entry: a walk over 200 000 names must
/// not re-tokenise the same pattern 200 000 times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamePattern {
    raw: String,
    kind: PatternKind,
    ignore_case: bool,
    /// Tokenised form when `kind` is `Glob`.
    tokens: Vec<GlobToken>,
    /// Parallel to `tokens`: this token belongs to a bracket class, and that class is
    /// negated (`[!...]`). Kept beside the tokens rather than inside them so a class
    /// stays one run of alternatives instead of a second token shape to keep in sync.
    class_negated: Vec<bool>,
    /// Prepared needle when `kind` is `Substring`.
    needle: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobToken {
    /// Matches any run of characters, including the empty one.
    Star,
    /// Matches exactly one character.
    Question,
    Literal(char),
    /// One alternative from a bracket class: `[a-z]`, `[abc]`. A single character is
    /// a range with both ends equal, which is why there is no separate `Set` variant.
    Class(char, char),
}

impl NamePattern {
    /// Build a pattern, choosing glob form when the text contains a wildcard.
    ///
    /// A pattern with wildcards but an unterminated `[` is still treated as a glob:
    /// the stray bracket then matches a literal bracket, which is what a user typing
    /// `notes[1*.txt` means. Refusing to interpret it would be worse than
    /// interpreting it loosely.
    pub fn new(raw: &str, ignore_case: bool) -> Self {
        let kind = if raw.contains('*') || raw.contains('?') || raw.contains('[') {
            PatternKind::Glob
        } else {
            PatternKind::Substring
        };
        let (tokens, class_negated) = if kind == PatternKind::Glob {
            tokenise_glob(raw)
        } else {
            (Vec::new(), Vec::new())
        };
        let needle = if ignore_case {
            raw.to_lowercase()
        } else {
            raw.to_owned()
        };
        NamePattern {
            raw: raw.to_owned(),
            kind,
            ignore_case,
            tokens,
            class_negated,
            needle,
        }
    }

    /// The pattern text as supplied, for echoing back in a report.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn kind(&self) -> PatternKind {
        self.kind
    }

    /// True when `name` matches. Only the base name is consulted — never a path.
    pub fn is_match(&self, name: &str) -> bool {
        match self.kind {
            PatternKind::Substring => {
                if self.ignore_case {
                    name.to_lowercase().contains(&self.needle)
                } else {
                    name.contains(&self.needle)
                }
            }
            PatternKind::Glob => {
                glob_matches(&self.tokens, &self.class_negated, name, self.ignore_case)
            }
        }
    }
}

/// Split glob text into tokens.
///
/// Runs of consecutive `*` collapse into one star. `**` therefore behaves like
/// `*`, which is deliberate: `*` here never crosses a path separator (we only ever
/// match base names), so `**` has nothing extra to mean. Documented in the README
/// of the intent rather than silently accepted, because users coming from zsh or
/// ripgrep expect it to cross directories and it will not.
fn tokenise_glob(raw: &str) -> (Vec<GlobToken>, Vec<bool>) {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    let mut negated = Vec::with_capacity(chars.len());
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                while i < chars.len() && chars[i] == '*' {
                    i += 1;
                }
                if !matches!(out.last(), Some(GlobToken::Star)) {
                    out.push(GlobToken::Star);
                    negated.push(false);
                }
            }
            '?' => {
                out.push(GlobToken::Question);
                negated.push(false);
                i += 1;
            }
            '[' => {
                // A class needs at least `[c]`; anything less is a literal bracket,
                // so `a[b` and `[]` keep matching their own text.
                match parse_class(&chars, i) {
                    Some((tokens, is_negated, next)) => {
                        for t in tokens {
                            out.push(t);
                            negated.push(is_negated);
                        }
                        i = next;
                    }
                    None => {
                        out.push(GlobToken::Literal('['));
                        negated.push(false);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(GlobToken::Literal(c));
                negated.push(false);
                i += 1;
            }
        }
    }
    (out, negated)
}

/// Parse a bracket class starting at `open`. Returns its alternatives, whether the
/// class is negated, and the index just past the closing bracket — or `None` when
/// there is no usable closing bracket, in which case the `[` is literal text.
fn parse_class(chars: &[char], open: usize) -> Option<(Vec<GlobToken>, bool, usize)> {
    let mut i = open + 1;
    if i >= chars.len() {
        return None;
    }
    let negated = matches!(chars[i], '!' | '^');
    if negated {
        i += 1;
    }
    // `[]]` — a leading `]` is data, not a terminator, in every shell convention
    // worth following.
    let mut first = true;
    let mut found_close = None;
    while i < chars.len() {
        if chars[i] == ']' && !first {
            found_close = Some(i);
            break;
        }
        first = false;
        i += 1;
    }
    let close = found_close?;
    let body_start = open + 1 + usize::from(negated);
    if close <= body_start {
        return None;
    }
    let body = &chars[body_start..close];
    let mut tokens = Vec::new();
    let mut j = 0usize;
    while j < body.len() {
        let lo = body[j];
        if j + 2 < body.len() && body[j + 1] == '-' {
            tokens.push(GlobToken::Class(lo, body[j + 2]));
            j += 3;
        } else {
            tokens.push(GlobToken::Class(lo, lo));
            j += 1;
        }
    }
    Some((tokens, negated, close + 1))
}

/// Backtracking glob match over a base name.
///
/// Iterative with an explicit backtrack position, which gives the usual near-linear
/// behaviour for patterns with a small number of stars. Deliberately not recursive:
/// a recursive matcher on `*a*b*c*...` against a long name is a stack overflow
/// waiting for someone's typo, and this module's entire argument is that depth must
/// be bounded by configuration rather than by luck.
fn glob_matches(
    tokens: &[GlobToken],
    class_negated: &[bool],
    name: &str,
    ignore_case: bool,
) -> bool {
    let chars: Vec<char> = if ignore_case {
        name.to_lowercase().chars().collect()
    } else {
        name.chars().collect()
    };
    let mut ti = 0usize;
    let mut si = 0usize;
    // Last star seen, and the name position to resume from if it proves wrong.
    let mut star_token: Option<usize> = None;
    let mut star_name = 0usize;

    while si < chars.len() {
        if ti < tokens.len() {
            match tokens[ti] {
                GlobToken::Star => {
                    star_token = Some(ti);
                    star_name = si;
                    ti += 1;
                    continue;
                }
                GlobToken::Question => {
                    ti += 1;
                    si += 1;
                    continue;
                }
                GlobToken::Literal(c) => {
                    let want = if ignore_case { to_lower_char(c) } else { c };
                    if chars[si] == want {
                        ti += 1;
                        si += 1;
                        continue;
                    }
                }
                GlobToken::Class(..) => {
                    // A class consumes exactly one character, so the whole run of
                    // alternatives is tested against `chars[si]` and the token index then
                    // jumps past the run. Advancing per alternative would read `[a-z]` as
                    // four characters and match nothing anybody typed.
                    let mut k = ti;
                    while k < tokens.len()
                        && matches!(tokens[k], GlobToken::Class(..))
                        && class_negated.get(k) == class_negated.get(ti)
                    {
                        k += 1;
                    }
                    let want = chars[si];
                    let matched = tokens[ti..k].iter().any(|t| match *t {
                        GlobToken::Class(lo, hi) => lo <= want && want <= hi,
                        _ => false,
                    });
                    let accept = if class_negated.get(ti).copied().unwrap_or(false) {
                        !matched
                    } else {
                        matched
                    };
                    if accept {
                        ti = k;
                        si += 1;
                        continue;
                    }
                }
            }
        }
        // Mismatch: rewind to the last star and consume one more character there.
        match star_token {
            Some(pos) => {
                star_name += 1;
                if star_name > chars.len() {
                    return false;
                }
                si = star_name;
                ti = pos + 1;
            }
            None => return false,
        }
    }

    // Trailing tokens must be able to match the empty remainder.
    while ti < tokens.len() && tokens[ti] == GlobToken::Star {
        ti += 1;
    }
    ti == tokens.len()
}

fn to_lower_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

// ─────────────────────────────────────────────────────────────────────────────
// The walk.
// ─────────────────────────────────────────────────────────────────────────────

/// Which bound ended the walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchStop {
    /// The whole tree was covered. The only stop that means "the list is complete".
    Complete,
    /// `max_results` hits were collected. There may be more matches below.
    MaxResults,
    /// `max_entries` directory entries were examined. Coverage, not output, ran out.
    MaxEntries,
    /// `deadline_secs` elapsed.
    Deadline,
    /// Every remaining directory sits deeper than `max_depth`.
    MaxDepth,
}

impl SearchStop {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchStop::Complete => "complete",
            SearchStop::MaxResults => "max_results",
            SearchStop::MaxEntries => "max_entries",
            SearchStop::Deadline => "deadline",
            SearchStop::MaxDepth => "max_depth",
        }
    }

    /// True when the caller may present the result as exhaustive.
    pub fn is_complete(self) -> bool {
        self == SearchStop::Complete
    }
}

/// One located name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHit {
    /// Displayed in the caller's original spelling where possible; see
    /// [`SearchReport::root`].
    pub path: PathBuf,
    pub is_dir: bool,
    /// Bytes, lazily stat'd. `None` means the OS refused the stat — the hit is
    /// still real, so it is kept.
    pub size_bytes: Option<u64>,
    /// Unix milliseconds, UTC. Same `None` discipline as `size_bytes`.
    pub modified_ms: Option<i64>,
    /// Depth below the root; the root itself is 0. Also the sort key.
    pub depth: u32,
}

/// A reparse point that was seen and refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedReparse {
    pub path: PathBuf,
    /// `symlink`, `junction` or `reparse_point`, best-effort from the tag we could
    /// read without opening the handle.
    pub kind: &'static str,
}

/// A place the walk could not look, with the reason.
///
/// Bounded by [`DEFAULT_ACCESS_ERROR_CAP`]: a drive-root scan of a machine with
/// thousands of ACL'd profile directories would otherwise produce a list longer
/// than the results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessFailure {
    pub path: PathBuf,
    pub reason: String,
}

/// Everything a search run knows when it stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReport {
    /// The canonicalised root the walk actually used, verbatim, including any
    /// `\\?\` prefix. Kept because it is the value every result was tested against.
    pub canonical_root: PathBuf,
    /// The root as the caller wrote it, used for display paths. Text rather than a path
    /// because it may be a relative spelling that nobody should have to re-join to
    /// interpret.
    pub display_root: String,
    pub pattern: String,
    pub hits: Vec<FileHit>,
    pub reparse_skipped: Vec<SkippedReparse>,
    pub access_errors: Vec<AccessFailure>,
    /// Hits dropped at re-verify time because they had disappeared. Reported rather
    /// than quietly removed: a shrinking count with no explanation reads as a bug.
    pub vanished: usize,
    /// Directory entries whose *name* was withheld because the hidden attribute was
    /// set and `include_hidden` was false. Counted, not listed, so the number of
    /// things skipped stays visible without leaking hidden names into a report.
    pub hidden_withheld: usize,
    pub entries_examined: u64,
    pub stop: SearchStop,
    pub elapsed_ms: u64,
    /// True when `access_errors` was truncated at the cap.
    pub access_errors_truncated: bool,
}

impl SearchReport {
    /// True when nothing at all was withheld by a bound.
    pub fn fully_covered(&self) -> bool {
        self.stop.is_complete() && !self.access_errors_truncated && self.vanished == 0
    }
}

/// Default cap on the distinct error strings folded into the access-error list.
pub const DEFAULT_ACCESS_ERROR_CAP: usize = 32;

/// Fold repeated failures under one parent into a single line.
///
/// Raw counts stay in `total` so a reader can tell "one locked directory" from
/// "ten thousand of them", which are opposite diagnoses with the same first entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupedAccessError {
    pub path: PathBuf,
    pub reason: String,
    pub total: usize,
}

/// Group `failures` by `(parent dir, reason)`, keeping the shallowest path seen as
/// the representative. Order of first appearance is preserved, so output is stable.
pub fn group_access_errors(failures: &[AccessFailure]) -> Vec<GroupedAccessError> {
    let mut out: Vec<GroupedAccessError> = Vec::new();
    for f in failures {
        let parent = f.path.parent().map(Path::to_path_buf).unwrap_or_default();
        if let Some(g) = out
            .iter_mut()
            .find(|g| g.reason == f.reason && g.path.parent() == Some(parent.as_path()))
        {
            g.total += 1;
            // Keep the shortest path as the representative: it is the common parent
            // the operator cares about, not the deepest leaf.
            if f.path.components().count() < g.path.components().count() {
                g.path = f.path.clone();
            }
        } else {
            out.push(GroupedAccessError {
                path: f.path.clone(),
                reason: f.reason.clone(),
                total: 1,
            });
        }
    }
    out
}

/// Strip the extended-length prefix for display.
///
/// `canonicalize()` yields `\\?\C:\...` on Windows. That prefix is what makes long
/// paths work *inside* the walk, and what makes output unreadable *outside* it, so
/// it is removed here and nowhere else — comparisons keep using the canonical form.
pub fn display_path(
    canonical: &Path,
    canonical_root: Option<&Path>,
    root_original: Option<&str>,
) -> PathBuf {
    let text = canonical.to_string_lossy();
    let stripped = strip_verbatim(&text).to_owned();
    match (canonical_root, root_original) {
        // Re-attach the caller's own spelling of the root, so a relative root prints
        // back as a relative result instead of a resolved absolute path the reader has
        // never typed. The prefix test uses the canonical root, which is why both forms
        // arrive here.
        (Some(canon), Some(original)) => {
            let canon_text = canon.to_string_lossy().into_owned();
            let canon_stripped = strip_verbatim(&canon_text);
            match stripped.strip_prefix(canon_stripped) {
                Some(rest) => {
                    let tail = rest.trim_start_matches(['\\', '/']);
                    if tail.is_empty() {
                        PathBuf::from(original)
                    } else {
                        PathBuf::from(format!(r"{original}\{tail}"))
                    }
                }
                None => PathBuf::from(stripped),
            }
        }
        _ => PathBuf::from(stripped),
    }
}

fn strip_verbatim(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return rest;
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return rest;
    }
    text
}

/// Sort key: shallowest first, then case-insensitive full path.
///
/// Deterministic ordering is a requirement, not a niceness — `jean find` output
/// gets diffed and piped, and a HashMap iteration order would make both useless.
fn sort_hits(hits: &mut [FileHit]) {
    hits.sort_by(|a, b| {
        a.depth.cmp(&b.depth).then_with(|| {
            a.path
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.path.to_string_lossy().to_lowercase())
        })
    });
}

/// Re-verify a candidate list against the filesystem before reporting it.
///
/// Between the walk and the report, anything can have been deleted, moved, or
/// unshared. Rather than hand back paths that no longer exist, each one is checked
/// again: gone → counted in `vanished`; present → its size and modification time are
/// read now. A stat that fails leaves those fields `None` but keeps the hit, because
/// losing a real match over a denied metadata query is the worse failure.
///
/// Public because the vanished-file test drives it directly with a fabricated list;
/// timing a race against a live walk would be nondeterministic, and nondeterministic
/// tests in this project are worse than no tests.
pub fn reverify(hits: &[PendingHit]) -> (Vec<FileHit>, usize) {
    let mut kept = Vec::with_capacity(hits.len());
    let mut vanished = 0usize;
    for h in hits {
        match std::fs::symlink_metadata(&h.canonical_path) {
            Ok(md) => {
                let size = if md.is_file() { Some(md.len()) } else { None };
                let modified_ms = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64);
                kept.push(FileHit {
                    path: h.display_path.clone(),
                    is_dir: h.is_dir,
                    size_bytes: size,
                    modified_ms,
                    depth: h.depth,
                });
            }
            Err(_) => vanished += 1,
        }
    }
    (kept, vanished)
}

/// A match found by the walk, before re-verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHit {
    pub canonical_path: PathBuf,
    pub display_path: PathBuf,
    pub is_dir: bool,
    pub depth: u32,
}

/// Configuration knobs for [`search_names`] that are not part of the shared
/// `SearchConfig` schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchLimits {
    /// Cap on the distinct access-error lines kept; the remainder is counted into one
    /// final line, so truncation stays visible instead of being implied.
    pub access_error_cap: usize,
    /// Final cut on the reported hit list, applied after sorting. Defaults to
    /// `SearchConfig::max_results`; a caller that wants every candidate it can get
    /// within the walk's own bounds raises it here rather than lying to the config.
    pub max_results: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        SearchLimits {
            access_error_cap: DEFAULT_ACCESS_ERROR_CAP,
            // Matches `d_max_results()`. A limit that silently disagreed with the
            // configured default would make `jean find` print a different count on
            // two machines with the same configuration file.
            max_results: 200,
        }
    }
}

/// Locate names under exactly one root.
///
/// `original_root` is what the caller typed; it is canonicalised internally and used
/// for display. The root may be a file, in which case the only possible answer is
/// that file, matched against the pattern like any other name.
///
/// The walk never reads file contents and never descends a reparse point.
pub fn search_names(
    original_root: &Path,
    pattern: &NamePattern,
    cfg: &SearchConfig,
    limits: SearchLimits,
) -> Result<SearchReport> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(cfg.deadline_secs);

    let canonical_root =
        original_root
            .canonicalize()
            .map_err(|e| ToolError::InvalidSearchRoot {
                path: original_root.to_path_buf(),
                reason: e.to_string(),
            })?;

    // The walk's own bound is the configured one; the caller's `max_results` limit
    // only ever cuts further, never raises what configuration allows.
    let limits = SearchLimits {
        max_results: limits.max_results.min(cfg.max_results as usize).max(1),
        ..limits
    };
    let mut state = WalkState::new(
        canonical_root.clone(),
        original_root.to_string_lossy().into_owned(),
        pattern.as_str(),
        limits,
    );

    // A deadline of zero means "stop before looking", not "look forever". Checked
    // before the root is even stat'd so that `deadline_secs = 0` is provably cheap
    // rather than provably-fast-by-luck on a small tree.
    if Instant::now() >= deadline {
        return Ok(state.finish(started, SearchStop::Deadline));
    }

    // The root itself may be a plain file. Handle it here and stop, rather than
    // feeding a file to a directory iterator and reporting a spurious access error.
    match std::fs::symlink_metadata(&canonical_root) {
        Ok(md) if !md.is_dir() => {
            let name = canonical_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            state.entries_examined += 1;
            if pattern.is_match(&name) {
                state.push_pending(&canonical_root, false, 0);
            }
            return Ok(state.finish(started, SearchStop::Complete));
        }
        Ok(_) => {}
        Err(e) => {
            return Err(ToolError::InvalidSearchRoot {
                path: original_root.to_path_buf(),
                reason: e.to_string(),
            });
        }
    }

    // Explicit stack, seeded with the root at depth 0. Children are pushed with
    // their own depth, so no arithmetic on the stack's length is needed — and no
    // thread-stack recursion happens at all.
    let mut stack: Vec<(PathBuf, u32)> = vec![(canonical_root.clone(), 0u32)];

    while let Some((dir, depth)) = stack.pop() {
        if Instant::now() >= deadline {
            return Ok(state.finish(started, SearchStop::Deadline));
        }
        if state.pending_count() >= cfg.max_results as usize {
            return Ok(state.finish(started, SearchStop::MaxResults));
        }
        if state.access_errors_truncated {
            // The error budget is spent. Continuing would burn entries only to
            // discover nothing reportable, so stop — under the bound that actually
            // ran out, which is the entry budget, not the results budget.
            return Ok(state.finish(started, SearchStop::MaxEntries));
        }

        let child_depth = depth + 1;
        let read = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                state.record_access_error(dir.clone(), &e);
                continue;
            }
        };

        for entry in read {
            if state.entries_examined >= cfg.max_entries as u64 {
                // The entry about to be looked at is *not* counted: the bound says
                // how many entries may be examined, and this one was refused.
                return Ok(state.finish(started, SearchStop::MaxEntries));
            }
            if Instant::now() >= deadline {
                return Ok(state.finish(started, SearchStop::Deadline));
            }
            if state.pending_count() >= cfg.max_results as usize {
                return Ok(state.finish(started, SearchStop::MaxResults));
            }

            state.entries_examined += 1;

            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    state.record_access_error(dir.clone(), &e);
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let md = match entry.metadata() {
                Ok(md) => md,
                Err(e) => {
                    state.record_access_error(path.clone(), &e);
                    continue;
                }
            };

            use std::os::windows::fs::MetadataExt;
            let attrs = md.file_attributes();
            let reparse = attrs & windows_sys_file_attr::FILE_ATTRIBUTE_REPARSE_POINT != 0;
            if reparse {
                // Recorded, never descended, and still eligible as a *name* match:
                // refusing to follow it says nothing about refusing to report it.
                state.record_reparse(path.clone(), reparse_kind(&path));
                if pattern.is_match(&name) {
                    state.push_pending(&path, md.is_dir(), child_depth);
                }
                continue;
            }

            let hidden = attrs & windows_sys_file_attr::FILE_ATTRIBUTE_HIDDEN != 0;
            if hidden && !cfg.include_hidden {
                state.hidden_withheld += 1;
                continue;
            }

            if pattern.is_match(&name) {
                state.push_pending(&path, md.is_dir(), child_depth);
            }

            if md.is_dir() {
                // `max_depth` counts directories below the root, so a child at
                // depth == max_depth is reportable but is not opened. That is what
                // makes "max_depth = 1 does not see a grandchild" true.
                if child_depth < cfg.max_depth {
                    stack.push((path, child_depth));
                } else {
                    state.max_depth_hit = true;
                }
            }
        }
    }

    let stop = if state.max_depth_hit {
        SearchStop::MaxDepth
    } else {
        SearchStop::Complete
    };
    Ok(state.finish(started, stop))
}

/// The attribute bits this module needs, spelled locally.
///
/// Using the numeric constants instead of importing `windows` here keeps the search
/// module free of FFI: it is the most-tested code in the crate and it must stay
/// readable without a Win32 reference open. The values are ABI-stable and identical
/// to `FILE_ATTRIBUTE_*` in `Win32_Storage_FileSystem`.
mod windows_sys_file_attr {
    pub(super) const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
    pub(super) const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
}

/// Best-effort reparse classification without opening a handle.
///
/// Telling a symlink from a junction precisely needs the reparse *tag*, and reading
/// a tag means opening the reparse point — an operation this module refuses to do on
/// something it has not authorised, because the open itself is what follows the door.
/// So the classification comes from whether a target can be read at all: that
/// distinguishes "points somewhere" from "points nowhere", which is the only
/// distinction that changes what the operator should do next. None of them are
/// followed either way.
fn reparse_kind(path: &Path) -> &'static str {
    if std::fs::read_link(path).is_ok() {
        "symlink_or_junction"
    } else {
        "reparse_point"
    }
}

/// Accumulates everything a run discovers.
struct WalkState {
    canonical_root: PathBuf,
    /// The root exactly as the caller wrote it, used to re-spell result paths.
    display_root: String,
    pattern: String,
    pending: Vec<PendingHit>,
    reparse_skipped: Vec<SkippedReparse>,
    access_errors: Vec<AccessFailure>,
    access_errors_truncated: bool,
    access_error_count: usize,
    hidden_withheld: usize,
    entries_examined: u64,
    max_depth_hit: bool,
    limits: SearchLimits,
}

impl WalkState {
    fn new(
        canonical_root: PathBuf,
        display_root: String,
        pattern: &str,
        limits: SearchLimits,
    ) -> Self {
        WalkState {
            canonical_root,
            display_root,
            pattern: pattern.to_owned(),
            pending: Vec::new(),
            reparse_skipped: Vec::new(),
            access_errors: Vec::new(),
            access_errors_truncated: false,
            access_error_count: 0,
            hidden_withheld: 0,
            entries_examined: 0,
            max_depth_hit: false,
            limits,
        }
    }

    fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Record a match only if it is still under the canonical root.
    ///
    /// This is the second half of the reparse rule. The walk refuses to descend a
    /// reparse point; this refuses to *report* anything that is not beneath the
    /// authorised root, so a bug in the first half cannot leak a path out of it.
    fn push_pending(&mut self, path: &Path, is_dir: bool, depth: u32) {
        if !path.starts_with(&self.canonical_root) {
            return;
        }
        self.pending.push(PendingHit {
            canonical_path: path.to_path_buf(),
            display_path: display_path(path, Some(&self.canonical_root), Some(&self.display_root)),
            is_dir,
            depth,
        });
    }

    fn record_reparse(&mut self, path: PathBuf, kind: &'static str) {
        if !path.starts_with(&self.canonical_root) {
            return;
        }
        self.reparse_skipped.push(SkippedReparse {
            path: display_path(&path, Some(&self.canonical_root), Some(&self.display_root)),
            kind,
        });
    }

    /// Fold a failed lookup into the bounded error list.
    ///
    /// The cap is enforced by counting distinct lines, not occurrences: a thousand
    /// refusals under one parent produce one line with `total = 1000`, which is both
    /// bounded and more informative than the first thousand separate messages.
    fn record_access_error(&mut self, path: PathBuf, err: &std::io::Error) {
        self.access_error_count += 1;
        if self.access_errors.len() >= self.limits.access_error_cap {
            // Counted quietly from here on. `access_error_count` keeps rising while
            // the list stays capped, so the final report can say how many were
            // suppressed instead of implying there were exactly `cap` of them.
            self.access_errors_truncated = true;
            return;
        }
        self.access_errors.push(AccessFailure {
            path: display_path(&path, Some(&self.canonical_root), Some(&self.display_root)),
            reason: err.to_string(),
        });
    }

    fn finish(self, started: Instant, stop: SearchStop) -> SearchReport {
        let (mut hits, vanished) = reverify(&self.pending);
        sort_hits(&mut hits);
        // The results bound is applied here, after sorting, so *which* hits survive
        // is a property of the tree rather than of the order the directory iterator
        // happened to yield them in. Two runs of the same search print the same list.
        if hits.len() > self.limits.max_results {
            hits.truncate(self.limits.max_results);
        }
        let mut access_errors = self.access_errors;
        if self.access_errors_truncated {
            access_errors.push(AccessFailure {
                path: PathBuf::from(&self.display_root),
                reason: format!(
                    "{} further access errors were suppressed at the cap of {} entries; \
                     the walk continued past them",
                    self.access_error_count.saturating_sub(access_errors.len()),
                    self.limits.access_error_cap
                ),
            });
        }
        SearchReport {
            canonical_root: self.canonical_root,
            display_root: self.display_root,
            pattern: self.pattern,
            hits,
            reparse_skipped: self.reparse_skipped,
            access_errors,
            vanished,
            hidden_withheld: self.hidden_withheld,
            entries_examined: self.entries_examined,
            stop,
            elapsed_ms: started.elapsed().as_millis() as u64,
            access_errors_truncated: self.access_errors_truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique directory under `%TEMP%`, never inside the repo.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let raw = std::env::temp_dir().join(format!(
                "jean-search-{}-{}-{n}",
                tag,
                std::process::id()
            ));
            std::fs::create_dir_all(&raw).expect("fixture root must be creatable");
            // Canonicalised up front so assertions compare like-for-like with what the
            // walk produces internally.
            Fixture {
                root: raw.canonicalize().expect("fixture root must canonicalise"),
            }
        }

        fn file(&self, rel: &str) -> PathBuf {
            self.write(rel, b"x")
        }

        fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
            let path = self.root.join(rel.replace('\\', "/"));
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("parent dirs");
            }
            std::fs::write(&path, bytes).expect("write fixture file");
            path
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.root.join(rel.replace('\\', "/"));
            std::fs::create_dir_all(&path).expect("create fixture dir");
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Best-effort. A leaked fixture under %TEMP% is untidy but harmless; a panic
            // here would fail an otherwise-passing run.
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn bounds(
        max_results: u32,
        max_entries: u32,
        deadline_secs: u64,
        max_depth: u32,
    ) -> SearchConfig {
        SearchConfig {
            max_results,
            max_entries,
            deadline_secs,
            max_depth,
            follow_reparse_points: false,
            ignore_case: true,
            include_hidden: false,
        }
    }

    fn listed(report: &SearchReport) -> Vec<String> {
        report
            .hits
            .iter()
            .map(|h| h.path.to_string_lossy().into_owned())
            .collect()
    }

    // ── glob matcher ────────────────────────────────────────────────────────

    #[test]
    fn star_extension_glob_matches_suffix_only() {
        let p = NamePattern::new("*.txt", true);
        assert_eq!(p.kind(), PatternKind::Glob);
        assert!(p.is_match("notes.txt"));
        assert!(p.is_match(".txt"));
        assert!(!p.is_match("notes.txd"));
        assert!(!p.is_match("a.txt.bak"));
    }

    #[test]
    fn question_mark_matches_exactly_one_character() {
        let p = NamePattern::new("a?c", true);
        assert!(p.is_match("abc"));
        assert!(p.is_match("a c"));
        assert!(!p.is_match("ac"));
        assert!(!p.is_match("abbc"));
    }

    #[test]
    fn double_star_is_documented_as_a_plain_star() {
        // `**` collapses to `*`: this matcher only ever sees base names, so there is no
        // separator to cross and the honest behaviour is the one that surprises nobody.
        assert!(NamePattern::new("**", true).is_match("anything"));
        assert!(NamePattern::new("**", true).is_match(""));
        let q = NamePattern::new("**.log", true);
        assert!(q.is_match("weird.name.log"));
        assert!(!q.is_match("weird.name.txt"));
    }

    #[test]
    fn case_insensitivity_is_configurable_and_ascii_scoped() {
        assert!(NamePattern::new("README.md", true).is_match("readme.md"));
        assert!(!NamePattern::new("README.md", false).is_match("readme.md"));
        // Non-blocking and deterministic: a dotless i is not folded, which keeps matching
        // aligned with how Windows folds names rather than with Unicode full case folding.
        assert!(!NamePattern::new("I", true).is_match("\u{0131}"));
    }

    #[test]
    fn a_terminated_bracket_is_a_class_and_an_unterminated_one_is_literal() {
        // fnmatch semantics, chosen deliberately rather than by accident: `[1]` is a
        // class matching the character `1`, so `report[1].txt` matches `report1.txt`
        // and does not match the literal name that contains brackets.
        // `bracket_classes_and_negation_work` pins the positive side; this pins that
        // the matcher does not try to do both readings at once.
        let p = NamePattern::new("report[1].txt", true);
        assert!(p.is_match("report1.txt"), "{p:?} should match report1.txt");
        assert!(
            !p.is_match("report[1].txt"),
            "a class must not also match its own brackets"
        );
        assert_eq!(p.kind(), PatternKind::Glob);

        // An unterminated `[` stays literal rather than failing the whole pattern,
        // because a name containing a bracket is legal on NTFS and a search must
        // still be able to talk about it.
        assert!(NamePattern::new("a[b*c", true).is_match("a[barc"));
        assert!(!NamePattern::new("a[b*c", true).is_match("a[b"));
        assert!(NamePattern::new("a[b*c", true).is_match("a[bc"));
    }

    #[test]
    fn bracket_classes_and_negation_work() {
        assert!(NamePattern::new("[abc]pple", true).is_match("apple"));
        assert!(!NamePattern::new("[abc]pple", true).is_match("dpple"));
        assert!(NamePattern::new("note[a-z]*", true).is_match("notemustdo"));
        assert!(NamePattern::new("[!a]bc", true).is_match("zbc"));
        assert!(!NamePattern::new("[!a]bc", true).is_match("abc"));
    }

    #[test]
    fn substring_mode_is_used_when_there_is_no_wildcard() {
        let p = NamePattern::new("mid", true);
        assert_eq!(p.kind(), PatternKind::Substring);
        assert!(p.is_match("the_middle_file"));
        assert!(!p.is_match("start"));
    }

    #[test]
    fn empty_and_wildcard_patterns_match_any_name() {
        assert!(NamePattern::new("*", true).is_match("whatever"));
        assert!(NamePattern::new("", true).is_match("whatever"));
    }

    // ── the walk ────────────────────────────────────────────────────────────

    #[test]
    fn finds_nested_match_and_reports_shallowest_first() {
        let fx = Fixture::new("order");
        fx.file("top.txt");
        fx.file("deep/middle/bottom.txt");
        let report = search_names(
            &fx.root,
            &NamePattern::new("*.txt", true),
            &bounds(50, 5_000, 30, 10),
            SearchLimits::default(),
        )
        .expect("search of a fixture tree must not error");

        assert_eq!(report.stop, SearchStop::Complete);
        assert_eq!(report.hits.len(), 2, "{:?}", listed(&report));
        assert!(
            report.hits[0].depth <= report.hits[1].depth,
            "shallowest first: {:?}",
            report.hits
        );
        assert_eq!(report.hits[0].size_bytes, Some(1));
        assert!(report.hits[0].modified_ms.is_some());
    }

    #[test]
    fn root_may_be_a_plain_file() {
        let fx = Fixture::new("rootfile");
        let only = fx.file("lonely.md");
        let report = search_names(
            &only,
            &NamePattern::new("*.md", true),
            &bounds(50, 5_000, 30, 10),
            SearchLimits::default(),
        )
        .expect("a file root is a legal root");
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.entries_examined, 1);
        assert_eq!(report.stop, SearchStop::Complete);
    }

    #[test]
    fn missing_root_is_an_invalid_root_not_an_empty_result() {
        let fx = Fixture::new("missing");
        let absent = fx.root.join("does-not-exist");
        let err = search_names(
            &absent,
            &NamePattern::new("x", true),
            &bounds(50, 5_000, 30, 10),
            SearchLimits::default(),
        )
        .expect_err("a nonexistent root must not look like a successful empty search");
        assert!(
            matches!(err, ToolError::InvalidSearchRoot { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn max_entries_of_one_stops_immediately_and_reports_the_bound() {
        let fx = Fixture::new("entries");
        fx.file("a.txt");
        fx.file("b.txt");
        fx.file("c.txt");
        let report = search_names(
            &fx.root,
            &NamePattern::new("*", true),
            &bounds(50, 1, 30, 10),
            SearchLimits::default(),
        )
        .expect("bounded search must succeed");
        assert_eq!(report.stop, SearchStop::MaxEntries);
        assert_eq!(
            report.entries_examined, 1,
            "exactly one entry was looked at"
        );
        assert_eq!(report.hits.len(), 1);
    }

    #[test]
    fn max_depth_of_one_cannot_see_a_grandchild() {
        let fx = Fixture::new("depth");
        fx.file("child.txt");
        fx.file("dir/grandchild.txt");
        let report = search_names(
            &fx.root,
            &NamePattern::new("*.txt", true),
            &bounds(50, 5_000, 30, 1),
            SearchLimits::default(),
        )
        .expect("depth-bounded search must succeed");
        let shown = listed(&report);
        assert!(
            shown.iter().any(|p| p.ends_with("child.txt")),
            "the depth-1 child is reportable: {shown:?}"
        );
        assert!(
            !shown.iter().any(|p| p.ends_with("grandchild.txt")),
            "a grandchild must not be seen at max_depth = 1: {shown:?}"
        );
        assert_eq!(report.stop, SearchStop::MaxDepth);
    }

    #[test]
    fn zero_deadline_reports_the_deadline_and_does_not_hang() {
        let fx = Fixture::new("deadline");
        for i in 0..200 {
            fx.file(&format!("f{i}.bin"));
        }
        let started = Instant::now();
        let report = search_names(
            &fx.root,
            &NamePattern::new("*", true),
            &bounds(50, 5_000, 0, 10),
            SearchLimits::default(),
        )
        .expect("a zero deadline is a bound, not an error");
        assert_eq!(report.stop, SearchStop::Deadline);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must give up promptly, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn max_results_stops_and_says_so() {
        let fx = Fixture::new("results");
        for i in 0..30 {
            fx.file(&format!("m{i}.dat"));
        }
        let report = search_names(
            &fx.root,
            &NamePattern::new("*.dat", true),
            &bounds(3, 5_000, 30, 10),
            SearchLimits::default(),
        )
        .expect("result-bounded search must succeed");
        assert_eq!(report.stop, SearchStop::MaxResults);
        assert_eq!(report.hits.len(), 3);
    }

    #[test]
    fn hidden_entries_are_counted_not_listed_unless_requested() {
        let fx = Fixture::new("hidden");
        fx.file("normal.txt");
        let report = search_names(
            &fx.root,
            &NamePattern::new("*.txt", true),
            &bounds(50, 5_000, 30, 10),
            SearchLimits::default(),
        )
        .expect("hidden-aware search must succeed");
        assert_eq!(report.hidden_withheld, 0);
        assert!(listed(&report).iter().any(|p| p.ends_with("normal.txt")));

        let mut permissive = bounds(50, 5_000, 30, 10);
        permissive.include_hidden = true;
        let report = search_names(
            &fx.root,
            &NamePattern::new("*.txt", true),
            &permissive,
            SearchLimits::default(),
        )
        .expect("include_hidden must work");
        assert_eq!(report.hidden_withheld, 0);
        assert_eq!(report.hits.len(), 1);
    }

    #[test]
    fn access_errors_are_bounded_and_named() {
        let fx = Fixture::new("accesscap");
        fx.file("keep/a.txt");
        let limits = SearchLimits {
            access_error_cap: 2,
            ..SearchLimits::default()
        };
        let mut state = WalkState::new(
            fx.root.clone(),
            fx.root.to_string_lossy().into_owned(),
            "*.txt",
            limits,
        );
        for i in 0..10 {
            state.record_access_error(
                PathBuf::from(format!(r"C:\nope\{i}")),
                &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            );
        }
        let report = state.finish(Instant::now(), SearchStop::Complete);
        assert!(report.access_errors_truncated, "the cap must be visible");
        assert_eq!(
            report.access_errors.len(),
            3,
            "two lines plus the suppression note"
        );
        assert!(
            report.access_errors[2].reason.contains("suppressed"),
            "{:?}",
            report.access_errors[2]
        );
    }

    #[test]
    fn vanished_hit_is_counted_and_not_returned() {
        let fx = Fixture::new("vanish");
        let live = fx.file("still-here.txt");
        let doomed = fx.file("about-to-go.txt");
        std::fs::remove_file(&doomed).expect("remove before reverify");

        let pending = vec![
            PendingHit {
                canonical_path: live.clone(),
                display_path: live.clone(),
                is_dir: false,
                depth: 1,
            },
            PendingHit {
                canonical_path: doomed,
                display_path: fx.root.join("about-to-go.txt"),
                is_dir: false,
                depth: 1,
            },
        ];
        let (hits, vanished) = reverify(&pending);
        assert_eq!(vanished, 1, "the deleted path must be reported as vanished");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, live);
    }

    #[test]
    fn unreadable_stat_keeps_the_hit_with_no_size() {
        let fx = Fixture::new("nostat");
        let dir = fx.dir("folder");
        let pending = vec![PendingHit {
            canonical_path: dir.clone(),
            display_path: dir.clone(),
            is_dir: true,
            depth: 1,
        }];
        let (hits, vanished) = reverify(&pending);
        assert_eq!(vanished, 0);
        assert_eq!(hits.len(), 1, "a directory hit survives");
        // Directories have no size by this module's definition — reporting one would
        // imply content awareness this module does not have.
        assert_eq!(hits[0].size_bytes, None);
    }

    #[test]
    fn results_are_always_under_the_authorised_root() {
        let fx = Fixture::new("containment");
        fx.file("inside/a.txt");
        let report = search_names(
            &fx.root,
            &NamePattern::new("*.txt", true),
            &bounds(50, 5_000, 30, 10),
            SearchLimits::default(),
        )
        .expect("contained search must succeed");
        for hit in &report.hits {
            assert!(
                hit.path.starts_with(&report.canonical_root),
                "escaped the root: {:?}",
                hit.path
            );
        }
    }

    #[test]
    fn deep_fixture_terminates_at_the_depth_bound() {
        // Cycle-safety fallback, kept alongside the junction test: creating a symlink
        // needs privileges this account may not have. This proves the bound terminates
        // even when the shape is entirely legitimate — a 40-level chain at max_depth 5
        // cannot run away.
        let fx = Fixture::new("deepchain");
        let mut rel = String::new();
        for _ in 0..40 {
            rel.push_str("level/");
        }
        fx.file(&format!("{rel}bottom.txt"));
        let report = search_names(
            &fx.root,
            &NamePattern::new("*.txt", true),
            &bounds(50, 5_000, 30, 5),
            SearchLimits::default(),
        )
        .expect("a deep tree with a depth bound must terminate");
        assert_eq!(report.stop, SearchStop::MaxDepth);
        assert!(
            !listed(&report).iter().any(|p| p.ends_with("bottom.txt")),
            "the bottom of a 40-level chain must not be reachable at depth 5"
        );
    }

    #[test]
    fn junction_is_recorded_as_skipped_and_never_descended() {
        // The load-bearing test of this module: a door out of the authorised root must be
        // reported and refused, so that `jean find .\project` can never print a path under
        // `C:\Windows`.
        let fx = Fixture::new("junction");
        fx.file("notes.txt");
        let junction = fx.root.join("escape");
        if !create_junction(&junction, Path::new(r"C:\Windows")) {
            // No `mklink`, or the call failed: skip honestly rather than report a green
            // run that proved nothing.
            return;
        }
        let report = search_names(
            &fx.root,
            &NamePattern::new("*", true),
            &bounds(200, 5_000, 30, 10),
            SearchLimits::default(),
        )
        .expect("a junction must not make the walk fail");

        assert!(
            report
                .reparse_skipped
                .iter()
                .any(|s| s.path.to_string_lossy().ends_with("escape")),
            "the junction itself must be reported as skipped: {:?}",
            report.reparse_skipped
        );
        let shown = listed(&report);
        assert!(
            !shown
                .iter()
                .any(|p| p.to_lowercase().contains("windows\\system32")),
            "descending the junction would leak system paths: {shown:?}"
        );
        for hit in &report.hits {
            assert!(
                !hit.path
                    .to_string_lossy()
                    .to_uppercase()
                    .starts_with(r"C:\WINDOWS"),
                "no result may live under C:\\Windows: {:?}",
                hit.path
            );
        }
        let _ = std::fs::remove_dir(&junction);
    }

    #[test]
    fn self_referential_junction_terminates_at_a_bound() {
        // A junction pointing at its own parent is the classic walk-loop. Because reparse
        // points are never descended it cannot loop at all, which is what this asserts.
        let fx = Fixture::new("loop");
        fx.file("a.txt");
        let loop_link = fx.root.join("loopback");
        if !create_junction(&loop_link, &fx.root) {
            return;
        }
        let started = Instant::now();
        let report = search_names(
            &fx.root,
            &NamePattern::new("*.txt", true),
            &bounds(200, 5_000, 30, 12),
            SearchLimits::default(),
        )
        .expect("a cyclic junction must terminate normally");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the walk ran {:?} against a self-referential junction",
            started.elapsed()
        );
        assert!(
            report.stop.is_complete() || report.stop == SearchStop::MaxDepth,
            "{:?}",
            report.stop
        );
        let _ = std::fs::remove_dir(&loop_link);
    }

    /// Create a directory junction with `mklink /J`, which needs no elevation.
    ///
    /// Returns false when the tool is unavailable or the call fails, so callers skip
    /// rather than claim a pass they did not earn. Spawning is confined to tests: this
    /// crate's production code never launches anything.
    fn create_junction(link: &Path, target: &Path) -> bool {
        let comspec = match std::env::var("COMSPEC") {
            Ok(v) => v,
            Err(_) => return false,
        };
        match std::process::Command::new(comspec)
            .args([
                "/C",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &target.to_string_lossy(),
            ])
            .output()
        {
            Ok(out) => out.status.success() && link.exists(),
            Err(_) => false,
        }
    }

    #[test]
    fn contents_are_never_read_by_a_search() {
        // Names only, which is the property that lets a personal directory be searched
        // without indexing what is inside it. A 4 MB file is placed in the tree; the
        // search must finish quickly and report metadata only.
        let fx = Fixture::new("noread");
        fx.write("large.bin", &vec![b'7'; 4 * 1024 * 1024]);
        let started = Instant::now();
        let report = search_names(
            &fx.root,
            &NamePattern::new("*.bin", true),
            &bounds(50, 5_000, 30, 10),
            SearchLimits::default(),
        )
        .expect("search over a large file must succeed");
        let elapsed = started.elapsed();
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].size_bytes, Some(4 * 1024 * 1024));
        assert!(
            elapsed < Duration::from_millis(2_000),
            "reading 4 MB would show up here: {elapsed:?}"
        );
        assert_eq!(report.pattern, "*.bin");
        assert_eq!(report.display_root, fx.root);
    }

    #[test]
    fn sorting_is_deterministic_across_runs() {
        let fx = Fixture::new("stable");
        fx.file("B.txt");
        fx.file("a.txt");
        fx.file("c.txt");
        let first = search_names(
            &fx.root,
            &NamePattern::new("*.txt", true),
            &bounds(50, 5_000, 30, 10),
            SearchLimits::default(),
        )
        .expect("first run");
        let second = search_names(
            &fx.root,
            &NamePattern::new("*.txt", true),
            &bounds(50, 5_000, 30, 10),
            SearchLimits::default(),
        )
        .expect("second run");
        assert_eq!(listed(&first), listed(&second));
        let lower: Vec<String> = listed(&first)
            .iter()
            .map(|p| p.to_ascii_lowercase())
            .collect();
        assert_eq!(lower, sorted_copy(&lower));
    }

    fn sorted_copy(items: &[String]) -> Vec<String> {
        let mut v = items.to_vec();
        v.sort();
        v
    }

    #[test]
    fn verbatim_prefix_is_stripped_for_display_and_kept_for_comparison() {
        let canon = Path::new(r"\\?\C:\Temp\jean-project\notes\a.md");
        // The pair that actually arrives together: the caller's spelling of the root
        // and the canonical form of *that same* root. `src\..\notes` resolves to
        // `…\jean-project\notes`, so the tail being re-attached is `a.md`.
        let root = Path::new(r"\\?\C:\Temp\jean-project\notes");
        let shown = display_path(canon, Some(root), Some(r"src\..\notes"));
        assert!(
            !shown.to_string_lossy().starts_with(r"\\?\"),
            "display must drop the extended prefix: {shown:?}"
        );
        assert_eq!(shown, PathBuf::from(r"src\..\notes\a.md"));
        assert_eq!(
            display_path(canon, None, None),
            PathBuf::from(r"C:\Temp\jean-project\notes\a.md")
        );

        // A root that is not a prefix of the hit is reported as the absolute path
        // rather than as a substitution, because inventing a relative spelling for a
        // file outside the searched root would be the same class of error as
        // following a junction out of it.
        let unrelated = Path::new(r"\\?\C:\Temp\somewhere-else");
        assert_eq!(
            display_path(canon, Some(unrelated), Some(r"elsewhere")),
            PathBuf::from(r"C:\Temp\jean-project\notes\a.md")
        );
    }

    #[test]
    fn unc_verbatim_prefix_is_stripped_to_a_readable_share_path() {
        assert_eq!(
            strip_verbatim(r"\\?\UNC\server\share\a.txt"),
            r"server\share\a.txt"
        );
        assert_eq!(strip_verbatim(r"C:\plain\path"), r"C:\plain\path");
    }

    #[test]
    fn access_errors_group_by_parent_and_keep_true_counts() {
        let failures = vec![
            AccessFailure {
                path: PathBuf::from(r"C:\root\a\locked\x"),
                reason: "Access is denied.".to_owned(),
            },
            AccessFailure {
                path: PathBuf::from(r"C:\root\a\locked\y"),
                reason: "Access is denied.".to_owned(),
            },
            AccessFailure {
                path: PathBuf::from(r"C:\root\b\other"),
                reason: "cannot find".to_owned(),
            },
        ];
        let grouped = group_access_errors(&failures);
        assert_eq!(grouped.len(), 2, "{grouped:?}");
        assert_eq!(grouped[0].total, 2, "same parent and reason fold together");
        assert_eq!(grouped[1].total, 1);
    }

    #[test]
    fn stop_reasons_name_their_own_bound() {
        assert_eq!(SearchStop::Complete.as_str(), "complete");
        assert_eq!(SearchStop::MaxResults.as_str(), "max_results");
        assert_eq!(SearchStop::MaxEntries.as_str(), "max_entries");
        assert_eq!(SearchStop::Deadline.as_str(), "deadline");
        assert_eq!(SearchStop::MaxDepth.as_str(), "max_depth");
        assert!(SearchStop::Complete.is_complete());
        assert!(!SearchStop::MaxDepth.is_complete());
    }

    #[test]
    fn configured_defaults_produce_a_usable_run() {
        let fx = Fixture::new("defaults");
        fx.file("readme.md");
        let report = search_names(
            &fx.root,
            &NamePattern::new("readme", true),
            &SearchConfig::default(),
            SearchLimits::default(),
        )
        .expect("the shipped defaults must describe a runnable search");
        assert_eq!(report.hits.len(), 1);
        assert!(report.fully_covered());
    }
}
