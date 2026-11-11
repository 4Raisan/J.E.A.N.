# JEAN Desktop — Voice Slice Status

## Voice defaults and Windows checks

Project 9 Faster-Whisper is now the default recognizer. Project 2 remains
selectable. Chat replies go directly to Project 9 Windows Speech. Task
results use the local Project 12-style readback before speech. It preserves
short fact lines, including counts written as words.

- Windows microphone capture opened a stream and wrote a WAV.
- The offscreen desktop decoded the bundled English WAV through its
  default recognizer and routed the transcript to the chat callback.
- A simulated long task result preserved the three-file count in a
  48-character readback. Windows Speech generated 4.54 seconds of
  non-silent WAV audio.
- The Stop control's handler terminated the active Windows speech process.
- Physical dictation through Talk and a live chat provider reply remain
  pending.

Date: 2026-11-11. Scope: `tools/jean-desktop/` only. No commits, no resets;
pre-existing dirty/untracked operator changes were left untouched.

## What was built

- `voice_bench.py` — repeatable local ASR benchmark plus scoring library.
  - Backends (both selectable, `--backend`): `project2-hybrid` (project 2
    Zipformer streaming partials + Parakeet offline final via
    `2 - Streaming Dictation/dictate.py --wav`, the **provisional default**)
    and `project9-whisper` (project 9 faster-whisper `base.en` via
    `9 - Local Whisper and Windows Speech/speech.py --wav`).
  - Both decode through the same pinned `uv run --offline --python 3.12
    --no-project --with …` environments as the project launchers; the
    harness adds `--offline` to the project 9 leg so a benchmark can never
    reach the network. No downloads.
  - Strict scoring: exact token normalization (lowercase, Unicode
    letters/numbers kept, all else → space, whitespace collapsed — the same
    rule as `tools/voice-tester/compare-asr.ps1`), Levenshtein word-error
    rate with sub/del/ins counts, and whole-phrase app-name matches that
    cannot match inside a longer word.
  - Manifest path: `bench/manifest.json` (absent by default → pending).
    Schema and consent rules documented in `bench/manifest.example.json`.
    Every clip must be an existing 16 kHz mono PCM16 WAV with a nonempty
    reference transcript; `wav` paths resolve relative to the manifest.
  - Reports per clip: cold (first decode, includes model load) vs warm wall
    time, partial count (project 2), transcript, return code, error; per
    backend: failures, cold/warm/mean decode ms, micro WER, exact-match
    count, per-term app-name hits. JSON report via `--output`.
  - `user_voice_accuracy` is `measured` only when at least one clip scored;
    otherwise `pending`. **No quality scores were invented**: the repository
    contains no WAV clips and no reference transcripts, and no comparable
    local corpus exists.
- `voice_capture.py` — microphone capture helper run under the project 2
  pinned uv env (sounddevice + numpy only, no ASR models). Streams
  `LEVEL`/`SPEECH` loudness lines (~10 Hz, real RMS), ends on a stop file,
  `--max` seconds, or speech endpointing, and writes the 16 kHz mono PCM16
  WAV both backends accept.
- `speech_render.py` — deterministic project-12-style readback: outcome
  (success/failure/unknown), then up to three verbatim fact lines carrying
  counts, ids, paths or proper names, capped at 600 chars. Short results
  pass through unchanged. No model calls; the Ollama/`render_prompt.py`
  path remains the documented upgrade once a local model is verified.
