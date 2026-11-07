//! Installed-application discovery.
//!
//! ## What this is for
//!
//! Turn a name a human typed ("notepad", "Ubuntu", "Obsidian") into an executable
//! path, or into the honest answer that several things could have been meant. This is
//! the first half of the `jean launch` intent; the second half — actually starting
//! something — lives in `jean-process`, behind the policy engine, and is deliberately
//! not reachable from here.
//!
//! ## The promise this module exists to keep
//!
//! **Nothing discovered here is ever executed.** Opening a `.lnk` with
//! `IPersistFile` and reading registry values are pure inspection. That matters more
//! than it sounds: every source below is attacker-writable by design. A Start Menu
//! entry can be created by any program the user has run, and `App Paths` entries live
//! per-user. If resolution were done by "just run it and see", launching would become
//! a code-execution primitive triggered by a typo. The tests plant a shortcut whose
//! target would be catastrophic to run and assert discovery still returns normally.
//!
//! ## Sources, in order, each individually disable-able
//!
//! | Order | Source | Config value | Why it is where it is |
//! |---|---|---|---|
//! | 1 | `PATH` / `PATHEXT` | `"path"` | Cheapest, and the answer users expect for CLI tools |
//! | 2 | Start Menu `.lnk` files | `"start_menu"` | Where GUI applications advertise themselves; needs COM |
//! | 3 | Registry (`App Paths`, `Uninstall`) | `"registry"` | Most complete names, and the only source needing access-denied reporting |
//!
//! The order is a ranking input, not a filter: all enabled sources are consulted so
//! ambiguity can be detected. Stopping at the first hit is how an orchestrator ends up
//! launching whatever shadowed the real thing on someone's `PATH`.
//!
//! ## Ambiguity is a result, not an error
//!
//! When several candidates match, they are all returned, ranked (exact name, then
//! prefix, then substring; earlier source before later source), with
//! [`DiscoveryOutcome::ambiguous`] set. The caller refuses to launch rather than
//! picking. Never silently pick the first: "first" depends on directory enumeration
//! order, which is not a property anyone agreed to.
//!
//! ## Bounds
//!
//! Candidate count, directories opened, entries examined, and a wall-clock deadline,
//! each reported when hit. A discovery run must not walk `C:\Users\*\AppData` for
//! every profile or recurse `%WINDIR%`; both are unbounded in practice and neither is
//! where an application's *name* is advertised.
//!
//! ## Not here
//!
//! No process spawning, no elevation, no Microsoft Store / UWP package lookup (that
//! needs `Win32_System_Deployment`, outside this build's feature set), and no caching
//! across calls — a cached candidate list is a stale one, and installs happen while
//! J.E.A.N. is running.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{Result, ToolError};

// ─────────────────────────────────────────────────────────────────────────────
// Vocabulary shared with jean-protocol's `tools.discovery` list.
// ─────────────────────────────────────────────────────────────────────────────

/// One discovery source, as named in configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiscoverySource {
    /// `PATH` + `PATHEXT`.
    Path,
    /// Start Menu `.lnk` files, per-user and machine-wide.
    StartMenu,
    /// `App Paths` and `Uninstall` registry keys.
    Registry,
}

/// Which hive a registry target lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hive {
    CurrentUser,
    LocalMachine,
}

impl Hive {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Hive::CurrentUser => "HKCU",
            Hive::LocalMachine => "HKLM",
        }
    }
}

/// One (hive, subkey) pair to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryTarget {
    pub(crate) hive: Hive,
    pub(crate) sub_key: String,
    /// Which shape the subkey has, so the reader knows how to interpret its children.
    pub(crate) layout: RegistryLayout,
}

/// The two registry shapes discovery understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryLayout {
    /// `App Paths`: each child key's default value is the executable, with an optional
    /// `Path` value supplementing `PATH` for that application alone.
    AppPaths,
    /// `Uninstall`: each child key carries `DisplayName`, `InstallLocation`,
    /// `DisplayIcon`.
    Uninstall,
}

impl DiscoverySource {
    /// The exact spelling used in `tools.discovery`.
    pub fn as_str(self) -> &'static str {
        match self {
            DiscoverySource::Path => "path",
            DiscoverySource::StartMenu => "start_menu",
            DiscoverySource::Registry => "registry",
        }
    }

    /// Parse one configured value. Unknown values yield `None` and are reported by
    /// the caller rather than guessed at — `jean-protocol` already refuses them at
    /// validation time, so reaching here with one means config was bypassed.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "path" => Some(DiscoverySource::Path),
            "start_menu" => Some(DiscoverySource::StartMenu),
            "registry" => Some(DiscoverySource::Registry),
            _ => None,
        }
    }

    /// Ranking weight: lower sorts earlier.
    pub(crate) const fn rank(self) -> u8 {
        match self {
            DiscoverySource::Path => 0,
            DiscoverySource::StartMenu => 1,
            DiscoverySource::Registry => 2,
        }
    }
}

/// How well a candidate's name matched the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchKind {
    /// Normalised names equal, ignoring the `.exe` suffix.
    Exact,
    /// Candidate name starts with the query.
    Prefix,
    /// Query appears anywhere in the candidate name.
    Substring,
}

impl MatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchKind::Exact => "exact",
            MatchKind::Prefix => "prefix",
            MatchKind::Substring => "substring",
        }
    }
}

/// Which bound stopped the run, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryBound {
    /// The sources were exhausted. The only value that means "this list is complete".
    None,
    /// `max_candidates` reached.
    MaxCandidates,
    /// `max_directories` reached.
    MaxDirectories,
    /// `deadline_ms` elapsed.
    Deadline,
    /// `max_entries` directory entries examined.
    MaxEntries,
}

impl DiscoveryBound {
    pub fn as_str(self) -> &'static str {
        match self {
            DiscoveryBound::None => "none",
            DiscoveryBound::MaxCandidates => "max_candidates",
            DiscoveryBound::MaxDirectories => "max_directories",
            DiscoveryBound::Deadline => "deadline",
            DiscoveryBound::MaxEntries => "max_entries",
        }
    }

    pub fn was_hit(self) -> bool {
        self != DiscoveryBound::None
    }
}

/// Where one candidate was seen.
///
/// Kept separate from the candidate because deduplication merges sources: the operator
/// is entitled to know that `Code.exe` came from both the Start Menu and `App Paths`,
/// since that agreement is evidence and a single provenance is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub source: DiscoverySource,
    /// The file or key the information came from — a `.lnk` path, a PATH directory,
    /// or a registry key path.
    pub seen_at: String,
    /// For `App Paths` entries: the key's `Path` sub-value, which Windows prepends to
    /// `PATH` for that application alone. Recorded because it changes which DLLs the
    /// exe resolves against, and therefore which binary actually runs.
    pub app_path_value: Option<String>,
}

/// One discovered application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppCandidate {
    /// Human-facing name: the `.lnk` base name, the `DisplayName`, or the exe stem.
    pub display_name: String,
    /// Resolved executable, when resolution succeeded.
    ///
    /// `None` does not mean "not installed": a broken shortcut and an Uninstall entry
    /// with only an `InstallLocation` both leave it empty. The candidate stays in the
    /// list because dropping it would make "no such application" a lie.
    pub exe_path: Option<PathBuf>,
    pub sources: Vec<Provenance>,
    pub match_kind: MatchKind,
    /// A `.lnk` whose target does not exist. Reported, never dropped: the user sees
    /// that entry in their Start Menu, so being told it does not resolve is useful
    /// and being told it does not exist is wrong.
    pub is_broken: bool,
    /// Registry `InstallLocation` when known — the directory to look in for the rest
    /// of the application's files.
    pub install_location: Option<PathBuf>,
}

