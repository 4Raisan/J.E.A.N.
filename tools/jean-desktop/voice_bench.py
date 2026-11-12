"""Repeatable local ASR benchmark for the JEAN desktop voice slice.

Compares two fully local backends on identical 16 kHz mono PCM16 WAV clips
against operator-supplied reference transcripts:

  * ``project2-hybrid``: project 2's streaming Zipformer
    partials plus the offline Parakeet final, run through ``dictate.py --wav``
    under the same pinned ``uv run --offline`` environment as Run Dictation.cmd.
  * ``project9-whisper`` (desktop default): project 9's faster-whisper ``base.en`` English-only
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
import tempfile
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
DEFAULT_BACKEND = "project9-whisper"
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


def score_transcript(reference, hypothesis, app_names=(), technical_terms=()):
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
        "technical_term_matches": {term: exact_phrase(hypothesis, term)
                                   for term in technical_terms},
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
        technical_terms = row.get("technical_terms", [])
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
                if not handle.getnframes():
                    problems.append(f"{label}: WAV is empty")
                    continue
        except (wave.Error, EOFError, OSError) as exc:
            problems.append(f"{label}: not a readable WAV: {exc}")
            continue
        if shape != EXPECTED_WAV:
            problems.append(f"{label}: expected 16 kHz mono PCM16 WAV "
                            f"(channels,sampwidth,rate={EXPECTED_WAV}), got {shape}")
            continue
        if not isinstance(reference, str) or not normalize_tokens(reference):
            problems.append(f"{label}: missing 'reference' transcript")
            continue
        if any(not isinstance(terms, list) or any(
                not isinstance(term, str) or not normalize_tokens(term)
                or not exact_phrase(reference, term) for term in terms
        ) for terms in (app_names, technical_terms)):
            problems.append(f"{label}: app_names and technical_terms must be lists "
                            "of nonempty phrases present in the reference")
            continue
        clips.append({
            "wav": str(clip_path),
            "reference": reference,
            "app_names": app_names,
            "technical_terms": technical_terms,
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


def capture_command(wav_path, stop_file, max_seconds=20.0, microphone=None):
    """Argv for voice_capture.py under the pinned offline uv environment —
    the same dependency pins as project 2's Run Dictation.cmd minus the ASR
    models, which capture does not need."""
    return ([_uv(), *UV_CAPTURE_ARGS, "python", "-B",
             str(HERE / "voice_capture.py"),
             "--out", str(Path(wav_path).resolve()),
             "--max", str(max_seconds),
             "--stop-file", str(Path(stop_file).resolve()),
             *(["--microphone", microphone] if microphone else [])], HERE)


def record_clip(destination, seconds):
    """Save one explicitly requested microphone clip for a private corpus."""
    destination = Path(destination)
    if destination.exists():
        raise ValueError("choose a new WAV path; existing files are preserved")
    with tempfile.TemporaryDirectory(prefix="jean-bench-", ignore_cleanup_errors=True) as directory:
        clip = Path(directory) / "clip.wav"
        argv, cwd = capture_command(clip, Path(directory) / "stop.marker", seconds)
        subprocess.run(argv, cwd=cwd, check=True, timeout=seconds + 15)
        with wave.open(str(clip), "rb") as wav:
            if (wav.getnchannels(), wav.getsampwidth(), wav.getframerate()) != EXPECTED_WAV or not wav.getnframes():
                raise ValueError("capture did not produce a usable mono PCM16 WAV")
        destination.parent.mkdir(parents=True, exist_ok=True)
        with clip.open("rb") as source:
            output = destination.open("xb")
            try:
                with output:
                    shutil.copyfileobj(source, output)
            except OSError:
                destination.unlink(missing_ok=True)
                raise


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
        result["wall_ms"] = round((time.monotonic() - started) * 1000)
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


def _percentile(values, percentile):
    return sorted(values)[(len(values) * percentile + 99) // 100 - 1] if values else None


def _term_totals(rows, key):
    terms = {}
    for row in rows:
        for term, hit in row[key].items():
            tally = terms.setdefault(term, {"hits": 0, "total": 0})
            tally["hits"] += int(hit)
            tally["total"] += 1
    return terms


def run_benchmark(clips, backends, timeout=240):
    report = {"clips": len(clips), "backends": {}, "rows": [],
              "timing": "wall time per fresh decoder process, including startup and model load"}
    by_backend = {backend: [] for backend in backends}
    paired = []
    for index, clip in enumerate(clips):
        order = backends if index % 2 == 0 else list(reversed(backends))
        pair = {}
        for backend in order:
            outcome = decode_clip(backend, clip["wav"], timeout=timeout)
            row = {"clip": index, "backend": backend, "cold": index == 0,
                   "wav": clip["wav"], "note": clip.get("note", ""),
                   "transcript": outcome["transcript"],
                   "partials": outcome["partials"],
                   "wall_ms": outcome["wall_ms"],
                   "returncode": outcome["returncode"],
                   "error": outcome["error"]}
            row.update(score_transcript(
                clip["reference"],
                outcome["transcript"] if outcome["error"] is None else "",
                clip["app_names"], clip.get("technical_terms", ()),
            ))
            report["rows"].append(row)
            by_backend[backend].append(row)
            pair[backend] = row
        if len(backends) == 2 and all(row["error"] is None for row in pair.values()):
            paired.append(pair)
    for backend, decoded in by_backend.items():
        successful = [row for row in decoded if row["error"] is None]
        timed = [row["wall_ms"] for row in decoded if row["wall_ms"] is not None]
        paired_rows = [pair[backend] for pair in paired]
        report["backends"][backend] = {
            "clips": len(decoded),
            "failures": len(decoded) - len(successful),
            "cold_wall_ms": decoded[0]["wall_ms"] if decoded else None,
            "wall_ms_p50": _percentile(timed, 50),
            "wall_ms_p95": _percentile(timed, 95),
            "scored_clips": len(successful),
            "micro_wer": _micro_wer(successful),
            "micro_wer_including_failures": _micro_wer(decoded),
            "paired_success_micro_wer": _micro_wer(paired_rows),
            "exact_transcript_matches": sum(row["exact_transcript"] for row in successful),
            "app_name_terms": _term_totals(decoded, "app_name_matches"),
            "technical_terms": _term_totals(decoded, "technical_term_matches"),
        }
    report["paired_success_clips"] = len(paired)
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
                               case.get("app_names", []), case.get("technical_terms", []))
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
        for term, expected_hit in case.get("expected_technical_matches", {}).items():
            checks[f"term:{term}"] = got["technical_term_matches"].get(term) is expected_hit
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
                        help="decode backend; default matches the desktop")
    parser.add_argument("--record", type=Path, metavar="WAV",
                        help="record one local microphone clip to a new file")
    parser.add_argument("--capture-seconds", type=int, default=20,
                        help="maximum seconds for --record (1–30)")
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

    if args.record is not None:
        if args.run or args.self_test or args.validate or args.dry_run or args.output:
            parser.error("--record is a standalone operation")
        if not 1 <= args.capture_seconds <= 30:
            parser.error("--capture-seconds must be between 1 and 30")
        try:
            record_clip(args.record, args.capture_seconds)
        except (OSError, RuntimeError, ValueError, wave.Error, EOFError, subprocess.SubprocessError) as exc:
            parser.exit(1, f"capture failed: {exc}\n")
        print(f"Saved {args.record}. Add it with a verified reference to your private manifest.")
        return 0

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
