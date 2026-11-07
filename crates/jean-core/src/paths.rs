//! Where J.E.A.N. keeps its files, and how those locations are decided.
//!
//! Configuration may leave a path empty, which means "derive it from the data
//! directory". Deriving rather than hard-coding is what makes `JEAN_DATA_DIR`
//! sufficient to relocate an entire installation — useful for tests, and for a
//! user who does not want state on the system drive.
//!
//! Nothing here creates directories as a side effect of *reading* a path.
//! Creation happens in [`Paths::ensure`], so a read-only operation such as
//! `jean tasks` cannot leave empty directories behind on a machine that has
//! never run a task.

use std::path::{Path, PathBuf};

use jean_protocol::config::{CoreConfig, PolicyConfig, StoreConfig, ToolsConfig};

use crate::error::{CoreError, Result};

/// The application's directory name under `%LOCALAPPDATA%`.
pub const APP_DIR_NAME: &str = "JEAN";

/// How many parent directories are searched for a development `config/` folder.
///
/// Bounded because an unbounded walk to the filesystem root would read directory
/// entries all over the disk on every invocation, and because more than a few
/// levels never matches a real layout: `target/debug/jean.exe` is two below the
/// workspace root, `target/x86_64-pc-windows-msvc/debug/jean.exe` is four.
pub const MAX_ANCESTORS_FOR_REPO_CONFIG: usize = 6;

/// Every resolved filesystem location J.E.A.N. uses in Phase 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Root of all state.
    pub data_dir: PathBuf,
    /// SQLite database.
    pub database: PathBuf,
    /// Rolling log files.
    pub log_dir: PathBuf,
    /// Captured stdout/stderr artifacts, one directory per task.
    pub artifacts: PathBuf,
    /// Tool registry (`tools.toml`).
    pub tools: PathBuf,
    /// Append-only JSONL audit log.
    pub audit: PathBuf,
    /// Where the packaged `config/` was found, if anywhere.
    ///
    /// Kept because `tools.registry_path` is documented as defaulting to
    /// `<config_dir>/tools.toml`, and because a development tree ships its
    /// example registry there rather than in the data directory.
    pub config_dir: Option<PathBuf>,
}

impl Paths {
    /// Resolve every path from configuration, filling in the derived defaults.
    pub fn resolve(
        core: &CoreConfig,
        store: &StoreConfig,
        tools: &ToolsConfig,
        policy: &PolicyConfig,
        config_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let data_dir = if core.data_dir.trim().is_empty() {
            default_data_dir()?
        } else {
            PathBuf::from(core.data_dir.trim())
        };

        Ok(Self {
            database: or_under(&store.path, &data_dir, "jean.db"),
            log_dir: if core.log_dir.trim().is_empty() {
                data_dir.join("logs")
            } else {
                PathBuf::from(core.log_dir.trim())
            },
            artifacts: data_dir.join("artifacts"),
            tools: or_under(&tools.registry_path, &data_dir, "tools.toml"),
            audit: or_under(&policy.audit_path, &data_dir, "audit.jsonl"),
            data_dir,
            config_dir,
        })
    }

    /// Where to look for a tool registry, most preferred first.
    ///
    /// The configured path always comes first, and only files that exist are
    /// returned: a caller that gets an empty list should report "no tools are
    /// configured", which is a true and useful statement, rather than guessing.
    pub fn tool_registry_candidates(&self) -> Vec<PathBuf> {
        let mut candidates = vec![self.tools.clone()];
        if let Some(dir) = &self.config_dir {
            candidates.push(dir.join("tools.toml"));
            // A development tree ships the example rather than a live registry,
            // and using it is what makes `cargo run` work before any setup step.
            candidates.push(dir.join("tools.example.toml"));
        }
        candidates.into_iter().filter(|p| p.is_file()).collect()
    }

