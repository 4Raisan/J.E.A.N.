//! Typed, layered TOML configuration.
//!
//! Layering order (later wins): compiled defaults → `config/defaults.toml` →
//! `%APPDATA%\JEAN\jean.toml` → `JEAN_*` environment variables.
//!
//! Three distinct failure modes are handled differently, deliberately:
//!
//! * **Unsupported `config_version`** → hard error. Silently reinterpreting old
//!   keys is how configuration becomes a security hole.
//! * **Wrong type** → hard error, from `toml`'s deserializer, naming the field.
//! * **Unknown key** → warning, then ignored. A typo in a user file must not
//!   brick the daemon, but it must not pass silently either.
//!
//! Unknown-key detection is derived from the serialized default config rather
//! than a hand-maintained list, so it cannot drift from the struct definitions.
//! See `tests::unknown_key_detection_is_derived_from_the_struct`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The only configuration schema version this build understands.
pub const SUPPORTED_CONFIG_VERSION: u32 = 1;

/// Sections defined in `PLANNING/04-SETTINGS.md` that belong to later phases.
///
/// They are recognised (so a user following the settings document is not told
/// their file is wrong) but not applied. Phase 3 implements `[delegation]`, `[mcp]` and
/// `[browser]` and Phase 5 adds `[voice]`; vision, agent-swarm and telemetry remain deferred
/// subsystems — see `PLANNING/05-ROADMAP.md`.
pub const DEFERRED_SECTIONS: &[&str] = &["vision", "agents", "telemetry"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechBackend {
    #[default]
    SherpaOnnx,
    Fixture,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpeechModels {
    pub encoder: String,
    pub decoder: String,
    pub joiner: String,
    pub tokens: String,
    pub tts_model: String,
    pub tts_tokens: String,
    pub tts_lexicon: String,
    /// Kokoro's speaker table (`voices.bin`). Setting it is also how the worker knows the
    /// synthesis bundle is Kokoro rather than VITS: the two take different files, and
    /// guessing from a filename would load one engine's weights into the other.
    pub tts_voices: String,
    /// Directory of the text frontend's data, used by the Kokoro bundle. Not a model file.
    pub tts_dict_dir: String,
    /// BCP-47-ish language tag the synthesis frontend expects, e.g. `en-us`.
    pub tts_lang: String,
    /// Speaker index inside `tts_voices`. Identity is an index, not a name: the bundle
    /// itself decides which voice `0` is.
    pub tts_speaker: u32,
    /// Silero VAD weights. Optional: without them the worker still recognises
    /// speech, it just cannot trim leading/trailing silence before decoding.
    pub vad: String,
    /// Directory holding the shared sherpa-onnx library, when the worker was
    /// linked against the DLL rather than the static archive. Empty means the
    /// static build, which needs no runtime library directory at all.
    pub native_lib_dir: String,
}

/// Which audio devices the worker is allowed to open.
///
/// Derived from the speech backend rather than configured separately: a fixture
/// engine paired with a real microphone would record the operator while claiming
/// to be a test, so the two are kept in lockstep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBackend {
    System,
    Fixture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VoiceConfig {
    pub enabled: bool,
    pub mode: String,
    pub backend: SpeechBackend,
    pub startup_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub shutdown_timeout_ms: u64,
    pub capture_timeout_ms: u64,
    pub utterance_timeout_ms: u64,
    /// Model loading is the one voice request that legitimately takes seconds.
    /// It gets its own deadline so a slow disk cannot be mistaken for a hung
    /// worker, and so `request_timeout_ms` can stay short for everything else.
    pub configure_timeout_ms: u64,
    /// How long the recognizer may spend flushing its tail after push-to-talk
    /// release. Must stay below `request_timeout_ms`, because the owner treats
    /// an unanswered release as a dead worker.
    pub finalize_budget_ms: u64,
    /// Upper bound on captured audio held between the device and the recognizer.
    /// Overflow drops the oldest frames and is counted, never silently ignored.
    pub capture_buffer_ms: u64,
    /// Sample rate the recognizer and synthesizer work at. Device rates are
    /// resampled to this; it is not a device selection.
    pub sample_rate: u32,
    /// Endpoint id from `jean voice devices`. Empty means the system default.
    pub input_device: String,
    pub output_device: String,
    /// A WAV that stands in for the microphone, for validation without a device.
    ///
    /// When set, the worker opens this file instead of any endpoint: it is what
    /// `capture_once` reads, and the render side becomes a counted sink, so a
    /// whole press-release-transcript cycle runs against a real engine while
    /// nothing is recorded and nothing is played. Empty — the shipped default —
    /// means real endpoints, which is the only thing an operator can actually use.
    pub offline_wav: String,
    /// Push-to-talk binding for the interactive control session.
    pub hotkey: String,
    /// Speak task state changes. Opt-in: an announcement occupies the single
    /// utterance slot, so enabling it changes when an explicit `speak` fits.
    pub announce_task_state: bool,
    /// Treat an exactly-matching spoken phrase as a status/cancel control rather
    /// than a task goal. Opt-in because it intercepts what would be submitted.
    pub spoken_controls: bool,
    pub models: SpeechModels,
    pub fixture_transcript: String,
    /// How long the fixture engine's utterance is, in milliseconds.
    ///
    /// The real engine's utterance length comes from the text; the fixture's is a
    /// fixed block of silence, and at the shipped 10 ms it is gone before an
    /// operator can react. Raising it makes playback long enough for an interrupt
    /// to land *during* it, which is how the interruption path gets tested without
    /// a synthesizer. It changes nothing about real speech.
    pub fixture_utterance_ms: u64,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "push_to_talk".into(),
            backend: SpeechBackend::SherpaOnnx,
            startup_timeout_ms: 5000,
            request_timeout_ms: 2000,
            shutdown_timeout_ms: 2000,
            capture_timeout_ms: 30000,
            utterance_timeout_ms: 120000,
            configure_timeout_ms: 30000,
            finalize_budget_ms: 1200,
            capture_buffer_ms: 2000,
            sample_rate: 16000,
            input_device: String::new(),
            output_device: String::new(),
            offline_wav: String::new(),
            hotkey: "Ctrl+Alt+Space".into(),
            announce_task_state: false,
            spoken_controls: false,
            models: SpeechModels::default(),
            fixture_transcript: "inspect this process".into(),
            fixture_utterance_ms: 10,
        }
    }
}

impl VoiceConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let fail = |reason: &str| ConfigError::Validation {
            field: "voice",
            reason: reason.into(),
        };
        if self.mode != "push_to_talk" {
            return Err(fail("only push_to_talk is implemented"));
        }
        for ms in [
            self.startup_timeout_ms,
            self.request_timeout_ms,
            self.shutdown_timeout_ms,
        ] {
            if !(100..=10000).contains(&ms) {
                return Err(fail("lifecycle deadlines must be 100..=10000 ms"));
            }
        }
        if !(100..=60000).contains(&self.capture_timeout_ms)
            || !(100..=300000).contains(&self.utterance_timeout_ms)
        {
            return Err(fail(
                "capture must be 100..=60000 ms and utterance 100..=300000 ms",
            ));
        }
        if !(100..=120000).contains(&self.configure_timeout_ms) {
            return Err(fail("configure deadline must be 100..=120000 ms"));
        }
        if !(50..=5000).contains(&self.finalize_budget_ms) {
            return Err(fail("finalize budget must be 50..=5000 ms"));
        }
        if self.finalize_budget_ms >= self.request_timeout_ms {
            return Err(fail(
                "finalize budget must stay below the request deadline, or the owner \
                 reaps the worker while the recognizer is still flushing",
            ));
        }
        if !(100..=10000).contains(&self.capture_buffer_ms) {
            return Err(fail("capture buffer must be 100..=10000 ms"));
        }
        if !(8000..=192000).contains(&self.sample_rate) {
            return Err(fail("engine sample rate must be 8000..=192000 Hz"));
        }
        for (name, device) in [
            ("input_device", &self.input_device),
            ("output_device", &self.output_device),
        ] {
            if device.len() > 512 || device.contains('\0') {
                return Err(fail(&format!(
                    "{name} must be an endpoint id of at most 512 bytes"
                )));
            }
        }
        Hotkey::parse(&self.hotkey).map_err(|reason| fail(&reason))?;
        if !crate::interaction::text_valid(&self.fixture_transcript) {
            return Err(fail(
                "fixture transcript must be nonempty and at most 4096 bytes",
            ));
        }
        if !(10..=30_000).contains(&self.fixture_utterance_ms) {
            return Err(fail("fixture utterance must be 10..=30000 ms"));
        }
        Ok(())
    }

    /// A fixture engine never opens a real device: recording the operator while
    /// reporting a test would be the exact failure this pairing prevents.
    pub fn audio_backend(&self) -> AudioBackend {
        match self.backend {
            SpeechBackend::Fixture => AudioBackend::Fixture,
            SpeechBackend::SherpaOnnx => AudioBackend::System,
        }
    }

    pub fn hotkey(&self) -> Hotkey {
        Hotkey::parse(&self.hotkey).expect("validate() rejected an unparseable hotkey")
    }
}

/// A push-to-talk binding, parsed once so the CLI and the tests agree on it.
///
/// This is a *trigger* for push-to-talk, not a listening mode: nothing is
/// captured until the binding is pressed and nothing is submitted until the
/// transcript is explicitly released and submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub key: HotkeyKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyKey {
    Space,
    Enter,
    Tab,
    Escape,
    F(u8),
    Char(char),
}

impl Hotkey {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut binding = Self {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            key: HotkeyKey::Space,
        };
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        if parts.len() < 2 || parts.iter().any(|p| p.is_empty()) {
            return Err(
                "hotkey needs at least one modifier and one key, e.g. Ctrl+Alt+Space".into(),
            );
        }
        for modifier in &parts[..parts.len() - 1] {
            match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => binding.ctrl = true,
                "alt" => binding.alt = true,
                "shift" => binding.shift = true,
                "win" | "meta" | "super" => binding.win = true,
                other => return Err(format!("unknown hotkey modifier `{other}`")),
            }
        }
        if !(binding.ctrl || binding.alt || binding.win) {
            return Err(
                "hotkey needs Ctrl, Alt or Win so ordinary typing cannot trigger it".into(),
            );
        }
        let key = parts[parts.len() - 1];
        let lower = key.to_ascii_lowercase();
        binding.key = match lower.as_str() {
            "space" => HotkeyKey::Space,
            "enter" | "return" => HotkeyKey::Enter,
            "tab" => HotkeyKey::Tab,
            "esc" | "escape" => HotkeyKey::Escape,
            _ if lower.starts_with('f')
                && lower[1..]
                    .parse::<u8>()
                    .is_ok_and(|n| (1..=24).contains(&n)) =>
            {
                HotkeyKey::F(lower[1..].parse::<u8>().unwrap())
            }
            _ if key.chars().count() == 1
                && key
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric()) =>
            {
                HotkeyKey::Char(key.chars().next().unwrap().to_ascii_uppercase())
            }
            _ => return Err(format!("unsupported hotkey key `{key}`")),
        };
        Ok(binding)
    }

    /// Windows virtual-key code for the non-modifier part of the binding.
    pub fn virtual_key(&self) -> u16 {
        match self.key {
            HotkeyKey::Space => 0x20,
            HotkeyKey::Enter => 0x0D,
            HotkeyKey::Tab => 0x09,
            HotkeyKey::Escape => 0x1B,
            HotkeyKey::F(n) => 0x6F + n as u16,
            HotkeyKey::Char(c) => c as u16,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file {path}: {reason}")]
    Io { path: PathBuf, reason: String },

    #[error("config file {path}: unsupported config_version {found}; this build supports exactly {supported}. Refusing to guess how older/newer keys map.")]
    UnsupportedVersion {
        path: PathBuf,
        found: u64,
        supported: u32,
    },

    #[error("config file {path}: {message}")]
    Parse { path: PathBuf, message: String },

    #[error("environment variable {name} is invalid: {reason}")]
    Env { name: &'static str, reason: String },

    #[error("invalid configuration value for `{field}`: {reason}")]
    Validation { field: &'static str, reason: String },
}

/// Where a loaded configuration came from, for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    CompiledDefaults,
    File(PathBuf),
    Env(&'static str),
}

/// Outcome of a load: the config plus everything a caller should surface.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: AppConfig,
    /// Layers actually applied, in order.
    pub sources: Vec<ConfigSource>,
    /// Unknown keys found in user files. Non-fatal by design.
    pub warnings: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Schema. Every field has a default so a partial user file is valid.
// No field is `Option`: an absent value means "use the default", never "null".
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default = "default_config_version")]
    pub config_version: u32,
    #[serde(default)]
    pub core: CoreConfig,
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub ipc: IpcConfig,
    #[serde(default)]
    pub orchestrator: OrchestratorConfig,
    #[serde(default)]
    pub process: ProcessConfig,
    #[serde(default)]
    pub terminal: TerminalConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
    /// Bounds for the `jean find` intent. Phase 1 added this section because a
    /// recursive walk needs limits that are not a property of the workspace.
    #[serde(default)]
    pub search: SearchConfig,
    /// Notification delivery. Only `[ui].notifications` and its AUMID are
    /// implemented; there is no tray, no window, no taskbar button.
    #[serde(default)]
    pub ui: UiConfig,
    /// Model-driven planning. Added by Phase 2; `provider = "none"` keeps the build
    /// deterministic, which is the shipped default.
    #[serde(default)]
    pub model: ModelConfig,
    /// Delegation to an external coding agent, added by Phase 3. `enabled = false` keeps every
    /// Phase-0/1/2 command working and makes `jean delegate` refuse.
    #[serde(default)]
    pub delegation: DelegationConfig,
    /// MCP servers JEAN spawns, and the tool ids JEAN publishes when it is the server.
    #[serde(default)]
    pub mcp: McpConfig,
    /// A browser under JEAN's own control. Off by default: nothing launches a browser until an
    /// operator says so (D-044).
    #[serde(default)]
    pub browser: BrowserConfig,
    /// The desktop boundary: what may be looked at, what may be clicked, and with what bounds.
    ///
    /// Its own section rather than an extension of `[vision]`, because `[vision]` is a deferred
    /// document and the desktop actuator is shipping now; the two names stay separate so that turning
    /// pixel capture on is never the same decision as letting JEAN press a button.
    #[serde(default)]
    pub desktop: DesktopConfig,
    #[serde(default)]
    pub voice: VoiceConfig,
}

