//! The speech engine boundary.
//!
//! Recognition and synthesis are separate traits because they live on separate
//! threads: a sherpa recognizer and a sherpa synthesizer both wrap raw C pointers
//! and are neither `Send` nor `Sync`, so each is created, used and dropped on the
//! one thread that owns it. The factory is the only shared piece, and it holds
//! paths rather than handles.
//!
//! Every method that can run long takes the cancellation flag, so a stop request
//! is noticed between decode steps and between synthesis chunks rather than
//! after the whole operation finishes.

use jean_protocol::config::VoiceConfig;

use crate::audio::Pcm;
use crate::device::CancelFlag;

pub trait Recognizer {
    fn begin(&mut self) -> Result<(), String>;
    fn feed(&mut self, pcm: &Pcm, cancel: &CancelFlag) -> Result<(), String>;
    /// Flush the tail and return the finalized transcript for this activation.
    fn finish(&mut self, cancel: &CancelFlag) -> Result<String, String>;
    /// Release any per-capture stream, buffered samples, or VAD state after an aborted capture.
    fn reset(&mut self) {}
}

pub trait OnlineStreamBackend {
    fn accept_waveform(&self, sample_rate: i32, samples: &[f32]);
    fn input_finished(&self);
}

pub trait OnlineDecoder {
    type Stream: OnlineStreamBackend;
    fn create_stream(&self) -> Self::Stream;
    fn is_ready(&self, stream: &Self::Stream) -> bool;
    fn decode(&self, stream: &Self::Stream);
    fn get_result(&self, stream: &Self::Stream) -> Option<String>;
}

pub trait VadBackend {
    fn reset(&self);
    fn accept_waveform(&self, samples: &[f32]);
    fn is_empty(&self) -> bool;
    fn front_samples(&self) -> Option<Vec<f32>>;
    fn pop(&self);
    fn flush(&self);
}

pub trait OfflineDecoder {
    fn decode(
        &self,
        sample_rate: u32,
        samples: &[f32],
        cancel: &CancelFlag,
    ) -> Result<String, String>;
}

pub struct SherpaRecognizer<R: OnlineDecoder, V: VadBackend> {
    pub recognizer: R,
    pub vad: Option<V>,
    pub stream: Option<R::Stream>,
    pub sample_rate: u32,
}

impl<R: OnlineDecoder, V: VadBackend> SherpaRecognizer<R, V> {
    pub fn new(recognizer: R, vad: Option<V>, sample_rate: u32) -> Self {
        Self {
            recognizer,
            vad,
            stream: None,
            sample_rate,
        }
    }

    fn decode_ready(&mut self, cancel: &CancelFlag) -> Result<(), String> {
        let stream = self
            .stream
            .as_ref()
            .ok_or("recognition has no active stream")?;
        while self.recognizer.is_ready(stream) {
            if cancel.is_cancelled() {
                return Err("recognition cancelled".into());
            }
            self.recognizer.decode(stream);
        }
        Ok(())
    }

    fn accept(&mut self, samples: &[f32], cancel: &CancelFlag) -> Result<(), String> {
        let stream = self
            .stream
            .as_ref()
            .ok_or("recognition has no active stream")?;
        match self.vad.as_ref() {
            Some(vad) => {
                vad.accept_waveform(samples);
                while !vad.is_empty() {
                    if cancel.is_cancelled() {
                        return Err("recognition cancelled".into());
                    }
                    if let Some(segment) = vad.front_samples() {
                        stream.accept_waveform(self.sample_rate as i32, &segment);
                    }
                    vad.pop();
                }
            }
            None => stream.accept_waveform(self.sample_rate as i32, samples),
        }
        self.decode_ready(cancel)
    }
}

impl<R: OnlineDecoder, V: VadBackend> Recognizer for SherpaRecognizer<R, V> {
    fn begin(&mut self) -> Result<(), String> {
        if self.stream.is_some() {
            return Err("a recognition stream is already open".into());
        }
        if let Some(vad) = self.vad.as_ref() {
            vad.reset();
        }
        self.stream = Some(self.recognizer.create_stream());
        Ok(())
    }

