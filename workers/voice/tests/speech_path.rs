//! Device-free coverage of the capture and playback pipelines.
//!
//! These tests drive `capture_once` and `speak_once` with scripted endpoints and
//! engines. They prove the audio mechanics, the bounds and the cancellation
//! behaviour; they are not evidence of real inference, and they never open a
//! microphone or a speaker.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use jean_protocol::config::{SpeechBackend, VoiceConfig};
use jean_voice::audio::{AudioError, DeviceFormat, Pcm, SampleFormat};
use jean_voice::device::{AudioBackend, CancelFlag, CaptureStream, RenderStream};
use jean_voice::runtime::{capture_once, speak_once, RecognitionEvent, SynthesisEvent};
use jean_voice::speech::{Recognizer, Synthesizer};

fn config(sample_rate: u32, buffer_ms: u64) -> VoiceConfig {
    VoiceConfig {
        enabled: true,
        mode: "push_to_talk".into(),
        backend: SpeechBackend::Fixture,
        sample_rate,
        capture_buffer_ms: buffer_ms,
        finalize_budget_ms: 50,
        fixture_transcript: "rotate the backup disks".into(),
        ..VoiceConfig::default()
    }
}

fn pcm(rate: u32, frames: usize) -> Pcm {
    Pcm {
        sample_rate: rate,
        samples: vec![0.1; frames],
    }
}

#[derive(Clone)]
struct Endpoint {
    packets: Vec<Pcm>,
    fail_after: Option<usize>,
    capture_open_error: Option<String>,
    render_open_error: Option<String>,
    render_write_error: bool,
    render_opens: Arc<AtomicUsize>,
    frames_written: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    format: DeviceFormat,
    /// Releases push-to-talk once the scripted packets run out, so a press can
    /// consume the whole scripted utterance instead of only its first packet.
    release_when_exhausted: Option<CancelFlag>,
}

impl Endpoint {
    fn capture(packets: Vec<Pcm>) -> Self {
        Self {
            packets,
            fail_after: None,
            capture_open_error: None,
            render_open_error: None,
            render_write_error: false,
            render_opens: Arc::new(AtomicUsize::new(0)),
            frames_written: Arc::new(AtomicUsize::new(0)),
            stopped: Arc::new(AtomicBool::new(false)),
            release_when_exhausted: None,
            format: DeviceFormat {
                sample_rate: 16000,
                channels: 1,
                format: SampleFormat::Float32,
            },
        }
    }

    fn at_rate(mut self, rate: u32) -> Self {
        self.format.sample_rate = rate;
        self
    }

    fn release_when_exhausted(mut self, release: CancelFlag) -> Self {
        self.release_when_exhausted = Some(release);
        self
    }
}

impl AudioBackend for Endpoint {
    fn devices(&self) -> Result<Vec<jean_voice::device::AudioDevice>, AudioError> {
        Ok(Vec::new())
    }

    fn open_capture(&self, _id: &str) -> Result<Box<dyn CaptureStream>, AudioError> {
        match &self.capture_open_error {
            Some(reason) => Err(AudioError::UnsupportedFormat(reason.clone())),
            None => Ok(Box::new(ScriptedCapture {
                packets: self.packets.clone(),
                format: self.format,
                reads: 0,
                fail_after: self.fail_after,
                release_when_exhausted: self.release_when_exhausted.clone(),
            })),
        }
    }

    fn open_render(&self, _id: &str) -> Result<Box<dyn RenderStream>, AudioError> {
        match &self.render_open_error {
            Some(reason) => Err(AudioError::DeviceLost(reason.clone())),
            None => {
                self.render_opens.fetch_add(1, Ordering::AcqRel);
                Ok(Box::new(ScriptedRender {
                    format: self.format,
                    frames_written: self.frames_written.clone(),
                    stopped: self.stopped.clone(),
                    write_error: self.render_write_error,
                }))
            }
        }
    }
}

struct ScriptedCapture {
    packets: Vec<Pcm>,
    format: DeviceFormat,
    reads: usize,
    fail_after: Option<usize>,
    release_when_exhausted: Option<CancelFlag>,
}