fn default_config_version() -> u32 {
    SUPPORTED_CONFIG_VERSION
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreConfig {
    #[serde(default = "d_name")]
    pub name: String,
    #[serde(default = "d_log_level")]
    pub log_level: String,
    /// Empty means "the platform default" (`%LOCALAPPDATA%\JEAN\logs`).
    #[serde(default)]
    pub log_dir: String,
    #[serde(default = "d_log_rotation")]
    pub log_rotation: String,
    #[serde(default = "d_log_retention_days")]
    pub log_retention_days: u32,
    #[serde(default = "d_log_max_files")]
    pub log_max_files: u32,
    /// Empty means "the platform default" (`%LOCALAPPDATA%\JEAN`).
    #[serde(default)]
    pub data_dir: String,
}

fn d_name() -> String {
    "J.E.A.N.".to_owned()
}
fn d_log_level() -> String {
    "info".to_owned()
}
fn d_log_rotation() -> String {
    "daily".to_owned()
}
fn d_log_retention_days() -> u32 {
    14
}
fn d_log_max_files() -> u32 {
    30
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    /// Empty means `<data_dir>\jean.db`.
    #[serde(default)]
    pub path: String,
    #[serde(default = "d_busy_timeout_ms")]
    pub busy_timeout_ms: u32,
    #[serde(default = "d_journal_mode")]
    pub journal_mode: String,
    #[serde(default = "d_synchronous")]
    pub synchronous: String,
    #[serde(default = "d_true")]
    pub migrations_on_start: bool,
    #[serde(default = "d_retain_events_days")]
    pub retain_events_days: u32,
}

fn d_busy_timeout_ms() -> u32 {
    5000
}
fn d_journal_mode() -> String {
    "wal".to_owned()
}
fn d_synchronous() -> String {
    "NORMAL".to_owned()
}
fn d_retain_events_days() -> u32 {
    90
}
const fn d_true() -> bool {
    true
}
const fn d_false() -> bool {
    false
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcConfig {
    #[serde(default = "d_ipc_transport")]
    pub transport: String,
    /// Empty means the default per-task pipe name derived from the task id.
    #[serde(default)]
    pub pipe_prefix: String,
    /// Minimum integrity level accepted from a connecting client.
    #[serde(default = "d_min_integrity")]
    pub min_client_integrity: String,
    #[serde(default = "d_ipc_auth")]
    pub auth: String,
    /// Phase 0 never opens a network listener. This exists so that turning it
    /// on is a deliberate, reviewable act rather than an accident.
    #[serde(default = "d_false")]
    pub http_enabled: bool,
}

fn d_ipc_transport() -> String {
    "named_pipe".to_owned()
}
fn d_min_integrity() -> String {
    "medium".to_owned()
}
fn d_ipc_auth() -> String {
    "peer_owner".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorConfig {
    #[serde(default = "d_max_concurrent_tasks")]
    pub max_concurrent_tasks: u32,
    #[serde(default = "d_default_timeout_secs")]
    pub default_timeout_secs: u64,
    #[serde(default = "d_default_retry_max")]
    pub default_retry_max: u32,
    /// D12: never report success on launch alone. Phase 0 records this and
    /// surfaces it; the per-tool verification predicates arrive in Phase 1.
    #[serde(default = "d_true")]
    pub verification_required: bool,
    #[serde(default = "d_bounded_task_queue")]
    pub bounded_task_queue: u32,
    #[serde(default = "d_true")]
    pub resume_on_restart: bool,
    /// Reconcile in-flight rows whose runner process is gone. Off only for
    /// tests that need to fabricate orphaned rows without them being reaped.
    #[serde(default = "d_true")]
    pub reconcile_on_start: bool,
}

fn d_max_concurrent_tasks() -> u32 {
    4
}
fn d_default_timeout_secs() -> u64 {
    300
}
fn d_default_retry_max() -> u32 {
    2
}
fn d_bounded_task_queue() -> u32 {
    64
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessConfig {
    /// D-003/R3: every child is assigned to a Job Object with kill-on-close.
    /// Turning this off is supported only so tests can prove the guard works.
    #[serde(default = "d_true")]
    pub use_job_objects: bool,
    #[serde(default = "d_true")]
    pub kill_on_job_close: bool,
    #[serde(default = "d_job_active_proc_limit")]
    pub job_active_proc_limit: u32,
    /// No blanket inheritance. Only allowlisted variables reach the child.
    #[serde(default = "d_false")]
    pub inherit_environment: bool,
    #[serde(default = "default_env_allowlist")]
    pub env_allowlist: Vec<String>,
    #[serde(default = "default_redact_patterns")]
    pub redact_patterns: Vec<String>,
    #[serde(default = "d_capture_output_max_bytes")]
    pub capture_output_max_bytes: u64,
    #[serde(default = "d_kill_grace_ms")]
    pub kill_grace_ms: u32,
}

fn d_job_active_proc_limit() -> u32 {
    64
}
fn d_capture_output_max_bytes() -> u64 {
    10 * 1024 * 1024
}
fn d_kill_grace_ms() -> u32 {
    3000
}

/// Windows variables the Phase-0 example tools genuinely need.
///
/// `SystemRoot` is the one that breaks things silently: without it, PowerShell
/// and most Win32 programs fail to locate system DLLs. `PATHEXT` and `COMSPEC`
/// are required for `cmd.exe` resolution. `TEMP`/`TMP` are required by anything
/// that writes a temporary file, including `Start-Process`.
pub fn default_env_allowlist() -> Vec<String> {
    [
        "PATH",
        "PATHEXT",
        "SystemRoot",
        "windir",
        "SystemDrive",
        "COMSPEC",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "PROGRAMDATA",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "COMMONPROGRAMFILES",
        "USERNAME",
        "COMPUTERNAME",
        "USERDOMAIN",
        "OS",
        "PROCESSOR_ARCHITECTURE",
        "NUMBER_OF_PROCESSORS",
        "PSModulePath",
        "HOME",
        "LANG",
        "LC_ALL",
        "TERM",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Default redaction patterns.
///
/// **Order matters.** Patterns are applied in sequence, and the `bearer`
/// pattern must run before the generic `key=value` pattern: otherwise
/// `Authorization: Bearer <token>` has `Bearer` redacted as the value of the
/// `Authorization` key, leaving the actual token in the log. See the test
/// `redact::tests::authorization_header_does_not_leak_the_token`.
///
/// A pattern whose *last* capture group is the secret has only that group
/// replaced, so the key name survives and the log stays diagnosable. A pattern
/// with no capture group has its whole match replaced.
pub fn default_redact_patterns() -> Vec<String> {
    [
        r"(?i)(\bbearer\s+)([A-Za-z0-9._\-]+)",
        r"(?i)(token|key|secret|password|passwd|pwd|authorization|cookie|api[-_]?key)(\s*[=:]\s*)(\S+)",
        // The same keys when passed as command-line flags, which is how they are
        // actually typed on Windows: `Set-Content -Password hunter2`,
        // `--api-key sk_…`, `/secret abc`. The `key=value` pattern above needs a
        // `=` or `:` and cannot see these, and the first version of this list
        // therefore let a flag-shaped secret into the audit log verbatim — caught
        // by `jean-policy`'s `secrets_are_redacted_by_the_writer_not_by_the_caller`.
        //
        // A leading `-` or `/` is required, so the ordinary English use of these
        // words in prose and in paths is left alone.
        r"(?i)((?:^|\s)[-/][A-Za-z0-9_-]*(?:password|passwd|pwd|token|secret|credential|api[-_]?key)[A-Za-z0-9_-]*[ =])(\S+)",
        r"\b(?:sk-|ghp_|gho_|github_pat_|xox[baprs]-)[A-Za-z0-9._\-]{8,}",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalConfig {
    #[serde(default = "d_interactive_backend")]
    pub interactive_backend: String,
    #[serde(default = "d_pty_rows")]
    pub pty_rows: u16,
    #[serde(default = "d_pty_cols")]
    pub pty_cols: u16,
    #[serde(default = "d_default_shell")]
    pub default_shell: String,
}

fn d_interactive_backend() -> String {
    "conpty".to_owned()
}
const fn d_pty_rows() -> u16 {
    40
}
const fn d_pty_cols() -> u16 {
    120
}
fn d_default_shell() -> String {
    "powershell".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsConfig {
    /// Empty means `<config_dir>\tools.toml`.
    #[serde(default)]
    pub registry_path: String,
    #[serde(default = "d_health_check_interval_secs")]
    pub health_check_interval_secs: u64,
    #[serde(default = "d_health_check_timeout_ms")]
    pub health_check_timeout_ms: u32,
    /// Consecutive failures before a tool is reported unhealthy.
    #[serde(default = "d_mark_unhealthy_after")]
    pub mark_unhealthy_after: u32,
    /// Where `jean apps` / `jean launch` look for installed applications.
    ///
    /// `PLANNING/04-SETTINGS.md` also lists `mcp` and `acp`; those sources are
    /// Phase 3 and are refused here rather than accepted and ignored, so a user
    /// who writes them gets told they do nothing yet.
    #[serde(default = "default_discovery_sources")]
    pub discovery: Vec<String>,
}

fn d_mark_unhealthy_after() -> u32 {
    3
}

fn default_discovery_sources() -> Vec<String> {
    ["path", "start_menu", "registry"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn d_health_check_interval_secs() -> u64 {
    300
}
fn d_health_check_timeout_ms() -> u32 {
    5000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Empty means "not yet chosen"; the CLI prompts or refuses to write.
    #[serde(default)]
    pub root: String,
    #[serde(default = "d_false")]
    pub allow_write_outside_root: bool,
    #[serde(default = "d_delete_mode")]
    pub delete_mode: String,
    #[serde(default = "d_max_file_read_bytes")]
    pub max_file_read_bytes: u64,
}

fn d_delete_mode() -> String {
    "recycle_bin".to_owned()
}
fn d_max_file_read_bytes() -> u64 {
    50 * 1024 * 1024
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    #[serde(default = "d_default_tier")]
    pub default_tier: String,
    #[serde(default = "d_approval_timeout_secs")]
    pub approval_timeout_secs: u64,
    /// Must default to deny. D-011: a timeout is not consent.
    #[serde(default = "d_on_timeout")]
    pub on_timeout: String,
    #[serde(default = "d_true")]
    pub audit_enabled: bool,
    /// Empty means `<data_dir>\audit.jsonl`.
    #[serde(default)]
    pub audit_path: String,
    #[serde(default = "d_true")]
    pub audit_redacts_secrets: bool,
    /// D-010: never run the whole process elevated.
    #[serde(default = "d_false")]
    pub run_elevated_by_default: bool,
    #[serde(default = "d_true")]
    pub block_on_uipi: bool,
    /// Capability names that require the operator to approve *that invocation*.
    ///
    /// An operation is matched by the capability strings it declares, never by
    /// the tool's name: a tool called `safe-helper` that writes outside the
    /// workspace still matches `write_outside_workspace`.
    #[serde(default = "default_require_approval")]
    pub require_approval: Vec<String>,
    /// Capability names refused outright, with no prompt and no approval path.
    #[serde(default = "default_auto_deny")]
    pub auto_deny: Vec<String>,
    /// Whether an approval may be remembered at all.
    #[serde(default = "d_true")]
    pub allow_remember: bool,
    /// What a remembered approval is bound to. Only `operation_and_arguments` is
    /// accepted; see [`APPROVAL_SCOPE_OPERATION_AND_ARGUMENTS`].
    #[serde(default = "d_approval_scope")]
    pub approval_scope: String,
    /// How long a remembered approval stays usable.
    #[serde(default = "d_approval_remember_secs")]
    pub approval_remember_secs: u64,
    /// Cap on remembered approvals, so the ledger cannot grow without bound.
    #[serde(default = "d_max_remembered_approvals")]
    pub max_remembered_approvals: u32,
    /// Named in the refusal for an `elevated` operation. This build ships no
    /// such sidecar; the key exists so the refusal can tell the operator what to
    /// provide (D-010).
    #[serde(default = "d_elevation_helper")]
    pub elevation_helper: String,
    /// Refusing `false` is the enforcement: nothing in this build stores a
    /// credential, and a config that asked for it would be asking for a feature
    /// that does not exist.
    #[serde(default = "d_true")]
    pub never_store_credentials: bool,
}

/// A remembered approval is bound to the operation **and** its validated
/// arguments and target, not to a tool at a tier.
pub const APPROVAL_SCOPE_OPERATION_AND_ARGUMENTS: &str = "operation_and_arguments";

/// The wording `PLANNING/04-SETTINGS.md` used before the requirement was
/// clarified. Kept as a constant so the refusal can name it precisely instead of
/// printing an opaque "invalid value".
pub const APPROVAL_SCOPE_REJECTED_LEGACY: &str = "tool_and_tier";

fn d_approval_scope() -> String {
    APPROVAL_SCOPE_OPERATION_AND_ARGUMENTS.to_owned()
}
fn d_approval_remember_secs() -> u64 {
    24 * 60 * 60
}
fn d_max_remembered_approvals() -> u32 {
    128
}
fn d_elevation_helper() -> String {
    "jean-elevated.exe".to_owned()
}

/// The documented approval list, verbatim from `PLANNING/04-SETTINGS.md`.
pub fn default_require_approval() -> Vec<String> {
    [
        "write_outside_workspace",
        "delete_permanent",
        "process_kill",
        "service_control",
        "registry_write",
        "install_software",
        "credential_read",
        "network_egress_new_domain",
        "elevated",
        "modify_security_settings",
        "bulk_file_change",
        // Phase 4 adds two capability names that are deliberately *not* in this default list:
        // `desktop.act` and `desktop.input`. Adding either here makes every desktop action ask a person
        // who is sitting at a terminal — and since a prompt nobody can answer is a denial, that also makes
        // the action unavailable to a script or an agent-driven run, which is a real choice an operator
        // can make but is not the default one. The shipped boundary is `[desktop] enabled` plus
        // `allow_synthetic_input` plus the audit line, exactly as `[browser] enabled` gates the browser.
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// The documented auto-deny list, verbatim from `PLANNING/04-SETTINGS.md`.
pub fn default_auto_deny() -> Vec<String> {
    [
        "format_volume",
        "registry_hive_load",
        "disable_defender",
        "modify_uac",
        "shadow_copy_delete",
        "disable_audit_logging",
        "write_to_own_config_from_model",
        "grant_self_permission",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn d_default_tier() -> String {
    "normal".to_owned()
}
fn d_approval_timeout_secs() -> u64 {
    120
}
fn d_on_timeout() -> String {
    "deny".to_owned()
}

// Hand-written Default impls so the defaults are the single source of truth and
// stay readable next to the `d_*` functions above. A `macro_rules!` is only
// visible to what follows it in the file, so this definition sits before the
// first section that uses it.
macro_rules! impl_default_from_fns {
    ($t:ty { $($field:ident => $expr:expr),* $(,)? }) => {
        impl Default for $t {
            fn default() -> Self {
                Self { $($field: $expr),* }
            }
        }
    };
}

// ─────────────────────────────────────────────────────────────────────────────

/// Bounds on a file-location search.
///
/// These are limits, not hints: the walk stops and says which limit it hit, so an
/// operator asking for `C:\` does not discover that the CLI has no exit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    #[serde(default = "d_max_results")]
    pub max_results: u32,
    /// Directory entries examined before the walk gives up. This is the bound
    /// that stops a search of a whole drive from running for an hour while
    /// producing nothing that matches.
    #[serde(default = "d_max_entries")]
    pub max_entries: u32,
    #[serde(default = "d_search_deadline_secs")]
    pub deadline_secs: u64,
    #[serde(default = "d_max_depth")]
    pub max_depth: u32,
    /// Following a reparse point would let a symlink or junction move the walk
    /// outside the root that was authorised, and can cycle. Refused if true.
    #[serde(default = "d_false")]
    pub follow_reparse_points: bool,
    #[serde(default = "d_true")]
    pub ignore_case: bool,
    /// Dotfiles are hidden on Windows only by attribute, not by name; `false`
    /// skips entries with the hidden attribute set.
    #[serde(default = "d_false")]
    pub include_hidden: bool,
}

fn d_max_results() -> u32 {
    200
}
fn d_max_entries() -> u32 {
    200_000
}
fn d_search_deadline_secs() -> u64 {
    30
}
fn d_max_depth() -> u32 {
    12
}

impl_default_from_fns!(SearchConfig {
    max_results => d_max_results(),
    max_entries => d_max_entries(),
    deadline_secs => d_search_deadline_secs(),
    max_depth => d_max_depth(),
    follow_reparse_points => false,
    ignore_case => true,
    include_hidden => false,
});

/// Model access for planning, added by Phase 2.
///
/// This section controls only *how a plan is obtained*: which endpoint, which model name, how
/// long to wait, how much to accept, how many transport retries. There is deliberately no key
/// for what the model may do — no tool allowlist, no tier, no auto-approve toggle. Those live
/// in `[policy]`, which a model's output cannot write to (D-011).
///
/// "OpenAI-compatible" is a family name, not a schema. What is sent is the documented subset in
/// `jean-model`'s header; anything outside it is a load error rather than a request that fails
/// later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// `none` | `ollama` | `openai_compatible`. `none` keeps every deterministic command working
    /// and makes `jean plan` / `jean do` refuse with a message naming what to configure.
    #[serde(default = "d_model_provider")]
    pub provider: String,
    /// Absolute HTTP endpoint, path included for servers mounted under a prefix, e.g.
    /// `http://127.0.0.1:8080/v1`. No userinfo, query or fragment.
    #[serde(default = "d_model_base_url")]
    pub base_url: String,
    /// Model name sent to the provider. Required whenever a provider is selected.
    #[serde(default)]
    pub model: String,
    /// The *name* of an environment variable holding a bearer token, never the token itself.
    #[serde(default = "d_model_api_key_env")]
    pub api_key_env: String,
    /// Deadline for one model request, including the whole response body.
    #[serde(default = "d_model_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// Ceiling on the bytes read from a response. Larger is a transport failure, not a truncation.
    #[serde(default = "d_model_max_response_bytes")]
    pub max_response_bytes: usize,
    /// Ceiling on the prompt built for one request, so a large registry cannot produce an
    /// unbounded request.
    #[serde(default = "d_model_max_prompt_bytes")]
    pub max_prompt_bytes: usize,
    /// Token budget asked of the provider, when it understands one.
    #[serde(default = "d_model_max_output_tokens")]
    pub max_output_tokens: u32,
    /// Sampling temperature, capped: a plan is a decision, and a stochastic decision cannot be
    /// reproduced from the record.
    #[serde(default = "d_model_temperature")]
    pub temperature: f32,
    /// Ask for the streamed response shape (NDJSON for Ollama, SSE for OpenAI-compatible). Off by
    /// default: one bounded JSON body is easier to verify than a reassembled stream.
    #[serde(default)]
    pub stream: bool,
    /// Backoff between model *transport* retries; its length is the retry count. Never applied to
    /// a tool: a lost response is reconciled or reported, not replayed.
    #[serde(default = "d_model_retry_backoff_ms")]
    pub retry_backoff_ms: Vec<u64>,
    /// Plan-length ceiling. Tightens [`crate::plan::MAX_PLAN_STEPS`]; never widens it.
    #[serde(default = "d_model_max_plan_steps")]
    pub max_plan_steps: usize,
    /// `record` | `model` — who writes the closing summary's prose. The facts (states, verdicts,
    /// exit codes) always come from the execution record.
    #[serde(default = "d_model_summary_wording")]
    pub summary_wording: String,
    /// Work a model may never be asked to decide. Enforced: the compiled floor in
    /// [`NEVER_DELEGATE_TO_MODEL`] is checked at load, so dropping an entry is refused rather
    /// than obeyed.
    #[serde(default = "default_never_delegate_to_model")]
    pub never_delegate_to_model: Vec<String>,
}

/// The decisions never put to a model, whatever configuration says.
pub const NEVER_DELEGATE_TO_MODEL: &[&str] = &[
    "permission_checks",
    "argument_validation",
    "path_handling",
    "process_supervision",
    "task_state_transitions",
    "verification",
    "policy_decisions",
    "secret_handling",
];

fn d_model_provider() -> String {
    "none".to_owned()
}
fn d_model_base_url() -> String {
    "http://127.0.0.1:11434".to_owned()
}
fn d_model_api_key_env() -> String {
    "JEAN_MODEL_API_KEY".to_owned()
}
const fn d_model_request_timeout_secs() -> u64 {
    120
}
const fn d_model_max_response_bytes() -> usize {
    1024 * 1024
}
const fn d_model_max_prompt_bytes() -> usize {
    256 * 1024
}
const fn d_model_max_output_tokens() -> u32 {
    2_048
}
const fn d_model_temperature() -> f32 {
    0.0
}
fn d_model_retry_backoff_ms() -> Vec<u64> {
    vec![500, 2_000, 8_000]
}
const fn d_model_max_plan_steps() -> usize {
    8
}
fn d_model_summary_wording() -> String {
    "record".to_owned()
}

/// The documented floor, so a user file that drops an entry is refused instead of obeyed.
pub fn default_never_delegate_to_model() -> Vec<String> {
    NEVER_DELEGATE_TO_MODEL
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

impl_default_from_fns!(ModelConfig {
    provider => d_model_provider(),
    base_url => d_model_base_url(),
    model => String::new(),
    api_key_env => d_model_api_key_env(),
    request_timeout_secs => d_model_request_timeout_secs(),
    max_response_bytes => d_model_max_response_bytes(),
    max_prompt_bytes => d_model_max_prompt_bytes(),
    max_output_tokens => d_model_max_output_tokens(),
    temperature => d_model_temperature(),
    stream => false,
    retry_backoff_ms => d_model_retry_backoff_ms(),
    max_plan_steps => d_model_max_plan_steps(),
    summary_wording => d_model_summary_wording(),
    never_delegate_to_model => default_never_delegate_to_model(),
});

/// Notification delivery.
///
/// Phase 1 has a CLI and no tray, so `[ui]` means exactly two things here:
/// whether a finished task announces itself, and the identity it announces
/// under. Everything else the settings document lists under `[ui]` is later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    /// `none` | `toast`.
    ///
    /// The default is `none`, not the `toast` that `PLANNING/04-SETTINGS.md`
    /// shows. A toast is drawn on the operator's desktop and, to be attributed
    /// at all, needs the AppUserModelId written into `HKCU` — a per-user state
    /// change that a completed command should not make by itself. Opt in with
    /// `jean notify register`.
    #[serde(default = "d_notifications")]
    pub notifications: String,
    #[serde(default = "d_aumid")]
    pub app_user_model_id: String,
}

fn d_notifications() -> String {
    "none".to_owned()
}
fn d_aumid() -> String {
    "JEAN.Orchestrator".to_owned()
}

impl_default_from_fns!(UiConfig {
    notifications => d_notifications(),
    app_user_model_id => d_aumid(),
});

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: SUPPORTED_CONFIG_VERSION,
            core: CoreConfig::default(),
            store: StoreConfig::default(),
            ipc: IpcConfig::default(),
            orchestrator: OrchestratorConfig::default(),
            process: ProcessConfig::default(),
            terminal: TerminalConfig::default(),
            tools: ToolsConfig::default(),
            workspace: WorkspaceConfig::default(),
            policy: PolicyConfig::default(),
            search: SearchConfig::default(),
            ui: UiConfig::default(),
            model: ModelConfig::default(),
            delegation: DelegationConfig::default(),
            mcp: McpConfig::default(),
            browser: BrowserConfig::default(),
            desktop: DesktopConfig::default(),
            voice: VoiceConfig::default(),
        }
    }
}

impl_default_from_fns!(CoreConfig {
    name => d_name(),
    log_level => d_log_level(),
    log_dir => String::new(),
    log_rotation => d_log_rotation(),
    log_retention_days => d_log_retention_days(),
    log_max_files => d_log_max_files(),
    data_dir => String::new(),
});

impl_default_from_fns!(StoreConfig {
    path => String::new(),
    busy_timeout_ms => d_busy_timeout_ms(),
    journal_mode => d_journal_mode(),
    synchronous => d_synchronous(),
    migrations_on_start => true,
    retain_events_days => d_retain_events_days(),
});

impl_default_from_fns!(IpcConfig {
    transport => d_ipc_transport(),
    pipe_prefix => String::new(),
    min_client_integrity => d_min_integrity(),
    auth => d_ipc_auth(),
    http_enabled => false,
});

impl_default_from_fns!(OrchestratorConfig {
    max_concurrent_tasks => d_max_concurrent_tasks(),
    default_timeout_secs => d_default_timeout_secs(),
    default_retry_max => d_default_retry_max(),
    verification_required => true,
    bounded_task_queue => d_bounded_task_queue(),
    resume_on_restart => true,
    reconcile_on_start => true,
});

impl_default_from_fns!(ProcessConfig {
    use_job_objects => true,
    kill_on_job_close => true,
    job_active_proc_limit => d_job_active_proc_limit(),
    inherit_environment => false,
    env_allowlist => default_env_allowlist(),
    redact_patterns => default_redact_patterns(),
    capture_output_max_bytes => d_capture_output_max_bytes(),
    kill_grace_ms => d_kill_grace_ms(),
});

impl_default_from_fns!(TerminalConfig {
    interactive_backend => d_interactive_backend(),
    pty_rows => d_pty_rows(),
    pty_cols => d_pty_cols(),
    default_shell => d_default_shell(),
});

impl_default_from_fns!(ToolsConfig {
    registry_path => String::new(),
    health_check_interval_secs => d_health_check_interval_secs(),
    health_check_timeout_ms => d_health_check_timeout_ms(),
    mark_unhealthy_after => d_mark_unhealthy_after(),
    discovery => default_discovery_sources(),
});

impl_default_from_fns!(WorkspaceConfig {
    root => String::new(),
    allow_write_outside_root => false,
    delete_mode => d_delete_mode(),
    max_file_read_bytes => d_max_file_read_bytes(),
});

impl_default_from_fns!(PolicyConfig {
    default_tier => d_default_tier(),
    approval_timeout_secs => d_approval_timeout_secs(),
    on_timeout => d_on_timeout(),
    audit_enabled => true,
    audit_path => String::new(),
    audit_redacts_secrets => true,
    run_elevated_by_default => false,
    block_on_uipi => true,
    require_approval => default_require_approval(),
    auto_deny => default_auto_deny(),
    allow_remember => true,
    approval_scope => d_approval_scope(),
    approval_remember_secs => d_approval_remember_secs(),
    max_remembered_approvals => d_max_remembered_approvals(),
    elevation_helper => d_elevation_helper(),
    never_store_credentials => true,
});

// ─────────────────────────────────────────────────────────────────────────────
// Phase 3: delegation to an external coding agent, MCP, and a browser under
// JEAN's own control.
//
// Every number here is a ceiling the implementation enforces, not a hint. The
// protocol SDKs JEAN now depends on frame NDJSON with an unbounded line reader and
// spawn nothing on their own behalf, so these bounds — and the job object in
// `jean-process::piped` — are the only limits on a worker that misbehaves.
// ─────────────────────────────────────────────────────────────────────────────

/// One worker connector.
///
/// `command` and `args` are a fixed argv resolved the same way every other tool is
/// (`jean-core::resolve`), never a shell string: the prompt travels as a protocol
/// message or as one argument, and a path an operator did not register cannot become
/// a worker by editing this file's text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    /// The name the operator types (`jean delegate --worker opencode`).
    #[serde(default)]
    pub name: String,
    /// `acp` | `codex-cli`. The closed set is in code; a new kind is a build change.
    #[serde(default = "d_worker_kind")]
    pub kind: String,
    /// Executable name or path. Required when the worker is enabled.
    #[serde(default)]
    pub command: String,
    /// Fixed argv handed to the executable before anything JEAN adds.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Model name passed to the worker when it takes one. Never a prompt.
    #[serde(default)]
    pub model: String,
    /// Environment variable **names** forwarded to the worker. Values are never
    /// configurable here, and a name listed here is forwarded only if the process
    /// allowlist in `[process].env_allowlist` also permits it.
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    /// The capability token the worker's actions are decided under. A worker with no
    /// declared capability is not a privileged one: it lands on the owner-command tier
    /// like any other tool.
    #[serde(default = "d_worker_capability")]
    pub capability: String,
    /// `codex-cli` only: the sandbox the agent runs in. `danger-full-access` is refused
    /// at load, because a delegation that can run anything is not a delegation.
    #[serde(default = "d_worker_sandbox")]
    pub sandbox: String,
}

/// Delegation: one outcome stated by a person, one external worker, one verification
/// the worker does not get to choose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Which worker `jean delegate` uses when `--worker` is absent.
    #[serde(default = "d_default_worker")]
    pub default_worker: String,
    /// Concurrent workers. This build ships **1**: the cancellation design has one
    /// active task per process (D-028), and claiming otherwise would be a promise the
    /// code cannot keep. A larger value is a load error, not a silent clamp.
    #[serde(default = "d_max_parallel")]
    pub max_parallel: u32,
    /// Deadline for one worker session, from spawn to teardown.
    #[serde(default = "d_session_timeout")]
    pub session_timeout_secs: u64,
    /// Silence from a worker before it is declared stalled. Distinct from the session
    /// deadline: a stalled worker is a reportable failure, a slow one is not.
    #[serde(default = "d_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// How long a worker may wait for a permission answer before the request is
    /// refused. Waiting forever is not an answer.
    #[serde(default = "d_permission_timeout")]
    pub permission_timeout_secs: u64,
    /// Grace before the job is terminated outright, on cancel and on completion.
    #[serde(default = "d_shutdown_grace_ms")]
    pub shutdown_grace_ms: u32,
    /// Events held between the reader thread and the consumer. A full queue stops the
    /// reader, which stops draining the socket, which makes the worker notice — that
    /// back-pressure is the point; dropping progress is not.
    #[serde(default = "d_max_event_queue")]
    pub max_event_queue: usize,
    /// Largest single protocol line accepted, in bytes. The SDK has no frame ceiling,
    /// so this is the only thing between a chatty agent and this process's memory.
    #[serde(default = "d_max_frame_bytes")]
    pub max_frame_bytes: usize,
    /// Ceiling on captured stdout+stderr bytes per worker, per stream.
    #[serde(default = "d_max_worker_output_bytes")]
    pub max_output_bytes: u64,
    #[serde(default)]
    pub workers: Vec<WorkerConfig>,
    /// The verification command JEAN runs itself, resolved **before** delegation so the
    /// worker can be told what has to pass. Empty means `jean delegate` refuses: an
    /// unverified delegation is the failure mode this phase exists to prevent.
    #[serde(default)]
    pub verify_command: String,
    #[serde(default)]
    pub verify_args: Vec<String>,
    #[serde(default = "d_verify_timeout")]
    pub verify_timeout_secs: u64,
    /// Run the worker in a detached `git worktree` of the workspace instead of the
    /// operator's own checkout.
    #[serde(default = "d_true")]
    pub isolate_in_worktree: bool,
    /// Where worktrees are created. Empty means under the resolved data directory.
    #[serde(default)]
    pub worktree_root: String,
    /// Refuse to delegate when the workspace has uncommitted changes that a worktree
    /// would not carry, rather than silently discarding someone's work.
    #[serde(default = "d_true")]
    pub require_clean_baseline: bool,
    /// How many changed files the summary lists before collapsing to a count.
    #[serde(default = "d_max_changed_files")]
    pub max_changed_files_reported: usize,
}

/// MCP: JEAN as client to a server it spawns, and as server to a client that connects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_mcp_timeout")]
    pub request_timeout_secs: u64,
    /// Largest single JSON-RPC line to or from a server.
    #[serde(default = "d_max_frame_bytes")]
    pub max_message_bytes: usize,
    /// Ceiling on one tool result's content, in bytes.
    #[serde(default = "d_mcp_max_result_bytes")]
    pub max_result_bytes: usize,
    #[serde(default = "d_shutdown_grace_ms")]
    pub shutdown_grace_ms: u32,
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    /// Tool ids this build will publish when JEAN itself is the server. Publishing is
    /// opt-in per tool: a registry the operator can see is not a registry a stranger can
    /// drive.
    #[serde(default)]
    pub expose: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    #[serde(default)]
    pub name: String,
    /// Executable and fixed argv of the server process.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Tool names from this server JEAN will offer. Empty means none: a server that
    /// publishes thirty tools does not get thirty capabilities by default.
    #[serde(default)]
    pub allow_tools: Vec<String>,
    #[serde(default = "d_mcp_capability")]
    pub capability: String,
    #[serde(default)]
    pub env_passthrough: Vec<String>,
}

/// A browser JEAN starts, owns and closes. Never the operator's.
///
/// Disabled by default and, when enabled, restricted to a JEAN-owned child process with
/// its own temporary profile. Attaching to a browser that is already running is not
/// implemented, because that browser belongs to a person, and its profile holds their
/// sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Path to the browser executable. Empty means detect an installed Chrome, Edge or
    /// Brave; detection that finds nothing is a reported blocker, not a download.
    #[serde(default)]
    pub browser_path: String,
    /// Headless by default. A window the operator did not ask for is a surprise, and
    /// `false` is the choice that needs a reason.
    #[serde(default = "d_true")]
    pub headless: bool,
    #[serde(default = "d_browser_startup_timeout")]
    pub startup_timeout_secs: u64,
    #[serde(default = "d_browser_action_timeout")]
    pub action_timeout_secs: u64,
    /// Ceiling on page content returned to a caller, in bytes.
    #[serde(default = "d_max_page_bytes")]
    pub max_page_bytes: usize,
    /// Origins a navigation may reach. `file` and loopback `http`/`https` by default;
    /// anything else is refused at the intent, not by a rule inside a browser.
    #[serde(default = "d_allowed_origins")]
    pub allowed_origins: Vec<String>,
}

fn d_worker_kind() -> String {
    "acp".to_owned()
}
fn d_worker_capability() -> String {
    "agent.delegate".to_owned()
}
fn d_worker_sandbox() -> String {
    "workspace-write".to_owned()
}
fn d_default_worker() -> String {
    "opencode".to_owned()
}
const fn d_max_parallel() -> u32 {
    1
}
const fn d_session_timeout() -> u64 {
    900
}
const fn d_idle_timeout() -> u64 {
    180
}
const fn d_permission_timeout() -> u64 {
    120
}
const fn d_shutdown_grace_ms() -> u32 {
    2_000
}
const fn d_max_event_queue() -> usize {
    256
}
const fn d_max_frame_bytes() -> usize {
    256 * 1024
}
const fn d_max_worker_output_bytes() -> u64 {
    8 * 1024 * 1024
}
const fn d_verify_timeout() -> u64 {
    900
}
const fn d_max_changed_files() -> usize {
    200
}
const fn d_mcp_timeout() -> u64 {
    120
}
const fn d_mcp_max_result_bytes() -> usize {
    1024 * 1024
}
fn d_mcp_capability() -> String {
    "mcp.call".to_owned()
}
const fn d_browser_startup_timeout() -> u64 {
    30
}
const fn d_browser_action_timeout() -> u64 {
    30
}
const fn d_max_page_bytes() -> usize {
    512 * 1024
}
fn d_allowed_origins() -> Vec<String> {
    vec!["file".to_owned(), "http://127.0.0.1".to_owned()]
}

/// Five seconds, which is the number the hung-provider bound is usually set to in accessibility tooling.
///
/// It is not generous: a provider that has not answered in five seconds is not going to answer for the
/// operator's benefit, and the alternative — waiting indefinitely on somebody else's message pump — is
/// the failure `docs/desktop-control-plan.md` asks to be bounded rather than tolerated.
const fn d_uia_timeout() -> u64 {
    5
}
const fn d_capture_timeout() -> u64 {
    10
}
const fn d_max_tree_nodes() -> usize {
    2_000
}
/// 4 MP, the same ceiling `[vision] max_capture_pixels` states in the settings document.
const fn d_max_capture_pixels() -> usize {
    4_194_304
}
fn d_capture_dir() -> String {
    "%LOCALAPPDATA%/JEAN/captures".to_owned()
}
/// `discard_after_task`, which means no file is ever written.
///
/// A cleaner now exists (`jean_core::captures`), so this default is a privacy choice rather than the only
/// policy the build can keep: the safest frame is still the one that was never written, and the plan's rule
/// that a mechanism module must not delete its own evidence is honoured by the ownership manifest, which
/// lets expiry remove only what this build claimed as its own artifact.
fn d_retention() -> String {
    "discard_after_task".to_owned()
}
/// Four desktop worker processes alive at once, after which desktop work is refused.
///
/// The number is a resource limit and not a throughput setting. What it counts changed when the provider
/// calls moved out of this process and into `jean-desktop-worker.exe` (D-051): a timed-out call now costs
/// a *process*, and the process is terminated — with its job object and every child it had — rather than
/// abandoned. So this is no longer a cap on leaked threads; it is a cap on workers that are alive at the
/// same moment, which is what `jean_windows::worker::live_workers` reports and what
/// `jean_core::desktop::gate_workers` refuses against.
///
/// Four is enough for the pathological case — one hung window per display — and refusing beyond it is the
/// honest answer to a desktop full of unresponsive applications, because a fifth worker would be a fifth
/// process started when the previous four have not reported their exits.
const fn d_max_stuck_workers() -> u32 {
    4
}

/// The connectors shipped with this build.
///
/// Listing them here rather than requiring an operator to write them is a deliberate choice: a
/// worker's argv is not a place for configuration creativity, because it is the one string a
/// delegated agent could otherwise influence. Presence is checked at run time by the same
/// executable resolution every tool uses, and a missing executable is a reported blocker.
fn default_workers() -> Vec<WorkerConfig> {
    vec![
        WorkerConfig {
            name: "opencode".to_owned(),
            kind: "acp".to_owned(),
            command: "opencode".to_owned(),
            // Native ACP server over stdio: `opencode acp --help` says so on this machine.
            args: vec!["acp".to_owned()],
            enabled: true,
            model: String::new(),
            env_passthrough: Vec::new(),
            capability: "agent.delegate".to_owned(),
            sandbox: "workspace-write".to_owned(),
        },
        WorkerConfig {
            name: "codex".to_owned(),
            // Codex 0.160.1 has no `acp` subcommand and the `@agentclientprotocol/codex-acp`
            // adapter is not installed, so this connector is the structured CLI fallback:
            // `codex exec --json` writes JSONL events on stdout, which is a protocol even if it
            // is not ACP. It is also the proof that the worker contract is not ACP-only.
            kind: "codex-cli".to_owned(),
            command: "codex".to_owned(),
            args: vec!["exec".to_owned()],
            enabled: true,
            model: String::new(),
            env_passthrough: Vec::new(),
            capability: "agent.delegate".to_owned(),
            sandbox: "workspace-write".to_owned(),
        },
    ]
}

/// The worker kinds this build implements. Anything else is a load error naming what is
/// available, because a kind that resolves to nothing is how a silent no-op ships.
pub const WORKER_KINDS: &[&str] = &["acp", "codex-cli"];

/// Sandboxes a `codex-cli` worker may be given. `danger-full-access` is absent on
/// purpose: the whole value of delegating under a job object and a policy engine is
/// lost if the worker can run anything.
pub const WORKER_SANDBOXES: &[&str] = &["read-only", "workspace-write"];

impl DelegationConfig {
    fn check(&self) -> std::result::Result<(), String> {
        if self.max_parallel > 4 {
            return Err(format!(
                "delegation.max_parallel is {}, but this build caps parallel workers at 4 (per-task cancellation via signal broadcast). Raise the cap with a deliberate scheduler change, not by editing this value.",
                self.max_parallel
            ));
        }
        if self.session_timeout_secs == 0 || self.idle_timeout_secs == 0 {
            return Err("session/idle timeouts must be greater than zero".to_owned());
        }
        if self.idle_timeout_secs >= self.session_timeout_secs {
            return Err(format!(
                "delegation.idle_timeout_secs ({}) must be shorter than session_timeout_secs ({}); \
                 otherwise the stall detector can never fire before the deadline does",
                self.idle_timeout_secs, self.session_timeout_secs
            ));
        }
        if self.permission_timeout_secs == 0 {
            return Err("permission_timeout_secs must be greater than zero".to_owned());
        }
        if self.max_event_queue == 0 || self.max_event_queue > 4096 {
            return Err(
                "max_event_queue must be between 1 and 4096; it is the back-pressure bound"
                    .to_owned(),
            );
        }
        if self.max_frame_bytes < 1024 || self.max_frame_bytes > 8 * 1024 * 1024 {
            return Err(
                "max_frame_bytes must be between 1 KiB and 8 MiB — the SDK frames lines without a \
                 limit, so this value is the only ceiling on one protocol message"
                    .to_owned(),
            );
        }
        if self.max_output_bytes < 1024 {
            return Err("max_output_bytes must be at least 1 KiB".to_owned());
        }
        let mut names: Vec<&str> = Vec::new();
        for worker in &self.workers {
            if worker.name.trim().is_empty() {
                return Err("every [[delegation.workers]] entry needs a non-empty name".to_owned());
            }
            if names.contains(&worker.name.as_str()) {
                return Err(format!("worker name {:?} is declared twice", worker.name));
            }
            names.push(worker.name.as_str());
            if !WORKER_KINDS.contains(&worker.kind.as_str()) {
                return Err(format!(
                    "worker {:?} kind {:?} is not implemented; this build speaks {}",
                    worker.name,
                    worker.kind,
                    WORKER_KINDS.join(" | ")
                ));
            }
            if worker.enabled && worker.command.trim().is_empty() {
                return Err(format!(
                    "worker {:?} is enabled but names no executable",
                    worker.name
                ));
            }
            if !WORKER_SANDBOXES.contains(&worker.sandbox.as_str()) {
                return Err(format!(
                    "worker {:?} sandbox {:?} is refused; a delegated worker runs inside a job \
                     object and a policy engine, and {} is what makes that true",
                    worker.name,
                    worker.sandbox,
                    WORKER_SANDBOXES.join(" | ")
                ));
            }
        }
        if self.enabled && !self.default_worker.trim().is_empty() {
            let known = self
                .workers
                .iter()
                .any(|w| w.name == self.default_worker && w.enabled);
            if !known {
                return Err(format!(
                    "delegation.default_worker {:?} is not an enabled [[delegation.workers]] name",
                    self.default_worker
                ));
            }
        }
        if !self.verify_command.trim().is_empty() && self.verify_timeout_secs == 0 {
            return Err("verify_timeout_secs must be greater than zero".to_owned());
        }
        if self.max_changed_files_reported == 0 {
            return Err("max_changed_files_reported must be at least 1".to_owned());
        }
        Ok(())
    }
}

impl McpConfig {
    fn check(&self) -> std::result::Result<(), String> {
        if self.request_timeout_secs == 0 || self.shutdown_grace_ms == 0 {
            return Err(
                "mcp.request_timeout_secs and shutdown_grace_ms must be greater than zero"
                    .to_owned(),
            );
        }
        if self.max_message_bytes < 1024 {
            return Err("mcp.max_message_bytes must be at least 1 KiB".to_owned());
        }
        if self.max_result_bytes < 1024 {
            return Err("mcp.max_result_bytes must be at least 1 KiB".to_owned());
        }
        let mut names: Vec<&str> = Vec::new();
        for server in &self.servers {
            if server.name.trim().is_empty() {
                return Err("every [[mcp.servers]] entry needs a non-empty name".to_owned());
            }
            if names.contains(&server.name.as_str()) {
                return Err(format!(
                    "mcp server name {:?} is declared twice",
                    server.name
                ));
            }
            names.push(server.name.as_str());
            if server.enabled && server.command.trim().is_empty() {
                return Err(format!(
                    "mcp server {:?} is enabled but names no executable",
                    server.name
                ));
            }
            if server.capability.trim().is_empty() {
                return Err(format!(
                    "mcp server {:?} needs a capability token; a call with no capability is a call \
                     no policy rule can see",
                    server.name
                ));
            }
        }
        for id in &self.expose {
            if id.trim().is_empty() {
                return Err("mcp.expose contains an empty tool id".to_owned());
            }
        }
        Ok(())
    }
}

impl DesktopConfig {
    fn check(&self) -> std::result::Result<(), String> {
        if self.uia_timeout_secs == 0 || self.capture_timeout_secs == 0 {
            return Err("desktop timeouts must be greater than zero".to_owned());
        }
        if self.max_tree_nodes == 0 {
            return Err("desktop.max_tree_nodes must be at least 1".to_owned());
        }
        // A ceiling below one megapixel cannot hold a full-screen window on any display this build is
        // expected to see, so a value that small is a mistake rather than a restriction, and saying so is
        // kinder than silently refusing every large window at run time.
        if self.max_capture_pixels < 1_000_000 {
            return Err("desktop.max_capture_pixels must be at least 1_000_000".to_owned());
        }
        if self.max_concurrent_workers == 0 {
            return Err("desktop.max_concurrent_workers must be at least 1".to_owned());
        }
        // The retention vocabulary is the settings document's, and nothing else. An unrecognised word is
        // refused rather than defaulted, because the failure mode of defaulting a retention setting is
        // keeping screenshots nobody agreed to keep.
        if !matches!(
            self.retention.as_str(),
            "discard_after_task" | "keep_24h" | "keep_until_manual"
        ) {
            return Err(format!(
                "desktop.retention must be discard_after_task, keep_24h or keep_until_manual, got {:?}",
                self.retention
            ));
        }
        if self.enabled
            && self.retention != "discard_after_task"
            && self.capture_dir.trim().is_empty()
        {
            return Err(
                "desktop.capture_dir must name somewhere when captures are kept; a kept capture with no                  path would be dropped on the floor"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

impl BrowserConfig {
    fn check(&self) -> std::result::Result<(), String> {
        if self.startup_timeout_secs == 0 || self.action_timeout_secs == 0 {
            return Err("browser timeouts must be greater than zero".to_owned());
        }
        if self.max_page_bytes < 1024 {
            return Err("browser.max_page_bytes must be at least 1 KiB".to_owned());
        }
        if self.allowed_origins.is_empty() {
            return Err(
                "browser.allowed_origins is empty, which would leave no page this build may open; \
                 name `file` or a loopback origin"
                    .to_owned(),
            );
        }
        for origin in &self.allowed_origins {
            let text = origin.trim();
            if text.is_empty() {
                return Err("browser.allowed_origins contains an empty entry".to_owned());
            }
            if text == "*" || text.ends_with("://*") {
                return Err(format!(
                    "browser.allowed_origins entry {origin:?} allows anywhere; a browser JEAN drives \
                     is bounded to the origins named here"
                ));
            }
        }
        Ok(())
    }
}

impl_default_from_fns!(WorkerConfig {
    name => String::new(),
    kind => d_worker_kind(),
    command => String::new(),
    args => Vec::new(),
    enabled => true,
    model => String::new(),
    env_passthrough => Vec::new(),
    capability => d_worker_capability(),
    sandbox => d_worker_sandbox(),
});

impl_default_from_fns!(DelegationConfig {
    enabled => true,
    default_worker => d_default_worker(),
    max_parallel => d_max_parallel(),
    session_timeout_secs => d_session_timeout(),
    idle_timeout_secs => d_idle_timeout(),
    permission_timeout_secs => d_permission_timeout(),
    shutdown_grace_ms => d_shutdown_grace_ms(),
    max_event_queue => d_max_event_queue(),
    max_frame_bytes => d_max_frame_bytes(),
    max_output_bytes => d_max_worker_output_bytes(),
    workers => default_workers(),
    verify_command => String::new(),
    verify_args => Vec::new(),
    verify_timeout_secs => d_verify_timeout(),
    isolate_in_worktree => true,
    worktree_root => String::new(),
    require_clean_baseline => true,
    max_changed_files_reported => d_max_changed_files(),
});

impl_default_from_fns!(McpServerConfig {
    name => String::new(),
    command => String::new(),
    args => Vec::new(),
    enabled => true,
    allow_tools => Vec::new(),
    capability => d_mcp_capability(),
    env_passthrough => Vec::new(),
});

impl_default_from_fns!(McpConfig {
    enabled => true,
    request_timeout_secs => d_mcp_timeout(),
    max_message_bytes => d_max_frame_bytes(),
    max_result_bytes => d_mcp_max_result_bytes(),
    shutdown_grace_ms => d_shutdown_grace_ms(),
    servers => Vec::new(),
    expose => Vec::new(),
});

/// Bounds and switches for the desktop actuator.
///
/// `enabled` gates the *actuator*, not the diagnostics: `jean desktop windows` and `jean desktop
/// diagnose` are reads and remain available, because the plan requires that a blocked window be
/// *diagnosed*, and a switch that turns diagnosis off with the actions would remove the only evidence an
/// operator has to decide whether to turn the actions on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopConfig {
    /// `false` refuses every action; reads and diagnostics still run.
    #[serde(default)]
    pub enabled: bool,
    /// How long one UI Automation conversation may be waited for before its worker process is terminated.
    ///
    /// The bound is the whole protection a hung application gets, so it is a setting and not a constant:
    /// an operator driving a slow CAD package needs a different number from one driving Notepad, and
    /// either number is honest only if it was chosen. The worker's job object is closed at the deadline, so
    /// the process and anything it spawned are ended rather than left running; the call's *effect*, if it
    /// had one, stays uncertain and is reported as uncertain (§51 defect 5, D-050).
    #[serde(default = "d_uia_timeout")]
    pub uia_timeout_secs: u64,
    /// The same bound for one capture.
    #[serde(default = "d_capture_timeout")]
    pub capture_timeout_secs: u64,
    /// Ceiling on the controls one tree read may return.
    #[serde(default = "d_max_tree_nodes")]
    pub max_tree_nodes: usize,
    /// Ceiling on one capture, in pixels. A larger window is refused rather than resized, because there
    /// is no scaler in this tree and a shrunken frame makes every coordinate claim built from it false.
    #[serde(default = "d_max_capture_pixels")]
    pub max_capture_pixels: usize,
    /// Where a *kept* capture is written. Only consulted when `retention` is not `discard_after_task`.
    #[serde(default = "d_capture_dir")]
    pub capture_dir: String,
    /// `discard_after_task` | `keep_24h` | `keep_until_manual`.
    ///
    /// All three are now true statements about the files this build writes. `discard_after_task` is
    /// implemented as *nothing is ever written*, which is why it is the default: a frame that does not
    /// exist needs no cleaner and cannot be leaked. `keep_24h` is an actual expiry — each kept frame is
    /// claimed in an ownership manifest **before** its bytes are written, and the claim is what authorises
    /// the deletion, so `jean_core::captures` removes exactly its own artifacts at 24 h and holds the kept
    /// set under a byte ceiling, at startup and on every desktop action. A frame that could not be claimed
    /// is not written at all, because an unclaimable frame is one the expiry is forbidden to remove and
    /// would therefore outlive the setting that promised to clear it. `keep_until_manual` expires nothing,
    /// and is excluded from the byte ceiling for that reason: the promise it made belongs to the operator.
    ///
    /// What none of the three does is redact. Text readback masks password-flagged controls in the digest;
    /// a retained frame is the pixels themselves, and pixels show whatever was on the screen — including
    /// what the digest was told not to say. `capture_dir` is the operator's own directory, and the note
    /// that says so is in the report rather than only here.
    #[serde(default = "d_retention")]
    pub retention: String,
    /// Whether `SendInput` may be used at all.
    ///
    /// Off by default and independent of `enabled`, because the semantic path is the one that is meant to
    /// be used: an operator who has switched the actuator on has agreed to pattern actions, not to
    /// synthesised keystrokes, and the difference is exactly what R-16 and the foreground checks are about.
    #[serde(default)]
    pub allow_synthetic_input: bool,
    /// How many desktop worker processes may be alive at once before desktop work is refused.
    ///
    /// Every call ends its own worker, so reaching this means workers are starting faster than they are
    /// exiting or one will not report its exit — a fact about the desktop, and a reason to stop rather than
    /// to add another process.
    #[serde(default = "d_max_stuck_workers")]
    pub max_concurrent_workers: u32,
}

impl_default_from_fns!(DesktopConfig {
    enabled => false,
    uia_timeout_secs => d_uia_timeout(),
    capture_timeout_secs => d_capture_timeout(),
    max_tree_nodes => d_max_tree_nodes(),
    max_capture_pixels => d_max_capture_pixels(),
    capture_dir => d_capture_dir(),
    retention => d_retention(),
    allow_synthetic_input => false,
    max_concurrent_workers => d_max_stuck_workers(),
});

impl_default_from_fns!(BrowserConfig {
    enabled => false,
    browser_path => String::new(),
    headless => true,
    startup_timeout_secs => d_browser_startup_timeout(),
    action_timeout_secs => d_browser_action_timeout(),
    max_page_bytes => d_max_page_bytes(),
    allowed_origins => d_allowed_origins(),
});

impl AppConfig {
    /// Validate cross-field invariants that no single field can express.
    ///
    /// These are the checks that stop a plausible-looking config from producing
    /// a supervisor that cannot supervise.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.voice.validate()?;
        let fail = |field: &'static str, reason: String| -> Result<(), ConfigError> {
            Err(ConfigError::Validation { field, reason })
        };

        if self.config_version != SUPPORTED_CONFIG_VERSION {
            return fail(
                "config_version",
                format!(
                    "this build supports exactly {SUPPORTED_CONFIG_VERSION}, found {}",
                    self.config_version
                ),
            );
        }

        match self.core.log_level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" | "off" => {}
            other => {
                return fail(
                    "core.log_level",
                    format!("{other:?} is not one of trace|debug|info|warn|error|off"),
                )
            }
        }

        match self.core.log_rotation.as_str() {
            "minutely" | "hourly" | "daily" | "never" => {}
            other => {
                return fail(
                    "core.log_rotation",
                    format!("{other:?} is not one of minutely|hourly|daily|never"),
                )
            }
        }

        match self.store.journal_mode.to_ascii_lowercase().as_str() {
            "wal" | "delete" | "truncate" | "persist" | "memory" => {}
            other => {
                return fail(
                    "store.journal_mode",
                    format!("{other:?} is not a SQLite journal mode"),
                )
            }
        }

        match self.store.synchronous.to_ascii_uppercase().as_str() {
            "OFF" | "NORMAL" | "FULL" | "EXTRA" => {}
            other => {
                return fail(
                    "store.synchronous",
                    format!("{other:?} is not one of OFF|NORMAL|FULL|EXTRA"),
                )
            }
        }

        if self.ipc.transport != "named_pipe" {
            return fail(
                "ipc.transport",
                format!(
                    "{:?} is not supported; Phase 0 speaks only named_pipe. A network listener is \
                     deliberately not implemented (see PLANNING/04-SETTINGS.md [ipc]).",
                    self.ipc.transport
                ),
            );
        }
        if self.ipc.http_enabled {
            return fail(
                "ipc.http_enabled",
                "HTTP IPC is not implemented in Phase 0 and must stay disabled".to_owned(),
            );
        }

        if self.process.capture_output_max_bytes == 0 {
            return fail(
                "process.capture_output_max_bytes",
                "must be greater than zero; a zero cap would discard all output".to_owned(),
            );
        }
        if self.process.job_active_proc_limit == 0 {
            return fail(
                "process.job_active_proc_limit",
                "must be greater than zero".to_owned(),
            );
        }
        if self.process.env_allowlist.iter().any(|v| v.is_empty()) {
            return fail(
                "process.env_allowlist",
                "contains an empty variable name".to_owned(),
            );
        }
        if self.process.inherit_environment {
            return fail(
                "process.inherit_environment",
                "blanket environment inheritance is not permitted; add the specific variables to \
                 process.env_allowlist instead"
                    .to_owned(),
            );
        }

        // These three are rejected rather than ignored because the supervisor has
        // no other behaviour to fall back on. Accepting `false` would mean the
        // setting reads as if it controlled something while the code did the
        // opposite, which is the worst outcome for a configuration file.
        if !self.process.use_job_objects {
            return fail(
                "process.use_job_objects",
                "every child must be contained in a Job Object; without one a cancelled task \
                 leaves its descendants running, and Phase 0 has no other containment mechanism"
                    .to_owned(),
            );
        }
        if !self.process.kill_on_job_close {
            return fail(
                "process.kill_on_job_close",
                "the job must be created with KILL_ON_JOB_CLOSE; without it, closing the job \
                 handle does not take the process tree with it"
                    .to_owned(),
            );
        }
        if !self.store.migrations_on_start {
            return fail(
                "store.migrations_on_start",
                "the schema must be migrated on open, or the store may be missing tables this \
                 build expects to write to"
                    .to_owned(),
            );
        }

        if self.orchestrator.max_concurrent_tasks == 0 {
            return fail(
                "orchestrator.max_concurrent_tasks",
                "must be greater than zero".to_owned(),
            );
        }
        if self.orchestrator.default_timeout_secs == 0 {
            return fail(
                "orchestrator.default_timeout_secs",
                "must be greater than zero; a zero timeout would kill every task immediately"
                    .to_owned(),
            );
        }

        if self.policy.on_timeout != "deny" {
            return fail(
                "policy.on_timeout",
                format!(
                    "{:?} is not permitted: an approval timeout must resolve to deny (D-011)",
                    self.policy.on_timeout
                ),
            );
        }
        if self.policy.run_elevated_by_default {
            return fail(
                "policy.run_elevated_by_default",
                "the orchestrator must not run elevated by default (D-010)".to_owned(),
            );
        }
        match self.policy.default_tier.as_str() {
            "read_only" | "normal" | "elevated" => {}
            other => {
                return fail(
                    "policy.default_tier",
                    format!("{other:?} is not one of read_only|normal|elevated"),
                )
            }
        }

        // Phase 1 — what an operator may write down about permissions.
        //
        // The settings document first expressed a remembered approval as being
        // scoped to `tool_and_tier`. That is refused here rather than accepted and
        // quietly reinterpreted, because a config key that does not mean what it
        // says is the failure mode this whole section exists to avoid: approving
        // `git push` once would then authorise every later `git` command at that
        // tier, including ones with different arguments. Approval is bound to the
        // operation and its validated arguments.
        if self.policy.approval_scope != APPROVAL_SCOPE_OPERATION_AND_ARGUMENTS {
            return fail(
                "policy.approval_scope",
                format!(
                    "{:?} is not supported. The only accepted value is {:?}: a remembered \
                     approval covers that operation with those validated arguments and target, \
                     and nothing else. An approval for one PowerShell command must not \
                     authorise the next one.",
                    self.policy.approval_scope, APPROVAL_SCOPE_OPERATION_AND_ARGUMENTS
                ),
            );
        }
        if !self.policy.never_store_credentials {
            return fail(
                "policy.never_store_credentials",
                "this build stores no credentials and has no elevation sidecar; there is no \
                 implementation for this setting to select (D-010)"
                    .to_owned(),
            );
        }
        if self.policy.approval_timeout_secs == 0 || self.policy.approval_timeout_secs > 3600 {
            return fail(
                "policy.approval_timeout_secs",
                "must be between 1 and 3600 seconds; the prompt is answered by a person at a \
                 keyboard, so a longer wait is not consent and a zero wait is an instant deny"
                    .to_owned(),
            );
        }
        if self.policy.elevation_helper.trim().is_empty() {
            return fail(
                "policy.elevation_helper",
                "must not be empty; it is named in the refusal shown for an elevated operation, \
                 so an empty value would tell the operator to run nothing"
                    .to_owned(),
            );
        }
        for list in [&self.policy.require_approval, &self.policy.auto_deny] {
            if list.iter().any(|c| c.trim().is_empty()) {
                return fail(
                    "policy.require_approval / policy.auto_deny",
                    "contains an empty capability name; an empty entry would match nothing and \
                     read as if it matched something"
                        .to_owned(),
                );
            }
        }

        if self.terminal.interactive_backend != "conpty"
            && self.terminal.interactive_backend != "plain"
        {
            return fail(
                "terminal.interactive_backend",
                format!(
                    "{:?} is not one of conpty|plain. `plain` runs an interactive command over \
                     pipes with stdin closed; `conpty` allocates a pseudo console.",
                    self.terminal.interactive_backend
                ),
            );
        }
        if self.terminal.pty_rows == 0 || self.terminal.pty_cols == 0 {
            return fail(
                "terminal.pty_rows / terminal.pty_cols",
                "a pseudo console of zero rows or columns cannot be created".to_owned(),
            );
        }

        if self.tools.mark_unhealthy_after == 0 {
            return fail(
                "tools.mark_unhealthy_after",
                "must be at least 1; a tool cannot be reported unhealthy before a check has \
                 failed"
                    .to_owned(),
            );
        }
        if self.tools.health_check_interval_secs == 0 || self.tools.health_check_timeout_ms == 0 {
            return fail(
                "tools.health_check_interval_secs / tools.health_check_timeout_ms",
                "both must be greater than zero; a zero timeout would fail every check".to_owned(),
            );
        }
        if self.tools.discovery.is_empty() {
            return fail(
                "tools.discovery",
                "at least one source is required, otherwise `jean launch` can resolve nothing"
                    .to_owned(),
            );
        }
        for source in &self.tools.discovery {
            match source.as_str() {
                "path" | "start_menu" | "registry" => {}
                "mcp" | "acp" => {
                    return fail(
                        "tools.discovery",
                        format!(
                            "{source:?} is a Phase 3 source; MCP servers and ACP agents are not \
                             connected by this build. Remove it rather than waiting for a \
                             discovery result that will never arrive."
                        ),
                    )
                }
                other => {
                    return fail(
                        "tools.discovery",
                        format!(
                            "{other:?} is not a discovery source; this build reads path, \
                             start_menu and registry"
                        ),
                    )
                }
            }
        }

        if self.search.max_results == 0
            || self.search.max_entries == 0
            || self.search.deadline_secs == 0
            || self.search.max_depth == 0
        {
            return fail(
                "search",
                "max_results, max_entries, deadline_secs and max_depth are bounds, and a bound of \
                 zero would make every search report nothing while looking like it had succeeded"
                    .to_owned(),
            );
        }
        if self.search.follow_reparse_points {
            return fail(
                "search.follow_reparse_points",
                "following a junction or symlink can move the walk outside the root that was \
                 authorised and can cycle; the search is bounded by its root and this setting \
                 would break that guarantee"
                    .to_owned(),
            );
        }

        match self.ui.notifications.as_str() {
            "none" | "toast" => {}
            other => {
                return fail(
                    "ui.notifications",
                    format!(
                        "{other:?} is not one of none|toast. There is no tray, window or \
                     taskbar button in Phase 1."
                    ),
                )
            }
        }
        if self.ui.app_user_model_id.trim().is_empty() {
            return fail(
                "ui.app_user_model_id",
                "must not be empty; a toast without an AppUserModelId is attributed to the wrong \
                 application or not delivered at all"
                    .to_owned(),
            );
        }

        if let Err(problem) = self.model.check() {
            return fail("model", problem);
        }
        if let Err(problem) = self.delegation.check() {
            return fail("delegation", problem);
        }
        if let Err(problem) = self.mcp.check() {
            return fail("mcp", problem);
        }
        if let Err(problem) = self.browser.check() {
            return fail("browser", problem);
        }
        if let Err(problem) = self.desktop.check() {
            return fail("desktop", problem);
        }

        Ok(())
    }
}

impl ModelConfig {
    /// Validate the model section. Kept as its own method so a test can call it directly and so
    /// the rules are readable in one place.
    fn check(&self) -> std::result::Result<(), String> {
        match self.provider.as_str() {
            "none" => {}
            "ollama" | "openai_compatible" => {
                if self.model.trim().is_empty() {
                    return Err(format!(
                        "provider {:?} needs model: no model name means no request can be built",
                        self.provider
                    ));
                }
            }
            other => {
                return Err(format!(
                    "{other:?} is not a provider this build implements. Supported are none, \
                     ollama and openai_compatible; the settings document also lists anthropic and \
                     openai, which are cloud APIs this batch was told not to use"
                ));
            }
        }

        let endpoint = split_endpoint(&self.base_url)?;
        // Refused only for a provider that would actually be used: `provider = "none"` keeps a
        // placeholder base_url from breaking every offline configuration.
        //
        // HTTPS is a supported scheme and its certificate is verified by the transport. What is
        // refused is the *plain* form of a remote endpoint: the prompt leaves the machine in the
        // clear, and so does the bearer token if `api_key_env` is set. Loopback stays allowed on
        // http because that is the documented local model server, which has no network to cross.
        if self.provider != "none" && endpoint.scheme == "http" && !endpoint.loopback {
            return Err(format!(
                "model.base_url {:?} is a non-loopback endpoint over plain http. Since this build \
                 verifies TLS, use https for anything that is not on this machine: a remote http \
                 endpoint would carry the prompt, and any configured bearer token, unencrypted",
                self.base_url
            ));
        }

        if self.request_timeout_secs == 0 || self.request_timeout_secs > 900 {
            return Err(
                "model.request_timeout_secs must be 1..=900; a zero deadline fails every request \
                 and a longer one makes a hung provider indistinguishable from a slow one"
                    .to_owned(),
            );
        }
        if !(4096..=8 * 1024 * 1024).contains(&self.max_response_bytes) {
            return Err(
                "model.max_response_bytes must be 4096..=8388608; an unbounded response is a \
                 memory hazard and a tiny one cannot hold a plan"
                    .to_owned(),
            );
        }
        if !(4096..=1024 * 1024).contains(&self.max_prompt_bytes) {
            return Err(
                "model.max_prompt_bytes must be 4096..=1048576; this is the ceiling on what JEAN \
                 sends, and it exists so a large registry cannot produce an unbounded request"
                    .to_owned(),
            );
        }
        if self.max_output_tokens == 0 || self.max_output_tokens > 32_768 {
            return Err("model.max_output_tokens must be 1..=32768".to_owned());
        }
        if !(0.0..=0.3).contains(&self.temperature) {
            return Err(format!(
                "model.temperature is {}; this build allows 0.0..=0.3. A plan is a decision, and \
                 a decision that cannot be reproduced from the record cannot be audited",
                self.temperature
            ));
        }
        if self.retry_backoff_ms.len() > 4 || self.retry_backoff_ms.iter().any(|ms| *ms > 60_000) {
            return Err(
                "model.retry_backoff_ms may hold at most 4 entries of at most 60000 ms each; \
                 retries are for transient transport faults, not for waiting a service out"
                    .to_owned(),
            );
        }
        if self.max_plan_steps == 0 || self.max_plan_steps > crate::plan::MAX_PLAN_STEPS {
            return Err(format!(
                "model.max_plan_steps must be 1..={} (the absolute ceiling). Configuration may \
                 tighten the plan limit, never widen it",
                crate::plan::MAX_PLAN_STEPS
            ));
        }
        if !matches!(self.summary_wording.as_str(), "record" | "model") {
            return Err(format!(
                "model.summary_wording is {:?}; the only choices are record and model, and either \
                 way the verdicts come from the execution record",
                self.summary_wording
            ));
        }
        if self.api_key_env.trim().is_empty() {
            if !endpoint.loopback {
                return Err(
                    "a non-loopback endpoint needs model.api_key_env: the name of an environment \
                     variable holding the bearer token. Refusing to talk to a remote provider \
                     with no credential is deliberate"
                        .to_owned(),
                );
            }
        } else if !is_env_var_name(self.api_key_env.trim()) {
            return Err(format!(
                "model.api_key_env is {:?}; this key takes the NAME of an environment variable, so \
                 it must be a bare identifier and must never hold the secret itself",
                self.api_key_env
            ));
        }

        let missing: Vec<&str> = NEVER_DELEGATE_TO_MODEL
            .iter()
            .filter(|item| {
                !self
                    .never_delegate_to_model
                    .iter()
                    .any(|have| have == *item)
            })
            .copied()
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "model.never_delegate_to_model is missing {}; this list is a floor, not a \
                 suggestion, and removing an entry would move a permission, verification or \
                 supervision decision into the model's hands (D-011)",
                missing.join(", ")
            ));
        }

        Ok(())
    }

    /// True when planning is available at all.
    pub fn planning_enabled(&self) -> bool {
        self.provider != "none"
    }
}

/// The pieces of `model.base_url` that the checks need. `jean-model` re-parses the URL when it
/// builds a request, because it also needs the host and port; what matters here is only that the
/// *shape* is one JEAN can act on.
struct Endpoint {
    scheme: String,
    loopback: bool,
}

/// Parse just enough of a URL to be strict about it, without a dependency.
///
/// Deliberately rejects anything with userinfo, query or fragment: a URL that carries a key is
/// a URL that will end up in a log line.
fn split_endpoint(raw: &str) -> std::result::Result<Endpoint, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err("model.base_url is empty".to_owned());
    }
    let Some((scheme, rest)) = text.split_once("://") else {
        return Err(format!(
            "model.base_url {text:?} has no scheme; write something like \
             \"http://127.0.0.1:11434\""
        ));
    };
    if !matches!(scheme, "http" | "https") {
        return Err(format!(
            "model.base_url scheme {scheme:?} is not supported; only http and https are, and a \
             non-loopback endpoint must be https"
        ));
    }
    if rest.contains('@') {
        return Err(
            "model.base_url must not embed credentials; a token belongs in the environment \
             variable named by model.api_key_env, never in a URL that gets logged"
                .to_owned(),
        );
    }
    if rest.contains('?') || rest.contains('#') {
        return Err(
            "model.base_url must not carry a query or fragment; those are part of a request, not \
             of an endpoint"
                .to_owned(),
        );
    }
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return Err(format!("model.base_url {text:?} has no host"));
    }
    let host = match authority.split_once(':') {
        Some((host, port)) => {
            if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
                return Err(format!(
                    "model.base_url has an invalid port in {authority:?}"
                ));
            }
            host
        }
        None => authority,
    };
    if host.is_empty() {
        return Err(format!("model.base_url {text:?} has an empty host"));
    }
    let loopback = host == "localhost" || host == "::1" || host.starts_with("127.");
    Ok(Endpoint {
        scheme: scheme.to_owned(),
        loopback,
    })
}