    fn feed(&mut self, pcm: &Pcm, cancel: &CancelFlag) -> Result<(), String> {
        if self.stream.is_none() {
            return Err("recognition has no active stream".into());
        }
        if cancel.is_cancelled() {
            self.reset();
            return Err("recognition cancelled".into());
        }
        if pcm.sample_rate != self.sample_rate {
            self.reset();
            return Err(format!(
                "Zipformer expects {} Hz PCM, got {} Hz",
                self.sample_rate, pcm.sample_rate
            ));
        }
        if let Err(error) = self.accept(&pcm.samples, cancel) {
            self.reset();
            return Err(error);
        }
        Ok(())
    }

    fn finish(&mut self, cancel: &CancelFlag) -> Result<String, String> {
        if self.stream.is_none() {
            return Err("recognition has no active stream".into());
        }
        if cancel.is_cancelled() {
            self.reset();
            return Err("recognition cancelled".into());
        }
        if let Some(vad) = self.vad.as_ref() {
            vad.flush();
            while !vad.is_empty() {
                if let Some(segment) = vad.front_samples() {
                    if let Some(stream) = self.stream.as_ref() {
                        stream.accept_waveform(self.sample_rate as i32, &segment);
                    }
                }
                vad.pop();
            }
        }
        let tail = (self.sample_rate as f32 * 0.5).round() as usize;
        if let Some(stream) = self.stream.as_ref() {
            stream.accept_waveform(self.sample_rate as i32, &vec![0.0; tail]);
            stream.input_finished();
        }
        if let Err(error) = self.decode_ready(cancel) {
            self.reset();
            return Err(error);
        }
        let text = self
            .stream
            .as_ref()
            .and_then(|stream| self.recognizer.get_result(stream))
            .map(|result| result.trim().to_owned())
            .unwrap_or_default();
        self.reset();
        Ok(text)
    }

    fn reset(&mut self) {
        self.stream = None;
        if let Some(vad) = self.vad.as_ref() {
            vad.reset();
        }
    }
}

/// Parakeet TDT is an offline model: collect the bounded press, then decode it
/// once on release. This implements the same worker interface as Zipformer.
pub struct OfflineSherpaRecognizer<D: OfflineDecoder> {
    pub recognizer: D,
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub max_frames: usize,
    pub active: bool,
}

impl<D: OfflineDecoder> OfflineSherpaRecognizer<D> {
    pub fn new(recognizer: D, sample_rate: u32, max_frames: usize) -> Self {
        Self {
            recognizer,
            samples: Vec::new(),
            sample_rate,
            max_frames,
            active: false,
        }
    }
}

impl<D: OfflineDecoder> Recognizer for OfflineSherpaRecognizer<D> {
    fn begin(&mut self) -> Result<(), String> {
        if self.active {
            return Err("Parakeet recognizer is already capturing".into());
        }
        self.samples.clear();
        self.active = true;
        Ok(())
    }

    fn feed(&mut self, pcm: &Pcm, cancel: &CancelFlag) -> Result<(), String> {
        if !self.active {
            return Err("audio arrived before push-to-talk activation".into());
        }
        if cancel.is_cancelled() {
            self.reset();
            return Err("recognition cancelled".into());
        }
        if pcm.sample_rate != self.sample_rate {
            self.reset();
            return Err(format!(
                "Parakeet expects {} Hz PCM, got {} Hz",
                self.sample_rate, pcm.sample_rate
            ));
        }
        if self.samples.len().saturating_add(pcm.samples.len()) > self.max_frames {
            self.reset();
            return Err("Parakeet capture exceeded voice.capture_timeout_ms".into());
        }
        self.samples.extend_from_slice(&pcm.samples);
        Ok(())
    }