impl AppCandidate {
    /// True when this candidate can be handed to a launcher.
    pub fn is_launchable(&self) -> bool {
        self.exe_path.is_some() && !self.is_broken
    }

    /// Merge another sighting of the same executable.
    ///
    /// Keeps the better (lower) match kind and orders provenance by source rank, so
    /// the merged candidate sorts as if it had been found through its best source.
    fn absorb(&mut self, other: &AppCandidate) {
        for s in &other.sources {
            if !self.sources.contains(s) {
                self.sources.push(s.clone());
            }
        }
        self.sources.sort_by_key(|p| p.source.rank());
        if other.match_kind < self.match_kind {
            self.match_kind = other.match_kind;
        }
        if self.exe_path.is_none() {
            self.exe_path = other.exe_path.clone();
        }
        if self.install_location.is_none() {
            self.install_location = other.install_location.clone();
        }
        // A candidate that resolved through some other source is not "broken" merely
        // because a shortcut pointing somewhere else failed to stat.
        if self.exe_path.is_some() {
            self.is_broken = false;
        }
    }
}

/// Everything a discovery run knows when it stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryOutcome {
    pub query: String,
    /// Ranked best-first. Empty means nothing matched in any consulted source.
    pub candidates: Vec<AppCandidate>,
    /// More than one candidate matched. The caller must ask, not choose.
    pub ambiguous: bool,
    /// Set when a bound stopped the run before the sources were exhausted.
    pub bound: DiscoveryBound,
    /// Registry subtrees this account could not open, with their Win32 codes.
    ///
    /// Reported rather than skipped: "not found" and "not readable" are different
    /// answers, and conflating them tells a user their application is missing when it
    /// is only invisible to them.
    pub inaccessible_keys: Vec<(String, u32)>,
    /// Sources actually consulted, in rank order.
    pub sources_used: Vec<DiscoverySource>,
    /// Directories opened during the run, against `max_directories`.
    pub directories_scanned: usize,
    pub entries_examined: u64,
    pub elapsed_ms: u64,
    /// Notes worth showing next to the result — chiefly why a `.bat`/`.cmd`/`.ps1`
    /// found on `PATH` was refused rather than offered.
    pub notes: Vec<String>,
}

impl DiscoveryOutcome {
    /// The single unambiguous launch target, if there is exactly one and it is
    /// launchable.
    ///
    /// Returning `Option` instead of picking makes refusal the default path: an
    /// ambiguous result cannot be launched by accident, only after the caller asks.
    pub fn sole_launchable(&self) -> Option<&AppCandidate> {
        if self.ambiguous || self.candidates.len() != 1 {
            return None;
        }
        self.candidates.first().filter(|c| c.is_launchable())
    }
}

/// Bounds for one discovery run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryLimits {
    /// Stop collecting after this many distinct candidates.
    pub max_candidates: usize,
    /// Stop opening directories after this many. Covers `PATH` entries and Start Menu
    /// trees alike, because both can be arbitrarily large.
    pub max_directories: usize,
    /// Wall-clock budget for the whole run, milliseconds.
    pub deadline_ms: u64,
    /// Directory entries examined before giving up, across all sources.
    pub max_entries: u64,
    /// Sources enabled by `tools.discovery`.
    pub sources: Vec<DiscoverySource>,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        DiscoveryLimits {
            max_candidates: 12,
            max_directories: 64,
            // Generous for three cheap sources, short enough that a hung shell
            // namespace cannot turn `jean apps` into a command nobody can interrupt.
            deadline_ms: 2_000,
            max_entries: 20_000,
            sources: vec![
                DiscoverySource::Path,
                DiscoverySource::StartMenu,
                DiscoverySource::Registry,
            ],
        }
    }
}

impl DiscoveryLimits {
    /// Build limits from the configured `tools.discovery` strings.
    ///
    /// Values that are not source names are collected rather than rejected: config
    /// validation should have caught them, and a run that dies over one stray entry
    /// is worse than a run that reports the entry was ignored.
    pub fn from_discovery_list(list: &[String]) -> (Self, Vec<String>) {
        let mut limits = DiscoveryLimits::default();
        let mut ignored = Vec::new();
        let mut enabled = Vec::new();
        for raw in list {
            match DiscoverySource::parse(raw) {
                Some(s) if !enabled.contains(&s) => enabled.push(s),
                Some(_) => {}
                None => ignored.push(raw.clone()),
            }
        }
        if !enabled.is_empty() {
            enabled.sort_by_key(|s| s.rank());
            limits.sources = enabled;
        }
        (limits, ignored)
    }

    fn enables(&self, source: DiscoverySource) -> bool {
        self.sources.contains(&source)
    }
}

/// The environment values discovery reads, passed in rather than fetched.
///
/// A test must be able to point `PATH` and the Start Menu at fixture directories
/// without touching the machine, and the production caller already holds an
/// allowlisted environment view from `jean-core`. Reading `std::env` inside the walk
/// would make both impossible.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryEnv {
    /// Raw `PATH` value, semicolon-separated.
    pub path: Option<String>,
    /// Raw `PATHEXT`; absent means "only `.exe` may be appended".
    pub pathext: Option<String>,
    /// Per-user Start Menu root.
    pub user_start_menu: Option<PathBuf>,
    /// Machine-wide Start Menu root.
    pub machine_start_menu: Option<PathBuf>,
    /// Additional roots treated as Start Menus — for fixtures, and for an operator who
    /// keeps shortcuts elsewhere. Each is still walked under the same bounds.
    pub extra_start_menu_roots: Vec<PathBuf>,
}

impl DiscoveryEnv {
    /// Read the environment a normal interactive session would use.
    ///
    /// Only variables already in `jean_protocol::config::default_env_allowlist()` are
    /// consulted, so this cannot reach past what the project permits a child to see.
    /// `SHGetKnownFolderPath` is deliberately not used: it needs `Win32_UI_Shell`'s
    /// known-folder surface plus a `PWSTR` free, and these two directories are stable,
    /// documented, and derived from `APPDATA`/`PROGRAMDATA` which are already
    /// allowlisted. See the report note about that API.
    pub fn from_process_environment() -> Self {
        let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
        let programdata = std::env::var_os("PROGRAMDATA").map(PathBuf::from);
        DiscoveryEnv {
            path: std::env::var("PATH").ok(),
            pathext: std::env::var("PATHEXT").ok(),
            user_start_menu: appdata
                .map(|a| a.join("Microsoft").join("Windows").join("Start Menu")),
            machine_start_menu: programdata
                .map(|p| p.join("Microsoft").join("Windows").join("Start Menu")),
            extra_start_menu_roots: Vec::new(),
        }
    }

