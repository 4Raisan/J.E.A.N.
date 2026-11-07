//! Tests for the configuration files that ship with the repository.
//!
//! These exist because `config/defaults.toml` and `config/tools.example.toml` are
//! documentation that has to *run*. A comment claiming a default is one thing; the
//! compiled default is the truth, and the two drift the moment someone changes one
//! and not the other.
//!
//! The key-set comparison is the load-bearing test: it fails when a field is added
//! to a config struct without being documented, or when a documented key is renamed
//! in the code. Without it, `deny_unknown_fields` turns the second case into a
//! startup failure on every machine that has the shipped file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use jean_protocol::config::AppConfig;
use jean_protocol::tool::{ToolRegistry, ToolRegistryFile};
use jean_protocol::SUPPORTED_CONFIG_VERSION;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/jean-core has the workspace root two levels up")
        .to_path_buf()
}

fn config_file(name: &str) -> PathBuf {
    repo_root().join("config").join(name)
}

/// Every dotted key path in a TOML tree, tables and leaves alike.
///
/// The caller filters to leaves, so collecting both is harmless and keeps this
/// function a plain recursion.
fn key_paths(value: &toml::Value, prefix: &str, out: &mut BTreeSet<String>) {
    let toml::Value::Table(table) = value else {
        return;
    };
    for (key, child) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        out.insert(path.clone());
        key_paths(child, &path, out);
    }
}

/// The subset of `paths` that name a setting rather than a section.
fn leaf_keys(value: &toml::Value, paths: &BTreeSet<String>) -> BTreeSet<String> {
    paths
        .iter()
        .filter(|path| {
            let mut current = value;
            for part in path.split('.') {
                match current.get(part) {
                    Some(next) => current = next,
                    None => return false,
                }
            }
            !current.is_table()
        })
        .cloned()
        .collect()
}

#[test]
fn the_shipped_defaults_file_exists_and_is_readable() {
    let path = config_file("defaults.toml");
    assert!(path.is_file(), "{} is missing", path.display());
    let text = std::fs::read_to_string(&path).expect("defaults.toml must be readable");
    assert!(
        text.len() > 1_000,
        "defaults.toml looks truncated: {} bytes",
        text.len()
    );
}

#[test]
fn the_shipped_defaults_parse_into_the_configuration_schema() {
    let path = config_file("defaults.toml");
    let text = std::fs::read_to_string(&path).unwrap();

    // `deny_unknown_fields` is on every section, so this fails if the file
    // documents a key the code does not have.
    let config: AppConfig =
        toml::from_str(&text).unwrap_or_else(|e| panic!("{} did not parse: {e}", path.display()));

    config.validate().unwrap_or_else(|e| {
        panic!(
            "{} does not satisfy the validation rules: {e}",
            path.display()
        )
    });

    assert_eq!(config.config_version, SUPPORTED_CONFIG_VERSION);
}

