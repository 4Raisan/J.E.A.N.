"""Where Jarvis finds his own files, and where he writes.

Two kinds of places:
  - RESOURCE_DIR: the code and bundled files. In an installed copy this is inside the program folder, which is
    read-only (C:\\Program Files, /Applications), so Jarvis must never write there.
  - DATA_DIR: everything Jarvis writes (settings, transcripts, logs, pictures, documents, caches, sign-in tokens,
    downloaded AI models). One folder per user.

Running from source (python run.py) keeps working exactly as before: DATA_DIR is the project folder itself, so
transcripts/, logs/ and images/ stay where they always were. The installed app passes JARVIS_DATA_DIR (Electron's
per-user folder); a frozen backend started any other way falls back to the usual per-user location.
"""
import os
import platform
import sys

FROZEN = bool(getattr(sys, "frozen", False))  # running from the packaged backend (PyInstaller), not from source


def _resource_dir() -> str:
    if FROZEN:
        return getattr(sys, "_MEIPASS", os.path.dirname(sys.executable))
    return os.path.dirname(os.path.abspath(__file__))


def _default_data_dir() -> str:
    system = platform.system()
    if system == "Windows":
        base = os.environ.get("APPDATA") or os.path.join(os.path.expanduser("~"), "AppData", "Roaming")
    elif system == "Darwin":
        base = os.path.join(os.path.expanduser("~"), "Library", "Application Support")
    else:
        base = os.environ.get("XDG_DATA_HOME") or os.path.join(os.path.expanduser("~"), ".local", "share")
    return adopt_renamed(os.path.join(base, "Jarvis"), os.path.join(base, "Jervis"))


def adopt_renamed(new: str, old: str) -> str:
    """Jarvis was called Jervis before: a folder still under the old name is moved to the new one (a rename on the
    same disk, so instant and nothing is copied). If that can't be done, the old folder keeps being used as it is
    — never a fresh, empty one that would lose settings, phone pairings or downloaded models."""
    if os.path.exists(new) or not os.path.exists(old):
        return new
    try:
        os.rename(old, new)
        return new
    except OSError:
        return old


def _data_dir() -> str:
    explicit = os.environ.get("JARVIS_DATA_DIR")
    if explicit:
        return os.path.abspath(explicit)
    return _default_data_dir() if FROZEN else RESOURCE_DIR


RESOURCE_DIR = _resource_dir()
DATA_DIR = _data_dir()


def resource(*parts: str) -> str:
    """A bundled, read-only file (e.g. a script Jarvis starts, a list shipped with the app)."""
    return os.path.join(RESOURCE_DIR, *parts)


def data(*parts: str) -> str:
    """A file or folder Jarvis writes. Its parent folder is created on first use."""
    path = os.path.join(DATA_DIR, *parts)
    os.makedirs(os.path.dirname(path) or DATA_DIR, exist_ok=True)
    return path


def data_dir(*parts: str) -> str:
    """A folder Jarvis writes into, created if missing."""
    path = os.path.join(DATA_DIR, *parts)
    os.makedirs(path, exist_ok=True)
    return path


# The folders every part of Jarvis agrees on.
def transcripts_dir() -> str:
    return data_dir("transcripts")


def logs_dir() -> str:
    return data_dir("logs")


def models_dir() -> str:
    """Downloaded AI models (speech recognition, and the local AI when Jarvis runs it himself).

    On Windows the speech engine opens its model files with narrow (ANSI) paths, which break when the user's folder
    has letters outside English, e.g. a Hebrew user name. There the models go to an ASCII-only path instead: the
    folder's short 8.3 name if Windows has one, else a folder under ProgramData."""
    folder = data_dir("models")
    if os.name != "nt" or folder.isascii():
        return folder
    try:
        import ctypes
        buffer = ctypes.create_unicode_buffer(1024)
        if ctypes.windll.kernel32.GetShortPathNameW(folder, buffer, 1024) and buffer.value.isascii():
            return buffer.value
    except (AttributeError, OSError):
        pass
    import hashlib
    who = hashlib.sha1(DATA_DIR.encode("utf-8")).hexdigest()[:10]
    program_data = os.environ.get("PROGRAMDATA") or "C:\\ProgramData"
    fallback = os.path.join(adopt_renamed(os.path.join(program_data, "Jarvis"), os.path.join(program_data, "Jervis")), who, "models")
    os.makedirs(fallback, exist_ok=True)
    return fallback