impl CaptureStream for ScriptedCapture {
    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn read(&mut self) -> Result<Option<Pcm>, AudioError> {
        if self.fail_after.is_some_and(|limit| self.reads >= limit) {
            return Err(AudioError::DeviceLost("the endpoint went away".into()));
        }
        self.reads += 1;
        let packet = self.packets.get(self.reads - 1).cloned();
        if packet.is_none() {
            if let Some(release) = &self.release_when_exhausted {
                release.cancel();
            }
        }
        Ok(packet)
    }
}

struct ScriptedRender {
    format: DeviceFormat,
    frames_written: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    write_error: bool,
}

impl RenderStream for ScriptedRender {
    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn write(&mut self, frames: &[f32]) -> Result<usize, AudioError> {
        if self.write_error {
            return Err(AudioError::DeviceLost(
                "the render endpoint disappeared mid-utterance".into(),
            ));
        }
        self.frames_written
            .fetch_add(frames.len(), Ordering::AcqRel);
        Ok(frames.len())
    }

    fn drain(&mut self, cancel: &CancelFlag) -> Result<(), AudioError> {
        let until = Instant::now() + Duration::from_millis(500);
        while !cancel.is_cancelled()
            && !self.stopped.load(Ordering::Acquire)
            && Instant::now() < until
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

/// A recognizer that records what reached it, and can be made to run long.
struct ScriptedRecognizer {
    text: String,
    fed: Arc<Mutex<Vec<usize>>>,
    begun: Arc<AtomicUsize>,
    in_finish: Arc<AtomicBool>,
    slow_finish: bool,
}

impl Recognizer for ScriptedRecognizer {
    fn begin(&mut self) -> Result<(), String> {
        self.begun.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn feed(&mut self, pcm: &Pcm, cancel: &CancelFlag) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("recognition cancelled".into());
        }
        self.fed.lock().unwrap().push(pcm.samples.len());
        Ok(())
    }

    fn finish(&mut self, cancel: &CancelFlag) -> Result<String, String> {
        self.in_finish.store(true, Ordering::Release);
        if self.slow_finish {
            let until = Instant::now() + Duration::from_secs(20);
            while !cancel.is_cancelled() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        Ok(self.text.clone())
    }
}

struct ScriptedSynthesizer {
    frames: usize,
    in_generate: Arc<AtomicBool>,
    slow: bool,
}

impl Synthesizer for ScriptedSynthesizer {
    fn synthesize(&mut self, _text: &str, cancel: &CancelFlag) -> Result<Pcm, String> {
        self.in_generate.store(true, Ordering::Release);
        if self.slow {
            let until = Instant::now() + Duration::from_secs(20);
            while !cancel.is_cancelled() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            if cancel.is_cancelled() {
                return Err("synthesis cancelled".into());
            }
        }
        Ok(pcm(16000, self.frames))
    }
}

fn channels<T>() -> (mpsc::SyncSender<T>, mpsc::Receiver<T>) {
    mpsc::sync_channel(8)
}

fn drain<T>(rx: &mpsc::Receiver<T>) -> Vec<T> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

#[test]
fn a_released_press_feeds_the_recognizer_and_returns_one_transcript() {
    let release = CancelFlag::new();
    let endpoint = Endpoint::capture(vec![pcm(16000, 800), pcm(16000, 400)])
        .release_when_exhausted(release.clone());
    let fed = Arc::new(Mutex::new(Vec::new()));
    let mut recognizer = ScriptedRecognizer {
        text: "rotate the backup disks".into(),
        fed: fed.clone(),
        begun: Arc::new(AtomicUsize::new(0)),
        in_finish: Arc::new(AtomicBool::new(false)),
        slow_finish: false,
    };
    let (tx, rx) = channels();
    capture_once(
        &config(16000, 2000),
        &endpoint,
        &mut recognizer,
        1,
        &CancelFlag::new(),
        &release,
        &tx,
    );
    let events = drain(&rx);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0], RecognitionEvent::Opened { activation: 1 });
    assert_eq!(
        events[1],
        RecognitionEvent::Transcript {
            activation: 1,
            text: "rotate the backup disks".into(),
            dropped: 0,
        },
        "one press produces exactly one transcript"
    );
    assert_eq!(*fed.lock().unwrap(), vec![800, 400]);
}

