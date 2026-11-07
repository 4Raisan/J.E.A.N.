//! Tool definitions and the tool registry.
//!
//! A tool is the smallest unit J.E.A.N. will execute. Everything about how it
//! is invoked is declared up front and validated at load time, so that a
//! malformed registry entry fails loudly at startup instead of at the moment a
//! user asks for it.
//!
//! Two rules from the task specification are enforced here rather than in the
//! caller, because they are security properties:
//!
//! * **Executable + argument vector only.** There is no field for "a command
//!   string", so there is no way to accidentally concatenate user input into a
//!   shell. A shell tool is declared as `command = "pwsh"` with a *fixed*
//!   argument vector; a user-supplied script path is passed as one argument.
//! * **Every tool declares how its result is verified** (`verify`). Phase 0
//!   accepts `none` explicitly, but an absent `verify` key is rejected — D-012
//!   says a missing verification method is a configuration bug, not a default.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::SUPPORTED_CONFIG_VERSION;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Shell,
    App,
    Cli,
    Agent,
    Mcp,
    Browser,
}

impl ToolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolKind::Shell => "shell",
            ToolKind::App => "app",
            ToolKind::Cli => "cli",
            ToolKind::Agent => "agent",
            ToolKind::Mcp => "mcp",
            ToolKind::Browser => "browser",
        }
    }
}

/// Risk tier, matching `PLANNING/04-SETTINGS.md` `[policy]`.
///
/// Phase 0 records the tier and refuses nothing on its basis — the policy
/// engine that acts on it is Phase 1. The field exists now so that tool
/// definitions do not have to be rewritten when it arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyTier {
    ReadOnly,
    Normal,
    Elevated,
}

impl PolicyTier {
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyTier::ReadOnly => "read_only",
            PolicyTier::Normal => "normal",
            PolicyTier::Elevated => "elevated",
        }
    }
}

/// How a tool run is verified. See D-012.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum VerifySpec {
    /// Process exited with the given code. The weakest form, and Phase 0's
    /// default for shell tools — it proves the command ran, not that the user's
    /// goal was met.
    ExitCode {
        #[serde(default = "default_expected_exit")]
        expected: i32,
    },
    /// Exit code plus a substring that must appear in stdout.
    OutputContains {
        #[serde(default = "default_expected_exit")]
        expected: i32,
        needle: String,
        #[serde(default)]
        case_insensitive: bool,
    },
    /// The tool ran and produced no error, but nothing proves the user's broader
    /// goal. The CLI must say so explicitly rather than reporting success.
    None,
}

fn default_expected_exit() -> i32 {
    0
}

