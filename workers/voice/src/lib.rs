//! The voice worker: an independently supervised process whose control loop
//! never runs inference.
//!
//! Recognition and synthesis each own a thread (see `runtime`); this loop sends
//! requests, drains events and answers `Ping`/`Interrupt`/`Shutdown` while a long
//! decode or a long synthesis is in flight. Cancellation stays in four separate
//! scopes: an utterance (`Interrupt`), a capture activation (release/deadline),
//! this worker (`Shutdown`) and the application above it. None of them reaches
//! the task worker.

use std::io::{self, BufReader};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use jean_protocol::config::{AudioBackend as AudioBackendKind, SpeechBackend, VoiceConfig};
use jean_protocol::interaction::{
    self as wire, Envelope, RequestGate, Role, WorkerCommand as Command, WorkerEvent as Event,
};

pub mod assets;
pub mod audio;
pub mod device;
pub mod runtime;
#[cfg(feature = "sherpa")]
mod sherpa;
pub mod speech;
#[cfg(windows)]
mod wasapi;
pub mod wav;

use audio::{PttState, PushToTalk};
use device::{AudioBackend, CancelFlag, FileAudio, FixtureAudio};
use runtime::{RecognitionEvent, RecognitionRequest, SynthesisEvent, SynthesisRequest};
use speech::EngineFactory;

pub use audio::Pcm;

struct Speaking {
    utterance: u64,
    request: u64,
    cancel: CancelFlag,
    until: Instant,
}

fn engine_factory(config: &VoiceConfig) -> Result<Arc<dyn EngineFactory>, String> {
    match config.backend {
        SpeechBackend::Fixture => Ok(Arc::new(speech::FixtureEngine::from_config(config))),
        SpeechBackend::SherpaOnnx => {
            #[cfg(feature = "sherpa")]
            {
                Ok(Arc::new(sherpa::SherpaEngine::new(config)?))
            }
            #[cfg(not(feature = "sherpa"))]
            {
                Err(assets::native_unavailable(config, false))
            }
        }
    }
}

fn audio_backend(config: &VoiceConfig) -> Arc<dyn AudioBackend> {
    // Checked before the backend pair, because it outranks both: a file is the
    // microphone and the speaker is a counter, so no endpoint is opened either way.
    if !config.offline_wav.trim().is_empty() {
        return Arc::new(FileAudio::new(
            config.offline_wav.trim(),
            config.sample_rate,
        ));
    }
    match config.audio_backend() {
        AudioBackendKind::Fixture => Arc::new(FixtureAudio::default()),
        AudioBackendKind::System => system_backend(),
    }
}

#[cfg(windows)]
fn system_backend() -> Arc<dyn AudioBackend> {
    Arc::new(wasapi::WasapiBackend)
}

#[cfg(not(windows))]
fn system_backend() -> Arc<dyn AudioBackend> {
    Arc::new(device::UnsupportedAudio)
}

