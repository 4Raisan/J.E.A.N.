use std::io::{self, Read};

use anyhow::{Context, ensure};

const INPUT_LIMIT: usize = 100_000;
const SHORT_REPLY: usize = 480;

fn readback(text: &str) -> String {
    if text.chars().count() <= SHORT_REPLY {
        return text.to_owned();
    }

    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let Some(first) = lines.next() else {
        return String::new();
    };
    let mut spoken: String = first.chars().take(160).collect();
    if first.chars().count() > 160 {
        spoken.push('…');
    }
    for line in lines.filter(|line| line.chars().count() <= 160).take(2) {
        spoken.push(' ');
        spoken.push_str(line);
    }
    spoken
}

pub fn speak_from_stdin(task_result: bool, voice: Option<String>, rate: i32) -> anyhow::Result<()> {
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
    )
}

#[cfg(windows)]
fn speak(text: &str, voice: Option<&str>, rate: i32) -> anyhow::Result<()> {
    use windows::Win32::Media::Speech::{ISpeechVoice, SVSFIsNotXML, SpVoice};
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::core::BSTR;

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
                if description.eq_ignore_ascii_case(name) || display_name.eq_ignore_ascii_case(name) {
                    speaker.putref_Voice(&token)?;
                    found = true;
                    break;
                }
            }
            ensure!(found, "Windows voice is unavailable: {name}");
        }
        speaker.SetRate(rate)?;
        speaker.Speak(&BSTR::from(text), SVSFIsNotXML)?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn speak(_text: &str, _voice: Option<&str>, _rate: i32) -> anyhow::Result<()> {
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
    }
}
