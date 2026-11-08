//! Device-free audio mechanics: sample conversion, resampling, a bounded capture
//! buffer and the push-to-talk state machine.
//!
//! Nothing here opens a device. The WASAPI backend in `wasapi.rs` produces bytes
//! and this module turns them into engine-rate mono frames, so the conversion,
//! the overflow accounting and every activation transition are testable without
//! a microphone, a speaker or an operator present.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Mono frames at a known rate. The only audio shape that crosses a boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    pub sample_rate: u32,
    pub samples: Vec<f32>,
}

impl Pcm {
    pub fn silent(sample_rate: u32, frames: usize) -> Self {
        Self {
            sample_rate,
            samples: vec![0.0; frames],
        }
    }

    pub fn duration(&self) -> Duration {
        if self.sample_rate == 0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(self.samples.len() as f64 / self.sample_rate as f64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    Float32,
    Int16,
    Int32,
    UInt8,
}

impl SampleFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::Float32 | Self::Int32 => 4,
            Self::Int16 => 2,
            Self::UInt8 => 1,
        }
    }
}

/// What a device actually delivers, before any conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    UnsupportedFormat(String),
    DeviceLost(String),
    Open(String),
    Truncated(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat(detail) => write!(f, "unsupported audio format: {detail}"),
            Self::DeviceLost(detail) => write!(f, "audio device lost: {detail}"),
            Self::Open(detail) => write!(f, "could not open audio device: {detail}"),
            Self::Truncated(detail) => write!(f, "truncated audio packet: {detail}"),
        }
    }
}

/// Interleaved device bytes to mono f32 in [-1, 1].
///
/// Channels are averaged rather than dropped, so a stereo microphone does not
/// lose half its signal, and the conversion refuses a packet that is not a whole
/// number of frames instead of silently decoding a partial sample.
pub fn to_mono_f32(bytes: &[u8], format: DeviceFormat) -> Result<Vec<f32>, AudioError> {
    if format.sample_rate == 0 || format.channels == 0 {
        return Err(AudioError::UnsupportedFormat(
            "device reported a zero sample rate or channel count".into(),
        ));
    }
    let stride = format.format.bytes_per_sample() * format.channels as usize;
    if !bytes.len().is_multiple_of(stride) {
        return Err(AudioError::Truncated(format!(
            "{} bytes is not a whole number of {stride}-byte frames",
            bytes.len()
        )));
    }
    let frames = bytes.len() / stride;
    let mut out = Vec::with_capacity(frames);
    for frame in 0..frames {
        let mut sum = 0.0f32;
        for channel in 0..format.channels as usize {
            let offset = frame * stride + channel * format.format.bytes_per_sample();
            let sample = match format.format {
                SampleFormat::Float32 => {
                    f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
                }
                SampleFormat::Int16 => {
                    i16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as f32
                        / 32768.0
                }
                SampleFormat::Int32 => {
                    i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as f32
                        / 2_147_483_648.0
                }
                SampleFormat::UInt8 => (bytes[offset] as f32 - 128.0) / 128.0,
            };
            sum += sample;
        }
        out.push((sum / format.channels as f32).clamp(-1.0, 1.0));
    }
    Ok(out)
}

/// Mono f32 back to interleaved device bytes, for playback.
pub fn from_mono_f32(samples: &[f32], format: DeviceFormat) -> Result<Vec<u8>, AudioError> {
    if format.channels == 0 {
        return Err(AudioError::UnsupportedFormat(
            "playback device reported zero channels".into(),
        ));
    }
    let width = format.format.bytes_per_sample();
    let mut out = Vec::with_capacity(samples.len() * format.channels as usize * width);
    for sample in samples {
        let value = sample.clamp(-1.0, 1.0);
        for _ in 0..format.channels {
            match format.format {
                SampleFormat::Float32 => out.extend_from_slice(&value.to_le_bytes()),
                SampleFormat::Int16 => {
                    let scaled = (value * 32767.0).round() as i16;
                    out.extend_from_slice(&scaled.to_le_bytes());
                }
                SampleFormat::Int32 => {
                    let scaled = (value * 2_147_483_647.0).round() as i32;
                    out.extend_from_slice(&scaled.to_le_bytes());
                }
                SampleFormat::UInt8 => out.push(((value * 127.0) + 128.0).round() as u8),
            }
        }
    }
    Ok(out)
}