pub fn serve(session: String) -> io::Result<()> {
    let (tx, rx) = mpsc::sync_channel(wire::QUEUE_CAPACITY);
    std::thread::Builder::new()
        .name("voice-control-input".into())
        .spawn(move || {
            let mut reader = BufReader::new(io::stdin());
            while let Ok(Some(message)) = wire::read::<Envelope<Command>>(&mut reader) {
                if tx.try_send(message).is_err() {
                    break;
                }
            }
        })?;

    let (rec_cmd_tx, rec_cmd_rx) = mpsc::channel::<RecognitionRequest>();
    let (rec_evt_tx, rec_evt_rx) = mpsc::sync_channel::<RecognitionEvent>(wire::QUEUE_CAPACITY);
    let (syn_cmd_tx, syn_cmd_rx) = mpsc::channel::<SynthesisRequest>();
    let (syn_evt_tx, syn_evt_rx) = mpsc::sync_channel::<SynthesisEvent>(wire::QUEUE_CAPACITY);

    let worker = Worker {
        gate: RequestGate::new(session.clone()),
        session,
        out: io::stdout().lock(),
        hello: false,
        config: None,
        ptt: PushToTalk::new(16000, 2000),
        rec_cmd_tx,
        rec_cmd_rx: Some(rec_cmd_rx),
        rec_evt_tx,
        rec_evt_rx,
        syn_cmd_tx,
        syn_cmd_rx: Some(syn_cmd_rx),
        syn_evt_tx,
        syn_evt_rx,
        configuring: None,
        configure_until: Instant::now(),
        recognizer_loaded: false,
        synthesis_unavailable: None,
        engine_ready: false,
        capture_cancel: CancelFlag::new(),
        capture_release: CancelFlag::new(),
        capture_reply: None,
        release_reply: None,
        speaking: None,
        interrupt_reply: None,
        last_utterance: 0,
        last_activation: 0,
        threads: Vec::new(),
    };
    worker.run(rx)
}

struct Worker {
    session: String,
    out: io::StdoutLock<'static>,
    gate: RequestGate,
    hello: bool,
    config: Option<VoiceConfig>,
    ptt: PushToTalk,
    rec_cmd_tx: mpsc::Sender<RecognitionRequest>,
    rec_cmd_rx: Option<mpsc::Receiver<RecognitionRequest>>,
    rec_evt_tx: mpsc::SyncSender<RecognitionEvent>,
    rec_evt_rx: mpsc::Receiver<RecognitionEvent>,
    syn_cmd_tx: mpsc::Sender<SynthesisRequest>,
    syn_cmd_rx: Option<mpsc::Receiver<SynthesisRequest>>,
    syn_evt_tx: mpsc::SyncSender<SynthesisEvent>,
    syn_evt_rx: mpsc::Receiver<SynthesisEvent>,
    configuring: Option<u64>,
    configure_until: Instant,
    recognizer_loaded: bool,
    /// Why this build cannot speak, when it cannot. Recognition does not depend on
    /// it: a machine with only an ASR bundle is a working listener, and `Speak` is
    /// what refuses rather than the whole session.
    synthesis_unavailable: Option<String>,
    engine_ready: bool,
    capture_cancel: CancelFlag,
    capture_release: CancelFlag,
    capture_reply: Option<u64>,
    release_reply: Option<u64>,
    speaking: Option<Speaking>,
    interrupt_reply: Option<u64>,
    last_utterance: u64,
    last_activation: u64,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Worker {
    fn reply(&mut self, request: u64, body: Event) -> io::Result<()> {
        wire::write(
            &mut self.out,
            &Envelope::new(self.session.clone(), request, wire::now_ms() + 2000, body),
        )
    }

    fn refused(&mut self, request: u64, reason: impl Into<String>) -> io::Result<()> {
        self.reply(
            request,
            Event::Refused {
                reason: reason.into(),
            },
        )
    }

    fn run(mut self, rx: mpsc::Receiver<Envelope<Command>>) -> io::Result<()> {
        let result = self.event_loop(rx);
        self.teardown();
        result
    }

    fn event_loop(&mut self, rx: mpsc::Receiver<Envelope<Command>>) -> io::Result<()> {
        loop {
            let now = Instant::now();
            self.drain_recognition()?;
            self.drain_synthesis()?;
            self.expire(now)?;
            let message = match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            };
            if let Err(reason) = self.gate.accept(&message) {
                self.refused(message.request, reason)?;
                continue;
            }
            let id = message.request;
            if !self.hello && !matches!(message.body, Command::Hello { role: Role::Voice }) {
                return Err(io::Error::other("voice handshake required"));
            }
            if self.command(id, message.body)? {
                return Ok(());
            }
        }
    }

