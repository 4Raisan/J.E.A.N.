//! Native speech driven through the **supervised worker process**.
//!
//! `speech_path.rs` proves the capture, buffer and cancellation mechanics against
//! scripted endpoints; `cli_voice.rs` proves the CLI hands the right configuration
//! to the right binary. Neither has ever met a model. These tests do: they spawn a
//! worker built with `--features sherpa`, feed it a prerecorded WAV through the
//! `voice.offline_wav` seam so no endpoint is opened in either direction, and drive
//! it with the same wire messages an owner sends — `Hello`, `ConfigureVoice`,
//! `BeginCapture`, `EndCapture`, `Ping`, `Speak`, `Shutdown`.
//!
//! Timings measured here are **native** measurements and are kept separate from the
//! fixture timings quoted elsewhere in the handoff.
//!
//! Skipped, with the reason printed, unless all three are true: the crate was built
//! with `--features sherpa`, `JEAN_NATIVE_WORKER_EXE` names an existing executable,
//! and the ASR bundle is in `%LOCALAPPDATA%\JEAN\models`. That is deliberate — the
//! default `cargo test` run must not need a 122 MB library set or 500 MB of weights.

#![cfg(feature = "sherpa")]

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use jean_protocol::config::{SpeechBackend, SpeechModels, VoiceConfig};
use jean_protocol::interaction::{self as wire, Envelope, Role, WorkerCommand, WorkerEvent};

const BUNDLE: &str = "sherpa-onnx-streaming-zipformer-en-2023-06-21";
/// Ground truth shipped inside the bundle, in its own `test_wavs/trans.txt`.
const CLIP_0_EXPECTED: &str = "AFTER EARLY NIGHTFALL THE YELLOW LAMPS WOULD LIGHT UP HERE AND THERE THE SQUALID QUARTER OF THE BROTHELS";
/// `0.wav` is 6.625 s and the offline seam delivers it at 1×, so a press shorter than
/// this has genuinely not heard the end of the clip.
const CLIP_0_HOLD: Duration = Duration::from_millis(7_000);

fn bundle_dir() -> Option<PathBuf> {
    let root = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    let dir = root.join("JEAN").join("models").join(BUNDLE);
    let complete = [
        "encoder-epoch-99-avg-1.int8.onnx",
        "decoder-epoch-99-avg-1.onnx",
        "joiner-epoch-99-avg-1.int8.onnx",
        "tokens.txt",
    ]
    .into_iter()
    .all(|name| dir.join(name).is_file());
    complete.then_some(dir)
}

/// A worker configured for one offline clip, with or without a synthesis bundle.
fn config(clip: &Path, capture_timeout_ms: u64, with_synthesis: bool) -> Option<VoiceConfig> {
    let dir = bundle_dir()?;
    let text = |name: &str| dir.join(name).display().to_string();
    let mut models = SpeechModels {
        encoder: text("encoder-epoch-99-avg-1.int8.onnx"),
        decoder: text("decoder-epoch-99-avg-1.onnx"),
        joiner: text("joiner-epoch-99-avg-1.int8.onnx"),
        tokens: text("tokens.txt"),
        ..SpeechModels::default()
    };
    if with_synthesis {
        let tts = dir
            .parent()
            .map(|p| p.join("tts").join("kokoro-en-v0_19"))
            .filter(|p| p.is_dir())?;
        models.tts_model = tts.join("model.onnx").display().to_string();
        models.tts_tokens = tts.join("tokens.txt").display().to_string();
        models.tts_voices = tts.join("voices.bin").display().to_string();
    }
    Some(VoiceConfig {
        enabled: true,
        mode: "push_to_talk".into(),
        backend: SpeechBackend::SherpaOnnx,
        sample_rate: 16000,
        capture_timeout_ms,
        finalize_budget_ms: 1200,
        offline_wav: clip.display().to_string(),
        models,
        ..VoiceConfig::default()
    })
}

struct Harness {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<Envelope<WorkerEvent>>,
    session: String,
    next: u64,
    exe: PathBuf,
}

