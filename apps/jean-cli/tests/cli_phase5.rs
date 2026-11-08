use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use jean_protocol::interaction::{self as wire, Envelope, OwnerCommand};

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<BufReader<std::fs::File>>,
    session: String,
    next: u64,
    dir: PathBuf,
}

impl Client {
    fn new(no_voice: bool) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("p5-cli-{}", jean_protocol::ipc::new_task_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("jean.toml"),
            "[voice]\nenabled = true\nbackend = 'fixture'\n",
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_jean"));
        command.env_clear();
        for key in [
            "SystemRoot",
            "PATH",
            "TEMP",
            "TMP",
            "LOCALAPPDATA",
            "APPDATA",
            "USERPROFILE",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        if no_voice {
            command.env("JEAN_NO_VOICE", "1");
        }
        let (output, sink) = unsafe {
            let mut read = windows::Win32::Foundation::HANDLE::default();
            let mut write = windows::Win32::Foundation::HANDLE::default();
            windows::Win32::System::Pipes::CreatePipe(&mut read, &mut write, None, 512).unwrap();
            (
                std::fs::File::from_raw_handle(read.0),
                OwnedHandle::from_raw_handle(write.0),
            )
        };
        let mut child = command
            .arg("interact")
            .env("JEAN_DATA_DIR", &dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(sink))
            .stderr(Stdio::from(
                std::fs::File::create(dir.join("stderr.log")).unwrap(),
            ))
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = Some(BufReader::new(output));
        let mut client = Self {
            child,
            input,
            output,
            session: String::new(),
            next: 0,
            dir,
        };
        let snapshot = client.read();
        client.session = snapshot["session"].as_str().unwrap().into();
        client
    }

    fn read(&mut self) -> serde_json::Value {
        let mut output = self.output.take().unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let result = wire::read::<serde_json::Value>(&mut output);
            let _ = tx.send((output, result));
        });
        let result = rx.recv_timeout(Duration::from_secs(8));
        if result.is_err() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        reader.join().unwrap();
        let (output, result) = result.expect("CLI response deadline");
        self.output = Some(output);
        result.unwrap().unwrap_or_else(|| {
            panic!(
                "CLI disconnected: {}",
                std::fs::read_to_string(self.dir.join("stderr.log")).unwrap()
            )
        })
    }

    fn send(&mut self, body: OwnerCommand) {
        self.next += 1;
        wire::write(
            self.input.as_mut().unwrap(),
            &Envelope::new(
                self.session.clone(),
                self.next,
                wire::now_ms() + 10000,
                body,
            ),
        )
        .unwrap();
    }

    fn status(&mut self) -> serde_json::Value {
        self.send(OwnerCommand::Status);
        self.read()["body"]["snapshot"].clone()
    }

    fn ready(&mut self) -> serde_json::Value {
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            let snapshot = self.status();
            if snapshot["voice"] == "ready" {
                return snapshot;
            }
            assert!(Instant::now() < until, "{snapshot}");
        }
    }

    fn wait(&mut self) -> ExitStatus {
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < until,
                "CLI did not stop within fixed bound"
            );
            std::thread::yield_now();
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[test]
fn json_control_stays_responsive_and_shutdown_joins_with_stdin_still_open() {
    let mut client = Client::new(false);
    client.ready();
    client.send(OwnerCommand::Speak {
        text: "synthetic acknowledgement".into(),
    });
    let utterance = client.read()["body"]["snapshot"]["utterance"]
        .as_u64()
        .unwrap();
    client.send(OwnerCommand::Interrupt { utterance });
    assert_eq!(client.read()["body"]["accepted"], true);
    client.send(OwnerCommand::Shutdown);
    assert_eq!(client.read()["body"]["accepted"], true);
    assert!(client.wait().success());
}

#[test]
fn no_voice_environment_override_creates_no_worker() {
    let mut client = Client::new(true);
    let state = client.status();
    assert_eq!(state["voice"], "disabled");
    assert!(state["voice_pid"].is_null());
    client.send(OwnerCommand::StartVoice);
    assert_eq!(client.read()["body"]["accepted"], false);
    client.input.take();
    assert!(client.wait().success());
}

#[test]
fn a_stalled_output_client_cannot_block_owner_teardown() {
    let mut client = Client::new(false);
    client.ready();
    client.send(OwnerCommand::Submit {
        goal: "fixture with disabled provider".into(),
    });
    assert_eq!(client.read()["body"]["accepted"], true);
    for _ in 0..wire::QUEUE_CAPACITY {
        client.send(OwnerCommand::Status);
    }
    let status = client.wait();
    assert!(
        !status.success(),
        "backpressure must be reported, not silently dropped"
    );
    let diagnostic = std::fs::read_to_string(client.dir.join("stderr.log")).unwrap();
    assert!(
        diagnostic.contains("control client") || diagnostic.contains("control output"),
        "{diagnostic}"
    );
}

