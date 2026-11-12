//! Command bootstrap. Opening the store may create its database and run migrations.
//! Logging and other state directories are prepared only for writing commands.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jean_protocol::config::{load_layers, AppConfig, ConfigSource, LoadedConfig};
use jean_protocol::{Redactor, ToolRegistry};
use jean_store::Store;

use crate::error::Result;
use crate::paths::{self, Paths};

pub struct Context {
    pub config: AppConfig,
    pub paths: Paths,
    pub store: Store,
    pub tools: ToolRegistry,
    pub redactor: Redactor,
    pub sources: Vec<ConfigSource>,
    pub warnings: Vec<String>,
    pub task_started: fn(&str),
    preference_base: AppConfig,
}

impl Context {
    pub fn load(exe_dir: Option<&Path>) -> Result<Self> {
        let loaded = load_config(exe_dir)?;
        let preference_base = jean_protocol::preferences::file_preference_base(&loaded)
            .map_err(|reason| crate::error::CoreError::Memory { reason })?;
        let mut config = preference_base.clone();

        let paths = Paths::resolve(
            &config.core,
            &config.store,
            &config.tools,
            &config.policy,
            find_config_dir(exe_dir),
        )?;

        let store = Store::open_with(
            &paths.database,
            &config.store.journal_mode,
            &config.store.synchronous,
            Duration::from_millis(u64::from(config.store.busy_timeout_ms)),
        )?;

        let mut warnings = loaded.warnings.clone();
        warnings.extend(apply_stored_preferences(&store, &mut config));

        let (registry, registry_warnings) = load_registry(&paths, &config.process.env_allowlist);
        warnings.extend(registry_warnings);

        let redactor = Redactor::new(&config.process.redact_patterns);
        warnings.extend(
            redactor
                .invalid_patterns
                .iter()
                .map(|pattern| format!("the redaction pattern {pattern:?} did not compile")),
        );

        Ok(Self {
            config,
            paths,
            store,
            tools: registry,
            redactor,
            sources: loaded.sources,
            warnings,
            task_started: |_| {},
            preference_base,
        })
    }

    // Refresh only supported settings against the original file snapshot: paths,
    // registry and redactor must not diverge after a preference is cleared.
    pub(crate) fn refresh_configuration_from_layers(&mut self) -> Result<()> {
        let mut config = self.preference_base.clone();
        self.warnings
            .extend(apply_stored_preferences(&self.store, &mut config));
        self.config = config;
        Ok(())
    }

    pub(crate) fn preference_candidate(&self, rows: &[(String, String)]) -> Result<AppConfig> {
        let mut candidate = self.preference_base.clone();
        jean_protocol::preferences::overlay(rows, &mut candidate)
            .map_err(|reason| crate::error::CoreError::Memory { reason })?;
        Ok(candidate)
    }

    pub(crate) fn install_preference_config(&mut self, mut config: AppConfig) {
        if std::env::var_os("JEAN_NO_VOICE").is_some() {
            config.voice.enabled = false;
        }
        self.config = config;
    }

    pub fn prepare_for_writing(&mut self) -> Result<()> {
        self.paths.ensure()?;
        crate::logging::init(&self.config.core, &self.paths.log_dir)?;
        Ok(())
    }

    pub fn env_allowlist(&self) -> &[String] {
        &self.config.process.env_allowlist
    }

    pub fn default_timeout(&self) -> Duration {
        Duration::from_secs(self.config.orchestrator.default_timeout_secs)
    }

    pub fn output_cap(&self) -> u64 {
        self.config.process.capture_output_max_bytes
    }
}

fn apply_stored_preferences(store: &Store, config: &mut AppConfig) -> Vec<String> {
    let result = store
        .preference_rows()
        .map_err(|e| e.to_string())
        .and_then(|rows| jean_protocol::preferences::overlay(&rows, config));
    let mut warnings = Vec::new();
    if result.is_err() {
        // Recovery commands remain usable, but a corrupt overlay cannot enable actions.
        config.voice.enabled = false;
        config.desktop.enabled = false;
        warnings.push("invalid stored preferences: voice and desktop disabled; use preferences get/clear to repair".into());
    }
    if std::env::var_os("JEAN_NO_VOICE").is_some() {
        config.voice.enabled = false;
    }
    warnings
}

fn load_config(exe_dir: Option<&Path>) -> Result<LoadedConfig> {
    let default_data = paths::default_data_dir()?;
    let layers = paths::config_layers(exe_dir, &default_data);
    let first = load_layers(&layers)?;

    let relocated = relocated_data_dir(&first.config)?;
    match relocated {
        Some(dir) if dir != default_data => {
            let layers = paths::config_layers(exe_dir, &dir);
            Ok(load_layers(&layers)?)
        }
        _ => Ok(first),
    }
}

fn relocated_data_dir(config: &AppConfig) -> Result<Option<PathBuf>> {
    if config.core.data_dir.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(config.core.data_dir.trim())))
}

fn find_config_dir(exe_dir: Option<&Path>) -> Option<PathBuf> {
    let dir = exe_dir?;
    let mut current = Some(dir);
    for _ in 0..paths::MAX_ANCESTORS_FOR_REPO_CONFIG {
        let here = current?;
        let candidate = here.join("config");
        if candidate.join("defaults.toml").is_file() {
            return Some(candidate);
        }
        current = here.parent();
    }
    None
}

