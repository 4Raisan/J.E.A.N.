//! The launch request and the result of honouring it.
//!
//! [`LaunchSpec`] is the only input to the supervisor. Everything policy-related
//! — which tools exist, whether one is allowed to run, which environment
//! variables it may see, how long it may run — is decided by `jean-core` before
//! a spec is built. This crate executes a spec and reports what happened; it
//! does not decide whether it should have.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use jean_protocol::{StreamSummary, Termination};

use crate::error::{ProcessError, Result};

/// Longest a single argument may be.
///
/// Windows bounds the whole command line at 32,767 characters. Checking each
/// argument separately gives a better error than letting `CreateProcessW` fail
/// with an opaque message once the concatenation is too long.
pub const MAX_ARG_CHARS: usize = 32_000;

/// How the child's standard streams are wired.
///
/// ## Why the pipe path is the default and stays that way
///
/// Every Phase-0 caller built a spec without saying anything about a terminal, and
/// silently moving those runs onto a pseudo console would merge their two streams
/// into one noisy screen capture. `Pipes` is therefore both the enum's default and
/// what an unset field means, so an old spec keeps behaving exactly as it did.
///
/// ## Why a pseudo console exists at all
///
/// `ssh`, a REPL, a TUI installer: a program that checks whether it owns a terminal
/// refuses to be useful when it does not, and stdin as a closed pipe cannot be typed
/// into however much the operator wants to. ConPTY gives the child a real terminal
/// that J.E.A.N. still owns, can still read, and can still kill through its job. See
/// [`crate::conpty`] for what that costs: one merged output stream instead of two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminalMode {
    /// Anonymous pipes: stdout and stderr captured separately, stdin unusable.
    #[default]
    Pipes,
    /// A pseudo console of `rows` x `cols`: one merged output stream, and the
    /// supervisor's own keystrokes relayed into it while the child runs.
    Pty { rows: u16, cols: u16 },
}

impl TerminalMode {
    /// A console at the geometry a program sees before anything has resized it.
    ///
    /// For a caller that wants a terminal but has no window size to pass. The
    /// numbers live with the primitive that enforces them, rather than being
    /// repeated here.
    pub const fn default_pty() -> Self {
        Self::Pty {
            rows: crate::conpty::DEFAULT_ROWS,
            cols: crate::conpty::DEFAULT_COLS,
        }
    }

    /// Whether this run gets a terminal rather than pipes.
    pub const fn is_pty(&self) -> bool {
        matches!(self, Self::Pty { .. })
    }

    /// The console geometry, when this run has a console.
    pub const fn pty_size(&self) -> Option<(u16, u16)> {
        match self {
            Self::Pty { rows, cols } => Some((*rows, *cols)),
            Self::Pipes => None,
        }
    }
}

/// A fully-resolved request to run one child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Absolute path to the executable.
    ///
    /// Must already be resolved: `CreateProcessW` is given this as
    /// `lpApplicationName`, which does **not** search `PATH`. Resolution is
    /// `jean-core`'s job, because it owns the tool registry.
    pub executable: PathBuf,
    /// Arguments, passed as a vector. Never a shell string.
    pub args: Vec<String>,
    /// Working directory for the child. `None` inherits the supervisor's, which
    /// is almost never what a tool wants — `jean-core` fills this in.
    pub working_dir: Option<PathBuf>,
    /// The child's complete environment. Allowlist filtering already happened.
    pub env: BTreeMap<String, String>,
    /// How long the child may run before the job is terminated.
    pub timeout: Option<Duration>,
    /// Per-stream retention ceiling, in bytes.
    pub output_cap_bytes: u64,
    /// Where to write captured stdout. `None` counts bytes without retaining.
    pub stdout_path: Option<PathBuf>,
    /// Where to write captured stderr. `None` counts bytes without retaining.
    pub stderr_path: Option<PathBuf>,
    /// How the child's streams are wired: plain pipes, or a pseudo console an
    /// operator can type into. A Phase-0 spec that says nothing gets
    /// [`TerminalMode::Pipes`], which is why `Default` is that variant.
    pub terminal: TerminalMode,
}

