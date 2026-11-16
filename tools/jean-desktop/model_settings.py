"""Desktop provider settings and credential storage."""

import json
import os
import re
import subprocess
import tempfile
from pathlib import Path
from urllib.parse import urlsplit

from PyQt6.QtWidgets import (
    QCheckBox, QComboBox, QDialog, QFormLayout, QHBoxLayout, QLabel, QLineEdit,
    QPushButton, QTabWidget, QVBoxLayout, QWidget,
)
from PyQt6.QtCore import Qt, pyqtSignal
from PyQt6.QtGui import QIcon, QPainter, QPainterPath, QPen, QPixmap

import gemini_listening
import voice_bench

KILO_URL = "https://api.kilo.ai/api/gateway"
GATEWAY_URL = "https://ai-gateway.vercel.sh/v1"
WHISPER = "project9-whisper"
HYBRID = "project2-hybrid"
GEMINI = "gemini-live"
LISTENERS = {WHISPER: "Local Faster-Whisper", HYBRID: "Local Zipformer + Parakeet",
             GEMINI: "Gemini Transcribe Live"}
MAIN_KEY = "JEAN_DESKTOP_MAIN_API_KEY"
DECISION_KEY = "JEAN_DESKTOP_DECISION_API_KEY"
DEFAULTS = {
    "main": {"provider": "", "base_url": KILO_URL, "model": "minimax/minimax-m3.1-flash:free",
             "api_key_env": ""},
    "decision": {"mode": "", "protocol": "gateway_evaluate", "base_url": GATEWAY_URL,
                 "model": "nace-ai/drex-max-v1.1", "api_key_env": "", "timeout_secs": 5,
                 "reserve_per_call_micros": 1000, "budget_micros": 100000},
    "listening": {"backend": WHISPER, "model": gemini_listening.MODEL, "api_key_env": ""},
}


def origin(role, config):
    if role == "listening":
        return gemini_listening.HOST
    url = urlsplit(config["base_url"])
    return url.netloc.lower()


def read(settings):
    raw = settings.value("models/v1", "{}", type=str)
    try:
        saved = json.loads(raw)
        return {role: {**defaults, **saved.get(role, {})} for role, defaults in DEFAULTS.items()}
    except (AttributeError, TypeError, ValueError):
        return {role: defaults.copy() for role, defaults in DEFAULTS.items()}


def credential(role, config):
    name = config.get("api_key_env", "")
    if name:
        return os.environ.get(name, "")
    try:
        return vault().get_password("JEAN Desktop " + role, origin(role, config)) or ""
    except Exception as exc:
        raise RuntimeError("Windows Credential Manager could not read the key.") from exc


def vault():
    try:
        from keyring.backends.Windows import WinVaultKeyring
    except ImportError as exc:
        raise RuntimeError("Install keyring to use Windows Credential Manager.") from exc
    return WinVaultKeyring()


def store_credential(role, config, key):
    service, endpoint = "JEAN Desktop " + role, origin(role, config)
    try:
        storage = vault()
        if key:
            storage.set_password(service, endpoint, key)
        elif storage.get_password(service, endpoint):
            storage.delete_password(service, endpoint)
    except Exception as exc:
        raise RuntimeError("Windows Credential Manager could not update the key.") from exc


def cli_environment(profiles, base=None, only=None, override=""):
    env = dict(os.environ if base is None else base)
    main, decision = profiles["main"], profiles["decision"]
    if main["provider"] and only in (None, "main"):
        env["JEAN_MODEL_PROVIDER"] = main["provider"]
        if main["provider"] != "none":
            key = (override or credential("main", main)) if main["provider"] == "openai_compatible" else ""
            key_env = MAIN_KEY if key or (main["provider"] == "openai_compatible"
                                          and not local_endpoint(main)) else ""
            env.update(JEAN_MODEL=main["model"], JEAN_MODEL_BASE_URL=main["base_url"],
                       JEAN_MODEL_API_KEY_ENV=key_env)
            env[MAIN_KEY] = key
    if decision["mode"] and only in (None, "decision"):
        enabled = decision["mode"] != "off"
        key = (override or credential("decision", decision)) if enabled else ""
        key_env = DECISION_KEY if enabled and (key or not local_endpoint(decision)) else ""
        env["JEAN_DECISION_CONFIG"] = json.dumps({
            **decision, "api_key_env": key_env,
        })
        env[DECISION_KEY] = key
    return env


def local_endpoint(config):
    return urlsplit(config["base_url"]).hostname in ("localhost", "127.0.0.1", "::1")