    /// True when this build can ask the shell where a `.lnk` points.
    ///
    /// The supported way to ask is `IShellLinkW`, and its two features
    /// (`Win32_UI_Shell`, `Win32_System_Com`) are in the workspace's declared set, so
    /// on Windows this is always true. It stays a function rather than a constant
    /// because the question can still be answered "no" at run time: COM can refuse to
    /// start, and `resolve_link_target` then reports `Unsupported` per link instead of
    /// failing the discovery run.
    pub const fn link_resolution_supported() -> bool {
        cfg!(windows)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Name normalisation.
// ─────────────────────────────────────────────────────────────────────────────

/// Strip a trailing `.exe`, case-insensitively.
///
/// Windows convention says `notepad` and `Notepad.exe` name the same application, so
/// the suffix is removed from both sides of every comparison. Only `.exe`: stripping
/// any extension would turn `tool.dll` into a match for `tool`, and a DLL is not
/// something you launch.
pub fn strip_exe(name: &str) -> &str {
    let bytes = name.as_bytes();
    // Compared case-insensitively: Windows folds the case of an extension, so
    // `NOTEPAD.EXE` and `notepad.exe` are the same name and must classify the same
    // way. A byte-exact comparison here silently drops every uppercase-named
    // application in a real Start Menu.
    if bytes.len() > 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".exe") {
        &name[..bytes.len() - 4]
    } else {
        name
    }
}

/// Lowercase ASCII, for Windows-flavoured case-insensitive comparison.
///
/// ASCII-only on purpose. Windows folds path case with a locale-dependent table, and
/// adopting full Unicode lowercase would make `straße` and `STRASSE` match when the
/// filesystem says they are two different names. Erring toward fewer matches is the
/// safe direction here: a missed match produces a question, a false match produces a
/// launch.
fn fold(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// Classify a candidate name against a query, best-first.
///
/// `None` when the query does not appear at all. Comparison is on folded,
/// suffix-stripped text, so the query `Notepad` matches a candidate called
/// `notepad.exe` as [`MatchKind::Exact`].
pub fn classify_match(query: &str, candidate_name: &str) -> Option<MatchKind> {
    let q = fold(strip_exe(query.trim()));
    let c = fold(strip_exe(candidate_name.trim()));
    if q.is_empty() || c.is_empty() {
        return None;
    }
    if q == c {
        Some(MatchKind::Exact)
    } else if c.starts_with(&q) {
        Some(MatchKind::Prefix)
    } else if c.contains(&q) {
        Some(MatchKind::Substring)
    } else {
        None
    }
}

/// Canonical key used to deduplicate candidates.
///
/// Case-folded absolute path. Deliberately **not** `canonicalize()`: that opens the
/// file, and opening is how a reparse point gets followed — this module's rule is
/// inspection only. Two spellings of one path therefore merge, while a junction onto a
/// second copy of an application stays its own candidate with its own provenance,
/// which is the safer answer when the question is "which one did the user mean".
fn dedupe_key(path: &Path) -> String {
    fold(&path.to_string_lossy())
}

/// Make a path absolute without touching the filesystem.
///
/// `std::fs::canonicalize` would resolve reparse points and require the file to exist,
/// which is exactly the inspection this module refuses to perform. Relative components
/// are kept as written: a relative `InstallLocation` is reported as one, not laundered
/// into a guess.
fn absolutise(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Source 1: PATH.
// ─────────────────────────────────────────────────────────────────────────────

/// The only extension appended when a bare name has none.
///
/// Mirrors `jean_core::resolve::SEARCH_EXTENSIONS`. Reimplemented rather than reused
/// because of the dependency direction: `jean-core` depends on this crate, so this
/// crate cannot depend back on it. The test
/// `path_extension_policy_matches_the_supervisor` states the invariant.
const SEARCH_EXTENSION: &str = ".exe";

/// Extensions that mean "hand this file to an interpreter".
///
/// Same exclusion list as the supervisor, for the same reason: resolving to one of
/// these would mean J.E.A.N. launched `cmd.exe` or PowerShell to interpret a script
/// nobody reviewed, and the "no shell strings" rule exists precisely to prevent that.
const SCRIPT_EXTENSIONS: &[&str] = &[
    ".cmd", ".bat", ".com", ".scr", ".vbs", ".vbe", ".js", ".jse", ".wsf", ".wsh", ".msc", ".ps1",
    ".psm1", ".lnk",
];

/// Split a raw `PATH` into directories, dropping empties and duplicates.
///
/// Quoted entries are legal on Windows and the quotes are not part of the path.
fn search_dirs(env_path: Option<&str>) -> Vec<PathBuf> {
    let Some(raw) = env_path else {
        return Vec::new();
    };
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for entry in raw.split(';') {
        let trimmed = entry.trim().trim_matches('"');
        if trimmed.is_empty() {
            continue;
        }
        let key = fold(trimmed);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(PathBuf::from(trimmed));
    }
    out
}

/// The extensions worth appending, `PATHEXT` minus interpreters.
fn executable_extensions(path_ext: Option<&str>) -> Vec<String> {
    let configured: Vec<String> = match path_ext {
        Some(raw) => raw
            .split(';')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(|e| {
                let mut e = e.to_ascii_lowercase();
                if !e.starts_with('.') {
                    e.insert(0, '.');
                }
                e
            })
            .collect(),
        None => vec![SEARCH_EXTENSION.to_owned()],
    };
    let kept: Vec<String> = configured
        .into_iter()
        .filter(|e| !SCRIPT_EXTENSIONS.contains(&e.as_str()))
        .collect();
    if kept.is_empty() {
        vec![SEARCH_EXTENSION.to_owned()]
    } else {
        kept
    }
}

/// Collect candidates whose base name matches `query` among `dirs`.
///
/// Reads each directory's *names* only — never contents, never more metadata than
/// "is this a file" — so a `PATH` entry pointing at `System32` costs one directory
/// read rather than a recursion. Matches carrying a script extension become a note.
fn scan_path(query: &str, dirs: &[PathBuf], exts: &[String], ctx: &mut RunCtx) {
    for dir in dirs {
        if ctx.out_of_budget() {
            return;
        }
        if !ctx.reserve_directory() {
            return;
        }
        let read = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            // A stale or unreadable PATH entry is ordinary: people uninstall things.
            // Skipping it is right; aborting the run over it would be a lie about
            // where the failure came from.
            Err(_) => continue,
        };
        for entry in read.flatten() {
            if ctx.out_of_budget() {
                return;
            }
            ctx.count_entry();
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let Some(kind) = classify_match(query, &file_name) else {
                continue;
            };
            let path = entry.path();
            let ext = path
                .extension()
                .map(|e| format!(".{}", fold(&e.to_string_lossy())))
                .unwrap_or_default();

            if SCRIPT_EXTENSIONS.contains(&ext.as_str()) {
                // Every matched script is annotated, not only the exact-name ones.
                // `RunCtx::note` de-duplicates and caps the list, so the noise is
                // bounded; what it must not do is go silent, because an operator who
                // typed `jean apps jeanscript`, got nothing back and no explanation
                // would conclude the tool is not installed when it is only not
                // launchable.
                ctx.note(format!(
                    "{} at {} was found but is refused: a `{}` file is handed to an \
                     interpreter, and this project launches executables only",
                    file_name,
                    path.display(),
                    ext.trim_start_matches('.')
                ));
                continue;
            }

            let known = ext.is_empty() || exts.iter().any(|x| x == &ext);
            if !known {
                continue;
            }
            if !entry.metadata().map(|m| m.is_file()).unwrap_or(false) {
                continue;
            }

            // An extensionless name is only launchable if appending the one permitted
            // extension finds a real file: `dir\foo` cannot be started, `dir\foo.exe`
            // can. Checked by existence, never by executing either candidate.
            let resolved = if ext.is_empty() {
                let with_ext = format!("{}{}", path.display(), SEARCH_EXTENSION);
                if Path::new(&with_ext).is_file() {
                    PathBuf::from(with_ext)
                } else {
                    continue;
                }
            } else {
                path.clone()
            };

            ctx.add_candidate(AppCandidate {
                display_name: strip_exe(&file_name).to_owned(),
                exe_path: Some(resolved),
                sources: vec![Provenance {
                    source: DiscoverySource::Path,
                    seen_at: dir.display().to_string(),
                    app_path_value: None,
                }],
                match_kind: kind,
                is_broken: false,
                install_location: None,
            });
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Source 2: Start Menu `.lnk` files.
// ─────────────────────────────────────────────────────────────────────────────

/// Depth cap for one Start Menu tree.
///
/// The Programs folder is nested by vendor, six levels deep is far beyond anything a
/// real installer produces, and the alternative — unbounded descent — turns a
/// directory loop into a hang. Bounds are what makes this function safe to call on a
/// directory someone else wrote.
const MAX_LNK_DEPTH: u32 = 6;

/// Walk the Start Menu roots for `*.lnk` and resolve each target by API.
///
/// Reparse points are recorded and never entered, matching [`crate::search`]: the
/// Start Menu is attacker-writable, so a junction is exactly the thing not to follow,
/// and following one leaves the root the caller authorised.
fn scan_start_menu(query: &str, roots: &[PathBuf], ctx: &mut RunCtx) {
    for root in roots {
        if ctx.out_of_budget() {
            return;
        }
        if !root.is_dir() {
            continue;
        }
        let mut stack: Vec<(PathBuf, u32)> = vec![(root.clone(), 0)];
        while let Some((dir, depth)) = stack.pop() {
            if ctx.out_of_budget() {
                return;
            }
            if !ctx.reserve_directory() {
                return;
            }
            let read = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for entry in read.flatten() {
                if ctx.out_of_budget() {
                    return;
                }
                ctx.count_entry();
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let md = match entry.metadata() {
                    Ok(md) => md,
                    Err(_) => continue,
                };

                use std::os::windows::fs::MetadataExt;
                let attrs = md.file_attributes();
                if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    // Named in the notes rather than silently omitted: the whole point
                    // of this module is that the operator learns where a name came
                    // from, including from a door we chose not to open.
                    ctx.note(format!(
                        "skipped reparse point inside the Start Menu: {}",
                        path.display()
                    ));
                    continue;
                }
                if md.is_dir() {
                    if depth < MAX_LNK_DEPTH {
                        stack.push((path, depth + 1));
                    }
                    continue;
                }
                if !fold(&name).ends_with(".lnk") {
                    continue;
                }
                let stem = name[..name.len() - 4].to_owned();
                let Some(kind) = classify_match(query, &stem) else {
                    continue;
                };
                let (exe_path, is_broken) = match resolve_link_target(&path) {
                    LinkTarget::Resolved(target) => {
                        let exists = target.is_file();
                        (if exists { Some(target) } else { None }, !exists)
                    }
                    // The shell was asked and had no target to give — which includes a
                    // file that is not a readable link at all. Either way this
                    // candidate has no launchable executable and says so, so `broken`
                    // is the honest flag: `is_launchable` reads it, and a candidate
                    // that is neither launchable nor flagged tells the operator
                    // nothing.
                    LinkTarget::Missing => (None, true),
                    LinkTarget::Failed(reason) => {
                        // A link this build could not parse. Broken, because there is
                        // no launchable executable behind it, and annotated with what
                        // the shell actually returned so "corrupt" is distinguishable
                        // from "on a drive that is not mounted".
                        ctx.note(format!(
                            "{} is unreadable as a shortcut: {reason}",
                            path.display()
                        ));
                        (None, true)
                    }
                    LinkTarget::Unsupported(reason) => {
                        // We could not ask at all. That is a limitation of this run,
                        // not a fact about the shortcut, so the entry keeps no target
                        // and nothing is called broken. The reason is carried into the
                        // note because "COM was unavailable" on a machine where COM
                        // normally works is a question with an answer in the code.
                        ctx.note(format!(
                            "the shell-link interface was unavailable ({reason}), so Start \
                             Menu shortcuts are listed by name without a target"
                        ));
                        (None, false)
                    }
                };
                ctx.add_candidate(AppCandidate {
                    display_name: stem,
                    exe_path,
                    sources: vec![Provenance {
                        source: DiscoverySource::StartMenu,
                        seen_at: path.display().to_string(),
                        app_path_value: None,
                    }],
                    match_kind: kind,
                    is_broken,
                    install_location: None,
                });
            }
        }
    }
}

/// The attribute bits this module needs, spelled locally.
///
/// Keeping the numbers here rather than importing `windows` in the scanning logic
/// leaves the walk readable without a Win32 reference open. These are ABI-stable and
/// identical to `FILE_ATTRIBUTE_*` in `Win32_Storage_FileSystem`.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Outcome of asking the shell where a `.lnk` points.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkTarget {
    /// The target path the shell reports. Existence is the caller's question.
    Resolved(PathBuf),
    /// Resolution succeeded and the link records no target.
    Missing,
    /// This build cannot ask at all: the shell-link CLSID is not registered, or COM
    /// refused to initialise on this thread. A limitation of the run, not a fact
    /// about the shortcut, so links stay listed without a target and nothing is
    /// called broken. The string is what the failing call reported, because "COM was
    /// unavailable" without a code is not actionable on a machine where it normally
    /// works.
    Unsupported(String),
    /// The shell was reached and this particular file could not be read, with the
    /// HRESULT text that says so. A corrupt or truncated link lands here, and is
    /// reported as broken — with a reason — rather than silently having no target.
    Failed(String),
}

#[cfg(windows)]
fn resolve_link_target(link: &Path) -> LinkTarget {
    // Initialising COM is per-thread bookkeeping, not an action with side effects on
    // the link, so it is attempted lazily once per discovery run rather than per file.
    if let Err(err) = ensure_com() {
        tracing::debug!(error = %err, "COM unavailable for shortcut resolution");
        return LinkTarget::Unsupported(format!("CoInitializeEx: {err}"));
    }
    link_target_inner(link)
}

/// Initialise COM on this thread at most once, remembering the outcome.
///
/// Three results matter and they are not interchangeable:
///
/// * `S_OK` — we initialised it, so we must uninitialise it.
/// * `S_FALSE` — it was already initialised *in this model* on this thread; ours is a
///   no-op refcount bump, and uninitialising would tear down somebody else's COM.
/// * `RPC_E_CHANGED_MODE` — already initialised in a *different* model (the host
///   chose MTA and we asked for STA, or the reverse). That is fine for a read-only
///   call, and equally: we did not initialise it, so we do not uninitialise it.
///
/// Anything else is a genuine failure and is reported.
#[cfg(windows)]
fn ensure_com() -> std::result::Result<(), windows::core::HRESULT> {
    use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_ok() {
            COM_OWNED.with(|owned| owned.set(true));
            Ok(())
        } else if hr == S_FALSE || hr == RPC_E_CHANGED_MODE {
            // Already running on this thread in a compatible or incompatible model:
            // usable, and not ours to shut down.
            Ok(())
        } else {
            Err(hr)
        }
    }
}

#[cfg(windows)]
thread_local! {
    /// Whether *this* call to `CoInitializeEx` created the apartment, which decides
    /// whether `CoUninitialize` is ours to call.
    static COM_OWNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Release COM if and only if this thread acquired it.
#[cfg(windows)]
pub fn shutdown_com() {
    COM_OWNED.with(|owned| {
        if owned.replace(false) {
            unsafe { windows::Win32::System::Com::CoUninitialize() }
        }
    })
}

#[cfg(not(windows))]
pub fn shutdown_com() {}

#[cfg(windows)]
fn link_target_inner(link: &Path) -> LinkTarget {
    use windows::core::{Interface, HSTRING, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ, STGM_SHARE_EXCLUSIVE,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLGP_RAWPATH};

    unsafe {
        let wide = HSTRING::from(link.as_os_str());
        let shell_link: IShellLinkW =
            // `ShellLink` is windows-rs' published class id for `CLSID_ShellLink`,
            // `{00021401-0000-0000-C000-000000000046}`. Taking it from the crate
            // instead of spelling bytes here matters: the id this code carried before
            // was `{461C7B78-…}`, which is the *shortcut file* class used for name
            // parsing, not the shell-link object — and `CoCreateInstance` answered
            // `CLASS_E_CLASSNOTAVAILABLE` for every shortcut on a machine where the
            // real class is registered. A wrong constant here is indistinguishable
            // from "this Windows has no shell links", which is exactly the kind of
            // silence this crate is not allowed to produce.
            match CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) {
                Ok(obj) => obj,
                Err(e) => {
                    tracing::debug!(error = %e, "the shell-link class is unavailable");
                    return LinkTarget::Unsupported(format!("CoCreateInstance: {e}"));
                }
            };
        let persist: IPersistFile = match shell_link.cast() {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(error = %e, "shell link does not expose IPersistFile");
                return LinkTarget::Unsupported(format!("QueryInterface(IPersistFile): {e}"));
            }
        };
        // READ + SHARE_EXCLUSIVE: the shell's own parser reads the file; taking a
        // share-exclusive mode avoids leaving the link locked for its owner while we
        // look at it, and there is nothing to write.
        if let Err(e) = persist.Load(PCWSTR(wide.as_ptr()), STGM_READ | STGM_SHARE_EXCLUSIVE) {
            // Not-a-link, a truncated link and a link whose drive is gone all arrive
            // here. The code is what tells the operator which, so it is kept.
            return LinkTarget::Failed(format!("{} ({e})", link.display()));
        }
        // `MAX_PATH` stopped being a limit long ago, and `GetPath` writes into a buffer
        // we size. 4096 UTF-16 units covers a legal extended-length path with room to
        // spare; a target longer than that is not something this run can offer anyway.
        let mut buffer = [0u16; 4096];
        if shell_link
            .GetPath(&mut buffer, core::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
            .is_err()
        {
            // A parsed link that cannot answer "where do you point" is a link with no
            // target, which is `Missing` rather than a failure of this call.
            return LinkTarget::Missing;
        }
        let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        if end == 0 {
            return LinkTarget::Missing;
        }
        let text = String::from_utf16_lossy(&buffer[..end]);
        if text.trim().is_empty() {
            return LinkTarget::Missing;
        }
        LinkTarget::Resolved(PathBuf::from(text))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Source 3: registry.
// ─────────────────────────────────────────────────────────────────────────────

/// `App Paths` key, relative to a hive.
pub const APP_PATHS_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths";
/// `Uninstall` key, relative to a hive.
pub const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";

/// The key pairs consulted, in rank order.
///
/// Both `Wow6432Node` variants are visited explicitly rather than relying on registry
/// redirection, because a 64-bit process reading `HKLM\SOFTWARE\...` sees only the
/// 64-bit view and would miss every 32-bit installer. Naming the node in the path also
/// makes `inaccessible_keys` actionable: it says which view was blocked.
///
/// `REG_OPTION_BACKUP_RESTORE` is *not* requested anywhere. It would ask Windows to
/// open the key through the backup privilege, which succeeds only for an elevated
/// process and would make a permission failure look like a successful read of a
/// different security context. A key this account cannot open is reported as
/// inaccessible instead.
fn registry_targets() -> Vec<RegistryTarget> {
    let mut out = Vec::new();
    for hive in [Hive::CurrentUser, Hive::LocalMachine] {
        for node in ["", r"Wow6432Node\"] {
            for (suffix, layout) in [
                (APP_PATHS_KEY, RegistryLayout::AppPaths),
                (UNINSTALL_KEY, RegistryLayout::Uninstall),
            ] {
                out.push(RegistryTarget {
                    hive,
                    // The hive-relative path, `SOFTWARE` included: `RegOpenKeyExW` takes
                    // exactly this string, and keeping the prefix here means the path
                    // reported to the operator is the path that was opened.
                    sub_key: format!(r"SOFTWARE\{node}{suffix}"),
                    layout,
                });
            }
        }
    }
    out
}

/// Shape one registry entry contributes to a candidate.
///
/// Public because the shaping rules are the part of this source testable without
/// depending on what happens to be installed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegistryAppEntry {
    /// Key name (`App Paths` child) or `DisplayName` (`Uninstall` child).
    pub name: String,
    /// The child key's default value, for `App Paths`.
    pub default_value: Option<String>,
    /// The `Path` value of an `App Paths` child.
    pub path_value: Option<String>,
    /// `InstallLocation` from an Uninstall child.
    pub install_location: Option<String>,
    /// `DisplayIcon` from an Uninstall child.
    pub display_icon: Option<String>,
}

/// Turn one registry entry into an executable path, if it names one.
///
/// Four shapes occur and all must work:
///
/// * `App Paths` default value that is already absolute (`C:\Windows\System32\notepad.exe`).
/// * `App Paths` default value that is a bare file name (`notepad.exe`), joined with
///   that key's own `Path` value — which is exactly what the shell does.
/// * `DisplayIcon`, frequently written as `C:\app\app.exe,0` with a resource index.
///   The index is stripped; a non-`.exe` target is refused rather than guessed at.
/// * `InstallLocation` with nothing else. Nothing is invented: no file name means no
///   executable, and the candidate is returned with `exe_path: None` so the operator
///   still learns the application exists.
///
/// The executable inside an `App Paths` default value: the first token, with the
/// surrounding quotes removed if there were any.
///
/// Returns the input unchanged when it does not start with a quote, because the
/// common form is a bare path and the caller has already trimmed it.
fn first_quoted_token(value: &str) -> &str {
    let rest = match value.strip_prefix('"') {
        Some(rest) => rest,
        None => return value,
    };
    // A doubled quote inside a quoted path is an escaped quote in the Windows
    // command-line grammar, so the closing quote is the first one that is not
    // followed by another.
    // A doubled quote inside a quoted path is an escaped quote in the Windows
    // command-line grammar. The scan below skips such a pair so it does not end the
    // token early; the returned slice keeps the doubled quote as written, because a
    // path containing one is not a path an installer would produce, and inventing an
    // unescaping rule here would be a second grammar to get right.
    let bytes = rest.as_bytes();
    let mut index = 0;
    while let Some(found) = rest[index..].find('"') {
        let absolute = index + found;
        if bytes.get(absolute + 1) == Some(&b'"') {
            index = absolute + 2;
            continue;
        }
        return &rest[..absolute];
    }
    // An unbalanced quote is malformed input; the whole string is the best guess at
    // what was meant, and it will fail the absoluteness test and be reported.
    rest
}

pub fn registry_executable(entry: &RegistryAppEntry) -> Option<PathBuf> {
    if let Some(default) = entry.default_value.as_deref().map(str::trim) {
        if !default.is_empty() {
            // `App Paths` default values are very often written by installers as a
            // quoted executable followed by arguments — `"C:\Apppp.exe" --quiet`.
            // Taking the whole string as a path yields a name that starts with a
            // quote character, which is never launchable and which
            // `jean launch` would then report as a missing application. The first
            // token is the executable; the rest is not part of a path.
            let default = first_quoted_token(default);
            let p = Path::new(default);
            if p.is_absolute() {
                return Some(PathBuf::from(clean_icon_suffix(p.as_os_str())));
            }
            if let Some(dir) = entry.path_value.as_deref().map(str::trim) {
                if !dir.is_empty() {
                    let joined = join_install_location(Path::new(dir), default);
                    if let Some(j) = joined.filter(|p| p.is_absolute()) {
                        return Some(j);
                    }
                }
            }
            // Neither absolute nor resolvable. Reporting no path beats reporting one
            // invented from the current directory, which would be a guess with the
            // authority of a registry read behind it.
            return None;
        }
    }
    if let Some(icon) = entry.display_icon.as_deref().map(str::trim) {
        if !icon.is_empty() {
            let cleaned = clean_icon_suffix(std::ffi::OsStr::new(icon));
            if fold(&cleaned.to_string_lossy()).ends_with(".exe") {
                return Some(PathBuf::from(cleaned));
            }
            // An `.ico` or `.dll` icon resource is not something you launch, and guessing
            // which sibling exe the installer meant is not this module's job.
            return None;
        }
    }
    None
}

/// Strip a trailing `,<index>` from an icon reference.
///
/// Installers write `DisplayIcon` as `path,index`. Leaving the index in yields a path
/// that cannot exist, and a candidate that looks broken when it is fine. Only a numeric
/// tail counts, so `app,0` is trimmed while `my,shop.exe` is left alone.
///
/// Takes and returns `OsString` rather than `&Path`: the trimmed form is a slice of a
/// temporary lossy conversion, so a borrowed return would either dangle or force the
/// caller to keep the original alive for no reason.
pub fn clean_icon_suffix(path: &std::ffi::OsStr) -> std::ffi::OsString {
    let text = path.to_string_lossy();
    match text.rfind(',') {
        // A negative resource index is the common form — `app.exe,-101` points at a
        // string resource rather than an icon — and dropping only the positive ones
        // would leave a path that ends in `,-101`, which no launcher can open.
        Some(idx)
            if idx > 0
                && idx + 1 < text.len()
                && text[idx + 1..]
                    .strip_prefix('-')
                    .unwrap_or(&text[idx + 1..])
                    .chars()
                    .all(|c| c.is_ascii_digit())
                && !text[idx + 1..].chars().all(|c| !c.is_ascii_digit()) =>
        {
            std::ffi::OsString::from(&text[..idx])
        }
        _ => path.to_owned(),
    }
}

/// Join a location with a candidate file name, refusing parent-directory escapes.
///
/// Installers write nonsense into these keys. `..\..\Windows\System32\x.exe` in
/// `DisplayIcon` must not become a candidate path. Discovery never executes, so this
/// is hygiene rather than a sandbox — but the field is read by a launcher later, and a
/// path that survives here is a path somebody might act on.
pub fn join_install_location(location: &Path, file: &str) -> Option<PathBuf> {
    let rel = Path::new(file);
    if rel.is_absolute() {
        return Some(rel.to_path_buf());
    }
    if rel.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    Some(location.join(rel))
}

/// Full key path as it appears in `regedit`, e.g.
/// `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths`.
///
/// Built from the same two pieces that are handed to `RegOpenKeyExW`, so a code quoted
/// against this string describes the key that produced it.
pub fn full_key_path(target: &RegistryTarget) -> String {
    format!(r"{}\{}", target.hive.label(), target.sub_key)
}

/// Read the registry sources.
///
/// Name matching tries two identifiers per child and keeps the better rank: an
/// `App Paths` child is a file name (`notepad.exe`) while an `Uninstall` child is an
/// installer id (`{A1B2-C3D4}`) whose human name lives in `DisplayName`. Matching only
/// the key would make every uninstall entry searchable by GUID; matching only the
/// display name would lose the applications that omit it.
fn scan_registry(query: &str, ctx: &mut RunCtx) {
    for target in registry_targets() {
        if ctx.out_of_budget() {
            return;
        }
        ctx.count_directory();
        let children = match reg_enumerate_subkeys(&target) {
            Ok(children) => children,
            Err(code) => {
                // A key that simply is not there (no HKCU `App Paths` on a clean
                // install) is ordinary. Anything else â access denied, a protected
                // hive â is recorded, because "nothing registered" and "blocked from
                // looking" lead to different next actions.
                if code != ERROR_FILE_NOT_FOUND && code != ERROR_PATH_NOT_FOUND {
                    ctx.inaccessible.push((full_key_path(&target), code));
                }
                continue;
            }
        };
        for child in children {
            if ctx.out_of_budget() {
                return;
            }
            ctx.count_entry();
            let entry = read_registry_entry(&target, &child);
            let names: [&str; 2] = match target.layout {
                RegistryLayout::AppPaths => [strip_exe(&child), child.as_str()],
                RegistryLayout::Uninstall => [entry.name.as_str(), child.as_str()],
            };
            let mut best: Option<(MatchKind, String)> = None;
            for n in names {
                if let Some(kind) = classify_match(query, n) {
                    let better = match best {
                        Some((prev, _)) => kind < prev,
                        None => true,
                    };
                    if better {
                        best = Some((kind, n.to_owned()));
                    }
                }
            }
            let Some((kind, display)) = best else {
                continue;
            };
            let exe = registry_executable(&entry).map(|p| absolutise(&p));
            let install_location = entry
                .install_location
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from);
            ctx.add_candidate(AppCandidate {
                display_name: display,
                exe_path: exe,
                sources: vec![Provenance {
                    source: DiscoverySource::Registry,
                    seen_at: format!(r"{}\{}", full_key_path(&target), child),
                    app_path_value: entry.path_value.clone(),
                }],
                match_kind: kind,
                is_broken: false,
                install_location,
            });
        }
    }
}

/// Assemble one registry child key into the shape the shapers expect.
#[cfg(windows)]
fn read_registry_entry(target: &RegistryTarget, child: &str) -> RegistryAppEntry {
    let mut entry = RegistryAppEntry {
        name: child.to_owned(),
        ..Default::default()
    };
    match target.layout {
        RegistryLayout::AppPaths => {
            entry.default_value = reg_value(target, child, "");
            entry.path_value = reg_value(target, child, "Path");
        }
        RegistryLayout::Uninstall => {
            entry.name = reg_value(target, child, "DisplayName").unwrap_or_else(|| {
                // `DisplayName` is absent for some patches and updates. The key name is
                // the only identifier left, and it is what Add/Remove Programs shows
                // when the value is missing too.
                child.to_owned()
            });
            entry.install_location = reg_value(target, child, "InstallLocation");
            entry.display_icon = reg_value(target, child, "DisplayIcon");
        }
    }
    entry
}

#[cfg(not(windows))]
fn read_registry_entry(_target: &RegistryTarget, child: &str) -> RegistryAppEntry {
    // Nothing to read; the shaping helpers above are still exercised by tests.
    RegistryAppEntry {
        name: child.to_owned(),
        ..Default::default()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Live registry access. Every `unsafe` in this module is in this block.
// ─────────────────────────────────────────────────────────────────────────────

/// `ERROR_FILE_NOT_FOUND`, which `RegOpenKeyExW` also returns for a *missing* key —
/// the code that means "nobody registered this", as distinct from access denied.
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_NO_MORE_ITEMS: u32 = 259;

#[cfg(windows)]
type RegKey = windows::Win32::System::Registry::HKEY;

#[cfg(windows)]
fn open_hive(hive: Hive) -> RegKey {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    match hive {
        Hive::CurrentUser => HKEY_CURRENT_USER,
        Hive::LocalMachine => HKEY_LOCAL_MACHINE,
    }
}

#[cfg(not(windows))]
fn open_hive(_hive: Hive) {}

/// Open a key for reading.
///
/// Returns the Win32 code rather than skipping silently: the caller distinguishes
/// "not present" from "refused", and only `inaccessible_keys` can tell an operator
/// that a hive exists but their account cannot see inside it.
#[cfg(windows)]
fn reg_open(target: &RegistryTarget) -> std::result::Result<RegKey, u32> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{RegOpenKeyExW, KEY_READ, KEY_WOW64_64KEY};

    let hive = open_hive(target.hive);
    let wide: Vec<u16> = target
        .sub_key
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut key = RegKey::default();
    // `KEY_WOW64_64KEY` states which view to read instead of letting registry
    // redirection decide per binary bitness; without it the `Wow6432Node` targets below
    // would be redirected back to the 64-bit view and every 32-bit installer would go
    // missing on some machines. No `REG_OPTION_BACKUP_RESTORE`: see `registry_targets`.
    let flags = KEY_READ | KEY_WOW64_64KEY;
    let code = unsafe { RegOpenKeyExW(hive, PCWSTR(wide.as_ptr()), Some(0u32), flags, &mut key) };
    if code.0 == 0 {
        Ok(key)
    } else {
        Err(code.0)
    }
}

#[cfg(not(windows))]
fn reg_open(_target: &RegistryTarget) -> std::result::Result<(), u32> {
    Err(ERROR_PATH_NOT_FOUND)
}

/// Enumerate a key's child names, then close it.
#[cfg(windows)]
fn reg_enumerate_subkeys(target: &RegistryTarget) -> std::result::Result<Vec<String>, u32> {
    use windows::core::PWSTR;
    use windows::Win32::System::Registry::{RegCloseKey, RegEnumKeyExW};

    struct Guard(RegKey);
    impl Drop for Guard {
        fn drop(&mut self) {
            // Every path out of this function closes the handle, including the early
            // `return Err(..)` below. A leaked `RegKey` in a long-running orchestrator is
            // a slow resource leak that no test on a short command would ever catch.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    let key = reg_open(target)?;
    let _guard = Guard(key);

    let mut names = Vec::new();
    // Child names are capped by `REGLEN_MAX` (255 characters plus terminator); 1024
    // UTF-16 units is comfortably more than the API can report, so there is no
    // truncation to detect or retry here.
    let mut buffer = [0u16; 1024];
    let mut index = 0u32;
    loop {
        let mut len = (buffer.len() - 1) as u32;
        let code = unsafe {
            RegEnumKeyExW(
                key,
                index,
                Some(PWSTR(buffer.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if code.0 == ERROR_NO_MORE_ITEMS {
            break;
        }
        if code.0 != 0 {
            return Err(code.0);
        }
        names.push(String::from_utf16_lossy(&buffer[..len as usize]));
        index += 1;
        // An enormous or deliberately stuffed key is bounded here rather than by the
        // caller's entry count, because enumeration is where the cost is actually paid.
        if names.len() >= MAX_REGISTRY_CHILDREN {
            break;
        }
    }
    Ok(names)
}

#[cfg(not(windows))]
fn reg_enumerate_subkeys(_target: &RegistryTarget) -> std::result::Result<Vec<String>, u32> {
    Err(ERROR_PATH_NOT_FOUND)
}

/// Cap on children enumerated per registry key.
///
/// Real `Uninstall` hives hold hundreds of keys; ten thousand is beyond any honest
/// machine and within reach of a hostile one, which is the number worth defending
/// against.
const MAX_REGISTRY_CHILDREN: usize = 4096;

/// Read one `REG_SZ`/`REG_EXPAND_SZ` value from `parent\child`.
///
/// Two calls to `RegGetValueW` rather than one: the type mask is a single bitset and
/// asking for both `SZ` and `EXPAND_SZ` at once is legal, but the second call exists
/// because a value written as `REG_EXPAND_SZ` must have its `%VARS%` expanded before
/// it becomes a usable path — the shell does this when it launches, and a candidate
/// built from the unexpanded text points at a directory that does not exist.
#[cfg(windows)]
fn reg_value(target: &RegistryTarget, child: &str, value_name: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_MORE_DATA;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegGetValueW, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    };

    let key = reg_open(target).ok()?;
    let rel: Vec<u16> = child.encode_utf16().chain(std::iter::once(0)).collect();
    let name: Vec<u16> = value_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut scratch = vec![0u16; 1024];
    let mut cb: u32 = (scratch.len() * 2) as u32;
    let code = unsafe {
        RegGetValueW(
            key,
            PCWSTR(rel.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            None,
            Some(scratch.as_mut_ptr().cast::<core::ffi::c_void>()),
            Some(&mut cb),
        )
    };
    if code == ERROR_MORE_DATA {
        // Re-read at the size the key actually reports. Values this large are rare
        // (`Uninstall` strings occasionally carry long publisher text) and silently
        // truncating one would produce a path that does not exist.
        let needed = (cb as usize).div_ceil(2).max(2);
        scratch = vec![0u16; needed];
        let mut cb2: u32 = (scratch.len() * 2) as u32;
        let again = unsafe {
            RegGetValueW(
                key,
                PCWSTR(rel.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                None,
                Some(scratch.as_mut_ptr().cast::<core::ffi::c_void>()),
                Some(&mut cb2),
            )
        };
        unsafe {
            let _ = RegCloseKey(key);
        }
        if again.0 != 0 {
            return None;
        }
        cb = cb2;
    } else if code.0 != 0 {
        unsafe {
            let _ = RegCloseKey(key);
        }
        return None;
    } else {
        unsafe {
            let _ = RegCloseKey(key);
        }
    }

    let units = (cb as usize / 2).saturating_sub(1).min(scratch.len());
    let text = String::from_utf16_lossy(&scratch[..units]);
    let expanded = expand_environment_strings(&text);
    if expanded.trim().is_empty() {
        None
    } else {
        Some(expanded)
    }
}

#[cfg(not(windows))]
fn reg_value(_target: &RegistryTarget, _child: &str, _value_name: &str) -> Option<String> {
    None
}

/// Expand `%NAME%` references in a registry string.
///
/// Only `%VAR%`, not `$env:VAR` or backticks: this text comes from the registry, and
/// anything more expressive than substitution would be evaluating somebody else's
/// script. Unknown variables are left as written, because silently emptying them can
/// produce a path that resolves to a *different* real directory.
pub fn expand_environment_strings(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == '%' {
            if let Some(end) = bytes[i + 1..].iter().position(|c| *c == '%') {
                let name: String = bytes[i + 1..i + 1 + end].iter().collect();
                if !name.is_empty() && name.chars().all(valid_env_char) {
                    match std::env::var(&name) {
                        Ok(value) => {
                            out.push_str(&value);
                            i += end + 2;
                            continue;
                        }
                        Err(_) => {
                            out.push('%');
                            out.push_str(&name);
                            out.push('%');
                            i += end + 2;
                            continue;
                        }
                    }
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn valid_env_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

// ─────────────────────────────────────────────────────────────────────────────
// The run.
// ─────────────────────────────────────────────────────────────────────────────

/// Accumulates one discovery run.
struct RunCtx {
    limits: DiscoveryLimits,
    deadline: Instant,
    started: Instant,
    candidates: Vec<AppCandidate>,
    /// Dedupe map: canonical exe path → index in `candidates`.
    by_path: BTreeMap<String, usize>,
    /// Candidates with no path to dedupe on, keyed by `(source, display name)` so two
    /// unresolved shortcuts with different names stay distinct.
    unnamed: Vec<(String, usize)>,
    inaccessible: Vec<(String, u32)>,
    sources_used: Vec<DiscoverySource>,
    directories_scanned: usize,
    entries_examined: u64,
    stop: DiscoveryBound,
    notes: Vec<String>,
}

impl RunCtx {
    fn new(limits: DiscoveryLimits, query: &str) -> Self {
        let started = Instant::now();
        let _ = query;
        RunCtx {
            deadline: started + Duration::from_millis(limits.deadline_ms),
            started,
            limits,
            candidates: Vec::new(),
            by_path: BTreeMap::new(),
            unnamed: Vec::new(),
            inaccessible: Vec::new(),
            sources_used: Vec::new(),
            directories_scanned: 0,
            entries_examined: 0,
            stop: DiscoveryBound::None,
            notes: Vec::new(),
        }
    }

    /// True when a bound has already stopped the run or the clock has expired.
    fn out_of_budget(&self) -> bool {
        self.stop.was_hit() || Instant::now() >= self.deadline
    }

    /// Consume one directory slot; `false` means the run must stop looking.
    fn reserve_directory(&mut self) -> bool {
        if Instant::now() >= self.deadline {
            self.stop = DiscoveryBound::Deadline;
            return false;
        }
        if self.directories_scanned >= self.limits.max_directories {
            self.stop = DiscoveryBound::MaxDirectories;
            return false;
        }
        if self.entries_examined >= self.limits.max_entries {
            self.stop = DiscoveryBound::MaxEntries;
            return false;
        }
        self.directories_scanned += 1;
        true
    }

    /// Count a directory opened that is not a filesystem walk (one registry key).
    fn count_directory(&mut self) {
        self.directories_scanned += 1;
        if Instant::now() >= self.deadline {
            self.stop = DiscoveryBound::Deadline;
        }
    }

    fn count_entry(&mut self) {
        self.entries_examined += 1;
        if self.entries_examined > self.limits.max_entries {
            self.stop = DiscoveryBound::MaxEntries;
        }
    }

    /// Record an advisory note, capped.
    ///
    /// Sixteen notes is plenty for a human-sized answer; beyond that a warning channel
    /// has become a data channel and the run should be reporting candidates instead.
    fn note(&mut self, text: String) {
        if self.notes.len() < 16 && !self.notes.contains(&text) {
            self.notes.push(text);
        }
    }

    /// Insert or merge a candidate, honouring the candidate bound.
    fn add_candidate(&mut self, candidate: AppCandidate) {
        if self.stop.was_hit() {
            return;
        }
        if self.candidates.len() >= self.limits.max_candidates {
            self.stop = DiscoveryBound::MaxCandidates;
            return;
        }
        if let Some(exe) = &candidate.exe_path {
            let key = dedupe_key(exe);
            if let Some(idx) = self.by_path.get(&key).copied() {
                self.candidates[idx].absorb(&candidate);
                return;
            }
            let idx = self.candidates.len();
            self.candidates.push(candidate);
            self.by_path.insert(key, idx);
            return;
        }
        let key = format!(
            "{}|{}",
            candidate
                .sources
                .first()
                .map(|p| p.source.as_str())
                .unwrap_or("unknown"),
            fold(&candidate.display_name)
        );
        if let Some((_, idx)) = self.unnamed.iter().find(|(k, _)| k == &key) {
            let idx = *idx;
            self.candidates[idx].absorb(&candidate);
            return;
        }
        let idx = self.candidates.len();
        self.candidates.push(candidate);
        self.unnamed.push((key, idx));
    }

    fn finish(mut self, query: &str) -> DiscoveryOutcome {
        if !self.stop.was_hit() && Instant::now() >= self.deadline {
            self.stop = DiscoveryBound::Deadline;
        }
        sort_candidates(&mut self.candidates);
        DiscoveryOutcome {
            query: query.to_owned(),
            ambiguous: self.candidates.len() > 1,
            candidates: self.candidates,
            bound: self.stop,
            inaccessible_keys: self.inaccessible,
            sources_used: self.sources_used,
            directories_scanned: self.directories_scanned,
            entries_examined: self.entries_examined,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            notes: self.notes,
        }
    }
}

/// Rank: best match kind, then earliest source, then name.
///
/// A total order, so output never depends on hash ordering or directory enumeration —
/// `jean apps notepad` prints the same list twice.
fn sort_candidates(candidates: &mut [AppCandidate]) {
    candidates.sort_by(|a, b| {
        a.match_kind
            .cmp(&b.match_kind)
            .then_with(|| {
                a.sources
                    .first()
                    .map(|p| p.source.rank())
                    .unwrap_or(u8::MAX)
                    .cmp(
                        &b.sources
                            .first()
                            .map(|p| p.source.rank())
                            .unwrap_or(u8::MAX),
                    )
            })
            .then_with(|| fold(&a.display_name).cmp(&fold(&b.display_name)))
    });
}

/// Discover installed applications matching `query`.
///
/// Every enabled source is consulted even after a perfect hit, because "is this
/// ambiguous?" cannot be answered from one source alone. A bound stops the run and
/// says which one did.
pub fn discover_apps(
    query: &str,
    env: &DiscoveryEnv,
    limits: &DiscoveryLimits,
) -> Result<DiscoveryOutcome> {
    let mut ctx = RunCtx::new(limits.clone(), query);

    if query.trim().is_empty() {
        // Enumerating everything would blow every bound and answer a question nobody
        // asked. Refusing is the honest result.
        ctx.note("an empty query matches nothing; refusing to enumerate the machine".to_owned());
        return Ok(ctx.finish(query));
    }

    let exts = executable_extensions(env.pathext.as_deref());

    if limits.enables(DiscoverySource::Path) {
        ctx.sources_used.push(DiscoverySource::Path);
        let dirs = search_dirs(env.path.as_deref());
        scan_path(query, &dirs, &exts, &mut ctx);
    }

    if limits.enables(DiscoverySource::StartMenu) && !ctx.out_of_budget() {
        ctx.sources_used.push(DiscoverySource::StartMenu);
        if !DiscoveryEnv::link_resolution_supported() {
            ctx.note(
                "shortcut targets are unresolved in this build: the shell-link COM interface \
                 was not compiled in, so .lnk entries are reported by name without an \
                 executable path"
                    .to_owned(),
            );
        }
        let roots: Vec<PathBuf> = env
            .user_start_menu
            .iter()
            .chain(env.machine_start_menu.iter())
            .chain(env.extra_start_menu_roots.iter())
            .cloned()
            .collect();
        scan_start_menu(query, &roots, &mut ctx);
    }

    if limits.enables(DiscoverySource::Registry) && !ctx.out_of_budget() {
        ctx.sources_used.push(DiscoverySource::Registry);
        scan_registry(query, &mut ctx);
    }

    let outcome = ctx.finish(query);
    if outcome.bound.was_hit() {
        tracing::debug!(
            bound = outcome.bound.as_str(),
            query_len = query.len(),
            "application discovery stopped at a bound; the candidate list is known-incomplete"
        );
    }
    Ok(outcome)
}

/// Map a Win32 registry code to whether it should be reported as inaccessible.
///
/// `ERROR_FILE_NOT_FOUND` and `ERROR_PATH_NOT_FOUND` are excluded on purpose: they say
/// the key is absent, which on HKCU is the normal state of a machine rather than a
/// problem an operator can act on. Everything else — above all
/// `ERROR_ACCESS_DENIED` — is surfaced.
pub fn registry_code_is_inaccessible(code: u32) -> bool {
    !matches!(code, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
}

/// Build the typed error for a registry subtree this account could not open.
pub fn inaccessible_registry_error(key: String, code: u32) -> ToolError {
    if code == ERROR_ACCESS_DENIED {
        return ToolError::Registry {
            key,
            context: "RegOpenKeyExW with KEY_READ",
            code,
        };
    }
    ToolError::RegistryInaccessible { key, code }
}

/// Discover through the live environment, using the configured source list.
///
/// The convenience entry point for `jean apps`: reads `tools.discovery` values, builds
/// the environment view, and reports unknown source names as notes rather than errors.
pub fn discover_installed_apps(query: &str, discovery_list: &[String]) -> Result<DiscoveryOutcome> {
    let (limits, ignored) = DiscoveryLimits::from_discovery_list(discovery_list);
    let outcome = discover_apps(query, &DiscoveryEnv::from_process_environment(), &limits)?;
    if ignored.is_empty() {
        return Ok(outcome);
    }
    let mut outcome = outcome;
    outcome.notes.insert(
        0,
        format!(
            "ignored unrecognised tools.discovery entries: {}",
            ignored.join(", ")
        ),
    );
    Ok(outcome)
}
