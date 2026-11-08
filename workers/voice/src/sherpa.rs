//! Native speech through the official sherpa-onnx 2.0.1 Rust crate.
//!
//! Compiled only with `--features sherpa`, which pulls the optional dependency in
//! `workers/voice/Cargo.toml`. That crate's `-sys` build script downloads a 122 MB
//! native archive unless `SHERPA_ONNX_LIB_DIR` or `SHERPA_ONNX_ARCHIVE_DIR` declares
//! local provisioning first, and both refusal paths were executed rather than read
//! (§59): with provisioning declared and the archive absent the build fails naming
//! the exact missing file, and nothing is fetched.
//!
//! The API surface here was read from the v2.0.1 sources, not guessed, and is now
//! checked by a compiler: `OnlineRecognizer`/`OnlineStream` (online_asr.rs),
//! `VoiceActivityDetector` (vad.rs), `OfflineTts::generate_with_config` (tts.rs),
//! `OfflineTtsKokoroModelConfig` (tts.rs:162). The first compile against the real
//! crate found one genuine error of mine — `OnlineModelConfig` has no `joiner`; the
//! transducer files live under `model_config.transducer`.
//!
//! The synthesis callback returns `false` to abort generation, which is how a stop
//! request reaches the middle of a long utterance rather than only its end.

use sherpa_onnx::{
    GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsKokoroModelConfig,
    OfflineTtsModelConfig, OfflineTtsVitsModelConfig, OnlineRecognizer, OnlineRecognizerConfig,
    VadModelConfig, VoiceActivityDetector,
};

use jean_protocol::config::VoiceConfig;

use crate::assets;
use crate::audio::Pcm;
use crate::device::CancelFlag;
use crate::speech::{EngineFactory, Recognizer, Synthesizer};

/// Silero v5 runs at 16 kHz with a 512-sample window. At any other engine rate
/// the VAD is skipped rather than fed audio it cannot interpret.
const VAD_SAMPLE_RATE: u32 = 16000;
const VAD_WINDOW: i32 = 512;

pub struct SherpaEngine {
    config: VoiceConfig,
}

impl SherpaEngine {
    pub fn new(config: &VoiceConfig) -> Result<Self, String> {
        if assets::recognition(config)
            .iter()
            .any(|check| !check.present)
        {
            return Err(assets::native_unavailable(config, true));
        }
        Ok(Self {
            config: config.clone(),
        })
    }

    fn recognizer_config(&self) -> OnlineRecognizerConfig {
        let models = &self.config.models;
        let mut config = OnlineRecognizerConfig::default();
        config.model_config.transducer.encoder = Some(models.encoder.clone());
        config.model_config.transducer.decoder = Some(models.decoder.clone());
        config.model_config.transducer.joiner = Some(models.joiner.clone());
        config.model_config.tokens = Some(models.tokens.clone());
        config.model_config.provider = Some("cpu".into());
        config.model_config.num_threads = 2;
        config.model_config.debug = false;
        config.decoding_method = Some("greedy_search".into());
        // Push-to-talk bounds the utterance, so endpointing must not split one
        // press into several transcripts the owner would have to reassemble.
        config.enable_endpoint = false;
        config
    }

    fn tts_config(&self) -> OfflineTtsConfig {
        let models = &self.config.models;
        let lexicon = if models.tts_lexicon.is_empty() {
            None
        } else {
            Some(models.tts_lexicon.clone())
        };
        let mut model = OfflineTtsModelConfig {
            num_threads: 2,
            debug: false,
            provider: Some("cpu".into()),
            ..Default::default()
        };
        // A speaker table is what distinguishes the two bundles the config can
        // name; neither engine can infer it from a filename.
        if models.tts_voices.is_empty() {
            model.vits = OfflineTtsVitsModelConfig {
                model: Some(models.tts_model.clone()),
                tokens: Some(models.tts_tokens.clone()),
                lexicon,
                ..Default::default()
            };
        } else {
            model.kokoro = OfflineTtsKokoroModelConfig {
                model: Some(models.tts_model.clone()),
                voices: Some(models.tts_voices.clone()),
                tokens: Some(models.tts_tokens.clone()),
                lexicon,
                dict_dir: if models.tts_dict_dir.is_empty() {
                    None
                } else {
                    Some(models.tts_dict_dir.clone())
                },
                lang: if models.tts_lang.is_empty() {
                    None
                } else {
                    Some(models.tts_lang.clone())
                },
                ..Default::default()
            };
        }
        OfflineTtsConfig {
            model,
            max_num_sentences: 1,
            ..Default::default()
        }
    }