impl VerifySpec {
    /// The method's name, in the same vocabulary as [`crate::plan::DeclaredVerification::as_str`].
    ///
    /// Used by the planner's catalog and by `jean plan`, where a tool's declared verification has
    /// to be shown next to the one a plan proposes without either one being re-explained.
    pub fn method_label(&self) -> &'static str {
        match self {
            Self::ExitCode { .. } => "exit_code",
            Self::OutputContains { .. } => "output_contains",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDef {
    pub id: String,
    pub kind: ToolKind,
    #[serde(default)]
    pub display_name: String,

    /// Executable to launch. Resolved against PATH or an absolute path.
    pub command: String,
    /// Tried if `command` is not found. Must also be a bare executable name.
    #[serde(default)]
    pub fallback_command: Option<String>,

    /// Fixed argument vector. Appended to, never substituted into a shell.
    #[serde(default)]
    pub args: Vec<String>,

    /// Requires a console (ConPTY). Phase 0 always uses pipes; the flag is
    /// recorded so Phase 1 can route interactive tools correctly.
    #[serde(default)]
    pub interactive: bool,

    #[serde(default = "default_tier")]
    pub tier: PolicyTier,

    #[serde(default)]
    pub capabilities: Vec<String>,

    /// Empty means "inherit the caller's working directory".
    #[serde(default)]
    pub working_dir: String,

    /// Overrides `orchestrator.default_timeout_secs`. Zero is rejected.
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Extra variables for this tool only. Keys must still be allowlisted.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Extra arguments appended to `args`. Present so a shell tool can carry a
    /// fixed script while still accepting a target path from the user.
    #[serde(default)]
    pub extra_args: Vec<String>,

    pub verify: VerifySpec,

    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_tier() -> PolicyTier {
    PolicyTier::Normal
}
const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRegistryFile {
    #[serde(default = "default_version")]
    pub config_version: u32,
    #[serde(default)]
    pub tool: Vec<ToolDef>,
}

fn default_version() -> u32 {
    SUPPORTED_CONFIG_VERSION
}

#[derive(Debug, Error)]
pub enum ToolValidationError {
    #[error("tools file {path}: {message}")]
    Parse { path: PathBuf, message: String },

    #[error("tool `{id}`: {reason}")]
    Invalid { id: String, reason: String },

    #[error("duplicate tool id `{0}`")]
    DuplicateId(String),

    #[error(
        "tools file {path}: unsupported config_version {found}; this build supports {supported}"
    )]
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
}

/// Upper bound on a single tool timeout. Ten hours is generous enough for any
/// legitimate Phase-0 command and small enough that a typo (`timeout_secs =
/// 3000000`) is caught instead of producing an unkillable-looking task.
pub const MAX_TIMEOUT_SECS: u64 = 36_000;

/// Characters that cannot appear in a Windows argument without corrupting the
/// command line. NUL truncates the wide string; CR/LF can break argument
/// parsing in some hosts. Quoting handles spaces and embedded quotes correctly,
/// so those are permitted.
fn reject_control_chars(s: &str, what: &str, id: &str) -> Result<(), ToolValidationError> {
    let bad = s
        .chars()
        .find(|c| *c == '\0' || *c == '\r' || *c == '\n')
        .map(|c| format!("{:?}", c));
    match bad {
        Some(c) => Err(ToolValidationError::Invalid {
            id: id.to_owned(),
            reason: format!("{what} contains control character {c}, which would truncate or split the command line"),
        }),
        None => Ok(()),
    }
}

impl ToolDef {
    /// Validate one tool definition. Called for every entry at load time.
    pub fn validate(&self, env_allowlist: &[String]) -> Result<(), ToolValidationError> {
        let invalid = |reason: String| ToolValidationError::Invalid {
            id: self.id.clone(),
            reason,
        };

        if self.id.trim().is_empty() {
            return Err(invalid("id must not be empty".to_owned()));
        }
        if self.id.len() > 64 {
            return Err(invalid(format!(
                "id is {} chars; the limit is 64 because ids are embedded in pipe names",
                self.id.len()
            )));
        }
        if !self
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(invalid(format!(
                "id {:?} must contain only ASCII letters, digits, '-', '_' or '.' \
                 (it is used to build a named-pipe name)",
                self.id
            )));
        }

        if self.command.trim().is_empty() {
            return Err(invalid("command must not be empty".to_owned()));
        }
        reject_control_chars(&self.command, "command", &self.id)?;
        // A command containing a shell metacharacter is almost always an attempt
        // to smuggle a compound command. Reject rather than quote-and-hope.
        if self.command.contains('|')
            || self.command.contains('&')
            || self.command.contains('>')
            || self.command.contains('<')
            || self.command.contains(';')
        {
            return Err(invalid(format!(
                "command {:?} contains a shell metacharacter; declare an executable and an \
                 argument vector instead — J.E.A.N. never builds shell command strings",
                self.command
            )));
        }
        if self.command.contains('"') {
            return Err(invalid(
                "command must not contain a double quote; use an absolute path without quoting"
                    .to_owned(),
            ));
        }

        if let Some(fb) = &self.fallback_command {
            if fb.trim().is_empty() {
                return Err(invalid(
                    "fallback_command is present but empty; omit the key instead".to_owned(),
                ));
            }
            reject_control_chars(fb, "fallback_command", &self.id)?;
        }

        for (i, arg) in self.args.iter().chain(self.extra_args.iter()).enumerate() {
            reject_control_chars(arg, &format!("argument {i}"), &self.id)?;
        }

        if !self.working_dir.is_empty() {
            let p = Path::new(&self.working_dir);
            if !p.exists() {
                return Err(invalid(format!(
                    "working_dir {:?} does not exist",
                    self.working_dir
                )));
            }
            if !p.is_dir() {
                return Err(invalid(format!(
                    "working_dir {:?} is not a directory",
                    self.working_dir
                )));
            }
        }