/// Whether `name` is a usable environment-variable name.
fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Deep-merge `overlay` onto `base`. Tables merge recursively; every other
/// value type (including arrays) replaces wholesale. Replacing arrays rather
/// than concatenating them is what makes `env_allowlist = ["PATH"]` in a user
/// file mean "only PATH", which is the safe reading.
fn merge_values(base: &mut toml::Value, overlay: &toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(existing) => merge_values(existing, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// Collect every dotted key path in a TOML tree.
fn collect_paths(value: &toml::Value, prefix: &str, out: &mut BTreeSet<String>) {
    if let Some(table) = value.as_table() {
        for (k, v) in table {
            let path = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{prefix}.{k}")
            };
            out.insert(path.clone());
            collect_paths(v, &path, out);
        }
    }
}

/// Key paths the schema actually defines, derived from the default config.
///
/// Deriving rather than listing means a new struct field is automatically
/// recognised and a removed one automatically stops being recognised.
fn known_key_paths() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    out.insert("config_version".to_owned());
    if let Ok(value) = toml::Value::try_from(AppConfig::default()) {
        collect_paths(&value, "", &mut out);
    }
    out
}

/// Inspect a user file, then **prune** everything the schema does not define.
///
/// Pruning is not optional: the schema structs carry `deny_unknown_fields`, so
/// an unpruned typo would turn a warning into a hard load failure. The warning
/// is emitted here, the key is removed, and loading proceeds. Deferred sections
/// (`[vision]`, `[telemetry]`, …) are pruned the same way — recognised, reported,
/// not applied.
fn inspect_and_prune(path: &Path, value: &mut toml::Value) -> Result<Vec<String>, ConfigError> {
    let mut warnings = Vec::new();

    if let Some(v) = value.get("config_version") {
        let found = match v {
            toml::Value::Integer(i) if *i >= 0 => *i as u64,
            other => {
                return Err(ConfigError::Parse {
                    path: path.to_path_buf(),
                    message: format!(
                        "config_version must be a positive integer, found `{other}`; \
                         refusing to guess which schema this file targets"
                    ),
                })
            }
        };
        if found != u64::from(SUPPORTED_CONFIG_VERSION) {
            return Err(ConfigError::UnsupportedVersion {
                path: path.to_path_buf(),
                found,
                supported: SUPPORTED_CONFIG_VERSION,
            });
        }
    }

    let known = known_key_paths();
    let mut present = BTreeSet::new();
    collect_paths(value, "", &mut present);

    let mut unknown: Vec<String> = Vec::new();
    for key in present.difference(&known) {
        let section = key.split('.').next().unwrap_or(key);
        if DEFERRED_SECTIONS.contains(&section) {
            continue;
        }
        unknown.push(key.clone());
        warnings.push(format!(
            "{path}: unknown configuration key `{key}` — ignored. \
             Check PLANNING/04-SETTINGS.md for the supported schema.",
            path = path.display()
        ));
    }
    tracing::debug!(count = unknown.len(), path = %path.display(), "pruned unknown config keys");

    for section in DEFERRED_SECTIONS {
        if value.get(section).is_some() {
            warnings.push(format!(
                "{path}: section `[{section}]` is recognised but not applied by this build — \
                 it belongs to a later phase (see PLANNING/05-ROADMAP.md).",
                path = path.display()
            ));
        }
    }

    prune_unknown(value, "", &known);

    Ok(warnings)
}