/// Chunked linear resampler.
///
/// The fractional position and the previous sample carry across calls, so a
/// stream split into device packets resamples identically to one long buffer.
/// At a packet edge the last sample is held rather than extrapolated: the error
/// is bounded by one sample and never grows with the number of packets.
pub struct Resampler {
    from: u32,
    to: u32,
    position: f64,
    last: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            from,
            to,
            position: 0.0,
            last: 0.0,
        }
    }

    pub fn push(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        if self.from == self.to || self.from == 0 || self.to == 0 {
            return input.to_vec();
        }
        let step = self.from as f64 / self.to as f64;
        let mut out = Vec::with_capacity((input.len() as f64 / step).ceil() as usize + 1);
        let mut time = self.position;
        while time < input.len() as f64 {
            let index = time.floor() as usize;
            let frac = (time - index as f64) as f32;
            let a = input[index];
            let b = input.get(index + 1).copied().unwrap_or(a);
            out.push(a + (b - a) * frac);
            time += step;
        }
        self.position = time - input.len() as f64;
        self.last = *input.last().unwrap();
        out
    }

    /// Emit the held edge sample so a released capture does not lose its tail.
    pub fn flush(&mut self) -> Vec<f32> {
        self.position = 0.0;
        if self.from == self.to {
            return Vec::new();
        }
        vec![self.last]
    }
}

/// A ceiling on audio held between the device and the recognizer.
///
/// When the recognizer falls behind, the oldest frames are dropped and counted.
/// The alternative — growing without bound — turns a slow model into a memory
/// leak that outlives the push-to-talk press that caused it.
pub struct BoundedPcm {
    capacity: usize,
    samples: VecDeque<f32>,
    dropped: u64,
}

impl BoundedPcm {
    pub fn new(capacity_frames: usize) -> Self {
        Self {
            capacity: capacity_frames.max(1),
            samples: VecDeque::new(),
            dropped: 0,
        }
    }

    pub fn for_duration(sample_rate: u32, ms: u64) -> Self {
        let frames = (sample_rate as u64 * ms / 1000).max(1) as usize;
        Self::new(frames)
    }

    pub fn push(&mut self, incoming: &[f32]) {
        if incoming.len() >= self.capacity {
            self.dropped += self.samples.len() as u64 + (incoming.len() - self.capacity) as u64;
            self.samples.clear();
            self.samples
                .extend(incoming[incoming.len() - self.capacity..].iter().copied());
            return;
        }
        let overflow = (self.samples.len() + incoming.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            self.samples.drain(..overflow);
            self.dropped += overflow as u64;
        }
        self.samples.extend(incoming.iter().copied());
    }

    pub fn take(&mut self) -> Vec<f32> {
        self.samples.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttState {
    Idle,
    Opening,
    Capturing,
    Finalizing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttError {
    Busy,
    Stale,
    NotCapturing,
    NotFinalizing,
}

impl std::fmt::Display for PttError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Busy => "push-to-talk is already active; release or interrupt first",
            Self::Stale => "stale push-to-talk activation",
            Self::NotCapturing => "push-to-talk is not capturing",
            Self::NotFinalizing => "push-to-talk is not finalizing",
        };
        f.write_str(text)
    }
}

/// The activation lifecycle, independent of IPC and of any device.
///
/// `Opening` exists because opening a capture endpoint is real work that can
/// fail: the worker must not report `Capturing` until the device actually opened,
/// and must not block its control loop while it finds out.
pub struct PushToTalk {
    state: PttState,
    current: Option<u64>,
    last: u64,
    deadline: Instant,
    pub buffer: BoundedPcm,
}

