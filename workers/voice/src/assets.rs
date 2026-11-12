//! Asset diagnostics. Every message names the configuration key to fix and the
//! document that lists the download, because "model not found" on its own sends
//! an operator looking in the wrong place.

use jean_protocol::config::{SpeechRecognizer, VoiceConfig};

pub struct AssetCheck {
    pub key: &'static str,
    pub path: String,
    pub present: bool,
}

fn check(key: &'static str, path: &str, dir: bool) -> AssetCheck {
    let present = !path.is_empty()
        && if dir {
            std::path::Path::new(path).is_dir()
        } else {
            std::path::Path::new(path).is_file()
        };
    AssetCheck {
        key,
        path: path.into(),
        present,
    }
}

/// The four files streaming recognition needs. Deliberately separate from
/// synthesis: the ASR bundle and a voice bundle are different downloads with
/// different licences, and a build that had one must not refuse to speak
/// because it lacks the other.
pub fn recognition(config: &VoiceConfig) -> Vec<AssetCheck> {
    let models = &config.models;
    let entries = match config.recognizer {
        SpeechRecognizer::Zipformer => vec![
            ("voice.models.encoder", models.encoder.as_str()),
            ("voice.models.decoder", models.decoder.as_str()),
            ("voice.models.joiner", models.joiner.as_str()),
            ("voice.models.tokens", models.tokens.as_str()),
        ],
        SpeechRecognizer::ParakeetTdtV2Int8 => vec![
            (
                "voice.models.parakeet.encoder",
                models.parakeet.encoder.as_str(),
            ),
            (
                "voice.models.parakeet.decoder",
                models.parakeet.decoder.as_str(),
            ),
            (
                "voice.models.parakeet.joiner",
                models.parakeet.joiner.as_str(),
            ),
            (
                "voice.models.parakeet.tokens",
                models.parakeet.tokens.as_str(),
            ),
        ],
    };
    entries
        .into_iter()
        .map(|(key, path)| check(key, path, false))
        .collect()
}

/// What synthesis needs, which depends on the bundle: VITS takes a model and a
/// token table, Kokoro additionally takes a speaker table. Setting `tts_voices` is
/// what selects Kokoro.
///
/// `tts_dict_dir` is listed only when it is set. The English Kokoro archive does
/// ship an `espeak-ng-data` directory, and 2.0.1 reports that espeak-ng is no
/// longer used, so a run that does not name one is not missing anything.
///
/// `needs_lexicon` is the phoneme question: text input has to be converted to
/// phonemes before the model can render it, and Kokoro on 2.0.1 does that only
/// from a lexicon file. Input that is already phonemes skips that step, so the
/// requirement is dropped for it and nothing else is.
pub fn synthesis_for(config: &VoiceConfig, needs_lexicon: bool) -> Vec<AssetCheck> {
    let models = &config.models;
    let mut checks = vec![
        check("voice.models.tts_model", &models.tts_model, false),
        check("voice.models.tts_tokens", &models.tts_tokens, false),
    ];
    if !models.tts_voices.is_empty() {
        checks.push(check("voice.models.tts_voices", &models.tts_voices, false));
        // Measured with `kokoro-en-v0_19` on sherpa-onnx 2.0.1: the engine accepts the
        // model, voices and tokens, then refuses at tokenization with "No lexicon
        // available. Please provide --kokoro-lexicon", and warns that espeak-ng is no
        // longer used. So for this engine the lexicon is not the optional extra it is
        // for VITS — reporting it as one would call a unusable bundle ready.
        if needs_lexicon {
            checks.push(check(
                "voice.models.tts_lexicon",
                &models.tts_lexicon,
                false,
            ));
        }
    }
    if !models.tts_dict_dir.is_empty() {
        checks.push(check(
            "voice.models.tts_dict_dir",
            &models.tts_dict_dir,
            true,
        ));
    }
    checks
}

/// The text path: everything synthesis needs, including the lexicon.
pub fn synthesis(config: &VoiceConfig) -> Vec<AssetCheck> {
    synthesis_for(config, true)
}

/// Every asset the configuration names, for diagnostics that list the whole
/// state at once.
pub fn required(config: &VoiceConfig) -> Vec<AssetCheck> {
    let mut all = recognition(config);
    all.extend(synthesis(config));
    all
}

