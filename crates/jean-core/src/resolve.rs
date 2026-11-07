//! Turning a tool's `command` into an absolute executable path.
//!
//! ## Why resolve at all
//!
//! `CreateProcessW` is called with `lpApplicationName`, which does **not** search
//! `PATH`. That is deliberate: resolution done here can be logged, validated and
//! tested, whereas resolution done by the operating system at spawn time cannot
//! be observed and can be influenced by the working directory.
//!
//! ## What this is not
//!
//! It is not a shell. Nothing here expands `%VAR%`, handles `&&`, or interprets
//! redirection. A `command` containing shell metacharacters is rejected earlier,
//! by [`jean_protocol::ToolDef::validate`], and this module would simply fail to
//! find a file with that name.
//!
//! ## Windows specifics
//!
//! `PATHEXT` decides which extensions may be appended when the configured command
//! has none (`.EXE`, `.CMD`, `.BAT`, …). Appending `.CMD` or `.BAT` means the
//! child is a batch file, which `cmd.exe` interprets — so those extensions are
//! excluded here. A tool that genuinely needs a batch file must name it
//! explicitly in `command`, which puts that choice in the registry where it can
//! be reviewed, rather than in a search order that could pick a `.bat` over an
//! `.exe` with the same base name.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use crate::error::Result;

/// The only extension J.E.A.N. appends when searching for a bare command name.
///
/// `.cmd`, `.bat`, `.ps1`, `.vbs`, `.js`, `.com` and `.scr` are all excluded, for
/// two reasons. The first is that most of them hand the file to an interpreter
/// (`cmd.exe`, PowerShell, Windows Script Host), which means J.E.A.N. would be
/// launching a shell to run a script whose contents nobody reviewed — the exact
/// thing the "no shell strings" rule exists to prevent. The second is collision
/// risk: if `tool.bat` and `tool.exe` both exist, a search order decides which
/// runs, and that decision would differ between machines.
///
/// A tool that genuinely needs one of those formats must name the file in
/// `command`, including its extension, which puts the choice in the registry
/// where it can be read and reviewed.
pub const SEARCH_EXTENSIONS: [&str; 1] = [".exe"];

/// A resolution failure, with enough detail to fix the registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveFailure {
    pub command: String,
    pub reason: String,
}

impl std::fmt::Display for ResolveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.command, self.reason)
    }
}

impl std::error::Error for ResolveFailure {}

/// Resolve `command` to an absolute path to an existing file.
///
/// `env_path` and `path_ext` are passed in rather than read from the process
/// environment, so a caller can resolve against the *child's* allowlisted
/// environment instead of the supervisor's — which is the environment that
/// actually matters, and the one a test can control.
pub fn resolve_executable(
    command: &str,
    env_path: Option<&str>,
    path_ext: Option<&str>,
) -> std::result::Result<PathBuf, ResolveFailure> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(fail(command, "the command is empty"));
    }

    let extensions = candidate_extensions(path_ext);
    let path = Path::new(trimmed);
    if has_separator(path) || path.is_absolute() {
        return resolve_explicit(path, &extensions).ok_or_else(|| {
            fail(
                command,
                "no file exists at that path, with or without a known executable extension",
            )
        });
    }

    let dirs = search_dirs(env_path);
    if dirs.is_empty() {
        return Err(fail(
            command,
            "PATH is empty or was not allowlisted, so a bare command name cannot be searched",
        ));
    }

    for dir in &dirs {
        for ext in &extensions {
            let candidate = dir.join(format!("{trimmed}{ext}"));
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        // A name that already carries an extension is tried verbatim too, so a
        // registry entry of `python3.12` still works.
        let verbatim = dir.join(trimmed);
        if verbatim.is_file() {
            return Ok(verbatim);
        }
    }

    Err(fail(
        command,
        &format!(
            "not found in {} PATH director{} (tried extensions: {})",
            dirs.len(),
            if dirs.len() == 1 { "y" } else { "ies" },
            if extensions.is_empty() {
                "none".to_owned()
            } else {
                extensions.join(", ")
            }
        ),
    ))
}

