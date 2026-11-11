"""Repeatable local ASR benchmark for the JEAN desktop voice slice.

Compares two fully local backends on identical 16 kHz mono PCM16 WAV clips
against operator-supplied reference transcripts:

  * ``project2-hybrid`` (provisional default): project 2's streaming Zipformer
    partials plus the offline Parakeet final, run through ``dictate.py --wav``
    under the same pinned ``uv run --offline`` environment as Run Dictation.cmd.
  * ``project9-whisper``: project 9's faster-whisper ``base.en`` English-only
    path, run through ``speech.py --wav`` under ``uv run --offline`` (the
    upstream Run Speech.cmd omits ``--offline``; this harness never lets a
    benchmark reach the network).

Scoring is strict and deterministic: exact token normalization (lowercase,
Unicode letters/numbers kept, everything else a space, whitespace collapsed),
word-error rate by Levenshtein alignment, and exact whole-phrase app-name
matches. No quality score is invented: with no consented local corpus the
harness reports ``user_voice_accuracy: pending`` and only the synthetic
scoring fixture (--self-test) is verified.

Usage:
    py -3 voice_bench.py --self-test
    py -3 voice_bench.py --validate [--manifest PATH]
    py -3 voice_bench.py --run [--backend project2-hybrid|project9-whisper|both]
                          [--manifest PATH] [--output PATH]
"""
import argparse
import json
import shutil
import subprocess
import sys
import time
import wave
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
PROJECT2 = ROOT / "2 - Streaming Dictation"
PROJECT9 = ROOT / "9 - Local Whisper and Windows Speech"
DEFAULT_MANIFEST = HERE / "bench" / "manifest.json"
SCORING_FIXTURE = HERE / "bench" / "fixtures" / "scoring.json"

BACKENDS = ("project2-hybrid", "project9-whisper")
DEFAULT_BACKEND = "project2-hybrid"
UV_ARGS_2 = ["run", "--offline", "--python", "3.12", "--no-project",
             "--with", "sherpa-onnx==2.0.1", "--with", "sounddevice==0.5.6",
             "--with", "numpy==2.5.4"]
UV_ARGS_9 = ["run", "--offline", "--python", "3.12", "--no-project",
             "--with", "faster-whisper==1.2.1", "--with", "scipy",
             "--with", "sounddevice"]
EXPECTED_WAV = (1, 2, 16000)  # channels, sample width, rate — dictate.py's contract


# ---------- scoring ----------

def normalize_tokens(text):
    """Exact token normalization shared by WER and phrase matching.

    Lowercase, keep Unicode letters/numbers, turn every other character into a
    space, collapse whitespace. This is the Python equivalent of the
    compare-asr.ps1 Normalize-Transcript used by tools/voice-tester.
    """
    return " ".join(
        "".join(ch.lower() if ch.isalnum() else " " for ch in str(text)).split()
    )


def _words(text):
    normalized = normalize_tokens(text)
    return normalized.split() if normalized else []


def wer(reference, hypothesis):
    """Word error rate via Levenshtein alignment over normalized tokens.

    Returns (wer, substitutions, deletions, insertions, reference_words).
    wer is None when the reference has no words.
    """
    ref, hyp = _words(reference), _words(hypothesis)
    n, m = len(ref), len(hyp)
    # Full DP table; transcripts here are command-length, so memory is trivial.
    dp = [[0] * (m + 1) for _ in range(n + 1)]
    for i in range(n + 1):
        dp[i][0] = i
    for j in range(m + 1):
        dp[0][j] = j
    for i in range(1, n + 1):
        for j in range(1, m + 1):
            cost = 0 if ref[i - 1] == hyp[j - 1] else 1
            dp[i][j] = min(dp[i - 1][j] + 1, dp[i][j - 1] + 1,
                           dp[i - 1][j - 1] + cost)
    subs = dels = ins = 0
    i, j = n, m
    while i > 0 or j > 0:
        if i > 0 and j > 0 and ref[i - 1] == hyp[j - 1] \
                and dp[i][j] == dp[i - 1][j - 1]:
            i -= 1
            j -= 1
        elif i > 0 and j > 0 and dp[i][j] == dp[i - 1][j - 1] + 1:
            subs += 1
            i -= 1
            j -= 1
        elif i > 0 and dp[i][j] == dp[i - 1][j] + 1:
            dels += 1
            i -= 1
        else:
            ins += 1
            j -= 1
    errors = subs + dels + ins
    return (None if n == 0 else errors / n, subs, dels, ins, n)


