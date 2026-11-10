"""The few things Jarvis does that differ per operating system, in one place: speech, notifications, chime, clipboard.

macOS uses the built-in tools (say, osascript, afplay, pbcopy). Windows uses PowerShell, which ships with every
Windows 10/11 (System.Speech for the voice, a toast for notifications, Set-Clipboard). Linux uses espeak / xclip when
they are installed.
"""
import base64
import os
import platform
import re
import shutil
import subprocess
import tempfile
import threading

SYSTEM = platform.system()
IS_MAC = SYSTEM == "Darwin"
IS_WIN = SYSTEM == "Windows"
IS_LINUX = SYSTEM == "Linux"

_NO_WINDOW = 0x08000000 if IS_WIN else 0  # don't flash a console window when running PowerShell


# ---------- macOS: never fork this process ----------
# Jarvis runs many threads (microphone, network, timers). On macOS, fork()ing a multi-threaded process can crash the
# forked copy before it ever starts the program ("crashed on child side of fork pre-exec": Apple's networking library
# runs code in the child that isn't safe there). Python's subprocess forks by default, so every launch (say, osascript,
# open, ...) risked it. posix_spawn starts the program without forking, so this makes subprocess use it everywhere:
#   - the program is given as an absolute path and close_fds=False (Python's own file handles are already
#     non-inheritable, so nothing leaks into the new program);
#   - a working directory (`cwd`), which would force a fork, is applied by a tiny `sh` wrapper instead.
def _make_subprocess_fork_free() -> None:
    original_init = subprocess.Popen.__init__
    if getattr(original_init, "_jarvis_fork_free", False):
        return

    def init(self, args, *pos, **kw):
        if not pos and not kw.get("shell") and isinstance(args, (list, tuple)) and args \
                and kw.get("executable") is None and not kw.get("preexec_fn") and not kw.get("pass_fds"):
            args = list(args)
            first = os.fspath(args[0])
            if not os.path.dirname(first):
                resolved = shutil.which(first)
                if resolved:
                    args[0] = resolved
            cwd = kw.get("cwd")
            if cwd is not None:
                kw["cwd"] = None
                args = ["/bin/sh", "-c", 'cd "$0" && exec "$@"', os.fspath(cwd), *args]
            kw.setdefault("close_fds", False)
        original_init(self, args, *pos, **kw)

    init._jarvis_fork_free = True
    subprocess.Popen.__init__ = init


if IS_MAC:
    _make_subprocess_fork_free()
_HEBREW = re.compile(r"[֐-׿]")


def has_hebrew(text: str) -> bool:
    return bool(_HEBREW.search(text or ""))


