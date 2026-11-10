"""Speech recognition on this computer (faster-whisper) — English and Hebrew, told apart sentence by sentence by
itself: no account, no key, no setting to switch, and it works without internet.

Engines (chosen by what the computer has):
  * Bilingual, on the GPU (an NVIDIA card, with the CUDA libraries Ollama already installs next to itself):
      - Whisper large-v3-turbo hears WHICH language a sentence is (only English or Hebrew are allowed answers — a
        Hebrew accent must never make it "Arabic"), and writes it when it's English;
      - ivrit.ai's turbo (Whisper fine-tuned on thousands of hours of Israeli speech, Apache-2.0) writes it when it's
        Hebrew — including the English words people mix in ("תפתח Blender").
    ~0.5 s a sentence on an RTX 4060, ~2.2 GB of GPU memory, which fits next to the local AI model.
    Measured on spoken commands: Hebrew 4% character errors (stock Whisper turbo 8%, small 24%); English as good as
    or better than base.en. ivrit.ai's model alone can't do English (it writes English speech in Hebrew letters),
    hence the pair.
  * Bilingual on the CPU (no usable GPU): "small" hears the language and writes Hebrew (2-4 s, much weaker), and
    base.en writes English. Large models on this CPU take 8-17 s a sentence: only a last resort.
  * English only (JARVIS_STT_LANGUAGE=en): base.en on the CPU, as before.

The online recognizers (Whisper on Groq, Google's free one) stay available in app.py as before.
Models are downloaded once, during first-run setup, into the data folder (an ASCII path: see paths.models_dir).
"""
import os
import threading
import time
from dataclasses import dataclass

import paths

# The models come from Hugging Face; keep their downloads quiet in Jarvis's log.
os.environ.setdefault("HF_HUB_DISABLE_PROGRESS_BARS", "1")
os.environ.setdefault("HF_HUB_DISABLE_TELEMETRY", "1")
os.environ.setdefault("HF_HUB_VERBOSITY", "error")

# "auto" (default): English and Hebrew, whichever is spoken. "en": English only (lighter: no Hebrew models).
# "he" is accepted for older settings files and means the same as "auto".
LANGUAGE = (os.getenv("JARVIS_STT_LANGUAGE") or "auto").strip().lower()
if LANGUAGE not in ("auto", "en", "he"):
    LANGUAGE = "auto"
BILINGUAL = LANGUAGE != "en"

MODELS = {   # name: (Hugging Face repo, approximate download size)
    "base.en": ("Systran/faster-whisper-base.en", 150e6),
    "small": ("Systran/faster-whisper-small", 490e6),
    "turbo": ("mobiuslabsgmbh/faster-whisper-large-v3-turbo", 1.6e9),
    "ivrit-turbo": ("ivrit-ai/whisper-large-v3-turbo-ct2", 1.6e9),
    "ivrit-large": ("ivrit-ai/whisper-large-v3-ct2", 3.1e9),
}
# Which ivrit.ai model writes Hebrew (Settings: JARVIS_HEBREW_SPEECH). Measured on real Israeli speech (FLEURS, 60
# sentences): word errors 15.8% (large) vs 17.3% (turbo) clean, and in a noisy room (10 dB) 20.6% vs 26.0%; on a
# 2.5 s command ~0.17 s slower, and 0.8 GB more GPU memory (3.0 GB with turbo, vs 2.2).
_HEBREW_CHOICE = (os.getenv("JARVIS_HEBREW_SPEECH") or "large").strip().lower()
HEBREW_MODEL = "ivrit-turbo" if _HEBREW_CHOICE == "turbo" else "ivrit-large"
# On the GPU: Whisper "small" tells the languages apart (measured on real speech, FLEURS he/en 60+60: no mistakes,
# also on 2.5 s clips; 63 ms; 0.3 GB), and only the big model for the language being spoken stays loaded beside
# it — ivrit.ai for Hebrew, turbo for English (5.3% word errors on real English; ivrit.ai's: 51%). The local AI
# model gets the rest of the GPU, which decides how fast it answers (see local_llm.gpu_slot).
GPU_MODELS = ("small", "turbo", HEBREW_MODEL)
GPU_MB = {"small": 320, "turbo": 1110, "ivrit-turbo": 1110, "ivrit-large": 1700}   # measured, loaded and warm
CPU_MODELS = ("small", "base.en")
# The language decision: English only when the detector is clearly sure. Hebrew speakers mixing in English words get
# split, low scores for both — and Hebrew is then right: ivrit.ai's model writes the English words too.
EN_SURE, EN_RATIO = 0.75, 3.0
EN_MAYBE = 0.4   # between EN_MAYBE and EN_SURE (and little Hebrew): transcribe as English, keep it only if confident
EN_LOGPROB = -0.5
_GPU_COMPUTE = "int8_float16"
# Right-to-left marks the Hebrew model sometimes writes: invisible, but they'd break matching and comparisons.
_INVISIBLE = dict.fromkeys(map(ord, "\u200e\u200f\u202a\u202b\u202c\u202d\u202e\u2066\u2067\u2068\u2069\ufeff"))


