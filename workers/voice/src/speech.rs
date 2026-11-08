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
        if cancel.is_cancelled() {
            return Err("recognition cancelled".into());
        }
        if !self.active {
            return Err("audio arrived before push-to-talk activation".into());
        }
        self.fed_frames += pcm.samples.len();
        Ok(())
    }

    fn finish(&mut self, cancel: &CancelFlag) -> Result<String, String> {
        if cancel.is_cancelled() {
            return Err("recognition cancelled".into());
        }
        if !std::mem::take(&mut self.active) {
            return Err("fixture recognizer was not capturing".into());
        }
        Ok(self.transcript.clone())
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
}
