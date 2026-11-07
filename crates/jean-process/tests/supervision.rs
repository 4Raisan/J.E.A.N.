//! Integration tests for process supervision.
//!
//! These are the tests that matter most in this crate, because the guarantees they
//! check cannot be reasoned about from the code alone: that a cancelled task takes
//! its descendants with it, that a timeout fires, that output is bounded, and that
//! a process J.E.A.N. did not start is left alone.
//!
//! ## The two paths
//!
//! Every guarantee is checked twice where it can be: once for a spec wired to
//! anonymous pipes, once for a spec wired to a pseudo console. The console path is
//! the one a human types into, and it must not be a weaker promise than the pipe
//! path — same job, same suspended create, same deadline, same caps.
//!
//! What the console path cannot be tested for here is the keystroke relay: the test
//! binary's own standard input is a pipe, not a terminal, so the honest outcome for
//! these runs is "not attached to a console". Which is itself asserted, because a
//! silent degradation is the thing that would bite an operator later.
//!
//! ## Fixtures
//!
//! Everything spawned here is either PowerShell running a read-only script, `cmd.exe`
//! echoing a line, or `ping` against the loopback address. `ping -n N 127.0.0.1`
//! cannot reach another machine, cannot write anything, and is the cheapest
//! long-running child available on every Windows install — which is what makes it a
//! safe stand-in for "a descendant that would otherwise survive".
//!
//! No fixture deletes, moves, formats, or writes outside a temporary directory that
//! the test created.

use std::collections::BTreeMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use jean_process::win32::{is_same_process_alive, pid_exists};
use jean_process::{supervise, CancelToken, LaunchSpec, TerminalMode, Termination};
static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "jean-supervision-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ))
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// An environment with only what PowerShell needs.
///
/// Deliberately minimal and deliberately built by hand rather than inherited: it
/// exercises the same allowlist path production does, and `SystemRoot` is the one
/// variable whose absence makes PowerShell fail in a confusing way.
fn powershell_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for name in [
        "PATH",
        "PATHEXT",
        "SystemRoot",
        "windir",
        "SystemDrive",
        "COMSPEC",
        "TEMP",
        "TMP",
        "PSModulePath",
    ] {
        if let Ok(value) = std::env::var(name) {
            env.insert(name.to_owned(), value);
        }
    }
    env
}

fn powershell() -> PathBuf {
    for candidate in [
        r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        r"C:\Windows\SysWOW64\WindowsPowerShell\v1.0\powershell.exe",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }
    panic!("powershell.exe was not found; these tests need it as a harmless fixture")
}

/// A spec that runs a fixed PowerShell script.
fn powershell_spec(script: &str, dir: &Path, timeout: Option<Duration>) -> LaunchSpec {
    let artifacts = dir.join("artifacts");
    std::fs::create_dir_all(&artifacts).unwrap();
    LaunchSpec {
        executable: powershell(),
        args: vec![
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            script.to_owned(),
        ],
        working_dir: Some(dir.to_path_buf()),
        env: powershell_env(),
        timeout,
        output_cap_bytes: 1024 * 1024,
        stdout_path: Some(artifacts.join("stdout.log")),
        stderr_path: Some(artifacts.join("stderr.log")),
        terminal: TerminalMode::Pipes,
    }
}

/// A spec that runs a PowerShell script **under a pseudo console**.
///
/// The geometry is fixed rather than defaulted so the captured screen is the same
/// on every machine, and `stderr_path` is `None` because the spec validator refuses
/// a stderr artifact for a console run: a console has one merged stream, and an
/// empty file that somebody asked for is worse than no file.
fn pty_spec(script: &str, dir: &Path, timeout: Option<Duration>) -> LaunchSpec {
    let artifacts = dir.join("artifacts");
    std::fs::create_dir_all(&artifacts).unwrap();
    LaunchSpec {
        executable: powershell(),
        args: vec![
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            script.to_owned(),
        ],
        working_dir: Some(dir.to_path_buf()),
        env: powershell_env(),
        timeout,
        output_cap_bytes: 1024 * 1024,
        stdout_path: Some(artifacts.join("console.log")),
        stderr_path: None,
        terminal: TerminalMode::Pty { rows: 24, cols: 80 },
    }
}

/// A spec that runs `cmd.exe` with the given arguments **under a pseudo console**.
///
/// For the fixtures that must start fast: `cmd.exe` is up in a fraction of the time
/// PowerShell takes, which matters when the test's deadline is three seconds.
fn pty_cmd_spec(args: &[String], dir: &Path, timeout: Option<Duration>) -> LaunchSpec {
    let mut spec = pty_spec("", dir, timeout);
    spec.executable = cmd_exe();
    spec.args = args.to_vec();
    spec
}

fn cmd_exe() -> PathBuf {
    let path = PathBuf::from(r"C:\Windows\System32\cmd.exe");
    if path.is_file() {
        return path;
    }
    PathBuf::from(
        std::env::var("COMSPEC").expect(
            "cmd.exe was not found and COMSPEC is not set; these tests need a shell fixture",
        ),
    )
}