    fn finish(&mut self, cancel: &CancelFlag) -> Result<String, String> {
        let was_active = std::mem::take(&mut self.active);
        let samples = std::mem::take(&mut self.samples);
        if cancel.is_cancelled() {
            return Err("recognition cancelled".into());
        }
        if !was_active {
            return Err("Parakeet recognizer was not capturing".into());
        }
        if samples.is_empty() {
            return Err("Parakeet capture contained no audio frames".into());
        }
        self.recognizer.decode(self.sample_rate, &samples, cancel)
    }

    fn reset(&mut self) {
        self.active = false;
        self.samples.clear();
    }
}

pub trait Synthesizer {
    fn synthesize(&mut self, text: &str, cancel: &CancelFlag) -> Result<Pcm, String>;

    /// Render text that is already in the bundle's phoneme alphabet, given as
    /// Unicode code points so the engine looks each one up in its own `tokens.txt`.
    ///
    /// This exists because sherpa-onnx 2.0.1 dropped the espeak-ng path and requires
    /// a lexicon file for text input, which the supplied Kokoro bundle does not
    /// contain: it is how the acoustic model can be proven to synthesize without the
    /// grapheme-to-phoneme half of the pipeline. Refusing by default is what a test
    /// double should do — pre-phonemized input rendered by a fixture proves nothing.
    fn synthesize_phonemes(
        &mut self,
        _codepoints: &[i32],
        _cancel: &CancelFlag,
    ) -> Result<Pcm, String> {
        Err("this engine cannot render pre-phonemized input".into())
    }
}

pub trait EngineFactory: Send + Sync {
    fn create_recognizer(&self) -> Result<Box<dyn Recognizer>, String>;
    fn create_synthesizer(&self) -> Result<Box<dyn Synthesizer>, String>;
    /// A synthesizer for input that is already in the bundle's phoneme alphabet.
    /// Such input never reaches the grapheme-to-phoneme table, so an engine whose
    /// text path demands a lexicon can still be created here — which is the only
    /// way to measure the acoustic model when that one asset is absent. Default:
    /// the same synthesizer, because a fixture has no asset requirements at all.
    fn create_phoneme_synthesizer(&self) -> Result<Box<dyn Synthesizer>, String> {
        self.create_synthesizer()
    }
    /// What this engine actually is, for the worker's own diagnostics. A fixture
    /// says "fixture"; nothing here is allowed to imply real inference.
    fn label(&self) -> &'static str;
}

/// The device-free test engine. It returns the configured transcript regardless
/// of the audio fed to it, which is exactly why it is never the default and never
/// reported as working speech.
pub struct FixtureEngine {
    pub transcript: String,
    pub sample_rate: u32,
    pub utterance_ms: u64,
}

impl FixtureEngine {
    pub fn from_config(config: &VoiceConfig) -> Self {
        Self {
            transcript: config.fixture_transcript.clone(),
            sample_rate: config.sample_rate,
            utterance_ms: config.fixture_utterance_ms,
        }
    }
}

impl EngineFactory for FixtureEngine {
    fn create_recognizer(&self) -> Result<Box<dyn Recognizer>, String> {
        Ok(Box::new(FixtureRecognizer {
            transcript: self.transcript.clone(),
            active: false,
            fed_frames: 0,
        }))
    }

    fn create_synthesizer(&self) -> Result<Box<dyn Synthesizer>, String> {
        Ok(Box::new(FixtureSynthesizer {
            sample_rate: self.sample_rate,
            utterance_ms: self.utterance_ms,
        }))
    }

    fn label(&self) -> &'static str {
        "fixture (no inference, no device)"
    }
}

pub struct FixtureRecognizer {
    transcript: String,
    active: bool,
    fed_frames: usize,
}

impl FixtureRecognizer {
    pub fn fed_frames(&self) -> usize {
        self.fed_frames
    }
}

impl Recognizer for FixtureRecognizer {
    fn begin(&mut self) -> Result<(), String> {
        if self.active {
            return Err("fixture recognizer is already capturing".into());
        }
        self.active = true;
        self.fed_frames = 0;
        Ok(())
    }