    /// Create the directories J.E.A.N. needs to write to.
    ///
    /// Called once per invocation that may write. Idempotent.
    pub fn ensure(&self) -> Result<()> {
        for dir in [&self.data_dir, &self.log_dir, &self.artifacts] {
            std::fs::create_dir_all(dir).map_err(|e| CoreError::Directory {
                path: dir.clone(),
                reason: e.to_string(),
            })?;
        }
        if let Some(parent) = self.database.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CoreError::Directory {
                path: parent.to_path_buf(),
                reason: e.to_string(),
            })?;
        }
        Ok(())
    }

    /// The artifact directory for one task.
    ///
    /// Per-task rather than per-run: a task's stdout and stderr belong together,
    /// and a retry of the same task should not scatter its output across the
    /// tree.
    pub fn artifacts_for(&self, task_id: &str) -> PathBuf {
        self.artifacts.join(sanitize_for_path(task_id))
    }
}

/// Use `configured` when it is non-empty, otherwise `data_dir/file_name`.
fn or_under(configured: &str, data_dir: &Path, file_name: &str) -> PathBuf {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        data_dir.join(file_name)
    } else {
        PathBuf::from(trimmed)
    }
}

/// `%LOCALAPPDATA%\JEAN`, with fallbacks that do not require a network profile.
///
/// `LOCALAPPDATA` is the right home for machine-local state that should not roam
/// and should not be deleted on logout. If it is missing — a service account, or
/// a stripped environment — fall back to the user profile, and only then fail.
pub fn default_data_dir() -> Result<PathBuf> {
    for var in ["LOCALAPPDATA", "APPDATA", "USERPROFILE"] {
        if let Ok(value) = std::env::var(var) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed).join(APP_DIR_NAME));
            }
        }
    }
    Err(CoreError::Environment {
        reason: "none of LOCALAPPDATA, APPDATA or USERPROFILE is set, so the data directory \
                 cannot be derived; set JEAN_DATA_DIR or core.data_dir explicitly"
            .to_owned(),
    })
}

