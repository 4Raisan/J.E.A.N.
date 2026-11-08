//! The two long-lived inference threads.
//!
//! The control loop never calls a model. It sends a request and drains events;
//! recognition owns the microphone and the recognizer, synthesis owns the
//! synthesizer and the speaker, and each notices cancellation between steps.
//! That is what keeps `Interrupt`, `Ping` and `Shutdown` answerable while a long
//! decode or a long synthesis is in flight.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use jean_protocol::config::VoiceConfig;
use jean_protocol::interaction;

use crate::audio::{BoundedPcm, Pcm, Resampler};
use crate::device::{AudioBackend, CancelFlag};
use crate::speech::EngineFactory;

pub enum RecognitionRequest {
    Begin {
        activation: u64,
        cancel: CancelFlag,
        release: CancelFlag,
    },
    Shutdown,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecognitionEvent {
    Loaded,
    LoadFailed {
        reason: String,
    },
    Opened {
        activation: u64,
    },
    OpenFailed {
        activation: u64,
        reason: String,
    },
    Transcript {
        activation: u64,
        text: String,
        dropped: u64,
    },
    Failed {
        activation: u64,
        reason: String,
    },
}

pub enum SynthesisRequest {
    Speak {
        utterance: u64,
        text: String,
        cancel: CancelFlag,
    },
    Shutdown,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SynthesisEvent {
    Loaded,
    LoadFailed { reason: String },
    Stopped { utterance: u64 },
    Failed { utterance: u64, reason: String },
}

pub fn recognition_thread(
    config: VoiceConfig,
    factory: Arc<dyn EngineFactory>,
    audio: Arc<dyn AudioBackend>,
    requests: mpsc::Receiver<RecognitionRequest>,
    events: mpsc::SyncSender<RecognitionEvent>,
) {
    let mut recognizer = match factory.create_recognizer() {
        Ok(recognizer) => {
            let _ = events.send(RecognitionEvent::Loaded);
            recognizer
        }
        Err(reason) => {
            let _ = events.send(RecognitionEvent::LoadFailed { reason });
            return;
        }
    };
    while let Ok(request) = requests.recv() {
        match request {
            RecognitionRequest::Shutdown => break,
            RecognitionRequest::Begin {
                activation,
                cancel,
                release,
            } => {
                capture_once(
                    &config,
                    audio.as_ref(),
                    recognizer.as_mut(),
                    activation,
                    &cancel,
                    &release,
                    &events,
                );
            }
        }
    }
}

/// One push-to-talk press: open, capture, resample, bound, feed, finalize.
///
/// Public so a test can drive it with a deliberately slow recognizer and prove
/// that a stop request during a long decode is answered without a transcript.
pub fn capture_once(
    config: &VoiceConfig,
    audio: &dyn AudioBackend,
    recognizer: &mut dyn crate::speech::Recognizer,
    activation: u64,
    cancel: &CancelFlag,
    release: &CancelFlag,
    events: &mpsc::SyncSender<RecognitionEvent>,
) {
    let mut capture = match audio.open_capture(&config.input_device) {
        Ok(capture) => capture,
        Err(error) => {
            let _ = events.send(RecognitionEvent::OpenFailed {
                activation,
                reason: error.to_string(),
            });
            return;
        }
    };
    if cancel.is_cancelled() {
        return;
    }
    let _ = events.send(RecognitionEvent::Opened { activation });
    if let Err(reason) = recognizer.begin() {
        let _ = events.send(RecognitionEvent::Failed { activation, reason });
        return;
    }
    let device_rate = capture.format().sample_rate;
    let mut resampler = Resampler::new(device_rate, config.sample_rate);
    let mut buffer = BoundedPcm::for_duration(config.sample_rate, config.capture_buffer_ms);
    let mut failure: Option<String> = None;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        match capture.read() {
            Ok(Some(packet)) => {
                let resampled = resampler.push(&packet.samples);
                buffer.push(&resampled);
                let frames = buffer.take();
                if !frames.is_empty() {
                    let chunk = Pcm {
                        sample_rate: config.sample_rate,
                        samples: frames,
                    };
                    if let Err(reason) = recognizer.feed(&chunk, cancel) {
                        if cancel.is_cancelled() {
                            return;
                        }
                        failure = Some(reason);
                        break;
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                failure = Some(error.to_string());
                break;
            }
        }
        if release.is_cancelled() {
            break;
        }
    }
    if cancel.is_cancelled() {
        return;
    }
    if let Some(reason) = failure {
        let _ = events.send(RecognitionEvent::Failed { activation, reason });
        return;
    }
    let tail = resampler.flush();
    if !tail.is_empty() {
        let chunk = Pcm {
            sample_rate: config.sample_rate,
            samples: tail,
        };
        if recognizer.feed(&chunk, cancel).is_err() && cancel.is_cancelled() {
            return;
        }
    }
    match recognizer.finish(cancel) {
        Ok(_) if cancel.is_cancelled() => {
            // A release that was cancelled mid-flush produces no transcript, even
            // from an engine that finished anyway: the operator already let go of
            // the push-to-talk key and the activation is gone.
        }
        Ok(text) if interaction::text_valid(&text) => {
            let _ = events.send(RecognitionEvent::Transcript {
                activation,
                text,
                dropped: buffer.dropped(),
            });
        }
        Ok(_) => {
            let _ = events.send(RecognitionEvent::Failed {
                activation,
                reason: "the recognizer returned an empty or oversized transcript".into(),
            });
        }
        Err(reason) => {
            if !cancel.is_cancelled() {
                let _ = events.send(RecognitionEvent::Failed { activation, reason });
            }
        }
    }
}

pub fn synthesis_thread(
    config: VoiceConfig,
    factory: Arc<dyn EngineFactory>,
    audio: Arc<dyn AudioBackend>,
    requests: mpsc::Receiver<SynthesisRequest>,
    events: mpsc::SyncSender<SynthesisEvent>,
) {
    let mut synthesizer = match factory.create_synthesizer() {
        Ok(synthesizer) => {
            let _ = events.send(SynthesisEvent::Loaded);
            synthesizer
        }
        Err(reason) => {
            let _ = events.send(SynthesisEvent::LoadFailed { reason });
            return;
        }
    };
    while let Ok(request) = requests.recv() {
        match request {
            SynthesisRequest::Shutdown => break,
            SynthesisRequest::Speak {
                utterance,
                text,
                cancel,
            } => {
                speak_once(
                    &config,
                    audio.as_ref(),
                    synthesizer.as_mut(),
                    utterance,
                    &text,
                    &cancel,
                    &events,
                );
            }
        }
    }
}

/// One utterance: synthesize, resample to the endpoint, write, drain, stop.
pub fn speak_once(
    config: &VoiceConfig,
    audio: &dyn AudioBackend,
    synthesizer: &mut dyn crate::speech::Synthesizer,
    utterance: u64,
    text: &str,
    cancel: &CancelFlag,
    events: &mpsc::SyncSender<SynthesisEvent>,
) {
    if cancel.is_cancelled() {
        let _ = events.send(SynthesisEvent::Stopped { utterance });
        return;
    }
    let pcm = match synthesizer.synthesize(text, cancel) {
        Ok(pcm) => pcm,
        Err(reason) => {
            let event = if cancel.is_cancelled() {
                SynthesisEvent::Stopped { utterance }
            } else {
                SynthesisEvent::Failed { utterance, reason }
            };
            let _ = events.send(event);
            return;
        }
    };
    if cancel.is_cancelled() {
        let _ = events.send(SynthesisEvent::Stopped { utterance });
        return;
    }
    let mut render = match audio.open_render(&config.output_device) {
        Ok(render) => render,
        Err(error) => {
            let _ = events.send(SynthesisEvent::Failed {
                utterance,
                reason: error.to_string(),
            });
            return;
        }
    };
    let mut resampler = Resampler::new(pcm.sample_rate, render.format().sample_rate);
    let frames = resampler.push(&pcm.samples);
    let mut offset = 0;
    let mut failure = None;
    while offset < frames.len() && !cancel.is_cancelled() {
        match render.write(&frames[offset..]) {
            Ok(0) => std::thread::sleep(Duration::from_millis(5)),
            Ok(written) => offset += written,
            Err(error) => {
                failure = Some(error.to_string());
                break;
            }
        }
    }
    if let Some(reason) = failure {
        render.stop();
        let _ = events.send(SynthesisEvent::Failed { utterance, reason });
        return;
    }
    let _ = render.drain(cancel);
    render.stop();
    let _ = events.send(SynthesisEvent::Stopped { utterance });
}