        if let Some(t) = self.timeout_secs {
            if t == 0 {
                return Err(invalid(
                    "timeout_secs must be greater than zero; omit it to use the default".to_owned(),
                ));
            }
            if t > MAX_TIMEOUT_SECS {
                return Err(invalid(format!(
                    "timeout_secs {t} exceeds the maximum of {MAX_TIMEOUT_SECS}"
                )));
            }
        }

        for (k, v) in &self.env {
            if k.trim().is_empty() {
                return Err(invalid("env contains an empty variable name".to_owned()));
            }
            if k.contains('=') {
                return Err(invalid(format!("env key {k:?} must not contain '='")));
            }
            reject_control_chars(k, &format!("env key {k:?}"), &self.id)?;
            reject_control_chars(v, &format!("env value for {k:?}"), &self.id)?;
            if !env_allowlist.iter().any(|a| a.eq_ignore_ascii_case(k)) {
                return Err(invalid(format!(
                    "env key {k:?} is not in process.env_allowlist; a tool cannot widen the \
                     allowlist, only the user configuration can"
                )));
            }
        }

        if let VerifySpec::OutputContains { needle, .. } = &self.verify {
            if needle.is_empty() {
                return Err(invalid(
                    "verify.method = output_contains requires a non-empty needle".to_owned(),
                ));
            }
        }

        for cap in &self.capabilities {
            reject_control_chars(cap, "capability", &self.id)?;
        }

        Ok(())
    }

    pub fn display_name(&self) -> &str {
        if self.display_name.is_empty() {
            &self.id
        } else {
            &self.display_name
        }
    }
}

/// Validated, id-indexed tool set.
#[derive(Debug, Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolDef>,
}