#[test]
fn audio_above_the_capture_buffer_is_dropped_oldest_first_and_counted() {
    let endpoint = Endpoint::capture(vec![pcm(16000, 10_000)]);
    let fed = Arc::new(Mutex::new(Vec::new()));
    let mut recognizer = ScriptedRecognizer {
        text: "held".into(),
        fed: fed.clone(),
        begun: Arc::new(AtomicUsize::new(0)),
        in_finish: Arc::new(AtomicBool::new(false)),
        slow_finish: false,
    };
    let (tx, rx) = channels();
    let release = CancelFlag::new();
    release.cancel();
    // 100 ms at 16 kHz holds 1600 frames; the arriving packet is 10000.
    capture_once(
        &config(16000, 100),
        &endpoint,
        &mut recognizer,
        3,
        &CancelFlag::new(),
        &release,
        &tx,
    );
    assert_eq!(
        *fed.lock().unwrap(),
        vec![1600],
        "only the bound reaches the engine"
    );
    let events = drain(&rx);
    assert!(
        matches!(
            events.as_slice(),
            [
                RecognitionEvent::Opened { .. },
                RecognitionEvent::Transcript { dropped: 8400, .. }
            ]
        ),
        "the drop count must reach the control loop: {events:?}"
    );
}

#[test]
fn device_audio_is_resampled_to_the_engine_rate_before_the_recognizer() {
    // A 48 kHz endpoint delivering 4800 frames (100 ms) must reach a 16 kHz
    // engine as about 1600 frames, not as 4800 at the wrong pitch.
    let endpoint = Endpoint::capture(vec![pcm(48000, 4800)]).at_rate(48000);
    let fed = Arc::new(Mutex::new(Vec::new()));
    let mut recognizer = ScriptedRecognizer {
        text: "resampled".into(),
        fed: fed.clone(),
        begun: Arc::new(AtomicUsize::new(0)),
        in_finish: Arc::new(AtomicBool::new(false)),
        slow_finish: false,
    };
    let (tx, rx) = channels();
    let release = CancelFlag::new();
    release.cancel();
    capture_once(
        &config(16000, 5000),
        &endpoint,
        &mut recognizer,
        4,
        &CancelFlag::new(),
        &release,
        &tx,
    );
    drop(rx);
    let fed = fed.lock().unwrap()[0];
    assert!(
        (1599..=1601).contains(&fed),
        "4800 frames at 48 kHz should arrive as about 1600 at 16 kHz, got {fed}"
    );
}

#[test]
fn an_endpoint_that_cannot_be_opened_fails_before_any_transcript_is_claimed() {
    let endpoint = Endpoint {
        capture_open_error: Some("32-bit integer PCM is not supported".into()),
        ..Endpoint::capture(vec![pcm(16000, 160)])
    };
    let mut recognizer = ScriptedRecognizer {
        text: "must not run".into(),
        fed: Arc::new(Mutex::new(Vec::new())),
        begun: Arc::new(AtomicUsize::new(0)),
        in_finish: Arc::new(AtomicBool::new(false)),
        slow_finish: false,
    };
    let (tx, rx) = channels();
    let release = CancelFlag::new();
    release.cancel();
    capture_once(
        &config(16000, 2000),
        &endpoint,
        &mut recognizer,
        5,
        &CancelFlag::new(),
        &release,
        &tx,
    );
    let events = drain(&rx);
    assert_eq!(
        events.len(),
        1,
        "an unopened device cannot also report a transcript"
    );
    assert!(
        matches!(&events[0], RecognitionEvent::OpenFailed { activation: 5, reason } if reason.contains("unsupported audio format")),
        "{events:?}"
    );
    assert_eq!(recognizer.begun.load(Ordering::Acquire), 0);
}