def exact_phrase(text, term):
    """Whole-phrase match on normalized tokens: 'JEAN' must not match 'JEANETTE'."""
    haystack = " " + normalize_tokens(text) + " "
    needle = " " + normalize_tokens(term) + " "
    return needle in haystack if needle.strip() else False


def score_transcript(reference, hypothesis, app_names=()):
    rate, subs, dels, ins, ref_words = wer(reference, hypothesis)
    matches = {str(term): exact_phrase(hypothesis, term) for term in app_names}
    return {
        "reference_words": ref_words,
        "substitutions": subs,
        "deletions": dels,
        "insertions": ins,
        "transcript_errors": subs + dels + ins,
        "wer": rate,
        "exact_transcript": normalize_tokens(reference) == normalize_tokens(hypothesis),
        "app_name_matches": matches,
    }


# ---------- manifest ----------

def load_manifest(path):
    """Load and validate a consented-clip manifest. Returns (clips, problems)."""
    path = Path(path)
    if not path.is_file():
        # No manifest yet is the expected state, not an error: the harness
        # reports user-voice accuracy as pending until a consented corpus
        # with reference transcripts is added.
        return [], []
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        return [], [f"manifest is not readable JSON: {exc}"]
    rows = data.get("clips") if isinstance(data, dict) else data
    if not isinstance(rows, list):
        return [], ["manifest must be a JSON list or an object with a 'clips' list"]
    clips, problems = [], []
    for index, row in enumerate(rows):
        label = f"clip {index}"
        if not isinstance(row, dict):
            problems.append(f"{label}: not an object")
            continue
        wav = row.get("wav")
        reference = row.get("reference")
        app_names = row.get("app_names", [])
        label = f"clip {index} ({wav})" if wav else f"clip {index}"
        if not wav or not isinstance(wav, str):
            problems.append(f"{label}: missing 'wav'")
            continue
        clip_path = (path.parent / wav).resolve() if not Path(wav).is_absolute() else Path(wav)
        if not clip_path.is_file():
            problems.append(f"{label}: wav not found: {clip_path}")
            continue
        try:
            with wave.open(str(clip_path), "rb") as handle:
                shape = (handle.getnchannels(), handle.getsampwidth(),
                         handle.getframerate())
        except (wave.Error, OSError) as exc:
            problems.append(f"{label}: not a readable WAV: {exc}")
            continue
        if shape != EXPECTED_WAV:
            problems.append(f"{label}: expected 16 kHz mono PCM16 WAV "
                            f"(channels,sampwidth,rate={EXPECTED_WAV}), got {shape}")
            continue
        if not reference or not str(reference).strip():
            problems.append(f"{label}: missing 'reference' transcript")
            continue
        if not isinstance(app_names, list):
            problems.append(f"{label}: 'app_names' must be a list")
            continue
        clips.append({
            "wav": str(clip_path),
            "reference": str(reference),
            "app_names": [str(term) for term in app_names],
            "note": str(row.get("note", "")),
        })
    return clips, problems


# ---------- backends ----------

def _uv():
    exe = shutil.which("uv")
    if not exe:
        raise RuntimeError("uv is not on PATH; both backends run through the "
                           "same pinned 'uv run --offline' environments as the "
                           "project launchers")
    return exe


def decode_command(backend, wav_path):
    """The exact argv a backend decode runs. Exposed for --dry-run checks."""
    wav = str(Path(wav_path).resolve())
    if backend == "project2-hybrid":
        return ([_uv(), *UV_ARGS_2, "python", "-B", "dictate.py", "--wav", wav],
                PROJECT2)
    if backend == "project9-whisper":
        return ([_uv(), *UV_ARGS_9, "python", "speech.py", "--wav", wav],
                PROJECT9)
    raise ValueError(f"unknown backend: {backend}")


UV_CAPTURE_ARGS = ["run", "--offline", "--python", "3.12", "--no-project",
                   "--with", "sounddevice==0.5.6", "--with", "numpy==2.5.4"]


def capture_command(wav_path, stop_file, max_seconds=20.0):
    """Argv for voice_capture.py under the pinned offline uv environment —
    the same dependency pins as project 2's Run Dictation.cmd minus the ASR
    models, which capture does not need."""
    return ([_uv(), *UV_CAPTURE_ARGS, "python", "-B",
             str(HERE / "voice_capture.py"),
             "--out", str(Path(wav_path).resolve()),
             "--max", str(max_seconds),
             "--stop-file", str(Path(stop_file).resolve())], HERE)


