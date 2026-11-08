//! The voice CLI surface, through the real binary.
//!
//! These tests run `jean voice doctor`, which reads configuration and asset
//! paths only. They never enumerate endpoints, open a device, play audio or
//! change any system setting — the enumeration and live-audio steps stay in the
//! provisioning document for a machine that has the speech assets.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn with_config(tag: &str, toml: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!(
                "p5e-cli-{tag}-{}",
                jean_protocol::ipc::new_task_id()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("jean.toml"), toml).unwrap();
        Self { dir }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
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
        command.env("JEAN_DATA_DIR", &self.dir);
        for (key, value) in extra {
            command.env(key, value);
        }
        command.args(args).output().expect("run jean")
    }

    fn doctor(&self) -> (bool, String) {
        let out = self.run(&["voice", "doctor"]);
        let text = String::from_utf8_lossy(if out.status.success() {
            &out.stdout
        } else {
            &out.stderr
        })
        .into_owned();
        (out.status.success(), text)
    }

    /// A file to point an asset key at, so `present` can be proven true.
    fn asset(&self, name: &str) -> String {
        let path = self.dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"not a real model; only its existence is under test")
            .unwrap();
        path.to_string_lossy().into_owned()
    }
}

const UNPROVISIONED: &str = "\
[model]
provider = 'none'

[voice]
enabled = false
backend = 'sherpa_onnx'
";

#[test]
fn doctor_names_the_absent_feature_instead_of_inventing_a_backend() {
    let fixture = Fixture::with_config("doctor-off", UNPROVISIONED);
    let (ok, text) = fixture.doctor();
    assert!(ok, "{text}");
    let report: serde_json::Value = serde_json::from_str(text.trim()).expect(&text);
    assert_eq!(report["enabled"], serde_json::json!(false));
    assert_eq!(report["sherpa_feature_linked"], serde_json::json!(false));
    assert_eq!(report["engine"]["available"], serde_json::json!(false));
    let reason = report["engine"]["reason"].as_str().unwrap();
    assert!(reason.contains("without the `sherpa` feature"), "{reason}");
    assert!(reason.contains("voice.models.encoder is unset"), "{reason}");
    assert!(
        reason.contains("never reported as speech"),
        "the doctor must not let a placeholder look like working speech: {reason}"
    );
    assert_eq!(report["audio_backend"], serde_json::json!("System"));
    assert_eq!(report["mode"], serde_json::json!("push_to_talk"));
    assert_eq!(report["hotkey"], serde_json::json!("Ctrl+Alt+Space"));
    assert_eq!(report["required_assets"].as_array().unwrap().len(), 6);
}

#[test]
fn a_fixture_backend_is_reported_as_a_test_double_with_no_device() {
    let fixture = Fixture::with_config(
        "doctor-fixture",
        "\
[model]
provider = 'none'

[voice]
enabled = true
backend = 'fixture'
",
    );
    let (ok, text) = fixture.doctor();
    assert!(ok, "{text}");
    let report: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(report["engine"]["available"], serde_json::json!(true));
    let label = report["engine"]["label"].as_str().unwrap();
    assert!(label.contains("fixture"), "{label}");
    assert!(label.contains("no inference"), "{label}");
    assert_eq!(report["audio_backend"], serde_json::json!("Fixture"));
}

#[test]
fn every_new_voice_key_reaches_the_worker_and_asset_paths_are_measured() {
    let fixture = Fixture::with_config("keys", UNPROVISIONED);
    let encoder = fixture.asset("encoder.onnx");
    let dict_dir = fixture.dir.display();
    std::fs::write(
        fixture.dir.join("jean.toml"),
        format!(
            "\
[model]
provider = 'none'

[voice]
enabled = true
backend = 'sherpa_onnx'
configure_timeout_ms = 45000
finalize_budget_ms = 900
capture_buffer_ms = 1500
sample_rate = 16000
input_device = '{{0.0.1.00000000}}{{AAAAAAAA}}##1234'
output_device = ''
hotkey = 'Ctrl+Shift+F6'
announce_task_state = true
spoken_controls = true

[voice.models]
encoder = '{encoder}'
decoder = '{encoder}'
joiner = '{encoder}'
tokens = '{encoder}'
tts_voices = '{encoder}'
tts_dict_dir = '{dict_dir}'
tts_lang = 'en-us'
tts_speaker = 3
vad = ''
native_lib_dir = ''
"
        ),
    )
    .unwrap();
    let (ok, text) = fixture.doctor();
    assert!(ok, "{text}");
    let report: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(report["hotkey"], serde_json::json!("Ctrl+Shift+F6"));
    let required = report["required_assets"].as_array().unwrap();
    let encoder = required
        .iter()
        .find(|entry| entry["key"] == serde_json::json!("voice.models.encoder"))
        .unwrap();
    assert_eq!(encoder["present"], serde_json::json!(true));
    assert!(
        required
            .iter()
            .any(|entry| entry["present"] == serde_json::json!(false)),
        "one present asset must not make the whole bundle look provisioned"
    );
    // The two bundles are provisioned separately, so the report must show them
    // separately: recognition complete, synthesis still missing its model.
    let recognition = report["recognition_assets"].as_array().unwrap();
    assert_eq!(recognition.len(), 4, "{recognition:?}");
    assert!(
        recognition
            .iter()
            .all(|entry| entry["present"] == serde_json::json!(true)),
        "the four recognition keys were all pointed at a real file: {recognition:?}"
    );
    let synthesis = report["synthesis_assets"].as_array().unwrap();
    assert!(
        synthesis
            .iter()
            .any(|entry| entry["key"] == serde_json::json!("voice.models.tts_voices")),
        "naming a speaker table must move the config onto the Kokoro asset list: {synthesis:?}"
    );
    assert!(
        synthesis.iter().any(|entry| entry["key"]
            == serde_json::json!("voice.models.tts_dict_dir")
            && entry["present"] == serde_json::json!(true)),
        "a directory key must be measured as a directory: {synthesis:?}"
    );
    assert!(
        synthesis.iter().any(
            |entry| entry["key"] == serde_json::json!("voice.models.tts_model")
                && entry["present"] == serde_json::json!(false)
        ),
        "a complete recognition bundle must not make synthesis look provisioned"
    );
}

