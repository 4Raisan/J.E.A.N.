"""Microphone capture helper for the JEAN desktop voice slice.

Records 16 kHz mono float32 audio through sounddevice, prints a loudness
line roughly every 100 ms block so the GUI reactor can show real mic
levels, and writes a 16 kHz mono PCM16 WAV — the exact contract both ASR
backends accept (dictate.py --wav / speech.py --wav).

Run under the same pinned environment as project 2's dictation:

    uv run --offline --python 3.12 --no-project --with sounddevice==0.5.6
        --with numpy==2.5.4 python voice_capture.py --out clip.wav
        --max 20 --stop-file stop.marker

Protocol on stdout (one event per line, flushed):
    READY                     capture stream is open
    LEVEL <0.0..1.0>          RMS loudness of the latest block
    SPEECH <0.0..1.0>          loudest level seen since the last SPEECH line
    ENDED                     capture stopped, before WAV encoding
    DONE <path>                WAV finalized and closed
    ERROR <message>            capture failed; exit code 2

The capture ends when the stop file appears, when --max seconds elapse, or
when speech-endpointing triggers (speech heard, then --endpoint seconds of
silence). It never transcribes and never touches the network.
"""
import argparse
import sys
import time
import wave
from pathlib import Path

RATE = 16000
BLOCK = 1600  # 100 ms at 16 kHz, same block size as dictate.py


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path, required=True,
                        help="where to write the PCM16 WAV")
    parser.add_argument("--max", type=float, default=20.0,
                        help="hard cap on capture seconds")
    parser.add_argument("--stop-file", type=Path, default=None,
                        help="capture ends as soon as this file exists")
    parser.add_argument("--microphone", help="verified Windows WASAPI microphone name")
    parser.add_argument("--threshold", type=float, default=0.012,
                        help="RMS level that counts as speech for endpointing")
    parser.add_argument("--endpoint", type=float, default=1.4,
                        help="seconds of silence after speech that ends capture; "
                             "0 disables endpointing")
    args = parser.parse_args(argv)
    if not 1 <= args.max <= 30:
        parser.error("--max must be between 1 and 30 seconds")

    try:
        import numpy as np
        import sounddevice as sd
    except Exception as exc:  # missing dep inside the uv env, or no audio backend
        print(f"ERROR audio backend unavailable: {exc}", flush=True)
        return 2

    blocks = []
    peak = 0.0
    speech_at = None

    def callback(data, frames, timing, status):
        nonlocal peak, speech_at
        chunk = data[:, 0].astype(np.float32)
        level = float(np.sqrt(np.mean(chunk * chunk))) if frames else 0.0
        level = min(1.0, level * 4.0)  # speech RMS sits low; stretch like audio.ts
        peak = max(peak, level)
        if level >= args.threshold:
            speech_at = time.monotonic()
        blocks.append(chunk.copy())
        print(f"LEVEL {level:.4f}", flush=True)
        print(f"SPEECH {peak:.4f}", flush=True)

    device = None
    if args.microphone:
        devices = [
            index for index, info in enumerate(sd.query_devices())
            if info["name"] == args.microphone and info["max_input_channels"] > 0
            and sd.query_hostapis(info["hostapi"])["name"] == "Windows WASAPI"
        ]
        if len(devices) != 1:
            print("ERROR verified microphone is missing or ambiguous in Windows WASAPI", flush=True)
            return 2
        device = devices[0]

    def open_stream(rate):
        return sd.InputStream(samplerate=rate, channels=1, dtype="float32",
                              device=device, blocksize=round(BLOCK * rate / RATE),
                              callback=callback)

    rate = RATE
    try:
        try:
            stream = open_stream(rate)
        except sd.PortAudioError:
            rate = round(sd.query_devices(device, kind="input")["default_samplerate"])
            if rate == RATE:
                raise
            stream = open_stream(rate)
        with stream:
            started = time.monotonic()
            print("READY", flush=True)
            while True:
                time.sleep(0.05)
                now = time.monotonic()
                if args.stop_file is not None and args.stop_file.exists():
                    break
                if now - started >= args.max:
                    break
                if (args.endpoint > 0 and speech_at is not None
                        and now - speech_at >= args.endpoint
                        and now - started > args.endpoint):
                    break
    except Exception as exc:
        print(f"ERROR could not capture microphone: {exc}", flush=True)
        return 2

    print("ENDED", flush=True)
    if not blocks:
        print("ERROR no audio captured", flush=True)
        return 2

    args.out.parent.mkdir(parents=True, exist_ok=True)
    samples = np.concatenate(blocks)
    if rate != RATE:
        positions = np.arange(round(len(samples) * RATE / rate)) * (rate / RATE)
        samples = np.interp(positions, np.arange(len(samples)), samples)
    pcm = (samples * 32767.0).clip(-32768, 32767).astype("<i2")
    try:
        with wave.open(str(args.out), "wb") as wav:
            wav.setparams((1, 2, RATE, 0, "NONE", "not compressed"))
            wav.writeframes(pcm.tobytes())
    except OSError as exc:
        print(f"ERROR could not write WAV: {exc}", flush=True)
        return 2
    print(f"DONE {args.out}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