def decode_clip(backend, wav_path, timeout=240):
    """Decode one WAV through a backend. Never raises for inference failures;
    they are recorded in the result so a benchmark row stays honest."""
    started = time.monotonic()
    result = {"backend": backend, "wav": str(wav_path), "transcript": "",
              "partials": 0, "wall_ms": None, "returncode": None,
              "error": None}
    try:
        argv, cwd = decode_command(backend, wav_path)
    except (RuntimeError, ValueError) as exc:
        result["error"] = str(exc)
        return result
    try:
        proc = subprocess.run(argv, cwd=cwd, capture_output=True, text=True,
                              encoding="utf-8", errors="replace",
                              timeout=timeout,
                              creationflags=getattr(subprocess,
                                                     "CREATE_NO_WINDOW", 0))
    except subprocess.TimeoutExpired:
        result["error"] = f"decode timed out after {timeout}s"
        return result
    except OSError as exc:
        result["error"] = f"could not start backend: {exc}"
        return result
    result["wall_ms"] = round((time.monotonic() - started) * 1000)
    result["returncode"] = proc.returncode
    out = proc.stdout or ""
    if backend == "project2-hybrid":
        partials = 0
        final = ""
        for line in out.splitlines():
            if line.startswith("Partial:"):
                partials += 1
            elif line.startswith("Final:"):
                final = line[len("Final:"):].strip()
        result["partials"] = partials
        result["transcript"] = final
    else:
        lines = [line.strip() for line in out.splitlines() if line.strip()]
        result["transcript"] = lines[-1] if lines else ""
    if proc.returncode != 0:
        tail = (proc.stderr or "").strip().splitlines()
        result["error"] = tail[-1] if tail else f"exit code {proc.returncode}"
    elif not result["transcript"]:
        result["error"] = "backend produced no transcript"
    return result


# ---------- benchmark ----------

def _micro_wer(rows):
    """Aggregate WER as total errors / total reference words across scored rows."""
    errors = sum(r["transcript_errors"] for r in rows)
    words = sum(r["reference_words"] for r in rows)
    return None if words == 0 else errors / words


def run_benchmark(clips, backends, timeout=240):
    report = {"clips": len(clips), "backends": {}, "rows": []}
    for backend in backends:
        decoded = []
        for index, clip in enumerate(clips):
            cold = index == 0  # first decode per backend carries model load
            outcome = decode_clip(backend, clip["wav"], timeout=timeout)
            row = {"clip": index, "backend": backend, "cold": cold,
                   "wav": clip["wav"], "note": clip.get("note", ""),
                   "transcript": outcome["transcript"],
                   "partials": outcome["partials"],
                   "wall_ms": outcome["wall_ms"],
                   "returncode": outcome["returncode"],
                   "error": outcome["error"]}
            if outcome["error"] is None:
                row.update(score_transcript(clip["reference"],
                                            outcome["transcript"],
                                            clip["app_names"]))
            report["rows"].append(row)
            decoded.append(row)
        scored = [r for r in decoded if r.get("wer") is not None]
        timed = [r for r in decoded if r["wall_ms"] is not None]
        warm = [r for r in decoded if not r["cold"] and r["wall_ms"] is not None]
        cold_rows = [r for r in decoded if r["cold"] and r["wall_ms"] is not None]
        app_terms = {}
        for r in scored:
            for term, hit in r["app_name_matches"].items():
                app_terms.setdefault(term, []).append(hit)
        report["backends"][backend] = {
            "clips": len(decoded),
            "failures": sum(1 for r in decoded if r["error"] is not None),
            "cold_wall_ms": cold_rows[0]["wall_ms"] if cold_rows else None,
            "warm_wall_ms_mean": (round(sum(r["wall_ms"] for r in warm) / len(warm))
                                  if warm else None),
            "decode_wall_ms_mean": (round(sum(r["wall_ms"] for r in timed) / len(timed))
                                    if timed else None),
            "scored_clips": len(scored),
            "micro_wer": _micro_wer(scored),
            "exact_transcript_matches": sum(1 for r in scored if r["exact_transcript"]),
            "app_name_terms": {t: {"hits": sum(v), "total": len(v)}
                               for t, v in app_terms.items()},
        }
    return report


