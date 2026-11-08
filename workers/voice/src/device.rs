//! Audio device boundary: enumeration, capture and render traits, plus the
//! deterministic fixture backend.
//!
//! The traits are deliberately not `Send`: a WASAPI stream is created and used on
//! one thread, in one COM apartment, and moving it would be the bug. The backend
//! itself is `Send + Sync` because the control loop, the recognition thread and
//! the synthesis thread each ask it to open something on their own thread.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use crate::audio::{AudioError, DeviceFormat, Pcm, SampleFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Capture,
    Render,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub default: bool,
    /// Known only after the endpoint is opened. Enumeration reports `None`
    /// rather than guessing a mix format it has not read.
    pub format: Option<DeviceFormat>,
}

/// A cancellation signal shared with an inference or playback thread.
///
/// This is the utterance scope. It never reaches the task worker: stopping speech
/// sets this flag and nothing else.
#[derive(Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub trait CaptureStream {
    fn format(&self) -> DeviceFormat;
    /// Wait briefly for the next packet. `Ok(None)` means nothing is ready yet,
    /// which is how the caller gets a chance to notice cancellation.
    fn read(&mut self) -> Result<Option<Pcm>, AudioError>;
}

pub trait RenderStream {
    fn format(&self) -> DeviceFormat;
    /// Accept up to the device's free buffer; returns the frames taken.
    fn write(&mut self, frames: &[f32]) -> Result<usize, AudioError>;
    /// Wait for the accepted frames to reach the speaker, noticing cancellation.
    fn drain(&mut self, cancel: &CancelFlag) -> Result<(), AudioError>;
    /// Silence the endpoint immediately, without waiting for the buffer.
    fn stop(&mut self);
}

pub trait AudioBackend: Send + Sync {
    fn devices(&self) -> Result<Vec<AudioDevice>, AudioError>;
    fn open_capture(&self, id: &str) -> Result<Box<dyn CaptureStream>, AudioError>;
    fn open_render(&self, id: &str) -> Result<Box<dyn RenderStream>, AudioError>;
}

/// The system backend on a platform this build has no audio path for. It refuses
/// every operation rather than silently substituting the fixture.
pub struct UnsupportedAudio;

impl AudioBackend for UnsupportedAudio {
    fn devices(&self) -> Result<Vec<AudioDevice>, AudioError> {
        Err(AudioError::Open(
            "no system audio backend is compiled for this platform; WASAPI is Windows-only".into(),
        ))
    }

    fn open_capture(&self, _id: &str) -> Result<Box<dyn CaptureStream>, AudioError> {
        Err(AudioError::Open(
            "no system capture backend is compiled for this platform".into(),
        ))
    }

    fn open_render(&self, _id: &str) -> Result<Box<dyn RenderStream>, AudioError> {
        Err(AudioError::Open(
            "no system render backend is compiled for this platform".into(),
        ))
    }
}

/// What the fixture endpoints deliver, and when they misbehave.
#[derive(Clone)]
pub struct FixtureScript {
    pub packets: Vec<Pcm>,
    /// Fail with device loss after this many packets have been delivered.
    pub lose_after: Option<usize>,
    /// Refuse the open outright, as an endpoint with an unsupported format does.
    pub refuse_open: bool,
    /// Fail the first render write, as a speaker that disappeared mid-call does.
    pub render_fails: bool,
}

impl Default for FixtureScript {
    fn default() -> Self {
        Self {
            packets: vec![Pcm::silent(16000, 160)],
            lose_after: None,
            refuse_open: false,
            render_fails: false,
        }
    }
}

#[derive(Default)]
struct FixtureState {
    devices: Vec<AudioDevice>,
    script: FixtureScript,
    rendered: Vec<Pcm>,
    stopped: bool,
}

/// A device-free backend. It never touches WASAPI, never records the operator,
/// and its render sink retains audio until stopped so interruption tests have a
/// deterministic utterance to interrupt.
#[derive(Clone, Default)]
pub struct FixtureAudio {
    state: Arc<Mutex<FixtureState>>,
}

impl FixtureAudio {
    pub fn with_script(script: FixtureScript) -> Self {
        let audio = Self::default();
        audio.state.lock().unwrap().script = script;
        audio
    }