/// A spec that runs `cmd.exe /c <script>` over the two pipes.
///
/// The mirror of [`pty_cmd_spec`] for the piped path, chosen for the same reason:
/// a test whose deadline is three seconds must not spend that budget on PowerShell's
/// start-up, because then its assertion about *captured output* is really an
/// assertion about how loaded the machine happens to be. `cmd.exe` is up in a
/// fraction of the time, and `echo` writes before anything else can happen.
fn cmd_spec(script: &str, dir: &Path, timeout: Option<Duration>) -> LaunchSpec {
    let artifacts = dir.join("artifacts");
    std::fs::create_dir_all(&artifacts).unwrap();
    LaunchSpec {
        executable: cmd_exe(),
        args: vec!["/c".to_owned(), script.to_owned()],
        working_dir: Some(dir.to_path_buf()),
        env: powershell_env(),
        timeout,
        output_cap_bytes: 1024 * 1024,
        stdout_path: Some(artifacts.join("stdout.log")),
        stderr_path: Some(artifacts.join("stderr.log")),
        terminal: TerminalMode::Pipes,
    }
}

/// Whether this machine can create a pseudo console at all.
///
/// ConPTY needs Windows 1809 or later and is absent in some containers. A fixture
/// that cannot be started is skipped with the reason printed rather than failed —
/// the same honesty the crate's own unit tests use, and the reason an operator who
/// sees "SKIPPED" here knows the pty path was not actually exercised.
fn pty_available() -> bool {
    jean_process::conpty::Pty::create(
        jean_process::conpty::DEFAULT_ROWS,
        jean_process::conpty::DEFAULT_COLS,
    )
    .is_ok()
}