@dataclass
class Transcript:
    text: str
    language: str = "en"       # "en" or "he": what was heard
    confidence: float = 1.0    # the language detector's probability for that language
    engine: str = ""
    seconds: float = 0.0
    logprob: float = 0.0       # the recognizer's average log-probability: low = it wasn't sure of the words


@dataclass(frozen=True)
class Setup:
    """Which model does which job, and where. detector None = English only."""
    detector: str = None
    english: str = "base.en"
    hebrew: str = None
    device: str = "cpu"


def whisper_language():
    """The language code for the online Whisper (Groq): None lets it detect, which handles both languages."""
    return "en" if LANGUAGE == "en" else None


_lock = threading.Lock()
_models = {}             # (name, device): loaded model
_failed_gpu = False
_dll_handles = []


def available() -> bool:
    """Whether the faster-whisper package is installed (it ships with the installed app)."""
    try:
        import faster_whisper  # noqa: F401
        return True
    except Exception:   # ImportError, or a broken native library
        return False


# ---------- the GPU: CUDA libraries ----------

def cuda_dirs() -> list:
    """Folders holding NVIDIA's CUDA 12 libraries that CTranslate2 needs (cuBLAS). Ollama installs exactly these next
    to itself — the user's Ollama or the one Jarvis manages — so nothing extra is downloaded or installed."""
    candidates = [os.getenv("JARVIS_CUDA_DIR") or ""]
    local, program_files = os.environ.get("LOCALAPPDATA", ""), os.environ.get("ProgramFiles", "")
    for root in (os.path.join(local, "Programs", "Ollama"), os.path.join(program_files, "Ollama")):
        candidates.append(os.path.join(root, "lib", "ollama", "cuda_v12"))
    runtime = os.path.join(paths.DATA_DIR, "runtime")
    if os.path.isdir(runtime):
        for folder, _dirs, files in os.walk(runtime):
            if "cublas64_12.dll" in files:
                candidates.append(folder)
    return [d for d in candidates if d and os.path.exists(os.path.join(d, "cublas64_12.dll"))]


def _cuda_count() -> int:
    try:
        import ctranslate2
        return ctranslate2.get_cuda_device_count()
    except Exception:
        return 0


_gpu_known = None


def gpu_possible() -> bool:
    """An NVIDIA GPU CTranslate2 can use, with its libraries found. (Loading can still fail — no free GPU memory —
    and then the CPU models are used.) JARVIS_STT_DEVICE=cpu turns the GPU off for speech."""
    global _gpu_known
    if not BILINGUAL or _failed_gpu or (os.getenv("JARVIS_STT_DEVICE") or "").lower() == "cpu":
        return False
    if _gpu_known is not None:
        return _gpu_known
    if os.name == "nt":
        dirs = cuda_dirs()
        if not dirs:
            _gpu_known = False
            return False
        for d in dirs:
            if d not in os.environ.get("PATH", "").split(os.pathsep):
                os.environ["PATH"] = d + os.pathsep + os.environ.get("PATH", "")
            try:
                _dll_handles.append(os.add_dll_directory(d))
            except (AttributeError, OSError):
                pass
    _gpu_known = _cuda_count() > 0
    return _gpu_known