/// True when the command names a location rather than a bare program name.
///
/// `Component::is_absolute` alone is not enough: `dir\tool` is relative but
/// still explicit, and must not be searched for on `PATH`.
fn has_separator(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c,
            Component::CurDir | Component::ParentDir | Component::RootDir
        )
    }) || path.to_string_lossy().contains(['/', '\\'])
}

/// Resolve an explicitly-located command, trying the configured extensions.
fn resolve_explicit(path: &Path, extensions: &[String]) -> Option<PathBuf> {
    if path.is_file() {
        return Some(absolute(path));
    }
    for ext in extensions {
        let mut candidate = path.as_os_str().to_owned();
        candidate.push(ext);
        let candidate = PathBuf::from(candidate);
        if candidate.is_file() {
            return Some(absolute(&candidate));
        }
    }
    None
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        // Relative to the supervisor's directory. The child gets an absolute
        // path so its own working directory cannot change which file runs.
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// Split `PATH` into directories, dropping empty and duplicate entries.
fn search_dirs(env_path: Option<&str>) -> Vec<PathBuf> {
    let Some(raw) = env_path else {
        return Vec::new();
    };
    let mut seen = BTreeSet::new();
    raw.split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        // Quoted PATH entries are legal on Windows and the quotes are not part
        // of the path.
        .map(|entry| entry.trim_matches('"'))
        .filter(|entry| !entry.is_empty())
        .filter(|entry| seen.insert(entry.to_ascii_lowercase()))
        .map(PathBuf::from)
        .collect()
}

/// The extensions to try, from `PATHEXT` minus the ones that mean "run a script
/// interpreter".
fn candidate_extensions(path_ext: Option<&str>) -> Vec<String> {
    let configured: Vec<String> = match path_ext {
        Some(raw) => raw
            .split(';')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(|e| {
                let e = e.to_ascii_lowercase();
                if e.starts_with('.') {
                    e
                } else {
                    format!(".{e}")
                }
            })
            .collect(),
        // No PATHEXT (it is not allowlisted, or the environment is minimal):
        // fall back to the built-in order rather than resolving nothing.
        None => SEARCH_EXTENSIONS
            .iter()
            .map(|e| e.to_ascii_lowercase())
            .collect(),
    };

    let filtered: Vec<String> = configured
        .into_iter()
        .filter(|e| !is_script_extension(e))
        .collect();

    if filtered.is_empty() {
        SEARCH_EXTENSIONS
            .iter()
            .map(|e| e.to_ascii_lowercase())
            .collect()
    } else {
        filtered
    }
}

/// True for extensions Windows hands to `cmd.exe`.
///
/// Resolving to one of these would mean J.E.A.N. launched a shell to interpret a
/// script whose contents nobody reviewed.
fn is_script_extension(ext: &str) -> bool {
    matches!(
        ext,
        ".cmd"
            | ".bat"
            | ".com"
            | ".scr"
            | ".vbs"
            | ".vbe"
            | ".js"
            | ".jse"
            | ".wsf"
            | ".wsh"
            | ".msc"
            | ".ps1"
            | ".psm1"
            | ".lnk"
    )
}

fn fail(command: &str, reason: &str) -> ResolveFailure {
    ResolveFailure {
        command: command.to_owned(),
        reason: reason.to_owned(),
    }
}

/// Resolve a tool's command, producing a [`CoreError`] that names the tool.
pub fn resolve_for_tool(
    tool_id: &str,
    command: &str,
    env_path: Option<&str>,
    path_ext: Option<&str>,
) -> Result<PathBuf> {
    resolve_executable(command, env_path, path_ext).map_err(|f| {
        crate::error::CoreError::UnresolvedExecutable {
            tool_id: tool_id.to_owned(),
            command: command.to_owned(),
            reason: f.reason,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory containing one real file, used as a fake PATH entry.
    struct FakePath {
        dir: PathBuf,
    }

    impl FakePath {
        fn new(tag: &str, files: &[&str]) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "jean-resolve-{}-{}-{}",
                tag,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            for name in files {
                std::fs::write(dir.join(name), b"not really an executable").unwrap();
            }
            Self { dir }
        }

        fn path_var(&self) -> String {
            self.dir.display().to_string()
        }
    }

    impl Drop for FakePath {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_bare_name_is_found_on_the_search_path() {
        let fake = FakePath::new("bare", &["mytool.exe"]);
        let found = resolve_executable("mytool", Some(&fake.path_var()), Some(".EXE;.CMD"))
            .expect("must resolve");
        assert_eq!(found, fake.dir.join("mytool.exe"));
        assert!(found.is_absolute());
    }

    #[test]
    fn the_exe_extension_is_appended_when_absent() {
        let fake = FakePath::new("ext", &["widget.exe"]);
        let found = resolve_executable("widget", Some(&fake.path_var()), Some(".EXE")).unwrap();
        assert_eq!(found, fake.dir.join("widget.exe"));
    }

    #[test]
    fn an_explicit_extension_is_honoured() {
        let fake = FakePath::new("explicit-ext", &["python3.12"]);
        let found = resolve_executable("python3.12", Some(&fake.path_var()), None).unwrap();
        assert_eq!(found, fake.dir.join("python3.12"));
    }

    #[test]
    fn the_first_matching_directory_wins() {
        let first = FakePath::new("first", &["dup.exe"]);
        let second = FakePath::new("second", &["dup.exe"]);
        let combined = format!("{};{}", first.path_var(), second.path_var());
        let found = resolve_executable("dup", Some(&combined), None).unwrap();
        assert_eq!(found, first.dir.join("dup.exe"));
    }

    #[test]
    fn an_absolute_path_is_not_searched() {
        let fake = FakePath::new("abs", &["tool.exe"]);
        let full = fake.dir.join("tool.exe").display().to_string();
        let found = resolve_executable(&full, None, None).unwrap();
        assert_eq!(found, fake.dir.join("tool.exe"));

        let missing = fake.dir.join("nope.exe").display().to_string();
        let err = resolve_executable(&missing, Some(&fake.path_var()), None).unwrap_err();
        assert!(
            err.reason.contains("no file exists"),
            "an absolute path that does not exist must not be searched for: {err}"
        );
    }

    #[test]
    fn a_relative_path_with_a_separator_is_not_searched() {
        // `subdir\tool` names a location. Searching PATH for it would find an
        // unrelated `tool.exe` somewhere else, which is the kind of surprise
        // that makes a registry entry mean something different on two machines.
        let fake = FakePath::new("rel", &["tool.exe"]);
        let err = resolve_executable("subdir\\tool", Some(&fake.path_var()), None).unwrap_err();
        assert!(err.reason.contains("no file exists"), "{err}");
    }

    #[test]
    fn a_missing_command_reports_how_many_directories_were_searched() {
        let a = FakePath::new("miss-a", &["other.exe"]);
        let b = FakePath::new("miss-b", &["more.exe"]);
        let combined = format!("{};{}", a.path_var(), b.path_var());
        let err = resolve_executable("absent", Some(&combined), None).unwrap_err();
        assert_eq!(err.command, "absent");
        assert!(err.reason.contains("2 PATH directories"), "{err}");
        assert!(err.reason.contains(".exe"), "{err}");
    }

    #[test]
    fn an_empty_path_variable_is_reported_clearly() {
        let err = resolve_executable("anything", Some(""), None).unwrap_err();
        assert!(err.reason.contains("PATH is empty"), "{err}");
    }

    #[test]
    fn no_path_variable_at_all_is_reported_clearly() {
        // The child environment is built from an allowlist. If PATH is not on it,
        // a bare command name genuinely cannot be resolved, and saying so is more
        // useful than quietly searching the supervisor's own PATH.
        let err = resolve_executable("anything", None, None).unwrap_err();
        assert!(err.reason.contains("PATH is empty"), "{err}");
        assert!(err.reason.contains("allowlisted"), "{err}");
    }

    #[test]
    fn an_empty_command_is_refused() {
        assert!(resolve_executable("", None, None).is_err());
        assert!(resolve_executable("   ", None, None).is_err());
    }

    #[test]
    fn duplicate_path_entries_are_searched_once() {
        let fake = FakePath::new("dupe", &["x.exe"]);
        let p = fake.path_var();
        let combined = format!("{p};{p};{p}");
        let err = resolve_executable("absent", Some(&combined), None).unwrap_err();
        assert!(
            err.reason.contains("1 PATH directory"),
            "duplicates must collapse: {err}"
        );
    }

    #[test]
    fn quoted_and_empty_path_entries_are_tolerated() {
        let fake = FakePath::new("quoted", &["qtool.exe"]);
        let combined = format!(";\"{}\";;", fake.path_var());
        let found = resolve_executable("qtool", Some(&combined), None).unwrap();
        assert_eq!(found, fake.dir.join("qtool.exe"));
    }

    #[test]
    fn script_extensions_are_never_resolved_to() {
        // A `.cmd` with the same base name must not win over nothing at all:
        // resolving to it would mean launching a shell interpreter.
        let fake = FakePath::new("scripts", &["runner.cmd", "runner.bat", "runner.ps1"]);
        let err = resolve_executable(
            "runner",
            Some(&fake.path_var()),
            Some(".COM;.EXE;.BAT;.CMD;.PS1"),
        )
        .unwrap_err();
        assert!(err.reason.contains("not found"), "{err}");
    }

    #[test]
    fn a_real_exe_beside_a_bat_is_resolved_to_the_exe() {
        let fake = FakePath::new("mixed", &["thing.bat", "thing.exe"]);
        let found = resolve_executable("thing", Some(&fake.path_var()), Some(".BAT;.EXE"))
            .expect("the exe must be found");
        assert_eq!(found, fake.dir.join("thing.exe"));
    }

    #[test]
    fn is_script_extension_covers_the_interpreted_set() {
        for ext in [
            ".cmd", ".bat", ".ps1", ".vbs", ".js", ".wsf", ".msc", ".lnk",
        ] {
            assert!(
                is_script_extension(ext),
                "{ext} hands control to something else"
            );
        }
        for ext in [".exe", ".com2", ""] {
            assert!(!is_script_extension(ext), "{ext:?} is not a script");
        }
    }

    #[test]
    fn only_exe_is_ever_appended() {
        // The single-extension rule is a security boundary, not a preference, so
        // it is asserted directly rather than left to the search tests.
        assert_eq!(SEARCH_EXTENSIONS.len(), 1);
        assert_eq!(SEARCH_EXTENSIONS[0], ".exe");
        assert!(
            !candidate_extensions(Some(".EXE;.COM;.BAT;.CMD;.PS1")).contains(&".com".to_owned())
        );
        assert!(
            !candidate_extensions(Some(".EXE;.COM;.BAT;.CMD;.PS1")).contains(&".bat".to_owned())
        );
    }

    #[test]
    fn a_pathext_of_only_script_extensions_falls_back_to_the_builtin_list() {
        // Falling back rather than resolving nothing: a registry entry should not
        // stop working because PATHEXT happened to list only scripts.
        let exts = candidate_extensions(Some(".BAT;.CMD"));
        assert_eq!(exts, vec![".exe"]);
    }

    #[test]
    fn pathext_entries_without_a_dot_are_normalised() {
        assert_eq!(candidate_extensions(Some("EXE")), vec![".exe"]);
    }

    #[test]
    fn resolve_for_tool_names_the_tool_in_the_error() {
        let err =
            resolve_for_tool("git", "definitely-not-installed-anywhere", None, None).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("git"), "{text}");
        assert!(text.contains("definitely-not-installed-anywhere"), "{text}");
    }

    #[test]
    fn a_directory_on_the_path_is_not_mistaken_for_an_executable() {
        let fake = FakePath::new("dir", &[]);
        std::fs::create_dir_all(fake.dir.join("folder.exe")).unwrap();
        assert!(resolve_executable("folder", Some(&fake.path_var()), None).is_err());
    }
}