def validate(role, values):
    if values["api_key_env"] and not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", values["api_key_env"]):
        raise ValueError("Enter an environment variable name, or paste the API key in the masked field.")
    if role == "listening":
        if values["backend"] == GEMINI and not values["model"]:
            raise ValueError("Enter the Gemini Live transcription model ID.")
        return
    if values.get("provider") in ("", "none") or values.get("mode") in ("", "off"):
        return
    url = urlsplit(values["base_url"])
    if (url.scheme not in ("https", "http") or not url.hostname or url.username is not None
            or url.query or url.fragment or any(c.isspace() for c in values["base_url"])
            or (url.scheme == "http" and not local_endpoint(values))):
        raise ValueError("Use an HTTPS base URL or a local HTTP endpoint without URL credentials.")
    if not values["model"]:
        raise ValueError("Enter the provider's exact model ID.")
    if role == "decision" and (not 1 <= values["timeout_secs"] <= 10
                               or values["reserve_per_call_micros"] < 0
                               or not values["budget_micros"] >= values["reserve_per_call_micros"]
                               or (not local_endpoint(values)
                                   and values["reserve_per_call_micros"] == 0)):
        raise ValueError("Set a timeout of 1–10 seconds and a budget covering the per-call reserve.")


def eye_icon(color, hidden):
    pixmap = QPixmap(20, 20)
    pixmap.fill(Qt.GlobalColor.transparent)
    painter = QPainter(pixmap)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing)
    painter.setPen(QPen(color, 1.5))
    outline = QPainterPath()
    outline.moveTo(2, 10)
    outline.quadTo(10, 1, 18, 10)
    outline.quadTo(10, 19, 2, 10)
    painter.drawPath(outline)
    painter.drawEllipse(8, 8, 4, 4)
    if not hidden:
        painter.drawLine(3, 3, 17, 17)
    painter.end()
    return QIcon(pixmap)


class ApiKeyField(QLineEdit):
    def __init__(self):
        super().__init__()
        self.saved = ""
        self.pending = ""
        self.revealed = False
        self.setPlaceholderText("No key configured")
        self.eye = self.addAction(QIcon(), QLineEdit.ActionPosition.LeadingPosition)
        self.eye.triggered.connect(self.toggle_visibility)
        self.editingFinished.connect(self.conceal)
        self.conceal()

    def replacement(self):
        return self.pending if self.isReadOnly() else self.text().strip()

    def set_saved(self, value, reset=False):
        self.pending = "" if reset else self.replacement()
        self.saved = value
        self.setReadOnly(True)
        self.conceal()

    def edit_key(self):
        self.setReadOnly(False)
        self.revealed = False
        self.setEchoMode(QLineEdit.EchoMode.Password)
        self.setText(self.pending)
        self.setPlaceholderText("Paste a new API key")
        self.setFocus()
        self.update_eye()

    def toggle_visibility(self):
        if not self.isReadOnly() and self.revealed:
            self.conceal()
            return
        self.revealed = not self.revealed
        self.render_key()

    def conceal(self):
        if not self.isReadOnly():
            self.pending = self.text().strip()
            self.setReadOnly(True)
        self.revealed = False
        self.render_key()

    def render_key(self):
        if self.isReadOnly():
            key = self.pending or self.saved
            if not self.revealed and key:
                key = key[:7] + "*" * (len(key) - 14) + key[-7:] if len(key) > 14 else "*" * len(key)
            self.setEchoMode(QLineEdit.EchoMode.Normal)
            self.setText(key)
            self.setPlaceholderText("No key configured")
            self.setCursorPosition(0)
        else:
            self.setEchoMode(QLineEdit.EchoMode.Normal if self.revealed else QLineEdit.EchoMode.Password)
        self.update_eye()

    def update_eye(self):
        label = "Hide API key" if self.revealed else "Show API key"
        self.eye.setIcon(eye_icon(self.palette().text().color(), not self.revealed))
        self.eye.setText(label)
        self.eye.setToolTip(label)


