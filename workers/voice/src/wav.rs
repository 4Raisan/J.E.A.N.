//! Reading a prerecorded WAV into engine-shaped audio.
//!
//! This exists so that tier-3 evidence — real inference on a known phrase — needs
//! a file and a model, not a microphone, a quiet room and an operator speaking
//! into a device. `jean voice transcribe <clip.wav>` goes through the same
//! `EngineFactory` and the same `Recognizer` trait as a live press, so what it
//! reports about a bundle is what a press would report. No device is opened here.

use crate::audio::{to_mono_f32, AudioError, DeviceFormat, Pcm, SampleFormat};

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, AudioError> {
    let window = bytes
        .get(at..at + 2)
        .ok_or_else(|| AudioError::Truncated(format!("wav: no 2 bytes at offset {at}")))?;
    Ok(u16::from_le_bytes([window[0], window[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, AudioError> {
    let window = bytes
        .get(at..at + 4)
        .ok_or_else(|| AudioError::Truncated(format!("wav: no 4 bytes at offset {at}")))?;
    Ok(u32::from_le_bytes([
        window[0], window[1], window[2], window[3],
    ]))
}

/// Map a (tag, bit depth) pair onto the one sample shape the pipeline knows.
///
/// 24-bit PCM is refused rather than widened by hand: a silent reinterpretation of
/// the sample grid would make a transcript difference impossible to attribute.
fn sample_format(tag: u16, bits: u16) -> Result<SampleFormat, AudioError> {
    match (tag, bits) {
        (WAVE_FORMAT_PCM, 8) => Ok(SampleFormat::UInt8),
        (WAVE_FORMAT_PCM, 16) => Ok(SampleFormat::Int16),
        (WAVE_FORMAT_PCM, 32) => Ok(SampleFormat::Int32),
        (WAVE_FORMAT_IEEE_FLOAT, 32) => Ok(SampleFormat::Float32),
        (WAVE_FORMAT_IEEE_FLOAT, 64) => Err(AudioError::UnsupportedFormat(
            "64-bit float WAV is not read here; export 32-bit float or 16-bit PCM".into(),
        )),
        (other, bits) => Err(AudioError::UnsupportedFormat(format!(
            "WAV format tag {other} at {bits} bits per sample is not a shape this build reads \
             (accepted: PCM at 8, 16 or 32 bits; IEEE float at 32 bits)"
        ))),
    }
}

fn format_of(fmt: &[u8]) -> Result<DeviceFormat, AudioError> {
    if fmt.len() < 16 {
        return Err(AudioError::Truncated(format!(
            "wav: 'fmt ' chunk is {} bytes, fewer than the 16 required",
            fmt.len()
        )));
    }
    let mut tag = u16_at(fmt, 0)?;
    let channels = u16_at(fmt, 2)?;
    let sample_rate = u32_at(fmt, 4)?;
    let bits = u16_at(fmt, 14)?;
    if tag == WAVE_FORMAT_EXTENSIBLE {
        // The subtype GUID decides what an extensible header really holds; its first
        // four bytes, little-endian, are the legacy format tag.
        if fmt.len() < 40 {
            return Err(AudioError::Truncated(
                "wav: extensible 'fmt ' chunk is shorter than its 40-byte header".into(),
            ));
        }
        tag = u32_at(fmt, 24)? as u16;
    }
    Ok(DeviceFormat {
        sample_rate,
        channels,
        format: sample_format(tag, bits)?,
    })
}

/// Parse a RIFF WAVE file and return its audio as mono frames at the file's rate.
pub fn read(bytes: &[u8]) -> Result<Pcm, AudioError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(AudioError::UnsupportedFormat(
            "not a RIFF WAVE file: expected the first four bytes to be 'RIFF' and bytes 8-12 \
             to be 'WAVE'"
                .into(),
        ));
    }
    let mut format: Option<DeviceFormat> = None;
    let mut data: Option<&[u8]> = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32_at(bytes, at + 4)? as usize;
        let body_start = at + 8;
        let body_end = body_start
            .checked_add(size)
            .ok_or_else(|| AudioError::Truncated("wav: chunk size overflows the file".into()))?
            .min(bytes.len());
        let body = &bytes[body_start.min(bytes.len())..body_end];
        match id {
            b"fmt " => format = Some(format_of(body)?),
            b"data" => data = Some(body),
            _ => {}
        }
        // Chunks are word-aligned: an odd size is followed by one pad byte.
        at = body_start + size + (size % 2);
    }
    let format = format
        .ok_or_else(|| AudioError::UnsupportedFormat("wav contained no 'fmt ' chunk".into()))?;
    let data =
        data.ok_or_else(|| AudioError::UnsupportedFormat("wav contained no 'data' chunk".into()))?;
    if data.is_empty() {
        return Err(AudioError::Truncated(
            "wav 'data' chunk is empty: nothing to recognise".into(),
        ));
    }
    Ok(Pcm {
        sample_rate: format.sample_rate,
        samples: to_mono_f32(data, format)?,
    })
}

/// Encode mono audio as a 16-bit PCM WAV: the shape [`read`] accepts, and the one
/// every player, inspector and spectrum tool outside this build understands.
///
/// 16-bit rather than the engine's f32 because the point of a synthesized file is
/// to be examined by something that is not JEAN. Frames past full scale are
/// clipped, which the caller reports separately through the peak it measured
/// before encoding — a silent clamp in here would hide a hot engine.
pub fn encode_pcm16(pcm: &Pcm) -> Vec<u8> {
    let frames = pcm.samples.len();
    let data_bytes = frames.saturating_mul(2);
    let mut out = Vec::with_capacity(44 + data_bytes);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(
        &(36u64.saturating_add(data_bytes as u64).min(u32::MAX as u64) as u32).to_le_bytes(),
    );
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&pcm.sample_rate.to_le_bytes());
    out.extend_from_slice(&pcm.sample_rate.saturating_mul(2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes.min(u32::MAX as usize) as u32).to_le_bytes());
    for sample in &pcm.samples {
        out.extend_from_slice(&((sample.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wave(fmt: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        out.extend_from_slice(fmt);
        if fmt.len() % 2 == 1 {
            out.push(0);
        }
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    fn fmt16(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&tag.to_le_bytes());
        f.extend_from_slice(&channels.to_le_bytes());
        f.extend_from_slice(&rate.to_le_bytes());
        f.extend_from_slice(&(rate * channels as u32 * bits as u32 / 8).to_le_bytes());
        f.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        f.extend_from_slice(&bits.to_le_bytes());
        f
    }

    #[test]
    fn a_16_bit_pcm_file_becomes_mono_frames_at_its_own_rate() {
        // Interleaved stereo: [L0, R0, L1, R1]. Averaging the pair is what makes
        // the first frame exactly zero and the second almost full scale.
        let samples: Vec<i16> = vec![16384, -16384, 32767, 32767];
        let mut data = Vec::new();
        for s in samples {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let bytes = wave(&fmt16(1, 2, 16000, 16), &data);
        let pcm = read(&bytes).unwrap();
        assert_eq!(pcm.sample_rate, 16000);
        assert_eq!(pcm.samples.len(), 2, "four samples across two channels");
        assert_eq!(pcm.samples[0], 0.0, "opposite channels cancel");
        assert!(pcm.samples[1] > 0.99, "both channels near full scale");
    }

    #[test]
    fn a_float32_file_and_an_extensible_header_are_both_read() {
        let mut data = Vec::new();
        for f in [0.25f32, -0.5] {
            data.extend_from_slice(&f.to_le_bytes());
        }
        let pcm = read(&wave(&fmt16(3, 1, 48000, 32), &data)).unwrap();
        assert_eq!(pcm.sample_rate, 48000u32);
        assert_eq!(pcm.samples, vec![0.25f32, -0.5]);

        // extensible: 40-byte fmt = 16 + cbSize + valid bits + reserved + subtype GUID,
        // whose first four bytes, little-endian, are the real format tag.
        let mut f = fmt16(0xFFFE, 1, 44100, 32);
        f.extend_from_slice(&22u16.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&0u16.to_le_bytes());
        f.extend_from_slice(&0u16.to_le_bytes());
        let mut guid = vec![0u8; 16];
        guid[0..4].copy_from_slice(&3u32.to_le_bytes());
        f.extend_from_slice(&guid);
        assert_eq!(f.len(), 40, "an extensible 'fmt ' chunk is 40 bytes");
        let pcm = read(&wave(&f, &data)).unwrap();
        assert_eq!(pcm.sample_rate, 44100u32);
        assert_eq!(pcm.samples, vec![0.25f32, -0.5]);
    }

    #[test]
    fn a_format_the_pipeline_cannot_represent_is_refused_by_name() {
        let error = read(&wave(&fmt16(6, 1, 16000, 16), &[0, 0])).unwrap_err();
        assert!(matches!(error, AudioError::UnsupportedFormat(_)), "{error}");
        assert!(error.to_string().contains("ADPCM") || error.to_string().contains("format tag 6"));
        let bits = read(&wave(&fmt16(1, 1, 16000, 24), &[0, 0, 0])).unwrap_err();
        assert!(
            bits.to_string().contains("24 bits"),
            "24-bit must be refused explicitly, not silently widened: {bits}"
        );
    }

    #[test]
    fn something_that_is_not_a_wav_file_says_so() {
        let error = read(b"NOT A WAVE FILE AT ALL").unwrap_err();
        assert!(error.to_string().contains("RIFF"), "{error}");
        let no_data = {
            let mut out = Vec::new();
            out.extend_from_slice(b"RIFF");
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(b"WAVE");
            let f = fmt16(1, 1, 16000, 16);
            out.extend_from_slice(b"fmt ");
            out.extend_from_slice(&(f.len() as u32).to_le_bytes());
            out.extend_from_slice(&f);
            out
        };
        assert!(read(&no_data)
            .unwrap_err()
            .to_string()
            .contains("no 'data' chunk"));
        assert!(read(&wave(&fmt16(1, 1, 16000, 16), &[]))
            .unwrap_err()
            .to_string()
            .contains("empty"));
    }

    /// A synthesized file is meant to be opened by things that are not this build,
    /// so the cheapest witness that the header is honest is our own strict reader.
    #[test]
    fn an_encoded_file_reads_back_as_the_same_audio() {
        let source = Pcm {
            sample_rate: 24000,
            samples: vec![0.0, 0.5, -0.5, 0.25],
        };
        let bytes = encode_pcm16(&source);
        assert_eq!(bytes.len(), 44 + source.samples.len() * 2);
        let back = read(&bytes).expect("the encoder's own output must be readable");
        assert_eq!(back.sample_rate, 24000);
        assert_eq!(back.samples.len(), 4);
        for (want, got) in source.samples.iter().zip(&back.samples) {
            assert!((want - got).abs() < 0.001, "{want} came back as {got}");
        }
    }

    #[test]
    fn a_sample_past_full_scale_is_clipped_rather_than_wrapped() {
        let hot = Pcm {
            sample_rate: 16000,
            samples: vec![2.0, -2.0],
        };
        let back = read(&encode_pcm16(&hot)).unwrap();
        assert!(
            back.samples[0] > 0.99 && back.samples[1] < -0.99,
            "{:?}",
            back.samples
        );
    }
}