# ---------- PowerShell ----------
def powershell_command(script: str) -> list:
    """argv that runs a script without any quoting problems (the script travels base64 encoded)."""
    encoded = base64.b64encode(script.encode("utf-16-le")).decode("ascii")
    return ["powershell", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", encoded]


def run_powershell(script: str, env: dict = None, timeout: int = 30):
    """Run a PowerShell script. Text data goes in through `env` (read as $env:NAME), never pasted into the script."""
    merged = {**os.environ, **{k: str(v) for k, v in (env or {}).items()}}
    utf8 = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8\n"  # so names and clipboard text in any language survive
    result = subprocess.run(powershell_command(utf8 + script), capture_output=True, text=True, encoding="utf-8",
                            errors="replace", timeout=timeout, env=merged, creationflags=_NO_WINDOW)
    return result.returncode, result.stdout.strip(), result.stderr.strip()


# ---------- speech ----------
_WIN_SPEECH = r"""
Add-Type -AssemblyName System.Speech
$s = New-Object System.Speech.Synthesis.SpeechSynthesizer
if ($env:JARVIS_VOLUME) { $s.Volume = [int]$env:JARVIS_VOLUME }
if ($env:JARVIS_HEBREW -eq '1') {
  $found = $false
  foreach ($v in $s.GetInstalledVoices()) {
    if ($v.Enabled -and $v.VoiceInfo.Culture.Name -like 'he*') { $s.SelectVoice($v.VoiceInfo.Name); $found = $true; break }
  }
  if (-not $found) {
    # Windows' Hebrew voice (Microsoft Asaf) comes with the Hebrew language pack, and is only visible to the newer
    # speech API (Windows.Media.SpeechSynthesis), not to System.Speech: speak through that one.
    Add-Type -AssemblyName System.Runtime.WindowsRuntime
    $null = [Windows.Media.SpeechSynthesis.SpeechSynthesizer, Windows.Media.SpeechSynthesis, ContentType = WindowsRuntime]
    $null = [Windows.Storage.Streams.DataReader, Windows.Storage.Streams, ContentType = WindowsRuntime]
    $voice = [Windows.Media.SpeechSynthesis.SpeechSynthesizer]::AllVoices | ? { $_.Language -like 'he*' } | Select-Object -First 1
    if (-not $voice) { [Console]::Error.WriteLine('No Hebrew voice is installed.'); exit 3 }
    $asTask = ([System.WindowsRuntimeSystemExtensions].GetMethods() | ? { $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' })[0]
    function Await($op, [Type]$t) { $task = $asTask.MakeGenericMethod($t).Invoke($null, @($op)); $task.Wait(-1) | Out-Null; $task.Result }
    $w = New-Object Windows.Media.SpeechSynthesis.SpeechSynthesizer
    $w.Voice = $voice
    try { if ($env:JARVIS_VOLUME) { $w.Options.AudioVolume = [double]$env:JARVIS_VOLUME / 100 } } catch { }
    $stream = Await ($w.SynthesizeTextToStreamAsync($env:JARVIS_TEXT)) ([Windows.Media.SpeechSynthesis.SpeechSynthesisStream])
    $reader = New-Object Windows.Storage.Streams.DataReader($stream.GetInputStreamAt(0))
    $n = [uint32]$stream.Size
    Await ($reader.LoadAsync($n)) ([uint32]) | Out-Null
    $bytes = New-Object byte[] $n
    $reader.ReadBytes($bytes)
    if ($env:JARVIS_SPEECH_OUT) { [IO.File]::WriteAllBytes($env:JARVIS_SPEECH_OUT, $bytes); exit 0 }
    $player = New-Object System.Media.SoundPlayer (New-Object IO.MemoryStream (, $bytes))
    $player.PlaySync()
    exit 0
  }
} elseif ($env:JARVIS_VOICE) {
  try { $s.SelectVoice($env:JARVIS_VOICE) } catch { }
}
if ($env:JARVIS_SPEECH_OUT) { $s.SetOutputToWaveFile($env:JARVIS_SPEECH_OUT) }
$s.Speak($env:JARVIS_TEXT)
"""
_WIN_VOICES = r"""
Add-Type -AssemblyName System.Speech
$s = New-Object System.Speech.Synthesis.SpeechSynthesizer
foreach ($v in $s.GetInstalledVoices()) { if ($v.Enabled) { $v.VoiceInfo.Name + '|' + $v.VoiceInfo.Culture.Name } }
"""


def _cleanup_after(proc, path: str) -> None:
    """Removes a temp audio file once the process playing it exits (doesn't block the caller's own wait/stop)."""
    def runner():
        proc.wait()
        try:
            os.remove(path)
        except OSError:
            pass
    threading.Thread(target=runner, daemon=True).start()


class _Finished:
    """Speech that's already done (written to a file for a test): the shape of a finished speaking process."""
    returncode = 0

    def poll(self):
        return 0

    def wait(self, timeout=None):
        return 0

    def terminate(self):
        pass

    kill = terminate


def speech_process(text: str, volume: int = 100):
    """Start speaking `text` at `volume` (0-100) and return the running process (so it can be stopped), or None if
    this machine can't speak at all."""
    if IS_MAC:
        chosen = os.getenv("JARVIS_VOICE", "").strip()
        voice = ["-v", "Carmit"] if has_hebrew(text) else (["-v", chosen] if chosen else [])
        if volume >= 100:   # the common case: speak directly, exactly as before (no extra latency)
            return subprocess.Popen(["say", *voice, text])
        # `say` has no volume knob: render to a file, then play it back at the chosen level (afplay -v is 0.0-1.0).
        path = tempfile.mktemp(suffix=".aiff")
        subprocess.run(["say", *voice, "-o", path, text])
        proc = subprocess.Popen(["afplay", "-v", f"{max(0, volume) / 100:.2f}", path])
        _cleanup_after(proc, path)
        return proc
    if IS_WIN and has_hebrew(text):   # the neural Hebrew voice when it's downloaded (hebrew_voice.py)
        try:
            import hebrew_voice
            if hebrew_voice.available():
                out = os.getenv("JARVIS_SPEECH_OUT")
                playing = hebrew_voice.speak(text, volume, out_path=out)
                return playing if playing is not None else _Finished()
        except Exception as e:
            print(f"The Hebrew voice failed ({type(e).__name__}: {e}); using Windows' voice.", flush=True)
    if IS_WIN:
        env = {**os.environ, "JARVIS_TEXT": text, "JARVIS_HEBREW": "1" if has_hebrew(text) else "0",
               "JARVIS_VOLUME": str(max(0, min(100, volume)))}
        return subprocess.Popen(powershell_command(_WIN_SPEECH), env=env, creationflags=_NO_WINDOW,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for tool in ("espeak-ng", "espeak"):
        if shutil.which(tool):
            return subprocess.Popen([tool, "-a", str(max(0, min(200, round(volume * 2)))), text])
    if shutil.which("spd-say"):   # no absolute volume flag here (its -i is a relative offset) - left alone
        return subprocess.Popen(["spd-say", text])
    return None



def list_voices() -> list:
    """The voices this computer can speak with, as [name, language] pairs, for the Settings screen."""
    voices = []
    try:
        if IS_MAC:
            out = subprocess.run(["say", "-v", "?"], capture_output=True, text=True, timeout=10).stdout
            for line in out.splitlines():
                match = re.match(r"^(.+?)\s{2,}([a-z]{2,3}[_-][A-Za-z0-9]+)\s", line)
                if match:
                    voices.append([match.group(1).strip(), match.group(2)])
        elif IS_WIN:
            _code, out, _err = run_powershell(_WIN_VOICES, timeout=20)
            for line in (out or "").splitlines():
                if "|" in line:
                    name, culture = line.strip().split("|", 1)
                    voices.append([name, culture])
    except (subprocess.SubprocessError, OSError):
        pass
    return voices


# ---------- notification + chime ----------
_WIN_TOAST = r"""
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
[Windows.UI.Notifications.ToastNotification, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
$xml = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$text = $xml.GetElementsByTagName('text')
$text.Item(0).AppendChild($xml.CreateTextNode($env:JARVIS_TITLE)) | Out-Null
$text.Item(1).AppendChild($xml.CreateTextNode($env:JARVIS_MESSAGE)) | Out-Null
$toast = [Windows.UI.Notifications.ToastNotification]::new($xml)
$appId = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($appId).Show($toast)
"""


def notify(title: str, message: str) -> None:
    try:
        if IS_MAC:
            subprocess.run(
                ["osascript", "-e", "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run",
                 title, message], capture_output=True, timeout=5)
        elif IS_WIN:
            run_powershell(_WIN_TOAST, {"JARVIS_TITLE": title, "JARVIS_MESSAGE": message}, timeout=15)
        elif shutil.which("notify-send"):
            subprocess.run(["notify-send", title, message], capture_output=True, timeout=5)
    except (subprocess.SubprocessError, OSError):
        pass


def chime(times: int = 2) -> None:
    """A short system sound, used when a timer ends and the window can't play its own."""
    import time
    for _ in range(times):
        try:
            if IS_MAC:
                subprocess.run(["afplay", "/System/Library/Sounds/Glass.aiff"], timeout=10)
            elif IS_WIN:
                import winsound
                winsound.MessageBeep(winsound.MB_ICONASTERISK)
                time.sleep(0.9)
            else:
                print("\a", end="", flush=True)
                time.sleep(0.6)
        except (subprocess.SubprocessError, OSError, RuntimeError):
            return
        time.sleep(0.3)


# ---------- opening a file with its default app ----------
def open_path(path: str) -> None:
    if IS_MAC:
        subprocess.run(["open", path], check=True, timeout=10)
    elif IS_WIN:
        os.startfile(path)  # type: ignore[attr-defined]
    elif shutil.which("xdg-open"):
        subprocess.run(["xdg-open", path], check=True, timeout=10)
    else:
        raise RuntimeError("no way to open files on this system")


# ---------- clipboard ----------
def get_clipboard():
    """The clipboard's text, or None if it holds no text (or can't be read) — so it can be put back afterwards."""
    try:
        if IS_MAC:
            return subprocess.run(["pbpaste"], capture_output=True, timeout=5).stdout.decode("utf-8", "replace")
        if IS_WIN:
            code, out, _err = run_powershell("$t = Get-Clipboard -Raw -Format Text; if ($null -ne $t) { "
                                             "[Console]::Out.Write($t) }", {}, timeout=15)
            return out if code == 0 and out else None
    except Exception:
        return None
    return None


def set_clipboard(text: str) -> None:
    """Copy text to the clipboard (any language)."""
    if IS_MAC:
        subprocess.run(["pbcopy"], input=text.encode("utf-8"), timeout=5)
    elif IS_WIN:
        run_powershell("Set-Clipboard -Value $env:JARVIS_TEXT", {"JARVIS_TEXT": text}, timeout=15)
    else:
        for cmd in (["wl-copy"], ["xclip", "-selection", "clipboard"]):
            if shutil.which(cmd[0]):
                subprocess.run(cmd, input=text.encode("utf-8"), timeout=5)
                return
