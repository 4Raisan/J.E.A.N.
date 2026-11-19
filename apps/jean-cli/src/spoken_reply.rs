use std::io::{self, Read};
#[cfg(windows)]
use std::io::Write;

use anyhow::{ensure, Context};

const INPUT_LIMIT: usize = 100_000;
const SHORT_REPLY: usize = 480;

fn clean_line_for_speech(line: &str) -> Option<String> {
    let mut trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Skip raw CLI footer/noise lines that sound robotic when spoken aloud.
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("exit status ")
        || lower.starts_with("provider ")
        || lower.starts_with("endpoint ")
        || lower.starts_with("credential ")
        || lower.starts_with("prompt ceiling ")
        || lower.starts_with("proposals ")
        || lower.starts_with("authority ")
        || lower.starts_with("task t-")
        || lower.starts_with("plan p-")
    {
        return None;
    }
    for prefix in [
        "result: ",
        "result ",
        "outcome: ",
        "outcome ",
        "verdict: ",
        "verdict ",
        "reason: ",
        "clarify: ",
        "playbook: ",
        "next: ",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            trimmed = rest.trim();
            break;
        }
    }
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_owned())
}

fn readback(text: &str) -> String {
    let cleaned_lines: Vec<String> = text
        .lines()
        .filter_map(clean_line_for_speech)
        .collect();
    let joined = if cleaned_lines.is_empty() {
        text.trim().to_owned()
    } else {
        cleaned_lines.join("\n")
    };
    if joined.chars().count() <= SHORT_REPLY {
        return joined.replace('\n', " ");
    }

    let mut lines = joined.lines().map(str::trim).filter(|line| !line.is_empty());
    let Some(first) = lines.next() else {
        return String::new();
    };
    let mut spoken: String = first.chars().take(180).collect();
    if first.chars().count() > 180 {
        spoken.push('…');
    }
    for line in lines.filter(|line| line.chars().count() <= 180).take(2) {
        spoken.push(' ');
        spoken.push_str(line);
    }
    spoken
}

pub fn speak_from_stdin(task_result: bool, voice: Option<String>, rate: i32, wav_stdout: bool) -> anyhow::Result<()> {
    let mut text = String::new();
    io::stdin()
        .take((INPUT_LIMIT + 1) as u64)
        .read_to_string(&mut text)
        .context("reading UTF-8 speech input")?;
    ensure!(
        text.len() <= INPUT_LIMIT,
        "speech input exceeds {INPUT_LIMIT} bytes"
    );
    let text = text.trim();
    ensure!(!text.is_empty(), "speech input is empty");
    let spoken = if task_result {
        readback(text)
    } else {
        text.to_owned()
    };
    let voice = voice.or_else(|| std::env::var("JARVIS_VOICE").ok());
    speak(
        &spoken,
        voice.as_deref().filter(|name| !name.is_empty()),
        rate,
        wav_stdout,
    )
}

#[cfg(windows)]
fn speak(text: &str, voice: Option<&str>, rate: i32, wav_stdout: bool) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{BSTR, PCWSTR};
    use windows::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT, SND_SYNC};
    use windows::Win32::Media::Speech::{
        ISpeechFileStream, ISpeechVoice, SSFMCreateForWrite, SVSFIsNotXML, SpFileStream, SpVoice,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    };

    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            // COM was initialized on this thread and all its objects have dropped.
            unsafe { CoUninitialize() }
        }
    }

    // This CLI command owns its thread and uses SAPI objects only on this apartment.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _apartment = Apartment;
        let speaker: ISpeechVoice = CoCreateInstance(&SpVoice, None, CLSCTX_ALL)
            .context("creating Windows Speech voice")?;
        if let Some(name) = voice {
            let voices = speaker.GetVoices(&BSTR::new(), &BSTR::new())?;
            let mut found = false;
            for index in 0..voices.Count()? {
                let token = voices.Item(index)?;
                let description = token.GetDescription(0)?.to_string();
                let display_name = description
                    .split_once(" - ")
                    .map_or(description.as_str(), |(name, _)| name);
                if description.eq_ignore_ascii_case(name) || display_name.eq_ignore_ascii_case(name)
                {
                    speaker.putref_Voice(&token)?;
                    found = true;
                    break;
                }
            }
            ensure!(found, "Windows voice is unavailable: {name}");
        }
        speaker.SetRate(rate)?;
        speaker.SetVolume(100)?;
        let directory = tempfile::tempdir().context("creating temporary speech directory")?;
        let path = directory.path().join("speech.wav");
        let filename: Vec<u16> = path.as_os_str().encode_wide().collect();
        let stream: ISpeechFileStream = CoCreateInstance(&SpFileStream, None, CLSCTX_ALL)
            .context("creating Windows Speech file stream")?;
        stream.Open(&BSTR::from_wide(&filename), SSFMCreateForWrite, false.into())?;
        speaker.putref_AudioOutputStream(&stream)?;
        speaker.Speak(&BSTR::from(text), SVSFIsNotXML)?;
        stream.Close()?;
        let audio = std::fs::read(&path).context("reading synthesized speech")?;
        directory.close().context("removing temporary speech file")?;
        if wav_stdout {
            io::stdout().lock().write_all(&audio)?;
            return Ok(());
        }
        ensure!(
            PlaySoundW(PCWSTR(audio.as_ptr().cast()), None, SND_MEMORY | SND_NODEFAULT | SND_SYNC)
                .as_bool(),
            "Windows could not play speech on the default output"
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn speak(_text: &str, _voice: Option<&str>, _rate: i32, _wav_stdout: bool) -> anyhow::Result<()> {
    anyhow::bail!("Windows Speech is available only on Windows")
}

#[cfg(test)]
mod tests {
    use super::readback;

    #[test]
    fn keeps_short_replies_and_leading_facts() {
        assert_eq!(readback("Three files saved."), "Three files saved.");
        let long = format!(
            "Export finished.\nThree files saved in Reports.\n{}",
            "Routine log output. ".repeat(40)
        );
        assert_eq!(
            readback(&long),
            "Export finished. Three files saved in Reports."
        );
        let cli_output = "provider ollama\nresult: Inspected 10 processes.\nexit status 0; `jean task t-1`";
        assert_eq!(readback(cli_output), "Inspected 10 processes.");
    }
}