    /// Load the synthesis engine. `needs_lexicon` is the text question: rendering
    /// words requires the grapheme-to-phoneme table the Kokoro path demands, and
    /// rendering phonemes that the caller already looked up does not. Every other
    /// asset requirement stands either way, so this cannot turn a missing model
    /// into a phoneme-shaped success.
    fn create_tts(&self, needs_lexicon: bool) -> Result<Box<dyn Synthesizer>, String> {
        if assets::synthesis_for(&self.config, needs_lexicon)
            .iter()
            .any(|c| !c.present)
        {
            return Err(assets::synthesis_unavailable_for(
                &self.config,
                needs_lexicon,
            ));
        }
        let kind = self.synthesis_kind();
        let tts = OfflineTts::create(&self.tts_config()).ok_or_else(|| {
            format!(
                "sherpa-onnx could not create a {kind} synthesizer from {} with tokens {}; \
                 check the model bundle and its license (docs/voice-assets.md §4)",
                self.config.models.tts_model, self.config.models.tts_tokens
            )
        })?;
        Ok(Box::new(SherpaSynthesizer {
            tts,
            sid: self.config.models.tts_speaker as i32,
        }))
    }

    /// Which synthesis engine the current configuration names, for messages that
    /// must not claim VITS about a Kokoro bundle or the other way round.
    fn synthesis_kind(&self) -> &'static str {
        if self.config.models.tts_voices.is_empty() {
            "VITS"
        } else {
            "Kokoro"
        }
    }
}

impl EngineFactory for SherpaEngine {
    fn create_recognizer(&self) -> Result<Box<dyn Recognizer>, String> {
        let recognizer = OnlineRecognizer::create(&self.recognizer_config()).ok_or_else(|| {
            format!(
                "sherpa-onnx could not create a streaming recognizer from {} / {} / {} with tokens {}; \
                 check that the encoder/decoder/joiner are the same exported bundle and that the \
                 library directory matches this build (docs/voice-assets.md)",
                self.config.models.encoder,
                self.config.models.decoder,
                self.config.models.joiner,
                self.config.models.tokens
            )
        })?;
        let vad = self.create_vad();
        Ok(Box::new(SherpaRecognizer {
            recognizer,
            vad,
            stream: None,
            sample_rate: self.config.sample_rate,
        }))
    }

    fn create_synthesizer(&self) -> Result<Box<dyn Synthesizer>, String> {
        self.create_tts(true)
    }

    fn create_phoneme_synthesizer(&self) -> Result<Box<dyn Synthesizer>, String> {
        self.create_tts(false)
    }

    fn label(&self) -> &'static str {
        "sherpa-onnx 2.0.1 (streaming transducer ASR; synthesis engine per config)"
    }
}

impl SherpaEngine {
    fn create_vad(&self) -> Option<VoiceActivityDetector> {
        let path = &self.config.models.vad;
        if path.is_empty() || self.config.sample_rate != VAD_SAMPLE_RATE {
            return None;
        }
        let mut config = VadModelConfig::default();
        config.silero_vad.model = Some(path.clone());
        config.silero_vad.threshold = 0.5;
        config.silero_vad.min_silence_duration = 0.25;
        config.silero_vad.min_speech_duration = 0.25;
        config.silero_vad.window_size = VAD_WINDOW;
        config.silero_vad.max_speech_duration = self.config.capture_timeout_ms as f32 / 1000.0;
        config.sample_rate = VAD_SAMPLE_RATE as i32;
        config.num_threads = 1;
        config.provider = Some("cpu".into());
        config.debug = false;
        VoiceActivityDetector::create(
            &config,
            self.config.capture_timeout_ms as f32 / 1000.0 + 1.0,
        )
    }
}

