fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--list-devices") => std::process::exit(jean_voice::list_devices()),
        Some("--doctor") => std::process::exit(jean_voice::doctor()),
        Some("--transcribe") => {
            let path = args
                .get(1)
                .ok_or_else(|| std::io::Error::other("missing clip path"))?;
            std::process::exit(jean_voice::transcribe(path))
        }
        Some("--synthesize") => {
            let path = args
                .get(1)
                .ok_or_else(|| std::io::Error::other("missing output path"))?;
            let text = args
                .get(2)
                .ok_or_else(|| std::io::Error::other("missing the phrase to speak"))?;
            let phonemes = args.iter().any(|arg| arg == "--phonemes");
            std::process::exit(jean_voice::synthesize(path, text, phonemes))
        }
        Some("--session") => {
            let session = args
                .get(1)
                .ok_or_else(|| std::io::Error::other("missing session"))?;
            jean_voice::serve(session.clone())
        }
        _ => Err(std::io::Error::other(
            "expected --session <id>, --list-devices, --doctor, --transcribe <clip.wav> \
             or --synthesize <out.wav> <phrase> [--phonemes]",
        )),
    }
}