# ---------- self-test fixture ----------

def run_self_test():
    """Score the synthetic fixture through the same code path as real clips.

    This verifies normalization, WER arithmetic and phrase matching without
    any audio or model — it is the only quality signal available until a
    consented local corpus exists.
    """
    fixture = json.loads(SCORING_FIXTURE.read_text(encoding="utf-8"))
    cases = fixture["cases"] if isinstance(fixture, dict) else fixture
    failures = []
    for case in cases:
        got = score_transcript(case["reference"], case["hypothesis"],
                               case.get("app_names", []))
        want = case["expected"]
        checks = {
            "wer": got["wer"] is not None and want["wer"] is not None
            and abs(got["wer"] - want["wer"]) < 1e-4,
            "substitutions": got["substitutions"] == want["substitutions"],
            "deletions": got["deletions"] == want["deletions"],
            "insertions": got["insertions"] == want["insertions"],
            "reference_words": got["reference_words"] == want["reference_words"],
        }
        for term, expected_hit in case.get("expected_app_matches", {}).items():
            checks[f"app:{term}"] = got["app_name_matches"].get(term) is expected_hit
        bad = [name for name, ok in checks.items() if not ok]
        if bad:
            failures.append({"case": case.get("name"), "failed": bad,
                             "got": got, "want": want})
    return {"cases": len(cases), "failures": failures}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST,
                        help="consented-WAV manifest (JSON); wav paths resolve "
                             "relative to the manifest")
    parser.add_argument("--backend", choices=BACKENDS + ("both",),
                        default=DEFAULT_BACKEND,
                        help="decode backend; project2-hybrid is the provisional "
                             "default")
    parser.add_argument("--run", action="store_true",
                        help="actually decode clips (otherwise validate only)")
    parser.add_argument("--validate", action="store_true",
                        help="validate the manifest and exit")
    parser.add_argument("--self-test", action="store_true",
                        help="run the synthetic scoring fixture only")
    parser.add_argument("--dry-run", action="store_true",
                        help="print the backend commands without running them")
    parser.add_argument("--output", type=Path, default=None,
                        help="write the JSON report here")
    parser.add_argument("--timeout", type=int, default=240,
                        help="per-clip decode timeout in seconds")
    args = parser.parse_args(argv)

    status = 0
    if args.self_test:
        outcome = run_self_test()
        if outcome["failures"]:
            print(json.dumps({"self_test": "FAIL", **outcome}, indent=2))
            return 1
        print(f"self-test: {outcome['cases']} synthetic scoring cases passed")
        if not (args.validate or args.run or args.dry_run):
            return 0

    clips, problems = load_manifest(args.manifest)
    if problems:
        for problem in problems:
            print(f"manifest problem: {problem}")
    if args.validate:
        if clips:
            print(f"manifest OK: {len(clips)} consented clip(s) validated; "
                  "no inference run")
        else:
            print(f"manifest OK: 0 clips at {args.manifest} — user-voice "
                  "accuracy pending until a consented local corpus with "
                  "reference transcripts exists")
        return 1 if problems else 0

    backends = list(BACKENDS) if args.backend == "both" else [args.backend]
    if args.dry_run:
        for backend in backends:
            for clip in clips[:1] or [{"wav": "<clip.wav>"}]:
                argv_cmd, cwd = decode_command(backend, clip["wav"])
                print(f"[{backend}] cwd={cwd}")
                print("  " + " ".join(argv_cmd))
        return 0

    if not args.run:
        print(f"{len(clips)} clip(s) in manifest; pass --run to decode. "
              "user_voice_accuracy: pending" if clips else
              "no comparable local corpus/reference exists — "
              "user_voice_accuracy: pending (synthetic fixture verified "
              "with --self-test)")
        return 0

    if not clips:
        print("nothing to benchmark: add consented clips + references to "
              f"{args.manifest} first")
        return 1
    report = run_benchmark(clips, backends, timeout=args.timeout)
    report["manifest"] = str(args.manifest)
    report["backends_requested"] = backends
    report["default_backend"] = DEFAULT_BACKEND
    report["user_voice_accuracy"] = "measured" if any(
        b["scored_clips"] for b in report["backends"].values()) else "pending"
    text = json.dumps(report, indent=2)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(text + "\n", encoding="utf-8")
        print(f"wrote {args.output}")
    else:
        print(text)
    return 0 if not problems else 1


if __name__ == "__main__":
    sys.exit(main())
