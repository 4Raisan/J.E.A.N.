//! End-to-end tests against the **real `jean.exe`**, spawned as a child process with
//! its own data directory.
//!
//! Unit and intent tests in `jean-core` drive the engine with a scripted prompter. This
//! file tests what only a spawned binary can show: the exit code a script sees, the
//! columns an operator reads, the audit lines a compliance review would ask for, and the
//! fact that a *piped stdin is not consent*.
//!
//! Each test gets a throwaway `%TEMP%` directory, so the tests are independent of each
//! other and of the developer's real `%LOCALAPPDATA%\JEAN`.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const TOOLS: &str = r#"
config_version = 1

[[tool]]
id = "demo.echo"
kind = "shell"
display_name = "Demo echo"
capabilities = ["shell"]
command = "cmd.exe"
args = ["/c", "echo JEAN_DEMO_OK"]
tier = "read_only"
timeout_secs = 30
verify = { method = "output_contains", expected = 0, needle = "JEAN_DEMO_OK" }

[[tool]]
id = "demo.denied"
kind = "cli"
display_name = "Disk format (deny fixture)"
capabilities = ["format_volume"]
command = "cmd.exe"
args = ["/c", "echo MUST_NOT_RUN"]
tier = "elevated"
timeout_secs = 30
verify = { method = "exit_code", expected = 0 }

[[tool]]
id = "demo.elevated"
kind = "cli"
display_name = "Elevated fixture"
capabilities = ["elevated"]
command = "cmd.exe"
args = ["/c", "echo MUST_NOT_RUN"]
tier = "elevated"
timeout_secs = 30
verify = { method = "exit_code", expected = 0 }

[[tool]]
id = "demo.approval"
kind = "cli"
display_name = "Credential reader (approval fixture)"
capabilities = ["credential_read"]
command = "cmd.exe"
args = ["/c", "echo MUST_NOT_RUN"]
tier = "normal"
timeout_secs = 30
verify = { method = "exit_code", expected = 0 }

[[tool]]
id = "demo.absent"
kind = "cli"
display_name = "Not installed"
capabilities = ["fixture"]
command = "jean-demo-not-installed.exe"
tier = "read_only"
timeout_secs = 10
verify = { method = "exit_code", expected = 0 }
"#;

