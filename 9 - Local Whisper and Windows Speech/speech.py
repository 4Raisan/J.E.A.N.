import argparse
import os
from pathlib import Path

os.environ["JARVIS_STT_LANGUAGE"] = "en"
os.environ["JARVIS_DATA_DIR"] = str(Path(__file__).resolve().parent / "data")

import osal
import stt_local

parser = argparse.ArgumentParser()
command = parser.add_mutually_exclusive_group(required=True)
command.add_argument("--wav", type=Path)
command.add_argument("--mic", type=int, metavar="SECONDS")
command.add_argument("--speak")
command.add_argument("--setup", action="store_true")
args = parser.parse_args()

if args.setup:
    print(stt_local.ensure_model())
elif args.speak:
    process = osal.speech_process(args.speak)
    if process is None or process.wait(timeout=60):
        raise SystemExit("Windows speech playback failed")
else:
    if not stt_local.ready():
        raise SystemExit("English Whisper assets are missing. Run with --setup first.")
    if args.mic is not None:
        import io
        import wave
        import sounddevice as sd

        if not 1 <= args.mic <= 30:
            parser.error("Microphone duration must be between 1 and 30 seconds")
        rate = int(sd.query_devices(kind="input")["default_samplerate"])
        print(f"Listening for {args.mic} seconds…", flush=True)
        samples = sd.rec(rate * args.mic, samplerate=rate, channels=1, dtype="int16", blocking=True)
        output = io.BytesIO()
        with wave.open(output, "wb") as wav:
            wav.setparams((1, 2, rate, 0, "NONE", "not compressed"))
            wav.writeframes(samples.astype("<i2", copy=False).tobytes())
        audio = output.getvalue()
    else:
        audio = args.wav.read_bytes()
    result = stt_local.transcribe_full(audio)
    if not result.text.strip():
        raise SystemExit("No speech recognized")
    print(result.text)