    /// Returns true when the worker should exit.
    fn command(&mut self, id: u64, body: Command) -> io::Result<bool> {
        match body {
            Command::Hello { role: Role::Voice } if !self.hello => {
                self.hello = true;
                self.reply(id, Event::Ready { role: Role::Voice })?;
            }
            Command::ConfigureVoice { config } if self.config.is_none() => {
                self.configure(id, *config)?;
            }
            Command::Speak { utterance, text } => self.speak(id, utterance, text)?,
            Command::Interrupt { utterance } => self.interrupt(id, utterance)?,
            Command::BeginCapture { activation } => self.begin_capture(id, activation)?,
            Command::EndCapture { activation } => self.end_capture(id, activation)?,
            Command::Ping => self.reply(id, Event::Pong)?,
            Command::Shutdown => {
                self.reply(id, Event::Bye)?;
                return Ok(true);
            }
            _ => self.refused(id, "operation is not available in the voice worker")?,
        }
        Ok(false)
    }

    fn configure(&mut self, id: u64, config: VoiceConfig) -> io::Result<()> {
        if let Err(error) = config.validate() {
            return self.refused(id, error.to_string());
        }
        if !config.enabled {
            return self.refused(id, "voice is disabled");
        }
        let factory = match engine_factory(&config) {
            Ok(factory) => factory,
            Err(reason) => return self.refused(id, reason),
        };
        let audio = audio_backend(&config);
        self.ptt = PushToTalk::new(config.sample_rate, config.capture_buffer_ms);
        self.configuring = Some(id);
        self.configure_until = Instant::now() + Duration::from_millis(config.configure_timeout_ms);
        self.recognizer_loaded = false;
        self.synthesis_unavailable = None;
        self.engine_ready = false;

        let rec_rx = self.rec_cmd_rx.take().expect("configure happens once");
        let syn_rx = self.syn_cmd_rx.take().expect("configure happens once");
        let recognition = std::thread::Builder::new()
            .name("voice-recognition".into())
            .spawn({
                let config = config.clone();
                let factory = factory.clone();
                let audio = audio.clone();
                let events = self.rec_evt_tx.clone();
                move || {
                    runtime::recognition_thread(config, factory, audio, rec_rx, events);
                }
            });
        let synthesis = std::thread::Builder::new()
            .name("voice-synthesis".into())
            .spawn({
                let config = config.clone();
                let events = self.syn_evt_tx.clone();
                move || {
                    runtime::synthesis_thread(config, factory, audio, syn_rx, events);
                }
            });
        match (recognition, synthesis) {
            (Ok(rec), Ok(syn)) => {
                self.threads.push(rec);
                self.threads.push(syn);
                self.config = Some(config);
                Ok(())
            }
            (rec, syn) => {
                self.configuring = None;
                let reason = match (rec, syn) {
                    (Err(e), _) | (_, Err(e)) => {
                        format!("could not start an inference thread: {e}")
                    }
                    _ => "could not start inference threads".into(),
                };
                self.refused(id, reason)
            }
        }
    }

    fn maybe_configured(&mut self) -> io::Result<()> {
        // Only the recognizer gates configuration. A missing or unusable synthesis
        // bundle is a reason to refuse one command, not a reason to refuse the
        // session: recognition and synthesis are provisioned separately (§59.7), and
        // an operator with only an ASR bundle must still be able to listen.
        if self.configuring.is_some() && self.recognizer_loaded {
            self.engine_ready = true;
            if let Some(request) = self.configuring.take() {
                self.reply(request, Event::Configured)?;
            }
        }
        Ok(())
    }

    fn fail_configuration(&mut self, reason: String) -> io::Result<()> {
        self.engine_ready = false;
        if let Some(request) = self.configuring.take() {
            self.refused(request, reason)?;
        }
        Ok(())
    }