/// Recursively drop any table key whose dotted path is not in `known`.
fn prune_unknown(value: &mut toml::Value, prefix: &str, known: &BTreeSet<String>) {
    let Some(table) = value.as_table_mut() else {
        return;
    };

    let keys: Vec<String> = table.keys().cloned().collect();
    for key in keys {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };

        if !known.contains(&path) {
            table.remove(&key);
            continue;
        }
        if let Some(child) = table.get_mut(&key) {
            prune_unknown(child, &path, known);
        }
    }
}

/// Read a TOML file into a `toml::Value`, mapping I/O and parse failures into
/// `ConfigError` that names the file.
fn read_toml_file(path: &Path) -> Result<toml::Value, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    toml::from_str(&text).map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

/// Load configuration from the compiled defaults plus every existing file in
/// `layers`, in order, then apply environment overrides.
///
/// Missing files are skipped, not an error: a first run has no user config.
pub fn load_layers(layers: &[PathBuf]) -> Result<LoadedConfig, ConfigError> {
    let mut merged =
        toml::Value::try_from(AppConfig::default()).map_err(|e| ConfigError::Parse {
            path: PathBuf::from("<compiled defaults>"),
            message: format!("internal error serialising compiled defaults: {e}"),
        })?;

    let mut sources = vec![ConfigSource::CompiledDefaults];
    let mut warnings = Vec::new();

    for path in layers {
        if !path.exists() {
            continue;
        }
        let mut value = read_toml_file(path)?;
        warnings.extend(inspect_and_prune(path, &mut value)?);
        merge_values(&mut merged, &value);
        sources.push(ConfigSource::File(path.clone()));
    }

    let mut config: AppConfig = AppConfig::deserialize(merged).map_err(|e| ConfigError::Parse {
        path: layers
            .last()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("<merged>")),
        message: e.to_string(),
    })?;

    apply_env_overrides(&mut config, &mut sources, &mut warnings)?;
    config.validate()?;

    Ok(LoadedConfig {
        config,
        sources,
        warnings,
    })
}