- `jean-desktop.py` — overlay GUI, same launcher (`Open JEAN Desktop.cmd`
  unchanged).
  - `Reactor`: project 3's circular particle reactor re-expressed in
    QPainter — 340-point orbiting shell that expands/brightens with level,
    complete ring, glowing core, idle breath floor (`max(level, breathe)`
    like `Scene.tsx`). Idle animation always runs.
  - Mic levels while listening come from `voice_capture.py` RMS (real
    measured levels). While TTS plays, the reactor runs a speech-like
    envelope — **animated, not measured** (System.Speech offers no level
    tap; documented gap vs project 3's Web Audio analyser).
  - Controls: `● Talk` (push-to-talk start/stop), `Mode: Chat|Task`,
    `ASR: Project 2|Project 9`, `Stop` (halts playback). Transcript,
    reply and task status shown in the card.
  - Chat mode → `jean chat <text>` (read-only, never a task). Typed entry
    submit uses the same path. Task mode → `jean exec --goal <text>
    <tool> <args…>` where the first spoken word is the tool id; missing
    tool id is refused with guidance.
  - Spoken output: `9 - Local Whisper and Windows Speech/speech.py
    --speak` via `sys.executable`; long results go through
    `speech_render.render_for_speech` first. Provider/playback failures
    (uv missing, mic open failure, decode error, TTS nonzero exit/timeout)
    surface in the status/detail labels. `Stop` terminates the TTS process.
- `bench/fixtures/scoring.json` — synthetic scoring fixture (7 cases:
  identical, substitution, deletion, insertions, normalization, app-name
  boundary, empty hypothesis). No audio decoded.
- `bench/manifest.example.json` — consented-corpus template.

## Checks actually run (all via `cmd.exe /d /c`, native Command Prompt)

| Command (cwd `tools\jean-desktop` unless noted) | Exit | Result |
|---|---|---|
| `py -3 -m py_compile voice_bench.py voice_capture.py speech_render.py jean-desktop.py` | 0 | all four compile |
| `py -3 voice_bench.py --self-test` | 0 | `self-test: 7 synthetic scoring cases passed` (after fixing one fixture expectation: "jean opens kokoro" is 3 reference words → WER 0.3333, initially mis-stated as 0.25/4) |
| `py -3 voice_bench.py --validate` | 0 | `manifest OK: 0 clips … user-voice accuracy pending` |
| `py -3 voice_bench.py --dry-run --backend both` | 0 | printed exact uv argv + cwd for both backends; `uv` resolved to `…\hermes\bin\uv.EXE` |
| `py -3 -c "import osal, stt_local, paths; print('imports ok', osal.IS_WIN)"` (cwd project 9) | 0 | `imports ok True` — the `--speak` import chain resolves on system Python 3.14 |
| `py -3 -c "import speech_render …"` spot checks | 0 | readback shortening/outcome/failure-path verified |
| `set QT_QPA_PLATFORM=offscreen&& py -3 jean-desktop.py` (background shell `242d8f`) | n/a | ran >15 s with no crash output; shell later terminated via `kill_shell`. Whether the python child exited with it was **not verified** — no process enumeration was permitted afterward. No on-screen window was ever shown; no mic or audible TTS was exercised. |

## Not verified / pending manual steps

- **User-voice accuracy: pending.** No consented WAV corpus or reference
  transcripts exist in the repo. To measure: record consented 16 kHz mono
  PCM16 clips, add rows to `bench/manifest.json` (see example), then
  `py -3 voice_bench.py --run --backend both --output bench/report.json`.
  First decode per backend is the cold-start measurement.
- Real decode of either backend end-to-end (models live under the
  operators' data dirs; not inspected per instructions).
- Microphone capture (`voice_capture.py`) and audible TTS playback were not
  exercised — no mic opened, nothing spoken.
- `jean chat` / `jean exec` round-trips from the GUI (provider config is
  the operator's; `chat` may refuse with `provider = "none"`).
- Live Zipformer partials are not shown in the GUI (decode is post-hoc on
  the finished WAV); partial counts are only measured by the benchmark.
- Playback reactor level is a synthetic envelope, not measured output.
- GUI process cleanup after the offscreen smoke test is unconfirmed; if a
  stray `python.exe` running `jean-desktop.py` remains, end it by PID.

## Changed files

- `tools/jean-desktop/voice_bench.py` (new)
- `tools/jean-desktop/voice_capture.py` (new)
- `tools/jean-desktop/speech_render.py` (new)
- `tools/jean-desktop/jean-desktop.py` (rewritten; launcher untouched)
- `tools/jean-desktop/bench/manifest.example.json` (new)
- `tools/jean-desktop/bench/fixtures/scoring.json` (new)
- `tools/jean-desktop/VOICE_SLICE_STATUS.md` (this file)
