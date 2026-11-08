use std::io::{self, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::Duration;

use clap::Subcommand;
use jean_core::interaction::{cancel_io_thread, ClientOutput, Controller, Settings};
use jean_core::Context;
use jean_protocol::config::Hotkey;
use jean_protocol::interaction::{self as wire, Envelope, OwnerCommand, Snapshot};

/// The longest tty line that gets kept. Anything past it is drained and refused, so
/// a pasted wall of text cannot make the session hold an unbounded buffer.
const MAX_TTY_LINE: usize = 4096;

/// One thing the operator's stdin produced. `Control` is the JSON-lines contract;
/// `Line` is the tty surface, whose command is only resolved once the session's own
/// state is readable, because `/stop` without an id means "the utterance playing now".
enum Input {
    Control(Envelope<OwnerCommand>),
    Line(String),
}

/// What a tty line turns out to mean. Nothing here reaches the controller unless it is
/// `Send`, so an unprefixed or unknown line is refused rather than executed.
enum Typed {
    Send(OwnerCommand),
    Refused(String),
    Note(String),
    Quiet,
}

pub fn interact(ctx: &mut Context, tty: bool) -> anyhow::Result<ExitCode> {
    ctx.prepare_for_writing()?;
    let mut controller = Controller::new(Settings::from_context(ctx)?)?;
    // The JSON client writes framed snapshots to stdout; the tty surface owns stdout
    // instead, so exactly one of them can exist.
    let mut output = if tty {
        None
    } else {
        Some(ClientOutput::new()?)
    };
    let token = jean_core::signals::owner_token()?;
    let (tx, rx) = mpsc::sync_channel(wire::QUEUE_CAPACITY);
    let input_failed = Arc::new(AtomicBool::new(false));
    let failed = input_failed.clone();
    let reader = std::thread::Builder::new()
        .name("jean-owner-input".into())
        .spawn(move || {
            let mut input = BufReader::new(io::stdin());
            loop {
                if tty {
                    match input.read_line_bounded(MAX_TTY_LINE) {
                        Ok(Some(text)) => {
                            if tx.try_send(Input::Line(text)).is_err() {
                                failed.store(true, Ordering::Release);
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(_) => {
                            failed.store(true, Ordering::Release);
                            break;
                        }
                    }
                    continue;
                }
                match wire::read::<Envelope<OwnerCommand>>(&mut input) {
                    Ok(Some(message)) => {
                        if tx.try_send(Input::Control(message)).is_err() {
                            failed.store(true, Ordering::Release);
                            break;
                        }
                    }
                    Ok(None) => break,
                    _ => {
                        failed.store(true, Ordering::Release);
                        break;
                    }
                }
            }
        })?;
    let result = (|| -> anyhow::Result<()> {
        let mut seen: Option<Snapshot> = None;
        let mut next = 0u64;
        match &mut output {
            Some(client) => client.send(wire::encode(&controller.snapshot())?)?,
            None => {
                println!(
                    "JEAN session {} — /help lists the commands; plain text is never a command.",
                    controller.snapshot().session
                );
                print_state(&controller.snapshot());
                seen = Some(controller.snapshot());
            }
        }
        loop {
            if input_failed.load(Ordering::Acquire) {
                anyhow::bail!("control input malformed, oversized, or overloaded");
            }
            if let Some(client) = &mut output {
                client.poll()?;
            }
            if token.is_cancelled() {
                controller.shutdown()?;
                break;
            }
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(Input::Control(request)) => {
                    let id = request.request;
                    let reply = controller.handle(request);
                    if let Some(client) = &mut output {
                        client.send(wire::encode(&Envelope::new(
                            reply.snapshot.session.clone(),
                            id,
                            wire::now_ms() + 2000,
                            reply,
                        ))?)?;
                    }
                }
                Ok(Input::Line(line)) => {
                    let utterance = controller.snapshot().utterance;
                    match tty_line(&line, utterance) {
                        Typed::Send(command) => {
                            let wants_state = matches!(command, OwnerCommand::Status);
                            let reply = owner_request(&mut controller, &mut next, command);
                            print_line(
                                if reply.accepted { "ok" } else { "refused" },
                                &reply.detail,
                            );
                            if wants_state {
                                print_state(&reply.snapshot);
                            }
                            seen = Some(reply.snapshot);
                        }
                        Typed::Refused(detail) => print_line("refused", &detail),
                        Typed::Note(text) => {
                            for part in text.lines() {
                                print_line("JEAN", part);
                            }
                        }
                        Typed::Quiet => {}
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    controller.tick();
                    if output.is_none() {
                        // A tty operator needs to see an utterance settle without typing,
                        // which is how an interruption looks from the keyboard.
                        let snapshot = controller.snapshot();
                        if !seen.as_ref().is_some_and(|p| same_state(p, &snapshot)) {
                            print_state(&snapshot);
                            seen = Some(snapshot);
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    controller.shutdown()?;
                    break;
                }
            }
            if controller.is_stopped() {
                break;
            }
        }
        if input_failed.load(Ordering::Acquire) {
            anyhow::bail!("control input malformed, oversized, or overloaded");
        }
        Ok(())
    })();
    let shutdown = controller.shutdown();
    cancel_io_thread(reader);
    let flushed = match &mut output {
        Some(client) => client.finish(),
        None => Ok(()),
    };
    jean_core::signals::disarm();
    let reconciled = jean_core::reconcile(&ctx.store);
    result?;
    shutdown?;
    flushed?;
    reconciled?;
    Ok(ExitCode::SUCCESS)
}

/// A tty line, already stripped, as the session reads it.
fn tty_line(line: &str, utterance: Option<u64>) -> Typed {
    let line = line.trim_end_matches(['\r', '\n']);
    let (verb, rest) = match line.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (line, ""),
    };
    if !line.starts_with('/') {
        return if line.trim().is_empty() {
            Typed::Quiet
        } else {
            Typed::Refused(
                "only commands are acted on; /help lists them and /do starts a task".into(),
            )
        };
    }
    match verb {
        "/help" => Typed::Note(HELP.to_owned()),
        "/status" => Typed::Send(OwnerCommand::Status),
        "/say" => Typed::Send(OwnerCommand::Speak { text: rest.into() }),
        // The id check stays in the controller: an id that is not the playing
        // utterance is refused there, and `/stop` with no id takes the one this
        // session actually owns instead of inventing a number.
        "/stop" => match (rest, utterance) {
            ("", Some(id)) => Typed::Send(OwnerCommand::Interrupt { utterance: id }),
            ("", None) => Typed::Refused("nothing is playing to interrupt".into()),
            (id, _) => match id.parse::<u64>() {
                Ok(utterance) => Typed::Send(OwnerCommand::Interrupt { utterance }),
                Err(_) => Typed::Refused("usage: /stop [utterance]".into()),
            },
        },
        "/do" => Typed::Send(OwnerCommand::Submit { goal: rest.into() }),
        "/cancel" if rest.is_empty() => Typed::Refused("usage: /cancel <task-id>".into()),
        "/cancel" => Typed::Send(OwnerCommand::CancelTask {
            task_id: rest
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned(),
        }),
        "/voice" => match rest {
            "on" => Typed::Send(OwnerCommand::StartVoice),
            "off" => Typed::Send(OwnerCommand::StopVoice),
            _ => Typed::Refused("usage: /voice on|off".into()),
        },
        "/quit" => Typed::Send(OwnerCommand::Shutdown),
        _ => Typed::Refused(format!("unknown command {verb}; /help lists them")),
    }
}

const HELP: &str = "\
/say <text>      speak a phrase through the voice worker
/stop [utterance] interrupt playback; without an id, the utterance this session owns
/status          report voice, playback, task and shutdown state
/do <goal>       submit a task goal
/cancel <task-id> cancel a task this session owns
/voice on|off    start or stop the voice worker
/quit            shut the session down
An unprefixed line is never acted on, and interruption reaches playback only: it \
never cancels a task.";

fn print_line(kind: &str, text: &str) {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{kind}: {text}");
    let _ = stdout.flush();
}

fn print_state(snapshot: &Snapshot) {
    let task = match &snapshot.task {
        Some(task) => format!("{} {}", task.id, task.state),
        None => "-".into(),
    };
    print_line(
        "state",
        &format!(
            "voice={} utterance={} activation={} task={} shutting_down={} rejected_events={}",
            snapshot.voice,
            snapshot
                .utterance
                .map(|u| u.to_string())
                .unwrap_or_else(|| "-".into()),
            snapshot
                .activation
                .map(|a| a.to_string())
                .unwrap_or_else(|| "-".into()),
            task,
            snapshot.shutting_down,
            snapshot.rejected_events
        ),
    );
}

/// The fields the operator sees, so an idle tick only prints when one of them moves.
fn same_state(previous: &Snapshot, current: &Snapshot) -> bool {
    let task = |s: &Snapshot| {
        s.task
            .as_ref()
            .map(|t| (t.id.clone(), t.state, t.cancelling))
    };
    previous.voice == current.voice
        && previous.utterance == current.utterance
        && previous.activation == current.activation
        && task(previous) == task(current)
        && previous.shutting_down == current.shutting_down
        && previous.rejected_events == current.rejected_events
}

/// Read one line, keeping at most `limit` bytes of it. The remainder of an oversized
/// line is drained so the next read starts at a real line boundary.
trait BoundedRead {
    fn read_line_bounded(&mut self, limit: usize) -> io::Result<Option<String>>;
}

impl<R: Read> BoundedRead for BufReader<R> {
    fn read_line_bounded(&mut self, limit: usize) -> io::Result<Option<String>> {
        let mut line = String::new();
        loop {
            let byte = {
                let mut buffer = [0u8; 1];
                if self.read(&mut buffer)? == 0 {
                    if line.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(line));
                }
                buffer[0]
            };
            if byte == b'\n' {
                return Ok(Some(line));
            }
            if line.len() < limit {
                line.push(char::from(byte));
            }
        }
    }
}

#[derive(Subcommand)]
pub enum VoiceAction {
    /// List audio endpoints. Read-only: opens no microphone and no speaker.
    Devices,
    /// Report what this voice build can do and which speech assets are missing.
    Doctor,
    /// Hold the configured hotkey to talk; release to submit the transcript as a task.
    Ptt,
    /// Recognise a prerecorded WAV through the configured engine. No device is
    /// opened, which is what makes this the microphone-free way to test a bundle.
    Transcribe {
        /// Path to a PCM or 32-bit-float WAV file.
        clip: PathBuf,
    },
    /// Synthesize a phrase into a WAV file. No speaker is opened, so the file is
    /// for the operator to play; the report says whether it contains sound.
    Synthesize {
        /// Where to write the audio.
        to: PathBuf,
        /// The exact words to speak.
        text: String,

        /// Treat the phrase as phonemes from the bundle's own `tokens.txt` instead
        /// of words. sherpa-onnx 2.0.1 needs a lexicon file to turn words into
        /// phonemes, and the English Kokoro bundle does not ship one; this renders
        /// the acoustic model without it, so a synthesis run is still possible to
        /// measure. It changes nothing about what `jean voice ptt` says back.
        #[arg(long)]
        phonemes: bool,
    },
}

pub fn voice(ctx: &mut Context, action: VoiceAction) -> anyhow::Result<ExitCode> {
    let config = || serde_json::to_string(&ctx.config.voice);
    match action {
        VoiceAction::Devices => probe(&["--list-devices"], None),
        VoiceAction::Doctor => probe(&["--doctor"], Some(config()?)),
        VoiceAction::Ptt => ptt(ctx),
        VoiceAction::Transcribe { clip } => {
            let clip = clip
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("the clip path is not valid text"))?
                .to_owned();
            probe(&["--transcribe", &clip], Some(config()?))
        }
        VoiceAction::Synthesize { to, text, phonemes } => {
            let to = to
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("the output path is not valid text"))?
                .to_owned();
            let mut args = vec!["--synthesize", &to, &text];
            if phonemes {
                args.push("--phonemes");
            }
            probe(&args, Some(config()?))
        }
    }
}

fn worker_exe() -> anyhow::Result<PathBuf> {
    // The sherpa worker is a different binary from the default one: enabling the
    // feature is a build-scoped act, and any `cargo build --workspace` rebuilds the
    // sibling of `jean.exe` without it. Naming a worker here is how a provisioned
    // native build stays runnable next to the default one without a build being able
    // to swap it out underneath the operator.
    if let Some(path) = std::env::var_os("JEAN_VOICE_WORKER") {
        let path = PathBuf::from(path);
        if !path.is_file() {
            anyhow::bail!(
                "JEAN_VOICE_WORKER names `{}`, which is not a file",
                path.display()
            );
        }
        return Ok(path);
    }
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("the jean executable has no directory"))?;
    Ok(dir.join("jean-voice.exe"))
}