#[test]
fn a_device_that_disappears_mid_press_reports_device_loss_not_silence() {
    let endpoint = Endpoint {
        fail_after: Some(1),
        ..Endpoint::capture(vec![pcm(16000, 320)])
    };
    let mut recognizer = ScriptedRecognizer {
        text: "partial".into(),
        fed: Arc::new(Mutex::new(Vec::new())),
        begun: Arc::new(AtomicUsize::new(0)),
        in_finish: Arc::new(AtomicBool::new(false)),
        slow_finish: false,
    };
    let (tx, rx) = channels();
    capture_once(
        &config(16000, 2000),
        &endpoint,
        &mut recognizer,
        6,
        &CancelFlag::new(),
        &CancelFlag::new(),
        &tx,
    );
    let events = drain(&rx);
    assert!(
        matches!(&events[1], RecognitionEvent::Failed { activation: 6, reason } if reason.contains("audio device lost")),
        "{events:?}"
    );
}

#[test]
fn a_stop_during_a_long_flush_is_answered_fast_and_yields_no_transcript() {
    let endpoint = Endpoint::capture(vec![pcm(16000, 160)]);
    let in_finish = Arc::new(AtomicBool::new(false));
    let mut recognizer = ScriptedRecognizer {
        text: "late transcript".into(),
        fed: Arc::new(Mutex::new(Vec::new())),
        begun: Arc::new(AtomicUsize::new(0)),
        in_finish: in_finish.clone(),
        slow_finish: true,
    };
    let (tx, rx) = channels();
    let cancel = CancelFlag::new();
    let release = CancelFlag::new();
    release.cancel();

    // The release is already in, so the only way out of the flush is cancellation.
    let flag = cancel.clone();
    let watch = in_finish.clone();
    let canceller = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(2);
        while !watch.load(Ordering::Acquire) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(watch.load(Ordering::Acquire), "the flush never started");
        flag.cancel();
    });

    let started = Instant::now();
    capture_once(
        &config(16000, 2000),
        &endpoint,
        &mut recognizer,
        7,
        &cancel,
        &release,
        &tx,
    );
    let elapsed = started.elapsed();
    canceller.join().unwrap();

    // The flush above only exits when its own 20 s bound expires, so a responsive
    // cancellation must return in milliseconds, not seconds.
    assert!(
        elapsed < Duration::from_millis(500),
        "flush took {elapsed:?}"
    );
    let events = drain(&rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RecognitionEvent::Transcript { .. })),
        "a cancelled press must not deliver a transcript: {events:?}"
    );
}

#[test]
fn cancellation_during_generation_stops_the_utterance_without_opening_a_speaker() {
    let endpoint = Endpoint::capture(vec![]);
    let (tx, rx) = channels();
    let cancel = CancelFlag::new();
    cancel.cancel();
    let mut synthesizer = ScriptedSynthesizer {
        frames: 1600,
        in_generate: Arc::new(AtomicBool::new(false)),
        slow: false,
    };
    speak_once(
        &config(16000, 2000),
        &endpoint,
        &mut synthesizer,
        9,
        "acknowledged",
        &cancel,
        &tx,
    );
    let events = drain(&rx);
    assert_eq!(events, vec![SynthesisEvent::Stopped { utterance: 9 }]);
    assert_eq!(endpoint.render_opens.load(Ordering::Acquire), 0);
    assert!(!endpoint.stopped.load(Ordering::Acquire), "nothing to stop");
}

#[test]
fn a_long_synthesis_is_cut_short_by_the_utterance_scope() {
    let endpoint = Endpoint::capture(vec![]);
    let (tx, rx) = channels();
    let cancel = CancelFlag::new();
    let mut synthesizer = ScriptedSynthesizer {
        frames: 32000,
        in_generate: Arc::new(AtomicBool::new(false)),
        slow: true,
    };
    let flag = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        flag.cancel();
    });
    let started = Instant::now();
    speak_once(
        &config(16000, 2000),
        &endpoint,
        &mut synthesizer,
        10,
        "a very long announcement",
        &cancel,
        &tx,
    );
    let elapsed = started.elapsed();
    canceller.join().unwrap();
    assert!(
        elapsed < Duration::from_secs(1),
        "generation took {elapsed:?}"
    );
    assert_eq!(
        drain(&rx),
        vec![SynthesisEvent::Stopped { utterance: 10 }],
        "a cancelled synthesis is a stop, not a failure"
    );
    assert_eq!(endpoint.frames_written.load(Ordering::Acquire), 0);
}

