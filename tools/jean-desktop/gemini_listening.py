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


class TranscriptionError(ValueError):
    pass


class Listening:
    def __init__(self, model, key, *, connect=None):
        self.chunks = queue.Queue()
        self.ended = Event()
        self.cancelled = Event()
        self.result = Future()
        self._connect = connect
        Thread(target=self._run, args=(model, key), daemon=True).start()

    def _run(self, model, key):
        try:
            result = self._transcribe(model, key)
        except TranscriptionError as exc:
            self.result.set_exception(RuntimeError(f"Gemini Live: {exc}"))
        except Exception as exc:
            # Provider exception messages can include request details.
            code = getattr(getattr(exc, "rcvd", None), "code", None)
            status = getattr(getattr(exc, "response", None), "status_code", None)
            detail = f" ({status or code})" if status or code else ""
            self.result.set_exception(RuntimeError(f"Gemini Live: {type(exc).__name__}{detail}"))
        else:
            self.result.set_result(result)

    def _transcribe(self, model, key):
        if self._connect is None:
            from websockets.sync.client import connect
        else:
            connect = self._connect

        def send(connection, value):
            connection.send(json.dumps(value))

        transcript = []
        turn_complete = False

        with connect(URL, additional_headers={"x-goog-api-key": key},
                     open_timeout=8, close_timeout=1) as connection:
            send(connection, {"setup": {
                "model": f"models/{model}",
                "generationConfig": {"responseModalities": ["TEXT"]},
                "inputAudioTranscription": {},
                "realtimeInputConfig": {"automaticActivityDetection": {"disabled": True}},
            }})
            if "setupComplete" not in json.loads(connection.recv(timeout=8)):
                raise TranscriptionError("server did not confirm session setup")

            send(connection, {"realtimeInput": {"activityStart": {}}})
            deadline = None
            final_quiet_deadline = None
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
                        if turn_complete:
                            break

                if deadline is not None and time.monotonic() >= min(
                        deadline, final_quiet_deadline or deadline):
                    break
                try:
                    message = json.loads(connection.recv(timeout=0.05))
                except TimeoutError:
                    continue
                content = message.get("serverContent") or {}
                piece = (content.get("inputTranscription") or {}).get("text", "")
                if piece and piece.strip():
                    if (transcript and not transcript[-1][-1].isspace()
                            and not piece[0].isspace()
                            and piece[0] not in ",.!?;:"):
                        transcript.append(" ")
                    transcript.append(piece)
                    if deadline is not None and model == MODEL:
                        final_quiet_deadline = time.monotonic() + 0.65
                if (content.get("generationComplete") or content.get("turnComplete")
                        or message.get("turnComplete")):
                    turn_complete = True
                if deadline is not None and turn_complete:
                    break

        if self.cancelled.is_set():
            return ""
        final = "".join(transcript).strip()
        if final:
            return final
        raise TranscriptionError("no transcript received after recording ended")


def probe(model, key, path, *, connect=None):
    live = Listening(model, key, connect=connect)
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
