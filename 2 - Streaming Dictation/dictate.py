"""Standalone CPU Zipformer and Parakeet dictation using Project 2's installed models."""
import argparse
import os
import queue
import wave
from pathlib import Path

import numpy as np
import sherpa_onnx
import sounddevice as sd

RATE = 16000
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--wav", type=Path)
args = parser.parse_args()
models = Path(os.environ["APPDATA"]) / "VoiceAssistant" / "models"


def assets(name):
    tokens = list((models / name).rglob("tokens.txt"))
    if len(tokens) != 1:
        raise SystemExit(f"Expected one model under {models / name}")
    folder = tokens[0].parent
    return {key: str(next(folder.glob(f"{key}*.int8.onnx")))
            for key in ("encoder", "decoder", "joiner")} | {"tokens": str(tokens[0])}


online = sherpa_onnx.OnlineRecognizer.from_transducer(
    **assets("asr"), num_threads=2, sample_rate=RATE, feature_dim=80,
    decoding_method="greedy_search", enable_endpoint_detection=True,
    rule1_min_trailing_silence=2.4, rule2_min_trailing_silence=0.8,
    rule3_min_utterance_length=20.0,
)
offline = sherpa_onnx.OfflineRecognizer.from_transducer(
    **assets("asr2"), num_threads=4, sample_rate=RATE, feature_dim=128,
    decoding_method="greedy_search", model_type="nemo_transducer",
)


def transcribe(chunks):
    stream = online.create_stream()
    audio = []
    partial = ""
    for chunk in chunks:
        audio.append(chunk)
        stream.accept_waveform(RATE, chunk)
        while online.is_ready(stream):
            online.decode_stream(stream)
        result = online.get_result(stream)
        if result and result != partial:
            partial = result
            print(f"Partial: {partial}", flush=True)
        if args.wav is None and online.is_endpoint(stream):
            break
    if not audio:
        raise SystemExit("No audio captured")
    final = offline.create_stream()
    final.accept_waveform(RATE, np.concatenate(audio))
    offline.decode_stream(final)
    if not final.result.text.strip():
        raise SystemExit("No speech recognized")
    print("Final:", final.result.text.strip())


if args.wav:
    with wave.open(str(args.wav)) as wav:
        if (wav.getnchannels(), wav.getsampwidth(), wav.getframerate()) != (1, 2, RATE):
            raise SystemExit("Expected 16 kHz mono PCM16 WAV")
        transcribe(np.frombuffer(data, dtype="<i2").astype(np.float32) / 32768
                   for data in iter(lambda: wav.readframes(1600), b""))
else:
    frames = queue.Queue(maxsize=50)

    import threading

    overflow = threading.Event()

    def capture(data, count, timing, status):
        try:
            frames.put_nowait(data[:, 0].copy())
        except queue.Full:
            overflow.set()

    print("Loading complete. Speak one sentence. Ctrl+C cancels.", flush=True)
    with sd.InputStream(samplerate=RATE, channels=1, dtype="float32",
                        blocksize=1600, callback=capture):
        def microphone_chunks():
            for _ in range(300):
                if overflow.is_set():
                    raise SystemExit("Microphone capture overran")
                try:
                    yield frames.get(timeout=5)
                except queue.Empty:
                    raise SystemExit("Microphone delivered no audio blocks") from None

        transcribe(microphone_chunks())