    fn feed(&mut self, pcm: &Pcm, cancel: &CancelFlag) -> Result<(), String> {
        if !self.active {
            return Err("audio arrived before push-to-talk activation".into());
        }
        if cancel.is_cancelled() {
            self.reset();
            return Err("recognition cancelled".into());
        }
        self.fed_frames += pcm.samples.len();
        Ok(())
    }

    fn finish(&mut self, cancel: &CancelFlag) -> Result<String, String> {
        let was_active = std::mem::take(&mut self.active);
        if cancel.is_cancelled() {
            self.fed_frames = 0;
            return Err("recognition cancelled".into());
        }
        if !was_active {
            return Err("fixture recognizer was not capturing".into());
        }
        Ok(self.transcript.clone())
    }

    fn reset(&mut self) {
        self.active = false;
        self.fed_frames = 0;
    }
}

pub struct FixtureSynthesizer {
    sample_rate: u32,
    utterance_ms: u64,
}

impl Synthesizer for FixtureSynthesizer {
    fn synthesize(&mut self, text: &str, cancel: &CancelFlag) -> Result<Pcm, String> {
        if cancel.is_cancelled() {
            return Err("synthesis cancelled".into());
        }
        if text.trim().is_empty() {
            return Err("cannot synthesize empty text".into());
        }
        Ok(Pcm::silent(
            self.sample_rate,
            (self.sample_rate as u64 * self.utterance_ms / 1000).max(1) as usize,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(transcript: &str, utterance_ms: u64) -> FixtureEngine {
        FixtureEngine {
            transcript: transcript.into(),
            sample_rate: 16000,
            utterance_ms,
        }
    }

    #[test]
    fn the_fixture_requires_activation_and_never_claims_inference() {
        let engine = engine("inspect this process", 10);
        assert!(engine.label().contains("no inference"));
        let mut recognizer = engine.create_recognizer().unwrap();
        let cancel = CancelFlag::new();
        assert!(recognizer.finish(&cancel).is_err());
        assert!(recognizer.feed(&Pcm::silent(16000, 16), &cancel).is_err());
        recognizer.begin().unwrap();
        assert!(recognizer.begin().is_err());
        recognizer.feed(&Pcm::silent(16000, 160), &cancel).unwrap();
        assert_eq!(recognizer.finish(&cancel).unwrap(), "inspect this process");
    }

    #[test]
    fn cancellation_is_noticed_by_both_halves() {
        let engine = engine("held", 10);
        let cancel = CancelFlag::new();
        cancel.cancel();
        let mut recognizer = engine.create_recognizer().unwrap();
        recognizer.begin().unwrap();
        assert!(recognizer.feed(&Pcm::silent(16000, 1), &cancel).is_err());
        assert!(recognizer.finish(&cancel).is_err());
        let mut synthesizer = engine.create_synthesizer().unwrap();
        assert!(synthesizer.synthesize("acknowledged", &cancel).is_err());
    }

    #[test]
    fn fixture_synthesis_produces_a_bounded_silent_utterance() {
        let mut synthesizer = engine("", 10).create_synthesizer().unwrap();
        let pcm = synthesizer
            .synthesize("acknowledged", &CancelFlag::new())
            .unwrap();
        assert_eq!(pcm.samples.len(), 160);
        assert_eq!(pcm.sample_rate, 16000);
        assert!(synthesizer.synthesize("   ", &CancelFlag::new()).is_err());
    }

    /// The shipped 10 ms block is gone before an interrupt can land in it, which is
    /// why the playback tests raise this: the length is what makes the sink's pacing
    /// observable, and only the fixture engine reads the key.
    #[test]
    fn a_raised_fixture_utterance_is_the_length_it_claims() {
        let mut synthesizer = engine("", 2000).create_synthesizer().unwrap();
        let pcm = synthesizer
            .synthesize("a long announcement", &CancelFlag::new())
            .unwrap();
        assert_eq!(pcm.samples.len(), 32_000);
        assert!(pcm.samples.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn recognizers_recover_after_cancellation_timeout_audio_error_and_frame_limit() {
        use std::cell::{Cell, RefCell};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use crate::audio::{AudioError, DeviceFormat, SampleFormat};
        use crate::device::{AudioBackend, AudioDevice, CaptureStream, RenderStream};
        use crate::runtime::{capture_once, RecognitionEvent};

        struct StubStream(Cell<usize>);
        impl OnlineStreamBackend for StubStream {
            fn accept_waveform(&self, _sample_rate: i32, samples: &[f32]) {
                self.0.set(self.0.get() + samples.len());
            }
            fn input_finished(&self) {}
        }

        struct StubOnlineDecoder;
        impl OnlineDecoder for StubOnlineDecoder {
            type Stream = StubStream;
            fn create_stream(&self) -> Self::Stream {
                StubStream(Cell::new(0))
            }
            fn is_ready(&self, _stream: &Self::Stream) -> bool {
                false
            }
            fn decode(&self, _stream: &Self::Stream) {}
            fn get_result(&self, stream: &Self::Stream) -> Option<String> {
                (stream.0.get() > 0).then(|| "zipformer recovered".to_owned())
            }
        }

        struct StubVad(RefCell<Vec<Vec<f32>>>);
        impl VadBackend for StubVad {
            fn reset(&self) {
                self.0.borrow_mut().clear();
            }
            fn accept_waveform(&self, samples: &[f32]) {
                self.0.borrow_mut().push(samples.to_vec());
            }
            fn is_empty(&self) -> bool {
                self.0.borrow().is_empty()
            }
            fn front_samples(&self) -> Option<Vec<f32>> {
                self.0.borrow().first().cloned()
            }
            fn pop(&self) {
                let mut q = self.0.borrow_mut();
                if !q.is_empty() {
                    q.remove(0);
                }
            }
            fn flush(&self) {}
        }

        struct StubOfflineDecoder;
        impl OfflineDecoder for StubOfflineDecoder {
            fn decode(
                &self,
                _sample_rate: u32,
                samples: &[f32],
                cancel: &CancelFlag,
            ) -> Result<String, String> {
                if cancel.is_cancelled() {
                    return Err("recognition cancelled".into());
                }
                if samples.is_empty() {
                    return Err("no samples".into());
                }
                Ok("parakeet recovered".to_owned())
            }
        }

        enum AudioMode {
            Healthy(usize),
            ReadError,
        }

        struct ScriptedCapture(AudioMode);
        impl CaptureStream for ScriptedCapture {
            fn format(&self) -> DeviceFormat {
                DeviceFormat {
                    sample_rate: 16000,
                    channels: 1,
                    format: SampleFormat::Float32,
                }
            }
            fn read(&mut self) -> Result<Option<Pcm>, AudioError> {
                match &mut self.0 {
                    AudioMode::Healthy(remaining) => {
                        if *remaining == 0 {
                            Ok(None)
                        } else {
                            *remaining -= 1;
                            Ok(Some(Pcm::silent(16000, 160)))
                        }
                    }
                    AudioMode::ReadError => Err(AudioError::DeviceLost("synthetic read error".into())),
                }
            }
        }

        struct ScriptedAudio(AtomicBool);
        impl AudioBackend for ScriptedAudio {
            fn devices(&self) -> Result<Vec<AudioDevice>, AudioError> {
                Ok(Vec::new())
            }
            fn open_capture(&self, _device: &str) -> Result<Box<dyn CaptureStream>, AudioError> {
                if self.0.load(Ordering::SeqCst) {
                    Ok(Box::new(ScriptedCapture(AudioMode::ReadError)))
                } else {
                    Ok(Box::new(ScriptedCapture(AudioMode::Healthy(2))))
                }
            }
            fn open_render(&self, _device: &str) -> Result<Box<dyn RenderStream>, AudioError> {
                Err(AudioError::Open("not used".into()))
            }
        }

        let mut recognizers: Vec<(&str, Box<dyn Recognizer>, &str)> = vec![
            (
                "fixture",
                engine("fixture recovered", 10).create_recognizer().unwrap(),
                "fixture recovered",
            ),
            (
                "zipformer",
                Box::new(SherpaRecognizer::new(
                    StubOnlineDecoder,
                    Some(StubVad(RefCell::new(Vec::new()))),
                    16000,
                )),
                "zipformer recovered",
            ),
            (
                "parakeet",
                Box::new(OfflineSherpaRecognizer::new(StubOfflineDecoder, 16000, 400)),
                "parakeet recovered",
            ),
        ];

        let config = VoiceConfig::default();
        let audio = ScriptedAudio(AtomicBool::new(false));

        for (label, recognizer, expected) in &mut recognizers {
            // 1. Cancellation during feed / finish
            let cancelled = CancelFlag::new();
            cancelled.cancel();
            recognizer.begin().expect(label);
            assert!(recognizer.feed(&Pcm::silent(16000, 160), &cancelled).is_err());

            // Second capture immediately succeeds.
            let ok = CancelFlag::new();
            recognizer.begin().expect(label);
            recognizer.feed(&Pcm::silent(16000, 160), &ok).expect(label);
            assert_eq!(recognizer.finish(&ok).expect(label), *expected);

            // 2. Timeout / abort mid-capture via capture_once
            let (tx, rx) = mpsc::sync_channel(16);
            let timeout_cancel = CancelFlag::new();
            timeout_cancel.cancel();
            let release = CancelFlag::new();
            // Even when begin() ran and capture_once exits on cancel, state is reset.
            recognizer.begin().expect(label);
            recognizer.reset();
            capture_once(
                &config,
                &audio,
                recognizer.as_mut(),
                1,
                &timeout_cancel,
                &release,
                &tx,
            );

            // Follow-up capture in the same worker succeeds.
            let ok_cancel = CancelFlag::new();
            let ok_release = CancelFlag::new();
            ok_release.cancel();
            capture_once(
                &config,
                &audio,
                recognizer.as_mut(),
                2,
                &ok_cancel,
                &ok_release,
                &tx,
            );
            let mut transcripts = Vec::new();
            while let Ok(evt) = rx.try_recv() {
                if let RecognitionEvent::Transcript { text, .. } = evt {
                    transcripts.push(text);
                }
            }
            assert_eq!(transcripts, vec![(*expected).to_owned()], "{label}");

            // 3. Audio read error in capture_once, followed by a clean capture.
            audio.0.store(true, Ordering::SeqCst);
            capture_once(
                &config,
                &audio,
                recognizer.as_mut(),
                3,
                &ok_cancel,
                &ok_release,
                &tx,
            );
            audio.0.store(false, Ordering::SeqCst);
            capture_once(
                &config,
                &audio,
                recognizer.as_mut(),
                4,
                &ok_cancel,
                &ok_release,
                &tx,
            );
            let mut saw_failed = false;
            let mut saw_recovered = false;
            while let Ok(evt) = rx.try_recv() {
                match evt {
                    RecognitionEvent::Failed { activation: 3, .. } => saw_failed = true,
                    RecognitionEvent::Transcript {
                        activation: 4,
                        text,
                        ..
                    } if text == *expected => saw_recovered = true,
                    _ => {}
                }
            }
            assert!(saw_failed && saw_recovered, "{label} did not recover after audio read error");

            // 4. Sample-rate mismatch / frame-limit exit during feed, followed by a clean capture.
            recognizer.begin().expect(label);
            if *label == "parakeet" {
                assert!(recognizer.feed(&Pcm::silent(16000, 500), &ok).is_err());
            } else if *label == "zipformer" {
                assert!(recognizer.feed(&Pcm::silent(8000, 160), &ok).is_err());
            } else {
                recognizer.reset();
            }
            recognizer.begin().expect(label);
            recognizer.feed(&Pcm::silent(16000, 160), &ok).expect(label);
            assert_eq!(recognizer.finish(&ok).expect(label), *expected);
        }
    }
}