fn apply_env_overrides(
    config: &mut AppConfig,
    sources: &mut Vec<ConfigSource>,
    warnings: &mut Vec<String>,
) -> Result<(), ConfigError> {
    if std::env::var_os("JEAN_NO_VOICE").is_some() {
        config.voice.enabled = false;
        sources.push(ConfigSource::Env("JEAN_NO_VOICE"));
    }
    // Only a small, explicit set of overrides is honoured. An environment
    // variable that can rewrite arbitrary policy would defeat D-011.
    if let Ok(v) = std::env::var("JEAN_LOG_LEVEL") {
        let trimmed = v.trim();
        if matches!(
            trimmed,
            "trace" | "debug" | "info" | "warn" | "error" | "off"
        ) {
            config.core.log_level = trimmed.to_owned();
            sources.push(ConfigSource::Env("JEAN_LOG_LEVEL"));
        } else {
            warnings.push(format!(
                "JEAN_LOG_LEVEL={trimmed:?} is not a valid level; keeping {:?}",
                config.core.log_level
            ));
        }
    }

    if let Ok(v) = std::env::var("JEAN_DATA_DIR") {
        if v.trim().is_empty() {
            return Err(ConfigError::Env {
                name: "JEAN_DATA_DIR",
                reason: "must not be empty".to_owned(),
            });
        }
        config.core.data_dir = v;
        sources.push(ConfigSource::Env("JEAN_DATA_DIR"));
    }

    if let Ok(v) = std::env::var("JEAN_MODEL_PROVIDER") {
        let trimmed = v.trim().to_owned();
        if matches!(trimmed.as_str(), "none" | "ollama" | "openai_compatible") {
            config.model.provider = trimmed;
            sources.push(ConfigSource::Env("JEAN_MODEL_PROVIDER"));
        } else {
            return Err(ConfigError::Env {
                name: "JEAN_MODEL_PROVIDER",
                reason: format!(
                    "{trimmed:?} is not a provider this build implements; supported are none, \
                     ollama and openai_compatible"
                ),
            });
        }
    }

    if let Ok(v) = std::env::var("JEAN_MODEL") {
        if !v.trim().is_empty() {
            config.model.model = v.trim().to_owned();
            sources.push(ConfigSource::Env("JEAN_MODEL"));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_defaults_validate() {
        let cfg = AppConfig::default();
        cfg.validate().expect("defaults must be valid");
        assert_eq!(cfg.config_version, SUPPORTED_CONFIG_VERSION);
    }

    #[test]
    fn defaults_match_the_settings_document() {
        // Spot-checks against PLANNING/04-SETTINGS.md. If these drift, the
        // document and the code disagree and one of them is wrong.
        let cfg = AppConfig::default();
        assert_eq!(cfg.core.log_level, "info");
        assert_eq!(cfg.core.log_retention_days, 14);
        assert_eq!(cfg.store.journal_mode, "wal");
        assert_eq!(cfg.store.synchronous, "NORMAL");
        assert_eq!(cfg.store.busy_timeout_ms, 5000);
        assert_eq!(cfg.ipc.transport, "named_pipe");
        assert!(!cfg.ipc.http_enabled, "no network listener, ever");
        assert_eq!(cfg.orchestrator.default_timeout_secs, 300);
        assert_eq!(cfg.orchestrator.max_concurrent_tasks, 4);
        assert!(cfg.process.use_job_objects);
        assert!(cfg.process.kill_on_job_close);
        assert!(!cfg.process.inherit_environment, "no blanket inheritance");
        assert_eq!(cfg.process.capture_output_max_bytes, 10 * 1024 * 1024);
        assert_eq!(cfg.policy.on_timeout, "deny");
        assert!(!cfg.policy.run_elevated_by_default);
        assert_eq!(cfg.workspace.delete_mode, "recycle_bin");
        assert_eq!(cfg.terminal.default_shell, "powershell");
    }

    #[test]
    fn allowlist_includes_what_windows_actually_needs() {
        let allow = AppConfig::default().process.env_allowlist;
        for required in [
            "PATH",
            "PATHEXT",
            "SystemRoot",
            "COMSPEC",
            "TEMP",
            "TMP",
            "USERPROFILE",
        ] {
            assert!(
                allow.iter().any(|v| v.eq_ignore_ascii_case(required)),
                "`{required}` must be allowlisted or PowerShell/cmd cannot start"
            );
        }
    }

    #[test]
    fn unsupported_version_is_a_hard_error() {
        let dir = tempdir();
        let path = dir.join("jean.toml");
        std::fs::write(&path, "config_version = 99\n").unwrap();

        let err = load_layers_isolated(std::slice::from_ref(&path)).expect_err("must reject");
        // The message is rendered first, because the match below moves `path`
        // out of `err` to assert on it.
        let msg = err.to_string();
        assert!(msg.contains("config_version"), "{msg}");
        assert!(msg.contains("99"), "{msg}");

        match err {
            ConfigError::UnsupportedVersion { found, path: p, .. } => {
                assert_eq!(found, 99);
                assert_eq!(p, path);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
        drop_dir(&dir);
    }

    #[test]
    fn version_zero_is_also_rejected() {
        let dir = tempdir();
        let path = dir.join("jean.toml");
        std::fs::write(&path, "config_version = 0\n").unwrap();
        assert!(matches!(
            load_layers_isolated(&[path]),
            Err(ConfigError::UnsupportedVersion { .. })
        ));
        drop_dir(&dir);
    }

    #[test]
    fn a_non_integer_version_is_a_parse_error_not_a_panic() {
        let dir = tempdir();
        let path = dir.join("jean.toml");
        std::fs::write(&path, "config_version = \"one\"\n").unwrap();
        let err = load_layers_isolated(&[path]).expect_err("must reject");
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
        drop_dir(&dir);
    }

    #[test]
    fn wrong_type_is_a_hard_error_naming_the_field() {
        let dir = tempdir();
        let path = dir.join("jean.toml");
        std::fs::write(&path, "[core]\nlog_retention_days = \"lots\"\n").unwrap();

        let err = load_layers_isolated(&[path]).expect_err("must reject");
        match &err {
            ConfigError::Parse { message, .. } => {
                assert!(
                    message.contains("log_retention_days"),
                    "error must name the field: {message}"
                );
            }
            other => panic!("expected Parse, got {other:?}"),
        }
        drop_dir(&dir);
    }

    #[test]
    fn unknown_key_warns_but_still_loads() {
        let dir = tempdir();
        let path = dir.join("jean.toml");
        std::fs::write(
            &path,
            "[core]\nlog_level = \"debug\"\nlog_levell = \"debug\"\n",
        )
        .unwrap();

        let loaded = load_layers_isolated(&[path]).expect("unknown key is non-fatal");
        assert_eq!(loaded.config.core.log_level, "debug", "known key applied");
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("core.log_levell")),
            "typo must be reported, got {:?}",
            loaded.warnings
        );
        assert!(
            loaded.warnings.iter().any(|w| w.contains("ignored")),
            "warning must say what happened to it"
        );
        drop_dir(&dir);
    }

    #[test]
    fn unknown_key_detection_is_derived_from_the_struct() {
        // Every field of the default config must be recognised. If someone adds
        // a field and forgets the Default impl, this catches it.
        let known = known_key_paths();
        let defaults = toml::Value::try_from(AppConfig::default()).unwrap();
        let mut present = BTreeSet::new();
        collect_paths(&defaults, "", &mut present);
        for key in &present {
            assert!(known.contains(key), "`{key}` should be a known key");
        }
        assert!(known.contains("core.log_level"));
        assert!(known.contains("process.env_allowlist"));
        assert!(!known.contains("core.nonexistent_field"));
    }

    #[test]
    fn deferred_sections_are_recognised_not_flagged_as_typos() {
        let dir = tempdir();
        let path = dir.join("jean.toml");
        std::fs::write(&path, "[vision]\nenabled = false\n").unwrap();

        let loaded = load_layers_isolated(&[path]).expect("deferred section must not fail");
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("[vision]") && w.contains("not applied")),
            "must explain the section is deferred, got {:?}",
            loaded.warnings
        );
        assert!(
            !loaded
                .warnings
                .iter()
                .any(|w| w.contains("unknown configuration key")),
            "a documented section is not an unknown key: {:?}",
            loaded.warnings
        );
        drop_dir(&dir);
    }

    #[test]
    fn layering_later_file_wins() {
        let dir = tempdir();
        let a = dir.join("a.toml");
        let b = dir.join("b.toml");
        std::fs::write(&a, "[core]\nlog_level = \"debug\"\nlog_max_files = 5\n").unwrap();
        std::fs::write(&b, "[core]\nlog_level = \"warn\"\n").unwrap();

        let loaded = load_layers_isolated(&[a, b]).unwrap();
        assert_eq!(loaded.config.core.log_level, "warn", "b overrides a");
        assert_eq!(loaded.config.core.log_max_files, 5, "a's value survives");
        assert_eq!(loaded.sources.len(), 3, "defaults + a + b");
        drop_dir(&dir);
    }

    #[test]
    fn missing_files_are_skipped_not_an_error() {
        let dir = tempdir();
        let loaded = load_layers_isolated(&[dir.join("does-not-exist.toml")]).unwrap();
        assert_eq!(loaded.config, AppConfig::default());
        assert_eq!(loaded.sources, vec![ConfigSource::CompiledDefaults]);
        drop_dir(&dir);
    }

    #[test]
    fn array_override_replaces_rather_than_concatenates() {
        let dir = tempdir();
        let path = dir.join("jean.toml");
        std::fs::write(&path, "[process]\nenv_allowlist = [\"PATH\"]\n").unwrap();

        let loaded = load_layers_isolated(&[path]).unwrap();
        assert_eq!(
            loaded.config.process.env_allowlist,
            vec!["PATH".to_owned()],
            "an allowlist must mean exactly what the user wrote, not that plus the defaults"
        );
        drop_dir(&dir);
    }

    #[test]
    fn env_override_applies_and_is_recorded() {
        // The lock is the serialisation, not a claim about ordering: this test
        // writes process-global state that every other config test reads.
        let _serialise = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let before = std::env::var("JEAN_LOG_LEVEL").ok();
        unsafe { std::env::set_var("JEAN_LOG_LEVEL", "trace") };
        let loaded = load_layers(&[]).unwrap();
        assert_eq!(loaded.config.core.log_level, "trace");
        assert!(loaded
            .sources
            .contains(&ConfigSource::Env("JEAN_LOG_LEVEL")));
        restore_env("JEAN_LOG_LEVEL", before.as_deref());
    }

    #[test]
    fn invalid_env_override_warns_and_keeps_the_file_value() {
        let _serialise = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let before = std::env::var("JEAN_LOG_LEVEL").ok();
        unsafe { std::env::set_var("JEAN_LOG_LEVEL", "shout") };
        let loaded = load_layers(&[]).unwrap();
        assert_eq!(loaded.config.core.log_level, "info");
        assert!(
            loaded.warnings.iter().any(|w| w.contains("JEAN_LOG_LEVEL")),
            "{:?}",
            loaded.warnings
        );
        restore_env("JEAN_LOG_LEVEL", before.as_deref());
    }

    #[test]
    fn invariants_that_would_disable_supervision_are_rejected() {
        let mut cfg = AppConfig::default();
        cfg.process.inherit_environment = true;
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::Validation {
                field: "process.inherit_environment",
                ..
            })
        ));

        let mut cfg = AppConfig::default();
        cfg.policy.on_timeout = "allow".to_owned();
        let err = cfg
            .validate()
            .expect_err("a timeout must never mean consent");
        assert!(err.to_string().contains("D-011"), "{err}");

        let mut cfg = AppConfig::default();
        cfg.policy.run_elevated_by_default = true;
        assert!(cfg.validate().is_err(), "D-010 forbids this");

        let mut cfg = AppConfig::default();
        cfg.process.capture_output_max_bytes = 0;
        assert!(cfg.validate().is_err(), "a zero cap discards all output");

        let mut cfg = AppConfig::default();
        cfg.orchestrator.default_timeout_secs = 0;
        assert!(cfg.validate().is_err(), "would kill every task at once");

        let mut cfg = AppConfig::default();
        cfg.ipc.http_enabled = true;
        let err = cfg.validate().expect_err("no network listener in Phase 0");
        assert!(err.to_string().contains("http_enabled"), "{err}");

        let mut cfg = AppConfig::default();
        cfg.ipc.transport = "tcp".to_owned();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn the_phase_one_defaults_are_a_valid_configuration() {
        // The compiled defaults must validate, or a fresh checkout with no config
        // file at all cannot run an intent.
        AppConfig::default()
            .validate()
            .expect("the shipped defaults must satisfy the shipped rules");

        let cfg = AppConfig::default();
        assert_eq!(
            cfg.policy.approval_scope,
            APPROVAL_SCOPE_OPERATION_AND_ARGUMENTS
        );
        assert_eq!(
            cfg.tools.discovery,
            ["path", "start_menu", "registry"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(cfg.tools.mark_unhealthy_after, 3);
        assert_eq!(cfg.ui.notifications, "none");
        assert!(!cfg.search.follow_reparse_points);
        assert!(cfg.policy.never_store_credentials);
    }

    /// Every rule below is asserted both ways: refused, and named in the refusal.
    ///
    /// A refusal that does not say which key it object to is how a user ends up
    /// commenting out settings one at a time to find out.
    #[test]
    fn phase_one_security_and_bound_settings_are_refused_when_wrong() {
        let mut cfg = AppConfig::default();
        cfg.policy.approval_scope = APPROVAL_SCOPE_REJECTED_LEGACY.to_owned();
        let err = cfg
            .validate()
            .expect_err("tier-scoped approvals would authorise commands nobody approved");
        assert!(
            err.to_string().contains(APPROVAL_SCOPE_REJECTED_LEGACY),
            "the refusal must name the value it is refusing: {err}"
        );
        assert!(err.to_string().contains("policy.approval_scope"), "{err}");

        let mut cfg = AppConfig::default();
        cfg.policy.never_store_credentials = false;
        let err = cfg
            .validate()
            .expect_err("no implementation stores a credential; the key cannot select one");
        assert!(err.to_string().contains("never_store_credentials"), "{err}");

        let mut cfg = AppConfig::default();
        cfg.policy.elevation_helper = "  ".to_owned();
        assert!(
            cfg.validate().is_err(),
            "an empty helper would make the elevation refusal tell the operator to run nothing"
        );

        let mut cfg = AppConfig::default();
        cfg.policy.auto_deny.push(String::new());
        assert!(
            cfg.validate().is_err(),
            "an empty capability matches nothing"
        );

        let mut cfg = AppConfig::default();
        cfg.policy.approval_timeout_secs = 0;
        assert!(
            cfg.validate().is_err(),
            "zero is an instant deny, not a prompt"
        );
        let mut cfg = AppConfig::default();
        cfg.policy.approval_timeout_secs = 3601;
        assert!(
            cfg.validate().is_err(),
            "a wait measured in hours is not consent"
        );

        let mut cfg = AppConfig::default();
        cfg.terminal.interactive_backend = "winpty".to_owned();
        let err = cfg
            .validate()
            .expect_err("there is no winpty backend in this build");
        assert!(err.to_string().contains("conpty|plain"), "{err}");
        for backend in ["conpty", "plain"] {
            let mut cfg = AppConfig::default();
            cfg.terminal.interactive_backend = backend.to_owned();
            cfg.validate()
                .unwrap_or_else(|e| panic!("{backend} is an accepted backend: {e}"));
        }

        let mut cfg = AppConfig::default();
        cfg.search.follow_reparse_points = true;
        let err = cfg
            .validate()
            .expect_err("a junction would move the walk outside the authorised root");
        assert!(
            err.to_string().contains("search.follow_reparse_points"),
            "{err}"
        );

        for zero in [
            |s: &mut SearchConfig| s.max_results = 0,
            |s: &mut SearchConfig| s.max_entries = 0,
            |s: &mut SearchConfig| s.deadline_secs = 0,
            |s: &mut SearchConfig| s.max_depth = 0,
        ] {
            let mut cfg = AppConfig::default();
            zero(&mut cfg.search);
            assert!(
                cfg.validate().is_err(),
                "a bound of zero reports nothing while looking like it succeeded"
            );
        }

        let mut cfg = AppConfig::default();
        cfg.ui.notifications = "tray".to_owned();
        let err = cfg.validate().expect_err("Phase 1 has no tray (D-009)");
        assert!(err.to_string().contains("none|toast"), "{err}");

        // The sources the settings document lists for later phases are refused by
        // name, so the operator is told they are not wired up rather than waiting
        // for applications that will never appear.
        for later in ["mcp", "acp"] {
            let mut cfg = AppConfig::default();
            cfg.tools.discovery = vec![later.to_owned()];
            let err = cfg.validate().expect_err("{later} is a Phase 3 source");
            let message = err.to_string();
            assert!(message.contains("Phase 3"), "{err}");
            assert!(
                message.contains(later),
                "the refusal must name {later}: {err}"
            );
        }

        let mut cfg = AppConfig::default();
        cfg.tools.discovery = vec!["desktop.ini".to_owned()];
        assert!(cfg.validate().is_err(), "not a discovery source");

        let mut cfg = AppConfig::default();
        cfg.tools.discovery = Vec::new();
        assert!(
            cfg.validate().is_err(),
            "with no source, launch resolves nothing"
        );

        let mut cfg = AppConfig::default();
        cfg.tools.mark_unhealthy_after = 0;
        assert!(
            cfg.validate().is_err(),
            "a tool cannot fail zero checks and be unhealthy"
        );
    }

    // ── helpers ──────────────────────────────────────────────────────────
    // A tiny temp-dir helper instead of pulling in `tempfile`: Phase 0 keeps
    // the dev-dependency set empty so the tree stays narrow.

    fn tempdir() -> PathBuf {
        // A clock reading alone is not unique: two tests that asked inside one tick shared a
        // directory, and then one test's `jean.toml` was half-overwritten by another's, which
        // surfaced as a TOML parse error in a fixture that was correct on disk in the source.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("jean-cfgtest-{nonce}-{seq}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn drop_dir(p: &Path) {
        let _ = std::fs::remove_dir_all(p);
    }

    fn restore_env(key: &str, value: Option<&str>) {
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    /// Held by anything that reads or writes the process environment.
    ///
    /// The environment is shared by every test thread in this binary, so one test
    /// setting `JEAN_LOG_LEVEL` silently changes what an unrelated test loads. That
    /// is not hypothetical: it made four tests here disagree with their own fixture
    /// files, each seeing the value another test had written.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The variables `apply_env_overrides` consults.
    const OVERRIDE_VARS: [&str; 5] = [
        "JEAN_NO_VOICE",
        "JEAN_LOG_LEVEL",
        "JEAN_DATA_DIR",
        "JEAN_MODEL_PROVIDER",
        "JEAN_MODEL",
    ];

    /// `load_layers` with the ambient overrides cleared and the environment lock
    /// held, so the config a test sees is the config its own files described.
    ///
    /// A poisoned lock is still usable: the invariant it protects is that these
    /// tests do not overlap, and a panic in one does not corrupt the environment
    /// in a way the next test cannot restore.
    fn load_layers_isolated(layers: &[PathBuf]) -> Result<LoadedConfig, ConfigError> {
        let guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut saved: Vec<(String, Option<String>)> = Vec::with_capacity(OVERRIDE_VARS.len());
        for key in OVERRIDE_VARS {
            saved.push((key.to_owned(), std::env::var(key).ok()));
            unsafe { std::env::remove_var(key) };
        }

        let loaded = load_layers(layers);

        for (key, value) in &saved {
            restore_env(key, value.as_deref());
        }
        drop(guard);
        loaded
    }
}