impl LaunchSpec {
    /// Check the spec before touching the Win32 API.
    ///
    /// Everything here is cheap and produces a better message than a failed
    /// `CreateProcessW`. The executable is checked for existence because "the
    /// file is not there" and "Windows refused to run it" have different fixes.
    pub fn validate(&self) -> Result<()> {
        if self.executable.as_os_str().is_empty() {
            return Err(invalid("executable", "it is empty"));
        }
        for arg in &self.args {
            if arg.chars().count() > MAX_ARG_CHARS {
                return Err(invalid(
                    "args",
                    &format!(
                        "one argument is {} characters, over the {MAX_ARG_CHARS} limit",
                        arg.chars().count()
                    ),
                ));
            }
            if arg.contains('\0') {
                return Err(invalid(
                    "args",
                    "an argument contains a NUL byte, which cannot be passed to CreateProcessW",
                ));
            }
        }
        if let Some(dir) = &self.working_dir {
            if !dir.is_dir() {
                return Err(invalid(
                    "working_dir",
                    &format!("{} does not exist or is not a directory", dir.display()),
                ));
            }
        }
        if self.output_cap_bytes == 0 {
            return Err(invalid(
                "output_cap_bytes",
                "it is zero, which would discard all output silently; use stdout_path = none to \
                 disable capture instead",
            ));
        }
        if self.timeout == Some(Duration::ZERO) {
            return Err(invalid(
                "timeout",
                "it is zero, so the child could never run",
            ));
        }
        self.validate_terminal()?;

        // Existence is checked last: the messages above are more specific, and
        // this is the only check that touches the filesystem.
        let meta =
            std::fs::metadata(&self.executable).map_err(|_| ProcessError::NoSuchExecutable {
                path: self.executable.clone(),
            })?;
        if !meta.is_file() {
            return Err(ProcessError::NoSuchExecutable {
                path: self.executable.clone(),
            });
        }
        Ok(())
    }

    /// Check the console request specifically.
    ///
    /// Two things here would otherwise be caught late and reported badly. A zero
    /// row or column reaches `CreatePseudoConsole`, which refuses it with a
    /// generic error, and a full-screen program that divides by a row count then
    /// crashes on its own. An `stderr_path` alongside a pseudo console is a
    /// contradiction: a console has one output surface, so the file would be
    /// created and left empty, and a reader would conclude the child printed no
    /// diagnostics when in fact the diagnostics are inside the stdout artifact.
    fn validate_terminal(&self) -> Result<()> {
        let TerminalMode::Pty { rows, cols } = self.terminal else {
            return Ok(());
        };
        if rows == 0 || cols == 0 {
            return Err(invalid(
                "terminal",
                &format!("a pseudo console must be at least 1x1, got {rows}x{cols}"),
            ));
        }
        if let Some(path) = &self.stderr_path {
            return Err(invalid(
                "stderr_path",
                &format!(
                    "{} was asked for under a pseudo console, which has no separate stderr \
                     stream; the console output is one merged stream and is captured by \
                     stdout_path",
                    path.display()
                ),
            ));
        }
        Ok(())
    }

    /// A one-line, secret-free description for logs and events.
    ///
    /// Arguments are summarised, not reproduced: the caller has already redacted
    /// what it could, but a log line is a second place for a missed secret to
    /// land, so the count is reported instead of the content.
    ///
    /// The stream wiring is reported, because "this run was a pseudo console"
    /// changes how the output artifact should be read, and it is the one part of
    /// the spec that is not already visible in the executable and its arguments.
    pub fn describe(&self) -> String {
        format!(
            "{} ({} argument{}) in {}{}",
            self.executable.display(),
            self.args.len(),
            if self.args.len() == 1 { "" } else { "s" },
            self.working_dir
                .as_ref()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|| "the supervisor's directory".to_owned()),
            match self.terminal {
                TerminalMode::Pipes => String::new(),
                TerminalMode::Pty { rows, cols } => format!(" on a {rows}x{cols} pseudo console"),
            }
        )
    }
}