    fn drain_recognition(&mut self) -> io::Result<()> {
        while let Ok(event) = self.rec_evt_rx.try_recv() {
            match event {
                RecognitionEvent::Loaded => {
                    self.recognizer_loaded = true;
                    self.maybe_configured()?;
                }
                RecognitionEvent::LoadFailed { reason } => self.fail_configuration(reason)?,
                RecognitionEvent::Opened { activation } => {
                    if self.ptt.activation() == Some(activation) {
                        // Whether or not release already arrived, the owed answer
                        // to BeginCapture is `Capturing`: the press did happen.
                        let _ = self.ptt.opened(activation);
                        if let Some(request) = self.capture_reply.take() {
                            self.reply(request, Event::Capturing { activation })?;
                        }
                    }
                }
                RecognitionEvent::OpenFailed { activation, reason } => {
                    self.abort_capture(activation, reason)?;
                }
                RecognitionEvent::Transcript {
                    activation,
                    text,
                    dropped,
                } => {
                    if self.ptt.activation() == Some(activation)
                        && self.ptt.state() == PttState::Finalizing
                    {
                        let _ = self.ptt.complete(activation);
                        if dropped > 0 {
                            eprintln!(
                                "voice: dropped {dropped} captured frames; the recognizer fell behind the device"
                            );
                        }
                        if let Some(request) = self.release_reply.take() {
                            self.reply(request, Event::Transcript { activation, text })?;
                        }
                    }
                }
                RecognitionEvent::Failed { activation, reason } => {
                    self.abort_capture(activation, reason)?;
                }
            }
        }
        Ok(())
    }

    fn abort_capture(&mut self, activation: u64, reason: String) -> io::Result<()> {
        if self.ptt.activation() != Some(activation) {
            return Ok(());
        }
        self.ptt.abort();
        self.capture_cancel.cancel();
        // An instant release owes two answers: the press it started and the
        // release it asked for. Leaving either unpaid would expire a pending
        // request and be read as a dead worker rather than a failed device.
        for request in [self.capture_reply.take(), self.release_reply.take()]
            .into_iter()
            .flatten()
            .rev()
        {
            self.refused(request, reason.clone())?;
        }
        Ok(())
    }