/// VAD is optional and reported separately, so a missing silence model is never
/// the reason speech refuses.
pub fn optional(config: &VoiceConfig) -> Vec<AssetCheck> {
    let models = &config.models;
    let mut entries: Vec<(&'static str, &str, bool)> = vec![
        ("voice.models.vad", &models.vad, false),
        ("voice.models.native_lib_dir", &models.native_lib_dir, true),
    ];
    // A Kokoro configuration has already been asked for its lexicon by `synthesis`;
    // listing it here as well would report one file as both required and optional.
    if models.tts_voices.is_empty() {
        entries.push(("voice.models.tts_lexicon", &models.tts_lexicon, false));
    }
    entries
        .into_iter()
        .map(|(key, path, dir)| check(key, path, dir))
        .collect()
}

fn describe_missing(checks: &[AssetCheck]) -> String {
    let absent: Vec<String> = checks
        .iter()
        .filter(|c| !c.present)
        .map(|c| {
            if c.path.is_empty() {
                format!("{} is unset", c.key)
            } else {
                format!("{} = `{}` does not exist", c.key, c.path)
            }
        })
        .collect();
    if absent.is_empty() {
        "none".into()
    } else {
        absent.join("; ")
    }
}

/// Why the native backend cannot serve, in the order an operator can act on.
///
/// `linked` is a compile-time fact, not a guess: a worker built without the
/// feature says so, and never reports the fixture as speech.
pub fn native_unavailable(config: &VoiceConfig, linked: bool) -> String {
    let missing = describe_missing(&recognition(config));
    if !linked {
        return format!(
            "this jean-voice was built without the `sherpa` feature, so no native speech is \
             linked. Provision the assets in docs/voice-assets.md, then rebuild with \
             `--features sherpa` and SHERPA_ONNX_LIB_DIR set to the extracted library \
             directory. Missing recognition assets: \
             {missing}. The fixture backend is a test double and is never reported as speech."
        );
    }
    let recognizer = match config.recognizer {
        SpeechRecognizer::Zipformer => "zipformer (streaming transducer)",
        SpeechRecognizer::ParakeetTdtV2Int8 => "parakeet_tdt_v2_int8 (offline Nemo transducer)",
    };
    format!(
        "sherpa-onnx is linked but selected recognizer {recognizer} has unusable assets: {missing}. \
         Point the voice.models keys at real files (docs/voice-assets.md lists the bundles, \
         versions, checksums and licenses). Optional assets: {}.",
        describe_missing(&optional(config))
    )
}

/// Why synthesis specifically cannot serve. Recognition may be perfectly healthy
/// when this is reported, and the message says so rather than taking the whole
/// voice plane down with the missing voice bundle.
pub fn synthesis_unavailable(config: &VoiceConfig) -> String {
    synthesis_unavailable_for(config, true)
}