fn invalid(field: &'static str, reason: &str) -> ProcessError {
    ProcessError::Invalid {
        field,
        reason: reason.to_owned(),
    }
}

/// What the supervisor observed.
///
/// Note what is *not* here: any judgement about whether the user's goal was
/// achieved. `termination == Exited` and `exit_code == Some(0)` means the
/// process ran and said it was happy. Whether that proves the task succeeded is
/// the verification step's question, and it lives in `jean-core`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildOutcome {
    /// The child's pid. Always present, even on launch failure, as 0.
    pub pid: u32,
    /// Exit code, when the child got far enough to have one.
    ///
    /// `None` means the code could not be read, not that it was zero. After a
    /// forced termination this is the code J.E.A.N. chose for the job kill, so
    /// `termination` must be read alongside it.
    pub exit_code: Option<i32>,
    pub termination: Termination,
    pub stdout: StreamSummary,
    pub stderr: StreamSummary,
    /// How long the child ran, from resume to exit.
    pub duration: Duration,
    /// A capture or supervision problem that did not prevent the run.
    pub error: Option<String>,
}

impl ChildOutcome {
    /// A pre-spawn failure: no child ever existed.
    pub fn launch_failed(reason: &str, stdout: StreamSummary, stderr: StreamSummary) -> Self {
        Self {
            pid: 0,
            exit_code: None,
            termination: Termination::LaunchFailed,
            stdout,
            stderr,
            duration: Duration::ZERO,
            error: Some(reason.to_owned()),
        }
    }

    /// True when the child ran to its own conclusion within the deadline.
    pub fn completed_naturally(&self) -> bool {
        self.termination == Termination::Exited
    }

    /// True when the child ran, produced an exit code of 0, and was not forced
    /// to stop. Still not proof the goal was met — see the type docs.
    pub fn looks_successful(&self) -> bool {
        self.termination == Termination::Exited && self.exit_code == Some(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An executable that exists on every Windows install and does nothing
    /// harmful when merely stat-ed. Tests only check existence, never run it.
    fn real_executable() -> PathBuf {
        for candidate in [
            r"C:\Windows\System32\cmd.exe",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        ] {
            let p = PathBuf::from(candidate);
            if p.is_file() {
                return p;
            }
        }
        // Fall back to this test binary's own path, which always exists.
        std::env::current_exe().expect("current_exe")
    }

    fn spec() -> LaunchSpec {
        LaunchSpec {
            executable: real_executable(),
            args: vec!["-NoProfile".to_owned()],
            working_dir: Some(std::env::temp_dir()),
            env: BTreeMap::new(),
            timeout: Some(Duration::from_secs(30)),
            output_cap_bytes: 1024,
            stdout_path: None,
            stderr_path: None,
            terminal: TerminalMode::Pipes,
        }
    }

    /// The same spec with a console requested at the given geometry.
    fn pty_spec(rows: u16, cols: u16) -> LaunchSpec {
        let mut s = spec();
        s.terminal = TerminalMode::Pty { rows, cols };
        s
    }

    #[test]
    fn a_well_formed_spec_validates() {
        spec().validate().expect("should validate");
    }

    #[test]
    fn the_terminal_default_is_the_pipe_path() {
        // Phase 0 built specs that never mentioned a terminal. If the default were
        // anything else, every one of those runs would silently change how its
        // output is captured.
        assert_eq!(TerminalMode::default(), TerminalMode::Pipes);
        assert!(!TerminalMode::default().is_pty());
        assert_eq!(
            TerminalMode::default_pty(),
            TerminalMode::Pty {
                rows: 40,
                cols: 120
            }
        );
        assert!(TerminalMode::default_pty().is_pty());
    }

    #[test]
    fn a_console_spec_validates_when_the_geometry_and_the_paths_agree() {
        pty_spec(24, 80)
            .validate()
            .expect("a 24x80 console is usable");
        // No artifact at all is legal: the bytes are still counted.
        let mut s = pty_spec(24, 80);
        s.stdout_path = None;
        s.validate()
            .expect("counting without retaining is a choice");
    }

    #[test]
    fn a_zero_sized_console_is_refused_by_the_spec_not_by_win32() {
        // `Pty::create` also rejects this, but by then the message is a Win32
        // failure with no field name in it.
        for (rows, cols) in [(0, 80), (24, 0), (0, 0)] {
            let err = pty_spec(rows, cols).validate().unwrap_err();
            assert!(matches!(err, ProcessError::Invalid { .. }), "{err:?}");
            let text = err.to_string();
            assert!(text.contains("terminal"), "{text}");
            assert!(text.contains("1x1"), "{text}");
        }
    }

    #[test]
    fn a_console_spec_with_a_stderr_artifact_is_refused_as_a_contradiction() {
        // A console has one output surface. Accepting the request would create the
        // file and leave it empty, which reads to a later reader as "the child
        // printed no diagnostics".
        let mut s = pty_spec(24, 80);
        s.stderr_path = Some(std::env::temp_dir().join("jean-spec-test-stderr.log"));
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("stderr_path"), "{err}");
        assert!(
            err.to_string().contains("merged"),
            "the message must say where the diagnostics actually went: {err}"
        );

        // The pipe path keeps its own stderr artifact.
        let mut pipes = spec();
        pipes.stderr_path = Some(std::env::temp_dir().join("jean-spec-test-stderr.log"));
        pipes.validate().expect("pipes do have a separate stderr");
    }

    #[test]
    fn an_empty_executable_path_is_refused() {
        let mut s = spec();
        s.executable = PathBuf::new();
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("executable"), "{err}");
    }