class ModelSettings(QDialog):
    tested = pyqtSignal(str, str)

    def __init__(self, parent, profiles):
        super().__init__(parent)
        self.setWindowTitle("J.E.A.N. model settings")
        self.resize(640, 435)
        self.fields = {}
        self.keys = {}
        self.forget = {}
        self.tabs = QTabWidget()
        layout = QVBoxLayout(self)
        layout.addWidget(self.tabs)
        for role, title in (("main", "Main brain"), ("decision", "Decision model"),
                            ("listening", "Listening")):
            pane = QWidget()
            form = QFormLayout(pane)
            self.tabs.addTab(pane, title)
            self.fields[role] = {}
            values = profiles[role]

            def choice(name, label, options):
                widget = QComboBox()
                for value, display in options.items():
                    widget.addItem(display, value)
                widget.setCurrentIndex(max(0, widget.findData(values[name])))
                self.fields[role][name] = widget
                form.addRow(label, widget)

            def line(name, label):
                widget = QLineEdit(str(values[name]))
                self.fields[role][name] = widget
                form.addRow(label, widget)

            if role == "main":
                choice("provider", "Provider", {"": "Use J.E.A.N. configuration", "none": "Disabled",
                       "ollama": "Ollama", "openai_compatible": "OpenAI-compatible (including Kilo)"})
            elif role == "decision":
                choice("mode", "Recovery selector", {"": "Use J.E.A.N. configuration", "off": "Off",
                       "shadow": "Shadow · observe only", "active": "Active · requires qualification"})
                choice("protocol", "API format", {"gateway_evaluate": "Vercel AI Gateway",
                       "system_one": "System One", "chat_completions": "Chat Completions"})
            else:
                choice("backend", "Speech recognition", LISTENERS)
            if role != "listening":
                line("base_url", "API base URL")
            line("model", "Gemini model ID" if role == "listening" else "Model ID")
            line("api_key_env", "API key environment variable")
            password = self.keys[role] = ApiKeyField()
            replace = QPushButton("Replace")
            replace.clicked.connect(password.edit_key)
            key_row = QHBoxLayout()
            key_row.addWidget(password, 1)
            key_row.addWidget(replace)
            form.addRow("API key", key_row)
            for name in ("base_url", "api_key_env"):
                field = self.fields[role].get(name)
                if field is not None:
                    field.textChanged.connect(lambda _, role=role: self.refresh_key(role))
            self.refresh_key(role)
            forget = self.forget[role] = QCheckBox("Remove saved credential for this endpoint on Save")
            form.addRow("", forget)
            if role == "decision":
                line("timeout_secs", "Timeout (seconds)")
                line("reserve_per_call_micros", "Per-call reserve (millionths of USD)")
                line("budget_micros", "Total reserve budget (millionths of USD)")
            hint = QLabel({
                "main": "Test can check the saved API key and model here. Select OpenAI-compatible and save to use these settings for chat and planning.",
                "decision": "Used only to choose among eligible workers during checkpoint recovery.",
                "listening": "In Google AI Studio choose Gemini 3.5 Transcribe Live. Talk audio streams to Google with this backend.",
            }[role])
            hint.setWordWrap(True)
            form.addRow(hint)
        self.status = QLabel()
        self.status.setWordWrap(True)
        layout.addWidget(self.status)
        self.test_button = QPushButton("Test selected tab")
        self.test_button.clicked.connect(lambda: parent.test_model_settings(self))
        layout.addWidget(self.test_button)
        save = QPushButton("Save")
        save.clicked.connect(lambda: parent.save_model_settings(self))
        layout.addWidget(save)
        self.tested.connect(self._show_test)
        self.tabs.currentChanged.connect(lambda _: self.conceal_keys())

    def refresh_key(self, role, reset=False):
        config = {name: self.fields[role][name].text().strip()
                  for name in ("base_url", "api_key_env") if name in self.fields[role]}
        try:
            key = credential(role, config) if origin(role, config) else ""
        except (RuntimeError, ValueError):
            key = ""
        self.keys[role].set_saved(key, reset)

    def conceal_keys(self):
        for key in self.keys.values():
            key.conceal()

    def hideEvent(self, event):
        self.conceal_keys()
        super().hideEvent(event)

    def _show_test(self, role, message):
        self.test_button.setEnabled(True)
        self.status.setText(f"{role.capitalize()}: {message}")

    def selected(self):
        return tuple(self.fields)[self.tabs.currentIndex()]

    def values(self, role):
        values = {}
        for name, widget in self.fields[role].items():
            value = widget.currentData() if isinstance(widget, QComboBox) else widget.text().strip()
            if isinstance(DEFAULTS[role][name], int):
                try:
                    value = int(value)
                except ValueError:
                    raise ValueError("Timeout and budget fields must be whole numbers.") from None
            values[name] = value
        validate(role, values)
        return values


def probe_listening(config, key):
    with tempfile.TemporaryDirectory(prefix="jean-listening-test-") as folder:
        path = Path(folder) / "test.wav"
        # Save synthetic audio directly to disk without speaker playback.
        script = (
            "Add-Type -AssemblyName System.Speech; "
            "$s = New-Object System.Speech.Synthesis.SpeechSynthesizer; "
            "$f = [System.Speech.AudioFormat.SpeechAudioFormatInfo]::new(16000, "
            "[System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, "
            "[System.Speech.AudioFormat.AudioChannel]::Mono); "
            "$s.SetOutputToWaveFile($env:JEAN_PROBE_WAV, $f); "
            "$s.Speak('Open PowerShell'); $s.Dispose()"
        )
        subprocess.run(["powershell.exe", "-NoProfile", "-Command", script],
                       env={**os.environ, "JEAN_PROBE_WAV": str(path)}, timeout=15, check=True,
                       capture_output=True, creationflags=subprocess.CREATE_NO_WINDOW)
        if config["backend"] == GEMINI:
            if not key:
                raise ValueError("Enter a Google AI Studio API key.")
            transcript = gemini_listening.probe(config["model"], key, path)
        else:
            result = voice_bench.decode_clip(config["backend"], path, timeout=45)
            if result["error"]:
                raise RuntimeError(result["error"])
            transcript = result["transcript"]
        if voice_bench.normalize_tokens(transcript) != "open powershell":
            raise RuntimeError("Synthetic test did not recognize ‘Open PowerShell’.")
        return "Recognized ‘Open PowerShell’."