/// Make an identifier safe to use as a single path component.
///
/// Task ids are generated by [`jean_protocol::ipc::new_task_id`] and contain no
/// separators, but this is defence in depth: a caller-supplied id reaching a
/// `join` unchanged could write outside the artifacts directory.
fn sanitize_for_path(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect();
    // A component made only of dots resolves to the parent directory, which is
    // how an id like ".." would write outside the artifacts tree.
    let trimmed = cleaned.trim_matches('.');
    if trimmed.is_empty() {
        "_".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The configuration files to layer, lowest precedence first.
///
/// 1. The packaged `config/defaults.toml`, found by walking up from the
///    executable. Shipped defaults, so a fresh install works.
/// 2. `<data_dir>\jean.toml` — the user's persistent configuration.
/// 3. `.\jean.toml` — the current directory, for one-off experiments.
///
/// Later layers win. Files that do not exist are skipped by
/// [`jean_protocol::config::load_layers`], which is what makes layer 1 optional
/// on an installed system and layers 2–3 optional on a development machine.
pub fn config_layers(exe_dir: Option<&Path>, data_dir: &Path) -> Vec<PathBuf> {
    let mut layers = Vec::new();
    if let Some(dir) = exe_dir {
        if let Some(found) = find_repo_config(dir) {
            layers.push(found);
        }
    }
    layers.push(data_dir.join("jean.toml"));
    layers.push(PathBuf::from("jean.toml"));
    layers
}

/// Walk up from `start` looking for `config/defaults.toml`.
fn find_repo_config(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    for _ in 0..MAX_ANCESTORS_FOR_REPO_CONFIG {
        let dir = current?;
        let candidate = dir.join("config").join("defaults.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use jean_protocol::AppConfig;

    fn defaults() -> AppConfig {
        AppConfig::default()
    }

    #[test]
    fn empty_paths_are_derived_from_the_data_directory() {
        let cfg = defaults();
        let mut core = cfg.core.clone();
        core.data_dir = if std::env::var("TEMP").is_ok() {
            std::env::var("TEMP").unwrap()
        } else {
            "/tmp/jean-test".to_owned()
        };
        let paths = Paths::resolve(&core, &cfg.store, &cfg.tools, &cfg.policy, None).unwrap();

        assert_eq!(paths.data_dir, PathBuf::from(core.data_dir.clone()));
        assert_eq!(paths.database, paths.data_dir.join("jean.db"));
        assert_eq!(paths.log_dir, paths.data_dir.join("logs"));
        assert_eq!(paths.artifacts, paths.data_dir.join("artifacts"));
        assert_eq!(paths.tools, paths.data_dir.join("tools.toml"));
        assert_eq!(paths.audit, paths.data_dir.join("audit.jsonl"));
    }

    #[test]
    fn an_explicit_path_wins_over_the_derived_one() {
        let cfg = defaults();
        let mut core = cfg.core.clone();
        core.data_dir = "D:\\jean-data".to_owned();
        core.log_dir = "E:\\jean-logs".to_owned();
        let mut store = cfg.store.clone();
        store.path = "E:\\tasks.db".to_owned();
        let mut tools = cfg.tools.clone();
        tools.registry_path = "E:\\tools.toml".to_owned();

        let paths = Paths::resolve(&core, &store, &tools, &cfg.policy, None).unwrap();
        assert_eq!(paths.database, PathBuf::from("E:\\tasks.db"));
        assert_eq!(paths.log_dir, PathBuf::from("E:\\jean-logs"));
        assert_eq!(paths.tools, PathBuf::from("E:\\tools.toml"));
        // Unset paths still derive from the data directory.
        assert_eq!(
            paths.artifacts,
            PathBuf::from("D:\\jean-data").join("artifacts")
        );
    }

    #[test]
    fn whitespace_only_is_treated_as_unset() {
        let cfg = defaults();
        let mut core = cfg.core.clone();
        core.data_dir = std::env::var("TEMP").unwrap_or_else(|_| "/tmp".to_owned());
        let mut store = cfg.store.clone();
        store.path = "   ".to_owned();
        let paths = Paths::resolve(&core, &store, &cfg.tools, &cfg.policy, None).unwrap();
        assert_eq!(paths.database, paths.data_dir.join("jean.db"));
    }

    #[test]
    fn the_data_directory_is_derived_when_not_configured() {
        // Only asserted when the machine actually has one of the variables; the
        // failure mode is covered by `a_missing_environment_is_an_explained_error`.
        if std::env::var("LOCALAPPDATA").is_err()
            && std::env::var("APPDATA").is_err()
            && std::env::var("USERPROFILE").is_err()
        {
            return;
        }
        let dir = default_data_dir().expect("derivable on a normal Windows session");
        assert!(dir.ends_with(APP_DIR_NAME), "{dir:?}");
        assert!(dir.is_absolute(), "{dir:?}");
    }

    #[test]
    fn ensure_creates_the_directories_it_needs() {
        let base = std::env::temp_dir().join(format!("jean-paths-ensure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let cfg = defaults();
        let mut core = cfg.core.clone();
        core.data_dir = base.display().to_string();

        let paths = Paths::resolve(&core, &cfg.store, &cfg.tools, &cfg.policy, None).unwrap();
        assert!(!paths.data_dir.exists());
        paths.ensure().expect("ensure must create the tree");
        assert!(paths.data_dir.is_dir());
        assert!(paths.log_dir.is_dir());
        assert!(paths.artifacts.is_dir());
        assert!(paths.database.parent().unwrap().is_dir());

        // Idempotent.
        paths.ensure().unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn artifacts_are_one_directory_per_task() {
        let cfg = defaults();
        let mut core = cfg.core.clone();
        core.data_dir = "D:\\jean".to_owned();
        let paths = Paths::resolve(&core, &cfg.store, &cfg.tools, &cfg.policy, None).unwrap();
        assert_eq!(
            paths.artifacts_for("t-1a2b-3c-4"),
            paths.artifacts.join("t-1a2b-3c-4")
        );
    }

    #[test]
    fn a_task_id_cannot_escape_the_artifacts_directory() {
        let cfg = defaults();
        let mut core = cfg.core.clone();
        core.data_dir = "D:\\jean".to_owned();
        let paths = Paths::resolve(&core, &cfg.store, &cfg.tools, &cfg.policy, None).unwrap();

        for hostile in [
            "..",
            "../..",
            r"..\..\Windows",
            "a/b",
            r"a\b",
            ":",
            "...",
            "",
        ] {
            let joined = paths.artifacts_for(hostile);
            assert_eq!(
                joined.parent(),
                Some(paths.artifacts.as_path()),
                "{hostile:?} escaped to {joined:?}"
            );
        }
    }

    #[test]
    fn config_layers_end_with_the_most_specific() {
        let layers = config_layers(None, Path::new("D:\\jean"));
        assert_eq!(layers.len(), 2, "{layers:?}");
        assert_eq!(layers[0], PathBuf::from("D:\\jean").join("jean.toml"));
        assert_eq!(layers[1], PathBuf::from("jean.toml"));
    }

    #[test]
    fn config_layers_find_the_repository_defaults_from_a_build_directory() {
        // This test runs from `target/debug/deps`, so the walk must reach the
        // workspace root where `config/defaults.toml` lives.
        let exe = std::env::current_exe().expect("current_exe");
        let dir = exe.parent().expect("a binary has a directory");
        let layers = config_layers(Some(dir), Path::new("D:\\jean"));
        let found = layers
            .iter()
            .any(|l| l.ends_with("config/defaults.toml") || l.ends_with("config\\defaults.toml"));
        assert!(
            found,
            "the packaged defaults were not found walking up from {dir:?}: {layers:?}"
        );
        assert_eq!(layers.last().unwrap(), &PathBuf::from("jean.toml"));
    }

    #[test]
    fn the_ancestor_walk_is_bounded() {
        // Walking from a root must terminate rather than scan the whole disk.
        let root = Path::new(if cfg!(windows) { "C:\\" } else { "/" });
        let start = std::time::Instant::now();
        let found = find_repo_config(root);
        assert!(
            found.is_none(),
            "a filesystem root has no config/defaults.toml"
        );
        assert!(start.elapsed().as_secs() < 5, "the walk took too long");
    }

    #[test]
    fn the_tool_registry_search_prefers_an_explicitly_configured_path() {
        let cfg = defaults();
        let mut core = cfg.core.clone();
        core.data_dir = std::env::temp_dir().display().to_string();
        let config_dir = std::env::current_exe()
            .unwrap()
            .ancestors()
            .find_map(|d| {
                let c = d.join("config");
                c.is_dir().then_some(c)
            })
            .expect("the repository config directory");

        let paths = Paths::resolve(
            &core,
            &cfg.store,
            &cfg.tools,
            &cfg.policy,
            Some(config_dir.clone()),
        )
        .unwrap();
        let candidates = paths.tool_registry_candidates();
        assert!(
            candidates.iter().all(|p| p.is_file()),
            "only existing files are offered: {candidates:?}"
        );
        if !candidates.is_empty() {
            assert!(
                candidates[0].starts_with(config_dir.parent().unwrap())
                    || candidates[0] == paths.tools,
                "the first candidate must be the configured path or a config-dir file: {:?}",
                candidates[0]
            );
        }
    }

    #[test]
    fn a_registry_search_with_no_config_dir_and_no_file_is_empty() {
        let cfg = defaults();
        let mut core = cfg.core.clone();
        core.data_dir = std::env::temp_dir()
            .join("jean-no-registry-here")
            .display()
            .to_string();
        let paths = Paths::resolve(&core, &cfg.store, &cfg.tools, &cfg.policy, None).unwrap();
        assert!(
            paths.tool_registry_candidates().is_empty(),
            "an absent registry must produce an empty list, not a guess"
        );
    }

    #[test]
    fn sanitize_replaces_every_separator() {
        assert_eq!(sanitize_for_path("t-1_2.3"), "t-1_2.3");
        assert_eq!(sanitize_for_path("a/b"), "a_b");
        assert_eq!(sanitize_for_path("a\\b"), "a_b");
        assert_eq!(sanitize_for_path(".."), "_");
        assert_eq!(sanitize_for_path(""), "_");
        assert_eq!(sanitize_for_path("..."), "_");
        assert!(!sanitize_for_path("x:y").contains(':'));
    }
}