    #[test]
    fn a_missing_executable_is_reported_as_missing_not_as_a_win32_failure() {
        let mut s = spec();
        s.executable = PathBuf::from(r"C:\definitely\not\here\jean-test-fixture.exe");
        let err = s.validate().unwrap_err();
        // Checked before the match, which moves `path` out of `err`.
        assert!(err.is_pre_spawn());
        match err {
            ProcessError::NoSuchExecutable { path } => {
                assert!(path.display().to_string().contains("jean-test-fixture.exe"));
            }
            other => panic!("expected NoSuchExecutable, got {other:?}"),
        }
    }

    #[test]
    fn a_directory_is_not_an_executable() {
        let mut s = spec();
        s.executable = std::env::temp_dir();
        assert!(matches!(
            s.validate().unwrap_err(),
            ProcessError::NoSuchExecutable { .. }
        ));
    }

    #[test]
    fn a_missing_working_directory_is_refused() {
        let mut s = spec();
        s.working_dir = Some(PathBuf::from(r"C:\definitely\not\a\directory"));
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("working_dir"), "{err}");
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    #[test]
    fn no_working_directory_is_allowed() {
        let mut s = spec();
        s.working_dir = None;
        s.validate().expect("inherit is legal, if inadvisable");
    }

    #[test]
    fn a_zero_output_cap_is_refused_because_it_would_hide_output() {
        let mut s = spec();
        s.output_cap_bytes = 0;
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("output_cap_bytes"), "{err}");
        assert!(
            err.to_string().contains("silently"),
            "the message must explain the risk, not just the rule: {err}"
        );
    }

    #[test]
    fn a_zero_timeout_is_refused() {
        let mut s = spec();
        s.timeout = Some(Duration::ZERO);
        assert!(s.validate().unwrap_err().to_string().contains("timeout"));
    }

    #[test]
    fn no_timeout_means_no_deadline() {
        let mut s = spec();
        s.timeout = None;
        s.validate()
            .expect("an unbounded run is a policy choice, not an error");
    }

    #[test]
    fn a_nul_in_an_argument_is_refused_before_any_win32_call() {
        let mut s = spec();
        s.args = vec!["fine".to_owned(), "bad\0arg".to_owned()];
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("NUL"), "{err}");
    }

    #[test]
    fn an_oversized_argument_is_refused_with_its_length() {
        let mut s = spec();
        s.args = vec!["x".repeat(MAX_ARG_CHARS + 1)];
        let err = s.validate().unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("32000")
                || text.contains("32,000")
                || text.contains(&format!("{MAX_ARG_CHARS}")),
            "{text}"
        );
    }

    #[test]
    fn an_argument_at_exactly_the_limit_is_allowed() {
        let mut s = spec();
        s.args = vec!["x".repeat(MAX_ARG_CHARS)];
        s.validate().expect("the limit is inclusive");
    }

    #[test]
    fn describe_reports_the_argument_count_not_the_arguments() {
        let mut s = spec();
        s.args = vec!["-Command".to_owned(), "hunter2".to_owned()];
        let text = s.describe();
        assert!(text.contains("2 arguments"), "{text}");
        assert!(
            !text.contains("hunter2"),
            "describe() is used in logs and events; it must not reproduce \
             argument content that a redactor might have missed: {text}"
        );
    }

    #[test]
    fn describe_handles_the_singular_and_the_inherited_directory() {
        let mut s = spec();
        s.args = vec!["only".to_owned()];
        s.working_dir = None;
        let text = s.describe();
        assert!(text.contains("1 argument)"), "{text}");
        assert!(text.contains("supervisor's directory"), "{text}");
        assert!(
            !text.contains("console"),
            "a pipe run must not look like a terminal run in the log: {text}"
        );
    }

    #[test]
    fn describe_says_when_the_run_was_a_terminal() {
        // The artifact of a console run is one merged screen, and reading it as a
        // plain stdout log is a different job. The log line has to say which it is.
        let text = pty_spec(30, 100).describe();
        assert!(text.contains("30x100 pseudo console"), "{text}");
    }

    #[test]
    fn a_launch_failure_outcome_says_so_without_an_exit_code() {
        let o = ChildOutcome::launch_failed(
            "The system cannot find the file specified.",
            StreamSummary::default(),
            StreamSummary::default(),
        );
        assert_eq!(o.termination, Termination::LaunchFailed);
        assert_eq!(o.pid, 0);
        assert_eq!(o.exit_code, None);
        assert!(!o.completed_naturally());
        assert!(!o.looks_successful());
        assert!(o.error.as_deref().unwrap().contains("cannot find"));
    }

    #[test]
    fn a_zero_exit_code_from_a_killed_child_is_not_success() {
        // A job kill reports the exit code J.E.A.N. chose, so reading
        // `exit_code == 0` alone would call a cancelled task a success.
        let mut o =
            ChildOutcome::launch_failed("x", StreamSummary::default(), StreamSummary::default());
        o.termination = Termination::Cancelled;
        o.exit_code = Some(0);
        assert!(!o.looks_successful());

        o.termination = Termination::Timeout;
        assert!(!o.looks_successful());

        o.termination = Termination::Exited;
        assert!(o.looks_successful());
    }

    #[test]
    fn a_nonzero_exit_code_is_not_success() {
        let o = ChildOutcome {
            pid: 1,
            exit_code: Some(1),
            termination: Termination::Exited,
            stdout: StreamSummary::default(),
            stderr: StreamSummary::default(),
            duration: Duration::from_millis(5),
            error: None,
        };
        assert!(o.completed_naturally());
        assert!(!o.looks_successful());
    }

    #[test]
    fn a_spec_is_comparable_so_tests_can_assert_on_it() {
        assert_eq!(spec(), spec());
        let mut other = spec();
        other.args.push("extra".to_owned());
        assert_ne!(spec(), other);
        // The wiring is part of the request's identity: a console run and a pipe
        // run are different tasks even when every other field matches.
        assert_ne!(spec(), pty_spec(24, 80));
    }

    #[test]
    fn validate_does_not_run_the_executable() {
        // Asserted structurally: the only filesystem call is `metadata`. A
        // fixture path that could not exist is enough to prove nothing was
        // spawned, because a spawn would have produced a different error.
        let s = LaunchSpec {
            executable: PathBuf::from("relative-path-with-no-extension"),
            args: vec![],
            working_dir: None,
            env: BTreeMap::new(),
            timeout: None,
            output_cap_bytes: 1,
            stdout_path: None,
            stderr_path: None,
            terminal: TerminalMode::Pipes,
        };
        assert!(matches!(
            s.validate().unwrap_err(),
            ProcessError::NoSuchExecutable { .. }
        ));
    }
}