    pub fn rendered(&self) -> Vec<Pcm> {
        self.state.lock().unwrap().rendered.clone()
    }

    pub fn was_stopped(&self) -> bool {
        self.state.lock().unwrap().stopped
    }

    fn default_format() -> DeviceFormat {
        DeviceFormat {
            sample_rate: 16000,
            channels: 1,
            format: SampleFormat::Float32,
        }
    }
}

impl AudioBackend for FixtureAudio {
    fn devices(&self) -> Result<Vec<AudioDevice>, AudioError> {
        let mut state = self.state.lock().unwrap();
        if state.devices.is_empty() {
            state.devices = vec![
                AudioDevice {
                    id: "fixture-capture".into(),
                    name: "Fixture microphone".into(),
                    kind: DeviceKind::Capture,
                    default: true,
                    format: Some(Self::default_format()),
                },
                AudioDevice {
                    id: "fixture-render".into(),
                    name: "Fixture speaker".into(),
                    kind: DeviceKind::Render,
                    default: true,
                    format: Some(Self::default_format()),
                },
            ];
        }
        Ok(state.devices.clone())
    }

    fn open_capture(&self, id: &str) -> Result<Box<dyn CaptureStream>, AudioError> {
        let state = self.state.lock().unwrap();
        if state.script.refuse_open {
            return Err(AudioError::UnsupportedFormat(
                "fixture capture endpoint refuses the requested format".into(),
            ));
        }
        if !id.is_empty() && id != "fixture-capture" {
            return Err(AudioError::Open(format!(
                "no fixture capture endpoint with id `{id}`; run `jean voice devices`"
            )));
        }
        Ok(Box::new(FixtureCapture {
            state: self.state.clone(),
            delivered: 0,
            format: Self::default_format(),
        }))
    }

    fn open_render(&self, id: &str) -> Result<Box<dyn RenderStream>, AudioError> {
        if !id.is_empty() && id != "fixture-render" {
            return Err(AudioError::Open(format!(
                "no fixture render endpoint with id `{id}`; run `jean voice devices`"
            )));
        }
        self.state.lock().unwrap().stopped = false;
        Ok(Box::new(FixtureRender {
            state: self.state.clone(),
            format: Self::default_format(),
        }))
    }
}

struct FixtureCapture {
    state: Arc<Mutex<FixtureState>>,
    delivered: usize,
    format: DeviceFormat,
}

impl CaptureStream for FixtureCapture {
    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn read(&mut self) -> Result<Option<Pcm>, AudioError> {
        let state = self.state.lock().unwrap();
        if state
            .script
            .lose_after
            .is_some_and(|limit| self.delivered >= limit)
        {
            return Err(AudioError::DeviceLost(
                "fixture capture endpoint disappeared".into(),
            ));
        }
        let packet = state.script.packets.get(self.delivered).cloned();
        if packet.is_some() {
            self.delivered += 1;
        }
        Ok(packet)
    }
}

struct FixtureRender {
    state: Arc<Mutex<FixtureState>>,
    format: DeviceFormat,
}

impl RenderStream for FixtureRender {
    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn write(&mut self, frames: &[f32]) -> Result<usize, AudioError> {
        let mut state = self.state.lock().unwrap();
        if state.script.render_fails {
            return Err(AudioError::DeviceLost(
                "fixture render endpoint disappeared mid-utterance".into(),
            ));
        }
        if state.stopped {
            return Ok(0);
        }
        state.rendered.push(Pcm {
            sample_rate: self.format.sample_rate,
            samples: frames.to_vec(),
        });
        Ok(frames.len())
    }