#[test]
fn an_unparseable_hotkey_is_refused_before_any_worker_starts() {
    for bad in ["Space", "Ctrl+", "Ctrl+F25", "Cmd+Space"] {
        let fixture = Fixture::with_config(
            "hotkey",
            &format!(
                "\
[model]
provider = 'none'

[voice]
enabled = true
hotkey = '{bad}'
"
            ),
        );
        let (ok, text) = fixture.doctor();
        assert!(!ok, "{bad} should not validate");
        assert!(
            text.contains("hotkey"),
            "{bad}: the error must name the key it rejected: {text}"
        );
    }
}

#[test]
fn a_finalize_budget_that_outruns_the_request_deadline_is_refused() {
    let fixture = Fixture::with_config(
        "budget",
        "\
[model]
provider = 'none'

[voice]
enabled = true
request_timeout_ms = 800
finalize_budget_ms = 900
",
    );
    let (ok, text) = fixture.doctor();
    assert!(!ok, "{text}");
    assert!(
        text.contains("finalize budget must stay below the request deadline"),
        "{text}"
    );
}

#[test]
fn voice_stays_off_when_the_environment_switch_is_set() {
    let fixture = Fixture::with_config(
        "no-voice",
        "\
[model]
provider = 'none'

[voice]
enabled = true
backend = 'fixture'
",
    );
    let out = fixture.run_with_env(&["voice", "doctor"], &[("JEAN_NO_VOICE", "1")]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{text}");
    let report: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(
        report["enabled"],
        serde_json::json!(false),
        "JEAN_NO_VOICE must still win over the file"
    );
}

/// One second of 16-bit mono silence at 16 kHz, as a RIFF WAVE byte string.
fn pcm16_wav(seconds: u32, sample_rate: u32) -> Vec<u8> {
    let frames = seconds * sample_rate;
    let data = vec![0u8; (frames * 2) as usize];
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

#[test]
fn a_prerecorded_clip_is_transcribed_with_no_device_opened() {
    let fixture = Fixture::with_config(
        "transcribe",
        "\
[model]
provider = 'none'

[voice]
enabled = true
backend = 'fixture'
fixture_transcript = 'open the process monitor'
",
    );
    let clip = fixture.dir.join("known-phrase.wav");
    std::fs::write(&clip, pcm16_wav(1, 16000)).unwrap();
    let out = fixture.run(&["voice", "transcribe", &clip.to_string_lossy()]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{text}");
    let report: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(
        report["transcript"],
        serde_json::json!("open the process monitor")
    );
    assert_eq!(report["duration_ms"], serde_json::json!(1000));
    assert_eq!(report["frames"], serde_json::json!(16000));
    let label = report["engine"]["label"]
        .as_str()
        .or(report["engine"].as_str())
        .unwrap_or("");
    assert!(
        label.contains("fixture") || text.contains("fixture"),
        "a fixture transcript must never read as inference: {text}"
    );
}

#[test]
fn a_file_that_is_not_audio_fails_with_the_shape_it_expected() {
    let fixture = Fixture::with_config(
        "transcribe-bad",
        "\
[model]
provider = 'none'

[voice]
enabled = true
backend = 'fixture'
",
    );
    let clip = fixture.dir.join("notes.txt");
    std::fs::write(&clip, b"this is not a wave file").unwrap();
    let out = fixture.run(&["voice", "transcribe", &clip.to_string_lossy()]);
    assert!(!out.status.success(), "a text file must not transcribe");
    let text = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(text.contains("RIFF"), "{text}");
}