    fn drain_synthesis(&mut self) -> io::Result<()> {
        while let Ok(event) = self.syn_evt_rx.try_recv() {
            match event {
                SynthesisEvent::Loaded => {
                    self.synthesis_unavailable = None;
                    self.maybe_configured()?;
                }
                SynthesisEvent::LoadFailed { reason } => {
                    self.synthesis_unavailable = Some(reason);
                    self.maybe_configured()?;
                }
                SynthesisEvent::Stopped { utterance } => {
                    if self
                        .speaking
                        .as_ref()
                        .is_some_and(|s| s.utterance == utterance)
                    {
                        let speaking = self.speaking.take().unwrap();
                        let request = self.interrupt_reply.take().unwrap_or(speaking.request);
                        self.reply(request, Event::SpeechStopped { utterance })?;
                    }
                }
                SynthesisEvent::Failed { utterance, reason } => {
                    if self
                        .speaking
                        .as_ref()
                        .is_some_and(|s| s.utterance == utterance)
                    {
                        let speaking = self.speaking.take().unwrap();
                        let request = self.interrupt_reply.take().unwrap_or(speaking.request);
                        self.refused(request, reason)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn expire(&mut self, now: Instant) -> io::Result<()> {
        if self.configuring.is_some() && now >= self.configure_until {
            self.configuring = None;
            self.engine_ready = false;
            // The reply slot is gone; the owner's own configure deadline retires
            // this worker. Say why on stderr so the cause is not a silent exit.
            eprintln!("voice: model loading exceeded its configure deadline");
        }
        if self.ptt.expired(now) {
            let activation = self.ptt.activation().unwrap_or(0);
            self.abort_capture(activation, "push-to-talk deadline expired".into())?;
        }
        if let Some(speaking) = self.speaking.as_ref() {
            if now >= speaking.until {
                speaking.cancel.cancel();
            }
        }
        Ok(())
    }

    fn speak(&mut self, id: u64, utterance: u64, text: String) -> io::Result<()> {
        if !self.engine_ready {
            return self.refused(id, "voice is not configured");
        }
        if let Some(reason) = self.synthesis_unavailable.clone() {
            // Verbatim: the reason already names the keys and says recognition is
            // unaffected. Wrapping it again would make the refusal harder to act on.
            return self.refused(id, reason);
        }
        if !wire::text_valid(&text) || utterance <= self.last_utterance {
            return self.refused(id, "invalid or stale utterance");
        }
        if self.speaking.is_some() || self.ptt.activation().is_some() {
            return self.refused(id, "voice is busy; interrupt first");
        }
        let timeout = self.config.as_ref().unwrap().utterance_timeout_ms;
        let cancel = CancelFlag::new();
        if self
            .syn_cmd_tx
            .send(SynthesisRequest::Speak {
                utterance,
                text,
                cancel: cancel.clone(),
            })
            .is_err()
        {
            return self.refused(id, "the synthesis thread is no longer running");
        }
        self.last_utterance = utterance;
        self.speaking = Some(Speaking {
            utterance,
            request: id,
            cancel,
            until: Instant::now() + Duration::from_millis(timeout),
        });
        self.reply(id, Event::SpeechStarted { utterance })
    }

    fn interrupt(&mut self, id: u64, utterance: u64) -> io::Result<()> {
        if let Some(speaking) = self.speaking.as_ref() {
            if speaking.utterance == utterance {
                speaking.cancel.cancel();
                self.interrupt_reply = Some(id);
                return Ok(());
            }
        }
        if utterance == self.last_utterance {
            return self.reply(id, Event::SpeechStopped { utterance });
        }
        self.refused(id, "stale utterance interruption")
    }

    fn begin_capture(&mut self, id: u64, activation: u64) -> io::Result<()> {
        if !self.engine_ready {
            return self.refused(id, "voice is not configured");
        }
        if self.speaking.is_some() {
            return self.refused(id, "playback is active; interrupt before push-to-talk");
        }
        let timeout = self.config.as_ref().unwrap().capture_timeout_ms;
        if let Err(error) =
            self.ptt
                .begin(activation, Instant::now(), Duration::from_millis(timeout))
        {
            return self.refused(id, error.to_string());
        }
        self.last_activation = activation;
        self.capture_cancel = CancelFlag::new();
        self.capture_release = CancelFlag::new();
        if self
            .rec_cmd_tx
            .send(RecognitionRequest::Begin {
                activation,
                cancel: self.capture_cancel.clone(),
                release: self.capture_release.clone(),
            })
            .is_err()
        {
            self.ptt.abort();
            return self.refused(id, "the recognition thread is no longer running");
        }
        self.capture_reply = Some(id);
        Ok(())
    }

    fn end_capture(&mut self, id: u64, activation: u64) -> io::Result<()> {
        let budget = self
            .config
            .as_ref()
            .map(|c| c.finalize_budget_ms)
            .unwrap_or(1200);
        match self
            .ptt
            .release(activation, Instant::now(), Duration::from_millis(budget))
        {
            Ok(()) => {
                self.capture_release.cancel();
                self.release_reply = Some(id);
                Ok(())
            }
            Err(error) => self.refused(id, error.to_string()),
        }
    }

    fn teardown(&mut self) {
        self.capture_cancel.cancel();
        self.capture_release.cancel();
        if let Some(speaking) = self.speaking.as_ref() {
            speaking.cancel.cancel();
        }
        let _ = self.rec_cmd_tx.send(RecognitionRequest::Shutdown);
        let _ = self.syn_cmd_tx.send(SynthesisRequest::Shutdown);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// Read-only endpoint enumeration for `jean voice devices`. Opens no capture or
/// render stream, so it never records or plays anything.
pub fn list_devices() -> i32 {
    let backend = system_backend();
    match backend.devices() {
        Ok(devices) => {
            let entries: Vec<serde_json::Value> = devices
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "id": d.id,
                        "name": d.name,
                        "kind": match d.kind {
                            device::DeviceKind::Capture => "capture",
                            device::DeviceKind::Render => "render",
                        },
                        "default": d.default,
                    })
                })
                .collect();
            println!("{}", serde_json::json!({ "devices": entries }));
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

/// Diagnostics for `jean voice doctor`: reads one `VoiceConfig` JSON line on
/// stdin and reports what this build can actually do. It deliberately does not
/// enumerate endpoints — that is `jean voice devices`, so a routine diagnostic
/// never touches the audio subsystem.
pub fn doctor() -> i32 {
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        eprintln!("could not read the voice configuration from stdin");
        return 1;
    }
    let config: VoiceConfig = match serde_json::from_str(line.trim()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("expected a VoiceConfig JSON object: {error}");
            return 1;
        }
    };
    let linked = cfg!(feature = "sherpa");
    let engine = match engine_factory(&config) {
        Ok(factory) => serde_json::json!({ "available": true, "label": factory.label() }),
        Err(reason) => serde_json::json!({ "available": false, "reason": reason }),
    };
    let required: Vec<serde_json::Value> = assets::required(&config)
        .iter()
        .map(|c| serde_json::json!({ "key": c.key, "path": c.path, "present": c.present }))
        .collect();
    let recognition: Vec<serde_json::Value> = assets::recognition(&config)
        .iter()
        .map(|c| serde_json::json!({ "key": c.key, "present": c.present }))
        .collect();
    let synthesis: Vec<serde_json::Value> = assets::synthesis(&config)
        .iter()
        .map(|c| serde_json::json!({ "key": c.key, "present": c.present }))
        .collect();
    let optional: Vec<serde_json::Value> = assets::optional(&config)
        .iter()
        .map(|c| serde_json::json!({ "key": c.key, "path": c.path, "present": c.present }))
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "enabled": config.enabled,
            "mode": config.mode,
            "backend": format!("{:?}", config.backend),
            "audio_backend": format!("{:?}", config.audio_backend()),
            // Non-empty means no endpoint is opened in either direction, which is
            // the difference between a validation run and a noisy one.
            "offline_wav": config.offline_wav,
            "system_audio_compiled": cfg!(windows),
            "sherpa_feature_linked": linked,
            "engine": engine,
            "required_assets": required,
            "recognition_assets": recognition,
            "synthesis_assets": synthesis,
            "optional_assets": optional,
            // Lossy, because `OsString` serialises as a code-unit array on Windows and an
            // operator cannot read their own provisioning path back as `[67, 58, 92, …]`.
            "native_provisioning": {
                "SHERPA_ONNX_LIB_DIR": provisioned("SHERPA_ONNX_LIB_DIR"),
                "SHERPA_ONNX_ARCHIVE_DIR": provisioned("SHERPA_ONNX_ARCHIVE_DIR"),
            },
            "hotkey": config.hotkey,
            "devices": "run `jean voice devices` to enumerate endpoints",
        })
    );
    0
}

/// A provisioning variable as the operator reads it: present with its value, absent as null.
fn provisioned(name: &str) -> Option<String> {
    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
}

/// The largest clip `--transcribe` will read. Recognition of a file this size
/// already takes minutes; past it the honest answer is to cut the clip.
const MAX_TRANSCRIBE_BYTES: u64 = 64 * 1024 * 1024;

/// Recognise one prerecorded WAV through the same engine boundary a live press
/// uses, with no device opened and no worker started.
///
/// This is the tier-3 path that does not need an operator at a microphone: the
/// clip is known, so expected and actual text can be recorded side by side. With
/// `backend = 'fixture'` it still answers, and the `engine` field says plainly
/// that the transcript is a test double's, not inference.
pub fn transcribe(path: &str) -> i32 {
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        eprintln!("could not read the voice configuration from stdin");
        return 1;
    }
    let config: VoiceConfig = match serde_json::from_str(line.trim()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("expected a VoiceConfig JSON object: {error}");
            return 1;
        }
    };
    let size = std::fs::metadata(path).map(|meta| meta.len());
    if let Ok(size) = size {
        if size > MAX_TRANSCRIBE_BYTES {
            eprintln!(
                "`{path}` is {size} bytes, over the {} MiB a single transcribe will read; \
                 cut the clip rather than raising the bound",
                MAX_TRANSCRIBE_BYTES / (1024 * 1024)
            );
            return 1;
        }
    }
    let file = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("could not read `{path}`: {error}");
            return 1;
        }
    };
    let source = match wav::read(&file) {
        Ok(pcm) => pcm,
        Err(error) => {
            eprintln!("`{path}` is not audio this build can read: {error}");
            return 1;
        }
    };
    let factory = match engine_factory(&config) {
        Ok(factory) => factory,
        Err(reason) => {
            eprintln!("{reason}");
            return 1;
        }
    };
    let mut recognizer = match factory.create_recognizer() {
        Ok(recognizer) => recognizer,
        Err(reason) => {
            eprintln!("{reason}");
            return 1;
        }
    };
    if let Err(reason) = recognizer.begin() {
        eprintln!("{reason}");
        return 1;
    }
    let mut resampler = audio::Resampler::new(source.sample_rate, config.sample_rate);
    let mut frames = resampler.push(&source.samples);
    frames.extend(resampler.flush());
    if frames.is_empty() {
        eprintln!("`{path}` decoded to no frames at {} Hz", config.sample_rate);
        return 1;
    }
    // The same packet size a 100 ms WASAPI callback would deliver, so feeding a
    // file and feeding a microphone take the identical path through the engine.
    let packet = (config.sample_rate / 10).max(1) as usize;
    let cancel = device::CancelFlag::new();
    for chunk in frames.chunks(packet) {
        let pcm = audio::Pcm {
            sample_rate: config.sample_rate,
            samples: chunk.to_vec(),
        };
        if let Err(reason) = recognizer.feed(&pcm, &cancel) {
            eprintln!("{reason}");
            return 1;
        }
    }
    let transcript = match recognizer.finish(&cancel) {
        Ok(text) => text,
        Err(reason) => {
            eprintln!("{reason}");
            return 1;
        }
    };
    println!(
        "{}",
        serde_json::json!({
            "engine": factory.label(),
            "file": path,
            "file_sample_rate": source.sample_rate,
            "engine_sample_rate": config.sample_rate,
            "frames": frames.len(),
            "duration_ms": source.duration().as_millis() as u64,
            "transcript": transcript,
        })
    );
    0
}

