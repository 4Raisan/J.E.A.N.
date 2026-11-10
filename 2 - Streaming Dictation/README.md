# Project 2 — Streaming dictation

Double-click Run Dictation.cmd and speak one sentence. Ctrl+C cancels.
The standalone implementation reuses the installed VoiceAssistant model
bundles in %APPDATA%\VoiceAssistant\models. Zipformer prints streaming
partials, and Parakeet decodes the completed utterance. Python dependencies
are managed by uv. No assistant or cloud provider is involved in inference.

Use --wav with a 16 kHz mono PCM16 WAV to test without the microphone.
The bundled Zipformer sample passed real inference on both engines.
Physical microphone input has not been verified.
Source behavior: VirtualyMichael/voice-assistant, commit 7ab6b081c57efe080614265c973780f2f6c4a649. The wrapper was implemented separately.