impl ToolRegistry {
    /// Parse and validate a registry file. Any invalid entry fails the whole
    /// load: a registry that is partly wrong is a registry you cannot trust.
    pub fn from_toml_str(
        text: &str,
        path: &Path,
        env_allowlist: &[String],
    ) -> Result<Self, ToolValidationError> {
        let file: ToolRegistryFile =
            toml::from_str(text).map_err(|e| ToolValidationError::Parse {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
        Self::from_file(file, path, env_allowlist)
    }

    pub fn from_file(
        file: ToolRegistryFile,
        path: &Path,
        env_allowlist: &[String],
    ) -> Result<Self, ToolValidationError> {
        if file.config_version != SUPPORTED_CONFIG_VERSION {
            return Err(ToolValidationError::UnsupportedVersion {
                path: path.to_path_buf(),
                found: file.config_version,
                supported: SUPPORTED_CONFIG_VERSION,
            });
        }

        let mut tools = BTreeMap::new();
        for tool in file.tool {
            tool.validate(env_allowlist)?;
            let id = tool.id.clone();
            if tools.contains_key(&id) {
                return Err(ToolValidationError::DuplicateId(id));
            }
            tools.insert(id, tool);
        }
        Ok(Self { tools })
    }

    pub fn load(path: &Path, env_allowlist: &[String]) -> Result<Self, ToolValidationError> {
        let text = std::fs::read_to_string(path).map_err(|e| ToolValidationError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        Self::from_toml_str(&text, path, env_allowlist)
    }

    /// An empty registry is legal: it means "no tools configured yet", which is
    /// the state on a first run before `tools.toml` is written.
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &str) -> Option<&ToolDef> {
        self.tools.get(id)
    }

    pub fn enabled(&self) -> impl Iterator<Item = &ToolDef> {
        self.tools.values().filter(|t| t.enabled)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowlist() -> Vec<String> {
        crate::config::default_env_allowlist()
    }

    fn tool(id: &str, command: &str) -> ToolDef {
        ToolDef {
            id: id.to_owned(),
            kind: ToolKind::Cli,
            display_name: String::new(),
            command: command.to_owned(),
            fallback_command: None,
            args: vec![],
            interactive: false,
            tier: PolicyTier::Normal,
            capabilities: vec![],
            working_dir: String::new(),
            timeout_secs: None,
            env: BTreeMap::new(),
            extra_args: vec![],
            verify: VerifySpec::ExitCode { expected: 0 },
            enabled: true,
        }
    }

    #[test]
    fn a_minimal_valid_tool_passes() {
        tool("echo", "cmd").validate(&allowlist()).expect("valid");
    }

    #[test]
    fn empty_or_whitespace_id_is_rejected() {
        for id in ["", "   "] {
            let err = tool(id, "cmd").validate(&allowlist()).expect_err("reject");
            assert!(err.to_string().contains("id must not be empty"), "{err}");
        }
    }

    #[test]
    fn id_must_be_pipe_name_safe() {
        let err = tool("my tool!", "cmd")
            .validate(&allowlist())
            .expect_err("reject");
        assert!(err.to_string().contains("named-pipe name"), "{err}");

        tool("my-tool_1.0", "cmd")
            .validate(&allowlist())
            .expect("these chars are fine");
    }

    #[test]
    fn empty_command_is_rejected() {
        let err = tool("t", "").validate(&allowlist()).expect_err("reject");
        assert!(
            err.to_string().contains("command must not be empty"),
            "{err}"
        );
    }

    #[test]
    fn shell_metacharacters_in_command_are_rejected() {
        // This is the guard that keeps user input out of a shell.
        for bad in [
            "cmd /c del foo && calc",
            "powershell | tee",
            "cmd > out.txt",
            "cmd < in.txt",
            "a; b",
        ] {
            let err = tool("t", bad)
                .validate(&allowlist())
                .expect_err("must reject");
            assert!(
                err.to_string().contains("shell metacharacter"),
                "{bad:?} -> {err}"
            );
        }
    }

    #[test]
    fn control_characters_are_rejected_everywhere() {
        let mut t = tool("t", "cmd");
        t.args = vec!["ok".into(), "bad\0arg".into()];
        let err = t.validate(&allowlist()).expect_err("reject");
        assert!(err.to_string().contains("control character"), "{err}");

        let t = tool("t", "cm\nd");
        assert!(t.validate(&allowlist()).is_err());
    }

    #[test]
    fn a_fixed_script_argument_vector_is_allowed() {
        // The sanctioned way to run a script: explicit argv, no shell string.
        let mut t = tool("setup", "pwsh");
        t.args = vec![
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-File".into(),
            "scripts\\setup.ps1".into(),
        ];
        t.validate(&allowlist())
            .expect("fixed argv is exactly what we want");
    }

    #[test]
    fn timeout_zero_and_absurd_are_rejected() {
        let mut t = tool("t", "cmd");
        t.timeout_secs = Some(0);
        let err = t.validate(&allowlist()).expect_err("reject");
        assert!(err.to_string().contains("greater than zero"), "{err}");

        let mut t = tool("t", "cmd");
        t.timeout_secs = Some(MAX_TIMEOUT_SECS + 1);
        assert!(t.validate(&allowlist()).is_err());

        let mut t = tool("t", "cmd");
        t.timeout_secs = Some(MAX_TIMEOUT_SECS);
        t.validate(&allowlist())
            .expect("the boundary value itself is fine");
    }

    #[test]
    fn working_dir_must_exist_and_be_a_directory() {
        let mut t = tool("t", "cmd");
        t.working_dir = if cfg!(windows) {
            "Z:\\definitely\\not\\here".into()
        } else {
            "/definitely/not/here".into()
        };
        let err = t.validate(&allowlist()).expect_err("reject");
        assert!(err.to_string().contains("does not exist"), "{err}");

        let mut t = tool("t", "cmd");
        t.working_dir = std::env::temp_dir().to_string_lossy().into_owned();
        t.validate(&allowlist()).expect("temp dir exists");

        let mut t = tool("t", "cmd");
        // A file, not a directory.
        let f = std::env::temp_dir().join("jean-tooltest-notadir.txt");
        std::fs::write(&f, b"x").unwrap();
        t.working_dir = f.to_string_lossy().into_owned();
        let err = t.validate(&allowlist()).expect_err("reject");
        assert!(err.to_string().contains("not a directory"), "{err}");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn env_keys_must_be_allowlisted() {
        let mut t = tool("t", "cmd");
        t.env.insert("PATH".into(), "C:\\bin".into());
        t.validate(&allowlist()).expect("PATH is allowlisted");

        let mut t = tool("t", "cmd");
        t.env.insert("AWS_SECRET_ACCESS_KEY".into(), "x".into());
        let err = t.validate(&allowlist()).expect_err("reject");
        assert!(
            err.to_string().contains("not in process.env_allowlist"),
            "{err}"
        );
        assert!(
            err.to_string().contains("cannot widen the allowlist"),
            "the message must say why: {err}"
        );
    }

    #[test]
    fn verify_is_mandatory_and_output_contains_needs_a_needle() {
        // `verify` has no #[serde(default)], so this fails to parse.
        let toml_text = r#"
            config_version = 1
            [[tool]]
            id = "t"
            kind = "cli"
            command = "cmd"
        "#;
        let err = ToolRegistry::from_toml_str(toml_text, Path::new("t.toml"), &allowlist())
            .expect_err("verify is required");
        assert!(err.to_string().contains("verify"), "{err}");

        let mut t = tool("t", "cmd");
        t.verify = VerifySpec::OutputContains {
            expected: 0,
            needle: String::new(),
            case_insensitive: false,
        };
        assert!(t.validate(&allowlist()).is_err());
    }

    #[test]
    fn unknown_tool_keys_are_rejected() {
        let toml_text = r#"
            config_version = 1
            [[tool]]
            id = "t"
            kind = "cli"
            command = "cmd"
            verify = { method = "exit_code" }
            typo_field = true
        "#;
        let err = ToolRegistry::from_toml_str(toml_text, Path::new("t.toml"), &allowlist())
            .expect_err("deny_unknown_fields");
        assert!(err.to_string().contains("typo_field"), "{err}");
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let toml_text = r#"
            config_version = 1
            [[tool]]
            id = "dup"
            kind = "cli"
            command = "cmd"
            verify = { method = "exit_code" }
            [[tool]]
            id = "dup"
            kind = "cli"
            command = "pwsh"
            verify = { method = "exit_code" }
        "#;
        let err = ToolRegistry::from_toml_str(toml_text, Path::new("t.toml"), &allowlist())
            .expect_err("reject duplicates");
        assert!(
            matches!(err, ToolValidationError::DuplicateId(_)),
            "{err:?}"
        );
    }

    #[test]
    fn unsupported_registry_version_is_rejected() {
        let toml_text = r#"
            config_version = 7
            [[tool]]
            id = "t"
            kind = "cli"
            command = "cmd"
            verify = { method = "exit_code" }
        "#;
        let err = ToolRegistry::from_toml_str(toml_text, Path::new("t.toml"), &allowlist())
            .expect_err("reject");
        assert!(
            matches!(
                err,
                ToolValidationError::UnsupportedVersion { found: 7, .. }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn one_invalid_entry_fails_the_whole_registry() {
        let toml_text = r#"
            config_version = 1
            [[tool]]
            id = "good"
            kind = "cli"
            command = "cmd"
            verify = { method = "exit_code" }
            [[tool]]
            id = "bad"
            kind = "cli"
            command = ""
            verify = { method = "exit_code" }
        "#;
        let err = ToolRegistry::from_toml_str(toml_text, Path::new("t.toml"), &allowlist())
            .expect_err("a partly-wrong registry cannot be trusted");
        assert!(err.to_string().contains("bad"), "{err}");
    }

    #[test]
    fn registry_indexes_and_filters() {
        let toml_text = r#"
            config_version = 1
            [[tool]]
            id = "on"
            kind = "cli"
            command = "cmd"
            verify = { method = "exit_code" }
            [[tool]]
            id = "off"
            kind = "cli"
            command = "cmd"
            verify = { method = "exit_code" }
            enabled = false
        "#;
        let reg =
            ToolRegistry::from_toml_str(toml_text, Path::new("t.toml"), &allowlist()).unwrap();
        assert_eq!(reg.len(), 2);
        assert!(reg.get("on").is_some());
        assert!(reg.get("missing").is_none());
        let enabled: Vec<&str> = reg.enabled().map(|t| t.id.as_str()).collect();
        assert_eq!(enabled, vec!["on"]);
        assert!(ToolRegistry::empty().is_empty());
    }

    #[test]
    fn display_name_falls_back_to_id() {
        let t = tool("mytool", "cmd");
        assert_eq!(t.display_name(), "mytool");
        let mut t = tool("mytool", "cmd");
        t.display_name = "My Tool".into();
        assert_eq!(t.display_name(), "My Tool");
    }

    #[test]
    fn tier_and_kind_serialise_as_snake_case() {
        assert_eq!(PolicyTier::ReadOnly.as_str(), "read_only");
        assert_eq!(ToolKind::Mcp.as_str(), "mcp");
        let json = serde_json::to_string(&PolicyTier::Elevated).unwrap();
        assert_eq!(json, "\"elevated\"");
    }
}