struct SherpaRecognizer {
    recognizer: OnlineRecognizer,
    vad: Option<VoiceActivityDetector>,
    stream: Option<sherpa_onnx::OnlineStream>,
    sample_rate: u32,
}

impl SherpaRecognizer {
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
                    if let Some(segment) = vad.front() {
                        stream.accept_waveform(self.sample_rate as i32, segment.samples());
                    }
                    vad.pop();
                }
            }
            None => stream.accept_waveform(self.sample_rate as i32, samples),
        }
        self.decode_ready(cancel)
    }
}

impl Recognizer for SherpaRecognizer {
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
        if cancel.is_cancelled() {
            return Err("recognition cancelled".into());
        }
        self.accept(&pcm.samples, cancel)
    }

    fn finish(&mut self, cancel: &CancelFlag) -> Result<String, String> {
        if cancel.is_cancelled() {
            return Err("recognition cancelled".into());
        }
        if let Some(vad) = self.vad.as_ref() {
            vad.flush();
            while !vad.is_empty() {
                if let Some(segment) = vad.front() {
                    if let Some(stream) = self.stream.as_ref() {
                        stream.accept_waveform(self.sample_rate as i32, segment.samples());
                    }
                }
                vad.pop();
            }
        }
        // Tail padding lets the transducer emit its last frames, and the length is
        // measured rather than guessed: at 0.3 s the bundle's 8 kHz clip lost its final
        // word (`…APPREHENS`) while adding 100 ms of silence to the file recovered it,
        // so the model wants roughly 0.4 s of trailing audio after the speech. 0.wav and
        // 1.wav need nothing because they end in their own recorded silence. Bounded, so
        // a release cannot turn into an unbounded decode.
        let tail = (self.sample_rate as f32 * 0.5).round() as usize;
        if let Some(stream) = self.stream.as_ref() {
            stream.accept_waveform(self.sample_rate as i32, &vec![0.0; tail]);
            stream.input_finished();
        }
        self.decode_ready(cancel)?;
        let text = self
            .stream
            .as_ref()
            .and_then(|stream| self.recognizer.get_result(stream))
            .map(|result| result.text.trim().to_owned())
            .unwrap_or_default();
        self.stream = None;
        Ok(text)
    }
}

struct SherpaSynthesizer {
    tts: OfflineTts,
    sid: i32,
}

impl SherpaSynthesizer {
    /// One generation call, two ways in. `text` is what the engine tokenizes itself;
    /// `phoneme_codepoints` is what the caller already converted.
    fn render(
        &mut self,
        text: &str,
        mut generation: GenerationConfig,
        cancel: &CancelFlag,
        what: &str,
    ) -> Result<Pcm, String> {
        if cancel.is_cancelled() {
            return Err("synthesis cancelled".into());
        }
        generation.sid = self.sid;
        generation.speed = 1.0;
        let flag = cancel.clone();
        let audio = self
            .tts
            .generate_with_config(
                text,
                &generation,
                // Returning false aborts generation mid-utterance. This is the
                // difference between a stop that lands now and one that lands
                // after the whole sentence has been synthesized.
                Some(move |_samples: &[f32], _progress: f32| !flag.is_cancelled()),
            )
            .ok_or_else(|| {
                if cancel.is_cancelled() {
                    "synthesis cancelled".to_owned()
                } else {
                    format!("sherpa-onnx returned no audio for {what}")
                }
            })?;
        if cancel.is_cancelled() {
            return Err("synthesis cancelled".into());
        }
        Ok(Pcm {
            sample_rate: audio.sample_rate() as u32,
            samples: audio.samples().to_vec(),
        })
    }
}

impl Synthesizer for SherpaSynthesizer {
    fn synthesize(&mut self, text: &str, cancel: &CancelFlag) -> Result<Pcm, String> {
        self.render(
            text,
            GenerationConfig::default(),
            cancel,
            "the requested text",
        )
    }

    fn synthesize_phonemes(
        &mut self,
        codepoints: &[i32],
        cancel: &CancelFlag,
    ) -> Result<Pcm, String> {
        self.render(
            "",
            GenerationConfig {
                phoneme_codepoints: Some(vec![codepoints.to_vec()]),
                ..Default::default()
            },
            cancel,
            "the phoneme sequence",
        )
    }
}
