# Project 9 — Local Whisper and Windows Speech

Run Speech.cmd --setup provisions the upstream English base.en model.
Run Speech.cmd --wav "recording.wav" uses the extracted faster-whisper
code. Run Speech.cmd --speak "Hello from JEAN" uses the extracted Windows
speech playback path. This wrapper chooses English-only recognition.
Microphone test: Run Speech.cmd --mic 5 records five seconds and prints the local transcript.

Verified on Windows with the same 16 kHz sample used for project 2.
The English Whisper model transcribed the sentence successfully.
The Windows speech launcher returned exit 0 for the spoken test.
Physical microphone input and audible playback were not independently observed.
Source: TheJarvis-App/jarvis-app, commit 7d06964126c02422351fe014b4b736cf4e365a78.