/// Run the voice worker in a probe mode and pass its output through verbatim.
/// The audio code stays in that process; the CLI never links it.
fn probe(args: &[&str], stdin_data: Option<String>) -> anyhow::Result<ExitCode> {
    let exe = worker_exe()?;
    if !exe.is_file() {
        anyhow::bail!(
            "the voice worker was not built alongside jean; run cargo build -p jean-voice"
        );
    }
    let mut command = std::process::Command::new(exe);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin_data.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn()?;
    if let Some(data) = stdin_data {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(data.as_bytes())?;
            stdin.write_all(b"\n")?;
        }
    }
    let output = child.wait_with_output()?;
    io::stdout().write_all(&output.stdout)?;
    io::stderr().write_all(&output.stderr)?;
    Ok(if output.status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn owner_request(
    controller: &mut Controller,
    next: &mut u64,
    command: OwnerCommand,
) -> wire::OwnerReply {
    *next += 1;
    controller.handle(Envelope::new(
        controller.snapshot().session,
        *next,
        wire::now_ms() + 5000,
        command,
    ))
}

fn ptt(ctx: &mut Context) -> anyhow::Result<ExitCode> {
    if !ctx.config.voice.enabled {
        anyhow::bail!("voice is disabled; set [voice] enabled = true to use push-to-talk");
    }
    ctx.prepare_for_writing()?;
    let hotkey = ctx.config.voice.hotkey();
    let binding = ctx.config.voice.hotkey.clone();
    let mut controller = Controller::new(Settings::from_context(ctx)?)?;
    let token = jean_core::signals::owner_token()?;
    let mut next = 0u64;

    let until = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        controller.tick();
        let snapshot = controller.snapshot();
        if snapshot.voice == "ready" {
            break;
        }
        if snapshot.voice.starts_with("unavailable") || snapshot.voice.starts_with("worker") {
            anyhow::bail!("voice did not become ready: {}", snapshot.voice);
        }
        anyhow::ensure!(std::time::Instant::now() < until, "voice startup timed out");
        std::thread::sleep(Duration::from_millis(20));
    }

    eprintln!("push-to-talk: hold {binding} to talk, release to submit; Ctrl-C quits");
    let mut held = false;
    let mut activation: Option<u64> = None;
    while !token.is_cancelled() {
        controller.tick();
        let down = hotkey_down(&hotkey);
        if down && !held {
            held = true;
            let reply = owner_request(&mut controller, &mut next, OwnerCommand::BeginCapture);
            if reply.accepted {
                activation = reply.snapshot.activation;
                eprintln!("listening…");
            } else {
                eprintln!("capture refused: {}", reply.detail);
            }
        } else if !down && held {
            held = false;
            if let Some(id) = activation.take() {
                let reply = owner_request(
                    &mut controller,
                    &mut next,
                    OwnerCommand::EndCapture { activation: id },
                );
                if !reply.accepted {
                    eprintln!("release refused: {}", reply.detail);
                    continue;
                }
                let snapshot = wait_transcript(&mut controller, &mut next, id);
                if snapshot.transcript_ready {
                    let reply = owner_request(
                        &mut controller,
                        &mut next,
                        OwnerCommand::SubmitTranscript { activation: id },
                    );
                    eprintln!("{}", reply.detail);
                } else {
                    eprintln!("no transcript was finalized for activation {id}");
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    controller.shutdown()?;
    jean_core::signals::disarm();
    Ok(ExitCode::SUCCESS)
}

fn wait_transcript(controller: &mut Controller, next: &mut u64, activation: u64) -> Snapshot {
    let until = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let reply = owner_request(controller, next, OwnerCommand::Status);
        if reply.snapshot.transcript_ready || reply.snapshot.activation != Some(activation) {
            return reply.snapshot;
        }
        if std::time::Instant::now() >= until {
            return reply.snapshot;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll only the keys in the binding. This reads keyboard state for the specific
/// hotkey, not a keylogger: no other key is inspected and nothing is recorded.
#[cfg(windows)]
fn hotkey_down(hotkey: &Hotkey) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    fn down(vk: i32) -> bool {
        (unsafe { GetAsyncKeyState(vk) } as u16) & 0x8000 != 0
    }
    let win = down(0x5B) || down(0x5C);
    down(hotkey.virtual_key() as i32)
        && (!hotkey.ctrl || down(0x11))
        && (!hotkey.alt || down(0x12))
        && (!hotkey.shift || down(0x10))
        && (!hotkey.win || win)
}

#[cfg(not(windows))]
fn hotkey_down(_hotkey: &Hotkey) -> bool {
    false
}