impl PushToTalk {
    pub fn new(sample_rate: u32, buffer_ms: u64) -> Self {
        Self {
            state: PttState::Idle,
            current: None,
            last: 0,
            deadline: Instant::now(),
            buffer: BoundedPcm::for_duration(sample_rate, buffer_ms),
        }
    }

    pub fn state(&self) -> PttState {
        self.state
    }

    pub fn activation(&self) -> Option<u64> {
        self.current
    }

    pub fn expired(&self, now: Instant) -> bool {
        self.current.is_some() && now >= self.deadline
    }

    pub fn begin(
        &mut self,
        activation: u64,
        now: Instant,
        timeout: Duration,
    ) -> Result<(), PttError> {
        if self.current.is_some() {
            return Err(PttError::Busy);
        }
        if activation <= self.last {
            return Err(PttError::Stale);
        }
        self.last = activation;
        self.current = Some(activation);
        self.state = PttState::Opening;
        self.deadline = now + timeout;
        self.buffer.clear();
        Ok(())
    }

    /// Report that the endpoint actually opened.
    ///
    /// Returns `false` when release had already arrived — an operator who lets
    /// go before the device opens still gets one coherent activation, not a
    /// refusal.
    pub fn opened(&mut self, activation: u64) -> Result<bool, PttError> {
        if self.current != Some(activation) {
            return Err(PttError::Stale);
        }
        match self.state {
            PttState::Opening => {
                self.state = PttState::Capturing;
                Ok(true)
            }
            PttState::Finalizing => Ok(false),
            _ => Err(PttError::NotCapturing),
        }
    }

    /// Release the press. Valid while opening as well as while capturing, so a
    /// short press cannot be refused merely because the device was slow.
    pub fn release(
        &mut self,
        activation: u64,
        now: Instant,
        budget: Duration,
    ) -> Result<(), PttError> {
        if self.current != Some(activation) {
            return Err(PttError::Stale);
        }
        if self.state == PttState::Idle {
            return Err(PttError::NotCapturing);
        }
        self.state = PttState::Finalizing;
        self.deadline = now + budget;
        Ok(())
    }

    pub fn complete(&mut self, activation: u64) -> Result<(), PttError> {
        if self.current != Some(activation) {
            return Err(PttError::Stale);
        }
        if self.state != PttState::Finalizing {
            return Err(PttError::NotFinalizing);
        }
        self.finish();
        Ok(())
    }