/// Synthesize one phrase into a WAV file and report what the audio actually is.
///
/// No render endpoint is opened, so this proves a synthesis bundle works without
/// a speaker and without playing anything at the operator. The level statistics
/// are computed before encoding, because "the file was written" is not evidence
/// that the engine produced sound rather than silence.
///
/// `phonemes` says the phrase is already in the bundle's phoneme alphabet, which
/// is the only way to reach the acoustic model when the grapheme-to-phoneme
/// lexicon the engine demands is absent. The report names which of the two ran.
pub fn synthesize(path: &str, text: &str, phonemes: bool) -> i32 {
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        eprintln!("could not read the voice configuration from stdin");
        return 1;
    }
    let config: VoiceConfig = match serde_json::from_str(line.trim()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("expected a VoiceConfig JSON object: {error}");
            return 1;
        }
    };
    if !wire::text_valid(text) {
        eprintln!("the phrase is empty or longer than the wire bound accepts");
        return 1;
    }
    let factory = match engine_factory(&config) {
        Ok(factory) => factory,
        Err(reason) => {
            eprintln!("{reason}");
            return 1;
        }
    };
    let mut synthesizer = match if phonemes {
        factory.create_phoneme_synthesizer()
    } else {
        factory.create_synthesizer()
    } {
        Ok(synthesizer) => synthesizer,
        Err(reason) => {
            eprintln!("{reason}");
            return 1;
        }
    };
    let cancel = CancelFlag::new();
    let started = Instant::now();
    let pcm = match if phonemes {
        let codepoints: Vec<i32> = text.chars().map(|c| c as i32).collect();
        synthesizer.synthesize_phonemes(&codepoints, &cancel)
    } else {
        synthesizer.synthesize(text, &cancel)
    } {
        Ok(pcm) => pcm,
        Err(reason) => {
            eprintln!("{reason}");
            return 1;
        }
    };
    let generated_ms = started.elapsed().as_millis() as u64;
    if pcm.samples.is_empty() {
        eprintln!(
            "the engine returned no audio for {} bytes of text",
            text.len()
        );
        return 1;
    }
    let peak = pcm
        .samples
        .iter()
        .fold(0.0f32, |acc, sample| acc.max(sample.abs()));
    let rms = (pcm.samples.iter().map(|s| s * s).sum::<f32>() / pcm.samples.len() as f32).sqrt();
    let clipped = pcm.samples.iter().any(|s| s.abs() > 1.0);
    let encoded = wav::encode_pcm16(&pcm);
    if let Err(error) = std::fs::write(path, &encoded) {
        eprintln!("could not write `{path}`: {error}");
        return 1;
    }
    let silent = peak < 0.01;
    println!(
        "{}",
        serde_json::json!({
            "engine": factory.label(),
            "file": path,
            "input": if phonemes { "phonemes" } else { "text" },
            "text": text,
            "sample_rate": pcm.sample_rate,
            "frames": pcm.samples.len(),
            "duration_ms": pcm.duration().as_millis() as u64,
            "peak": peak,
            "rms": rms,
            "clipped": clipped,
            "silent": silent,
            "generated_ms": generated_ms,
            "bytes_written": encoded.len(),
        })
    );
    if silent {
        eprintln!("the engine produced {peak:.6} peak audio: that is silence, not speech");
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::provisioned;

    /// Restores a variable to whatever the process had, so the suite stays
    /// order-independent even though the environment is process-wide.
    fn with_env<T>(name: &str, value: Option<&std::ffi::OsStr>, run: impl FnOnce() -> T) -> T {
        let previous = std::env::var_os(name);
        match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
        let result = run();
        match previous {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
        result
    }

    /// `doctor`'s provisioning field is where an operator confirms that the archive they
    /// pointed at is the one the worker inherited. A Windows `OsString` serialises as a
    /// code-unit array, which reads as `[67, 58, 92, …]` and answers nothing.
    #[test]
    fn a_provisioning_path_is_reported_as_text_and_not_as_a_code_unit_array() {
        let path = std::ffi::OsStr::new("C:\\JEAN\\native\\sherpa-onnx-v2.0.1\\lib");
        let reported = with_env("SHERPA_ONNX_LIB_DIR", Some(path), || {
            provisioned("SHERPA_ONNX_LIB_DIR")
        });
        assert_eq!(reported.as_deref(), Some(path.to_str().unwrap()));
        let json = serde_json::to_string(&reported).unwrap();
        assert!(json.starts_with("\"C:"), "{json} must be a path");
    }

    #[test]
    fn an_unset_provisioning_variable_stays_absent_rather_than_empty() {
        let reported = with_env("SHERPA_ONNX_ARCHIVE_DIR", None, || {
            provisioned("SHERPA_ONNX_ARCHIVE_DIR")
        });
        assert_eq!(reported, None, "unset must serialise as null");
    }
}