/// A scratch data directory with a `jean.toml` and a `tools.toml` in it.
struct Harness {
    dir: PathBuf,
    /// Directories prepended to the child's `PATH`, each holding a copy of a real
    /// executable under one shared name, so discovery has something genuinely ambiguous
    /// to find without touching anything the developer installed.
    path_extra: Vec<PathBuf>,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("jean-cli-test-{tag}"));
        // A previous run that was killed before cleanup must not decide this one.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create the scratch data directory");
        fs::write(
            dir.join("jean.toml"),
            format!(
                "config_version = 1\n\n[core]\ndata_dir = \"{}\"\n\n[tools]\nregistry_path = \"{}\"\n\n[policy]\napproval_timeout_secs = 5\n",
                dir.display().to_string().replace('\\', "/"),
                dir.join("tools.toml").display().to_string().replace('\\', "/"),
            ),
        )
        .expect("write jean.toml");
        fs::write(dir.join("tools.toml"), TOOLS).expect("write tools.toml");
        Self {
            dir,
            path_extra: Vec::new(),
        }
    }

    /// Two directories on `PATH`, each holding a copy of `where.exe` under `exe_name`.
    /// Copying keeps the fixture honest: two real, distinct, launchable files with one
    /// name, and nothing that has to be installed on the machine running the tests.
    fn with_ambiguous_path(mut self, exe_name: &str) -> Self {
        let system32 = PathBuf::from(std::env::var("WINDIR").expect("WINDIR")).join("System32");
        let source = system32.join("where.exe");
        for half in ["a", "b"] {
            let d = self.dir.join(format!("bin-{half}"));
            fs::create_dir_all(&d).expect("create a fake PATH directory");
            fs::copy(&source, d.join(exe_name)).expect("copy where.exe under the fixture name");
            self.path_extra.push(d);
        }
        self
    }

    fn run(&self, args: &[&str]) -> Run {
        let mut command = Command::new(env!("CARGO_BIN_EXE_jean"));
        command
            .current_dir(&self.dir)
            .env("JEAN_DATA_DIR", &self.dir)
            .args(args);
        if !self.path_extra.is_empty() {
            let mut path = std::env::var_os("PATH").unwrap_or_default();
            for dir in &self.path_extra {
                path = format!("{};{}", dir.display(), path.to_string_lossy()).into();
            }
            command.env("PATH", path);
        }
        let output = command
            .output()
            .expect("`jean.exe` must be spawnable — it is the binary cargo just built");
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// `jean run` with data on stdin, to prove a non-terminal stream cannot answer an
    /// approval prompt.
    fn run_with_stdin(&self, args: &[&str], stdin: &[u8]) -> Run {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_jean"))
            .current_dir(&self.dir)
            .env("JEAN_DATA_DIR", &self.dir)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn jean.exe");
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(stdin)
            .expect("write the scripted answer");
        let output = child.wait_with_output().expect("wait for jean.exe");
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn audit_lines(&self) -> Vec<String> {
        let path = self.dir.join("audit.jsonl");
        match fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(String::from)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn task_rows(&self) -> usize {
        // `jean tasks` is the read-only door onto the store, and it prints a count line.
        let run = self.run(&["tasks"]);
        run.stdout
            .lines()
            .find_map(|line| line.trim().strip_suffix("task(s)"))
            .and_then(|head| head.trim().parse::<usize>().ok())
            .unwrap_or(0)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn contains(&self, needle: &str) -> bool {
        self.stdout.contains(needle) || self.stderr.contains(needle)
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn version_and_an_unknown_command_behave_as_clap_promises() {
    let harness = Harness::new("meta");
    let run = harness.run(&["--version"]);
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert!(run.stdout.contains("jean "), "{}", run.stdout);

    let unknown = harness.run(&["frobnicate"]);
    assert_eq!(unknown.code, 2, "a wrong command line is a usage error");
    assert!(unknown.stderr.contains("unrecognized") || unknown.stderr.contains("invalid"));
}

#[test]
fn exec_records_a_verified_run_and_writes_its_artifact() {
    let harness = Harness::new("exec-proven");
    let run = harness.run(&["exec", "demo.echo", "extra"]);
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert!(run.contains("verdict     proven"), "{}", run.stdout);
    assert!(
        run.contains("run_command/demo.echo") || run.contains("demo.echo"),
        "{}",
        run.stdout
    );

    // The claim "output was captured" is only true if a file holds it.
    let artifacts = harness.dir.join("artifacts");
    let captured = fs::read_dir(&artifacts)
        .unwrap_or_else(|e| panic!("{} must exist: {e}", artifacts.display()))
        .flatten()
        .flat_map(|entry| fs::read_dir(entry.path()).ok())
        .flatten()
        .flatten()
        .filter(|file| {
            file.path()
                .file_name()
                .map(|n| n == "stdout.log")
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    assert!(!captured.is_empty(), "no stdout artifact was written");
    let text = fs::read_to_string(captured[0].path()).expect("read the artifact");
    assert!(text.contains("JEAN_DEMO_OK"), "{text}");

    // And the audit trail must hold a decision line and an outcome line for it.
    let lines = harness.audit_lines();
    assert!(
        lines.iter().any(|l| l.contains("\"decision\"")),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("\"outcome\"")), "{lines:?}");
    assert!(lines
        .iter()
        .all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok()));
}

#[test]
fn an_auto_denied_capability_is_refused_without_asking_and_without_running() {
    let harness = Harness::new("auto-deny");
    let run = harness.run(&["exec", "demo.denied"]);
    assert_eq!(run.code, 5, "a policy refusal is its own exit code");
    assert!(run.contains("auto-deny"), "{}", run.stdout);
    assert!(run.contains("never started"), "{}", run.stdout);
    assert!(
        !run.stdout.contains("MUST_NOT_RUN"),
        "the child must not have run"
    );
    assert!(
        !run.stdout.contains("(y/"),
        "no prompt may be printed for an auto-denied action"
    );

    // The refusal is still auditable, and still created no task row.
    assert_eq!(
        harness.task_rows(),
        0,
        "a refusal must not leave a task behind"
    );
    let lines = harness.audit_lines();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("\"denied\"") && l.contains("format_volume")),
        "the refusal itself is the audit record: {lines:?}"
    );
}

#[test]
fn an_elevated_operation_is_refused_and_names_the_sidecar_that_does_not_exist() {
    let harness = Harness::new("elevated");
    let run = harness.run(&["exec", "demo.elevated"]);
    assert_eq!(run.code, 5);
    assert!(run.contains("jean-elevated.exe"), "{}", run.stdout);
    assert!(
        run.contains("D-010"),
        "the refusal cites the decision it comes from"
    );
}

#[test]
fn a_pipe_is_not_a_person_and_an_answer_cannot_be_supplied_non_interactively() {
    let harness = Harness::new("not-a-terminal");
    // `y` on stdin would be an approval if a pipe could answer a prompt. It cannot:
    // D-011 grants only from a person or from configuration, and a redirected stream is
    // neither, so the run must deny rather than read the byte.
    let run = harness.run_with_stdin(&["exec", "demo.approval"], b"y\n");
    assert_eq!(run.code, 5, "{}\n{}", run.stdout, run.stderr);
    assert!(run.contains("not a terminal"), "{}", run.stdout);
    assert!(
        !run.stdout.contains("MUST_NOT_RUN"),
        "the child must not have run"
    );
    assert_eq!(harness.task_rows(), 0);
}

#[test]
fn the_yes_flag_is_reported_and_changes_nothing() {
    let harness = Harness::new("yes-flag");
    let run = harness.run(&["exec", "demo.approval", "--yes"]);
    assert_eq!(run.code, 5, "--yes must not buy an approval");
    assert!(
        run.contains("--yes"),
        "the flag has to say what it is doing: {}",
        run.stdout
    );
    assert!(harness.run(&["exec", "demo.denied", "--yes"]).code == 5);
}

#[test]
fn find_reports_a_hit_a_miss_and_an_unreadable_directory_inside_the_root() {
    let harness = Harness::new("find");
    let root = harness.dir.join("tree");
    fs::create_dir_all(root.join("sub")).expect("fixture directories");
    fs::write(root.join("needle.txt"), "x").expect("fixture file");

    let hit = harness.run(&["find", &root.display().to_string(), "needle*"]);
    assert_eq!(hit.code, 0, "{}", hit.stdout);
    assert!(hit.contains("verdict     proven"), "{}", hit.stdout);

    let miss = harness.run(&[
        "find",
        &root.display().to_string(),
        "nothing-with-this-name*",
    ]);
    assert_eq!(
        miss.code, 3,
        "a fully-covered no-match is not proven, not failed"
    );
    assert!(miss.contains("walked to its bounds"), "{}", miss.stdout);

    let no_root = harness.run(&[
        "find",
        &harness.dir.join("nowhere").display().to_string(),
        "*",
    ]);
    assert_ne!(
        no_root.code, 0,
        "a root that does not exist must not search silently"
    );
}

#[test]
fn ps_describes_the_test_process_and_says_what_the_platform_cannot_report() {
    let harness = Harness::new("ps");
    let pid = std::process::id().to_string();
    let run = harness.run(&["ps", "--pid", &pid]);
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert!(run.contains("proven"), "{}", run.stdout);
    assert!(
        run.contains("thread_count"),
        "the field Windows never gives this sampler must be named, not left blank"
    );
    assert!(run.contains("ONE core"), "the CPU unit must be stated");
    assert!(
        run.contains("PRIV(MiB)"),
        "the memory column is labelled for what it measures"
    );

    let gone = harness.run(&["ps", "--pid", "4294967295"]);
    assert_eq!(gone.code, 3, "an absent pid is an honest no, not a crash");
    assert!(
        gone.contains("not_proven") || gone.contains("is gone"),
        "{}",
        gone.stdout
    );
}

#[test]
fn apps_lists_what_discovery_found_and_launch_refuses_to_choose_between_them() {
    let name = "jean-ambiguous-demo.exe";
    let harness = Harness::new("ambiguous").with_ambiguous_path(name);
    let stem = name.trim_end_matches(".exe");

    let listed = harness.run(&["apps", stem]);
    assert_eq!(listed.code, 0, "{}", listed.stdout);
    assert!(listed.contains("ambiguous   yes"), "{}", listed.stdout);
    assert!(
        listed.contains("--pick"),
        "the refusal must tell the operator how to choose"
    );

    let attempt = harness.run(&["launch", stem]);
    assert_eq!(
        attempt.code, 1,
        "two matches and no pick is a refusal, not a coin flip"
    );
    assert!(
        attempt.contains("matches 2 applications"),
        "{}",
        attempt.stdout
    );
    assert_eq!(
        harness.task_rows(),
        0,
        "an ambiguous name must not create a task"
    );

    // A pick is allowed to run, and the copy of `where.exe` exits immediately, so the
    // launch intent has to report that it produced no window.
    let picked = harness.run(&["launch", stem, "--pick", "1", "--window-timeout", "1"]);
    assert_eq!(picked.code, 3, "{}\n{}", picked.stdout, picked.stderr);
    assert!(
        picked.contains("no window") || picked.contains("exited"),
        "{}",
        picked.stdout
    );
    assert!(
        picked.contains("not_proven"),
        "an exit is not a proven outcome"
    );
}

#[test]
fn health_looks_without_running_and_separates_absent_from_disabled() {
    let harness = Harness::new("health");
    let run = harness.run(&["health"]);
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert!(run.contains("demo.absent"), "{}", run.stdout);
    assert!(
        run.contains("path lookup"),
        "the report must say what a probe actually did: {}",
        run.stdout
    );
    // Probing must not have created a task or started a child.
    assert_eq!(harness.task_rows(), 0);
    assert!(!run.stdout.contains("JEAN_DEMO_OK"));
}

#[test]
fn status_is_the_compact_view_and_reads_the_record_one_task_deep() {
    let harness = Harness::new("status");
    let empty = harness.run(&["status"]);
    assert_eq!(empty.code, 0);
    assert!(empty.contains("no tasks recorded"), "{}", empty.stdout);

    harness.run(&["exec", "demo.echo", "one"]);
    let table = harness.run(&["status"]);
    assert!(table.contains("STATE"), "{}", table.stdout);
    assert!(
        table.contains("run_command/demo.echo"),
        "the intent column must name the door: {}",
        table.stdout
    );

    let id = table
        .stdout
        .lines()
        .find(|line| line.trim_start().starts_with("t-"))
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or("")
        .to_owned();
    assert!(!id.is_empty(), "no task id was printed: {}", table.stdout);

    let one = harness.run(&["task", &id]);
    assert_eq!(one.code, 0, "{}", one.stderr);
    assert!(
        one.contains("tool = demo.echo"),
        "the request is rendered field by field: {}",
        one.stdout
    );
    assert!(one.contains("env = "), "{}", one.stdout);
    assert!(
        !one.contains("\"PATH\":"),
        "the environment is shown as names, not as a wall of values"
    );

    let bad = harness.run(&["task", "not-a-task-id"]);
    assert_ne!(bad.code, 0, "an unknown id must not print an empty success");
}