impl Harness {
    /// `None` with a printed reason when this machine cannot run a native test.
    fn open(config: VoiceConfig, configure_budget: Duration) -> Option<Self> {
        let Some(exe) = std::env::var_os("JEAN_NATIVE_WORKER_EXE").map(PathBuf::from) else {
            eprintln!("SKIP: JEAN_NATIVE_WORKER_EXE is unset, so no native worker was named");
            return None;
        };
        if !exe.is_file() {
            eprintln!("SKIP: `{}` is not a file", exe.display());
            return None;
        }
        let session = format!("native-test-{}", std::process::id());
        let mut child = Command::new(&exe)
            .arg("--session")
            .arg(&session)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited on purpose: a native worker's own warnings — dropped frames,
            // an engine deprecation notice — are the difference between a diagnosis
            // and a mystery when a press comes back empty.
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the native worker must start");
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let (tx, events) = mpsc::channel();
        std::thread::Builder::new()
            .name("native-test-reader".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                while let Ok(Some(envelope)) = wire::read::<Envelope<WorkerEvent>>(&mut reader) {
                    if tx.send(envelope).is_err() {
                        break;
                    }
                }
            })
            .expect("reader thread");

        let mut harness = Self {
            child,
            stdin,
            events,
            session,
            next: 0,
            exe,
        };
        harness.ask(WorkerCommand::Hello { role: Role::Voice });
        harness.expect_reply_contains(configure_budget, &["ready"], "handshake");
        harness.ask(WorkerCommand::ConfigureVoice {
            config: Box::new(config),
        });
        let started = Instant::now();
        harness.expect_reply_contains(configure_budget, &["configured"], "model loading")?;
        eprintln!(
            "native worker `{}` configured in {} ms",
            harness.exe.display(),
            started.elapsed().as_millis()
        );
        Some(harness)
    }

    fn ask(&mut self, body: WorkerCommand) -> u64 {
        self.next += 1;
        let request = self.next;
        let envelope = Envelope::new(
            self.session.clone(),
            request,
            wire::now_ms() + 120_000,
            body,
        );
        wire::write(&mut self.stdin, &envelope).expect("the worker's stdin is open");
        request
    }

    /// The next event for `request`, or panic with everything else that arrived.
    fn reply(&mut self, request: u64, timeout: Duration, what: &str) -> WorkerEvent {
        let until = Instant::now() + timeout;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left.max(Duration::from_millis(1))) {
                Ok(envelope) if envelope.request == request => return envelope.body,
                Ok(other) => eprintln!(
                    "(ignoring event for request {}: {:?})",
                    other.request, other.body
                ),
                Err(RecvTimeoutError::Timeout) => {
                    panic!("no reply to {what} (request {request}) within {timeout:?}")
                }
                Err(RecvTimeoutError::Disconnected) => panic!("the worker exited during {what}"),
            }
        }
    }

    fn expect_reply_contains(
        &mut self,
        timeout: Duration,
        names: &[&str],
        what: &str,
    ) -> Option<()> {
        let until = Instant::now() + timeout;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left.max(Duration::from_millis(1))) {
                Ok(envelope) => {
                    let name = serde_json::to_value(&envelope.body)
                        .ok()
                        .and_then(|v| v.get("event").and_then(|e| e.as_str().map(str::to_owned)))
                        .unwrap_or_default();
                    if names.contains(&name.as_str()) {
                        return Some(());
                    }
                    eprintln!("(unexpected event during {what}: {name})");
                }
                Err(RecvTimeoutError::Timeout) => {
                    eprintln!(
                        "SKIP: no {} during {what} within {timeout:?}",
                        name_placeholder(names)
                    );
                    return None;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    eprintln!("SKIP: the worker exited during {what}");
                    return None;
                }
            }
        }
    }

    /// Fails if a Transcript for any activation arrives within `window`.
    fn assert_silent(&mut self, window: Duration, context: &str) {
        let until = Instant::now() + window;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.events.recv_timeout(left) {
                Ok(envelope) => {
                    if let WorkerEvent::Transcript { text, .. } = envelope.body {
                        panic!("{context}: a cancelled press still delivered `{text}`");
                    }
                }
                Err(RecvTimeoutError::Timeout) => return,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn shutdown(mut self) {
        let request = self.ask(WorkerCommand::Shutdown);
        let _ = self.reply(request, Duration::from_secs(5), "shutdown");
        let _ = self.child.wait();
    }
}

fn name_placeholder(names: &[&str]) -> String {
    names.join(" or ")
}

fn clip_0() -> Option<PathBuf> {
    Some(bundle_dir()?.join("test_wavs").join("0.wav"))
}

fn clip_1() -> Option<PathBuf> {
    Some(bundle_dir()?.join("test_wavs").join("1.wav"))
}

/// One real press: the worker recognises a known clip, answers pings while the
/// native decode runs, and does it again on a second press.
#[test]
fn a_supervised_native_press_transcribes_a_known_clip_and_stays_answerable() {
    let Some(clip) = clip_0() else {
        eprintln!("SKIP: the ASR bundle is not provisioned");
        return;
    };
    let Some(config) = config(&clip, 30_000, false) else {
        eprintln!("SKIP: the ASR bundle is not provisioned");
        return;
    };
    let mut harness = match Harness::open(config, Duration::from_secs(60)) {
        Some(harness) => harness,
        None => return,
    };

    let begin = harness.ask(WorkerCommand::BeginCapture { activation: 1 });
    match harness.reply(begin, Duration::from_secs(10), "BeginCapture") {
        WorkerEvent::Capturing { activation: 1 } => {}
        other => panic!("expected Capturing, got {other:?}"),
    }

    // The clip is delivered at 1× real time, so until this hold expires the
    // recognition thread is feeding a live native decode the whole way through. The
    // control loop has to keep answering across all of it.
    let mut slowest = Duration::ZERO;
    let mut pings = 0usize;
    let pressed = Instant::now();
    while pressed.elapsed() < CLIP_0_HOLD {
        let started = Instant::now();
        let ping = harness.ask(WorkerCommand::Ping);
        match harness.reply(ping, Duration::from_secs(5), "Ping") {
            WorkerEvent::Pong => slowest = slowest.max(started.elapsed()),
            other => panic!("expected Pong, got {other:?}"),
        }
        pings += 1;
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!(
        "{pings} pings answered during a {} ms native press, slowest {} ms",
        pressed.elapsed().as_millis(),
        slowest.as_millis()
    );
    assert!(
        slowest < Duration::from_millis(500),
        "slowest native-decode ping was {} ms",
        slowest.as_millis()
    );

    let released = Instant::now();
    let end = harness.ask(WorkerCommand::EndCapture { activation: 1 });
    let transcript = match harness.reply(end, Duration::from_secs(10), "EndCapture") {
        WorkerEvent::Transcript {
            activation: 1,
            text,
        } => text,
        other => panic!("expected a Transcript, got {other:?}"),
    };
    let elapsed = released.elapsed();
    eprintln!(
        "native release->transcript {} ms, {} bytes of text",
        elapsed.as_millis(),
        transcript.len()
    );
    assert_eq!(
        transcript.trim(),
        CLIP_0_EXPECTED,
        "the bundle's own ground truth did not come back"
    );

    // A second press on the same worker: the stream is rebuilt and the engine has to
    // hear the clip again, at 1×, before the release can produce it.
    let begin = harness.ask(WorkerCommand::BeginCapture { activation: 2 });
    let _ = harness.reply(begin, Duration::from_secs(10), "second BeginCapture");
    let pressed = Instant::now();
    while pressed.elapsed() < CLIP_0_HOLD {
        std::thread::sleep(Duration::from_millis(100));
    }
    let end = harness.ask(WorkerCommand::EndCapture { activation: 2 });
    match harness.reply(end, Duration::from_secs(10), "second EndCapture") {
        WorkerEvent::Transcript { text, .. } => assert_eq!(text.trim(), CLIP_0_EXPECTED),
        other => panic!("the worker stopped recognising after one press: {other:?}"),
    }

    harness.shutdown();
}

/// A press whose deadline expires **during** native decoding: the utterance scope
/// closes, no transcript is delivered, and the worker is still there afterwards.
#[test]
fn an_expired_press_interrupts_native_decode_without_delivering_a_transcript() {
    let Some(clip) = clip_1() else {
        eprintln!("SKIP: the ASR bundle is not provisioned");
        return;
    };
    // 16.7 s of audio against a 100 ms press: the cancel lands inside the decode.
    let Some(config) = config(&clip, 100, false) else {
        eprintln!("SKIP: the ASR bundle is not provisioned");
        return;
    };
    let mut harness = match Harness::open(config, Duration::from_secs(60)) {
        Some(harness) => harness,
        None => return,
    };

    let started = Instant::now();
    let begin = harness.ask(WorkerCommand::BeginCapture { activation: 1 });
    let opening = harness.reply(begin, Duration::from_secs(10), "BeginCapture");
    eprintln!(
        "press opened after {} ms with {opening:?}",
        started.elapsed().as_millis()
    );
    // Opening is answered as soon as the capture exists, so the press deadline is what
    // closes this one — and with a 1× file still streaming, the cancel lands while the
    // native decoder is mid-utterance rather than after it.
    std::thread::sleep(Duration::from_millis(250));
    let end = harness.ask(WorkerCommand::EndCapture { activation: 1 });
    match harness.reply(end, Duration::from_secs(5), "EndCapture after expiry") {
        WorkerEvent::Refused { reason } => eprintln!("expired release refused: {reason}"),
        other => panic!("an expired press must not complete normally: {other:?}"),
    }
    harness.assert_silent(Duration::from_millis(500), "after the press expired");

    let ping = harness.ask(WorkerCommand::Ping);
    match harness.reply(ping, Duration::from_secs(2), "Ping after cancellation") {
        WorkerEvent::Pong => {}
        other => panic!("the control loop stopped answering after a cancellation: {other:?}"),
    }
    harness.shutdown();
}

/// Recognition-only provisioning is a working listener. A synthesis bundle that is
/// absent must refuse one command, not the session.
#[test]
fn a_missing_synthesis_bundle_refuses_speech_but_not_listening() {
    let Some(clip) = clip_0() else {
        eprintln!("SKIP: the ASR bundle is not provisioned");
        return;
    };
    let Some(config) = config(&clip, 30_000, false) else {
        eprintln!("SKIP: the ASR bundle is not provisioned");
        return;
    };
    let mut harness = match Harness::open(config, Duration::from_secs(60)) {
        Some(harness) => harness,
        None => return,
    };

    let speak = harness.ask(WorkerCommand::Speak {
        utterance: 1,
        text: "this should be refused".into(),
    });
    let reason = match harness.reply(speak, Duration::from_secs(5), "Speak") {
        WorkerEvent::Refused { reason } => reason,
        other => panic!("a worker with no synthesis bundle must refuse Speak, got {other:?}"),
    };
    assert!(
        reason.contains("voice.models.tts_model") && reason.contains("Recognition is unaffected"),
        "the refusal has to name the keys and say what still works: {reason}"
    );

    let begin = harness.ask(WorkerCommand::BeginCapture { activation: 1 });
    let _ = harness.reply(begin, Duration::from_secs(10), "BeginCapture");
    let pressed = Instant::now();
    while pressed.elapsed() < CLIP_0_HOLD {
        std::thread::sleep(Duration::from_millis(100));
    }
    let end = harness.ask(WorkerCommand::EndCapture { activation: 1 });
    match harness.reply(end, Duration::from_secs(10), "EndCapture") {
        WorkerEvent::Transcript { text, .. } => assert_eq!(text.trim(), CLIP_0_EXPECTED),
        other => panic!("a missing voice bundle must not stop recognition: {other:?}"),
    }
    harness.shutdown();
}