/// The same refusal for one of the two ways into the synthesizer. The phoneme
/// path names a lexicon it does not need, so it must not be told to go find one.
pub fn synthesis_unavailable_for(config: &VoiceConfig, needs_lexicon: bool) -> String {
    format!(
        "speech synthesis is unavailable: {}. Recognition is unaffected. \
         docs/voice-assets.md §4 records which bundles may be redistributed and what each \
         key must point at.",
        describe_missing(&synthesis_for(config, needs_lexicon))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use jean_protocol::config::{SpeechBackend, SpeechModels, SpeechRecognizer};

    #[test]
    fn an_unprovisioned_default_names_every_unset_key_and_the_feature() {
        let config = VoiceConfig {
            backend: SpeechBackend::SherpaOnnx,
            ..VoiceConfig::default()
        };
        let message = native_unavailable(&config, false);
        assert!(message.contains("without the `sherpa` feature"));
        assert!(message.contains("voice.models.encoder is unset"));
        assert!(message.contains("docs/voice-assets.md"));
        assert!(message.contains("never reported as speech"));
        assert_eq!(required(&config).iter().filter(|c| c.present).count(), 0);
    }

    #[test]
    fn a_linked_build_blames_the_assets_not_the_feature() {
        let config = VoiceConfig::default();
        let message = native_unavailable(&config, true);
        assert!(!message.contains("without the `sherpa` feature"));
        assert!(message.contains("voice.models.decoder is unset"));
        assert!(message.contains("Optional assets"));
    }

    #[test]
    fn parakeet_reports_its_separate_bundle_and_actionable_missing_keys() {
        let config = VoiceConfig {
            recognizer: SpeechRecognizer::ParakeetTdtV2Int8,
            ..Default::default()
        };
        let checks = recognition(&config);
        assert_eq!(checks.len(), 4);
        assert!(checks.iter().all(|check| !check.present));
        assert!(checks
            .iter()
            .all(|check| check.key.starts_with("voice.models.parakeet.")));
        let error = native_unavailable(&config, true);
        assert!(
            error.contains("voice.models.parakeet.encoder is unset"),
            "{error}"
        );
        assert!(error.contains("docs/voice-assets.md"), "{error}");
        assert!(!error.contains("voice.models.encoder is unset"), "{error}");
    }

    #[test]
    fn a_path_that_does_not_exist_is_reported_with_its_key() {
        let mut config = VoiceConfig::default();
        config.models.encoder = "Z:/definitely-not-here/encoder.onnx".into();
        let checks = required(&config);
        let encoder = checks
            .iter()
            .find(|c| c.key == "voice.models.encoder")
            .unwrap();
        assert!(!encoder.present);
        assert!(native_unavailable(&config, true).contains(
            "voice.models.encoder = `Z:/definitely-not-here/encoder.onnx` does not exist"
        ));
    }

    /// The whole point of splitting the lists: an ASR-only machine must recognise.
    #[test]
    fn recognition_assets_alone_leave_synthesis_the_only_complaint() {
        let real = std::env::current_exe().unwrap();
        let path = real.to_str().unwrap().to_string();
        let config = VoiceConfig {
            models: SpeechModels {
                encoder: path.clone(),
                decoder: path.clone(),
                joiner: path.clone(),
                tokens: path,
                ..SpeechModels::default()
            },
            ..VoiceConfig::default()
        };
        assert!(
            recognition(&config).iter().all(|c| c.present),
            "the four recognition files are all the test binary itself"
        );
        assert!(
            !native_unavailable(&config, true).contains("voice.models.tts_model"),
            "recognition must not be blocked by a synthesis bundle it does not need"
        );
        let message = synthesis_unavailable(&config);
        assert!(message.contains("voice.models.tts_model is unset"));
        assert!(message.contains("Recognition is unaffected"));
    }

    #[test]
    fn naming_a_kokoro_speaker_table_asks_for_the_files_kokoro_needs() {
        let mut config = VoiceConfig::default();
        config.models.tts_voices = "Z:/no/voices.bin".into();
        let keys: Vec<&str> = synthesis(&config)
            .iter()
            .map(|c| c.key)
            .filter(|k| k.contains("tts"))
            .collect();
        assert!(
            keys.contains(&"voice.models.tts_voices"),
            "a VITS-shaped list would let Kokoro fail at load with no hint: {keys:?}"
        );
        assert!(
            !keys.contains(&"voice.models.tts_dict_dir"),
            "an unset key must not be reported as a blocker: {keys:?}"
        );
        config.models.tts_dict_dir = "Z:/no/dict".into();
        let listed = synthesis(&config);
        let dict = listed
            .iter()
            .find(|c| c.key == "voice.models.tts_dict_dir")
            .expect("a set dictionary directory is a real asset to measure");
        assert!(!dict.present, "it is checked as a path, not as a guess");

        // Measured: 2.0.1 loads a Kokoro model, voices and tokens and then refuses at
        // tokenization for want of a lexicon, so the list has to name it. A bundle the
        // engine cannot use must not be reported as ready.
        let lexicon = listed
            .iter()
            .find(|c| c.key == "voice.models.tts_lexicon")
            .expect("Kokoro needs a lexicon; it is not optional for this engine");
        assert!(!lexicon.present, "the path is unset here");
        assert!(
            optional(&config)
                .iter()
                .all(|c| c.key != "voice.models.tts_lexicon"),
            "one key must not be reported as both required and optional"
        );

        // Pre-phonemized input never reaches the grapheme-to-phoneme table, so the
        // lexicon must not gate it — while every other Kokoro asset still does. A
        // seam that dropped the model check too would report an empty bundle as
        // synthesized speech.
        let phoneme_keys: Vec<&str> = synthesis_for(&config, false)
            .iter()
            .map(|c| c.key)
            .collect();
        assert!(
            !phoneme_keys.contains(&"voice.models.tts_lexicon"),
            "the phoneme path does not use a lexicon: {phoneme_keys:?}"
        );
        for required in [
            "voice.models.tts_model",
            "voice.models.tts_tokens",
            "voice.models.tts_voices",
        ] {
            assert!(
                phoneme_keys.contains(&required),
                "the phoneme path still needs {required}: {phoneme_keys:?}"
            );
        }
        assert!(
            !synthesis_unavailable_for(&config, false).contains("tts_lexicon"),
            "a refusal for phoneme input must not tell the operator to go find a lexicon"
        );

        // VITS still treats it as the optional pronunciation list it has always been.
        config.models.tts_voices.clear();
        assert!(
            synthesis(&config)
                .iter()
                .all(|c| c.key != "voice.models.tts_lexicon"),
            "without a speaker table this is a VITS bundle"
        );
        assert!(
            optional(&config)
                .iter()
                .any(|c| c.key == "voice.models.tts_lexicon"),
            "and the lexicon belongs in the optional list again"
        );
    }
}