/// A spec that runs a script **file** rather than an inline command.
///
/// `-File` binds the arguments that follow it to PowerShell's `$args`; `-Command`
/// does not — it folds the rest of the line into the command text and leaves
/// `$args` empty. Measured: the inline form reported `count=0` for three real
/// arguments. Only `-File` can therefore prove that argv survived the whole
/// journey, which is what the quoting in `cmdline.rs` is for.
fn powershell_file_spec(script: &Path, dir: &Path, timeout: Option<Duration>) -> LaunchSpec {
    let artifacts = dir.join("artifacts");
    std::fs::create_dir_all(&artifacts).unwrap();
    LaunchSpec {
        executable: powershell(),
        args: vec![
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-File".to_owned(),
            script.display().to_string(),
        ],
        working_dir: Some(dir.to_path_buf()),
        env: powershell_env(),
        timeout,
        output_cap_bytes: 1024 * 1024,
        stdout_path: Some(artifacts.join("stdout.log")),
        stderr_path: Some(artifacts.join("stderr.log")),
        terminal: TerminalMode::Pipes,
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

// ─────────────────────────────────────────────────────────────────────────────
// Basic execution
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_harmless_command_runs_and_its_output_is_captured() {
    let dir = unique("basic");
    std::fs::create_dir_all(&dir).unwrap();

    let spec = powershell_spec(
        r#"Write-Output "JEAN_TEST_OK"; Write-Error "a warning on stderr"; exit 0"#,
        &dir,
        Some(Duration::from_secs(60)),
    );
    let token = CancelToken::new().unwrap();
    let outcome = supervise(&spec, &token, |_| {}).expect("supervision must succeed");

    assert_eq!(outcome.termination, Termination::Exited);
    assert_eq!(outcome.exit_code, Some(0));
    assert!(outcome.completed_naturally());
    assert!(outcome.looks_successful());
    assert!(outcome.pid > 0, "a real pid must be recorded");
    assert!(
        outcome.duration > Duration::from_millis(100),
        "PowerShell startup is not instantaneous; {} suggests the child never ran",
        outcome.duration.as_secs_f64()
    );

    let stdout = read(Path::new(outcome.stdout.path.as_deref().unwrap()));
    assert!(stdout.contains("JEAN_TEST_OK"), "stdout was {stdout:?}");
    assert!(outcome.stdout.bytes > 0);
    assert!(!outcome.stdout.truncated);

    let stderr = read(Path::new(outcome.stderr.path.as_deref().unwrap()));
    assert!(
        stderr.contains("a warning on stderr"),
        "stderr was {stderr:?}"
    );

    cleanup(&dir);
}

#[test]
fn a_nonzero_exit_code_is_reported_as_the_child_gave_it() {
    let dir = unique("exitcode");
    std::fs::create_dir_all(&dir).unwrap();

    let spec = powershell_spec("exit 7", &dir, Some(Duration::from_secs(60)));
    let token = CancelToken::new().unwrap();
    let outcome = supervise(&spec, &token, |_| {}).unwrap();

    assert_eq!(outcome.exit_code, Some(7));
    assert_eq!(outcome.termination, Termination::Exited);
    assert!(
        !outcome.looks_successful(),
        "a nonzero exit code must not be reported as success"
    );
    cleanup(&dir);
}

#[test]
fn arguments_reach_the_child_as_separate_argv_entries() {
    // The quoting rules are unit-tested against a model of CommandLineToArgvW;
    // this checks the real thing. If an argument with spaces and a quote arrives
    // split into two, the count will not be 3.
    let dir = unique("argv");
    std::fs::create_dir_all(&dir).unwrap();

    let script = dir.join("argv probe.ps1");
    std::fs::write(
        &script,
        r#"Write-Output ("count=" + $args.Count)
foreach ($a in $args) { Write-Output ("arg=[" + $a + "]") }
"#,
    )
    .unwrap();

    let mut spec = powershell_file_spec(&script, &dir, Some(Duration::from_secs(60)));
    spec.args.push("one".to_owned());
    spec.args.push("two words".to_owned());
    spec.args.push(r#"three "quoted""#.to_owned());

    let token = CancelToken::new().unwrap();
    let outcome = supervise(&spec, &token, |_| {}).unwrap();
    let stdout = read(Path::new(outcome.stdout.path.as_deref().unwrap()));

    assert!(stdout.contains("count=3"), "stdout was {stdout:?}");
    assert!(stdout.contains("arg=[one]"), "{stdout}");
    assert!(stdout.contains("arg=[two words]"), "{stdout}");
    assert!(
        stdout.contains(r#"arg=[three "quoted"]"#),
        "an embedded quote must survive the round trip: {stdout}"
    );
    cleanup(&dir);
}

#[test]
fn the_working_directory_is_honoured() {
    let dir = unique("cwd");
    let inner = dir.join("nested");
    std::fs::create_dir_all(&inner).unwrap();

    let mut spec = powershell_spec(
        "Write-Output (Get-Location).Path",
        &dir,
        Some(Duration::from_secs(60)),
    );
    spec.working_dir = Some(inner.clone());

    let token = CancelToken::new().unwrap();
    let outcome = supervise(&spec, &token, |_| {}).unwrap();
    let stdout = read(Path::new(outcome.stdout.path.as_deref().unwrap()));

    // Compare case-insensitively: PowerShell normalises the drive letter.
    assert!(
        stdout.to_lowercase().contains(
            inner
                .canonicalize()
                .unwrap_or_else(|_| inner.clone())
                .to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_lowercase()
                .as_str()
        ),
        "the child ran in the wrong directory: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn the_child_does_not_see_variables_that_were_not_given_to_it() {
    let dir = unique("env");
    std::fs::create_dir_all(&dir).unwrap();

    // Set a variable in the supervisor that is not in the spec's environment.
    std::env::set_var("JEAN_TEST_SECRET", "should-not-be-visible");

    let spec = powershell_spec(
        r#"if ($env:JEAN_TEST_SECRET) { Write-Output "LEAKED" } else { Write-Output "NOT_VISIBLE" }; Write-Output ("vars=" + (Get-ChildItem Env:).Count)"#,
        &dir,
        Some(Duration::from_secs(60)),
    );
    let token = CancelToken::new().unwrap();
    let outcome = supervise(&spec, &token, |_| {}).unwrap();
    let stdout = read(Path::new(outcome.stdout.path.as_deref().unwrap()));

    assert!(stdout.contains("NOT_VISIBLE"), "stdout was {stdout:?}");
    assert!(!stdout.contains("LEAKED"), "{stdout}");
    assert!(!stdout.contains("should-not-be-visible"), "{stdout}");

    std::env::remove_var("JEAN_TEST_SECRET");
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// Launch failures
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_missing_executable_is_reported_before_anything_is_spawned() {
    let dir = unique("missing");
    std::fs::create_dir_all(&dir).unwrap();

    let mut spec = powershell_spec("Write-Output x", &dir, Some(Duration::from_secs(5)));
    spec.executable = dir.join("definitely-not-here.exe");

    let token = CancelToken::new().unwrap();
    let err = supervise(&spec, &token, |_| {}).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("definitely-not-here.exe"), "{text}");
    assert!(err.is_pre_spawn(), "{err:?}");
    cleanup(&dir);
}

#[test]
fn a_working_directory_that_does_not_exist_is_refused() {
    let dir = unique("badcwd");
    std::fs::create_dir_all(&dir).unwrap();

    let mut spec = powershell_spec("Write-Output x", &dir, Some(Duration::from_secs(5)));
    spec.working_dir = Some(dir.join("not-created"));

    let token = CancelToken::new().unwrap();
    let err = supervise(&spec, &token, |_| {}).unwrap_err();
    assert!(err.to_string().contains("working_dir"), "{err}");
    assert!(err.is_pre_spawn());
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// Timeout
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_timeout_terminates_the_child_and_is_reported_as_a_timeout() {
    let dir = unique("timeout");
    std::fs::create_dir_all(&dir).unwrap();

    let spec = cmd_spec(
        "echo starting& ping -n 300 127.0.0.1 >nul& echo finished",
        &dir,
        Some(Duration::from_secs(3)),
    );
    let token = CancelToken::new().unwrap();

    let started = Instant::now();
    let outcome = supervise(&spec, &token, |_| {}).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.termination,
        Termination::Timeout,
        "the run must be classified as a timeout, not a natural exit"
    );
    assert!(
        elapsed < Duration::from_secs(60),
        "the deadline was 3 s but the call took {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(3),
        "it returned before the deadline: {elapsed:?}"
    );
    assert!(
        !outcome.looks_successful(),
        "a killed child must never be reported as successful, whatever exit code the kill used"
    );
    assert!(
        !is_same_process_alive(outcome.pid, None),
        "the child survived its own timeout"
    );

    let stdout = read(Path::new(outcome.stdout.path.as_deref().unwrap()));
    assert!(stdout.contains("starting"), "{stdout:?}");
    assert!(
        !stdout.contains("finished"),
        "the child was supposed to be killed before it got here: {stdout:?}"
    );
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// Cancellation, including the whole tree
// ─────────────────────────────────────────────────────────────────────────────

/// A PowerShell fixture that spawns three grandchildren, writes their pids to a
/// file, and then sleeps.
///
/// The pids go to a file rather than to stdout because `supervise` does not return
/// until the child is gone: a test that needs the pids while the child is running
/// has to read them from somewhere else.
fn tree_fixture_script(pid_file: &Path) -> String {
    format!(
        r#"$pids = @(); 1..3 | ForEach-Object {{ $p = Start-Process -FilePath "cmd.exe" -ArgumentList "/c","ping -n 300 127.0.0.1" -WindowStyle Hidden -PassThru; $pids += $p.Id }}; Set-Content -Path "{file}" -Value ($pids -join ","); Write-Output "CHILDREN_SPAWNED"; Start-Sleep -Seconds 300"#,
        file = pid_file.display()
    )
}

fn wait_for_pid_file(path: &Path, limit: Duration) -> Vec<u32> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            let pids: Vec<u32> = text
                .split(',')
                .filter_map(|part| part.trim().parse::<u32>().ok())
                .collect();
            if pids.len() == 3 {
                return pids;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "the fixture never wrote three pids to {} within {limit:?}",
        path.display()
    );
}

#[test]
fn cancelling_terminates_the_whole_process_tree() {
    let dir = unique("tree");
    std::fs::create_dir_all(&dir).unwrap();
    let pid_file = dir.join("grandchildren.txt");

    let spec = powershell_spec(
        &tree_fixture_script(&pid_file),
        &dir,
        Some(Duration::from_secs(120)),
    );
    let token = CancelToken::new().unwrap();
    let shared = token.clone();

    // Run the supervisor on another thread so this one can cancel it, which is
    // exactly what the IPC listener does in production.
    let runner = std::thread::spawn(move || supervise(&spec, &shared, |_| {}));

    let grandchildren = wait_for_pid_file(&pid_file, Duration::from_secs(60));
    assert_eq!(grandchildren.len(), 3);
    for pid in &grandchildren {
        assert!(
            pid_exists(*pid),
            "grandchild {pid} should be alive before the cancel"
        );
    }

    token.cancel().expect("the cancel event must be settable");

    let outcome = runner
        .join()
        .expect("the supervisor thread must not panic")
        .expect("supervision must return an outcome, not an error");

    assert_eq!(outcome.termination, Termination::Cancelled);
    assert!(
        outcome.error.as_deref().unwrap_or("").contains("cancel"),
        "the outcome should say why it stopped: {:?}",
        outcome.error
    );

    // The direct child.
    assert!(
        !is_same_process_alive(outcome.pid, None),
        "the PowerShell child survived cancellation"
    );

    // The grandchildren. This is the assertion the Job Object exists for: killing
    // the parent alone would leave all three running.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let alive: Vec<u32> = grandchildren
            .iter()
            .copied()
            .filter(|pid| pid_exists(*pid))
            .collect();
        if alive.is_empty() || Instant::now() > deadline {
            assert!(
                alive.is_empty(),
                "these descendants survived cancellation of their parent: {alive:?}"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    cleanup(&dir);
}

#[test]
fn cancelling_does_not_touch_a_process_jean_did_not_start() {
    // The other half of the containment guarantee. A supervisor that killed
    // everything matching a name, or everything in the session, would be far more
    // dangerous than one that leaked a process.
    let dir = unique("bystander");
    std::fs::create_dir_all(&dir).unwrap();
    let pid_file = dir.join("grandchildren.txt");

    // A long-lived process started by this test, outside any job J.E.A.N. owns.
    //
    // `ping.exe` directly rather than `cmd.exe /c ping`: killing cmd would leave
    // its `ping` child running, and that orphan inherited the harness's console
    // and printed reply lines through the following tests. Silence and no
    // descendants are both part of making the fixture tidy.
    let mut bystander = std::process::Command::new("ping.exe")
        .args(["-n", "300", "127.0.0.1"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn()
        .expect("ping.exe must be startable for this fixture");
    let bystander_pid = bystander.id();
    assert!(pid_exists(bystander_pid));

    let spec = powershell_spec(
        &tree_fixture_script(&pid_file),
        &dir,
        Some(Duration::from_secs(120)),
    );
    let token = CancelToken::new().unwrap();
    let shared = token.clone();
    let runner = std::thread::spawn(move || supervise(&spec, &shared, |_| {}));

    let grandchildren = wait_for_pid_file(&pid_file, Duration::from_secs(60));
    token.cancel().unwrap();
    let outcome = runner.join().unwrap().unwrap();

    assert_eq!(outcome.termination, Termination::Cancelled);
    assert!(
        pid_exists(bystander_pid),
        "a process J.E.A.N. never started was killed by the cancellation"
    );
    for pid in grandchildren {
        assert!(!pid_exists(pid), "grandchild {pid} survived");
    }

    // Clean up the bystander explicitly: it is this test's own child, so leaving
    // it running would be the test leaking, not J.E.A.N.
    let _ = bystander.kill();
    let _ = bystander.wait();
    cleanup(&dir);
}

#[test]
fn a_cancel_that_arrives_before_the_spawn_is_honoured() {
    let dir = unique("precancel");
    std::fs::create_dir_all(&dir).unwrap();

    let spec = powershell_spec("Write-Output x", &dir, Some(Duration::from_secs(30)));
    let token = CancelToken::new().unwrap();
    token.cancel().unwrap();

    let mut spawned = false;
    let outcome = supervise(&spec, &token, |_| spawned = true).unwrap();

    assert_eq!(outcome.termination, Termination::Cancelled);
    assert_eq!(outcome.pid, 0, "nothing should have been created");
    assert!(
        !spawned,
        "the spawn callback must not fire for a pre-cancelled run"
    );
    assert!(
        outcome.error.as_deref().unwrap_or("").contains("before"),
        "the reason must say the child was never started: {:?}",
        outcome.error
    );
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// Output bounds
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn output_past_the_cap_is_truncated_but_still_drained() {
    let dir = unique("cap");
    std::fs::create_dir_all(&dir).unwrap();

    // 4 MB of output against a 64 KiB cap. If the reader stopped at the cap the
    // pipe would fill, the child would block on its own write, and the run would
    // hang until the timeout instead of completing.
    //
    // The bytes are built as one string and written once, rather than emitted
    // 20 000 times through the PowerShell pipeline: a per-line loop takes the
    // *interpreter* about a minute, which measures PowerShell's throughput and not
    // the supervisor's draining, and it makes the test's own deadline the thing
    // that decides the result on a busy machine.
    let mut spec = powershell_spec(
        r#"[Console]::Out.Write((("x" * 200) + [char]10) * 20000); [Console]::Out.Write("REACHED_THE_END")"#,
        &dir,
        Some(Duration::from_secs(120)),
    );
    spec.output_cap_bytes = 64 * 1024;

    let token = CancelToken::new().unwrap();
    let outcome = supervise(&spec, &token, |_| {}).unwrap();

    assert_eq!(
        outcome.termination,
        Termination::Exited,
        "the child must finish on its own; a timeout here means the reader stopped draining"
    );
    assert!(
        outcome.stdout.truncated,
        "output should have exceeded the cap"
    );
    assert!(
        outcome.stdout.bytes > 4_000_000,
        "the byte count must report what the child produced, not what was kept: {}",
        outcome.stdout.bytes
    );

    let path = Path::new(outcome.stdout.path.as_deref().unwrap());
    let size = std::fs::metadata(path).unwrap().len();
    assert!(
        size <= spec.output_cap_bytes,
        "the artifact must fit inside the cap: {size} > {}",
        spec.output_cap_bytes
    );

    let text = read(path);
    assert!(
        text.contains("output truncated here"),
        "the cut must be announced at the cut point"
    );
    assert!(
        text.contains(&format!(
            "the child produced {} bytes",
            outcome.stdout.bytes
        )),
        "the artifact must state the true total so a reader knows the tail is missing"
    );
    // REACHED_THE_END was written last, so it is in the discarded tail. Its absence
    // is the expected result and the reason the byte count is reported separately.
    assert!(
        !text.contains("REACHED_THE_END"),
        "the cap should have cut before the final line"
    );
    cleanup(&dir);
}

#[test]
fn both_streams_are_drained_concurrently_without_deadlocking() {
    let dir = unique("both");
    std::fs::create_dir_all(&dir).unwrap();

    // Writes to both streams, each well past a small pipe buffer. A supervisor
    // that read stdout to EOF before touching stderr would deadlock here, because
    // the child would block writing stderr into a full pipe.
    //
    // Interleaved one-line-at-a-time through the PowerShell pipeline is avoided for
    // the same reason as the cap test above: the interpreter's per-object overhead
    // dominates, and `Write-Error` additionally builds an ErrorRecord per line. The
    // deadlock property being tested needs only that both pipes fill.
    let mut spec = powershell_spec(
        r#"[Console]::Out.Write("out-" * 20000); [Console]::Error.Write("err-" * 20000); [Console]::Out.Write("BOTH_STREAMS_DONE")"#,
        &dir,
        Some(Duration::from_secs(180)),
    );
    spec.output_cap_bytes = 1024 * 1024;

    let token = CancelToken::new().unwrap();
    let outcome = supervise(&spec, &token, |_| {}).unwrap();

    assert_eq!(
        outcome.termination,
        Termination::Exited,
        "a timeout here means one of the two readers starved the other"
    );
    assert!(outcome.stdout.bytes > 10_000, "{}", outcome.stdout.bytes);
    assert!(outcome.stderr.bytes > 10_000, "{}", outcome.stderr.bytes);

    let stdout = read(Path::new(outcome.stdout.path.as_deref().unwrap()));
    assert!(
        stdout.contains("BOTH_STREAMS_DONE"),
        "the child did not finish"
    );
    cleanup(&dir);
}

#[test]
fn a_child_that_writes_nothing_produces_empty_artifacts() {
    let dir = unique("quiet");
    std::fs::create_dir_all(&dir).unwrap();

    let spec = powershell_spec("exit 0", &dir, Some(Duration::from_secs(60)));
    let token = CancelToken::new().unwrap();
    let outcome = supervise(&spec, &token, |_| {}).unwrap();

    assert_eq!(outcome.termination, Termination::Exited);
    assert_eq!(outcome.exit_code, Some(0));
    assert!(!outcome.stdout.truncated);
    assert!(!outcome.stderr.truncated);
    assert!(
        outcome.stdout.path.is_some(),
        "the artifact is still created"
    );
    cleanup(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────
// The pseudo console path
//
// The same guarantees, checked against a child that owns a terminal. A skip here
// means this machine could not create a pseudo console, and the reason is printed
// rather than hidden: an operator must be able to tell "the console path was not
// exercised" from "the console path passed".
// ─────────────────────────────────────────────────────────────────────────────

/// The notes every console run must carry: what was done about keystrokes, and the
/// fact that the two streams merged into one.
fn assert_console_notes_are_honest(outcome: &jean_process::ChildOutcome) -> String {
    let notes = outcome.error.clone().unwrap_or_default();
    assert!(
        notes.contains("not attached to a console")
            || notes.contains("were forwarded to the console"),
        "the outcome must say what happened to keystroke forwarding: {notes}"
    );
    assert!(
        notes.contains("one merged output stream"),
        "the outcome must say stderr was not a separate stream: {notes}"
    );
    notes
}

#[test]
fn a_console_run_captures_the_screen_and_exits_with_the_childs_code() {
    if !pty_available() {
        println!("SKIPPED: this machine could not create a pseudo console");
        return;
    }
    let dir = unique("pty-basic");
    std::fs::create_dir_all(&dir).unwrap();

    let spec = pty_spec(
        r#"Write-Output "JEAN_PTY_OK"; exit 7"#,
        &dir,
        Some(Duration::from_secs(60)),
    );
    let token = CancelToken::new().unwrap();
    let mut seen_pid = 0;
    let outcome = supervise(&spec, &token, |pid| seen_pid = pid).unwrap();

    assert_eq!(
        seen_pid, outcome.pid,
        "the spawn callback saw the console child"
    );
    assert!(outcome.pid > 0, "a real pid must be recorded");
    assert_eq!(outcome.termination, Termination::Exited);
    // The child's own code, not a code the console invented: an interactive tool
    // that reports failure has to still be able to report it.
    assert_eq!(outcome.exit_code, Some(7));
    assert!(outcome.completed_naturally());
    assert!(
        !outcome.looks_successful(),
        "a nonzero exit code from a console run is still not success"
    );

    let console_path = Path::new(
        outcome
            .stdout
            .path
            .as_deref()
            .expect("the merged console stream is the stdout artifact"),
    );
    let screen = read(console_path);
    assert!(
        screen.contains("JEAN_PTY_OK"),
        "the console output was not captured: {screen:?}"
    );
    assert!(outcome.stdout.bytes > 0);

    // One stream. The stderr summary is empty *by construction*, and the notes say
    // so instead of leaving a reader to conclude the child wrote no diagnostics.
    assert_eq!(outcome.stderr, jean_process::StreamSummary::default());
    assert!(outcome.stderr.path.is_none(), "no stderr artifact exists");
    let notes = assert_console_notes_are_honest(&outcome);
    assert!(
        !dir.join("artifacts").join("stderr.log").exists(),
        "a console run must not produce a stderr artifact: {notes}"
    );

    assert!(
        !is_same_process_alive(outcome.pid, None),
        "the console child survived its own exit"
    );
    cleanup(&dir);
}

#[test]
fn a_console_timeout_terminates_the_whole_child_and_is_classified_as_a_timeout() {
    if !pty_available() {
        println!("SKIPPED: this machine could not create a pseudo console");
        return;
    }
    let dir = unique("pty-timeout");
    std::fs::create_dir_all(&dir).unwrap();

    // `cmd.exe` echoing a marker and then pinging the loopback address, rather than
    // PowerShell: the deadline has to be short enough to test, and the marker has to
    // be on the screen *before* that deadline. Pinging alone relies on `ping.exe`
    // starting inside the budget, which a loaded machine does not promise — measured
    // here, six seconds was not always enough for it to print "Pinging", so the
    // assertion was testing process-start-up speed rather than console capture. `cmd`
    // writes the marker itself, and `ping` then holds the console open for long enough
    // that only the deadline can end the run.
    let spec = pty_cmd_spec(
        &[
            "/c".to_owned(),
            "echo PTY_MARKER& ping -n 300 127.0.0.1 >nul".to_owned(),
        ],
        &dir,
        Some(Duration::from_secs(6)),
    );

    let token = CancelToken::new().unwrap();
    let started = Instant::now();
    let outcome = supervise(&spec, &token, |_| {}).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.termination,
        Termination::Timeout,
        "a console run must be classified like a piped one, not as a natural exit"
    );
    assert!(
        elapsed >= Duration::from_secs(6),
        "it returned before the deadline: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(60),
        "the deadline was 6 s but the call took {elapsed:?}"
    );
    assert_eq!(
        outcome.exit_code,
        Some(jean_process::TIMEOUT_EXIT_CODE as i32),
        "the job kill must write the same code the pipe path writes"
    );
    assert!(
        !outcome.looks_successful(),
        "a killed console run must never read as success"
    );
    assert!(
        !is_same_process_alive(outcome.pid, None),
        "the console child survived its own timeout"
    );

    let screen = read(Path::new(outcome.stdout.path.as_deref().unwrap()));
    assert!(
        screen.contains("PTY_MARKER"),
        "the console output of a live child must be captured: {screen:?}"
    );
    assert_console_notes_are_honest(&outcome);
    cleanup(&dir);
}

#[test]
fn cancelling_a_console_run_terminates_the_whole_tree() {
    if !pty_available() {
        println!("SKIPPED: this machine could not create a pseudo console");
        return;
    }
    let dir = unique("pty-tree");
    std::fs::create_dir_all(&dir).unwrap();
    let pid_file = dir.join("grandchildren.txt");

    // The same fixture the pipe test uses: three `cmd.exe /c ping` grandchildren,
    // their pids written to a file, then a sleep that only a kill can interrupt.
    let spec = pty_spec(
        &tree_fixture_script(&pid_file),
        &dir,
        Some(Duration::from_secs(120)),
    );
    let token = CancelToken::new().unwrap();
    let shared = token.clone();

    let runner = std::thread::spawn(move || supervise(&spec, &shared, |_| {}));

    let grandchildren = wait_for_pid_file(&pid_file, Duration::from_secs(60));
    for pid in &grandchildren {
        assert!(pid_exists(*pid), "grandchild {pid} should be alive first");
    }

    token.cancel().unwrap();
    let outcome = runner
        .join()
        .expect("the supervisor thread must not panic")
        .expect("supervision returns an outcome, not an error");

    assert_eq!(outcome.termination, Termination::Cancelled);
    assert_eq!(
        outcome.exit_code,
        Some(jean_process::CANCEL_EXIT_CODE as i32),
        "the cancel code is written into the job on the console path too"
    );
    assert!(
        !is_same_process_alive(outcome.pid, None),
        "the console child survived cancellation"
    );

    // The point of the job object under a console: a child that owns a terminal can
    // still not leave descendants behind.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let alive: Vec<u32> = grandchildren
            .iter()
            .copied()
            .filter(|pid| pid_exists(*pid))
            .collect();
        if alive.is_empty() || Instant::now() > deadline {
            assert!(
                alive.is_empty(),
                "these console-tree descendants survived cancellation of their parent: {alive:?}"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let notes = outcome.error.unwrap_or_default();
    assert!(notes.contains("cancel"), "{notes}");
    cleanup(&dir);
}

#[test]
fn a_console_run_stops_at_its_output_cap_and_still_drains() {
    if !pty_available() {
        println!("SKIPPED: this machine could not create a pseudo console");
        return;
    }
    let dir = unique("pty-cap");
    std::fs::create_dir_all(&dir).unwrap();

    // ~80 KB of screen against a 32 KiB cap. If the reader stopped at the cap the
    // console's output pipe would fill, PowerShell would block on its own write, and
    // the run would end on the timeout instead of exiting. The payload is modest on
    // purpose: a console renders line by line, so this is the slowest fixture in the
    // file and a megabyte of screen would buy nothing new.
    let mut spec = pty_spec(
        r#"$line = "p" * 200; 1..400 | ForEach-Object { Write-Output $line }; Write-Output "PTY_REACHED_THE_END""#,
        &dir,
        Some(Duration::from_secs(120)),
    );
    spec.output_cap_bytes = 32 * 1024;

    let token = CancelToken::new().unwrap();
    let outcome = supervise(&spec, &token, |_| {}).unwrap();

    assert_eq!(
        outcome.termination,
        Termination::Exited,
        "the console reader must keep draining past the cap: {:?}",
        outcome.error
    );
    assert!(
        outcome.stdout.truncated,
        "the screen should have exceeded the cap"
    );
    assert!(
        outcome.stdout.bytes > spec.output_cap_bytes,
        "the byte count reports what was produced, not what was kept: {}",
        outcome.stdout.bytes
    );

    let path = Path::new(outcome.stdout.path.as_deref().unwrap());
    let size = std::fs::metadata(path).unwrap().len();
    assert!(
        size <= spec.output_cap_bytes,
        "the console artifact must fit inside the cap: {size} > {}",
        spec.output_cap_bytes
    );
    let text = read(path);
    assert!(
        text.contains("output truncated here"),
        "the cut must be announced at the cut point"
    );
    assert!(
        !text.contains("PTY_REACHED_THE_END"),
        "the cap should have cut before the final line"
    );
    let notes = outcome.error.unwrap_or_default();
    assert!(
        notes.contains("console output exceeded its 32768-byte cap"),
        "the loss must be reported, and named as the console stream: {notes}"
    );
    cleanup(&dir);
}

#[test]
fn a_spec_that_does_not_ask_for_a_console_still_gets_two_separate_streams() {
    // The Phase-0 behaviour, asserted rather than assumed: adding the terminal field
    // must not have quietly routed ordinary runs through a console. The tell is
    // stderr — a piped run puts the diagnostic in its own artifact, a console run
    // would have merged it into stdout.
    let dir = unique("pipes-unchanged");
    std::fs::create_dir_all(&dir).unwrap();

    let mut spec = powershell_spec(
        r#"Write-Output "PIPE_STDOUT_MARKER"; Write-Error "PIPE_STDERR_MARKER"; exit 0"#,
        &dir,
        Some(Duration::from_secs(60)),
    );
    spec.terminal = TerminalMode::Pipes;
    let token = CancelToken::new().unwrap();
    let outcome = supervise(&spec, &token, |_| {}).unwrap();

    assert_eq!(outcome.termination, Termination::Exited);
    assert_eq!(outcome.exit_code, Some(0));

    let stdout = read(Path::new(outcome.stdout.path.as_deref().unwrap()));
    let stderr = read(Path::new(outcome.stderr.path.as_deref().unwrap()));
    assert!(stdout.contains("PIPE_STDOUT_MARKER"), "{stdout:?}");
    assert!(
        stderr.contains("PIPE_STDERR_MARKER"),
        "a piped run must keep its own stderr artifact: {stderr:?}"
    );
    assert!(
        !stdout.contains("PIPE_STDERR_MARKER"),
        "the two streams must not have merged: {stdout:?}"
    );
    assert!(outcome.stderr.bytes > 0, "stderr was captured separately");
    let notes = outcome.error.unwrap_or_default();
    assert!(
        !notes.contains("console"),
        "a pipe run must not be described as a console run: {notes}"
    );
    cleanup(&dir);
}

#[test]
fn a_console_spec_that_asks_for_a_stderr_artifact_is_refused_before_spawning() {
    // The contradiction is caught here, not in the outcome: an empty stderr file is
    // exactly the kind of artifact a later reader trusts.
    let dir = unique("pty-reject");
    std::fs::create_dir_all(&dir).unwrap();

    let mut spec = pty_spec("Write-Output x", &dir, Some(Duration::from_secs(30)));
    spec.stderr_path = Some(dir.join("artifacts").join("stderr.log"));
    let token = CancelToken::new().unwrap();

    let err = supervise(&spec, &token, |_| {}).unwrap_err();
    assert!(err.is_pre_spawn(), "{err:?}");
    assert!(err.to_string().contains("stderr_path"), "{err}");

    spec.stderr_path = None;
    spec.terminal = TerminalMode::Pty { rows: 0, cols: 80 };
    let err = supervise(&spec, &token, |_| {}).unwrap_err();
    assert!(err.to_string().contains("terminal"), "{err}");

    // Nothing was spawned by either refusal.
    assert!(
        !dir.join("artifacts").join("stdout.log").exists(),
        "a refused spec must not have run"
    );
    cleanup(&dir);
}