#[test]
fn oversized_control_frames_close_the_session_without_execution() {
    let mut client = Client::new(false);
    client.ready();
    let _ = client
        .input
        .as_mut()
        .unwrap()
        .write_all(&vec![b'x'; wire::MAX_FRAME + 1]);
    assert!(!client.wait().success());
    let store = jean_store::Store::open(&client.dir.join("jean.db"), "WAL", Duration::from_secs(2))
        .unwrap();
    assert!(store
        .list_tasks(jean_store::TaskFilter::All, 10)
        .unwrap()
        .is_empty());
}

/// The operator surface: the same session, driven through `jean interact --tty`, whose
/// stdin is commands and whose stdout is lines a person reads. The JSON client above is
/// what a machine uses; this is what an interruption looks like from a keyboard.
struct Tty {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<BufReader<std::fs::File>>,
    dir: PathBuf,
}

/// Silence of `frames` samples, so `offline_wav` can point at a real file. Nothing in
/// these tests presses the microphone: the key selects the offline seam, whose render
/// half is a counted sink that paces at 1× — the only reason a fixture utterance has a
/// duration an interrupt can land inside.
fn write_silent_wav(path: &std::path::Path, frames: usize) {
    fn push(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    fn push16(bytes: &mut Vec<u8>, value: u16) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let data = frames * 2;
    let mut bytes = Vec::with_capacity(44 + data);
    bytes.extend_from_slice(b"RIFF");
    push(&mut bytes, 36 + data as u32);
    bytes.extend_from_slice(b"WAVEfmt ");
    push(&mut bytes, 16);
    push16(&mut bytes, 1);
    push16(&mut bytes, 1);
    push(&mut bytes, 16_000);
    push(&mut bytes, 32_000);
    push16(&mut bytes, 2);
    push16(&mut bytes, 16);
    bytes.extend_from_slice(b"data");
    push(&mut bytes, data as u32);
    bytes.resize(44 + data, 0);
    std::fs::write(path, bytes).unwrap();
}

/// A registered tool that only takes real time, so an interruption that is not allowed
/// to reach tasks has something live to not reach.
const SLEEPER: &str = r#"config_version = 1

[[tool]]
id = "indep.sleeper"
kind = "shell"
display_name = "Sleep (voice independence witness)"
capabilities = ["fixture"]
command = "powershell.exe"
args = [
  "-NoProfile",
  "-NonInteractive",
  "-Command",
  'Write-Output "TASK_ALIVE_BEFORE"; Start-Sleep -Seconds 8; Write-Output "TASK_ALIVE_AFTER_VOICE_INTERRUPT"',
]
tier = "normal"
timeout_secs = 60
verify = { method = "output_contains", expected = 0, needle = "TASK_ALIVE_AFTER_VOICE_INTERRUPT" }
"#;

impl Tty {
    fn new(utterance_ms: u64) -> Self {
        Self::with(utterance_ms, None)
    }

    /// `worker` is `JEAN_VOICE_WORKER`: the executable the session must launch for
    /// playback, which is how a provisioned native worker survives a workspace build.
    fn with(utterance_ms: u64, worker: Option<PathBuf>) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("p5r-tty-{}", jean_protocol::ipc::new_task_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("mic.wav");
        write_silent_wav(&clip, 16_000);
        std::fs::write(
            dir.join("jean.toml"),
            format!(
                "[voice]\nenabled = true\nbackend = 'fixture'\noffline_wav = '{}'\nfixture_utterance_ms = {utterance_ms}\n",
                clip.display()
            ),
        )
        .unwrap();
        std::fs::write(dir.join("tools.toml"), SLEEPER).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_jean"));
        command.env_clear();
        for key in [
            "SystemRoot",
            "PATH",
            "TEMP",
            "TMP",
            "LOCALAPPDATA",
            "APPDATA",
            "USERPROFILE",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let (output, sink) = unsafe {
            let mut read = windows::Win32::Foundation::HANDLE::default();
            let mut write = windows::Win32::Foundation::HANDLE::default();
            windows::Win32::System::Pipes::CreatePipe(&mut read, &mut write, None, 4096).unwrap();
            (
                std::fs::File::from_raw_handle(read.0),
                OwnedHandle::from_raw_handle(write.0),
            )
        };
        let spec = command
            .args(["interact", "--tty"])
            .env("JEAN_DATA_DIR", &dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(sink))
            .stderr(Stdio::from(
                std::fs::File::create(dir.join("stderr.log")).unwrap(),
            ));
        if let Some(worker) = worker {
            spec.env("JEAN_VOICE_WORKER", worker);
        }
        let mut child = spec.spawn().unwrap();
        let input = child.stdin.take();
        Self {
            child,
            input,
            output: Some(BufReader::new(output)),
            dir,
        }
    }

    fn read(&mut self) -> String {
        let mut output = self.output.take().unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let read = output
                .read_line(&mut line)
                .map(|n| if n == 0 { None } else { Some(line) });
            let _ = tx.send((output, read));
        });
        let result = rx.recv_timeout(Duration::from_secs(10));
        if result.is_err() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        reader.join().unwrap();
        let (output, result) = result.expect("tty session line deadline");
        self.output = Some(output);
        let line = result
            .unwrap()
            .unwrap_or_else(|| panic!("lines ended: {}", self.diagnostic()));
        line.trim_end_matches(['\r', '\n']).to_owned()
    }

    fn diagnostic(&self) -> String {
        std::fs::read_to_string(self.dir.join("stderr.log")).unwrap_or_default()
    }

    fn send(&mut self, line: &str) {
        self.input
            .as_mut()
            .unwrap()
            .write_all(format!("{line}\n").as_bytes())
            .unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
    }

    /// Lines up to and including the first that contains `needle`, and how long that
    /// took from now: the elapsed time is the measurement, not a side effect.
    fn until(&mut self, needle: &str) -> (Vec<String>, Duration) {
        let started = Instant::now();
        let mut lines = Vec::new();
        loop {
            let line = self.read();
            let found = line.contains(needle);
            lines.push(line);
            if found {
                return (lines, started.elapsed());
            }
        }
    }

    /// The id the session just gave the utterance it started, read off its own answer.
    fn utterance(&mut self) -> u64 {
        loop {
            let line = self.read();
            if let Some(rest) = line.strip_prefix("ok: utterance ") {
                return rest
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap_or_else(|e| panic!("{line:?}: {e}"));
            }
        }
    }

    fn wait(&mut self) -> ExitStatus {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < until, "tty session did not stop");
            std::thread::yield_now();
        }
    }

    fn tasks(&self) -> Vec<jean_store::TaskRecord> {
        let store =
            jean_store::Store::open(&self.dir.join("jean.db"), "WAL", Duration::from_secs(2))
                .unwrap();
        store.list_tasks(jean_store::TaskFilter::All, 10).unwrap()
    }

    /// The state of the one task in this directory, as the operator would read it.
    fn task_state(&self) -> Option<String> {
        self.tasks().first().map(|task| task.state.to_string())
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[test]
fn tty_stop_cuts_playback_short_and_a_stale_id_cannot_stop_the_newer_utterance() {
    const PLAYBACK: Duration = Duration::from_millis(1500);
    let mut tty = Tty::new(1500);
    tty.until("voice=ready");

    // The control. Nothing touches this one, so the paced sink spends the whole
    // utterance length before the session reports playback closed.
    tty.send("/say a phrase nobody interrupts");
    let first = tty.utterance();
    let (lines, control) = tty.until("utterance=- ");
    assert!(
        control >= PLAYBACK / 2,
        "uninterrupted playback closed after {control:?}: {lines:?}"
    );

    // The interrupt. Same length, stopped a fraction of the way in.
    tty.send("/say a phrase stopped mid playback");
    let second = tty.utterance();
    assert_ne!(second, first, "each utterance gets its own id");
    let started = Instant::now();
    tty.send("/stop");
    let (requested, _) = tty.until("speech interruption requested; task unchanged");
    let (settled, elapsed) = tty.until("utterance=- ");
    assert!(
        started.elapsed() < PLAYBACK / 3,
        "playback had closed {started:?} before /stop was even answered: {requested:?}"
    );
    assert!(
        elapsed < control / 3,
        "interrupted playback took {elapsed:?}, the full one took {control:?}: {settled:?}"
    );

    // The stale interrupt: the id of the utterance that already ended must not be able
    // to stop the one now playing, and the newer playback has to survive the attempt.
    tty.send("/say a phrase a stale interrupt cannot stop");
    let third = tty.utterance();
    tty.send(&format!("/stop {second}"));
    assert_eq!(tty.read(), "refused: stale utterance");
    tty.send("/status");
    let (state, _) = tty.until(&format!("utterance={third} "));
    let (_, remaining) = tty.until("utterance=- ");
    assert!(
        remaining >= PLAYBACK / 2,
        "the stale interrupt did not stop the newer utterance: {state:?}"
    );

    // Nothing unprefixed is acted on, and a session with nothing playing says so.
    tty.send("status");
    assert!(tty
        .read()
        .starts_with("refused: only commands are acted on"));
    tty.send("/stop");
    assert_eq!(tty.read(), "refused: nothing is playing to interrupt");
    tty.send("/quit");
    tty.until("ok: shutdown requested");
    drop(tty.input.take());
    assert!(tty.wait().success(), "{}", tty.diagnostic());
}

#[test]
fn tty_playback_interrupts_leave_an_independent_task_running_and_succeeded() {
    let mut tty = Tty::new(1200);
    // The session gets first run on the store. Two JEAN processes starting against a
    // brand-new data directory race each other through the schema migrations, and the
    // loser exits with "table tool_health already exists" (§62, defect 1) — sequencing
    // them here keeps this test measuring playback and tasks rather than that race.
    tty.until("voice=ready");

    let mut command = Command::new(env!("CARGO_BIN_EXE_jean"));
    command.env_clear();
    for key in [
        "SystemRoot",
        "PATH",
        "TEMP",
        "TMP",
        "LOCALAPPDATA",
        "APPDATA",
        "USERPROFILE",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut sleeper = command
        .args([
            "exec",
            "indep.sleeper",
            "--goal",
            "survive a voice playback interrupt",
        ])
        .env("JEAN_DATA_DIR", &tty.dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            std::fs::File::create(tty.dir.join("exec-stderr.log")).unwrap(),
        ))
        .spawn()
        .unwrap();
    // The task's own output is collected on a thread: it arrives across an eight-second
    // sleep, and a blocking read here would hold the test until after it had finished.
    let mut pipe = sleeper.stdout.take().unwrap();
    let collector = std::thread::spawn(move || {
        let mut text = String::new();
        pipe.read_to_string(&mut text).unwrap();
        text
    });

    let started = Instant::now();
    while tty.task_state().as_deref() != Some("running") {
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "the independent task never reached running: {:?}",
            tty.tasks()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    for round in 0..4 {
        tty.send(&format!("/say playback round {round} over a live task"));
        tty.utterance();
        tty.send("/stop");
        let (requested, _) = tty.until("speech interruption requested; task unchanged");
        let (_, elapsed) = tty.until("utterance=- ");
        assert!(
            elapsed < Duration::from_millis(600),
            "round {round} was not cut short: {requested:?}"
        );
    }

    // Interruption reaches playback and nothing else: the task slot this session owns
    // stayed empty, and the task it never owned is still in flight beside it.
    assert_eq!(
        tty.task_state().as_deref(),
        Some("running"),
        "a playback interrupt reached a task"
    );
    tty.send("/status");
    let (state, _) = tty.until("task=- ");
    assert!(
        state.last().unwrap().contains("utterance=-"),
        "the session still owned an utterance it never asked for: {state:?}"
    );

    let outcome = sleeper.wait_with_output().unwrap();
    assert!(
        outcome.status.success(),
        "{}",
        std::fs::read_to_string(tty.dir.join("exec-stderr.log")).unwrap()
    );
    assert!(
        collector
            .join()
            .unwrap()
            .contains("TASK_ALIVE_AFTER_VOICE_INTERRUPT"),
        "the task did not outlive the playback it was running beside"
    );
    let started = Instant::now();
    while tty.task_state().as_deref() != Some("succeeded") {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the task that survived the interrupts did not verify: {:?}",
            tty.tasks()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    tty.send("/quit");
    tty.until("ok: shutdown requested");
    drop(tty.input.take());
    assert!(tty.wait().success(), "{}", tty.diagnostic());
}

/// The session takes the worker the operator names. Neither half of this proves it on
/// its own: a named worker that answers could be the default sibling, which is why the
/// second half points the same variable at a path that is not a file and shows the
/// session going to that one instead of quietly launching its sibling.
#[test]
fn the_session_launches_the_worker_it_was_named_and_refuses_a_path_that_is_not_one() {
    let default_worker = std::path::PathBuf::from(env!("CARGO_BIN_EXE_jean"))
        .parent()
        .unwrap()
        .join("jean-voice.exe");
    let mut tty = Tty::with(400, Some(default_worker.clone()));
    let (lines, _) = tty.until("voice=ready");
    assert!(
        lines.last().unwrap().contains("voice=ready"),
        "{default_worker:?} did not become ready: {lines:?}"
    );
    tty.send("/quit");
    tty.until("ok: shutdown requested");
    drop(tty.input.take());
    assert!(tty.wait().success(), "{}", tty.diagnostic());

    let missing = default_worker.with_file_name("jean-voice-that-does-not-exist.exe");
    assert!(!missing.exists());
    let mut tty = Tty::with(400, Some(missing));
    let status = tty.wait();
    assert!(!status.success(), "a named non-worker must not start");
    assert!(
        tty.diagnostic().contains("JEAN_VOICE_WORKER"),
        "the refusal must name the variable the operator set: {}",
        tty.diagnostic()
    );
}