fn load_registry(paths: &Paths, allowlist: &[String]) -> (ToolRegistry, Vec<String>) {
    let mut warnings = Vec::new();
    let Some(candidate) = paths.tool_registry_candidates().first().cloned() else {
        warnings.push(format!(
            "no tool registry was found (looked for {}); `jean run` will have no tools",
            paths.tools.display()
        ));
        return (ToolRegistry::empty(), warnings);
    };

    match ToolRegistry::load(&candidate, allowlist) {
        Ok(registry) => {
            tracing::debug!(
                path = %candidate.display(),
                tools = registry.len(),
                "loaded the tool registry"
            );
            (registry, warnings)
        }
        Err(e) => {
            warnings.push(format!(
                "the tool registry at {} was rejected: {e}",
                candidate.display()
            ));
            (ToolRegistry::empty(), warnings)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_data_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "jean-context-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_relocated_data_directory_is_honoured() {
        let dir = temp_data_dir("relocated");
        std::fs::create_dir_all(&dir).unwrap();
        let previous = std::env::var("JEAN_DATA_DIR").ok();
        std::env::set_var("JEAN_DATA_DIR", &dir);

        let loaded = load_config(None).expect("configuration must load");
        assert_eq!(
            PathBuf::from(loaded.config.core.data_dir.trim()),
            dir,
            "JEAN_DATA_DIR must relocate the data directory"
        );

        match previous {
            Some(v) => std::env::set_var("JEAN_DATA_DIR", v),
            None => std::env::remove_var("JEAN_DATA_DIR"),
        }
        cleanup(&dir);
    }

    #[test]
    fn the_compiled_defaults_are_always_the_first_source() {
        let loaded = load_config(None).unwrap();
        assert!(
            matches!(loaded.sources[0], ConfigSource::CompiledDefaults),
            "{:?}",
            loaded.sources
        );
        assert_eq!(loaded.config.config_version, 1);
    }

    #[test]
    fn an_absent_registry_is_reported_not_fatal() {
        let paths = Paths {
            data_dir: PathBuf::from("Z:\\does-not-exist"),
            database: PathBuf::from("Z:\\does-not-exist\\jean.db"),
            log_dir: PathBuf::from("Z:\\does-not-exist\\logs"),
            artifacts: PathBuf::from("Z:\\does-not-exist\\artifacts"),
            tools: PathBuf::from("Z:\\does-not-exist\\tools.toml"),
            audit: PathBuf::from("Z:\\does-not-exist\\audit.jsonl"),
            config_dir: None,
        };
        let (registry, warnings) = load_registry(&paths, &["PATH".to_owned()]);
        assert!(registry.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("no tool registry was found"),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_invalid_registry_is_refused_with_a_reason() {
        let dir = temp_data_dir("invalid-registry");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tools.toml");
        std::fs::write(
            &file,
            "config_version = 1\n\n[[tool]]\nid = \"bad\"\nkind = \"shell\"\n\
             command = \"powershell.exe -Command x | y\"\nverify = { method = \"none\" }\n",
        )
        .unwrap();

        let paths = Paths {
            data_dir: dir.clone(),
            database: dir.join("jean.db"),
            log_dir: dir.join("logs"),
            artifacts: dir.join("artifacts"),
            tools: file.clone(),
            audit: dir.join("audit.jsonl"),
            config_dir: None,
        };
        let (registry, warnings) = load_registry(&paths, &["PATH".to_owned()]);
        assert!(
            registry.is_empty(),
            "an invalid registry must not partially load"
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("rejected"), "{warnings:?}");
        cleanup(&dir);
    }

    #[test]
    fn a_valid_registry_loads_its_tools() {
        let dir = temp_data_dir("valid-registry");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tools.toml");
        std::fs::write(
            &file,
            "config_version = 1\n\n[[tool]]\nid = \"echo\"\nkind = \"shell\"\n\
             command = \"powershell.exe\"\nargs = [\"-NoProfile\"]\n\
             verify = { method = \"exit_code\", expected = 0 }\n",
        )
        .unwrap();

        let paths = Paths {
            data_dir: dir.clone(),
            database: dir.join("jean.db"),
            log_dir: dir.join("logs"),
            artifacts: dir.join("artifacts"),
            tools: file,
            audit: dir.join("audit.jsonl"),
            config_dir: None,
        };
        let (registry, warnings) = load_registry(&paths, &["PATH".to_owned()]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(registry.len(), 1);
        assert!(registry.get("echo").is_some());
        cleanup(&dir);
    }

    #[test]
    fn the_config_directory_is_found_next_to_a_defaults_file() {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap();
        let found = find_config_dir(Some(dir));
        assert!(
            found.is_some(),
            "a test binary under target/ must be able to find the workspace config/ directory"
        );
        let found = found.unwrap();
        assert!(found.join("defaults.toml").is_file());
    }

    #[test]
    fn no_config_directory_is_found_without_an_executable_path() {
        assert!(find_config_dir(None).is_none());
    }

    #[test]
    fn relocated_data_dir_is_none_when_unset() {
        let config = AppConfig::default();
        assert_eq!(relocated_data_dir(&config).unwrap(), None);
    }

    #[test]
    fn relocated_data_dir_reads_the_configured_value() {
        let mut config = AppConfig::default();
        config.core.data_dir = "  D:\\jean-state  ".to_owned();
        assert_eq!(
            relocated_data_dir(&config).unwrap(),
            Some(PathBuf::from("D:\\jean-state"))
        );
    }
}