#[test]
fn synthesized_audio_is_written_and_then_stopped() {
    let endpoint = Endpoint::capture(vec![]);
    let (tx, rx) = channels();
    let mut synthesizer = ScriptedSynthesizer {
        frames: 1600,
        in_generate: Arc::new(AtomicBool::new(false)),
        slow: false,
    };
    speak_once(
        &config(16000, 2000),
        &endpoint,
        &mut synthesizer,
        11,
        "task succeeded",
        &CancelFlag::new(),
        &tx,
    );
    assert_eq!(endpoint.frames_written.load(Ordering::Acquire), 1600);
    assert!(
        endpoint.stopped.load(Ordering::Acquire),
        "every utterance ends by stopping the endpoint"
    );
    assert_eq!(drain(&rx), vec![SynthesisEvent::Stopped { utterance: 11 }]);
}

#[test]
fn a_speaker_that_vanishes_mid_utterance_is_a_failure_the_owner_sees() {
    let endpoint = Endpoint {
        render_write_error: true,
        ..Endpoint::capture(vec![])
    };
    let (tx, rx) = channels();
    let mut synthesizer = ScriptedSynthesizer {
        frames: 1600,
        in_generate: Arc::new(AtomicBool::new(false)),
        slow: false,
    };
    speak_once(
        &config(16000, 2000),
        &endpoint,
        &mut synthesizer,
        12,
        "task succeeded",
        &CancelFlag::new(),
        &tx,
    );
    let events = drain(&rx);
    assert!(
        matches!(&events[..], [SynthesisEvent::Failed { utterance: 12, reason }] if reason.contains("render endpoint disappeared")),
        "{events:?}"
    );
    assert!(
        endpoint.stopped.load(Ordering::Acquire),
        "a failed render is still silenced"
    );
}

#[test]
fn a_render_endpoint_that_cannot_open_fails_before_any_audio_is_written() {
    let endpoint = Endpoint {
        render_open_error: Some("no default output endpoint".into()),
        ..Endpoint::capture(vec![])
    };
    let (tx, rx) = channels();
    let mut synthesizer = ScriptedSynthesizer {
        frames: 1600,
        in_generate: Arc::new(AtomicBool::new(false)),
        slow: false,
    };
    speak_once(
        &config(16000, 2000),
        &endpoint,
        &mut synthesizer,
        13,
        "task succeeded",
        &CancelFlag::new(),
        &tx,
    );
    assert_eq!(endpoint.frames_written.load(Ordering::Acquire), 0);
    assert!(matches!(
        drain(&rx)[..],
        [SynthesisEvent::Failed { utterance: 13, .. }]
    ));
}

#[test]
fn a_cancelled_recognizer_is_never_fed_or_finished_into_a_transcript() {
    let endpoint = Endpoint::capture(vec![pcm(16000, 160); 4]);
    let fed = Arc::new(Mutex::new(Vec::new()));
    let mut recognizer = ScriptedRecognizer {
        text: "must not arrive".into(),
        fed: fed.clone(),
        begun: Arc::new(AtomicUsize::new(0)),
        in_finish: Arc::new(AtomicBool::new(false)),
        slow_finish: false,
    };
    let (tx, rx) = channels();
    let cancel = CancelFlag::new();
    cancel.cancel();
    capture_once(
        &config(16000, 2000),
        &endpoint,
        &mut recognizer,
        14,
        &cancel,
        &CancelFlag::new(),
        &tx,
    );
    // Cancellation is noticed before the device is even reported open, so the
    // press leaves no trace at all.
    assert!(
        drain(&rx).is_empty(),
        "a press cancelled before it opened must report nothing"
    );
    assert!(fed.lock().unwrap().is_empty());
    assert_eq!(recognizer.begun.load(Ordering::Acquire), 0);
}