def plan() -> tuple:
    """The models this computer should have (what setup downloads)."""
    if not BILINGUAL:
        return ("base.en",)
    return GPU_MODELS if gpu_possible() else CPU_MODELS


# ---------- models on disk ----------

def _model_dir(name: str) -> str:
    return os.path.join(paths.models_dir(), "whisper", name)


def has(name: str) -> bool:
    return bool(name) and os.path.exists(os.path.join(_model_dir(name), "model.bin"))


def model_ready() -> bool:
    return all(has(name) for name in plan())


def setup() -> Setup:
    """What to use right now, from what the computer can do and which models are on disk — the best plan when it's
    all here, and otherwise whatever still works (English keeps working while the Hebrew models download)."""
    if BILINGUAL and gpu_possible() and has("turbo"):
        for hebrew in (HEBREW_MODEL, "ivrit-turbo", "ivrit-large"):   # the chosen one, else what's already here
            if has(hebrew):
                return Setup("small" if has("small") else "turbo", "turbo", hebrew, "cuda")
    if BILINGUAL:
        detector = next((n for n in ("small", "turbo") if has(n)), None)   # ivrit.ai's can't tell English
        if detector:
            hebrew = "small" if has("small") else ("ivrit-turbo" if has("ivrit-turbo") else detector)
            return Setup(detector, "base.en" if has("base.en") else detector, hebrew, "cpu")
    return Setup(None, "base.en", None, "cpu")


def ensure_model(progress=None) -> str:
    """Download the models this computer needs if they aren't here yet. progress(detail, fraction) is called along
    the way; the smallest come first, so English works as early as possible."""
    from huggingface_hub import snapshot_download
    wanted = sorted((name for name in plan() if not has(name)), key=lambda n: MODELS[n][1])
    for i, name in enumerate(wanted, 1):
        repo, size = MODELS[name]
        if progress:
            progress(f"Downloading speech model {i} of {len(wanted)} ({name}, about {round(size / 1e6)} MB)…", None)
        snapshot_download(repo_id=repo, local_dir=_model_dir(name),
                          allow_patterns=["config.json", "model.bin", "tokenizer.json", "vocabulary.*",
                                          "preprocessor_config.json"])
        if not has(name):
            raise RuntimeError(f"the speech model {name} didn't download completely")
    return _model_dir(plan()[0])


# ---------- loading ----------

class _UseCpu(Exception):
    pass