    /// Drop the activation without a transcript: interrupt, deadline, shutdown
    /// or a device that disappeared mid-press.
    pub fn abort(&mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        self.state = PttState::Idle;
        self.current = None;
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(rate: u32, channels: u16, sample: SampleFormat) -> DeviceFormat {
        DeviceFormat {
            sample_rate: rate,
            channels,
            format: sample,
        }
    }

    #[test]
    fn every_supported_sample_format_converts_and_round_trips() {
        let cases = [
            (SampleFormat::Float32, vec![0.5f32, -0.5]),
            (SampleFormat::Int16, vec![0.5, -0.5]),
            (SampleFormat::Int32, vec![0.5, -0.5]),
            (SampleFormat::UInt8, vec![0.5, -0.5]),
        ];
        for (sample, values) in cases {
            let fmt = format(16000, 1, sample);
            let bytes = from_mono_f32(&values, fmt).unwrap();
            let back = to_mono_f32(&bytes, fmt).unwrap();
            assert_eq!(back.len(), 2);
            for (got, want) in back.iter().zip(values.iter()) {
                assert!(
                    (got - want).abs() < 0.01,
                    "{sample:?}: {got} should be near {want}"
                );
            }
        }
    }

    #[test]
    fn stereo_is_averaged_and_a_partial_frame_is_refused() {
        let fmt = format(48000, 2, SampleFormat::Int16);
        let bytes: Vec<u8> = [16384i16, -16384i16]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(to_mono_f32(&bytes, fmt).unwrap(), vec![0.0]);
        assert!(matches!(
            to_mono_f32(&bytes[..3], fmt),
            Err(AudioError::Truncated(_))
        ));
        assert!(matches!(
            to_mono_f32(&[], format(0, 1, SampleFormat::Float32)),
            Err(AudioError::UnsupportedFormat(_))
        ));
    }

    #[test]
    fn resampling_is_continuous_across_packet_boundaries() {
        let signal: Vec<f32> = (0..960).map(|i| (i as f32 / 960.0) * 2.0 - 1.0).collect();
        let mut whole = Resampler::new(48000, 16000);
        let expected = whole.push(&signal);
        let mut chunked = Resampler::new(48000, 16000);
        let mut got = Vec::new();
        for packet in signal.chunks(100) {
            got.extend(chunked.push(packet));
        }
        assert_eq!(got.len(), expected.len());
        for (a, b) in got.iter().zip(expected.iter()) {
            assert!(
                (a - b).abs() < 0.02,
                "chunked resampling drifted: {a} vs {b}"
            );
        }
        let mut identity = Resampler::new(16000, 16000);
        assert_eq!(identity.push(&signal), signal);
    }

    #[test]
    fn the_capture_buffer_drops_oldest_frames_and_counts_them() {
        let mut buffer = BoundedPcm::new(4);
        buffer.push(&[1.0, 2.0, 3.0]);
        buffer.push(&[4.0, 5.0, 6.0]);
        assert_eq!(buffer.take(), vec![3.0, 4.0, 5.0, 6.0]);
        assert_eq!(buffer.dropped(), 2);
        buffer.push(&[9.0; 10]);
        assert_eq!(buffer.len(), 4);
        assert_eq!(
            buffer.dropped(),
            8,
            "six of the ten arriving frames are dropped on an already-empty buffer"
        );
        assert_eq!(buffer.take(), vec![9.0; 4]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn push_to_talk_refuses_busy_stale_and_out_of_order_transitions() {
        let mut ptt = PushToTalk::new(16000, 1000);
        let now = Instant::now();
        let second = Duration::from_secs(2);
        assert_eq!(ptt.state(), PttState::Idle);
        assert_eq!(ptt.release(1, now, second), Err(PttError::Stale));
        ptt.begin(1, now, second).unwrap();
        assert_eq!(ptt.state(), PttState::Opening);
        assert_eq!(ptt.begin(2, now, second), Err(PttError::Busy));
        assert_eq!(ptt.complete(1), Err(PttError::NotFinalizing));
        ptt.opened(1).unwrap();
        assert_eq!(ptt.state(), PttState::Capturing);
        assert_eq!(ptt.opened(1), Err(PttError::NotCapturing));
        ptt.buffer.push(&[0.25; 8]);
        ptt.release(1, now, second).unwrap();
        assert_eq!(ptt.state(), PttState::Finalizing);
        assert_eq!(ptt.complete(2), Err(PttError::Stale));
        ptt.complete(1).unwrap();
        assert_eq!(ptt.state(), PttState::Idle);
        assert!(
            ptt.buffer.is_empty(),
            "a completed activation retains no audio"
        );
        assert_eq!(ptt.begin(1, now, second), Err(PttError::Stale));
        ptt.begin(2, now, second).unwrap();
        assert!(ptt.expired(now + second));
        ptt.abort();
        assert_eq!(ptt.state(), PttState::Idle);
        assert_eq!(ptt.activation(), None);
    }

    #[test]
    fn a_press_released_before_the_device_opened_still_completes() {
        // The operator lets go in the same instant they press. The activation is
        // real, the release is accepted, and the open that arrives afterwards
        // reports that it did not start a fresh capture.
        let mut ptt = PushToTalk::new(16000, 1000);
        let now = Instant::now();
        ptt.begin(7, now, Duration::from_secs(30)).unwrap();
        ptt.release(7, now, Duration::from_millis(1200)).unwrap();
        assert_eq!(ptt.state(), PttState::Finalizing);
        assert!(!ptt.opened(7).unwrap());
        assert_eq!(ptt.state(), PttState::Finalizing);
        ptt.complete(7).unwrap();
        assert_eq!(ptt.state(), PttState::Idle);
    }
}