#[test]
fn the_shipped_defaults_match_the_compiled_defaults_exactly() {
    let path = config_file("defaults.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let documented: AppConfig = toml::from_str(&text).expect("parses, per the test above");
    let compiled = AppConfig::default();

    assert_eq!(
        documented,
        compiled,
        "{} has drifted from the compiled defaults. Either update the file or, if \
         the change was intentional, update the compiled default in \
         crates/jean-protocol/src/config.rs — but the two must agree, because a \
         shipped file that silently changes behaviour is worse than no file at all.",
        path.display()
    );
}

#[test]
fn every_configurable_key_is_documented_in_the_shipped_file() {
    let path = config_file("defaults.toml");
    let text = std::fs::read_to_string(&path).unwrap();

    let documented_value: toml::Value = toml::from_str(&text).unwrap();
    let mut documented = BTreeSet::new();
    key_paths(&documented_value, "", &mut documented);

    let compiled_value =
        toml::Value::try_from(AppConfig::default()).expect("the schema must serialise");
    let mut compiled = BTreeSet::new();
    key_paths(&compiled_value, "", &mut compiled);

    // Comparing leaves only avoids counting section headers as settings.
    let documented_leaves = leaf_keys(&documented_value, &documented);
    let compiled_leaves = leaf_keys(&compiled_value, &compiled);

    let undocumented: Vec<&String> = compiled_leaves.difference(&documented_leaves).collect();
    assert!(
        undocumented.is_empty(),
        "these settings exist in the schema but are not documented in {}: {undocumented:?}",
        path.display()
    );

    let invented: Vec<&String> = documented_leaves.difference(&compiled_leaves).collect();
    assert!(
        invented.is_empty(),
        "these keys are documented in {} but do not exist in the schema, and would \
         be pruned with a warning at startup: {invented:?}",
        path.display()
    );
}

#[test]
fn the_shipped_defaults_are_loadable_through_the_real_loader() {
    // `load_layers` is what the CLI actually calls. It merges over the compiled
    // defaults, prunes unknown keys and then validates, so this covers the path a
    // user hits rather than a direct deserialize.
    let path = config_file("defaults.toml");
    let loaded = jean_protocol::config::load_layers(std::slice::from_ref(&path))
        .expect("the shipped defaults must load cleanly");

    assert!(
        loaded.warnings.is_empty(),
        "the shipped file should produce no warnings, got: {:?}",
        loaded.warnings
    );
    assert_eq!(loaded.config, AppConfig::default());
    assert!(
        loaded.sources.len() >= 2,
        "the compiled defaults plus the file: {:?}",
        loaded.sources
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// The example tool registry
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn the_example_registry_loads_and_validates_against_the_default_allowlist() {
    let path = config_file("tools.example.toml");
    assert!(path.is_file(), "{} is missing", path.display());

    let registry = ToolRegistry::load(&path, &jean_protocol::config::default_env_allowlist())
        .unwrap_or_else(|e| panic!("{} was rejected: {e}", path.display()));

    assert!(
        registry.len() >= 5,
        "the example registry should offer a useful starting set, found {}",
        registry.len()
    );
}

#[test]
fn every_example_tool_declares_a_verification_method() {
    let path = config_file("tools.example.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let file: ToolRegistryFile = toml::from_str(&text).unwrap();

    assert!(!file.tool.is_empty());
    for tool in &file.tool {
        // `verify` has no serde default, so parsing succeeded only because every
        // entry declares one. This asserts the intent explicitly, so removing the
        // default from the schema does not silently weaken the examples.
        let declared = matches!(
            tool.verify,
            jean_protocol::tool::VerifySpec::ExitCode { .. }
                | jean_protocol::tool::VerifySpec::OutputContains { .. }
                | jean_protocol::tool::VerifySpec::None
        );
        assert!(declared, "{} has no verification", tool.id);
        assert_ne!(
            format!("{:?}", tool.verify),
            "None",
            "{} verifies nothing; an example should show a real check",
            tool.id
        );
    }
}

#[test]
fn every_example_tool_is_read_only_or_an_explicit_fixture() {
    let path = config_file("tools.example.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let file: ToolRegistryFile = toml::from_str(&text).unwrap();

    for tool in &file.tool {
        let is_fixture = tool.id.starts_with("examples.");
        assert!(
            tool.tier == jean_protocol::tool::PolicyTier::ReadOnly || is_fixture,
            "{} is tier {:?} but is not an `examples.` fixture; a shipped example \
             should not ask for more than it needs",
            tool.id,
            tool.tier
        );
        assert_ne!(
            tool.tier,
            jean_protocol::tool::PolicyTier::Elevated,
            "{} must not require elevation; Phase 0 never elevates",
            tool.id
        );
    }
}

#[test]
fn no_example_tool_runs_a_script_interpreter_implicitly() {
    // `command` is always a bare executable, never a string with shell syntax in
    // it. This is the rule that keeps "J.E.A.N. never builds shell command
    // strings" true for the shipped examples too.
    let path = config_file("tools.example.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let file: ToolRegistryFile = toml::from_str(&text).unwrap();

    for tool in &file.tool {
        for c in ['|', '&', '>', '<', ';', '"'] {
            assert!(
                !tool.command.contains(c),
                "{} has {c:?} in its command: {:?}",
                tool.id,
                tool.command
            );
        }
        assert!(
            !tool.command.contains(' ')
                || tool.command.contains('\\')
                || tool.command.contains('/'),
            "{} should name a bare executable resolvable on PATH, got {:?}",
            tool.id,
            tool.command
        );
    }
}

#[test]
fn the_example_registry_includes_a_disabled_entry() {
    // `enabled = false` needs an example, or the flag is undocumented in practice.
    let path = config_file("tools.example.toml");
    let registry =
        ToolRegistry::load(&path, &jean_protocol::config::default_env_allowlist()).unwrap();

    let disabled: Vec<&str> = registry
        .ids()
        .filter(|id| !registry.get(id).unwrap().enabled)
        .collect();
    assert!(
        !disabled.is_empty(),
        "the example registry should show how to disable a tool"
    );
    assert!(
        registry.enabled().count() < registry.len(),
        "the disabled tool must actually be excluded from `enabled()`"
    );
}

#[test]
fn the_example_registry_includes_an_interactive_entry() {
    // Phase 1 ships a ConPTY backend, and a registry with no `interactive = true`
    // example would leave that whole path unreachable out of the box — the same
    // reasoning that put a disabled entry in the file.
    let path = config_file("tools.example.toml");
    let registry =
        ToolRegistry::load(&path, &jean_protocol::config::default_env_allowlist()).unwrap();

    let interactive: Vec<&str> = registry
        .ids()
        .filter(|id| registry.get(id).unwrap().interactive)
        .collect();
    assert!(
        !interactive.is_empty(),
        "the example registry should show how to ask for a console"
    );
}

#[test]
fn every_example_tool_id_is_safe_in_a_pipe_name_and_a_path() {
    let path = config_file("tools.example.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let file: ToolRegistryFile = toml::from_str(&text).unwrap();

    for tool in &file.tool {
        assert!(
            jean_protocol::ipc::validate_task_id(&tool.id).is_ok(),
            "{:?} is not usable as an identifier",
            tool.id
        );
    }
}