def _load(name: str, device: str):
    from faster_whisper import WhisperModel
    if device == "cuda":
        return WhisperModel(_model_dir(name), device="cuda", compute_type=_GPU_COMPUTE)
    threads = max(1, min(8, (os.cpu_count() or 2) // 2))
    return WhisperModel(_model_dir(name), device="cpu", compute_type="int8", cpu_threads=threads)


def _get(name: str, device: str):
    """A loaded model (loading it the first time). Called with _lock held."""
    global _failed_gpu
    key = (name, device)
    if key not in _models:
        try:
            _models[key] = _load(name, device)
        except Exception as e:
            if device != "cuda":
                raise
            # No free GPU memory, a driver problem...: the CPU models from now on, said once in the log.
            print(f"Speech recognition can't use the GPU ({type(e).__name__}: {str(e)[:160]}); using the CPU.",
                  flush=True)
            _failed_gpu = True
            for k in [k for k in _models if k[1] == "cuda"]:
                del _models[k]
            raise _UseCpu() from e
    return _models[key]


_conversation = "en"    # the language being spoken: whose big model stays loaded (see prefer)
_pending_mb = 0         # GPU memory a model being loaded right now is about to take (local_llm counts it)


def pending_gpu_mb() -> int:
    """GPU memory speech recognition is about to take but doesn't hold yet: a model being loaded, or the one the
    conversation's language needs next (the AI model's GPU plan must leave room for it, or both spill)."""
    try:
        s = setup()
    except Exception:
        return _pending_mb
    if s.device != "cuda" or s.detector != "small":
        return _pending_mb
    loaded = {name for name, device in _models if device == "cuda"}
    target = {"small", _resident(s, _conversation)}
    held = sum(GPU_MB.get(n, 1200) for n in loaded)
    after = sum(GPU_MB.get(n, 1200) for n in target)   # what will be loaded once any switch is done
    return max(_pending_mb, after - held)


def _resident(s: Setup, code: str) -> str:
    return s.hebrew if code == "he" else s.english


def prefer(code: str, background: bool = True) -> None:
    """The conversation is now in `code` ("he"/"en"): keep that language's big model loaded on the GPU and let the
    other go (about 4-8 s, in the background). Speech in the other language meanwhile still works (see
    _speaking_models)."""
    global _conversation
    if code not in ("he", "en") or code == _conversation:
        return
    _conversation = code
    if background:
        threading.Thread(target=_switch, daemon=True, name="speech-switch").start()
    else:
        _switch()


def _switch() -> None:
    global _pending_mb
    try:
        with _lock:
            s = setup()
            if s.device != "cuda" or s.detector != "small":
                return
            want, other = _resident(s, _conversation), _resident(s, "en" if _conversation == "he" else "he")
            if (want, "cuda") in _models:
                return
            started = time.time()
            if (other, "cuda") in _models:
                del _models[(other, "cuda")]
                import gc
                gc.collect()
            _pending_mb = GPU_MB.get(want, 1200)
            try:
                _warm(_get(want, "cuda"))
            finally:
                _pending_mb = 0
            print(f"Speech recognition: {want} loaded for {'Hebrew' if _conversation == 'he' else 'English'} "
                  f"({time.time() - started:.1f}s)", flush=True)
    except _UseCpu:
        pass
    except Exception as e:
        print(f"Couldn't switch speech models: {e}", flush=True)


def _warm(model) -> None:
    """One pass on a second of silence: the GPU working memory is taken now, so the first sentence isn't slow and
    the AI model's GPU plan (made after this) already counts it."""
    import numpy as np
    model.detect_language(np.zeros(16000, dtype=np.float32))


def preload() -> None:
    """Load the models now (a few seconds), so the first sentence isn't slow. Safe to call more than once."""
    if not available():
        return
    try:
        with _lock:
            try:
                _load_setup(setup())
            except _UseCpu:
                _load_setup(setup())
    except Exception as e:
        print(f"Couldn't preload speech recognition: {e}", flush=True)


def _load_setup(s: Setup) -> None:
    global _pending_mb
    names = {s.detector, s.english, s.hebrew} - {None}
    if s.device == "cuda" and s.detector == "small":   # the language detector and the spoken language's model only
        names = {"small", _resident(s, _conversation)}
    for name in names:
        if has(name):
            if s.device == "cuda" and (name, "cuda") not in _models:
                _pending_mb = GPU_MB.get(name, 1200)
            try:
                model = _get(name, s.device)
                if s.device == "cuda":
                    _warm(model)
            finally:
                _pending_mb = 0


# ---------- recognising ----------

def _audio(wav_bytes: bytes):
    import io
    import wave

    import numpy as np
    with wave.open(io.BytesIO(wav_bytes)) as wav:
        frames = wav.readframes(wav.getnframes())
        width, rate, channels = wav.getsampwidth(), wav.getframerate(), wav.getnchannels()
    if width != 2 or rate != 16000:
        # A microphone records at 44.1/48 kHz. Brought down with a proper filtered resampler (libswresample, via
        # faster-whisper's own decoder) — not speech_recognition's audioop.ratecv, which has no anti-alias filter and
        # folds the sound above 8 kHz (hiss, the top of ש/ס/צ/ז) into what Whisper hears.
        from faster_whisper import decode_audio
        return decode_audio(io.BytesIO(wav_bytes), sampling_rate=16000)
    audio = np.frombuffer(frames, dtype=np.int16).astype(np.float32) / 32768.0
    if channels > 1:
        audio = audio.reshape(-1, channels).mean(axis=1)
    return audio


def _write(model, audio, language: str, beam: int):
    segments, _info = model.transcribe(audio, language=language, beam_size=beam, vad_filter=True,
                                       condition_on_previous_text=False)
    segments = list(segments)   # the transcription happens here: segments is lazy
    text = " ".join(s.text.strip() for s in segments).translate(_INVISIBLE).strip()
    logprob = (sum(s.avg_logprob for s in segments) / len(segments)) if segments else -9.0
    return text, logprob


def choose_language(probabilities: dict) -> str:
    """English only when the detector is clearly sure; otherwise Hebrew (also for mixed speech)."""
    en, he = probabilities.get("en", 0.0), probabilities.get("he", 0.0)
    if en >= EN_SURE and en >= EN_RATIO * he:
        return "en"
    return "he"


def transcribe_full(wav_bytes: bytes) -> Transcript:
    """What was said, and in which language."""
    started = time.time()
    audio = _audio(wav_bytes)
    with _lock:
        try:
            result = _recognise(audio, setup())
        except _UseCpu:
            result = _recognise(audio, setup())
    result.seconds = time.time() - started
    return result


def _recognise(audio, s: Setup) -> Transcript:
    if s.detector is None:
        text, logprob = _write(_get(s.english, s.device), audio, "en", 1)
        return Transcript(text, "en", 1.0, s.english, logprob=logprob)
    beam = 5 if s.device == "cuda" else 1
    _lang, _p, all_probs = _get(s.detector, s.device).detect_language(audio)
    probs = dict(all_probs)
    en, he = probs.get("en", 0.0), probs.get("he", 0.0)
    language = choose_language(probs)
    english_model, hebrew_model = _speaking_models(s, language)
    if language == "he" and EN_MAYBE <= en and he < 0.15:
        # Unsure, and Hebrew isn't likely either: try English, and keep it only if the model is confident in it.
        english, logprob = _write(_get(english_model, s.device), audio, "en", beam)
        if english and logprob >= EN_LOGPROB:
            return Transcript(english, "en", en, english_model + " (checked)", logprob=logprob)
    if language == "en":
        text, logprob = _write(_get(english_model, s.device), audio, "en", beam)
        return Transcript(text, "en", en, english_model, logprob=logprob)
    text, logprob = _write(_get(hebrew_model, s.device), audio, "he", beam)
    return Transcript(text, "he", he, hebrew_model, logprob=logprob)


def _speaking_models(s: Setup, language: str) -> tuple:
    """(English model, Hebrew model) for this sentence. With the small detector only one big model is loaded: a few
    English words in a Hebrew conversation ("OK", "hey Jarvis") go to "small" — no switch for them; Hebrew in an
    English conversation goes to turbo (which writes Hebrew too, less well) while ivrit.ai loads in the background
    for what follows. A sentence never waits seconds for a model to load, unless none is loaded at all."""
    if s.device != "cuda" or s.detector != "small":
        return s.english, s.hebrew
    loaded = {name for name, device in _models if device == "cuda"}
    english = s.english if s.english in loaded else "small"
    if s.hebrew in loaded or s.english not in loaded:
        hebrew = s.hebrew
    else:
        hebrew = s.english
        if language == "he" and _conversation != "he":
            prefer("he")   # Hebrew from now on, most likely: get ivrit.ai ready (it waits for this sentence)
    return english, hebrew


def transcribe(wav_bytes: bytes) -> str:
    """Text of a short WAV recording (16 kHz mono, as Jarvis records it)."""
    return transcribe_full(wav_bytes).text


def ready() -> bool:
    """Local recognition works now (perhaps English only, while the Hebrew models download)."""
    return available() and (has(setup().english))


def hebrew_ready() -> bool:
    return available() and setup().hebrew is not None


def engine_summary() -> str:
    s = setup()
    if s.detector is None:
        return f"{s.english} (English only) on the CPU"
    where = "GPU" if s.device == "cuda" else "CPU"
    names = " + ".join(dict.fromkeys(n for n in (s.detector, s.english, s.hebrew) if n))
    return f"{names} on the {where}" + ("" if model_ready() else " (more models still to download)")
