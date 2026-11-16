"""Streaming Gemini transcription for desktop Talk and its synthetic probe."""

import base64
import json
import queue
import time
import wave
from concurrent.futures import Future
from threading import Event, Thread

HOST = "generativelanguage.googleapis.com"
MODEL = "gemini-3.5-transcribe-live"
URL = f"wss://{HOST}/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent"


class Listening:
    def __init__(self, model, key):
        self.chunks = queue.Queue()
        self.ended = Event()
        self.cancelled = Event()
        self.result = Future()
        Thread(target=self._run, args=(model, key), daemon=True).start()

    def _run(self, model, key):
        try:
            result = self._transcribe(model, key)
        except Exception as exc:
            # Provider exception messages can include request details.
            code = getattr(getattr(exc, "rcvd", None), "code", None)
            status = getattr(getattr(exc, "response", None), "status_code", None)
            detail = f" ({status or code})" if status or code else ""
            self.result.set_exception(RuntimeError(f"Gemini Live: {type(exc).__name__}{detail}"))
        else:
            self.result.set_result(result)

    def _transcribe(self, model, key):
        from websockets.sync.client import connect

        def send(connection, value):
            connection.send(json.dumps(value))

        with connect(URL, additional_headers={"x-goog-api-key": key},
                     open_timeout=8, close_timeout=1) as connection:
            send(connection, {"setup": {
                "model": f"models/{model}",
                "generationConfig": {"responseModalities": ["TEXT"]},
                "inputAudioTranscription": {},
                "realtimeInputConfig": {"automaticActivityDetection": {"disabled": True}},
            }})
            if "setupComplete" not in json.loads(connection.recv(timeout=8)):
                raise ValueError("setup failed")

            send(connection, {"realtimeInput": {"activityStart": {}}})
            deadline = None
            while not self.cancelled.is_set():
                if deadline is None:
                    while not self.cancelled.is_set():
                        try:
                            data = self.chunks.get_nowait()
                        except queue.Empty:
                            break
                        send(connection, {"realtimeInput": {"audio": {
                            "data": data, "mimeType": "audio/pcm;rate=16000",
                        }}})
                    if self.ended.is_set() and self.chunks.empty():
                        send(connection, {"realtimeInput": {"activityEnd": {}}})
                        deadline = time.monotonic() + 5

                try:
                    message = json.loads(connection.recv(timeout=0.05))
                except TimeoutError:
                    if deadline is not None and time.monotonic() >= deadline:
                        break
                    continue
                content = message.get("serverContent", {})
                final = content.get("inputTranscription", {}).get("text", "").strip()
                if deadline is not None and final:
                    return final

        if self.cancelled.is_set():
            return ""
        raise ValueError("no final transcript")


def probe(model, key, path):
    live = Listening(model, key)
    try:
        with wave.open(str(path)) as wav:
            if (wav.getnchannels(), wav.getsampwidth(), wav.getframerate()) != (1, 2, 16000):
                raise ValueError("Expected 16 kHz mono PCM16 WAV")
            started = time.monotonic()
            frames = 0
            while chunk := wav.readframes(1600):
                live.chunks.put(base64.b64encode(chunk).decode("ascii"))
                frames += len(chunk) // 2
                time.sleep(max(0, started + frames / 16000 - time.monotonic()))
        live.ended.set()
        return live.result.result(timeout=23)
    finally:
        live.cancelled.set()