    fn drain(&mut self, cancel: &CancelFlag) -> Result<(), AudioError> {
        // The fixture sink has no clock: it holds the utterance open until it is
        // stopped, which is what makes an interruption test reproducible.
        while !cancel.is_cancelled() && !self.state.lock().unwrap().stopped {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.state.lock().unwrap().stopped = true;
    }
}

/// One WAV as the microphone, and a counted sink in place of the speaker.
///
/// This is the seam that lets the supervised worker drive a real engine end to end
/// with no endpoint opened. Capture delivers the file in 100 ms packets **at
/// 1× real time**, which is what makes it a microphone-shaped source rather than a
/// file dump: a press released after 40 ms of a 6-second recording has genuinely
/// only heard 40 ms, and an engine that returns nothing for it is correct. Once the
/// file is exhausted the stream reports "nothing ready", because a live microphone
/// does not run out — the release flag is what ends a press.
///
/// Render paces at 1× too, so an interrupt stops playback part-way: the frames already
/// handed to the sink stay counted in `rendered_frames`, and the owed time that had not
/// been spent is abandoned the moment the flag is set. Nothing here opens WASAPI, so a
/// test using it cannot make noise.
pub struct FileAudio {
    path: PathBuf,
    sample_rate: u32,
    rendered: Arc<AtomicUsize>,
}

impl FileAudio {
    pub fn new(path: impl Into<PathBuf>, sample_rate: u32) -> Self {
        Self {
            path: path.into(),
            sample_rate,
            rendered: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Frames the sink has taken since this backend was created.
    pub fn rendered_frames(&self) -> usize {
        self.rendered.load(Ordering::Acquire)
    }
}

struct FileCapture {
    format: DeviceFormat,
    packets: VecDeque<Pcm>,
    started: std::time::Instant,
    delivered: usize,
}

impl CaptureStream for FileCapture {
    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn read(&mut self) -> Result<Option<Pcm>, AudioError> {
        let Some(packet) = self.packets.pop_front() else {
            std::thread::sleep(Duration::from_millis(10));
            return Ok(None);
        };
        self.delivered += packet.samples.len();
        let due = self.started
            + Duration::from_secs_f64(self.delivered as f64 / self.format.sample_rate as f64);
        while Instant::now() < due {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(Some(packet))
    }
}

struct FileRender {
    format: DeviceFormat,
    owed_seconds: f64,
    rendered: Arc<AtomicUsize>,
}

impl RenderStream for FileRender {
    fn format(&self) -> DeviceFormat {
        self.format
    }

    fn write(&mut self, frames: &[f32]) -> Result<usize, AudioError> {
        let chunk = (self.format.sample_rate as usize / 10)
            .max(1)
            .min(frames.len());
        self.owed_seconds += chunk as f64 / self.format.sample_rate as f64;
        self.rendered.fetch_add(chunk, Ordering::AcqRel);
        Ok(chunk)
    }

    fn drain(&mut self, cancel: &CancelFlag) -> Result<(), AudioError> {
        let step = Duration::from_millis(10);
        while self.owed_seconds > 0.0 && !cancel.is_cancelled() {
            std::thread::sleep(step);
            self.owed_seconds -= step.as_secs_f64();
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.owed_seconds = 0.0;
    }
}

impl AudioBackend for FileAudio {
    fn devices(&self) -> Result<Vec<AudioDevice>, AudioError> {
        let name = self.path.display().to_string();
        Ok(vec![
            AudioDevice {
                id: name.clone(),
                name: "offline capture file".into(),
                kind: DeviceKind::Capture,
                default: true,
                format: None,
            },
            AudioDevice {
                id: name,
                name: "offline counted sink".into(),
                kind: DeviceKind::Render,
                default: true,
                format: Some(DeviceFormat {
                    sample_rate: self.sample_rate,
                    channels: 1,
                    format: SampleFormat::Float32,
                }),
            },
        ])
    }

    fn open_capture(&self, _id: &str) -> Result<Box<dyn CaptureStream>, AudioError> {
        let detail = |reason: String| {
            AudioError::Open(format!(
                "offline capture `{}`: {reason}",
                self.path.display()
            ))
        };
        let bytes = std::fs::read(&self.path).map_err(|e| detail(e.to_string()))?;
        let pcm = crate::wav::read(&bytes).map_err(|e| detail(e.to_string()))?;
        let format = DeviceFormat {
            sample_rate: pcm.sample_rate,
            channels: 1,
            format: SampleFormat::Float32,
        };
        let chunk = (pcm.sample_rate as usize / 10).max(1);
        Ok(Box::new(FileCapture {
            format,
            started: Instant::now(),
            delivered: 0,
            packets: pcm
                .samples
                .chunks(chunk)
                .map(|frames| Pcm {
                    sample_rate: pcm.sample_rate,
                    samples: frames.to_vec(),
                })
                .collect(),
        }))
    }

    fn open_render(&self, _id: &str) -> Result<Box<dyn RenderStream>, AudioError> {
        Ok(Box::new(FileRender {
            format: DeviceFormat {
                sample_rate: self.sample_rate,
                channels: 1,
                format: SampleFormat::Float32,
            },
            owed_seconds: 0.0,
            rendered: self.rendered.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_enumerates_defaults_and_refuses_an_unknown_endpoint() {
        let audio = FixtureAudio::default();
        let devices = audio.devices().unwrap();
        assert_eq!(devices.len(), 2);
        assert!(devices
            .iter()
            .any(|d| d.default && d.kind == DeviceKind::Capture));
        assert!(audio.open_capture("fixture-capture").is_ok());
        assert!(audio.open_capture("").is_ok());
        assert!(matches!(
            audio.open_capture("not-an-endpoint"),
            Err(AudioError::Open(_))
        ));
    }

    #[test]
    fn a_scripted_device_loss_surfaces_as_device_lost_not_silence() {
        let audio = FixtureAudio::with_script(FixtureScript {
            packets: vec![Pcm::silent(16000, 16); 3],
            lose_after: Some(1),
            refuse_open: false,
            ..FixtureScript::default()
        });
        let mut capture = audio.open_capture("").unwrap();
        assert!(capture.read().unwrap().is_some());
        assert!(matches!(capture.read(), Err(AudioError::DeviceLost(_))));
    }

    #[test]
    fn an_unsupported_endpoint_refuses_the_open_instead_of_falling_back() {
        let audio = FixtureAudio::with_script(FixtureScript {
            refuse_open: true,
            ..FixtureScript::default()
        });
        assert!(matches!(
            audio.open_capture(""),
            Err(AudioError::UnsupportedFormat(_))
        ));
    }

    #[test]
    fn render_stop_is_immediate_and_drain_notices_cancellation() {
        let audio = FixtureAudio::default();
        let mut render = audio.open_render("").unwrap();
        assert_eq!(render.write(&[0.1, 0.2]).unwrap(), 2);
        assert_eq!(audio.rendered().len(), 1);
        let cancel = CancelFlag::new();
        let flag = cancel.clone();
        let waiter = std::thread::spawn(move || {
            flag.cancel();
        });
        render.drain(&cancel).unwrap();
        waiter.join().unwrap();
        render.stop();
        assert!(audio.was_stopped());
        assert_eq!(
            render.write(&[0.3]).unwrap(),
            0,
            "a stopped sink accepts nothing"
        );
    }

    /// One second handed to the offline sink is one second of real time, and a cancel
    /// that lands inside it returns with that second unspent. The CLI interrupt test
    /// measures the same pair from outside the process; this pins the pacing itself.
    #[test]
    fn an_offline_render_paces_and_a_cancel_spends_what_was_not_played() {
        let audio = FileAudio::new("offline sink: never opened for capture", 16_000);
        let pcm = vec![0.0f32; 16_000];
        let hand_over = || {
            let mut render = audio.open_render("").unwrap();
            let mut offset = 0;
            while offset < pcm.len() {
                offset += render.write(&pcm[offset..]).unwrap();
            }
            assert_eq!(offset, pcm.len());
            render
        };

        let started = Instant::now();
        let mut render = hand_over();
        render.drain(&CancelFlag::new()).unwrap();
        let uninterrupted = started.elapsed();
        assert_eq!(
            audio.rendered_frames(),
            pcm.len(),
            "the sink counts every frame it took"
        );
        assert!(
            uninterrupted >= Duration::from_millis(900),
            "a paced second drained in {uninterrupted:?}"
        );
        render.stop();

        let cancel = CancelFlag::new();
        let flag = cancel.clone();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            flag.cancel();
        });
        let started = Instant::now();
        let mut render = hand_over();
        render.drain(&cancel).unwrap();
        let interrupted = started.elapsed();
        releaser.join().unwrap();
        render.stop();
        assert!(
            interrupted < Duration::from_millis(500),
            "a cancel ended the drain after {interrupted:?} instead of the full second"
        );
    }
}
